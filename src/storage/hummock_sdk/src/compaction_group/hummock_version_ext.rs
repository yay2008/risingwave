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

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use itertools::Itertools;
use risingwave_common::catalog::TableId;
use risingwave_common::hash::VnodeBitmapExt;
use risingwave_pb::hummock::{
    CompactionConfig, CompatibilityVersion, GroupConstruct, GroupMerge, PbLevelType,
};
use tracing::warn;

use super::group_split::get_sub_level_insert_hint;
use super::{group_split, StateTableId};
use crate::change_log::TableChangeLogCommon;
use crate::compaction_group::StaticCompactionGroupId;
use crate::key_range::KeyRangeCommon;
use crate::level::{Level, Levels, OverlappingLevel};
use crate::sstable_info::SstableInfo;
use crate::table_watermark::{ReadTableWatermark, TableWatermarks};
use crate::version::{
    GroupDelta, GroupDeltas, HummockVersion, HummockVersionDelta, HummockVersionStateTableInfo,
    IntraLevelDelta,
};
use crate::{can_concat, CompactionGroupId, HummockSstableId, HummockSstableObjectId};

pub struct GroupDeltasSummary {
    pub delete_sst_levels: Vec<u32>,
    pub delete_sst_ids_set: HashSet<u64>,
    pub insert_sst_level_id: u32,
    pub insert_sub_level_id: u64,
    pub insert_table_infos: Vec<SstableInfo>,
    pub group_construct: Option<GroupConstruct>,
    pub group_destroy: Option<CompactionGroupId>,
    pub new_vnode_partition_count: u32,
    pub group_merge: Option<GroupMerge>,
}

pub fn summarize_group_deltas(
    group_deltas: &GroupDeltas,
    compaction_group_id: CompactionGroupId,
) -> GroupDeltasSummary {
    let mut delete_sst_levels = Vec::with_capacity(group_deltas.group_deltas.len());
    let mut delete_sst_ids_set = HashSet::new();
    let mut insert_sst_level_id = u32::MAX;
    let mut insert_sub_level_id = u64::MAX;
    let mut insert_table_infos = vec![];
    let mut group_construct = None;
    let mut group_destroy = None;
    let mut new_vnode_partition_count = 0;
    let mut group_merge = None;

    for group_delta in &group_deltas.group_deltas {
        match group_delta {
            GroupDelta::IntraLevel(intra_level) => {
                if !intra_level.removed_table_ids.is_empty() {
                    delete_sst_levels.push(intra_level.level_idx);
                    delete_sst_ids_set.extend(intra_level.removed_table_ids.iter().clone());
                }
                if !intra_level.inserted_table_infos.is_empty() {
                    insert_sst_level_id = intra_level.level_idx;
                    insert_sub_level_id = intra_level.l0_sub_level_id;
                    insert_table_infos.extend(intra_level.inserted_table_infos.iter().cloned());
                }
                new_vnode_partition_count = intra_level.vnode_partition_count;
            }
            GroupDelta::GroupConstruct(construct_delta) => {
                assert!(group_construct.is_none());
                group_construct = Some(construct_delta.clone());
            }
            GroupDelta::GroupDestroy(_) => {
                assert!(group_destroy.is_none());
                group_destroy = Some(compaction_group_id);
            }
            GroupDelta::GroupMerge(merge_delta) => {
                assert!(group_merge.is_none());
                group_merge = Some(*merge_delta);
                group_destroy = Some(merge_delta.right_group_id);
            }
        }
    }

    delete_sst_levels.sort();
    delete_sst_levels.dedup();

    GroupDeltasSummary {
        delete_sst_levels,
        delete_sst_ids_set,
        insert_sst_level_id,
        insert_sub_level_id,
        insert_table_infos,
        group_construct,
        group_destroy,
        new_vnode_partition_count,
        group_merge,
    }
}

#[derive(Clone, Default)]
pub struct TableGroupInfo {
    pub group_id: CompactionGroupId,
    pub group_size: u64,
    pub table_statistic: HashMap<StateTableId, u64>,
    pub split_by_table: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SstDeltaInfo {
    pub insert_sst_level: u32,
    pub insert_sst_infos: Vec<SstableInfo>,
    pub delete_sst_object_ids: Vec<HummockSstableObjectId>,
}

pub type BranchedSstInfo = HashMap<CompactionGroupId, Vec<HummockSstableId>>;

impl HummockVersion {
    pub fn get_compaction_group_levels(&self, compaction_group_id: CompactionGroupId) -> &Levels {
        self.levels
            .get(&compaction_group_id)
            .unwrap_or_else(|| panic!("compaction group {} does not exist", compaction_group_id))
    }

    pub fn get_compaction_group_levels_mut(
        &mut self,
        compaction_group_id: CompactionGroupId,
    ) -> &mut Levels {
        self.levels
            .get_mut(&compaction_group_id)
            .unwrap_or_else(|| panic!("compaction group {} does not exist", compaction_group_id))
    }

    pub fn get_combined_levels(&self) -> impl Iterator<Item = &'_ Level> + '_ {
        self.levels
            .values()
            .flat_map(|level| level.l0.sub_levels.iter().rev().chain(level.levels.iter()))
    }

    pub fn get_object_ids(&self) -> HashSet<HummockSstableObjectId> {
        self.get_sst_infos().map(|s| s.object_id).collect()
    }

    pub fn get_sst_ids(&self) -> HashSet<HummockSstableObjectId> {
        self.get_sst_infos().map(|s| s.sst_id).collect()
    }

    pub fn get_sst_infos(&self) -> impl Iterator<Item = &SstableInfo> {
        self.get_combined_levels()
            .flat_map(|level| level.table_infos.iter())
            .chain(self.table_change_log.values().flat_map(|change_log| {
                change_log.0.iter().flat_map(|epoch_change_log| {
                    epoch_change_log
                        .old_value
                        .iter()
                        .chain(epoch_change_log.new_value.iter())
                })
            }))
    }

    // only scan the sst infos from levels in the specified compaction group (without table change log)
    pub fn get_sst_ids_by_group_id(
        &self,
        compaction_group_id: CompactionGroupId,
    ) -> impl Iterator<Item = u64> + '_ {
        self.levels
            .iter()
            .filter_map(move |(cg_id, level)| {
                if *cg_id == compaction_group_id {
                    Some(level)
                } else {
                    None
                }
            })
            .flat_map(|level| level.l0.sub_levels.iter().rev().chain(level.levels.iter()))
            .flat_map(|level| level.table_infos.iter())
            .map(|s| s.sst_id)
    }

    /// `get_sst_infos_from_groups` doesn't guarantee that all returned sst info belongs to `select_group`.
    /// i.e. `select_group` is just a hint.
    /// We separate `get_sst_infos_from_groups` and `get_sst_infos` because `get_sst_infos_from_groups` may be further customized in the future.
    pub fn get_sst_infos_from_groups<'a>(
        &'a self,
        select_group: &'a HashSet<CompactionGroupId>,
    ) -> impl Iterator<Item = &SstableInfo> + 'a {
        self.levels
            .iter()
            .filter_map(|(cg_id, level)| {
                if select_group.contains(cg_id) {
                    Some(level)
                } else {
                    None
                }
            })
            .flat_map(|level| level.l0.sub_levels.iter().rev().chain(level.levels.iter()))
            .flat_map(|level| level.table_infos.iter())
            .chain(self.table_change_log.values().flat_map(|change_log| {
                // TODO: optimization: strip table change log
                change_log.0.iter().flat_map(|epoch_change_log| {
                    epoch_change_log
                        .old_value
                        .iter()
                        .chain(epoch_change_log.new_value.iter())
                })
            }))
    }

    pub fn level_iter<F: FnMut(&Level) -> bool>(
        &self,
        compaction_group_id: CompactionGroupId,
        mut f: F,
    ) {
        if let Some(levels) = self.levels.get(&compaction_group_id) {
            for sub_level in &levels.l0.sub_levels {
                if !f(sub_level) {
                    return;
                }
            }
            for level in &levels.levels {
                if !f(level) {
                    return;
                }
            }
        }
    }

    pub fn num_levels(&self, compaction_group_id: CompactionGroupId) -> usize {
        // l0 is currently separated from all levels
        self.levels
            .get(&compaction_group_id)
            .map(|group| group.levels.len() + 1)
            .unwrap_or(0)
    }

    pub fn safe_epoch_table_watermarks(
        &self,
        existing_table_ids: &[u32],
    ) -> BTreeMap<u32, TableWatermarks> {
        safe_epoch_table_watermarks_impl(
            &self.table_watermarks,
            &self.state_table_info,
            existing_table_ids,
        )
    }
}

