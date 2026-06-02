//! Threaded execution owner for regional simulation.
//!
//! `RegionalGameRunner` is the first production owner above the regional worker
//! path. It starts exactly one worker thread, keeps worker handles private, and
//! exposes only narrow UI-safe operations.

use crate::core::regional_types::{RegionViewSnapshot, UiReply, UiRequestId};
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::RegionRuntime;
use crate::core::regions::threaded::{
    ThreadedRegionWorker, ThreadedWorkerError, ThreadedWorkerShutdown,
};
use crate::core::regions::worker::{RegionWorker, WorkerId, WorkerRoutingError};
use crate::core::regions::{RegionId, RegionState};
use crate::interface::view::InspectView;

const INITIAL_WORKER_ID: WorkerId = WorkerId(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic errors returned by the regional game runner.
pub enum RegionalGameRunnerError {
    DuplicateRegion {
        region_id: RegionId,
    },
    UnknownRegion {
        region_id: RegionId,
    },
    RegionAddFailed {
        worker_id: WorkerId,
        error: WorkerRoutingError,
    },
    WorkerStopped {
        worker_id: WorkerId,
    },
    WorkerPanicked {
        worker_id: WorkerId,
    },
}

#[derive(Debug)]
/// Public threaded runner that owns one regional worker thread.
pub struct RegionalGameRunner {
    worker: ThreadedRegionWorker,
    handles: Vec<RegionHandle>,
}

impl RegionalGameRunner {
    pub fn start(regions: Vec<RegionState>) -> Result<Self, RegionalGameRunnerError> {
        let mut worker = RegionWorker::new(INITIAL_WORKER_ID);
        let mut handles = Vec::new();

        for region in regions {
            let runtime = RegionRuntime::new(region);
            let handle = runtime.handle();

            if let Err(error) = worker.add_region(runtime) {
                return Err(match error.routing_error() {
                    WorkerRoutingError::DuplicateRegion { region_id } => {
                        RegionalGameRunnerError::DuplicateRegion { region_id }
                    }
                    error => RegionalGameRunnerError::RegionAddFailed {
                        worker_id: worker.id(),
                        error,
                    },
                });
            }

            handles.push(handle);
        }

        Ok(Self {
            worker: ThreadedRegionWorker::start(worker),
            handles,
        })
    }

    pub fn tick_region(&self, region_id: RegionId) -> Result<(), RegionalGameRunnerError> {
        let handle = self
            .handle_for(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;

        handle.send(crate::core::regions::runtime::RegionEvent::Tick);
        // RegionWorker scheduling is fair across every owned runtime. This sends
        // Tick to one region, then gives all regions one chance to drain already
        // queued work; callers that need target-only processing will need a
        // narrower worker command in a later patch.
        self.worker
            .process_region_events(1)
            .map_err(RegionalGameRunnerError::from)?;
        Ok(())
    }

    pub fn request_region_snapshot(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<UiReply, RegionalGameRunnerError> {
        let snapshot = self
            .worker
            .region_view(region_id)
            .map_err(RegionalGameRunnerError::from)?
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })
            .map(|view| RegionViewSnapshot {
                region_id,
                revision: view.status.turn as u64,
                view,
            })?;

        Ok(UiReply::RegionSnapshotReady {
            request_id,
            region_id,
            snapshot,
        })
    }

    pub fn inspect_region(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<InspectView, RegionalGameRunnerError> {
        self.worker
            .inspect_region(region_id, x, y)
            .map_err(RegionalGameRunnerError::from)?
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })
    }

    pub fn shutdown(self) -> Result<RecoveredRegionalGame, RegionalGameRunnerError> {
        let shutdown = self
            .worker
            .shutdown(ThreadedWorkerShutdown::RejectPending)
            .map_err(RegionalGameRunnerError::from)?;

        Ok(RecoveredRegionalGame {
            worker: shutdown.worker,
        })
    }

    fn handle_for(&self, region_id: RegionId) -> Option<&RegionHandle> {
        self.handles
            .iter()
            .find(|handle| handle.region_id() == region_id)
    }
}

#[derive(Debug)]
/// Authoritative regional state recovered after runner shutdown.
pub struct RecoveredRegionalGame {
    worker: RegionWorker,
}

impl RecoveredRegionalGame {
    pub fn region_snapshot(
        &self,
        region_id: RegionId,
    ) -> Result<RegionViewSnapshot, RegionalGameRunnerError> {
        let runtime = self
            .worker
            .region(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;
        let view = runtime.state().view();

        Ok(RegionViewSnapshot {
            region_id,
            revision: view.status.turn as u64,
            view,
        })
    }
}

impl From<ThreadedWorkerError> for RegionalGameRunnerError {
    fn from(error: ThreadedWorkerError) -> Self {
        match error {
            ThreadedWorkerError::WorkerThreadStopped { worker_id } => {
                Self::WorkerStopped { worker_id }
            }
            ThreadedWorkerError::WorkerThreadPanicked { worker_id } => {
                Self::WorkerPanicked { worker_id }
            }
        }
    }
}
