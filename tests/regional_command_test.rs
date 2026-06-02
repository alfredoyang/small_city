//! Integration tests for player commands routed through the regional facade.

use std::sync::{Arc, Barrier};
use std::thread;

use small_city::core::game::Game;
use small_city::core::regional_game::{RegionCommand, RegionCommandReply, RegionalGame};
use small_city::core::regional_types::UiRequestId;
use small_city::core::regions::runtime::{OutboundMessage, RegionEvent, RegionRuntime};
use small_city::core::regions::{RegionId, RegionState};
use small_city::interface::events::CommandResult;
use small_city::interface::input::{BuildingKind, MapOverlayInput};
use small_city::interface::view::BuildPreviewView;

#[test]
fn build_command_changes_only_requested_region_view() {
    let region_a = RegionId(1);
    let region_b = RegionId(2);
    let game = RegionalGame::from_regions(vec![
        RegionState::new(region_a, 3, 3),
        RegionState::new(region_b, 3, 3),
    ])
    .unwrap();

    let result = game
        .build(region_a, 1, 1, BuildingKind::Residential)
        .unwrap();
    let view = game.view().unwrap();

    assert!(result.success);
    assert_eq!(
        cell_building(&view.regions[0].view, 1, 1),
        Some(BuildingKind::Residential)
    );
    assert_eq!(cell_building(&view.regions[1].view, 1, 1), None);
}

#[test]
fn preview_build_returns_owned_data_without_mutating_region() {
    let region_id = RegionId(3);
    let game = RegionalGame::from_regions(vec![RegionState::new(region_id, 3, 3)]).unwrap();

    let before = game.view().unwrap();
    let preview = game
        .preview_build(region_id, 0, 0, BuildingKind::Commercial)
        .unwrap();
    let after = game.view().unwrap();

    assert_preview_is_owned(preview);
    assert_eq!(before.regions[0].view, after.regions[0].view);
    assert_eq!(cell_building(&after.regions[0].view, 0, 0), None);
}

#[test]
fn bulldoze_replace_and_upgrade_match_game_command_results() {
    let region_id = RegionId(4);
    let regional = RegionalGame::from_regions(vec![RegionState::new(region_id, 4, 4)]).unwrap();
    let mut single = Game::new(4, 4);

    assert_eq!(
        regional
            .build(region_id, 0, 0, BuildingKind::Residential)
            .unwrap(),
        single.build(0, 0, BuildingKind::Residential)
    );
    assert_eq!(
        regional.upgrade(region_id, 0, 0).unwrap(),
        single.upgrade(0, 0)
    );
    assert_eq!(
        regional
            .replace(region_id, 0, 0, BuildingKind::Commercial)
            .unwrap(),
        single.replace(0, 0, BuildingKind::Commercial)
    );
    assert_eq!(
        regional.bulldoze(region_id, 0, 0).unwrap(),
        single.bulldoze(0, 0)
    );

    assert_eq!(regional.view().unwrap().regions[0].view, single.view());
}

#[test]
fn command_for_unknown_region_returns_error() {
    let region_id = RegionId(5);
    let game = RegionalGame::from_regions(vec![RegionState::new(region_id, 3, 3)]).unwrap();

    let error = game
        .build(RegionId(99), 0, 0, BuildingKind::Road)
        .expect_err("unknown command region should fail");

    assert_eq!(
        error,
        small_city::core::regional_game::RegionalGameError::UnknownRegion {
            region_id: RegionId(99),
        }
    );
}

#[test]
fn command_payloads_and_replies_are_owned() {
    assert_send_static::<RegionCommand>();
    assert_send_static::<RegionCommandReply>();
    assert_send_static::<CommandResult>();
    assert_send_static::<BuildPreviewView>();

    let command = RegionCommand::Build {
        x: 1,
        y: 2,
        kind: BuildingKind::Park,
    };
    let reply = RegionCommandReply::BuildPreview(BuildPreviewView {
        kind: BuildingKind::Park,
        label: "Park".to_string(),
        cost: 10,
        can_build: true,
        reason: None,
        effects: vec!["owned".to_string()],
    });

    let _owned_tuple = (UiRequestId(123), command, reply);
}

#[test]
fn commands_and_ticks_process_in_call_order() {
    let region_id = RegionId(6);
    let regional = RegionalGame::from_regions(vec![RegionState::new(region_id, 3, 3)]).unwrap();
    let mut single = Game::new(3, 3);

    assert_eq!(
        regional.build(region_id, 1, 1, BuildingKind::Road).unwrap(),
        single.build(1, 1, BuildingKind::Road)
    );
    regional.tick_region(region_id).unwrap();
    single.tick();

    assert_eq!(regional.view().unwrap().regions[0].view, single.view());
}

#[test]
fn command_and_snapshot_events_share_fifo_ordering() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(7), 3, 3));

    runtime.push_event(RegionEvent::RunCommand {
        request_id: UiRequestId(1),
        command: RegionCommand::Build {
            x: 1,
            y: 1,
            kind: BuildingKind::Road,
        },
    });
    runtime.push_event(RegionEvent::BuildSnapshot {
        request_id: UiRequestId(2),
        overlay: MapOverlayInput::Normal,
    });

    let outbound = runtime.process_some_events(2);

    let OutboundMessage::RegionCommandCompleted(command_reply) = &outbound[0] else {
        panic!("first outbound message should be command reply");
    };
    let OutboundMessage::RegionSnapshotReady(snapshot_reply) = &outbound[1] else {
        panic!("second outbound message should be snapshot reply");
    };

    assert_eq!(command_reply.request_id, UiRequestId(1));
    assert_eq!(snapshot_reply.request_id, UiRequestId(2));
    assert_eq!(
        cell_building(&snapshot_reply.snapshot.view, 1, 1),
        Some(BuildingKind::Road)
    );
}

#[test]
fn concurrent_regional_commands_do_not_steal_each_others_replies() {
    let region_id = RegionId(8);
    let game =
        Arc::new(RegionalGame::from_regions(vec![RegionState::new(region_id, 6, 6)]).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let placements = [(0, 0), (1, 0), (2, 0), (3, 0)];

    let workers = placements
        .into_iter()
        .map(|(x, y)| {
            let game = Arc::clone(&game);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                game.build(region_id, x, y, BuildingKind::Road)
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        let result = worker.join().unwrap().unwrap();
        assert!(result.success);
    }

    let view = game.view().unwrap();
    for (x, y) in placements {
        assert_eq!(
            cell_building(&view.regions[0].view, x, y),
            Some(BuildingKind::Road)
        );
    }
}

fn cell_building(
    view: &small_city::interface::view::GameView,
    x: usize,
    y: usize,
) -> Option<BuildingKind> {
    view.map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.building)
}

fn assert_preview_is_owned(preview: BuildPreviewView) {
    let _effects = preview.effects;
}

fn assert_send_static<T: Send + 'static>() {}
