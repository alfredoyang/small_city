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

pub mod continuation;

use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionCommandResponse, RegionSnapshotResponse,
    RegionTickResponse, RegionViewSnapshot, UiRequestId,
};
use crate::core::regions::handle::{RegionEventReceiver, RegionHandle, mailbox};
use crate::core::regions::runtime::continuation::{CallerContinuation, NeighborRequest};
use crate::core::regions::{
    ImportedResource, ImportedResourceResult, PendingPowerDemand, PowerExportGrant, RegionId,
    RegionRoadNetworkId, RegionState, RegionalExport, RegionalExportChange, RegionalTickPowerPhase,
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
    /// Process an imported resource in this target region.
    ProcessImportedResource(ImportedResourceRequest),
    /// Run one player command through this region's local event loop.
    RunCommand {
        request_id: UiRequestId,
        command: RegionCommand,
    },
    /// Apply a completed neighbor result after it has been routed back here.
    RunImportedResourceContinuation {
        continuation: CallerContinuation<ImportedResourceResult>,
        result: ImportedResourceResult,
    },
    /// Authoritative producer-side export allocation request.
    ProcessPowerExportRequest(PowerExportAllocationRequest),
    /// Producer-side release for a caller's previous export allocations.
    ReleasePowerExportAllocations(PowerExportAllocationRelease),
    /// Caller-side export grant result.
    ApplyPowerExportGrant(PowerExportGrant),
    /// Internal event used to publish exports after startup or load.
    RefreshExports,
}

