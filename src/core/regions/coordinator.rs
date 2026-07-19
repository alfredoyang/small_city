//! Coordinator-owned transport for cross-region runtime events.
//!
//! P1 deliberately leaves this transport inactive. It establishes one routing
//! authority and testable failure handling before P2 moves runtime output onto
//! it.

#![allow(dead_code)] // P1 installs the inactive transport seam; P2 activates it.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::core::regions::RegionId;
use crate::core::regions::runtime::RegionEvent;
use crate::core::regions::threaded::ThreadedWorkerCommand;
use crate::core::regions::worker::{RegionOwnerDirectory, WorkerId};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Coordinator-routable recipients for one region event.
pub(crate) enum RegionRecipients {
    One(RegionId),
    Many(Vec<RegionId>),
    All,
}

#[derive(Debug, Clone)]
/// One event plus the regions that must receive it.
pub(crate) struct RoutedRegionEvent {
    pub recipients: RegionRecipients,
    pub event: RegionEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A routing failure that stops the runner instead of silently dropping work.
pub(crate) enum CoordinatorError {
    MissingTargetRegion(RegionId),
    MissingWorker(WorkerId),
    WorkerStopped(WorkerId),
    CoordinatorStopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// First failure observed by the coordinator-owned execution path.
pub(crate) enum CoordinatorFault {
    Routing(CoordinatorError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Wake signal for the runner while the fault details stay in shared health.
pub(crate) enum RunnerSignal {
    Faulted,
}

#[derive(Debug, Default)]
/// Shared first-fault latch for coordinator and runner control paths.
pub(crate) struct RunnerHealth {
    fault: Mutex<Option<Arc<CoordinatorFault>>>,
}

impl RunnerHealth {
    pub(crate) fn fault(&self) -> Option<Arc<CoordinatorFault>> {
        self.fault
            .lock()
            .expect("runner health lock poisoned")
            .clone()
    }

    fn latch(&self, fault: CoordinatorFault) -> bool {
        let mut stored = self.fault.lock().expect("runner health lock poisoned");
        if stored.is_some() {
            return false;
        }
        *stored = Some(Arc::new(fault));
        true
    }
}

/// Handle used by runners and future workers to enqueue coordinator commands.
#[derive(Clone)]
pub(crate) struct CoordinatorHandle {
    commands: Sender<CoordinatorCommand>,
    health: Arc<RunnerHealth>,
}

impl CoordinatorHandle {
    pub(crate) fn route(&self, event: RoutedRegionEvent) -> Result<(), CoordinatorError> {
        self.route_events(vec![event])
    }

    pub(crate) fn route_events(
        &self,
        events: Vec<RoutedRegionEvent>,
    ) -> Result<(), CoordinatorError> {
        self.commands
            .send(CoordinatorCommand::Route(events))
            .map_err(|_| {
                let error = CoordinatorError::CoordinatorStopped;
                self.health.latch(CoordinatorFault::Routing(error.clone()));
                error
            })
    }
}

/// Coordinator thread and the handle used to enqueue its work.
pub(crate) struct RegionEventCoordinator {
    handle: CoordinatorHandle,
    join_handle: Option<JoinHandle<()>>,
}

impl RegionEventCoordinator {
    pub(crate) fn start(
        owners: Arc<RegionOwnerDirectory>,
        workers: BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
        health: Arc<RunnerHealth>,
        signals: Sender<RunnerSignal>,
    ) -> Self {
        let (commands, receiver) = mpsc::channel();
        let handle = CoordinatorHandle {
            commands,
            health: health.clone(),
        };
        let join_handle =
            thread::spawn(move || run_coordinator(receiver, owners, workers, health, signals));
        Self {
            handle,
            join_handle: Some(join_handle),
        }
    }

    pub(crate) fn handle(&self) -> CoordinatorHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) {
        let (reply, ready) = mpsc::channel();
        if self
            .handle
            .commands
            .send(CoordinatorCommand::Shutdown { reply })
            .is_ok()
        {
            let _ = ready.recv();
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

enum CoordinatorCommand {
    Route(Vec<RoutedRegionEvent>),
    Shutdown { reply: Sender<()> },
}

struct ResolvedDelivery {
    target_region: RegionId,
    worker_id: WorkerId,
    sender: Sender<ThreadedWorkerCommand>,
    event: RegionEvent,
}

fn run_coordinator(
    commands: mpsc::Receiver<CoordinatorCommand>,
    owners: Arc<RegionOwnerDirectory>,
    workers: BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
    health: Arc<RunnerHealth>,
    signals: Sender<RunnerSignal>,
) {
    for command in commands {
        match command {
            CoordinatorCommand::Route(events) => {
                if let Err(error) = route_events(&owners, &workers, events)
                    && health.latch(CoordinatorFault::Routing(error))
                {
                    let _ = signals.send(RunnerSignal::Faulted);
                }
            }
            CoordinatorCommand::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }
}

fn route_events(
    owners: &RegionOwnerDirectory,
    workers: &BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
    events: Vec<RoutedRegionEvent>,
) -> Result<(), CoordinatorError> {
    let mut deliveries = Vec::new();
    for routed in events {
        let recipients = normalized_recipients(owners, routed.recipients);
        for target_region in recipients {
            let worker_id = owners
                .owner_of(target_region)
                .ok_or(CoordinatorError::MissingTargetRegion(target_region))?;
            let sender = workers
                .get(&worker_id)
                .ok_or(CoordinatorError::MissingWorker(worker_id))?
                .clone();
            deliveries.push(ResolvedDelivery {
                target_region,
                worker_id,
                sender,
                event: routed.event.clone(),
            });
        }
    }

    for delivery in deliveries {
        delivery
            .sender
            .send(ThreadedWorkerCommand::Deliver {
                target_region: delivery.target_region,
                event: delivery.event,
            })
            .map_err(|_| CoordinatorError::WorkerStopped(delivery.worker_id))?;
    }
    Ok(())
}

fn normalized_recipients(
    owners: &RegionOwnerDirectory,
    recipients: RegionRecipients,
) -> Vec<RegionId> {
    let mut recipients = match recipients {
        RegionRecipients::One(region_id) => vec![region_id],
        RegionRecipients::Many(region_ids) => region_ids,
        RegionRecipients::All => owners.region_ids(),
    };
    recipients.sort_unstable();
    recipients.dedup();
    recipients
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    use super::*;
    use crate::core::regions::worker::RegionOwnerDirectory;

    const RECEIVE_TIMEOUT: Duration = Duration::from_millis(100);

    fn event() -> RegionEvent {
        RegionEvent::EmploymentDirectoryReady
    }

    fn coordinator(
        owners: Arc<RegionOwnerDirectory>,
        workers: BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
    ) -> (
        RegionEventCoordinator,
        Arc<RunnerHealth>,
        Receiver<RunnerSignal>,
    ) {
        let health = Arc::new(RunnerHealth::default());
        let (signals, receiver) = mpsc::channel();
        let coordinator = RegionEventCoordinator::start(owners, workers, health.clone(), signals);
        (coordinator, health, receiver)
    }

    fn register(owners: &RegionOwnerDirectory, region_id: u32, worker_id: u32) {
        owners
            .register_region(RegionId(region_id), WorkerId(worker_id))
            .expect("test region registration should succeed");
    }

    fn received_target(receiver: &Receiver<ThreadedWorkerCommand>) -> RegionId {
        match receiver
            .recv_timeout(RECEIVE_TIMEOUT)
            .expect("coordinator should deliver the event")
        {
            ThreadedWorkerCommand::Deliver {
                target_region,
                event: RegionEvent::EmploymentDirectoryReady,
            } => target_region,
            _ => panic!("unexpected worker command"),
        }
    }

    #[test]
    fn direct_event_reaches_its_owner() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 2, 7);
        let (sender, receiver) = mpsc::channel();
        let (coordinator, _health, _signals) =
            coordinator(owners, BTreeMap::from([(WorkerId(7), sender)]));

        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(2)),
                event: event(),
            })
            .expect("route command should enqueue");

        assert_eq!(received_target(&receiver), RegionId(2));
        coordinator.shutdown();
    }

