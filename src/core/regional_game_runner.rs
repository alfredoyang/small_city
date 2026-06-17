//! Threaded execution owner for regional simulation.
//!
//! `RegionalGameRunner` is the first production owner above the regional worker
//! path. It starts worker threads, keeps worker handles private, and
//! exposes only narrow UI-safe operations.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionViewSnapshot, UiReply, UiRequestId,
};
use crate::core::regions::directory::RegionDirectory;
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::RegionRuntime;
use crate::core::regions::threaded::{
    ThreadedRegionWorker, ThreadedWorkerError, ThreadedWorkerShutdown,
};
use crate::core::regions::worker::{
    ForwardedRegionEvent, RegionOwnerDirectory, RegionWorker, WorkerId, WorkerRoutingError,
    WorkerRunSummary, sort_forwarded_events,
};
use crate::core::regions::{RegionId, RegionNeighborLink, RegionState};
use crate::interface::events::CommandResult;
use crate::interface::input::MapOverlayInput;
use crate::interface::view::InspectView;

const INITIAL_WORKER_ID: WorkerId = WorkerId(1);
// UI calls are synchronous today, so the runner pumps bounded worker passes
// until the matching event-loop reply appears behind any older queued events.
const MAX_REPLY_PASSES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic errors returned by the regional game runner.
pub enum RegionalGameRunnerError {
    InvalidWorkerCount {
        worker_count: usize,
    },
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
    TickReplyMissing {
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
/// Public threaded runner that owns regional worker threads.
pub struct RegionalGameRunner {
    workers: Vec<ThreadedRegionWorker>,
    handles: Vec<RegionHandle>,
    owners: Arc<RegionOwnerDirectory>,
    operation_lock: Mutex<()>,
}

impl RegionalGameRunner {
    pub fn start(regions: Vec<RegionState>) -> Result<Self, RegionalGameRunnerError> {
        Self::start_with_topology(regions, Vec::new())
    }

    pub fn start_with_worker_count(
        regions: Vec<RegionState>,
        worker_count: usize,
    ) -> Result<Self, RegionalGameRunnerError> {
        Self::start_with_topology_and_worker_count(regions, Vec::new(), worker_count)
    }

    pub fn start_with_topology(
        regions: Vec<RegionState>,
        topology: Vec<RegionNeighborLink>,
    ) -> Result<Self, RegionalGameRunnerError> {
        Self::start_with_topology_and_worker_count(regions, topology, 1)
    }

    pub fn start_with_topology_and_worker_count(
        regions: Vec<RegionState>,
        topology: Vec<RegionNeighborLink>,
        worker_count: usize,
    ) -> Result<Self, RegionalGameRunnerError> {
        if worker_count == 0 || worker_count > u32::MAX as usize {
            return Err(RegionalGameRunnerError::InvalidWorkerCount { worker_count });
        }

        let directory = Arc::new(RegionDirectory::new(topology));
        let owners = Arc::new(RegionOwnerDirectory::new());
        let mut workers = (0..worker_count)
            .map(|index| {
                let worker_id = WorkerId(INITIAL_WORKER_ID.0 + index as u32);
                RegionWorker::with_directory_and_owners(
                    worker_id,
                    Arc::clone(&directory),
                    Arc::clone(&owners),
                )
            })
            .collect::<Vec<_>>();
        let mut handles = Vec::new();

        for (index, region) in regions.into_iter().enumerate() {
            let worker_index = index % workers.len();
            let runtime = RegionRuntime::new(region);
            let handle = runtime.handle();

            if let Err(error) = workers[worker_index].add_region(runtime) {
                return Err(match error.routing_error() {
                    WorkerRoutingError::DuplicateRegion { region_id } => {
                        RegionalGameRunnerError::DuplicateRegion { region_id }
                    }
                    error => RegionalGameRunnerError::RegionAddFailed {
                        worker_id: workers[worker_index].id(),
                        error,
                    },
                });
            }

            handles.push(handle);
        }

        let runner = Self {
            workers: workers
                .into_iter()
                .map(ThreadedRegionWorker::start)
                .collect(),
            handles,
            owners,
            operation_lock: Mutex::new(()),
        };
        runner.process_worker_until_drained()?;

        Ok(runner)
    }

    pub fn tick_region(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<CommandResult, RegionalGameRunnerError> {
        Ok(self.tick_regions(&[(request_id, region_id)])?.remove(0))
    }

    pub fn tick_regions(
        &self,
        requests: &[(UiRequestId, RegionId)],
    ) -> Result<Vec<CommandResult>, RegionalGameRunnerError> {
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        let handles = requests
            .iter()
            .map(|(_, region_id)| self.handle_for_owned_region(*region_id))
            .collect::<Result<Vec<_>, _>>()?;

        for ((request_id, _), handle) in requests.iter().zip(handles) {
            handle.send(crate::core::regions::runtime::RegionEvent::Tick {
                request_id: *request_id,
            });
        }
        self.wait_for_tick_replies(requests)
    }

    pub fn run_region_command(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
        command: RegionCommand,
    ) -> Result<RegionCommandReply, RegionalGameRunnerError> {
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        self.handle_for_owned_region(region_id)?.send(
            crate::core::regions::runtime::RegionEvent::RunCommand {
                request_id,
                command,
            },
        );
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
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        self.handle_for_owned_region(region_id)?.send(
            crate::core::regions::runtime::RegionEvent::BuildSnapshot {
                request_id,
                overlay,
            },
        );
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
        self.worker_for_region(region_id)?
            .inspect_region(region_id, x, y)
            .map_err(RegionalGameRunnerError::from)?
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })
    }

    pub fn shutdown(self) -> Result<RecoveredRegionalGame, RegionalGameRunnerError> {
        let mut workers = Vec::new();
        for worker in self.workers {
            let shutdown = worker
                .shutdown(ThreadedWorkerShutdown::RejectPending)
                .map_err(RegionalGameRunnerError::from)?;
            workers.push(shutdown.worker);
        }

        Ok(RecoveredRegionalGame { workers })
    }

    fn handle_for_owned_region(
        &self,
        region_id: RegionId,
    ) -> Result<&RegionHandle, RegionalGameRunnerError> {
        self.worker_for_region(region_id)?;
        self.handle_for(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })
    }

