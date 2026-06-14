//! Single-threaded regional runtime that processes owned events in FIFO order.
//!
//! This module introduces the actor-style shell around `RegionState` without
//! spawning OS threads or exposing ECS storage. Worker patches can later route
//! `OutboundMessage` values between runtimes.
//!
//! Cross-region power export allocation flow:
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
//! Some consumers still unpowered?
//!   |
//!   +-- no --> finish tick normally
//!   |
//!   +-- yes
//!         |
//!         v
//!    Pause tick after local power
//!         |
//!         v
//!    Emit allocation release + export request
//!         |
//!         v
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
//! Apply grant to matching pending consumer
//!         |
//!         v
//! All pending demands resolved?
//!   |
//!   +-- no --> stay paused
//!   |
//!   +-- yes --> continue population/economy/events --> tick completed
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

use crate::core::entity::Entity;
use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionCommandResponse, RegionSnapshotResponse,
    RegionTickResponse, RegionViewSnapshot, UiRequestId,
};
use crate::core::regions::handle::{RegionEventReceiver, RegionHandle, mailbox};
use crate::core::regions::{
    JobExportGrant, PendingJobDemand, PendingPowerDemand, PowerExportGrant, RegionId,
    RegionRoadNetworkId, RegionState, RegionalTickJobPhase, RegionalTickPowerPhase,
};
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
    /// Authoritative producer-side power export allocation request.
    ProcessPowerExportRequest(PowerExportAllocationRequest),
    /// Producer-side release for a caller's previous power export allocations.
    ReleasePowerExportAllocations(PowerExportAllocationRelease),
    /// Caller-side power export grant result.
    ApplyPowerExportGrant(PowerExportGrant),
    /// Authoritative producer-side job-slot export allocation request.
    ProcessJobExportRequest(JobExportAllocationRequest),
    /// Producer-side release for a caller's previous job export allocations.
    ReleaseJobExportAllocations(JobExportAllocationRelease),
    /// Caller-side job export grant result.
    ApplyJobExportGrant(JobExportGrant),
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Consumer request for a producer to export enough power for one local demand.
pub struct PowerExportRequest {
    pub request_id: UiRequestId,
    pub caller_region: RegionId,
    pub caller_network: RegionRoadNetworkId,
    pub token: u32,
    pub demand: i32,
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
    RuntimeError(RegionRuntimeError),
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    tick_state: TickState,
    power_export_allocations: ExportAllocations<i32>,
    job_export_allocations: ExportAllocations<Entity>,
    power_export_producers: Vec<RegionId>,
    job_export_producers: Vec<RegionId>,
    handle: RegionHandle,
    receiver: RegionEventReceiver,
}

#[derive(Debug)]
/// Explicit tick lifecycle for one region runtime.
///
/// Cross-region export pauses a tick at two points so the importing region resolves
/// over the event flow before downstream systems read the result. Power resolves
/// first (it sets `powered`, which jobs and economy then read), then jobs; each is
/// its own waiting sub-state and is skipped when that resource has no exportable
/// demand:
///
/// ```text
/// Idle
///   -- Tick --> WaitingForPowerExports (power demand)
///            \-> WaitingForJobExports  (no power demand, job demand)
///            \-> finish immediately, stay Idle (neither)
/// WaitingForPowerExports
///   -- ApplyPowerExportGrant (demands remain) --> WaitingForPowerExports
///   -- ApplyPowerExportGrant (last demand)    --> enter job phase
/// WaitingForJobExports
///   -- ApplyJobExportGrant (demands remain) --> WaitingForJobExports
///   -- ApplyJobExportGrant (last demand)    --> finish tick, back to Idle
/// ```
///
/// While waiting, only export control events run (grants, producer-side requests,
/// releases for either resource); a second `Tick` is deferred in the inbox until
/// the paused tick finishes. A grant that arrives while `Idle`, in the wrong phase,
/// or with an unknown token, leaves the current state unchanged.
enum TickState {
    Idle,
    WaitingForPowerExports(TickPowerContinuation),
    WaitingForJobExports(TickJobContinuation),
}

impl TickState {
    /// Returns true while a tick is paused waiting for export grants.
    fn is_waiting(&self) -> bool {
        matches!(
            self,
            TickState::WaitingForPowerExports(_) | TickState::WaitingForJobExports(_)
        )
    }
}

