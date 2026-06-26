//! Regional state ownership and owned cross-region resource summaries.
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
//! Cross-region sharing uses discovery plus producer-owned export allocation.
//! Regions exchange owned power/job requests and grants through runtimes; no
//! region stores another region's generic exported-resource cache.
//! ```

use std::collections::HashMap;

use crate::core::city_refs::CityCellRef;
use crate::core::components::{
    PendingHandoff, Position, PowerSource, ReturnHop, TravelPurpose, TravelState, TravelerHandoff,
    WorkplaceAssignment,
};
use crate::core::entity::Entity;
use crate::core::resources::CityStats;
use crate::core::simulation::{
    TickJobPhase, TickPowerPhase, begin_tick_power_phase, continue_to_job_phase,
    ensure_derived_state, finish_tick_after_goods_phase, finish_tick_after_job_phase,
    refresh_derived_state_for_world,
};
use crate::core::systems::{
    build, bulldoze, economy, power, replace, road_connectivity, travel, upgrade,
};
use crate::core::world::{CrossRegionGoodsRoutes, World};
use crate::interface::adapter::{
    inspect_world, remote_workers_for, view_world, view_world_with_overlay,
};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, CitizenDetailView, GameView, InspectView};
use serde::{Deserialize, Serialize};

pub mod directory;
pub mod handle;
pub mod runtime;
pub mod threaded;
pub mod worker;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Stable identity for one independently owned simulation region.
///
/// Future runtimes and workers will use this as a routing key. It is not an ECS
/// entity ID and should never identify another region's local `World` storage.
pub struct RegionId(pub u32);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Owned region-level spare capacity summary for cross-region planning.
///
/// This intentionally contains only aggregate counts. It does not expose ECS
/// entities, component references, or handles to this region's private `World`.
pub(crate) struct RegionalSpareCapacity {
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

#[derive(Debug, Clone, PartialEq, Eq)]
/// Stale-tolerant availability hint published for regional discovery.
///
/// Claims still have to be confirmed by the source region runtime.
pub struct RegionalAvailabilityHint {
    pub network: RegionRoadNetworkId,
    pub has_spare_power: bool,
    /// Opaque producer-owned ids for spare workplace slots reachable on this
    /// road network.
    ///
    /// Consumers only use these for stale capacity counting; actual assignment
    /// still comes from the producer runtime's authoritative export grant.
    pub spare_job_slot_ids: Vec<u32>,
    /// Exportable industrial goods left after local commercial storage is filled.
    ///
    /// Goods are fungible, so a scalar per road network is enough; producer-side
    /// allocation still authoritatively reserves units before consumers can use
    /// them.
    pub spare_goods_units: u32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Caller-local commercial goods demand that may need producer-exported units.
pub(crate) struct PendingGoodsDemand {
    pub token: u32,
    pub commercial: Entity,
    pub units: u32,
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

#[derive(Debug)]
/// Paused tick state after job exports and before future goods exports.
pub(crate) struct RegionalTickGoodsPhase {
    phase: RegionalTickJobPhase,
    pub goods_demands: Vec<PendingGoodsDemand>,
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
/// Unlike power, the grant carries identity as city-wide refs: the producer's
/// `workplace` (a region-tagged `Entity`, owned by the producer; the consumer
/// never dereferences it, only stores/echoes it), its `location` (the self-describing
/// workplace cell for the consumer's roster/display), and the `salary` the home region
/// pays the worker. Workplace tax accrues to the exporting region instead.
pub struct JobExportGrant {
    pub token: u32,
    pub granted: bool,
    pub workplace: Option<Entity>,
    pub location: Option<CityCellRef>,
    pub salary: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Result of an authoritative producer-owned goods export allocation request.
pub struct GoodsExportGrant {
    pub token: u32,
    pub granted: bool,
    pub source_region: Option<RegionId>,
    pub units: u32,
}

#[derive(Debug, Serialize, Deserialize)]
/// Serialized authoritative region state.
pub(crate) struct RegionStateSaveRecord {
    id: RegionId,
    world: World,
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
}

impl RegionState {
    /// Creates a region with its own private ECS world and empty import cache.
    pub fn new(id: RegionId, width: usize, height: usize) -> Self {
        let mut world = World::new(width, height);
        world.set_region_id(id);
        Self { id, world }
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    /// Sets this region's tunable building rules (the regional game injects the save-stamped ruleset).
    pub(crate) fn set_building_rules(&mut self, rules: crate::core::building_rules::BuildingRules) {
        self.world.set_building_rules(rules);
    }

    /// This region's tunable building rules.
    pub(crate) fn building_rules(&self) -> crate::core::building_rules::BuildingRules {
        self.world.building_rules().clone()
    }

    /// Advances only this region's local simulation using the shared tick order.
    pub fn tick_local(&mut self) -> CommandResult {
        let phase = begin_tick_power_phase(&mut self.world, self.id);
        let job_phase = continue_to_job_phase(&mut self.world, self.id, phase);
        finish_tick_after_job_phase(&mut self.world, job_phase, &[])
    }

    /// Applies one player build command through the core systems.
    ///
    /// DT1: the build only mutates config (which marks the derived state dirty);
    /// the derived pass is recomputed lazily at the next view/inspect/tick read,
    /// so a paused build still updates the view without advancing time.
    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = build::build(&mut self.world, x, y, kind);
        if result.success {
            self.world.mark_derived_dirty();
        }
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
            self.world.mark_derived_dirty();
        }
        result
    }

