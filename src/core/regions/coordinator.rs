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
use std::time::Duration;

use crate::core::regional_types::{
    RegionCommandResponse, RegionSnapshotResponse, RegionTickResponse,
};
use crate::core::regions::runtime::{RegionEvent, RuntimeReply};
use crate::core::regions::threaded::{ThreadedWorkerCommand, WorkerIdleReport};
use crate::core::regions::worker::{RegionOwnerDirectory, WorkerId};
use crate::core::regions::{GoodsSupplyGrant, PowerExportGrant, RegionId};

#[derive(Debug, Clone, PartialEq, Eq)]
/// Coordinator-routable recipients for one region event.
pub enum RegionRecipients {
    One(RegionId),
    Many(Vec<RegionId>),
    All,
}

#[derive(Debug, Clone)]
/// One event plus the regions that must receive it.
pub struct RoutedRegionEvent {
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
    DrainLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// First failure observed by the coordinator-owned execution path.
pub(crate) enum CoordinatorFault {
    Routing(CoordinatorError),
    CoordinatorStopped,
    WorkerRoundLimitExceeded(WorkerId),
    WorkerStopped(WorkerId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoordinatorShutdownError {
    ThreadPanicked,
}

#[derive(Debug)]
/// Runtime reply or fault delivered to the runner.
pub(crate) enum RunnerSignal {
    Faulted,
    CommandReply(RegionCommandResponse),
    TickReply(RegionTickResponse),
    SnapshotReply(RegionSnapshotResponse),
    RuntimeReply(RuntimeReply),
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
                // P1 returns this error synchronously. P5 adds the wake needed
                // when another thread is blocked awaiting a runner reply.
                self.health.latch(CoordinatorFault::CoordinatorStopped);
                error
            })
    }

    pub(crate) fn health_failed(&self) -> bool {
        self.health.fault().is_some()
    }

    pub(crate) fn worker_round_limit_exceeded(&self, worker_id: WorkerId) {
        let _ = self.commands.send(CoordinatorCommand::Fault(
            CoordinatorFault::WorkerRoundLimitExceeded(worker_id),
        ));
    }

    pub(crate) fn worker_stopped(&self, worker_id: WorkerId) {
        let _ = self
            .commands
            .send(CoordinatorCommand::Fault(CoordinatorFault::WorkerStopped(
                worker_id,
            )));
    }

    pub(crate) fn command_reply(&self, reply: RegionCommandResponse) {
        let _ = self.commands.send(CoordinatorCommand::CommandReply(reply));
    }

    pub(crate) fn tick_reply(&self, reply: RegionTickResponse) {
        let _ = self.commands.send(CoordinatorCommand::TickReply(reply));
    }

    pub(crate) fn snapshot_reply(&self, reply: RegionSnapshotResponse) {
        let _ = self.commands.send(CoordinatorCommand::SnapshotReply(reply));
    }

    pub(crate) fn runtime_reply(&self, reply: RuntimeReply) {
        let _ = self.commands.send(CoordinatorCommand::RuntimeReply(reply));
    }

    /// Fence that waits until coordinator-routed worker events reach quiescence.
    pub(crate) fn drain_until_idle(&self) -> Result<(), CoordinatorError> {
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(CoordinatorCommand::DrainUntilIdle { reply })
            .map_err(|_| CoordinatorError::CoordinatorStopped)?;
        receiver
            .recv_timeout(DRAIN_REPLY_TIMEOUT)
            .map_err(|_| CoordinatorError::CoordinatorStopped)?
    }
}

/// Coordinator thread and the handle used to enqueue its work.
pub(crate) struct RegionEventCoordinator {
    handle: CoordinatorHandle,
    join_handle: Option<JoinHandle<()>>,
    stopped: bool,
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
            stopped: false,
        }
    }

    pub(crate) fn handle(&self) -> CoordinatorHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) -> Result<(), CoordinatorShutdownError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), CoordinatorShutdownError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
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
            join_handle
                .join()
                .map_err(|_| CoordinatorShutdownError::ThreadPanicked)?;
        }
        Ok(())
    }
}

