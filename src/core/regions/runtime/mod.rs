//! Single-threaded regional runtime that processes owned events in FIFO order.
//!
//! This module introduces the actor-style shell around `RegionState` without
//! spawning OS threads or exposing ECS storage. Worker patches can later route
//! `OutboundMessage` values between runtimes.
//!
//! Cross-region power export allocation flow (retire-tickstate, P-a): the
//! tick no longer pauses for power. It fires the release/request and moves
//! straight on to the job phase; whichever reply lands later is applied to
//! local state whenever it arrives, for the *next* tick to see:
//!
//! ```text
//! Region A needs power          Coordinator routing        Region B has spare power
//! --------------------          --------------             ------------------------
//! Tick starts
//!   |
//!   v
//! Run local power
//!   |
//!   v
//! Emit allocation release + export request (fire-and-forget)
//!   |
//!   v
//! Continue this SAME pass into the job phase --> tick completed
//!                              Route release/request directly
//!                                      |
//!                                      v
//!                                                        Check spare power
//!                                                        minus active allocations
//!                                                              |
//!                                                              v
//!                                                  grant or deny whole consumer
//!                                                              |
//!                                                              v
//!                              Route grant back to Region A
//!         |
//!         v
//! Apply grant if it matches the current batch (current_power_request_id);
//! a superseded batch's reply is dropped (and released back to the producer
//! if it arrived granted). No effect on THIS tick -- picked up by whichever
//! tick runs next.
//! ```
//!
//! Export allocations are transient runtime coordination owned by the producer.
//! They prevent double-spending producer capacity during one scheduling round:
//!
//! ```text
//! Producer Region B spare power = 1
//!
//! A request granted:
//!   allocation (caller=A, request=10, token=0) -> demand 1
//!
//! C request in same round:
//!   available = spare 1 - allocated 1 = 0
//!   deny C
//!
//! Next A tick:
//!   A emits release(request=11) and a new request
//!   B's request handling drops A's stale allocation before measuring capacity
//! ```
//!
//! Topology is the gate for sharing power. Border road networks can only join
//! when the regional layout says the two edges are neighbors:
//!
//! ```text
//! Region A east border road      Region B west border road
//!         |                               |
//!         +-- East:offset 0 matches -----+
//!                  West:offset 0
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;

#[cfg(test)]
use crate::core::city_refs::CityCellRef;
use crate::core::components::{GoodsOrderId, PlaceRef, TravelerHandoff, TravelerId};
use crate::core::entity::Entity;
use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionCommandResponse, RegionSnapshotResponse,
    RegionTickResponse, RegionViewSnapshot, UiRequestId,
};
use crate::core::regions::coordinator::{RegionRecipients, RoutedRegionEvent};
use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::employment_directory::{
    CitizenRef, EmploymentDirectory, EmploymentLeaseRef, JobClaimDecision, JobLoss, JobLossReason,
    choose_best_pool,
};
use crate::core::regions::handle::{RegionEventReceiver, RegionHandle, mailbox};
use crate::core::regions::worker::{
    cross_region_goods_routes_for_region, importable_remote_jobs_for_region,
};
use crate::core::regions::{
    ExitLink, ExportAllocationKey, GOODS_PER_TRUCK, GoodsSupplyGrant, PendingGoodsDemand,
    PendingPowerDemand, PowerExportGrant, RegionId, RegionRoadNetworkId, RegionState,
    RegionalTickGoodsPhase, RegionalTickJobPhase, RegionalTickPowerPhase, denied_goods_grant,
};
use crate::core::systems::economy;
use crate::core::world::CrossRegionGoodsRoutes;
use crate::interface::input::MapOverlayInput;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// Monotonic city-wide travel sub-tick identity.
pub struct TravelStepId(pub u64);

impl TravelStepId {
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(Debug, Clone)]
/// Event owned by one region runtime inbox.
pub enum RegionEvent {
    /// Advance this region's local deterministic simulation by one tick.
    Tick { request_id: UiRequestId },
    /// Build an owned UI-safe snapshot through the region event loop.
    BuildSnapshot {
        request_id: UiRequestId,
        overlay: MapOverlayInput,
    },
    /// Build one UI-safe inspection view on the owning region worker.
    InspectRegion {
        request_id: UiRequestId,
        x: usize,
        y: usize,
    },
    /// Build one UI-safe road traveler panel seed on the owning worker.
    RoadTravelerPanelSeed {
        request_id: UiRequestId,
        x: usize,
        y: usize,
    },
    /// Resolve the anchor for a UI-selected building footprint cell.
    BuildingAnchorAt {
        request_id: UiRequestId,
        x: usize,
        y: usize,
    },
    /// Return this home region's workers for one producer workplace.
    RemoteWorkersFor {
        request_id: UiRequestId,
        producer_region: RegionId,
        pos: crate::core::components::Position,
    },
    /// Run one player command through this region's local event loop.
    RunCommand {
        request_id: UiRequestId,
        command: RegionCommand,
    },
    /// Time-neutral loader refresh for imported power.
    SettlePowerImports { request_id: UiRequestId },
    /// Authoritative producer-side power export allocation request.
    ProcessPowerExportRequest(PowerExportAllocationRequest),
    /// Producer-side release for a caller's previous power export allocations.
    ReleasePowerExportAllocations(PowerExportAllocationRelease),
    /// Caller-side power export grant result.
    ///
    /// Retire-tickstate, P-a: the reply carries the request it answers (the
    /// worker already holds it at the moment it routes the result back), so
    /// the caller needs no continuation to remember what the token meant.
    ApplyPowerExportGrant {
        request: PowerExportAllocationRequest,
        grant: PowerExportGrant,
    },
    /// Authoritative producer-side goods supply allocation request.
    ProcessGoodsSupplyRequest(GoodsSupplyAllocationRequest),
    /// Producer-side release for a caller's previous goods supply allocations.
    ReleaseGoodsSupplyAllocations(GoodsSupplyAllocationRelease),
    /// Caller-side goods supply grant result.
    ///
    /// Retire-tickstate, P-d: the reply carries the request it answers,
    /// same shape as power/jobs.
    ApplyGoodsSupplyGrant {
        request: GoodsSupplyAllocationRequest,
        grant: GoodsSupplyGrant,
    },
    /// P5b: a cross-region travel token handed in by a neighbor region (fire and
    /// forget — no grant, no tick pause).
    ReceiveTraveler {
        eligible_step: TravelStepId,
        handoff: TravelerHandoff,
    },
    /// Retire-tickstate, P-b: the eager nudge. Fired at a neighbor the
    /// instant this region's availability hint actually changes (worker's
    /// P-1 hint-publish sweep), instead of waiting for that neighbor's own
    /// next tick to notice via the discovery-generation gate. Fire and
    /// forget, modeled on `ReceiveTraveler` — no grant, no tick pause; it
    /// only ever makes the *common* case faster, never changes the
    /// gate-guaranteed worst case.
    PowerCapacityRecheck {
        request_id: UiRequestId,
        source_region: RegionId,
    },
    /// P7c: advance movement by one 10-minute sub-tick (no economy). Broadcast to
    /// every region by the runner's `step_travel_city`; emits the crossings it
    /// buffers as `TravelerHandedOff` for the coordinator to route.
    StepTravel { step: TravelStepId },
    /// P1: a token host reports that a citizen reached its work endpoint. The
    /// home region validates its own assignment and trip-purpose state.
    DestinationArrived {
        traveler: TravelerId,
        destination: PlaceRef,
    },
    /// P2 goods trucks: the factory accepted a truck arrival at a commercial
    /// endpoint and asks that commercial owner to apply the cargo.
    ApplyGoodsDelivery {
        traveler: TravelerId,
        order: GoodsOrderId,
        allocation_key: ExportAllocationKey,
        commercial: Entity,
        units: i32,
    },
    /// P2 goods trucks: the commercial owner applied the cargo, so the factory can
    /// consume warehouse reservation and let the truck return home.
    ConfirmGoodsDelivery {
        traveler: TravelerId,
        order: GoodsOrderId,
        allocation_key: ExportAllocationKey,
        units: i32,
    },
    /// P2 goods trucks: the commercial owner could not apply the cargo. The
    /// factory frees its reservation and the existing travel token returns home.
    RejectGoodsDelivery {
        traveler: TravelerId,
        order: GoodsOrderId,
        allocation_key: ExportAllocationKey,
        units: i32,
    },
    /// Directory employment ledger plan, P3: a payload-free wake.
    ///
    /// It carries no claims, contracts, or losses — it only tells the region
    /// to *pull* whatever employment work the directory holds for it. That
    /// keeps the directory the single coordination source and avoids polling
    /// every region each tick.
    EmploymentDirectoryReady,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Consumer request for a producer to export enough power for one local demand.
pub struct PowerExportRequest {
    pub request_id: UiRequestId,
    pub caller_region: RegionId,
    pub caller_network: RegionRoadNetworkId,
    pub token: u32,
    pub demand: i32,
    /// Retire-tickstate, P-a: echoed back by the grant reply so the caller
    /// can re-derive the demand it answers without keeping a demand list.
    pub consumer: Entity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Consumer request for a producer to supply a whole batch of goods units.
pub struct GoodsSupplyRequest {
    pub request_id: UiRequestId,
    pub caller_region: RegionId,
    pub caller_network: RegionRoadNetworkId,
    pub token: u32,
    pub units: u32,
    /// Retire-tickstate, P-d: echoed back by the grant reply so the caller
    /// can stage the goods without keeping a demand list.
    pub commercial: Entity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Producer-side allocation request: one consumer request plus the remaining
/// candidate producer networks to try if a stale availability hint denies it.
///
/// Generic over the resource's request payload `R` (power demand vs job seeker);
/// the candidate-walk transport around it is identical for both (see CR3R).
pub struct ExportAllocationRequest<R> {
    pub request: R,
    pub candidates: Vec<RegionRoadNetworkId>,
    pub candidate_index: usize,
}

/// Producer-side power allocation request (one consumer request + candidates).
pub type PowerExportAllocationRequest = ExportAllocationRequest<PowerExportRequest>;
/// Producer-side goods supply allocation request (one consumer request + candidates).
pub type GoodsSupplyAllocationRequest = ExportAllocationRequest<GoodsSupplyRequest>;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Producer-side marker that a caller started a new tick export-resolution
/// generation, so producers may drop the caller's prior allocations.
///
/// Shared by power and jobs: the release shape and routing are identical (CR3R).
pub struct ExportAllocationRelease {
    pub caller_region: RegionId,
    pub request_id: UiRequestId,
    /// Producers that granted this caller in the previous generation.
    ///
    /// M3 routes release only to known producers instead of broadcasting to every
    /// region. The set is transient runtime coordination and is intentionally not
    /// saved.
    pub producer_regions: Vec<RegionId>,
}

/// Power flavor of the shared export allocation release.
pub type PowerExportAllocationRelease = ExportAllocationRelease;
/// Goods supply flavor of the shared export allocation release.
pub type GoodsSupplyAllocationRelease = ExportAllocationRelease;

/// A consumer export request that can name the producer-side reservation key it
/// belongs to. Both power and job requests carry the same `(caller, gen, token)`.
trait ExportRequestKey {
    fn allocation_key(&self) -> ExportAllocationKey;
}

impl ExportRequestKey for PowerExportRequest {
    fn allocation_key(&self) -> ExportAllocationKey {
        ExportAllocationKey {
            caller_region: self.caller_region,
            request_id: self.request_id,
            token: self.token,
        }
    }
}

impl ExportRequestKey for GoodsSupplyRequest {
    fn allocation_key(&self) -> ExportAllocationKey {
        ExportAllocationKey {
            caller_region: self.caller_region,
            request_id: self.request_id,
            token: self.token,
        }
    }
}

fn export_allocation_key<R: ExportRequestKey>(request: &R) -> ExportAllocationKey {
    request.allocation_key()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic runtime error returned as outbound routing feedback.
pub enum RegionRuntimeError {
    /// A continuation arrived at a region other than its caller region.
    ContinuationRoutedToWrongRegion {
        expected_region: RegionId,
        actual_region: RegionId,
    },
}

#[derive(Debug)]
/// Message returned by a runtime for the caller or worker to route.
pub enum OutboundMessage {
    RegionCommandCompleted(RegionCommandResponse),
    RegionTickCompleted(RegionTickResponse),
    RegionSnapshotReady(RegionSnapshotResponse),
    RegionInspectReady {
        request_id: UiRequestId,
        region_id: RegionId,
        inspect: crate::interface::view::InspectView,
    },
    RoadTravelerPanelSeedReady {
        request_id: UiRequestId,
        region_id: RegionId,
        seed: crate::interface::view::RoadTravelerPanelSeedView,
    },
    BuildingAnchorReady {
        request_id: UiRequestId,
        region_id: RegionId,
        anchor: Option<crate::core::components::Position>,
    },
    RemoteWorkersReady {
        request_id: UiRequestId,
        region_id: RegionId,
        workers: Vec<crate::interface::view::CitizenDetailView>,
    },
    PowerImportsSettled {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    /// P5b: a travel token to route to `handoff.to_region` (worker delivers it as
    /// `RegionEvent::ReceiveTraveler`).
    /// A target-bearing fire-and-forget delivery owned by the coordinator.
    CoordinatorRoute(RoutedRegionEvent),
    /// Directory employment ledger plan, P3: wake `target_region` so it pulls
    /// its employment work from the directory. Payload-free by design; the
    /// worker delivers it as `RegionEvent::EmploymentDirectoryReady`.
    RuntimeError(RegionRuntimeError),
}

#[derive(Debug)]
/// UI-safe reply produced by one runtime event and forwarded through the runner.
pub enum RuntimeReply {
    Inspect {
        request_id: UiRequestId,
        region_id: RegionId,
        inspect: Box<crate::interface::view::InspectView>,
    },
    RoadTravelerPanelSeed {
        request_id: UiRequestId,
        region_id: RegionId,
        seed: crate::interface::view::RoadTravelerPanelSeedView,
    },
    BuildingAnchor {
        request_id: UiRequestId,
        region_id: RegionId,
        anchor: Option<crate::core::components::Position>,
    },
    RemoteWorkers {
        request_id: UiRequestId,
        region_id: RegionId,
        workers: Vec<crate::interface::view::CitizenDetailView>,
    },
    PowerImportsSettled {
        request_id: UiRequestId,
        region_id: RegionId,
    },
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    power_export_allocations: ExportAllocations<i32>,
    goods_supply_allocations: ExportAllocations<u32>,
    power_export_producers: Vec<RegionId>,
    goods_supply_producers: Vec<RegionId>,
    pending_goods_stock: Vec<(Entity, u32)>,
    // Retire-tickstate, P-a: the only caller-side memory power needs now that
    // nothing pauses. A reply echoes the request it answers (its own
    // `request_id`), so comparing against this one scalar tells "my current
    // batch" from "a superseded one" -- no continuation, no demand list.
    // Starts at UiRequestId(0), a sentinel no real UI-driven tick ever mints
    // (RegionalGame's counter starts at 1), so the very first reply this
    // runtime ever sees compares correctly.
    current_power_request_id: UiRequestId,
    // Retire-tickstate, P-d: same shape and same reasoning, for goods.
    current_goods_request_id: UiRequestId,
    // Event-driven plan (docs/20260703-event-driven-architecture.md), P-2: the
    // directory snapshot generation as of this worker pass's per-slice install
    // (`set_discovery_generation`, mirrors `set_region_routes`), and the
    // generation this region last reconciled its power exports against. Both
    // start at 0 so a fresh/loaded runtime always reconciles once — matching
    // the "load = all-dirty" decision `hints_dirty` already follows. Neither
    // is serialized (`RegionRuntime` never is).
    discovery_generation: u64,
    seen_power_generation: u64,
    // Directory employment ledger plan, P7-d: the connectivity fingerprint this
    // region last reconciled cross-region employment against. The daily gate
    // fires when the installed discovery's fingerprint differs from this — a
    // connectivity change (road/link/topology), NOT hint-value noise. Replaces
    // the raw `discovery_generation` comparison for the job gate (P7-b). Starts
    // at 0 so a fresh/loaded runtime reconciles once.
    seen_connectivity_fingerprint: u64,
    // Event-driven plan, P-5: same shape, for the goods reconcile gate.
    seen_goods_generation: u64,
    // Directory employment ledger plan, P3: installed once per worker slice
    // (`set_employment_directory`), mirroring how `set_discovery_generation`
    // and `set_region_routes` hand this runtime the pass's shared data. `None`
    // until a worker installs one, so a bare `RegionRuntime::new` still works
    // and an `EmploymentDirectoryReady` without a directory is a no-op.
    employment_directory: Option<Arc<EmploymentDirectory>>,
    // Directory employment ledger plan, P7-c: the full discovery snapshot for
    // this slice, installed alongside `discovery_generation`
    // (`set_discovery_snapshot`). `discovery_generation` alone answers "did
    // anything change"; the daily employment phase (P7-d) needs the component
    // graph itself to re-check contract reachability. `None` until a worker
    // installs one.
    discovery: Option<Arc<CrossRegionDiscovery>>,
    // A crossing is transport only until the next travel sub-tick. Keeping this
    // transient preserves the existing travel save/rebuild rule.
    pending_traveler_handoffs: Vec<(TravelStepId, TravelerHandoff)>,
    last_travel_step: Option<TravelStepId>,
    handle: RegionHandle,
    receiver: RegionEventReceiver,
}

// CR3R — one reservation engine shared by power and goods.
//
// Power and goods once carried byte-for-byte copies of the producer-side
// reservation bookkeeping (jobs shared it too until the employment ledger
// replaced that path — P8). CR3R keeps ONE generic engine and varies only the
// reserved unit `U`: power reserves an `i32` demand, goods reserve a `u32` batch.
// The transport/lifecycle (keying, staleness, upsert) is identical; only "how do
// I read available capacity out of these units" stays resource-specific.
//
//                    ExportAllocations<U>                 (one engine, two U's)
//   ┌───────────────────────────────────────────────────────────────────────┐
//   │ Vec<ExportAllocation<U>>                                                │
//   │   each = { key: ExportAllocationKey,   ← (caller_region, gen, token)    │
//   │           network: RegionRoadNetworkId,                                 │
//   │           unit: U,                     ← i32 (power) | u32 (goods)       │
//   │           caller_generation }                                           │
//   │                                                                         │
//   │ shared lifecycle:  upsert · release_stale_for_caller                    │
//   │ shared read:       reserved_units_excluding(key, network)               │
//   └───────────────────────────────────────────────────────────────────────┘
//          ▲ instantiated as                          ▲ instantiated as
//          power_export_allocations:  ExportAllocations<i32>
//          goods_supply_allocations:  ExportAllocations<u32>
//
//   resource-specific (NOT shared) — how units become "available capacity":
//     power → sum reserved demand, subtract from per-network remaining
//     goods → sum reserved units,  subtract from per-network free capacity
//
// Both are POOLED PER NETWORK, so `reserved_units_excluding` is network-scoped: a
// reservation on one network must not shrink another's remaining capacity.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One producer-owned transient reservation of a reserved unit `U` on a network.
///
/// `U` is the resource's reserved unit: `i32` demand for power, `u32` units for
/// goods. `caller_generation` lets a producer drop a caller's reservations once
/// the caller starts a new tick generation.
struct ExportAllocation<U> {
    key: ExportAllocationKey,
    network: RegionRoadNetworkId,
    unit: U,
    caller_generation: UiRequestId,
}

#[derive(Debug)]
/// Producer-side reservation bookkeeping shared by power and goods (CR3R).
///
/// This carries only the transport/lifecycle that both resources share. How
/// available capacity is computed from the reserved units stays resource-specific
/// (a per-network remaining amount for either), but both read it network-scoped
/// via `reserved_units_excluding`.
struct ExportAllocations<U> {
    allocations: Vec<ExportAllocation<U>>,
}

impl<U: Copy> ExportAllocations<U> {
    fn new() -> Self {
        Self {
            allocations: Vec::new(),
        }
    }

    /// Drops a caller's reservations once it starts a new tick generation.
    fn release_stale_for_caller(
        &mut self,
        caller_region: RegionId,
        caller_generation: UiRequestId,
    ) {
        self.allocations.retain(|allocation| {
            allocation.key.caller_region != caller_region
                || allocation.caller_generation == caller_generation
        });
    }

    /// Reserved units on one network held by callers other than `key`.
    ///
    /// Power uses this network-scoped view: capacity is pooled per road network, so
    /// a reservation on one network must not reduce another's remaining capacity.
    /// Excluding `key` lets a caller's own retry re-grant.
    fn reserved_units_excluding(
        &self,
        key: ExportAllocationKey,
        network: RegionRoadNetworkId,
    ) -> impl Iterator<Item = U> + '_ {
        self.allocations
            .iter()
            .filter(move |allocation| allocation.network == network && allocation.key != key)
            .map(|allocation| allocation.unit)
    }

    /// Inserts or refreshes the reservation identified by `key`.
    fn upsert(
        &mut self,
        key: ExportAllocationKey,
        network: RegionRoadNetworkId,
        unit: U,
        caller_generation: UiRequestId,
    ) {
        if let Some(allocation) = self
            .allocations
            .iter_mut()
            .find(|allocation| allocation.key == key)
        {
            allocation.network = network;
            allocation.unit = unit;
            allocation.caller_generation = caller_generation;
        } else {
            self.allocations.push(ExportAllocation {
                key,
                network,
                unit,
                caller_generation,
            });
        }
    }

    /// The reserved unit of every active reservation, in insertion order.
    fn units(&self) -> impl Iterator<Item = U> + '_ {
        self.allocations.iter().map(|allocation| allocation.unit)
    }
}

fn insert_sorted_unique(regions: &mut Vec<RegionId>, region: RegionId) {
    match regions.binary_search(&region) {
        Ok(_) => {}
        Err(index) => regions.insert(index, region),
    }
}

impl RegionRuntime {
    /// Creates a runtime that owns one region and an empty inbox.
    pub fn new(state: RegionState) -> Self {
        let (handle, receiver) = mailbox(state.id());
        Self {
            state,
            power_export_allocations: ExportAllocations::new(),
            goods_supply_allocations: ExportAllocations::new(),
            power_export_producers: Vec::new(),
            goods_supply_producers: Vec::new(),
            pending_goods_stock: Vec::new(),
            current_power_request_id: UiRequestId(0),
            current_goods_request_id: UiRequestId(0),
            discovery_generation: 0,
            seen_power_generation: 0,
            seen_connectivity_fingerprint: 0,
            seen_goods_generation: 0,
            employment_directory: None,
            discovery: None,
            pending_traveler_handoffs: Vec::new(),
            last_travel_step: None,
            handle,
            receiver,
        }
    }

