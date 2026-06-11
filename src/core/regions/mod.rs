//! Regional state ownership plus resource cache rules for future cross-region simulation.
//!
//! This module keeps each region's ECS `World` private inside `RegionState`.
//! Runtime and worker code can use owned resource summaries and UI-safe views
//! without reading another region's ECS storage.
//!
//! ```text
//! Local tick path:
//!
//!   RegionState::tick_local()
//!                 |
//!                 v
//!   tick_world(&mut World)
//!                 |
//!                 v
//!   shared deterministic simulation helpers
//!     power -> stats -> local effects
//!     -> citizens/population/economy/business
//!                 |
//!                 v
//!   CommandResult events
//!
//! Imported resource processing:
//!
//!   RegionState::process_imported_resource(...)
//!                 |
//!                 v
//!   imported_resources.accept(resource)
//!                 |
//!       +---------+-------------------+
//!       |         |                   |
//!       v         v                   v
//!   Accepted  ReplacedOlderGeneration RejectedDuplicate/RejectedStale
//!       |         |                   |
//!       +----+----+                   v
//!            |                 forwarded_resources = []
//!            v
//!   Build forwarded resources for target neighbors:
//!     - skip source neighbor
//!     - subtract local_used_capacity
//!     - add border_crossing_cost
//!     - increment hop_count
//!     - stop at max_hops or zero capacity
//!            |
//!            v
//!   ImportedResourceResult
//!     decision
//!     forwarded_resources
//!
//! Neighbor reply recording:
//!
//!   RegionState::apply_neighbor_import_result(result)
//!                 |
//!                 v
//!   neighbor_import_results.push(result)
//!
//!   No other region's World is touched.
//! ```

use crate::core::components::{PowerSource, RemoteWorkplace};
use crate::core::entity::Entity;
use crate::core::resource_registry::ResourceRegistry;
use crate::core::simulation::{
    TickJobPhase, TickPowerPhase, begin_tick_power_phase, continue_to_job_phase,
    finish_tick_after_job_phase, refresh_derived_state_for_world, tick_world,
};
use crate::core::systems::{build, bulldoze, replace, road_connectivity, upgrade};
use crate::core::world::World;
use crate::interface::adapter::{inspect_world, view_world, view_world_with_overlay};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameView, InspectView};
use serde::{Deserialize, Serialize};

pub mod handle;
pub mod load_manager;
pub mod runtime;
pub mod threaded;
pub mod worker;
pub use runtime::continuation;