    fn handle_for(&self, region_id: RegionId) -> Option<&RegionHandle> {
        self.handles
            .iter()
            .find(|handle| handle.region_id() == region_id)
    }

    fn worker_for_region(
        &self,
        region_id: RegionId,
    ) -> Result<&ThreadedRegionWorker, RegionalGameRunnerError> {
        let worker_id = self
            .owners
            .owner_of(region_id)
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;

        self.workers
            .iter()
            .find(|worker| worker.worker_id() == worker_id)
            .ok_or(RegionalGameRunnerError::WorkerStopped { worker_id })
    }

    /// Pumps every worker once while a synchronous runner call waits for its reply.
    ///
    /// This does not mean "produce exactly one reply." Region runtimes are event
    /// loops: the requested command/tick/snapshot may sit behind older events, or
    /// may need cross-region export request/grant events before its final reply
    /// exists. One pass gives each region at most one event of work, collects any
    /// replies produced along the way, then delivers forwarded cross-worker
    /// events through the deterministic barrier. The `wait_for_*` helpers repeat
    /// this bounded pass until the matching `(request_id, region_id)` reply is
    /// found or the safety cap is reached.
    fn process_one_reply_pass(&self) -> Result<WorkerRunSummary, RegionalGameRunnerError> {
        let mut combined = WorkerRunSummary::default();

        for worker in &self.workers {
            let mut summary = worker
                .process_region_events_for_barrier(1)
                .map_err(RegionalGameRunnerError::from)?;

            if let Some(error) = summary.routing_errors.first().copied() {
                return Err(RegionalGameRunnerError::WorkerRoutingFailed {
                    worker_id: worker.worker_id(),
                    error,
                });
            }

            combined.processed_regions += summary.processed_regions;
            combined
                .command_replies
                .append(&mut summary.command_replies);
            combined.tick_replies.append(&mut summary.tick_replies);
            combined
                .snapshot_replies
                .append(&mut summary.snapshot_replies);
            combined
                .forwarded_events
                .append(&mut summary.forwarded_events);
        }
        self.deliver_forwarded_events(std::mem::take(&mut combined.forwarded_events))?;

        Ok(combined)
    }

