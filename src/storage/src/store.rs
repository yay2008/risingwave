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

use std::cmp::min;
use std::collections::HashSet;
use std::default::Default;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Bound;
use std::sync::{Arc, LazyLock};

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use futures_async_stream::try_stream;
use prost::Message;
use risingwave_common::array::Op;
use risingwave_common::bitmap::Bitmap;
use risingwave_common::catalog::{TableId, TableOption};
use risingwave_common::hash::VirtualNode;
use risingwave_common::util::epoch::{Epoch, EpochPair};
use risingwave_hummock_sdk::key::{FullKey, TableKey, TableKeyRange};
use risingwave_hummock_sdk::table_watermark::{VnodeWatermark, WatermarkDirection};
use risingwave_hummock_sdk::{HummockReadEpoch, SyncResult};
use risingwave_hummock_trace::{
    TracedInitOptions, TracedNewLocalOptions, TracedOpConsistencyLevel, TracedPrefetchOptions,
    TracedReadOptions, TracedSealCurrentEpochOptions, TracedWriteOptions,
};
use risingwave_pb::hummock::PbVnodeWatermark;

use crate::error::{StorageError, StorageResult};
use crate::hummock::CachePolicy;
use crate::monitor::{MonitoredStateStore, MonitoredStorageMetrics};
use crate::storage_value::StorageValue;

pub trait StaticSendSync = Send + Sync + 'static;

pub trait IterItem: Send + 'static {
    type ItemRef<'a>: Send + Copy + 'a;
}

impl IterItem for StateStoreIterItem {
    type ItemRef<'a> = StateStoreIterItemRef<'a>;
}

impl IterItem for StateStoreReadLogItem {
    type ItemRef<'a> = StateStoreReadLogItemRef<'a>;
}

pub trait StateStoreIter<T: IterItem = StateStoreIterItem>: Send {
    fn try_next(
        &mut self,
    ) -> impl Future<Output = StorageResult<Option<T::ItemRef<'_>>>> + Send + '_;
}

pub fn to_owned_item((key, value): StateStoreIterItemRef<'_>) -> StorageResult<StateStoreIterItem> {
    Ok((key.copy_into(), Bytes::copy_from_slice(value)))
}

pub trait StateStoreIterExt<T: IterItem = StateStoreIterItem>: StateStoreIter<T> + Sized {
    type ItemStream<O: Send, F: Send + for<'a> Fn(T::ItemRef<'a>) -> StorageResult<O>>: Stream<Item = StorageResult<O>>
        + Send;

    fn into_stream<O: Send, F: for<'a> Fn(T::ItemRef<'a>) -> StorageResult<O> + Send>(
        self,
        f: F,
    ) -> Self::ItemStream<O, F>;

    fn fused(self) -> FusedStateStoreIter<Self, T> {
        FusedStateStoreIter::new(self)
    }
}

#[try_stream(ok = O, error = StorageError)]
async fn into_stream_inner<
    T: IterItem,
    I: StateStoreIter<T>,
    O: Send,
    F: for<'a> Fn(T::ItemRef<'a>) -> StorageResult<O> + Send,
>(
    iter: I,
    f: F,
) {
    let mut iter = iter.fused();
    while let Some(item) = iter.try_next().await? {
        yield f(item)?;
    }
}

pub struct FromStreamStateStoreIter<S> {
    inner: S,
    item_buffer: Option<StateStoreIterItem>,
}

impl<S> FromStreamStateStoreIter<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            item_buffer: None,
        }
    }
}

impl<S: Stream<Item = StorageResult<StateStoreIterItem>> + Unpin + Send> StateStoreIter
    for FromStreamStateStoreIter<S>
{
    async fn try_next(&mut self) -> StorageResult<Option<StateStoreIterItemRef<'_>>> {
        self.item_buffer = self.inner.try_next().await?;
        Ok(self
            .item_buffer
            .as_ref()
            .map(|(key, value)| (key.to_ref(), value.as_ref())))
    }
}

