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
use std::num::NonZeroUsize;
use std::sync::Arc;

use assert_matches::assert_matches;
use itertools::Itertools;
use risingwave_common::bail;
use risingwave_common::bitmap::Bitmap;
use risingwave_common::hash::{ActorId, ActorMapping, WorkerSlotId};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::util::stream_graph_visitor::visit_tables;
use risingwave_pb::meta::table_fragments::Fragment;
use risingwave_pb::plan_common::ExprContext;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::update_mutation::MergeUpdate;
use risingwave_pb::stream_plan::{
    DispatchStrategy, Dispatcher, DispatcherType, MergeNode, StreamActor, StreamNode,
    StreamScanType,
};

use super::id::GlobalFragmentIdsExt;
use super::Locations;
use crate::manager::{IdGenManagerImpl, MetaSrvEnv, StreamingClusterInfo, StreamingJob};
use crate::model::{DispatcherId, FragmentId};
use crate::stream::stream_graph::fragment::{
    CompleteStreamFragmentGraph, EdgeId, EitherFragment, StreamFragmentEdge,
};
use crate::stream::stream_graph::id::{GlobalActorId, GlobalActorIdGen, GlobalFragmentId};
use crate::stream::stream_graph::schedule;
use crate::stream::stream_graph::schedule::Distribution;
use crate::MetaResult;

/// The upstream information of an actor during the building process. This will eventually be used
/// to create the `MergeNode`s as the leaf executor of each actor.
#[derive(Debug, Clone)]
struct ActorUpstream {
    /// The ID of this edge.
    edge_id: EdgeId,

    /// Upstream actors.
    actors: Vec<GlobalActorId>,

    /// The fragment ID of this upstream.
    fragment_id: GlobalFragmentId,
}

/// [`ActorBuilder`] builds a stream actor in a stream DAG.
#[derive(Debug)]
struct ActorBuilder {
    /// The ID of this actor.
    actor_id: GlobalActorId,

    /// The fragment ID of this actor.
    fragment_id: GlobalFragmentId,

    /// The body of this actor, verbatim from the frontend.
    ///
    /// This cannot be directly used for execution, and it will be rewritten after we know all of
    /// the upstreams and downstreams in the end. See `rewrite`.
    nodes: Arc<StreamNode>,

    /// The dispatchers to the downstream actors.
    downstreams: HashMap<DispatcherId, Dispatcher>,

    /// The upstream actors.
    upstreams: HashMap<EdgeId, ActorUpstream>,

    /// The virtual node bitmap, if this fragment is hash distributed.
    vnode_bitmap: Option<Bitmap>,
}

impl ActorBuilder {
    fn new(
        actor_id: GlobalActorId,
        fragment_id: GlobalFragmentId,
        vnode_bitmap: Option<Bitmap>,
        node: Arc<StreamNode>,
    ) -> Self {
        Self {
            actor_id,
            fragment_id,
            nodes: node,
            downstreams: HashMap::new(),
            upstreams: HashMap::new(),
            vnode_bitmap,
        }
    }

    fn fragment_id(&self) -> GlobalFragmentId {
        self.fragment_id
    }

    /// Add a dispatcher to this actor.
    fn add_dispatcher(&mut self, dispatcher: Dispatcher) {
        self.downstreams
            .try_insert(dispatcher.dispatcher_id, dispatcher)
            .unwrap();
    }

    /// Add an upstream to this actor.
    fn add_upstream(&mut self, upstream: ActorUpstream) {
        self.upstreams
            .try_insert(upstream.edge_id, upstream)
            .unwrap();
    }

    /// Rewrite the actor body.
    ///
    /// During this process, the following things will be done:
    /// 1. Replace the logical `Exchange` in node's input with `Merge`, which can be executed on the
    ///    compute nodes.
    /// 2. Fill the upstream mview info of the `Merge` node under the other "leaf" nodes.
    fn rewrite(&self) -> MetaResult<StreamNode> {
        self.rewrite_inner(&self.nodes, 0)
    }