pub fn safe_epoch_table_watermarks_impl(
    table_watermarks: &HashMap<TableId, Arc<TableWatermarks>>,
    state_table_info: &HummockVersionStateTableInfo,
    existing_table_ids: &[u32],
) -> BTreeMap<u32, TableWatermarks> {
    fn extract_single_table_watermark(
        table_watermarks: &TableWatermarks,
        safe_epoch: u64,
    ) -> Option<TableWatermarks> {
        if let Some((first_epoch, first_epoch_watermark)) = table_watermarks.watermarks.first() {
            assert!(
                *first_epoch >= safe_epoch,
                "smallest epoch {} in table watermark should be at least safe epoch {}",
                first_epoch,
                safe_epoch
            );
            if *first_epoch == safe_epoch {
                Some(TableWatermarks {
                    watermarks: vec![(*first_epoch, first_epoch_watermark.clone())],
                    direction: table_watermarks.direction,
                })
            } else {
                None
            }
        } else {
            None
        }
    }
    table_watermarks
        .iter()
        .filter_map(|(table_id, table_watermarks)| {
            let u32_table_id = table_id.table_id();
            if !existing_table_ids.contains(&u32_table_id) {
                None
            } else {
                extract_single_table_watermark(
                    table_watermarks,
                    state_table_info
                        .info()
                        .get(table_id)
                        .expect("table should exist")
                        .safe_epoch,
                )
                .map(|table_watermarks| (table_id.table_id, table_watermarks))
            }
        })
        .collect()
}

pub fn safe_epoch_read_table_watermarks_impl(
    safe_epoch_watermarks: &BTreeMap<u32, TableWatermarks>,
) -> BTreeMap<TableId, ReadTableWatermark> {
    safe_epoch_watermarks
        .iter()
        .map(|(table_id, watermarks)| {
            assert_eq!(watermarks.watermarks.len(), 1);
            let vnode_watermarks = &watermarks.watermarks.first().expect("should exist").1;
            let mut vnode_watermark_map = BTreeMap::new();
            for vnode_watermark in vnode_watermarks.iter() {
                let watermark = Bytes::copy_from_slice(vnode_watermark.watermark());
                for vnode in vnode_watermark.vnode_bitmap().iter_vnodes() {
                    assert!(
                        vnode_watermark_map
                            .insert(vnode, watermark.clone())
                            .is_none(),
                        "duplicate table watermark on vnode {}",
                        vnode.to_index()
                    );
                }
            }
            (
                TableId::from(*table_id),
                ReadTableWatermark {
                    direction: watermarks.direction,
                    vnode_watermarks: vnode_watermark_map,
                },
            )
        })
        .collect()
}

impl HummockVersion {
    pub fn count_new_ssts_in_group_split(
        &self,
        parent_group_id: CompactionGroupId,
        member_table_ids: HashSet<StateTableId>,
    ) -> u64 {
        self.levels
            .get(&parent_group_id)
            .map_or(0, |parent_levels| {
                parent_levels
                    .l0
                    .sub_levels
                    .iter()
                    .chain(parent_levels.levels.iter())
                    .flat_map(|level| &level.table_infos)
                    .map(|sst_info| {
                        // `sst_info.table_ids` will never be empty.
                        for table_id in &sst_info.table_ids {
                            if member_table_ids.contains(table_id) {
                                return 2;
                            }
                        }
                        0
                    })
                    .sum()
            })
    }

    pub fn init_with_parent_group(
        &mut self,
        parent_group_id: CompactionGroupId,
        group_id: CompactionGroupId,
        member_table_ids: BTreeSet<StateTableId>,
        new_sst_start_id: u64,
    ) {
        let mut new_sst_id = new_sst_start_id;
        if parent_group_id == StaticCompactionGroupId::NewCompactionGroup as CompactionGroupId {
            if new_sst_start_id != 0 {
                if cfg!(debug_assertions) {
                    panic!(
                        "non-zero sst start id {} for NewCompactionGroup",
                        new_sst_start_id
                    );
                } else {
                    warn!(
                        new_sst_start_id,
                        "non-zero sst start id for NewCompactionGroup"
                    );
                }
            }
            return;
        } else if !self.levels.contains_key(&parent_group_id) {
            warn!(parent_group_id, "non-existing parent group id to init from");
            return;
        }
        let [parent_levels, cur_levels] = self
            .levels
            .get_many_mut([&parent_group_id, &group_id])
            .unwrap();
        let l0 = &mut parent_levels.l0;
        {
            for sub_level in &mut l0.sub_levels {
                let target_l0 = &mut cur_levels.l0;
                // Remove SST from sub level may result in empty sub level. It will be purged
                // whenever another compaction task is finished.
                let insert_table_infos =
                    split_sst_info_for_level(&member_table_ids, sub_level, &mut new_sst_id);
                sub_level
                    .table_infos
                    .extract_if(|sst_info| sst_info.table_ids.is_empty())
                    .for_each(|sst_info| {
                        sub_level.total_file_size -= sst_info.sst_size;
                        sub_level.uncompressed_file_size -= sst_info.uncompressed_file_size;
                        l0.total_file_size -= sst_info.sst_size;
                        l0.uncompressed_file_size -= sst_info.uncompressed_file_size;
                    });
                if insert_table_infos.is_empty() {
                    continue;
                }
                match get_sub_level_insert_hint(&target_l0.sub_levels, sub_level) {
                    Ok(idx) => {
                        add_ssts_to_sub_level(target_l0, idx, insert_table_infos);
                    }
                    Err(idx) => {
                        insert_new_sub_level(
                            target_l0,
                            sub_level.sub_level_id,
                            sub_level.level_type,
                            insert_table_infos,
                            Some(idx),
                        );
                    }
                }
            }
        }
        for (idx, level) in parent_levels.levels.iter_mut().enumerate() {
            let insert_table_infos =
                split_sst_info_for_level(&member_table_ids, level, &mut new_sst_id);
            cur_levels.levels[idx].total_file_size += insert_table_infos
                .iter()
                .map(|sst| sst.sst_size)
                .sum::<u64>();
            cur_levels.levels[idx].uncompressed_file_size += insert_table_infos
                .iter()
                .map(|sst| sst.uncompressed_file_size)
                .sum::<u64>();
            cur_levels.levels[idx]
                .table_infos
                .extend(insert_table_infos);
            cur_levels.levels[idx]
                .table_infos
                .sort_by(|sst1, sst2| sst1.key_range.cmp(&sst2.key_range));
            assert!(can_concat(&cur_levels.levels[idx].table_infos));
            level
                .table_infos
                .extract_if(|sst_info| sst_info.table_ids.is_empty())
                .for_each(|sst_info| {
                    level.total_file_size -= sst_info.sst_size;
                    level.uncompressed_file_size -= sst_info.uncompressed_file_size;
                });
        }
    }

