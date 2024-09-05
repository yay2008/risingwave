// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use rdkafka::consumer::{BaseConsumer, Consumer};
use rdkafka::error::KafkaResult;
use rdkafka::{Offset, TopicPartitionList};
use risingwave_common::bail;

use crate::error::ConnectorResult;
use crate::source::base::SplitEnumerator;
use crate::source::kafka::split::KafkaSplit;
use crate::source::kafka::{
    KafkaContextCommon, KafkaProperties, RwConsumerContext, KAFKA_ISOLATION_LEVEL,
};
use crate::source::SourceEnumeratorContextRef;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum KafkaEnumeratorOffset {
    Earliest,
    Latest,
    Timestamp(i64),
    None,
}

pub struct KafkaSplitEnumerator {
    context: SourceEnumeratorContextRef,
    broker_address: String,
    topic: String,
    client: BaseConsumer<RwConsumerContext>,
    start_offset: KafkaEnumeratorOffset,

    // maybe used in the future for batch processing
    stop_offset: KafkaEnumeratorOffset,

    sync_call_timeout: Duration,
}

impl KafkaSplitEnumerator {}

#[async_trait]
impl SplitEnumerator for KafkaSplitEnumerator {
    type Properties = KafkaProperties;
    type Split = KafkaSplit;

    async fn new(
        properties: KafkaProperties,
        context: SourceEnumeratorContextRef,
    ) -> ConnectorResult<KafkaSplitEnumerator> {
        let mut config = rdkafka::ClientConfig::new();
        let common_props = &properties.common;

        let broker_address = common_props.brokers.clone();
        let broker_rewrite_map = properties.privatelink_common.broker_rewrite_map.clone();
        let topic = common_props.topic.clone();
        config.set("bootstrap.servers", &broker_address);
        config.set("isolation.level", KAFKA_ISOLATION_LEVEL);
        common_props.set_security_properties(&mut config);
        properties.set_client(&mut config);
        let mut scan_start_offset = match properties
            .scan_startup_mode
            .as_ref()
            .map(|s| s.to_lowercase())
            .as_deref()
        {
            Some("earliest") => KafkaEnumeratorOffset::Earliest,
            Some("latest") => KafkaEnumeratorOffset::Latest,
            None => KafkaEnumeratorOffset::Earliest,
            _ => bail!(
                "properties `scan_startup_mode` only supports earliest and latest or leaving it empty"
            ),
        };

        if let Some(s) = &properties.time_offset {
            let time_offset = s.parse::<i64>().map_err(|e| anyhow!(e))?;
            scan_start_offset = KafkaEnumeratorOffset::Timestamp(time_offset)
        }

        // don't need kafka metrics from enumerator
        let ctx_common = KafkaContextCommon::new(
            broker_rewrite_map,
            None,
            None,
            properties.aws_auth_props,
            common_props.is_aws_msk_iam(),
        )
        .await?;
        let client_ctx = RwConsumerContext::new(ctx_common);
        let client: BaseConsumer<RwConsumerContext> =
            config.create_with_context(client_ctx).await?;

        // Note that before any SASL/OAUTHBEARER broker connection can succeed the application must call
        // rd_kafka_oauthbearer_set_token() once – either directly or, more typically, by invoking either
        // rd_kafka_poll(), rd_kafka_consumer_poll(), rd_kafka_queue_poll(), etc, in order to cause retrieval
        // of an initial token to occur.
        // https://docs.confluent.io/platform/current/clients/librdkafka/html/rdkafka_8h.html#a988395722598f63396d7a1bedb22adaf
        if common_props.is_aws_msk_iam() {
            #[cfg(not(madsim))]
            client.poll(Duration::from_secs(10)); // note: this is a blocking call
            #[cfg(madsim)]
            client.poll(Duration::from_secs(10)).await;
        }

        Ok(Self {
            context,
            broker_address,
            topic,
            client,
            start_offset: scan_start_offset,
            stop_offset: KafkaEnumeratorOffset::None,
            sync_call_timeout: properties.common.sync_call_timeout,
        })
    }