    fn deliver_forwarded_events(
        &self,
        mut events: Vec<ForwardedRegionEvent>,
    ) -> Result<(), RegionalGameRunnerError> {
        sort_forwarded_events(&mut events);

        let mut by_worker: BTreeMap<WorkerId, Vec<ForwardedRegionEvent>> = BTreeMap::new();
        for event in events {
            by_worker
                .entry(event.target_worker)
                .or_default()
                .push(event);
        }

        for (worker_id, worker_events) in by_worker {
            let target_region = worker_events[0].target_region;
            let Some(worker) = self
                .workers
                .iter()
                .find(|worker| worker.worker_id() == worker_id)
            else {
                return Err(RegionalGameRunnerError::WorkerRoutingFailed {
                    worker_id,
                    error: WorkerRoutingError::MissingTargetRegion { target_region },
                });
            };
            let errors = worker
                .deliver_forwarded_events(worker_events)
                .map_err(RegionalGameRunnerError::from)?;
            if let Some(error) = errors.into_iter().next() {
                return Err(RegionalGameRunnerError::WorkerRoutingFailed { worker_id, error });
            }
        }

        Ok(())
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
        // Worker passes can surface replies from older queued events or other
        // regions. Match on request ID and region so this synchronous facade call
        // receives exactly the command reply it enqueued.
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

    fn wait_for_tick_replies(
        &self,
        requests: &[(UiRequestId, RegionId)],
    ) -> Result<Vec<CommandResult>, RegionalGameRunnerError> {
        let mut results = vec![None; requests.len()];
        let mut pending = requests.to_vec();

        for _ in 0..MAX_REPLY_PASSES {
            let summary = self.process_one_reply_pass()?;
            for reply in summary.tick_replies {
                if let Some(request_position) =
                    requests.iter().position(|(request_id, region_id)| {
                        *request_id == reply.request_id && *region_id == reply.region_id
                    })
                {
                    results[request_position] = Some(reply.result);
                }
                if let Some(position) = pending.iter().position(|(request_id, region_id)| {
                    *request_id == reply.request_id && *region_id == reply.region_id
                }) {
                    pending.remove(position);
                }
            }
            if pending.is_empty() {
                self.process_worker_until_drained()?;
                return Ok(results
                    .into_iter()
                    .map(|result| result.expect("all pending tick replies were collected"))
                    .collect());
            }
        }

        let (request_id, region_id) = pending[0];
        Err(RegionalGameRunnerError::TickReplyMissing {
            request_id,
            region_id,
        })
    }

    fn wait_for_snapshot_reply(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<RegionViewSnapshot, RegionalGameRunnerError> {
        // Snapshot replies are also correlated because view requests can sit
        // behind earlier commands, ticks, or snapshot requests in the region
        // inbox.
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
    workers: Vec<RegionWorker>,
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
                self.workers
                    .iter_mut()
                    .find_map(|worker| worker.remove_region(region_id))
                    .map(RegionRuntime::into_state)
            })
            .collect()
    }

    pub fn region_snapshot(
        &mut self,
        region_id: RegionId,
    ) -> Result<RegionViewSnapshot, RegionalGameRunnerError> {
        // DT1: bring the derived pass current first, so a recovered runtime that
        // ended on a paused command returns current state, not stale derived data.
        let runtime = self
            .workers
            .iter_mut()
            .find_map(|worker| worker.region_mut(region_id))
            .ok_or(RegionalGameRunnerError::UnknownRegion { region_id })?;
        runtime.ensure_derived_state();
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
        let (runner, _handle) = runner_with_preloaded_event(region_id, tick(100));

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
        let (runner, _handle) = runner_with_preloaded_event(region_id, tick(200));

        let reply = runner
            .request_region_snapshot(UiRequestId(2), region_id)
            .expect("snapshot reply should arrive after older queued work drains");

        let UiReply::RegionSnapshotReady { snapshot, .. } = reply;

        assert_eq!(snapshot.view.status.turn, 1);
        runner.shutdown().unwrap();
    }

    #[test]
    fn tick_request_waits_behind_already_queued_region_event() {
        let region_id = RegionId(13);
        let (runner, _handle) = runner_with_preloaded_event(region_id, tick(300));

        let result = runner
            .tick_region(UiRequestId(301), region_id)
            .expect("tick reply should arrive after older queued work drains");

        assert!(result.success);
        let reply = runner
            .request_region_snapshot(UiRequestId(302), region_id)
            .unwrap();
        let UiReply::RegionSnapshotReady { snapshot, .. } = reply;

        assert_eq!(snapshot.view.status.turn, 2);
        runner.shutdown().unwrap();
    }

    fn tick(request_id: u64) -> RegionEvent {
        RegionEvent::Tick {
            request_id: UiRequestId(request_id),
        }
    }

    fn runner_with_preloaded_event(
        region_id: RegionId,
        event: RegionEvent,
    ) -> (RegionalGameRunner, RegionHandle) {
        let runtime = RegionRuntime::new(RegionState::new(region_id, 3, 3));
        let handle = runtime.handle();
        handle.send(event);

        let directory = Arc::new(RegionDirectory::default());
        let owners = Arc::new(RegionOwnerDirectory::new());
        let mut worker = RegionWorker::with_directory_and_owners(
            INITIAL_WORKER_ID,
            Arc::clone(&directory),
            Arc::clone(&owners),
        );
        worker.add_region(runtime).unwrap();

        (
            RegionalGameRunner {
                workers: vec![ThreadedRegionWorker::start(worker)],
                handles: vec![handle.clone()],
                owners,
                operation_lock: Mutex::new(()),
            },
            handle,
        )
    }
}
