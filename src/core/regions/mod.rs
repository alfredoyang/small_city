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

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::core::city_refs::CityCellRef;
use crate::core::components::{
    CitizenArrivalAction, HandoffKind, PendingDestinationArrival, PendingHandoff, PlaceRef,
    Position, PowerSource, TravelState, TravelToken, TravelerHandoff, TravelerId,
    WorkplaceAssignment,
};
use crate::core::entity::Entity;
use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::employment_directory::{
    CitizenRef, EmployerState as EmploymentEmployerState, EmploymentContract, JobClaim, JobPool,
};
use crate::core::resources::CityStats;
use crate::core::simulation::{
    TickJobPhase, TickPowerPhase, begin_tick_power_phase, begin_tick_power_phase_quiet,
    clear_imported_power, continue_to_job_phase, ensure_derived_state,
    finish_tick_after_goods_phase, finish_tick_after_job_phase, imported_power_grants,
    reapply_imported_power, refresh_derived_state_for_world,
};
use crate::core::systems::{
    build, bulldoze, economy, power, replace, road_connectivity, travel, upgrade,
};
use crate::core::world::{CrossRegionGoodsRoutes, World};
use crate::interface::adapter::{
    inspect_world, remote_workers_for, road_traveler_panel_seed, view_world,
    view_world_with_overlay,
};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{
    BuildPreviewView, CitizenDetailView, GameView, InspectView, RoadTravelerPanelSeedView,
};
use serde::{Deserialize, Serialize};

pub mod coordinator;
pub mod directory;
pub mod employment_directory;
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

/// P-?: one border-edge on the region road graph: my link faces neighbour.
/// Pairs with the neighbour's complementary link across the same map border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionBorderLink {
    pub link: BorderLinkId,
    pub neighbour: RegionId,
}

/// P-?: the cost of crossing me on the road graph: enter at `entry`, exit at `exit`,
/// where entry and exit are two of my border links. `cost` is the Layer-2 (road-cell)
/// Dijkstra distance between them — share-nothing, each region computes its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionCrossCost {
    pub entry: BorderLinkId,
    pub exit: BorderLinkId,
    pub cost: u32,
}

/// P-?: per-region INPUT to the directory's Layer-1 Dijkstra. The region prices
/// its own crossings (one Layer-2 Dijkstra per border-link pair) and publishes
/// this report alongside the existing availability hint. The directory assembles
/// all reports and runs the small Layer-1 Dijkstra on the region road graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionRoadReport {
    pub region: RegionId,
    pub border_links: Vec<RegionBorderLink>,
    pub crossing_costs: Vec<RegionCrossCost>,
}

/// P-?: the directory's OUTPUT for the Layer-1 router. One Dijkstra-at-T tree:
/// every source's answer to "how do I get toward T?". Outer key = DESTINATION
/// region; inner key = SOURCE region; value = `RouteHop` (next-hop exits).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionRoutes {
    pub to: std::collections::HashMap<RegionId, RouteField>,
}

impl RegionRoutes {
    /// For region `r`: each reachable destination `T` → r's next-hop exits
    /// toward T. Convenience accessor for the stepper.
    pub fn exits_from(&self, r: RegionId) -> std::collections::HashMap<RegionId, Vec<ExitLink>> {
        self.to
            .iter()
            .filter_map(|(t, field)| field.from.get(&r).map(|hop| (*t, hop.exits.clone())))
            .collect()
    }
}

/// P-?: one Dijkstra-at-T tree: every source's answer for T.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteField {
    pub from: std::collections::HashMap<RegionId, RouteHop>,
}

/// P-?: r's answer for "how do I get toward T?" — the cost-sorted next-hop
/// exits. Each [`ExitLink`] carries its own remaining route cost; there is no
/// separate summary cost to keep in sync.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteHop {
    pub exits: Vec<ExitLink>,
}

/// Layer-1 routing answer: "leave this region through `link`, then the worker
/// sends the token to `to_region`." This is border-only; it does not know which
/// local road cell the token can stand on.
///
/// ```text
/// ExitLink = which border to use + next region + remaining route cost
/// ```
///
/// `link` is the local-side BorderLinkId; the receiving region has the matching
/// link on its side. `cost` is the remaining Layer-1 distance from this exit
/// onward to the final target (crossing cost + border-node distance to the
/// destination's nearest border); the stepper adds its local distance to the
/// concrete road cell when ranking exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExitLink {
    pub link: BorderLinkId,
    pub to_region: RegionId,
    pub cost: u32,
}

/// Layer-2 movement-ready exit: the Layer-1 [`ExitLink`] plus the concrete local
/// road `cell` the token walks to.
///
/// ```text
/// RouteExit = ExitLink + the concrete local road cell to walk to
/// ```
///
/// `link` and `to_region` are carried through to the handoff, so the old
/// direct-neighbour hint is not needed to re-derive routing at the border.
/// `cost` mirrors `ExitLink.cost` and is the per-exit Layer-1 distance to the
/// final target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteExit {
    pub cell: Entity,
    pub link: BorderLinkId,
    pub to_region: RegionId,
    pub cost: u32,
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
}

#[derive(Debug)]
/// Paused tick state after the job phase and before the goods phase.
pub(crate) struct RegionalTickGoodsPhase {
    phase: RegionalTickJobPhase,
    pub goods_demands: Vec<PendingGoodsDemand>,
}

impl RegionalTickJobPhase {
    /// Whether this tick crosses a daily boundary (when jobs/economy resolve).
    pub(crate) fn is_daily(&self) -> bool {
        self.phase.is_daily()
    }

    /// Whether the daily employment reconciliation actually ran (computed fresh
    /// after population growth — see `continue_to_job_phase`).
    pub(crate) fn jobs_dirty(&self) -> bool {
        self.phase.jobs_dirty()
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
    /// P6: durable employer-side truth — the contracts this region holds at its
    /// own workplaces, as a flat `(workplace, holder, contract)` list. A flat list
    /// (not the nested `contracts_by_workplace` map) because a `CitizenRef` map key
    /// is not JSON-serializable. `#[serde(default)]` migrates pre-P6 saves (no
    /// field) to an empty list: a loaded region with no contracts, reconciled on
    /// load.
    #[serde(default)]
    employer_contracts: Vec<(Entity, CitizenRef, EmploymentContract)>,
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
    /// Directory employment ledger plan, P3: this region's employer-side
    /// truth — which citizens hold a contract at which of *its* workplaces.
    /// The directory's `accepted_by_*` maps are only a read cache; this is
    /// the authority for whether a seat is really reserved.
    ///
    /// Not serialized: `RegionState` itself is not `Serialize` (only `World`
    /// is), so contracts are transient until P6 makes them durable.
    employer_state: EmploymentEmployerState,
}

impl RegionState {
    /// Creates a region with its own private ECS world and empty import cache.
    pub fn new(id: RegionId, width: usize, height: usize) -> Self {
        let mut world = World::new(width, height);
        world.set_region_id(id);
        Self {
            id,
            world,
            employer_state: EmploymentEmployerState::default(),
        }
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
        // Single-region path: no cross-region reconcile gate exists here, so
        // there's nothing to skip -- always run the local job rematch.
        let job_phase = continue_to_job_phase(&mut self.world, self.id, phase, true);
        finish_tick_after_job_phase(&mut self.world, job_phase, &[])
    }

    /// P7c: advances movement by one 10-minute sub-tick (no economy). Driven 6×
    /// per game hour by the runner, separately from `tick_local`/the hourly tick.
    /// Buffers any cross-region crossings into `outgoing_handoffs` for the regions
    /// layer to drain (`drain_traveler_handoffs`).
    pub(crate) fn step_travel(&mut self) {
        travel::step_tokens(&mut self.world);
    }

    /// Drains work-arrival facts produced by the core movement step. The runtime
    /// owns coordinator routing; this state layer owns the World buffer.
    pub(crate) fn drain_destination_arrivals(&mut self) -> Vec<PendingDestinationArrival> {
        std::mem::take(&mut self.world.outgoing_destination_arrivals)
    }