    pub fn region_id(&self) -> RegionId {
        self.state.id()
    }

    pub fn state(&self) -> &RegionState {
        &self.state
    }

    /// P3: the employer side of claim validation must record a contract in its
    /// own `RegionState`, so it needs mutable access. Still `pub(crate)` — the
    /// UI never reaches a `RegionRuntime`.
    pub(crate) fn state_mut(&mut self) -> &mut RegionState {
        &mut self.state
    }

    /// Directory employment ledger plan, P3: install this pass's shared
    /// employment directory, mirroring `set_discovery_generation`'s per-slice
    /// install. The worker owns the `Arc`; every runtime it schedules gets a
    /// clone before its events are processed.
    pub(crate) fn set_employment_directory(&mut self, directory: Arc<EmploymentDirectory>) {
        self.employment_directory = Some(directory);
    }

    pub(crate) fn set_importable_remote_jobs(&mut self, jobs: i32) {
        self.state.set_importable_remote_jobs(jobs);
    }

    /// Event-driven plan, P-2: install this pass's directory snapshot
    /// generation (mirrors `set_region_routes`'s per-slice install). Read by
    /// the power reconcile gate in `start_tick_power_phase` to decide whether
    /// a cross-region change happened since this region's last reconcile.
    pub(crate) fn set_discovery_generation(&mut self, generation: u64) {
        self.discovery_generation = generation;
    }

    /// P7-c: install this pass's full discovery snapshot (the component graph),
    /// installed by the worker alongside `set_discovery_generation`. The daily
    /// employment phase (P7-d) reads it to re-check contract reachability.
    pub(crate) fn set_discovery_snapshot(&mut self, discovery: Arc<CrossRegionDiscovery>) {
        self.discovery = Some(discovery);
    }

    /// P7-c: the installed discovery snapshot, if any. `None` on a bare
    /// `RegionRuntime::new` or before a worker slice has run.
    #[allow(dead_code)] // P7-c: installed here; P7-d's daily phase reads it.
    pub(crate) fn discovery_snapshot(&self) -> Option<&CrossRegionDiscovery> {
        self.discovery.as_deref()
    }

    /// P-c: install this region's slice of the directory's `region_routes`
    /// (its `exits_from(self.id)` map — for every reachable target T, the
    /// first-hop exits r should use) and rebuild the multi-hop
    /// `remote_exit_cells` from it.
    pub(crate) fn set_region_routes(
        &mut self,
        exits_from: &std::collections::HashMap<RegionId, Vec<ExitLink>>,
    ) {
        self.state.set_region_routes(exits_from);
    }

    pub(crate) fn set_cross_region_goods_routes(&mut self, routes: CrossRegionGoodsRoutes) {
        self.state.set_cross_region_goods_routes(routes);
    }

    fn refresh_inspect_routes(&mut self) {
        let Some(discovery) = self.discovery.clone() else {
            return;
        };
        let border_links = self.state.network_border_links();
        let jobs = importable_remote_jobs_for_region(&discovery, self.region_id(), &border_links);
        let goods =
            cross_region_goods_routes_for_region(&discovery, self.region_id(), &border_links);
        self.set_importable_remote_jobs(jobs);
        self.set_cross_region_goods_routes(goods);
    }

    /// Inspects one cell, recomputing the derived pass first if a paused command
    /// left it dirty (DT1), so inspect reflects the latest config like the view.
    pub fn inspect(&mut self, x: usize, y: usize) -> crate::interface::view::InspectView {
        self.ensure_derived_state();
        self.state.inspect(x, y)
    }

    /// Enter-panel detail for the travelers on a road cell, recomputing the
    /// derived pass first (DT1), mirroring `inspect`.
    pub fn road_traveler_panel_seed(
        &mut self,
        x: usize,
        y: usize,
    ) -> crate::interface::view::RoadTravelerPanelSeedView {
        self.ensure_derived_state();
        self.state.road_traveler_panel_seed(x, y)
    }

    /// Residents of THIS region who commute to `(producer_region, pos)` in another
    /// region — the consumer half of a workplace's remote roster.
    ///
    /// Recomputes the derived pass first (DT1), mirroring `inspect`, for a uniform
    /// `&mut` read boundary. Remote assignments are set by the daily employment
    /// phase, not the derived pass, so this is a no-op for the returned data today; it stays
    /// for consistency and future-proofing and is bounded (called only on panel open).
    pub fn remote_workers_for(
        &mut self,
        producer_region: RegionId,
        pos: crate::core::components::Position,
    ) -> Vec<crate::interface::view::CitizenDetailView> {
        self.ensure_derived_state();
        self.state.remote_workers_for(producer_region, pos)
    }

    /// Anchor `Position` of the building occupying `(x, y)` (see
    /// `RegionState::building_anchor_at`). Pure config read — building footprints
    /// are not part of the derived pass, so no DT1 recompute is needed.
    pub fn building_anchor_at(
        &self,
        x: usize,
        y: usize,
    ) -> Option<crate::core::components::Position> {
        self.state.building_anchor_at(x, y)
    }

    /// Recomputes the derived pass if a paused command left it dirty (DT1).
    ///
    /// The worker calls this after a scheduling slice and before reading the
    /// region's published summaries (border links, availability hints), which
    /// depend on derived state (effective workplaces gate on applied power).
    ///
    /// Safe to call mid-tick: `derived_dirty` is only set by out-of-tick commands
    /// (see `World::mark_derived_dirty`), so a paused cross-region tick leaves it
    /// false and this is a no-op -- it never re-runs `power::run` to wipe in-flight
    /// imported power.
    pub(crate) fn ensure_derived_state(&mut self) {
        self.state.ensure_derived_state();
    }

    pub(crate) fn into_state(self) -> RegionState {
        self.state
    }

    pub fn handle(&self) -> RegionHandle {
        self.handle.clone()
    }

    pub fn send_to_region(&self, target: &RegionHandle, event: RegionEvent) {
        target.send(event);
    }

    /// Adds one event to the back of this region's FIFO inbox.
    pub fn push_event(&mut self, event: RegionEvent) {
        self.receiver.push_event(event);
    }

    pub fn pending_event_count(&self) -> usize {
        self.receiver.pending_event_count()
    }

    /// Processes the next inbox event, returning messages for external routing.
    pub fn process_next_event(&mut self) -> Vec<OutboundMessage> {
        let Some(event) = self.pop_next_runnable_event() else {
            return Vec::new();
        };

        self.process_event(event)
    }

    /// Processes up to `max_events` events from this region's inbox.
    pub fn process_some_events(&mut self, max_events: usize) -> Vec<OutboundMessage> {
        let mut outbound = Vec::new();

        for _ in 0..max_events {
            if self.pending_event_count() == 0 {
                break;
            }
            outbound.extend(self.process_next_event());
        }

        outbound
    }