pub struct FusedStateStoreIter<I, T> {
    inner: I,
    finished: bool,
    _phantom: PhantomData<T>,
}

impl<I, T> FusedStateStoreIter<I, T> {
    fn new(inner: I) -> Self {
        Self {
            inner,
            finished: false,
            _phantom: PhantomData,
        }
    }
}

impl<T: IterItem, I: StateStoreIter<T>> FusedStateStoreIter<I, T> {
    async fn try_next(&mut self) -> StorageResult<Option<T::ItemRef<'_>>> {
        assert!(!self.finished, "call try_next after finish");
        let result = self.inner.try_next().await;
        match &result {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                self.finished = true;
            }
        }
        result
    }
}

impl<T: IterItem, I: StateStoreIter<T>> StateStoreIterExt<T> for I {
    type ItemStream<O: Send, F: Send + for<'a> Fn(T::ItemRef<'a>) -> StorageResult<O>> =
        impl Stream<Item = StorageResult<O>> + Send;

    fn into_stream<O: Send, F: for<'a> Fn(T::ItemRef<'a>) -> StorageResult<O> + Send>(
        self,
        f: F,
    ) -> Self::ItemStream<O, F> {
        into_stream_inner(self, f)
    }
}

pub type StateStoreIterItemRef<'a> = (FullKey<&'a [u8]>, &'a [u8]);
pub type StateStoreIterItem = (FullKey<Bytes>, Bytes);
pub trait StateStoreReadIter = StateStoreIter + 'static;

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum ChangeLogValue<T> {
    Insert(T),
    Update { new_value: T, old_value: T },
    Delete(T),
}

impl<T> ChangeLogValue<T> {
    pub fn try_map<O>(self, f: impl Fn(T) -> StorageResult<O>) -> StorageResult<ChangeLogValue<O>> {
        Ok(match self {
            ChangeLogValue::Insert(value) => ChangeLogValue::Insert(f(value)?),
            ChangeLogValue::Update {
                new_value,
                old_value,
            } => ChangeLogValue::Update {
                new_value: f(new_value)?,
                old_value: f(old_value)?,
            },
            ChangeLogValue::Delete(value) => ChangeLogValue::Delete(f(value)?),
        })
    }

    pub fn into_op_value_iter(self) -> impl Iterator<Item = (Op, T)> {
        std::iter::from_coroutine(
            #[coroutine]
            move || match self {
                Self::Insert(row) => {
                    yield (Op::Insert, row);
                }
                Self::Delete(row) => {
                    yield (Op::Delete, row);
                }
                Self::Update {
                    old_value,
                    new_value,
                } => {
                    yield (Op::UpdateDelete, old_value);
                    yield (Op::UpdateInsert, new_value);
                }
            },
        )
    }
}

impl<T: AsRef<[u8]>> ChangeLogValue<T> {
    pub fn to_ref(&self) -> ChangeLogValue<&[u8]> {
        match self {
            ChangeLogValue::Insert(val) => ChangeLogValue::Insert(val.as_ref()),
            ChangeLogValue::Update {
                new_value,
                old_value,
            } => ChangeLogValue::Update {
                new_value: new_value.as_ref(),
                old_value: old_value.as_ref(),
            },
            ChangeLogValue::Delete(val) => ChangeLogValue::Delete(val.as_ref()),
        }
    }
}

pub type StateStoreReadLogItem = (TableKey<Bytes>, ChangeLogValue<Bytes>);
pub type StateStoreReadLogItemRef<'a> = (TableKey<&'a [u8]>, ChangeLogValue<&'a [u8]>);
#[derive(Clone)]
pub struct ReadLogOptions {
    pub table_id: TableId,
}

pub trait StateStoreReadChangeLogIter = StateStoreIter<StateStoreReadLogItem> + Send + 'static;

