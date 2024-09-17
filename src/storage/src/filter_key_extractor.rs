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

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;

use itertools::Itertools;
use parking_lot::RwLock;
use risingwave_common::catalog::ColumnDesc;
use risingwave_common::hash::VirtualNode;
use risingwave_common::util::row_serde::OrderedRowSerde;
use risingwave_common::util::sort_util::OrderType;
use risingwave_hummock_sdk::info_in_release;
use risingwave_hummock_sdk::key::{get_table_id, TABLE_PREFIX_LEN};
use risingwave_pb::catalog::Table;
use risingwave_rpc_client::error::{Result as RpcResult, RpcError};
use risingwave_rpc_client::MetaClient;
use thiserror_ext::AsReport;

use crate::hummock::{HummockError, HummockResult};

/// `FilterKeyExtractor` generally used to extract key which will store in BloomFilter
pub trait FilterKeyExtractor: Send + Sync {
    fn extract<'a>(&self, full_key: &'a [u8]) -> &'a [u8];
}

pub enum FilterKeyExtractorImpl {
    Schema(SchemaFilterKeyExtractor),
    FullKey(FullKeyFilterKeyExtractor),
    Dummy(DummyFilterKeyExtractor),
    Multi(MultiFilterKeyExtractor),
    FixedLength(FixedLengthFilterKeyExtractor),
}

impl FilterKeyExtractorImpl {
    pub fn from_table(table_catalog: &Table) -> Self {
        let read_prefix_len = table_catalog.get_read_prefix_len_hint() as usize;

        if read_prefix_len == 0 || read_prefix_len > table_catalog.get_pk().len() {
            // for now frontend had not infer the table_id_to_filter_key_extractor, so we
            // use FullKeyFilterKeyExtractor
            FilterKeyExtractorImpl::Dummy(DummyFilterKeyExtractor)
        } else {
            FilterKeyExtractorImpl::Schema(SchemaFilterKeyExtractor::new(table_catalog))
        }
    }
}

macro_rules! impl_filter_key_extractor {
    ($( { $variant_name:ident } ),*) => {
        impl FilterKeyExtractorImpl {
            pub fn extract<'a>(&self, full_key: &'a [u8]) -> &'a [u8]{
                match self {
                    $( Self::$variant_name(inner) => inner.extract(full_key), )*
                }
            }
        }
    }

}

macro_rules! for_all_filter_key_extractor_variants {
    ($macro:ident) => {
        $macro! {
            { Schema },
            { FullKey },
            { Dummy },
            { Multi },
            { FixedLength }
        }
    };
}

for_all_filter_key_extractor_variants! { impl_filter_key_extractor }

#[derive(Default)]
pub struct FullKeyFilterKeyExtractor;

impl FilterKeyExtractor for FullKeyFilterKeyExtractor {
    fn extract<'a>(&self, user_key: &'a [u8]) -> &'a [u8] {
        user_key
    }
}

#[derive(Default)]
pub struct DummyFilterKeyExtractor;
impl FilterKeyExtractor for DummyFilterKeyExtractor {
    fn extract<'a>(&self, _full_key: &'a [u8]) -> &'a [u8] {
        &[]
    }
}

/// [`SchemaFilterKeyExtractor`] build from `table_catalog` and extract a `full_key` to prefix for
#[derive(Default)]
pub struct FixedLengthFilterKeyExtractor {
    fixed_length: usize,
}

impl FilterKeyExtractor for FixedLengthFilterKeyExtractor {
    fn extract<'a>(&self, full_key: &'a [u8]) -> &'a [u8] {
        &full_key[0..self.fixed_length]
    }
}

impl FixedLengthFilterKeyExtractor {
    pub fn new(fixed_length: usize) -> Self {
        Self { fixed_length }
    }
}

/// [`SchemaFilterKeyExtractor`] build from `table_catalog` and transform a `full_key` to prefix for
/// `prefix_bloom_filter`
pub struct SchemaFilterKeyExtractor {
    /// Each stateful operator has its own read pattern, partly using prefix scan.
    /// Prefix key length can be decoded through its `DataType` and `OrderType` which obtained from
    /// `TableCatalog`. `read_pattern_prefix_column` means the count of column to decode prefix
    /// from storage key.
    read_prefix_len: usize,
    deserializer: OrderedRowSerde,
    // TODO:need some bench test for same prefix case like join (if we need a prefix_cache for same
    // prefix_key)
}