    fn process_event(&mut self, event: RegionEvent) -> Vec<OutboundMessage> {
        match event {
            RegionEvent::Tick { request_id } => self.start_tick_power_phase(request_id),
            RegionEvent::BuildSnapshot {
                request_id,
                overlay,
            } => {
                vec![OutboundMessage::RegionSnapshotReady(
                    self.build_snapshot(request_id, overlay),
                )]
            }
            RegionEvent::InspectRegion { request_id, x, y } => {
                self.refresh_inspect_routes();
                vec![OutboundMessage::RegionInspectReady {
                    request_id,
                    region_id: self.region_id(),
                    inspect: self.inspect(x, y),
                }]
            }
            RegionEvent::RoadTravelerPanelSeed { request_id, x, y } => {
                vec![OutboundMessage::RoadTravelerPanelSeedReady {
                    request_id,
                    region_id: self.region_id(),
                    seed: self.road_traveler_panel_seed(x, y),
                }]
            }
            RegionEvent::BuildingAnchorAt { request_id, x, y } => {
                vec![OutboundMessage::BuildingAnchorReady {
                    request_id,
                    region_id: self.region_id(),
                    anchor: self.building_anchor_at(x, y),
                }]
            }
            RegionEvent::RemoteWorkersFor {
                request_id,
                producer_region,
                pos,
            } => vec![OutboundMessage::RemoteWorkersReady {
                request_id,
                region_id: self.region_id(),
                workers: self.remote_workers_for(producer_region, pos),
            }],
            RegionEvent::RunCommand {
                request_id,
                command,
            } => {
                let response = self.run_command(request_id, command);
                vec![OutboundMessage::RegionCommandCompleted(response)]
            }
            RegionEvent::SettlePowerImports { request_id } => {
                let mut outbound = self.start_power_import_settlement(request_id);
                outbound.push(OutboundMessage::PowerImportsSettled {
                    request_id,
                    region_id: self.region_id(),
                });
                outbound
            }
            RegionEvent::ProcessPowerExportRequest(request) => {
                let grant = self.process_power_export_request(&request);
                vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(request.request.caller_region),
                    event: RegionEvent::ApplyPowerExportGrant { request, grant },
                })]
            }
            RegionEvent::ReleasePowerExportAllocations(release) => {
                self.power_export_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyPowerExportGrant { request, grant } => {
                self.apply_power_export_result(request, grant)
            }
            RegionEvent::ProcessGoodsSupplyRequest(request) => {
                let grant = self.process_goods_supply_request(&request);
                vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(request.request.caller_region),
                    event: RegionEvent::ApplyGoodsSupplyGrant { request, grant },
                })]
            }
            RegionEvent::ReleaseGoodsSupplyAllocations(release) => {
                self.goods_supply_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyGoodsSupplyGrant { request, grant } => {
                self.apply_goods_supply_result(request, grant)
            }
            RegionEvent::ReceiveTraveler {
                eligible_step,
                handoff,
            } => {
                self.pending_traveler_handoffs
                    .push((eligible_step, handoff));
                Vec::new()
            }
            RegionEvent::PowerCapacityRecheck { request_id, .. } => {
                self.power_capacity_recheck(request_id)
            }
            RegionEvent::StepTravel { step } => {
                if self.last_travel_step.is_some_and(|last| step <= last) {
                    return Vec::new();
                }
                self.last_travel_step = Some(step);
                let inbound = self.take_handoffs_eligible_at(step);
                let mut outbound = Vec::new();
                for handoff in inbound {
                    outbound.extend(
                        self.state
                            .receive_traveler_handoff(handoff)
                            .into_iter()
                            .map(|handoff| self.traveler_handoff_route(step, handoff)),
                    );
                }
                self.state.step_travel();
                outbound.extend(self.route_traveler_handoffs(step));
                outbound.extend(self.route_destination_arrivals());
                outbound
            }
            RegionEvent::DestinationArrived {
                traveler,
                destination,
            } => {
                if let Some(shipment) = self
                    .state
                    .validate_goods_truck_arrival(traveler, destination)
                {
                    return vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                        recipients: RegionRecipients::One(shipment.commercial.region),
                        event: RegionEvent::ApplyGoodsDelivery {
                            traveler,
                            order: shipment.order,
                            allocation_key: shipment.allocation_key,
                            commercial: shipment.commercial.building,
                            units: shipment.units,
                        },
                    })];
                }
                self.state.apply_destination_arrived(traveler, destination);
                Vec::new()
            }
            RegionEvent::ApplyGoodsDelivery {
                traveler,
                order,
                allocation_key,
                commercial,
                units,
            } => {
                let applied = commercial == order.commercial
                    && self.state.apply_goods_delivery(traveler, order, units);
                let event = if applied {
                    RegionEvent::ConfirmGoodsDelivery {
                        traveler,
                        order,
                        allocation_key,
                        units,
                    }
                } else {
                    RegionEvent::RejectGoodsDelivery {
                        traveler,
                        order,
                        allocation_key,
                        units,
                    }
                };
                vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(traveler.entity.region()),
                    event,
                })]
            }
            RegionEvent::ConfirmGoodsDelivery {
                traveler,
                order: _,
                allocation_key: _,
                units,
            } => {
                self.state.confirm_goods_delivery(traveler, units);
                Vec::new()
            }
            RegionEvent::RejectGoodsDelivery {
                traveler,
                order: _,
                allocation_key: _,
                units,
            } => {
                self.state.cancel_goods_delivery(traveler, units);
                Vec::new()
            }
            RegionEvent::EmploymentDirectoryReady => self.handle_employment_directory_ready(),
        }
    }

    /// P3/P4/P5: pull whatever employment work the directory holds for this
    /// region. This is now the plan's full four-call handler.
    ///
    /// One region can be both an employer and a home, so all four run. The order
    /// is the plan's, and it matters: employer-side work settles this pass's
    /// accepts and releases into the directory *before* the home-side work reads
    /// the accepted cache and the loss queue.
    ///
    /// ```text
    ///   employer_validate_claims        accept/reject pending claims
    ///   employer_apply_releases         free seats the home gave back
    ///   home_apply_accepted_employment  write Citizen.workplace_assignment
    ///   home_apply_losses               clear assignments the employer lost
    /// ```
    ///
    /// A runtime with no directory installed (a bare `RegionRuntime::new`, or
    /// a worker that never set one) treats the wake as a no-op rather than
    /// panicking.
    fn handle_employment_directory_ready(&mut self) -> Vec<OutboundMessage> {
        let Some(directory) = self.employment_directory.clone() else {
            return Vec::new();
        };
        let mut outbound = employer_validate_claims(self, &directory);
        employer_apply_releases(self, &directory);
        outbound.extend(home_apply_accepted_employment(self, &directory));
        home_apply_losses(self, &directory);
        outbound
    }

    /// P3: the wake fan-out. One payload-free message per target region; the
    /// coordinator routes them through the same event path as every other
    /// cross-region event.
    fn emit_employment_directory_ready(&self, regions: Vec<RegionId>) -> Vec<OutboundMessage> {
        regions
            .into_iter()
            .map(|target_region| {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(target_region),
                    event: RegionEvent::EmploymentDirectoryReady,
                })
            })
            .collect()
    }

    fn take_handoffs_eligible_at(&mut self, step: TravelStepId) -> Vec<TravelerHandoff> {
        let pending = std::mem::take(&mut self.pending_traveler_handoffs);
        let (ready, future): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .partition(|(eligible, _)| *eligible <= step);
        self.pending_traveler_handoffs = future;
        let mut handoffs = ready
            .into_iter()
            .map(|(_, handoff)| handoff)
            .collect::<Vec<_>>();
        handoffs.sort_by_key(|handoff| {
            (
                handoff.traveler.entity,
                handoff.traveler.generation,
                match handoff.kind {
                    crate::core::components::HandoffKind::Move => 0,
                    crate::core::components::HandoffKind::Rollback => 1,
                },
            )
        });
        handoffs
    }

    /// Routes this step's resolved crossings for delivery on the following step.
    fn route_traveler_handoffs(&mut self, step: TravelStepId) -> Vec<OutboundMessage> {
        self.state
            .resolve_pending_traveler_handoffs()
            .into_iter()
            .map(|handoff| self.traveler_handoff_route(step, handoff))
            .collect()
    }

    /// Routes this movement step's core-produced work-arrival facts through the
    /// same coordinator/FIFO path for local and cross-region commuters.
    fn route_destination_arrivals(&mut self) -> Vec<OutboundMessage> {
        self.state
            .take_pending_destination_arrivals()
            .into_iter()
            .map(|arrival| self.destination_arrival_route(arrival.traveler, arrival.destination))
            .collect()
    }

    fn destination_arrival_route(
        &self,
        traveler: TravelerId,
        destination: PlaceRef,
    ) -> OutboundMessage {
        OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(traveler.entity.region()),
            event: RegionEvent::DestinationArrived {
                traveler,
                destination,
            },
        })
    }

    fn traveler_handoff_route(
        &self,
        step: TravelStepId,
        handoff: TravelerHandoff,
    ) -> OutboundMessage {
        OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(handoff.to_region),
            event: RegionEvent::ReceiveTraveler {
                eligible_step: step.next(),
                handoff,
            },
        })
    }

    fn remember_power_export_producer(&mut self, grant: &PowerExportGrant) {
        if grant.granted
            && let Some(source_region) = grant.source_region
        {
            insert_sorted_unique(&mut self.power_export_producers, source_region);
        }
    }

    fn remember_goods_supply_producer(&mut self, grant: &GoodsSupplyGrant) {
        if grant.granted
            && let Some(source_region) = grant.source_region
        {
            insert_sorted_unique(&mut self.goods_supply_producers, source_region);
        }
    }

    fn pop_next_runnable_event(&mut self) -> Option<RegionEvent> {
        self.receiver.pop_event()
    }

    /// Event-driven plan, P-2: gate the power reconcile on local/cross-region
    /// change instead of running it every tick unconditionally.
    ///
    /// The gate is decided BEFORE any demand collection — deliberately: once
    /// P-3 lands (diff-apply `power::run`), a kept import stays `powered`, so
    /// a demand scan taken first would not list it, and a dirty reconcile's
    /// release-all would then strip its producer reservation with no
    /// replacement request (the starvation fix's round-1 desync,
    /// reintroduced). The gate's own inputs (the dirty flag and the
    /// generation) need no demand scan, so this ordering is free even before
    /// P-3 lands.
    /// Retire-tickstate, P-a: power never pauses the tick anymore. It fires
    /// its release/request (when dirty) and moves straight into the job
    /// phase in the same pass; whichever reply lands later is applied
    /// whenever it arrives (`apply_power_export_grant`, gated on
    /// `current_power_request_id`), for the *next* tick to see.
    fn start_tick_power_phase(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
        let dirty = self.state.is_power_exports_dirty()
            || self.discovery_generation > self.seen_power_generation;
        if !dirty {
            // Quiet path: time still advances, but `power::run` is skipped
            // entirely (P-6 — the gate already guarantees nothing that could
            // affect power changed, so there's nothing for it to recompute),
            // and there's no demand scan and no release/request traffic.
            // Grants and the producer's ledger are untouched.
            let phase = self.state.begin_tick_power_phase_quiet();
            return self.enter_job_phase(request_id, phase);
        }
        self.seen_power_generation = self.discovery_generation;
        self.state.clear_power_exports_dirty();
        let phase = self.state.begin_tick_power_demand_phase();
        let mut outbound = self.release_and_request_power(request_id, &phase.power_demands);
        outbound.extend(self.enter_job_phase(request_id, phase));
        outbound
    }

    /// Time-neutral load-time re-negotiation: reuses `release_and_request_power`
    /// as a plain fire-and-forget call, same as a dirty tick's power phase.
    fn start_power_import_settlement(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
        let demands = self.state.power_import_settlement_demands();
        self.release_and_request_power(request_id, &demands)
    }

    /// Release this caller's previous-generation power reservations, then
    /// request the current demand batch. Fire-and-forget: stamps
    /// `current_power_request_id` so a later reply can tell "my current
    /// batch" from "a superseded one" (see `apply_power_export_grant`), and
    /// returns immediately without waiting for any reply.
    ///
    /// Shared by a dirty tick's power phase and load-time import settlement
    /// (`start_power_import_settlement`) — both just need "release what I
    /// held, request what I need now," never mind how the demand was
    /// collected.
    fn release_and_request_power(
        &mut self,
        request_id: UiRequestId,
        demands: &[PendingPowerDemand],
    ) -> Vec<OutboundMessage> {
        self.current_power_request_id = request_id;
        let producer_regions = std::mem::take(&mut self.power_export_producers);
        let release = PowerExportAllocationRelease {
            caller_region: self.region_id(),
            request_id,
            producer_regions,
        };
        let mut outbound = self.power_release_routes(release);
        for demand in demands {
            outbound.extend(self.begin_power_export(PowerExportRequest {
                request_id,
                caller_region: self.region_id(),
                caller_network: demand.caller_network,
                token: demand.token,
                demand: demand.demand,
                consumer: demand.consumer,
            }));
        }
        outbound
    }

    fn begin_power_export(&mut self, request: PowerExportRequest) -> Vec<OutboundMessage> {
        let candidates = self.power_candidates(request.caller_network);
        let attempt = PowerExportAllocationRequest {
            request,
            candidates,
            candidate_index: 0,
        };
        let Some(network) = attempt.candidates.first() else {
            let token = attempt.request.token;
            return self.apply_power_export_result(
                attempt,
                PowerExportGrant {
                    token,
                    granted: false,
                    source_region: None,
                },
            );
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(network.region),
            event: RegionEvent::ProcessPowerExportRequest(attempt),
        })]
    }

    fn power_candidates(&self, caller: RegionRoadNetworkId) -> Vec<RegionRoadNetworkId> {
        let Some(discovery) = self.discovery.as_ref() else {
            return Vec::new();
        };
        discovery
            .component_of(caller)
            .unwrap_or(&[])
            .iter()
            .copied()
            .filter(|network| network.region != self.region_id())
            .filter(|network| {
                discovery
                    .availability_hints
                    .iter()
                    .any(|hint| hint.network == *network && hint.has_spare_power)
            })
            .collect()
    }

    fn power_release_routes(&self, release: PowerExportAllocationRelease) -> Vec<OutboundMessage> {
        release
            .producer_regions
            .iter()
            .copied()
            .map(|region| {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(region),
                    event: RegionEvent::ReleasePowerExportAllocations(release.clone()),
                })
            })
            .collect()
    }

    /// Retire-tickstate, P-b: the eager nudge's handler. Time-neutral —
    /// unlike a normal tick, a nudge must not advance the game clock as a
    /// side effect, so it collects fresh demand via
    /// `RegionState::power_demand_recheck` (mirrors `power::run` directly,
    /// not `begin_tick_power_phase`) instead of
    /// `begin_tick_power_demand_phase`. Reuses the same
    /// `release_and_request_power` helper as a normal dirty tick — fire the
    /// release/request, don't wait.
    fn power_capacity_recheck(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
        let demands = self.state.power_demand_recheck();
        self.release_and_request_power(request_id, &demands)
    }

    /// The job phase: jobs never pause the tick, and cross-region employment is
    /// the ledger's (`daily_employment_phase`), reconciled only on a dirty daily
    /// boundary.
    ///
    /// The daily gate's connectivity half is read here (the installed discovery's
    /// fingerprint vs `seen_connectivity_fingerprint` — a connectivity change, not
    /// hint-value noise, P7-b); the other halves (`jobs_exports_dirty`,
    /// `has_unassigned_citizen`) are checked fresh inside
    /// `continue_tick_to_job_demand_phase`, AFTER population growth, so a citizen
    /// spawned this tick is noticed the same day. The combined answer is
    /// `phase.jobs_dirty()`.
    ///
    /// On a dirty daily tick the ledger runs; local assignment already ran inside
    /// `continue_tick_to_job_demand_phase`, so the ledger sees an accurate jobless
    /// set for its claims. A quiet day skips reconciliation entirely: applying an
    /// assignment does not re-dirty the gate
    /// (`World::refresh_jobs_cache_after_grant_applied`), so a settled worker's
    /// day reads quiet and leaves the assignment untouched — the same trust-the-gate
    /// shape as power's quiet path leaving `powered` alone.
    fn enter_job_phase(
        &mut self,
        request_id: UiRequestId,
        power_phase: RegionalTickPowerPhase,
    ) -> Vec<OutboundMessage> {
        let connectivity_dirty =
            self.installed_connectivity_fingerprint() != Some(self.seen_connectivity_fingerprint);
        let phase = self
            .state
            .continue_tick_to_job_demand_phase(power_phase, connectivity_dirty);
        if !phase.is_daily() {
            // Jobs resolve only on a daily boundary; an hourly tick neither makes
            // nor invalidates employment, so it runs no ledger reconciliation.
            let response = self.finish_job_phase(request_id, phase);
            return vec![OutboundMessage::RegionTickCompleted(response)];
        }
        let mut outbound = if phase.jobs_dirty() {
            if let Some(fingerprint) = self.installed_connectivity_fingerprint() {
                self.seen_connectivity_fingerprint = fingerprint;
            }
            self.state.clear_jobs_exports_dirty();
            self.daily_employment_phase()
        } else {
            // Quiet daily: existing assignments (local AND remote) and every
            // contract persist untouched -- no reconciliation. Mirrors power's
            // quiet path (P-6): trust the gate, skip the recompute entirely.
            Vec::new()
        };
        outbound.extend(self.enter_goods_phase(request_id, phase));
        outbound
    }

    /// P7-d: the connectivity fingerprint of the installed discovery snapshot,
    /// or `None` if no worker slice has installed one. `None != Some(seen)`, so
    /// a runtime with no discovery installed treats the connectivity gate as
    /// dirty once and reconciles (harmless: its route pass finds nothing).
    fn installed_connectivity_fingerprint(&self) -> Option<u64> {
        self.discovery
            .as_ref()
            .map(|discovery| discovery.connectivity_fingerprint)
    }

    /// P7-d: one region's daily cross-region employment reconciliation, run on a
    /// dirty daily tick. Local assignment already happened (no wipe); this owns
    /// the ledger side.
    ///
    /// Order (deviates from the plan pseudocode, which predates P7-a's retained
    /// reservations and its own double resolve/apply is now automatic):
    ///   1. route invalidation — drop contracts whose home no longer reaches the
    ///      workplace (frees their reserved seats via the P7-a sync).
    ///   2. capacity invalidation — drop contracts a shrunk/bulldozed workplace
    ///      can no longer physically hold.
    ///   3. report every dropped contract as an explicit `JobLoss` (wakes homes).
    ///   4. publish this region's pools (open_count already net of contracts).
    ///   5. submit fresh claims for this region's still-jobless citizens.
    ///
    /// The employer-validate / home-apply / release halves run when the wakes
    /// this emits are processed (`handle_employment_directory_ready`).
    ///
    /// A runtime with no directory installed does only local work (steps 1-5 are
    /// skipped): a single-region game has no cross-region employment.
    fn daily_employment_phase(&mut self) -> Vec<OutboundMessage> {
        let Some(directory) = self.employment_directory.clone() else {
            return Vec::new();
        };
        let discovery = self.discovery.clone();

        let mut lost = Vec::new();
        if let Some(discovery) = discovery.as_deref() {
            lost.extend(
                self.state
                    .release_contracts_with_unreachable_homes(discovery),
            );
        }
        lost.extend(self.state.release_contracts_over_current_capacity());

        let mut wake = std::collections::BTreeSet::new();
        for (workplace, citizen, _contract) in lost {
            wake.extend(directory.report_lost_employment(JobLoss {
                lease: EmploymentLeaseRef { citizen, workplace },
                reason: JobLossReason::PoolInvalid,
            }));
        }

        directory.publish_pools(self.region_id(), self.state.published_job_pools());

        let mut outbound = self.emit_employment_directory_ready(wake.into_iter().collect());
        if let Some(discovery) = discovery.as_deref() {
            outbound.extend(home_region_daily_jobs(self, &directory, discovery));
        }
        outbound
    }

    /// Event-driven plan, P-5: same gate shape as P-2/P-4, wrapping the
    /// whole reconcile decision. `enter_goods_phase` is only ever reached on
    /// a daily boundary in practice — `enter_job_phase`'s own `is_daily()`
    /// early-out (above) returns before calling this on an hourly tick — so
    /// no separate `is_daily()` check is needed here; this gate only ever
    /// decides dirty-vs-quiet for the daily case, same cadence as before
    /// this patch. `apply_pending_goods_stock` and
    /// `continue_tick_to_goods_demand_phase` always run regardless of the
    /// gate (same as power's quiet path still running `power::run`).
    fn enter_goods_phase(
        &mut self,
        request_id: UiRequestId,
        job_phase: RegionalTickJobPhase,
    ) -> Vec<OutboundMessage> {
        self.apply_pending_goods_stock();
        let phase = self.state.continue_tick_to_goods_demand_phase(job_phase);
        let dirty = self.state.is_goods_exports_dirty()
            || self.discovery_generation > self.seen_goods_generation;
        let prepared_goods_flow = self.prepare_and_apply_local_goods_exports(&phase);
        if !dirty {
            // Quiet: existing grants + the producer's ledger persist; no
            // release, no requests.
            let response = self.finish_goods_phase(request_id, phase, prepared_goods_flow);
            return vec![OutboundMessage::RegionTickCompleted(response)];
        }
        self.seen_goods_generation = self.discovery_generation;
        self.state.clear_goods_exports_dirty();
        let mut outbound = self.release_and_request_goods(request_id, &phase.goods_demands);
        let response = self.finish_goods_phase(request_id, phase, prepared_goods_flow);
        outbound.push(OutboundMessage::RegionTickCompleted(response));
        outbound
    }

    fn prepare_and_apply_local_goods_exports(
        &mut self,
        phase: &RegionalTickGoodsPhase,
    ) -> Option<economy::PreparedGoodsFlow> {
        if !phase.phase.is_daily() {
            return None;
        }
        self.state.produce_factory_goods_for_daily_tick();
        let exported_goods_units = self.goods_supply_allocations.units().sum();
        let prepared = self.state.prepare_goods_flow(exported_goods_units);
        for (token, grant) in prepared.local_grants().iter().enumerate() {
            let mut remaining = grant.units;
            let mut chunk = 0_u32;
            while remaining > 0 {
                let units = remaining.min(GOODS_PER_TRUCK as u32);
                let request = GoodsSupplyRequest {
                    request_id: self.current_goods_request_id,
                    caller_region: self.region_id(),
                    caller_network: RegionRoadNetworkId {
                        region: self.region_id(),
                        road_network: grant.caller_network,
                    },
                    token: (token as u32).saturating_mul(1024).saturating_add(chunk),
                    units,
                    commercial: grant.commercial,
                };
                let local_grant =
                    self.process_goods_supply_request(&GoodsSupplyAllocationRequest {
                        request: request.clone(),
                        candidates: vec![request.caller_network],
                        candidate_index: 0,
                    });
                self.apply_goods_supply_grant(request, local_grant);
                remaining -= units;
                chunk += 1;
            }
        }
        Some(prepared)
    }

    fn apply_pending_goods_stock(&mut self) {
        // Cross-region goods are stale by one economy settlement and runtime-only.
        // Save/restart drops this in-flight stock rather than persisting a
        // consumer-side good without the producer's matching transient allocation.
        for (commercial, units) in std::mem::take(&mut self.pending_goods_stock) {
            self.state.add_commercial_goods(commercial, units);
        }
    }

    fn release_and_request_goods(
        &mut self,
        request_id: UiRequestId,
        demands: &[PendingGoodsDemand],
    ) -> Vec<OutboundMessage> {
        self.current_goods_request_id = request_id;
        let producer_regions = std::mem::take(&mut self.goods_supply_producers);
        let release = GoodsSupplyAllocationRelease {
            caller_region: self.region_id(),
            request_id,
            producer_regions,
        };
        let mut outbound = self.goods_supply_release_routes(release);
        for demand in demands {
            outbound.extend(self.begin_goods_supply(GoodsSupplyRequest {
                request_id,
                caller_region: self.region_id(),
                caller_network: demand.caller_network,
                token: demand.token,
                units: demand.units,
                commercial: demand.commercial,
            }));
        }
        outbound
    }

    fn begin_goods_supply(&mut self, request: GoodsSupplyRequest) -> Vec<OutboundMessage> {
        let candidates = self.goods_candidates(request.caller_network);
        let attempt = GoodsSupplyAllocationRequest {
            request,
            candidates,
            candidate_index: 0,
        };
        let Some(network) = attempt.candidates.first() else {
            let token = attempt.request.token;
            return self.apply_goods_supply_result(
                attempt,
                GoodsSupplyGrant {
                    token,
                    granted: false,
                    source_region: None,
                    units: 0,
                },
            );
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(network.region),
            event: RegionEvent::ProcessGoodsSupplyRequest(attempt),
        })]
    }

    fn goods_candidates(&self, caller: RegionRoadNetworkId) -> Vec<RegionRoadNetworkId> {
        let Some(discovery) = self.discovery.as_ref() else {
            return Vec::new();
        };
        let mut candidates = discovery
            .component_of(caller)
            .unwrap_or(&[])
            .iter()
            .copied()
            .filter(|network| {
                discovery
                    .availability_hints
                    .iter()
                    .any(|hint| hint.network == *network && hint.spare_goods_units > 0)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|network| {
            (
                network.region != self.region_id(),
                network.region.0,
                network.road_network,
            )
        });
        candidates
    }

    fn goods_supply_release_routes(
        &self,
        release: GoodsSupplyAllocationRelease,
    ) -> Vec<OutboundMessage> {
        release
            .producer_regions
            .iter()
            .copied()
            .map(|region| {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(region),
                    event: RegionEvent::ReleaseGoodsSupplyAllocations(release.clone()),
                })
            })
            .collect()
    }

    fn finish_job_phase(
        &mut self,
        request_id: UiRequestId,
        phase: RegionalTickJobPhase,
    ) -> RegionTickResponse {
        // Slots this region reserved for remote workers accrue their workplace tax
        // to this region in its own economy step.
        //
        // Settlement can lag the home region by one daily tick, by design: regions
        // finish ticks independently with no cross-region economy fence. A
        // producer with no local job seekers finishes its own job phase in the same
        // pass as its Tick -- often before a fresh consumer request reaches it -- so
        // that day's economy uses the *previous* day's reservations (they persist
        // until the caller's next-generation release). This is deterministic and
        // self-correcting in steady state; pairing salary and tax on the same day
        // would require a global "all exports resolved" fence before any economy
        // runs, which is far more synchronization for little gain.
        // Producer-owned workplace tax comes from EmploymentContract state (one
        // slot per contracted seat).
        let exported_job_slots = self.state.contracted_workplace_tax_slots();
        RegionTickResponse {
            request_id,
            region_id: self.region_id(),
            result: self
                .state
                .finish_tick_job_demand_phase(phase, &exported_job_slots),
        }
    }

    fn finish_goods_phase(
        &mut self,
        request_id: UiRequestId,
        phase: RegionalTickGoodsPhase,
        prepared_goods_flow: Option<economy::PreparedGoodsFlow>,
    ) -> RegionTickResponse {
        // Producer-owned workplace tax comes from EmploymentContract state (one
        // slot per contracted seat).
        let exported_job_slots = self.state.contracted_workplace_tax_slots();
        let exported_goods_units = self.goods_supply_allocations.units().sum();
        RegionTickResponse {
            request_id,
            region_id: self.region_id(),
            result: if let Some(prepared_goods_flow) = prepared_goods_flow {
                self.state
                    .finish_tick_goods_demand_phase_with_prepared_goods(
                        phase,
                        &exported_job_slots,
                        exported_goods_units,
                        prepared_goods_flow,
                    )
            } else {
                self.state.finish_tick_goods_demand_phase(
                    phase,
                    &exported_job_slots,
                    exported_goods_units,
                )
            },
        }
    }

    fn process_power_export_request(
        &mut self,
        request: &PowerExportAllocationRequest,
    ) -> PowerExportGrant {
        let Some(producer_network) = request.candidates.get(request.candidate_index).copied()
        else {
            return PowerExportGrant {
                token: request.request.token,
                granted: false,
                source_region: None,
            };
        };
        let allocation_key = export_allocation_key(&request.request);
        // TODO(CR2 lifecycle): reservations clear when the caller starts a new tick
        // generation. Add explicit cleanup when caller regions are removed,
        // reassigned, or intentionally stop ticking. Not reachable single-worker;
        // tracked in docs/regional-multi-worker-plan.md (M6).
        self.power_export_allocations
            .release_stale_for_caller(request.request.caller_region, request.request.request_id);
        let active_export_allocations: i32 = self
            .power_export_allocations
            .reserved_units_excluding(allocation_key, producer_network)
            .sum();
        // Producer-owned export capacity is authoritative here:
        // local remaining capacity minus active transient export allocations.
        let remaining = self
            .state
            .power_network_remaining_capacity(producer_network)
            .saturating_sub(active_export_allocations);

        if remaining < request.request.demand {
            return PowerExportGrant {
                token: request.request.token,
                granted: false,
                source_region: None,
            };
        }

        self.power_export_allocations.upsert(
            allocation_key,
            producer_network,
            request.request.demand,
            request.request.request_id,
        );

        PowerExportGrant {
            token: request.request.token,
            granted: true,
            source_region: Some(self.region_id()),
        }
    }

    fn process_goods_supply_request(
        &mut self,
        request: &GoodsSupplyAllocationRequest,
    ) -> GoodsSupplyGrant {
        let Some(producer_network) = request.candidates.get(request.candidate_index).copied()
        else {
            return denied_goods_grant(request.request.token);
        };
        let allocation_key = export_allocation_key(&request.request);
        if producer_network.region == self.region_id()
            && request.request.caller_region == self.region_id()
        {
            let grant = self.state.dispatch_local_goods_shipment(
                &request.request,
                producer_network,
                allocation_key,
            );
            if grant.granted {
                self.goods_supply_allocations.upsert(
                    allocation_key,
                    producer_network,
                    grant.units,
                    request.request.request_id,
                );
            }
            return grant;
        }
        self.goods_supply_allocations
            .release_stale_for_caller(request.request.caller_region, request.request.request_id);
        let active_export_allocations: u32 = self
            .goods_supply_allocations
            .reserved_units_excluding(allocation_key, producer_network)
            .sum();
        let remaining = self
            .state
            .goods_network_remaining_units(producer_network)
            .saturating_sub(active_export_allocations);

        if remaining < request.request.units {
            return denied_goods_grant(request.request.token);
        }

        self.goods_supply_allocations.upsert(
            allocation_key,
            producer_network,
            request.request.units,
            request.request.request_id,
        );

        GoodsSupplyGrant {
            token: request.request.token,
            granted: true,
            source_region: Some(self.region_id()),
            units: request.request.units,
        }
    }

    /// Retire-tickstate, P-a: no continuation to consult — the reply carries
    /// the request it answers. One staleness check against
    /// `current_power_request_id` tells "my current batch" (apply it) from
    /// "a superseded one" (drop it, but see below).
    fn apply_power_export_grant(
        &mut self,
        request: PowerExportRequest,
        grant: PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        // The producer reserved capacity as soon as it emitted a granted reply.
        // Remember that producer even if this caller later ignores the grant
        // because its local demand disappeared or was already powered; the next
        // release must still reach the producer and clear that allocation.
        self.remember_power_export_producer(&grant);
        if request.request_id != self.current_power_request_id {
            // Caught in review: a superseded batch's release already fired
            // (`release_and_request_power` stamped a newer generation and
            // released this producer at that time) -- UNLESS this exact
            // grant arrived after that release, in which case the producer
            // reserved capacity no future release will ever target (this
            // caller has moved on and won't repeat an old generation). Send
            // one targeted release, stamped with the CURRENT generation, so
            // the producer's `release_stale_for_caller` drops this stale
            // reservation instead of holding it forever.
            return Self::release_stale_granted_power(
                self.region_id(),
                self.current_power_request_id,
                &grant,
            );
        }
        let demand = PendingPowerDemand {
            token: request.token,
            consumer: request.consumer,
            demand: request.demand,
            caller_network: request.caller_network,
        };
        self.state.apply_power_export_grant(demand, grant);
        Vec::new()
    }

    fn apply_power_export_result(
        &mut self,
        mut attempt: PowerExportAllocationRequest,
        grant: PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        if grant.granted || attempt.request.request_id != self.current_power_request_id {
            return self.apply_power_export_grant(attempt.request, grant);
        }
        attempt.candidate_index += 1;
        let Some(network) = attempt.candidates.get(attempt.candidate_index) else {
            return self.apply_power_export_grant(attempt.request, grant);
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(network.region),
            event: RegionEvent::ProcessPowerExportRequest(attempt),
        })]
    }

    /// A stale but *granted* reply reserved producer capacity that no future
    /// release is guaranteed to reach (this caller has already moved past
    /// that generation). Release it now instead of leaving it stuck. A
    /// stale denial reserved nothing, so it needs no release.
    fn release_stale_granted_power(
        caller_region: RegionId,
        current_request_id: UiRequestId,
        grant: &PowerExportGrant,
    ) -> Vec<OutboundMessage> {
        match grant.source_region {
            Some(producer) if grant.granted => {
                let release = PowerExportAllocationRelease {
                    caller_region,
                    request_id: current_request_id,
                    producer_regions: vec![producer],
                };
                vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(producer),
                    event: RegionEvent::ReleasePowerExportAllocations(release),
                })]
            }
            _ => Vec::new(),
        }
    }

    /// Retire-tickstate, P-d: same shape as power/jobs. The reply carries
    /// the request it answers, so a single request-id check replaces the
    /// old goods continuation and pending demand list.
    fn apply_goods_supply_grant(
        &mut self,
        request: GoodsSupplyRequest,
        grant: GoodsSupplyGrant,
    ) -> Vec<OutboundMessage> {
        self.remember_goods_supply_producer(&grant);
        if request.request_id != self.current_goods_request_id {
            // A stale but granted goods reply reserved producer inventory
            // after this caller already moved to a newer generation. Clear
            // that stale reservation now, matching power/jobs.
            return Self::release_stale_granted_goods(
                self.region_id(),
                self.current_goods_request_id,
                &grant,
            );
        }
        if grant.granted && grant.units > 0 {
            if grant.source_region == Some(self.region_id()) {
                self.state.record_local_goods_grant(
                    request.request_id,
                    request.token,
                    request.commercial,
                    grant.units,
                );
            } else {
                self.pending_goods_stock
                    .push((request.commercial, grant.units));
            }
        }
        Vec::new()
    }

    fn apply_goods_supply_result(
        &mut self,
        mut attempt: GoodsSupplyAllocationRequest,
        grant: GoodsSupplyGrant,
    ) -> Vec<OutboundMessage> {
        if grant.granted || attempt.request.request_id != self.current_goods_request_id {
            return self.apply_goods_supply_grant(attempt.request, grant);
        }
        attempt.candidate_index += 1;
        let Some(network) = attempt.candidates.get(attempt.candidate_index) else {
            return self.apply_goods_supply_grant(attempt.request, grant);
        };
        vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
            recipients: RegionRecipients::One(network.region),
            event: RegionEvent::ProcessGoodsSupplyRequest(attempt),
        })]
    }

    /// A stale but *granted* goods reply reserved producer stock that no
    /// future release is guaranteed to reach. A stale denial reserved
    /// nothing, so it needs no release.
    fn release_stale_granted_goods(
        caller_region: RegionId,
        current_request_id: UiRequestId,
        grant: &GoodsSupplyGrant,
    ) -> Vec<OutboundMessage> {
        match grant.source_region {
            Some(producer) if grant.granted => {
                let release = GoodsSupplyAllocationRelease {
                    caller_region,
                    request_id: current_request_id,
                    producer_regions: vec![producer],
                };
                vec![OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(producer),
                    event: RegionEvent::ReleaseGoodsSupplyAllocations(release),
                })]
            }
            _ => Vec::new(),
        }
    }

    fn run_command(
        &mut self,
        request_id: UiRequestId,
        command: RegionCommand,
    ) -> RegionCommandResponse {
        let reply = match command {
            RegionCommand::Build { x, y, kind } => {
                RegionCommandReply::CommandResult(self.state.build(x, y, kind))
            }
            RegionCommand::PreviewBuild { x, y, kind } => {
                RegionCommandReply::BuildPreview(self.state.preview_build(x, y, kind))
            }
            RegionCommand::Bulldoze { x, y } => {
                RegionCommandReply::CommandResult(self.state.bulldoze(x, y))
            }
            RegionCommand::Replace { x, y, kind } => {
                RegionCommandReply::CommandResult(self.state.replace(x, y, kind))
            }
            RegionCommand::Upgrade { x, y } => {
                RegionCommandReply::CommandResult(self.state.upgrade(x, y))
            }
        };

        RegionCommandResponse {
            request_id,
            region_id: self.region_id(),
            reply,
        }
    }

    fn build_snapshot(
        &mut self,
        request_id: UiRequestId,
        overlay: MapOverlayInput,
    ) -> RegionSnapshotResponse {
        // DT1: a snapshot is a derived-state read. Recompute the derived pass first
        // if a prior paused command left it dirty, so a snapshot taken with no tick
        // in between reflects the build/bulldoze. (`view_with_overlay` is a pure
        // read; the recompute is owned by this `&mut` boundary.)
        self.ensure_derived_state();
        let view = self.state.view_with_overlay(overlay);
        RegionSnapshotResponse {
            request_id,
            region_id: self.region_id(),
            snapshot: RegionViewSnapshot::from_view(self.region_id(), view),
        }
    }
}