    /// Replaces one occupied cell through the core systems.
    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = replace::replace(&mut self.world, x, y, kind);
        if result.success {
            self.world.mark_derived_dirty();
        }
        result
    }

    /// Upgrades one supported occupied cell through the core systems.
    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        let result = upgrade::upgrade(&mut self.world, x, y);
        if result.success {
            self.world.mark_derived_dirty();
        }
        result
    }

    /// Recomputes the derived pass if a config change has marked it dirty (DT1).
    ///
    /// Read boundaries that hold `&mut RegionState` call this before reading the
    /// world so a paused command (which only marks dirty) is reflected. The tick
    /// path recomputes via `begin_tick_power_phase` instead. Derived-summary reads
    /// that gate on applied power (`availability_hints`, `regional_spare_capacity`,
    /// `spare_job_slots_on_network`) require the caller to bring derived state
    /// current first; the worker does this before publishing summaries.
    pub fn ensure_derived_state(&mut self) {
        ensure_derived_state(&mut self.world, self.id);
    }

    /// Returns a UI-safe snapshot without exposing this region's ECS world.
    ///
    /// This is a pure read of already-applied derived state. Because regions are
    /// shared for reading through immutable `RegionRuntime::state()`/`region()`
    /// accessors, and the derived pass needs `&mut` to write applied state, the
    /// DT1 recompute-on-read happens at the owning `&mut` boundary (the runtime
    /// snapshot/inspect handlers and the worker before publishing summaries), not
    /// here. A direct caller reading after a paused command must call
    /// `ensure_derived_state` first.
    pub fn view(&self) -> GameView {
        view_world(&self.world)
    }

    /// Returns owned derived stats without exposing this region's ECS world.
    ///
    /// This is a core-facing inspection surface for scheduler/parity checks.
    /// UI code should continue to read stats through `GameView`.
    pub fn stats_snapshot(&self) -> CityStats {
        self.world.stats.clone()
    }

    /// Returns a UI-safe snapshot using the requested map overlay.
    pub fn view_with_overlay(&self, overlay: MapOverlayInput) -> GameView {
        view_world_with_overlay(&self.world, overlay)
    }

    /// Returns a UI-safe inspect model without exposing this region's ECS world.
    pub fn inspect(&self, x: usize, y: usize) -> InspectView {
        inspect_world(&self.world, x, y)
    }

    /// Local residents who commute to the workplace at `(producer_region, pos)`
    /// in another region — the reverse of the local-only workplace roster.
    ///
    /// The producer's export ledger holds only an opaque slot count, never worker
    /// identities, so a workplace's remote staff can only be enumerated from the
    /// consumer regions where they live. This region tags each returned worker
    /// with itself (`self.id`) as their home region.
    pub fn remote_workers_for(
        &self,
        producer_region: RegionId,
        position: Position,
    ) -> Vec<CitizenDetailView> {
        remote_workers_for(&self.world, self.id, producer_region, position)
    }

    /// Number of local citizens currently working in another region (CR3 import).
    pub fn imported_job_count(&self) -> usize {
        self.world
            .citizens
            .values()
            .filter(|citizen| {
                // A remote job's workplace is owned by another region.
                citizen
                    .workplace_assignment
                    .is_some_and(|assignment| assignment.workplace.region() != self.id)
            })
            .count()
    }

