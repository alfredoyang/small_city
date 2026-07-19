# Asynchronous region event coordinator plan

Status: **plan**.

## Goal

Replace deterministic cross-worker event barriers and direct runner-to-worker
routing with one asynchronous region-addressed event loop.

```text
today

worker A emits events
       |
       v
runner waits for every worker
       |
       v
sort all forwarded events
       |
       v
deliver to workers
       |
       v
runner drives another pass when a reply is required
```

```text
target

UI, clock, or worker emits event
       |
       v
coordinator queue
       |
       v
target worker mailbox wakes
       |
       v
target region handles event
```

The exact winner of contested cross-region work may vary with thread scheduling.
Correctness invariants may not vary.

```text
allowed
  either valid remote citizen may win one workplace seat
  either valid consumer may receive limited producer capacity first
  snapshots may be stale

required
  no workplace seat is double-booked
  no citizen holds two jobs
  no producer allocates more goods or power than it owns
  reachable capacity is always found: a consumer walks its whole candidate
    list before reporting a shortage, so which producer supplied it is
    unspecified but whether it was supplied is not
  duplicate and stale events are harmless
  each region mutates its own World on its owning worker only
  UI never reads World
```

## Non-goals

```text
  no exact replay of cross-region winner identity
  no global total order across events from different workers
  no migration of regions between workers
  no central ownership of regional ECS truth
  no balance-formula or gameplay-rule changes
  no replacement of local deterministic iteration
```

"No balance changes" means no formula, price, or rate is edited. It does not
mean run-to-run identity is preserved: extending contended admission to power
and goods (P4) lets the *supplying* region vary with worker sharding, so two
runs can book the same trade to different regions. This is an accepted
widening of the CLAUDE.md determinism exception, which previously covered job
admission only. P4 must land together with that CLAUDE.md amendment.

## Authority

```text
fact                              authority
----                              ---------
citizen assignment               citizen home region
workplace contracts/capacity      employer region
employment claim coordination     employment broker
goods stock/export reservation    producer region
power capacity/export reservation producer region
traveler body/token               current host region
road topology snapshot            region directory
event routing                     event coordinator
```

The coordinator is transport, not simulation authority.

```text
coordinator may                   coordinator must not
---------------                   --------------------
resolve RegionId -> WorkerId      inspect World
expand a broadcast                accept a job claim
enqueue worker commands           reserve goods or power
report missing targets            decide route reachability
coordinate pause/shutdown         mutate regional truth
```

## Architecture

```text
                          +--------------------------+
                          | RegionEventCoordinator   |
                          |                          |
                          | one MPSC receive loop    |
                          | immutable owner table    |
                          | worker command senders   |
                          | runner signal sender     |
                          +------------+-------------+
                                       |
             +-------------------------+-------------------------+
             |                         |                         |
             v                         v                         v
      +-------------+           +-------------+           +-------------+
      | worker 0    |           | worker 1    |           | worker 2    |
      | region 1, 2 |           | region 3    |           | region 4, 5 |
      +------+------+           +------+------+           +------+------+
             |                         |                         |
             +------------- outbound events -------------------+
                                       |
                                       v
                             coordinator MPSC sender
```

The runner also uses the coordinator:

```text
UI -> RegionalGame -> RegionalGameRunner
                           |
                           | Route([One(region), RunCommand])
                           v
                      coordinator
                           |
                           v
                     owning worker
                           |
                           | correlated RegionalReply
                           v
                      coordinator -> runner -> UI
```

All region-addressed events use this path, including UI commands and two
regions on the same worker.

```text
region 1 and region 2 share worker 0

region 1 -> coordinator -> worker 0 mailbox -> region 2
```

This keeps one routing rule and one set of tests.

## Routed Events

Use one envelope for direct delivery and broadcast expansion.

```rust
pub struct RoutedRegionEvent {
    pub recipients: RegionRecipients,
    pub event: RegionEvent,
}

pub enum RegionRecipients {
    One(RegionId),
    Many(Vec<RegionId>),
    All,
}
```

`Many` is normalized before delivery:

```rust
fn normalize_recipients(recipients: &mut Vec<RegionId>) {
    recipients.sort_unstable();
    recipients.dedup();
}
```

Road-aware scopes are resolved before routing. The coordinator does not learn
what a connected component means.

```text
bad
  BroadcastScope::ReachableByGoodsRoad
  coordinator reads topology and chooses targets

good
  directory snapshot computes [region 2, region 5]
  sender uses RegionRecipients::Many([2, 5])
```

Most domain work remains direct:

```text
UI command                -> selected region
snapshot request          -> selected region
inspect/panel query        -> selected region
Tick                      -> one event per selected region
StepTravel                -> all regions
employment accepted       -> citizen home region
employment release        -> employer region
destination arrived       -> traveler home region
goods reservation request -> selected producer region
power reservation request -> selected producer region
```

Broadcast is mainly for small invalidation/version notices:

```text
connectivity fingerprint changed -> affected regions
published availability changed   -> interested regions
shutdown/pause                    -> all workers, using control commands
```

Do not broadcast snapshots. Broadcast a version/fingerprint, then let each
region clone the latest immutable snapshot.

## Coordinator Loop

The coordinator owns one thread and one receive queue.

```rust
enum CoordinatorCommand {
    WorkerExited {
        worker_id: WorkerId,
        expected: bool,
    },
    Route(Vec<RoutedRegionEvent>),
    Reply(RegionalReply),
    RuntimeFault {
        worker_id: WorkerId,
        error: RegionRuntimeError,
    },
    WorkerRoundLimitExceeded {
        worker_id: WorkerId,
    },
    DrainUntilIdle { reply: Sender<Result<(), CoordinatorError>> },
    Shutdown { reply: Sender<()> },
}

fn run_coordinator(
    receiver: Receiver<CoordinatorCommand>,
    owners: Arc<RegionOwnerDirectory>,
    workers: BTreeMap<WorkerId, Sender<ThreadedWorkerCommand>>,
    runner_signals: Sender<RunnerSignal>,
    health: Arc<RunnerHealth>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            CoordinatorCommand::WorkerExited { worker_id, expected } => {
                if !expected {
                    fail_runner(
                        &health,
                        &runner_signals,
                        CoordinatorFault::WorkerStopped(worker_id),
                    );
                }
            }
            CoordinatorCommand::Route(events) => {
                if let Err(error) = route_events(events, &owners, &workers) {
                    fail_runner(
                        &health,
                        &runner_signals,
                        CoordinatorFault::Routing(error),
                    );
                }
            }
            CoordinatorCommand::Reply(reply) => {
                let _ = runner_signals.send(RunnerSignal::Reply(reply));
            }
            CoordinatorCommand::RuntimeFault { worker_id, error } => {
                fail_runner(
                    &health,
                    &runner_signals,
                    CoordinatorFault::Runtime { worker_id, error },
                );
            }
            CoordinatorCommand::WorkerRoundLimitExceeded { worker_id } => {
                fail_runner(
                    &health,
                    &runner_signals,
                    CoordinatorFault::WorkerRoundLimitExceeded(worker_id),
                );
            }
            CoordinatorCommand::DrainUntilIdle { reply } => {
                let result = drain_until_idle(
                    &receiver,
                    &owners,
                    &workers,
                    &health,
                    &runner_signals,
                );
                if let Err(error) = &result {
                    fail_runner(
                        &health,
                        &runner_signals,
                        CoordinatorFault::Drain(error.clone()),
                    );
                }
                let _ = reply.send(result);
            }
            CoordinatorCommand::Shutdown { reply } => {
                let _ = reply.send(());
                break;
            }
        }
    }
}
```

Direct, broadcast, and multi-event routing use one implementation:

```rust
fn route_events(events: Vec<RoutedRegionEvent>, ...) -> Result<(), CoordinatorError> {
    // Resolve the whole command before delivering its first event.
    let mut deliveries = Vec::new();
    for event in events {
        deliveries.extend(resolve_event_deliveries(event, owners, workers)?);
    }

    for delivery in deliveries {
        delivery.send()?;
    }
    Ok(())
}
```

`CoordinatorHandle::route` reports only whether the command entered the
coordinator queue. Recipient prevalidation prevents configuration errors from
partially expanding a broadcast. A worker can still stop between validation
and send. Any routing, worker, or runtime failure therefore latches the runner
as failed. No later tick, UI command, query, drain, or save may continue from a
possibly partially advanced city. Only shutdown or an explicit recovery path
is allowed. A worker never waits for routing acknowledgement.