    fn rewrite_inner(&self, stream_node: &StreamNode, depth: usize) -> MetaResult<StreamNode> {
        match stream_node.get_node_body()? {
            // Leaf node `Exchange`.
            NodeBody::Exchange(exchange) => {
                // The exchange node should always be the bottom of the plan node. If we find one
                // when the depth is 0, it means that the plan node is not well-formed.
                if depth == 0 {
                    bail!(
                        "there should be no ExchangeNode on the top of the plan node: {:#?}",
                        stream_node
                    )
                }
                assert!(!stream_node.get_fields().is_empty());
                assert!(stream_node.input.is_empty());

                // Index the upstreams by the an internal edge ID.
                let upstreams = &self.upstreams[&EdgeId::Internal {
                    link_id: stream_node.get_operator_id(),
                }];

                Ok(StreamNode {
                    node_body: Some(NodeBody::Merge(MergeNode {
                        upstream_actor_id: upstreams.actors.as_global_ids(),
                        upstream_fragment_id: upstreams.fragment_id.as_global_id(),
                        upstream_dispatcher_type: exchange.get_strategy()?.r#type,
                        fields: stream_node.get_fields().clone(),
                    })),
                    identity: "MergeExecutor".to_string(),
                    ..stream_node.clone()
                })
            }

            // "Leaf" node `StreamScan`.
            NodeBody::StreamScan(stream_scan) => {
                let input = stream_node.get_input();
                assert_eq!(input.len(), 2);

                let merge_node = &input[0];
                assert_matches!(merge_node.node_body, Some(NodeBody::Merge(_)));
                let batch_plan_node = &input[1];
                assert_matches!(batch_plan_node.node_body, Some(NodeBody::BatchPlan(_)));

                // Index the upstreams by the an external edge ID.
                let upstreams = &self.upstreams[&EdgeId::UpstreamExternal {
                    upstream_table_id: stream_scan.table_id.into(),
                    downstream_fragment_id: self.fragment_id,
                }];

                let upstream_actor_id = upstreams.actors.as_global_ids();
                let is_shuffled_backfill = stream_scan.stream_scan_type
                    == StreamScanType::ArrangementBackfill as i32
                    || stream_scan.stream_scan_type == StreamScanType::SnapshotBackfill as i32;
                if !is_shuffled_backfill {
                    assert_eq!(upstream_actor_id.len(), 1);
                }

                let upstream_dispatcher_type = if is_shuffled_backfill {
                    // FIXME(kwannoel): Should the upstream dispatcher type depends on the upstream distribution?
                    // If singleton, use `Simple` dispatcher, otherwise use `Hash` dispatcher.
                    DispatcherType::Hash as _
                } else {
                    DispatcherType::NoShuffle as _
                };

                let input = vec![
                    // Fill the merge node body with correct upstream info.
                    StreamNode {
                        node_body: Some(NodeBody::Merge(MergeNode {
                            upstream_actor_id,
                            upstream_fragment_id: upstreams.fragment_id.as_global_id(),
                            upstream_dispatcher_type,
                            fields: merge_node.fields.clone(),
                        })),
                        ..merge_node.clone()
                    },
                    batch_plan_node.clone(),
                ];

                Ok(StreamNode {
                    input,
                    ..stream_node.clone()
                })
            }

            // "Leaf" node `CdcFilter` and `SourceBackfill`. They both `Merge` an upstream `Source`
            // cdc_filter -> backfill -> mview
            // source_backfill -> mview
            NodeBody::CdcFilter(_) | NodeBody::SourceBackfill(_) => {
                let input = stream_node.get_input();
                assert_eq!(input.len(), 1);

                let merge_node = &input[0];
                assert_matches!(merge_node.node_body, Some(NodeBody::Merge(_)));

                let upstream_source_id = match stream_node.get_node_body()? {
                    NodeBody::CdcFilter(node) => node.upstream_source_id,
                    NodeBody::SourceBackfill(node) => node.upstream_source_id,
                    _ => unreachable!(),
                };

                // Index the upstreams by the an external edge ID.
                let upstreams = &self.upstreams[&EdgeId::UpstreamExternal {
                    upstream_table_id: upstream_source_id.into(),
                    downstream_fragment_id: self.fragment_id,
                }];

                let upstream_actor_id = upstreams.actors.as_global_ids();
                // Upstream Cdc Source should be singleton.
                // SourceBackfill is NoShuffle 1-1 correspondence.
                // So they both should have only one upstream actor.
                assert_eq!(upstream_actor_id.len(), 1);

                // rewrite the input
                let input = vec![
                    // Fill the merge node body with correct upstream info.
                    StreamNode {
                        node_body: Some(NodeBody::Merge(MergeNode {
                            upstream_actor_id,
                            upstream_fragment_id: upstreams.fragment_id.as_global_id(),
                            upstream_dispatcher_type: DispatcherType::NoShuffle as _,
                            fields: merge_node.fields.clone(),
                        })),
                        ..merge_node.clone()
                    },
                ];
                Ok(StreamNode {
                    input,
                    ..stream_node.clone()
                })
            }

            // For other nodes, visit the children recursively.
            _ => {
                let mut new_stream_node = stream_node.clone();
                for (input, new_input) in stream_node
                    .input
                    .iter()
                    .zip_eq_fast(&mut new_stream_node.input)
                {
                    *new_input = self.rewrite_inner(input, depth + 1)?;
                }
                Ok(new_stream_node)
            }
        }
    }