impl FilterKeyExtractor for SchemaFilterKeyExtractor {
    fn extract<'a>(&self, full_key: &'a [u8]) -> &'a [u8] {
        if full_key.len() < TABLE_PREFIX_LEN + VirtualNode::SIZE {
            return &[];
        }

        let (_table_prefix, key) = full_key.split_at(TABLE_PREFIX_LEN);
        let (_vnode_prefix, pk) = key.split_at(VirtualNode::SIZE);

        // if the key with table_id deserializer fail from schema, that should panic here for early
        // detection.

        let bloom_filter_key_len = self
            .deserializer
            .deserialize_prefix_len(pk, self.read_prefix_len)
            .unwrap();

        let end_position = TABLE_PREFIX_LEN + VirtualNode::SIZE + bloom_filter_key_len;
        &full_key[TABLE_PREFIX_LEN + VirtualNode::SIZE..end_position]
    }
}

impl SchemaFilterKeyExtractor {
    pub fn new(table_catalog: &Table) -> Self {
        let pk_indices: Vec<usize> = table_catalog
            .pk
            .iter()
            .map(|col_order| col_order.column_index as usize)
            .collect();

        let read_prefix_len = table_catalog.get_read_prefix_len_hint() as usize;

        let data_types = pk_indices
            .iter()
            .map(|column_idx| &table_catalog.columns[*column_idx])
            .map(|col| ColumnDesc::from(col.column_desc.as_ref().unwrap()).data_type)
            .collect();

        let order_types: Vec<OrderType> = table_catalog
            .pk
            .iter()
            .map(|col_order| OrderType::from_protobuf(col_order.get_order_type().unwrap()))
            .collect();

        Self {
            read_prefix_len,
            deserializer: OrderedRowSerde::new(data_types, order_types),
        }
    }
}

#[derive(Default)]
pub struct MultiFilterKeyExtractor {
    id_to_filter_key_extractor: HashMap<u32, Arc<FilterKeyExtractorImpl>>,
    // cached state
    // last_filter_key_extractor_state: Mutex<Option<(u32, Arc<FilterKeyExtractorImpl>)>>,
}

impl MultiFilterKeyExtractor {
    pub fn register(&mut self, table_id: u32, filter_key_extractor: Arc<FilterKeyExtractorImpl>) {
        self.id_to_filter_key_extractor
            .insert(table_id, filter_key_extractor);
    }

    pub fn size(&self) -> usize {
        self.id_to_filter_key_extractor.len()
    }

    pub fn get_existing_table_ids(&self) -> HashSet<u32> {
        self.id_to_filter_key_extractor.keys().cloned().collect()
    }
}

impl Debug for MultiFilterKeyExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MultiFilterKeyExtractor size {} ", self.size())
    }
}

impl FilterKeyExtractor for MultiFilterKeyExtractor {
    fn extract<'a>(&self, full_key: &'a [u8]) -> &'a [u8] {
        if full_key.len() < TABLE_PREFIX_LEN + VirtualNode::SIZE {
            return full_key;
        }

        let table_id = get_table_id(full_key);
        self.id_to_filter_key_extractor
            .get(&table_id)
            .unwrap()
            .extract(full_key)
    }
}

#[async_trait::async_trait]
pub trait StateTableAccessor: Send + Sync {
    async fn get_tables(&self, table_ids: &[u32]) -> RpcResult<HashMap<u32, Table>>;
}

#[derive(Default)]
pub struct FakeRemoteTableAccessor {}

pub struct RemoteTableAccessor {
    meta_client: MetaClient,
}

impl RemoteTableAccessor {
    pub fn new(meta_client: MetaClient) -> Self {
        Self { meta_client }
    }
}

#[async_trait::async_trait]
impl StateTableAccessor for RemoteTableAccessor {
    async fn get_tables(&self, table_ids: &[u32]) -> RpcResult<HashMap<u32, Table>> {
        self.meta_client.get_tables(table_ids).await
    }
}