    /// Records attendance only when the home-owned current work trip reached the
    /// citizen's still-assigned workplace.
    pub(crate) fn apply_destination_arrived(
        &mut self,
        traveler: TravelerId,
        destination: PlaceRef,
    ) {
        let Some(citizen) = self.world.citizens.get_mut(&traveler.citizen) else {
            return;
        };
        let Some(assignment) = citizen.workplace_assignment else {
            return;
        };
        if citizen.arrival_action != CitizenArrivalAction::StartWorkShift
            || citizen.work_trip_generation != traveler.generation
            || assignment.workplace != destination.building
        {
            return;
        }

        citizen.attended_since_daily_settlement = true;
        citizen.arrival_action = CitizenArrivalAction::ReturnHome;
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

    /// P6: force the derived pass (which includes `assign_local_jobs`) to re-run
    /// after a rebuild correction. A `release_contract`/`clear_employment` only
    /// invalidates the jobs *cache*, not the derived-dirty flag, so a freed local
    /// seat would otherwise be published to remote claimants before a local
    /// unemployed citizen gets its normal first chance at it.
    pub(crate) fn resettle_derived_state(&mut self) {
        self.world.mark_derived_dirty();
        self.ensure_derived_state();
    }

    pub(crate) fn is_road_topology_dirty(&self) -> bool {
        self.world.is_road_topology_dirty()
    }

    pub(crate) fn clear_road_topology_dirty(&self) {
        self.world.clear_road_topology_dirty();
    }

    /// Event-driven plan, P-1: whether this region's availability hints are stale.
    pub(crate) fn is_hints_dirty(&self) -> bool {
        self.world.is_hints_dirty()
    }

    pub(crate) fn clear_hints_dirty(&self) {
        self.world.clear_hints_dirty();
    }

    /// Event-driven plan, P-2: whether this region's power export demand/
    /// capacity may have changed since its last reconcile.
    pub(crate) fn is_power_exports_dirty(&self) -> bool {
        self.world.is_power_exports_dirty()
    }

    pub(crate) fn clear_power_exports_dirty(&self) {
        self.world.clear_power_exports_dirty();
    }

    pub(crate) fn clear_jobs_exports_dirty(&self) {
        self.world.clear_jobs_exports_dirty();
    }

    /// Event-driven plan, P-5: whether this region's goods export demand/
    /// capacity may have changed since its last reconcile.
    pub(crate) fn is_goods_exports_dirty(&self) -> bool {
        self.world.is_goods_exports_dirty()
    }

    pub(crate) fn clear_goods_exports_dirty(&self) {
        self.world.clear_goods_exports_dirty();
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

    /// Enter-panel detail for the travelers on the road cell at `(x, y)`: local
    /// citizen rows plus visitor endpoint summaries. Local-only — no cross-region
    /// query, unlike `remote_workers_for`.
    pub fn road_traveler_panel_seed(&self, x: usize, y: usize) -> RoadTravelerPanelSeedView {
        road_traveler_panel_seed(&self.world, x, y)
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

    /// P-a: per-region INPUT to the directory's Layer-1 Dijkstra. The region
    /// prices its own crossings (one Layer-2 Dijkstra per border-link pair) and
    /// publishes this report alongside the existing availability hint. The
    /// directory assembles all reports and runs the small Layer-1 Dijkstra on
    /// the region road graph.
    ///
    /// `border_neighbours` is the worker-supplied `BorderLinkId → neighbour
    /// RegionId` map used only to label this region's border openings for the
    /// report. It is not a travel-routing source; travel exits come from
    /// `RegionRoutes`. The region prices its own road graph by measuring the
    /// Layer-2 distance from each border entry to each border exit on the same
    /// local network.
    pub fn road_report(
        &self,
        border_neighbours: &HashMap<BorderLinkId, RegionId>,
    ) -> RegionRoadReport {
        let mut border_links: Vec<RegionBorderLink> = border_neighbours
            .iter()
            .map(|(link, neighbour)| RegionBorderLink {
                link: *link,
                neighbour: *neighbour,
            })
            .collect();
        border_links.sort_by_key(|bl| (bl.link.edge, bl.link.offset, bl.neighbour));

        // Per network: collect (cell, link) pairs that are border-road cells
        // (i.e. the cell has at least one border link). Each border link is
        // unique to a network (it's a map edge on that network's border).
        let width = self.world.grid.width();
        let height = self.world.grid.height();
        let mut crossing_costs: Vec<RegionCrossCost> = Vec::new();

        for network in road_connectivity::discover_road_networks(&self.world) {
            let network_id = RegionRoadNetworkId {
                region: self.id,
                road_network: network.id,
            };
            // Border cells on THIS network: road cells with at least one
            // border link.
            let mut border_cells: Vec<(Entity, BorderLinkId)> = Vec::new();
            for &road in &network.roads {
                if let Some(position) = self.world.positions.get(&road) {
                    for link in
                        border_links_for_cell(network_id, position.x, position.y, width, height)
                    {
                        border_cells.push((road, link.link));
                    }
                }
            }
            // Price every entry → every exit (no self-loops; dedup by pair).
            for &(entry_cell, entry_link) in &border_cells {
                for &(exit_cell, exit_link) in &border_cells {
                    if entry_link == exit_link {
                        // A border link is a single point — staying on it is
                        // a no-op (the mover never needs to "enter" and "exit"
                        // through the same link). Skip the 0-cost self-pair;
                        // it's never a useful crossing on the region graph.
                        continue;
                    }
                    if let Some(cost) = self.world.road_distance_to(exit_cell, entry_cell, &network)
                    {
                        // `cost` is the Layer-2 road distance from exit back
                        // to entry (= the came_from-tree hops from entry to
                        // exit, by tree symmetry).
                        if cost > 0 {
                            crossing_costs.push(RegionCrossCost {
                                entry: entry_link,
                                exit: exit_link,
                                cost,
                            });
                        }
                    }
                }
            }
        }
        crossing_costs.sort_by_key(|c| (c.entry, c.exit));

        RegionRoadReport {
            region: self.id,
            border_links,
            crossing_costs,
        }
    }

    /// P-c: rebuild `remote_exit_cells` (FINAL target region → local route
    /// exits) from `RegionRoutes::exits_from(self.id)`. The map key is final
    /// target T; each `ExitLink` says which local border link to use and which
    /// neighbour receives the token. The route may be 1 hop or N hops; either
    /// way the first hop starts at a local border, so we resolve each
    /// `BorderLinkId` to concrete local road cells.
    ///
    /// ```text
    /// Layer-1 route answer for source A:
    ///
    ///   final target C -> ExitLink { link: East/0, to_region: B }
    ///
    /// Local road graph in A:
    ///
    ///   BorderLinkId East/0 -> [road cell r42]
    ///
    /// Stored mover answer:
    ///
    ///   remote_exit_cells[C] = [
    ///     RouteExit { cell: r42, link: East/0, to_region: B }
    ///   ]
    /// ```
    ///
    /// `RouteExit` deliberately carries all three pieces:
    ///
    /// ```text
    /// cell      = Layer-2 local movement target ("walk to this road cell")
    /// link      = sender-side border link carried in the handoff
    /// to_region = immediate next-hop region for worker routing
    /// ```
    ///
    /// This keeps Layer-1's chosen route intact through crossing; the drain path
    /// no longer re-derives a neighbour from a separate direct-border map.
    pub(crate) fn set_region_routes(
        &mut self,
        exits_from: &std::collections::HashMap<RegionId, Vec<ExitLink>>,
    ) {
        // Build a quick index from BorderLinkId → local cells.
        let mut cells_by_link: HashMap<BorderLinkId, Vec<Entity>> = HashMap::new();
        for (cell, link) in self.border_road_links() {
            cells_by_link.entry(link).or_default().push(cell);
        }
        let mut map: HashMap<RegionId, Vec<RouteExit>> = HashMap::new();
        for (target, exits) in exits_from {
            for exit in exits {
                if let Some(cells) = cells_by_link.get(&exit.link) {
                    map.entry(*target)
                        .or_default()
                        .extend(cells.iter().copied().map(|cell| RouteExit {
                            cell,
                            link: exit.link,
                            to_region: exit.to_region,
                            cost: exit.cost,
                        }));
                }
            }
        }
        for exits in map.values_mut() {
            exits.sort();
            exits.dedup();
        }
        self.world.remote_exit_cells = map;
    }

    /// P5b: drain this tick's buffered crossings into routed handoffs. A `Move`
    /// whose carried exit link no longer resolves to its exit cell is rolled back
    /// home (never strands the citizen).
    /// `Rollback` handoffs are emitted by a neighbour that could not place an
    /// inbound token — we route them back to the home region.
    pub(crate) fn drain_traveler_handoffs(&mut self) -> Vec<TravelerHandoff> {
        let pending = std::mem::take(&mut self.world.outgoing_handoffs);
        let mut handoffs = Vec::new();
        for handoff in pending {
            match handoff {
                PendingHandoff::Move {
                    traveler,
                    token,
                    to_region,
                    exit_cell,
                    exit_link,
                } => {
                    if self.cell_at_border_link(exit_link) == Some(exit_cell) {
                        handoffs.push(TravelerHandoff {
                            token,
                            traveler,
                            to_region,
                            entry_link: Some(exit_link),
                            kind: HandoffKind::Move,
                        });
                    } else {
                        // The selected exit link no longer resolves to this cell —
                        // the outbound can't route. Two cases:
                        //   - Home-side: the home just lost its border link (or
                        //     the route went stale). Apply `apply_traveler_return`
                        //     locally — it clears `away_residents` (the citizen
                        //     wasn't really away).
                        //   - Host-side: a foreign visitor's exit became
                        //     unroutable. B has no home record, so a local
                        //     `apply_traveler_return` would no-op. Emit a `Rollback`
                        //     to the home region so it can clear `away_residents`
                        //     there.
                        if token.home.region == self.id {
                            travel::apply_traveler_return(&mut self.world, traveler);
                        } else {
                            handoffs.push(TravelerHandoff {
                                token: token.clone(),
                                traveler,
                                to_region: token.home.region,
                                entry_link: None,
                                kind: HandoffKind::Rollback,
                            });
                        }
                    }
                }
                PendingHandoff::Rollback {
                    traveler,
                    to_region,
                } => {
                    // The home's own entry vanished (self-bounce) or a neighbour
                    // bounced a foreign citizen home. Pass it through to the home
                    // region; no `entry_link` is needed (the home applies
                    // `apply_traveler_return` directly).
                    handoffs.push(TravelerHandoff {
                        token: TravelToken {
                            state: TravelState::default(),
                            home: crate::core::components::PlaceRef {
                                region: to_region,
                                building: crate::core::entity::Entity::default(),
                            },
                            work: None,
                            trip_gen: traveler.generation,
                        },
                        traveler,
                        to_region,
                        entry_link: None,
                        kind: HandoffKind::Rollback,
                    });
                }
            }
        }
        handoffs
    }

    /// P5b: apply an inbound crossing. `Move` at a host (foreign home) → place
    /// the token at the entry cell; `Move` completing at home or `Rollback` →
    /// apply the home guard. Returns any bounce handoffs (entry-cell vanished
    /// or self-bounce) for the worker to route onward.
    pub(crate) fn receive_traveler_handoff(
        &mut self,
        handoff: TravelerHandoff,
    ) -> Vec<TravelerHandoff> {
        match handoff.kind {
            HandoffKind::Move => {
                // If this is the home region, gate on the four-part guard.
                if handoff.token.home.region == self.id
                    && !travel::home_accepts(
                        &self.world,
                        handoff.traveler.citizen,
                        handoff.traveler.generation,
                    )
                {
                    // Stale or duplicate — drop silently.
                    return Vec::new();
                }
                let Some(entry_link) = handoff.entry_link else {
                    // A Move with no entry_link is malformed; bounce home.
                    return self.bounce_to_home(&handoff);
                };
                let local_link = entry_link.matching_neighbor_link();
                match self.cell_at_border_link(local_link) {
                    Some(entry_cell) => {
                        travel::receive_traveler(
                            &mut self.world,
                            handoff.traveler,
                            handoff.token,
                            entry_cell,
                        );
                        Vec::new()
                    }
                    None => {
                        // Entry road gone (stale route snapshot) — bounce a Rollback
                        // home. If THIS is the home region (its own entry vanished),
                        // it self-bounces: next sub-tick `apply_traveler_return`
                        // clears `away_residents`, so the abandoned trip is
                        // re-departable. (Never drop the traveller.)
                        self.bounce_to_home(&handoff)
                    }
                }
            }
            HandoffKind::Rollback => {
                travel::apply_traveler_return(&mut self.world, handoff.traveler);
                Vec::new()
            }
        }
    }

    /// Build a `PendingHandoff::Rollback` handoff to push to `outgoing_handoffs`
    /// (the worker's drain routes it back to the home).
    fn bounce_to_home(&self, handoff: &TravelerHandoff) -> Vec<TravelerHandoff> {
        let to_region = handoff.token.home.region;
        vec![TravelerHandoff {
            token: handoff.token.clone(),
            traveler: handoff.traveler,
            to_region,
            entry_link: None,
            kind: HandoffKind::Rollback,
        }]
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
        // Event-driven plan, P-3: capture before explicitly clearing. Diff-
        // apply `power::run` keeps an existing `Imported` source on its own
        // when no fresh local grant covers a consumer, so a dirty reconcile
        // — which is about to release every producer reservation and request
        // only what this tick's demand scan finds — must clear the captured
        // imports itself first (`clear_imported_power`), or an import-needing
        // consumer would still read as `powered` and never make it into the
        // fresh batch (the round-1 desync from the starvation fix,
        // reintroduced; see that clear/collect ordering note there).
        //
        // Restore happens AFTER `pending_power_demands()` below, not before:
        // demand collection must see the true, freshly cleared state so an
        // imported consumer is correctly re-included in this tick's fresh
        // request batch. The restore only protects reads that happen later
        // in this same pass — e.g. this region answering another region's
        // incoming power export request before its own fresh grant has
        // round-tripped back — see
        // docs/20260703-bug-cross-region-export-starvation-fix.md.
        let imported = imported_power_grants(&self.world);
        clear_imported_power(&mut self.world, &imported);
        let phase = begin_tick_power_phase(&mut self.world, self.id);
        let power_demands = self.pending_power_demands();
        // Only restore a captured import for a consumer that actually made it
        // into this tick's fresh demand batch. `pending_power_demands` skips
        // a consumer with no border-connected road network to request through
        // (e.g. a bulldozed connecting road, or the whole region losing its
        // last border link) — no fresh request means no reply will ever
        // arrive to confirm or deny it, so `apply_power_export_grant`'s
        // denial-cleanup never runs. Restoring it anyway would leave it
        // optimistically "powered" forever with no producer reservation
        // behind it, even though the old one was already unconditionally
        // released above (`reconcile_power_export_allocations` releases every
        // tick regardless of whether a replacement request follows).
        let requestable: std::collections::HashSet<Entity> =
            power_demands.iter().map(|demand| demand.consumer).collect();
        let restorable_imports = imported
            .into_iter()
            .filter(|(entity, _, _)| requestable.contains(entity))
            .collect::<Vec<_>>();
        reapply_imported_power(&mut self.world, &restorable_imports);
        RegionalTickPowerPhase {
            phase,
            power_demands,
        }
    }

    /// Quiet-tick variant of `begin_tick_power_demand_phase` (event-driven
    /// plan, P-2): used when the reconcile gate finds no local or
    /// cross-region change since the last reconcile, so this tick will
    /// neither release nor request anything. Skips the demand scan entirely.
    /// Event-driven plan, P-6: uses `begin_tick_power_phase_quiet`
    /// (simulation.rs), which skips `power::run` itself rather than relying
    /// on P-3's diff-apply making it a cheap no-op — the gate already
    /// guarantees nothing that could affect power changed, so every
    /// consumer's grant is already exactly what a fresh recompute would
    /// produce; no capture/restore is needed here at all.
    pub(crate) fn begin_tick_power_phase_quiet(&mut self) -> RegionalTickPowerPhase {
        let phase = begin_tick_power_phase_quiet(&mut self.world, self.id);
        RegionalTickPowerPhase {
            phase,
            power_demands: Vec::new(),
        }
    }

    pub(crate) fn power_import_settlement_demands(&mut self) -> Vec<PendingPowerDemand> {
        // Load-time settlement is time-neutral: re-run only local power to clear
        // transient imported flags, then let the normal producer-owned export
        // request/grant flow reapply imports.
        power::run(&mut self.world);
        self.pending_power_demands()
    }

    /// Retire-tickstate, P-b: time-neutral demand collection for the eager
    /// nudge (`RegionEvent::PowerCapacityRecheck`) — must NOT advance the
    /// clock, unlike `begin_tick_power_demand_phase`. Mirrors that
    /// function's capture/clear/restore dance for the identical reason:
    /// `release_and_request_power` is about to release every producer
    /// reservation and request only what this scan finds, so an
    /// already-imported consumer must be re-included in the scan — diff-
    /// apply `power::run` otherwise leaves it looking powered and skips it
    /// (the starvation-fix round-1 desync, reintroduced if skipped).
    pub(crate) fn power_demand_recheck(&mut self) -> Vec<PendingPowerDemand> {
        // Catch up any pending config change; no time advance.
        ensure_derived_state(&mut self.world, self.id);
        let imported = imported_power_grants(&self.world);
        clear_imported_power(&mut self.world, &imported);
        power::run(&mut self.world); // NOT begin_tick_power_phase — no advance_hours
        let power_demands = self.pending_power_demands();
        let requestable: std::collections::HashSet<Entity> =
            power_demands.iter().map(|demand| demand.consumer).collect();
        let restorable = imported
            .into_iter()
            .filter(|(entity, _, _)| requestable.contains(entity))
            .collect::<Vec<_>>();
        reapply_imported_power(&mut self.world, &restorable);
        power_demands
    }

    /// Advances from the resolved power phase into the local job assignment phase.
    ///
    /// Runs the post-power systems and (on a daily boundary, when jobs-dirty)
    /// local job assignment, then collects the job seekers that found no
    /// reachable local slot so the runtime can request remote workplace slots
    /// before the economy settles.
    ///
    /// Retire-tickstate, P-c: `discovery_dirty` is only the caller's own
    /// reconcile-gate half (discovery generation moved); the full jobs-dirty
    /// decision is made inside `continue_to_job_phase`, AFTER population
    /// growth, and read back via `RegionalTickJobPhase::jobs_dirty()` — see
    /// that function's doc comment for why (a citizen born this same tick
    /// must not wait a full extra day for its first job attempt).
    pub(crate) fn continue_tick_to_job_demand_phase(
        &mut self,
        power_phase: RegionalTickPowerPhase,
        connectivity_dirty: bool,
    ) -> RegionalTickJobPhase {
        let phase = continue_to_job_phase(
            &mut self.world,
            self.id,
            power_phase.phase,
            connectivity_dirty,
        );
        // The ledger (`daily_employment_phase`) owns cross-region employment now;
        // this phase only carries the local job/economy boundary.
        RegionalTickJobPhase { phase }
    }

    /// Finishes the tick after the job phase resolves.
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
            // This consumer was included in this tick's fresh demand batch, so
            // begin_tick_power_demand_phase's optimistic restore (see its
            // comment) is the only thing that could have left it marked
            // powered via an Imported source — the old reservation it
            // protected was already released by release_and_request_power
            // before this request was sent. A denied replacement must undo
            // that optimism, or the consumer would read as powered forever
            // with no reservation backing it anywhere.
            //
            // Retire-tickstate, P-a: only undo an Imported source here, never
            // Local. Ticks no longer pause between request and reply, so a
            // later tick (or the eager nudge) can legitimately power this
            // same consumer locally before this now-late denial arrives;
            // clearing unconditionally would wipe real local power and
            // subtract it from the supplied stat.
            if let Some(consumer) = self.world.power_consumers.get_mut(&demand.consumer)
                && consumer.powered
                && matches!(consumer.source, Some(PowerSource::Imported { .. }))
            {
                consumer.powered = false;
                consumer.source = None;
                self.world.stats.power.total_power_supplied -= demand.demand;
                self.world.stats.power.total_power_shortage =
                    (self.world.stats.power.total_power_demand
                        - self.world.stats.power.total_power_supplied)
                        .max(0);
            }
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

    pub(crate) fn add_commercial_goods(&mut self, commercial: Entity, units: u32) {
        economy::add_commercial_goods(&mut self.world, commercial, units as i32);
        // Event-driven plan, P-1/P-5: this cross-region delivery path writes
        // `local_goods_stored` directly and bypasses every invalidate_*/mark_*
        // chokepoint, so it needs its own explicit marks — both hints (so
        // remote shoppers see updated availability) and this region's own
        // goods export dirty flag (a delivered shipment changes this
        // building's future free capacity).
        self.world.mark_hints_dirty();
        self.world.mark_goods_exports_dirty();
    }

    /// Returns spare local workplace slot entities reachable from one road network.
    ///
    /// These are the slots in `remaining_workplaces` whose building connects to
    /// `network` — i.e. left over after both local job resolution **and** (P7-a)
    /// employer-contract reservation. The producer exports from this set; jobs are
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

    /// Directory employment ledger plan, P2: this employer region's current
    /// derived job pools, ready to publish. One `JobPool` row per workplace
    /// with open local capacity, `open_count` aggregated from the same
    /// effective-workplace slots the old export path already reads
    /// (`spare_job_slots_on_network`) — mirrors that path into the new pool
    /// shape, does not replace it. `generation` is left at `0`; the
    /// directory stamps it on publish (`same_pool_facts` never compares it).
    ///
    /// A workplace adjacent to two disconnected networks (a bridge) is
    /// published under its first (lowest-id) network only: `JobPool` names
    /// exactly one network, so listing the same pool under two would let
    /// both components see it as independently claimable.
    ///
    /// P3: `open_count` is *claimable* capacity, so seats this region has
    /// already contracted to remote citizens are subtracted. The directory
    /// decrements its own cached `open_count` on an accepted claim only "until
    /// next employer publish" — this republished count is that authoritative
    /// replacement, and must not resurrect a contracted seat. A workplace with
    /// nothing left to claim publishes no row at all.
    #[allow(dead_code)] // P2: staged; the daily tick starts publishing in P7.
    pub(crate) fn published_job_pools(&self) -> Vec<JobPool> {
        let mut seen = HashSet::new();
        let mut pools = Vec::new();

        for capacity in self.world.cached_power_resolution().network_capacities {
            let network = RegionRoadNetworkId {
                region: self.id,
                road_network: capacity.road_network,
            };

            let mut open_counts: BTreeMap<Entity, u16> = BTreeMap::new();
            for slot in self.spare_job_slots_on_network(network) {
                if seen.contains(&slot) {
                    continue;
                }
                *open_counts.entry(slot).or_insert(0) += 1;
            }

            for (workplace, open_count) in open_counts {
                seen.insert(workplace);
                // P7-a: `spare_job_slots_on_network` reads `remaining_workplaces`,
                // which the registry already netted of reserved seats — so
                // `open_count` is the claimable count as-is (and always >= 1,
                // since a fully-reserved workplace contributes no slots to count).
                // Subtracting `contracted_seats_at` here too would double-count.
                pools.push(JobPool {
                    region: self.id,
                    workplace,
                    open_count,
                    network,
                    salary: self.workplace_salary(workplace),
                    generation: 0,
                });
            }
        }

        pools
    }

    /// P3, home side: this region's citizens with no workplace assignment,
    /// in deterministic entity order. `world.citizens` is a `HashMap`, so the
    /// sort is load-bearing — same pattern as `pending_job_demands`.
    pub(crate) fn unemployed_citizens(&self) -> Vec<Entity> {
        let mut citizens = self.world.citizens.keys().copied().collect::<Vec<_>>();
        citizens.sort_by_key(|citizen| citizen.0);
        citizens.retain(|citizen| {
            self.world
                .citizens
                .get(citizen)
                .is_some_and(|data| data.workplace_assignment.is_none())
        });
        citizens
    }

    /// P7-d: does any citizen still want work? Half of the daily-employment
    /// gate. A loss clears an assignment without re-flagging `jobs_exports_dirty`
    /// (P-c's `refresh_jobs_cache_after_grant_applied`), so without this a
    /// laid-off citizen would never retry on an otherwise-quiet day.
    #[allow(dead_code)] // P7-d: staged until the daily phase is tick-wired below.
    pub(crate) fn has_unassigned_citizen(&self) -> bool {
        self.world
            .citizens
            .values()
            .any(|data| data.workplace_assignment.is_none())
    }

    /// This region's workplace entities reserved for remote contract holders, one
    /// entry per contract, in `(workplace, citizen)` order. Feeds the daily
    /// economy's producer-owned workplace tax (`finish_goods_phase`): a workplace
    /// with N contracts contributes N entries, one taxed slot each.
    pub(crate) fn contracted_workplace_tax_slots(&self) -> Vec<Entity> {
        let mut slots = Vec::new();
        for (workplace, holders) in &self.employer_state.contracts_by_workplace {
            for _ in 0..holders.len() {
                slots.push(*workplace);
            }
        }
        slots
    }

    /// P6: this region's employer-side truth, flattened for the load-time
    /// directory rebuild — every `(workplace, holder, contract)` it holds, in
    /// deterministic `(workplace, citizen)` order (both maps are `BTreeMap`s).
    pub(crate) fn employer_contracts(&self) -> Vec<(Entity, CitizenRef, EmploymentContract)> {
        let mut contracts = Vec::new();
        for (workplace, holders) in &self.employer_state.contracts_by_workplace {
            for (citizen, contract) in holders {
                contracts.push((*workplace, *citizen, *contract));
            }
        }
        contracts
    }

    /// P6: this region's home-side truth for the load-time rebuild — its citizens'
    /// applied **cross-region** assignments, in deterministic entity order. Local
    /// assignments are deliberately excluded: they carry no `EmploymentContract`
    /// and are not directory-coordinated, so the reconciliation (which clears an
    /// assignment that has no matching contract) must never see them.
    pub(crate) fn home_assignments(&self) -> Vec<(Entity, WorkplaceAssignment)> {
        let mut assignments = self
            .world
            .citizens
            .iter()
            .filter_map(|(citizen, data)| {
                let assignment = data.workplace_assignment?;
                (assignment.workplace.region() != self.id).then_some((*citizen, assignment))
            })
            .collect::<Vec<_>>();
        assignments.sort_by_key(|(citizen, _)| citizen.0);
        assignments
    }

    /// P3, employer side: does this region still have a free seat at
    /// `workplace` that it could contract out?
    ///
    /// The plan's protocol step 4 defines the whole check as *"pool still
    /// exists and has employer-owned capacity"* — so this is deliberately
    /// **not** given the claim's `generation` or the claimant's home region,
    /// which the plan's `job_pool_still_has_open_capacity` signature names but
    /// never uses. Generation was already validated by the directory at submit
    /// time (and is recorded onto the contract as `accepted_generation`);
    /// reachability was already decided by `choose_best_pool`, and an employer
    /// cannot re-check it anyway — it owns no topology.
    ///
    /// "Employer-owned capacity" is the spare seats this workplace publishes
    /// (`spare_job_slots_for_workplace`, from `remaining_workplaces`).
    ///
    /// P7-a: `remaining_workplaces` is already net of *both* local assignment
    /// and reserved contract seats, so a free seat is simply `remaining > 0` —
    /// no separate `contracted_seats_at` subtraction (that would double-count).
    /// The claim under validation is still *pending*, not yet a contract, so it
    /// is not among the reserved seats; `> 0` correctly asks "is there a seat
    /// not already taken locally or by an existing contract?".
    pub(crate) fn job_pool_still_has_open_capacity(&self, workplace: Entity) -> bool {
        self.spare_job_slots_for_workplace(workplace) > 0
    }

    /// Claimable seats at one workplace: entries in `remaining_workplaces`,
    /// which (P7-a) excludes both locally-assigned and employer-contracted
    /// seats. Counts repeated entries — one per open seat.
    fn spare_job_slots_for_workplace(&self, workplace: Entity) -> usize {
        self.world
            .with_cached_remaining_job_workplaces(|remaining_workplaces| {
                remaining_workplaces
                    .iter()
                    .filter(|slot| **slot == workplace)
                    .count()
            })
    }

    /// P4, home side: write the durable `Citizen.workplace_assignment` the
    /// economy already pays from. Returns whether this call actually applied
    /// it — the caller reports only *newly* applied citizens back to the
    /// directory.
    ///
    /// Never overwrites an existing assignment. That single guard gives P4
    /// both of its forbidden behaviours at once:
    /// - *idempotent repeated wakes*: the directory's accepted read cache keeps
    ///   re-offering an already-applied citizen on every wake; the second call
    ///   is a no-op returning `false`.
    /// - *"do not clear an old assignment while merely checking for replacement
    ///   work"*: a citizen who picked up a local job between claim and apply
    ///   keeps it.
    ///
    /// `refresh_jobs_cache_after_grant_applied`, not `invalidate_jobs_registry`
    /// — re-flagging `jobs_exports_dirty` here would re-open the daily employment
    /// gate every day even for a settled worker, churning the assignment just
    /// applied instead of leaving a quiet day quiet.
    pub(crate) fn apply_workplace_assignment(
        &mut self,
        citizen: Entity,
        assignment: WorkplaceAssignment,
    ) -> bool {
        let Some(citizen_data) = self.world.citizens.get_mut(&citizen) else {
            return false; // citizen moved away or was removed
        };
        if citizen_data.workplace_assignment.is_some() {
            return false;
        }
        citizen_data.workplace_assignment = Some(assignment);
        self.world.refresh_jobs_cache_after_grant_applied();
        true
    }

    /// Does this citizen already hold an assignment naming `workplace`?
    ///
    /// P7-d: `apply_workplace_assignment` answers `false` both for the idempotent
    /// re-offer of an already-applied lease *and* for a stale accepted lease the
    /// home can no longer take (the citizen grabbed a local job or left). Only the
    /// latter is a phantom contract to decline, so the home tells them apart with
    /// this check before requesting a release.
    pub(crate) fn citizen_holds_workplace(&self, citizen: Entity, workplace: Entity) -> bool {
        self.world
            .citizens
            .get(&citizen)
            .and_then(|data| data.workplace_assignment)
            .is_some_and(|assignment| assignment.workplace == workplace)
    }

    /// P3, employer side: record the contract in this region's own state and
    /// hand back the assignment the home region will later apply (P4).
    ///
    /// The employer is the authority for the seat; the returned
    /// `WorkplaceAssignment` is owned data (city-wide `Entity` + self-describing
    /// cell + salary), so the home never dereferences this region's ECS.
    pub(crate) fn accept_claim_and_create_assignment(
        &mut self,
        claim: &JobClaim,
    ) -> WorkplaceAssignment {
        let workplace = claim.workplace;
        let salary = self.workplace_salary(workplace);
        self.employer_state
            .contracts_by_workplace
            .entry(workplace)
            .or_default()
            .insert(
                claim.citizen,
                EmploymentContract {
                    salary,
                    accepted_generation: claim.generation,
                },
            );
        self.sync_job_reservations();

        let position = self
            .workplace_position(workplace)
            .unwrap_or(Position { x: 0, y: 0 });
        WorkplaceAssignment {
            workplace,
            location: CityCellRef::local(self.id, position.x, position.y),
            salary,
        }
    }

    /// P5, home side: give up this citizen's job, returning the assignment it
    /// held so the caller can name the lease to release. `None` if it had none.
    ///
    /// The home clears its own truth *first*; the employer's seat is only freed
    /// once it confirms (`release_contract_if_matches` → `confirm_release`).
    /// Until then the directory still lists the citizen as accepted, which is
    /// what stops it claiming a second job mid-release.
    #[allow(dead_code)] // P5: staged; no gameplay action releases a job yet.
    pub(crate) fn clear_employment(&mut self, citizen: Entity) -> Option<WorkplaceAssignment> {
        let assignment = self
            .world
            .citizens
            .get_mut(&citizen)?
            .workplace_assignment
            .take()?;
        self.world.refresh_jobs_cache_after_grant_applied();
        Some(assignment)
    }

    /// P5, home side: clear this citizen's job only if it is still *the* job
    /// that was lost.
    ///
    /// P5 forbids clearing "a home assignment if the citizen already moved to a
    /// different workplace" — a loss report can be one pass stale, and the
    /// citizen may have been re-hired somewhere else in between.
    ///
    /// The plan also passes the workplace's region; that is redundant, since an
    /// `Entity` already packs its owning region, and comparing the workplace
    /// alone is strictly stronger.
    pub(crate) fn clear_employment_if_matches(
        &mut self,
        citizen: Entity,
        workplace: Entity,
    ) -> bool {
        let Some(citizen_data) = self.world.citizens.get_mut(&citizen) else {
            return false;
        };
        if citizen_data.workplace_assignment.map(|a| a.workplace) != Some(workplace) {
            return false;
        }
        citizen_data.workplace_assignment = None;
        self.world.refresh_jobs_cache_after_grant_applied();
        true
    }

    /// P5, employer side: drop the contract for exactly this citizen at exactly
    /// this workplace. Returns whether one was actually held — only then may the
    /// directory confirm the release and hand the seat back.
    pub(crate) fn release_contract_if_matches(
        &mut self,
        workplace: Entity,
        citizen: CitizenRef,
    ) -> bool {
        let Some(holders) = self
            .employer_state
            .contracts_by_workplace
            .get_mut(&workplace)
        else {
            return false;
        };
        if holders.remove(&citizen).is_none() {
            return false;
        }
        if holders.is_empty() {
            self.employer_state
                .contracts_by_workplace
                .remove(&workplace);
        }
        self.sync_job_reservations();
        true
    }

    /// P7-a: push the current per-workplace contract counts into the jobs
    /// registry's retained reservation input, so local matching (and every
    /// downstream `remaining_workplaces` consumer) holds those seats out. Called
    /// after any contract mutation; `World::set_job_reservations` no-ops and
    /// skips the cache invalidation when the set is unchanged.
    fn sync_job_reservations(&mut self) {
        let reservations = self
            .employer_state
            .contracts_by_workplace
            .iter()
            .map(|(workplace, holders)| (*workplace, holders.len() as u16))
            .collect();
        self.world.set_job_reservations(reservations);
    }

    /// P5, employer side: drop every contract this region can no longer honour,
    /// and hand them back so the caller can report each as an explicit
    /// `JobLoss`. Loss is never *inferred* — this is what makes it explicit.
    ///
    /// A workplace can honour `min(contract_count, physical seats)` contracts.
    /// Contracts beyond its physical seat count mean it was bulldozed, lost
    /// power or road access, or was downgraded — those are evicted.
    ///
    /// P7-a: the honourable count is read from the registry's
    /// `reserved_seats_by_workplace` (= `min(contracts, physical)`), NOT from
    /// `spare_job_slots_for_workplace`. `spare_job_slots_for_workplace` is now
    /// *remaining after reservation*, which already subtracted these very
    /// contracts — using it would be the circular double-subtraction the plan
    /// warns against. Syncing first guarantees the reservation reflects the
    /// current contracts, so `evict = contracts - reserved = max(0, contracts -
    /// physical)`.
    ///
    /// **Eviction policy** — the plan says only "the employer chooses which
    /// contracts are lost using deterministic local policy". This one keeps the
    /// longest-serving workers: sort by `(accepted_generation, citizen)` and
    /// evict from the end. Seniority, with a total order for determinism.
    #[allow(dead_code)] // P7-a: staged; the daily tick starts calling this in P7-d.
    pub(crate) fn release_contracts_over_current_capacity(
        &mut self,
    ) -> Vec<(Entity, CitizenRef, EmploymentContract)> {
        self.sync_job_reservations();
        let reserved = self
            .world
            .cached_job_resolution()
            .reserved_seats_by_workplace;
        let workplaces = self
            .employer_state
            .contracts_by_workplace
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut lost = Vec::new();

        for workplace in workplaces {
            let honourable = reserved.get(&workplace).copied().unwrap_or(0) as usize;
            let holders = &self.employer_state.contracts_by_workplace[&workplace];
            if holders.len() <= honourable {
                continue;
            }

            let mut by_seniority = holders
                .iter()
                .map(|(citizen, contract)| (*citizen, *contract))
                .collect::<Vec<_>>();
            by_seniority
                .sort_by_key(|(citizen, contract)| (contract.accepted_generation, *citizen));

            let evicted = by_seniority.len() - honourable;
            for (citizen, contract) in by_seniority.into_iter().rev().take(evicted) {
                lost.push((workplace, citizen, contract));
            }
        }

        lost.sort_by_key(|(workplace, citizen, _)| (*workplace, *citizen));
        for (workplace, citizen, _) in &lost {
            self.release_contract_if_matches(*workplace, *citizen);
        }
        lost
    }

    /// P7-c: the road networks this workplace touches, as region-tagged
    /// `RegionRoadNetworkId`s. A workplace touches a network iff one of its
    /// orthogonally-adjacent road tiles belongs to that network's component.
    /// Mirrors the reachability side of `spare_job_slots_on_network`.
    pub(crate) fn workplace_networks(&self, workplace: Entity) -> Vec<RegionRoadNetworkId> {
        road_connectivity::discover_road_networks(&self.world)
            .into_iter()
            .filter(|network| {
                road_connectivity::adjacent_road_entities(&self.world, workplace)
                    .any(|road| network.roads.contains(&road))
            })
            .map(|network| RegionRoadNetworkId {
                region: self.id,
                road_network: network.id,
            })
            .collect()
    }

    /// P7-c: does the home region still reach this workplace? True iff any of
    /// the workplace's current road networks sits in a discovery component that
    /// also contains a network in `home`. Uses live workplace networks, not the
    /// stale `JobPool` rows — see "Use current workplace networks" in the plan.
    ///
    /// A workplace with no road networks at all (bulldozed, or lost road access)
    /// is unreachable from everywhere, which is the correct answer.
    pub(crate) fn contract_route_is_reachable(
        &self,
        discovery: &CrossRegionDiscovery,
        workplace: Entity,
        home: RegionId,
    ) -> bool {
        self.workplace_networks(workplace).iter().any(|network| {
            discovery
                .component_of(*network)
                .is_some_and(|component| component.iter().any(|member| member.region == home))
        })
    }

    /// P7-c, employer side: drop every contract whose home region can no longer
    /// reach the workplace, and hand them back so the caller reports each as an
    /// explicit `JobLoss`. This is the employer-side route invalidation the
    /// plan requires: a neighbour's road change never dirties the employer
    /// locally, so the employer learns of a disconnection only by re-checking
    /// reachability against the current discovery snapshot.
    ///
    /// Deterministic: contracts are visited in `(workplace, citizen)` order.
    #[allow(dead_code)] // P7-c: staged; the daily employment phase calls it in P7-d.
    pub(crate) fn release_contracts_with_unreachable_homes(
        &mut self,
        discovery: &CrossRegionDiscovery,
    ) -> Vec<(Entity, CitizenRef, EmploymentContract)> {
        let mut lost = Vec::new();
        let workplaces = self
            .employer_state
            .contracts_by_workplace
            .keys()
            .copied()
            .collect::<Vec<_>>();

        for workplace in workplaces {
            let holders = self
                .employer_state
                .contracts_by_workplace
                .get(&workplace)
                .map(|holders| {
                    holders
                        .iter()
                        .map(|(citizen, contract)| (*citizen, *contract))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for (citizen, contract) in holders {
                if !self.contract_route_is_reachable(discovery, workplace, citizen.region) {
                    lost.push((workplace, citizen, contract));
                }
            }
        }

        lost.sort_by_key(|(workplace, citizen, _)| (*workplace, *citizen));
        for (workplace, citizen, _) in &lost {
            self.release_contract_if_matches(*workplace, *citizen);
        }
        lost
    }

    #[cfg(test)]
    pub(crate) fn contract_holders_at(&self, workplace: Entity) -> Vec<CitizenRef> {
        self.employer_state
            .contracts_by_workplace
            .get(&workplace)
            .map(|holders| holders.keys().copied().collect())
            .unwrap_or_default()
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
        let employer_contracts = self.employer_contracts();
        let mut world = self.world;
        scrub_transient_import_state_for_save(&mut world);
        RegionStateSaveRecord {
            id: self.id,
            world,
            employer_contracts,
        }
    }

    pub(crate) fn from_save_record(record: RegionStateSaveRecord) -> Self {
        let mut contracts_by_workplace: BTreeMap<Entity, BTreeMap<CitizenRef, EmploymentContract>> =
            BTreeMap::new();
        for (workplace, citizen, contract) in record.employer_contracts {
            // Drop any contract for a workplace this region does not own (a
            // hand-edited/corrupt save): keeping it would leave stale employer
            // truth that the rebuild filters out yet the region would re-save.
            if workplace.region() != record.id {
                continue;
            }
            contracts_by_workplace
                .entry(workplace)
                .or_default()
                .insert(citizen, contract);
        }
        let employer_state = EmploymentEmployerState {
            contracts_by_workplace,
            ..EmploymentEmployerState::default()
        };
        Self::from_world_with_employer_state(record.id, record.world, employer_state)
    }

    pub(crate) fn from_legacy_world_bytes(
        id: RegionId,
        bytes: &[u8],
    ) -> Result<Self, serde_json::Error> {
        let world = serde_json::from_slice(bytes)?;
        Ok(Self::from_world(id, world))
    }

    pub(crate) fn from_world(id: RegionId, world: World) -> Self {
        Self::from_world_with_employer_state(id, world, EmploymentEmployerState::default())
    }

    /// Load boundary shared by fresh-world and save-record construction.
    ///
    /// P6: employer contracts are restored *before* `refresh_derived_state_for_world`
    /// derives local jobs, because `assign_local_jobs` reads the jobs registry's
    /// retained reservation (P7-a). Sync the reservation from the restored
    /// contracts first, or local matching would seat a local citizen into a slot
    /// a remote contract already holds — double-booking the pool on load.
    fn from_world_with_employer_state(
        id: RegionId,
        mut world: World,
        employer_state: EmploymentEmployerState,
    ) -> Self {
        world.rebuild_entity_records();
        // Stamp the owning region onto the world (and rebuild each citizen's `id`
        // from its map key) before derived state reads it. Homes need no stamping:
        // the `home` Entity already packs its birth region.
        world.set_region_id(id);
        let mut state = Self {
            id,
            world,
            employer_state,
        };
        state.sync_job_reservations();
        refresh_derived_state_for_world(&mut state.world, id);
        // Event-driven plan, P-1: force hints_dirty true on load (runtime state,
        // including this flag, is never serialized) so the first worker pass
        // after load republishes this region's availability hints.
        state.world.mark_hints_dirty();
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
            // one message per unit because producer grants are
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
/// Power and goods export grants are runtime coordination, not durable world truth.
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
    // P6: `workplace_assignment` is durable home-side truth now, not derived
    // state, so it is NOT scrubbed — it is persisted and reconciled on load.
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
    use crate::core::components::{Citizen, Morale};
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

        assert!(region.apply_workplace_assignment(
            citizen,
            WorkplaceAssignment {
                workplace: Entity::new(RegionId(2), 42),
                location: CityCellRef::local(RegionId(2), 1, 0),
                salary: 4,
            },
        ));

        assert_eq!(region.imported_job_slots(), vec![(RegionId(2), 42)]);
        assert_eq!(region.world.cached_job_counts().unemployment, 0);
    }

    /// Regression for docs/20260703-bug-cross-region-export-starvation-fix.md.
    /// `begin_tick_power_demand_phase` must both (a) still collect a fresh
    /// demand for a consumer that already holds an imported grant — otherwise
    /// the caller's per-tick reconciliation (which unconditionally releases
    /// the old producer-side reservation every tick) desyncs from the
    /// producer's ledger — and (b) leave the consumer reading as powered
    /// immediately afterward, so another region asking this one for export
    /// eligibility before the fresh grant round-trips back doesn't see a
    /// false zero. An earlier version of this fix restored power *before*
    /// demand collection, which broke (a); caught in review.
    #[test]
    fn begin_tick_power_demand_phase_still_requests_fresh_demand_for_an_imported_consumer() {
        let mut region = RegionState::new(RegionId(1), 2, 1);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        let consumer = region.world.grid.get(0, 0).expect("residential entity");
        let demand = region.world.power_consumers[&consumer].demand;
        let caller_network = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };

        // Simulate: this consumer already holds an import grant from a
        // producer, as if a previous tick's request/grant round trip landed.
        region.apply_power_export_grant(
            PendingPowerDemand {
                token: 1,
                consumer,
                demand,
                caller_network,
            },
            PowerExportGrant {
                token: 1,
                granted: true,
                source_region: Some(RegionId(2)),
            },
        );
        assert!(region.world.power_consumers[&consumer].powered);

        let phase = region.begin_tick_power_demand_phase();

        assert!(
            phase
                .power_demands
                .iter()
                .any(|pending| pending.consumer == consumer),
            "an already-imported consumer must still be included in this \
             tick's fresh power demand batch, so the caller's reconciliation \
             re-requests the allocation it is about to release"
        );
        assert!(
            region.world.power_consumers[&consumer].powered,
            "the not-yet-released import must still read as powered for the \
             rest of this tick, protecting reads that happen before the \
             fresh grant round-trips back"
        );
    }

    /// Regression for docs/20260703-bug-cross-region-export-starvation-fix.md
    /// (second review round). If the fresh power request that follows the
    /// optimistic restore above gets DENIED — the producer had no capacity
    /// after all — the consumer must end unpowered with no import source and
    /// no phantom supplied-power stat, not stay optimistically powered
    /// forever with nothing backing it (the old reservation was already
    /// released; no new one replaced it).
    #[test]
    fn apply_power_export_grant_denial_clears_the_optimistic_restore() {
        let mut region = RegionState::new(RegionId(1), 2, 1);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        let consumer = region.world.grid.get(0, 0).expect("residential entity");
        let demand = region.world.power_consumers[&consumer].demand;
        let caller_network = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };

        region.apply_power_export_grant(
            PendingPowerDemand {
                token: 1,
                consumer,
                demand,
                caller_network,
            },
            PowerExportGrant {
                token: 1,
                granted: true,
                source_region: Some(RegionId(2)),
            },
        );
        region.begin_tick_power_demand_phase();
        assert!(
            region.world.power_consumers[&consumer].powered,
            "optimistically restored after this tick's demand collection"
        );
        let supplied_while_optimistic = region.world.stats.power.total_power_supplied;

        // The fresh request this tick's demand collection triggered comes
        // back denied (the producer had no spare capacity after all).
        region.apply_power_export_grant(
            PendingPowerDemand {
                token: 2,
                consumer,
                demand,
                caller_network,
            },
            PowerExportGrant {
                token: 2,
                granted: false,
                source_region: None,
            },
        );

        assert!(
            !region.world.power_consumers[&consumer].powered,
            "a denied replacement must clear the optimistic restore, not \
             leave the consumer powered with nothing backing it"
        );
        assert!(region.world.power_consumers[&consumer].source.is_none());
        assert_eq!(
            region.world.stats.power.total_power_supplied,
            supplied_while_optimistic - demand,
            "the phantom supplied-power stat must be unwound too"
        );
    }

    /// Retire-tickstate, P-a: a stale-but-granted reply's caller-side
    /// staleness check now happens BEFORE this function is even called
    /// (`RegionRuntime::apply_power_export_grant`), so `RegionState`'s ECS
    /// write only ever sees a denial from the caller's OWN current batch.
    /// But nothing pauses anymore, so between that request going out and its
    /// denial coming back, a LATER tick (or a local rebuild) can legitimately
    /// power this same consumer from `PowerSource::Local`. The denial must
    /// leave that untouched — it protects only the optimistic `Imported`
    /// restore, never a genuine local supply.
    #[test]
    fn apply_power_export_grant_denial_does_not_clear_local_power() {
        let mut region = RegionState::new(RegionId(1), 2, 1);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        let consumer = region.world.grid.get(0, 0).expect("residential entity");
        let demand = region.world.power_consumers[&consumer].demand;
        let caller_network = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };

        // Simulate: a later tick already powered this consumer locally
        // (e.g. a power plant was built and connected in the meantime).
        {
            let power_consumer = region.world.power_consumers.get_mut(&consumer).unwrap();
            power_consumer.powered = true;
            power_consumer.source = Some(PowerSource::Local(Entity::new(RegionId(1), 99)));
        }
        region.world.stats.power.total_power_supplied += demand;

        // A denial for an OLDER, now-superseded request arrives late.
        region.apply_power_export_grant(
            PendingPowerDemand {
                token: 1,
                consumer,
                demand,
                caller_network,
            },
            PowerExportGrant {
                token: 1,
                granted: false,
                source_region: None,
            },
        );

        assert!(
            region.world.power_consumers[&consumer].powered,
            "a denial must never clear genuine LOCAL power, only the \
             optimistic Imported restore it was meant to protect"
        );
        assert_eq!(
            region.world.power_consumers[&consumer].source,
            Some(PowerSource::Local(Entity::new(RegionId(1), 99)))
        );
    }

    /// Regression for docs/20260703-bug-cross-region-export-starvation-fix.md
    /// (third review round). `pending_power_demands` skips a consumer with no
    /// border-connected road network to request exported power through — e.g.
    /// the whole region has no border link at all, as reproduced here. If
    /// that consumer held an import from an earlier tick, no fresh request
    /// will ever be sent for it, so no reply will ever arrive to confirm or
    /// deny it — `apply_power_export_grant`'s denial cleanup never runs.
    /// Restoring it anyway would leave it powered forever even though the old
    /// reservation was already unconditionally released.
    #[test]
    fn begin_tick_power_demand_phase_does_not_restore_a_disconnected_consumers_import() {
        let mut region = RegionState::new(RegionId(1), 5, 5);
        assert!(region.build(2, 2, BuildingKind::Residential).success);
        assert!(region.build(2, 1, BuildingKind::Road).success);
        let consumer = region.world.grid.get(2, 2).expect("residential entity");
        let demand = region.world.power_consumers[&consumer].demand;
        let caller_network = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };
        // No cell in this 5x5 region touches a map edge, so there is no
        // border link anywhere for a fresh request to go through.
        assert!(region.network_border_links().is_empty());

        // Simulate: this consumer held an import from an earlier tick, back
        // when it (or the region) still had a border link to request through.
        region.apply_power_export_grant(
            PendingPowerDemand {
                token: 1,
                consumer,
                demand,
                caller_network,
            },
            PowerExportGrant {
                token: 1,
                granted: true,
                source_region: Some(RegionId(2)),
            },
        );
        assert!(region.world.power_consumers[&consumer].powered);

        let phase = region.begin_tick_power_demand_phase();

        assert!(
            phase.power_demands.is_empty(),
            "no border-connected network exists, so no fresh request can be made"
        );
        assert!(
            !region.world.power_consumers[&consumer].powered,
            "an import with no fresh request in flight must not be \
             optimistically restored — nothing will ever confirm or deny it \
             this tick, so it would stay powered forever with no producer \
             reservation behind it"
        );
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

        assert!(region.apply_workplace_assignment(
            commuter,
            WorkplaceAssignment {
                workplace: Entity::new(RegionId(2), 9),
                location: CityCellRef::local(RegionId(2), 1, 0),
                salary: 4,
            },
        ));

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
    fn published_job_pools_reports_open_count_and_salary_for_one_effective_workplace() {
        let mut region = RegionState::new(RegionId(1), 4, 4);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success);
        assert!(region.build(1, 1, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Commercial).success);
        region.ensure_derived_state();

        let workplace = region.world.grid.get(1, 0).expect("commercial entity");
        let pools = region.published_job_pools();

        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].region, RegionId(1));
        assert_eq!(pools[0].workplace, workplace);
        assert_eq!(
            pools[0].open_count, 2,
            "level-1 Commercial capacity_for is 2"
        );
        assert_eq!(pools[0].salary, region.workplace_salary(workplace));
        assert_eq!(
            pools[0].generation, 0,
            "directory stamps this on publish, not here"
        );
    }