/// P3, home side: submit one claim batch for this region's unemployed citizens.
///
/// **Not called from the tick.** P3 stages the claim flow; the old
/// request/grant path is still the live allocator until P7, and P4 is what
/// teaches the home region to apply an accepted assignment. Wiring this into
/// the daily job phase now would have two allocators drawing on the same spare
/// workplace slots. Tests drive it directly.
///
/// Citizens already spoken for — pending or accepted, per the directory's
/// `active_citizens_by_home_region` — are skipped before any lock is taken.
/// The directory re-checks the same rule inside `submit_claims`, because this
/// snapshot may be one pass stale.
#[allow(dead_code)] // P3: staged; the daily job phase starts calling this in P7.
pub(crate) fn home_region_daily_jobs(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
    discovery: &CrossRegionDiscovery,
) -> Vec<OutboundMessage> {
    let snapshot = directory.snapshot(); // cheap Arc clone; no directory lock held below
    let home = runtime.region_id();
    let active_citizens = snapshot
        .active_citizens_by_home_region
        .get(&home)
        .cloned()
        .unwrap_or_default();

    let home_networks = runtime
        .state()
        .network_border_links()
        .into_iter()
        .map(|link| link.network)
        .collect::<Vec<_>>();

    // Hoisted out of the per-citizen loop: the plan writes
    // `choose_best_pool(&snapshot, citizen)`, but reachability and ranking
    // depend only on the *home region*, not on which of its citizens is
    // asking. Every unemployed citizen would get the same answer, so compute
    // it once. `submit_claims` caps the batch at the pool's `open_count`; the
    // citizens it turns away retry next pass, when the snapshot no longer
    // advertises the seats already reserved.
    let Some(pool) = choose_best_pool(&snapshot, discovery, home, &home_networks) else {
        return Vec::new(); // nothing reachable and open; nobody to wake
    };

    let claims = runtime
        .state()
        .unemployed_citizens()
        .into_iter()
        .filter(|citizen| !active_citizens.contains(citizen))
        .map(|citizen| {
            (
                CitizenRef {
                    region: home,
                    citizen,
                },
                pool.workplace,
                pool.generation,
            )
        })
        .collect::<Vec<_>>();

    // One short lock to reserve pending claims. The returned regions are wake
    // targets only; the claims themselves stay in the directory.
    let regions_to_wake = directory.submit_claims(claims);
    runtime.emit_employment_directory_ready(regions_to_wake)
}

/// P3, employer side: decide every pending claim against this region's own ECS.
///
/// Reads the batch, validates each claim against employer-owned capacity, and
/// hands compact decisions back. Both accepted *and* rejected decisions wake
/// the home region: an acceptance is ready to apply (P4), and a rejection is
/// what releases the home's citizen-side pending guard so it can retry.
///
/// If several wakes land before this runs, the first call decides the pending
/// claims and `apply_claim_decisions` clears them, so later wakes see an empty
/// batch and return immediately.
pub(crate) fn employer_validate_claims(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let claims = directory.take_pending_claims_for_employer(runtime.region_id());
    if claims.is_empty() {
        return Vec::new();
    }

    let decisions = claims
        .into_iter()
        .map(|claim| {
            if runtime
                .state()
                .job_pool_still_has_open_capacity(claim.workplace)
            {
                JobClaimDecision::Accepted {
                    claim_id: claim.claim_id,
                    assignment: runtime
                        .state_mut()
                        .accept_claim_and_create_assignment(&claim),
                }
            } else {
                JobClaimDecision::Rejected {
                    claim_id: claim.claim_id,
                }
            }
        })
        .collect::<Vec<_>>();

    let regions_to_wake = directory.apply_claim_decisions(runtime.region_id(), decisions);
    runtime.emit_employment_directory_ready(regions_to_wake)
}