    pub fn build_sst_delta_infos(&self, version_delta: &HummockVersionDelta) -> Vec<SstDeltaInfo> {
        let mut infos = vec![];

        // Skip trivial move delta for refiller
        // The trivial move task only changes the position of the sst in the lsm, it does not modify the object information corresponding to the sst, and does not need to re-execute the refill.
        if version_delta.trivial_move {
            return infos;
        }

        for (group_id, group_deltas) in &version_delta.group_deltas {
            let mut info = SstDeltaInfo::default();

            let mut removed_l0_ssts: BTreeSet<u64> = BTreeSet::new();
            let mut removed_ssts: BTreeMap<u32, BTreeSet<u64>> = BTreeMap::new();

            // Build only if all deltas are intra level deltas.
            if !group_deltas
                .group_deltas
                .iter()
                .all(|delta| matches!(delta, GroupDelta::IntraLevel(_)))
            {
                continue;
            }

            // TODO(MrCroxx): At most one insert delta is allowed here. It's okay for now with the
            // current `hummock::manager::gen_version_delta` implementation. Better refactor the
            // struct to reduce conventions.
            for group_delta in &group_deltas.group_deltas {
                if let GroupDelta::IntraLevel(intra_level) = group_delta {
                    if !intra_level.inserted_table_infos.is_empty() {
                        info.insert_sst_level = intra_level.level_idx;
                        info.insert_sst_infos
                            .extend(intra_level.inserted_table_infos.iter().cloned());
                    }
                    if !intra_level.removed_table_ids.is_empty() {
                        for id in &intra_level.removed_table_ids {
                            if intra_level.level_idx == 0 {
                                removed_l0_ssts.insert(*id);
                            } else {
                                removed_ssts
                                    .entry(intra_level.level_idx)
                                    .or_default()
                                    .insert(*id);
                            }
                        }
                    }
                }
            }

            let group = self.levels.get(group_id).unwrap();
            for l0_sub_level in &group.level0().sub_levels {
                for sst_info in &l0_sub_level.table_infos {
                    if removed_l0_ssts.remove(&sst_info.sst_id) {
                        info.delete_sst_object_ids.push(sst_info.object_id);
                    }
                }
            }
            for level in &group.levels {
                if let Some(mut removed_level_ssts) = removed_ssts.remove(&level.level_idx) {
                    for sst_info in &level.table_infos {
                        if removed_level_ssts.remove(&sst_info.sst_id) {
                            info.delete_sst_object_ids.push(sst_info.object_id);
                        }
                    }
                    if !removed_level_ssts.is_empty() {
                        tracing::error!(
                            "removed_level_ssts is not empty: {:?}",
                            removed_level_ssts,
                        );
                    }
                    debug_assert!(removed_level_ssts.is_empty());
                }
            }

            if !removed_l0_ssts.is_empty() || !removed_ssts.is_empty() {
                tracing::error!(
                    "not empty removed_l0_ssts: {:?}, removed_ssts: {:?}",
                    removed_l0_ssts,
                    removed_ssts
                );
            }
            debug_assert!(removed_l0_ssts.is_empty());
            debug_assert!(removed_ssts.is_empty());

            infos.push(info);
        }

        infos
    }

    pub fn apply_version_delta(&mut self, version_delta: &HummockVersionDelta) {
        assert_eq!(self.id, version_delta.prev_id);

        let (changed_table_info, mut is_commit_epoch) = self.state_table_info.apply_delta(
            &version_delta.state_table_info_delta,
            &version_delta.removed_table_ids,
        );

        if !is_commit_epoch
            && self.visible_table_committed_epoch() < version_delta.visible_table_committed_epoch()
        {
            is_commit_epoch = true;
            tracing::trace!("max committed epoch bumped but no table committed epoch is changed");
        }

        // apply to `levels`, which is different compaction groups
        for (compaction_group_id, group_deltas) in &version_delta.group_deltas {
            let summary = summarize_group_deltas(group_deltas, *compaction_group_id);
            if let Some(group_construct) = &summary.group_construct {
                let mut new_levels = build_initial_compaction_group_levels(
                    *compaction_group_id,
                    group_construct.get_group_config().unwrap(),
                );
                let parent_group_id = group_construct.parent_group_id;
                new_levels.parent_group_id = parent_group_id;
                #[expect(deprecated)]
                // for backward-compatibility of previous hummock version delta
                new_levels
                    .member_table_ids
                    .clone_from(&group_construct.table_ids);
                self.levels.insert(*compaction_group_id, new_levels);
                let member_table_ids =
                    if group_construct.version >= CompatibilityVersion::NoMemberTableIds as _ {
                        self.state_table_info
                            .compaction_group_member_table_ids(*compaction_group_id)
                            .iter()
                            .map(|table_id| table_id.table_id)
                            .collect()
                    } else {
                        #[expect(deprecated)]
                        // for backward-compatibility of previous hummock version delta
                        BTreeSet::from_iter(group_construct.table_ids.clone())
                    };

                self.init_with_parent_group(
                    parent_group_id,
                    *compaction_group_id,
                    member_table_ids,
                    group_construct.get_new_sst_start_id(),
                );
            } else if let Some(group_merge) = &summary.group_merge {
                tracing::info!(
                    "group_merge left {:?} right {:?}",
                    group_merge.left_group_id,
                    group_merge.right_group_id
                );
                self.merge_compaction_group(group_merge.left_group_id, group_merge.right_group_id)
            }
            let visible_table_committed_epoch = self.visible_table_committed_epoch();
            let group_destroy = summary.group_destroy;
            let levels = self.levels.get_mut(compaction_group_id).unwrap_or_else(|| {
                panic!("compaction group {} does not exist", compaction_group_id)
            });

            assert!(
                visible_table_committed_epoch <= version_delta.visible_table_committed_epoch(),
                "new max commit epoch {} is older than the current max commit epoch {}",
                version_delta.visible_table_committed_epoch(),
                visible_table_committed_epoch
            );
            if is_commit_epoch {
                // `max_committed_epoch` increases. It must be a `commit_epoch`
                let GroupDeltasSummary {
                    delete_sst_levels,
                    delete_sst_ids_set,
                    ..
                } = summary;

                assert!(
                    delete_sst_levels.is_empty() && delete_sst_ids_set.is_empty()
                        || group_destroy.is_some(),
                    "no sst should be deleted when committing an epoch"
                );
                for group_delta in &group_deltas.group_deltas {
                    if let GroupDelta::IntraLevel(IntraLevelDelta {
                        level_idx,
                        l0_sub_level_id,
                        inserted_table_infos,
                        ..
                    }) = group_delta
                    {
                        assert_eq!(
                            *level_idx, 0,
                            "we should only add to L0 when we commit an epoch."
                        );
                        if !inserted_table_infos.is_empty() {
                            insert_new_sub_level(
                                &mut levels.l0,
                                *l0_sub_level_id,
                                PbLevelType::Overlapping,
                                inserted_table_infos.clone(),
                                None,
                            );
                        }
                    }
                }
            } else {
                // `max_committed_epoch` is not changed. The delta is caused by compaction.
                levels.apply_compact_ssts(
                    summary,
                    self.state_table_info
                        .compaction_group_member_table_ids(*compaction_group_id),
                );
            }
            if let Some(destroy_group_id) = &group_destroy {
                self.levels.remove(destroy_group_id);
            }
        }
        self.id = version_delta.id;
        self.set_max_committed_epoch(version_delta.visible_table_committed_epoch());
        self.set_safe_epoch(version_delta.visible_table_safe_epoch());

        // apply to table watermark

        // Store the table watermarks that needs to be updated. None means to remove the table watermark of the table id
        let mut modified_table_watermarks: HashMap<TableId, Option<TableWatermarks>> =
            HashMap::new();

        // apply to table watermark
        for (table_id, table_watermarks) in &version_delta.new_table_watermarks {
            if let Some(current_table_watermarks) = self.table_watermarks.get(table_id) {
                if version_delta.removed_table_ids.contains(table_id) {
                    modified_table_watermarks.insert(*table_id, None);
                } else {
                    let mut current_table_watermarks = (**current_table_watermarks).clone();
                    current_table_watermarks.apply_new_table_watermarks(table_watermarks);
                    modified_table_watermarks.insert(*table_id, Some(current_table_watermarks));
                }
            } else {
                modified_table_watermarks.insert(*table_id, Some(table_watermarks.clone()));
            }
        }
        for (table_id, table_watermarks) in &self.table_watermarks {
            let safe_epoch = if let Some(state_table_info) =
                self.state_table_info.info().get(table_id)
                && let Some((oldest_epoch, _)) = table_watermarks.watermarks.first()
                && state_table_info.safe_epoch > *oldest_epoch
            {
                // safe epoch has progressed, need further clear.
                state_table_info.safe_epoch
            } else {
                // safe epoch not progressed or the table has been removed. No need to truncate
                continue;
            };
            let table_watermarks = modified_table_watermarks
                .entry(*table_id)
                .or_insert_with(|| Some((**table_watermarks).clone()));
            if let Some(table_watermarks) = table_watermarks {
                table_watermarks.clear_stale_epoch_watermark(safe_epoch);
            }
        }
        // apply the staging table watermark to hummock version
        for (table_id, table_watermarks) in modified_table_watermarks {
            if let Some(table_watermarks) = table_watermarks {
                self.table_watermarks
                    .insert(table_id, Arc::new(table_watermarks));
            } else {
                self.table_watermarks.remove(&table_id);
            }
        }

        // apply to table change log
        for (table_id, change_log_delta) in &version_delta.change_log_delta {
            let new_change_log = change_log_delta.new_log.as_ref().unwrap();
            match self.table_change_log.entry(*table_id) {
                Entry::Occupied(entry) => {
                    let change_log = entry.into_mut();
                    if let Some(prev_log) = change_log.0.last() {
                        assert!(
                            prev_log.epochs.last().expect("non-empty")
                                < new_change_log.epochs.first().expect("non-empty")
                        );
                    }
                    change_log.0.push(new_change_log.clone());
                }
                Entry::Vacant(entry) => {
                    entry.insert(TableChangeLogCommon(vec![new_change_log.clone()]));
                }
            };
        }

        // If a table has no new change log entry (even an empty one), it means we have stopped maintained
        // the change log for the table, and then we will remove the table change log.
        // The table change log will also be removed when the table id is removed.
        self.table_change_log.retain(|table_id, _| {
            if version_delta.removed_table_ids.contains(table_id) {
                return false;
            }
            if let Some(table_info_delta) = version_delta.state_table_info_delta.get(table_id)
                && let Some(Some(prev_table_info)) = changed_table_info.get(table_id) && table_info_delta.committed_epoch > prev_table_info.committed_epoch {
                // the table exists previously, and its committed epoch has progressed.
            } else {
                // otherwise, the table change log should be kept anyway
                return true;
            }
            let contains = version_delta.change_log_delta.contains_key(table_id);
            if !contains {
                warn!(
                        ?table_id,
                        max_committed_epoch = version_delta.visible_table_committed_epoch(),
                        "table change log dropped due to no further change log at newly committed epoch",
                    );
            }
            contains
        });

        // truncate the remaining table change log
        for (table_id, change_log_delta) in &version_delta.change_log_delta {
            if let Some(change_log) = self.table_change_log.get_mut(table_id) {
                change_log.truncate(change_log_delta.truncate_epoch);
            }
        }
    }