    /// Build an actor after all the upstreams and downstreams are processed.
    fn build(self, job: &StreamingJob, expr_context: ExprContext) -> MetaResult<StreamActor> {
        let rewritten_nodes = self.rewrite()?;

        // TODO: store each upstream separately
        let upstream_actor_id = self
            .upstreams
            .into_values()
            .flat_map(|ActorUpstream { actors, .. }| actors.as_global_ids())
            .collect();
        // Only fill the definition when debug assertions enabled, otherwise use name instead.
        #[cfg(not(debug_assertions))]
        let mview_definition = job.name();
        #[cfg(debug_assertions)]
        let mview_definition = job.definition();

        Ok(StreamActor {
            actor_id: self.actor_id.as_global_id(),
            fragment_id: self.fragment_id.as_global_id(),
            nodes: Some(rewritten_nodes),
            dispatcher: self.downstreams.into_values().collect(),
            upstream_actor_id,
            vnode_bitmap: self.vnode_bitmap.map(|b| b.to_protobuf()),
            mview_definition,
            expr_context: Some(expr_context),
        })
    }
}

/// The required changes to an existing external actor to build the graph of a streaming job.
///
/// For example, when we're creating an mview on an existing mview, we need to add new downstreams
/// to the upstream actors, by adding new dispatchers.
#[derive(Default)]
struct ExternalChange {
    /// The new downstreams to be added, indexed by the dispatcher ID.
    new_downstreams: HashMap<DispatcherId, Dispatcher>,

    /// The new upstreams to be added (replaced), indexed by the upstream fragment ID.
    new_upstreams: HashMap<GlobalFragmentId, ActorUpstream>,
}

impl ExternalChange {
    /// Add a dispatcher to the external actor.
    fn add_dispatcher(&mut self, dispatcher: Dispatcher) {
        self.new_downstreams
            .try_insert(dispatcher.dispatcher_id, dispatcher)
            .unwrap();
    }

    /// Add an upstream to the external actor.
    fn add_upstream(&mut self, upstream: ActorUpstream) {
        self.new_upstreams
            .try_insert(upstream.fragment_id, upstream)
            .unwrap();
    }
}

/// The worker slot location of actors.
type ActorLocations = BTreeMap<GlobalActorId, WorkerSlotId>;

/// The actual mutable state of building an actor graph.
///
/// When the fragments are visited in a topological order, actor builders will be added to this
/// state and the scheduled locations will be added. As the building process is run on the
/// **complete graph** which also contains the info of the existing (external) fragments, the info
/// of them will be also recorded.
#[derive(Default)]
struct ActorGraphBuildStateInner {
    /// The builders of the actors to be built.
    actor_builders: BTreeMap<GlobalActorId, ActorBuilder>,