impl Drop for RegionEventCoordinator {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

enum CoordinatorCommand {
    Route(Vec<RoutedRegionEvent>),
    CommandReply(RegionCommandResponse),
    TickReply(RegionTickResponse),
    SnapshotReply(RegionSnapshotResponse),
    RuntimeReply(RuntimeReply),
    Fault(CoordinatorFault),
    DrainUntilIdle {
        reply: Sender<Result<(), CoordinatorError>>,
    },
    Shutdown {
        reply: Sender<()>,
    },
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
    while let Ok(command) = commands.recv() {
        match command {
            CoordinatorCommand::Route(events) => {
                if let Err(error) = route_events(&owners, &workers, events)
                    && health.latch(CoordinatorFault::Routing(error))
                {
                    let _ = signals.send(RunnerSignal::Faulted);
                }
            }
            CoordinatorCommand::Fault(fault) => {
                if health.latch(fault) {
                    let _ = signals.send(RunnerSignal::Faulted);
                }
            }
            CoordinatorCommand::CommandReply(reply) => {
                let _ = signals.send(RunnerSignal::CommandReply(reply));
            }
            CoordinatorCommand::TickReply(reply) => {
                let _ = signals.send(RunnerSignal::TickReply(reply));
            }
            CoordinatorCommand::SnapshotReply(reply) => {
                let _ = signals.send(RunnerSignal::SnapshotReply(reply));
            }
            CoordinatorCommand::RuntimeReply(reply) => {
                let _ = signals.send(RunnerSignal::RuntimeReply(reply));
            }
            CoordinatorCommand::DrainUntilIdle { reply } => {
                let outcome = drain_until_idle(&commands, &owners, &workers, &health, &signals);
                let _ = reply.send(outcome.result);
                if let Some(shutdown_reply) = outcome.shutdown_reply {
                    let _ = shutdown_reply.send(());
                    break;
                }
            }
            CoordinatorCommand::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }
}

const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_REPLY_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_DRAIN_ROUNDS: usize = 32;

struct DrainOutcome {
    result: Result<(), CoordinatorError>,
    shutdown_reply: Option<Sender<()>>,
}

fn drain_until_idle(
    commands: &mpsc::Receiver<CoordinatorCommand>,
    owners: &RegionOwnerDirectory,
    workers: &BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
    health: &RunnerHealth,
    signals: &Sender<RunnerSignal>,
) -> DrainOutcome {
    for _ in 0..MAX_DRAIN_ROUNDS {
        if health.fault().is_some() {
            return DrainOutcome {
                result: Err(CoordinatorError::CoordinatorStopped),
                shutdown_reply: None,
            };
        }
        let workers_idle = match workers_are_idle(workers) {
            Ok(workers_idle) => workers_idle,
            Err(error) => return drain_error(health, signals, error),
        };
        let mut routed = false;
        while let Ok(command) = commands.try_recv() {
            match command {
                CoordinatorCommand::Route(events) => {
                    if let Err(error) = route_events(owners, workers, events) {
                        return drain_error(health, signals, error);
                    }
                    routed = true;
                }
                CoordinatorCommand::Fault(fault) => {
                    if health.latch(fault) {
                        let _ = signals.send(RunnerSignal::Faulted);
                    }
                    return DrainOutcome {
                        result: Err(CoordinatorError::CoordinatorStopped),
                        shutdown_reply: None,
                    };
                }
                CoordinatorCommand::CommandReply(reply) => {
                    let _ = signals.send(RunnerSignal::CommandReply(reply));
                }
                CoordinatorCommand::TickReply(reply) => {
                    let _ = signals.send(RunnerSignal::TickReply(reply));
                }
                CoordinatorCommand::SnapshotReply(reply) => {
                    let _ = signals.send(RunnerSignal::SnapshotReply(reply));
                }
                CoordinatorCommand::RuntimeReply(reply) => {
                    let _ = signals.send(RunnerSignal::RuntimeReply(reply));
                }
                CoordinatorCommand::DrainUntilIdle { reply } => {
                    let _ = reply.send(Err(CoordinatorError::DrainLimitExceeded));
                }
                CoordinatorCommand::Shutdown { reply } => {
                    return DrainOutcome {
                        result: Err(CoordinatorError::CoordinatorStopped),
                        shutdown_reply: Some(reply),
                    };
                }
            }
        }
        if workers_idle && !routed {
            return DrainOutcome {
                result: Ok(()),
                shutdown_reply: None,
            };
        }
    }
    drain_error(health, signals, CoordinatorError::DrainLimitExceeded)
}

fn drain_error(
    health: &RunnerHealth,
    signals: &Sender<RunnerSignal>,
    error: CoordinatorError,
) -> DrainOutcome {
    let fault = match error {
        CoordinatorError::WorkerStopped(worker_id) => CoordinatorFault::WorkerStopped(worker_id),
        _ => CoordinatorFault::Routing(error.clone()),
    };
    if health.latch(fault) {
        let _ = signals.send(RunnerSignal::Faulted);
    }
    DrainOutcome {
        result: Err(error),
        shutdown_reply: None,
    }
}

fn workers_are_idle(
    workers: &BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
) -> Result<bool, CoordinatorError> {
    let mut replies = Vec::new();
    for (worker_id, sender) in workers {
        let (reply, receiver) = mpsc::channel();
        if sender
            .send(ThreadedWorkerCommand::DrainReport { reply })
            .is_err()
        {
            return Err(CoordinatorError::WorkerStopped(*worker_id));
        }
        replies.push(receiver);
    }
    Ok(replies.into_iter().all(|receiver| {
        receiver.recv_timeout(CONTROL_REPLY_TIMEOUT).is_ok_and(
            |WorkerIdleReport {
                 pending_events,
                 dirty_hints,
             }| !pending_events && !dirty_hints,
        )
    }))
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
            let Some(worker_id) = owners.owner_of(target_region) else {
                if let Some(denial) = missing_export_target_denial(&routed.event) {
                    let caller = denial.caller_region;
                    let caller_worker = owners
                        .owner_of(caller)
                        .ok_or(CoordinatorError::MissingTargetRegion(caller))?;
                    let sender = workers
                        .get(&caller_worker)
                        .ok_or(CoordinatorError::MissingWorker(caller_worker))?
                        .clone();
                    deliveries.push(ResolvedDelivery {
                        target_region: caller,
                        worker_id: caller_worker,
                        sender,
                        event: denial.event,
                    });
                    continue;
                }
                if is_export_release(&routed.event) {
                    continue;
                }
                return Err(CoordinatorError::MissingTargetRegion(target_region));
            };
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

fn is_export_release(event: &RegionEvent) -> bool {
    matches!(
        event,
        RegionEvent::ReleasePowerExportAllocations(_)
            | RegionEvent::ReleaseGoodsSupplyAllocations(_)
    )
}

/// A discovery snapshot can outlive a region reassignment. Treat a missing
/// producer as an ordinary rejected attempt so the caller runtime can walk its
/// remaining snapshot candidates; do not turn stale availability into a runner
/// fault. All other missing targets remain routing failures.
fn missing_export_target_denial(event: &RegionEvent) -> Option<MissingExportTargetDenial> {
    match event {
        RegionEvent::ProcessPowerExportRequest(request) => Some(MissingExportTargetDenial {
            caller_region: request.request.caller_region,
            event: RegionEvent::ApplyPowerExportGrant {
                request: request.clone(),
                grant: PowerExportGrant {
                    token: request.request.token,
                    granted: false,
                    source_region: None,
                },
            },
        }),
        RegionEvent::ProcessGoodsSupplyRequest(request) => Some(MissingExportTargetDenial {
            caller_region: request.request.caller_region,
            event: RegionEvent::ApplyGoodsSupplyGrant {
                request: request.clone(),
                grant: GoodsSupplyGrant {
                    token: request.request.token,
                    granted: false,
                    source_region: None,
                    units: 0,
                },
            },
        }),
        _ => None,
    }
}

struct MissingExportTargetDenial {
    caller_region: RegionId,
    event: RegionEvent,
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
    use crate::core::entity::Entity;
    use crate::core::regional_types::UiRequestId;
    use crate::core::regions::runtime::{ExportAllocationRequest, PowerExportRequest};
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

    #[test]
    fn missing_power_producer_becomes_a_denial_for_the_caller() {
        let caller = RegionId(1);
        let event = RegionEvent::ProcessPowerExportRequest(ExportAllocationRequest {
            request: PowerExportRequest {
                request_id: UiRequestId(4),
                caller_region: caller,
                caller_network: crate::core::regions::RegionRoadNetworkId {
                    region: caller,
                    road_network: 0,
                },
                token: 7,
                demand: 1,
                consumer: Entity::new(caller, 3),
            },
            candidates: vec![crate::core::regions::RegionRoadNetworkId {
                region: RegionId(2),
                road_network: 0,
            }],
            candidate_index: 0,
        });

        let denial = missing_export_target_denial(&event).expect("export request");
        assert_eq!(denial.caller_region, caller);
        assert!(matches!(
            denial.event,
            RegionEvent::ApplyPowerExportGrant { grant, .. }
                if !grant.granted && grant.source_region.is_none()
        ));
    }

    #[test]
    fn missing_power_producer_delivery_reaches_the_caller() {
        let owners = RegionOwnerDirectory::new();
        register(&owners, 1, 7);
        let (sender, receiver) = mpsc::channel();
        let workers = BTreeMap::from([(WorkerId(7), sender)]);
        let request = ExportAllocationRequest {
            request: PowerExportRequest {
                request_id: UiRequestId(4),
                caller_region: RegionId(1),
                caller_network: crate::core::regions::RegionRoadNetworkId {
                    region: RegionId(1),
                    road_network: 0,
                },
                token: 7,
                demand: 1,
                consumer: Entity::new(RegionId(1), 3),
            },
            candidates: vec![crate::core::regions::RegionRoadNetworkId {
                region: RegionId(2),
                road_network: 0,
            }],
            candidate_index: 0,
        };

        route_events(
            &owners,
            &workers,
            vec![RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(2)),
                event: RegionEvent::ProcessPowerExportRequest(request),
            }],
        )
        .expect("missing producer is a normal denial");

        match receiver
            .recv_timeout(RECEIVE_TIMEOUT)
            .expect("caller delivery")
        {
            ThreadedWorkerCommand::Deliver {
                target_region,
                event: RegionEvent::ApplyPowerExportGrant { grant, .. },
            } => {
                assert_eq!(target_region, RegionId(1));
                assert!(!grant.granted);
            }
            _ => panic!("unexpected coordinator delivery"),
        }
    }

