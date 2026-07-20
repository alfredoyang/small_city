//! Threaded execution owner for regional simulation.
//!
//! `RegionalGameRunner` is the first production owner above the regional worker
//! path. It starts worker threads, keeps worker handles private, and
//! exposes only narrow UI-safe operations.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::core::regional_types::{
    RegionCommand, RegionCommandReply, RegionViewSnapshot, UiReply, UiRequestId,
};
use crate::core::regions::coordinator::{
    RegionEventCoordinator, RegionRecipients, RoutedRegionEvent, RunnerHealth, RunnerSignal,
};
use crate::core::regions::directory::RegionDirectory;
use crate::core::regions::employment_directory::{
    EmploymentDirectory, rebuild_employment_broker_state,
};
#[cfg(test)]
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::{RegionEvent, RegionRuntime, RuntimeReply, TravelStepId};
use crate::core::regions::threaded::{
    PreparedThreadedRegionWorker, ThreadedRegionWorker, ThreadedWorkerError, ThreadedWorkerShutdown,
};
use crate::core::regions::worker::{
    RegionOwnerDirectory, RegionWorker, WorkerId, WorkerRoutingError, WorkerRunSummary,
};
use crate::core::regions::{RegionId, RegionNeighborLink, RegionState};
use crate::interface::events::CommandResult;
use crate::interface::input::MapOverlayInput;
use crate::interface::view::{
    CitizenDetailView, CitizenRelation, InspectView, RoadTravelerPanelSeedView,
};

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
    InvalidWorkerAssignmentCount {
        region_count: usize,
        assignment_count: usize,
    },
    InvalidWorkerAssignment {
        region_index: usize,
        worker_index: usize,
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
    RuntimeReplyMissing {
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
    CoordinatorFaulted,
}

/// Public threaded runner that owns regional worker threads.
pub struct RegionalGameRunner {
    workers: Vec<ThreadedRegionWorker>,
    owners: Arc<RegionOwnerDirectory>,
    operation_lock: Mutex<()>,
    next_travel_step: AtomicU64,
    next_runtime_request: AtomicU64,
    coordinator: Option<RegionEventCoordinator>,
    runner_signals: Mutex<mpsc::Receiver<RunnerSignal>>,
    health: Arc<RunnerHealth>,
}

impl std::fmt::Debug for RegionalGameRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegionalGameRunner")
            .field("worker_count", &self.workers.len())
            .finish_non_exhaustive()
    }
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

    pub fn start_with_worker_assignments(
        regions: Vec<RegionState>,
        worker_count: usize,
        region_worker_indexes: Vec<usize>,
    ) -> Result<Self, RegionalGameRunnerError> {
        Self::start_with_topology_and_worker_assignments(
            regions,
            Vec::new(),
            worker_count,
            region_worker_indexes,
        )
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
        Self::start_with_topology_and_optional_worker_assignments(
            regions,
            topology,
            worker_count,
            None,
        )
    }

    pub fn start_with_topology_and_worker_assignments(
        regions: Vec<RegionState>,
        topology: Vec<RegionNeighborLink>,
        worker_count: usize,
        region_worker_indexes: Vec<usize>,
    ) -> Result<Self, RegionalGameRunnerError> {
        Self::start_with_topology_and_optional_worker_assignments(
            regions,
            topology,
            worker_count,
            Some(region_worker_indexes),
        )
    }

    fn start_with_topology_and_optional_worker_assignments(
        mut regions: Vec<RegionState>,
        topology: Vec<RegionNeighborLink>,
        worker_count: usize,
        region_worker_indexes: Option<Vec<usize>>,
    ) -> Result<Self, RegionalGameRunnerError> {
        if worker_count == 0 || worker_count > u32::MAX as usize {
            return Err(RegionalGameRunnerError::InvalidWorkerCount { worker_count });
        }
        let region_worker_indexes =
            validate_region_worker_indexes(regions.len(), worker_count, region_worker_indexes)?;

        let owners = Arc::new(RegionOwnerDirectory::new());
        let directory = Arc::new(RegionDirectory::with_owners(topology, Arc::clone(&owners)));
        // P3: one employment broker for the whole city, shared by every worker
        // — exactly like `directory`. Per-worker brokers would each hand out
        // the same workplace seat.
        //
        // P6: seed that broker from the regions' durable employment truth and
        // reconcile any half-torn lease *before* the regions move into workers
        // and their threads start — so the very first tick sees a consistent
        // directory, never a partially rebuilt one. On a fresh city this is a
        // no-op: no contracts, no assignments, an empty broker.
        let employment_directory = Arc::new(EmploymentDirectory::default());
        employment_directory.replace_broker_state(rebuild_employment_broker_state(&mut regions));
        let mut workers = (0..worker_count)
            .map(|index| {
                let worker_id = WorkerId(INITIAL_WORKER_ID.0 + index as u32);
                RegionWorker::with_directories_and_owners(
                    worker_id,
                    Arc::clone(&directory),
                    Arc::clone(&employment_directory),
                    Arc::clone(&owners),
                )
            })
            .collect::<Vec<_>>();

        for (index, region) in regions.into_iter().enumerate() {
            let worker_index = region_worker_indexes[index];
            let runtime = RegionRuntime::new(region);
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
        }

        let prepared = workers
            .into_iter()
            .map(PreparedThreadedRegionWorker::prepare)
            .collect::<Vec<_>>();
        let coordinator_workers = prepared
            .iter()
            .map(|worker| (worker.worker_id(), worker.command_sender()))
            .collect::<BTreeMap<_, _>>();
        let health = Arc::new(RunnerHealth::default());
        let (signals, runner_signals) = mpsc::channel::<RunnerSignal>();
        let coordinator = RegionEventCoordinator::start(
            Arc::clone(&owners),
            coordinator_workers,
            Arc::clone(&health),
            signals,
        );
        let runner = Self {
            workers: prepared
                .into_iter()
                .map(|worker| worker.start_with_coordinator(coordinator.handle()))
                .collect(),
            owners,
            operation_lock: Mutex::new(()),
            next_travel_step: AtomicU64::new(0),
            next_runtime_request: AtomicU64::new(1),
            coordinator: Some(coordinator),
            runner_signals: Mutex::new(runner_signals),
            health,
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
        for (_, region_id) in requests {
            self.worker_for_region(*region_id)?;
        }
        for (request_id, region_id) in requests {
            self.route_region_event(
                *region_id,
                RegionEvent::Tick {
                    request_id: *request_id,
                },
            )?;
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
        self.route_region_event(
            region_id,
            RegionEvent::RunCommand {
                request_id,
                command,
            },
        )?;
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
        self.route_region_event(
            region_id,
            RegionEvent::BuildSnapshot {
                request_id,
                overlay,
            },
        )?;
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
        let request_id = self.next_runtime_request_id();
        self.route_region_event(region_id, RegionEvent::InspectRegion { request_id, x, y })?;
        match self.wait_for_runtime_reply(request_id, region_id)? {
            RuntimeReply::Inspect { inspect, .. } => Ok(*inspect),
            _ => Err(RegionalGameRunnerError::RuntimeReplyMissing {
                request_id,
                region_id,
            }),
        }
    }

    /// Enter-panel road-traveler detail for `(region_id, x, y)`, local-only like
    /// `inspect_region` (one worker, no cross-region fan-out).
    pub fn road_traveler_panel_seed(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<RoadTravelerPanelSeedView, RegionalGameRunnerError> {
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        let request_id = self.next_runtime_request_id();
        self.route_region_event(
            region_id,
            RegionEvent::RoadTravelerPanelSeed { request_id, x, y },
        )?;
        match self.wait_for_runtime_reply(request_id, region_id)? {
            RuntimeReply::RoadTravelerPanelSeed { seed, .. } => Ok(seed),
            _ => Err(RegionalGameRunnerError::RuntimeReplyMissing {
                request_id,
                region_id,
            }),
        }
    }

    /// Remote staff of the workplace at `(producer_region, pos)`: cross-region
    /// commuters who live in any region, on any worker.
    ///
    /// Unlike `inspect_region` (one worker), this fans out to **every** worker
    /// because the commuters may live in regions owned by different workers. The
    /// per-region runs come back `Entity.0`-ordered and contiguous (each region is
    /// owned by exactly one worker), so a **stable** sort by home region yields a
    /// deterministic `(region, entity)` order regardless of region→worker layout.
    pub fn remote_workers_at(
        &self,
        producer_region: RegionId,
        pos: crate::core::components::Position,
    ) -> Result<Vec<CitizenDetailView>, RegionalGameRunnerError> {
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");

        // Parity with inspect_region: reject an unknown workplace region instead of
        // silently returning an empty roster.
        let request_id = self.next_runtime_request_id();
        self.route_region_event(
            producer_region,
            RegionEvent::BuildingAnchorAt {
                request_id,
                x: pos.x,
                y: pos.y,
            },
        )?;
        let RuntimeReply::BuildingAnchor { anchor, .. } =
            self.wait_for_runtime_reply(request_id, producer_region)?
        else {
            return Err(RegionalGameRunnerError::RuntimeReplyMissing {
                request_id,
                region_id: producer_region,
            });
        };
        let Some(anchor) = anchor else {
            return Ok(Vec::new());
        };

        let mut workers = Vec::new();
        let request_id = self.next_runtime_request_id();
        for region_id in self.owners.region_ids() {
            self.route_region_event(
                region_id,
                RegionEvent::RemoteWorkersFor {
                    request_id,
                    producer_region,
                    pos: anchor,
                },
            )?;
            let RuntimeReply::RemoteWorkers {
                workers: region_workers,
                ..
            } = self.wait_for_runtime_reply(request_id, region_id)?
            else {
                return Err(RegionalGameRunnerError::RuntimeReplyMissing {
                    request_id,
                    region_id,
                });
            };
            workers.extend(region_workers);
        }
        workers.sort_by_key(home_region_key);
        Ok(workers)
    }

    pub fn settle_power_imports(
        &self,
        request_id: UiRequestId,
    ) -> Result<(), RegionalGameRunnerError> {
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        let regions = self.owners.region_ids();
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or(RegionalGameRunnerError::CoordinatorFaulted)?;
        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::Many(regions.clone()),
                event: RegionEvent::SettlePowerImports { request_id },
            })
            .map_err(|_| RegionalGameRunnerError::CoordinatorFaulted)?;
        self.wait_for_power_import_settlements(request_id, &regions)
    }

    pub fn shutdown(self) -> Result<RecoveredRegionalGame, RegionalGameRunnerError> {
        let RegionalGameRunner {
            workers: threaded_workers,
            coordinator,
            ..
        } = self;
        let mut workers = Vec::new();
        for worker in threaded_workers {
            let shutdown = worker
                .shutdown(ThreadedWorkerShutdown::RejectPending)
                .map_err(RegionalGameRunnerError::from)?;
            workers.push(shutdown.worker);
        }
        if let Some(coordinator) = coordinator {
            coordinator
                .shutdown()
                .map_err(|_| RegionalGameRunnerError::CoordinatorFaulted)?;
        }

        Ok(RecoveredRegionalGame { workers })
    }

    fn check_health(&self) -> Result<(), RegionalGameRunnerError> {
        if self.health.fault().is_some() {
            return Err(RegionalGameRunnerError::CoordinatorFaulted);
        }
        Ok(())
    }

    fn next_runtime_request_id(&self) -> UiRequestId {
        UiRequestId(self.next_runtime_request.fetch_add(1, Ordering::Relaxed))
    }

    fn route_region_event(
        &self,
        region_id: RegionId,
        event: RegionEvent,
    ) -> Result<(), RegionalGameRunnerError> {
        self.worker_for_region(region_id)?;
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or(RegionalGameRunnerError::CoordinatorFaulted)?;
        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(region_id),
                event,
            })
            .map_err(|_| RegionalGameRunnerError::CoordinatorFaulted)
    }

    fn wait_for_runtime_reply(
        &self,
        request_id: UiRequestId,
        region_id: RegionId,
    ) -> Result<RuntimeReply, RegionalGameRunnerError> {
        for _ in 0..MAX_REPLY_PASSES {
            let summary = self.process_one_reply_pass()?;
            if let Some(reply) = summary
                .runtime_replies
                .into_iter()
                .find(|reply| match reply {
                    RuntimeReply::Inspect {
                        request_id: id,
                        region_id: region,
                        ..
                    }
                    | RuntimeReply::RoadTravelerPanelSeed {
                        request_id: id,
                        region_id: region,
                        ..
                    }
                    | RuntimeReply::BuildingAnchor {
                        request_id: id,
                        region_id: region,
                        ..
                    }
                    | RuntimeReply::RemoteWorkers {
                        request_id: id,
                        region_id: region,
                        ..
                    }
                    | RuntimeReply::PowerImportsSettled {
                        request_id: id,
                        region_id: region,
                    } => *id == request_id && *region == region_id,
                })
            {
                return Ok(reply);
            }
        }
        Err(RegionalGameRunnerError::RuntimeReplyMissing {
            request_id,
            region_id,
        })
    }

    fn wait_for_power_import_settlements(
        &self,
        request_id: UiRequestId,
        regions: &[RegionId],
    ) -> Result<(), RegionalGameRunnerError> {
        let mut pending = regions.to_vec();
        for _ in 0..MAX_REPLY_PASSES {
            let summary = self.process_one_reply_pass()?;
            for reply in summary.runtime_replies {
                let RuntimeReply::PowerImportsSettled {
                    request_id: reply_request_id,
                    region_id,
                } = reply
                else {
                    continue;
                };
                if reply_request_id != request_id {
                    continue;
                }
                if let Some(position) = pending.iter().position(|region| *region == region_id) {
                    pending.remove(position);
                }
            }
            if pending.is_empty() {
                return Ok(());
            }
        }
        Err(RegionalGameRunnerError::RuntimeReplyMissing {
            request_id,
            region_id: pending[0],
        })
    }

    fn collect_runner_replies(
        &self,
        summary: &mut WorkerRunSummary,
    ) -> Result<(), RegionalGameRunnerError> {
        let signals = self
            .runner_signals
            .lock()
            .expect("runner signal receiver poisoned");
        while let Ok(signal) = signals.try_recv() {
            match signal {
                RunnerSignal::Faulted => return Err(RegionalGameRunnerError::CoordinatorFaulted),
                RunnerSignal::CommandReply(reply) => summary.command_replies.push(reply),
                RunnerSignal::TickReply(reply) => summary.tick_replies.push(reply),
                RunnerSignal::SnapshotReply(reply) => summary.snapshot_replies.push(reply),
                RunnerSignal::RuntimeReply(reply) => summary.runtime_replies.push(reply),
            }
        }
        self.check_health()
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

    /// Advances movement by one 10-minute sub-tick across EVERY region, as a
    /// one coordinator-driven pass. Broadcasts `StepTravel` to all region
    /// mailboxes, then drains each region's inbox (any `ReceiveTraveler`s handed in
    /// last sub-tick — processed first, FIFO, inserting their visiting tokens — plus
    /// the one `StepTravel`) at a full-inbox budget, and delivers the crossings
    /// `StepTravel` emitted to neighbour inboxes for the NEXT sub-tick. ONE pass, not
    /// looped: looping would re-consume freshly-delivered handoffs in the same
    /// sub-tick and break the one-sub-tick-stale, can't-skip-two-regions guarantee.
    pub fn step_travel_city(&self) -> Result<(), RegionalGameRunnerError> {
        let _operation = self
            .operation_lock
            .lock()
            .expect("regional runner operation lock poisoned");
        let step = TravelStepId(self.next_travel_step.fetch_add(1, Ordering::Relaxed) + 1);
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or(RegionalGameRunnerError::CoordinatorFaulted)?;
        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::All,
                event: RegionEvent::StepTravel { step },
            })
            .map_err(|_| RegionalGameRunnerError::CoordinatorFaulted)?;
        coordinator
            .handle()
            .drain_until_idle()
            .map_err(|_| RegionalGameRunnerError::CoordinatorFaulted)?;
        self.check_health()?;
        Ok(())
    }

    /// Pumps every worker once while a synchronous runner call waits for its reply.
    ///
    /// This does not mean "produce exactly one reply." Region runtimes are event
    /// loops: the requested command/tick/snapshot may sit behind older events, or
    /// may need cross-region export request/grant events before its final reply
    /// exists. One pass gives each region at most one event of work, collects any
    /// replies produced along the way, then asks the coordinator to drain queued
    /// cross-worker events. The `wait_for_*` helpers repeat this bounded pass until
    /// the matching `(request_id, region_id)` reply is
    /// found or the safety cap is reached.
    fn process_one_reply_pass(&self) -> Result<WorkerRunSummary, RegionalGameRunnerError> {
        let mut combined = WorkerRunSummary::default();
        self.collect_runner_replies(&mut combined)?;
        let coordinator = self
            .coordinator
            .as_ref()
            .ok_or(RegionalGameRunnerError::CoordinatorFaulted)?;
        coordinator
            .handle()
            .drain_until_idle()
            .map_err(|_| RegionalGameRunnerError::CoordinatorFaulted)?;
        self.collect_runner_replies(&mut combined)?;
        Ok(combined)
    }

    fn process_worker_until_drained(&self) -> Result<(), RegionalGameRunnerError> {
        self.process_one_reply_pass()?;
        self.check_health()?;
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

/// Stable-sort key for merging remote workers: their home region id. Every entry
/// from `remote_workers_at` is a `LivesAt { region: Some(_) }`; the `u32::MAX`
/// fallback is unreachable and only keeps the key total.
fn home_region_key(worker: &CitizenDetailView) -> u32 {
    match worker.relation {
        CitizenRelation::LivesAt {
            region: Some(region),
            ..
        } => region.0,
        _ => u32::MAX,
    }
}

fn validate_region_worker_indexes(
    region_count: usize,
    worker_count: usize,
    region_worker_indexes: Option<Vec<usize>>,
) -> Result<Vec<usize>, RegionalGameRunnerError> {
    let Some(region_worker_indexes) = region_worker_indexes else {
        return Ok((0..region_count)
            .map(|index| index % worker_count)
            .collect());
    };

    if region_worker_indexes.len() != region_count {
        return Err(RegionalGameRunnerError::InvalidWorkerAssignmentCount {
            region_count,
            assignment_count: region_worker_indexes.len(),
        });
    }

    for (region_index, worker_index) in region_worker_indexes.iter().copied().enumerate() {
        if worker_index >= worker_count {
            return Err(RegionalGameRunnerError::InvalidWorkerAssignment {
                region_index,
                worker_index,
                worker_count,
            });
        }
    }

    Ok(region_worker_indexes)
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

        let prepared = PreparedThreadedRegionWorker::prepare(worker);
        let coordinator_workers =
            BTreeMap::from([(prepared.worker_id(), prepared.command_sender())]);
        let health = Arc::new(RunnerHealth::default());
        let (signal_sender, runner_signals) = mpsc::channel();
        let coordinator = RegionEventCoordinator::start(
            owners.clone(),
            coordinator_workers,
            Arc::clone(&health),
            signal_sender,
        );
        (
            RegionalGameRunner {
                workers: vec![prepared.start_with_coordinator(coordinator.handle())],
                owners,
                operation_lock: Mutex::new(()),
                next_travel_step: AtomicU64::new(0),
                next_runtime_request: AtomicU64::new(1),
                coordinator: Some(coordinator),
                runner_signals: Mutex::new(runner_signals),
                health,
            },
            handle,
        )
    }
}