#[async_trait::async_trait]
impl StateTableAccessor for FakeRemoteTableAccessor {
    async fn get_tables(&self, _table_ids: &[u32]) -> RpcResult<HashMap<u32, Table>> {
        Err(RpcError::Internal(anyhow::anyhow!(
            "fake accessor does not support fetch remote table"
        )))
    }
}
struct FilterKeyExtractorManagerInner {
    table_id_to_filter_key_extractor: RwLock<HashMap<u32, Arc<FilterKeyExtractorImpl>>>,
    table_accessor: Box<dyn StateTableAccessor>,
}

impl FilterKeyExtractorManagerInner {
    fn update(&self, table_id: u32, filter_key_extractor: Arc<FilterKeyExtractorImpl>) {
        self.table_id_to_filter_key_extractor
            .write()
            .insert(table_id, filter_key_extractor);
    }

    fn sync(&self, filter_key_extractor_map: HashMap<u32, Arc<FilterKeyExtractorImpl>>) {
        let mut guard = self.table_id_to_filter_key_extractor.write();
        guard.clear();
        guard.extend(filter_key_extractor_map);
    }

    fn remove(&self, table_id: u32) {
        self.table_id_to_filter_key_extractor
            .write()
            .remove(&table_id);
    }

    async fn acquire(
        &self,
        mut table_id_set: HashSet<u32>,
    ) -> HummockResult<FilterKeyExtractorImpl> {
        if table_id_set.is_empty() {
            // table_id_set is empty
            // the table in sst has been deleted

            // use full key as default
            return Ok(FilterKeyExtractorImpl::FullKey(FullKeyFilterKeyExtractor));
        }

        let mut multi_filter_key_extractor = MultiFilterKeyExtractor::default();
        {
            let guard = self.table_id_to_filter_key_extractor.read();
            table_id_set.retain(|table_id| match guard.get(table_id) {
                Some(filter_key_extractor) => {
                    multi_filter_key_extractor.register(*table_id, filter_key_extractor.clone());
                    false
                }

                None => true,
            });
        }

        if !table_id_set.is_empty() {
            let table_ids = table_id_set.iter().cloned().collect_vec();
            let mut state_tables =
                self.table_accessor
                    .get_tables(&table_ids)
                    .await
                    .map_err(|e| {
                        HummockError::other(format!(
                            "request rpc list_tables for meta failed: {}",
                            e.as_report()
                        ))
                    })?;
            let mut guard = self.table_id_to_filter_key_extractor.write();
            for table_id in table_ids {
                if let Some(table) = state_tables.remove(&table_id) {
                    let key_extractor = Arc::new(FilterKeyExtractorImpl::from_table(&table));
                    guard.insert(table_id, key_extractor.clone());
                    multi_filter_key_extractor.register(table_id, key_extractor);
                }
            }
        }

        Ok(FilterKeyExtractorImpl::Multi(multi_filter_key_extractor))
    }
}

/// `RpcFilterKeyExtractorManager` is a wrapper for inner, and provide a protected read and write
/// interface, its thread safe
pub struct RpcFilterKeyExtractorManager {
    inner: FilterKeyExtractorManagerInner,
}

impl Default for RpcFilterKeyExtractorManager {
    fn default() -> Self {
        Self::new(Box::<FakeRemoteTableAccessor>::default())
    }
}

impl RpcFilterKeyExtractorManager {
    pub fn new(table_accessor: Box<dyn StateTableAccessor>) -> Self {
        Self {
            inner: FilterKeyExtractorManagerInner {
                table_id_to_filter_key_extractor: Default::default(),
                table_accessor,
            },
        }
    }

    /// Insert (`table_id`, `filter_key_extractor`) as mapping to `HashMap` for `acquire`
    pub fn update(&self, table_id: u32, filter_key_extractor: Arc<FilterKeyExtractorImpl>) {
        info_in_release!("update key extractor of {}", table_id);
        self.inner.update(table_id, filter_key_extractor);
    }

    /// Remove a mapping by `table_id`
    pub fn remove(&self, table_id: u32) {
        info_in_release!("remove key extractor of {}", table_id);
        self.inner.remove(table_id);
    }