    /// The scheduled locations of the actors to be built.
    building_locations: ActorLocations,

    /// The required changes to the external actors. See [`ExternalChange`].
    external_changes: BTreeMap<GlobalActorId, ExternalChange>,

    /// The actual locations of the external actors.
    external_locations: ActorLocations,
}

/// The information of a fragment, used for parameter passing for `Inner::add_link`.
struct FragmentLinkNode<'a> {
    fragment_id: GlobalFragmentId,
    actor_ids: &'a [GlobalActorId],
    distribution: &'a Distribution,
}

impl ActorGraphBuildStateInner {
    /// Insert new generated actor and record its location.
    ///
    /// The `vnode_bitmap` should be `Some` for the actors of hash-distributed fragments.
    fn add_actor(
        &mut self,
        actor_id: GlobalActorId,
        fragment_id: GlobalFragmentId,
        worker_slot_id: WorkerSlotId,
        vnode_bitmap: Option<Bitmap>,
        node: Arc<StreamNode>,
    ) {
        self.actor_builders
            .try_insert(
                actor_id,
                ActorBuilder::new(actor_id, fragment_id, vnode_bitmap, node),
            )
            .unwrap();

        self.building_locations
            .try_insert(actor_id, worker_slot_id)
            .unwrap();
    }

    /// Record the location of an external actor.
    fn record_external_location(&mut self, actor_id: GlobalActorId, worker_slot_id: WorkerSlotId) {
        self.external_locations
            .try_insert(actor_id, worker_slot_id)
            .unwrap();
    }

