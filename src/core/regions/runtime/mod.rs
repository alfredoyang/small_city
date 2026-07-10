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
//! Region A needs power          Worker routing             Region B has spare power
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
//!                              Route releases first
//!                              Route request by topology
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
//!   A emits release(request=11)
//!   B removes old A allocation before routing new requests
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

use std::sync::Arc;

use crate::core::city_refs::CityCellRef;
use crate::core::components::TravelerHandoff;
use crate::core::entity::Entity;
use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionCommandResponse, RegionSnapshotResponse,
    RegionTickResponse, RegionViewSnapshot, UiRequestId,
};
use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::employment_directory::{
    CitizenRef, EmploymentDirectory, JobClaimDecision, choose_best_pool,
};
use crate::core::regions::handle::{RegionEventReceiver, RegionHandle, mailbox};
use crate::core::regions::{
    ExitLink, GoodsExportGrant, JobExportGrant, PendingGoodsDemand, PendingJobDemand,
    PendingPowerDemand, PowerExportGrant, RegionId, RegionRoadNetworkId, RegionState,
    RegionalTickGoodsPhase, RegionalTickJobPhase, RegionalTickPowerPhase,
};
use crate::core::world::CrossRegionGoodsRoutes;
use crate::interface::input::MapOverlayInput;

#[derive(Debug)]
/// Event owned by one region runtime inbox.
pub enum RegionEvent {
    /// Advance this region's local deterministic simulation by one tick.
    Tick { request_id: UiRequestId },
    /// Build an owned UI-safe snapshot through the region event loop.
    BuildSnapshot {
        request_id: UiRequestId,
        overlay: MapOverlayInput,
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
        request: PowerExportRequest,
        grant: PowerExportGrant,
    },
    /// Authoritative producer-side job-slot export allocation request.
    ProcessJobExportRequest(JobExportAllocationRequest),
    /// Producer-side release for a caller's previous job export allocations.
    ReleaseJobExportAllocations(JobExportAllocationRelease),
    /// Caller-side job export grant result.
    ///
    /// Retire-tickstate, P-c: the reply carries the request it answers,
    /// same shape as `ApplyPowerExportGrant` (P-a).
    ApplyJobExportGrant {
        request: JobExportRequest,
        grant: JobExportGrant,
    },
    /// Authoritative producer-side goods export allocation request.
    ProcessGoodsExportRequest(GoodsExportAllocationRequest),
    /// Producer-side release for a caller's previous goods export allocations.
    ReleaseGoodsExportAllocations(GoodsExportAllocationRelease),
    /// Caller-side goods export grant result.
    ///
    /// Retire-tickstate, P-d: the reply carries the request it answers,
    /// same shape as power/jobs.
    ApplyGoodsExportGrant {
        request: GoodsExportRequest,
        grant: GoodsExportGrant,
    },
    /// P5b: a cross-region travel token handed in by a neighbor region (fire and
    /// forget — no grant, no tick pause).
    ReceiveTraveler(TravelerHandoff),
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
    /// buffers as `TravelerHandedOff` for the barrier to route.
    StepTravel,
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
/// Consumer request for a producer to export one workplace slot for a job seeker.
///
/// Unlike power there is no demand amount: one citizen fills one whole slot.
pub struct JobExportRequest {
    pub request_id: UiRequestId,
    pub caller_region: RegionId,
    pub caller_network: RegionRoadNetworkId,
    pub token: u32,
    /// Retire-tickstate, P-c: echoed back by the grant reply so the caller
    /// can re-derive the demand it answers without keeping a demand list.
    pub citizen: Entity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Consumer request for a producer to export a whole batch of goods units.
pub struct GoodsExportRequest {
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
/// Producer-side job allocation request (one consumer request + candidates).
pub type JobExportAllocationRequest = ExportAllocationRequest<JobExportRequest>;
/// Producer-side goods allocation request (one consumer request + candidates).
pub type GoodsExportAllocationRequest = ExportAllocationRequest<GoodsExportRequest>;

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
/// Job flavor of the shared export allocation release.
pub type JobExportAllocationRelease = ExportAllocationRelease;
/// Goods flavor of the shared export allocation release.
pub type GoodsExportAllocationRelease = ExportAllocationRelease;

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

impl ExportRequestKey for JobExportRequest {
    fn allocation_key(&self) -> ExportAllocationKey {
        ExportAllocationKey {
            caller_region: self.caller_region,
            request_id: self.request_id,
            token: self.token,
        }
    }
}

impl ExportRequestKey for GoodsExportRequest {
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
    PowerExportRequested(PowerExportRequest),
    PowerExportRequestCompleted {
        request: PowerExportAllocationRequest,
        grant: PowerExportGrant,
    },
    PowerExportAllocationsReleased(PowerExportAllocationRelease),
    JobExportRequested(JobExportRequest),
    JobExportRequestCompleted {
        request: JobExportAllocationRequest,
        grant: JobExportGrant,
    },
    JobExportAllocationsReleased(JobExportAllocationRelease),
    GoodsExportRequested(GoodsExportRequest),
    GoodsExportRequestCompleted {
        request: GoodsExportAllocationRequest,
        grant: GoodsExportGrant,
    },
    GoodsExportAllocationsReleased(GoodsExportAllocationRelease),
    /// P5b: a travel token to route to `handoff.to_region` (worker delivers it as
    /// `RegionEvent::ReceiveTraveler`).
    TravelerHandedOff(TravelerHandoff),
    /// Directory employment ledger plan, P3: wake `target_region` so it pulls
    /// its employment work from the directory. Payload-free by design; the
    /// worker delivers it as `RegionEvent::EmploymentDirectoryReady`.
    EmploymentDirectoryReady {
        target_region: RegionId,
        source_region: RegionId,
    },
    RuntimeError(RegionRuntimeError),
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    power_export_allocations: ExportAllocations<i32>,
    job_export_allocations: ExportAllocations<Entity>,
    goods_export_allocations: ExportAllocations<u32>,
    power_export_producers: Vec<RegionId>,
    job_export_producers: Vec<RegionId>,
    goods_export_producers: Vec<RegionId>,
    pending_goods_stock: Vec<(Entity, u32)>,
    // Retire-tickstate, P-a: the only caller-side memory power needs now that
    // nothing pauses. A reply echoes the request it answers (its own
    // `request_id`), so comparing against this one scalar tells "my current
    // batch" from "a superseded one" -- no continuation, no demand list.
    // Starts at UiRequestId(0), a sentinel no real UI-driven tick ever mints
    // (RegionalGame's counter starts at 1), so the very first reply this
    // runtime ever sees compares correctly.
    current_power_request_id: UiRequestId,
    // Retire-tickstate, P-c: same shape and same reasoning, for jobs.
    current_job_request_id: UiRequestId,
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
    // Event-driven plan, P-4: same shape as `seen_power_generation`, but for
    // the job reconcile gate — a separate marker so the hourly power gate
    // (which updates its own marker every hour) can never absorb a bump or
    // local change destined for the daily job gate.
    seen_jobs_generation: u64,
    // Event-driven plan, P-5: same shape, for the goods reconcile gate.
    seen_goods_generation: u64,
    // Directory employment ledger plan, P3: installed once per worker slice
    // (`set_employment_directory`), mirroring how `set_discovery_generation`
    // and `set_region_routes` hand this runtime the pass's shared data. `None`
    // until a worker installs one, so a bare `RegionRuntime::new` still works
    // and an `EmploymentDirectoryReady` without a directory is a no-op.
    employment_directory: Option<Arc<EmploymentDirectory>>,
    handle: RegionHandle,
    receiver: RegionEventReceiver,
}

// CR3R — one reservation engine shared by power and jobs.
//
// Power and jobs once carried two byte-for-byte copies of the producer-side
// reservation bookkeeping. CR3R keeps ONE generic engine and varies only the
// reserved unit `U`: power reserves an `i32` demand, jobs reserve an `Entity`
// slot. The transport/lifecycle (keying, staleness, upsert) is identical; only
// "how do I read available capacity out of these units" stays resource-specific.
//
//                    ExportAllocations<U>                 (one engine, two U's)
//   ┌───────────────────────────────────────────────────────────────────────┐
//   │ Vec<ExportAllocation<U>>                                                │
//   │   each = { key: ExportAllocationKey,   ← (caller_region, gen, token)    │
//   │           network: RegionRoadNetworkId,                                 │
//   │           unit: U,                     ← i32 (power) | Entity (jobs)     │
//   │           caller_generation }                                           │
//   │                                                                         │
//   │ shared lifecycle:  upsert · release_stale_for_caller · units            │
//   │ shared read:       reserved_units_excluding(key, network)  ← power      │
//   │                    reserved_units_excluding_key(key)        ← jobs      │
//   └───────────────────────────────────────────────────────────────────────┘
//          ▲ instantiated as                          ▲ instantiated as
//          power_export_allocations: ExportAllocations<i32>
//          job_export_allocations:   ExportAllocations<Entity>
//
//   resource-specific (NOT shared) — how units become "available capacity":
//     power → sum reserved demand, subtract from per-network remaining (scalar)
//     jobs  → remove each reserved slot Entity from the spare-slot set (discrete)
//
// The two `reserved_units_*` readers above are deliberately different and that
// difference is the whole point: power capacity is POOLED PER NETWORK, so a
// reservation on one network must not shrink another's; a job slot is ONE global
// `Entity` that can be adjacent to two networks, so it must be excluded globally.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Identity of one caller demand a producer has reserved for, shared by power and
/// jobs: a `(caller_region, generation, token)` triple.
struct ExportAllocationKey {
    caller_region: RegionId,
    request_id: UiRequestId,
    token: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One producer-owned transient reservation of a reserved unit `U` on a network.
///
/// `U` is the resource's reserved unit: `i32` demand for power, `Entity` slot for
/// jobs. `caller_generation` lets a producer drop a caller's reservations once the
/// caller starts a new tick generation.
struct ExportAllocation<U> {
    key: ExportAllocationKey,
    network: RegionRoadNetworkId,
    unit: U,
    caller_generation: UiRequestId,
}

#[derive(Debug)]
/// Producer-side reservation bookkeeping shared by power and jobs (CR3R).
///
/// This carries only the transport/lifecycle that both resources share. How
/// available capacity is computed from the reserved units stays resource-specific
/// (a scalar remaining for power, a discrete spare set for jobs).
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

