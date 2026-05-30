//! Single-threaded regional runtime that processes owned events in FIFO order.
//!
//! This module introduces the actor-style shell around `RegionState` without
//! spawning OS threads or exposing ECS storage. Worker patches can later route
//! `OutboundMessage` values between runtimes.

use std::collections::VecDeque;

use crate::core::regions::{ImportedResource, ImportedResourceResult, RegionId, RegionState};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Event owned by one region runtime inbox.
pub enum RegionEvent {
    /// Advance this region's local deterministic simulation by one tick.
    Tick,
    /// Process an imported resource in this target region.
    ProcessImportedResource(ImportedResourceRequest),
    /// Apply a completed neighbor result after it has been routed back here.
    RunImportedResourceContinuation { result: ImportedResourceResult },
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Owned request for target-region imported resource work.
///
/// The target region processes only this payload and returns an outbound result
/// for the caller. It does not borrow or mutate the caller's region state.
pub struct ImportedResourceRequest {
    pub caller_region: RegionId,
    pub resource: ImportedResource,
    pub local_used_capacity: u32,
    pub border_crossing_cost: u32,
    pub target_neighbors: Vec<RegionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Message returned by a runtime for the caller or worker to route.
pub enum OutboundMessage {
    /// A completed imported-resource result that must be routed back to the caller.
    ReturnImportedResourceResult {
        caller_region: RegionId,
        result: ImportedResourceResult,
    },
}

#[derive(Debug)]
/// Single-region event loop with deterministic FIFO processing.
pub struct RegionRuntime {
    state: RegionState,
    inbox: VecDeque<RegionEvent>,
}

impl RegionRuntime {
    /// Creates a runtime that owns one region and an empty inbox.
    pub fn new(state: RegionState) -> Self {
        Self {
            state,
            inbox: VecDeque::new(),
        }
    }

    pub fn region_id(&self) -> RegionId {
        self.state.id()
    }

    pub fn state(&self) -> &RegionState {
        &self.state
    }

    /// Adds one event to the back of this region's FIFO inbox.
    pub fn push_event(&mut self, event: RegionEvent) {
        self.inbox.push_back(event);
    }

    pub fn pending_event_count(&self) -> usize {
        self.inbox.len()
    }

    /// Processes the next inbox event, returning messages for external routing.
    pub fn process_next_event(&mut self) -> Vec<OutboundMessage> {
        let Some(event) = self.inbox.pop_front() else {
            return Vec::new();
        };

        self.process_event(event)
    }

    /// Processes up to `max_events` events from this region's inbox.
    pub fn process_some_events(&mut self, max_events: usize) -> Vec<OutboundMessage> {
        let mut outbound = Vec::new();

        for _ in 0..max_events {
            if self.inbox.is_empty() {
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
                    request.resource,
                    request.local_used_capacity,
                    request.border_crossing_cost,
                    &request.target_neighbors,
                );

                vec![OutboundMessage::ReturnImportedResourceResult {
                    caller_region: request.caller_region,
                    result,
                }]
            }
            RegionEvent::RunImportedResourceContinuation { result } => {
                self.state.apply_neighbor_import_result(result);
                Vec::new()
            }
        }
    }
}
