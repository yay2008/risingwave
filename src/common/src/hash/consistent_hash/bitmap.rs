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

use std::ops::RangeInclusive;

use crate::bitmap::Bitmap;
use crate::hash::table_distribution::SINGLETON_VNODE;
use crate::hash::VirtualNode;

/// An extension trait for `Bitmap` to support virtual node operations.
#[easy_ext::ext(VnodeBitmapExt)]
impl Bitmap {
    /// Enumerates the virtual nodes set to 1 in the bitmap.
    pub fn iter_vnodes(&self) -> impl Iterator<Item = VirtualNode> + '_ {
        self.iter_ones().map(VirtualNode::from_index)
    }

    /// Enumerates the virtual nodes set to 1 in the bitmap.
    pub fn iter_vnodes_scalar(&self) -> impl Iterator<Item = i16> + '_ {
        self.iter_vnodes().map(|vnode| vnode.to_scalar())
    }

    /// Returns an iterator which yields the position ranges of continuous virtual nodes set to 1 in
    /// the bitmap.
    pub fn vnode_ranges(&self) -> impl Iterator<Item = RangeInclusive<VirtualNode>> + '_ {
        self.high_ranges()
            .map(|r| (VirtualNode::from_index(*r.start())..=VirtualNode::from_index(*r.end())))
    }

    /// Returns whether only the [`SINGLETON_VNODE`] is set in the bitmap.
    ///
    /// Note that this method returning `true` does not imply that the bitmap was created by
    /// [`VnodeBitmapExt::singleton`], or that the bitmap has length 1.
    pub fn is_singleton(&self) -> bool {
        self.count_ones() == 1 && self.iter_vnodes().next().unwrap() == SINGLETON_VNODE
    }

    /// Creates a bitmap with length 1 and the single bit set.
    pub fn singleton() -> Self {
        Self::ones(1)
    }
}