const IMPORTED_RESOURCE_CAPACITY_PER_SOURCE: u32 = 1;
const IMPORTED_RESOURCE_MAX_HOPS: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Stable identity for one independently owned simulation region.
///
/// Future runtimes and workers will use this as a routing key. It is not an ECS
/// entity ID and should never identify another region's local `World` storage.
pub struct RegionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Compact categories of cross-region access that can be imported as cache.
///
/// These variants describe what a region exports through its borders without
/// exposing the building, citizen, or road entities that produced the resource.
pub enum ResourceKind {
    Jobs,
    ParkAccess,
    ServiceAccess,
    ShoppingAccess,
    RoadExitAccess,
    TrafficPressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Stable identity for one exported regional resource generation.
///
/// The origin region and kind identify the source of the resource, while
/// `generation` changes when that source's exported value changes. Forwarding
/// regions must preserve this ID so the same remote supply cannot echo back as
/// new supply under a different origin.
pub struct ResourceId {
    pub origin_region: RegionId,
    pub resource_kind: ResourceKind,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Rebuildable imported resource cache entry received from a neighboring region.
///
/// This is not authoritative remote state. It is a compact summary that a region
/// may use locally and forward to other neighbors until capacity or hop limits
/// are exhausted.
pub struct ImportedResource {
    /// Original exported resource identity. It stays unchanged while forwarded.
    pub id: ResourceId,
    /// Capacity still available after earlier regions have used part of it.
    pub remaining_capacity: u32,
    /// Number of border-to-border forwards already taken from the origin.
    pub hop_count: u32,
    /// Maximum allowed forwards before propagation stops.
    pub max_hops: u32,
    /// Integer distance/cost accumulated along the import path.
    pub travel_cost: u32,
    /// Neighbor that sent this resource to the receiving region.
    pub source_neighbor: RegionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Authoritative export count produced by one region runtime.
pub struct RegionalExport {
    pub region_id: RegionId,
    pub resource_kind: ResourceKind,
    pub count: u32,
    /// Monotonic only while a runtime is alive. Imported caches are rebuilt
    /// empty after load, so save files do not need to preserve this generation.
    pub generation: u64,
}

impl RegionalExport {
    pub fn imported_resource(self) -> ImportedResource {
        ImportedResource {
            id: ResourceId {
                origin_region: self.region_id,
                resource_kind: self.resource_kind,
                generation: self.generation,
            },
            remaining_capacity: self
                .count
                .saturating_mul(IMPORTED_RESOURCE_CAPACITY_PER_SOURCE),
            hop_count: 0,
            max_hops: IMPORTED_RESOURCE_MAX_HOPS,
            travel_cost: 0,
            source_neighbor: self.region_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Runtime-owned export delta for the worker to route to neighboring regions.
pub struct RegionalExportChange {
    pub source_region: RegionId,
    pub current: Vec<RegionalExport>,
    pub removed: Vec<ResourceKind>,
}

impl RegionalExportChange {
    pub fn tombstone(source_region: RegionId, resource_kind: ResourceKind) -> ImportedResource {
        ImportedResource {
            id: ResourceId {
                origin_region: source_region,
                resource_kind,
                generation: u64::MAX,
            },
            remaining_capacity: 0,
            hop_count: 0,
            max_hops: IMPORTED_RESOURCE_MAX_HOPS,
            travel_cost: 0,
            source_neighbor: source_region,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Owned region-level spare capacity summary for cross-region planning.
///
/// This intentionally contains only aggregate counts. It does not expose ECS
/// entities, component references, or handles to this region's private `World`.
pub struct RegionalSpareCapacity {
    pub power_capacity: i32,
    pub job_slots: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Owned summary key for one deterministic road network inside one region.
pub struct RegionRoadNetworkId {
    pub region: RegionId,
    pub road_network: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Region map edge used to identify where local road networks can meet neighbors.
pub enum BorderEdge {
    North,
    South,
    West,
    East,
}

impl BorderEdge {
    /// Returns the edge that faces this one on an adjacent neighbor region.
    pub fn complementary_neighbor_edge(self) -> Self {
        match self {
            BorderEdge::North => BorderEdge::South,
            BorderEdge::South => BorderEdge::North,
            BorderEdge::West => BorderEdge::East,
            BorderEdge::East => BorderEdge::West,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Owned border-road identity; offset is x for north/south and y for west/east.
pub struct BorderLinkId {
    pub edge: BorderEdge,
    pub offset: usize,
}

impl BorderLinkId {
    /// Maps this local link to the matching link a neighbor must expose.
    pub fn matching_neighbor_link(self) -> Self {
        Self {
            edge: self.edge.complementary_neighbor_edge(),
            offset: self.offset,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// One border link owned by one local road network summary.
pub struct NetworkBorderLink {
    pub network: RegionRoadNetworkId,
    pub link: BorderLinkId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Owned region-topology edge used to decide which border links may match.
pub struct RegionNeighborLink {
    pub region: RegionId,
    pub edge: BorderEdge,
    pub neighbor: RegionId,
}

impl RegionNeighborLink {
    /// Builds one directional topology edge. The component graph unions matched
    /// road networks symmetrically, but callers should publish both directions
    /// when they maintain a full regional layout.
    pub fn new(region: RegionId, edge: BorderEdge, neighbor: RegionId) -> Self {
        Self {
            region,
            edge,
            neighbor,
        }
    }

    pub fn allows_source(self, region: RegionId, edge: BorderEdge) -> bool {
        self.region == region && self.edge == edge
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Stale-tolerant availability hint published for regional discovery.
///
/// Claims still have to be confirmed by the source region runtime.
pub struct RegionalAvailabilityHint {
    pub network: RegionRoadNetworkId,
    pub has_spare_power: bool,
    pub has_spare_jobs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Caller-local consumer demand that may need producer-exported power.
pub(crate) struct PendingPowerDemand {
    pub token: u32,
    pub consumer: Entity,
    pub demand: i32,
    pub caller_network: RegionRoadNetworkId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Caller-local job seeker that may need a producer-exported workplace slot.
pub(crate) struct PendingJobDemand {
    pub token: u32,
    pub citizen: Entity,
    pub caller_network: RegionRoadNetworkId,
}

#[derive(Debug)]
/// Paused tick state after local power and before downstream systems.
pub(crate) struct RegionalTickPowerPhase {
    phase: TickPowerPhase,
    pub power_demands: Vec<PendingPowerDemand>,
}

#[derive(Debug)]
/// Paused tick state after local job assignment and before the daily economy.
pub(crate) struct RegionalTickJobPhase {
    phase: TickJobPhase,
    pub job_demands: Vec<PendingJobDemand>,
}

impl RegionalTickJobPhase {
    /// Whether this tick crosses a daily boundary (when jobs/economy resolve).
    pub(crate) fn is_daily(&self) -> bool {
        self.phase.is_daily()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Result of an authoritative producer-owned export allocation request.
pub struct PowerExportGrant {
    pub token: u32,
    pub granted: bool,
    pub source_region: Option<RegionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Result of an authoritative producer-owned job-slot export allocation request.
///
/// Unlike power, the grant carries identity: the producer's `slot_id` (an opaque
/// owned id, not a remote ECS entity to the consumer) and the `salary` the home
/// region pays the worker. Workplace tax accrues to the exporting region instead.
pub struct JobExportGrant {
    pub token: u32,
    pub granted: bool,
    pub source_region: Option<RegionId>,
    pub slot_id: Option<u32>,
    pub salary: i32,
}

impl ImportedResource {
    /// Builds the copy that should be sent from `current_region` to one neighbor.
    ///
    /// This returns `None` when forwarding would immediately echo the resource
    /// back to the sender, exceed the hop limit, or leave no capacity for the
    /// next region.
    pub fn forwarded_to(
        self,
        current_region: RegionId,
        target_neighbor: RegionId,
        local_used_capacity: u32,
        border_crossing_cost: u32,
    ) -> Option<Self> {
        if target_neighbor == self.source_neighbor || self.hop_count >= self.max_hops {
            return None;
        }

        let remaining_capacity = self.remaining_capacity.saturating_sub(local_used_capacity);
        if remaining_capacity == 0 {
            return None;
        }

        Some(Self {
            remaining_capacity,
            hop_count: self.hop_count.saturating_add(1),
            travel_cost: self.travel_cost.saturating_add(border_crossing_cost),
            // From the target region's view, this region becomes the neighbor
            // that supplied the forwarded resource.
            source_neighbor: current_region,
            ..self
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Outcome of attempting to place an imported resource into a region cache.
///
/// Runtime code can use this later for deterministic tracing and for deciding
/// whether there is anything new to forward to neighboring regions.
pub enum ImportDecision {
    /// The cache had no matching origin/kind/generation and stored the resource.
    Accepted,
    /// The exact same `ResourceId` was already known.
    RejectedDuplicate,
    /// A newer generation for the same origin and kind was already known.
    RejectedStale,
    /// The resource was newer than older cached generations for its origin/kind.
    ReplacedOlderGeneration,
    /// A zero-capacity tombstone removed cached resources for its origin/kind.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result returned after one region processes a neighbor's imported resource.
///
/// Later runtime patches can route this owned value back to the caller region
/// without giving either side access to the other's ECS `World`.
pub struct ImportedResourceResult {
    pub decision: ImportDecision,
    pub forwarded_resources: Vec<ImportedResource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
/// Region-local cache of imported resources accepted from neighbors.
///
/// The cache intentionally stores a small vector. Patch 1 favors readable,
/// deterministic behavior over lookup complexity, and expected regional border
/// resource counts are small.
pub struct ImportedResourceCache {
    resources: Vec<ImportedResource>,
}

#[derive(Debug, Serialize, Deserialize)]
/// Serialized authoritative region state.
///
/// Rebuildable imported-resource caches and neighbor reply traces are
/// intentionally excluded from permanent saves.
pub(crate) struct RegionStateSaveRecord {
    id: RegionId,
    world: World,
}

impl ImportedResourceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resources(&self) -> &[ImportedResource] {
        &self.resources
    }

    /// Accepts a resource if it is new enough for this region's local cache.
    ///
    /// The same `ResourceId` is rejected as a duplicate. An older generation is
    /// rejected after a newer generation for the same origin and kind is known.
    /// A newer generation replaces older cached entries for that origin/kind.
    pub fn accept(&mut self, resource: ImportedResource) -> ImportDecision {
        if self.resources.iter().any(|known| known.id == resource.id) {
            return ImportDecision::RejectedDuplicate;
        }

        let same_source_kind = |known: &&ImportedResource| {
            known.id.origin_region == resource.id.origin_region
                && known.id.resource_kind == resource.id.resource_kind
        };

        if self
            .resources
            .iter()
            .filter(same_source_kind)
            .any(|known| known.id.generation > resource.id.generation)
        {
            return ImportDecision::RejectedStale;
        }

        let before_len = self.resources.len();
        self.resources.retain(|known| {
            known.id.origin_region != resource.id.origin_region
                || known.id.resource_kind != resource.id.resource_kind
                || known.id.generation > resource.id.generation
        });

        let decision = if self.resources.len() == before_len {
            ImportDecision::Accepted
        } else {
            ImportDecision::ReplacedOlderGeneration
        };

        self.resources.push(resource);
        decision
    }

    /// Removes cached resources for one authoritative origin/kind pair.
    ///
    /// Multi-region play uses zero-capacity resource messages as tombstones
    /// when the source region no longer exports a resource after bulldoze,
    /// replace, or another mutating command.
    pub fn remove_origin_kind(
        &mut self,
        origin_region: RegionId,
        resource_kind: ResourceKind,
    ) -> bool {
        let before_len = self.resources.len();
        self.resources.retain(|known| {
            known.id.origin_region != origin_region || known.id.resource_kind != resource_kind
        });
        self.resources.len() != before_len
    }

    /// Produces deterministic outbound resource copies for neighboring regions.
    ///
    /// Target neighbors are considered in caller-provided order. Each resource
    /// copy subtracts the same locally used capacity and adds the same border
    /// crossing cost; later gameplay patches can replace those inputs with
    /// per-neighbor route costs without changing the cache identity rule.
    pub fn forwarded_resources(
        &self,
        current_region: RegionId,
        local_used_capacity: u32,
        border_crossing_cost: u32,
        target_neighbors: &[RegionId],
    ) -> Vec<ImportedResource> {
        self.resources
            .iter()
            .flat_map(|resource| {
                target_neighbors.iter().filter_map(move |target_neighbor| {
                    resource.forwarded_to(
                        current_region,
                        *target_neighbor,
                        local_used_capacity,
                        border_crossing_cost,
                    )
                })
            })
            .collect()
    }
}

#[derive(Debug)]
/// Authoritative state for one independently simulated region.
///
/// The ECS `World` stays private inside this core wrapper. Runtime and worker
/// code should interact through these methods and owned regional resource
/// summaries, while UI code continues to use regional facades and UI-safe view models.
pub struct RegionState {
    id: RegionId,
    world: World,
    imported_resources: ImportedResourceCache,
    neighbor_import_results: Vec<ImportedResourceResult>,
}

impl RegionState {
    /// Creates a region with its own private ECS world and empty import cache.
    pub fn new(id: RegionId, width: usize, height: usize) -> Self {
        Self {
            id,
            world: World::new(width, height),
            imported_resources: ImportedResourceCache::new(),
            neighbor_import_results: Vec::new(),
        }
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    /// Advances only this region's local simulation using the shared tick order.
    pub fn tick_local(&mut self) -> CommandResult {
        tick_world(&mut self.world)
    }

    /// Applies one player build command through the core systems.
    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = build::build(&mut self.world, x, y, kind);
        refresh_derived_state_for_world(&mut self.world);
        result
    }

    /// Explains whether a build would succeed without mutating this region.
    pub fn preview_build(&self, x: usize, y: usize, kind: BuildingKind) -> BuildPreviewView {
        build::preview_build(&self.world, x, y, kind)
    }

    /// Removes one occupied cell through the core systems.
    pub fn bulldoze(&mut self, x: usize, y: usize) -> CommandResult {
        let result = bulldoze::bulldoze(&mut self.world, x, y);
        if result.success {
            refresh_derived_state_for_world(&mut self.world);
        }
        result
    }

    /// Replaces one occupied cell through the core systems.
    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = replace::replace(&mut self.world, x, y, kind);
        if result.success {
            refresh_derived_state_for_world(&mut self.world);
        }
        result
    }

    /// Upgrades one supported occupied cell through the core systems.
    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        let result = upgrade::upgrade(&mut self.world, x, y);
        if result.success {
            refresh_derived_state_for_world(&mut self.world);
        }
        result
    }

    /// Accepts one imported resource and builds deterministic forwarded copies.
    pub fn process_imported_resource(
        &mut self,
        resource: ImportedResource,
        local_used_capacity: u32,
        border_crossing_cost: u32,
        target_neighbors: &[RegionId],
    ) -> ImportedResourceResult {
        if resource.remaining_capacity == 0 {
            let removed = self
                .imported_resources
                .remove_origin_kind(resource.id.origin_region, resource.id.resource_kind);
            return ImportedResourceResult {
                decision: if removed {
                    ImportDecision::Removed
                } else {
                    ImportDecision::RejectedStale
                },
                forwarded_resources: Vec::new(),
            };
        }

        let decision = self.imported_resources.accept(resource);
        let forwarded_resources = if matches!(
            decision,
            ImportDecision::Accepted | ImportDecision::ReplacedOlderGeneration
        ) {
            target_neighbors
                .iter()
                .filter_map(|target_neighbor| {
                    resource.forwarded_to(
                        self.id,
                        *target_neighbor,
                        local_used_capacity,
                        border_crossing_cost,
                    )
                })
                .collect()
        } else {
            Vec::new()
        };

        ImportedResourceResult {
            decision,
            forwarded_resources,
        }
    }

    /// Records a completed neighbor import reply in this caller-owned region.
    pub fn apply_neighbor_import_result(&mut self, result: ImportedResourceResult) {
        self.neighbor_import_results.push(result);
    }

    /// Returns a UI-safe snapshot without exposing this region's ECS world.
    pub fn view(&self) -> GameView {
        view_world(&self.world)
    }

    /// Returns a UI-safe snapshot using the requested map overlay.
    pub fn view_with_overlay(&self, overlay: MapOverlayInput) -> GameView {
        view_world_with_overlay(&self.world, overlay)
    }

    /// Returns a UI-safe inspect model without exposing this region's ECS world.
    pub fn inspect(&self, x: usize, y: usize) -> InspectView {
        let mut inspect = inspect_world(&self.world, x, y);
        if !self.imported_resources.resources().is_empty() {
            // This is region-level imported-resource awareness surfaced through
            // the existing cell inspect channel until regional status panels
            // get a dedicated field.
            inspect.explanations.push(format!(
                "Imported regional resources: {}",
                self.imported_resources.resources().len()
            ));
        }
        inspect
    }

    pub fn imported_resources(&self) -> &[ImportedResource] {
        self.imported_resources.resources()
    }

    /// Number of local citizens currently working in another region (CR3 import).
    pub fn imported_job_count(&self) -> usize {
        self.world
            .citizens
            .values()
            .filter(|citizen| citizen.remote_workplace.is_some())
            .count()
    }

    /// Owned `(source region, slot id)` pairs for citizens working remotely.
    ///
    /// This is owned summary data: the slot id is an opaque `u32`, never a remote
    /// ECS entity. Useful for CR4 visibility and for verifying cross-region jobs.
    pub fn imported_job_slots(&self) -> Vec<(RegionId, u32)> {
        let mut slots = self
            .world
            .citizens
            .values()
            .filter_map(|citizen| {
                citizen
                    .remote_workplace
                    .map(|remote| (remote.region, remote.slot_id))
            })
            .collect::<Vec<_>>();
        slots.sort();
        slots
    }

    /// Returns entity-free border links grouped by local road network.
    pub fn network_border_links(&self) -> Vec<NetworkBorderLink> {
        let mut links = Vec::new();
        for network in road_connectivity::discover_road_networks(&self.world) {
            let network_id = RegionRoadNetworkId {
                region: self.id,
                road_network: network.id,
            };
            for road in network.roads {
                let Some(position) = self.world.positions.get(&road) else {
                    continue;
                };
                links.extend(border_links_for_cell(
                    network_id,
                    position.x,
                    position.y,
                    self.world.grid.width(),
                    self.world.grid.height(),
                ));
            }
        }
        links.sort();
        links
    }

    /// Derives current local exports from authoritative region state.
    pub fn exported_resource_counts(&self) -> Vec<(ResourceKind, u32)> {
        let mut exports: Vec<(ResourceKind, u32)> = Vec::new();
        for building in self.world.buildings.values() {
            let Some(resource_kind) = exported_resource_kind_for_building(building.kind) else {
                continue;
            };
            if let Some((_, count)) = exports
                .iter_mut()
                .find(|(known_kind, _)| *known_kind == resource_kind)
            {
                *count = (*count).saturating_add(1);
            } else {
                exports.push((resource_kind, 1));
            }
        }

        exports.sort_by_key(|(resource_kind, _)| *resource_kind);
        exports
    }

    /// Returns aggregate spare local capacity without exposing ECS storage.
    ///
    /// Power spare capacity is the remaining pooled capacity after local power
    /// grants. Job spare capacity is the unused effective workplace slots after
    /// local citizens are accounted for.
    pub fn regional_spare_capacity(&self) -> RegionalSpareCapacity {
        let power = ResourceRegistry::for_power(&self.world).resolve_local_power();
        let jobs = ResourceRegistry::for_jobs(&self.world).resolve_local_jobs();

        RegionalSpareCapacity {
            power_capacity: power.remaining_capacity,
            job_slots: jobs.remaining_slots,
        }
    }

    /// Returns stale-tolerant per-network availability hints for discovery.
    pub fn availability_hints(&self) -> Vec<RegionalAvailabilityHint> {
        // `network_border_links` and the power registry both use deterministic
        // road-network discovery over this same world, so their `road_network`
        // ids are stable summary keys for one discovery snapshot.
        let power = ResourceRegistry::for_power(&self.world).resolve_local_power();
        let jobs = ResourceRegistry::for_jobs(&self.world).resolve_local_jobs();
        let has_spare_jobs = jobs.remaining_slots > 0;

        let mut hints = power
            .network_capacities
            .into_iter()
            .map(|capacity| RegionalAvailabilityHint {
                network: RegionRoadNetworkId {
                    region: self.id,
                    road_network: capacity.road_network,
                },
                has_spare_power: capacity.remaining_capacity > 0,
                has_spare_jobs,
            })
            .collect::<Vec<_>>();
        hints.sort_by_key(|hint| hint.network);
        hints
    }

    pub(crate) fn power_network_remaining_capacity(&self, network: RegionRoadNetworkId) -> i32 {
        if network.region != self.id {
            return 0;
        }

        // TODO(CR2 perf): producer export requests re-resolve local power for every
        // request. Cache per-network remaining capacity for one scheduling pass
        // once cross-region exports move beyond the first implementation.
        ResourceRegistry::for_power(&self.world)
            .resolve_local_power()
            .network_capacities
            .into_iter()
            .find(|capacity| capacity.road_network == network.road_network)
            .map(|capacity| capacity.remaining_capacity)
            .unwrap_or(0)
    }

    pub(crate) fn begin_tick_power_demand_phase(&mut self) -> RegionalTickPowerPhase {
        let phase = begin_tick_power_phase(&mut self.world);
        let power_demands = self.pending_power_demands();
        RegionalTickPowerPhase {
            phase,
            power_demands,
        }
    }

    /// Advances from the resolved power phase into the local job assignment phase.
    ///
    /// Runs the post-power systems and (on a daily boundary) local job assignment,
    /// then collects the job seekers that found no reachable local slot so the
    /// runtime can request remote workplace slots before the economy settles.
    pub(crate) fn continue_tick_to_job_demand_phase(
        &mut self,
        power_phase: RegionalTickPowerPhase,
    ) -> RegionalTickJobPhase {
        let phase = continue_to_job_phase(&mut self.world, power_phase.phase);
        // Jobs (and the economy) resolve only on a daily boundary, so cross-region
        // job export is sought only then; hourly ticks carry no job demands.
        let job_demands = if phase.is_daily() {
            self.pending_job_demands()
        } else {
            Vec::new()
        };
        RegionalTickJobPhase { phase, job_demands }
    }

    /// Finishes the tick after job exports resolve.
    ///
    /// `exported_job_slots` are this region's workplace entities reserved for
    /// remote workers; the economy accrues their workplace tax to this region.
    pub(crate) fn finish_tick_job_demand_phase(
        &mut self,
        phase: RegionalTickJobPhase,
        exported_job_slots: &[Entity],
    ) -> CommandResult {
        finish_tick_after_job_phase(&mut self.world, phase.phase, exported_job_slots)
    }

    pub(crate) fn apply_power_export_grant(
        &mut self,
        demand: PendingPowerDemand,
        grant: PowerExportGrant,
    ) {
        if !grant.granted {
            return;
        }
        let Some(source_region) = grant.source_region else {
            return;
        };
        let Some(consumer) = self.world.power_consumers.get_mut(&demand.consumer) else {
            return;
        };
        if consumer.powered || consumer.demand != demand.demand {
            return;
        }

        consumer.powered = true;
        consumer.source = Some(PowerSource::Imported { source_region });
        // TODO(CR4 visibility): exported power demand is counted as supplied in the
        // consumer region only. Surface producer export load separately when
        // regional power trade stats are added.
        self.world.stats.power.total_power_supplied += demand.demand;
        self.world.stats.power.total_power_shortage = (self.world.stats.power.total_power_demand
            - self.world.stats.power.total_power_supplied)
            .max(0);
    }

    /// Records a granted remote workplace on the still-jobless caller citizen.
    ///
    /// The consumer stores only owned summary data (region + opaque slot id +
    /// salary); the exporting region keeps the slot's workplace tax. A citizen that
    /// gained a local job since the request, or an already-remote one, is skipped.
    ///
    /// The salary is captured here at grant time and paid even if the producer's
    /// slot stops being effective before its economy runs (producer-side tax is
    /// guarded by `is_effective_workplace`, so it would not collect). The window is
    /// one tick with no intervening world mutation, so this minor producer/consumer
    /// asymmetry is harmless.
    pub(crate) fn apply_job_export_grant(
        &mut self,
        demand: PendingJobDemand,
        grant: JobExportGrant,
    ) {
        if !grant.granted {
            return;
        }
        let (Some(source_region), Some(slot_id)) = (grant.source_region, grant.slot_id) else {
            return;
        };
        let Some(citizen) = self.world.citizens.get_mut(&demand.citizen) else {
            return;
        };
        if citizen.workplace.is_some() || citizen.remote_workplace.is_some() {
            return;
        }
        citizen.remote_workplace = Some(RemoteWorkplace {
            region: source_region,
            slot_id,
            salary: grant.salary,
        });
    }

    /// Returns spare local workplace slot entities reachable from one road network.
    ///
    /// These are the slots left unassigned after local job resolution whose
    /// building connects to `network`. The producer exports from this set; jobs are
    /// network-scoped across regions exactly like power, so a slot on a different
    /// local network (a different component) is never offered.
    pub(crate) fn spare_job_slots_on_network(&self, network: RegionRoadNetworkId) -> Vec<Entity> {
        if network.region != self.id {
            return Vec::new();
        }

        // TODO(CR3 perf): this rebuilds the jobs registry (the full local
        // assignment) on every export request, and `workplace_salary` re-resolves
        // per grant -- exactly the per-tick cost the R2 split aimed to avoid. Cache
        // spare slots per scheduling pass, mirroring TODO(CR2 perf) for power.
        let Some(roads) = road_connectivity::discover_road_networks(&self.world)
            .into_iter()
            .find(|candidate| candidate.id == network.road_network)
            .map(|candidate| candidate.roads)
        else {
            return Vec::new();
        };

        ResourceRegistry::for_jobs(&self.world)
            .remaining_job_slots()
            .iter()
            .copied()
            .filter(|slot| {
                road_connectivity::adjacent_road_entities(&self.world, *slot)
                    .any(|road| roads.contains(&road))
            })
            .collect()
    }

    /// Salary an exported workplace slot pays its (remote) worker.
    ///
    /// Captured at grant time so the home region can pay the citizen without
    /// reading this region's `World`. Zero if the slot is no longer effective.
    pub(crate) fn workplace_salary(&self, slot: Entity) -> i32 {
        crate::core::systems::economy::salary_for_workplace(&self.world, slot).unwrap_or(0)
    }

    /// Rebuilds transient imported cache state from authoritative local data.
    ///
    /// Regional export generation does not exist yet, so the current
    /// authoritative rebuild is an empty cache. Later export rules can populate
    /// this method without making imported resources permanent save data.
    pub fn rebuild_imported_resource_cache(&mut self) {
        self.imported_resources = ImportedResourceCache::new();
    }

    pub fn neighbor_import_results(&self) -> &[ImportedResourceResult] {
        &self.neighbor_import_results
    }

    pub(crate) fn into_save_record(self) -> RegionStateSaveRecord {
        let mut world = self.world;
        scrub_transient_import_state_for_save(&mut world);
        RegionStateSaveRecord { id: self.id, world }
    }

    pub(crate) fn from_save_record(record: RegionStateSaveRecord) -> Self {
        Self::from_world(record.id, record.world)
    }

    pub(crate) fn from_legacy_world_bytes(
        id: RegionId,
        bytes: &[u8],
    ) -> Result<Self, serde_json::Error> {
        let world = serde_json::from_slice(bytes)?;
        Ok(Self::from_world(id, world))
    }

    pub(crate) fn from_world(id: RegionId, mut world: World) -> Self {
        world.rebuild_entity_records();
        refresh_derived_state_for_world(&mut world);

        let mut state = Self {
            id,
            world,
            imported_resources: ImportedResourceCache::new(),
            neighbor_import_results: Vec::new(),
        };
        state.rebuild_imported_resource_cache();
        state
    }

    fn pending_power_demands(&self) -> Vec<PendingPowerDemand> {
        let border_networks = self
            .network_border_links()
            .into_iter()
            .map(|link| link.network)
            .collect::<Vec<_>>();
        if border_networks.is_empty() {
            return Vec::new();
        }

        let networks = road_connectivity::discover_road_networks(&self.world);
        let mut consumers = self
            .world
            .power_consumers
            .keys()
            .copied()
            .collect::<Vec<_>>();
        road_connectivity::sort_entities_by_position(&self.world, &mut consumers);

        let mut demands = Vec::new();
        for consumer in consumers {
            let Some(power_consumer) = self.world.power_consumers.get(&consumer) else {
                continue;
            };
            if power_consumer.powered {
                continue;
            }
            let Some(caller_network) = networks
                .iter()
                .filter(|network| {
                    border_networks.contains(&RegionRoadNetworkId {
                        region: self.id,
                        road_network: network.id,
                    })
                })
                .find(|network| {
                    road_connectivity::adjacent_road_entities(&self.world, consumer)
                        .any(|road| network.roads.contains(&road))
                })
                .map(|network| RegionRoadNetworkId {
                    region: self.id,
                    road_network: network.id,
                })
            else {
                continue;
            };

            demands.push(PendingPowerDemand {
                token: demands.len() as u32,
                consumer,
                demand: power_consumer.demand,
                caller_network,
            });
        }
        demands
    }

    fn pending_job_demands(&self) -> Vec<PendingJobDemand> {
        let border_networks = self
            .network_border_links()
            .into_iter()
            .map(|link| link.network)
            .collect::<Vec<_>>();
        if border_networks.is_empty() {
            return Vec::new();
        }

        let networks = road_connectivity::discover_road_networks(&self.world);
        let mut citizens = self.world.citizens.keys().copied().collect::<Vec<_>>();
        citizens.sort_by_key(|citizen| citizen.0);

        let mut demands = Vec::new();
        for citizen in citizens {
            let Some(citizen_data) = self.world.citizens.get(&citizen) else {
                continue;
            };
            // Only a citizen left without any local or remote workplace seeks one.
            if citizen_data.workplace.is_some() || citizen_data.remote_workplace.is_some() {
                continue;
            }
            let home = citizen_data.home;
            // A citizen reaches remote slots through the border road network its
            // home connects to, mirroring the power consumer's caller network.
            let Some(caller_network) = networks
                .iter()
                .filter(|network| {
                    border_networks.contains(&RegionRoadNetworkId {
                        region: self.id,
                        road_network: network.id,
                    })
                })
                .find(|network| {
                    road_connectivity::adjacent_road_entities(&self.world, home)
                        .any(|road| network.roads.contains(&road))
                })
                .map(|network| RegionRoadNetworkId {
                    region: self.id,
                    road_network: network.id,
                })
            else {
                continue;
            };

            demands.push(PendingJobDemand {
                token: demands.len() as u32,
                citizen,
                caller_network,
            });
        }
        demands
    }
}

/// Removes transient cross-region allocation results before saving a region.
///
/// ```text
/// authoritative world buildings/citizens/resources
///        |
///        v
/// scrub transient import results
///   - powered/source from latest power phase
///   - remote workplaces from latest daily job phase
///        |
///        v
/// save durable region world only
///        |
///        v
/// load/start derives local registries, topology, hints, and requests exports again
/// ```
///
/// Power and job export grants are runtime coordination, not durable world truth.
/// The loaded runner recomputes local derived state from buildings/resources, then
/// future regional ticks rebuild imports through the normal event flow.
fn scrub_transient_import_state_for_save(world: &mut World) {
    for consumer in world.power_consumers.values_mut() {
        consumer.powered = false;
        // `source` is already skipped by serde, but the save record also feeds
        // the post-save restarted game. Clear it here so restart and load share
        // the same "derived state must be rebuilt" boundary.
        consumer.source = None;
    }
    for citizen in world.citizens.values_mut() {
        // `remote_workplace` is also skipped by serde; clearing it keeps the
        // recovered in-memory game consistent with a fresh load from disk.
        citizen.remote_workplace = None;
    }
}

fn border_links_for_cell(
    network: RegionRoadNetworkId,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Vec<NetworkBorderLink> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let mut links = Vec::new();
    if y == 0 {
        links.push(NetworkBorderLink {
            network,
            link: BorderLinkId {
                edge: BorderEdge::North,
                offset: x,
            },
        });
    }
    if y == height - 1 {
        links.push(NetworkBorderLink {
            network,
            link: BorderLinkId {
                edge: BorderEdge::South,
                offset: x,
            },
        });
    }
    if x == 0 {
        links.push(NetworkBorderLink {
            network,
            link: BorderLinkId {
                edge: BorderEdge::West,
                offset: y,
            },
        });
    }
    if x == width - 1 {
        links.push(NetworkBorderLink {
            network,
            link: BorderLinkId {
                edge: BorderEdge::East,
                offset: y,
            },
        });
    }
    links
}

fn exported_resource_kind_for_building(kind: BuildingKind) -> Option<ResourceKind> {
    match kind {
        BuildingKind::Road => None,
        BuildingKind::Residential => Some(ResourceKind::ServiceAccess),
        BuildingKind::Commercial => Some(ResourceKind::ShoppingAccess),
        BuildingKind::Industrial => Some(ResourceKind::Jobs),
        BuildingKind::PowerPlant => Some(ResourceKind::ServiceAccess),
        BuildingKind::Park => Some(ResourceKind::ParkAccess),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_record_restores_authoritative_world_without_import_cache() {
        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(1, 1, BuildingKind::Residential).success);
        let result = region.process_imported_resource(
            ImportedResource {
                id: ResourceId {
                    origin_region: RegionId(9),
                    resource_kind: ResourceKind::Jobs,
                    generation: 1,
                },
                remaining_capacity: 4,
                hop_count: 0,
                max_hops: 2,
                travel_cost: 0,
                source_neighbor: RegionId(9),
            },
            0,
            1,
            &[],
        );
        assert_eq!(result.decision, ImportDecision::Accepted);
        assert_eq!(region.imported_resources().len(), 1);

        let saved_view = region.view();
        let restored = RegionState::from_save_record(region.into_save_record());

        assert_eq!(restored.view(), saved_view);
        assert!(restored.imported_resources().is_empty());
        assert!(restored.neighbor_import_results().is_empty());
    }

    #[test]
    fn exported_resource_counts_use_authoritative_buildings_without_view_adapter() {
        let mut region = RegionState::new(RegionId(1), 4, 4);
        assert!(region.build(0, 0, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Park).success);
        assert!(region.build(2, 0, BuildingKind::Park).success);
        assert!(region.build(3, 0, BuildingKind::Industrial).success);

        assert_eq!(
            region.exported_resource_counts(),
            vec![(ResourceKind::Jobs, 1), (ResourceKind::ParkAccess, 2)]
        );
    }

    #[test]
    fn regional_state_imports_shared_simulation_helpers_not_game_facade() {
        let source = std::fs::read_to_string("src/core/regions/mod.rs").expect("region source");
        let forbidden = ["crate::core::", "game"].concat();

        assert!(!source.contains(&forbidden));
        assert!(source.contains("crate::core::simulation"));
    }
}