/// P4, home side: write every accepted assignment for this region's citizens
/// into their durable `Citizen.workplace_assignment`, then tell the directory
/// which ones actually landed.
///
/// The directory's `accepted_by_home_region` is a **read cache**, not truth: it
/// keeps re-offering an already-applied citizen on every wake, and
/// `apply_workplace_assignment` answers `false` for those. So a repeated
/// `EmploymentDirectoryReady` is idempotent, and only *newly* applied citizens
/// are acknowledged.
///
/// Nothing is paid from a *pending* claim: a claim only reaches
/// `accepted_by_home_region` once its employer has accepted it and recorded a
/// contract. The economy then pays from the applied assignment on the next
/// daily settlement, using the salary captured at accept time — the same path
/// the old export grant already used.
pub(crate) fn home_apply_accepted_employment(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let snapshot = directory.snapshot(); // cheap Arc clone; no directory lock held below
    let Some(accepted) = snapshot.accepted_by_home_region.get(&runtime.region_id()) else {
        return Vec::new();
    };

    let mut applied = Vec::new();
    let mut declined = Vec::new();
    for (citizen, assignment) in accepted {
        if runtime
            .state_mut()
            .apply_workplace_assignment(citizen.citizen, *assignment)
        {
            applied.push(*citizen);
        } else if !runtime
            .state()
            .citizen_holds_workplace(citizen.citizen, assignment.workplace)
        {
            // The employer accepted this claim and is reserving+taxing the seat,
            // but this home can never apply it: the citizen took a local job or
            // left between claim and apply. Decline so the employer frees the
            // seat, otherwise the contract is a phantom that reserves and taxes a
            // seat nobody works. (An already-applied lease answers `false` too,
            // but `citizen_holds_workplace` keeps it out of this branch.)
            declined.push(EmploymentLeaseRef {
                citizen: *citizen,
                workplace: assignment.workplace,
            });
        }
    }

    directory.acknowledge_home_applied(applied);

    let mut wake = std::collections::BTreeSet::new();
    for lease in declined {
        wake.extend(directory.request_release(lease));
    }
    runtime.emit_employment_directory_ready(wake.into_iter().collect())
}

/// P5, home side: this citizen gives its job up.
///
/// The home clears its own truth first, then asks the employer to free the
/// seat. Between the two, the directory still lists the citizen as accepted,
/// which is what stops it claiming a second job before the first is confirmed
/// released — and what stops the seat being advertised twice.
#[allow(dead_code)] // P5: staged; no gameplay action releases a job yet.
pub(crate) fn home_release_job(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
    citizen: Entity,
) -> Vec<OutboundMessage> {
    let Some(assignment) = runtime.state_mut().clear_employment(citizen) else {
        return Vec::new(); // nothing to release
    };
    let regions_to_wake = directory.request_release(EmploymentLeaseRef {
        citizen: CitizenRef {
            region: runtime.region_id(),
            citizen,
        },
        workplace: assignment.workplace,
    });
    runtime.emit_employment_directory_ready(regions_to_wake)
}

/// P5, employer side: honour the release requests queued for this region.
///
/// Only a release the employer can actually match is confirmed. One it cannot
/// (the contract is already gone — typically because the employer lost it first
/// and reported that loss) is dropped: the accepted cache was already cleared
/// by that loss, so there is nothing left to free.
pub(crate) fn employer_apply_releases(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) {
    for release in directory.take_releases_for_employer(runtime.region_id()) {
        if runtime
            .state_mut()
            .release_contract_if_matches(release.workplace, release.citizen)
        {
            directory.confirm_release(runtime.region_id(), release);
        }
    }
}

/// P5, home side: apply the losses an employer has confirmed.
///
/// Clears the citizen's assignment only if it still names the lost workplace —
/// a loss report can be one pass stale, and the citizen may already have moved
/// to a different job.
pub(crate) fn home_apply_losses(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    for loss in directory.take_losses_for_home(runtime.region_id()) {
        runtime
            .state_mut()
            .clear_employment_if_matches(loss.lease.citizen.citizen, loss.lease.workplace);
    }
}

/// P5, employer side: republish this region's pools, and explicitly report
/// every contract it can no longer honour.
///
/// Loss is never inferred from a pool vanishing out of a snapshot. The employer
/// decides, drops the contract in its own state, and *tells* the home region.
///
/// Deviation: the plan passes the freshly computed `pools` into
/// `release_contracts_over_current_capacity`; they cannot answer the question
/// (see that method, which reads the registry's reservation instead). The
/// pool/eviction *ordering* is revisited by P7-d's cutover; this staged
/// version keeps the plan's "pools before release" order.
#[allow(dead_code)] // P7-a: staged; the daily tick starts publishing in P7-d.
pub(crate) fn employer_publish_pools(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let pools = runtime.state().published_job_pools();
    let lost_contracts = runtime
        .state_mut()
        .release_contracts_over_current_capacity();

    directory.publish_pools(runtime.region_id(), pools);

    let mut regions_to_wake = BTreeSet::new();
    for (workplace, citizen, _contract) in lost_contracts {
        regions_to_wake.extend(directory.report_lost_employment(JobLoss {
            lease: EmploymentLeaseRef { citizen, workplace },
            reason: JobLossReason::PoolInvalid,
        }));
    }
    runtime.emit_employment_directory_ready(regions_to_wake.into_iter().collect())
}

#[cfg(test)]
mod tick_state_tests {
    //! Unit tests for the runtime event loop's fire-and-forget export flows
    //! (retire-tickstate P-a/P-c/P-d) and producer-side allocation handling.

    use super::*;
    use crate::core::regions::{RegionState, RegionalAvailabilityHint};
    use crate::interface::input::BuildingKind;

    // A residential consumer next to a border road has no local power and one
    // exportable demand.
    fn consumer_runtime(region_id: RegionId) -> RegionRuntime {
        let mut region = RegionState::new(region_id, 2, 2);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        let mut runtime = RegionRuntime::new(region);
        install_remote_supplier(&mut runtime);
        runtime
    }

    fn install_remote_supplier(runtime: &mut RegionRuntime) {
        let caller_network = runtime.state().network_border_links()[0].network;
        let supplier_network = RegionRoadNetworkId {
            region: RegionId(2),
            road_network: 0,
        };
        runtime.set_discovery_snapshot(Arc::new(CrossRegionDiscovery {
            components: vec![vec![caller_network, supplier_network]],
            availability_hints: vec![RegionalAvailabilityHint {
                network: supplier_network,
                has_spare_power: true,
                spare_job_slot_ids: Vec::new(),
                spare_goods_units: u32::MAX,
            }],
            ..Default::default()
        }));
    }