    /// Sync all filter key extractors by snapshot
    pub fn sync(&self, filter_key_extractor_map: HashMap<u32, Arc<FilterKeyExtractorImpl>>) {
        self.inner.sync(filter_key_extractor_map)
    }

    /// Acquire a `MultiFilterKeyExtractor` by `table_id_set`
    /// Internally, try to get all `filter_key_extractor` from `hashmap`. Will block the caller if
    /// `table_id` does not util version update (notify), and retry to get
    async fn acquire(&self, table_id_set: HashSet<u32>) -> HummockResult<FilterKeyExtractorImpl> {
        self.inner.acquire(table_id_set).await
    }
}

#[derive(Clone)]
pub enum FilterKeyExtractorManager {
    RpcFilterKeyExtractorManager(Arc<RpcFilterKeyExtractorManager>),
    StaticFilterKeyExtractorManager(Arc<StaticFilterKeyExtractorManager>),
}

impl FilterKeyExtractorManager {
    pub async fn acquire(
        &self,
        table_id_set: HashSet<u32>,
    ) -> HummockResult<FilterKeyExtractorImpl> {
        match self {
            FilterKeyExtractorManager::RpcFilterKeyExtractorManager(
                rpc_filter_key_exactor_manager,
            ) => rpc_filter_key_exactor_manager.acquire(table_id_set).await,
            FilterKeyExtractorManager::StaticFilterKeyExtractorManager(
                static_filter_key_extractor_manager,
            ) => static_filter_key_extractor_manager.acquire(table_id_set),
        }
    }
}

#[derive(Clone)]
pub struct StaticFilterKeyExtractorManager {
    id_to_table: HashMap<u32, Table>,
}

impl StaticFilterKeyExtractorManager {
    pub fn new(id_to_table: HashMap<u32, Table>) -> Self {
        Self { id_to_table }
    }

    fn acquire(&self, table_id_set: HashSet<u32>) -> HummockResult<FilterKeyExtractorImpl> {
        let mut multi_filter_key_extractor = MultiFilterKeyExtractor::default();
        for table_id in table_id_set {
            if let Some(table) = self.id_to_table.get(&table_id) {
                let key_extractor = Arc::new(FilterKeyExtractorImpl::from_table(table));
                multi_filter_key_extractor.register(table_id, key_extractor);
            } else {
                return Err(HummockError::other(format!(
                    "table {} is absent in id_to_table, need to request rpc list_tables to get the schema", table_id,
                )));
            }
        }
        Ok(FilterKeyExtractorImpl::Multi(multi_filter_key_extractor))
    }
}

