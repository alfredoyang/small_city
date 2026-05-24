//! Deterministic single-threaded region actor runtime prototype.
//!
//! This module is intentionally disconnected from the real city `World`. It proves the
//! tick/phase/message ordering rules that a future multithreaded region model can reuse.
//!
//! The model has three layers:
//! - `RegionMessage` is input work addressed to one actor for one `(SimTick, SimPhase)`.
//! - `RegionEvent` is staged output created while processing messages.
//! - `ActorState` is committed only when the phase closes.
//!
//! Keeping messages, events, and committed state separate is the main safety rule. Worker
//! scheduling can decide when code runs, but only the coordinator decides which phase is open.
//! Future messages stay queued, late messages are rejected, and state changes happen through
//! sorted local events so delivery timing does not change simulation results.

use std::collections::BTreeMap;

use crate::core::actor_executor::{
    ActorExecutor, PhaseWork, SingleThreadActorExecutor, ThreadedActorExecutor,
};
use crate::core::region::{RegionBounds, RegionPartition};
use crate::core::region_promise::{
    PromiseChain, PromiseGroup, PromiseId, PromiseResolved, PromiseResponse,
};
use crate::core::resources::{LocalEffects, LocalEffectsMap};
use crate::core::systems::local_effects::{
    LocalEffectsRegionWork, derive_region_local_effect_cells,
};