    fn export_requests(outbound: &[OutboundMessage]) -> Vec<&PowerExportRequest> {
        outbound
            .iter()
            .filter_map(|message| match message {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    event: RegionEvent::ProcessPowerExportRequest(attempt),
                    ..
                }) => Some(&attempt.request),
                _ => None,
            })
            .collect()
    }

    fn goods_supply_requests(outbound: &[OutboundMessage]) -> Vec<&GoodsSupplyRequest> {
        outbound
            .iter()
            .filter_map(|message| match message {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    event: RegionEvent::ProcessGoodsSupplyRequest(attempt),
                    ..
                }) => Some(&attempt.request),
                _ => None,
            })
            .collect()
    }

    fn routed_event(outbound: &[OutboundMessage]) -> RegionEvent {
        match outbound {
            [OutboundMessage::CoordinatorRoute(RoutedRegionEvent { event, .. })] => event.clone(),
            _ => panic!("expected exactly one routed event, got {outbound:?}"),
        }
    }

    fn local_goods_runtime() -> (RegionRuntime, Entity, Entity, RegionRoadNetworkId) {
        let mut region = RegionState::new(RegionId(1), 3, 2);
        assert!(region.build(0, 0, BuildingKind::Industrial).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        assert!(region.build(2, 0, BuildingKind::Commercial).success);
        assert!(region.build(1, 1, BuildingKind::PowerPlant).success);
        region.ensure_derived_state();
        region.produce_factory_goods_for_daily_tick();

        let factory = region.world.grid.get(0, 0).expect("factory");
        let commercial = region.world.grid.get(2, 0).expect("commercial");
        let road_network =
            crate::core::systems::road_network_analysis::access_for(&region.world, commercial)
                .network_id
                .expect("commercial road network");
        (
            RegionRuntime::new(region),
            factory,
            commercial,
            RegionRoadNetworkId {
                region: RegionId(1),
                road_network,
            },
        )
    }

    fn power_attempt(request: PowerExportRequest) -> PowerExportAllocationRequest {
        PowerExportAllocationRequest {
            request,
            candidates: Vec::new(),
            candidate_index: 0,
        }
    }

    fn goods_supply_attempt(request: GoodsSupplyRequest) -> GoodsSupplyAllocationRequest {
        GoodsSupplyAllocationRequest {
            request,
            candidates: Vec::new(),
            candidate_index: 0,
        }
    }

    fn message_index(
        outbound: &[OutboundMessage],
        predicate: impl Fn(&OutboundMessage) -> bool,
    ) -> usize {
        outbound
            .iter()
            .position(predicate)
            .expect("expected outbound message")
    }

    fn has_tick_completed(outbound: &[OutboundMessage]) -> bool {
        outbound
            .iter()
            .any(|message| matches!(message, OutboundMessage::RegionTickCompleted(_)))
    }

    #[test]
    fn inbound_traveler_handoff_waits_for_its_eligible_step() {
        use crate::core::components::{
            HandoffKind, PlaceRef, TravelKind, TravelState, TravelToken, TravelerId,
        };

        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
        let handoff = TravelerHandoff {
            token: TravelToken {
                state: TravelState::default(),
                home: PlaceRef {
                    region: RegionId(1),
                    building: Entity::new(RegionId(1), 1),
                },
                kind: TravelKind::Citizen { work: None },
                trip_gen: 7,
            },
            traveler: TravelerId {
                entity: Entity::new(RegionId(1), 9),
                generation: 7,
            },
            to_region: RegionId(2),
            entry_link: None,
            kind: HandoffKind::Rollback,
        };

        runtime.process_event(RegionEvent::ReceiveTraveler {
            eligible_step: TravelStepId(2),
            handoff,
        });
        assert_eq!(runtime.pending_traveler_handoffs.len(), 1);
        assert!(
            runtime
                .take_handoffs_eligible_at(TravelStepId(1))
                .is_empty()
        );
        assert_eq!(runtime.pending_traveler_handoffs.len(), 1);
        assert_eq!(runtime.take_handoffs_eligible_at(TravelStepId(2)).len(), 1);
    }

    #[test]
    fn eligible_traveler_handoffs_are_ordered_by_traveler_identity() {
        use crate::core::components::{
            HandoffKind, PlaceRef, TravelKind, TravelState, TravelToken, TravelerId,
        };

        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
        for citizen in [Entity::new(RegionId(1), 9), Entity::new(RegionId(1), 3)] {
            runtime.pending_traveler_handoffs.push((
                TravelStepId(2),
                TravelerHandoff {
                    token: TravelToken {
                        state: TravelState::default(),
                        home: PlaceRef {
                            region: RegionId(1),
                            building: Entity::new(RegionId(1), 1),
                        },
                        kind: TravelKind::Citizen { work: None },
                        trip_gen: 7,
                    },
                    traveler: TravelerId {
                        entity: citizen,
                        generation: 7,
                    },
                    to_region: RegionId(2),
                    entry_link: None,
                    kind: HandoffKind::Rollback,
                },
            ));
        }

        let handoffs = runtime.take_handoffs_eligible_at(TravelStepId(2));
        assert_eq!(handoffs[0].traveler.entity.local(), 3);
        assert_eq!(handoffs[1].traveler.entity.local(), 9);
    }

    #[test]
    fn duplicate_travel_step_is_ignored() {
        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
        runtime.process_event(RegionEvent::StepTravel {
            step: TravelStepId(3),
        });
        runtime.process_event(RegionEvent::StepTravel {
            step: TravelStepId(3),
        });
        assert_eq!(runtime.last_travel_step, Some(TravelStepId(3)));
    }

    #[test]
    fn step_travel_routes_pending_arrival_to_the_citizens_home_region() {
        use crate::core::components::PendingDestinationArrival;

        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
        let traveler = TravelerId {
            entity: Entity::new(RegionId(1), 7),
            generation: 3,
        };
        let destination = PlaceRef {
            region: RegionId(2),
            building: Entity::new(RegionId(2), 4),
        };
        runtime
            .state_mut()
            .world
            .outgoing_destination_arrivals
            .push(PendingDestinationArrival {
                traveler,
                destination,
            });

        let outbound = runtime.process_event(RegionEvent::StepTravel {
            step: TravelStepId(1),
        });

        assert!(matches!(
            outbound.as_slice(),
            [OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(1)),
                event: RegionEvent::DestinationArrived {
                    traveler: routed_traveler,
                    destination: routed_destination,
                },
            })] if *routed_traveler == traveler && *routed_destination == destination
        ));
    }

    #[test]
    fn destination_arrival_records_attendance_once_for_the_current_work_trip() {
        use crate::core::city_refs::CityCellRef;
        use crate::core::components::{ArrivalAction, Citizen, Morale, WorkplaceAssignment};

        let citizen_id = Entity::new(RegionId(1), 7);
        let workplace = Entity::new(RegionId(2), 4);
        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
        runtime.state_mut().world.citizens.insert(
            citizen_id,
            Citizen {
                id: citizen_id,
                age: 1,
                home: Entity::new(RegionId(1), 0),
                workplace_assignment: Some(WorkplaceAssignment {
                    workplace,
                    location: CityCellRef::local(RegionId(2), 0, 0),
                    salary: 100,
                }),
                morale: Morale::default(),
                money: 0,
                arrival_action: ArrivalAction::StartWorkShift,
                work_trip_generation: 3,
                attended_since_daily_settlement: false,
            },
        );
        let event = RegionEvent::DestinationArrived {
            traveler: TravelerId {
                entity: citizen_id,
                generation: 3,
            },
            destination: PlaceRef {
                region: RegionId(2),
                building: workplace,
            },
        };

        runtime.process_event(event.clone());
        runtime.process_event(event);
        let citizen = &runtime.state().world.citizens[&citizen_id];
        assert!(citizen.attended_since_daily_settlement);
        assert_eq!(citizen.arrival_action, ArrivalAction::ReturnHome);

        {
            let citizen = runtime
                .state_mut()
                .world
                .citizens
                .get_mut(&citizen_id)
                .expect("citizen remains present");
            citizen.attended_since_daily_settlement = false;
            citizen.arrival_action = ArrivalAction::StartWorkShift;
        }
        runtime.process_event(RegionEvent::DestinationArrived {
            traveler: TravelerId {
                entity: citizen_id,
                generation: 2,
            },
            destination: PlaceRef {
                region: RegionId(2),
                building: workplace,
            },
        });
        assert_eq!(
            runtime.state().world.citizens[&citizen_id].work_trip_generation,
            3,
            "a stale arrival must not affect the current work trip"
        );
        assert!(
            !runtime.state().world.citizens[&citizen_id].attended_since_daily_settlement,
            "a stale generation must not record attendance for the new trip"
        );
    }

    #[test]
    fn tick_with_exportable_demand_completes_immediately_and_requests_export() {
        // Retire-tickstate, P-a: power no longer pauses the tick. It fires
        // the release/request fire-and-forget and finishes in the same
        // pass; whichever reply lands later is applied whenever it
        // arrives, for the *next* tick to see.
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });

        let outbound = runtime.process_next_event();
        assert!(has_tick_completed(&outbound));
        assert_eq!(export_requests(&outbound).len(), 1);
    }

    #[test]
    fn power_capacity_recheck_requests_export_without_advancing_time() {
        // Retire-tickstate, P-b: the eager nudge is fire-and-forget and time-
        // neutral -- it must collect fresh demand and fire release/request
        // exactly like a dirty tick's power phase, but WITHOUT ticking the
        // clock (a nudge is triggered by a neighbor's road/building change,
        // not by this region's own turn advancing).
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::PowerCapacityRecheck {
            request_id: UiRequestId(42),
            source_region: RegionId(2),
        });

        let outbound = runtime.process_next_event();

        assert_eq!(
            runtime.state().view().status.turn,
            0,
            "a nudge must never advance the clock"
        );
        assert!(
            !has_tick_completed(&outbound),
            "a nudge is fire-and-forget, not a tick -- it never completes a tick"
        );
        assert_eq!(export_requests(&outbound).len(), 1);
    }

    #[test]
    fn tick_without_exportable_demand_finishes_immediately() {
        // Event-driven plan, P-2: an empty, never-touched region starts with
        // both `power_exports_dirty` and the seen/current generation clean
        // (0 == 0), so its first tick takes the quiet path — no demand scan,
        // no release/request traffic at all, not even the previously
        // unconditional empty allocation release. The tick still completes
        // immediately; that part of this test's name is even more true now.
        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });

        let outbound = runtime.process_next_event();
        assert!(has_tick_completed(&outbound));
        assert!(
            !outbound.iter().any(|message| matches!(
                message,
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    event: RegionEvent::ReleasePowerExportAllocations(_)
                        | RegionEvent::ProcessPowerExportRequest(_),
                    ..
                })
            )),
            "a quiet tick must not emit any power export traffic"
        );
    }

    #[test]
    fn daily_tick_without_goods_demands_finishes_without_export_traffic() {
        // Event-driven plan, P-5: mirrors the job version above, but for
        // goods. Same border-safe fixture (a lone Commercial building on a
        // grid large enough that its road never touches an edge, so
        // pending_goods_demands's border_networks check is always empty,
        // and nothing can ever pause waiting for a producer that doesn't
        // exist in this bare-runtime test). Placing the Commercial building
        // dirties goods_exports_dirty from construction
        // (invalidate_resource_registry), so day 1's reconcile genuinely
        // fires with zero actual goods demands.
        let mut region = RegionState::new(RegionId(2), 5, 5);
        assert!(region.build(2, 2, BuildingKind::Commercial).success);
        assert!(region.build(2, 1, BuildingKind::Road).success);
        let mut runtime = RegionRuntime::new(region);
        let mut last_outbound = Vec::new();

        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            last_outbound = runtime.process_next_event();
        }

        assert!(has_tick_completed(&last_outbound));
        assert!(
            !last_outbound.iter().any(|message| matches!(
                message,
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    event: RegionEvent::ReleaseGoodsSupplyAllocations(_)
                        | RegionEvent::ProcessGoodsSupplyRequest(_),
                    ..
                })
            )),
            "empty producer history and demand should emit no goods export traffic"
        );

        // Event-driven plan, P-5 (positive proof): this region has no
        // industrial building and no citizens, so the daily economy
        // settlement's own EconomyBreakdown shows zero goods activity every
        // day (local_goods_produced/sold, imported_goods_sold,
        // exported_goods all stay 0) — the conditional mark in
        // finish_tick_after_goods_phase never re-dirties goods_exports_dirty
        // after day 1's reconcile clears it. Day 2 must be genuinely quiet.
        for request_id in 25..=48 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            let outbound = runtime.process_next_event();
            assert!(
                !outbound.iter().any(|message| matches!(
                    message,
                    OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                        event: RegionEvent::ReleaseGoodsSupplyAllocations(_)
                            | RegionEvent::ProcessGoodsSupplyRequest(_),
                        ..
                    })
                )),
                "day 2 must be genuinely quiet: no goods export traffic at all"
            );
        }
    }

    #[test]
    fn matching_grant_applies_without_completing_a_second_tick() {
        // Retire-tickstate, P-a: the tick already completed when it asked
        // (previous test). A later reply for the SAME batch
        // (`current_power_request_id` unchanged since) is applied straight
        // to local state -- it must not, and cannot, re-emit
        // `RegionTickCompleted`; that already happened.
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(7),
        });
        let started = runtime.process_next_event();
        assert!(has_tick_completed(&started));
        let request = export_requests(&started)[0].clone();

        runtime.push_event(RegionEvent::ApplyPowerExportGrant {
            request: power_attempt(request.clone()),
            grant: PowerExportGrant {
                token: request.token,
                granted: true,
                source_region: Some(RegionId(2)),
            },
        });
        let outbound = runtime.process_next_event();

        assert!(
            !has_tick_completed(&outbound),
            "applying a grant must not re-complete a tick that already finished"
        );
        assert!(
            runtime.state().world.power_consumers[&request.consumer].powered,
            "a grant matching the current batch is applied to local state"
        );
    }

    #[test]
    fn rejected_power_candidate_retries_the_next_reachable_producer() {
        let mut runtime = consumer_runtime(RegionId(1));
        let request = PowerExportRequest {
            request_id: UiRequestId(0),
            caller_region: RegionId(1),
            caller_network: runtime.state().network_border_links()[0].network,
            token: 4,
            demand: 1,
            consumer: runtime.state().world.grid.get(0, 0).expect("consumer"),
        };
        let first = RegionRoadNetworkId {
            region: RegionId(2),
            road_network: 0,
        };
        let second = RegionRoadNetworkId {
            region: RegionId(3),
            road_network: 0,
        };
        runtime.push_event(RegionEvent::ApplyPowerExportGrant {
            request: PowerExportAllocationRequest {
                request,
                candidates: vec![first, second],
                candidate_index: 0,
            },
            grant: PowerExportGrant {
                token: 4,
                granted: false,
                source_region: None,
            },
        });

        let outbound = runtime.process_next_event();
        assert!(matches!(
            outbound.as_slice(),
            [OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(3)),
                event: RegionEvent::ProcessPowerExportRequest(attempt),
            })] if attempt.candidate_index == 1
        ));
    }

    #[test]
    fn zero_candidate_goods_request_applies_a_denial_immediately() {
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
        let caller_network = runtime.state().network_border_links()[0].network;
        let supplier_network = RegionRoadNetworkId {
            region: RegionId(2),
            road_network: 0,
        };
        runtime.set_discovery_snapshot(Arc::new(CrossRegionDiscovery {
            components: vec![vec![caller_network, supplier_network]],
            availability_hints: vec![RegionalAvailabilityHint {
                network: supplier_network,
                has_spare_power: false,
                spare_job_slot_ids: Vec::new(),
                spare_goods_units: 0,
            }],
            ..Default::default()
        }));
        let commercial = runtime.state().world.grid.get(1, 1).expect("commercial");
        let outbound = runtime.begin_goods_supply(GoodsSupplyRequest {
            request_id: UiRequestId(0),
            caller_region: RegionId(1),
            caller_network: RegionRoadNetworkId {
                region: RegionId(1),
                road_network: 0,
            },
            token: 2,
            units: 1,
            commercial,
        });

        assert!(outbound.is_empty());
        assert!(runtime.pending_goods_stock.is_empty());
    }

    #[test]
    fn goods_candidates_try_local_network_before_foreign_networks() {
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
        let caller_network = runtime.state().network_border_links()[0].network;
        let foreign_network = RegionRoadNetworkId {
            region: RegionId(2),
            road_network: 0,
        };
        runtime.set_discovery_snapshot(Arc::new(CrossRegionDiscovery {
            components: vec![vec![foreign_network, caller_network]],
            availability_hints: vec![
                RegionalAvailabilityHint {
                    network: foreign_network,
                    has_spare_power: false,
                    spare_job_slot_ids: Vec::new(),
                    spare_goods_units: 1,
                },
                RegionalAvailabilityHint {
                    network: caller_network,
                    has_spare_power: false,
                    spare_job_slot_ids: Vec::new(),
                    spare_goods_units: 1,
                },
            ],
            ..Default::default()
        }));
        let commercial = runtime.state().world.grid.get(1, 1).expect("commercial");

        let outbound = runtime.begin_goods_supply(GoodsSupplyRequest {
            request_id: UiRequestId(0),
            caller_region: RegionId(1),
            caller_network,
            token: 2,
            units: 1,
            commercial,
        });

        assert!(
            matches!(
                outbound.as_slice(),
                [OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(1)),
                    event: RegionEvent::ProcessGoodsSupplyRequest(attempt),
                })] if attempt.candidates == vec![caller_network, foreign_network]
                    && attempt.candidate_index == 0
            ),
            "same-region goods supply should use the existing candidate walk before foreign supply"
        );
    }

    #[test]
    fn local_goods_grant_records_order_without_immediate_stock() {
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
        let commercial = runtime.state().world.grid.get(1, 1).expect("commercial");
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            0
        );

        runtime.push_event(RegionEvent::ApplyGoodsSupplyGrant {
            request: goods_supply_attempt(GoodsSupplyRequest {
                request_id: UiRequestId(0),
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 7,
                units: 1,
                commercial,
            }),
            grant: GoodsSupplyGrant {
                token: 7,
                granted: true,
                source_region: Some(RegionId(1)),
                units: 1,
            },
        });
        let outbound = runtime.process_next_event();

        assert!(outbound.is_empty());
        assert!(runtime.pending_goods_stock.is_empty());
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            0,
            "same-region goods grants now wait for a truck arrival"
        );
        assert_eq!(
            runtime.state().world.goods_orders.len(),
            1,
            "the commercial keeps the pending inbound order"
        );
    }

    #[test]
    fn malformed_export_attempt_is_rejected_without_panicking_the_producer() {
        let mut runtime = RegionRuntime::new(goods_producer_region(RegionId(2)));
        let power = runtime.process_power_export_request(&PowerExportAllocationRequest {
            request: PowerExportRequest {
                request_id: UiRequestId(1),
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 1,
                demand: 1,
                consumer: Entity::new(RegionId(1), 1),
            },
            candidates: Vec::new(),
            candidate_index: 0,
        });
        let goods = runtime.process_goods_supply_request(&GoodsSupplyAllocationRequest {
            request: GoodsSupplyRequest {
                request_id: UiRequestId(1),
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 1,
                units: 1,
                commercial: Entity::new(RegionId(1), 1),
            },
            candidates: Vec::new(),
            candidate_index: 0,
        });

        assert!(!power.granted);
        assert!(!goods.granted);
    }

    #[test]
    fn stale_reply_is_dropped_and_releases_the_producer() {
        // Retire-tickstate, P-a: a reply for a batch this caller has
        // already superseded (a newer request_id is now current) must be
        // dropped rather than applied -- applying it would power a
        // consumer using a producer reservation this caller no longer
        // holds. Caught in review: a stale but GRANTED reply must also
        // actively release that producer, or its reservation could get
        // stuck if this caller then goes quiet.
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });
        let first = runtime.process_next_event();
        let stale_request = export_requests(&first)[0].clone();

        // A second dirty tick supersedes the first batch's generation. (Bare
        // `RegionRuntime` tests bypass the worker's per-slice
        // `set_discovery_generation` call, so bump it directly to reopen the
        // gate the same way a real worker pass would if something
        // discoverable had changed.)
        runtime.set_discovery_generation(1);
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(2),
        });
        runtime.process_next_event();

        // The FIRST tick's (now stale) request comes back granted.
        runtime.push_event(RegionEvent::ApplyPowerExportGrant {
            request: power_attempt(stale_request.clone()),
            grant: PowerExportGrant {
                token: stale_request.token,
                granted: true,
                source_region: Some(RegionId(2)),
            },
        });
        let outbound = runtime.process_next_event();

        assert!(
            !runtime.state().world.power_consumers[&stale_request.consumer].powered,
            "a stale batch's grant must not power the consumer"
        );
        assert!(
            matches!(
                outbound.as_slice(),
                [OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(2)),
                    event: RegionEvent::ReleasePowerExportAllocations(release),
                })] if release.producer_regions == vec![RegionId(2)]
            ),
            "a stale but granted reply must release the producer's reservation \
             immediately, since this caller may never send another release"
        );
    }

    #[test]
    fn grant_for_a_since_removed_consumer_is_a_no_op() {
        // Retire-tickstate, P-a risk: a reply can echo a consumer that was
        // bulldozed after the request went out (same batch, so the
        // staleness check passes). The ECS write must tolerate an entity
        // that no longer exists.
        let mut runtime = consumer_runtime(RegionId(1));

        runtime.push_event(RegionEvent::ApplyPowerExportGrant {
            request: power_attempt(PowerExportRequest {
                request_id: UiRequestId(0), // matches a never-ticked runtime's current
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 0,
                demand: 1,
                consumer: crate::core::entity::Entity::new(RegionId(1), 999), // no such entity
            }),
            grant: PowerExportGrant {
                token: 0,
                granted: true,
                source_region: Some(RegionId(2)),
            },
        });
        let outbound = runtime.process_next_event();

        assert!(outbound.is_empty());
    }

    #[test]
    fn daily_tick_with_goods_demand_completes_immediately_and_requests_export() {
        // Retire-tickstate, P-d: goods now matches power/jobs. A daily
        // goods demand fires release/request and completes the tick in the
        // same pass; the grant, if it arrives, is staged for a later daily
        // goods phase.
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
        install_remote_supplier(&mut runtime);
        while runtime.pending_event_count() > 0 {
            runtime.process_next_event();
        }

        let mut requested = false;
        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            let outbound = runtime.process_next_event();
            if outbound.iter().any(|message| {
                matches!(
                    message,
                    OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                        event: RegionEvent::ProcessGoodsSupplyRequest(_),
                        ..
                    })
                )
            }) {
                let request = message_index(&outbound, |message| {
                    matches!(
                        message,
                        OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                            event: RegionEvent::ProcessGoodsSupplyRequest(_),
                            ..
                        })
                    )
                });
                let completed = message_index(&outbound, |message| {
                    matches!(message, OutboundMessage::RegionTickCompleted(_))
                });
                assert!(request < completed);
                requested = true;
                break;
            }
        }
        assert!(
            requested,
            "a daily tick should eventually request remote goods"
        );
    }

    #[test]
    fn local_goods_grant_waits_for_truck_arrival() {
        let (mut runtime, factory, commercial, network) = local_goods_runtime();
        let request = GoodsSupplyRequest {
            request_id: UiRequestId(11),
            caller_region: RegionId(1),
            caller_network: network,
            token: 0,
            units: GOODS_PER_TRUCK as u32,
            commercial,
        };
        runtime.current_goods_request_id = request.request_id;
        let attempt = GoodsSupplyAllocationRequest {
            request,
            candidates: vec![network],
            candidate_index: 0,
        };

        let grant = runtime.process_goods_supply_request(&attempt);
        assert!(grant.granted, "factory should accept a truck delivery");
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            0,
            "accepted local goods do not refill commercial storage immediately"
        );
        runtime.apply_goods_supply_grant(attempt.request, grant);
        assert_eq!(runtime.state().world.goods_orders.len(), 1);
        assert_eq!(runtime.state().world.tokens.len(), 1);
        assert_eq!(
            runtime.state().world.buildings[&factory].data,
            crate::core::components::BuildingData::Industrial {
                goods: crate::core::components::FactoryGoodsState {
                    stored_units: 4,
                    reserved_outbound_units: GOODS_PER_TRUCK,
                },
                business: Default::default(),
            }
        );

        let step_outbound = runtime.process_event(RegionEvent::StepTravel {
            step: TravelStepId(1),
        });
        let arrival = routed_event(&step_outbound);
        assert!(matches!(arrival, RegionEvent::DestinationArrived { .. }));

        let delivery = routed_event(&runtime.process_event(arrival.clone()));
        assert!(matches!(delivery, RegionEvent::ApplyGoodsDelivery { .. }));
        assert_eq!(
            runtime
                .state()
                .world
                .trucks
                .values()
                .next()
                .unwrap()
                .arrival_action,
            crate::core::components::ArrivalAction::ReturnHome
        );

        let confirm = routed_event(&runtime.process_event(delivery));
        assert!(matches!(confirm, RegionEvent::ConfirmGoodsDelivery { .. }));
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            GOODS_PER_TRUCK,
            "commercial storage refills only when the delivery event applies"
        );
        runtime.process_event(confirm);
        let truck = runtime.state().world.trucks.values().next().unwrap();
        assert!(
            truck.shipment.is_none(),
            "confirmation clears the factory shipment"
        );
        assert_eq!(
            runtime.state().world.buildings[&factory].data,
            crate::core::components::BuildingData::Industrial {
                goods: crate::core::components::FactoryGoodsState {
                    stored_units: 1,
                    reserved_outbound_units: 0,
                },
                business: Default::default(),
            }
        );

        let duplicate = runtime.process_event(arrival);
        assert!(
            duplicate.is_empty(),
            "duplicate arrival cannot authorize a second delivery"
        );
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            GOODS_PER_TRUCK
        );
    }

    #[test]
    fn busy_factory_truck_denies_a_second_local_shipment() {
        let (mut runtime, _factory, commercial, network) = local_goods_runtime();
        runtime.current_goods_request_id = UiRequestId(12);
        let first = GoodsSupplyRequest {
            request_id: UiRequestId(12),
            caller_region: RegionId(1),
            caller_network: network,
            token: 0,
            units: GOODS_PER_TRUCK as u32,
            commercial,
        };
        let second = GoodsSupplyRequest { token: 1, ..first };

        let first_grant = runtime.process_goods_supply_request(&GoodsSupplyAllocationRequest {
            request: first,
            candidates: vec![network],
            candidate_index: 0,
        });
        assert!(first_grant.granted);
        let second_grant = runtime.process_goods_supply_request(&GoodsSupplyAllocationRequest {
            request: second,
            candidates: vec![network],
            candidate_index: 0,
        });
        assert!(
            !second_grant.granted,
            "the single level-1 truck is busy until delivery returns"
        );
    }

    #[test]
    fn disconnected_local_factory_cannot_dispatch_goods_truck() {
        let mut region = RegionState::new(RegionId(1), 3, 2);
        assert!(region.build(0, 0, BuildingKind::Industrial).success);
        assert!(region.build(2, 0, BuildingKind::Commercial).success);
        assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
        region.ensure_derived_state();
        region.produce_factory_goods_for_daily_tick();
        let commercial = region.world.grid.get(2, 0).expect("commercial");
        let request = GoodsSupplyRequest {
            caller_region: RegionId(1),
            request_id: UiRequestId(13),
            token: 0,
            units: 1,
            commercial,
            caller_network: RegionRoadNetworkId {
                region: RegionId(1),
                road_network: 0,
            },
        };
        let grant = region.dispatch_local_goods_shipment(
            &request,
            request.caller_network,
            ExportAllocationKey {
                caller_region: RegionId(1),
                request_id: UiRequestId(13),
                token: 0,
            },
        );

        assert!(!grant.granted);
        assert!(region.world.tokens.is_empty());
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(&region.world, commercial),
            0
        );
    }

    #[test]
    fn matching_goods_grant_applies_on_next_daily_goods_phase() {
        // Goods imports were already delayed via `pending_goods_stock`; P-d
        // keeps that behavior while removing the pause. A grant received
        // after day 1's tick is not stored immediately. It lands when the
        // next daily goods phase starts.
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
        install_remote_supplier(&mut runtime);
        while runtime.pending_event_count() > 0 {
            runtime.process_next_event();
        }

        let mut day1_outbound = Vec::new();
        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            day1_outbound = runtime.process_next_event();
        }
        let request = goods_supply_requests(&day1_outbound)[0].clone();
        let commercial = request.commercial;
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            0
        );

        runtime.push_event(RegionEvent::ApplyGoodsSupplyGrant {
            request: goods_supply_attempt(request.clone()),
            grant: GoodsSupplyGrant {
                token: request.token,
                granted: true,
                source_region: Some(RegionId(2)),
                units: 2,
            },
        });
        let grant_outbound = runtime.process_next_event();
        assert!(grant_outbound.is_empty());
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            0,
            "granted goods are staged, not applied immediately"
        );

        for request_id in 25..=48 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            runtime.process_next_event();
        }
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            2,
            "pending goods stock applies at the next daily goods phase"
        );
    }

    #[test]
    fn stale_goods_reply_is_dropped_and_releases_the_producer() {
        // Retire-tickstate, P-d: same staleness protection as power/jobs.
        // A stale granted goods reply reserved producer stock, so dropping
        // it locally must also emit a targeted release.
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
        install_remote_supplier(&mut runtime);
        while runtime.pending_event_count() > 0 {
            runtime.process_next_event();
        }

        let mut day1_outbound = Vec::new();
        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            day1_outbound = runtime.process_next_event();
        }
        let stale_request = goods_supply_requests(&day1_outbound)[0].clone();

        runtime.set_discovery_generation(1);
        for request_id in 25..=48 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            runtime.process_next_event();
        }

        let producer = RegionId(9);
        runtime.push_event(RegionEvent::ApplyGoodsSupplyGrant {
            request: goods_supply_attempt(stale_request.clone()),
            grant: GoodsSupplyGrant {
                token: stale_request.token,
                granted: true,
                source_region: Some(producer),
                units: stale_request.units,
            },
        });
        let outbound = runtime.process_next_event();

        assert!(
            matches!(
                outbound.as_slice(),
                [OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(target),
                    event: RegionEvent::ReleaseGoodsSupplyAllocations(release),
                })] if *target == producer && release.producer_regions == vec![producer]
            ),
            "a stale but granted goods reply must release the producer's reservation"
        );
    }

    #[test]
    fn goods_supply_request_reserves_producer_surplus_units() {
        let mut runtime = RegionRuntime::new(goods_producer_region(RegionId(2)));
        runtime.ensure_derived_state();
        let network = region_network(2, 0);

        let grant_a = runtime.process_goods_supply_request(&goods_supply_request(
            RegionId(10),
            0,
            3,
            network,
        ));
        assert_eq!(
            grant_a,
            GoodsSupplyGrant {
                token: 0,
                granted: true,
                source_region: Some(RegionId(2)),
                units: 3,
            }
        );

        let grant_b = runtime.process_goods_supply_request(&goods_supply_request(
            RegionId(11),
            1,
            2,
            network,
        ));
        assert_eq!(
            grant_b,
            GoodsSupplyGrant {
                token: 1,
                granted: false,
                source_region: None,
                units: 0,
            }
        );
    }

    fn goods_supply_request(
        caller: RegionId,
        token: u32,
        units: u32,
        producer_network: RegionRoadNetworkId,
    ) -> GoodsSupplyAllocationRequest {
        ExportAllocationRequest {
            request: GoodsSupplyRequest {
                request_id: UiRequestId(1),
                caller_region: caller,
                caller_network: producer_network,
                token,
                units,
                commercial: crate::core::entity::Entity::new(caller, token),
            },
            candidates: vec![producer_network],
            candidate_index: 0,
        }
    }

    fn region_network(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }

    // A producer whose only workplace bridges two single-cell, disconnected road
    // networks: roads at (0,0) [west] and (2,0) [east] never connect (the commercial
    // between them is not a road), but the commercial is adjacent to both. The plant
    // powers the west network, so the commercial is powered and offers its slots.

    fn goods_producer_region(region_id: RegionId) -> RegionState {
        let mut region = RegionState::new(region_id, 2, 2);
        assert!(region.build(0, 0, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
        assert!(region.build(1, 1, BuildingKind::Industrial).success);
        region
    }

    // A powered Commercial building with no local Industrial anywhere: its
    // free storage capacity is never filled locally, so it has an ongoing
    // exportable goods demand. Small grid (matching `goods_producer_region`)
    // means every cell touches a border, so a real cross-region request can
    // be sent -- unlike `daily_tick_without_goods_demands...`'s interior-road
    // fixture, which deliberately has no border link at all.
    fn goods_seeker_region(region_id: RegionId) -> RegionState {
        let mut region = RegionState::new(region_id, 2, 2);
        assert!(region.build(0, 0, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
        assert!(region.build(1, 1, BuildingKind::Commercial).success);
        region
    }

    // Residential on a locally-powered border network whose only workplace is on a
    // disconnected network: jobs are counted (population grows) but unreachable, so
    // citizens stay locally jobless and seek a remote slot.
}

#[cfg(test)]
mod export_allocations_tests {
    //! Unit tests for the shared producer-side reservation bookkeeping (CR3R).

    use super::*;

    fn key(caller: u32, generation: u64, token: u32) -> ExportAllocationKey {
        ExportAllocationKey {
            caller_region: RegionId(caller),
            request_id: UiRequestId(generation),
            token,
        }
    }

    fn net(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }

    #[test]
    fn upsert_inserts_then_refreshes_same_key() {
        let mut allocations = ExportAllocations::<i32>::new();
        allocations.upsert(key(1, 5, 0), net(2, 0), 3, UiRequestId(5));
        // Same key updates in place rather than adding a second reservation.
        allocations.upsert(key(1, 5, 0), net(2, 0), 7, UiRequestId(5));
        assert_eq!(allocations.units().collect::<Vec<_>>(), vec![7]);
    }

    #[test]
    fn reserved_units_excludes_own_key_and_other_networks() {
        let mut allocations = ExportAllocations::<i32>::new();
        allocations.upsert(key(1, 5, 0), net(2, 0), 3, UiRequestId(5));
        allocations.upsert(key(1, 5, 1), net(2, 0), 4, UiRequestId(5));
        allocations.upsert(key(1, 5, 2), net(2, 1), 9, UiRequestId(5));

        // On network (2,0), excluding token-0's own key, only token-1's 4 remains;
        // token-2's reservation on a different network is never counted.
        let reserved = allocations.reserved_units_excluding(key(1, 5, 0), net(2, 0));
        assert_eq!(reserved.collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn release_drops_only_stale_generations_for_caller() {
        let mut allocations = ExportAllocations::<i32>::new();
        allocations.upsert(key(1, 5, 0), net(2, 0), 3, UiRequestId(5));
        allocations.upsert(key(2, 5, 0), net(2, 0), 4, UiRequestId(5));

        // Caller 1 starts generation 6: its generation-5 reservation is dropped,
        // while caller 2's reservation is untouched.
        allocations.release_stale_for_caller(RegionId(1), UiRequestId(6));
        assert_eq!(allocations.units().collect::<Vec<_>>(), vec![4]);
    }
}

#[cfg(test)]
mod employment_claim_flow_tests {
    //! Directory employment ledger plan, P3: the home -> directory -> employer
    //! round trip, driven directly (the daily tick does not call it yet).

    use super::*;
    use crate::core::components::WorkplaceAssignment;
    use crate::core::regions::RegionRoadNetworkId;
    use crate::core::regions::RegionState;
    use crate::core::regions::employment_directory::{JobClaim, JobClaimId, JobPool};
    use crate::core::systems::citizens;
    use crate::interface::input::BuildingKind;

    /// Employer region 9: a powered Commercial workplace (2 seats) whose road
    /// spine reaches the west border, facing home region 1.
    fn employer_runtime() -> RegionRuntime {
        let mut region = RegionState::new(RegionId(9), 3, 3);
        assert!(region.build(0, 0, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
        assert!(region.build(1, 1, BuildingKind::Commercial).success);
        region.ensure_derived_state();
        RegionRuntime::new(region)
    }

    /// Home region 1: `count` jobless citizens and a road on its east border.
    fn home_runtime(count: i32) -> RegionRuntime {
        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        assert!(region.build(2, 0, BuildingKind::Road).success);
        let home = region.world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut region.world, home, count);
        region.ensure_derived_state();
        RegionRuntime::new(region)
    }

    /// A discovery snapshot that puts home 1 and employer 9 in one component.
    fn shared_component(home: &RegionRuntime, employer: &RegionRuntime) -> CrossRegionDiscovery {
        let mut networks = home
            .state()
            .network_border_links()
            .into_iter()
            .map(|link| link.network)
            .collect::<Vec<_>>();
        networks.extend(
            employer
                .state()
                .network_border_links()
                .into_iter()
                .map(|link| link.network),
        );
        assert!(!networks.is_empty(), "fixture must have border networks");
        CrossRegionDiscovery {
            components: vec![networks],
            ..Default::default()
        }
    }

    fn wake_targets(outbound: &[OutboundMessage]) -> Vec<RegionId> {
        outbound
            .iter()
            .filter_map(|message| match message {
                OutboundMessage::CoordinatorRoute(route) => match route.recipients {
                    RegionRecipients::One(target_region)
                        if matches!(route.event, RegionEvent::EmploymentDirectoryReady) =>
                    {
                        Some(target_region)
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_claim_round_trip_contracts_the_seat_and_wakes_the_home() {
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);

        let workplace = employer.state().published_job_pools()[0].workplace;
        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));

        // Home submits; the employer is woken (payload-free).
        let outbound = home_region_daily_jobs(&mut home, &directory, &discovery);
        assert_eq!(wake_targets(&outbound), vec![RegionId(9)]);

        // Employer validates; the home is woken back.
        let outbound = employer_validate_claims(&mut employer, &directory);
        assert_eq!(wake_targets(&outbound), vec![RegionId(1)]);

        // Employer-side truth: a real contract exists.
        assert_eq!(
            employer.state().contract_holders_at(workplace).len(),
            1,
            "the employer owns the contract, not the directory"
        );
        // Directory read cache mirrors it, and the pending indexes are clear.
        let snapshot = directory.snapshot();
        assert_eq!(snapshot.accepted_by_home_region[&RegionId(1)].len(), 1);
        assert!(snapshot.pending_claims_by_employer.is_empty());
    }

    #[test]
    fn the_wake_event_carries_no_claim_payload_and_pulls_work_from_the_directory() {
        // P3 review check: "EmploymentDirectoryReady carries no claim payload;
        // regions pull work from the directory."
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);
        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));
        home_region_daily_jobs(&mut home, &directory, &discovery);

        // Deliver the wake through the real event path, with the directory
        // installed exactly as a worker slice would.
        employer.set_employment_directory(Arc::clone(&directory));
        employer.push_event(RegionEvent::EmploymentDirectoryReady);
        let outbound = employer.process_next_event();

        assert_eq!(
            wake_targets(&outbound),
            vec![RegionId(1)],
            "the employer pulled its pending claim from the directory and answered"
        );
        assert!(directory.snapshot().pending_claims_by_employer.is_empty());
    }

    #[test]
    fn a_second_wake_is_a_cheap_no_op_once_the_claims_are_decided() {
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);
        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));
        home_region_daily_jobs(&mut home, &directory, &discovery);

        let workplace = employer.state().published_job_pools()[0].workplace;
        assert!(!employer_validate_claims(&mut employer, &directory).is_empty());
        assert_eq!(employer.state().contract_holders_at(workplace).len(), 1);

        assert!(
            employer_validate_claims(&mut employer, &directory).is_empty(),
            "a repeat wake finds an empty batch and wakes nobody"
        );
        assert_eq!(
            employer.state().contract_holders_at(workplace).len(),
            1,
            "the repeat wake must not create a second contract for the same citizen"
        );
    }

    #[test]
    fn a_wake_without_an_installed_directory_is_a_no_op() {
        let mut employer = employer_runtime();
        employer.push_event(RegionEvent::EmploymentDirectoryReady);
        assert!(employer.process_next_event().is_empty());
    }

    #[test]
    fn the_discovery_snapshot_install_roundtrips() {
        // P7-c: a bare runtime has no snapshot; the worker installs one per slice
        // (same site as set_employment_directory). P7-d reads it for reachability.
        let mut employer = employer_runtime();
        assert!(employer.discovery_snapshot().is_none());

        let installed = Arc::new(CrossRegionDiscovery {
            connectivity_fingerprint: 7,
            ..Default::default()
        });
        employer.set_discovery_snapshot(Arc::clone(&installed));
        assert_eq!(
            employer
                .discovery_snapshot()
                .map(|d| d.connectivity_fingerprint),
            Some(7)
        );
    }

    #[test]
    fn an_employer_never_contracts_more_seats_than_it_has() {
        // P3 behavior forbidden: "no workplace pool accepts more than open_count."
        // Two seats, four hopeful citizens.
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(4);
        let discovery = shared_component(&home, &employer);
        let pools = employer.state().published_job_pools();
        let workplace = pools[0].workplace;
        assert_eq!(pools[0].open_count, 2, "level-1 Commercial has 2 seats");
        assert!(directory.publish_pools(RegionId(9), pools));

        home_region_daily_jobs(&mut home, &directory, &discovery);
        employer_validate_claims(&mut employer, &directory);

        assert_eq!(
            employer.state().contract_holders_at(workplace).len(),
            2,
            "exactly open_count citizens are hired, whichever two they are"
        );
        let snapshot = directory.snapshot();
        assert_eq!(snapshot.accepted_by_home_region[&RegionId(1)].len(), 2);
    }

    #[test]
    fn choose_best_pool_ignores_a_pool_in_another_component() {
        let directory = Arc::new(EmploymentDirectory::default());
        let employer = employer_runtime();
        let mut home = home_runtime(1);
        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));

        // Discovery where the home shares no component with the employer.
        let isolated = CrossRegionDiscovery {
            components: vec![vec![RegionRoadNetworkId {
                region: RegionId(1),
                road_network: 0,
            }]],
            ..Default::default()
        };

        let outbound = home_region_daily_jobs(&mut home, &directory, &isolated);
        assert!(
            outbound.is_empty(),
            "no reachable pool -> no claim, no wake"
        );
        assert!(directory.snapshot().pending_claims_by_employer.is_empty());
    }

    #[test]
    fn an_employer_never_validates_a_claim_chosen_from_stale_pool_facts() {
        // Found in review. The employer's own check is capacity-only, which is
        // only sound because the directory drops a pending claim the moment its
        // target pool's facts (and therefore generation) change. Prove the
        // employer never gets handed such a claim, and contracts nobody.
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);

        let pools = employer.state().published_job_pools();
        let workplace = pools[0].workplace;
        assert!(directory.publish_pools(RegionId(9), pools.clone()));
        home_region_daily_jobs(&mut home, &directory, &discovery);
        assert_eq!(
            directory
                .take_pending_claims_for_employer(RegionId(9))
                .len(),
            1,
            "the claim is pending before the pool changes"
        );

        // The employer republishes the same workplace with a changed salary,
        // before it ever processes its wake.
        let changed = pools
            .into_iter()
            .map(|pool| JobPool {
                salary: pool.salary + 25,
                generation: 0,
                ..pool
            })
            .collect::<Vec<_>>();
        assert!(directory.publish_pools(RegionId(9), changed));

        assert!(
            directory
                .take_pending_claims_for_employer(RegionId(9))
                .is_empty(),
            "the stale claim must never reach the employer"
        );
        assert!(
            employer_validate_claims(&mut employer, &directory).is_empty(),
            "nothing to decide, so nobody is woken"
        );
        assert!(
            employer.state().contract_holders_at(workplace).is_empty(),
            "no contract may be created from stale facts"
        );

        // The citizen is free to claim again against the fresh facts.
        assert!(!home_region_daily_jobs(&mut home, &directory, &discovery).is_empty());
        assert!(!employer_validate_claims(&mut employer, &directory).is_empty());
        assert_eq!(employer.state().contract_holders_at(workplace).len(), 1);
    }

    #[test]
    fn republishing_after_an_accept_is_a_no_op_so_surviving_claims_are_not_churned() {
        // The property that ties the two review fixes together. After an accept:
        //   directory cached open_count  = published - 1   (apply_claim_decisions)
        //   employer's next published    = spare - contracted
        // Those converge on the same number, so the republish is UNCHANGED --
        // no generation bump, and therefore no invalidation of any other pool's
        // still-valid pending claims. Without published_job_pools subtracting
        // contracts, the republish would resurrect the seat, look "changed", and
        // churn pending claims every single pass.
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);

        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));
        home_region_daily_jobs(&mut home, &directory, &discovery);
        employer_validate_claims(&mut employer, &directory);

        assert!(
            !directory.publish_pools(RegionId(9), employer.state().published_job_pools()),
            "the post-accept republish must be a no-op, not a 'changed' pool"
        );
    }

    #[test]
    fn a_fully_contracted_workplace_publishes_no_pool_but_keeps_its_accepted_workers() {
        // open_count == 0 -> the row is omitted -> publish_pools sees it as
        // `removed` -> invalidate_pending_claims_for_pool. That must clear only
        // PENDING coordination state; the accepted workers keep their jobs until
        // an explicit release/loss (P5).
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(2);
        let discovery = shared_component(&home, &employer);
        let workplace = employer.state().published_job_pools()[0].workplace;

        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));
        home_region_daily_jobs(&mut home, &directory, &discovery);
        employer_validate_claims(&mut employer, &directory);
        assert_eq!(employer.state().contract_holders_at(workplace).len(), 2);

        assert!(
            employer.state().published_job_pools().is_empty(),
            "both seats contracted -> nothing left to advertise"
        );
        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));

        let snapshot = directory.snapshot();
        assert!(
            snapshot.open_pools_by_network.is_empty(),
            "the pool row is gone"
        );
        assert_eq!(
            snapshot.accepted_by_home_region[&RegionId(1)].len(),
            2,
            "accepted employment survives the pool row disappearing"
        );
    }

    // ---- P4: home apply ----

    fn only_citizen(runtime: &RegionRuntime) -> Entity {
        *runtime
            .state()
            .world
            .citizens
            .keys()
            .next()
            .expect("one citizen")
    }

    fn assignment_of(runtime: &RegionRuntime, citizen: Entity) -> Option<WorkplaceAssignment> {
        runtime.state().world.citizens[&citizen].workplace_assignment
    }

    /// Runs the full P3 claim flow and leaves an accepted, un-applied claim in
    /// the directory for home region 1's single citizen.
    fn accepted_but_not_yet_applied() -> (Arc<EmploymentDirectory>, RegionRuntime, RegionRuntime) {
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);

        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));
        home_region_daily_jobs(&mut home, &directory, &discovery);
        employer_validate_claims(&mut employer, &directory);

        let citizen = only_citizen(&home);
        assert!(
            assignment_of(&home, citizen).is_none(),
            "accepted in the directory, but the home has not applied it yet"
        );
        (directory, employer, home)
    }

    #[test]
    fn home_apply_writes_the_accepted_assignment_onto_the_citizen() {
        let (directory, _employer, mut home) = accepted_but_not_yet_applied();
        let citizen = only_citizen(&home);

        home_apply_accepted_employment(&mut home, &directory);

        let applied = assignment_of(&home, citizen).expect("assignment applied");
        assert_eq!(
            applied.workplace.region(),
            RegionId(9),
            "a remote workplace"
        );
        assert_eq!(
            applied.salary,
            directory.snapshot().accepted_by_home_region[&RegionId(1)][0]
                .1
                .salary,
            "the home applies exactly the salary the employer accepted"
        );
    }

    #[test]
    fn the_home_wake_applies_accepted_employment_through_the_real_event_path() {
        let (directory, _employer, mut home) = accepted_but_not_yet_applied();
        let citizen = only_citizen(&home);

        home.set_employment_directory(Arc::clone(&directory));
        home.push_event(RegionEvent::EmploymentDirectoryReady);
        home.process_next_event();

        assert!(
            assignment_of(&home, citizen).is_some(),
            "the home's own wake is what applies its accepted employment"
        );
    }

    #[test]
    fn repeated_home_wakes_are_idempotent() {
        // P4 review check: "repeated EmploymentDirectoryReady events are
        // idempotent." The accepted read cache keeps re-offering the citizen.
        let (directory, _employer, mut home) = accepted_but_not_yet_applied();
        let citizen = only_citizen(&home);

        home_apply_accepted_employment(&mut home, &directory);
        let first = assignment_of(&home, citizen).expect("applied");

        home_apply_accepted_employment(&mut home, &directory);
        home_apply_accepted_employment(&mut home, &directory);

        assert_eq!(
            assignment_of(&home, citizen),
            Some(first),
            "re-applying the same accepted employment changes nothing"
        );
        assert_eq!(
            directory.snapshot().accepted_by_home_region[&RegionId(1)].len(),
            1,
            "and the read cache still holds it -- acknowledge does not evict"
        );
    }

    #[test]
    fn an_unapplicable_accepted_lease_is_declined_so_the_employer_frees_the_seat() {
        // P7-d High (codex): if the home can never apply an accepted lease -- the
        // citizen took a local job (or left) between claim and apply -- the
        // employer must not keep reserving and taxing a seat nobody works. The
        // home declines it, and the employer frees the seat.
        let (directory, mut employer, mut home) = accepted_but_not_yet_applied();
        let citizen = only_citizen(&home);
        assert_eq!(
            employer.state().contracted_workplace_tax_slots().len(),
            1,
            "the employer holds one contract for the accepted claim"
        );

        // The citizen grabs a *local* job before the home applies the remote one.
        let local_workplace = Entity::new(RegionId(1), 7);
        assert!(home.state_mut().apply_workplace_assignment(
            citizen,
            WorkplaceAssignment {
                workplace: local_workplace,
                location: CityCellRef {
                    region: RegionId(1),
                    x: 0,
                    y: 0,
                },
                salary: 3,
            }
        ));

        // Home wake: cannot apply the remote lease, so it declines it and wakes
        // the employer.
        let wake = home_apply_accepted_employment(&mut home, &directory);
        assert!(
            wake.iter().any(|msg| matches!(
                msg,
                OutboundMessage::CoordinatorRoute(route)
                    if matches!(route.recipients, RegionRecipients::One(RegionId(9)))
                        && matches!(route.event, RegionEvent::EmploymentDirectoryReady)
            )),
            "declining the stale lease wakes its employer"
        );

        // Employer wake: it honours the release and frees the seat.
        employer_apply_releases(&mut employer, &directory);
        assert!(
            employer.state().contracted_workplace_tax_slots().is_empty(),
            "the phantom contract is gone -- no seat reserved or taxed"
        );
        assert!(
            directory
                .snapshot()
                .accepted_by_home_region
                .get(&RegionId(1))
                .is_none_or(|accepted| accepted.is_empty()),
            "and the accepted cache no longer lists the declined lease"
        );
        // The citizen keeps its local job untouched.
        assert_eq!(
            assignment_of(&home, citizen).map(|a| a.workplace),
            Some(local_workplace)
        );
    }

    #[test]
    fn a_pending_claim_is_never_applied_or_paid() {
        // P4 behavior forbidden: "do not pay from pending claims."
        let directory = Arc::new(EmploymentDirectory::default());
        let employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);
        assert!(directory.publish_pools(RegionId(9), employer.state().published_job_pools()));

        // Submit, but never let the employer validate.
        home_region_daily_jobs(&mut home, &directory, &discovery);
        let citizen = only_citizen(&home);
        assert!(!directory.snapshot().pending_claims_by_employer.is_empty());

        home_apply_accepted_employment(&mut home, &directory);
        assert!(
            assignment_of(&home, citizen).is_none(),
            "a merely pending claim must never become an assignment"
        );
    }

    #[test]
    fn a_rejected_claim_never_becomes_an_assignment() {
        // P4 review check: "rejected claims do not create assignments."
        let directory = Arc::new(EmploymentDirectory::default());
        let mut employer = employer_runtime();
        let mut home = home_runtime(1);
        let discovery = shared_component(&home, &employer);
        let pools = employer.state().published_job_pools();
        let workplace = pools[0].workplace;
        assert!(directory.publish_pools(RegionId(9), pools));
        home_region_daily_jobs(&mut home, &directory, &discovery);

        // Fill every seat with local contracts so the claim is rejected for
        // want of employer-owned capacity.
        for local in 0..2u32 {
            employer
                .state_mut()
                .accept_claim_and_create_assignment(&JobClaim {
                    claim_id: JobClaimId(900 + local as u64),
                    citizen: CitizenRef {
                        region: RegionId(5),
                        citizen: Entity::new(RegionId(5), local),
                    },
                    workplace,
                    generation: 1,
                });
        }
        employer_validate_claims(&mut employer, &directory);

        let citizen = only_citizen(&home);
        assert!(
            !directory
                .snapshot()
                .accepted_by_home_region
                .contains_key(&RegionId(1)),
            "the claim was rejected, so nothing is accepted for this home"
        );
        home_apply_accepted_employment(&mut home, &directory);
        assert!(
            assignment_of(&home, citizen).is_none(),
            "a rejected claim must not create an assignment"
        );
    }

    #[test]
    fn home_apply_does_not_overwrite_a_job_the_citizen_took_meanwhile() {
        // P4 behavior forbidden: "do not clear an old assignment while merely
        // checking for replacement work."
        let (directory, _employer, mut home) = accepted_but_not_yet_applied();
        let citizen = only_citizen(&home);

        let local = WorkplaceAssignment {
            workplace: Entity::new(RegionId(1), 7),
            location: CityCellRef::local(RegionId(1), 2, 2),
            salary: 5,
        };
        assert!(home.state_mut().apply_workplace_assignment(citizen, local));

        home_apply_accepted_employment(&mut home, &directory);
        assert_eq!(
            assignment_of(&home, citizen),
            Some(local),
            "the local job the citizen already took survives the accepted remote one"
        );
    }

    #[test]
    fn the_directory_cache_is_not_the_durable_source_of_home_employment_truth() {
        // P4 behavior forbidden: "do not make directory cache the durable source
        // of home employment truth." Once applied, the assignment lives in the
        // region. Losing the whole broker must not un-employ the citizen.
        let (directory, _employer, mut home) = accepted_but_not_yet_applied();
        let citizen = only_citizen(&home);
        home_apply_accepted_employment(&mut home, &directory);
        let applied = assignment_of(&home, citizen).expect("applied");

        // Throw the entire directory away, cache and all.
        drop(directory);
        let rebuilt = Arc::new(EmploymentDirectory::default());
        assert!(
            !rebuilt
                .snapshot()
                .accepted_by_home_region
                .contains_key(&RegionId(1)),
            "the fresh broker knows nothing"
        );

        assert_eq!(
            assignment_of(&home, citizen),
            Some(applied),
            "the citizen keeps its job: the region owns that truth, not the cache"
        );
        home_apply_accepted_employment(&mut home, &rebuilt);
        assert_eq!(
            assignment_of(&home, citizen),
            Some(applied),
            "and an empty cache does not clear it either"
        );
    }

    // ---- P5: release and invalidation ----

    /// Runs the whole flow through P4: the citizen is accepted, contracted, and
    /// has applied its assignment.
    fn a_worker_employed_across_the_border() -> (
        Arc<EmploymentDirectory>,
        RegionRuntime,
        RegionRuntime,
        Entity,
        Entity,
    ) {
        let (directory, employer, mut home) = accepted_but_not_yet_applied();
        home_apply_accepted_employment(&mut home, &directory);
        let citizen = only_citizen(&home);
        let workplace = assignment_of(&home, citizen).expect("applied").workplace;
        assert_eq!(employer.state().contract_holders_at(workplace).len(), 1);
        (directory, employer, home, citizen, workplace)
    }

    #[test]
    fn an_explicit_release_frees_the_seat_only_after_the_employer_confirms() {
        // P5 behavior allowed: "explicit home release clears home assignment
        // first, then employer confirms capacity."
        let (directory, mut employer, mut home, citizen, workplace) =
            a_worker_employed_across_the_border();

        let outbound = home_release_job(&mut home, &directory, citizen);
        assert_eq!(wake_targets(&outbound), vec![RegionId(9)]);
        assert!(
            assignment_of(&home, citizen).is_none(),
            "the home clears its own truth first"
        );
        assert_eq!(
            employer.state().contract_holders_at(workplace).len(),
            1,
            "but the employer still holds the seat until it confirms"
        );
        assert!(
            directory
                .snapshot()
                .active_citizens_by_home_region
                .get(&RegionId(1))
                .is_some_and(|active| active.contains(&citizen)),
            "and the citizen cannot claim a second job mid-release"
        );

        employer_apply_releases(&mut employer, &directory);

        assert!(
            employer.state().contract_holders_at(workplace).is_empty(),
            "the employer dropped the contract"
        );
        assert!(
            directory
                .snapshot()
                .active_citizens_by_home_region
                .get(&RegionId(1))
                .is_none_or(|active| !active.contains(&citizen)),
            "and only now is the citizen free to claim again"
        );
    }

    #[test]
    fn republishing_after_a_confirmed_release_is_a_no_op() {
        // The same convergence property P3/P4 rely on, now for the release path:
        //   directory cached open_count = published + 1   (confirm_release)
        //   employer's next published   = spare - contracted (one fewer contract)
        // Those agree, so the republish is UNCHANGED -- no generation bump, and
        // therefore no churn of any other pool's still-valid pending claims.
        let (directory, mut employer, mut home, citizen, _workplace) =
            a_worker_employed_across_the_border();

        home_release_job(&mut home, &directory, citizen);
        employer_apply_releases(&mut employer, &directory);

        assert!(
            !directory.publish_pools(RegionId(9), employer.state().published_job_pools()),
            "the post-release republish must be a no-op, not a 'changed' pool"
        );
    }

    #[test]
    fn a_released_seat_can_be_claimed_again() {
        // End to end: release, confirm, and the same citizen wins the seat back.
        let (directory, mut employer, mut home, citizen, workplace) =
            a_worker_employed_across_the_border();
        let discovery = shared_component(&home, &employer);

        home_release_job(&mut home, &directory, citizen);
        employer_apply_releases(&mut employer, &directory);
        assert!(employer.state().contract_holders_at(workplace).is_empty());

        // The employer republishes its (now roomier) pools, and the citizen
        // claims again.
        employer_publish_pools(&mut employer, &directory);
        assert!(!home_region_daily_jobs(&mut home, &directory, &discovery).is_empty());
        employer_validate_claims(&mut employer, &directory);
        home_apply_accepted_employment(&mut home, &directory);

        assert_eq!(
            employer.state().contract_holders_at(workplace).len(),
            1,
            "the freed seat is contracted again"
        );
        assert!(assignment_of(&home, citizen).is_some());
    }

    #[test]
    fn a_release_racing_an_employer_loss_strands_nothing() {
        // employer_apply_releases DROPS a release it cannot match. The only way
        // the contract is already gone is that the employer lost it and reported
        // that loss -- which already cleared the accepted cache. Prove the
        // citizen is not left stranded as "active" and unable to re-claim.
        let (directory, mut employer, mut home, citizen, workplace) =
            a_worker_employed_across_the_border();

        // Home asks to release...
        home_release_job(&mut home, &directory, citizen);
        // ...but the employer independently loses the whole workplace first.
        assert!(employer.state_mut().bulldoze(1, 1).success);
        employer.ensure_derived_state();
        employer_publish_pools(&mut employer, &directory);
        assert!(employer.state().contract_holders_at(workplace).is_empty());

        // The queued release can no longer be matched, so it is dropped.
        employer_apply_releases(&mut employer, &directory);

        let snapshot = directory.snapshot();
        assert!(
            snapshot.accepted_by_home_region.is_empty(),
            "the loss already cleared the accepted cache -- nothing is stranded"
        );
        assert!(
            snapshot
                .active_citizens_by_home_region
                .get(&RegionId(1))
                .is_none_or(|active| !active.contains(&citizen)),
            "so the citizen is free to claim again"
        );

        // And the home's loss queue drains harmlessly: it already cleared itself.
        home_apply_losses(&mut home, &directory);
        assert!(assignment_of(&home, citizen).is_none());
    }

    #[test]
    fn a_bulldozed_workplace_reports_an_explicit_loss_that_the_home_applies() {
        // P5 scope: "employer loss reporting sends JobLoss to the home region",
        // "home loss handling clears assignment only if it still matches".
        let (directory, mut employer, mut home, citizen, workplace) =
            a_worker_employed_across_the_border();

        assert!(employer.state_mut().bulldoze(1, 1).success);
        employer.ensure_derived_state();

        let outbound = employer_publish_pools(&mut employer, &directory);
        assert_eq!(
            wake_targets(&outbound),
            vec![RegionId(1)],
            "the home region is woken with a loss"
        );
        assert!(
            employer.state().contract_holders_at(workplace).is_empty(),
            "the employer dropped the contract it can no longer honour"
        );
        assert!(
            assignment_of(&home, citizen).is_some(),
            "the home is not cleared until it applies the loss"
        );

        home_apply_losses(&mut home, &directory);
        assert!(
            assignment_of(&home, citizen).is_none(),
            "the employer-confirmed loss clears the home assignment"
        );
    }

    #[test]
    fn a_stable_worker_keeps_the_job_across_unrelated_republishes() {
        // P5 review check: "stable workers keep being paid until explicit release
        // or employer-confirmed loss." P5 behavior forbidden: "do not infer
        // accepted job loss from missing snapshot rows" -- note the workplace is
        // fully contracted here, so it publishes NO pool row at all.
        let (directory, mut employer, home, citizen, workplace) =
            a_worker_employed_across_the_border();

        for _ in 0..3 {
            let outbound = employer_publish_pools(&mut employer, &directory);
            assert!(
                wake_targets(&outbound).is_empty(),
                "a healthy republish reports no loss"
            );
        }

        assert_eq!(
            employer.state().contract_holders_at(workplace).len(),
            1,
            "the contract survives"
        );
        assert!(
            assignment_of(&home, citizen).is_some(),
            "and so does the home assignment it is paid from"
        );
        assert!(
            directory
                .snapshot()
                .accepted_by_home_region
                .contains_key(&RegionId(1)),
            "the accepted lease is untouched"
        );
    }

    #[test]
    fn a_stale_loss_never_clears_a_job_the_citizen_moved_to() {
        // P5 behavior forbidden: "do not clear a home assignment if the citizen
        // already moved to a different workplace."
        let (directory, mut employer, mut home, citizen, workplace) =
            a_worker_employed_across_the_border();

        // The employer loses the contract and reports it...
        assert!(employer.state_mut().bulldoze(1, 1).success);
        employer.ensure_derived_state();
        employer_publish_pools(&mut employer, &directory);

        // ...but before the home applies the loss, the citizen takes another job.
        let elsewhere = WorkplaceAssignment {
            workplace: Entity::new(RegionId(1), 7),
            location: CityCellRef::local(RegionId(1), 2, 2),
            salary: 5,
        };
        home.state_mut().clear_employment(citizen);
        assert!(
            home.state_mut()
                .apply_workplace_assignment(citizen, elsewhere)
        );

        home_apply_losses(&mut home, &directory);
        assert_eq!(
            assignment_of(&home, citizen),
            Some(elsewhere),
            "the stale loss names the old workplace, so it must not clear the new job"
        );
        let _ = workplace;
    }

    #[test]
    fn the_wake_handler_runs_release_and_loss_work_through_the_real_event_path() {
        // P5 scope: "EmploymentDirectoryReady wakes both employer release work
        // and home loss work."
        let (directory, mut employer, mut home, citizen, workplace) =
            a_worker_employed_across_the_border();
        home_release_job(&mut home, &directory, citizen);

        employer.set_employment_directory(Arc::clone(&directory));
        employer.push_event(RegionEvent::EmploymentDirectoryReady);
        employer.process_next_event();
        assert!(
            employer.state().contract_holders_at(workplace).is_empty(),
            "the employer's wake confirmed the release"
        );

        // Now the employer loses a (re-created) contract and the home's own wake
        // must apply the loss.
        let assignment = employer
            .state_mut()
            .accept_claim_and_create_assignment(&JobClaim {
                claim_id: JobClaimId(77),
                citizen: CitizenRef {
                    region: RegionId(1),
                    citizen,
                },
                workplace,
                generation: 1,
            });
        assert!(
            home.state_mut()
                .apply_workplace_assignment(citizen, assignment)
        );
        assert!(employer.state_mut().bulldoze(1, 1).success);
        employer.ensure_derived_state();
        employer_publish_pools(&mut employer, &directory);

        home.set_employment_directory(Arc::clone(&directory));
        home.push_event(RegionEvent::EmploymentDirectoryReady);
        home.process_next_event();
        assert!(
            assignment_of(&home, citizen).is_none(),
            "the home's wake applied the employer-confirmed loss"
        );
    }
}