    #[cfg(test)]
    /// Owned `(source region, producer workplace entity id)` pairs for citizens
    /// working remotely.
    ///
    /// This test-only summary reads the producer-owned `Entity` a remote job
    /// stores. The consumer never dereferences that ref locally (its region is the
    /// producer's); the region tag is what makes carrying it safe. UI should use facade
    /// snapshots instead.
    pub(crate) fn imported_job_slots(&self) -> Vec<(RegionId, u32)> {
        let mut slots = self
            .world
            .citizens
            .values()
            .filter_map(|citizen| {
                let assignment = citizen.workplace_assignment?;
                let workplace = assignment.workplace;
                // Only remote jobs (workplace owned by another region) are imports.
                (workplace.region() != self.id).then_some((workplace.region(), workplace.local()))
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

    /// P5b: install the worker-supplied border→neighbor hint and rebuild the
    /// per-mover exit-cell map from it. Called before each region's processing
    /// slice (like `set_importable_remote_jobs`).
    pub(crate) fn set_border_neighbor_map(&mut self, map: HashMap<BorderLinkId, RegionId>) {
        self.world.border_neighbor_map = map;
        self.refresh_remote_exit_cells();
    }

    /// Rebuilds `remote_exit_cells` (neighbor region → local exit road cells) from
    /// the current border-neighbor hint. Every border road cell whose link faces a
    /// neighbor is an exit toward that neighbor.
    fn refresh_remote_exit_cells(&mut self) {
        let mut map: HashMap<RegionId, Vec<Entity>> = HashMap::new();
        for (cell, link) in self.border_road_links() {
            if let Some(&neighbor) = self.world.border_neighbor_map.get(&link) {
                map.entry(neighbor).or_default().push(cell);
            }
        }
        for cells in map.values_mut() {
            cells.sort();
            cells.dedup();
        }
        self.world.remote_exit_cells = map;
    }

    /// P5b: drain this tick's buffered crossings into routed handoffs, resolving
    /// each border link from the topology. An outbound whose exit cell no longer
    /// maps to a link toward its region is rolled back home (never strands `Away`).
    pub(crate) fn drain_traveler_handoffs(&mut self) -> Vec<TravelerHandoff> {
        let pending = std::mem::take(&mut self.world.outgoing_handoffs);
        let mut handoffs = Vec::new();
        for handoff in pending {
            match handoff {
                PendingHandoff::Outbound {
                    traveler,
                    token,
                    to_region,
                    exit_cell,
                } => match self.exit_link_for(exit_cell, to_region) {
                    Some(entry_link) => handoffs.push(TravelerHandoff {
                        token,
                        traveler,
                        to_region,
                        entry_link,
                        return_path: vec![ReturnHop {
                            region: self.id,
                            entry_link,
                        }],
                        purpose: TravelPurpose::Outbound,
                    }),
                    None => travel::apply_traveler_return(&mut self.world, traveler),
                },
                PendingHandoff::Return {
                    traveler,
                    mut return_path,
                } => {
                    // Pop the last hop: it names the region to return to and the link.
                    if let Some(hop) = return_path.pop() {
                        handoffs.push(TravelerHandoff {
                            token: TravelState::default(), // unused on Return
                            traveler,
                            to_region: hop.region,
                            entry_link: hop.entry_link,
                            return_path,
                            purpose: TravelPurpose::Return,
                        });
                    }
                }
            }
        }
        handoffs
    }

    /// P5b: apply an inbound crossing. Outbound → place the token at the local
    /// entry cell (mapped from the sender's exit link) so it walks to the
    /// workplace; if the link can't be placed (topology drift), bounce a `Return`
    /// so the home citizen is never stranded. Return → clear the `Away` mark.
    /// Returns any handoffs the worker must route onward (the bounce).
    pub(crate) fn receive_traveler_handoff(
        &mut self,
        handoff: TravelerHandoff,
    ) -> Vec<TravelerHandoff> {
        match handoff.purpose {
            TravelPurpose::Outbound => {
                let local_link = handoff.entry_link.matching_neighbor_link();
                match self.cell_at_border_link(local_link) {
                    Some(entry_cell) => {
                        travel::receive_traveler(
                            &mut self.world,
                            handoff.traveler,
                            handoff.token,
                            entry_cell,
                            handoff.return_path,
                        );
                        Vec::new()
                    }
                    None => {
                        // Can't place — bounce a Return along the path it came in on.
                        let mut return_path = handoff.return_path;
                        return_path
                            .pop()
                            .map(|hop| TravelerHandoff {
                                token: TravelState::default(),
                                traveler: handoff.traveler,
                                to_region: hop.region,
                                entry_link: hop.entry_link,
                                return_path,
                                purpose: TravelPurpose::Return,
                            })
                            .into_iter()
                            .collect()
                    }
                }
            }
            TravelPurpose::Return => {
                travel::apply_traveler_return(&mut self.world, handoff.traveler);
                Vec::new()
            }
        }
    }

    /// `(cell, border link)` for every border road cell, in deterministic order.
    fn border_road_links(&self) -> Vec<(Entity, BorderLinkId)> {
        let width = self.world.grid.width();
        let height = self.world.grid.height();
        // The network id only tags the returned `NetworkBorderLink`; the
        // `BorderLinkId` itself is network-independent, so a placeholder is fine.
        let network = RegionRoadNetworkId {
            region: self.id,
            road_network: 0,
        };
        let mut out = Vec::new();
        for road in road_connectivity::road_entities_by_position(&self.world) {
            if let Some(position) = self.world.positions.get(&road) {
                for link in border_links_for_cell(network, position.x, position.y, width, height) {
                    out.push((road, link.link));
                }
            }
        }
        out
    }

    /// The border link from `exit_cell` that faces `to_region`, if any.
    fn exit_link_for(&self, exit_cell: Entity, to_region: RegionId) -> Option<BorderLinkId> {
        let position = self.world.positions.get(&exit_cell)?;
        let width = self.world.grid.width();
        let height = self.world.grid.height();
        let network = RegionRoadNetworkId {
            region: self.id,
            road_network: 0,
        };
        border_links_for_cell(network, position.x, position.y, width, height)
            .into_iter()
            .map(|link| link.link)
            .find(|link| self.world.border_neighbor_map.get(link) == Some(&to_region))
    }

    /// The local road cell sitting on `link`, if it exists and is a road.
    fn cell_at_border_link(&self, link: BorderLinkId) -> Option<Entity> {
        let width = self.world.grid.width();
        let height = self.world.grid.height();
        let (x, y) = match link.edge {
            BorderEdge::North => (link.offset, 0),
            BorderEdge::South => (link.offset, height.checked_sub(1)?),
            BorderEdge::West => (0, link.offset),
            BorderEdge::East => (width.checked_sub(1)?, link.offset),
        };
        let cell = self.world.grid.get(x, y)?;
        road_connectivity::is_road_entity(&self.world, cell).then_some(cell)
    }

    /// Returns aggregate spare local capacity without exposing ECS storage.
    ///
    /// Power spare capacity is the remaining pooled capacity after local power
    /// grants. Job spare capacity is the unused effective workplace slots after
    /// local citizens are accounted for.
    #[cfg(test)]
    pub(crate) fn regional_spare_capacity(&self) -> RegionalSpareCapacity {
        let power = self.world.cached_power_resolution();
        let jobs = self.world.cached_job_resolution();

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
        let power = self.world.cached_power_resolution();
        let mut hints = power
            .network_capacities
            .into_iter()
            .map(|capacity| {
                let network = RegionRoadNetworkId {
                    region: self.id,
                    road_network: capacity.road_network,
                };
                let mut spare_job_slot_ids = self
                    .spare_job_slots_on_network(network)
                    .into_iter()
                    .map(|slot| slot.local())
                    .collect::<Vec<_>>();
                spare_job_slot_ids.sort();
                RegionalAvailabilityHint {
                    network,
                    has_spare_power: capacity.remaining_capacity > 0,
                    spare_job_slot_ids,
                    spare_goods_units: economy::exportable_goods_units_on_network(
                        &self.world,
                        network.road_network,
                    ),
                }
            })
            .collect::<Vec<_>>();
        hints.sort_by_key(|hint| hint.network);
        hints
    }

    pub(crate) fn set_importable_remote_jobs(&mut self, jobs: i32) {
        self.world.importable_remote_jobs = jobs.max(0);
    }

    pub(crate) fn set_cross_region_goods_routes(&mut self, routes: CrossRegionGoodsRoutes) {
        self.world.cross_region_goods_routes = routes;
    }

    pub(crate) fn power_network_remaining_capacity(&self, network: RegionRoadNetworkId) -> i32 {
        if network.region != self.id {
            return 0;
        }

        // Reads the persistent registry cache; producer allocation remains
        // authoritative in the runtime ledger on top of this local spare summary.
        self.world
            .cached_power_resolution()
            .network_capacities
            .into_iter()
            .find(|capacity| capacity.road_network == network.road_network)
            .map(|capacity| capacity.remaining_capacity)
            .unwrap_or(0)
    }

    pub(crate) fn goods_network_remaining_units(&self, network: RegionRoadNetworkId) -> u32 {
        if network.region != self.id {
            return 0;
        }
        economy::exportable_goods_units_on_network(&self.world, network.road_network)
    }

    pub(crate) fn begin_tick_power_demand_phase(&mut self) -> RegionalTickPowerPhase {
        let phase = begin_tick_power_phase(&mut self.world, self.id);
        let power_demands = self.pending_power_demands();
        RegionalTickPowerPhase {
            phase,
            power_demands,
        }
    }

    pub(crate) fn power_import_settlement_demands(&mut self) -> Vec<PendingPowerDemand> {
        // Load-time settlement is time-neutral: re-run only local power to clear
        // transient imported flags, then let the normal producer-owned export
        // request/grant flow reapply imports.
        power::run(&mut self.world);
        self.pending_power_demands()
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
        let phase = continue_to_job_phase(&mut self.world, self.id, power_phase.phase);
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

    pub(crate) fn continue_tick_to_goods_demand_phase(
        &mut self,
        job_phase: RegionalTickJobPhase,
    ) -> RegionalTickGoodsPhase {
        let goods_demands = if job_phase.is_daily() {
            self.pending_goods_demands()
        } else {
            Vec::new()
        };
        RegionalTickGoodsPhase {
            phase: job_phase,
            goods_demands,
        }
    }

    pub(crate) fn finish_tick_goods_demand_phase(
        &mut self,
        phase: RegionalTickGoodsPhase,
        exported_job_slots: &[Entity],
        exported_goods_units: u32,
    ) -> CommandResult {
        finish_tick_after_goods_phase(
            &mut self.world,
            phase.phase.phase,
            exported_job_slots,
            exported_goods_units,
        )
    }

    pub(crate) fn apply_power_export_grant(
        &mut self,
        demand: PendingPowerDemand,
        grant: PowerExportGrant,
    ) {
        // Power grants, including denials, complete the cross-region power phase.
        // Effective workplaces depend on final powered state, so job registry
        // readers must re-check after this phase instead of trusting a cache built
        // before imported power resolved or was lost.
        self.world.invalidate_jobs_registry();
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
        let (Some(workplace), Some(location)) = (grant.workplace, grant.location) else {
            return;
        };
        let Some(citizen) = self.world.citizens.get_mut(&demand.citizen) else {
            return;
        };
        if citizen.workplace_assignment.is_some() {
            return;
        }
        citizen.workplace_assignment = Some(WorkplaceAssignment {
            workplace,
            location,
            salary: grant.salary,
        });
        self.world.invalidate_jobs_registry();
    }

    pub(crate) fn add_commercial_goods(&mut self, commercial: Entity, units: u32) {
        economy::add_commercial_goods(&mut self.world, commercial, units as i32);
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

        let Some(roads) = road_connectivity::discover_road_networks(&self.world)
            .into_iter()
            .find(|candidate| candidate.id == network.road_network)
            .map(|candidate| candidate.roads)
        else {
            return Vec::new();
        };

        self.world
            .with_cached_remaining_job_workplaces(|remaining_workplaces| {
                remaining_workplaces
                    .iter()
                    .copied()
                    .filter(|slot| {
                        road_connectivity::adjacent_road_entities(&self.world, *slot)
                            .any(|road| roads.contains(&road))
                    })
                    .collect()
            })
    }

    /// Salary an exported workplace slot pays its (remote) worker.
    ///
    /// Captured at grant time so the home region can pay the citizen without
    /// reading this region's `World`. Zero if the slot is no longer effective.
    pub(crate) fn workplace_salary(&self, slot: Entity) -> i32 {
        crate::core::systems::economy::salary_for_workplace(&self.world, slot).unwrap_or(0)
    }

    pub(crate) fn workplace_position(&self, slot: Entity) -> Option<Position> {
        self.world.positions.get(&slot).copied()
    }

    /// The anchor `Position` of whatever building occupies cell `(x, y)`.
    ///
    /// A multi-cell building is one entity with a single `Position` (its anchor),
    /// and the grid maps every footprint cell back to that entity. So any clicked
    /// footprint cell resolves to the same anchor — exactly the position a remote
    /// worker's assignment records (`workplace_position`). The remote-roster reverse
    /// lookup normalizes the clicked cell through this before matching, so a remote
    /// worker shows on every footprint cell, not only the anchor.
    ///
    /// ```text
    ///   grid:  (1,2)->E (2,2)->E      positions: E -> (1,2)   (anchor)
    ///   click (2,2) ─► grid.get ─► E ─► positions.get ─► (1,2) == assignment.position
    /// ```
    pub(crate) fn building_anchor_at(&self, x: usize, y: usize) -> Option<Position> {
        let entity = self.world.grid.get(x, y)?;
        self.world.positions.get(&entity).copied()
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
        // Stamp the owning region onto the world (and rebuild each citizen's `id`
        // from its map key) before derived state reads it. Homes need no stamping:
        // the `home` Entity already packs its birth region.
        world.set_region_id(id);
        refresh_derived_state_for_world(&mut world, id);

        Self { id, world }
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
            if citizen_data.workplace_assignment.is_some() {
                continue;
            }
            // Home is always local to this region; it's already the local id.
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

    fn pending_goods_demands(&self) -> Vec<PendingGoodsDemand> {
        let border_networks = self
            .network_border_links()
            .into_iter()
            .map(|link| link.network)
            .collect::<Vec<_>>();
        if border_networks.is_empty() {
            return Vec::new();
        }

        let mut demands = Vec::new();
        for (commercial, units, network_id) in
            economy::commercial_goods_demands_after_local_distribution(&self.world)
        {
            let caller_network = RegionRoadNetworkId {
                region: self.id,
                road_network: network_id,
            };
            if !border_networks.contains(&caller_network) {
                continue;
            }
            // ponytail: one message per unit because ExportResource grants are
            // all-or-deny today. If goods capacity gets large, replace this with
            // a batched request and producer partial-grant support.
            for _ in 0..units {
                demands.push(PendingGoodsDemand {
                    token: demands.len() as u32,
                    commercial,
                    units: 1,
                    caller_network,
                });
            }
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
        citizen.workplace_assignment = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::systems::citizens;
    use crate::interface::input::BuildingKind;

    #[test]
    fn regional_state_imports_shared_simulation_helpers_not_game_facade() {
        let source = std::fs::read_to_string("src/core/regions/mod.rs").expect("region source");
        let forbidden = ["crate::core::", "game"].concat();

        assert!(!source.contains(&forbidden));
        assert!(source.contains("crate::core::simulation"));
    }

    #[test]
    fn imported_job_slots_are_owned_region_and_slot_summaries() {
        let mut region = RegionState::new(RegionId(1), 2, 1);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        let home = region.world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut region.world, home, 1);
        let citizen = *region.world.citizens.keys().next().expect("citizen");
        assert_eq!(region.world.cached_job_counts().unemployment, 1);

        region.apply_job_export_grant(
            PendingJobDemand {
                token: 7,
                citizen,
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
            },
            JobExportGrant {
                token: 7,
                granted: true,
                workplace: Some(Entity::new(RegionId(2), 42)),
                location: Some(CityCellRef::local(RegionId(2), 1, 0)),
                salary: 4,
            },
        );

        assert_eq!(region.imported_job_slots(), vec![(RegionId(2), 42)]);
        assert_eq!(region.world.cached_job_counts().unemployment, 0);
    }

    #[test]
    fn remote_workers_for_lists_commuters_by_producer_cell() {
        use crate::interface::view::CitizenRelation;

        // Region 1 (consumer): two residents at (0,0); one takes a remote job at
        // region 2 cell (1,0), the other stays unemployed.
        let mut region = RegionState::new(RegionId(1), 2, 1);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        let home = region.world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut region.world, home, 2);
        let mut residents = region.world.citizens.keys().copied().collect::<Vec<_>>();
        residents.sort_by_key(|entity| entity.0);
        let commuter = residents[0];

        region.apply_job_export_grant(
            PendingJobDemand {
                token: 1,
                citizen: commuter,
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
            },
            JobExportGrant {
                token: 1,
                granted: true,
                workplace: Some(Entity::new(RegionId(2), 9)),
                location: Some(CityCellRef::local(RegionId(2), 1, 0)),
                salary: 4,
            },
        );

        // Matches the producer (region, cell): one commuter, tagged with its own
        // region (1) as home, at its home cell (0,0).
        let workers = region.remote_workers_for(RegionId(2), Position { x: 1, y: 0 });
        assert_eq!(workers.len(), 1);
        assert!(matches!(
            workers[0].relation,
            CitizenRelation::LivesAt {
                region: Some(RegionId(1)),
                x: 0,
                y: 0
            }
        ));

        // Wrong cell, wrong producer region, and the unemployed resident: nothing.
        assert!(
            region
                .remote_workers_for(RegionId(2), Position { x: 0, y: 0 })
                .is_empty()
        );
        assert!(
            region
                .remote_workers_for(RegionId(3), Position { x: 1, y: 0 })
                .is_empty()
        );
    }

    #[test]
    fn ensure_derived_state_makes_a_paused_build_visible_to_a_direct_view() {
        // RegionState::view() is a pure read (the derived pass runs at the owning
        // &mut boundary). A direct caller reading after a paused command brings the
        // derived state current via ensure_derived_state, then sees the new config.
        let mut region = RegionState::new(RegionId(21), 4, 4);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success);
        assert!(region.build(1, 1, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Commercial).success);

        region.ensure_derived_state();
        let powered = region
            .view()
            .map
            .cells
            .iter()
            .find(|cell| cell.x == 1 && cell.y == 0)
            .and_then(|cell| cell.powered);
        assert_eq!(powered, Some(true), "paused build is visible after ensure");
        assert_eq!(region.view().status.turn, 0, "no tick advanced time");
    }

    #[test]
    fn regional_spare_capacity_matches_local_registry_remaining_capacity() {
        let mut region = RegionState::new(RegionId(5), 5, 3);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success);
        assert!(region.build(1, 1, BuildingKind::Road).success);
        assert!(region.build(2, 1, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Commercial).success);
        assert!(region.build(2, 0, BuildingKind::Industrial).success);

        // DT1: spare capacity is a derived-state read; bring the derived pass
        // current after the paused builds (the worker does this before reading).
        region.ensure_derived_state();
        assert_eq!(
            region.regional_spare_capacity(),
            RegionalSpareCapacity {
                power_capacity: 5,
                job_slots: 5,
            }
        );
    }

    #[test]
    fn regional_spare_capacity_keeps_unreachable_jobs_spare() {
        let mut region = RegionState::new(RegionId(7), 6, 3);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success);
        assert!(region.build(1, 1, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Residential).success);

        assert!(region.build(5, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(5, 1, BuildingKind::Road).success);
        assert!(region.build(4, 1, BuildingKind::Road).success);
        assert!(region.build(4, 0, BuildingKind::Commercial).success);

        for _ in 0..24 {
            assert!(region.tick_local().success);
        }

        assert_eq!(region.view().status.population, 1);
        assert_eq!(region.regional_spare_capacity().job_slots, 2);
    }

    #[test]
    fn regional_spare_capacity_is_owned_summary_without_ecs_identity() {
        let region = RegionState::new(RegionId(6), 3, 3);
        let summary = region.regional_spare_capacity();

        let copied = summary;
        assert_eq!(summary, copied);
        assert_eq!(summary.power_capacity, 0);
        assert_eq!(summary.job_slots, 0);
    }

    // ---- P5b: cross-region token handoff (regions wiring) ----

    use crate::core::components::{PendingHandoff, TravelStatus, TravelerId};

    /// An East-edge road in region A; the worker hint says East faces region B.
    /// `set_border_neighbor_map` must record that cell as an exit toward B.
    #[test]
    fn border_hint_populates_remote_exit_cells() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success); // East edge (x = width-1)
        let exit = a.world.grid.get(1, 0).expect("road");
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        a.set_border_neighbor_map(HashMap::from([(link, RegionId(2))]));
        assert_eq!(
            a.world.remote_exit_cells.get(&RegionId(2)),
            Some(&vec![exit])
        );
    }

    /// Draining an outbound resolves the facing border link and seeds the return path.
    #[test]
    fn drain_outbound_resolves_border_link() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success);
        let exit = a.world.grid.get(1, 0).expect("road");
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        a.set_border_neighbor_map(HashMap::from([(link, RegionId(2))]));