    #[test]
    fn same_worker_cross_region_event_still_uses_coordinator() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 1, 7);
        register(&owners, 2, 7);
        let (sender, receiver) = mpsc::channel();
        let (coordinator, _health, _signals) =
            coordinator(owners, BTreeMap::from([(WorkerId(7), sender)]));

        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(2)),
                event: event(),
            })
            .expect("route command should enqueue");

        assert_eq!(received_target(&receiver), RegionId(2));
        coordinator.shutdown();
    }

    #[test]
    fn many_recipients_are_sorted_and_deduplicated() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        for region_id in [1, 2, 3] {
            register(&owners, region_id, 7);
        }
        let (sender, receiver) = mpsc::channel();
        let (coordinator, _health, _signals) =
            coordinator(owners, BTreeMap::from([(WorkerId(7), sender)]));

        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::Many(vec![
                    RegionId(3),
                    RegionId(1),
                    RegionId(3),
                    RegionId(2),
                ]),
                event: event(),
            })
            .expect("route command should enqueue");

        assert_eq!(received_target(&receiver), RegionId(1));
        assert_eq!(received_target(&receiver), RegionId(2));
        assert_eq!(received_target(&receiver), RegionId(3));
        coordinator.shutdown();
    }

    #[test]
    fn all_recipients_receive_once() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 1, 7);
        register(&owners, 2, 8);
        let (first_sender, first_receiver) = mpsc::channel();
        let (second_sender, second_receiver) = mpsc::channel();
        let (coordinator, _health, _signals) = coordinator(
            owners,
            BTreeMap::from([(WorkerId(7), first_sender), (WorkerId(8), second_sender)]),
        );

        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::All,
                event: event(),
            })
            .expect("route command should enqueue");

        assert_eq!(received_target(&first_receiver), RegionId(1));
        assert_eq!(received_target(&second_receiver), RegionId(2));
        coordinator.shutdown();
    }

    #[test]
    fn one_route_command_enqueues_every_valid_delivery() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 1, 7);
        register(&owners, 2, 7);
        let (sender, receiver) = mpsc::channel();
        let (coordinator, _health, _signals) =
            coordinator(owners, BTreeMap::from([(WorkerId(7), sender)]));

        coordinator
            .handle()
            .route_events(vec![
                RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(1)),
                    event: event(),
                },
                RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(2)),
                    event: event(),
                },
            ])
            .expect("route command should enqueue");

        assert_eq!(received_target(&receiver), RegionId(1));
        assert_eq!(received_target(&receiver), RegionId(2));
        coordinator.shutdown();
    }

    #[test]
    fn bad_target_prevents_every_delivery_in_the_batch() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 1, 7);
        let (sender, receiver) = mpsc::channel();
        let (coordinator, health, signals) =
            coordinator(owners, BTreeMap::from([(WorkerId(7), sender)]));

        coordinator
            .handle()
            .route_events(vec![
                RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(1)),
                    event: event(),
                },
                RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(2)),
                    event: event(),
                },
            ])
            .expect("route command should enqueue");

        assert_eq!(
            signals.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        );
        assert_eq!(
            health.fault().as_deref(),
            Some(&CoordinatorFault::Routing(
                CoordinatorError::MissingTargetRegion(RegionId(2))
            ))
        );
        assert!(receiver.recv_timeout(RECEIVE_TIMEOUT).is_err());
        coordinator.shutdown();
    }

    #[test]
    fn missing_region_latches_health_and_notifies_runner() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        let (coordinator, health, signals) = coordinator(owners, BTreeMap::new());

        coordinator
            .handle()
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(99)),
                event: event(),
            })
            .expect("route command should enqueue");

        assert_eq!(
            signals.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        );
        assert_eq!(
            health.fault().as_deref(),
            Some(&CoordinatorFault::Routing(
                CoordinatorError::MissingTargetRegion(RegionId(99))
            ))
        );
        coordinator.shutdown();
    }

    #[test]
    fn stopped_worker_latches_first_fault_and_notifies_runner() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 1, 7);
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let (coordinator, health, signals) =
            coordinator(owners, BTreeMap::from([(WorkerId(7), sender)]));
        let handle = coordinator.handle();

        handle
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(1)),
                event: event(),
            })
            .expect("route command should enqueue");
        assert_eq!(
            signals.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        );

        handle
            .route(RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(99)),
                event: event(),
            })
            .expect("later route command should enqueue");
        assert!(signals.recv_timeout(RECEIVE_TIMEOUT).is_err());
        assert_eq!(
            health.fault().as_deref(),
            Some(&CoordinatorFault::Routing(CoordinatorError::WorkerStopped(
                WorkerId(7)
            )))
        );
        coordinator.shutdown();
    }

    #[test]
    fn shutdown_joins_the_coordinator_thread() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        let (coordinator, _health, _signals) = coordinator(owners, BTreeMap::new());
        coordinator.shutdown();
    }
}