const MAX_SAME_PHASE_DRAIN_PASSES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimTick(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimPhase(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageSequence(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionMessage {
    /// Simulation tick this message belongs to. Actors only process matching ticks.
    pub tick: SimTick,
    /// Ordered phase inside the tick. A completed phase rejects late messages.
    pub phase: SimPhase,
    /// Region that produced the message. Used for deterministic ordering and replies.
    pub source: RegionId,
    /// Region that owns this message. Direct delivery rejects wrong-target messages.
    pub target: RegionId,
    /// Runtime-assigned sequence number used as a final stable sort key.
    pub sequence: MessageSequence,
    pub kind: RegionMessageKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionMessageKind {
    /// Prototype/test message that stages a counter delta event.
    AddCounter(i32),
    /// Prototype cross-region request used to prove deterministic promise resolution.
    FakeBorderMetricRequest { promise_id: PromiseId },
    /// Read-only derived metric sample used by the border-pollution actor prototype.
    ReadOnlyBorderPollutionSample { value: i32 },
    /// One already-computed local-effects cell result owned by the target region actor.
    LocalEffectsCellSample(LocalEffectsCell),
    /// One region-level local-effects job computed on the actor worker.
    LocalEffectsRegionWork(LocalEffectsRegionWork),
    #[cfg(test)]
    /// Test-only message that intentionally creates a same-phase cycle.
    CyclicSamePhaseRequest,
    /// Response for an unordered dependency group. Resolves after all dependencies reply.
    PromiseGroupResponse(PromiseResponse),
    /// Response for an ordered dependency chain. Resolves in declared dependency order.
    PromiseChainResponse(PromiseResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionEvent {
    pub tick: SimTick,
    pub phase: SimPhase,
    pub source: RegionId,
    pub sequence: MessageSequence,
    pub kind: RegionEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionEventKind {
    /// Commits a counter delta after phase-local message processing has finished.
    CommitCounterDelta(i32),
    /// Commits one border pollution sample into the phase's read-only metric total.
    CommitBorderPollutionSample(i32),
    /// Commits one local-effects cell into the actor's read-only cell list.
    CommitLocalEffectsCell(LocalEffectsCell),
    /// Commits a resolved promise result after its dependency responses are complete.
    PromiseResolved(PromiseResolved),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDelivery {
    Accepted,
    RejectedStale,
    UnknownTarget,
    WrongTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseRun {
    Completed,
    RejectedStale,
    RejectedMessageLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorPhaseRun {
    pub status: PhaseRun,
    pub outgoing: Vec<RegionMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActorState {
    pub counter: i32,
    pub read_only: ReadOnlyDerivedMetrics,
    pub committed_events: Vec<RegionEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadOnlyDerivedMetrics {
    pub border_pollution: i32,
    pub(crate) local_effect_cells: Vec<LocalEffectsCell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalEffectsCell {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) effects: LocalEffects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionActor {
    pub id: RegionId,
    pub current_tick: SimTick,
    pub current_phase: SimPhase,
    pub inbox: Vec<RegionMessage>,
    pub local_events: Vec<RegionEvent>,
    pub promise_groups: BTreeMap<PromiseId, PromiseGroup>,
    pub promise_chains: BTreeMap<PromiseId, PromiseChain>,
    pub state: ActorState,
}

impl RegionActor {
    pub fn new(id: RegionId) -> Self {
        Self {
            id,
            current_tick: SimTick(0),
            current_phase: SimPhase(0),
            inbox: Vec::new(),
            local_events: Vec::new(),
            promise_groups: BTreeMap::new(),
            promise_chains: BTreeMap::new(),
            state: ActorState::default(),
        }
    }

    pub fn deliver(&mut self, message: RegionMessage) -> MessageDelivery {
        // Actor ownership is strict: a message can only enter the inbox of its target region.
        if message.target != self.id {
            return MessageDelivery::WrongTarget;
        }
        // The cursor represents the last completed phase. Same-phase late delivery is stale.
        if (message.tick, message.phase) <= self.current_cursor() {
            return MessageDelivery::RejectedStale;
        }
        self.inbox.push(message);
        MessageDelivery::Accepted
    }

    pub fn advance_to_tick(&mut self, tick: SimTick) -> PhaseRun {
        if tick < self.current_tick
            || (tick == self.current_tick && self.current_phase > SimPhase(0))
        {
            return PhaseRun::RejectedStale;
        }
        if tick > self.current_tick {
            self.current_tick = tick;
        }
        self.current_phase = SimPhase(0);
        PhaseRun::Completed
    }

    pub fn register_promise_group(&mut self, group: PromiseGroup) {
        self.promise_groups.insert(group.id(), group);
    }

    pub fn register_promise_chain(&mut self, chain: PromiseChain) {
        self.promise_chains.insert(chain.id(), chain);
    }

    pub(crate) fn process_inbox_to_local_events(
        &mut self,
        tick: SimTick,
        phase: SimPhase,
    ) -> Vec<RegionMessage> {
        let mut outgoing = Vec::new();
        // Process messages in a stable order so worker scheduling cannot affect outcomes.
        self.inbox.sort_by_key(region_message_order);
        let messages = std::mem::take(&mut self.inbox);
        for message in messages {
            // Future messages stay queued until the coordinator opens their exact phase.
            if message.tick != tick || message.phase != phase {
                self.inbox.push(message);
                continue;
            }
            match message.kind {
                RegionMessageKind::AddCounter(delta) => {
                    self.local_events.push(RegionEvent {
                        tick: message.tick,
                        phase: message.phase,
                        source: message.source,
                        sequence: message.sequence,
                        kind: RegionEventKind::CommitCounterDelta(delta),
                    });
                }
                RegionMessageKind::FakeBorderMetricRequest { promise_id } => {
                    outgoing.push(self.fake_border_metric_response(message, promise_id));
                }
                RegionMessageKind::ReadOnlyBorderPollutionSample { value } => {
                    self.local_events.push(RegionEvent {
                        tick: message.tick,
                        phase: message.phase,
                        source: message.source,
                        sequence: message.sequence,
                        kind: RegionEventKind::CommitBorderPollutionSample(value),
                    });
                }
                RegionMessageKind::LocalEffectsCellSample(cell) => {
                    self.local_events.push(RegionEvent {
                        tick: message.tick,
                        phase: message.phase,
                        source: message.source,
                        sequence: message.sequence,
                        kind: RegionEventKind::CommitLocalEffectsCell(cell),
                    });
                }
                RegionMessageKind::LocalEffectsRegionWork(work) => {
                    for cell in derive_region_local_effect_cells(&work.snapshot, work.bounds) {
                        self.local_events.push(RegionEvent {
                            tick: message.tick,
                            phase: message.phase,
                            source: message.source,
                            sequence: message.sequence,
                            kind: RegionEventKind::CommitLocalEffectsCell(cell),
                        });
                    }
                }
                #[cfg(test)]
                RegionMessageKind::CyclicSamePhaseRequest => {
                    outgoing.push(RegionMessage {
                        target: message.source,
                        source: self.id,
                        ..message
                    });
                }
                RegionMessageKind::PromiseGroupResponse(response) => {
                    if let Some(group) = self.promise_groups.get_mut(&response.promise_id) {
                        if let Some(resolved) = group.record_response(response) {
                            self.enqueue_promise_resolved(message, resolved);
                        }
                    }
                }
                RegionMessageKind::PromiseChainResponse(response) => {
                    if let Some(chain) = self.promise_chains.get_mut(&response.promise_id) {
                        if let Some(resolved) = chain.record_response(response) {
                            self.enqueue_promise_resolved(message, resolved);
                        }
                    }
                }
            }
        }
        outgoing
    }

    pub(crate) fn commit_local_events(&mut self, tick: SimTick, phase: SimPhase) {
        // Events are the mutation boundary. They are sorted before commit for determinism.
        self.local_events.sort_by_key(region_event_order);
        let events = std::mem::take(&mut self.local_events);
        let mut border_pollution = 0;
        let mut local_effect_cells = Vec::new();
        for event in events {
            if event.tick != tick || event.phase != phase {
                self.local_events.push(event);
                continue;
            }
            match event.kind {
                RegionEventKind::CommitCounterDelta(delta) => {
                    self.state.counter += delta;
                    self.state.committed_events.push(event);
                }
                RegionEventKind::CommitBorderPollutionSample(value) => {
                    border_pollution += value;
                    self.state.committed_events.push(event);
                }
                RegionEventKind::CommitLocalEffectsCell(cell) => {
                    local_effect_cells.push(cell);
                    self.state.committed_events.push(event);
                }
                RegionEventKind::PromiseResolved(ref resolved) => {
                    self.state.counter += resolved.total;
                    self.state.committed_events.push(event);
                }
            }
        }
        // Read-only metrics are fresh phase outputs, so missing samples clear old values.
        self.state.read_only.border_pollution = border_pollution;
        local_effect_cells.sort_by_key(|cell| (cell.y, cell.x));
        self.state.read_only.local_effect_cells = local_effect_cells;
    }

    pub fn run_phase(&mut self, tick: SimTick, phase: SimPhase) -> ActorPhaseRun {
        if (tick, phase) <= self.current_cursor() {
            return ActorPhaseRun {
                status: PhaseRun::RejectedStale,
                outgoing: Vec::new(),
            };
        }
        // A direct actor run returns generated outgoing messages to the caller. The runtime
        // path drains those messages across actors before committing the shared phase.
        let outgoing = self.process_inbox_to_local_events(tick, phase);
        self.commit_local_events(tick, phase);
        self.advance_to_phase(tick, phase);
        ActorPhaseRun {
            status: PhaseRun::Completed,
            outgoing,
        }
    }

    pub(crate) fn current_cursor(&self) -> (SimTick, SimPhase) {
        (self.current_tick, self.current_phase)
    }

    pub(crate) fn advance_to_phase(&mut self, tick: SimTick, phase: SimPhase) {
        self.current_tick = tick;
        self.current_phase = phase;
    }

    fn enqueue_promise_resolved(&mut self, message: RegionMessage, resolved: PromiseResolved) {
        self.local_events.push(RegionEvent {
            tick: message.tick,
            phase: message.phase,
            source: message.source,
            sequence: message.sequence,
            kind: RegionEventKind::PromiseResolved(resolved),
        });
    }

    fn fake_border_metric_response(
        &self,
        request: RegionMessage,
        promise_id: PromiseId,
    ) -> RegionMessage {
        RegionMessage {
            tick: request.tick,
            phase: request.phase,
            source: self.id,
            target: request.source,
            sequence: request.sequence,
            kind: RegionMessageKind::PromiseGroupResponse(PromiseResponse {
                promise_id,
                tick: request.tick,
                phase: request.phase,
                dependency: self.id,
                value: self.fake_border_metric(),
            }),
        }
    }

    fn fake_border_metric(&self) -> i32 {
        self.id.0 as i32 + 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorRuntime {
    actors: BTreeMap<RegionId, RegionActor>,
    next_sequence: MessageSequence,
    executor_mode: ActorRuntimeExecutorMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorRuntimeExecutorMode {
    SingleThread,
    Threaded,
}

impl ActorRuntime {
    pub fn new(region_ids: impl IntoIterator<Item = RegionId>) -> Self {
        let actors = region_ids
            .into_iter()
            .map(|id| (id, RegionActor::new(id)))
            .collect();
        Self {
            actors,
            next_sequence: MessageSequence(0),
            executor_mode: ActorRuntimeExecutorMode::SingleThread,
        }
    }

    pub fn new_threaded(region_ids: impl IntoIterator<Item = RegionId>) -> Self {
        let mut runtime = Self::new(region_ids);
        runtime.executor_mode = ActorRuntimeExecutorMode::Threaded;
        runtime
    }

    pub fn next_sequence(&mut self) -> MessageSequence {
        let sequence = self.next_sequence;
        self.next_sequence.0 += 1;
        sequence
    }

    pub fn send(
        &mut self,
        tick: SimTick,
        phase: SimPhase,
        source: RegionId,
        target: RegionId,
        kind: RegionMessageKind,
    ) -> MessageDelivery {
        let sequence = self.next_sequence();
        self.deliver(RegionMessage {
            tick,
            phase,
            source,
            target,
            sequence,
            kind,
        })
    }

    pub fn deliver(&mut self, message: RegionMessage) -> MessageDelivery {
        let Some(actor) = self.actors.get_mut(&message.target) else {
            return MessageDelivery::UnknownTarget;
        };
        actor.deliver(message)
    }

    pub fn run_phase(&mut self, tick: SimTick, phase: SimPhase) -> BTreeMap<RegionId, PhaseRun> {
        match self.executor_mode {
            ActorRuntimeExecutorMode::SingleThread => {
                let mut executor = SingleThreadActorExecutor;
                self.run_phase_with_executor(tick, phase, &mut executor)
            }
            ActorRuntimeExecutorMode::Threaded => {
                let mut executor = ThreadedActorExecutor;
                self.run_phase_with_executor(tick, phase, &mut executor)
            }
        }
    }

    pub(crate) fn run_phase_with_executor(
        &mut self,
        tick: SimTick,
        phase: SimPhase,
        executor: &mut impl ActorExecutor,
    ) -> BTreeMap<RegionId, PhaseRun> {
        let mut results = BTreeMap::new();
        let runnable_actors: Vec<_> = self
            .actors
            .iter()
            .filter_map(|(id, actor)| ((tick, phase) > actor.current_cursor()).then_some(*id))
            .collect();
        let actors_before_run = self.actors.clone();

        for _ in 0..MAX_SAME_PHASE_DRAIN_PASSES {
            // Each pass runs the still-open phase for the original runnable actor set. Outgoing
            // messages are delivered back into actor inboxes and drained in the next pass.
            let work = PhaseWork {
                tick,
                phase,
                actors: runnable_actors
                    .iter()
                    .filter_map(|actor_id| self.actors.get(actor_id).cloned())
                    .collect(),
            };
            let phase_result = executor.run_phase(work);
            for actor in phase_result.actors {
                self.actors.insert(actor.id, actor);
            }
            let outgoing = phase_result.outgoing;
            if outgoing.is_empty() {
                // No more same-phase messages exist, so the runtime can close the phase and
                // commit every runnable actor at the same coordinator-controlled boundary.
                for actor in self.actors.values_mut() {
                    if runnable_actors.contains(&actor.id) {
                        actor.commit_local_events(tick, phase);
                        actor.advance_to_phase(tick, phase);
                        results.insert(actor.id, PhaseRun::Completed);
                    } else {
                        results.insert(actor.id, PhaseRun::RejectedStale);
                    }
                }
                return results;
            }
            for message in outgoing {
                self.deliver(message);
            }
        }

        // A same-phase message cycle would otherwise spin forever. Roll back to the snapshot
        // from before the phase and report a deterministic rejection.
        self.actors = actors_before_run;
        for actor in self.actors.values() {
            if runnable_actors.contains(&actor.id) {
                results.insert(actor.id, PhaseRun::RejectedMessageLimit);
            } else {
                results.insert(actor.id, PhaseRun::RejectedStale);
            }
        }
        results
    }

    pub fn start_fake_neighbor_metric_query(
        &mut self,
        partition: &RegionPartition,
        requester: RegionId,
        tick: SimTick,
        phase: SimPhase,
        promise_id: PromiseId,
    ) -> Vec<(RegionId, MessageDelivery)> {
        let neighbors = partition.neighbors(requester);
        if let Some(actor) = self.actors.get_mut(&requester) {
            actor.register_promise_group(PromiseGroup::new(
                promise_id,
                tick,
                phase,
                neighbors.clone(),
            ));
        }

        neighbors
            .into_iter()
            .map(|neighbor| {
                let delivery = self.send(
                    tick,
                    phase,
                    requester,
                    neighbor,
                    RegionMessageKind::FakeBorderMetricRequest { promise_id },
                );
                (neighbor, delivery)
            })
            .collect()
    }

    pub(crate) fn enqueue_border_pollution_samples(
        &mut self,
        partition: &RegionPartition,
        effects: &LocalEffectsMap,
        tick: SimTick,
        phase: SimPhase,
    ) -> BTreeMap<RegionId, Vec<MessageDelivery>> {
        let mut deliveries = BTreeMap::new();
        for region in partition.region_ids() {
            let samples = border_pollution_samples(partition, effects, region);
            let region_deliveries = samples
                .into_iter()
                .map(|value| {
                    self.send(
                        tick,
                        phase,
                        region,
                        region,
                        RegionMessageKind::ReadOnlyBorderPollutionSample { value },
                    )
                })
                .collect();
            deliveries.insert(region, region_deliveries);
        }
        deliveries
    }

    pub fn advance_actor_to_tick(&mut self, actor: RegionId, tick: SimTick) -> Option<PhaseRun> {
        self.actors
            .get_mut(&actor)
            .map(|actor| actor.advance_to_tick(tick))
    }

    pub fn actor(&self, id: RegionId) -> Option<&RegionActor> {
        self.actors.get(&id)
    }
}

pub fn region_message_order(
    message: &RegionMessage,
) -> (SimTick, SimPhase, RegionId, MessageSequence) {
    (
        message.tick,
        message.phase,
        message.source,
        message.sequence,
    )
}

pub fn region_event_order(event: &RegionEvent) -> (SimTick, SimPhase, RegionId, MessageSequence) {
    (event.tick, event.phase, event.source, event.sequence)
}

#[cfg(test)]
pub(crate) fn border_pollution_summary(
    partition: &RegionPartition,
    effects: &LocalEffectsMap,
    region: RegionId,
) -> i32 {
    border_pollution_samples(partition, effects, region)
        .into_iter()
        .sum()
}

fn border_pollution_samples(
    partition: &RegionPartition,
    effects: &LocalEffectsMap,
    region: RegionId,
) -> Vec<i32> {
    let Some(bounds) = partition.bounds(region) else {
        return Vec::new();
    };

    border_cells(bounds)
        .into_iter()
        .map(|(x, y)| effects.get(x, y).pollution_pressure)
        .collect()
}

fn border_cells(bounds: RegionBounds) -> Vec<(usize, usize)> {
    let mut cells = Vec::new();
    for y in bounds.min_y..bounds.max_y {
        for x in bounds.min_x..bounds.max_x {
            if x == bounds.min_x
                || x + 1 == bounds.max_x
                || y == bounds.min_y
                || y + 1 == bounds.max_y
            {
                cells.push((x, y));
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::actor_executor::ThreadedActorExecutor;
    use crate::core::components::{Building, BuildingData, Position};
    use crate::core::region::RegionPartition;
    use crate::core::region_promise::{PromiseChain, PromiseGroup, PromiseId, PromiseResponse};
    use crate::core::systems::local_effects;
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn messages_sort_by_tick_phase_source_sequence() {
        let mut messages = vec![
            message(1, 2, 2, 9, 10),
            message(1, 1, 3, 4, 20),
            message(0, 3, 1, 7, 30),
            message(1, 1, 1, 5, 40),
            message(1, 1, 1, 2, 50),
        ];

        messages.sort_by_key(region_message_order);

        assert_eq!(
            messages
                .iter()
                .map(|message| (
                    message.tick.0,
                    message.phase.0,
                    message.source.0,
                    message.sequence.0
                ))
                .collect::<Vec<_>>(),
            vec![
                (0, 3, 1, 7),
                (1, 1, 1, 2),
                (1, 1, 1, 5),
                (1, 1, 3, 4),
                (1, 2, 2, 9)
            ]
        );
    }

    #[test]
    fn shuffled_delivery_produces_same_actor_state() {
        let ordered = vec![
            message(2, 1, 1, 1, 5),
            message(1, 2, 3, 2, 7),
            message(1, 1, 2, 3, 11),
            message(1, 1, 1, 4, 13),
        ];
        let shuffled = vec![
            ordered[2].clone(),
            ordered[0].clone(),
            ordered[3].clone(),
            ordered[1].clone(),
        ];

        let ordered_state = runtime_state_after_delivery(ordered);
        let shuffled_state = runtime_state_after_delivery(shuffled);

        assert_eq!(ordered_state, shuffled_state);
        assert_eq!(ordered_state.counter, 36);
    }

    #[test]
    fn actor_commits_state_only_through_local_events() {
        let mut actor = RegionActor::new(RegionId(10));
        let mut message = message(1, 1, 2, 1, 8);
        message.target = RegionId(10);
        assert_eq!(actor.deliver(message), MessageDelivery::Accepted);

        actor.process_inbox_to_local_events(SimTick(1), SimPhase(1));

        assert_eq!(actor.state.counter, 0);
        assert_eq!(actor.local_events.len(), 1);

        actor.commit_local_events(SimTick(1), SimPhase(1));

        assert_eq!(actor.state.counter, 8);
        assert_eq!(actor.local_events.len(), 0);
    }

    #[test]
    fn stale_tick_messages_are_rejected() {
        let mut runtime = ActorRuntime::new([RegionId(1)]);
        runtime.advance_actor_to_tick(RegionId(1), SimTick(3));

        let mut stale_message = message(2, 1, 1, 1, 10);
        stale_message.target = RegionId(1);
        let delivery = runtime.deliver(stale_message);
        runtime.run_phase(SimTick(2), SimPhase(1));

        assert_eq!(delivery, MessageDelivery::RejectedStale);
        assert_eq!(runtime.actor(RegionId(1)).unwrap().state.counter, 0);
    }

    #[test]
    fn future_messages_wait_for_matching_tick_and_phase() {
        let mut runtime = ActorRuntime::new([RegionId(1)]);
        let mut future = message(3, 1, 1, 1, 30);
        future.target = RegionId(1);
        let mut current = message(1, 1, 1, 2, 10);
        current.target = RegionId(1);

        assert_eq!(runtime.deliver(future), MessageDelivery::Accepted);
        assert_eq!(runtime.deliver(current), MessageDelivery::Accepted);

        runtime.run_phase(SimTick(1), SimPhase(1));
        assert_eq!(runtime.actor(RegionId(1)).unwrap().state.counter, 10);

        runtime.run_phase(SimTick(3), SimPhase(1));
        assert_eq!(runtime.actor(RegionId(1)).unwrap().state.counter, 40);
    }

    #[test]
    fn same_phase_late_messages_are_rejected_after_phase_closes() {
        let mut runtime = ActorRuntime::new([RegionId(1)]);
        runtime.run_phase(SimTick(1), SimPhase(1));
        let mut late_same_phase = message(1, 1, 1, 1, 10);
        late_same_phase.target = RegionId(1);

        let delivery = runtime.deliver(late_same_phase);
        let rerun = runtime.run_phase(SimTick(1), SimPhase(1));

        assert_eq!(delivery, MessageDelivery::RejectedStale);
        assert_eq!(rerun[&RegionId(1)], PhaseRun::RejectedStale);
        assert_eq!(runtime.actor(RegionId(1)).unwrap().state.counter, 0);
    }

    #[test]
    fn same_tick_older_phase_messages_are_rejected_after_phase_advance() {
        let mut runtime = ActorRuntime::new([RegionId(1)]);
        runtime.run_phase(SimTick(1), SimPhase(2));
        let mut late_phase_one = message(1, 1, 1, 1, 10);
        late_phase_one.target = RegionId(1);

        let delivery = runtime.deliver(late_phase_one);
        runtime.run_phase(SimTick(1), SimPhase(1));

        assert_eq!(delivery, MessageDelivery::RejectedStale);
        assert_eq!(runtime.actor(RegionId(1)).unwrap().state.counter, 0);
    }

    #[test]
    fn actor_clock_rejects_backward_phase_and_tick_movement() {
        let mut actor = RegionActor::new(RegionId(1));
        assert_eq!(
            actor.run_phase(SimTick(1), SimPhase(2)).status,
            PhaseRun::Completed
        );

        assert_eq!(
            actor.run_phase(SimTick(1), SimPhase(1)).status,
            PhaseRun::RejectedStale
        );
        assert_eq!(actor.advance_to_tick(SimTick(1)), PhaseRun::RejectedStale);
        assert_eq!(actor.advance_to_tick(SimTick(0)), PhaseRun::RejectedStale);
        assert_eq!(actor.current_cursor(), (SimTick(1), SimPhase(2)));
    }

    #[test]
    fn direct_actor_delivery_rejects_wrong_target() {
        let mut actor = RegionActor::new(RegionId(10));

        let delivery = actor.deliver(message(1, 1, 2, 1, 8));

        assert_eq!(delivery, MessageDelivery::WrongTarget);
        assert!(actor.inbox.is_empty());
    }

    #[test]
    fn direct_actor_run_phase_returns_generated_fake_metric_responses() {
        let mut actor = RegionActor::new(RegionId(2));
        let request = fake_metric_request(1, 1, RegionId(0), RegionId(2), 12, 8);
        assert_eq!(actor.deliver(request), MessageDelivery::Accepted);

        let result = actor.run_phase(SimTick(1), SimPhase(1));

        assert_eq!(result.status, PhaseRun::Completed);
        assert!(actor.state.committed_events.is_empty());
        assert_eq!(result.outgoing.len(), 1);
        let response = &result.outgoing[0];
        assert_eq!(response.tick, SimTick(1));
        assert_eq!(response.phase, SimPhase(1));
        assert_eq!(response.source, RegionId(2));
        assert_eq!(response.target, RegionId(0));
        assert_eq!(response.sequence, MessageSequence(12));
        match &response.kind {
            RegionMessageKind::PromiseGroupResponse(response) => {
                assert_eq!(response.promise_id, PromiseId(8));
                assert_eq!(response.dependency, RegionId(2));
                assert_eq!(response.value, 3);
            }
            other => panic!("expected fake metric response, got {other:?}"),
        }
    }

    #[test]
    fn runtime_rejects_same_phase_message_cycles_instead_of_spinning() {
        let mut runtime = ActorRuntime::new([RegionId(1), RegionId(2)]);
        assert_eq!(
            runtime.deliver(cyclic_same_phase_message(1, 1, RegionId(1), RegionId(2), 1)),
            MessageDelivery::Accepted
        );

        let result = runtime.run_phase(SimTick(1), SimPhase(1));

        assert_eq!(result[&RegionId(1)], PhaseRun::RejectedMessageLimit);
        assert_eq!(result[&RegionId(2)], PhaseRun::RejectedMessageLimit);
        assert_eq!(
            runtime.actor(RegionId(1)).unwrap().current_cursor(),
            (SimTick(0), SimPhase(0))
        );
        assert_eq!(
            runtime.actor(RegionId(2)).unwrap().current_cursor(),
            (SimTick(0), SimPhase(0))
        );
        assert!(
            runtime
                .actor(RegionId(1))
                .unwrap()
                .state
                .committed_events
                .is_empty()
        );
        assert!(
            runtime
                .actor(RegionId(2))
                .unwrap()
                .state
                .committed_events
                .is_empty()
        );
    }

    #[test]
    fn promise_group_resolves_after_all_responses_arrive() {
        let mut actor = RegionActor::new(RegionId(10));
        actor.register_promise_group(PromiseGroup::new(
            PromiseId(1),
            SimTick(1),
            SimPhase(1),
            [RegionId(1), RegionId(2)],
        ));
        assert_eq!(
            actor.deliver(promise_group_message(1, 1, 1, 1, 1, 7)),
            MessageDelivery::Accepted
        );

        actor.process_inbox_to_local_events(SimTick(1), SimPhase(1));
        assert!(actor.local_events.is_empty());
        assert_eq!(
            actor.deliver(promise_group_message(1, 1, 2, 2, 1, 11)),
            MessageDelivery::Accepted
        );

        actor.process_inbox_to_local_events(SimTick(1), SimPhase(1));

        assert_eq!(actor.local_events.len(), 1);
        actor.commit_local_events(SimTick(1), SimPhase(1));
        assert_eq!(actor.state.counter, 18);
    }

    #[test]
    fn promise_group_applies_responses_in_stable_dependency_order() {
        let mut runtime = ActorRuntime::new([RegionId(10)]);
        runtime
            .actors
            .get_mut(&RegionId(10))
            .unwrap()
            .register_promise_group(PromiseGroup::new(
                PromiseId(2),
                SimTick(1),
                SimPhase(1),
                [RegionId(3), RegionId(1), RegionId(2)],
            ));
        for message in [
            promise_group_message(1, 1, 3, 1, 2, 30),
            promise_group_message(1, 1, 1, 2, 2, 10),
            promise_group_message(1, 1, 2, 3, 2, 20),
        ] {
            assert_eq!(runtime.deliver(message), MessageDelivery::Accepted);
        }

        runtime.run_phase(SimTick(1), SimPhase(1));

        let event = only_committed_event(runtime.actor(RegionId(10)).unwrap());
        match &event.kind {
            RegionEventKind::PromiseResolved(resolved) => {
                assert_eq!(
                    resolved.ordered_dependencies,
                    vec![RegionId(1), RegionId(2), RegionId(3)]
                );
                assert_eq!(resolved.total, 60);
            }
            other => panic!("expected promise resolution, got {other:?}"),
        }
    }

    #[test]
    fn promise_chain_preserves_declared_step_order() {
        let mut runtime = ActorRuntime::new([RegionId(10)]);
        runtime
            .actors
            .get_mut(&RegionId(10))
            .unwrap()
            .register_promise_chain(PromiseChain::new(
                PromiseId(3),
                SimTick(1),
                SimPhase(1),
                [RegionId(2), RegionId(1)],
            ));
        assert_eq!(
            runtime.deliver(promise_chain_message(1, 1, 1, 1, 3, 10)),
            MessageDelivery::Accepted
        );
        assert_eq!(
            runtime.deliver(promise_chain_message(1, 1, 2, 2, 3, 20)),
            MessageDelivery::Accepted
        );

        runtime.run_phase(SimTick(1), SimPhase(1));

        let event = only_committed_event(runtime.actor(RegionId(10)).unwrap());
        match &event.kind {
            RegionEventKind::PromiseResolved(resolved) => {
                assert_eq!(
                    resolved.ordered_dependencies,
                    vec![RegionId(2), RegionId(1)]
                );
                assert_eq!(resolved.total, 30);
            }
            other => panic!("expected promise resolution, got {other:?}"),
        }
    }

    #[test]
    fn promise_callback_enqueues_local_event_without_committing_state() {
        let mut actor = RegionActor::new(RegionId(10));
        actor.register_promise_group(PromiseGroup::new(
            PromiseId(4),
            SimTick(1),
            SimPhase(1),
            [RegionId(1)],
        ));
        assert_eq!(
            actor.deliver(promise_group_message(1, 1, 1, 1, 4, 12)),
            MessageDelivery::Accepted
        );

        actor.process_inbox_to_local_events(SimTick(1), SimPhase(1));

        assert_eq!(actor.state.counter, 0);
        assert_eq!(actor.local_events.len(), 1);

        actor.commit_local_events(SimTick(1), SimPhase(1));

        assert_eq!(actor.state.counter, 12);
    }

    #[test]
    fn late_promise_response_cannot_modify_completed_tick() {
        let mut runtime = ActorRuntime::new([RegionId(10)]);
        runtime
            .actors
            .get_mut(&RegionId(10))
            .unwrap()
            .register_promise_group(PromiseGroup::new(
                PromiseId(5),
                SimTick(1),
                SimPhase(1),
                [RegionId(1)],
            ));
        runtime.run_phase(SimTick(1), SimPhase(1));

        let delivery = runtime.deliver(promise_group_message(1, 1, 1, 1, 5, 99));
        let rerun = runtime.run_phase(SimTick(1), SimPhase(1));

        assert_eq!(delivery, MessageDelivery::RejectedStale);
        assert_eq!(rerun[&RegionId(10)], PhaseRun::RejectedStale);
        assert_eq!(runtime.actor(RegionId(10)).unwrap().state.counter, 0);
    }

    #[test]
    fn promise_group_ignores_responses_from_different_phase_scope() {
        let mut actor = RegionActor::new(RegionId(10));
        actor.register_promise_group(PromiseGroup::new(
            PromiseId(6),
            SimTick(1),
            SimPhase(1),
            [RegionId(1), RegionId(2)],
        ));
        assert_eq!(
            actor.deliver(promise_group_message(1, 1, 1, 1, 6, 7)),
            MessageDelivery::Accepted
        );
        assert_eq!(
            actor.deliver(promise_group_message(2, 1, 2, 2, 6, 11)),
            MessageDelivery::Accepted
        );

        actor.process_inbox_to_local_events(SimTick(1), SimPhase(1));
        actor.process_inbox_to_local_events(SimTick(2), SimPhase(1));

        assert!(actor.local_events.is_empty());
        assert_eq!(actor.state.counter, 0);
    }

    #[test]
    fn promise_chain_ignores_responses_from_different_phase_scope() {
        let mut actor = RegionActor::new(RegionId(10));
        actor.register_promise_chain(PromiseChain::new(
            PromiseId(7),
            SimTick(1),
            SimPhase(1),
            [RegionId(1), RegionId(2)],
        ));
        assert_eq!(
            actor.deliver(promise_chain_message(1, 1, 1, 1, 7, 7)),
            MessageDelivery::Accepted
        );
        assert_eq!(
            actor.deliver(promise_chain_message(1, 2, 2, 2, 7, 11)),
            MessageDelivery::Accepted
        );

        actor.process_inbox_to_local_events(SimTick(1), SimPhase(1));
        actor.process_inbox_to_local_events(SimTick(1), SimPhase(2));

        assert!(actor.local_events.is_empty());
        assert_eq!(actor.state.counter, 0);
    }

    #[test]
    fn fake_neighbor_query_requests_only_expected_regions() {
        let partition = RegionPartition::new(4, 4, 2, 2);
        let mut runtime = ActorRuntime::new([RegionId(0), RegionId(1), RegionId(2), RegionId(3)]);

        let requested = runtime.start_fake_neighbor_metric_query(
            &partition,
            RegionId(0),
            SimTick(1),
            SimPhase(1),
            PromiseId(8),
        );

        assert_eq!(
            requested,
            vec![
                (RegionId(1), MessageDelivery::Accepted),
                (RegionId(2), MessageDelivery::Accepted)
            ]
        );
        assert_eq!(runtime.actor(RegionId(1)).unwrap().inbox.len(), 1);
        assert_eq!(runtime.actor(RegionId(2)).unwrap().inbox.len(), 1);
        assert!(runtime.actor(RegionId(3)).unwrap().inbox.is_empty());
    }

    #[test]
    fn fake_neighbor_query_response_order_does_not_change_metric() {
        let ordered = runtime_after_fake_query_requests([RegionId(1), RegionId(2)]);
        let shuffled = runtime_after_fake_query_requests([RegionId(2), RegionId(1)]);

        assert_eq!(ordered.actor(RegionId(0)).unwrap().state.counter, 5);
        assert_eq!(shuffled.actor(RegionId(0)).unwrap().state.counter, 5);
        assert_eq!(
            only_promise_resolution(ordered.actor(RegionId(0)).unwrap()).ordered_dependencies,
            only_promise_resolution(shuffled.actor(RegionId(0)).unwrap()).ordered_dependencies
        );
    }

    #[test]
    fn threaded_runtime_executor_matches_single_thread_runtime_result() {
        let single = runtime_after_fake_query_with_executor([RegionId(1), RegionId(2)], None);
        let mut threaded_executor = ThreadedActorExecutor;
        let threaded = runtime_after_fake_query_with_executor(
            [RegionId(2), RegionId(1)],
            Some(&mut threaded_executor),
        );

        assert_eq!(single.actor(RegionId(0)).unwrap().state.counter, 5);
        assert_eq!(threaded.actor(RegionId(0)).unwrap().state.counter, 5);
        assert_eq!(
            only_promise_resolution(single.actor(RegionId(0)).unwrap()).ordered_dependencies,
            only_promise_resolution(threaded.actor(RegionId(0)).unwrap()).ordered_dependencies
        );
        assert_eq!(single.actor(RegionId(1)).unwrap().state.counter, 0);
        assert_eq!(threaded.actor(RegionId(1)).unwrap().state.counter, 0);
    }

    #[test]
    fn fake_neighbor_query_commits_only_at_expected_phase() {
        let partition = RegionPartition::new(4, 4, 2, 2);
        let mut runtime = ActorRuntime::new([RegionId(0), RegionId(1), RegionId(2), RegionId(3)]);

        runtime.start_fake_neighbor_metric_query(
            &partition,
            RegionId(0),
            SimTick(1),
            SimPhase(2),
            PromiseId(10),
        );
        runtime.run_phase(SimTick(1), SimPhase(1));
        assert_eq!(runtime.actor(RegionId(0)).unwrap().state.counter, 0);

        runtime.run_phase(SimTick(1), SimPhase(2));
        assert_eq!(runtime.actor(RegionId(0)).unwrap().state.counter, 5);
    }

    #[test]
    fn actor_border_pollution_matches_existing_local_effects_result() {
        let mut world = World::new(6, 4);
        attach_building(&mut world, 1, 1, BuildingKind::Industrial);
        attach_building(&mut world, 4, 2, BuildingKind::Industrial);
        local_effects::run(&mut world);
        let partition = RegionPartition::new(6, 4, 3, 2);
        let mut runtime = ActorRuntime::new(partition.region_ids());

        let deliveries = runtime.enqueue_border_pollution_samples(
            &partition,
            &world.local_effects,
            SimTick(1),
            SimPhase(1),
        );
        runtime.run_phase(SimTick(1), SimPhase(1));

        for region in partition.region_ids() {
            assert!(
                deliveries[&region]
                    .iter()
                    .all(|delivery| *delivery == MessageDelivery::Accepted)
            );
            assert_eq!(
                runtime
                    .actor(region)
                    .unwrap()
                    .state
                    .read_only
                    .border_pollution,
                border_pollution_summary(&partition, &world.local_effects, region)
            );
        }
    }

    #[test]
    fn shuffled_border_pollution_samples_produce_same_actor_result() {
        let ordered = actor_after_border_pollution_samples([1, 4, 2]);
        let shuffled = actor_after_border_pollution_samples([2, 1, 4]);

        assert_eq!(ordered.state.read_only.border_pollution, 7);
        assert_eq!(
            ordered.state.read_only.border_pollution,
            shuffled.state.read_only.border_pollution
        );
    }

    #[test]
    fn shuffled_local_effect_cells_produce_same_actor_result() {
        let ordered = actor_after_local_effect_cells([(0, 0, 2), (1, 0, 5), (0, 1, 3)]);
        let shuffled = actor_after_local_effect_cells([(0, 1, 3), (0, 0, 2), (1, 0, 5)]);

        assert_eq!(
            ordered.state.read_only.local_effect_cells,
            shuffled.state.read_only.local_effect_cells
        );
        assert_eq!(
            ordered.state.read_only.local_effect_cells[0]
                .effects
                .land_value,
            2
        );
        assert_eq!(
            ordered.state.read_only.local_effect_cells[1]
                .effects
                .land_value,
            5
        );
        assert_eq!(
            ordered.state.read_only.local_effect_cells[2]
                .effects
                .land_value,
            3
        );
    }

    #[test]
    fn border_pollution_clears_when_later_phase_has_no_samples() {
        let mut actor = actor_after_border_pollution_samples([1, 4, 2]);
        assert_eq!(actor.state.read_only.border_pollution, 7);

        let result = actor.run_phase(SimTick(1), SimPhase(2));

        assert_eq!(result.status, PhaseRun::Completed);
        assert_eq!(actor.state.read_only.border_pollution, 0);
    }

    fn runtime_state_after_delivery(messages: Vec<RegionMessage>) -> ActorState {
        let mut runtime = ActorRuntime::new([RegionId(99)]);
        for mut message in messages {
            message.target = RegionId(99);
            assert_eq!(runtime.deliver(message), MessageDelivery::Accepted);
        }
        runtime.run_phase(SimTick(1), SimPhase(1));
        runtime.run_phase(SimTick(1), SimPhase(2));
        runtime.run_phase(SimTick(2), SimPhase(1));
        runtime.actor(RegionId(99)).unwrap().state.clone()
    }

    fn actor_after_border_pollution_samples<const N: usize>(samples: [i32; N]) -> RegionActor {
        let mut actor = RegionActor::new(RegionId(3));
        for (sequence, value) in samples.into_iter().enumerate() {
            assert_eq!(
                actor.deliver(border_pollution_sample_message(
                    1,
                    1,
                    RegionId(3),
                    sequence as u64,
                    value
                )),
                MessageDelivery::Accepted
            );
        }
        assert_eq!(
            actor.run_phase(SimTick(1), SimPhase(1)).status,
            PhaseRun::Completed
        );
        actor
    }

    fn actor_after_local_effect_cells<const N: usize>(
        cells: [(usize, usize, i32); N],
    ) -> RegionActor {
        let mut actor = RegionActor::new(RegionId(4));
        for (sequence, (x, y, land_value)) in cells.into_iter().enumerate() {
            assert_eq!(
                actor.deliver(local_effect_cell_message(
                    1,
                    1,
                    RegionId(4),
                    sequence as u64,
                    x,
                    y,
                    land_value
                )),
                MessageDelivery::Accepted
            );
        }
        assert_eq!(
            actor.run_phase(SimTick(1), SimPhase(1)).status,
            PhaseRun::Completed
        );
        actor
    }

    fn runtime_after_fake_query_requests<const N: usize>(
        request_order: [RegionId; N],
    ) -> ActorRuntime {
        runtime_after_fake_query_with_executor(request_order, None)
    }

    fn runtime_after_fake_query_with_executor<const N: usize>(
        request_order: [RegionId; N],
        mut executor: Option<&mut ThreadedActorExecutor>,
    ) -> ActorRuntime {
        let mut runtime = ActorRuntime::new([RegionId(0), RegionId(1), RegionId(2)]);
        runtime
            .actors
            .get_mut(&RegionId(0))
            .unwrap()
            .register_promise_group(PromiseGroup::new(
                PromiseId(9),
                SimTick(1),
                SimPhase(1),
                [RegionId(1), RegionId(2)],
            ));
        for (index, neighbor) in request_order.into_iter().enumerate() {
            let message = fake_metric_request(1, 1, RegionId(0), neighbor, index as u64, 9);
            assert_eq!(runtime.deliver(message), MessageDelivery::Accepted);
        }
        if let Some(executor) = executor.as_mut() {
            runtime.run_phase_with_executor(SimTick(1), SimPhase(1), *executor);
        } else {
            runtime.run_phase(SimTick(1), SimPhase(1));
        }
        runtime
    }

    fn message(tick: u64, phase: u8, source: u32, sequence: u64, delta: i32) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source: RegionId(source),
            target: RegionId(0),
            sequence: MessageSequence(sequence),
            kind: RegionMessageKind::AddCounter(delta),
        }
    }

    fn promise_group_message(
        tick: u64,
        phase: u8,
        source: u32,
        sequence: u64,
        promise: u64,
        value: i32,
    ) -> RegionMessage {
        promise_message(
            tick,
            phase,
            source,
            sequence,
            RegionMessageKind::PromiseGroupResponse(PromiseResponse {
                promise_id: PromiseId(promise),
                tick: SimTick(tick),
                phase: SimPhase(phase),
                dependency: RegionId(source),
                value,
            }),
        )
    }

    fn promise_chain_message(
        tick: u64,
        phase: u8,
        source: u32,
        sequence: u64,
        promise: u64,
        value: i32,
    ) -> RegionMessage {
        promise_message(
            tick,
            phase,
            source,
            sequence,
            RegionMessageKind::PromiseChainResponse(PromiseResponse {
                promise_id: PromiseId(promise),
                tick: SimTick(tick),
                phase: SimPhase(phase),
                dependency: RegionId(source),
                value,
            }),
        )
    }

    fn promise_message(
        tick: u64,
        phase: u8,
        source: u32,
        sequence: u64,
        kind: RegionMessageKind,
    ) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source: RegionId(source),
            target: RegionId(10),
            sequence: MessageSequence(sequence),
            kind,
        }
    }

    fn fake_metric_request(
        tick: u64,
        phase: u8,
        source: RegionId,
        target: RegionId,
        sequence: u64,
        promise: u64,
    ) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source,
            target,
            sequence: MessageSequence(sequence),
            kind: RegionMessageKind::FakeBorderMetricRequest {
                promise_id: PromiseId(promise),
            },
        }
    }

    fn border_pollution_sample_message(
        tick: u64,
        phase: u8,
        target: RegionId,
        sequence: u64,
        value: i32,
    ) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source: target,
            target,
            sequence: MessageSequence(sequence),
            kind: RegionMessageKind::ReadOnlyBorderPollutionSample { value },
        }
    }

    fn local_effect_cell_message(
        tick: u64,
        phase: u8,
        target: RegionId,
        sequence: u64,
        x: usize,
        y: usize,
        land_value: i32,
    ) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source: target,
            target,
            sequence: MessageSequence(sequence),
            kind: RegionMessageKind::LocalEffectsCellSample(LocalEffectsCell {
                x,
                y,
                effects: LocalEffects {
                    land_value,
                    pollution_pressure: 0,
                    accessibility: 0,
                    desirability: land_value,
                },
            }),
        }
    }

    fn cyclic_same_phase_message(
        tick: u64,
        phase: u8,
        source: RegionId,
        target: RegionId,
        sequence: u64,
    ) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source,
            target,
            sequence: MessageSequence(sequence),
            kind: RegionMessageKind::CyclicSamePhaseRequest,
        }
    }

    fn only_committed_event(actor: &RegionActor) -> &RegionEvent {
        assert_eq!(actor.state.committed_events.len(), 1);
        &actor.state.committed_events[0]
    }

    fn only_promise_resolution(actor: &RegionActor) -> &PromiseResolved {
        match &only_committed_event(actor).kind {
            RegionEventKind::PromiseResolved(resolved) => resolved,
            other => panic!("expected promise resolution, got {other:?}"),
        }
    }

    fn attach_building(world: &mut World, x: usize, y: usize, kind: BuildingKind) {
        let entity = world.spawn();
        world.attach_position(entity, Position { x, y });
        world.attach_building(
            entity,
            Building {
                kind,
                level: 1,
                data: BuildingData::None,
            },
        );
    }
}