    pub fn build_branched_sst_info(&self) -> BTreeMap<HummockSstableObjectId, BranchedSstInfo> {
        let mut ret: BTreeMap<_, _> = BTreeMap::new();
        for (compaction_group_id, group) in &self.levels {
            let mut levels = vec![];
            levels.extend(group.l0.sub_levels.iter());
            levels.extend(group.levels.iter());
            for level in levels {
                for table_info in &level.table_infos {
                    if table_info.sst_id == table_info.object_id {
                        continue;
                    }
                    let object_id = table_info.object_id;
                    let entry: &mut BranchedSstInfo = ret.entry(object_id).or_default();
                    entry
                        .entry(*compaction_group_id)
                        .or_default()
                        .push(table_info.sst_id)
                }
            }
        }
        ret
    }

    pub fn merge_compaction_group(
        &mut self,
        left_group_id: CompactionGroupId,
        right_group_id: CompactionGroupId,
    ) {
        // Double check
        let left_group_id_table_ids = self
            .state_table_info
            .compaction_group_member_table_ids(left_group_id)
            .iter()
            .map(|table_id| table_id.table_id);
        let right_group_id_table_ids = self
            .state_table_info
            .compaction_group_member_table_ids(right_group_id)
            .iter()
            .map(|table_id| table_id.table_id);

        assert!(left_group_id_table_ids
            .chain(right_group_id_table_ids)
            .is_sorted());

        let total_cg = self.levels.keys().cloned().collect::<Vec<_>>();
        let right_levels = self.levels.remove(&right_group_id).unwrap_or_else(|| {
            panic!(
                "compaction group should exist right {} all {:?}",
                right_group_id, total_cg
            )
        });

        let left_levels = self.levels.get_mut(&left_group_id).unwrap_or_else(|| {
            panic!(
                "compaction group should exist left {} all {:?}",
                left_group_id, total_cg
            )
        });

        group_split::merge_levels(left_levels, right_levels);
    }
}