    /// Create a new hash dispatcher.
    fn new_hash_dispatcher(
        strategy: &DispatchStrategy,
        downstream_fragment_id: GlobalFragmentId,
        downstream_actors: &[GlobalActorId],
        downstream_actor_mapping: ActorMapping,
    ) -> Dispatcher {
        assert_eq!(strategy.r#type(), DispatcherType::Hash);

        Dispatcher {
            r#type: DispatcherType::Hash as _,
            dist_key_indices: strategy.dist_key_indices.clone(),
            output_indices: strategy.output_indices.clone(),
            hash_mapping: Some(downstream_actor_mapping.to_protobuf()),
            dispatcher_id: downstream_fragment_id.as_global_id() as u64,
            downstream_actor_id: downstream_actors.as_global_ids(),
        }
    }

    /// Create a new dispatcher for non-hash types.
    fn new_normal_dispatcher(
        strategy: &DispatchStrategy,
        downstream_fragment_id: GlobalFragmentId,
        downstream_actors: &[GlobalActorId],
    ) -> Dispatcher {
        assert_ne!(strategy.r#type(), DispatcherType::Hash);
        assert!(strategy.dist_key_indices.is_empty());

        Dispatcher {
            r#type: strategy.r#type,
            dist_key_indices: vec![],
            output_indices: strategy.output_indices.clone(),
            hash_mapping: None,
            dispatcher_id: downstream_fragment_id.as_global_id() as u64,
            downstream_actor_id: downstream_actors.as_global_ids(),
        }
    }

    /// Add the new dispatcher for an actor.
    ///
    /// - If the actor is to be built, the dispatcher will be added to the actor builder.
    /// - If the actor is an external actor, the dispatcher will be added to the external changes.
    fn add_dispatcher(&mut self, actor_id: GlobalActorId, dispatcher: Dispatcher) {
        if let Some(actor_builder) = self.actor_builders.get_mut(&actor_id) {
            actor_builder.add_dispatcher(dispatcher);
        } else {
            self.external_changes
                .entry(actor_id)
                .or_default()
                .add_dispatcher(dispatcher);
        }
    }

    /// Add the new upstream for an actor.
    ///
    /// - If the actor is to be built, the upstream will be added to the actor builder.
    /// - If the actor is an external actor, the upstream will be added to the external changes.
    fn add_upstream(&mut self, actor_id: GlobalActorId, upstream: ActorUpstream) {
        if let Some(actor_builder) = self.actor_builders.get_mut(&actor_id) {
            actor_builder.add_upstream(upstream);
        } else {
            self.external_changes
                .entry(actor_id)
                .or_default()
                .add_upstream(upstream);
        }
    }

    /// Get the location of an actor. Will look up the location map of both the actors to be built
    /// and the external actors.
    fn get_location(&self, actor_id: GlobalActorId) -> WorkerSlotId {
        self.building_locations
            .get(&actor_id)
            .copied()
            .or_else(|| self.external_locations.get(&actor_id).copied())
            .unwrap()
    }

    /// Add a "link" between two fragments in the graph.
    ///
    /// The `edge` will be expanded into multiple (downstream - upstream) pairs for the actors in
    /// the two fragments, based on the distribution and the dispatch strategy. They will be
    /// finally transformed to `Dispatcher` and `Merge` nodes when building the actors.
    ///
    /// If there're existing (external) fragments, the info will be recorded in `external_changes`,
    /// instead of the actor builders.
    fn add_link<'a>(
        &mut self,
        upstream: FragmentLinkNode<'a>,
        downstream: FragmentLinkNode<'a>,
        edge: &'a StreamFragmentEdge,
    ) {
        let dt = edge.dispatch_strategy.r#type();

        match dt {
            // For `NoShuffle`, make n "1-1" links between the actors.
            DispatcherType::NoShuffle => {
                assert_eq!(upstream.actor_ids.len(), downstream.actor_ids.len());
                let upstream_locations: HashMap<_, _> = upstream
                    .actor_ids
                    .iter()
                    .map(|id| (self.get_location(*id), *id))
                    .collect();
                let downstream_locations: HashMap<_, _> = downstream
                    .actor_ids
                    .iter()
                    .map(|id| (self.get_location(*id), *id))
                    .collect();

                for (location, upstream_id) in upstream_locations {
                    let downstream_id = downstream_locations.get(&location).unwrap();

                    // Create a new dispatcher just between these two actors.
                    self.add_dispatcher(
                        upstream_id,
                        Self::new_normal_dispatcher(
                            &edge.dispatch_strategy,
                            downstream.fragment_id,
                            &[*downstream_id],
                        ),
                    );

                    // Also record the upstream for the downstream actor.
                    self.add_upstream(
                        *downstream_id,
                        ActorUpstream {
                            edge_id: edge.id,
                            actors: vec![upstream_id],
                            fragment_id: upstream.fragment_id,
                        },
                    );
                }
            }

            // Otherwise, make m * n links between the actors.
            DispatcherType::Hash | DispatcherType::Broadcast | DispatcherType::Simple => {
                // Add dispatchers for the upstream actors.
                let dispatcher = if let DispatcherType::Hash = dt {
                    // Transform the `WorkerSlotMapping` from the downstream distribution to the
                    // `ActorMapping`, used for the `HashDispatcher` for the upstream actors.
                    let downstream_locations: HashMap<WorkerSlotId, ActorId> = downstream
                        .actor_ids
                        .iter()
                        .map(|&actor_id| (self.get_location(actor_id), actor_id.as_global_id()))
                        .collect();
                    let actor_mapping = downstream
                        .distribution
                        .as_hash()
                        .unwrap()
                        .to_actor(&downstream_locations);

                    Self::new_hash_dispatcher(
                        &edge.dispatch_strategy,
                        downstream.fragment_id,
                        downstream.actor_ids,
                        actor_mapping,
                    )
                } else {
                    Self::new_normal_dispatcher(
                        &edge.dispatch_strategy,
                        downstream.fragment_id,
                        downstream.actor_ids,
                    )
                };
                for upstream_id in upstream.actor_ids {
                    self.add_dispatcher(*upstream_id, dispatcher.clone());
                }

                // Add upstreams for the downstream actors.
                let actor_upstream = ActorUpstream {
                    edge_id: edge.id,
                    actors: upstream.actor_ids.to_vec(),
                    fragment_id: upstream.fragment_id,
                };
                for downstream_id in downstream.actor_ids {
                    self.add_upstream(*downstream_id, actor_upstream.clone());
                }
            }

            DispatcherType::Unspecified => unreachable!(),
        }
    }
}

/// The mutable state of building an actor graph. See [`ActorGraphBuildStateInner`].
struct ActorGraphBuildState {
    /// The actual state.
    inner: ActorGraphBuildStateInner,