Replies are forwarded unchanged through the same signal channel; the
coordinator does not decode command results or inspect view payloads.
`Faulted` carries no duplicate fault payload: it only wakes a blocked runner,
which reads the first fault from shared `RunnerHealth`. Health stores an
`Arc<CoordinatorFault>`, so `check()` clones the `Arc`, not the fault value.

```rust
pub enum CoordinatorFault {
    Routing(CoordinatorError),
    Drain(CoordinatorError),
    WorkerStopped(WorkerId),
    WorkerRoundLimitExceeded(WorkerId),
    Runtime {
        worker_id: WorkerId,
        error: RegionRuntimeError,
    },
    ReplyTimedOut {
        expected: Vec<ReplyKey>,
    },
    CoordinatorStopped,
}

pub enum RunnerSignal {
    Reply(RegionalReply),
    Faulted,
}

pub struct RunnerHealth {
    fault: Mutex<Option<Arc<CoordinatorFault>>>,
}

fn fail_runner(
    health: &RunnerHealth,
    signals: &Sender<RunnerSignal>,
    fault: CoordinatorFault,
) {
    if health.latch_first(fault) {
        let _ = signals.send(RunnerSignal::Faulted);
    }
}
```

`RegionEvent` can derive `Clone` now: its command, overlay, traveler, grant, and
allocation-request payloads already implement `Clone`. Broadcast expansion uses
that derive directly; no fallback copy path is needed.

## Worker Loop

Today a threaded worker runs regional events only when the runner requests a
processing pass. The target worker wakes and drains work when the coordinator
delivers an event.

```text
worker blocked on command_receiver.recv()
                 |
                 | Deliver { target, event }
                 v
push event into target RegionRuntime FIFO
                 |
                 v
process bounded regional slice
                 |
                 v
send routed outputs to coordinator
                 |
                 v
continue while local work remains; otherwise sleep
```

P2 defines the worker loop once, including bounded autonomous rounds, the
all-region hint sweep, control checks, and coordinator-failure behavior.

The worker still processes each owned region serially. No two threads mutate
one `RegionState`. UI commands are ordinary delivered `RegionEvent` values;
there is no direct `RunUiOperation` worker bypass.

## Runtime Output

Move routing decisions out of `RegionWorker` domain matches. A runtime output
should name its target directly.

The P2 hint-publish sweep is the one worker-created maintenance output: after
the directory helper returns explicit regions, the worker emits
`PowerCapacityRecheck` with `Many(regions)`. It does not match or reroute a
runtime domain message.

```rust
pub enum RuntimeOutput {
    Reply(RegionalReply),
    Route(RoutedRegionEvent),
    Error(RegionRuntimeError),
}
```

```text
RuntimeOutput::Reply
  -> worker sends CoordinatorCommand::Reply
  -> coordinator forwards RunnerSignal::Reply to runner
  -> runner matches request_id

RuntimeOutput::Error
  -> worker sends CoordinatorCommand::RuntimeFault
  -> coordinator latches runner failure
  -> worker stops processing simulation events
```

```text
today
  OutboundMessage::GoodsExportRequested(request)
  worker recognizes GoodsExportRequested
  worker computes where it goes

target
  RuntimeOutput::Route {
      recipients: One(producer_region),
      event: ProcessGoodsExportRequest(request),
  }
  coordinator routes without understanding goods
```

Candidate selection belongs to the requesting region, using a published
snapshot. A stale candidate may reject the request; the requesting region then
tries another candidate.

```text
consumer reads snapshot
  -> sends request to producer A
  -> producer A validates current truth
       accepted -> reply accepted
       rejected -> consumer tries producer B later
```

## Correctness Without Exact Ordering

The coordinator processes events in receive order. MPSC receive order between
different sending workers depends on scheduling and is intentionally not part
of game behavior.

```text
worker A sends claim A ----+
                           +-> coordinator -> employment broker
worker B sends claim B ----+

claim A or claim B may arrive first
```

Correctness comes from the authority's serial mutation:

```rust
fn reserve_one_seat(employer: &mut RegionState, claim: JobClaim) -> Decision {
    if employer.contract_count(claim.workplace) < employer.physical_seats(claim.workplace) {
        employer.insert_contract(claim);
        Decision::Accepted
    } else {
        Decision::Rejected
    }
}
```

Every authoritative mutation needs a domain identity already meaningful to its
owner:

```text
employment  citizen + claim generation
goods       caller region + request id + token
power       caller region + request id + token
travel      traveler entity + trip generation
```

Rules:

```text
duplicate active request  -> return the same result or no-op
duplicate release         -> no-op
older generation          -> reject or ignore
unknown release           -> no-op
stale snapshot request    -> validate against current owner truth
```

Do not add one universal transaction framework. Keep these checks in each
small domain protocol.

## Time And Replies

Normal region-to-region delivery does not wait for all workers.

```text
region event -> coordinator.send(...) -> caller continues
```

UI operations may still wait for their own correlated reply:

```text
RegionalGame::run_command(region, command)
  -> runner allocates request_id
  -> coordinator Route([One(region), RunCommand(request_id, command)])
  -> target region processes its FIFO
  -> worker sends Reply(CommandCompleted { request_id, region, reply })
  -> coordinator forwards reply to runner
  -> waiting RegionalGame call returns
```

Waiting for one requested reply is not a cross-region barrier. Other workers do
not need to finish a matching pass.

```rust
fn run_command(
    &self,
    region: RegionId,
    request_id: UiRequestId,
    command: RegionCommand,
) -> Result<RegionCommandReply, RegionalGameRunnerError> {
    self.coordinator.route(RoutedRegionEvent {
        recipients: RegionRecipients::One(region),
        event: RegionEvent::RunCommand { request_id, command },
    })?;

    self.wait_for_command_reply(request_id)
}
```

`BuildSnapshot`, `InspectRegion`, `RoadTravelerPanelSeed`, `BuildingAnchorAt`,
`RemoteWorkersFor`, and `SettlePowerImports` use the same transport. A tick is
one direct event per selected region because the public API supports one-region
and subset ticks and each region keeps its own request ID. `StepTravel` uses
`RegionRecipients::All`. The runner waits only for the correlated region
completions required by that public operation.

Events whose gameplay meaning depends on occurrence time carry that time in the
domain payload. The receiving time is not substituted for it.

```rust
DestinationArrived {
    traveler: TravelerId,
    destination: PlaceRef,
    arrived_at: GameTime,
}
```

City-clock advancement keeps its synchronous public API. The coordinator
removes event-routing barriers; it does not require ticks, save, snapshots, or
UI commands to become fire-and-forget.

## Pause, Save, And Shutdown

Quiescence is allowed to use an explicit global pause. It is not ordinary
simulation communication. Keep the existing save model: saving consumes the
runner, recovers regional state from stopped workers, writes it, then starts a
new runner.

```text
runner takes operation lock
  -> operation lock prevents new time/UI operations
  -> coordinator keeps accepting worker-emitted route events
  -> coordinator drains routed queue
  -> workers drain regional FIFOs and report idle
  -> repeat if draining emitted more routed events
  -> stop and recover workers
  -> stop coordinator
  -> serialize recovered regional truth
  -> start a new runner after the save
```

The single drain algorithm is specified in P5 under "Quiescence." It uses
bounded rounds and returns an error if a protocol produces an event cycle that
never becomes idle.

Shutdown order:

```text
stop new UI/time operations
  -> pause and drain or explicitly reject pending work
  -> stop workers
  -> join worker threads
  -> stop coordinator
  -> join coordinator thread
```

## Backpressure

Use the existing standard-library channels for the first implementation. Do
not add an async runtime.

```text
P1-P4
  unbounded internal control channels
  bounded work per worker slice
  counters for queued/routed/handled events in tests and diagnostics

later, only if measurements require it
  bounded channels or per-source quotas
```

A bounded coordinator-to-worker channel can deadlock if the coordinator blocks
while the full worker is trying to send a reply into the coordinator. Do not
introduce bounded channels without a nonblocking pending-delivery design.

## Patch Split

### P1: Coordinator transport, inactive