pub type FilterKeyExtractorManagerRef = Arc<RpcFilterKeyExtractorManager>;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::mem;
    use std::sync::Arc;

    use bytes::{BufMut, BytesMut};
    use itertools::Itertools;
    use risingwave_common::catalog::ColumnDesc;
    use risingwave_common::hash::VirtualNode;
    use risingwave_common::row::OwnedRow;
    use risingwave_common::types::DataType;
    use risingwave_common::types::ScalarImpl::{self};
    use risingwave_common::util::row_serde::OrderedRowSerde;
    use risingwave_common::util::sort_util::OrderType;
    use risingwave_hummock_sdk::key::TABLE_PREFIX_LEN;
    use risingwave_pb::catalog::table::TableType;
    use risingwave_pb::catalog::{PbCreateType, PbStreamJobStatus, PbTable};
    use risingwave_pb::common::{PbColumnOrder, PbDirection, PbNullsAre, PbOrderType};
    use risingwave_pb::plan_common::PbColumnCatalog;

    use super::{DummyFilterKeyExtractor, FilterKeyExtractor, SchemaFilterKeyExtractor};
    use crate::filter_key_extractor::{
        FilterKeyExtractorImpl, FullKeyFilterKeyExtractor, MultiFilterKeyExtractor,
        RpcFilterKeyExtractorManager,
    };
    const fn dummy_vnode() -> [u8; VirtualNode::SIZE] {
        VirtualNode::from_index(233).to_be_bytes()
    }

    #[test]
    fn test_default_filter_key_extractor() {
        let dummy_filter_key_extractor = DummyFilterKeyExtractor;
        let full_key = "full_key".as_bytes();
        let output_key = dummy_filter_key_extractor.extract(full_key);

        assert_eq!("".as_bytes(), output_key);

        let full_key_filter_key_extractor = FullKeyFilterKeyExtractor;
        let output_key = full_key_filter_key_extractor.extract(full_key);

        assert_eq!(full_key, output_key);
    }

    fn build_table_with_prefix_column_num(column_count: u32) -> PbTable {
        PbTable {
            id: 0,
            schema_id: 0,
            database_id: 0,
            name: "test".to_string(),
            table_type: TableType::Table as i32,
            columns: vec![
                PbColumnCatalog {
                    column_desc: Some(
                        (&ColumnDesc::new_atomic(DataType::Int64, "_row_id", 0)).into(),
                    ),
                    is_hidden: true,
                },
                PbColumnCatalog {
                    column_desc: Some(
                        (&ColumnDesc::new_atomic(DataType::Int64, "col_1", 0)).into(),
                    ),
                    is_hidden: false,
                },
                PbColumnCatalog {
                    column_desc: Some(
                        (&ColumnDesc::new_atomic(DataType::Float64, "col_2", 0)).into(),
                    ),
                    is_hidden: false,
                },
                PbColumnCatalog {
                    column_desc: Some(
                        (&ColumnDesc::new_atomic(DataType::Varchar, "col_3", 0)).into(),
                    ),
                    is_hidden: false,
                },
            ],
            pk: vec![
                PbColumnOrder {
                    column_index: 1,
                    order_type: Some(PbOrderType {
                        direction: PbDirection::Ascending as _,
                        nulls_are: PbNullsAre::Largest as _,
                    }),
                },
                PbColumnOrder {
                    column_index: 3,
                    order_type: Some(PbOrderType {
                        direction: PbDirection::Ascending as _,
                        nulls_are: PbNullsAre::Largest as _,
                    }),
                },
            ],
            stream_key: vec![0],
            dependent_relations: vec![],
            distribution_key: (0..column_count as i32).collect_vec(),
            optional_associated_source_id: None,
            append_only: false,
            owner: risingwave_common::catalog::DEFAULT_SUPER_USER_ID,
            retention_seconds: Some(300),
            fragment_id: 0,
            dml_fragment_id: None,
            initialized_at_epoch: None,
            vnode_col_index: None,
            row_id_index: Some(0),
            value_indices: vec![0],
            definition: "".into(),
            handle_pk_conflict_behavior: 0,
            version_column_index: None,
            read_prefix_len_hint: 1,
            version: None,
            watermark_indices: vec![],
            dist_key_in_pk: vec![],
            cardinality: None,
            created_at_epoch: None,
            cleaned_by_watermark: false,
            stream_job_status: PbStreamJobStatus::Created.into(),
            create_type: PbCreateType::Foreground.into(),
            description: None,
            incoming_sinks: vec![],
            initialized_at_cluster_version: None,
            created_at_cluster_version: None,
            cdc_table_id: None,
            maybe_vnode_count: None,
        }
    }

    #[test]
    fn test_schema_filter_key_extractor() {
        let prost_table = build_table_with_prefix_column_num(1);
        let schema_filter_key_extractor = SchemaFilterKeyExtractor::new(&prost_table);

        let order_types: Vec<OrderType> = vec![OrderType::ascending(), OrderType::ascending()];
        let schema = vec![DataType::Int64, DataType::Varchar];
        let serializer = OrderedRowSerde::new(schema, order_types);
        let row = OwnedRow::new(vec![
            Some(ScalarImpl::Int64(100)),
            Some(ScalarImpl::Utf8("abc".into())),
        ]);
        let mut row_bytes = vec![];
        serializer.serialize(&row, &mut row_bytes);

        let table_prefix = {
            let mut buf = BytesMut::with_capacity(TABLE_PREFIX_LEN);
            buf.put_u32(1);
            buf.to_vec()
        };

        let vnode_prefix = &dummy_vnode()[..];

        let full_key = [&table_prefix, vnode_prefix, &row_bytes].concat();
        let output_key = schema_filter_key_extractor.extract(&full_key);
        assert_eq!(1 + mem::size_of::<i64>(), output_key.len());
    }

    #[test]
    fn test_multi_filter_key_extractor() {
        let mut multi_filter_key_extractor = MultiFilterKeyExtractor::default();
        {
            // test table_id 1
            let prost_table = build_table_with_prefix_column_num(1);
            let schema_filter_key_extractor = SchemaFilterKeyExtractor::new(&prost_table);
            multi_filter_key_extractor.register(
                1,
                Arc::new(FilterKeyExtractorImpl::Schema(schema_filter_key_extractor)),
            );
            let order_types: Vec<OrderType> = vec![OrderType::ascending(), OrderType::ascending()];
            let schema = vec![DataType::Int64, DataType::Varchar];
            let serializer = OrderedRowSerde::new(schema, order_types);
            let row = OwnedRow::new(vec![
                Some(ScalarImpl::Int64(100)),
                Some(ScalarImpl::Utf8("abc".into())),
            ]);
            let mut row_bytes = vec![];
            serializer.serialize(&row, &mut row_bytes);

            let table_prefix = {
                let mut buf = BytesMut::with_capacity(TABLE_PREFIX_LEN);
                buf.put_u32(1);
                buf.to_vec()
            };

            let vnode_prefix = &dummy_vnode()[..];

            let full_key = [&table_prefix, vnode_prefix, &row_bytes].concat();
            let output_key = multi_filter_key_extractor.extract(&full_key);

            let data_types = vec![DataType::Int64];
            let order_types = vec![OrderType::ascending()];
            let deserializer = OrderedRowSerde::new(data_types, order_types);

            let pk_prefix_len = deserializer.deserialize_prefix_len(&row_bytes, 1).unwrap();
            assert_eq!(pk_prefix_len, output_key.len());
        }

        {
            // test table_id 1
            let prost_table = build_table_with_prefix_column_num(2);
            let schema_filter_key_extractor = SchemaFilterKeyExtractor::new(&prost_table);
            multi_filter_key_extractor.register(
                2,
                Arc::new(FilterKeyExtractorImpl::Schema(schema_filter_key_extractor)),
            );
            let order_types: Vec<OrderType> = vec![OrderType::ascending(), OrderType::ascending()];
            let schema = vec![DataType::Int64, DataType::Varchar];
            let serializer = OrderedRowSerde::new(schema, order_types);
            let row = OwnedRow::new(vec![
                Some(ScalarImpl::Int64(100)),
                Some(ScalarImpl::Utf8("abc".into())),
            ]);
            let mut row_bytes = vec![];
            serializer.serialize(&row, &mut row_bytes);

            let table_prefix = {
                let mut buf = BytesMut::with_capacity(TABLE_PREFIX_LEN);
                buf.put_u32(2);
                buf.to_vec()
            };

            let vnode_prefix = &dummy_vnode()[..];

            let full_key = [&table_prefix, vnode_prefix, &row_bytes].concat();
            let output_key = multi_filter_key_extractor.extract(&full_key);

            let data_types = vec![DataType::Int64, DataType::Varchar];
            let order_types = vec![OrderType::ascending(), OrderType::ascending()];
            let deserializer = OrderedRowSerde::new(data_types, order_types);

            let pk_prefix_len = deserializer.deserialize_prefix_len(&row_bytes, 1).unwrap();

            assert_eq!(pk_prefix_len, output_key.len());
        }
    }

    #[tokio::test]
    async fn test_filter_key_extractor_manager() {
        let filter_key_extractor_manager = Arc::new(RpcFilterKeyExtractorManager::default());

        filter_key_extractor_manager.update(
            1,
            Arc::new(FilterKeyExtractorImpl::Dummy(DummyFilterKeyExtractor)),
        );

        let remaining_table_id_set = HashSet::from([1]);
        let multi_filter_key_extractor = filter_key_extractor_manager
            .acquire(remaining_table_id_set)
            .await
            .unwrap();

        match multi_filter_key_extractor {
            FilterKeyExtractorImpl::Multi(multi_filter_key_extractor) => {
                assert_eq!(1, multi_filter_key_extractor.size());
            }

            _ => {
                unreachable!()
            }
        }
    }
}
