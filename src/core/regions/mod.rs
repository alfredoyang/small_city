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
    HandoffKind, PendingHandoff, Position, PowerSource, TravelState, TravelToken, TravelerHandoff,
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
/// region; inner key = SOURCE region; value = `RouteHop` (min-cost next-hop
/// exits + the source's total road cost to T).
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
/// exits (r may have several, tied at the same cost), plus the total road
/// cost from r to T.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteHop {
    pub exits: Vec<ExitLink>,
    pub cost: u32,
}

/// P-?: one region's local answer for T — leave through this border link,
/// arriving in `to_region`. `link` is the local-side BorderLinkId; the
/// receiving region has the matching link on its side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExitLink {
    pub link: BorderLinkId,
    pub to_region: RegionId,
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

    /// P7c: advances movement by one 10-minute sub-tick (no economy). Driven 6×
    /// per game hour by the runner, separately from `tick_local`/the hourly tick.
    /// Buffers any cross-region crossings into `outgoing_handoffs` for the regions
    /// layer to drain (`drain_traveler_handoffs`).
    pub(crate) fn step_travel(&mut self) {
        travel::step_tokens(&mut self.world);
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

    /// P-a: per-region INPUT to the directory's Layer-1 Dijkstra. The region
    /// prices its own crossings (one Layer-2 Dijkstra per border-link pair) and
    /// publishes this report alongside the existing availability hint. The
    /// directory assembles all reports and runs the small Layer-1 Dijkstra on
    /// the region road graph.
    ///
    /// `border_neighbours` is the worker-supplied `BorderLinkId → neighbour
    /// RegionId` map (the direct-neighbour hint, provided by the worker from
    /// the topology). The region's own border-road cells come from
    /// `border_road_cells` per network; the cost from each entry to each exit
    /// on the same network is the Layer-2 Dijkstra distance.
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

    /// P-c: rebuild `remote_exit_cells` (FINAL target region → local exit
    /// road cells) from a `region_routes.exits_from(self.id)` map. The
    /// map's key is the FINAL target T; the value is the list of
    /// `ExitLink`s — local border `BorderLinkId`s r should use to begin
    /// a shortest path toward T. The route may be 1 hop (direct
    /// neighbour) or N hops (multi-hop); either way the first hop
    /// always starts at a local border, so we resolve each `BorderLinkId`
    /// to its local cells.
    ///
    /// `border_neighbor_map` is consulted as a fallback so a target with
    /// no published route (e.g. a brand-new neighbour whose road report
    /// hasn't landed yet) still gets the direct exit.
    pub(crate) fn set_region_routes(
        &mut self,
        exits_from: &std::collections::HashMap<RegionId, Vec<ExitLink>>,
    ) {
        // Build a quick index from BorderLinkId → local cells.
        let mut cells_by_link: HashMap<BorderLinkId, Vec<Entity>> = HashMap::new();
        for (cell, link) in self.border_road_links() {
            cells_by_link.entry(link).or_default().push(cell);
        }
        let mut map: HashMap<RegionId, Vec<Entity>> = HashMap::new();
        // Track every target the routes MENTIONED, even if no local cell
        // resolved (e.g. a multi-hop first hop whose BorderLinkId has no
        // local road yet). The fallback must skip these so the route's
        // choice (possibly a non-direct first hop) is not defeated.
        for (target, exits) in exits_from {
            for exit in exits {
                if let Some(cells) = cells_by_link.get(&exit.link) {
                    map.entry(*target)
                        .or_default()
                        .extend(cells.iter().copied());
                }
            }
        }
        // Fallback: a direct neighbour with NO entry in `exits_from` (no
        // route published yet) keeps the direct border exit so the mover
        // can still reach it.
        for (link, cells) in &cells_by_link {
            if let Some(&neighbor) = self.world.border_neighbor_map.get(link) {
                if !exits_from.contains_key(&neighbor) {
                    map.entry(neighbor)
                        .or_default()
                        .extend(cells.iter().copied());
                }
            }
        }
        for cells in map.values_mut() {
            cells.sort();
            cells.dedup();
        }
        self.world.remote_exit_cells = map;
    }

    /// P5b: drain this tick's buffered crossings into routed handoffs, resolving
    /// each border link from the topology. A `Move` whose exit cell no longer maps
    /// to a link toward its region is rolled back home (never strands the citizen).
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
                } => match self.exit_link_for(exit_cell, to_region) {
                    Some(entry_link) => handoffs.push(TravelerHandoff {
                        token,
                        traveler,
                        to_region,
                        entry_link: Some(entry_link),
                        kind: HandoffKind::Move,
                    }),
                    None => {
                        // The exit cell no longer faces `to_region` — the outbound
                        // can't route. Two cases:
                        //   - Home-side: the home just lost its border link (or
                        //     the link map went stale). Apply `apply_traveler_return`
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
                },
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
                        // Entry road gone (topology drift) — bounce a Rollback
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

    use crate::core::components::{PendingHandoff, TravelerId};

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
        // A's exits toward T=region 2: first hop is region 2 via East.
        let exits_from: HashMap<RegionId, Vec<ExitLink>> = HashMap::from([(
            RegionId(2),
            vec![ExitLink {
                link,
                to_region: RegionId(2),
            }],
        )]);
        a.set_region_routes(&exits_from);
        // The mover's `remote_exit_cells[target_region]` (FINAL target 2)
        // contains the local East-edge cell.
        assert_eq!(
            a.world.remote_exit_cells.get(&RegionId(2)),
            Some(&vec![exit])
        );
    }

    /// P-c fallback: when the routes field is empty (no report published
    /// yet for any neighbour), `set_region_routes` falls back to the
    /// direct `border_neighbor_map` so a fresh build still works.
    #[test]
    fn set_region_routes_falls_back_to_border_neighbor_map() {
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success); // East edge
        let exit = a.world.grid.get(1, 0).expect("road");
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        a.set_border_neighbor_map(HashMap::from([(link, RegionId(2))]));
        // Empty routes field: only the fallback path contributes.
        a.set_region_routes(&HashMap::new());
        assert_eq!(
            a.world.remote_exit_cells.get(&RegionId(2)),
            Some(&vec![exit])
        );
    }

    /// P-c routes take priority: when routes already cover a direct
    /// neighbour, the fallback does NOT inject the direct border cell
    /// (which would defeat the route's cheaper-corridor choice). The
    /// fallback is gap-fill only.
    #[test]
    fn set_region_routes_does_not_inject_direct_when_routes_cover() {
        use crate::core::regions::ExitLink;
        let mut a = RegionState::new(RegionId(1), 2, 1);
        assert!(a.build(1, 0, BuildingKind::Road).success); // East edge
        let direct = a.world.grid.get(1, 0).expect("road");
        // A south-edge road at a hypothetical detour exit (we'll fake its
        // BorderLinkId for the route even though the geometry wouldn't
        // actually produce one — the point is the route picks a non-East
        // cell and the fallback must respect that).
        let direct_link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let detour_link = BorderLinkId {
            edge: BorderEdge::South,
            offset: 0,
        };
        a.set_border_neighbor_map(HashMap::from([(direct_link, RegionId(2))]));
        // Routes pick a detour (South) toward target region 2.
        let exits_from: HashMap<RegionId, Vec<ExitLink>> = HashMap::from([(
            RegionId(2),
            vec![ExitLink {
                link: detour_link,
                to_region: RegionId(2),
            }],
        )]);
        a.set_region_routes(&exits_from);
        // Routes set target=2 → empty (the detour_link has no local cell).
        // The fallback MUST NOT then append the direct East cell. If it
        // did, the mover would take the direct (cost-suboptimal) route.
        let cells = a.world.remote_exit_cells.get(&RegionId(2));
        let has_direct = cells.is_some_and(|v| v.contains(&direct));
        assert!(
            !has_direct,
            "fallback must not inject the direct cell when routes cover the target"
        );
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
        a.set_border_neighbor_map(HashMap::from([(link, RegionId(2))]));

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
        // No border_neighbor_map entry → the exit faces nothing.
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
            },
        );
        a.world.away_residents.insert(citizen);
        a.world.away_generation.insert(citizen, 1);
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
            },
        );
        a.world.away_residents.insert(citizen);
        a.world.away_generation.insert(citizen, 1);

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
}
