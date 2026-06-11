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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Stable identity for one independently owned simulation region.
///
/// Future runtimes and workers will use this as a routing key. It is not an ECS
/// entity ID and should never identify another region's local `World` storage.
pub struct RegionId(pub u32);

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
        inspect_world(&self.world, x, y)
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

#[cfg(test)]
mod tests {
    #[test]
    fn regional_state_imports_shared_simulation_helpers_not_game_facade() {
        let source = std::fs::read_to_string("src/core/regions/mod.rs").expect("region source");
        let forbidden = ["crate::core::", "game"].concat();

        assert!(!source.contains(&forbidden));
        assert!(source.contains("crate::core::simulation"));
    }
}
