//! Single-threaded worker that schedules multiple region runtimes fairly.
//!
//! The worker routes owned runtime messages between region inboxes. It never
//! reads or mutates ECS state directly; all simulation work stays inside each
//! `RegionRuntime`.

use crate::core::regions::RegionId;
use crate::core::regions::runtime::{
    OutboundMessage, RegionEvent, RegionRuntime, RegionRuntimeError,
};

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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
/// Summary returned after one worker scheduling pass.
pub struct WorkerRunSummary {
    pub processed_regions: usize,
    pub routing_errors: Vec<WorkerRoutingError>,
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

    pub fn add_region(&mut self, runtime: RegionRuntime) -> Result<(), WorkerRoutingError> {
        let region_id = runtime.region_id();
        if self.region(region_id).is_some() {
            return Err(WorkerRoutingError::DuplicateRegion { region_id });
        }

        self.regions.push(runtime);
        Ok(())
    }

    pub fn region(&self, region_id: RegionId) -> Option<&RegionRuntime> {
        self.regions
            .iter()
            .find(|runtime| runtime.region_id() == region_id)
    }

    pub fn push_event(
        &mut self,
        target_region: RegionId,
        event: RegionEvent,
    ) -> Result<(), WorkerRoutingError> {
        let Some(runtime) = self.region_mut(target_region) else {
            return Err(WorkerRoutingError::MissingTargetRegion { target_region });
        };

        runtime.push_event(event);
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

        let routing_errors = outbound
            .into_iter()
            .filter_map(|(source_region, message)| {
                self.route_outbound(source_region, message).err()
            })
            .collect();

        WorkerRunSummary {
            processed_regions,
            routing_errors,
        }
    }

    fn route_outbound(
        &mut self,
        source_region: RegionId,
        message: OutboundMessage,
    ) -> Result<(), WorkerRoutingError> {
        match message {
            OutboundMessage::ReturnImportedResourceContinuation {
                caller_region,
                continuation,
                result,
            } => self.push_event(
                caller_region,
                RegionEvent::RunImportedResourceContinuation {
                    continuation,
                    result,
                },
            ),
            OutboundMessage::RuntimeError(error) => Err(WorkerRoutingError::RuntimeError {
                source_region,
                error,
            }),
        }
    }

    fn region_mut(&mut self, region_id: RegionId) -> Option<&mut RegionRuntime> {
        self.regions
            .iter_mut()
            .find(|runtime| runtime.region_id() == region_id)
    }
}
