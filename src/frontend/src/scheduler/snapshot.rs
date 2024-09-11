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

use std::sync::Arc;

use risingwave_common::util::epoch::{Epoch, INVALID_EPOCH};
use risingwave_pb::common::{batch_query_epoch, BatchQueryEpoch};
use risingwave_pb::hummock::PbHummockSnapshot;
use tokio::sync::watch;

use crate::expr::InlineNowProcTime;
use crate::meta_client::FrontendMetaClient;

/// The storage snapshot to read from in a query, which can be freely cloned.
#[derive(Clone)]
pub enum ReadSnapshot {
    /// A frontend-pinned snapshot.
    FrontendPinned {
        snapshot: PinnedSnapshotRef,
        // It's embedded here because we always use it together with snapshot.
        is_barrier_read: bool,
    },

    /// Other arbitrary epoch, e.g. user specified.
    /// Availability and consistency of underlying data should be guaranteed accordingly.
    /// Currently it's only used for querying meta snapshot backup.
    Other(Epoch),
}

impl ReadSnapshot {
    /// Get the [`BatchQueryEpoch`] for this snapshot.
    pub fn batch_query_epoch(&self) -> BatchQueryEpoch {
        match self {
            ReadSnapshot::FrontendPinned {
                snapshot,
                is_barrier_read,
            } => snapshot.batch_query_epoch(*is_barrier_read),
            ReadSnapshot::Other(e) => BatchQueryEpoch {
                epoch: Some(batch_query_epoch::Epoch::Backup(e.0)),
            },
        }
    }

    pub fn inline_now_proc_time(&self) -> InlineNowProcTime {
        let epoch = match self {
            ReadSnapshot::FrontendPinned { snapshot, .. } => Epoch(snapshot.committed_epoch()),
            ReadSnapshot::Other(epoch) => *epoch,
        };
        InlineNowProcTime::new(epoch)
    }

    /// Returns true if this snapshot is a barrier read.
    pub fn support_barrier_read(&self) -> bool {
        match self {
            ReadSnapshot::FrontendPinned {
                snapshot: _,
                is_barrier_read,
            } => *is_barrier_read,
            ReadSnapshot::Other(_) => false,
        }
    }
}

/// A frontend-pinned snapshot that notifies the [`UnpinWorker`] when it's dropped.
// DO NOT implement `Clone` for `PinnedSnapshot` because it's a "resource" that should always be a
// singleton for each snapshot. Use `PinnedSnapshotRef` instead.
pub struct PinnedSnapshot {
    value: PbHummockSnapshot,
}

impl std::fmt::Debug for PinnedSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

/// A reference to a frontend-pinned snapshot.
pub type PinnedSnapshotRef = Arc<PinnedSnapshot>;

impl PinnedSnapshot {
    fn batch_query_epoch(&self, is_barrier_read: bool) -> BatchQueryEpoch {
        let epoch = if is_barrier_read {
            batch_query_epoch::Epoch::Current(u64::MAX)
        } else {
            batch_query_epoch::Epoch::Committed(self.value.committed_epoch)
        };
        BatchQueryEpoch { epoch: Some(epoch) }
    }

    pub fn committed_epoch(&self) -> u64 {
        self.value.committed_epoch
    }
}

/// Returns an invalid snapshot, used for initial values.
fn invalid_snapshot() -> PbHummockSnapshot {
    PbHummockSnapshot {
        committed_epoch: INVALID_EPOCH,
    }
}

/// Cache of hummock snapshot in meta.
pub struct HummockSnapshotManager {
    /// The latest snapshot synced from the meta service.
    ///
    /// The `max_committed_epoch` and `max_current_epoch` are pushed from meta node to reduce rpc
    /// number.
    ///
    /// We have two epoch(committed and current), We only use `committed_epoch` to pin or unpin,
    /// because `committed_epoch` always less or equal `current_epoch`, and the data with
    /// `current_epoch` is always in the shared buffer, so it will never be gc before the data
    /// of `committed_epoch`.
    latest_snapshot: watch::Sender<PinnedSnapshotRef>,
}

pub type HummockSnapshotManagerRef = Arc<HummockSnapshotManager>;

impl HummockSnapshotManager {
    pub fn new(_meta_client: Arc<dyn FrontendMetaClient>) -> Self {
        let latest_snapshot = Arc::new(PinnedSnapshot {
            value: invalid_snapshot(),
        });

        let (latest_snapshot, _) = watch::channel(latest_snapshot);

        Self { latest_snapshot }
    }

    /// Acquire the latest snapshot by increasing its reference count.
    pub fn acquire(&self) -> PinnedSnapshotRef {
        self.latest_snapshot.borrow().clone()
    }

    /// Update the latest snapshot.
    ///
    /// Should only be called by the observer manager.
    pub fn update(&self, snapshot: PbHummockSnapshot) {
        self.latest_snapshot.send_if_modified(move |old_snapshot| {
            // Note(bugen): theoretically, the snapshots from the observer should always be
            // monotonically increasing, so there's no need to `max` them or check whether they are
            // the same. But we still do it here to be safe.
            // TODO: turn this into an assertion.
            let snapshot = PbHummockSnapshot {
                committed_epoch: std::cmp::max(
                    old_snapshot.value.committed_epoch,
                    snapshot.committed_epoch,
                ),
            };

            if old_snapshot.value == snapshot {
                // Ignore the same snapshot
                false
            } else {
                // Then set the latest snapshot.
                *old_snapshot = Arc::new(PinnedSnapshot { value: snapshot });

                true
            }
        });
    }

    /// Wait until the latest snapshot is newer than the given one.
    pub async fn wait(&self, snapshot: PbHummockSnapshot) {
        let mut rx = self.latest_snapshot.subscribe();
        while rx.borrow_and_update().value.committed_epoch < snapshot.committed_epoch {
            rx.changed().await.unwrap();
        }
    }
}