#[derive(Debug)]
/// Paused tick waiting for cross-region power export grants to resolve.
struct TickPowerContinuation {
    request_id: UiRequestId,
    phase: RegionalTickPowerPhase,
    pending_demands: Vec<PendingPowerDemand>,
}

#[derive(Debug)]
/// Paused tick waiting for cross-region job-slot export grants to resolve.
struct TickJobContinuation {
    request_id: UiRequestId,
    phase: RegionalTickJobPhase,
    pending_demands: Vec<PendingJobDemand>,
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
            tick_state: TickState::Idle,
            power_export_allocations: ExportAllocations::new(),
            job_export_allocations: ExportAllocations::new(),
            power_export_producers: Vec::new(),
            job_export_producers: Vec::new(),
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
            RegionEvent::ProcessPowerExportRequest(request) => {
                let grant = self.process_power_export_request(&request);
                vec![OutboundMessage::PowerExportRequestCompleted { request, grant }]
            }
            RegionEvent::ReleasePowerExportAllocations(release) => {
                self.power_export_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyPowerExportGrant(grant) => self.apply_power_export_grant(grant),
            RegionEvent::ProcessJobExportRequest(request) => {
                let grant = self.process_job_export_request(&request);
                vec![OutboundMessage::JobExportRequestCompleted { request, grant }]
            }
            RegionEvent::ReleaseJobExportAllocations(release) => {
                self.job_export_allocations
                    .release_stale_for_caller(release.caller_region, release.request_id);
                Vec::new()
            }
            RegionEvent::ApplyJobExportGrant(grant) => self.apply_job_export_grant(grant),
        }
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
            if let Some(source_region) = grant.source_region {
                insert_sorted_unique(&mut self.job_export_producers, source_region);
            }
        }
    }

    fn pop_next_runnable_event(&mut self) -> Option<RegionEvent> {
        if !self.tick_state.is_waiting() {
            return self.receiver.pop_event();
        }

        // A tick paused for exported power or jobs must finish before ordinary
        // gameplay events run. Export grants are control replies for that paused
        // tick; producer-side requests must also run, otherwise two regions that
        // both consume and export on different networks can deadlock each other.
        self.receiver.pop_event_matching(|event| {
            matches!(
                event,
                RegionEvent::ApplyPowerExportGrant(_)
                    | RegionEvent::ProcessPowerExportRequest(_)
                    | RegionEvent::ReleasePowerExportAllocations(_)
                    | RegionEvent::ApplyJobExportGrant(_)
                    | RegionEvent::ProcessJobExportRequest(_)
                    | RegionEvent::ReleaseJobExportAllocations(_)
            )
        })
    }

    fn start_tick_power_phase(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
        let phase = self.state.begin_tick_power_demand_phase();
        self.reconcile_power_export_allocations(request_id, phase)
    }

    // Reconciliation currently uses the simple policy:
    // release all previous allocations for this caller generation, then request all
    // current demands. Future patches can make this incremental by tracking granted
    // producer regions and invalidating only when local demand, producer capacity,
    // or road components change.
    // TODO(CR allocation lifecycle): trigger reconciliation from explicit demand,
    // producer-capacity, or component-change events so it runs only when needed
    // instead of every tick. Tracked under "Deferred optimizations" in
    // docs/regional-multi-worker-plan.md: incremental reconciliation is a
    // distributed cache-coherence problem, kept eager (correct, simple) until scale
    // justifies it.
    fn reconcile_power_export_allocations(
        &mut self,
        request_id: UiRequestId,
        phase: RegionalTickPowerPhase,
    ) -> Vec<OutboundMessage> {
        let producer_regions = std::mem::take(&mut self.power_export_producers);
        let release =
            OutboundMessage::PowerExportAllocationsReleased(PowerExportAllocationRelease {
                caller_region: self.region_id(),
                request_id,
                producer_regions,
            });
        if phase.power_demands.is_empty() {
            // No exported power needed; advance straight to the job phase.
            let mut outbound = vec![release];
            outbound.extend(self.enter_job_phase(request_id, phase));
            return outbound;
        }

        let pending_demands = phase.power_demands.clone();
        let mut outbound = vec![release];
        outbound.extend(
            pending_demands
                .iter()
                .map(|demand| {
                    OutboundMessage::PowerExportRequested(PowerExportRequest {
                        request_id,
                        caller_region: self.region_id(),
                        caller_network: demand.caller_network,
                        token: demand.token,
                        demand: demand.demand,
                    })
                })
                .collect::<Vec<_>>(),
        );

        self.tick_state = TickState::WaitingForPowerExports(TickPowerContinuation {
            request_id,
            phase,
            pending_demands,
        });
        outbound
    }

    /// Advances a tick whose power is resolved into the job export phase.
    ///
    /// Emits a job allocation release for this caller generation, then either
    /// finishes the tick immediately (no job seekers need a remote slot) or sends
    /// one job export request per seeker and pauses in `WaitingForJobExports`.
    fn enter_job_phase(
        &mut self,
        request_id: UiRequestId,
        power_phase: RegionalTickPowerPhase,
    ) -> Vec<OutboundMessage> {
        let phase = self.state.continue_tick_to_job_demand_phase(power_phase);
        if !phase.is_daily() {
            // Jobs resolve only on a daily boundary; an hourly tick neither makes
            // nor invalidates job reservations, so it sends no release or requests.
            return vec![OutboundMessage::RegionTickCompleted(
                self.finish_job_phase(request_id, phase),
            )];
        }
        self.reconcile_job_export_allocations(request_id, phase)
    }

    // Reconciliation currently uses the simple policy:
    // release all previous allocations for this caller generation, then request all
    // current demands. Future patches can make this incremental by tracking granted
    // producer regions and invalidating only when local demand, producer capacity,
    // or road components change.
    // TODO(CR allocation lifecycle): trigger reconciliation from explicit demand,
    // producer-capacity, or component-change events so it runs only when needed
    // instead of every daily job tick. Tracked under "Deferred optimizations" in
    // docs/regional-multi-worker-plan.md: incremental reconciliation is a
    // distributed cache-coherence problem, kept eager (correct, simple) until scale
    // justifies it.
    fn reconcile_job_export_allocations(
        &mut self,
        request_id: UiRequestId,
        phase: RegionalTickJobPhase,
    ) -> Vec<OutboundMessage> {
        let release = OutboundMessage::JobExportAllocationsReleased(JobExportAllocationRelease {
            caller_region: self.region_id(),
            request_id,
            producer_regions: std::mem::take(&mut self.job_export_producers),
        });
        if phase.job_demands.is_empty() {
            return vec![
                release,
                OutboundMessage::RegionTickCompleted(self.finish_job_phase(request_id, phase)),
            ];
        }

        let pending_demands = phase.job_demands.clone();
        let mut outbound = vec![release];
        outbound.extend(
            pending_demands
                .iter()
                .map(|demand| {
                    OutboundMessage::JobExportRequested(JobExportRequest {
                        request_id,
                        caller_region: self.region_id(),
                        caller_network: demand.caller_network,
                        token: demand.token,
                    })
                })
                .collect::<Vec<_>>(),
        );

        self.tick_state = TickState::WaitingForJobExports(TickJobContinuation {
            request_id,
            phase,
            pending_demands,
        });
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
                source_region: None,
                position: None,
                slot_id: None,
                salary: 0,
            };
        };
        let salary = self.state.workplace_salary(workplace);
        let position = self.state.workplace_position(workplace);

        self.job_export_allocations.upsert(
            allocation_key,
            producer_network,
            workplace,
            request.request.request_id,
        );

        JobExportGrant {
            token: request.request.token,
            granted: true,
            source_region: Some(self.region_id()),
            position,
            slot_id: Some(workplace.0),
            salary,
        }
    }

    fn apply_power_export_grant(&mut self, grant: PowerExportGrant) -> Vec<OutboundMessage> {
        // The producer reserved capacity as soon as it emitted a granted reply.
        // Remember that producer even if this caller later ignores the grant
        // because its local demand disappeared or was already powered; the next
        // release must still reach the producer and clear that allocation.
        self.remember_power_export_producer(&grant);
        // A grant only applies to a paused tick. Take the continuation out and
        // fall back to `Idle`; an unrelated grant restores the prior state below.
        let TickState::WaitingForPowerExports(mut continuation) =
            std::mem::replace(&mut self.tick_state, TickState::Idle)
        else {
            return Vec::new();
        };
        let Some(position) = continuation
            .pending_demands
            .iter()
            .position(|demand| demand.token == grant.token)
        else {
            // Unknown token: keep waiting for the demands this tick still expects.
            self.tick_state = TickState::WaitingForPowerExports(continuation);
            return Vec::new();
        };

        let demand = continuation.pending_demands.remove(position);
        self.state.apply_power_export_grant(demand, grant);
        if !continuation.pending_demands.is_empty() {
            self.tick_state = TickState::WaitingForPowerExports(continuation);
            return Vec::new();
        }

        // Last power demand resolved: advance to the job export phase.
        self.enter_job_phase(continuation.request_id, continuation.phase)
    }

    fn apply_job_export_grant(&mut self, grant: JobExportGrant) -> Vec<OutboundMessage> {
        // Same producer-owned allocation invariant as power: a granted reply
        // means the producer may hold a transient slot allocation even if this
        // caller no longer applies it locally.
        self.remember_job_export_producer(&grant);
        // A job grant only applies while waiting for job exports; ignore it
        // otherwise (idle or still in the power phase).
        let TickState::WaitingForJobExports(mut continuation) =
            std::mem::replace(&mut self.tick_state, TickState::Idle)
        else {
            return Vec::new();
        };
        let Some(position) = continuation
            .pending_demands
            .iter()
            .position(|demand| demand.token == grant.token)
        else {
            // Unknown token: keep waiting for the demands this tick still expects.
            self.tick_state = TickState::WaitingForJobExports(continuation);
            return Vec::new();
        };

        let demand = continuation.pending_demands.remove(position);
        self.state.apply_job_export_grant(demand, grant);
        if !continuation.pending_demands.is_empty() {
            self.tick_state = TickState::WaitingForJobExports(continuation);
            return Vec::new();
        }

        // Last job demand resolved: finish the paused tick and return to `Idle`.
        vec![OutboundMessage::RegionTickCompleted(self.finish_job_phase(
            continuation.request_id,
            continuation.phase,
        ))]
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
        &self,
        request_id: UiRequestId,
        overlay: MapOverlayInput,
    ) -> RegionSnapshotResponse {
        let view = self.state.view_with_overlay(overlay);
        RegionSnapshotResponse {
            request_id,
            region_id: self.region_id(),
            snapshot: RegionViewSnapshot::from_view(self.region_id(), view),
        }
    }
}