#[easy_ext::ext(HummockLevelsExt)]
impl Levels {
    pub fn apply_compact_ssts(
        &mut self,
        summary: GroupDeltasSummary,
        member_table_ids: &BTreeSet<TableId>,
    ) {
        let GroupDeltasSummary {
            delete_sst_levels,
            delete_sst_ids_set,
            insert_sst_level_id,
            insert_sub_level_id,
            insert_table_infos,
            new_vnode_partition_count,
            ..
        } = summary;

        if !self.check_deleted_sst_exist(&delete_sst_levels, delete_sst_ids_set.clone()) {
            warn!(
                "This VersionDelta may be committed by an expired compact task. Please check it. \n
                    delete_sst_levels: {:?}\n,
                    delete_sst_ids_set: {:?}\n,
                    insert_sst_level_id: {}\n,
                    insert_sub_level_id: {}\n,
                    insert_table_infos: {:?}\n",
                delete_sst_levels,
                delete_sst_ids_set,
                insert_sst_level_id,
                insert_sub_level_id,
                insert_table_infos
                    .iter()
                    .map(|sst| (sst.sst_id, sst.object_id))
                    .collect_vec()
            );
            return;
        }
        for level_idx in &delete_sst_levels {
            if *level_idx == 0 {
                for level in &mut self.l0.sub_levels {
                    level_delete_ssts(level, &delete_sst_ids_set);
                }
            } else {
                let idx = *level_idx as usize - 1;
                level_delete_ssts(&mut self.levels[idx], &delete_sst_ids_set);
            }
        }

        if !insert_table_infos.is_empty() {
            if insert_sst_level_id == 0 {
                let l0 = &mut self.l0;
                let index = l0
                    .sub_levels
                    .partition_point(|level| level.sub_level_id < insert_sub_level_id);
                assert!(
                    index < l0.sub_levels.len() && l0.sub_levels[index].sub_level_id == insert_sub_level_id,
                    "should find the level to insert into when applying compaction generated delta. sub level idx: {},  removed sst ids: {:?}, sub levels: {:?},",
                    insert_sub_level_id, delete_sst_ids_set, l0.sub_levels.iter().map(|level| level.sub_level_id).collect_vec()
                );
                if l0.sub_levels[index].table_infos.is_empty()
                    && member_table_ids.len() == 1
                    && insert_table_infos.iter().all(|sst| {
                        sst.table_ids.len() == 1
                            && sst.table_ids[0]
                                == member_table_ids.iter().next().expect("non-empty").table_id
                    })
                {
                    // Only change vnode_partition_count for group which has only one state-table.
                    // Only change vnode_partition_count for level which update all sst files in this compact task.
                    l0.sub_levels[index].vnode_partition_count = new_vnode_partition_count;
                }
                level_insert_ssts(&mut l0.sub_levels[index], insert_table_infos);
            } else {
                let idx = insert_sst_level_id as usize - 1;
                if self.levels[idx].table_infos.is_empty()
                    && insert_table_infos
                        .iter()
                        .all(|sst| sst.table_ids.len() == 1)
                {
                    self.levels[idx].vnode_partition_count = new_vnode_partition_count;
                } else if self.levels[idx].vnode_partition_count != 0
                    && new_vnode_partition_count == 0
                    && member_table_ids.len() > 1
                {
                    self.levels[idx].vnode_partition_count = 0;
                }
                level_insert_ssts(&mut self.levels[idx], insert_table_infos);
            }
        }
        if delete_sst_levels.iter().any(|level_id| *level_id == 0) {
            self.l0
                .sub_levels
                .retain(|level| !level.table_infos.is_empty());
            self.l0.total_file_size = self
                .l0
                .sub_levels
                .iter()
                .map(|level| level.total_file_size)
                .sum::<u64>();
            self.l0.uncompressed_file_size = self
                .l0
                .sub_levels
                .iter()
                .map(|level| level.uncompressed_file_size)
                .sum::<u64>();
        }
    }

    pub fn check_deleted_sst_exist(
        &self,
        delete_sst_levels: &[u32],
        mut delete_sst_ids_set: HashSet<u64>,
    ) -> bool {
        for level_idx in delete_sst_levels {
            if *level_idx == 0 {
                for level in &self.l0.sub_levels {
                    level.table_infos.iter().for_each(|table| {
                        delete_sst_ids_set.remove(&table.sst_id);
                    });
                }
            } else {
                let idx = *level_idx as usize - 1;
                self.levels[idx].table_infos.iter().for_each(|table| {
                    delete_sst_ids_set.remove(&table.sst_id);
                });
            }
        }
        delete_sst_ids_set.is_empty()
    }
}

pub fn build_initial_compaction_group_levels(
    group_id: CompactionGroupId,
    compaction_config: &CompactionConfig,
) -> Levels {
    let mut levels = vec![];
    for l in 0..compaction_config.get_max_level() {
        levels.push(Level {
            level_idx: (l + 1) as u32,
            level_type: PbLevelType::Nonoverlapping,
            table_infos: vec![],
            total_file_size: 0,
            sub_level_id: 0,
            uncompressed_file_size: 0,
            vnode_partition_count: 0,
        });
    }
    #[expect(deprecated)] // for backward-compatibility of previous hummock version delta
    Levels {
        levels,
        l0: OverlappingLevel {
            sub_levels: vec![],
            total_file_size: 0,
            uncompressed_file_size: 0,
        },
        group_id,
        parent_group_id: StaticCompactionGroupId::NewCompactionGroup as _,
        member_table_ids: vec![],
    }
}

fn split_sst_info_for_level(
    member_table_ids: &BTreeSet<u32>,
    level: &mut Level,
    new_sst_id: &mut u64,
) -> Vec<SstableInfo> {
    // Remove SST from sub level may result in empty sub level. It will be purged
    // whenever another compaction task is finished.
    let mut insert_table_infos = vec![];
    for sst_info in &mut level.table_infos {
        let removed_table_ids = sst_info
            .table_ids
            .iter()
            .filter(|table_id| member_table_ids.contains(table_id))
            .cloned()
            .collect_vec();
        if !removed_table_ids.is_empty() {
            let branch_sst = split_sst(
                sst_info,
                new_sst_id,
                sst_info.sst_size / 2,
                sst_info.sst_size / 2,
                member_table_ids.iter().cloned().collect_vec(),
            );
            insert_table_infos.push(branch_sst);
        }
    }
    insert_table_infos
}

/// Gets all compaction group ids.
pub fn get_compaction_group_ids(
    version: &HummockVersion,
) -> impl Iterator<Item = CompactionGroupId> + '_ {
    version.levels.keys().cloned()
}

pub fn get_table_compaction_group_id_mapping(
    version: &HummockVersion,
) -> HashMap<StateTableId, CompactionGroupId> {
    version
        .state_table_info
        .info()
        .iter()
        .map(|(table_id, info)| (table_id.table_id, info.compaction_group_id))
        .collect()
}

/// Gets all SSTs in `group_id`
pub fn get_compaction_group_ssts(
    version: &HummockVersion,
    group_id: CompactionGroupId,
) -> impl Iterator<Item = (HummockSstableObjectId, HummockSstableId)> + '_ {
    let group_levels = version.get_compaction_group_levels(group_id);
    group_levels
        .l0
        .sub_levels
        .iter()
        .rev()
        .chain(group_levels.levels.iter())
        .flat_map(|level| {
            level
                .table_infos
                .iter()
                .map(|table_info| (table_info.object_id, table_info.sst_id))
        })
}

pub fn new_sub_level(
    sub_level_id: u64,
    level_type: PbLevelType,
    table_infos: Vec<SstableInfo>,
) -> Level {
    if level_type == PbLevelType::Nonoverlapping {
        debug_assert!(
            can_concat(&table_infos),
            "sst of non-overlapping level is not concat-able: {:?}",
            table_infos
        );
    }
    let total_file_size = table_infos.iter().map(|table| table.sst_size).sum();
    let uncompressed_file_size = table_infos
        .iter()
        .map(|table| table.uncompressed_file_size)
        .sum();
    Level {
        level_idx: 0,
        level_type,
        table_infos,
        total_file_size,
        sub_level_id,
        uncompressed_file_size,
        vnode_partition_count: 0,
    }
}

pub fn add_ssts_to_sub_level(
    l0: &mut OverlappingLevel,
    sub_level_idx: usize,
    insert_table_infos: Vec<SstableInfo>,
) {
    insert_table_infos.iter().for_each(|sst| {
        l0.sub_levels[sub_level_idx].total_file_size += sst.sst_size;
        l0.sub_levels[sub_level_idx].uncompressed_file_size += sst.uncompressed_file_size;
        l0.total_file_size += sst.sst_size;
        l0.uncompressed_file_size += sst.uncompressed_file_size;
    });
    l0.sub_levels[sub_level_idx]
        .table_infos
        .extend(insert_table_infos);
    if l0.sub_levels[sub_level_idx].level_type == PbLevelType::Nonoverlapping {
        l0.sub_levels[sub_level_idx]
            .table_infos
            .sort_by(|sst1, sst2| sst1.key_range.cmp(&sst2.key_range));
        assert!(
            can_concat(&l0.sub_levels[sub_level_idx].table_infos),
            "sstable ids: {:?}",
            l0.sub_levels[sub_level_idx]
                .table_infos
                .iter()
                .map(|sst| sst.sst_id)
                .collect_vec()
        );
    }
}

