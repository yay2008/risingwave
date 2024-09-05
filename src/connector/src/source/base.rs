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
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use aws_sdk_s3::types::Object;
use bytes::Bytes;
use enum_as_inner::EnumAsInner;
use futures::stream::BoxStream;
use futures::Stream;
use itertools::Itertools;
use risingwave_common::array::StreamChunk;
use risingwave_common::bail;
use risingwave_common::catalog::TableId;
use risingwave_common::secret::LocalSecretManager;
use risingwave_common::types::{JsonbVal, Scalar};
use risingwave_pb::catalog::{PbSource, PbStreamSourceInfo};
use risingwave_pb::plan_common::ExternalTableDesc;
use risingwave_pb::source::ConnectorSplit;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use super::cdc::DebeziumCdcMeta;
use super::datagen::DatagenMeta;
use super::google_pubsub::GooglePubsubMeta;
use super::kafka::KafkaMeta;
use super::kinesis::KinesisMeta;
use super::monitor::SourceMetrics;
use super::nexmark::source::message::NexmarkMeta;
use super::{AZBLOB_CONNECTOR, GCS_CONNECTOR, OPENDAL_S3_CONNECTOR, POSIX_FS_CONNECTOR};
use crate::error::ConnectorResult as Result;
use crate::parser::schema_change::SchemaChangeEnvelope;
use crate::parser::ParserConfig;
use crate::source::filesystem::FsPageItem;
use crate::source::monitor::EnumeratorMetrics;
use crate::source::SplitImpl::{CitusCdc, MongodbCdc, MysqlCdc, PostgresCdc, SqlServerCdc};
use crate::with_options::WithOptions;
use crate::{
    dispatch_source_prop, dispatch_split_impl, for_all_sources, impl_connector_properties,
    impl_split, match_source_name_str, WithOptionsSecResolved,
};

const SPLIT_TYPE_FIELD: &str = "split_type";
const SPLIT_INFO_FIELD: &str = "split_info";
pub const UPSTREAM_SOURCE_KEY: &str = "connector";

pub trait TryFromBTreeMap: Sized + UnknownFields {
    /// Used to initialize the source properties from the raw untyped `WITH` options.
    fn try_from_btreemap(
        props: BTreeMap<String, String>,
        deny_unknown_fields: bool,
    ) -> Result<Self>;
}

/// Represents `WITH` options for sources.
///
/// Each instance should add a `#[derive(with_options::WithOptions)]` marker.
pub trait SourceProperties: TryFromBTreeMap + Clone + WithOptions {
    const SOURCE_NAME: &'static str;
    type Split: SplitMetaData
        + TryFrom<SplitImpl, Error = crate::error::ConnectorError>
        + Into<SplitImpl>;
    type SplitEnumerator: SplitEnumerator<Properties = Self, Split = Self::Split>;
    type SplitReader: SplitReader<Split = Self::Split, Properties = Self>;

    /// Load additional info from `PbSource`. Currently only used by CDC.
    fn init_from_pb_source(&mut self, _source: &PbSource) {}

    /// Load additional info from `ExternalTableDesc`. Currently only used by CDC.
    fn init_from_pb_cdc_table_desc(&mut self, _table_desc: &ExternalTableDesc) {}
}

pub trait UnknownFields {
    /// Unrecognized fields in the `WITH` clause.
    fn unknown_fields(&self) -> HashMap<String, String>;
}

impl<P: DeserializeOwned + UnknownFields> TryFromBTreeMap for P {
    fn try_from_btreemap(
        props: BTreeMap<String, String>,
        deny_unknown_fields: bool,
    ) -> Result<Self> {
        let json_value = serde_json::to_value(props)?;
        let res = serde_json::from_value::<P>(json_value)?;

        if !deny_unknown_fields || res.unknown_fields().is_empty() {
            Ok(res)
        } else {
            bail!(
                "Unknown fields in the WITH clause: {:?}",
                res.unknown_fields()
            )
        }
    }
}