#[cfg(test)]
mod tick_state_tests {
    //! Unit tests for the `TickState` lifecycle: entering and leaving the paused
    //! `WaitingForPowerExports` state through the runtime event loop.

    use super::*;
    use crate::core::regions::RegionState;
    use crate::interface::input::BuildingKind;

    // A residential consumer next to a border road has no local power and one
    // exportable demand, so ticking it pauses the tick for power exports.
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
    fn tick_with_exportable_demand_enters_waiting_state() {
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });

        let outbound = runtime.process_next_event();

        assert!(runtime.tick_state.is_waiting());
        assert!(!has_tick_completed(&outbound));
        assert_eq!(export_requests(&outbound).len(), 1);
        assert!(matches!(
            outbound.first(),
            Some(OutboundMessage::PowerExportAllocationsReleased(_))
        ));
    }

    #[test]
    fn tick_without_exportable_demand_finishes_immediately() {
        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });

        let outbound = runtime.process_next_event();

        assert!(!runtime.tick_state.is_waiting());
        assert!(has_tick_completed(&outbound));
        assert!(matches!(
            outbound.first(),
            Some(OutboundMessage::PowerExportAllocationsReleased(_))
        ));
    }

    #[test]
    fn daily_tick_without_job_demands_releases_job_allocations_before_finishing() {
        let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
        let mut last_outbound = Vec::new();

        for request_id in 1..=24 {
            runtime.push_event(RegionEvent::Tick {
                request_id: UiRequestId(request_id),
            });
            last_outbound = runtime.process_next_event();
            assert!(!runtime.tick_state.is_waiting());
        }

        let power_release = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::PowerExportAllocationsReleased(_))
        });
        let job_release = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::JobExportAllocationsReleased(_))
        });
        let completed = message_index(&last_outbound, |message| {
            matches!(message, OutboundMessage::RegionTickCompleted(_))
        });

        assert!(
            power_release < job_release,
            "power reconciliation should run before daily job reconciliation"
        );
        assert!(
            job_release < completed,
            "job reconciliation should release old allocations before finishing"
        );
    }

    #[test]
    fn last_grant_finishes_tick_and_returns_to_idle() {
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(7),
        });
        let started = runtime.process_next_event();
        let token = export_requests(&started)[0].token;
        assert!(runtime.tick_state.is_waiting());

        runtime.push_event(RegionEvent::ApplyPowerExportGrant(PowerExportGrant {
            token,
            granted: true,
            source_region: Some(RegionId(2)),
        }));
        let outbound = runtime.process_next_event();

        assert!(!runtime.tick_state.is_waiting());
        let completed = outbound
            .iter()
            .find_map(|message| match message {
                OutboundMessage::RegionTickCompleted(reply) => Some(reply),
                _ => None,
            })
            .expect("tick should finish after the last grant resolves");
        assert_eq!(completed.request_id, UiRequestId(7));
    }

    #[test]
    fn second_tick_is_deferred_while_waiting() {
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });
        let started = runtime.process_next_event();
        let token = export_requests(&started)[0].token;
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(2),
        });

        // The queued second tick is not runnable while the first is paused.
        let deferred = runtime.process_next_event();
        assert!(deferred.is_empty());
        assert!(runtime.tick_state.is_waiting());
        assert_eq!(runtime.pending_event_count(), 1);

        // Resolving the grant finishes the first tick and frees the runtime.
        runtime.push_event(RegionEvent::ApplyPowerExportGrant(PowerExportGrant {
            token,
            granted: false,
            source_region: None,
        }));
        let finished_first = runtime.process_next_event();
        assert!(has_tick_completed(&finished_first));
        assert!(!runtime.tick_state.is_waiting());

        // The deferred second tick now runs; the still-short consumer pauses again.
        let started_second = runtime.process_next_event();
        assert!(!has_tick_completed(&started_second));
        assert!(runtime.tick_state.is_waiting());
    }

    #[test]
    fn grant_while_idle_is_ignored() {
        let mut runtime = consumer_runtime(RegionId(1));
        assert!(!runtime.tick_state.is_waiting());

        runtime.push_event(RegionEvent::ApplyPowerExportGrant(PowerExportGrant {
            token: 0,
            granted: true,
            source_region: Some(RegionId(2)),
        }));
        let outbound = runtime.process_next_event();

        assert!(outbound.is_empty());
        assert!(!runtime.tick_state.is_waiting());
    }

    #[test]
    fn unknown_grant_token_keeps_waiting() {
        let mut runtime = consumer_runtime(RegionId(1));
        runtime.push_event(RegionEvent::Tick {
            request_id: UiRequestId(1),
        });
        let started = runtime.process_next_event();
        let token = export_requests(&started)[0].token;
        assert!(runtime.tick_state.is_waiting());

        runtime.push_event(RegionEvent::ApplyPowerExportGrant(PowerExportGrant {
            token: token.wrapping_add(99),
            granted: true,
            source_region: Some(RegionId(2)),
        }));
        let outbound = runtime.process_next_event();

        assert!(runtime.tick_state.is_waiting());
        assert!(!has_tick_completed(&outbound));
    }

    #[test]
    fn job_grant_while_idle_is_ignored() {
        let mut runtime = consumer_runtime(RegionId(1));
        assert!(!runtime.tick_state.is_waiting());

        runtime.push_event(RegionEvent::ApplyJobExportGrant(JobExportGrant {
            token: 0,
            granted: true,
            source_region: Some(RegionId(2)),
            position: Some(crate::core::components::Position { x: 0, y: 0 }),
            slot_id: Some(0),
            salary: 4,
        }));
        let outbound = runtime.process_next_event();

        assert!(outbound.is_empty());
        assert!(!runtime.tick_state.is_waiting());
    }

    #[test]
    fn daily_tick_with_jobless_seeker_enters_job_wait_state() {
        // A locally-powered residential whose only workplace sits on a separate,
        // unreachable road network grows citizens that stay locally jobless. The
        // first daily tick that produces such a seeker pauses for job exports.
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
                assert!(matches!(
                    runtime.tick_state,
                    TickState::WaitingForJobExports(_)
                ));
                requested = true;
                break;
            }
            // A completed (or power-only) tick must not be left paused for jobs.
            assert!(!matches!(
                runtime.tick_state,
                TickState::WaitingForJobExports(_)
            ));
        }
        assert!(
            requested,
            "a daily tick should eventually pause to import a remote job"
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

        // Both disconnected networks expose the single bridge slot.
        let networks: Vec<RegionRoadNetworkId> = runtime
            .state()
            .availability_hints()
            .into_iter()
            .filter(|hint| hint.has_spare_jobs)
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
            assert_eq!(grant.slot_id, Some(bridge[0].0));
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
            },
            candidates: vec![producer_network],
            candidate_index: 0,
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
