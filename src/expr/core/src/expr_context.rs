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

use std::future::Future;

use risingwave_common::hash::VirtualNode;
use risingwave_expr::{define_context, Result as ExprResult};
use risingwave_pb::plan_common::ExprContext;

// For all execution mode.
define_context! {
    pub TIME_ZONE: String,
    pub FRAGMENT_ID: u32,
    pub VNODE_COUNT: usize,
}

pub fn capture_expr_context() -> ExprResult<ExprContext> {
    let time_zone = TIME_ZONE::try_with(ToOwned::to_owned)?;
    Ok(ExprContext { time_zone })
}

/// Get the vnode count from the context, or [`VirtualNode::COUNT_FOR_COMPAT`] if not set.
// TODO(var-vnode): the only case where this is not set is for batch queries, is it still
// necessary to support `rw_vnode` expression in batch queries?
pub fn vnode_count() -> usize {
    VNODE_COUNT::try_with(|&x| x).unwrap_or(VirtualNode::COUNT_FOR_COMPAT)
}

pub async fn expr_context_scope<Fut>(expr_context: ExprContext, future: Fut) -> Fut::Output
where
    Fut: Future,
{
    TIME_ZONE::scope(expr_context.time_zone.to_owned(), future).await
}
