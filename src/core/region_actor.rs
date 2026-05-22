//! Deterministic single-threaded region actor runtime prototype.
//!
//! This module is intentionally disconnected from the real city `World`. It proves the
//! tick/phase/message ordering rules that a future multithreaded region model can reuse.

use std::collections::BTreeMap;

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
    pub tick: SimTick,
    pub phase: SimPhase,
    pub source: RegionId,
    pub target: RegionId,
    pub sequence: MessageSequence,
    pub kind: RegionMessageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionMessageKind {
    AddCounter(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionEvent {
    pub tick: SimTick,
    pub phase: SimPhase,
    pub source: RegionId,
    pub sequence: MessageSequence,
    pub kind: RegionEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionEventKind {
    CommitCounterDelta(i32),
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
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActorState {
    pub counter: i32,
    pub committed_events: Vec<RegionEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionActor {
    pub id: RegionId,
    pub current_tick: SimTick,
    pub current_phase: SimPhase,
    pub inbox: Vec<RegionMessage>,
    pub local_events: Vec<RegionEvent>,
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
            state: ActorState::default(),
        }
    }

    pub fn deliver(&mut self, message: RegionMessage) -> MessageDelivery {
        if message.target != self.id {
            return MessageDelivery::WrongTarget;
        }
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

    fn process_inbox_to_local_events(&mut self, tick: SimTick, phase: SimPhase) {
        self.inbox.sort_by_key(region_message_order);
        let messages = std::mem::take(&mut self.inbox);
        for message in messages {
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
            }
        }
    }

    fn commit_local_events(&mut self, tick: SimTick, phase: SimPhase) {
        self.local_events.sort_by_key(region_event_order);
        let events = std::mem::take(&mut self.local_events);
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
            }
        }
    }

    pub fn run_phase(&mut self, tick: SimTick, phase: SimPhase) -> PhaseRun {
        if (tick, phase) <= self.current_cursor() {
            return PhaseRun::RejectedStale;
        }
        self.process_inbox_to_local_events(tick, phase);
        self.commit_local_events(tick, phase);
        self.advance_to_phase(tick, phase);
        PhaseRun::Completed
    }

    fn current_cursor(&self) -> (SimTick, SimPhase) {
        (self.current_tick, self.current_phase)
    }

    fn advance_to_phase(&mut self, tick: SimTick, phase: SimPhase) {
        self.current_tick = tick;
        self.current_phase = phase;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorRuntime {
    actors: BTreeMap<RegionId, RegionActor>,
    next_sequence: MessageSequence,
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
        }
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
        let mut results = BTreeMap::new();
        for actor in self.actors.values_mut() {
            results.insert(actor.id, actor.run_phase(tick, phase));
        }
        results
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
mod tests {
    use super::*;

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
            actor.run_phase(SimTick(1), SimPhase(2)),
            PhaseRun::Completed
        );

        assert_eq!(
            actor.run_phase(SimTick(1), SimPhase(1)),
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
}