    /// The actor IDs of each fragment.
    fragment_actors: HashMap<GlobalFragmentId, Vec<GlobalActorId>>,

    /// The next local actor id to use.
    next_local_id: u32,

    /// The global actor id generator.
    actor_id_gen: GlobalActorIdGen,
}

impl ActorGraphBuildState {
    /// Create an empty state with the given id generator.
    fn new(actor_id_gen: GlobalActorIdGen) -> Self {
        Self {
            inner: Default::default(),
            fragment_actors: Default::default(),
            next_local_id: 0,
            actor_id_gen,
        }
    }

    /// Get the next global actor id.
    fn next_actor_id(&mut self) -> GlobalActorId {
        let local_id = self.next_local_id;
        self.next_local_id += 1;

        self.actor_id_gen.to_global_id(local_id)
    }

    /// Finish the build and return the inner state.
    fn finish(self) -> ActorGraphBuildStateInner {
        // Assert that all the actors are built.
        assert_eq!(self.actor_id_gen.len(), self.next_local_id);

        self.inner
    }
}

/// The result of a built actor graph. Will be further embedded into the `Context` for building
/// actors on the compute nodes.
pub struct ActorGraphBuildResult {
    /// The graph of sealed fragments, including all actors.
    pub graph: BTreeMap<FragmentId, Fragment>,

    /// The scheduled locations of the actors to be built.
    pub building_locations: Locations,

    /// The actual locations of the external actors.
    pub existing_locations: Locations,

    /// The new dispatchers to be added to the upstream mview actors. Used for MV on MV.
    pub dispatchers: HashMap<ActorId, Vec<Dispatcher>>,

    /// The updates to be applied to the downstream chain actors. Used for schema change (replace
    /// table plan).
    pub merge_updates: Vec<MergeUpdate>,
}

/// [`ActorGraphBuilder`] builds the actor graph for the given complete fragment graph, based on the
/// current cluster info and the required parallelism.
pub struct ActorGraphBuilder {
    /// The pre-scheduled distribution for each building fragment.
    distributions: HashMap<GlobalFragmentId, Distribution>,

    /// The actual distribution for each existing fragment.
    existing_distributions: HashMap<GlobalFragmentId, Distribution>,

    /// The complete fragment graph.
    fragment_graph: CompleteStreamFragmentGraph,

    /// The cluster info for creating a streaming job.
    cluster_info: StreamingClusterInfo,
}

impl ActorGraphBuilder {
    /// Create a new actor graph builder with the given "complete" graph. Returns an error if the
    /// graph is failed to be scheduled.
    pub fn new(
        streaming_job_id: u32,
        fragment_graph: CompleteStreamFragmentGraph,
        cluster_info: StreamingClusterInfo,
        default_parallelism: NonZeroUsize,
    ) -> MetaResult<Self> {
        let expected_vnode_count = fragment_graph.expected_vnode_count();
        let existing_distributions = fragment_graph.existing_distribution();

        // Schedule the distribution of all building fragments.
        let scheduler = schedule::Scheduler::new(
            streaming_job_id,
            &cluster_info.worker_nodes,
            default_parallelism,
            expected_vnode_count,
        )?;
        let distributions = scheduler.schedule(&fragment_graph)?;

        // Fill the vnode count for each internal table, based on schedule result.
        let mut fragment_graph = fragment_graph;
        for (id, fragment) in fragment_graph.building_fragments_mut() {
            let vnode_count = distributions[id].vnode_count();
            visit_tables(fragment, |table, _| {
                table.maybe_vnode_count = Some(vnode_count as _);
            })
        }

        Ok(Self {
            distributions,
            existing_distributions,
            fragment_graph,
            cluster_info,
        })
    }