```text
Scope
  add RegionEventCoordinator + handle + thread lifecycle
  add RoutedRegionEvent and RegionRecipients
  direct, many, all-recipient, and multi-event expansion through one command
  missing-target and stopped-worker errors
  no live runtime output uses it yet

Allowed behavior
  none; test-only coordinator instances route synthetic events

Forbidden
  no RegionRuntime behavior changes
  no removal of barriers
  no power/goods/employment/travel migration
  no World access in coordinator

Tests
  direct event reaches the owning worker
  same-worker cross-region event still passes through coordinator
  Many sorts/deduplicates recipients
  All reaches every region exactly once
  one Route command enqueues every valid delivery before processing worker output
  one bad target prevents every delivery in that Route command
  missing region and stopped worker latch health and emit RunnerSignal::Faulted
  a later fault does not overwrite the first latched fault
  shutdown joins the coordinator thread
```

```rust
let prepared_workers = prepare_workers(region_workers);
let worker_senders = prepared_workers.command_senders();
let health = Arc::new(RunnerHealth::healthy());
let coordinator = RegionEventCoordinator::start(
    owners,
    worker_senders,
    runner_signals,
    health,
);
coordinator.route(RoutedRegionEvent {
    recipients: RegionRecipients::One(RegionId(2)),
    event,
})?;
```

Implementation outline:

```text
Files
  src/core/regions/coordinator.rs  new transport types and event loop
  src/core/regions/mod.rs          module registration and crate-private exports
  src/core/regions/threaded.rs     prepare command channels before thread start

Construction
  1. finish the RegionId -> WorkerId owner table
  2. prepare each worker command channel without starting its thread
  3. collect the complete WorkerId -> command sender table
  4. start coordinator with both complete tables
  5. start prepared workers only after coordinator is ready
  6. expose CoordinatorHandle to tests only; no runtime uses route yet
```

```rust
pub struct CoordinatorHandle {
    commands: Sender<CoordinatorCommand>,
    health: Arc<RunnerHealth>,
}

impl CoordinatorHandle {
    pub fn route(&self, event: RoutedRegionEvent) -> Result<(), CoordinatorSendError> {
        self.route_events(vec![event])
    }

    pub fn route_events(
        &self,
        events: Vec<RoutedRegionEvent>,
    ) -> Result<(), CoordinatorSendError> {
        self.commands
            .send(CoordinatorCommand::Route(events))
            .map_err(|_| {
                self.health
                    .latch_first(CoordinatorFault::CoordinatorStopped);
                CoordinatorSendError::Stopped
            })
    }

}
```

```rust
pub struct PreparedThreadedRegionWorker {
    worker: RegionWorker,
    command_sender: Sender<ThreadedWorkerCommand>,
    command_receiver: Receiver<ThreadedWorkerCommand>,
}

impl PreparedThreadedRegionWorker {
    pub fn prepare(worker: RegionWorker) -> Self {
        let (command_sender, command_receiver) = mpsc::channel();
        Self {
            worker,
            command_sender,
            command_receiver,
        }
    }

    pub fn start(self, coordinator: CoordinatorHandle) -> ThreadedRegionWorker {
        spawn_worker(
            self.worker,
            self.command_sender,
            self.command_receiver,
            coordinator,
        )
    }

    pub fn worker_id(&self) -> WorkerId {
        self.worker.id()
    }

    pub(crate) fn command_sender(&self) -> Sender<ThreadedWorkerCommand> {
        self.command_sender.clone()
    }
}
```

```rust
fn recipients(
    requested: RegionRecipients,
    owners: &RegionOwnerDirectory,
) -> Result<Vec<RegionId>, CoordinatorError> {
    let mut regions = match requested {
        RegionRecipients::One(region) => vec![region],
        RegionRecipients::Many(regions) => regions,
        RegionRecipients::All => owners.region_ids().collect(),
    };

    regions.sort_unstable();
    regions.dedup();

    for region in &regions {
        owners
            .worker_for(*region)
            .ok_or(CoordinatorError::MissingTargetRegion(*region))?;
    }
    Ok(regions)
}
```

Route errors are asynchronous:

```text
worker calls handle.route(event)
  -> Ok means queued at coordinator
  -> coordinator later finds missing target
  -> CoordinatorFault::Routing goes to runner signal receiver
  -> worker never waits for an acknowledgement
```

P1 test harness:

```rust
let (worker_0_tx, worker_0_rx) = mpsc::channel();
let health = Arc::new(RunnerHealth::healthy());
let coordinator = RegionEventCoordinator::start(
    owners,
    BTreeMap::from([(WorkerId(0), worker_0_tx)]),
    runner_signals,
    health,
);
coordinator.route(event_to_region_2)?;

let ThreadedWorkerCommand::Deliver { target, event } =
    worker_0_rx.recv_timeout(TEST_TIMEOUT)?
else {
    panic!("expected Deliver");
};
assert_eq!(target, RegionId(2));
assert!(matches!(event, RegionEvent::EmploymentDirectoryReady));
```

### P2: Autonomous worker wake, inactive adapter

```text
Scope
  add ThreadedWorkerCommand::Deliver without a reply channel
  delivered events wake and drive a bounded worker slice
  define one autonomous worker round to replace the current per-pass sweep
  install directory/runtime inputs once per active region per bounded round
  publish dirty hints from every owned region after each round
  retain PowerCapacityRecheck as a worker maintenance nudge with explicit targets
  bound consecutive autonomous rounds and observe shared runner health
  worker sends resulting RoutedRegionEvent values to coordinator
  worker exit/panic notifies coordinator
  add coordinator drain-until-idle for asynchronous integration tests
  retain current ProcessBarrier path for live callers

Allowed behavior
  test-only asynchronous worker/coordinator loop

Forbidden
  no live domain cutover
  no removal of current runner barriers
  no change to local region processing order

Tests
  sleeping worker wakes on Deliver
  target runtime processes its FIFO serially
  emitted reply reaches another worker without runner pumping
  bounded slices do not starve DrainReport or Shutdown
  one active region installs snapshots once for a multi-event bounded slice
  a road publish refreshes that runtime's exits and later installs see it
  an event-idle hints-dirty region publishes in the next worker round
  changed power hints emit one sorted/deduplicated Many-recipient recheck
  unchanged hints emit no PowerCapacityRecheck
  a same-worker event cycle reaches the round limit and fails the runner
  a worker exits its round loop after another thread latches runner health
  worker panic wakes a runner waiting for a reply with WorkerStopped
```

Implementation outline:

```text
Files
  src/core/regions/coordinator.rs health check and worker-round-limit report
  src/core/regions/threaded.rs  Deliver command and autonomous drive loop
  src/core/regions/worker.rs    autonomous round, input install, hint sweep
  src/core/regions/directory.rs stable power-recheck target helper
  src/core/regions/runtime/mod.rs
                                target-bearing RuntimeOutput seam, inactive

Command ownership
  Deliver owns one RegionEvent and does not carry a reply sender
  DrainReport/Shutdown remain worker-control commands
  UI commands still use the old live path until P5
```

```rust
enum ThreadedWorkerCommand {
    Deliver {
        target: RegionId,
        event: RegionEvent,
    },

    // Existing synchronous controls remain during migration.
    ProcessBarrier { ... },
    DrainReport { reply: Sender<WorkerIdleReport> },
    Shutdown { reply: Sender<RegionWorker> },
}
```

```rust
impl CoordinatorHandle {
    fn reply(&self, reply: RegionalReply) -> Result<(), CoordinatorSendError> {
        self.commands
            .send(CoordinatorCommand::Reply(reply))
            .map_err(|_| CoordinatorSendError::Stopped)
    }

    fn worker_exited(
        &self,
        worker_id: WorkerId,
        expected: bool,
    ) -> Result<(), CoordinatorSendError> {
        self.commands
            .send(CoordinatorCommand::WorkerExited {
                worker_id,
                expected,
            })
            .map_err(|_| CoordinatorSendError::Stopped)
    }

    fn health_failed(&self) -> bool {
        self.health.is_failed()
    }

    fn worker_round_limit_exceeded(
        &self,
        worker_id: WorkerId,
    ) -> Result<(), CoordinatorSendError> {
        self.commands
            .send(CoordinatorCommand::WorkerRoundLimitExceeded { worker_id })
            .map_err(|_| {
                self.health
                    .latch_first(CoordinatorFault::CoordinatorStopped);
                CoordinatorSendError::Stopped
            })
    }
}
```

One delivery wakes the worker; the worker then processes bounded slices until
its owned regional FIFOs are empty:

