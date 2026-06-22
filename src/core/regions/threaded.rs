//! Opt-in OS thread runner for regional workers.
//!
//! This module keeps the default game path single-threaded. A caller can move a
//! whole `RegionWorker` into one OS thread, send region events through
//! `RegionHandle`, explicitly request bounded processing passes, and then
//! recover the worker during shutdown for deterministic inspection or handoff.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::core::regions::RegionId;
use crate::core::regions::worker::{
    ForwardedRegionEvent, RegionWorker, WorkerId, WorkerRoutingError, WorkerRunSummary,
};
use crate::interface::view::InspectView;

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

#[derive(Debug)]
/// Owns the control surface for one worker thread.
pub struct ThreadedRegionWorker {
    worker_id: WorkerId,
    commands: Sender<ThreadedWorkerCommand>,
    join_handle: Option<JoinHandle<()>>,
}

impl ThreadedRegionWorker {
    pub fn start(worker: RegionWorker) -> Self {
        let worker_id = worker.id();
        let (commands, command_receiver) = mpsc::channel();
        let join_handle = thread::spawn(move || run_worker(worker, command_receiver));

        Self {
            worker_id,
            commands,
            join_handle: Some(join_handle),
        }
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

    pub fn process_region_events_for_barrier(
        &self,
        max_events_per_region: usize,
    ) -> Result<WorkerRunSummary, ThreadedWorkerError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .send(ThreadedWorkerCommand::ProcessBarrier {
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

    pub fn deliver_forwarded_events(
        &self,
        events: Vec<ForwardedRegionEvent>,
    ) -> Result<Vec<WorkerRoutingError>, ThreadedWorkerError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .send(ThreadedWorkerCommand::DeliverForwarded {
                events,
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

    pub fn inspect_region(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<Option<InspectView>, ThreadedWorkerError> {
        // Like region_view, inspect reads the worker-owned runtime directly on
        // the worker thread after explicitly requested event processing.
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .send(ThreadedWorkerCommand::Inspect {
                region_id,
                x,
                y,
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

    /// Anchor `Position` of the building at `(region_id, x, y)`, read on the worker
    /// thread. The runner uses it to normalize a clicked footprint cell to the
    /// workplace anchor before the remote-roster fan-out.
    pub fn building_anchor_at(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Result<Option<crate::core::components::Position>, ThreadedWorkerError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .send(ThreadedWorkerCommand::BuildingAnchorAt {
                region_id,
                x,
                y,
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

    /// Remote staff (cross-region commuters) of the workplace at `(producer_region,
    /// pos)` that live in THIS worker's owned regions. Like `inspect_region`, it
    /// reads the worker-owned runtimes directly on the worker thread. The runner
    /// fans this out to every worker and merges the results.
    pub fn remote_workers_at(
        &self,
        producer_region: RegionId,
        pos: crate::core::components::Position,
    ) -> Result<Vec<crate::interface::view::CitizenDetailView>, ThreadedWorkerError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.commands
            .send(ThreadedWorkerCommand::RemoteWorkersAt {
                producer_region,
                pos,
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
enum ThreadedWorkerCommand {
    /// Run the normal single-worker scheduler.
    ///
    /// Same-worker outbound events are delivered immediately. This is kept for
    /// direct threaded-worker tests and single-worker utility calls; the
    /// multi-worker runner uses `ProcessBarrier` instead.
    Process {
        max_events_per_region: usize,
        reply: Sender<WorkerRunSummary>,
    },
    /// Run one deterministic-barrier scheduler pass.
    ///
    /// Purpose: make competing cross-region export requests deterministic across
    /// worker threads. A normal `Process` pass immediately delivers same-worker
    /// events, so a region on the producer's worker could reach the producer
    /// before a lower-key request from another worker. `ProcessBarrier` avoids
    /// that shortcut: every region-to-region outbound event, including
    /// same-worker targets, is returned as a `ForwardedRegionEvent`. The runner
    /// then merges all workers' events, sorts them by the stable routing key,
    /// and sends them back with `DeliverForwarded`.
    ///
    /// ```text
    /// Worker A              runner barrier              Worker B
    /// ────────              ──────────────              ────────
    /// ProcessBarrier ──┐
    ///                  ├─ collect all forwarded events
    /// ProcessBarrier ──┘
    ///                    sort by deterministic key
    ///                         ├─ DeliverForwarded -> target worker
    ///                         └─ DeliverForwarded -> target worker
    /// ```
    ProcessBarrier {
        max_events_per_region: usize,
        reply: Sender<WorkerRunSummary>,
    },
    /// Push already-sorted forwarded events into this worker's owned region inboxes.
    ///
    /// The runner sorts and groups events before sending this command; the worker
    /// only validates that each target region is owned here and enqueues it.
    DeliverForwarded {
        events: Vec<ForwardedRegionEvent>,
        reply: Sender<Vec<WorkerRoutingError>>,
    },
    /// Read an inspect view from one owned runtime on the worker thread.
    ///
    /// This keeps UI-facing reads out of ECS internals while avoiding direct
    /// access to `RegionRuntime` from the runner thread.
    Inspect {
        region_id: RegionId,
        x: usize,
        y: usize,
        reply: Sender<Option<InspectView>>,
    },
    /// Resolve the anchor `Position` of the building at `(region_id, x, y)`.
    ///
    /// Lets the runner normalize a clicked footprint cell to the workplace anchor
    /// before the remote-roster fan-out, so a multi-cell workplace lists its remote
    /// staff on every footprint cell.
    BuildingAnchorAt {
        region_id: RegionId,
        x: usize,
        y: usize,
        reply: Sender<Option<crate::core::components::Position>>,
    },
    /// Enumerate the remote staff of a workplace from this worker's owned regions.
    ///
    /// The reverse of `Inspect`'s local-only roster: it scans the consumer regions
    /// where commuters live, keyed on `(producer_region, pos)`.
    RemoteWorkersAt {
        producer_region: RegionId,
        pos: crate::core::components::Position,
        reply: Sender<Vec<crate::interface::view::CitizenDetailView>>,
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
            ThreadedWorkerCommand::Process {
                max_events_per_region,
                reply,
            } => {
                let _ = reply.send(worker.process_region_events(max_events_per_region));
            }
            ThreadedWorkerCommand::ProcessBarrier {
                max_events_per_region,
                reply,
            } => {
                let _ = reply.send(worker.process_region_events_for_barrier(max_events_per_region));
            }
            ThreadedWorkerCommand::DeliverForwarded { events, reply } => {
                let _ = reply.send(worker.deliver_forwarded_events(events));
            }
            ThreadedWorkerCommand::Inspect {
                region_id,
                x,
                y,
                reply,
            } => {
                let _ = reply.send(inspect_from_worker(&mut worker, region_id, x, y));
            }
            ThreadedWorkerCommand::BuildingAnchorAt {
                region_id,
                x,
                y,
                reply,
            } => {
                let _ = reply.send(worker.building_anchor_at(region_id, x, y));
            }
            ThreadedWorkerCommand::RemoteWorkersAt {
                producer_region,
                pos,
                reply,
            } => {
                let _ = reply.send(worker.remote_workers_at(producer_region, pos));
            }
            ThreadedWorkerCommand::Shutdown { mode, reply } => {
                let final_pass = match mode {
                    ThreadedWorkerShutdown::RejectPending => WorkerRunSummary::default(),
                    ThreadedWorkerShutdown::DrainOnce {
                        max_events_per_region,
                    } => worker.process_region_events(max_events_per_region),
                };
                let _ = reply.send(ThreadedWorkerShutdownResult { final_pass, worker });
                break;
            }
        }
    }
}

fn inspect_from_worker(
    worker: &mut RegionWorker,
    region_id: RegionId,
    x: usize,
    y: usize,
) -> Option<InspectView> {
    worker.refresh_importable_remote_jobs(region_id);
    worker.refresh_cross_region_goods_routes(region_id);
    worker
        .region_mut(region_id)
        .map(|runtime| runtime.inspect(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
