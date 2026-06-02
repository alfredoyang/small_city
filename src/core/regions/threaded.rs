//! Opt-in OS thread runner for regional workers.
//!
//! This module keeps the default game path single-threaded. A caller can move a
//! whole `RegionWorker` into one OS thread, send region events through
//! `RegionHandle`, explicitly request bounded processing passes, and then
//! recover the worker during shutdown for deterministic inspection or handoff.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::core::regions::RegionId;
use crate::core::regions::worker::{RegionWorker, WorkerId, WorkerRunSummary};
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

enum ThreadedWorkerCommand {
    Process {
        max_events_per_region: usize,
        reply: Sender<WorkerRunSummary>,
    },
    Inspect {
        region_id: RegionId,
        x: usize,
        y: usize,
        reply: Sender<Option<InspectView>>,
    },
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
            ThreadedWorkerCommand::Inspect {
                region_id,
                x,
                y,
                reply,
            } => {
                let _ = reply.send(inspect_from_worker(&worker, region_id, x, y));
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
    worker: &RegionWorker,
    region_id: RegionId,
    x: usize,
    y: usize,
) -> Option<InspectView> {
    worker
        .region(region_id)
        .map(|runtime| runtime.state().inspect(x, y))
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
