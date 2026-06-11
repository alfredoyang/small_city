//! Integration tests for stable in-process region handles.

use small_city::core::regional_game::UiRequestId;
use small_city::core::regions::runtime::{OutboundMessage, RegionEvent, RegionRuntime};
use small_city::core::regions::{RegionId, RegionState};

#[test]
fn region_can_send_event_through_neighbor_handle() {
    let source = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
    let target_handle = target.handle();

    source.send_to_region(&target_handle, tick(1));

    assert_eq!(target.pending_event_count(), 1);
    assert_eq!(tick_reply_count(&target.process_next_event()), 1);
    assert_eq!(target.state().view().status.turn, 1);
    assert_eq!(source.state().view().status.turn, 0);
}

#[test]
fn sender_handle_can_be_cloned_without_cloning_receiver() {
    let mut target = RegionRuntime::new(RegionState::new(RegionId(3), 2, 2));
    let first_sender = target.handle();
    let second_sender = first_sender.clone();

    first_sender.send(tick(2));
    second_sender.send(tick(3));

    assert_eq!(target.pending_event_count(), 2);
    assert_eq!(tick_reply_count(&target.process_some_events(2)), 2);
    assert_eq!(target.state().view().status.turn, 2);
}

#[test]
fn receiver_remains_owned_by_target_runtime_after_runtime_moves() {
    let target = RegionRuntime::new(RegionState::new(RegionId(4), 2, 2));
    let handle = target.handle();

    let mut moved_target = target;
    handle.send(tick(4));

    assert_eq!(moved_target.pending_event_count(), 1);
    assert_eq!(tick_reply_count(&moved_target.process_next_event()), 1);
    assert_eq!(moved_target.state().view().status.turn, 1);
}

fn tick(request_id: u64) -> RegionEvent {
    RegionEvent::Tick {
        request_id: UiRequestId(request_id),
    }
}

fn tick_reply_count(outbound: &[OutboundMessage]) -> usize {
    outbound
        .iter()
        .filter(|message| matches!(message, OutboundMessage::RegionTickCompleted(_)))
        .count()
}