    async fn list_splits(&mut self) -> ConnectorResult<Vec<KafkaSplit>> {
        let topic_partitions = self.fetch_topic_partition().await.with_context(|| {
            format!(
                "failed to fetch metadata from kafka ({})",
                self.broker_address
            )
        })?;

        let watermarks = self.get_watermarks(topic_partitions.as_ref()).await?;
        let mut start_offsets = self
            .fetch_start_offset(topic_partitions.as_ref(), &watermarks)
            .await?;

        let mut stop_offsets = self
            .fetch_stop_offset(topic_partitions.as_ref(), &watermarks)
            .await?;

        let ret = topic_partitions
            .into_iter()
            .map(|partition| KafkaSplit {
                topic: self.topic.clone(),
                partition,
                start_offset: start_offsets.remove(&partition).unwrap(),
                stop_offset: stop_offsets.remove(&partition).unwrap(),
                hack_seek_to_latest: false,
            })
            .collect();

        Ok(ret)
    }
}

impl KafkaSplitEnumerator {
    async fn get_watermarks(&self, partitions: &[i32]) -> KafkaResult<HashMap<i32, (i64, i64)>> {
        let mut map = HashMap::new();
        for partition in partitions {
            let (low, high) = self
                .client
                .fetch_watermarks(self.topic.as_str(), *partition, self.sync_call_timeout)
                .await?;
            self.report_high_watermark(*partition, high);
            map.insert(*partition, (low, high));
        }
        tracing::debug!("fetch kafka watermarks: {map:?}");
        Ok(map)
    }

    pub async fn list_splits_batch(
        &mut self,
        expect_start_timestamp_millis: Option<i64>,
        expect_stop_timestamp_millis: Option<i64>,
    ) -> ConnectorResult<Vec<KafkaSplit>> {
        let topic_partitions = self.fetch_topic_partition().await.with_context(|| {
            format!(
                "failed to fetch metadata from kafka ({})",
                self.broker_address
            )
        })?;

        // here we are getting the start offset and end offset for each partition with the given
        // timestamp if the timestamp is None, we will use the low watermark and high
        // watermark as the start and end offset if the timestamp is provided, we will use
        // the watermark to narrow down the range
        let mut expect_start_offset = if let Some(ts) = expect_start_timestamp_millis {
            Some(
                self.fetch_offset_for_time(topic_partitions.as_ref(), ts)
                    .await?,
            )
        } else {
            None
        };

        let mut expect_stop_offset = if let Some(ts) = expect_stop_timestamp_millis {
            Some(
                self.fetch_offset_for_time(topic_partitions.as_ref(), ts)
                    .await?,
            )
        } else {
            None
        };

        // Watermark here has nothing to do with watermark in streaming processing. Watermark
        // here means smallest/largest offset available for reading.
        let mut watermarks = {
            let mut ret = HashMap::new();
            for partition in &topic_partitions {
                let (low, high) = self
                    .client
                    .fetch_watermarks(self.topic.as_str(), *partition, self.sync_call_timeout)
                    .await?;
                ret.insert(partition, (low - 1, high));
            }
            ret
        };

        Ok(topic_partitions
            .iter()
            .map(|partition| {
                let (low, high) = watermarks.remove(&partition).unwrap();
                let start_offset = {
                    let start = expect_start_offset
                        .as_mut()
                        .map(|m| m.remove(partition).flatten().map(|t| t-1).unwrap_or(low))
                        .unwrap_or(low);
                    i64::max(start, low)
                };
                let stop_offset = {
                    let stop = expect_stop_offset
                        .as_mut()
                        .map(|m| m.remove(partition).unwrap_or(Some(high)))
                        .unwrap_or(Some(high))
                        .unwrap_or(high);
                    i64::min(stop, high)
                };

                if start_offset > stop_offset {
                    tracing::warn!(
                        "Skipping topic {} partition {}: requested start offset {} is greater than stop offset {}",
                        self.topic,
                        partition,
                        start_offset,
                        stop_offset
                    );
                }
                KafkaSplit {
                    topic: self.topic.clone(),
                    partition: *partition,
                    start_offset: Some(start_offset),
                    stop_offset: Some(stop_offset),
                    hack_seek_to_latest:false
                }
            })
            .collect::<Vec<KafkaSplit>>())
    }

