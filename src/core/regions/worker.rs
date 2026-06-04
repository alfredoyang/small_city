//! Single-threaded worker that schedules multiple region runtimes fairly.
//!
//! The worker routes owned runtime messages between region inboxes. It never
//! reads or mutates ECS state directly; all simulation work stays inside each
//! `RegionRuntime`.

use crate::core::regional_types::{
    RegionCommandResponse, RegionSnapshotResponse, RegionTickResponse,
};
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::load_manager::WorkerLoad;
use crate::core::regions::runtime::{
    OutboundMessage, RegionEvent, RegionRuntime, RegionRuntimeError,
};
use crate::core::regions::{RegionId, RegionalExportChange};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable identity for one single-threaded worker scheduler.
pub struct WorkerId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic routing error produced by a worker pass.
pub enum WorkerRoutingError {
    /// A worker cannot own two runtimes with the same routing key.
    DuplicateRegion { region_id: RegionId },
    /// A routed event targeted a region this worker does not own.
    MissingTargetRegion { target_region: RegionId },
    /// A region runtime returned its own deterministic processing error.
    RuntimeError {
        source_region: RegionId,
        error: RegionRuntimeError,
    },
}

#[derive(Debug)]
/// Failed region attachment that returns the still-owned runtime to the caller.
pub struct RegionAddError {
    error: WorkerRoutingError,
    runtime: Box<RegionRuntime>,
}

impl RegionAddError {
    pub fn routing_error(&self) -> WorkerRoutingError {
        self.error
    }