    /// Get the distribution of the given fragment. Will look up the distribution map of both the
    /// building and existing fragments.
    fn get_distribution(&self, fragment_id: GlobalFragmentId) -> &Distribution {
        self.distributions
            .get(&fragment_id)
            .or_else(|| self.existing_distributions.get(&fragment_id))
            .unwrap()
    }

    /// Convert the actor location map to the [`Locations`] struct.
    fn build_locations(&self, actor_locations: ActorLocations) -> Locations {
        let actor_locations = actor_locations
            .into_iter()
            .map(|(id, worker_slot_id)| (id.as_global_id(), worker_slot_id))
            .collect();

        let worker_locations = self.cluster_info.worker_nodes.clone();

        Locations {
            actor_locations,
            worker_locations,
        }
    }

    /// Build a stream graph by duplicating each fragment as parallel actors. Returns
    /// [`ActorGraphBuildResult`] that will be further used to build actors on the compute nodes.
    pub async fn generate_graph(
        self,
        env: &MetaSrvEnv,
        job: &StreamingJob,
        expr_context: ExprContext,
    ) -> MetaResult<ActorGraphBuildResult> {
        // Pre-generate IDs for all actors.
        let actor_len = self
            .distributions
            .values()
            .map(|d| d.parallelism())
            .sum::<usize>() as u64;
        let id_gen = match env.id_gen_manager() {
            IdGenManagerImpl::Kv(mgr) => GlobalActorIdGen::new(mgr, actor_len).await?,
            IdGenManagerImpl::Sql(mgr) => GlobalActorIdGen::new_v2(mgr, actor_len),
        };

        // Build the actor graph and get the final state.
        let ActorGraphBuildStateInner {
            actor_builders,
            building_locations,
            external_changes,
            external_locations,
        } = self.build_actor_graph(id_gen)?;

        for worker_slot_id in external_locations.values() {
            if self
                .cluster_info
                .unschedulable_workers
                .contains(&worker_slot_id.worker_id())
            {
                bail!(
                    "The worker {} where the associated upstream is located is unschedulable",
                    worker_slot_id.worker_id(),
                );
            }
        }

        // Serialize the graph into a map of sealed fragments.
        let graph = {
            let mut actors: HashMap<GlobalFragmentId, Vec<StreamActor>> = HashMap::new();

            // As all fragments are processed, we can now `build` the actors where the `Exchange`
            // and `Chain` are rewritten.
            for builder in actor_builders.into_values() {
                let fragment_id = builder.fragment_id();
                let actor = builder.build(job, expr_context.clone())?;
                actors.entry(fragment_id).or_default().push(actor);
            }

            actors
                .into_iter()
                .map(|(fragment_id, actors)| {
                    let distribution = self.distributions[&fragment_id].clone();
                    let fragment =
                        self.fragment_graph
                            .seal_fragment(fragment_id, actors, distribution);
                    let fragment_id = fragment_id.as_global_id();
                    (fragment_id, fragment)
                })
                .collect()
        };

        // Convert the actor location map to the `Locations` struct.
        let building_locations = self.build_locations(building_locations);
        let existing_locations = self.build_locations(external_locations);

        // Extract the new dispatchers from the external changes.
        let dispatchers = external_changes
            .iter()
            .map(|(actor_id, change)| {
                (
                    actor_id.as_global_id(),
                    change.new_downstreams.values().cloned().collect_vec(),
                )
            })
            .filter(|(_, v)| !v.is_empty())
            .collect();

        // Extract the updates for merge executors from the external changes.
        let merge_updates = external_changes
            .iter()
            .flat_map(|(actor_id, change)| {
                change.new_upstreams.values().map(move |upstream| {
                    let EdgeId::DownstreamExternal {
                        original_upstream_fragment_id,
                        ..
                    } = upstream.edge_id
                    else {
                        unreachable!("edge from internal to external must be `DownstreamExternal`")
                    };

                    MergeUpdate {
                        actor_id: actor_id.as_global_id(),
                        upstream_fragment_id: original_upstream_fragment_id.as_global_id(),
                        new_upstream_fragment_id: Some(upstream.fragment_id.as_global_id()),
                        added_upstream_actor_id: upstream.actors.as_global_ids(),
                        removed_upstream_actor_id: vec![],
                    }
                })
            })
            .collect();

        Ok(ActorGraphBuildResult {
            graph,
            building_locations,
            existing_locations,
            dispatchers,
            merge_updates,
        })
    }