    #[test]
    fn missing_export_release_is_not_a_routing_fault() {
        let owners = RegionOwnerDirectory::new();
        let workers = BTreeMap::new();
        route_events(
            &owners,
            &workers,
            vec![RoutedRegionEvent {
                recipients: RegionRecipients::One(RegionId(2)),
                event: RegionEvent::ReleasePowerExportAllocations(
                    crate::core::regions::runtime::ExportAllocationRelease {
                        caller_region: RegionId(1),
                        request_id: UiRequestId(4),
                        producer_regions: vec![RegionId(2)],
                    },
                ),
            }],
        )
        .expect("a departed producer has no allocation state left to release");
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
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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
            .route_events(vec![
                RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(2)),
                    event: event(),
                },
                RoutedRegionEvent {
                    recipients: RegionRecipients::One(RegionId(1)),
                    event: event(),
                },
            ])
            .expect("route command should enqueue");

        assert_eq!(received_target(&receiver), RegionId(2));
        assert_eq!(received_target(&receiver), RegionId(1));
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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

        assert!(matches!(
            signals.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        ));
        assert_eq!(
            health.fault().as_deref(),
            Some(&CoordinatorFault::Routing(
                CoordinatorError::MissingTargetRegion(RegionId(2))
            ))
        );
        assert!(receiver.recv_timeout(RECEIVE_TIMEOUT).is_err());
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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