```rust
struct RuntimeSliceSummary {
    outputs: Vec<RuntimeOutput>,
    processed_events: usize,
}

struct WorkerRoundSummary {
    outputs: Vec<RuntimeOutput>,
    processed_events: usize,
    published_hints: usize,
}

fn process_autonomous_round(
    worker: &mut RegionWorker,
    max_events_per_region: usize,
) -> WorkerRoundSummary {
    let mut summary = WorkerRoundSummary::default();
    let directory = Arc::clone(&worker.directory);
    let employment_directory = Arc::clone(&worker.employment_directory);
    let owners = Arc::clone(&worker.owners);

    // One install per active region per bounded round, not per event.
    for runtime in worker.regions_with_pending_events() {
        let discovery = directory.discovery_snapshot();
        let border_links = runtime.state().network_border_links();
        runtime.set_importable_remote_jobs(
            importable_remote_jobs_for_region(
                &discovery,
                runtime.region_id(),
                &border_links,
            ),
        );
        runtime.set_discovery_generation(discovery.generation);
        runtime.set_discovery_snapshot(Arc::clone(&discovery));
        runtime.set_employment_directory(Arc::clone(
            &employment_directory,
        ));
        runtime.set_region_routes(
            &directory
                .exits_from(runtime.region_id())
                .unwrap_or_default(),
        );

        let slice = runtime.process_some_events(max_events_per_region);
        summary.outputs.extend(slice.outputs);
        summary.processed_events += slice.processed_events;
        refresh_derived_and_publish_road_report_if_dirty(
            &directory,
            &owners,
            runtime,
        );
    }

    // Preserve P-1: inspect every owned region once per round, including an
    // event-idle region left dirty by earlier work.
    let changed = collect_and_clear_dirty_hint_summaries(worker);
    for (region, links, hints) in changed {
        if !directory.publish_region(region, links, hints.clone()) {
            continue;
        }

        summary.published_hints += 1;
        let discovery = directory.discovery_snapshot();
        let targets = power_capacity_recheck_targets(
            &discovery,
            region,
            &hints,
        );
        if !targets.is_empty() {
            summary.outputs.push(RuntimeOutput::Route(
                RoutedRegionEvent {
                    recipients: RegionRecipients::Many(targets),
                    event: RegionEvent::PowerCapacityRecheck {
                        request_id: worker.next_worker_request_id(),
                        source_region: region,
                    },
                },
            ));
        }
    }

    summary
}

fn collect_and_clear_dirty_hint_summaries(
    worker: &mut RegionWorker,
) -> Vec<(RegionId, Vec<NetworkBorderLink>, Vec<AvailabilityHint>)> {
    let mut changed = Vec::new();
    for runtime in worker.regions_mut() {
        if !runtime.state().is_hints_dirty() {
            continue;
        }
        runtime.ensure_derived_state();
        changed.push((
            runtime.region_id(),
            runtime.state().network_border_links(),
            runtime.state().availability_hints(),
        ));
        runtime.state().clear_hints_dirty();
    }
    changed
}

fn power_capacity_recheck_targets(
    discovery: &CrossRegionDiscovery,
    source: RegionId,
    hints: &[AvailabilityHint],
) -> Vec<RegionId> {
    let mut targets = BTreeSet::new();
    for hint in hints {
        for network in discovery.component_of(hint.network).unwrap_or(&[]) {
            if network.region != source {
                targets.insert(network.region);
            }
        }
    }
    targets.into_iter().collect()
}
```

`power_capacity_recheck_targets` is a pure directory helper. It walks the
published components touched by the changed hints, removes the source region,
and returns sorted/deduplicated `RegionId` values. The worker retains the
existing maintenance trigger and request-ID ownership; the coordinator sees
only `Many(targets)` and never reads topology or power state.

`refresh_derived_and_publish_road_report_if_dirty` mirrors the current
post-slice sequence exactly: ensure derived state, publish a changed road
report, refresh that runtime's route exits from the rebuilt directory snapshot,
then clear the road-topology dirty flag.

```text
one autonomous worker round
  active region A: install once -> process up to N events
  active region B: install once -> process up to N events
  all owned regions: check hints_dirty once
  changed publish: directory computes recheck targets
  emit compact routed outputs
  check worker controls
```

```rust
#[derive(Default)]
struct DrainOutcome {
    deferred_reports: Vec<Sender<WorkerIdleReport>>,
    shutdown: Option<Sender<ThreadedWorkerShutdownResult>>,
    failed: bool,
}

const MAX_AUTONOMOUS_ROUNDS: usize = MAX_DRAIN_ROUNDS;

fn drive_worker(
    worker: &mut RegionWorker,
    first: ThreadedWorkerCommand,
    coordinator: &CoordinatorHandle,
    commands: &Receiver<ThreadedWorkerCommand>,
) -> DrainOutcome {
    let mut outcome = DrainOutcome::default();
    if !accept_worker_command(worker, first, &mut outcome) {
        return outcome;
    }

    for _ in 0..MAX_AUTONOMOUS_ROUNDS {
        if coordinator.health_failed() {
            outcome.failed = true;
            return outcome;
        }

        let summary = worker.process_autonomous_round(MAX_EVENTS_PER_SLICE);

        for output in summary.outputs {
            let sent = match output {
                RuntimeOutput::Route(event) => coordinator.route(event),
                RuntimeOutput::Reply(reply) => coordinator.reply(reply),
                RuntimeOutput::Error(error) => {
                    coordinator.runtime_fault(worker.id(), error)
                }
            };
            if sent.is_err() {
                // CoordinatorHandle already latched CoordinatorStopped.
                outcome.failed = true;
                return outcome;
            }
        }

        // Control is checked between bounded slices.
        while let Ok(command) = commands.try_recv() {
            if !accept_worker_command(worker, command, &mut outcome) {
                return outcome;
            }
        }

        if !worker.has_pending_events() && !worker.has_dirty_hints() {
            return outcome;
        }
    }

    let _ = coordinator.worker_round_limit_exceeded(worker.id());
    outcome.failed = true;
    outcome
}

fn accept_worker_command(
    worker: &mut RegionWorker,
    command: ThreadedWorkerCommand,
    outcome: &mut DrainOutcome,
) -> bool {
    match command {
        ThreadedWorkerCommand::Deliver { target, event } => {
            if worker.push_event(target, event).is_err() {
                outcome.failed = true;
                return false;
            }
        }
        ThreadedWorkerCommand::DrainReport { reply } => {
            outcome.deferred_reports.push(reply);
        }
        ThreadedWorkerCommand::Shutdown { reply } => {
            outcome.shutdown = Some(reply);
        }
        legacy => handle_legacy_migration_command(worker, legacy),
    }
    true
}
```

The outer worker loop sends `deferred_reports` after the round reaches idle,
performs `shutdown` if present, and exits immediately when `failed` is true.
Every first command, including a maintenance-only `DrainReport`, runs at least
one autonomous round, so an event-idle dirty region cannot miss the hint sweep.
`ensure_derived_state` does not set `hints_dirty`; only simulation mutations do,
so the dirty-hint idle check cannot self-trigger an empty spin. The round cap
handles actual event cycles, while the health check stops a worker after a
timeout or fault was latched elsewhere.
When the coordinator disappears mid-slice, the shared health is already
latched and the worker exits; it never continues mutating an unreachable city.

Worker exit is always observable, including panic unwinding:

```rust
struct WorkerExitGuard {
    worker_id: WorkerId,
    coordinator: CoordinatorHandle,
    expected: bool,
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let _ = self.coordinator.worker_exited(
            self.worker_id,
            self.expected,
        );
    }
}

fn run_worker(...) {
    let mut exit = WorkerExitGuard::new(worker.id(), coordinator.clone());
    let reason = run_worker_commands(...);
    exit.expected = matches!(reason, WorkerExitReason::ExplicitShutdown);
}
```

Test-only quiescence sends `CoordinatorCommand::DrainUntilIdle` and waits with
`CONTROL_REPLY_TIMEOUT`. The command's only algorithm is the P5 Quiescence
implementation; P2 does not add a second drain loop.

P5 makes this runner-owned save/test control part of the live architecture. P2
adds it only so P2-P4 asynchronous tests have a sleep-free completion point.

```text
Deliver arrives
  -> push target FIFO
  -> process at most MAX_EVENTS_PER_SLICE per region
  -> route outputs without waiting
  -> check control channel
  -> repeat or sleep
```

P2 does not switch a live domain. Its integration test injects an existing
harmless test event through `Deliver` and observes processing through a reply or
test probe. Existing runner barrier tests must remain unchanged and green.

### P3: Travel and employment notifications