/// `None` value of `sub_level_insert_hint` means append.
pub fn insert_new_sub_level(
    l0: &mut OverlappingLevel,
    insert_sub_level_id: u64,
    level_type: PbLevelType,
    insert_table_infos: Vec<SstableInfo>,
    sub_level_insert_hint: Option<usize>,
) {
    if insert_sub_level_id == u64::MAX {
        return;
    }
    let insert_pos = if let Some(insert_pos) = sub_level_insert_hint {
        insert_pos
    } else {
        if let Some(newest_level) = l0.sub_levels.last() {
            assert!(
                newest_level.sub_level_id < insert_sub_level_id,
                "inserted new level is not the newest: prev newest: {}, insert: {}. L0: {:?}",
                newest_level.sub_level_id,
                insert_sub_level_id,
                l0,
            );
        }
        l0.sub_levels.len()
    };
    #[cfg(debug_assertions)]
    {
        if insert_pos > 0 {
            if let Some(smaller_level) = l0.sub_levels.get(insert_pos - 1) {
                debug_assert!(smaller_level.sub_level_id < insert_sub_level_id);
            }
        }
        if let Some(larger_level) = l0.sub_levels.get(insert_pos) {
            debug_assert!(larger_level.sub_level_id > insert_sub_level_id);
        }
    }
    // All files will be committed in one new Overlapping sub-level and become
    // Nonoverlapping  after at least one compaction.
    let level = new_sub_level(insert_sub_level_id, level_type, insert_table_infos);
    l0.total_file_size += level.total_file_size;
    l0.uncompressed_file_size += level.uncompressed_file_size;
    l0.sub_levels.insert(insert_pos, level);
}

/// Delete sstables if the table id is in the id set.
///
/// Return `true` if some sst is deleted, and `false` is the deletion is trivial
fn level_delete_ssts(
    operand: &mut Level,
    delete_sst_ids_superset: &HashSet<HummockSstableId>,
) -> bool {
    let original_len = operand.table_infos.len();
    operand
        .table_infos
        .retain(|table| !delete_sst_ids_superset.contains(&table.sst_id));
    operand.total_file_size = operand
        .table_infos
        .iter()
        .map(|table| table.sst_size)
        .sum::<u64>();
    operand.uncompressed_file_size = operand
        .table_infos
        .iter()
        .map(|table| table.uncompressed_file_size)
        .sum::<u64>();
    original_len != operand.table_infos.len()
}

fn level_insert_ssts(operand: &mut Level, insert_table_infos: Vec<SstableInfo>) {
    operand.total_file_size += insert_table_infos
        .iter()
        .map(|sst| sst.sst_size)
        .sum::<u64>();
    operand.uncompressed_file_size += insert_table_infos
        .iter()
        .map(|sst| sst.uncompressed_file_size)
        .sum::<u64>();
    operand.table_infos.extend(insert_table_infos);
    operand
        .table_infos
        .sort_by(|sst1, sst2| sst1.key_range.cmp(&sst2.key_range));
    if operand.level_type == PbLevelType::Overlapping {
        operand.level_type = PbLevelType::Nonoverlapping;
    }
    assert!(
        can_concat(&operand.table_infos),
        "sstable ids: {:?}",
        operand
            .table_infos
            .iter()
            .map(|sst| sst.sst_id)
            .collect_vec()
    );
}

pub fn object_size_map(version: &HummockVersion) -> HashMap<HummockSstableObjectId, u64> {
    version
        .levels
        .values()
        .flat_map(|cg| {
            cg.level0()
                .sub_levels
                .iter()
                .chain(cg.levels.iter())
                .flat_map(|level| level.table_infos.iter().map(|t| (t.object_id, t.file_size)))
        })
        .chain(version.table_change_log.values().flat_map(|c| {
            c.0.iter().flat_map(|l| {
                l.old_value
                    .iter()
                    .chain(l.new_value.iter())
                    .map(|t| (t.object_id, t.file_size))
            })
        }))
        .collect()
}

/// Verify the validity of a `HummockVersion` and return a list of violations if any.
/// Currently this method is only used by risectl validate-version.
pub fn validate_version(version: &HummockVersion) -> Vec<String> {
    let mut res = Vec::new();

    // Ensure safe_epoch <= max_committed_epoch
    if version.visible_table_safe_epoch() > version.visible_table_committed_epoch() {
        res.push(format!(
            "VERSION: safe_epoch {} > max_committed_epoch {}",
            version.visible_table_safe_epoch(),
            version.visible_table_committed_epoch()
        ));
    }

    // Ensure each table maps to only one compaction group
    for (group_id, levels) in &version.levels {
        // Ensure compaction group id matches
        if levels.group_id != *group_id {
            res.push(format!(
                "GROUP {}: inconsistent group id {} in Levels",
                group_id, levels.group_id
            ));
        }

        let validate_level = |group: CompactionGroupId,
                              expected_level_idx: u32,
                              level: &Level,
                              res: &mut Vec<String>| {
            let mut level_identifier = format!("GROUP {} LEVEL {}", group, level.level_idx);
            if level.level_idx == 0 {
                level_identifier.push_str(format!("SUBLEVEL {}", level.sub_level_id).as_str());
                // Ensure sub-level is not empty
                if level.table_infos.is_empty() {
                    res.push(format!("{}: empty level", level_identifier));
                }
            } else if level.level_type != PbLevelType::Nonoverlapping {
                // Ensure non-L0 level is non-overlapping level
                res.push(format!(
                    "{}: level type {:?} is not non-overlapping",
                    level_identifier, level.level_type
                ));
            }

            // Ensure level idx matches
            if level.level_idx != expected_level_idx {
                res.push(format!(
                    "{}: mismatched level idx {}",
                    level_identifier, expected_level_idx
                ));
            }

            let mut prev_table_info: Option<&SstableInfo> = None;
            for table_info in &level.table_infos {
                // Ensure table_ids are sorted and unique
                if !table_info.table_ids.is_sorted_by(|a, b| a < b) {
                    res.push(format!(
                        "{} SST {}: table_ids not sorted",
                        level_identifier, table_info.object_id
                    ));
                }

                // Ensure SSTs in non-overlapping level have non-overlapping key range
                if level.level_type == PbLevelType::Nonoverlapping {
                    if let Some(prev) = prev_table_info.take() {
                        if prev
                            .key_range
                            .compare_right_with(&table_info.key_range.left)
                            != Ordering::Less
                        {
                            res.push(format!(
                                "{} SST {}: key range should not overlap. prev={:?}, cur={:?}",
                                level_identifier, table_info.object_id, prev, table_info
                            ));
                        }
                    }
                    let _ = prev_table_info.insert(table_info);
                }
            }
        };

        let l0 = &levels.l0;
        let mut prev_sub_level_id = u64::MAX;
        for sub_level in &l0.sub_levels {
            // Ensure sub_level_id is sorted and unique
            if sub_level.sub_level_id >= prev_sub_level_id {
                res.push(format!(
                    "GROUP {} LEVEL 0: sub_level_id {} >= prev_sub_level {}",
                    group_id, sub_level.level_idx, prev_sub_level_id
                ));
            }
            prev_sub_level_id = sub_level.sub_level_id;

            validate_level(*group_id, 0, sub_level, &mut res);
        }

        for idx in 1..=levels.levels.len() {
            validate_level(*group_id, idx as u32, levels.get_level(idx), &mut res);
        }
    }
    res
}

