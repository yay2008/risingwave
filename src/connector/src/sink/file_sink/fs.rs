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
use std::collections::{BTreeMap, HashMap};

use anyhow::anyhow;
use opendal::layers::{LoggingLayer, RetryLayer};
use opendal::services::Fs;
use opendal::Operator;
use serde::Deserialize;
use serde_with::serde_as;
use with_options::WithOptions;

use super::opendal_sink::{BatchingStrategy, FileSinkBatchingStrategy};
use crate::sink::file_sink::opendal_sink::{FileSink, OpendalSinkBackend};
use crate::sink::{Result, SinkError, SINK_TYPE_APPEND_ONLY, SINK_TYPE_OPTION, SINK_TYPE_UPSERT};
use crate::source::UnknownFields;

#[derive(Deserialize, Debug, Clone, WithOptions)]
pub struct FsCommon {
    /// The directory where the sink file is located.
    #[serde(rename = "fs.path")]
    pub path: String,
}

#[serde_as]
#[derive(Clone, Debug, Deserialize, WithOptions)]
pub struct FsConfig {
    #[serde(flatten)]
    pub common: FsCommon,
    #[serde(flatten)]
    pub batching_strategy: FileSinkBatchingStrategy,

    pub r#type: String, // accept "append-only"

    #[serde(flatten)]
    pub unknown_fields: HashMap<String, String>,
}

impl UnknownFields for FsConfig {
    fn unknown_fields(&self) -> HashMap<String, String> {
        self.unknown_fields.clone()
    }
}

pub const FS_SINK: &str = "fs";

impl<S: OpendalSinkBackend> FileSink<S> {
    pub fn new_fs_sink(config: FsConfig) -> Result<Operator> {
        // Create fs builder.
        let mut builder = Fs::default();
        // Create fs backend builder.
        builder.root(&config.common.path);
        let operator: Operator = Operator::new(builder)?
            .layer(LoggingLayer::default())
            .layer(RetryLayer::default())
            .finish();
        Ok(operator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsSink;

impl OpendalSinkBackend for FsSink {
    type Properties = FsConfig;

    const SINK_NAME: &'static str = FS_SINK;

    fn from_btreemap(btree_map: BTreeMap<String, String>) -> Result<Self::Properties> {
        let config = serde_json::from_value::<FsConfig>(serde_json::to_value(btree_map).unwrap())
            .map_err(|e| SinkError::Config(anyhow!(e)))?;
        if config.r#type != SINK_TYPE_APPEND_ONLY && config.r#type != SINK_TYPE_UPSERT {
            return Err(SinkError::Config(anyhow!(
                "`{}` must be {}, or {}",
                SINK_TYPE_OPTION,
                SINK_TYPE_APPEND_ONLY,
                SINK_TYPE_UPSERT
            )));
        }
        Ok(config)
    }

    fn new_operator(properties: FsConfig) -> Result<Operator> {
        FileSink::<FsSink>::new_fs_sink(properties)
    }

    fn get_path(properties: Self::Properties) -> String {
        properties.common.path
    }

    fn get_engine_type() -> super::opendal_sink::EngineType {
        super::opendal_sink::EngineType::Fs
    }

    fn get_batching_strategy(properties: Self::Properties) -> Option<BatchingStrategy> {
        //     && properties.batching_strategy.inactivity_interval.is_none()
        if properties.batching_strategy.max_row_count.is_none()
            && properties.batching_strategy.max_file_size.is_none()
        {
            return None;
        }

        Some(BatchingStrategy {
            // batching_interval: properties
            //     .batching_strategy
            //     .batching_interval
            //     .map(|s| parse_duration(&s))
            //     .transpose()
            //     .ok(),
            // inactivity_interval: properties
            //     .batching_strategy
            //     .inactivity_interval
            //     .map(|s| parse_duration(&s))
            //     .transpose()
            //     .ok(),
            max_row_count: properties
                .batching_strategy
                .max_row_count
                .and_then(|s| s.parse().ok()),
            max_file_size: properties.batching_strategy.max_file_size,
            rollover_seconds: properties
                .batching_strategy
                .rollover_seconds
                .and_then(|s| s.parse().ok()),
        })
    }
}