```text
Scope
  route TravelerHandedOff through coordinator
  route DestinationArrived through coordinator when that event exists
  route EmploymentDirectoryReady directly through coordinator
  tag each handoff with the next eligible TravelStepId
  stage inbound handoffs until that StepTravel
  make current step_travel_city wait for coordinator idle before returning
  same-worker and cross-worker targets use one path

Allowed behavior
  these fire-and-forget protocols no longer wait for a deterministic barrier
  exact arrival order between different workers may vary

Forbidden
  no employment authority change
  no workplace capacity or salary change
  no token moving twice in one travel subtick
  no recursive rollback across multiple borders in one travel subtick
  no goods/power migration

Tests
  cross-worker traveler handoff wakes destination worker
  same-worker handoff uses coordinator
  rollback/rebound crosses at most one border per StepTravel
  a multi-region rollback is not fully consumed by one coordinator drain
  a handoff produced in step N is first applied in step N+1
  duplicate StepTravel(step_id) does not move a token twice
  eligible handoffs apply in traveler-id order regardless of arrival order
  employment capacity and citizen uniqueness invariants hold under contention
  contested test does not assert winner identity
  stale trip generation remains ignored
```

Implementation outline:

```text
Files
  src/core/regions/runtime/mod.rs  target-bearing travel/employment outputs
                                       and step-tagged inbound handoffs
  src/core/regions/worker.rs       delete live travel/employment route matches
  src/core/regions/threaded.rs     send those runtime outputs to coordinator
  src/core/regional_game_runner.rs retain current StepTravel broadcast; add drain fence

Travel truth does not move
  source host removes/exports token using existing logic
  destination host accepts token using existing generation guard
  coordinator only carries ReceiveTraveler

Employment truth does not move
  EmploymentDirectory keeps claim coordination
  employer RegionState keeps contracts
  home RegionState keeps citizen assignment
  coordinator replaces only EmploymentDirectoryReady routing
```

Traveler handoff:

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TravelStepId(u64);

fn route_traveler_handoff(
    produced_in: TravelStepId,
    handoff: TravelerHandoff,
) -> RoutedRegionEvent {
    RoutedRegionEvent {
        recipients: RegionRecipients::One(handoff.to_region),
        event: RegionEvent::ReceiveTraveler {
            eligible_step: produced_in.next(),
            handoff,
        },
    }
}
```

`ReceiveTraveler` only stages transport. `StepTravel` applies handoffs whose
eligibility has arrived before it moves tokens. Therefore a drain can deliver
an arbitrarily long event chain, but it cannot apply another border handoff in
the same travel step.

```rust
pub struct RegionRuntime {
    // Transient like current travel tokens/away bookkeeping.
    pending_traveler_handoffs: Vec<(TravelStepId, TravelerHandoff)>,
    last_travel_step: Option<TravelStepId>,
    // existing fields...
}

fn stage_traveler_handoff(
    runtime: &mut RegionRuntime,
    eligible_step: TravelStepId,
    handoff: TravelerHandoff,
) {
    runtime
        .pending_traveler_handoffs
        .push((eligible_step, handoff));
}

fn take_handoffs_eligible_at(
    runtime: &mut RegionRuntime,
    step: TravelStepId,
) -> Vec<TravelerHandoff> {
    let pending = std::mem::take(&mut runtime.pending_traveler_handoffs);
    let (ready, future): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .partition(|(eligible, _)| *eligible <= step);
    runtime.pending_traveler_handoffs = future;

    let mut handoffs = ready
        .into_iter()
        .map(|(_, handoff)| handoff)
        .collect::<Vec<_>>();
    handoffs.sort_by_key(|handoff| {
        (
            handoff.traveler.citizen,
            handoff.traveler.generation,
            handoff_kind_rank(handoff.kind),
        )
    });
    handoffs
}

fn step_travel(
    runtime: &mut RegionRuntime,
    step: TravelStepId,
) -> Vec<RuntimeOutput> {
    if runtime.last_travel_step.is_some_and(|last| step <= last) {
        return vec![RuntimeOutput::Reply(
            RegionalReply::TravelStepCompleted {
                request_id: step,
                region: runtime.region_id(),
            },
        )];
    }
    runtime.last_travel_step = Some(step);

    let inbound = runtime.take_handoffs_eligible_at(step);
    let mut outputs = Vec::new();

    // Current behavior: apply last step's crossings before this step's move.
    for handoff in inbound {
        outputs.extend(runtime.apply_handoff(handoff, step));
    }

    outputs.extend(runtime.step_existing_tokens_once(step));
    outputs.push(RuntimeOutput::Reply(
        RegionalReply::TravelStepCompleted {
            request_id: step,
            region: runtime.region_id(),
        },
    ));
    outputs
}

fn route_step_output(
    source: RegionId,
    step: TravelStepId,
    output: TravelOutput,
) -> RuntimeOutput {
    match output {
        TravelOutput::Handoff(handoff) => RuntimeOutput::Route(
            route_traveler_handoff(step, handoff),
        ),
        other => other.into_runtime_output(source),
    }
}
```

The pending vector follows the current travel save rule: tokens and
away-traveler bookkeeping are transient and reconstructed after load. The
drain fence normally leaves only next-step entries; `eligible_step` remains the
authoritative guard for stale, duplicate, or unexpectedly early delivery. P3
does not introduce a separate durable travel protocol.

```text
StepTravel at host A
  -> token reaches border
  -> A removes local token and emits TravelerHandoff
  -> coordinator queues ReceiveTraveler(eligible = N+1) at B
  -> B wakes and stages it; no traveler state is applied
  -> step N drain becomes idle
  -> StepTravel N+1 applies it before moving B's tokens once

StepTravel N+1 at B rejects or rolls back
  -> rebound is routed with eligible = N+2
  -> A stages it without applying it
  -> only StepTravel N+2 may apply the rebound
```

During P3, the runner still starts movement with its old broadcast/barrier. It
must wait for the new asynchronous handoff path before returning:

```rust
fn step_travel_city(&self) -> Result<(), RegionalGameRunnerError> {
    let _operation = self.operation_lock.lock()?;

    run_existing_step_travel_broadcast_and_worker_pass()?;
    self.coordinator.drain_until_idle()?;
    Ok(())
}
```

This fence stages direct handoffs before returning. The eligibility rule
preserves the current one-border-per-subtick contract even though the
coordinator drains all immediately routable descendants:

```text
step N returns only after destination staged the handoff
step N+1 applies it before moving tokens
```

Destination arrival, when the arrival-pay plan adds the event:

```rust
fn route_destination_arrived(
    arrived: DestinationArrived,
) -> RoutedRegionEvent {
    RoutedRegionEvent {
        recipients: RegionRecipients::One(arrived.traveler.citizen.region()),
        event: RegionEvent::DestinationArrived(arrived),
    }
}
```

Employment wake:

```rust
fn employment_wakes(
    targets: impl IntoIterator<Item = RegionId>,
) -> impl Iterator<Item = RoutedRegionEvent> {
    targets.into_iter().map(move |target| RoutedRegionEvent {
        recipients: RegionRecipients::One(target),
        event: RegionEvent::EmploymentDirectoryReady,
    })
}
```

```text
home submits claim to EmploymentDirectory
  -> route wake to employer
  -> employer validates and records contract
  -> route wake to home
  -> home applies matching accepted lease
```

The worker no longer matches these domain variants to discover a target:

```rust
// Remove after both variants emit RoutedRegionEvent directly.
match outbound {
    OutboundMessage::TravelerHandedOff(..) => ..,
    OutboundMessage::EmploymentDirectoryReady { .. } => ..,
}
```

P3 tests wait through the test-only coordinator `drain_until_idle`; they do not
sleep or call the deterministic barrier to make progress.

### P4: Goods and power request/reply

```text
Scope
  requesting region chooses candidate from directory snapshot
  route request, grant/rejection, and release directly through coordinator
  producer remains sole allocation authority
  remove worker's resource-specific routing decisions after both resources move

Allowed behavior
  two consumers racing for limited capacity may produce either valid winner
  the supplying producer is unspecified when several could serve the demand
  stale snapshot candidates may reject and retry later
  another caller's late release may delay satisfaction beyond the current tick

Forbidden
  no producer over-allocation
  no allocation mutation outside producer region
  no goods/power balance-formula changes
  no shortage reported while an untried reachable candidate remains
  no shared generic transaction framework

