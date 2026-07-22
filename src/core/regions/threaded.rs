//! Opt-in OS thread runner for regional workers.
//!
//! This module keeps the default game path single-threaded. A caller can move a
//! whole `RegionWorker` into one OS thread, send region events through
//! `RegionHandle`, explicitly request bounded processing passes, and then
//! recover the worker during shutdown for deterministic inspection or handoff.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::core::regions::RegionId;
use crate::core::regions::coordinator::CoordinatorHandle;
use crate::core::regions::worker::{RegionWorker, WorkerId, WorkerRunSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic shutdown behavior for a threaded worker.
pub enum ThreadedWorkerShutdown {
    /// Return the worker without consuming pending mailbox events.
    RejectPending,
    /// Run one final bounded scheduling pass before returning the worker.
    DrainOnce { max_events_per_region: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Lifecycle errors returned by the threaded worker control surface.
pub enum ThreadedWorkerError {
    WorkerThreadStopped { worker_id: WorkerId },
    WorkerThreadPanicked { worker_id: WorkerId },
}

#[derive(Debug)]
/// Worker returned from an explicit threaded shutdown.
pub struct ThreadedWorkerShutdownResult {
    pub final_pass: WorkerRunSummary,
    pub worker: RegionWorker,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Report of whether a coordinator-driven worker reached quiescence.
pub(crate) struct WorkerIdleReport {
    pub pending_events: bool,
    pub dirty_hints: bool,
}

const MAX_AUTONOMOUS_ROUNDS: usize = 32;
const MAX_EVENTS_PER_AUTONOMOUS_SLICE: usize = 32;

#[derive(Debug)]
/// Owns the control surface for one worker thread.
pub struct ThreadedRegionWorker {
    worker_id: WorkerId,
    commands: Sender<ThreadedWorkerCommand>,
    join_handle: Option<JoinHandle<()>>,
}

/// Worker command channel prepared before the worker thread starts.
///
/// The coordinator owns a clone of the sender. P1 only prepares this seam;
/// P2 will make the worker's event loop consume coordinator deliveries.
pub(crate) struct PreparedThreadedRegionWorker {
    worker_id: WorkerId,
    worker: RegionWorker,
    commands: Sender<ThreadedWorkerCommand>,
    command_receiver: Receiver<ThreadedWorkerCommand>,
}

impl PreparedThreadedRegionWorker {
    pub(crate) fn prepare(worker: RegionWorker) -> Self {
        let worker_id = worker.id();
        let (commands, command_receiver) = mpsc::channel();
        Self {
            worker_id,
            worker,
            commands,
            command_receiver,
        }
    }

    #[allow(dead_code)] // P1 prepares this coordinator-facing channel for P2.
    pub(crate) fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    #[allow(dead_code)] // P1 prepares this coordinator-facing channel for P2.
    pub(crate) fn command_sender(&self) -> Sender<ThreadedWorkerCommand> {
        self.commands.clone()
    }

    pub(crate) fn start(self) -> ThreadedRegionWorker {
        let join_handle = thread::spawn(move || run_worker(self.worker, self.command_receiver));
        ThreadedRegionWorker {
            worker_id: self.worker_id,
            commands: self.commands,
            join_handle: Some(join_handle),
        }
    }

    /// Starts the coordinator-driven worker loop.
    pub(crate) fn start_with_coordinator(
        self,
        coordinator: CoordinatorHandle,
    ) -> ThreadedRegionWorker {
        let join_handle = thread::spawn(move || {
            run_worker_with_coordinator(self.worker, self.command_receiver, coordinator)
        });
        ThreadedRegionWorker {
            worker_id: self.worker_id,
            commands: self.commands,
            join_handle: Some(join_handle),
        }
    }
}

impl ThreadedRegionWorker {
    pub fn start(worker: RegionWorker) -> Self {
        PreparedThreadedRegionWorker::prepare(worker).start()
    }

    pub fn worker_id(&self) -> WorkerId {
        self.worker_id
    }

    pub fn process_region_events(
        &self,
        max_events_per_region: usize,
    ) -> Result<WorkerRunSummary, ThreadedWorkerError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .send(ThreadedWorkerCommand::Process {
                max_events_per_region,
                reply: reply_sender,
            })
            .map_err(|_| ThreadedWorkerError::WorkerThreadStopped {
                worker_id: self.worker_id,
            })?;

        reply_receiver
            .recv()
            .map_err(|_| ThreadedWorkerError::WorkerThreadStopped {
                worker_id: self.worker_id,
            })
    }

    pub fn shutdown(
        mut self,
        mode: ThreadedWorkerShutdown,
    ) -> Result<ThreadedWorkerShutdownResult, ThreadedWorkerError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        if self
            .commands
            .send(ThreadedWorkerCommand::Shutdown {
                mode,
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(self.stopped_error_after_join());
        }

        let result = match reply_receiver.recv() {
            Ok(result) => result,
            Err(_) => return Err(self.stopped_error_after_join()),
        };
        self.join()?;

        Ok(result)
    }

    fn stopped_error_after_join(&mut self) -> ThreadedWorkerError {
        self.join()
            .err()
            .unwrap_or(ThreadedWorkerError::WorkerThreadStopped {
                worker_id: self.worker_id,
            })
    }

    fn join(&mut self) -> Result<(), ThreadedWorkerError> {
        let Some(join_handle) = self.join_handle.take() else {
            return Ok(());
        };

        join_handle
            .join()
            .map_err(|_| ThreadedWorkerError::WorkerThreadPanicked {
                worker_id: self.worker_id,
            })
    }
}

/// Private control messages sent from `ThreadedRegionWorker` to its worker thread.
///
/// Region events still enter regions through `RegionHandle`; this enum is only
/// for scheduler/control operations that must run on the worker-owning thread.
pub(crate) enum ThreadedWorkerCommand {
    /// Deliver one coordinator-routed event to an owned region inbox.
    Deliver {
        target_region: RegionId,
        event: crate::core::regions::runtime::RegionEvent,
    },
    /// Quiescence report for the coordinator drain loop.
    DrainReport { reply: Sender<WorkerIdleReport> },
    /// Run the normal single-worker scheduler.
    Process {
        max_events_per_region: usize,
        reply: Sender<WorkerRunSummary>,
    },
    /// Stop the worker thread and return the owned `RegionWorker`.
    ///
    /// The shutdown mode decides whether pending events are rejected or one final
    /// bounded pass is run before ownership returns to the caller.
    Shutdown {
        mode: ThreadedWorkerShutdown,
        reply: Sender<ThreadedWorkerShutdownResult>,
    },
}

fn run_worker(mut worker: RegionWorker, commands: Receiver<ThreadedWorkerCommand>) {
    for command in commands {
        match command {
            ThreadedWorkerCommand::Deliver {
                target_region,
                event,
            } => {
                worker
                    .push_event(target_region, event)
                    .expect("coordinator routed an event to the wrong worker");
            }
            ThreadedWorkerCommand::DrainReport { reply } => {
                let _ = reply.send(WorkerIdleReport {
                    pending_events: worker.has_pending_events(),
                    dirty_hints: worker.has_dirty_hints(),
                });
            }
            ThreadedWorkerCommand::Process {
                max_events_per_region,
                reply,
            } => {
                let _ = reply.send(worker.process_region_events(max_events_per_region));
            }
            ThreadedWorkerCommand::Shutdown { mode, reply } => {
                let final_pass = match mode {
                    ThreadedWorkerShutdown::RejectPending => WorkerRunSummary::default(),
                    ThreadedWorkerShutdown::DrainOnce {
                        max_events_per_region,
                    } => worker.process_region_events(max_events_per_region),
                };
                let _ = reply.send(ThreadedWorkerShutdownResult { final_pass, worker });
                return;
            }
        }
    }
}

fn run_worker_with_coordinator(
    mut worker: RegionWorker,
    commands: Receiver<ThreadedWorkerCommand>,
    coordinator: CoordinatorHandle,
) {
    let mut exit = WorkerExitGuard {
        worker_id: worker.id(),
        coordinator: coordinator.clone(),
        expected: false,
        reported: false,
    };
    for command in commands {
        match command {
            ThreadedWorkerCommand::Deliver {
                target_region,
                event,
            } => {
                if worker.push_event(target_region, event).is_err() {
                    break;
                }
                match drive_autonomous_worker(&mut worker, &coordinator) {
                    AutonomousDriveResult::Idle => {}
                    AutonomousDriveResult::Stopped => break,
                    AutonomousDriveResult::RoundLimit => {
                        exit.reported = true;
                        break;
                    }
                }
            }
            ThreadedWorkerCommand::DrainReport { reply } => {
                if worker.has_dirty_hints() {
                    match drive_autonomous_worker(&mut worker, &coordinator) {
                        AutonomousDriveResult::Idle => {}
                        AutonomousDriveResult::Stopped | AutonomousDriveResult::RoundLimit => {
                            exit.reported = true;
                            break;
                        }
                    }
                }
                let _ = reply.send(WorkerIdleReport {
                    pending_events: worker.has_pending_events(),
                    dirty_hints: worker.has_dirty_hints(),
                });
            }
            ThreadedWorkerCommand::Process {
                max_events_per_region,
                reply,
            } => {
                let _ = reply.send(worker.process_region_events(max_events_per_region));
            }
            ThreadedWorkerCommand::Shutdown { mode, reply } => {
                let final_pass = match mode {
                    ThreadedWorkerShutdown::RejectPending => WorkerRunSummary::default(),
                    ThreadedWorkerShutdown::DrainOnce {
                        max_events_per_region,
                    } => worker.process_region_events(max_events_per_region),
                };
                let _ = reply.send(ThreadedWorkerShutdownResult { final_pass, worker });
                exit.expected = true;
                return;
            }
        }
    }
}

/// Makes worker failure observable even when a scheduler round panics.
struct WorkerExitGuard {
    worker_id: WorkerId,
    coordinator: CoordinatorHandle,
    expected: bool,
    reported: bool,
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        if !self.expected && !self.reported {
            self.coordinator.worker_stopped(self.worker_id);
        }
    }
}

enum AutonomousDriveResult {
    Idle,
    Stopped,
    RoundLimit,
}

fn drive_autonomous_worker(
    worker: &mut RegionWorker,
    coordinator: &CoordinatorHandle,
) -> AutonomousDriveResult {
    for _ in 0..MAX_AUTONOMOUS_ROUNDS {
        if coordinator.health_failed() {
            return AutonomousDriveResult::Stopped;
        }
        let round = worker.process_autonomous_round(MAX_EVENTS_PER_AUTONOMOUS_SLICE);
        if !round.routing_errors.is_empty() {
            return AutonomousDriveResult::Stopped;
        }
        for reply in round.command_replies {
            coordinator.command_reply(reply);
        }
        for reply in round.tick_replies {
            coordinator.tick_reply(reply);
        }
        for reply in round.snapshot_replies {
            coordinator.snapshot_reply(reply);
        }
        for reply in round.runtime_replies {
            coordinator.runtime_reply(reply);
        }
        for event in round.coordinator_events {
            if coordinator.route(event).is_err() {
                return AutonomousDriveResult::Stopped;
            }
        }
        if !worker.has_pending_events() && !worker.has_dirty_hints() {
            return AutonomousDriveResult::Idle;
        }
    }
    coordinator.worker_round_limit_exceeded(worker.id());
    AutonomousDriveResult::RoundLimit
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::core::regions::RegionState;
    use crate::core::regions::coordinator::{
        RegionEventCoordinator, RegionRecipients, RoutedRegionEvent, RunnerHealth,
    };
    use crate::core::regions::directory::RegionDirectory;
    use crate::core::regions::runtime::RegionRuntime;
    use crate::core::regions::worker::RegionOwnerDirectory;

    #[test]
    fn shutdown_reports_panic_when_worker_thread_panicked() {
        let (commands, receiver) = mpsc::channel();
        drop(receiver);
        let worker_id = WorkerId(99);
        let threaded = ThreadedRegionWorker {
            worker_id,
            commands,
            join_handle: Some(thread::spawn(|| panic!("intentional test panic"))),
        };

        let error = threaded
            .shutdown(ThreadedWorkerShutdown::RejectPending)
            .expect_err("panic should be reported");

        assert_eq!(
            error,
            ThreadedWorkerError::WorkerThreadPanicked { worker_id }
        );
    }

    #[test]
    fn coordinator_delivery_wakes_a_sleeping_worker() {
        let worker_id = WorkerId(7);
        let region_id = RegionId(1);
        let owners = Arc::new(RegionOwnerDirectory::new());
        let directory = Arc::new(RegionDirectory::with_owners(
            Vec::new(),
            Arc::clone(&owners),
        ));
        let mut worker =
            RegionWorker::with_directory_and_owners(worker_id, directory, Arc::clone(&owners));
        worker
            .add_region(RegionRuntime::new(RegionState::new(region_id, 2, 2)))
            .expect("test region should attach");

        let prepared = PreparedThreadedRegionWorker::prepare(worker);
        let worker_sender = prepared.command_sender();
        let health = Arc::new(RunnerHealth::default());
        let (signal_sender, signal_receiver) = mpsc::channel();
        let coordinator = RegionEventCoordinator::start(
            owners,
            BTreeMap::from([(worker_id, worker_sender)]),
            health.clone(),
            signal_sender,
        );
        let threaded = prepared.start_with_coordinator(coordinator.handle());

        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(region_id),
                event: crate::core::regions::runtime::RegionEvent::Tick {
                    request_id: crate::core::regional_types::UiRequestId(1),
                },
            })
            .expect("delivery should queue");
        coordinator
            .handle()
            .drain_until_idle()
            .expect("coordinator should drain delivered work");

        let shutdown = threaded
            .shutdown(ThreadedWorkerShutdown::RejectPending)
            .expect("worker should stop cleanly");
        assert_eq!(
            shutdown
                .worker
                .region(region_id)
                .expect("region should remain owned")
                .state()
                .world
                .resources
                .turn,
            1
        );
        assert!(matches!(
            signal_receiver.try_recv(),
            Ok(crate::core::regions::coordinator::RunnerSignal::TickReply(
                _
            ))
        ));
        assert!(health.fault().is_none());
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
    }
}
