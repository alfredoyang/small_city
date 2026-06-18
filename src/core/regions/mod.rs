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

use crate::core::components::{Position, PowerSource, WorkplaceAssignment, WorkplaceSource};
use crate::core::entity::Entity;
use crate::core::resources::CityStats;
use crate::core::simulation::{
    TickJobPhase, TickPowerPhase, begin_tick_power_phase, continue_to_job_phase,
    ensure_derived_state, finish_tick_after_job_phase, refresh_derived_state_for_world,
};
use crate::core::systems::{build, bulldoze, economy, replace, road_connectivity, upgrade};
use crate::core::world::World;
use crate::interface::adapter::{inspect_world, view_world, view_world_with_overlay};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameView, InspectView};
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
/// Unlike power, the grant carries identity: the producer's `slot_id` (an opaque
/// owned id, not a remote ECS entity to the consumer) and the `salary` the home
/// region pays the worker. Workplace tax accrues to the exporting region instead.
pub struct JobExportGrant {
    pub token: u32,
    pub granted: bool,
    pub source_region: Option<RegionId>,
    pub position: Option<Position>,
    pub slot_id: Option<u32>,
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
        Self {
            id,
            world: World::new(width, height),
        }
    }

    pub fn id(&self) -> RegionId {
        self.id
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

    /// Number of local citizens currently working in another region (CR3 import).
    pub fn imported_job_count(&self) -> usize {
        self.world
            .citizens
            .values()
            .filter(|citizen| {
                matches!(
                    citizen
                        .workplace_assignment
                        .map(|assignment| assignment.source),
                    Some(WorkplaceSource::Remote { .. })
                )
            })
            .count()
    }

    #[cfg(test)]
    /// Owned `(source region, slot id)` pairs for citizens working remotely.
    ///
    /// This test-only summary verifies remote jobs store owned opaque ids, never a
    /// remote ECS entity. UI should use facade snapshots instead.
    pub(crate) fn imported_job_slots(&self) -> Vec<(RegionId, u32)> {
        let mut slots = self
            .world
            .citizens
            .values()
            .filter_map(|citizen| {
                let assignment = citizen.workplace_assignment?;
                match assignment.source {
                    WorkplaceSource::Remote { slot_id } => Some((assignment.region, slot_id)),
                    WorkplaceSource::Local { .. } => None,
                }
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
                    .map(|slot| slot.0)
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
        // G1 only wires the goods export phase. G2 will replace this empty demand
        // set with commercial storage needs that would otherwise hit the edge
        // market.
        RegionalTickGoodsPhase {
            phase: job_phase,
            goods_demands: Vec::new(),
        }
    }

    pub(crate) fn finish_tick_goods_demand_phase(
        &mut self,
        phase: RegionalTickGoodsPhase,
        exported_job_slots: &[Entity],
    ) -> CommandResult {
        self.finish_tick_job_demand_phase(phase.phase, exported_job_slots)
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
        let (Some(source_region), Some(position), Some(slot_id)) =
            (grant.source_region, grant.position, grant.slot_id)
        else {
            return;
        };
        let Some(citizen) = self.world.citizens.get_mut(&demand.citizen) else {
            return;
        };
        if citizen.workplace_assignment.is_some() {
            return;
        }
        citizen.workplace_assignment = Some(WorkplaceAssignment {
            region: source_region,
            position,
            salary: grant.salary,
            source: WorkplaceSource::Remote { slot_id },
        });
        self.world.invalidate_jobs_registry();
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
                source_region: Some(RegionId(2)),
                position: Some(Position { x: 1, y: 0 }),
                slot_id: Some(42),
                salary: 4,
            },
        );

        assert_eq!(region.imported_job_slots(), vec![(RegionId(2), 42)]);
        assert_eq!(region.world.cached_job_counts().unemployment, 0);
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
}