    async fn fetch_stop_offset(
        &self,
        partitions: &[i32],
        watermarks: &HashMap<i32, (i64, i64)>,
    ) -> KafkaResult<HashMap<i32, Option<i64>>> {
        match self.stop_offset {
            KafkaEnumeratorOffset::Earliest => unreachable!(),
            KafkaEnumeratorOffset::Latest => {
                let mut map = HashMap::new();
                for partition in partitions {
                    let (_, high_watermark) = watermarks.get(partition).unwrap();
                    map.insert(*partition, Some(*high_watermark));
                }
                Ok(map)
            }
            KafkaEnumeratorOffset::Timestamp(time) => {
                self.fetch_offset_for_time(partitions, time).await
            }
            KafkaEnumeratorOffset::None => partitions
                .iter()
                .map(|partition| Ok((*partition, None)))
                .collect(),
        }
    }

    async fn fetch_start_offset(
        &self,
        partitions: &[i32],
        watermarks: &HashMap<i32, (i64, i64)>,
    ) -> KafkaResult<HashMap<i32, Option<i64>>> {
        match self.start_offset {
            KafkaEnumeratorOffset::Earliest | KafkaEnumeratorOffset::Latest => {
                let mut map = HashMap::new();
                for partition in partitions {
                    let (low_watermark, high_watermark) = watermarks.get(partition).unwrap();
                    let offset = match self.start_offset {
                        KafkaEnumeratorOffset::Earliest => low_watermark - 1,
                        KafkaEnumeratorOffset::Latest => high_watermark - 1,
                        _ => unreachable!(),
                    };
                    map.insert(*partition, Some(offset));
                }
                Ok(map)
            }
            KafkaEnumeratorOffset::Timestamp(time) => {
                self.fetch_offset_for_time(partitions, time).await
            }
            KafkaEnumeratorOffset::None => partitions
                .iter()
                .map(|partition| Ok((*partition, None)))
                .collect(),
        }
    }

    async fn fetch_offset_for_time(
        &self,
        partitions: &[i32],
        time: i64,
    ) -> KafkaResult<HashMap<i32, Option<i64>>> {
        let mut tpl = TopicPartitionList::new();

        for partition in partitions {
            tpl.add_partition_offset(self.topic.as_str(), *partition, Offset::Offset(time))?;
        }

        let offsets = self
            .client
            .offsets_for_times(tpl, self.sync_call_timeout)
            .await?;

        let mut result = HashMap::with_capacity(partitions.len());

        for elem in offsets.elements_for_topic(self.topic.as_str()) {
            match elem.offset() {
                Offset::Offset(offset) => {
                    // XXX(rc): currently in RW source, `offset` means the last consumed offset, so we need to subtract 1
                    result.insert(elem.partition(), Some(offset - 1));
                }
                _ => {
                    let (_, high_watermark) = self
                        .client
                        .fetch_watermarks(
                            self.topic.as_str(),
                            elem.partition(),
                            self.sync_call_timeout,
                        )
                        .await?;
                    result.insert(elem.partition(), Some(high_watermark));
                }
            }
        }

        Ok(result)
    }

    #[inline]
    fn report_high_watermark(&self, partition: i32, offset: i64) {
        self.context
            .metrics
            .high_watermark
            .with_guarded_label_values(&[
                &self.context.info.source_id.to_string(),
                &partition.to_string(),
            ])
            .set(offset);
    }

    pub async fn check_reachability(&self) -> bool {
        self.client
            .fetch_metadata(None, self.sync_call_timeout)
            .await
            .is_ok()
    }

    async fn fetch_topic_partition(&self) -> ConnectorResult<Vec<i32>> {
        // for now, we only support one topic
        let metadata = self
            .client
            .fetch_metadata(Some(self.topic.as_str()), self.sync_call_timeout)
            .await?;

        let topic_meta = match metadata.topics() {
            [meta] => meta,
            _ => bail!("topic {} not found", self.topic),
        };

        if topic_meta.partitions().is_empty() {
            bail!("topic {} not found", self.topic);
        }

        Ok(topic_meta
            .partitions()
            .iter()
            .map(|partition| partition.id())
            .collect())
    }
}
