//! Single-threaded regional runtime that processes owned events in FIFO order.
//!
//! This module introduces the actor-style shell around `RegionState` without
//! spawning OS threads or exposing ECS storage. Worker patches can later route
//! `OutboundMessage` values between runtimes.

pub mod continuation;

use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionCommandResponse, RegionSnapshotResponse,
    RegionViewSnapshot, UiRequestId,
};
use crate::core::regions::handle::{RegionEventReceiver, RegionHandle, mailbox};
use crate::core::regions::runtime::continuation::{CallerContinuation, NeighborRequest};
use crate::core::regions::{
    ImportedResource, ImportedResourceResult, RegionId, RegionState, RegionalExport,
    RegionalExportChange,
};
use crate::interface::input::MapOverlayInput;

#[derive(Debug)]
/// Event owned by one region runtime inbox.
pub enum RegionEvent {
    /// Advance this region's local deterministic simulation by one tick.
    Tick,
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
    RegionSnapshotReady(RegionSnapshotResponse),
    RegionExportsChanged(RegionalExportChange),
    RuntimeError(RegionRuntimeError),
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    exports: Vec<RegionalExport>,
    handle: RegionHandle,
    receiver: RegionEventReceiver,
}

impl RegionRuntime {
    /// Creates a runtime that owns one region and an empty inbox.
    pub fn new(state: RegionState) -> Self {
        let (handle, receiver) = mailbox(state.id());
        let has_initial_exports = !state.exported_resource_counts().is_empty();
        let mut runtime = Self {
            state,
            exports: Vec::new(),
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
        let Some(event) = self.receiver.pop_event() else {
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
            RegionEvent::Tick => {
                self.state.tick_local();
                self.export_change_messages()
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
            RegionEvent::RefreshExports => self.export_change_messages(),
        }
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