pub fn split_sst(
    sst_info: &mut SstableInfo,
    new_sst_id: &mut u64,
    old_sst_size: u64,
    new_sst_size: u64,
    new_table_ids: Vec<u32>,
) -> SstableInfo {
    let mut branch_table_info = sst_info.clone();
    branch_table_info.sst_id = *new_sst_id;
    branch_table_info.sst_size = new_sst_size;
    *new_sst_id += 1;

    sst_info.sst_id = *new_sst_id;
    sst_info.sst_size = old_sst_size;
    *new_sst_id += 1;

    {
        // related github.com/risingwavelabs/risingwave/pull/17898/
        // This is a temporary implementation that will update `table_ids`` based on the new split rule after PR 17898
        // sst_info.table_ids = vec[1, 2, 3];
        // new_table_ids = vec[2, 3, 4];
        // branch_table_info.table_ids = vec[1, 2, 3] ∩ vec[2, 3, 4] = vec[2, 3]
        let set1: BTreeSet<_> = sst_info.table_ids.iter().cloned().collect();
        let set2: BTreeSet<_> = new_table_ids.into_iter().collect();
        let intersection: Vec<_> = set1.intersection(&set2).cloned().collect();

        // Update table_ids
        branch_table_info.table_ids = intersection;
        sst_info
            .table_ids
            .retain(|table_id| !branch_table_info.table_ids.contains(table_id));
    }

    branch_table_info
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bytes::Bytes;
    use risingwave_common::catalog::TableId;
    use risingwave_common::hash::VirtualNode;
    use risingwave_pb::hummock::{CompactionConfig, GroupConstruct, GroupDestroy, LevelType};

    use crate::compaction_group::group_split;
    use crate::compaction_group::hummock_version_ext::build_initial_compaction_group_levels;
    use crate::key::{gen_key_from_str, FullKey};
    use crate::key_range::KeyRange;
    use crate::level::{Level, Levels, OverlappingLevel};
    use crate::sstable_info::SstableInfo;
    use crate::version::{
        GroupDelta, GroupDeltas, HummockVersion, HummockVersionDelta, IntraLevelDelta,
    };
    use crate::HummockVersionId;

    #[test]
    fn test_get_sst_object_ids() {
        let mut version = HummockVersion {
            id: HummockVersionId::new(0),
            levels: HashMap::from_iter([(
                0,
                Levels {
                    levels: vec![],
                    l0: OverlappingLevel {
                        sub_levels: vec![],
                        total_file_size: 0,
                        uncompressed_file_size: 0,
                    },
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert_eq!(version.get_object_ids().len(), 0);

        // Add to sub level
        version
            .levels
            .get_mut(&0)
            .unwrap()
            .l0
            .sub_levels
            .push(Level {
                table_infos: vec![SstableInfo {
                    object_id: 11,
                    sst_id: 11,
                    ..Default::default()
                }],
                ..Default::default()
            });
        assert_eq!(version.get_object_ids().len(), 1);

        // Add to non sub level
        version.levels.get_mut(&0).unwrap().levels.push(Level {
            table_infos: vec![SstableInfo {
                object_id: 22,
                sst_id: 22,
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(version.get_object_ids().len(), 2);
    }

    #[test]
    fn test_apply_version_delta() {
        let mut version = HummockVersion {
            id: HummockVersionId::new(0),
            levels: HashMap::from_iter([
                (
                    0,
                    build_initial_compaction_group_levels(
                        0,
                        &CompactionConfig {
                            max_level: 6,
                            ..Default::default()
                        },
                    ),
                ),
                (
                    1,
                    build_initial_compaction_group_levels(
                        1,
                        &CompactionConfig {
                            max_level: 6,
                            ..Default::default()
                        },
                    ),
                ),
            ]),
            ..Default::default()
        };
        let version_delta = HummockVersionDelta {
            id: HummockVersionId::new(1),
            group_deltas: HashMap::from_iter([
                (
                    2,
                    GroupDeltas {
                        group_deltas: vec![GroupDelta::GroupConstruct(GroupConstruct {
                            group_config: Some(CompactionConfig {
                                max_level: 6,
                                ..Default::default()
                            }),
                            ..Default::default()
                        })],
                    },
                ),
                (
                    0,
                    GroupDeltas {
                        group_deltas: vec![GroupDelta::GroupDestroy(GroupDestroy {})],
                    },
                ),
                (
                    1,
                    GroupDeltas {
                        group_deltas: vec![GroupDelta::IntraLevel(IntraLevelDelta::new(
                            1,
                            0,
                            vec![],
                            vec![SstableInfo {
                                object_id: 1,
                                sst_id: 1,
                                ..Default::default()
                            }],
                            0,
                        ))],
                    },
                ),
            ]),
            ..Default::default()
        };
        let version_delta = version_delta;

        version.apply_version_delta(&version_delta);
        let mut cg1 = build_initial_compaction_group_levels(
            1,
            &CompactionConfig {
                max_level: 6,
                ..Default::default()
            },
        );
        cg1.levels[0] = Level {
            level_idx: 1,
            level_type: LevelType::Nonoverlapping,
            table_infos: vec![SstableInfo {
                object_id: 1,
                sst_id: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            version,
            HummockVersion {
                id: HummockVersionId::new(1),
                levels: HashMap::from_iter([
                    (
                        2,
                        build_initial_compaction_group_levels(
                            2,
                            &CompactionConfig {
                                max_level: 6,
                                ..Default::default()
                            },
                        ),
                    ),
                    (1, cg1),
                ]),
                ..Default::default()
            }
        );
    }

    fn gen_sst_info(object_id: u64, table_ids: Vec<u32>, left: Bytes, right: Bytes) -> SstableInfo {
        SstableInfo {
            object_id,
            sst_id: object_id,
            key_range: KeyRange {
                left,
                right,
                right_exclusive: false,
            },
            table_ids,
            file_size: 100,
            sst_size: 100,
            uncompressed_file_size: 100,
            ..Default::default()
        }
    }

    #[test]
    fn test_merge_levels() {
        let mut left_levels = build_initial_compaction_group_levels(
            1,
            &CompactionConfig {
                max_level: 6,
                ..Default::default()
            },
        );

        let mut right_levels = build_initial_compaction_group_levels(
            2,
            &CompactionConfig {
                max_level: 6,
                ..Default::default()
            },
        );

        left_levels.levels[0] = Level {
            level_idx: 1,
            level_type: LevelType::Nonoverlapping,
            table_infos: vec![
                gen_sst_info(
                    1,
                    vec![3],
                    FullKey::for_test(
                        TableId::new(3),
                        gen_key_from_str(VirtualNode::from_index(1), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                    FullKey::for_test(
                        TableId::new(3),
                        gen_key_from_str(VirtualNode::from_index(200), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                ),
                gen_sst_info(
                    10,
                    vec![3, 4],
                    FullKey::for_test(
                        TableId::new(3),
                        gen_key_from_str(VirtualNode::from_index(201), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                    FullKey::for_test(
                        TableId::new(4),
                        gen_key_from_str(VirtualNode::from_index(10), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                ),
                gen_sst_info(
                    11,
                    vec![4],
                    FullKey::for_test(
                        TableId::new(4),
                        gen_key_from_str(VirtualNode::from_index(11), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                    FullKey::for_test(
                        TableId::new(4),
                        gen_key_from_str(VirtualNode::from_index(200), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                ),
            ],
            total_file_size: 300,
            ..Default::default()
        };

        left_levels.l0.sub_levels.push(Level {
            level_idx: 0,
            table_infos: vec![gen_sst_info(
                3,
                vec![3],
                FullKey::for_test(
                    TableId::new(3),
                    gen_key_from_str(VirtualNode::from_index(1), "1"),
                    0,
                )
                .encode()
                .into(),
                FullKey::for_test(
                    TableId::new(3),
                    gen_key_from_str(VirtualNode::from_index(200), "1"),
                    0,
                )
                .encode()
                .into(),
            )],
            sub_level_id: 101,
            level_type: LevelType::Overlapping,
            total_file_size: 100,
            ..Default::default()
        });

        left_levels.l0.sub_levels.push(Level {
            level_idx: 0,
            table_infos: vec![gen_sst_info(
                3,
                vec![3],
                FullKey::for_test(
                    TableId::new(3),
                    gen_key_from_str(VirtualNode::from_index(1), "1"),
                    0,
                )
                .encode()
                .into(),
                FullKey::for_test(
                    TableId::new(3),
                    gen_key_from_str(VirtualNode::from_index(200), "1"),
                    0,
                )
                .encode()
                .into(),
            )],
            sub_level_id: 103,
            level_type: LevelType::Overlapping,
            total_file_size: 100,
            ..Default::default()
        });

        left_levels.l0.sub_levels.push(Level {
            level_idx: 0,
            table_infos: vec![gen_sst_info(
                3,
                vec![3],
                FullKey::for_test(
                    TableId::new(3),
                    gen_key_from_str(VirtualNode::from_index(1), "1"),
                    0,
                )
                .encode()
                .into(),
                FullKey::for_test(
                    TableId::new(3),
                    gen_key_from_str(VirtualNode::from_index(200), "1"),
                    0,
                )
                .encode()
                .into(),
            )],
            sub_level_id: 105,
            level_type: LevelType::Nonoverlapping,
            total_file_size: 100,
            ..Default::default()
        });

        right_levels.levels[0] = Level {
            level_idx: 1,
            level_type: LevelType::Nonoverlapping,
            table_infos: vec![
                gen_sst_info(
                    1,
                    vec![5],
                    FullKey::for_test(
                        TableId::new(5),
                        gen_key_from_str(VirtualNode::from_index(1), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                    FullKey::for_test(
                        TableId::new(5),
                        gen_key_from_str(VirtualNode::from_index(200), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                ),
                gen_sst_info(
                    10,
                    vec![5, 6],
                    FullKey::for_test(
                        TableId::new(5),
                        gen_key_from_str(VirtualNode::from_index(201), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                    FullKey::for_test(
                        TableId::new(6),
                        gen_key_from_str(VirtualNode::from_index(10), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                ),
                gen_sst_info(
                    11,
                    vec![6],
                    FullKey::for_test(
                        TableId::new(6),
                        gen_key_from_str(VirtualNode::from_index(11), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                    FullKey::for_test(
                        TableId::new(6),
                        gen_key_from_str(VirtualNode::from_index(200), "1"),
                        0,
                    )
                    .encode()
                    .into(),
                ),
            ],
            total_file_size: 300,
            ..Default::default()
        };

        right_levels.l0.sub_levels.push(Level {
            level_idx: 0,
            table_infos: vec![gen_sst_info(
                3,
                vec![5],
                FullKey::for_test(
                    TableId::new(5),
                    gen_key_from_str(VirtualNode::from_index(1), "1"),
                    0,
                )
                .encode()
                .into(),
                FullKey::for_test(
                    TableId::new(5),
                    gen_key_from_str(VirtualNode::from_index(200), "1"),
                    0,
                )
                .encode()
                .into(),
            )],
            sub_level_id: 101,
            level_type: LevelType::Overlapping,
            total_file_size: 100,
            ..Default::default()
        });

        right_levels.l0.sub_levels.push(Level {
            level_idx: 0,
            table_infos: vec![gen_sst_info(
                5,
                vec![5],
                FullKey::for_test(
                    TableId::new(5),
                    gen_key_from_str(VirtualNode::from_index(1), "1"),
                    0,
                )
                .encode()
                .into(),
                FullKey::for_test(
                    TableId::new(5),
                    gen_key_from_str(VirtualNode::from_index(200), "1"),
                    0,
                )
                .encode()
                .into(),
            )],
            sub_level_id: 102,
            level_type: LevelType::Overlapping,
            total_file_size: 100,
            ..Default::default()
        });

        right_levels.l0.sub_levels.push(Level {
            level_idx: 0,
            table_infos: vec![gen_sst_info(
                3,
                vec![5],
                FullKey::for_test(
                    TableId::new(5),
                    gen_key_from_str(VirtualNode::from_index(1), "1"),
                    0,
                )
                .encode()
                .into(),
                FullKey::for_test(
                    TableId::new(5),
                    gen_key_from_str(VirtualNode::from_index(200), "1"),
                    0,
                )
                .encode()
                .into(),
            )],
            sub_level_id: 103,
            level_type: LevelType::Nonoverlapping,
            total_file_size: 100,
            ..Default::default()
        });

        {
            // test empty
            let mut left_levels = Levels::default();
            let right_levels = Levels::default();

            group_split::merge_levels(&mut left_levels, right_levels);
        }

        {
            // test empty left
            let mut left_levels = build_initial_compaction_group_levels(
                1,
                &CompactionConfig {
                    max_level: 6,
                    ..Default::default()
                },
            );
            let right_levels = right_levels.clone();

            group_split::merge_levels(&mut left_levels, right_levels);

            assert!(left_levels.l0.sub_levels.len() == 3);
            assert!(left_levels.l0.sub_levels[0].sub_level_id == 101);
            assert_eq!(100, left_levels.l0.sub_levels[0].total_file_size);
            assert!(left_levels.l0.sub_levels[1].sub_level_id == 102);
            assert_eq!(100, left_levels.l0.sub_levels[1].total_file_size);
            assert!(left_levels.l0.sub_levels[2].sub_level_id == 103);
            assert_eq!(100, left_levels.l0.sub_levels[2].total_file_size);

            assert!(left_levels.levels[0].level_idx == 1);
            assert_eq!(300, left_levels.levels[0].total_file_size);
        }

        {
            // test empty right
            let mut left_levels = left_levels.clone();
            let right_levels = build_initial_compaction_group_levels(
                2,
                &CompactionConfig {
                    max_level: 6,
                    ..Default::default()
                },
            );

            group_split::merge_levels(&mut left_levels, right_levels);

            assert!(left_levels.l0.sub_levels.len() == 3);
            assert!(left_levels.l0.sub_levels[0].sub_level_id == 101);
            assert_eq!(100, left_levels.l0.sub_levels[0].total_file_size);
            assert!(left_levels.l0.sub_levels[1].sub_level_id == 103);
            assert_eq!(100, left_levels.l0.sub_levels[1].total_file_size);
            assert!(left_levels.l0.sub_levels[2].sub_level_id == 105);
            assert_eq!(100, left_levels.l0.sub_levels[2].total_file_size);

            assert!(left_levels.levels[0].level_idx == 1);
            assert_eq!(300, left_levels.levels[0].total_file_size);
        }

        {
            let mut left_levels = left_levels.clone();
            let right_levels = right_levels.clone();

            group_split::merge_levels(&mut left_levels, right_levels);

            assert!(left_levels.l0.sub_levels.len() == 6);
            assert!(left_levels.l0.sub_levels[0].sub_level_id == 101);
            assert_eq!(100, left_levels.l0.sub_levels[0].total_file_size);
            assert!(left_levels.l0.sub_levels[1].sub_level_id == 103);
            assert_eq!(100, left_levels.l0.sub_levels[1].total_file_size);
            assert!(left_levels.l0.sub_levels[2].sub_level_id == 105);
            assert_eq!(100, left_levels.l0.sub_levels[2].total_file_size);
            assert!(left_levels.l0.sub_levels[3].sub_level_id == 106);
            assert_eq!(100, left_levels.l0.sub_levels[3].total_file_size);
            assert!(left_levels.l0.sub_levels[4].sub_level_id == 107);
            assert_eq!(100, left_levels.l0.sub_levels[4].total_file_size);
            assert!(left_levels.l0.sub_levels[5].sub_level_id == 108);
            assert_eq!(100, left_levels.l0.sub_levels[5].total_file_size);

            assert!(left_levels.levels[0].level_idx == 1);
            assert_eq!(600, left_levels.levels[0].total_file_size);
        }
    }
}