pub trait StateStoreRead: StaticSendSync {
    type Iter: StateStoreReadIter;
    type RevIter: StateStoreReadIter;
    type ChangeLogIter: StateStoreReadChangeLogIter;

    /// Point gets a value from the state store.
    /// The result is based on a snapshot corresponding to the given `epoch`.
    fn get(
        &self,
        key: TableKey<Bytes>,
        epoch: u64,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Option<Bytes>>> + Send + '_;

    /// Opens and returns an iterator for given `prefix_hint` and `full_key_range`
    /// Internally, `prefix_hint` will be used to for checking `bloom_filter` and
    /// `full_key_range` used for iter. (if the `prefix_hint` not None, it should be be included
    /// in `key_range`) The returned iterator will iterate data based on a snapshot
    /// corresponding to the given `epoch`.
    fn iter(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Self::Iter>> + Send + '_;

    fn rev_iter(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Self::RevIter>> + Send + '_;

    fn iter_log(
        &self,
        epoch_range: (u64, u64),
        key_range: TableKeyRange,
        options: ReadLogOptions,
    ) -> impl Future<Output = StorageResult<Self::ChangeLogIter>> + Send + '_;
}

pub trait StateStoreReadExt: StaticSendSync {
    /// Scans `limit` number of keys from a key range. If `limit` is `None`, scans all elements.
    /// Internally, `prefix_hint` will be used to for checking `bloom_filter` and
    /// `full_key_range` used for iter.
    /// The result is based on a snapshot corresponding to the given `epoch`.
    ///
    ///
    /// By default, this simply calls `StateStore::iter` to fetch elements.
    fn scan(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        limit: Option<usize>,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Vec<StateStoreIterItem>>> + Send + '_;
}

impl<S: StateStoreRead> StateStoreReadExt for S {
    async fn scan(
        &self,
        key_range: TableKeyRange,
        epoch: u64,
        limit: Option<usize>,
        mut read_options: ReadOptions,
    ) -> StorageResult<Vec<StateStoreIterItem>> {
        if limit.is_some() {
            read_options.prefetch_options.prefetch = false;
        }
        const MAX_INITIAL_CAP: usize = 1024;
        let limit = limit.unwrap_or(usize::MAX);
        let mut ret = Vec::with_capacity(min(limit, MAX_INITIAL_CAP));
        let mut iter = self.iter(key_range, epoch, read_options).await?;
        while let Some((key, value)) = iter.try_next().await? {
            ret.push((key.copy_into(), Bytes::copy_from_slice(value)))
        }
        Ok(ret)
    }
}

pub trait StateStoreWrite: StaticSendSync {
    /// Writes a batch to storage. The batch should be:
    /// * Ordered. KV pairs will be directly written to the table, so it must be ordered.
    /// * Locally unique. There should not be two or more operations on the same key in one write
    ///   batch.
    ///
    /// Ingests a batch of data into the state store. One write batch should never contain operation
    /// on the same key. e.g. Put(233, x) then Delete(233).
    /// An epoch should be provided to ingest a write batch. It is served as:
    /// - A handle to represent an atomic write session. All ingested write batches associated with
    ///   the same `Epoch` have the all-or-nothing semantics, meaning that partial changes are not
    ///   queryable and will be rolled back if instructed.
    /// - A version of a kv pair. kv pair associated with larger `Epoch` is guaranteed to be newer
    ///   then kv pair with smaller `Epoch`. Currently this version is only used to derive the
    ///   per-key modification history (e.g. in compaction), not across different keys.
    fn ingest_batch(
        &self,
        kv_pairs: Vec<(TableKey<Bytes>, StorageValue)>,
        delete_ranges: Vec<(Bound<Bytes>, Bound<Bytes>)>,
        write_options: WriteOptions,
    ) -> StorageResult<usize>;
}

pub trait SyncFuture = Future<Output = StorageResult<SyncResult>> + Send + 'static;

pub trait StateStore: StateStoreRead + StaticSendSync + Clone {
    type Local: LocalStateStore;

    /// If epoch is `Committed`, we will wait until the epoch is committed and its data is ready to
    /// read. If epoch is `Current`, we will only check if the data can be read with this epoch.
    fn try_wait_epoch(
        &self,
        epoch: HummockReadEpoch,
    ) -> impl Future<Output = StorageResult<()>> + Send + '_;

    fn sync(&self, epoch: u64, table_ids: HashSet<TableId>) -> impl SyncFuture;

    /// Creates a [`MonitoredStateStore`] from this state store, with given `stats`.
    fn monitored(self, storage_metrics: Arc<MonitoredStorageMetrics>) -> MonitoredStateStore<Self> {
        MonitoredStateStore::new(self, storage_metrics)
    }

    fn new_local(&self, option: NewLocalOptions) -> impl Future<Output = Self::Local> + Send + '_;
}

/// A state store that is dedicated for streaming operator, which only reads the uncommitted data
/// written by itself. Each local state store is not `Clone`, and is owned by a streaming state
/// table.
pub trait LocalStateStore: StaticSendSync {
    type Iter<'a>: StateStoreIter + 'a;
    type RevIter<'a>: StateStoreIter + 'a;

    /// Point gets a value from the state store.
    /// The result is based on the latest written snapshot.
    fn get(
        &self,
        key: TableKey<Bytes>,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Option<Bytes>>> + Send + '_;

    /// Opens and returns an iterator for given `prefix_hint` and `full_key_range`
    /// Internally, `prefix_hint` will be used to for checking `bloom_filter` and
    /// `full_key_range` used for iter. (if the `prefix_hint` not None, it should be be included
    /// in `key_range`) The returned iterator will iterate data based on the latest written
    /// snapshot.
    fn iter(
        &self,
        key_range: TableKeyRange,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Self::Iter<'_>>> + Send + '_;

    fn rev_iter(
        &self,
        key_range: TableKeyRange,
        read_options: ReadOptions,
    ) -> impl Future<Output = StorageResult<Self::RevIter<'_>>> + Send + '_;

    /// Get last persisted watermark for a given vnode.
    fn get_table_watermark(&self, vnode: VirtualNode) -> Option<Bytes>;

    /// Inserts a key-value entry associated with a given `epoch` into the state store.
    fn insert(
        &mut self,
        key: TableKey<Bytes>,
        new_val: Bytes,
        old_val: Option<Bytes>,
    ) -> StorageResult<()>;

    /// Deletes a key-value entry from the state store. Only the key-value entry with epoch smaller
    /// than the given `epoch` will be deleted.
    fn delete(&mut self, key: TableKey<Bytes>, old_val: Bytes) -> StorageResult<()>;

    fn flush(&mut self) -> impl Future<Output = StorageResult<usize>> + Send + '_;

    fn try_flush(&mut self) -> impl Future<Output = StorageResult<()>> + Send + '_;
    fn epoch(&self) -> u64;

    fn is_dirty(&self) -> bool;

    /// Initializes the state store with given `epoch` pair.
    /// Typically we will use `epoch.curr` as the initialized epoch,
    /// Since state table will begin as empty.
    /// In some cases like replicated state table, state table may not be empty initially,
    /// as such we need to wait for `epoch.prev` checkpoint to complete,
    /// hence this interface is made async.
    fn init(&mut self, opts: InitOptions) -> impl Future<Output = StorageResult<()>> + Send + '_;

    /// Updates the monotonically increasing write epoch to `new_epoch`.
    /// All writes after this function is called will be tagged with `new_epoch`. In other words,
    /// the previous write epoch is sealed.
    fn seal_current_epoch(&mut self, next_epoch: u64, opts: SealCurrentEpochOptions);

    // Updates the vnode bitmap corresponding to the local state store
    // Returns the previous vnode bitmap
    fn update_vnode_bitmap(&mut self, vnodes: Arc<Bitmap>) -> Arc<Bitmap>;
}

/// If `prefetch` is true, prefetch will be enabled. Prefetching may increase the memory
/// footprint of the CN process because the prefetched blocks cannot be evicted.
/// Since the streaming-read of object-storage may hung in some case, we still use sync short read
/// for both batch-query and streaming process. So this configure is unused.
#[derive(Default, Clone, Copy)]
pub struct PrefetchOptions {
    pub prefetch: bool,
    pub for_large_query: bool,
}

impl PrefetchOptions {
    pub fn prefetch_for_large_range_scan() -> Self {
        Self {
            prefetch: true,
            for_large_query: true,
        }
    }

    pub fn prefetch_for_small_range_scan() -> Self {
        Self {
            prefetch: true,
            for_large_query: false,
        }
    }

    pub fn new(prefetch: bool, for_large_query: bool) -> Self {
        Self {
            prefetch,
            for_large_query,
        }
    }
}

impl From<TracedPrefetchOptions> for PrefetchOptions {
    fn from(value: TracedPrefetchOptions) -> Self {
        Self {
            prefetch: value.prefetch,
            for_large_query: value.for_large_query,
        }
    }
}

impl From<PrefetchOptions> for TracedPrefetchOptions {
    fn from(value: PrefetchOptions) -> Self {
        Self {
            prefetch: value.prefetch,
            for_large_query: value.for_large_query,
        }
    }
}

#[derive(Default, Clone)]
pub struct ReadOptions {
    /// A hint for prefix key to check bloom filter.
    /// If the `prefix_hint` is not None, it should be included in
    /// `key` or `key_range` in the read API.
    pub prefix_hint: Option<Bytes>,
    pub ignore_range_tombstone: bool,
    pub prefetch_options: PrefetchOptions,
    pub cache_policy: CachePolicy,

    pub retention_seconds: Option<u32>,
    pub table_id: TableId,
    /// Read from historical hummock version of meta snapshot backup.
    /// It should only be used by `StorageTable` for batch query.
    pub read_version_from_backup: bool,
    pub read_committed: bool,
}

impl From<TracedReadOptions> for ReadOptions {
    fn from(value: TracedReadOptions) -> Self {
        Self {
            prefix_hint: value.prefix_hint.map(|b| b.into()),
            ignore_range_tombstone: value.ignore_range_tombstone,
            prefetch_options: value.prefetch_options.into(),
            cache_policy: value.cache_policy.into(),
            retention_seconds: value.retention_seconds,
            table_id: value.table_id.into(),
            read_version_from_backup: value.read_version_from_backup,
            read_committed: value.read_committed,
        }
    }
}

impl From<ReadOptions> for TracedReadOptions {
    fn from(value: ReadOptions) -> Self {
        Self {
            prefix_hint: value.prefix_hint.map(|b| b.into()),
            ignore_range_tombstone: value.ignore_range_tombstone,
            prefetch_options: value.prefetch_options.into(),
            cache_policy: value.cache_policy.into(),
            retention_seconds: value.retention_seconds,
            table_id: value.table_id.into(),
            read_version_from_backup: value.read_version_from_backup,
            read_committed: value.read_committed,
        }
    }
}

pub fn gen_min_epoch(base_epoch: u64, retention_seconds: Option<&u32>) -> u64 {
    let base_epoch = Epoch(base_epoch);
    match retention_seconds {
        Some(retention_seconds_u32) => {
            base_epoch
                .subtract_ms(*retention_seconds_u32 as u64 * 1000)
                .0
        }
        None => 0,
    }
}

#[derive(Default, Clone)]
pub struct WriteOptions {
    pub epoch: u64,
    pub table_id: TableId,
}

impl From<TracedWriteOptions> for WriteOptions {
    fn from(value: TracedWriteOptions) -> Self {
        Self {
            epoch: value.epoch,
            table_id: value.table_id.into(),
        }
    }
}

pub trait CheckOldValueEquality = Fn(&Bytes, &Bytes) -> bool + Send + Sync;

pub static CHECK_BYTES_EQUAL: LazyLock<Arc<dyn CheckOldValueEquality>> =
    LazyLock::new(|| Arc::new(|first: &Bytes, second: &Bytes| first == second));

#[derive(Default, Clone)]
pub enum OpConsistencyLevel {
    #[default]
    Inconsistent,
    ConsistentOldValue {
        check_old_value: Arc<dyn CheckOldValueEquality>,
        /// whether should store the old value
        is_log_store: bool,
    },
}

impl Debug for OpConsistencyLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            OpConsistencyLevel::Inconsistent => f.write_str("OpConsistencyLevel::Inconsistent"),
            OpConsistencyLevel::ConsistentOldValue { is_log_store, .. } => f
                .debug_struct("OpConsistencyLevel::ConsistentOldValue")
                .field("is_log_store", is_log_store)
                .finish(),
        }
    }
}

impl PartialEq<Self> for OpConsistencyLevel {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (
                OpConsistencyLevel::Inconsistent,
                OpConsistencyLevel::Inconsistent
            ) | (
                OpConsistencyLevel::ConsistentOldValue {
                    is_log_store: true,
                    ..
                },
                OpConsistencyLevel::ConsistentOldValue {
                    is_log_store: true,
                    ..
                },
            ) | (
                OpConsistencyLevel::ConsistentOldValue {
                    is_log_store: false,
                    ..
                },
                OpConsistencyLevel::ConsistentOldValue {
                    is_log_store: false,
                    ..
                },
            )
        )
    }
}

impl Eq for OpConsistencyLevel {}

impl OpConsistencyLevel {
    pub fn update(&mut self, new_level: &OpConsistencyLevel) {
        assert_ne!(self, new_level);
        *self = new_level.clone()
    }
}

#[derive(Clone)]
pub struct NewLocalOptions {
    pub table_id: TableId,
    /// Whether the operation is consistent. The term `consistent` requires the following:
    ///
    /// 1. A key cannot be inserted or deleted for more than once, i.e. inserting to an existing
    ///    key or deleting an non-existing key is not allowed.
    ///
    /// 2. The old value passed from
    ///    `update` and `delete` should match the original stored value.
    pub op_consistency_level: OpConsistencyLevel,
    pub table_option: TableOption,

