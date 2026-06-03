//! Threaded execution owner for regional simulation.
//!
//! `RegionalGameRunner` is the first production owner above the regional worker
//! path. It starts exactly one worker thread, keeps worker handles private, and
//! exposes only narrow UI-safe operations.

use std::sync::Mutex;

use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionViewSnapshot, UiReply, UiRequestId,
};
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::RegionRuntime;
use crate::core::regions::threaded::{
    ThreadedRegionWorker, ThreadedWorkerError, ThreadedWorkerShutdown,
};
use crate::core::regions::worker::{RegionWorker, WorkerId, WorkerRoutingError};
use crate::core::regions::{RegionId, RegionState};
use crate::interface::input::MapOverlayInput;
use crate::interface::view::InspectView;

const INITIAL_WORKER_ID: WorkerId = WorkerId(1);
// UI calls are synchronous today, so the runner pumps bounded worker passes
// until the matching event-loop reply appears behind any older queued events.
const MAX_REPLY_PASSES: usize = 64;

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
    CommandReplyMissing {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    SnapshotReplyMissing {
        request_id: UiRequestId,
        region_id: RegionId,
    },
    WorkerRoutingFailed {
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
    operation_lock: Mutex<()>,
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

        let runner = Self {
            worker: ThreadedRegionWorker::start(worker),
            handles,
            operation_lock: Mutex::new(()),
        };
        runner.process_worker_until_drained()?;

        Ok(runner)
    }

    pub fn tick_region(&self, region_id: RegionId) -> Result<(), RegionalGameRunnerError> {
        let handle = self
            .handle_for(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;

        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        handle.send(crate::core::regions::runtime::RegionEvent::Tick);
        // RegionWorker scheduling is fair across every owned runtime. This sends
        // Tick to one region, then gives all regions one chance to drain already
        // queued work; callers that need target-only processing will need a
        // narrower worker command in a later patch.
        self.worker
            .process_region_events(1)
            .map_err(RegionalGameRunnerError::from)?;
        self.process_worker_until_drained()?;
        Ok(())
    }

    pub fn run_region_command(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
        command: RegionCommand,
    ) -> Result<RegionCommandReply, RegionalGameRunnerError> {
        let handle = self
            .handle_for(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;

        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        handle.send(crate::core::regions::runtime::RegionEvent::RunCommand {
            request_id,
            command,
        });
        self.wait_for_command_reply(request_id, region_id)
    }

    pub fn request_region_snapshot(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<UiReply, RegionalGameRunnerError> {
        self.request_region_snapshot_with_overlay(request_id, region_id, MapOverlayInput::Normal)
    }

    pub fn request_region_snapshot_with_overlay(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
        overlay: MapOverlayInput,
    ) -> Result<UiReply, RegionalGameRunnerError> {
        let handle = self
            .handle_for(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;

        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        handle.send(crate::core::regions::runtime::RegionEvent::BuildSnapshot {
            request_id,
            overlay,
        });
        let snapshot = self.wait_for_snapshot_reply(request_id, region_id)?;

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
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
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

    fn process_one_reply_pass(
        &self,
    ) -> Result<crate::core::regions::worker::WorkerRunSummary, RegionalGameRunnerError> {
        let summary = self
            .worker
            .process_region_events(1)
            .map_err(RegionalGameRunnerError::from)?;

        if let Some(error) = summary.routing_errors.first().copied() {
            return Err(RegionalGameRunnerError::WorkerRoutingFailed {
                worker_id: self.worker.worker_id(),
                error,
            });
        }

        Ok(summary)
    }

    fn process_worker_until_drained(&self) -> Result<(), RegionalGameRunnerError> {
        // Export propagation and imported-resource continuations are
        // asynchronous within worker passes. Stop once a pass finds no queued
        // work, while keeping the same safety cap used by command replies.
        for _ in 0..MAX_REPLY_PASSES {
            let summary = self.process_one_reply_pass()?;
            if summary.processed_regions == 0 {
                break;
            }
        }
        Ok(())
    }

    fn wait_for_command_reply(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<RegionCommandReply, RegionalGameRunnerError> {
        for _ in 0..MAX_REPLY_PASSES {
            let summary = self.process_one_reply_pass()?;
            if let Some(reply) = summary
                .command_replies
                .into_iter()
                .find(|reply| reply.request_id == request_id && reply.region_id == region_id)
            {
                self.process_worker_until_drained()?;
                return Ok(reply.reply);
            }
        }

        Err(RegionalGameRunnerError::CommandReplyMissing {
            request_id,
            region_id,
        })
    }

    fn wait_for_snapshot_reply(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<RegionViewSnapshot, RegionalGameRunnerError> {
        for _ in 0..MAX_REPLY_PASSES {
            let summary = self.process_one_reply_pass()?;
            if let Some(reply) = summary
                .snapshot_replies
                .into_iter()
                .find(|reply| reply.request_id == request_id && reply.region_id == region_id)
            {
                return Ok(reply.snapshot);
            }
        }

        Err(RegionalGameRunnerError::SnapshotReplyMissing {
            request_id,
            region_id,
        })
    }
}

#[derive(Debug)]
/// Authoritative regional state recovered after runner shutdown.
pub struct RecoveredRegionalGame {
    worker: RegionWorker,
}

impl RecoveredRegionalGame {
    pub(crate) fn into_region_states_in_order(
        mut self,
        region_ids: &[RegionId],
    ) -> Vec<RegionState> {
        region_ids
            .iter()
            .copied()
            .filter_map(|region_id| {
                self.worker
                    .remove_region(region_id)
                    .map(RegionRuntime::into_state)
            })
            .collect()
    }

    pub fn region_snapshot(
        &self,
        region_id: RegionId,
    ) -> Result<RegionViewSnapshot, RegionalGameRunnerError> {
        let runtime = self
            .worker
            .region(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;
        let view = runtime.state().view();

        Ok(RegionViewSnapshot::from_view(region_id, view))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::regions::runtime::{RegionEvent, RegionRuntime};
    use crate::interface::input::BuildingKind;

    #[test]
    fn command_request_waits_behind_already_queued_region_event() {
        let region_id = RegionId(11);
        let (runner, _handle) = runner_with_preloaded_event(region_id, RegionEvent::Tick);

        let reply = runner
            .run_region_command(
                UiRequestId(1),
                region_id,
                RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::Road,
                },
            )
            .expect("command reply should arrive after older queued work drains");

        let RegionCommandReply::CommandResult(result) = reply else {
            panic!("build command should return a command result");
        };

        assert!(result.success);
        runner.shutdown().unwrap();
    }

    #[test]
    fn snapshot_request_waits_behind_already_queued_region_event() {
        let region_id = RegionId(12);
        let (runner, _handle) = runner_with_preloaded_event(region_id, RegionEvent::Tick);

        let reply = runner
            .request_region_snapshot(UiRequestId(2), region_id)
            .expect("snapshot reply should arrive after older queued work drains");

        let UiReply::RegionSnapshotReady { snapshot, .. } = reply;

        assert_eq!(snapshot.view.status.turn, 1);
        runner.shutdown().unwrap();
    }

    fn runner_with_preloaded_event(
        region_id: RegionId,
        event: RegionEvent,
    ) -> (RegionalGameRunner, RegionHandle) {
        let runtime = RegionRuntime::new(RegionState::new(region_id, 3, 3));
        let handle = runtime.handle();
        handle.send(event);

        let mut worker = RegionWorker::new(INITIAL_WORKER_ID);
        worker.add_region(runtime).unwrap();

        (
            RegionalGameRunner {
                worker: ThreadedRegionWorker::start(worker),
                handles: vec![handle.clone()],
                operation_lock: Mutex::new(()),
            },
            handle,
        )
    }
}
