//! Single-threaded regional runtime that processes owned events in FIFO order.
//!
//! This module introduces the actor-style shell around `RegionState` without
//! spawning OS threads or exposing ECS storage. Worker patches can later route
//! `OutboundMessage` values between runtimes.

pub mod continuation;

use crate::core::regions::handle::{RegionEventReceiver, RegionHandle, mailbox};
use crate::core::regions::runtime::continuation::{CallerContinuation, NeighborRequest};
use crate::core::regions::{ImportedResource, ImportedResourceResult, RegionId, RegionState};

#[derive(Debug)]
/// Event owned by one region runtime inbox.
pub enum RegionEvent {
    /// Advance this region's local deterministic simulation by one tick.
    Tick,
    /// Process an imported resource in this target region.
    ProcessImportedResource(ImportedResourceRequest),
    /// Apply a completed neighbor result after it has been routed back here.
    RunImportedResourceContinuation {
        continuation: CallerContinuation<ImportedResourceResult>,
        result: ImportedResourceResult,
    },
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
    RuntimeError(RegionRuntimeError),
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    handle: RegionHandle,
    receiver: RegionEventReceiver,
}

impl RegionRuntime {
    /// Creates a runtime that owns one region and an empty inbox.
    pub fn new(state: RegionState) -> Self {
        let (handle, receiver) = mailbox(state.id());
        Self {
            state,
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
                Vec::new()
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
            RegionEvent::RunImportedResourceContinuation {
                continuation,
                result,
            } => self.run_imported_resource_continuation(continuation, result),
        }
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
}