    /// Indicate if this is replicated. If it is, we should not
    /// upload its `ReadVersions`.
    pub is_replicated: bool,

    /// The vnode bitmap for the local state store instance
    pub vnodes: Arc<Bitmap>,
}

impl From<TracedNewLocalOptions> for NewLocalOptions {
    fn from(value: TracedNewLocalOptions) -> Self {
        Self {
            table_id: value.table_id.into(),
            op_consistency_level: match value.op_consistency_level {
                TracedOpConsistencyLevel::Inconsistent => OpConsistencyLevel::Inconsistent,
                TracedOpConsistencyLevel::ConsistentOldValue => {
                    OpConsistencyLevel::ConsistentOldValue {
                        check_old_value: CHECK_BYTES_EQUAL.clone(),
                        // TODO: for simplicity, set it to false
                        is_log_store: false,
                    }
                }
            },
            table_option: value.table_option.into(),
            is_replicated: value.is_replicated,
            vnodes: Arc::new(value.vnodes.into()),
        }
    }
}

impl From<NewLocalOptions> for TracedNewLocalOptions {
    fn from(value: NewLocalOptions) -> Self {
        Self {
            table_id: value.table_id.into(),
            op_consistency_level: match value.op_consistency_level {
                OpConsistencyLevel::Inconsistent => TracedOpConsistencyLevel::Inconsistent,
                OpConsistencyLevel::ConsistentOldValue { .. } => {
                    TracedOpConsistencyLevel::ConsistentOldValue
                }
            },
            table_option: value.table_option.into(),
            is_replicated: value.is_replicated,
            vnodes: value.vnodes.as_ref().clone().into(),
        }
    }
}

impl NewLocalOptions {
    pub fn new(
        table_id: TableId,
        op_consistency_level: OpConsistencyLevel,
        table_option: TableOption,
        vnodes: Arc<Bitmap>,
    ) -> Self {
        NewLocalOptions {
            table_id,
            op_consistency_level,
            table_option,
            is_replicated: false,
            vnodes,
        }
    }