pub async fn create_split_reader<P: SourceProperties + std::fmt::Debug>(
    prop: P,
    splits: Vec<SplitImpl>,
    parser_config: ParserConfig,
    source_ctx: SourceContextRef,
    columns: Option<Vec<Column>>,
) -> Result<P::SplitReader> {
    let splits = splits.into_iter().map(P::Split::try_from).try_collect()?;
    P::SplitReader::new(prop, splits, parser_config, source_ctx, columns).await
}

/// [`SplitEnumerator`] fetches the split metadata from the external source service.
/// NOTE: It runs in the meta server, so probably it should be moved to the `meta` crate.
#[async_trait]
pub trait SplitEnumerator: Sized {
    type Split: SplitMetaData + Send;
    type Properties;

    async fn new(properties: Self::Properties, context: SourceEnumeratorContextRef)
        -> Result<Self>;
    async fn list_splits(&mut self) -> Result<Vec<Self::Split>>;
}

pub type SourceContextRef = Arc<SourceContext>;
pub type SourceEnumeratorContextRef = Arc<SourceEnumeratorContext>;

/// The max size of a chunk yielded by source stream.
pub const MAX_CHUNK_SIZE: usize = 1024;

#[derive(Debug, Clone)]
pub struct SourceCtrlOpts {
    /// The max size of a chunk yielded by source stream.
    pub chunk_size: usize,
    /// Rate limit of source
    pub rate_limit: Option<u32>,
}

// The options in `SourceCtrlOpts` are so important that we don't want to impl `Default` for it,
// so that we can prevent any unintentional use of the default value.
impl !Default for SourceCtrlOpts {}

#[derive(Debug)]
pub struct SourceEnumeratorContext {
    pub info: SourceEnumeratorInfo,
    pub metrics: Arc<EnumeratorMetrics>,
}