    pub fn into_runtime(self) -> RegionRuntime {
        *self.runtime
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
/// Summary returned after one worker scheduling pass.
pub struct WorkerRunSummary {
    pub processed_regions: usize,
    pub routing_errors: Vec<WorkerRoutingError>,
    pub command_replies: Vec<RegionCommandResponse>,
    pub tick_replies: Vec<RegionTickResponse>,
    pub snapshot_replies: Vec<RegionSnapshotResponse>,
}

#[derive(Debug)]
/// Owns and schedules multiple regional runtimes on one thread.
pub struct RegionWorker {
    id: WorkerId,
    regions: Vec<RegionRuntime>,
}

impl RegionWorker {
    pub fn new(id: WorkerId) -> Self {
        Self {
            id,
            regions: Vec::new(),
        }
    }

    pub fn id(&self) -> WorkerId {
        self.id
    }

    pub fn add_region(&mut self, runtime: RegionRuntime) -> Result<(), RegionAddError> {
        let region_id = runtime.region_id();
        if self.region(region_id).is_some() {
            return Err(RegionAddError {
                error: WorkerRoutingError::DuplicateRegion { region_id },
                runtime: Box::new(runtime),
            });
        }

        self.regions.push(runtime);
        Ok(())
    }

    /// Removes one owned runtime so a caller can move it at a safe point.
    pub fn remove_region(&mut self, region_id: RegionId) -> Option<RegionRuntime> {
        let position = self
            .regions
            .iter()
            .position(|runtime| runtime.region_id() == region_id)?;

        Some(self.regions.remove(position))
    }

    pub fn region(&self, region_id: RegionId) -> Option<&RegionRuntime> {
        self.regions
            .iter()
            .find(|runtime| runtime.region_id() == region_id)
    }

    pub fn handle_for(&self, region_id: RegionId) -> Option<RegionHandle> {
        self.region(region_id).map(RegionRuntime::handle)
    }

    pub fn load(&self) -> WorkerLoad {
        let region_ids = self
            .regions
            .iter()
            .map(RegionRuntime::region_id)
            .collect::<Vec<_>>();
        let queued_events = self
            .regions
            .iter()
            .map(RegionRuntime::pending_event_count)
            .sum();

        WorkerLoad::new(self.id, region_ids, queued_events)
    }

    pub fn push_event(
        &mut self,
        target_region: RegionId,
        event: RegionEvent,
    ) -> Result<(), WorkerRoutingError> {
        let Some(handle) = self.handle_for(target_region) else {
            return Err(WorkerRoutingError::MissingTargetRegion { target_region });
        };

        handle.send(event);
        Ok(())
    }

    /// Gives each owned region up to `max_events_per_region` events of work.
    ///
    /// Outbound messages are routed after all regions receive their scheduling
    /// slice. This keeps one region from creating same-pass work for another
    /// region that has not yet had its turn.
    pub fn process_region_events(&mut self, max_events_per_region: usize) -> WorkerRunSummary {
        if max_events_per_region == 0 {
            return WorkerRunSummary::default();
        }

        let mut processed_regions = 0;
        let mut outbound = Vec::new();

        for runtime in &mut self.regions {
            if runtime.pending_event_count() == 0 {
                continue;
            }

            processed_regions += 1;
            let source_region = runtime.region_id();
            outbound.extend(
                runtime
                    .process_some_events(max_events_per_region)
                    .into_iter()
                    .map(|message| (source_region, message)),
            );
        }

        let mut routing_errors = Vec::new();
        let mut command_replies = Vec::new();
        let mut tick_replies = Vec::new();
        let mut snapshot_replies = Vec::new();

        for (source_region, message) in outbound {
            match self.route_outbound(source_region, message) {
                Ok(WorkerRoutedMessage::CommandReply(reply)) => command_replies.push(reply),
                Ok(WorkerRoutedMessage::TickReply(reply)) => tick_replies.push(reply),
                Ok(WorkerRoutedMessage::SnapshotReply(reply)) => snapshot_replies.push(reply),
                Ok(WorkerRoutedMessage::None) => {}
                Err(error) => routing_errors.push(error),
            }
        }

        WorkerRunSummary {
            processed_regions,
            routing_errors,
            command_replies,
            tick_replies,
            snapshot_replies,
        }
    }

    fn route_outbound(
        &mut self,
        source_region: RegionId,
        message: OutboundMessage,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        match message {
            OutboundMessage::ReturnImportedResourceContinuation {
                caller_region,
                continuation,
                result,
            } => {
                self.push_event(
                    caller_region,
                    RegionEvent::RunImportedResourceContinuation {
                        continuation,
                        result,
                    },
                )?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::RegionCommandCompleted(reply) => {
                Ok(WorkerRoutedMessage::CommandReply(reply))
            }
            OutboundMessage::RegionTickCompleted(reply) => {
                Ok(WorkerRoutedMessage::TickReply(reply))
            }
            OutboundMessage::RegionSnapshotReady(reply) => {
                Ok(WorkerRoutedMessage::SnapshotReply(reply))
            }
            OutboundMessage::RegionExportsChanged(change) => {
                self.route_export_change(change)?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::RuntimeError(error) => Err(WorkerRoutingError::RuntimeError {
                source_region,
                error,
            }),
        }
    }

    fn route_export_change(
        &mut self,
        change: RegionalExportChange,
    ) -> Result<(), WorkerRoutingError> {
        let target_regions = self
            .regions
            .iter()
            .map(RegionRuntime::region_id)
            .filter(|region_id| *region_id != change.source_region)
            .collect::<Vec<_>>();

        for target_region in &target_regions {
            let target_neighbors = target_regions
                .iter()
                .copied()
                .filter(|region_id| *region_id != *target_region)
                .collect::<Vec<_>>();

            for export in &change.current {
                self.push_event(
                    *target_region,
                    RegionEvent::process_imported_resource(
                        change.source_region,
                        export.imported_resource(),
                        target_neighbors.clone(),
                    ),
                )?;
            }

            for removed_kind in &change.removed {
                self.push_event(
                    *target_region,
                    RegionEvent::process_imported_resource(
                        change.source_region,
                        RegionalExportChange::tombstone(change.source_region, *removed_kind),
                        target_neighbors.clone(),
                    ),
                )?;
            }
        }

        Ok(())
    }
}

enum WorkerRoutedMessage {
    None,
    CommandReply(RegionCommandResponse),
    TickReply(RegionTickResponse),
    SnapshotReply(RegionSnapshotResponse),
}