        let traveler = TravelerId {
            citizen: Entity::new(RegionId(1), 5),
            generation: 1,
        };
        let workplace = Entity::new(RegionId(2), 9);
        a.world.outgoing_handoffs.push(PendingHandoff::Outbound {
            traveler,
            token: TravelState {
                status: TravelStatus::Traveling,
                current_cell: Some(exit),
                destination: Some(workplace),
                building: None,
            },
            to_region: RegionId(2),
            exit_cell: exit,
        });

        let handoffs = a.drain_traveler_handoffs();
        assert_eq!(handoffs.len(), 1);
        let handoff = &handoffs[0];
        assert_eq!(handoff.to_region, RegionId(2));
        assert_eq!(handoff.entry_link, link);
        assert_eq!(handoff.purpose, TravelPurpose::Outbound);
        assert_eq!(
            handoff.return_path,
            vec![ReturnHop {
                region: RegionId(1),
                entry_link: link
            }]
        );
        assert!(a.world.outgoing_handoffs.is_empty(), "buffer drained");
    }

    /// An unroutable outbound (no facing link) rolls the away citizen back home
    /// rather than stranding it.
    #[test]
    fn drain_outbound_rolls_back_when_unroutable() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success);
        let exit = a.world.grid.get(1, 0).expect("road");
        // No border_neighbor_map entry → the exit faces nothing.
        let citizen = Entity::new(RegionId(1), 5);
        a.world.travel.insert(
            citizen,
            TravelState {
                status: TravelStatus::Away,
                current_cell: None,
                destination: None,
                building: None,
            },
        );
        a.world.away_generation.insert(citizen, 1);
        a.world.outgoing_handoffs.push(PendingHandoff::Outbound {
            traveler: TravelerId {
                citizen,
                generation: 1,
            },
            token: TravelState::default(),
            to_region: RegionId(2),
            exit_cell: exit,
        });