Tests
  goods contention preserves capacity for either winner
  power contention preserves capacity for either winner
  reachable capacity is eventually found after stale reservations release
  a consumer rejected by candidate A is satisfied by candidate B, not denied
  duplicate request/release is idempotent
  zero-candidate power and goods requests apply their denied results immediately
  stale candidate rejects and consumer retries
  stale granted reply emits a targeted release and does not leak capacity
  a late release does not permanently deny another consumer
  disconnect releases allocations through normal domain events
```

Implementation outline:

```text
Files
  src/core/regions/runtime/mod.rs  caller-owned candidate continuation
  src/core/regions/worker.rs       remove ExportResource routing implementation
  src/core/regions/directory.rs    unchanged snapshot authority
  src/core/regions/mod.rs          unchanged producer allocation truth

Keep
  PowerExportRequest / GoodsExportRequest
  ExportAllocationRequest candidate list and index
  producer-owned ExportAllocations
  existing grant math and balance

Move
  candidate retry decision: worker -> requesting RegionRuntime
  ApplyPowerExportGrant / ApplyGoodsExportGrant echo full allocation attempt
  request/result/release transport: worker matches -> RoutedRegionEvent
```

The consumer chooses from one immutable discovery snapshot:

```rust
fn begin_power_request(
    consumer: &mut RegionRuntime,
    request: PowerExportRequest,
    discovery: &CrossRegionDiscovery,
) -> Vec<RoutedRegionEvent> {
    let candidates = discovery.power_candidates(request.caller_network);
    let attempt = PowerExportAllocationRequest {
        request,
        candidates,
        candidate_index: 0,
    };

    if let Some(routed) = route_power_attempt(attempt.clone()) {
        return vec![routed];
    }

    // Preserve the current no-candidate behavior. A silent return would leave
    // the consumer waiting without applying its missing-resource state.
    let denied = PowerExportGrant::denied(attempt.request.token);
    consumer
        .apply_power_export_grant(attempt.request, denied)
        .into_iter()
        .map(|output| output.into_routed(consumer.region_id()))
        .collect()
}
```

```rust
fn route_power_attempt(
    attempt: PowerExportAllocationRequest,
) -> Option<RoutedRegionEvent> {
    let producer_network = *attempt.candidates.get(attempt.candidate_index)?;

    Some(RoutedRegionEvent {
        recipients: RegionRecipients::One(producer_network.region),
        event: RegionEvent::ProcessPowerExportRequest(attempt),
    })
}
```

The producer validates current owned state and replies directly:

```rust
fn process_power_export_request(
    producer: &mut RegionRuntime,
    attempt: PowerExportAllocationRequest,
) -> RoutedRegionEvent {
    let grant = producer.allocate_power_from_current_state(&attempt);
    let caller = attempt.request.caller_region;

    RoutedRegionEvent {
        recipients: RegionRecipients::One(caller),
        event: RegionEvent::ApplyPowerExportGrant {
            request: attempt,
            grant,
        },
    }
}
```

The caller owns rejection continuation:

```rust
fn apply_power_result(
    caller: &mut RegionRuntime,
    mut attempt: PowerExportAllocationRequest,
    grant: PowerExportGrant,
) -> Vec<RoutedRegionEvent> {
    let stale = attempt.request.request_id != caller.current_power_request_id();

    if grant.granted || stale {
        // Existing apply logic records a current grant or releases a stale
        // granted reservation. A stale denial is a no-op.
        return caller
            .apply_power_export_grant(attempt.request, grant)
            .into_iter()
            .map(|output| output.into_routed(caller.region_id()))
            .collect();
    }

    attempt.candidate_index += 1;
    if let Some(next) = route_power_attempt(attempt.clone()) {
        return vec![next];
    }

    caller
        .apply_power_export_grant(attempt.request, grant)
        .into_iter()
        .map(|output| output.into_routed(caller.region_id()))
        .collect()
}
```

If the snapshot contains no candidate, the caller immediately applies an
explicit denied grant. If every candidate rejects, the caller applies the final
denial in the same way. Goods uses the equivalent denied goods grant. A stale
attempt does not retry. A stale granted reply still emits the existing targeted
release, so moving the continuation out of the worker cannot leak producer
capacity. A rejection never mutates producer allocation state.

Producer handling already calls
`release_stale_for_caller(caller_region, request_id)` before measuring capacity.
Therefore a request arriving before its own caller's release is safe without
worker-side output ordering. A different caller's stale reservation can still
make the producer temporarily reject a valid request: its units remain included
by `reserved_units_excluding` until that caller's release arrives. Tests must
assert eventual retry/satisfaction, not single-tick satisfaction under
cross-caller contention.

Release uses explicit known producer regions:

```rust
fn route_power_releases(
    release: PowerExportAllocationRelease,
) -> Vec<RoutedRegionEvent> {
    let mut producers = release.producer_regions.clone();
    producers.sort_unstable();
    producers.dedup();

    producers
        .into_iter()
        .map(|producer| RoutedRegionEvent {
            recipients: RegionRecipients::One(producer),
            event: RegionEvent::ReleasePowerExportAllocations(release.clone()),
        })
        .collect()
}
```

Goods follows the same message sequence with its existing goods request, grant,
and allocation types. Shared candidate-index helpers are allowed; do not add a
generic coordinator transaction or move allocation truth into the coordinator.

```text
consumer runtime       coordinator       producer runtime
       |                    |                    |
       | ProcessRequest --->|------------------->|
       |                    |             validate current truth
       |<-------------------|<--- ApplyGrant ----|
       | apply or retry     |                    |
```

### P5: Cutover and barrier deletion

```text
Scope
  runner starts and owns coordinator lifecycle
  route every current region-addressed command/query through coordinator:
    RunCommand, BuildSnapshot, InspectRegion, RoadTravelerPanelSeed,
    BuildingAnchorAt, RemoteWorkersFor, and SettlePowerImports
  route one Tick per selected region and broadcast StepTravel through coordinator
  refresh Inspect's job/goods view inputs inside its runtime event handler
  forward correlated runtime replies through RunnerSignal
  remove direct runner-to-worker event delivery
  UI/time replies are correlated without global worker passes
  latch routing, worker, runtime, and reply-timeout faults in shared runner health
  add pause-and-drain for save/load and tests
  remove ForwardedEventOrderKey and deterministic cross-worker barrier routing
  remove obsolete forwarding adapters and barrier-only tests

Allowed behavior
  cross-worker event interleaving and contested winner identity may vary

Forbidden
  no change to local deterministic simulation
  no loss of save/load consistency
  no continuation after a partial broadcast or runtime failure
  no UI access to worker/runtime/World
  no background thread left unjoined

Tests
  targeted UI command reaches its region and returns the matching reply
  snapshot and inspect requests return through coordinator without World exposure
  inspect preserves current importable-job and cross-region-goods refresh behavior
  targeted, subset, and city Tick preserve each (request_id, region_id) pair
  duplicate Tick replies cannot substitute for a missing region reply
  duplicate Tick request keys are rejected before Route delivery
  StepTravel reaches every region exactly once
  remote-worker roster still fans out and returns the same stable aggregate
  SettlePowerImports has no direct worker-control path
  full multi-region scripts satisfy occupancy/resource invariants on 1-N workers
  tests assert eventual completion without sleeps
  save pauses, drains, restores, and resumes
  worker failure surfaces through runner error
  missing reply times out, latches failure, and cannot grow a pending-reply map
  partial Tick/StepTravel delivery failure poisons later operations and save
  clean shutdown with pending direct and broadcast events
  no source reference to retired barrier routing remains