    pub fn new_replicated(
        table_id: TableId,
        op_consistency_level: OpConsistencyLevel,
        table_option: TableOption,
        vnodes: Arc<Bitmap>,
    ) -> Self {
        NewLocalOptions {
            table_id,
            op_consistency_level,
            table_option,
            is_replicated: true,
            vnodes,
        }
    }

    pub fn for_test(table_id: TableId) -> Self {
        Self {
            table_id,
            op_consistency_level: OpConsistencyLevel::Inconsistent,
            table_option: TableOption {
                retention_seconds: None,
            },
            is_replicated: false,
            vnodes: Arc::new(Bitmap::ones(VirtualNode::COUNT)),
        }
    }
}

#[derive(Clone)]
pub struct InitOptions {
    pub epoch: EpochPair,
}

impl InitOptions {
    pub fn new(epoch: EpochPair) -> Self {
        Self { epoch }
    }
}

impl From<InitOptions> for TracedInitOptions {
    fn from(value: InitOptions) -> Self {
        TracedInitOptions {
            epoch: value.epoch.into(),
        }
    }
}

impl From<TracedInitOptions> for InitOptions {
    fn from(value: TracedInitOptions) -> Self {
        InitOptions {
            epoch: value.epoch.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SealCurrentEpochOptions {
    pub table_watermarks: Option<(WatermarkDirection, Vec<VnodeWatermark>)>,
    pub switch_op_consistency_level: Option<OpConsistencyLevel>,
}

impl From<SealCurrentEpochOptions> for TracedSealCurrentEpochOptions {
    fn from(value: SealCurrentEpochOptions) -> Self {
        TracedSealCurrentEpochOptions {
            table_watermarks: value.table_watermarks.map(|(direction, watermarks)| {
                (
                    direction == WatermarkDirection::Ascending,
                    watermarks
                        .into_iter()
                        .map(|watermark| {
                            let pb_watermark = PbVnodeWatermark::from(watermark);
                            Message::encode_to_vec(&pb_watermark)
                        })
                        .collect(),
                )
            }),
            switch_op_consistency_level: value
                .switch_op_consistency_level
                .map(|level| matches!(level, OpConsistencyLevel::ConsistentOldValue { .. })),
        }
    }
}

impl From<TracedSealCurrentEpochOptions> for SealCurrentEpochOptions {
    fn from(value: TracedSealCurrentEpochOptions) -> SealCurrentEpochOptions {
        SealCurrentEpochOptions {
            table_watermarks: value.table_watermarks.map(|(is_ascending, watermarks)| {
                (
                    if is_ascending {
                        WatermarkDirection::Ascending
                    } else {
                        WatermarkDirection::Descending
                    },
                    watermarks
                        .into_iter()
                        .map(|serialized_watermark| {
                            Message::decode(serialized_watermark.as_slice())
                                .map(|pb: PbVnodeWatermark| VnodeWatermark::from(pb))
                                .expect("should not failed")
                        })
                        .collect(),
                )
            }),
            switch_op_consistency_level: value.switch_op_consistency_level.map(|enable| {
                if enable {
                    OpConsistencyLevel::ConsistentOldValue {
                        check_old_value: CHECK_BYTES_EQUAL.clone(),
                        is_log_store: false,
                    }
                } else {
                    OpConsistencyLevel::Inconsistent
                }
            }),
        }
    }
}

impl SealCurrentEpochOptions {
    pub fn for_test() -> Self {
        Self {
            table_watermarks: None,
            switch_op_consistency_level: None,
        }
    }
}