impl RegionEvent {
    /// Builds target-region imported-resource work with the standard caller reply continuation.
    pub fn process_imported_resource(
        caller_region: RegionId,
        resource: ImportedResource,
        target_neighbors: Vec<RegionId>,
    ) -> Self {
        Self::ProcessImportedResource(NeighborRequest {
            payload: ImportedResourcePayload {
                resource,
                local_used_capacity: 0,
                border_crossing_cost: 1,
                target_neighbors,
            },
            continuation: CallerContinuation::<ImportedResourceResult>::new(
                caller_region,
                |region, result| {
                    region.apply_neighbor_import_result(result);
                },
            ),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Owned payload for target-region imported resource work.
pub struct ImportedResourcePayload {
    pub resource: ImportedResource,
    pub local_used_capacity: u32,
    pub border_crossing_cost: u32,
    pub target_neighbors: Vec<RegionId>,
}

pub type ImportedResourceRequest = NeighborRequest<ImportedResourcePayload, ImportedResourceResult>;

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
/// Producer-side allocation request plus remaining candidates if a stale hint denies it.
pub struct PowerExportAllocationRequest {
    pub request: PowerExportRequest,
    pub candidates: Vec<RegionRoadNetworkId>,
    pub candidate_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Producer-side marker that a caller started a new tick power-resolution generation.
pub struct PowerExportAllocationRelease {
    pub caller_region: RegionId,
    pub request_id: UiRequestId,
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
    /// A completed imported-resource continuation that must be routed to caller.
    ReturnImportedResourceContinuation {
        caller_region: RegionId,
        continuation: CallerContinuation<ImportedResourceResult>,
        result: ImportedResourceResult,
    },
    RegionCommandCompleted(RegionCommandResponse),
    RegionTickCompleted(RegionTickResponse),
    RegionSnapshotReady(RegionSnapshotResponse),
    RegionExportsChanged(RegionalExportChange),
    PowerExportRequested(PowerExportRequest),
    PowerExportRequestCompleted {
        request: PowerExportAllocationRequest,
        grant: PowerExportGrant,
    },
    PowerExportAllocationsReleased(PowerExportAllocationRelease),
    RuntimeError(RegionRuntimeError),
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    exports: Vec<RegionalExport>,
    tick_state: TickState,
    power_export_allocations: Vec<PowerExportAllocation>,
    handle: RegionHandle,
    receiver: RegionEventReceiver,
}

#[derive(Debug)]
/// Explicit tick lifecycle for one region runtime.
///
/// Cross-region power export pauses a tick between local power resolution and the
/// downstream systems that read `powered`. This enum makes the two legal states
/// explicit and documents the transitions:
///
/// ```text
/// Idle
///   -- Tick (no exportable demand) --> finish immediately, stay Idle
///   -- Tick (exportable demand)    --> WaitingForPowerExports
/// WaitingForPowerExports
///   -- ApplyPowerExportGrant (demands remain) --> WaitingForPowerExports
///   -- ApplyPowerExportGrant (last demand)    --> finish tick, back to Idle
/// ```
///
/// While `WaitingForPowerExports`, only export control events run (grants,
/// producer-side requests, releases); a second `Tick` is deferred in the inbox
/// until the paused tick finishes. A grant that arrives while `Idle`, or one with
/// an unknown token, leaves the current state unchanged.
enum TickState {
    Idle,
    WaitingForPowerExports(TickPowerContinuation),
}

impl TickState {
    /// Returns true while a tick is paused waiting for export grants.
    fn is_waiting(&self) -> bool {
        matches!(self, TickState::WaitingForPowerExports(_))
    }
}

#[derive(Debug)]
/// Paused tick waiting for cross-region power export grants to resolve.
struct TickPowerContinuation {
    request_id: UiRequestId,
    phase: RegionalTickPowerPhase,
    pending_demands: Vec<PendingPowerDemand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PowerExportAllocation {
    key: PowerExportAllocationKey,
    network: RegionRoadNetworkId,
    demand: i32,
    caller_generation: UiRequestId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PowerExportAllocationKey {
    caller_region: RegionId,
    request_id: UiRequestId,
    token: u32,
}

impl RegionRuntime {
    /// Creates a runtime that owns one region and an empty inbox.
    pub fn new(state: RegionState) -> Self {
        let (handle, receiver) = mailbox(state.id());
        let has_initial_exports = !state.exported_resource_counts().is_empty();
        let mut runtime = Self {
            state,
            exports: Vec::new(),
            tick_state: TickState::Idle,
            power_export_allocations: Vec::new(),
            handle,
            receiver,
        };
        if has_initial_exports {
            runtime.push_event(RegionEvent::RefreshExports);
        }
        runtime
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

    pub fn rebuild_imported_resource_cache(&mut self) {
        self.state.rebuild_imported_resource_cache();
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
            RegionEvent::Tick { request_id } => {
                self.start_tick_with_power_export_requests(request_id)
            }
            RegionEvent::BuildSnapshot {
                request_id,
                overlay,
            } => {
                vec![OutboundMessage::RegionSnapshotReady(
                    self.build_snapshot(request_id, overlay),
                )]
            }
            RegionEvent::ProcessImportedResource(request) => {
                let result = self.state.process_imported_resource(
                    request.payload.resource,
                    request.payload.local_used_capacity,
                    request.payload.border_crossing_cost,
                    &request.payload.target_neighbors,
                );
                let caller_region = request.continuation.caller_region();

                vec![OutboundMessage::ReturnImportedResourceContinuation {
                    caller_region,
                    continuation: request.continuation,
                    result,
                }]
            }
            RegionEvent::RunCommand {
                request_id,
                command,
            } => {
                let response = self.run_command(request_id, command);
                let mut outbound = vec![OutboundMessage::RegionCommandCompleted(response.clone())];
                if command_can_mutate_exports(command)
                    && matches!(
                        response.reply,
                        RegionCommandReply::CommandResult(ref result) if result.success
                    )
                {
                    outbound.extend(self.export_change_messages());
                }
                outbound
            }
            RegionEvent::RunImportedResourceContinuation {
                continuation,
                result,
            } => self.run_imported_resource_continuation(continuation, result),
            RegionEvent::ProcessPowerExportRequest(request) => {
                let grant = self.process_power_export_request(&request);
                vec![OutboundMessage::PowerExportRequestCompleted { request, grant }]
            }
            RegionEvent::ReleasePowerExportAllocations(release) => {
                self.release_stale_power_export_allocations_for_caller(
                    release.caller_region,
                    release.request_id,
                );
                Vec::new()
            }
            RegionEvent::ApplyPowerExportGrant(grant) => self.apply_power_export_grant(grant),
            RegionEvent::RefreshExports => self.export_change_messages(),
        }
    }

    fn pop_next_runnable_event(&mut self) -> Option<RegionEvent> {
        if !self.tick_state.is_waiting() {
            return self.receiver.pop_event();
        }

        // A tick paused for exported power must finish before ordinary gameplay
        // events run. Export grants are control replies for that paused tick;
        // producer-side requests must also run, otherwise two regions that
        // both consume and export on different networks can deadlock each other.
        self.receiver.pop_event_matching(|event| {
            matches!(
                event,
                RegionEvent::ApplyPowerExportGrant(_)
                    | RegionEvent::ProcessPowerExportRequest(_)
                    | RegionEvent::ReleasePowerExportAllocations(_)
            )
        })
    }

    fn start_tick_with_power_export_requests(
        &mut self,
        request_id: UiRequestId,
    ) -> Vec<OutboundMessage> {
        let phase = self.state.begin_tick_power_demand_phase();
        let release =
            OutboundMessage::PowerExportAllocationsReleased(PowerExportAllocationRelease {
                caller_region: self.region_id(),
                request_id,
            });
        if phase.power_demands.is_empty() {
            let mut outbound = vec![
                release,
                OutboundMessage::RegionTickCompleted(self.finish_tick_phase(request_id, phase)),
            ];
            outbound.extend(self.export_change_messages());
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

    fn finish_tick_phase(
        &mut self,
        request_id: UiRequestId,
        phase: RegionalTickPowerPhase,
    ) -> RegionTickResponse {
        RegionTickResponse {
            request_id,
            region_id: self.region_id(),
            result: self.state.finish_tick_power_demand_phase(phase),
        }
    }

    fn process_power_export_request(
        &mut self,
        request: &PowerExportAllocationRequest,
    ) -> PowerExportGrant {
        let producer_network = request.candidates[request.candidate_index];
        let allocation_key = PowerExportAllocationKey {
            caller_region: request.request.caller_region,
            request_id: request.request.request_id,
            token: request.request.token,
        };
        self.release_stale_power_export_allocations_for_caller(
            request.request.caller_region,
            request.request.request_id,
        );
        let active_export_allocations = self
            .power_export_allocations
            .iter()
            .filter(|allocation| {
                allocation.network == producer_network && allocation.key != allocation_key
            })
            .map(|allocation| allocation.demand)
            .sum::<i32>();
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

        if let Some(allocation) = self
            .power_export_allocations
            .iter_mut()
            .find(|allocation| allocation.key == allocation_key)
        {
            allocation.network = producer_network;
            allocation.demand = request.request.demand;
            allocation.caller_generation = request.request.request_id;
        } else {
            self.power_export_allocations.push(PowerExportAllocation {
                key: allocation_key,
                network: producer_network,
                demand: request.request.demand,
                caller_generation: request.request.request_id,
            });
        }

        PowerExportGrant {
            token: request.request.token,
            granted: true,
            source_region: Some(self.region_id()),
        }
    }

    fn release_stale_power_export_allocations_for_caller(
        &mut self,
        caller_region: RegionId,
        caller_generation: UiRequestId,
    ) {
        // TODO(CR2 lifecycle): allocations clear when the caller starts a new
        // tick generation. Add explicit cleanup when caller regions are
        // removed, reassigned, or intentionally stop ticking.
        self.power_export_allocations.retain(|allocation| {
            allocation.key.caller_region != caller_region
                || allocation.caller_generation == caller_generation
        });
    }

    fn apply_power_export_grant(&mut self, grant: PowerExportGrant) -> Vec<OutboundMessage> {
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

        // Last demand resolved: finish the paused tick and return to `Idle`.
        let mut outbound = vec![OutboundMessage::RegionTickCompleted(
            self.finish_tick_phase(continuation.request_id, continuation.phase),
        )];
        outbound.extend(self.export_change_messages());
        outbound
    }

    fn export_change_messages(&mut self) -> Vec<OutboundMessage> {
        self.detect_export_change()
            .map(|change| vec![OutboundMessage::RegionExportsChanged(change)])
            .unwrap_or_default()
    }

    fn detect_export_change(&mut self) -> Option<RegionalExportChange> {
        let current_counts = self.state.exported_resource_counts();
        let previous_kinds = self
            .exports
            .iter()
            .map(|export| export.resource_kind)
            .collect::<Vec<_>>();
        let current_kinds = current_counts
            .iter()
            .map(|(resource_kind, _)| *resource_kind)
            .collect::<Vec<_>>();

        let current = current_counts
            .into_iter()
            .map(|(resource_kind, count)| {
                let generation = self
                    .exports
                    .iter()
                    .find(|previous| previous.resource_kind == resource_kind)
                    .map(|previous| {
                        if previous.count == count {
                            previous.generation
                        } else {
                            previous.generation.saturating_add(1)
                        }
                    })
                    .unwrap_or(1);
                RegionalExport {
                    region_id: self.region_id(),
                    resource_kind,
                    count,
                    generation,
                }
            })
            .collect::<Vec<_>>();

        let removed = previous_kinds
            .into_iter()
            .filter(|resource_kind| !current_kinds.contains(resource_kind))
            .collect::<Vec<_>>();

        if current == self.exports && removed.is_empty() {
            return None;
        }

        self.exports = current.clone();
        Some(RegionalExportChange {
            source_region: self.region_id(),
            current,
            removed,
        })
    }

    fn run_imported_resource_continuation(
        &mut self,
        continuation: CallerContinuation<ImportedResourceResult>,
        result: ImportedResourceResult,
    ) -> Vec<OutboundMessage> {
        let expected_region = continuation.caller_region();
        let actual_region = self.region_id();
        if expected_region != actual_region {
            return vec![OutboundMessage::RuntimeError(
                RegionRuntimeError::ContinuationRoutedToWrongRegion {
                    expected_region,
                    actual_region,
                },
            )];
        }

        continuation.run(&mut self.state, result);
        Vec::new()
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

fn command_can_mutate_exports(command: RegionCommand) -> bool {
    matches!(
        command,
        RegionCommand::Build { .. }
            | RegionCommand::Bulldoze { .. }
            | RegionCommand::Replace { .. }
            | RegionCommand::Upgrade { .. }
    )
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
        let mut runtime = RegionRuntime::new(region);
        // A region with exports enqueues a startup RefreshExports event; drain it
        // so each test observes a clean idle inbox before driving ticks.
        while runtime.pending_event_count() > 0 {
            runtime.process_next_event();
        }
        runtime
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
}