    /// Reserved units held by callers other than `key`, across *all* networks.
    ///
    /// Jobs use this network-agnostic view: the reserved unit is a global workplace
    /// `Entity`, and one physical slot can be adjacent to two disconnected road
    /// networks (a "bridge" workplace). A reservation taken under one network must
    /// still block that same slot when requested via the other, or the producer
    /// would double-grant one slot. Excluding `key` lets a caller's own retry
    /// re-grant.
    fn reserved_units_excluding_key(
        &self,
        key: ExportAllocationKey,
    ) -> impl Iterator<Item = U> + '_ {
        self.allocations
            .iter()
            .filter(move |allocation| allocation.key != key)
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
            job_export_allocations: ExportAllocations::new(),
            goods_export_allocations: ExportAllocations::new(),
            power_export_producers: Vec::new(),
            job_export_producers: Vec::new(),
            goods_export_producers: Vec::new(),
            pending_goods_stock: Vec::new(),
            current_power_request_id: UiRequestId(0),
            current_job_request_id: UiRequestId(0),
            current_goods_request_id: UiRequestId(0),
            discovery_generation: 0,
            seen_power_generation: 0,
            seen_jobs_generation: 0,
            seen_goods_generation: 0,
            employment_directory: None,
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
    /// `&mut` read boundary. Remote assignments are set in the job-export tick, not
    /// the derived pass, so this is a no-op for the returned data today; it stays
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
            RegionEvent::RunCommand {
                request_id,
                command,
            } => {
                let response = self.run_command(request_id, command);
                vec![OutboundMessage::RegionCommandCompleted(response)]
            }
            RegionEvent::SettlePowerImports { request_id } => {
                self.start_power_import_settlement(request_id)
            }
            RegionEvent::ProcessPowerExportRequest(request) => {
                let grant = self.process_power_export_request(&request);
                vec![OutboundMessage::PowerExportRequestCompleted { request, grant }]
            }
            RegionEvent::ReleasePowerExportAllocations(release) => {
                self.power_export_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyPowerExportGrant { request, grant } => {
                self.apply_power_export_grant(request, grant)
            }
            RegionEvent::ProcessJobExportRequest(request) => {
                let grant = self.process_job_export_request(&request);
                vec![OutboundMessage::JobExportRequestCompleted { request, grant }]
            }
            RegionEvent::ReleaseJobExportAllocations(release) => {
                self.job_export_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyJobExportGrant { request, grant } => {
                self.apply_job_export_grant(request, grant)
            }
            RegionEvent::ProcessGoodsExportRequest(request) => {
                let grant = self.process_goods_export_request(&request);
                vec![OutboundMessage::GoodsExportRequestCompleted { request, grant }]
            }
            RegionEvent::ReleaseGoodsExportAllocations(release) => {
                self.goods_export_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyGoodsExportGrant { request, grant } => {
                self.apply_goods_export_grant(request, grant)
            }
            RegionEvent::ReceiveTraveler(handoff) => self
                .state
                .receive_traveler_handoff(handoff)
                .into_iter()
                .map(OutboundMessage::TravelerHandedOff)
                .collect(),
            RegionEvent::PowerCapacityRecheck { request_id, .. } => {
                self.power_capacity_recheck(request_id)
            }
            RegionEvent::StepTravel => {
                // P7c: one movement sub-tick, then drain the crossings it buffered
                // so the barrier routes them to neighbours for the next sub-tick.
                self.state.step_travel();
                self.drained_traveler_handoff_messages()
            }
            RegionEvent::EmploymentDirectoryReady => self.handle_employment_directory_ready(),
        }
    }

    /// P3: pull whatever employment work the directory holds for this region.
    ///
    /// The plan's handler also calls `employer_apply_releases`,
    /// `home_apply_accepted_employment`, and `home_apply_losses` — those are
    /// P4/P5 scope and are deliberately absent here. P3 wires only the
    /// employer-side validation half.
    ///
    /// A runtime with no directory installed (a bare `RegionRuntime::new`, or
    /// a worker that never set one) treats the wake as a no-op rather than
    /// panicking.
    fn handle_employment_directory_ready(&mut self) -> Vec<OutboundMessage> {
        let Some(directory) = self.employment_directory.clone() else {
            return Vec::new();
        };
        employer_validate_claims(self, &directory)
    }

    /// P3: the wake fan-out. One payload-free message per target region; the
    /// worker routes them through the same deterministic barrier every other
    /// cross-region event uses.
    fn emit_employment_directory_ready(&self, regions: Vec<RegionId>) -> Vec<OutboundMessage> {
        let source_region = self.region_id();
        regions
            .into_iter()
            .map(|target_region| OutboundMessage::EmploymentDirectoryReady {
                target_region,
                source_region,
            })
            .collect()
    }

    /// P5b: this tick's buffered crossings, routed by the worker.
    fn drained_traveler_handoff_messages(&mut self) -> Vec<OutboundMessage> {
        self.state
            .drain_traveler_handoffs()
            .into_iter()
            .map(OutboundMessage::TravelerHandedOff)
            .collect()
    }

    fn remember_power_export_producer(&mut self, grant: &PowerExportGrant) {
        if grant.granted {
            if let Some(source_region) = grant.source_region {
                insert_sorted_unique(&mut self.power_export_producers, source_region);
            }
        }
    }

    fn remember_job_export_producer(&mut self, grant: &JobExportGrant) {
        if grant.granted {
            // The granting producer is the owner of the granted workplace ref.
            if let Some(workplace) = grant.workplace {
                insert_sorted_unique(&mut self.job_export_producers, workplace.region());
            }
        }
    }

    fn remember_goods_export_producer(&mut self, grant: &GoodsExportGrant) {
        if grant.granted {
            if let Some(source_region) = grant.source_region {
                insert_sorted_unique(&mut self.goods_export_producers, source_region);
            }
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
        let mut outbound = vec![OutboundMessage::PowerExportAllocationsReleased(
            PowerExportAllocationRelease {
                caller_region: self.region_id(),
                request_id,
                producer_regions,
            },
        )];
        outbound.extend(demands.iter().map(|demand| {
            OutboundMessage::PowerExportRequested(PowerExportRequest {
                request_id,
                caller_region: self.region_id(),
                caller_network: demand.caller_network,
                token: demand.token,
                demand: demand.demand,
                consumer: demand.consumer,
            })
        }));
        outbound
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

    /// Retire-tickstate, P-c: jobs no longer pause the tick either (same
    /// cutover as P-a for power). Fires the release/request (when dirty)
    /// and moves straight into the goods phase in the same pass; whichever
    /// reply lands later is applied whenever it arrives
    /// (`apply_job_export_grant`, gated on `current_job_request_id`), for
    /// the *next* tick to see.
    ///
    /// Self-dirtying-loop fix (caught while implementing this cutover, not
    /// in the original plan text): applying a grant no longer re-dirties
    /// the gate (`World::refresh_jobs_cache_after_grant_applied`), so once
    /// a remote assignment settles, the next day reads quiet and leaves it
    /// alone — exactly like power's quiet path leaving `powered` alone.
    /// Without this, the daily wipe would re-run every day even for a
    /// citizen whose remote job was already granted, permanently starving
    /// them of salary (economy also only runs on the daily boundary, right
    /// after the wipe).
    ///
    /// Only the discovery-generation half of the gate is read here, before
    /// `continue_tick_to_job_demand_phase` runs: the OTHER half
    /// (`jobs_exports_dirty`) is checked fresh, inside that call, AFTER
    /// population growth — a citizen population growth spawns THIS tick
    /// must not wait an extra day to be noticed (caught in review). The
    /// effective, combined answer comes back via `phase.jobs_dirty()`.
    fn enter_job_phase(
        &mut self,
        request_id: UiRequestId,
        power_phase: RegionalTickPowerPhase,
    ) -> Vec<OutboundMessage> {
        let discovery_dirty = self.discovery_generation > self.seen_jobs_generation;
        let phase = self
            .state
            .continue_tick_to_job_demand_phase(power_phase, discovery_dirty);
        if !phase.is_daily() {
            // Jobs resolve only on a daily boundary; an hourly tick neither makes
            // nor invalidates job reservations, so it sends no release or requests.
            let response = self.finish_job_phase(request_id, phase);
            let mut outbound = vec![OutboundMessage::RegionTickCompleted(response)];
            outbound.extend(self.drained_traveler_handoff_messages());
            return outbound;
        }
        let mut outbound = if phase.jobs_dirty() {
            self.seen_jobs_generation = self.discovery_generation;
            self.state.clear_jobs_exports_dirty();
            self.release_and_request_job(request_id, &phase.job_demands)
        } else {
            // Quiet daily: existing assignments (local AND remote) and the
            // producer's ledger all persist untouched -- no wipe, no
            // release, no requests. Mirrors power's quiet path (P-6):
            // trust the gate, skip the recompute entirely.
            Vec::new()
        };
        outbound.extend(self.enter_goods_phase(request_id, phase));
        outbound
    }

    /// Release this caller's previous-generation job reservations, then
    /// request the current demand batch. Fire-and-forget, mirrors
    /// `release_and_request_power` exactly (P-a): stamps
    /// `current_job_request_id` so a later reply can tell "my current
    /// batch" from "a superseded one" (see `apply_job_export_grant`), and
    /// returns immediately without waiting for any reply.
    fn release_and_request_job(
        &mut self,
        request_id: UiRequestId,
        demands: &[PendingJobDemand],
    ) -> Vec<OutboundMessage> {
        self.current_job_request_id = request_id;
        let producer_regions = std::mem::take(&mut self.job_export_producers);
        let mut outbound = vec![OutboundMessage::JobExportAllocationsReleased(
            JobExportAllocationRelease {
                caller_region: self.region_id(),
                request_id,
                producer_regions,
            },
        )];
        outbound.extend(demands.iter().map(|demand| {
            OutboundMessage::JobExportRequested(JobExportRequest {
                request_id,
                caller_region: self.region_id(),
                caller_network: demand.caller_network,
                token: demand.token,
                citizen: demand.citizen,
            })
        }));
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
        if !dirty {
            // Quiet: existing grants + the producer's ledger persist; no
            // release, no requests.
            let response = self.finish_goods_phase(request_id, phase);
            let mut outbound = vec![OutboundMessage::RegionTickCompleted(response)];
            outbound.extend(self.drained_traveler_handoff_messages());
            return outbound;
        }
        self.seen_goods_generation = self.discovery_generation;
        self.state.clear_goods_exports_dirty();
        let mut outbound = self.release_and_request_goods(request_id, &phase.goods_demands);
        let response = self.finish_goods_phase(request_id, phase);
        outbound.push(OutboundMessage::RegionTickCompleted(response));
        outbound.extend(self.drained_traveler_handoff_messages());
        outbound
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
        let producer_regions = std::mem::take(&mut self.goods_export_producers);
        let mut outbound = vec![OutboundMessage::GoodsExportAllocationsReleased(
            GoodsExportAllocationRelease {
                caller_region: self.region_id(),
                request_id,
                producer_regions,
            },
        )];
        outbound.extend(demands.iter().map(|demand| {
            OutboundMessage::GoodsExportRequested(GoodsExportRequest {
                request_id,
                caller_region: self.region_id(),
                caller_network: demand.caller_network,
                token: demand.token,
                units: demand.units,
                commercial: demand.commercial,
            })
        }));
        outbound
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
        // finish ticks independently with no cross-region economy barrier. A
        // producer with no local job seekers finishes its own job phase in the same
        // pass as its Tick -- often before a fresh consumer request reaches it -- so
        // that day's economy uses the *previous* day's reservations (they persist
        // until the caller's next-generation release). This is deterministic and
        // self-correcting in steady state; pairing salary and tax on the same day
        // would require a global "all exports resolved" barrier before any economy
        // runs, which is far more synchronization for little gain.
        let exported_job_slots = self.job_export_allocations.units().collect::<Vec<_>>();
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
    ) -> RegionTickResponse {
        let exported_job_slots = self.job_export_allocations.units().collect::<Vec<_>>();
        let exported_goods_units = self.goods_export_allocations.units().sum();
        RegionTickResponse {
            request_id,
            region_id: self.region_id(),
            result: self.state.finish_tick_goods_demand_phase(
                phase,
                &exported_job_slots,
                exported_goods_units,
            ),
        }
    }

    fn process_power_export_request(
        &mut self,
        request: &PowerExportAllocationRequest,
    ) -> PowerExportGrant {
        let producer_network = request.candidates[request.candidate_index];
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

    fn process_job_export_request(
        &mut self,
        request: &JobExportAllocationRequest,
    ) -> JobExportGrant {
        let producer_network = request.candidates[request.candidate_index];
        let allocation_key = export_allocation_key(&request.request);
        self.job_export_allocations
            .release_stale_for_caller(request.request.caller_region, request.request.request_id);

        // Producer-owned spare: slots on this network minus those already reserved
        // by other active allocations (one slot occurrence per reservation). The
        // exclusion spans all networks, not just `producer_network`: a workplace
        // bridging two disconnected networks is one physical slot, so a reservation
        // taken via either network must block it here. This key's own reservation,
        // if any, is left in so a retry re-grants a slot.
        let mut available = self.state.spare_job_slots_on_network(producer_network);
        for reserved in self
            .job_export_allocations
            .reserved_units_excluding_key(allocation_key)
        {
            if let Some(index) = available.iter().position(|slot| *slot == reserved) {
                available.remove(index);
            }
        }

        let Some(workplace) = available.first().copied() else {
            return JobExportGrant {
                token: request.request.token,
                granted: false,
                workplace: None,
                location: None,
                salary: 0,
            };
        };
        let salary = self.state.workplace_salary(workplace);
        let region = self.region_id();
        let location = self
            .state
            .workplace_position(workplace)
            .map(|position| CityCellRef::local(region, position.x, position.y));

        self.job_export_allocations.upsert(
            allocation_key,
            producer_network,
            workplace,
            request.request.request_id,
        );

        JobExportGrant {
            token: request.request.token,
            granted: true,
            workplace: Some(workplace),
            location,
            salary,
        }
    }

    fn process_goods_export_request(
        &mut self,
        request: &GoodsExportAllocationRequest,
    ) -> GoodsExportGrant {
        let producer_network = request.candidates[request.candidate_index];
        let allocation_key = export_allocation_key(&request.request);
        self.goods_export_allocations
            .release_stale_for_caller(request.request.caller_region, request.request.request_id);

        let active_export_allocations: u32 = self
            .goods_export_allocations
            .reserved_units_excluding(allocation_key, producer_network)
            .sum();
        let remaining = self
            .state
            .goods_network_remaining_units(producer_network)
            .saturating_sub(active_export_allocations);

        if remaining < request.request.units {
            return GoodsExportGrant {
                token: request.request.token,
                granted: false,
                source_region: None,
                units: 0,
            };
        }

        self.goods_export_allocations.upsert(
            allocation_key,
            producer_network,
            request.request.units,
            request.request.request_id,
        );

        GoodsExportGrant {
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
                vec![OutboundMessage::PowerExportAllocationsReleased(
                    PowerExportAllocationRelease {
                        caller_region,
                        request_id: current_request_id,
                        producer_regions: vec![producer],
                    },
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Retire-tickstate, P-c: same shape as `apply_power_export_grant`
    /// (P-a) — no continuation to consult, one staleness check against
    /// `current_job_request_id` tells "my current batch" from "a
    /// superseded one."
    fn apply_job_export_grant(
        &mut self,
        request: JobExportRequest,
        grant: JobExportGrant,
    ) -> Vec<OutboundMessage> {
        // Same producer-owned allocation invariant as power: a granted reply
        // means the producer may hold a transient slot allocation even if this
        // caller no longer applies it locally.
        self.remember_job_export_producer(&grant);
        if request.request_id != self.current_job_request_id {
            // Same reasoning as power's stale-granted-release fix: a stale
            // but granted reply reserved producer capacity that no future
            // release is guaranteed to reach (this caller has already moved
            // past that generation). Release it now instead of leaving it
            // stuck.
            return Self::release_stale_granted_job(
                self.region_id(),
                self.current_job_request_id,
                &grant,
            );
        }
        let demand = PendingJobDemand {
            token: request.token,
            citizen: request.citizen,
            caller_network: request.caller_network,
        };
        self.state.apply_job_export_grant(demand, grant);
        Vec::new()
    }

    /// A stale but *granted* job reply reserved a producer's workplace slot
    /// that no future release is guaranteed to reach. Release it now. A
    /// stale denial reserved nothing, so it needs no release.
    fn release_stale_granted_job(
        caller_region: RegionId,
        current_request_id: UiRequestId,
        grant: &JobExportGrant,
    ) -> Vec<OutboundMessage> {
        match grant.workplace {
            Some(workplace) if grant.granted => {
                vec![OutboundMessage::JobExportAllocationsReleased(
                    JobExportAllocationRelease {
                        caller_region,
                        request_id: current_request_id,
                        producer_regions: vec![workplace.region()],
                    },
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Retire-tickstate, P-d: same shape as power/jobs. The reply carries
    /// the request it answers, so a single request-id check replaces the
    /// old goods continuation and pending demand list.
    fn apply_goods_export_grant(
        &mut self,
        request: GoodsExportRequest,
        grant: GoodsExportGrant,
    ) -> Vec<OutboundMessage> {
        self.remember_goods_export_producer(&grant);
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
            self.pending_goods_stock
                .push((request.commercial, grant.units));
        }
        Vec::new()
    }

    /// A stale but *granted* goods reply reserved producer stock that no
    /// future release is guaranteed to reach. A stale denial reserved
    /// nothing, so it needs no release.
    fn release_stale_granted_goods(
        caller_region: RegionId,
        current_request_id: UiRequestId,
        grant: &GoodsExportGrant,
    ) -> Vec<OutboundMessage> {
        match grant.source_region {
            Some(producer) if grant.granted => {
                vec![OutboundMessage::GoodsExportAllocationsReleased(
                    GoodsExportAllocationRelease {
                        caller_region,
                        request_id: current_request_id,
                        producer_regions: vec![producer],
                    },
                )]
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
#[allow(dead_code)] // P3: staged; the daily job phase starts calling this in P4/P7.
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

#[cfg(test)]
mod tick_state_tests {
    //! Unit tests for the runtime event loop's fire-and-forget export flows
    //! (retire-tickstate P-a/P-c/P-d) and producer-side allocation handling.

    use super::*;
    use crate::core::regions::RegionState;
    use crate::interface::input::BuildingKind;

    // A residential consumer next to a border road has no local power and one
    // exportable demand.
    fn consumer_runtime(region_id: RegionId) -> RegionRuntime {
        let mut region = RegionState::new(region_id, 2, 2);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(1, 0, BuildingKind::Road).success);
        RegionRuntime::new(region)
    }

    fn export_requests(outbound: &[OutboundMessage]) -> Vec<&PowerExportRequest> {
        outbound
            .iter()
            .filter_map(|message| match message {
                OutboundMessage::PowerExportRequested(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    fn goods_export_requests(outbound: &[OutboundMessage]) -> Vec<&GoodsExportRequest> {
        outbound
            .iter()
            .filter_map(|message| match message {
                OutboundMessage::GoodsExportRequested(request) => Some(request),
                _ => None,
            })
            .collect()
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
        assert!(matches!(
            outbound.first(),
            Some(OutboundMessage::PowerExportAllocationsReleased(_))
        ));
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
        assert!(matches!(
            outbound.first(),
            Some(OutboundMessage::PowerExportAllocationsReleased(_))
        ));
    }

    #[test]
    fn tick_without_exportable_demand_finishes_immediately() {
        // Event-driven plan, P-2: an empty, never-touched region starts with
        // both `power_exports_dirty` and the seen/current generation clean
        // (0 == 0), so its first tick takes the quiet path — no demand scan,
        // no release/request traffic at all, not even the previously
        // unconditional (and, for an empty region, always-empty-payload)
        // `PowerExportAllocationsReleased`. The tick still completes
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
                OutboundMessage::PowerExportAllocationsReleased(_)
                    | OutboundMessage::PowerExportRequested(_)
            )),
            "a quiet tick must not emit any power export traffic"
        );
    }

    #[test]
    fn daily_tick_without_job_demands_releases_job_allocations_before_finishing() {
        // Event-driven plan, P-2: this region's power gate goes quiet after
        // its first tick (nothing ever dirties it or moves its generation
        // across these 24 ticks), so it no longer emits a
        // `PowerExportAllocationsReleased` on the 24th (daily) tick — that
        // part of the original assertion tested the pre-P-2 unconditional
        // reconcile, not this test's actual subject.
        //
        // Event-driven plan, P-4: jobs are now gated too, on `jobs_exports_dirty`
        // (or a moved generation). A genuinely empty region (the original
        // fixture here) starts and stays clean forever, so its daily
        // reconcile would ALSO go quiet — no longer able to exercise this
        // test's actual subject at all. One building is enough to dirty
        // `jobs_exports_dirty` from construction (placement's
        // `invalidate_resource_registry` chokepoint sets it, same as
        // `power_exports_dirty`), so day 1's reconcile genuinely fires, with
        // zero actual job demands (no citizens exist yet to seek one) — the
        // exact "without_job_demands" shape this test's name describes. Built
        // on a grid large enough that the road sits away from every edge, so
        // `network_border_links` reports no border link at all — nothing
        // (power, jobs, or goods) can attempt a real cross-region request, so
        // the tick can never pause waiting for a producer that doesn't exist
        // in this bare-runtime test. The gate itself doesn't care: it decides
        // purely from `jobs_exports_dirty`/generation, independent of whether
        // `phase.job_demands` ends up empty.
        let mut region = RegionState::new(RegionId(1), 5, 5);
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

        let job_release = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::JobExportAllocationsReleased(_))
        });
        let completed = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::RegionTickCompleted(_))
        });

        assert!(
            job_release < completed,
            "job reconciliation should release old allocations before finishing"
        );

        // Event-driven plan, P-4 (positive proof): this region never spawns a
        // citizen (no Residential) and never receives a grant, so nothing
        // ever re-dirties `jobs_exports_dirty` after day 1's reconcile clears
        // it (unlike a region with an ongoing remote-employed citizen —
        // `assign_local_jobs_for_daily_tick` unconditionally wipes every
        // workplace assignment, local AND remote, every day "so remote jobs
        // can be requested again from producer regions after local matching
        // has taken its current slots" (economy.rs), and a successful grant
        // re-dirties the flag via `apply_job_export_grant`'s own
        // `invalidate_jobs_registry` call — so a region with ANY ongoing
        // remote employment can never go quiet; it must genuinely
        // re-reconcile every day, by design). Day 2 here has none of that:
        // it must emit zero job export traffic at all.
        for request_id in 25..=48 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            let outbound = runtime.process_next_event();
            assert!(
                !outbound.iter().any(|message| matches!(
                    message,
                    OutboundMessage::JobExportAllocationsReleased(_)
                        | OutboundMessage::JobExportRequested(_)
                )),
                "day 2 must be genuinely quiet: no job export traffic at all"
            );
        }
    }

    #[test]
    fn daily_tick_without_goods_demands_releases_goods_allocations_before_finishing() {
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

        let goods_release = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::GoodsExportAllocationsReleased(_))
        });
        let completed = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::RegionTickCompleted(_))
        });
        assert!(
            goods_release < completed,
            "goods reconciliation should release old allocations before finishing"
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
                    OutboundMessage::GoodsExportAllocationsReleased(_)
                        | OutboundMessage::GoodsExportRequested(_)
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
            request: request.clone(),
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
            request: stale_request.clone(),
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
                [OutboundMessage::PowerExportAllocationsReleased(release)]
                    if release.producer_regions == vec![RegionId(2)]
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
            request: PowerExportRequest {
                request_id: UiRequestId(0), // matches a never-ticked runtime's current
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 0,
                demand: 1,
                consumer: crate::core::entity::Entity::new(RegionId(1), 999), // no such entity
            },
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
        while runtime.pending_event_count() > 0 {
            runtime.process_next_event();
        }

        let mut requested = false;
        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            let outbound = runtime.process_next_event();
            if outbound
                .iter()
                .any(|message| matches!(message, OutboundMessage::GoodsExportRequested(_)))
            {
                let release = message_index(&outbound, |message| {
                    matches!(message, OutboundMessage::GoodsExportAllocationsReleased(_))
                });
                let request = message_index(&outbound, |message| {
                    matches!(message, OutboundMessage::GoodsExportRequested(_))
                });
                let completed = message_index(&outbound, |message| {
                    matches!(message, OutboundMessage::RegionTickCompleted(_))
                });
                assert!(release < request);
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
    fn matching_goods_grant_applies_on_next_daily_goods_phase() {
        // Goods imports were already delayed via `pending_goods_stock`; P-d
        // keeps that behavior while removing the pause. A grant received
        // after day 1's tick is not stored immediately. It lands when the
        // next daily goods phase starts.
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
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
        let request = goods_export_requests(&day1_outbound)[0].clone();
        let commercial = request.commercial;
        assert_eq!(
            crate::core::systems::economy::commercial_goods_stored(
                &runtime.state().world,
                commercial
            ),
            0
        );

        runtime.push_event(RegionEvent::ApplyGoodsExportGrant {
            request: request.clone(),
            grant: GoodsExportGrant {
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
    fn job_grant_for_a_since_removed_citizen_is_a_no_op() {
        // Retire-tickstate, P-c: same shape as power's
        // `grant_for_a_since_removed_consumer_is_a_no_op` (P-a) -- a reply
        // can echo a citizen that no longer exists (moved away, bulldozed
        // home). request_id 0 matches a never-ticked runtime's current
        // generation, so this is NOT dropped as stale; it reaches the ECS
        // write, which must still no-op.
        let mut runtime = consumer_runtime(RegionId(1));

        runtime.push_event(RegionEvent::ApplyJobExportGrant {
            request: JobExportRequest {
                request_id: UiRequestId(0),
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 0,
                citizen: crate::core::entity::Entity::new(RegionId(1), 999), // no such entity
            },
            grant: JobExportGrant {
                token: 0,
                granted: true,
                workplace: Some(crate::core::entity::Entity::new(RegionId(2), 0)),
                location: Some(crate::core::city_refs::CityCellRef::local(
                    RegionId(2),
                    0,
                    0,
                )),
                salary: 4,
            },
        });
        let outbound = runtime.process_next_event();

        assert!(outbound.is_empty());
    }

    #[test]
    fn stale_job_reply_is_dropped_and_releases_the_producer() {
        // Retire-tickstate, P-c: same staleness protection as power (P-a)
        // -- a reply for a batch this caller has already superseded must
        // be dropped, and if it arrived GRANTED, the producer's
        // reservation must be actively released (a future release may
        // never reach it otherwise).
        let mut region = RegionState::new(RegionId(1), 5, 5);
        assert!(region.build(2, 2, BuildingKind::Commercial).success);
        assert!(region.build(2, 1, BuildingKind::Road).success);
        let mut runtime = RegionRuntime::new(region);

        // Day 1: dirty from construction, stamps current_job_request_id = 24.
        let mut day1_outbound = Vec::new();
        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            day1_outbound = runtime.process_next_event();
        }
        assert!(
            day1_outbound
                .iter()
                .any(|message| matches!(message, OutboundMessage::JobExportAllocationsReleased(_))),
            "day 1 must be dirty (construction) and stamp a generation"
        );

        // Force day 2 dirty too (nothing else would re-dirty a region with
        // no ongoing remote employment -- same trick used elsewhere in this
        // module to reopen a gate deterministically).
        runtime.set_discovery_generation(1);
        for request_id in 25..=48 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            runtime.process_next_event();
        }

        // A grant for DAY 1's now-superseded batch arrives late, GRANTED.
        let producer = RegionId(9);
        runtime.push_event(RegionEvent::ApplyJobExportGrant {
            request: JobExportRequest {
                request_id: UiRequestId(24),
                caller_region: RegionId(1),
                caller_network: RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 0,
                citizen: crate::core::entity::Entity::new(RegionId(1), 999),
            },
            grant: JobExportGrant {
                token: 0,
                granted: true,
                workplace: Some(crate::core::entity::Entity::new(producer, 0)),
                location: Some(crate::core::city_refs::CityCellRef::local(producer, 0, 0)),
                salary: 4,
            },
        });
        let outbound = runtime.process_next_event();

        assert!(
            matches!(
                outbound.as_slice(),
                [OutboundMessage::JobExportAllocationsReleased(release)]
                    if release.producer_regions == vec![producer]
            ),
            "a stale but granted job reply must release the producer's reservation"
        );
    }

    #[test]
    fn stale_goods_reply_is_dropped_and_releases_the_producer() {
        // Retire-tickstate, P-d: same staleness protection as power/jobs.
        // A stale granted goods reply reserved producer stock, so dropping
        // it locally must also emit a targeted release.
        let mut runtime = RegionRuntime::new(goods_seeker_region(RegionId(1)));
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
        let stale_request = goods_export_requests(&day1_outbound)[0].clone();

        runtime.set_discovery_generation(1);
        for request_id in 25..=48 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            runtime.process_next_event();
        }

        let producer = RegionId(9);
        runtime.push_event(RegionEvent::ApplyGoodsExportGrant {
            request: stale_request.clone(),
            grant: GoodsExportGrant {
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
                [OutboundMessage::GoodsExportAllocationsReleased(release)]
                    if release.producer_regions == vec![producer]
            ),
            "a stale but granted goods reply must release the producer's reservation"
        );
    }

    #[test]
    fn daily_tick_with_jobless_seeker_completes_immediately_and_requests_export() {
        // Retire-tickstate, P-c: a locally-powered residential whose only
        // workplace sits on a separate, unreachable road network grows
        // citizens that stay locally jobless. The first daily tick that
        // produces such a seeker now completes immediately (jobs no longer
        // pause), same cutover as power (P-a).
        let mut runtime = RegionRuntime::new(job_seeker_region(RegionId(1)));
        while runtime.pending_event_count() > 0 {
            runtime.process_next_event();
        }

        let mut requested = false;
        for request_id in 1..=240u64 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            let outbound = runtime.process_next_event();
            if outbound
                .iter()
                .any(|message| matches!(message, OutboundMessage::JobExportRequested(_)))
            {
                let release = message_index(&outbound, |message| {
                    matches!(message, OutboundMessage::JobExportAllocationsReleased(_))
                });
                let request = message_index(&outbound, |message| {
                    matches!(message, OutboundMessage::JobExportRequested(_))
                });
                assert!(
                    release < request,
                    "job reconciliation should release before requesting current demands"
                );
                assert!(
                    has_tick_completed(&outbound),
                    "the tick completes in the same pass it asks for exported jobs"
                );
                requested = true;
                break;
            }
        }
        assert!(
            requested,
            "a daily tick should eventually request a remote job"
        );
    }

    #[test]
    fn bridge_workplace_is_not_granted_twice_across_networks() {
        // A single workplace adjacent to two disconnected road networks is one
        // physical slot, yet it appears as spare on BOTH networks. CR1 deliberately
        // keeps such networks in separate components, so two consumers in different
        // components can each request this slot via a different network. A
        // reservation taken via one network must block the other request, or the
        // producer double-grants one slot (charging its tax twice while two remote
        // citizens both "hold" the job). Regression guard for the CR3R network-scoped
        // exclusion bug.
        let mut runtime = RegionRuntime::new(bridge_producer_region(RegionId(2)));
        // DT1: producer-side reads gate on applied power; bring the derived pass
        // current after the paused builds (the worker does this before reading).
        runtime.ensure_derived_state();

        // Both disconnected networks expose the single bridge slot.
        let networks: Vec<RegionRoadNetworkId> = runtime
            .state()
            .availability_hints()
            .into_iter()
            .filter(|hint| !hint.spare_job_slot_ids.is_empty())
            .map(|hint| hint.network)
            .collect();
        assert_eq!(
            networks.len(),
            2,
            "bridge slots should be spare on both networks"
        );
        let bridge = runtime.state().spare_job_slots_on_network(networks[0]);
        // The bridge building's slots are the same physical slots on both networks.
        assert!(!bridge.is_empty());
        assert!(
            bridge.iter().all(|slot| *slot == bridge[0]),
            "one bridge building"
        );
        assert_eq!(
            runtime.state().spare_job_slots_on_network(networks[1]),
            bridge,
            "the same physical slots are spare on the second network"
        );

        // Caller A reserves every slot of the bridge building via the first network.
        for token in 0..bridge.len() as u32 {
            let grant = runtime.process_job_export_request(&job_export_request(
                RegionId(10),
                token,
                networks[0],
            ));
            assert!(grant.granted, "caller A takes bridge slot {token}");
            assert_eq!(grant.workplace, Some(bridge[0]));
        }

        // Caller B (a different component) requests a slot of the same building via
        // the second network. Every slot is already reserved, so it must be denied:
        // the network-scoped exclusion bug missed caller A's reservations here.
        let grant_b =
            runtime.process_job_export_request(&job_export_request(RegionId(11), 99, networks[1]));
        assert!(
            !grant_b.granted,
            "all bridge slots are reserved on the other network; no double-grant"
        );
    }

    #[test]
    fn goods_export_request_reserves_producer_surplus_units() {
        let mut runtime = RegionRuntime::new(goods_producer_region(RegionId(2)));
        runtime.ensure_derived_state();
        let network = region_network(2, 0);

        let grant_a = runtime.process_goods_export_request(&goods_export_request(
            RegionId(10),
            0,
            3,
            network,
        ));
        assert_eq!(
            grant_a,
            GoodsExportGrant {
                token: 0,
                granted: true,
                source_region: Some(RegionId(2)),
                units: 3,
            }
        );

        let grant_b = runtime.process_goods_export_request(&goods_export_request(
            RegionId(11),
            1,
            2,
            network,
        ));
        assert_eq!(
            grant_b,
            GoodsExportGrant {
                token: 1,
                granted: false,
                source_region: None,
                units: 0,
            }
        );
    }

    fn job_export_request(
        caller: RegionId,
        token: u32,
        producer_network: RegionRoadNetworkId,
    ) -> JobExportAllocationRequest {
        ExportAllocationRequest {
            request: JobExportRequest {
                request_id: UiRequestId(1),
                caller_region: caller,
                // The producer ignores the caller's own network; only the candidate
                // (producer) network and the (caller, gen, token) key matter here.
                caller_network: producer_network,
                token,
                citizen: crate::core::entity::Entity::new(caller, token),
            },
            candidates: vec![producer_network],
            candidate_index: 0,
        }
    }

    fn goods_export_request(
        caller: RegionId,
        token: u32,
        units: u32,
        producer_network: RegionRoadNetworkId,
    ) -> GoodsExportAllocationRequest {
        ExportAllocationRequest {
            request: GoodsExportRequest {
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
    fn bridge_producer_region(region_id: RegionId) -> RegionState {
        let mut region = RegionState::new(region_id, 3, 2);
        assert!(region.build(0, 0, BuildingKind::Road).success);
        assert!(region.build(2, 0, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Commercial).success);
        assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
        region
    }

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
    fn job_seeker_region(region_id: RegionId) -> RegionState {
        let mut region = RegionState::new(region_id, 6, 3);
        assert!(region.build(0, 0, BuildingKind::Residential).success);
        assert!(region.build(0, 1, BuildingKind::Park).success);
        for x in 1..=5 {
            assert!(region.build(x, 0, BuildingKind::Road).success);
        }
        assert!(region.build(4, 1, BuildingKind::PowerPlant).success);
        assert!(region.build(3, 2, BuildingKind::Road).success);
        assert!(region.build(4, 2, BuildingKind::Road).success);
        assert!(region.build(5, 2, BuildingKind::Industrial).success);
        region
    }
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
    fn reserved_units_excluding_key_spans_all_networks() {
        let mut allocations = ExportAllocations::<i32>::new();
        allocations.upsert(key(1, 5, 0), net(2, 0), 3, UiRequestId(5));
        allocations.upsert(key(1, 5, 1), net(2, 0), 4, UiRequestId(5));
        allocations.upsert(key(1, 5, 2), net(2, 1), 9, UiRequestId(5));

        // Network-agnostic (jobs): excluding token-0's own key, every other
        // reservation counts regardless of which network it was taken under, so a
        // bridge slot reserved via one network still blocks the other.
        let reserved = allocations.reserved_units_excluding_key(key(1, 5, 0));
        assert_eq!(reserved.collect::<Vec<_>>(), vec![4, 9]);
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
    use crate::core::regions::RegionRoadNetworkId;
    use crate::core::regions::RegionState;
    use crate::core::regions::employment_directory::JobPool;
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
                OutboundMessage::EmploymentDirectoryReady { target_region, .. } => {
                    Some(*target_region)
                }
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
}
