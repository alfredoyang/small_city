//! Executor boundary for running region actor phase work.
//!
//! The single-threaded executor is the default coordinator path. The threaded executor uses
//! worker threads behind the same input/output shape so determinism can be tested before any real
//! simulation system is moved onto workers.

use std::collections::BTreeMap;
use std::thread;

use crate::core::region_actor::{
    PhaseRun, RegionActor, RegionId, RegionMessage, SimPhase, SimTick, region_message_order,
};

pub(crate) trait ActorExecutor {
    fn run_phase(&mut self, work: PhaseWork) -> PhaseResult;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhaseWork {
    pub tick: SimTick,
    pub phase: SimPhase,
    pub actors: Vec<RegionActor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhaseResult {
    pub actors: Vec<RegionActor>,
    pub outgoing: Vec<RegionMessage>,
    pub statuses: BTreeMap<RegionId, PhaseRun>,
}

#[derive(Debug, Default)]
pub(crate) struct SingleThreadActorExecutor;

impl ActorExecutor for SingleThreadActorExecutor {
    fn run_phase(&mut self, work: PhaseWork) -> PhaseResult {
        run_phase_work(work)
    }
}

#[derive(Debug, Default)]
pub(crate) struct ThreadedActorExecutor;

impl ActorExecutor for ThreadedActorExecutor {
    fn run_phase(&mut self, work: PhaseWork) -> PhaseResult {
        let mut handles = Vec::new();
        for actor in work.actors {
            let tick = work.tick;
            let phase = work.phase;
            handles.push(thread::spawn(move || run_actor_phase(actor, tick, phase)));
        }

        let mut actors = Vec::new();
        let mut outgoing = Vec::new();
        let mut statuses = BTreeMap::new();
        for handle in handles {
            let (actor, actor_outgoing, status) = handle.join().expect("actor worker panicked");
            statuses.insert(actor.id, status);
            actors.push(actor);
            outgoing.extend(actor_outgoing);
        }

        actors.sort_by_key(|actor| actor.id);
        outgoing.sort_by_key(region_message_order);
        PhaseResult {
            actors,
            outgoing,
            statuses,
        }
    }
}

fn run_phase_work(work: PhaseWork) -> PhaseResult {
    let mut actors = Vec::new();
    let mut outgoing = Vec::new();
    let mut statuses = BTreeMap::new();
    for actor in work.actors {
        let (actor, actor_outgoing, status) = run_actor_phase(actor, work.tick, work.phase);
        statuses.insert(actor.id, status);
        actors.push(actor);
        outgoing.extend(actor_outgoing);
    }

    actors.sort_by_key(|actor| actor.id);
    outgoing.sort_by_key(region_message_order);
    PhaseResult {
        actors,
        outgoing,
        statuses,
    }
}

fn run_actor_phase(
    mut actor: RegionActor,
    tick: SimTick,
    phase: SimPhase,
) -> (RegionActor, Vec<RegionMessage>, PhaseRun) {
    if (tick, phase) <= actor.current_cursor() {
        return (actor, Vec::new(), PhaseRun::RejectedStale);
    }

    let outgoing = actor.process_inbox_to_local_events(tick, phase);
    (actor, outgoing, PhaseRun::Completed)
}

#[cfg(test)]
mod tests {
    use super::{ActorExecutor, PhaseWork, SingleThreadActorExecutor, ThreadedActorExecutor};
    use crate::core::region_actor::{
        MessageDelivery, MessageSequence, RegionActor, RegionId, RegionMessage, RegionMessageKind,
        SimPhase, SimTick,
    };

    #[test]
    fn single_thread_and_threaded_executors_produce_identical_phase_results() {
        let work = phase_work_with_messages(vec![
            message(1, 1, RegionId(1), RegionId(1), 2, 10),
            message(1, 1, RegionId(2), RegionId(2), 1, 20),
        ]);

        let single = SingleThreadActorExecutor.run_phase(work.clone());
        let threaded = ThreadedActorExecutor.run_phase(work);

        assert_eq!(single, threaded);
    }

    #[test]
    fn threaded_executor_keeps_shuffled_delivery_deterministic() {
        let ordered = phase_work_with_messages(vec![
            message(1, 1, RegionId(1), RegionId(1), 1, 10),
            message(1, 1, RegionId(1), RegionId(1), 2, 20),
            message(1, 1, RegionId(2), RegionId(2), 1, 30),
        ]);
        let shuffled = phase_work_with_messages(vec![
            message(1, 1, RegionId(2), RegionId(2), 1, 30),
            message(1, 1, RegionId(1), RegionId(1), 2, 20),
            message(1, 1, RegionId(1), RegionId(1), 1, 10),
        ]);

        let ordered = ThreadedActorExecutor.run_phase(ordered);
        let shuffled = ThreadedActorExecutor.run_phase(shuffled);

        assert_eq!(ordered, shuffled);
    }

    fn phase_work_with_messages(messages: Vec<RegionMessage>) -> PhaseWork {
        let mut actors = vec![RegionActor::new(RegionId(1)), RegionActor::new(RegionId(2))];
        for message in messages {
            let actor = actors
                .iter_mut()
                .find(|actor| actor.id == message.target)
                .expect("target actor");
            assert_eq!(actor.deliver(message), MessageDelivery::Accepted);
        }
        PhaseWork {
            tick: SimTick(1),
            phase: SimPhase(1),
            actors,
        }
    }

    fn message(
        tick: u64,
        phase: u8,
        source: RegionId,
        target: RegionId,
        sequence: u64,
        delta: i32,
    ) -> RegionMessage {
        RegionMessage {
            tick: SimTick(tick),
            phase: SimPhase(phase),
            source,
            target,
            sequence: MessageSequence(sequence),
            kind: RegionMessageKind::AddCounter(delta),
        }
    }
}