        let handoffs = a.drain_traveler_handoffs();
        assert!(handoffs.is_empty(), "nothing routed");
        assert_eq!(
            a.world.travel[&citizen].status,
            TravelStatus::AtHome,
            "rolled back home"
        );
    }

    /// Receiving an outbound places the token at the matching local entry cell.
    #[test]
    fn receive_outbound_places_token_at_entry_cell() {
        // Region B with a West-edge road at (0,0).
        let mut b = RegionState::new(RegionId(2), 2, 1);
        assert!(b.build(0, 0, BuildingKind::Road).success);
        let entry = b.world.grid.get(0, 0).expect("road");

        let traveler = TravelerId {
            citizen: Entity::new(RegionId(1), 5),
            generation: 1,
        };
        let workplace = Entity::new(RegionId(2), 9);
        // Sender's exit link was East/offset 0; B maps it via matching_neighbor_link.
        let handoff = TravelerHandoff {
            token: TravelState {
                status: TravelStatus::Traveling,
                current_cell: None,
                destination: Some(workplace),
                building: None,
            },
            traveler,
            to_region: RegionId(2),
            entry_link: BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            },
            return_path: vec![ReturnHop {
                region: RegionId(1),
                entry_link: BorderLinkId {
                    edge: BorderEdge::East,
                    offset: 0,
                },
            }],
            purpose: TravelPurpose::Outbound,
        };
        let bounce = b.receive_traveler_handoff(handoff);
        assert!(bounce.is_empty(), "placed, no bounce");
        let visiting = b.world.visiting_travel.get(&traveler).expect("visiting");
        assert_eq!(visiting.token.current_cell, Some(entry));
    }

    /// Receiving a Return clears the home citizen's Away mark.
    #[test]
    fn receive_return_clears_away() {
        let mut a = RegionState::new(RegionId(1), 1, 1);
        let citizen = Entity::new(RegionId(1), 5);
        a.world.travel.insert(
            citizen,
            TravelState {
                status: TravelStatus::Away,
                current_cell: None,
                destination: None,
                building: None,
            },
        );
        a.world.away_generation.insert(citizen, 1);

        let bounce = a.receive_traveler_handoff(TravelerHandoff {
            token: TravelState::default(),
            traveler: TravelerId {
                citizen,
                generation: 1,
            },
            to_region: RegionId(1),
            entry_link: BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            },
            return_path: Vec::new(),
            purpose: TravelPurpose::Return,
        });
        assert!(bounce.is_empty());
        assert_eq!(a.world.travel[&citizen].status, TravelStatus::AtHome);
    }
}