    /// Build actor graph for each fragment, using topological order.
    fn build_actor_graph(&self, id_gen: GlobalActorIdGen) -> MetaResult<ActorGraphBuildStateInner> {
        let mut state = ActorGraphBuildState::new(id_gen);

        // Use topological sort to build the graph from downstream to upstream. (The first fragment
        // popped out from the heap will be the top-most node in plan, or the sink in stream graph.)
        for fragment_id in self.fragment_graph.topo_order()? {
            self.build_actor_graph_fragment(fragment_id, &mut state)?;
        }

        Ok(state.finish())
    }

    /// Build actor graph for a specific fragment.
    fn build_actor_graph_fragment(
        &self,
        fragment_id: GlobalFragmentId,
        state: &mut ActorGraphBuildState,
    ) -> MetaResult<()> {
        let current_fragment = self.fragment_graph.get_fragment(fragment_id);
        let distribution = self.get_distribution(fragment_id);

        // First, add or record the actors for the current fragment into the state.
        let actor_ids = match current_fragment {
            // For building fragments, we need to generate the actor builders.
            EitherFragment::Building(current_fragment) => {
                let node = Arc::new(current_fragment.node.clone().unwrap());
                let bitmaps = distribution.as_hash().map(|m| m.to_bitmaps());

                distribution
                    .worker_slots()
                    .map(|worker_slot| {
                        let actor_id = state.next_actor_id();
                        let vnode_bitmap = bitmaps
                            .as_ref()
                            .map(|m: &HashMap<WorkerSlotId, Bitmap>| &m[&worker_slot])
                            .cloned();

                        state.inner.add_actor(
                            actor_id,
                            fragment_id,
                            worker_slot,
                            vnode_bitmap,
                            node.clone(),
                        );

                        actor_id
                    })
                    .collect_vec()
            }

            // For existing fragments, we only need to record the actor locations.
            EitherFragment::Existing(existing_fragment) => existing_fragment
                .actors
                .iter()
                .map(|a| {
                    let actor_id = GlobalActorId::new(a.actor_id);
                    let worker_slot_id = match &distribution {
                        Distribution::Singleton(worker_slot_id) => *worker_slot_id,
                        Distribution::Hash(mapping) => mapping
                            .get_matched(&Bitmap::from(a.get_vnode_bitmap().unwrap()))
                            .unwrap(),
                    };

                    state
                        .inner
                        .record_external_location(actor_id, worker_slot_id);

                    actor_id
                })
                .collect_vec(),
        };

        // Then, add links between the current fragment and its downstream fragments.
        for (downstream_fragment_id, edge) in self.fragment_graph.get_downstreams(fragment_id) {
            let downstream_actors = state
                .fragment_actors
                .get(&downstream_fragment_id)
                .expect("downstream fragment not processed yet");

            let downstream_distribution = self.get_distribution(downstream_fragment_id);

            state.inner.add_link(
                FragmentLinkNode {
                    fragment_id,
                    actor_ids: &actor_ids,
                    distribution,
                },
                FragmentLinkNode {
                    fragment_id: downstream_fragment_id,
                    actor_ids: downstream_actors,
                    distribution: downstream_distribution,
                },
                edge,
            );
        }

        // Finally, record the actor IDs for the current fragment.
        state
            .fragment_actors
            .try_insert(fragment_id, actor_ids)
            .unwrap_or_else(|_| panic!("fragment {:?} is already processed", fragment_id));

        Ok(())
    }
}