impl SourceEnumeratorContext {
    /// Create a dummy `SourceEnumeratorContext` for testing purpose, or for the situation
    /// where the real context doesn't matter.
    pub fn dummy() -> SourceEnumeratorContext {
        SourceEnumeratorContext {
            info: SourceEnumeratorInfo { source_id: 0 },
            metrics: Arc::new(EnumeratorMetrics::default()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SourceEnumeratorInfo {
    pub source_id: u32,
}

#[derive(Debug)]
pub struct SourceContext {
    pub actor_id: u32,
    pub source_id: TableId,
    pub fragment_id: u32,
    pub source_name: String,
    pub metrics: Arc<SourceMetrics>,
    pub source_ctrl_opts: SourceCtrlOpts,
    pub connector_props: ConnectorProperties,
    // source parser put schema change event into this channel
    pub schema_change_tx:
        Option<mpsc::Sender<(SchemaChangeEnvelope, tokio::sync::oneshot::Sender<()>)>>,
}

impl SourceContext {
    pub fn new(
        actor_id: u32,
        source_id: TableId,
        fragment_id: u32,
        source_name: String,
        metrics: Arc<SourceMetrics>,
        source_ctrl_opts: SourceCtrlOpts,
        connector_props: ConnectorProperties,
        schema_change_channel: Option<
            mpsc::Sender<(SchemaChangeEnvelope, tokio::sync::oneshot::Sender<()>)>,
        >,
    ) -> Self {
        Self {
            actor_id,
            source_id,
            fragment_id,
            source_name,
            metrics,
            source_ctrl_opts,
            connector_props,
            schema_change_tx: schema_change_channel,
        }
    }

    /// Create a dummy `SourceContext` for testing purpose, or for the situation
    /// where the real context doesn't matter.
    pub fn dummy() -> Self {
        Self::new(
            0,
            TableId::new(0),
            0,
            "dummy".to_string(),
            Arc::new(SourceMetrics::default()),
            SourceCtrlOpts {
                chunk_size: MAX_CHUNK_SIZE,
                rate_limit: None,
            },
            ConnectorProperties::default(),
            None,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SourceFormat {
    #[default]
    Invalid,
    Native,
    None,
    Debezium,
    DebeziumMongo,
    Maxwell,
    Canal,
    Upsert,
    Plain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SourceEncode {
    #[default]
    Invalid,
    Native,
    None,
    Avro,
    Csv,
    Protobuf,
    Json,
    Bytes,
    Parquet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SourceStruct {
    pub format: SourceFormat,
    pub encode: SourceEncode,
}

impl SourceStruct {
    pub fn new(format: SourceFormat, encode: SourceEncode) -> Self {
        Self { format, encode }
    }
}

// Only return valid (format, encode)
pub fn extract_source_struct(info: &PbStreamSourceInfo) -> Result<SourceStruct> {
    use risingwave_pb::plan_common::{PbEncodeType, PbFormatType, RowFormatType};

    // old version meta.
    if let Ok(format) = info.get_row_format() {
        let (format, encode) = match format {
            RowFormatType::Json => (SourceFormat::Plain, SourceEncode::Json),
            RowFormatType::Protobuf => (SourceFormat::Plain, SourceEncode::Protobuf),
            RowFormatType::DebeziumJson => (SourceFormat::Debezium, SourceEncode::Json),
            RowFormatType::Avro => (SourceFormat::Plain, SourceEncode::Avro),
            RowFormatType::Maxwell => (SourceFormat::Maxwell, SourceEncode::Json),
            RowFormatType::CanalJson => (SourceFormat::Canal, SourceEncode::Json),
            RowFormatType::Csv => (SourceFormat::Plain, SourceEncode::Csv),
            RowFormatType::Native => (SourceFormat::Native, SourceEncode::Native),
            RowFormatType::DebeziumAvro => (SourceFormat::Debezium, SourceEncode::Avro),
            RowFormatType::UpsertJson => (SourceFormat::Upsert, SourceEncode::Json),
            RowFormatType::UpsertAvro => (SourceFormat::Upsert, SourceEncode::Avro),
            RowFormatType::DebeziumMongoJson => (SourceFormat::DebeziumMongo, SourceEncode::Json),
            RowFormatType::Bytes => (SourceFormat::Plain, SourceEncode::Bytes),
            RowFormatType::RowUnspecified => unreachable!(),
        };
        return Ok(SourceStruct::new(format, encode));
    }
    let source_format = info.get_format()?;
    let source_encode = info.get_row_encode()?;
    let (format, encode) = match (source_format, source_encode) {
        (PbFormatType::Plain, PbEncodeType::Json) => (SourceFormat::Plain, SourceEncode::Json),
        (PbFormatType::Plain, PbEncodeType::Protobuf) => {
            (SourceFormat::Plain, SourceEncode::Protobuf)
        }
        (PbFormatType::Debezium, PbEncodeType::Json) => {
            (SourceFormat::Debezium, SourceEncode::Json)
        }
        (PbFormatType::Plain, PbEncodeType::Avro) => (SourceFormat::Plain, SourceEncode::Avro),
        (PbFormatType::Maxwell, PbEncodeType::Json) => (SourceFormat::Maxwell, SourceEncode::Json),
        (PbFormatType::Canal, PbEncodeType::Json) => (SourceFormat::Canal, SourceEncode::Json),
        (PbFormatType::Plain, PbEncodeType::Csv) => (SourceFormat::Plain, SourceEncode::Csv),
        (PbFormatType::Plain, PbEncodeType::Parquet) => {
            (SourceFormat::Plain, SourceEncode::Parquet)
        }
        (PbFormatType::Native, PbEncodeType::Native) => {
            (SourceFormat::Native, SourceEncode::Native)
        }
        (PbFormatType::None, PbEncodeType::None) => (SourceFormat::None, SourceEncode::None),
        (PbFormatType::Debezium, PbEncodeType::Avro) => {
            (SourceFormat::Debezium, SourceEncode::Avro)
        }
        (PbFormatType::Upsert, PbEncodeType::Json) => (SourceFormat::Upsert, SourceEncode::Json),
        (PbFormatType::Upsert, PbEncodeType::Avro) => (SourceFormat::Upsert, SourceEncode::Avro),
        (PbFormatType::DebeziumMongo, PbEncodeType::Json) => {
            (SourceFormat::DebeziumMongo, SourceEncode::Json)
        }
        (PbFormatType::Plain, PbEncodeType::Bytes) => (SourceFormat::Plain, SourceEncode::Bytes),
        (PbFormatType::Upsert, PbEncodeType::Protobuf) => {
            (SourceFormat::Upsert, SourceEncode::Protobuf)
        }
        (format, encode) => {
            bail!(
                "Unsupported combination of format {:?} and encode {:?}",
                format,
                encode
            );
        }
    };
    Ok(SourceStruct::new(format, encode))
}

/// Stream of [`SourceMessage`].
pub type BoxSourceStream = BoxStream<'static, crate::error::ConnectorResult<Vec<SourceMessage>>>;

// Manually expand the trait alias to improve IDE experience.
pub trait ChunkSourceStream:
    Stream<Item = crate::error::ConnectorResult<StreamChunk>> + Send + 'static
{
}
impl<T> ChunkSourceStream for T where
    T: Stream<Item = crate::error::ConnectorResult<StreamChunk>> + Send + 'static
{
}

pub type BoxChunkSourceStream = BoxStream<'static, crate::error::ConnectorResult<StreamChunk>>;
pub type BoxTryStream<M> = BoxStream<'static, crate::error::ConnectorResult<M>>;

/// [`SplitReader`] is a new abstraction of the external connector read interface which is
/// responsible for parsing, it is used to read messages from the outside and transform them into a
/// stream of parsed [`StreamChunk`]
#[async_trait]
pub trait SplitReader: Sized + Send {
    type Properties;
    type Split: SplitMetaData;

    async fn new(
        properties: Self::Properties,
        state: Vec<Self::Split>,
        parser_config: ParserConfig,
        source_ctx: SourceContextRef,
        columns: Option<Vec<Column>>,
    ) -> crate::error::ConnectorResult<Self>;

    fn into_stream(self) -> BoxChunkSourceStream;

    fn backfill_info(&self) -> HashMap<SplitId, BackfillInfo> {
        HashMap::new()
    }
}

/// Information used to determine whether we should start and finish source backfill.
///
/// XXX: if a connector cannot provide the latest offsets (but we want to make it shareable),
/// perhaps we should ban blocking DDL for it.
#[derive(Debug, Clone)]
pub enum BackfillInfo {
    HasDataToBackfill {
        /// The last available offsets for each split (**inclusive**).
        ///
        /// This will be used to determine whether source backfill is finished when
        /// there are no _new_ messages coming from upstream `SourceExecutor`. Otherwise,
        /// blocking DDL cannot finish until new messages come.
        ///
        /// When there are upstream messages, we will use the latest offsets from the upstream.
        latest_offset: String,
    },
    /// If there are no messages in the split at all, we don't need to start backfill.
    /// In this case, there will be no message from the backfill stream too.
    /// If we started backfill, we cannot finish it until new messages come.
    /// So we mark this a special case for optimization.
    NoDataToBackfill,
}

for_all_sources!(impl_connector_properties);

impl Default for ConnectorProperties {
    fn default() -> Self {
        ConnectorProperties::Test(Box::default())
    }
}

impl ConnectorProperties {
    pub fn is_new_fs_connector_hash_map(with_properties: &HashMap<String, String>) -> bool {
        with_properties
            .get(UPSTREAM_SOURCE_KEY)
            .map(|s| {
                s.eq_ignore_ascii_case(OPENDAL_S3_CONNECTOR)
                    || s.eq_ignore_ascii_case(POSIX_FS_CONNECTOR)
                    || s.eq_ignore_ascii_case(GCS_CONNECTOR)
                    || s.eq_ignore_ascii_case(AZBLOB_CONNECTOR)
            })
            .unwrap_or(false)
    }
}

impl ConnectorProperties {
    /// Creates typed source properties from the raw `WITH` properties.
    ///
    /// It checks the `connector` field, and them dispatches to the corresponding type's `try_from_btreemap` method.
    ///
    /// `deny_unknown_fields`: Since `WITH` options are persisted in meta, we do not deny unknown fields when restoring from
    /// existing data to avoid breaking backwards compatibility. We only deny unknown fields when creating new sources.
    pub fn extract(
        with_properties: WithOptionsSecResolved,
        deny_unknown_fields: bool,
    ) -> Result<Self> {
        let (options, secret_refs) = with_properties.into_parts();
        let mut options_with_secret =
            LocalSecretManager::global().fill_secrets(options, secret_refs)?;
        let connector = options_with_secret
            .remove(UPSTREAM_SOURCE_KEY)
            .ok_or_else(|| anyhow!("Must specify 'connector' in WITH clause"))?;
        match_source_name_str!(
            connector.to_lowercase().as_str(),
            PropType,
            PropType::try_from_btreemap(options_with_secret, deny_unknown_fields)
                .map(ConnectorProperties::from),
            |other| bail!("connector '{}' is not supported", other)
        )
    }

    pub fn enable_split_scale_in(&self) -> bool {
        // enable split scale in just for Kinesis
        matches!(self, ConnectorProperties::Kinesis(_))
    }

    /// Load additional info from `PbSource`. Currently only used by CDC.
    pub fn init_from_pb_source(&mut self, source: &PbSource) {
        dispatch_source_prop!(self, prop, prop.init_from_pb_source(source))
    }

    /// Load additional info from `ExternalTableDesc`. Currently only used by CDC.
    pub fn init_from_pb_cdc_table_desc(&mut self, cdc_table_desc: &ExternalTableDesc) {
        dispatch_source_prop!(self, prop, prop.init_from_pb_cdc_table_desc(cdc_table_desc))
    }

    pub fn support_multiple_splits(&self) -> bool {
        matches!(self, ConnectorProperties::Kafka(_))
            || matches!(self, ConnectorProperties::OpendalS3(_))
            || matches!(self, ConnectorProperties::Gcs(_))
            || matches!(self, ConnectorProperties::Azblob(_))
    }
}

for_all_sources!(impl_split);

impl From<&SplitImpl> for ConnectorSplit {
    fn from(split: &SplitImpl) -> Self {
        dispatch_split_impl!(split, inner, SourcePropType, {
            ConnectorSplit {
                split_type: String::from(SourcePropType::SOURCE_NAME),
                encoded_split: inner.encode_to_bytes().to_vec(),
            }
        })
    }
}

impl TryFrom<&ConnectorSplit> for SplitImpl {
    type Error = crate::error::ConnectorError;

    fn try_from(split: &ConnectorSplit) -> std::result::Result<Self, Self::Error> {
        match_source_name_str!(
            split.split_type.to_lowercase().as_str(),
            PropType,
            {
                <PropType as SourceProperties>::Split::restore_from_bytes(
                    split.encoded_split.as_ref(),
                )
                .map(Into::into)
            },
            |other| bail!("connector '{}' is not supported", other)
        )
    }
}

impl SplitImpl {
    fn restore_from_json_inner(split_type: &str, value: JsonbVal) -> Result<Self> {
        match_source_name_str!(
            split_type.to_lowercase().as_str(),
            PropType,
            <PropType as SourceProperties>::Split::restore_from_json(value).map(Into::into),
            |other| bail!("connector '{}' is not supported", other)
        )
    }

    pub fn is_cdc_split(&self) -> bool {
        matches!(
            self,
            MysqlCdc(_) | PostgresCdc(_) | MongodbCdc(_) | CitusCdc(_) | SqlServerCdc(_)
        )
    }

    /// Get the current split offset.
    pub fn get_cdc_split_offset(&self) -> String {
        match self {
            MysqlCdc(split) => split.start_offset().clone().unwrap_or_default(),
            PostgresCdc(split) => split.start_offset().clone().unwrap_or_default(),
            MongodbCdc(split) => split.start_offset().clone().unwrap_or_default(),
            CitusCdc(split) => split.start_offset().clone().unwrap_or_default(),
            SqlServerCdc(split) => split.start_offset().clone().unwrap_or_default(),
            _ => unreachable!("get_cdc_split_offset() is only for cdc split"),
        }
    }
}

impl SplitMetaData for SplitImpl {
    fn id(&self) -> SplitId {
        dispatch_split_impl!(self, inner, IgnoreType, inner.id())
    }

    fn encode_to_json(&self) -> JsonbVal {
        use serde_json::json;
        let inner = self.encode_to_json_inner().take();
        json!({ SPLIT_TYPE_FIELD: self.get_type(), SPLIT_INFO_FIELD: inner}).into()
    }

    fn restore_from_json(value: JsonbVal) -> Result<Self> {
        let mut value = value.take();
        let json_obj = value.as_object_mut().unwrap();
        let split_type = json_obj
            .remove(SPLIT_TYPE_FIELD)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let inner_value = json_obj.remove(SPLIT_INFO_FIELD).unwrap();
        Self::restore_from_json_inner(&split_type, inner_value.into())
    }

    fn update_offset(&mut self, last_seen_offset: String) -> Result<()> {
        dispatch_split_impl!(
            self,
            inner,
            IgnoreType,
            inner.update_offset(last_seen_offset)
        )
    }
}

impl SplitImpl {
    pub fn get_type(&self) -> String {
        dispatch_split_impl!(self, _ignored, PropType, {
            PropType::SOURCE_NAME.to_string()
        })
    }

    pub fn update_in_place(&mut self, last_seen_offset: String) -> Result<()> {
        dispatch_split_impl!(self, inner, IgnoreType, {
            inner.update_offset(last_seen_offset)?
        });
        Ok(())
    }

    pub fn encode_to_json_inner(&self) -> JsonbVal {
        dispatch_split_impl!(self, inner, IgnoreType, inner.encode_to_json())
    }
}

pub type DataType = risingwave_common::types::DataType;

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    /// This field is only used by datagen.
    pub is_visible: bool,
}

/// Split id resides in every source message, use `Arc` to avoid copying.
pub type SplitId = Arc<str>;

/// The message pumped from the external source service.
/// The third-party message structs will eventually be transformed into this struct.
#[derive(Debug, Clone)]
pub struct SourceMessage {
    pub key: Option<Vec<u8>>,
    pub payload: Option<Vec<u8>>,
    pub offset: String, // TODO: use `Arc<str>`
    pub split_id: SplitId,
    pub meta: SourceMeta,
}

impl SourceMessage {
    /// Create a dummy `SourceMessage` with all fields unset for testing purposes.
    pub fn dummy() -> Self {
        Self {
            key: None,
            payload: None,
            offset: "".to_string(),
            split_id: "".into(),
            meta: SourceMeta::Empty,
        }
    }
}

#[derive(Debug, Clone)]
pub enum SourceMeta {
    Kafka(KafkaMeta),
    Kinesis(KinesisMeta),
    Nexmark(NexmarkMeta),
    GooglePubsub(GooglePubsubMeta),
    Datagen(DatagenMeta),
    DebeziumCdc(DebeziumCdcMeta),
    // For the source that doesn't have meta data.
    Empty,
}

/// Implement Eq manually to ignore the `meta` field.
impl PartialEq for SourceMessage {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
            && self.split_id == other.split_id
            && self.payload == other.payload
    }
}
impl Eq for SourceMessage {}

/// The metadata of a split.
pub trait SplitMetaData: Sized {
    fn id(&self) -> SplitId;
    fn encode_to_bytes(&self) -> Bytes {
        self.encode_to_json()
            .as_scalar_ref()
            .value_serialize()
            .into()
    }
    fn restore_from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::restore_from_json(JsonbVal::value_deserialize(bytes).unwrap())
    }

    /// Encode the whole split metadata to a JSON object
    fn encode_to_json(&self) -> JsonbVal;
    fn restore_from_json(value: JsonbVal) -> Result<Self>;
    fn update_offset(&mut self, last_seen_offset: String) -> crate::error::ConnectorResult<()>;
}

/// [`ConnectorState`] maintains the consuming splits' info. In specific split readers,
/// `ConnectorState` cannot be [`None`] and contains one(for mq split readers) or many(for fs
/// split readers) [`SplitImpl`]. If no split is assigned to source executor, `ConnectorState` is
/// [`None`] and the created source stream will be a pending stream.
pub type ConnectorState = Option<Vec<SplitImpl>>;

#[derive(Debug, Clone, Default)]
pub struct FsFilterCtrlCtx;
pub type FsFilterCtrlCtxRef = Arc<FsFilterCtrlCtx>;

#[async_trait]
pub trait FsListInner: Sized {
    // fixme: better to implement as an Iterator, but the last page still have some contents
    async fn get_next_page<T: for<'a> From<&'a Object>>(&mut self) -> Result<(Vec<T>, bool)>;
    fn filter_policy(&self, ctx: &FsFilterCtrlCtx, page_num: usize, item: &FsPageItem) -> bool;
}

#[cfg(test)]
mod tests {
    use maplit::*;
    use nexmark::event::EventType;

    use super::*;
    use crate::source::cdc::{DebeziumCdcSplit, Mysql};
    use crate::source::kafka::KafkaSplit;

    #[test]
    fn test_split_impl_get_fn() -> Result<()> {
        let split = KafkaSplit::new(0, Some(0), Some(0), "demo".to_string());
        let split_impl = SplitImpl::Kafka(split.clone());
        let get_value = split_impl.into_kafka().unwrap();
        println!("{:?}", get_value);
        assert_eq!(split.encode_to_bytes(), get_value.encode_to_bytes());
        assert_eq!(split.encode_to_json(), get_value.encode_to_json());

        Ok(())
    }

    #[test]
    fn test_cdc_split_state() -> Result<()> {
        let offset_str = "{\"sourcePartition\":{\"server\":\"RW_CDC_mydb.products\"},\"sourceOffset\":{\"transaction_id\":null,\"ts_sec\":1670407377,\"file\":\"binlog.000001\",\"pos\":98587,\"row\":2,\"server_id\":1,\"event\":2}}";
        let split = DebeziumCdcSplit::<Mysql>::new(1001, Some(offset_str.to_string()), None);
        let split_impl = SplitImpl::MysqlCdc(split);
        let encoded_split = split_impl.encode_to_bytes();
        let restored_split_impl = SplitImpl::restore_from_bytes(encoded_split.as_ref())?;
        assert_eq!(
            split_impl.encode_to_bytes(),
            restored_split_impl.encode_to_bytes()
        );
        assert_eq!(
            split_impl.encode_to_json(),
            restored_split_impl.encode_to_json()
        );

        let encoded_split = split_impl.encode_to_json();
        let restored_split_impl = SplitImpl::restore_from_json(encoded_split)?;
        assert_eq!(
            split_impl.encode_to_bytes(),
            restored_split_impl.encode_to_bytes()
        );
        assert_eq!(
            split_impl.encode_to_json(),
            restored_split_impl.encode_to_json()
        );
        Ok(())
    }

    #[test]
    fn test_extract_nexmark_config() {
        let props = convert_args!(btreemap!(
            "connector" => "nexmark",
            "nexmark.table.type" => "Person",
            "nexmark.split.num" => "1",
        ));

        let props =
            ConnectorProperties::extract(WithOptionsSecResolved::without_secrets(props), true)
                .unwrap();

        if let ConnectorProperties::Nexmark(props) = props {
            assert_eq!(props.table_type, Some(EventType::Person));
            assert_eq!(props.split_num, 1);
        } else {
            panic!("extract nexmark config failed");
        }
    }

    #[test]
    fn test_extract_kafka_config() {
        let props = convert_args!(btreemap!(
            "connector" => "kafka",
            "properties.bootstrap.server" => "b1,b2",
            "topic" => "test",
            "scan.startup.mode" => "earliest",
            "broker.rewrite.endpoints" => r#"{"b-1:9092":"dns-1", "b-2:9092":"dns-2"}"#,
        ));

        let props =
            ConnectorProperties::extract(WithOptionsSecResolved::without_secrets(props), true)
                .unwrap();
        if let ConnectorProperties::Kafka(k) = props {
            let btreemap = btreemap! {
                "b-1:9092".to_string() => "dns-1".to_string(),
                "b-2:9092".to_string() => "dns-2".to_string(),
            };
            assert_eq!(k.privatelink_common.broker_rewrite_map, Some(btreemap));
        } else {
            panic!("extract kafka config failed");
        }
    }

    #[test]
    fn test_extract_cdc_properties() {
        let user_props_mysql = convert_args!(btreemap!(
            "connector" => "mysql-cdc",
            "database.hostname" => "127.0.0.1",
            "database.port" => "3306",
            "database.user" => "root",
            "database.password" => "123456",
            "database.name" => "mydb",
            "table.name" => "products",
        ));

        let user_props_postgres = convert_args!(btreemap!(
            "connector" => "postgres-cdc",
            "database.hostname" => "127.0.0.1",
            "database.port" => "5432",
            "database.user" => "root",
            "database.password" => "654321",
            "schema.name" => "public",
            "database.name" => "mypgdb",
            "table.name" => "orders",
        ));

        let conn_props = ConnectorProperties::extract(
            WithOptionsSecResolved::without_secrets(user_props_mysql),
            true,
        )
        .unwrap();
        if let ConnectorProperties::MysqlCdc(c) = conn_props {
            assert_eq!(c.properties.get("database.hostname").unwrap(), "127.0.0.1");
            assert_eq!(c.properties.get("database.port").unwrap(), "3306");
            assert_eq!(c.properties.get("database.user").unwrap(), "root");
            assert_eq!(c.properties.get("database.password").unwrap(), "123456");
            assert_eq!(c.properties.get("database.name").unwrap(), "mydb");
            assert_eq!(c.properties.get("table.name").unwrap(), "products");
        } else {
            panic!("extract cdc config failed");
        }

        let conn_props = ConnectorProperties::extract(
            WithOptionsSecResolved::without_secrets(user_props_postgres),
            true,
        )
        .unwrap();
        if let ConnectorProperties::PostgresCdc(c) = conn_props {
            assert_eq!(c.properties.get("database.hostname").unwrap(), "127.0.0.1");
            assert_eq!(c.properties.get("database.port").unwrap(), "5432");
            assert_eq!(c.properties.get("database.user").unwrap(), "root");
            assert_eq!(c.properties.get("database.password").unwrap(), "654321");
            assert_eq!(c.properties.get("schema.name").unwrap(), "public");
            assert_eq!(c.properties.get("database.name").unwrap(), "mypgdb");
            assert_eq!(c.properties.get("table.name").unwrap(), "orders");
        } else {
            panic!("extract cdc config failed");
        }
    }
}