        assert!(matches!(
            signals.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        ));
        assert_eq!(
            health.fault().as_deref(),
            Some(&CoordinatorFault::Routing(
                CoordinatorError::MissingTargetRegion(RegionId(99))
            ))
        );
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
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
        assert!(matches!(
            signals.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        ));

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
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
    }

    #[test]
    fn shutdown_joins_the_coordinator_thread() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        let (coordinator, _health, _signals) = coordinator(owners, BTreeMap::new());
        coordinator
            .shutdown()
            .expect("coordinator should stop cleanly");
    }

    #[test]
    fn shutdown_reports_a_panicked_coordinator_thread() {
        let (commands, receiver) = mpsc::channel();
        drop(receiver);
        let coordinator = RegionEventCoordinator {
            handle: CoordinatorHandle {
                commands,
                health: Arc::new(RunnerHealth::default()),
            },
            join_handle: Some(thread::spawn(|| panic!("intentional test panic"))),
            stopped: false,
        };

        assert_eq!(
            coordinator.shutdown(),
            Err(CoordinatorShutdownError::ThreadPanicked)
        );
    }

    #[test]
    fn drain_waits_for_routes_emitted_during_worker_reports() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        register(&owners, 1, 7);
        register(&owners, 2, 8);
        let (commands, command_receiver) = mpsc::channel();
        let health = RunnerHealth::default();
        let (signals, _signal_receiver) = mpsc::channel();
        let (first_sender, first_receiver) = mpsc::channel();
        let (second_sender, second_receiver) = mpsc::channel();
        let (delivered, delivered_receiver) = mpsc::channel();

        let route_sender = commands.clone();
        let first_worker = thread::spawn(move || {
            let mut emitted = false;
            for command in first_receiver {
                if let ThreadedWorkerCommand::DrainReport { reply } = command {
                    if !emitted {
                        route_sender
                            .send(CoordinatorCommand::Route(vec![RoutedRegionEvent {
                                recipients: RegionRecipients::One(RegionId(2)),
                                event: event(),
                            }]))
                            .expect("test route should queue");
                        emitted = true;
                    }
                    let _ = reply.send(WorkerIdleReport::default());
                }
            }
        });
        let second_worker = thread::spawn(move || {
            for command in second_receiver {
                match command {
                    ThreadedWorkerCommand::Deliver {
                        target_region,
                        event: RegionEvent::EmploymentDirectoryReady,
                    } => {
                        delivered
                            .send(target_region)
                            .expect("test delivery receiver should stay alive");
                    }
                    ThreadedWorkerCommand::DrainReport { reply } => {
                        let _ = reply.send(WorkerIdleReport::default());
                    }
                    _ => panic!("unexpected test worker command"),
                }
            }
        });
        let workers = BTreeMap::from([(WorkerId(7), first_sender), (WorkerId(8), second_sender)]);

        drain_until_idle(&command_receiver, &owners, &workers, &health, &signals)
            .result
            .expect("drain should process the emitted route before returning");
        assert_eq!(
            delivered_receiver.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RegionId(2))
        );

        drop(workers);
        drop(commands);
        first_worker.join().expect("first worker should stop");
        second_worker.join().expect("second worker should stop");
    }

    #[test]
    fn drain_limit_latches_runner_health() {
        let owners = Arc::new(RegionOwnerDirectory::new());
        let (commands, command_receiver) = mpsc::channel();
        let health = RunnerHealth::default();
        let (signals, signal_receiver) = mpsc::channel();
        let (worker_sender, worker_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            for command in worker_receiver {
                if let ThreadedWorkerCommand::DrainReport { reply } = command {
                    let _ = reply.send(WorkerIdleReport {
                        pending_events: true,
                        dirty_hints: false,
                    });
                }
            }
        });
        let workers = BTreeMap::from([(WorkerId(7), worker_sender)]);

        let outcome = drain_until_idle(&command_receiver, &owners, &workers, &health, &signals);

        assert_eq!(outcome.result, Err(CoordinatorError::DrainLimitExceeded));
        assert_eq!(
            health.fault().as_deref(),
            Some(&CoordinatorFault::Routing(
                CoordinatorError::DrainLimitExceeded
            ))
        );
        assert!(matches!(
            signal_receiver.recv_timeout(RECEIVE_TIMEOUT),
            Ok(RunnerSignal::Faulted)
        ));

        drop(workers);
        drop(commands);
        worker.join().expect("test worker should stop");
    }
}
