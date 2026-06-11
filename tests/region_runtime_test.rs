//! Integration tests for the single-threaded regional event runtime.

use small_city::core::regional_types::{RegionCommand, RegionCommandReply, UiRequestId};
use small_city::core::regions::runtime::{OutboundMessage, RegionEvent, RegionRuntime};
use small_city::core::regions::{RegionId, RegionState};
use small_city::interface::events::GameEventView;
use small_city::interface::input::BuildingKind;

#[test]
fn local_tick_is_processed_through_runtime() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 3, 2));
    runtime.push_event(RegionEvent::Tick {
        request_id: UiRequestId(10),
    });

    let outbound = runtime.process_next_event();

    let reply = outbound
        .iter()
        .find_map(|message| match message {
            OutboundMessage::RegionTickCompleted(reply) => Some(reply),
            _ => None,
        })
        .expect("expected tick reply");
    assert_eq!(reply.request_id, UiRequestId(10));
    assert_eq!(reply.region_id, RegionId(1));
    assert!(reply.result.success);
    assert!(matches!(
        &reply.result.event,
        GameEventView::TickSummary { turn: 1, .. }
    ));
    assert_eq!(runtime.state().view().status.turn, 1);
    assert_eq!(runtime.pending_event_count(), 0);
}

#[test]
fn events_are_processed_in_insertion_order() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
    runtime.push_event(RegionEvent::Tick {
        request_id: UiRequestId(30),
    });
    runtime.push_event(RegionEvent::Tick {
        request_id: UiRequestId(31),
    });

    let outbound = runtime.process_some_events(2);

    assert_eq!(
        tick_reply_ids(&outbound),
        vec![UiRequestId(30), UiRequestId(31)]
    );
    assert_eq!(runtime.state().view().status.turn, 2);
}

#[test]
fn process_some_events_respects_max_events() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(3), 3, 2));
    runtime.push_event(RegionEvent::Tick {
        request_id: UiRequestId(20),
    });
    runtime.push_event(RegionEvent::Tick {
        request_id: UiRequestId(21),
    });

    let outbound = runtime.process_some_events(1);

    assert!(outbound.iter().any(|message| matches!(
        message,
        OutboundMessage::RegionTickCompleted(reply)
            if reply.request_id == UiRequestId(20) && reply.region_id == RegionId(3)
    )));
    assert_eq!(runtime.state().view().status.turn, 1);
    assert_eq!(runtime.pending_event_count(), 1);
}

#[test]
fn successful_build_returns_command_reply_from_runtime() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(30), 3, 3));

    runtime.push_event(RegionEvent::RunCommand {
        request_id: UiRequestId(1),
        command: RegionCommand::Build {
            x: 1,
            y: 1,
            kind: BuildingKind::Park,
        },
    });

    let outbound = runtime.process_next_event();

    assert!(
        outbound.iter().any(|message| matches!(
            message,
            OutboundMessage::RegionCommandCompleted(reply)
                if reply.request_id == UiRequestId(1)
                    && matches!(reply.reply, RegionCommandReply::CommandResult(ref result) if result.success)
        ))
    );
}

#[test]
fn bulldoze_returns_command_reply_from_runtime() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(31), 3, 3));

    runtime.push_event(RegionEvent::RunCommand {
        request_id: UiRequestId(1),
        command: RegionCommand::Build {
            x: 1,
            y: 1,
            kind: BuildingKind::Park,
        },
    });
    runtime.process_next_event();
    runtime.push_event(RegionEvent::RunCommand {
        request_id: UiRequestId(2),
        command: RegionCommand::Bulldoze { x: 1, y: 1 },
    });

    let outbound = runtime.process_next_event();
    assert!(outbound.iter().any(|message| matches!(
        message,
        OutboundMessage::RegionCommandCompleted(reply)
            if reply.request_id == UiRequestId(2)
                && matches!(reply.reply, RegionCommandReply::CommandResult(ref result) if result.success)
    )));
}

fn tick_reply_ids(outbound: &[OutboundMessage]) -> Vec<UiRequestId> {
    outbound
        .iter()
        .filter_map(|message| match message {
            OutboundMessage::RegionTickCompleted(reply) => Some(reply.request_id),
            _ => None,
        })
        .collect()
}