```

Implementation outline:

```text
Files
  src/core/regional_game_runner.rs coordinator lifecycle, route/wait helpers
  src/core/regional_game.rs        public API unchanged
  src/core/regions/threaded.rs     remove live ProcessBarrier/forward delivery
  src/core/regions/worker.rs       remove deterministic merge and route adapters
  src/core/regions/runtime/mod.rs  correlated replies for every routed request
  src/core/regions/directory.rs    immutable snapshots only; no authority move
  tests/*                          invariant-based asynchronous assertions
```

Startup order avoids a coordinator/worker constructor cycle:

```rust
fn start_runner(
    regions: Vec<RegionState>,
    assignments: WorkerAssignments,
) -> Result<Self, RegionalGameRunnerError> {
    let (runner_signal_tx, runner_signal_rx) = mpsc::channel();
    let health = Arc::new(RunnerHealth::healthy());
    let (region_workers, owners) =
        build_region_workers_and_owners(regions, assignments)?;
    let prepared = region_workers
        .into_iter()
        .map(PreparedThreadedRegionWorker::prepare)
        .collect::<Vec<_>>();
    let worker_senders = prepared
        .iter()
        .map(|worker| (worker.worker_id(), worker.command_sender()))
        .collect();

    let (coordinator, coordinator_join) =
        RegionEventCoordinator::start(
            owners.clone(),
            worker_senders,
            runner_signal_tx,
            health.clone(),
        );

    let workers = prepared
        .into_iter()
        .map(|worker| worker.start(coordinator.clone()))
        .collect();

    let runner = Self {
        coordinator,
        coordinator_join,
        runner_signals: Mutex::new(runner_signal_rx),
        health,
        workers,
        operation_lock: Mutex::new(()),
        ..
    };

    // Preserve the current startup drain for RegionRuntime initialization
    // events before exposing the runner to callers.
    runner.coordinator.drain_until_idle()?;
    runner.health.check()?;
    Ok(runner)
}
```

The cutover must cover the complete current region-addressed surface, not only
the common UI commands:

`RegionalGame` mints its existing monotonic `UiRequestId` for inspect, panel,
and remote-roster calls that currently need no asynchronous correlation. Their
public signatures remain unchanged. One remote-roster request ID is reused for
its sequential anchor and fan-out stages; `ReplyKey` also includes reply kind
and region, so the stages cannot collide.

```rust
enum RegionEvent {
    RunCommand { request_id: UiRequestId, command: RegionCommand },
    BuildSnapshot { request_id: UiRequestId, overlay: MapOverlayInput },
    InspectRegion { request_id: UiRequestId, x: usize, y: usize },
    RoadTravelerPanelSeed { request_id: UiRequestId, x: usize, y: usize },
    BuildingAnchorAt { request_id: UiRequestId, x: usize, y: usize },
    RemoteWorkersFor {
        request_id: UiRequestId,
        producer_region: RegionId,
        pos: Position,
    },
    SettlePowerImports { request_id: UiRequestId },
    Tick { request_id: UiRequestId },
    StepTravel { request_id: TravelStepId },
    // existing domain events...
}

enum RegionalReply {
    CommandCompleted { request_id: UiRequestId, region: RegionId, reply: RegionCommandReply },
    SnapshotBuilt { request_id: UiRequestId, region: RegionId, view: RegionView },
    InspectCompleted { request_id: UiRequestId, region: RegionId, view: InspectView },
    RoadTravelerPanelSeedCompleted { request_id: UiRequestId, region: RegionId, seed: PanelSeed },
    BuildingAnchorFound { request_id: UiRequestId, region: RegionId, anchor: Option<Position> },
    RemoteWorkersFound {
        request_id: UiRequestId,
        region: RegionId,
        workers: Vec<CitizenDetailView>,
    },
    PowerImportsSettled { request_id: UiRequestId, region: RegionId },
    TickCompleted { request_id: UiRequestId, region: RegionId, response: RegionTickResponse },
    TravelStepCompleted { request_id: TravelStepId, region: RegionId },
}
```

The worker keeps installing the immutable discovery snapshot before runtime
processing, as it does today. The `InspectRegion` event refreshes the two view
inputs that the old direct worker helper refreshed:

```rust
fn process_inspect(
    runtime: &mut RegionRuntime,
    request_id: UiRequestId,
    x: usize,
    y: usize,
) -> RegionalReply {
    let discovery = runtime.discovery_snapshot();
    let region = runtime.region_id();
    let border_links = runtime.state().network_border_links();
    let jobs = importable_remote_jobs_for_region(
        &discovery,
        region,
        &border_links,
    );
    let goods = cross_region_goods_routes_for_region(
        &discovery,
        region,
        &border_links,
    );

    runtime.set_importable_remote_jobs(jobs);
    runtime.set_cross_region_goods_routes(goods);
    RegionalReply::InspectCompleted {
        request_id,
        region,
        view: runtime.inspect(x, y),
    }
}
```

The calculation remains inside the owning region thread. The coordinator sees
only the event and opaque reply; it never reads private `World` storage.

UI command path:

Every public runner entry takes `operation_lock`, checks health before routing,
and checks it again after collecting its final reply or drain result.

```rust
fn run_command(
    &self,
    region: RegionId,
    request_id: UiRequestId,
    command: RegionCommand,
) -> Result<RegionCommandReply, RegionalGameRunnerError> {
    let _operation = self.operation_lock.lock()?;
    self.health.check()?;

    self.coordinator.route(RoutedRegionEvent {
        recipients: RegionRecipients::One(region),
        event: RegionEvent::RunCommand { request_id, command },
    })?;

    let key = ReplyKey::command(request_id, region);
    let mut replies = self.wait_for_expected(BTreeSet::from([key]))?;
    let reply = replies
        .remove(&key)
        .ok_or(RegionalGameRunnerError::MissingReply(key))?;
    match reply {
        RegionalReply::CommandCompleted { reply, .. } => Ok(reply),
        other => Err(RegionalGameRunnerError::UnexpectedReply(other.kind())),
    }
}
```

The operation lock allows only one public UI/time operation to own expected
replies. Therefore an unmatched reply cannot belong to a future operation; it
is stale and is discarded. Do not retain an unbounded `pending_replies` map.
The runner waits on one signal stream with a production watchdog, so a worker
failure or missing reply cannot block forever:

```rust
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

fn wait_for_expected(
    &self,
    expected: BTreeSet<ReplyKey>,
) -> Result<BTreeMap<ReplyKey, RegionalReply>, RegionalGameRunnerError> {
    self.health.check()?;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut remaining = expected;
    let mut replies = BTreeMap::new();

    while !remaining.is_empty() {
        let wait = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| self.fail_reply_timeout(&remaining))?;

        match self.runner_signals.lock()?.recv_timeout(wait) {
            Ok(RunnerSignal::Reply(reply)) => {
                let key = reply.key(); // kind + request_id + region_id
                if remaining.remove(&key) {
                    replies.insert(key, reply);
                }
                // Duplicate, wrong-kind, and late replies are stale. Drop them.
            }
            Ok(RunnerSignal::Faulted) => {
                self.health.check()?;
                return Err(RegionalGameRunnerError::FaultSignalWithoutFault);
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err(self.fail_reply_timeout(&remaining).into());
            }
            Err(RecvTimeoutError::Disconnected) => return Err(self.fail_signal_disconnect()),
        }
    }

    self.health.check()?;
    Ok(replies)
}
```

`fail_reply_timeout` atomically latches
`CoordinatorFault::ReplyTimedOut { expected }` only if health is still clean;
otherwise it returns the earlier fault. `REPLY_TIMEOUT` is only a failure
watchdog. Simulation scheduling and tests never use sleeps. Tests call the same
collector with a short injected deadline or create an immediate worker failure.

Coordinator drain replies, worker idle reports, and shutdown acknowledgements
use the same bounded watchdog rule. A timeout latches runner failure; no
production control path performs an unbounded `recv()`.

Ticks preserve the existing request correlation. `tick_region` supplies one
pair, `tick_regions` supplies a subset, and `tick_city` allocates one request ID
for each region. They all use one helper:

```rust
fn tick_regions(
    &self,
    requests: &[(UiRequestId, RegionId)],
) -> Result<Vec<RegionTickResponse>, Error> {
    let _operation = self.operation_lock.lock()?;
    self.health.check()?;
    self.validate_owned_regions(requests.iter().map(|(_, region)| *region))?;
    let expected = requests
        .iter()
        .map(|&(request_id, region)| ReplyKey::tick(request_id, region))
        .collect::<BTreeSet<_>>();
    if expected.len() != requests.len() {
        return Err(Error::DuplicateTickRequest);
    }

    let events = requests
        .iter()
        .map(|&(request_id, region)| RoutedRegionEvent {
            recipients: RegionRecipients::One(region),
            event: RegionEvent::Tick { request_id },
        })
        .collect();
    self.coordinator.route_events(events)?;

    let mut replies = self.wait_for_expected(expected)?;

    // Preserve the caller's requested order, independent of worker arrival.
    requests
        .iter()
        .map(|&(request_id, region)| {
            take_tick_response(&mut replies, ReplyKey::tick(request_id, region))
        })
        .collect()
}
```

One `Route(Vec<_>)` command is transport-only. The coordinator resolves the
whole vector, then enqueues every Tick delivery before reading worker-generated
commands. This preserves the current "all selected clocks start, then
descendants route" phase without giving the coordinator any tick policy.

Remote-worker inspection remains a two-stage query without direct worker
controls:

```text
BuildingAnchorAt(request A) -> producer region
  -> BuildingAnchorFound(A, anchor Position)

for each region in stable RegionId order
  RemoteWorkersFor(request N, producer, anchor Position) -> that region
  -> RemoteWorkersFound(N, region, workers)

runner concatenates replies in RegionId order
runner stable-sorts workers by (home_region, citizen)
```

`SettlePowerImports` is routed to each selected region and collected through
`PowerImportsSettled`; it is not a worker control command.

Travel keeps one broadcast because every region participates in a city travel
step. Completion is keyed by `(TravelStepId, RegionId)`, so two replies from one
region cannot substitute for a missing region. The drain stages handoffs and
applies arrivals, while P3's eligibility rule prevents a second border
crossing.

The coordinator expands the whole `All` event before reading another command,
and it is the sole sender of `Deliver`. Therefore every worker's StepTravel
delivery is ahead of every TravelerHandoff produced by that step. The
`eligible_step` check is still authoritative; FIFO ordering is not used as the
only cadence guard.

```rust
fn step_travel_city(&self, request_id: TravelStepId) -> Result<(), Error> {
    let _operation = self.operation_lock.lock()?;
    self.health.check()?;

    self.coordinator.route(RoutedRegionEvent {
        recipients: RegionRecipients::All,
        event: RegionEvent::StepTravel { request_id },
    })?;

    let expected = self
        .region_ids()
        .map(|region| ReplyKey::travel(request_id, region))
        .collect();
    self.wait_for_expected(expected)?;
    self.coordinator.drain_until_idle()?;
    self.health.check()?;
    Ok(())
}
```

Quiescence is a control protocol, not deterministic event sorting. Its
ordering contract is explicit:

```text
1. coordinator is the only sender of worker Deliver commands
2. runner operation_lock prevents a new external Route during drain
3. coordinator routes every command currently ahead of DrainUntilIdle
4. coordinator then sends DrainReport on each worker's same FIFO control channel
5. each DrainReport is therefore behind every earlier Deliver to that worker
6. worker drains its regional FIFOs and sends all Route/Reply/RuntimeFault outputs
7. worker sends WorkerIdleReport only after those outputs were enqueued
8. coordinator consumes those outputs and repeats if any work was emitted
```

While a drain is active, the coordinator handles worker-originated `Route`,
`Reply`, `RuntimeFault`, and `WorkerExited` commands. A nested runner control
command is rejected or deferred until the current operation releases the
operation lock. The algorithm does not depend on an unreliable channel
`is_empty` observation.

```rust
fn drain_until_idle(...) -> Result<(), CoordinatorError> {
    for _ in 0..MAX_DRAIN_ROUNDS {
        health.check()?;
        let before = handle_current_worker_commands_with_try_recv()?;
        health.check()?;

        let reports = workers
            .values()
            .map(request_worker_drain_report)
            .collect::<Result<Vec<_>, _>>()?;

        // Worker outputs are sent before that worker's idle report. Process
        // anything they placed in the coordinator queue.
        let after = handle_current_worker_commands_with_try_recv()?;
        health.check()?;

        let workers_idle = reports.iter().all(|report| {
            report.pending_events == 0 && report.emitted_events == 0
        });

        if before.routed == 0
            && after.routed == 0
            && workers_idle
        {
            return Ok(());
        }
    }

    Err(CoordinatorError::DrainLimitExceeded)
}
```

Required cascade tests:

```text
A -> B -> A route chain reaches idle without sleeping
output queued after the first worker report forces another round
worker exit during drain latches failure and aborts the drain
self-sustaining event cycle reaches MAX_DRAIN_ROUNDS and fails
same-worker event cycle reaches MAX_AUTONOMOUS_ROUNDS and fails
```

Save/load:

```rust
fn shutdown_for_save(self) -> Result<RecoveredRegionalGame, Error> {
    {
        let _operation = self.operation_lock.lock()?;
        // A failed runner may be shut down, but its partially advanced state
        // must not be serialized as a valid save.
        self.health.check()?;
        self.coordinator.drain_until_idle()?;
        self.health.check()?;
    }

    let recovered_workers = shutdown_and_join_workers(self.workers)?;
    self.coordinator.shutdown_and_join()?;

    Ok(RecoveredRegionalGame::new(recovered_workers))
}
```

`RegionalGame::save_to_file` continues to serialize the recovered
`RegionState` values and start a replacement runner. P5 changes only the
pre-shutdown drain and coordinator lifecycle; it does not invent a second save
snapshot protocol.

If health is failed, `save_to_file` returns the latched fault. A separate
cleanup path still joins workers and the coordinator without serializing state.

Shutdown keeps the coordinator alive while workers finish, because final worker
events and faults still need a receiver:

```text
take operation lock
  -> drain until idle
  -> send worker shutdown controls
  -> join every worker
  -> send coordinator shutdown
  -> join coordinator
```

Deletion checklist:

```text
remove
  ForwardedEventOrderKey
  ForwardedRegionEvent
  process_workers_with_deterministic_barrier
  Process, ProcessBarrier, and DeliverForwarded live commands
  runner-owned RegionHandle list and direct RegionHandle::send calls
  runner collect/sort/deliver loops
  worker request/grant/release routing matches migrated in P3/P4
  direct worker controls for Inspect, RoadTravelerPanelSeed, BuildingAnchorAt,
    RemoteWorkersAt, and SettlePowerImports
  exact cross-worker ordering tests

keep
  RegionRuntime FIFO
  local deterministic maps/sorts
  RegionOwnerDirectory
  employment and resource authority checks
  request ids, trip generations, allocation tokens
  P2 autonomous worker round and per-active-region input install
  all-region hints_dirty publish sweep
  worker-triggered PowerCapacityRecheck with directory-computed Many targets

ThreadedWorkerCommand after cutover
  Deliver
  DrainReport
  Shutdown
```

## Test Style

Do not use wall-clock sleeps to wait for asynchronous behavior.

```rust
game.drain_region_events_until_idle()?;

assert_eq!(contracts_at(workplace), 1);
assert!(citizen_a_has_job ^ citizen_b_has_job);
```

```text
assert
  final invariants
  idempotency
  eventual delivery
  authority ownership

do not assert
  which worker sent first
  which valid citizen won
  exact cross-worker processing sequence
```

Local deterministic tests continue asserting exact local outcomes.

## Review Checks

```text
  coordinator never imports or reads World
  coordinator routing contains no goods/power/job policy
  all region-addressed events, including UI and same-worker targets, use one transport
  no direct ThreadedWorkerCommand remains for simulation or UI queries
  every producer validates current owned state after reading stale snapshots
  broadcasts normalize recipients and deliver once per region
  worker installs directory inputs once per active region per bounded round
  every autonomous round sweeps all owned regions for dirty hints
  worker round loop checks health and has a hard round limit
  PowerCapacityRecheck targets come from the directory helper, not coordinator policy
  Tick preserves one (request_id, region_id) pair per selected region
  StepTravel cannot recursively consume a rollback across multiple borders
  workers process owned RegionState values serially
  no worker blocks waiting for coordinator acknowledgement
  routing/runtime/worker/timeout failures latch runner health
  unmatched or duplicate replies are discarded, not retained without bound
  save/load has an explicit quiescence protocol
  shutdown joins every worker and coordinator thread
  contention tests assert invariants, not winner identity
```

## Risks

```text
risk                         control
----                         -------
event cycle never idles      worker-round/drain limits + protocol tests
event storm starves commands bounded worker slices + control checks
coordinator bottleneck       route compact events; snapshot data stays shared Arc
duplicate delivery           domain operation identities and idempotency
late time-sensitive event    carry occurrence time in payload
save races live events       operation lock + pause-and-drain
partial broadcast failure    prevalidate; latch failure; reject continuation/save
missing correlated reply     recv_timeout watchdog + sticky failure
hidden broadcast coupling    broadcast only small invalidation notices
```

## Completion State

```text
RegionalGameRunner
  owns coordinator lifecycle, request ids, and synchronous UI waits

RegionEventCoordinator
  is the only region-addressed ingress
  asynchronously routes direct and broadcast events
  forwards opaque correlated replies to the runner

ThreadedRegionWorker
  wakes and processes delivered events without runner pumping

RegionRuntime / RegionState
  retain all simulation authority

RegionDirectory / EmploymentDirectory
  publish snapshots and coordinate their existing domains

cross-region determinism
  winner identity unspecified
  correctness invariants enforced
```