    #[test]
    fn published_job_pools_lists_a_bridge_workplace_once_not_once_per_network() {
        let mut region = RegionState::new(RegionId(1), 4, 4);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success); // network A, powered
        assert!(region.build(1, 2, BuildingKind::Road).success); // network B, disconnected from A
        assert!(region.build(1, 1, BuildingKind::Commercial).success); // adjacent to both roads
        region.ensure_derived_state();

        let workplace = region.world.grid.get(1, 1).expect("commercial entity");
        let pools = region.published_job_pools();

        let rows_for_workplace: Vec<_> = pools
            .iter()
            .filter(|pool| pool.workplace == workplace)
            .collect();
        assert_eq!(
            rows_for_workplace.len(),
            1,
            "a workplace touching two networks must publish exactly one pool row, not one per network"
        );
        assert_eq!(rows_for_workplace[0].open_count, 2);
    }

    /// A powered, road-connected Commercial workplace (2 seats at level 1) and
    /// one jobless local citizen.
    fn employer_region_with_one_workplace() -> (RegionState, Entity) {
        let mut region = RegionState::new(RegionId(9), 4, 4);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success);
        assert!(region.build(1, 1, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Commercial).success);
        region.ensure_derived_state();
        let workplace = region.world.grid.get(1, 0).expect("commercial entity");
        (region, workplace)
    }

    #[test]
    fn unemployed_citizens_lists_only_jobless_citizens_in_entity_order() {
        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        let home = region.world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut region.world, home, 3);

        let mut all = region.world.citizens.keys().copied().collect::<Vec<_>>();
        all.sort_by_key(|citizen| citizen.0);
        assert_eq!(
            region.unemployed_citizens(),
            all,
            "every citizen starts jobless, in entity order"
        );

        // Give the middle citizen a job; it must drop out of the list.
        let employed = all[1];
        region
            .world
            .citizens
            .get_mut(&employed)
            .expect("citizen")
            .workplace_assignment = Some(WorkplaceAssignment {
            workplace: Entity::new(RegionId(2), 42),
            location: CityCellRef::local(RegionId(2), 0, 0),
            salary: 5,
        });

        assert_eq!(region.unemployed_citizens(), vec![all[0], all[2]]);
    }

    #[test]
    fn job_pool_still_has_open_capacity_counts_down_as_contracts_are_created() {
        // P3: "employer-owned capacity" = spare seats minus seats already
        // contracted to remote citizens.
        let (mut region, workplace) = employer_region_with_one_workplace();
        assert!(region.job_pool_still_has_open_capacity(workplace));

        let claim_for = |local: u32| JobClaim {
            claim_id: crate::core::regions::employment_directory::JobClaimId(local as u64),
            citizen: crate::core::regions::employment_directory::CitizenRef {
                region: RegionId(1),
                citizen: Entity::new(RegionId(1), local),
            },
            workplace,
            generation: 7,
        };

        let first = region.accept_claim_and_create_assignment(&claim_for(50));
        assert_eq!(first.workplace, workplace);
        assert_eq!(first.location.region, RegionId(9), "self-describing cell");
        assert_eq!(first.salary, region.workplace_salary(workplace));
        assert!(
            region.job_pool_still_has_open_capacity(workplace),
            "2 seats, 1 contracted -> still open"
        );

        region.accept_claim_and_create_assignment(&claim_for(51));
        assert!(
            !region.job_pool_still_has_open_capacity(workplace),
            "2 seats, 2 contracted -> full"
        );
        assert_eq!(region.contract_holders_at(workplace).len(), 2);
    }

    #[test]
    fn published_job_pools_excludes_seats_already_contracted_to_remote_citizens() {
        // The employer's republished open_count must not resurrect a contracted
        // seat. P7-a: the subtraction now lives in the registry (reserved seats
        // are held out of `remaining_workplaces`), so `published_job_pools`
        // reads an already-net count rather than subtracting contracts itself.
        let (mut region, workplace) = employer_region_with_one_workplace();
        assert_eq!(region.published_job_pools()[0].open_count, 2);

        region.accept_claim_and_create_assignment(&JobClaim {
            claim_id: crate::core::regions::employment_directory::JobClaimId(1),
            citizen: crate::core::regions::employment_directory::CitizenRef {
                region: RegionId(1),
                citizen: Entity::new(RegionId(1), 50),
            },
            workplace,
            generation: 1,
        });
        assert_eq!(
            region.published_job_pools()[0].open_count,
            1,
            "one of two seats is contracted; only one is still claimable"
        );

        region.accept_claim_and_create_assignment(&JobClaim {
            claim_id: crate::core::regions::employment_directory::JobClaimId(2),
            citizen: crate::core::regions::employment_directory::CitizenRef {
                region: RegionId(1),
                citizen: Entity::new(RegionId(1), 51),
            },
            workplace,
            generation: 1,
        });
        assert!(
            region.published_job_pools().is_empty(),
            "a fully contracted workplace advertises no pool at all"
        );
    }

    fn contract_claim(local: u32, workplace: Entity, generation: u64) -> JobClaim {
        JobClaim {
            claim_id: crate::core::regions::employment_directory::JobClaimId(local as u64),
            citizen: CitizenRef {
                region: RegionId(1),
                citizen: Entity::new(RegionId(1), local),
            },
            workplace,
            generation,
        }
    }

    #[test]
    fn save_record_round_trip_preserves_employer_contracts() {
        // P6: employer-side truth is durable. A contract survives
        // into_save_record -> from_save_record, and its reserved seat is
        // re-synced (from_world_with_employer_state) so it is still held out.
        let (mut region, workplace) = employer_region_with_one_workplace();
        region.accept_claim_and_create_assignment(&contract_claim(50, workplace, 1));
        let before = region.employer_contracts();
        assert_eq!(before.len(), 1, "one contract before save");

        let restored = RegionState::from_save_record(region.into_save_record());

        assert_eq!(
            restored.employer_contracts(),
            before,
            "the employer contract survives save/load"
        );
        assert_eq!(
            restored.contracted_workplace_tax_slots(),
            vec![workplace],
            "and its reserved seat is re-synced into the jobs registry"
        );
    }

    #[test]
    fn save_record_round_trip_preserves_a_remote_workplace_assignment() {
        // P6: home-side truth is durable. Pre-P6 the scrub cleared every
        // workplace_assignment on save; now a cross-region assignment survives.
        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        let home = region.world.grid.get(0, 0).expect("home entity");
        crate::core::systems::citizens::spawn_for_home(&mut region.world, home, 1);
        region.ensure_derived_state();
        let citizen = *region.world.citizens.keys().next().expect("one citizen");

        let remote_workplace = Entity::new(RegionId(9), 1);
        let assignment = WorkplaceAssignment {
            workplace: remote_workplace,
            location: CityCellRef::local(RegionId(9), 1, 0),
            salary: 5,
        };
        assert!(region.apply_workplace_assignment(citizen, assignment));

        let restored = RegionState::from_save_record(region.into_save_record());

        assert_eq!(
            restored.home_assignments(),
            vec![(citizen, assignment)],
            "the remote assignment survives save/load instead of being scrubbed"
        );
    }

    #[test]
    fn rebuild_releases_a_lone_contract_and_frees_its_seat() {
        // P6 codex Medium: a contract with no matching home assignment is a
        // half-torn lease. The rebuild releases it from employer truth and, since
        // its seat is no longer reserved, republishes that seat as open.
        use crate::core::regions::employment_directory::rebuild_employment_broker_state;

        let open_at = |employer: &RegionState, workplace: Entity| -> u16 {
            employer
                .published_job_pools()
                .iter()
                .find(|pool| pool.workplace == workplace)
                .map_or(0, |pool| pool.open_count)
        };

        let (mut employer, workplace) = employer_region_with_one_workplace();
        let open_before = open_at(&employer, workplace);
        employer.accept_claim_and_create_assignment(&contract_claim(50, workplace, 1));
        assert_eq!(employer.employer_contracts().len(), 1);
        assert_eq!(
            open_at(&employer, workplace),
            open_before - 1,
            "the contract reserves one seat, so one fewer is published open"
        );

        let _ = rebuild_employment_broker_state(std::slice::from_mut(&mut employer));

        assert!(
            employer.employer_contracts().is_empty(),
            "the lone contract is released on rebuild"
        );
        assert_eq!(
            open_at(&employer, workplace),
            open_before,
            "and its freed seat is published open again"
        );
    }

    #[test]
    fn rebuild_re_fills_a_freed_seat_with_a_local_seeker() {
        // P6 codex Medium: releasing a lone contract must let a blocked LOCAL
        // seeker take the freed seat DURING the rebuild (resettle_derived_state),
        // not leave it advertised to remote regions until the first tick. Without
        // the resettle the citizen would still be unemployed here.
        use crate::core::regions::employment_directory::{
            JobClaimId, rebuild_employment_broker_state,
        };

        let mut region = RegionState::new(RegionId(1), 4, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Commercial).success);
        assert!(region.build(0, 1, BuildingKind::Road).success);
        assert!(region.build(1, 1, BuildingKind::Road).success);
        assert!(region.build(2, 1, BuildingKind::PowerPlant).success);
        region.ensure_derived_state();
        let workplace = region.world.grid.get(1, 0).expect("commercial");
        assert!(
            region
                .published_job_pools()
                .iter()
                .any(|pool| pool.workplace == workplace),
            "the commercial must be a powered, connected workplace with open seats"
        );

        // Reserve every seat with lone remote contracts (no home assignment).
        let mut next = 0u32;
        while region
            .published_job_pools()
            .iter()
            .any(|pool| pool.workplace == workplace)
        {
            region.accept_claim_and_create_assignment(&JobClaim {
                claim_id: JobClaimId(next as u64),
                citizen: CitizenRef {
                    region: RegionId(2),
                    citizen: Entity::new(RegionId(2), next),
                },
                workplace,
                generation: 1,
            });
            next += 1;
            assert!(next < 100, "the workplace should saturate well before this");
        }

        // The local seeker is blocked while every seat is reserved.
        let home = region.world.grid.get(0, 0).expect("home entity");
        crate::core::systems::citizens::spawn_for_home(&mut region.world, home, 1);
        region.resettle_derived_state();
        let citizen = *region.world.citizens.keys().next().expect("one citizen");
        assert!(
            region.world.citizens[&citizen]
                .workplace_assignment
                .is_none(),
            "the local seeker cannot work while every seat is reserved"
        );

        let _ = rebuild_employment_broker_state(std::slice::from_mut(&mut region));

        assert_eq!(
            region.world.citizens[&citizen]
                .workplace_assignment
                .map(|a| a.workplace),
            Some(workplace),
            "the freed seat goes to the local seeker during the rebuild, not to a remote"
        );
    }

    #[test]
    fn rebuild_clears_a_lone_remote_assignment() {
        // P6 codex Medium: a cross-region home assignment with no matching
        // employer contract is a half-torn lease from the home side. The rebuild
        // marks the citizen unemployed so it re-claims on the first daily phase.
        use crate::core::regions::employment_directory::rebuild_employment_broker_state;

        let mut home = RegionState::new(RegionId(1), 3, 3);
        assert!(home.build(0, 0, BuildingKind::Residential).success);
        let home_building = home.world.grid.get(0, 0).expect("home entity");
        crate::core::systems::citizens::spawn_for_home(&mut home.world, home_building, 1);
        home.ensure_derived_state();
        let citizen = *home.world.citizens.keys().next().expect("one citizen");
        assert!(home.apply_workplace_assignment(
            citizen,
            WorkplaceAssignment {
                workplace: Entity::new(RegionId(9), 1),
                location: CityCellRef::local(RegionId(9), 1, 0),
                salary: 5,
            }
        ));
        assert_eq!(home.home_assignments().len(), 1);

        let _ = rebuild_employment_broker_state(std::slice::from_mut(&mut home));

        assert!(
            home.home_assignments().is_empty(),
            "the unbacked remote assignment is cleared -- the citizen re-claims later"
        );
    }

    #[test]
    fn rebuild_keeps_a_local_assignment_untouched() {
        // P6 codex Medium: local jobs carry no contract and are not
        // directory-coordinated, so the reconciliation must never clear them.
        use crate::core::regions::employment_directory::rebuild_employment_broker_state;

        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        let home_building = region.world.grid.get(0, 0).expect("home entity");
        crate::core::systems::citizens::spawn_for_home(&mut region.world, home_building, 1);
        region.ensure_derived_state();
        let citizen = *region.world.citizens.keys().next().expect("one citizen");
        let local_workplace = Entity::new(RegionId(1), 42);
        assert!(region.apply_workplace_assignment(
            citizen,
            WorkplaceAssignment {
                workplace: local_workplace,
                location: CityCellRef::local(RegionId(1), 1, 0),
                salary: 4,
            }
        ));

        let _ = rebuild_employment_broker_state(std::slice::from_mut(&mut region));

        assert_eq!(
            region.world.citizens[&citizen]
                .workplace_assignment
                .map(|a| a.workplace),
            Some(local_workplace),
            "a local assignment is never touched by the cross-region reconciliation"
        );
    }

    #[test]
    fn release_contracts_over_current_capacity_keeps_a_healthy_fully_contracted_workplace() {
        // The reason this cannot be driven off `published_job_pools`: a fully
        // contracted, perfectly healthy workplace publishes NO row (open_count
        // 0), exactly like an invalid one. Contracts must be checked against the
        // employer's own seats, not against its published pools.
        let (mut region, workplace) = employer_region_with_one_workplace();
        region.accept_claim_and_create_assignment(&contract_claim(50, workplace, 1));
        region.accept_claim_and_create_assignment(&contract_claim(51, workplace, 1));
        assert!(region.published_job_pools().is_empty(), "no row published");

        assert!(
            region.release_contracts_over_current_capacity().is_empty(),
            "2 seats, 2 contracts: nothing is lost"
        );
        assert_eq!(region.contract_holders_at(workplace).len(), 2);
    }

    #[test]
    fn release_contracts_over_current_capacity_drops_every_contract_when_the_workplace_dies() {
        let (mut region, workplace) = employer_region_with_one_workplace();
        region.accept_claim_and_create_assignment(&contract_claim(50, workplace, 1));
        region.accept_claim_and_create_assignment(&contract_claim(51, workplace, 1));

        // Bulldoze the workplace: no effective seats remain.
        assert!(region.bulldoze(1, 0).success);
        region.ensure_derived_state();

        let lost = region.release_contracts_over_current_capacity();
        assert_eq!(lost.len(), 2, "both contracts are lost");
        assert!(lost.iter().all(|(w, _, _)| *w == workplace));
        assert!(region.contract_holders_at(workplace).is_empty());
    }

    #[test]
    fn release_contracts_evicts_the_most_recently_hired_first() {
        // The plan leaves the eviction policy to "deterministic local policy".
        // Ours is seniority: sort by (accepted_generation, citizen), evict from
        // the end.
        //
        // P7-a: eviction fires only on genuine PHYSICAL capacity loss (a local
        // citizen can no longer shrink contract capacity -- contracts are
        // reserved before local matching). Simulate a workplace that held 3
        // contracts and then lost a seat: 2 physical seats, 3 contracts ->
        // evict the newest, keep the two most senior.
        let (mut region, workplace) = employer_region_with_one_workplace(); // 2 seats
        let senior = contract_claim(50, workplace, 1);
        let middle = contract_claim(51, workplace, 5);
        let junior = contract_claim(52, workplace, 9);
        region.accept_claim_and_create_assignment(&senior);
        region.accept_claim_and_create_assignment(&middle);
        region.accept_claim_and_create_assignment(&junior);

        let lost = region.release_contracts_over_current_capacity();
        assert_eq!(lost.len(), 1, "one contract exceeds the two physical seats");
        assert_eq!(
            lost[0].1, junior.citizen,
            "the most recently hired worker is the one who loses the job"
        );
        assert_eq!(
            region.contract_holders_at(workplace),
            vec![senior.citizen, middle.citizen],
            "the two longest-serving workers keep their seats"
        );
    }

    #[test]
    fn contracted_seats_are_reserved_before_local_matching() {
        // P7-a: a fully contracted workplace leaves a local job seeker
        // unmatched, and evicts nobody. Contracts win.
        let (mut region, workplace) = employer_region_with_one_workplace(); // 2 seats
        region.accept_claim_and_create_assignment(&contract_claim(50, workplace, 1));
        region.accept_claim_and_create_assignment(&contract_claim(51, workplace, 2));

        // A local resident moves in, wanting work.
        assert!(region.build(0, 2, BuildingKind::Road).success);
        assert!(region.build(1, 2, BuildingKind::Residential).success);
        let home = region.world.grid.get(1, 2).expect("home");
        citizens::spawn_for_home(&mut region.world, home, 1);
        region.ensure_derived_state();
        let local = *region
            .world
            .citizens
            .keys()
            .find(|c| region.world.citizens[c].home == home)
            .expect("local citizen");

        assert_eq!(
            region.spare_job_slots_for_workplace(workplace),
            0,
            "both physical seats are reserved for the contracts"
        );
        region.ensure_derived_state();
        assert!(
            region.world.citizens[&local].workplace_assignment.is_none(),
            "the local citizen cannot take a contracted seat"
        );
        assert!(
            region.release_contracts_over_current_capacity().is_empty(),
            "the local seeker does not shrink contract capacity: nobody is evicted"
        );
        assert_eq!(region.contract_holders_at(workplace).len(), 2);
        assert_eq!(
            region.world.cached_job_counts().unemployment,
            1,
            "the unmatched local counts as unemployed: reserved seats are not local jobs"
        );
    }

    #[test]
    fn release_contract_if_matches_only_drops_the_exact_contract() {
        let (mut region, workplace) = employer_region_with_one_workplace();
        let held = contract_claim(50, workplace, 1);
        region.accept_claim_and_create_assignment(&held);

        let stranger = CitizenRef {
            region: RegionId(1),
            citizen: Entity::new(RegionId(1), 99),
        };
        assert!(!region.release_contract_if_matches(workplace, stranger));
        assert!(!region.release_contract_if_matches(Entity::new(RegionId(9), 77), held.citizen));
        assert_eq!(region.contract_holders_at(workplace).len(), 1);

        assert!(region.release_contract_if_matches(workplace, held.citizen));
        assert!(region.contract_holders_at(workplace).is_empty());
        assert!(
            !region.release_contract_if_matches(workplace, held.citizen),
            "releasing twice is not a second release"
        );
    }

    #[test]
    fn clear_employment_returns_the_assignment_it_gave_up() {
        let (mut region, citizen, assignment) = home_region_with_one_citizen();
        assert!(region.apply_workplace_assignment(citizen, assignment));

        assert_eq!(region.clear_employment(citizen), Some(assignment));
        assert!(
            region.world.citizens[&citizen]
                .workplace_assignment
                .is_none()
        );
        assert_eq!(
            region.clear_employment(citizen),
            None,
            "there is nothing left to release"
        );
    }

    #[test]
    fn clear_employment_if_matches_leaves_a_citizen_who_moved_on_alone() {
        // P5 behavior forbidden: "do not clear a home assignment if the citizen
        // already moved to a different workplace."
        let (mut region, citizen, assignment) = home_region_with_one_citizen();
        assert!(region.apply_workplace_assignment(citizen, assignment));

        let elsewhere = Entity::new(RegionId(9), 77);
        assert!(!region.clear_employment_if_matches(citizen, elsewhere));
        assert_eq!(
            region.world.citizens[&citizen].workplace_assignment,
            Some(assignment),
            "a stale loss for a different workplace changes nothing"
        );

        assert!(region.clear_employment_if_matches(citizen, assignment.workplace));
        assert!(
            region.world.citizens[&citizen]
                .workplace_assignment
                .is_none()
        );
    }

    // ---- P7-c: route invalidation ----

    /// A discovery snapshot whose single component contains `nets`.
    fn discovery_with_component(nets: Vec<RegionRoadNetworkId>) -> CrossRegionDiscovery {
        CrossRegionDiscovery {
            components: vec![nets],
            ..Default::default()
        }
    }

    fn contract_from(home: u32, local: u32, workplace: Entity) -> JobClaim {
        JobClaim {
            claim_id: crate::core::regions::employment_directory::JobClaimId(local as u64),
            citizen: CitizenRef {
                region: RegionId(home),
                citizen: Entity::new(RegionId(home), local),
            },
            workplace,
            generation: 1,
        }
    }

    #[test]
    fn workplace_networks_returns_the_road_networks_the_workplace_touches() {
        let (region, workplace) = employer_region_with_one_workplace();
        let nets = region.workplace_networks(workplace);
        assert_eq!(
            nets,
            vec![RegionRoadNetworkId {
                region: RegionId(9),
                road_network: 0,
            }],
            "the commercial touches its single road network"
        );
    }

    #[test]
    fn contract_route_is_reachable_only_when_home_shares_the_component() {
        let (region, workplace) = employer_region_with_one_workplace();
        let workplace_net = region.workplace_networks(workplace)[0];
        let home_net = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };

        // Home 1 and the workplace's network are in one component -> reachable.
        let shared = discovery_with_component(vec![workplace_net, home_net]);
        assert!(region.contract_route_is_reachable(&shared, workplace, RegionId(1)));

        // The workplace's network is in a component with no region-1 network.
        let split = discovery_with_component(vec![workplace_net]);
        assert!(!region.contract_route_is_reachable(&split, workplace, RegionId(1)));

        // An empty discovery reaches nobody.
        assert!(!region.contract_route_is_reachable(
            &CrossRegionDiscovery::default(),
            workplace,
            RegionId(1)
        ));
    }

    #[test]
    fn a_workplace_that_lost_its_roads_is_unreachable() {
        let (mut region, workplace) = employer_region_with_one_workplace();
        let workplace_net = region.workplace_networks(workplace)[0];
        let home_net = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };
        let discovery = discovery_with_component(vec![workplace_net, home_net]);
        assert!(region.contract_route_is_reachable(&discovery, workplace, RegionId(1)));

        // Bulldoze the road tiles the workplace touched: it now has no networks.
        assert!(region.bulldoze(0, 1).success);
        assert!(region.bulldoze(1, 1).success);
        region.ensure_derived_state();
        assert!(
            region.workplace_networks(workplace).is_empty(),
            "no adjacent roads remain"
        );
        assert!(
            !region.contract_route_is_reachable(&discovery, workplace, RegionId(1)),
            "a workplace with no road networks is reachable from nowhere"
        );
    }

    #[test]
    fn release_contracts_with_unreachable_homes_drops_only_the_disconnected() {
        let (mut region, workplace) = employer_region_with_one_workplace();
        let workplace_net = region.workplace_networks(workplace)[0];

        // Two contracts: citizen from home 1 and citizen from home 2.
        let c1 = contract_from(1, 50, workplace);
        let c2 = contract_from(2, 60, workplace);
        region.accept_claim_and_create_assignment(&c1);
        region.accept_claim_and_create_assignment(&c2);

        // Discovery: home 1 shares the workplace's component; home 2 does not.
        let discovery = discovery_with_component(vec![
            workplace_net,
            RegionRoadNetworkId {
                region: RegionId(1),
                road_network: 0,
            },
        ]);

        let lost = region.release_contracts_with_unreachable_homes(&discovery);
        assert_eq!(
            lost.len(),
            1,
            "only the disconnected home's contract is lost"
        );
        assert_eq!(lost[0].1.region, RegionId(2));
        assert_eq!(
            region.contract_holders_at(workplace),
            vec![c1.citizen],
            "the reachable home's worker keeps the job"
        );
        // P7-a interaction: eviction re-syncs the reservation, so the freed seat
        // becomes publishable. 2 seats, 2 contracts -> 0 open; after dropping
        // one -> 1 open.
        assert_eq!(
            region.published_job_pools()[0].open_count,
            1,
            "the evicted contract's seat is freed back to publishable capacity"
        );
    }

    #[test]
    fn a_bridge_workplace_stays_reachable_while_either_network_reaches_home() {
        // The `.any()` case, and the plan's bridge-asymmetry note: a workplace
        // touching two disconnected local networks stays reachable as long as
        // EITHER network shares a component with the home.
        let mut region = RegionState::new(RegionId(9), 4, 4);
        assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
        assert!(region.build(0, 1, BuildingKind::Road).success); // network A (powered)
        assert!(region.build(1, 2, BuildingKind::Road).success); // network B (disconnected)
        assert!(region.build(1, 1, BuildingKind::Commercial).success); // adjacent to both
        region.ensure_derived_state();
        let workplace = region.world.grid.get(1, 1).expect("commercial");
        let nets = region.workplace_networks(workplace);
        assert_eq!(nets.len(), 2, "the workplace bridges two road networks");

        let home_net = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };
        // Home reaches ONLY the second network; the first is in a lone component.
        let discovery = CrossRegionDiscovery {
            components: vec![vec![nets[0]], vec![nets[1], home_net]],
            ..Default::default()
        };
        assert!(
            region.contract_route_is_reachable(&discovery, workplace, RegionId(1)),
            "reachable via the second network alone"
        );

        // Now home reaches neither network.
        let unreachable = CrossRegionDiscovery {
            components: vec![vec![nets[0]], vec![nets[1]], vec![home_net]],
            ..Default::default()
        };
        assert!(!region.contract_route_is_reachable(&unreachable, workplace, RegionId(1)));
    }

    #[test]
    fn accept_claim_records_the_claims_generation_on_the_contract() {
        let (mut region, workplace) = employer_region_with_one_workplace();
        region.accept_claim_and_create_assignment(&JobClaim {
            claim_id: crate::core::regions::employment_directory::JobClaimId(1),
            citizen: crate::core::regions::employment_directory::CitizenRef {
                region: RegionId(1),
                citizen: Entity::new(RegionId(1), 50),
            },
            workplace,
            generation: 12,
        });

        let holders = region.contract_holders_at(workplace);
        assert_eq!(holders.len(), 1);
        assert_eq!(
            region.employer_state.contracts_by_workplace[&workplace][&holders[0]]
                .accepted_generation,
            12,
            "the pool generation in effect at accept time is recorded on the contract"
        );
    }

    /// A home region with one jobless citizen, and a remote assignment for it.
    fn home_region_with_one_citizen() -> (RegionState, Entity, WorkplaceAssignment) {
        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        let home = region.world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut region.world, home, 1);
        region.ensure_derived_state();
        let citizen = *region.world.citizens.keys().next().expect("citizen");
        let assignment = WorkplaceAssignment {
            workplace: Entity::new(RegionId(9), 42),
            location: CityCellRef::local(RegionId(9), 1, 0),
            salary: 40,
        };
        (region, citizen, assignment)
    }

    #[test]
    fn apply_workplace_assignment_writes_the_citizen_and_is_idempotent() {
        // P4 review check: "repeated EmploymentDirectoryReady events are
        // idempotent." The directory's accepted read cache re-offers an
        // already-applied citizen on every wake.
        let (mut region, citizen, assignment) = home_region_with_one_citizen();

        assert!(
            region.apply_workplace_assignment(citizen, assignment),
            "the first apply lands"
        );
        assert_eq!(
            region.world.citizens[&citizen].workplace_assignment,
            Some(assignment)
        );

        assert!(
            !region.apply_workplace_assignment(citizen, assignment),
            "a repeated apply reports 'nothing newly applied'"
        );
        assert_eq!(
            region.world.citizens[&citizen].workplace_assignment,
            Some(assignment),
            "and leaves the assignment exactly as it was"
        );
    }

    #[test]
    fn apply_workplace_assignment_never_clears_an_existing_assignment() {
        // P4 behavior forbidden: "do not clear an old assignment while merely
        // checking for replacement work." A citizen who picked up a local job
        // between claim and apply keeps it.
        let (mut region, citizen, remote) = home_region_with_one_citizen();
        let local = WorkplaceAssignment {
            workplace: Entity::new(RegionId(1), 7),
            location: CityCellRef::local(RegionId(1), 2, 2),
            salary: 5,
        };
        region
            .world
            .citizens
            .get_mut(&citizen)
            .unwrap()
            .workplace_assignment = Some(local);

        assert!(!region.apply_workplace_assignment(citizen, remote));
        assert_eq!(
            region.world.citizens[&citizen].workplace_assignment,
            Some(local),
            "the existing (local) job survives an incoming remote assignment"
        );
    }

    #[test]
    fn apply_workplace_assignment_ignores_a_citizen_that_no_longer_exists() {
        let (mut region, _citizen, assignment) = home_region_with_one_citizen();
        let ghost = Entity::new(RegionId(1), 9999);
        assert!(!region.apply_workplace_assignment(ghost, assignment));
    }

    #[test]
    fn an_attended_remote_assignment_is_paid_by_the_next_daily_economy_phase() {
        // P4 review checks: "payment path uses home-region
        // Citizen.workplace_assignment" and "accepted worker is paid on the next
        // daily economy phase after apply."
        // Salary is private citizen money (the city collects only workplace tax,
        // and a remote workplace's tax accrues to the *exporting* region).
        let (mut region, citizen, assignment) = home_region_with_one_citizen();
        // Start solvent, so rent is paid on BOTH days and the only difference
        // between them is the salary. (A broke citizen skips rent, which would
        // otherwise show up in the delta.)
        region.world.citizens.get_mut(&citizen).unwrap().money = 1_000;

        let before = region.world.citizens[&citizen].money;
        economy::run(&mut region.world, &[]);
        let jobless_delta = region.world.citizens[&citizen].money - before;

        // Now apply the remote assignment, record the P1 arrival, and settle.
        assert!(region.apply_workplace_assignment(citizen, assignment));
        region
            .world
            .citizens
            .get_mut(&citizen)
            .unwrap()
            .attended_since_daily_settlement = true;
        let before = region.world.citizens[&citizen].money;
        economy::run(&mut region.world, &[]);
        let employed_delta = region.world.citizens[&citizen].money - before;

        assert_eq!(
            employed_delta - jobless_delta,
            assignment.salary,
            "the applied assignment's captured salary is what the citizen is paid"
        );
    }

    #[test]
    fn applying_an_assignment_does_not_re_dirty_the_daily_employment_gate() {
        // If apply re-flagged `jobs_exports_dirty`, the next daily employment
        // phase would churn the very assignment just applied (the bug
        // `refresh_jobs_cache_after_grant_applied` exists to close).
        let (mut region, citizen, assignment) = home_region_with_one_citizen();
        region.clear_jobs_exports_dirty();

        assert!(region.apply_workplace_assignment(citizen, assignment));
        assert!(
            !region.world.is_jobs_exports_dirty(),
            "applying an accepted assignment must not re-open the daily employment gate"
        );
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

    use crate::core::components::{PendingHandoff, TravelerId};

    /// P-c: a multi-hop `exits_from(r)` map populates `remote_exit_cells`
    /// for a final target T. The test region is built with one East-edge
    /// road; the exits_from map says A's first hop toward T (region 2) is
    /// the East border. The consumer reads by FINAL target, so the cell
    /// is recorded under region 2 even though region 2 is also the first
    /// hop in this 1-hop test.
    #[test]
    fn set_region_routes_populates_remote_exit_cells() {
        use crate::core::regions::ExitLink;
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success); // East edge
        let exit = a.world.grid.get(1, 0).expect("road");
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        // A's exits toward T=region 2: first hop is region 2 via East,
        // remaining Layer-1 cost = 1 (the cross; B is the destination).
        let exits_from: HashMap<RegionId, Vec<ExitLink>> = HashMap::from([(
            RegionId(2),
            vec![ExitLink {
                link,
                to_region: RegionId(2),
                cost: 1,
            }],
        )]);
        a.set_region_routes(&exits_from);
        // The mover's `remote_exit_cells[target_region]` (FINAL target 2)
        // contains the local East-edge cell, with the same per-exit cost.
        assert_eq!(
            a.world.remote_exit_cells.get(&RegionId(2)),
            Some(&vec![RouteExit {
                cell: exit,
                link,
                to_region: RegionId(2),
                cost: 1,
            }])
        );
    }

    /// P-c: an empty route field means no remote exits. Direct-neighbour
    /// routing also comes through `RegionRoutes`; there is no separate
    /// border-neighbour fallback.
    #[test]
    fn set_region_routes_empty_routes_clear_remote_exit_cells() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success); // East edge
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        a.set_region_routes(&HashMap::from([(
            RegionId(2),
            vec![ExitLink {
                link,
                to_region: RegionId(2),
                cost: 1,
            }],
        )]));
        assert!(a.world.remote_exit_cells.contains_key(&RegionId(2)));
        a.set_region_routes(&HashMap::new());
        assert!(a.world.remote_exit_cells.is_empty());
    }

    /// Draining a Move resolves the facing border link and emits a `TravelerHandoff`.
    #[test]
    fn drain_move_resolves_border_link() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success);
        let exit = a.world.grid.get(1, 0).expect("road");
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let traveler = TravelerId {
            citizen: Entity::new(RegionId(1), 5),
            generation: 1,
        };
        let home = Entity::new(RegionId(1), 0);
        let workplace = Entity::new(RegionId(2), 9);
        let token = TravelToken {
            state: TravelState {
                status: crate::core::components::TravelStatus::Traveling,
                current_cell: Some(exit),
                destination: Some(workplace),
                building: None,
                dwell: 0,
                prev_cell: None,
            },
            home: crate::core::components::PlaceRef {
                region: RegionId(1),
                building: home,
            },
            work: Some(crate::core::components::PlaceRef {
                region: RegionId(2),
                building: workplace,
            }),
            trip_gen: 1,
        };
        a.world.outgoing_handoffs.push(PendingHandoff::Move {
            traveler,
            token,
            to_region: RegionId(2),
            exit_cell: exit,
            exit_link: link,
        });

        let handoffs = a.drain_traveler_handoffs();
        assert_eq!(handoffs.len(), 1);
        let handoff = &handoffs[0];
        assert_eq!(handoff.to_region, RegionId(2));
        assert_eq!(handoff.entry_link, Some(link));
        assert_eq!(handoff.kind, HandoffKind::Move);
        assert!(a.world.outgoing_handoffs.is_empty(), "buffer drained");
    }

    /// An unroutable Move (no facing link) rolls the away citizen back home
    /// rather than stranding it.
    #[test]
    fn drain_move_rolls_back_when_unroutable() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success);
        let exit = a.world.grid.get(1, 0).expect("road");
        // The handoff carries a stale link that no longer resolves to the exit.
        let citizen = Entity::new(RegionId(1), 5);
        // The guard `home_accepts` requires the citizen to be in
        // `world.citizens`; the test pre-registers it so the rollback fires.
        a.world.citizens.insert(
            citizen,
            Citizen {
                id: citizen,
                age: 1,
                home: Entity::new(RegionId(1), 0),
                workplace_assignment: None,
                morale: Morale::default(),
                money: 0,
                arrival_action: CitizenArrivalAction::ReturnHome,
                work_trip_generation: 1,
                attended_since_daily_settlement: false,
            },
        );
        a.world.away_residents.insert(citizen);
        let home = Entity::new(RegionId(1), 0);
        let workplace = Entity::new(RegionId(2), 9);
        let token = TravelToken {
            state: TravelState::default(),
            home: crate::core::components::PlaceRef {
                region: RegionId(1),
                building: home,
            },
            work: Some(crate::core::components::PlaceRef {
                region: RegionId(2),
                building: workplace,
            }),
            trip_gen: 1,
        };
        a.world.outgoing_handoffs.push(PendingHandoff::Move {
            traveler: TravelerId {
                citizen,
                generation: 1,
            },
            token,
            to_region: RegionId(2),
            exit_cell: exit,
            exit_link: BorderLinkId {
                edge: BorderEdge::West,
                offset: 0,
            },
        });

        let handoffs = a.drain_traveler_handoffs();
        assert!(handoffs.is_empty(), "nothing routed");
        assert!(
            !a.world.away_residents.contains(&citizen),
            "rolled back home"
        );
    }

    /// Receiving a Move at a host (foreign home) places the token at the entry cell.
    #[test]
    fn receive_move_at_host_places_token() {
        // Region B with a West-edge road at (0,0).
        let mut b = RegionState::new(RegionId(2), 2, 1);
        assert!(b.build(0, 0, BuildingKind::Road).success);
        let entry = b.world.grid.get(0, 0).expect("road");

        let traveler = TravelerId {
            citizen: Entity::new(RegionId(1), 5),
            generation: 1,
        };
        let home = Entity::new(RegionId(1), 0);
        let workplace = Entity::new(RegionId(2), 9);
        let handoff = TravelerHandoff {
            token: TravelToken {
                state: TravelState::default(),
                home: crate::core::components::PlaceRef {
                    region: RegionId(1),
                    building: home,
                },
                work: Some(crate::core::components::PlaceRef {
                    region: RegionId(2),
                    building: workplace,
                }),
                trip_gen: 1,
            },
            traveler,
            to_region: RegionId(2),
            entry_link: Some(BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            }),
            kind: HandoffKind::Move,
        };
        let bounce = b.receive_traveler_handoff(handoff);
        assert!(bounce.is_empty(), "placed, no bounce");
        let token = b.world.tokens.get(&traveler.citizen).expect("token placed");
        assert_eq!(token.state.current_cell, Some(entry));
    }

    /// Receiving a Rollback clears the home citizen's `away_residents` record.
    #[test]
    fn receive_rollback_clears_away() {
        let mut a = RegionState::new(RegionId(1), 1, 1);
        let citizen = Entity::new(RegionId(1), 5);
        // `home_accepts` requires the citizen to be in `world.citizens`.
        a.world.citizens.insert(
            citizen,
            Citizen {
                id: citizen,
                age: 1,
                home: Entity::new(RegionId(1), 0),
                workplace_assignment: None,
                morale: Morale::default(),
                money: 0,
                arrival_action: CitizenArrivalAction::ReturnHome,
                work_trip_generation: 1,
                attended_since_daily_settlement: false,
            },
        );
        a.world.away_residents.insert(citizen);

        let bounce = a.receive_traveler_handoff(TravelerHandoff {
            token: TravelToken {
                state: TravelState::default(),
                home: crate::core::components::PlaceRef {
                    region: RegionId(1),
                    building: Entity::new(RegionId(1), 0),
                },
                work: None,
                trip_gen: 1,
            },
            traveler: TravelerId {
                citizen,
                generation: 1,
            },
            to_region: RegionId(1),
            entry_link: None,
            kind: HandoffKind::Rollback,
        });
        assert!(bounce.is_empty());
        assert!(!a.world.away_residents.contains(&citizen));
    }

    /// P-a: the per-region report prices entry → exit crossings on the
    /// region's own road graph. A simple 2-road setup: two adjacent cells on
    /// the West/East borders of a 2×2 region. The report contains symmetric
    /// crossing costs between the two border links (adjacent → cost 1).
    #[test]
    fn road_report_prices_entry_to_exit() {
        let mut a = RegionState::new(RegionId(0), 2, 2);
        assert!(a.build(0, 0, BuildingKind::Road).success);
        assert!(a.build(1, 0, BuildingKind::Road).success);

        let mut border_neighbours = std::collections::HashMap::new();
        border_neighbours.insert(
            BorderLinkId {
                edge: BorderEdge::West,
                offset: 0,
            },
            RegionId(1),
        );
        border_neighbours.insert(
            BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            },
            RegionId(2),
        );

        let report = a.road_report(&border_neighbours);

        assert_eq!(report.region, RegionId(0));

        // The report includes the West↔East pair (adjacent cells, 1 hop on the
        // same network). Other pairs may also exist (each border cell has
        // multiple border links on a 2×2 grid with 4-edge borders), but
        // West↔East at 1 hop is the load-bearing invariant.
        let west_east = |entry_edge, exit_edge| {
            report
                .crossing_costs
                .iter()
                .find(|c| {
                    c.entry.edge == entry_edge
                        && c.exit.edge == exit_edge
                        && c.entry.offset == 0
                        && c.exit.offset == 0
                })
                .map(|c| c.cost)
        };
        assert_eq!(west_east(BorderEdge::West, BorderEdge::East), Some(1));
        assert_eq!(west_east(BorderEdge::East, BorderEdge::West), Some(1));

        // Self-pairs (entry == exit) are filtered out.
        for c in &report.crossing_costs {
            assert_ne!(c.entry, c.exit, "self-pair must be filtered out");
        }
    }

    /// Self-dirtying-loop fix (caught while implementing the P7-d cutover, not in
    /// the original plan text): applying a remote assignment must NOT re-flag
    /// `jobs_exports_dirty`, or the daily employment reconciliation -- gated on
    /// that same flag, so a quiet day skips it entirely -- would churn this very
    /// assignment on the next daily tick, permanently starving the citizen of
    /// salary every day forever (economy also only runs on the daily boundary).
    /// Regression guard for
    /// `regional_view_reports_city_goods_and_city_aware_inspect_notes`, which
    /// caught this at the full multi-region-gameplay level.
    #[test]
    fn apply_workplace_assignment_does_not_redirty_jobs_exports() {
        let mut region = RegionState::new(RegionId(1), 2, 2);
        let citizen = Entity::new(RegionId(1), 0);
        region.world.attach_citizen(
            citizen,
            crate::core::components::Citizen {
                id: citizen,
                age: 0,
                home: Entity::new(RegionId(1), 1),
                workplace_assignment: None,
                morale: crate::core::components::Morale {
                    actual: 0,
                    target: 0,
                    decay: 0,
                    rent_stress: 0,
                },
                money: 0,
                arrival_action: CitizenArrivalAction::ReturnHome,
                work_trip_generation: 0,
                attended_since_daily_settlement: false,
            },
        );
        // attach_citizen itself dirties the flag; clear it to isolate the
        // grant-application call under test.
        region.clear_jobs_exports_dirty();
        assert!(!region.world.is_jobs_exports_dirty());

        assert!(region.apply_workplace_assignment(
            citizen,
            WorkplaceAssignment {
                workplace: Entity::new(RegionId(2), 0),
                location: CityCellRef::local(RegionId(2), 0, 0),
                salary: 4,
            },
        ));

        assert!(
            region.world.citizens[&citizen]
                .workplace_assignment
                .is_some(),
            "the assignment should have been applied"
        );
        assert!(
            !region.world.is_jobs_exports_dirty(),
            "applying an assignment must not re-dirty jobs_exports_dirty, or the \
             daily employment reconciliation (gated on this same flag) would \
             churn this very assignment on the next daily tick, forever"
        );
    }
}
