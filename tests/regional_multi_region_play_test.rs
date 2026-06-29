//! Integration tests for player-visible multi-region regional gameplay.

use small_city::core::regional_game::RegionalGame;
use small_city::core::regions::RegionId;
use small_city::interface::input::BuildingKind;
use small_city::interface::view::{CitizenRelation, InspectDetailsView, InspectFlag};
use small_city::ui::city_driver::{CityDriver, CityLaunchMode};

fn has_generic_imported_resource_note(game: &RegionalGame, region_id: RegionId) -> bool {
    game.inspect_region(region_id, 0, 0)
        .unwrap()
        .explanations
        .iter()
        .any(|note| note.contains("Imported regional resources"))
}

#[test]
fn player_can_build_in_two_regions_through_ui_driver() {
    let mut driver =
        CityDriver::new(CityLaunchMode::RegionalMultiRegion).expect("regional UI driver");

    assert!(driver.build(1, 1, BuildingKind::Residential).success);
    assert!(driver.region_label().contains("1/9"));
    let initial_region_a = driver.view();
    assert!(
        initial_region_a.map.cells[1 + initial_region_a.map.width]
            .building
            .is_some()
    );

    let switched = driver.select_next_region();
    assert!(switched.contains("2/9"));
    assert!(driver.build(2, 1, BuildingKind::Park).success);
    let region_b = driver.view();

    assert_eq!(
        region_b.map.cells[2 + region_b.map.width].building,
        Some(BuildingKind::Park)
    );
    assert_eq!(region_b.map.cells[1 + region_b.map.width].building, None);

    let switched_back = driver.select_previous_region();
    assert!(switched_back.contains("1/9"));
    let region_a = driver.view();

    assert_eq!(
        region_a.map.cells[1 + region_a.map.width].building,
        Some(BuildingKind::Residential)
    );
    assert_eq!(region_a.map.cells[2 + region_a.map.width].building, None);
}

#[test]
fn selected_region_switching_changes_composed_view_deterministically() {
    let mut game = RegionalGame::two_region_default(3, 3).unwrap();

    assert_eq!(game.selected_region().unwrap(), RegionId(1));
    assert_eq!(game.selected_region_position().unwrap(), (1, 2));

    game.select_next_region().unwrap();
    assert_eq!(game.selected_region().unwrap(), RegionId(2));
    assert_eq!(game.selected_region_position().unwrap(), (2, 2));
    assert_eq!(game.view().unwrap().selected_region, Some(RegionId(2)));

    game.select_previous_region().unwrap();
    assert_eq!(game.selected_region().unwrap(), RegionId(1));
    assert_eq!(game.selected_region_position().unwrap(), (1, 2));
}

#[test]
fn regional_view_reports_city_goods_and_city_aware_inspect_notes() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();
    build_goods_producer(&game, RegionId(1));
    build_goods_consumer(&game, RegionId(2));

    for _ in 0..(7 * 24) {
        game.tick_all_regions().unwrap();
    }

    let view = game.view().unwrap();
    assert!(view.goods.city_goods_produced > 0, "{:?}", view.goods);
    assert_eq!(
        view.goods.goods_imported_from_outside, 0,
        "{:?}",
        view.goods
    );

    game.tick_all_regions().unwrap();
    assert_eq!(game.view().unwrap().goods, view.goods);

    let inspect = game.inspect_region(RegionId(2), 1, 0).unwrap();
    let Some(InspectDetailsView::Commercial {
        goods_sold_from_city,
        goods_sold_from_outside,
        ..
    }) = inspect.details
    else {
        panic!("expected commercial inspect");
    };
    assert!(goods_sold_from_city > 0);
    assert_eq!(goods_sold_from_outside, 0);
    assert!(
        inspect
            .explanations
            .iter()
            .any(|note| note.contains("city goods"))
    );
    assert!(
        !inspect
            .explanations
            .iter()
            .any(|note| note.contains("local goods"))
    );
}

#[test]
fn commercial_inspect_reports_outside_goods_sales_when_city_supply_missing() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();
    build_goods_consumer(&game, RegionId(2));

    for _ in 0..(2 * 24) {
        game.tick_all_regions().unwrap();
    }

    let inspect = game.inspect_region(RegionId(2), 1, 0).unwrap();
    let Some(InspectDetailsView::Commercial {
        goods_sold_from_city,
        goods_sold_from_outside,
        ..
    }) = inspect.details
    else {
        panic!("expected commercial inspect");
    };

    assert_eq!(goods_sold_from_city, 0);
    assert!(goods_sold_from_outside > 0);
    assert_eq!(
        goods_sold_from_outside,
        game.view().unwrap().goods.goods_imported_from_outside
    );
}

#[test]
fn commercial_inspect_reports_neighbor_goods_route_when_border_connected() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();
    build_goods_producer(&game, RegionId(1));
    build_goods_consumer(&game, RegionId(2));

    for _ in 0..24 {
        game.tick_all_regions().unwrap();
    }

    let inspect = game.inspect_region(RegionId(2), 1, 0).unwrap();

    assert!(inspect.flags.contains(&InspectFlag::GoodsSupplyNeighbor));
    assert!(
        !inspect
            .explanations
            .iter()
            .any(|note| note.starts_with("Goods: nearest industrial route"))
    );
}

#[test]
fn commercial_inspect_keeps_unreachable_goods_route_without_border_link() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();
    build_goods_producer(&game, RegionId(1));
    build_goods_consumer_without_border_link(&game, RegionId(2));

    for _ in 0..24 {
        game.tick_all_regions().unwrap();
    }

    let inspect = game.inspect_region(RegionId(2), 1, 0).unwrap();

    assert!(inspect.flags.contains(&InspectFlag::GoodsSupplyMissing));
    assert!(
        !inspect
            .explanations
            .iter()
            .any(|note| note.starts_with("Goods: nearest industrial route"))
    );
}

#[test]
fn remote_spare_jobs_allow_connected_residential_population_growth() {
    let game = RegionalGame::two_region_default(4, 3).unwrap();
    build_connected_remote_job_fixture(&game);

    tick_region_for_one_day(&game, RegionId(1));

    let inspect = game.inspect_region(RegionId(1), 1, 1).unwrap();
    let Some(InspectDetailsView::Residential {
        population,
        job_assignments,
        ..
    }) = inspect.details
    else {
        panic!("expected residential inspect");
    };

    assert!(population > 0);
    let assignment = job_assignments
        .first()
        .copied()
        .expect("remote job assignment");
    assert_eq!(assignment.cell.region, RegionId(2));
    assert!(assignment.is_remote);
}

fn region_hours(game: &RegionalGame) -> Vec<u64> {
    game.view()
        .unwrap()
        .regions
        .into_iter()
        .map(|snapshot| snapshot.view.status.time.total_hours)
        .collect()
}

#[test]
fn tick_city_advances_every_region_to_the_same_hour() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();
    assert!(region_hours(&game).iter().all(|hours| *hours == 0));

    game.tick_city().unwrap();

    let hours = region_hours(&game);
    assert!(
        hours.iter().all(|h| *h == 1),
        "every region advances one hour: {hours:?}"
    );
    assert!(
        hours.windows(2).all(|pair| pair[0] == pair[1]),
        "all regions share one clock: {hours:?}"
    );
}

#[test]
fn city_driver_tick_advances_non_selected_regions_too() {
    // Regression: before the fix CityDriver::tick() ticked only the selected region,
    // so a non-selected region stayed frozen at hour 0 (divergent clocks).
    let mut driver = CityDriver::regional_multi_region().expect("regional driver");
    driver.tick();

    // The selected region (region 1) advanced.
    assert_eq!(driver.view().status.time.total_hours, 1);
    // A different region must have advanced too — not be frozen at 0.
    driver.select_next_region();
    assert_eq!(driver.view().status.time.total_hours, 1);
}

#[test]
fn remote_workers_at_lists_cross_region_commuters_on_the_workplace() {
    // Region-1 resident at (1,1) commutes to region-2 commercial at (1,2). The
    // workplace's own region holds no local worker there, so the only staff is the
    // remote commuter — invisible to the local-only roster but found by the reverse
    // lookup across regions.
    let game = RegionalGame::two_region_default(4, 3).unwrap();
    build_connected_remote_job_fixture(&game);
    tick_region_for_one_day(&game, RegionId(1));

    let workers = game.remote_workers_at(RegionId(2), 1, 2).unwrap();
    assert!(!workers.is_empty(), "remote commuter should be listed");
    assert!(workers.iter().all(|worker| matches!(
        worker.relation,
        CitizenRelation::LivesAt {
            region: Some(RegionId(1)),
            x: 1,
            y: 1
        }
    )));

    // The producer region is skipped (its own workers there would be Local, not
    // double-counted), and a non-workplace cell yields nothing.
    assert!(
        game.remote_workers_at(RegionId(2), 0, 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn remote_workers_show_on_every_footprint_cell_of_a_multicell_workplace() {
    // Region-1 resident commutes to region-2's workplace. After the workplace grows
    // to a multi-cell footprint, the remote commuter must appear on EVERY footprint
    // cell — not only the anchor cell the assignment records.
    let game = RegionalGame::two_region_default(4, 3).unwrap();
    build_connected_remote_job_fixture(&game);
    // Grow the region-2 commercial at (1,2) into a multi-cell building, then let the
    // commuter (re)assign to its current anchor.
    assert!(game.upgrade(RegionId(2), 1, 2).unwrap().success);
    tick_region_for_one_day(&game, RegionId(1));

    // Footprint cells = region-2 cells whose inspect reports Commercial details.
    let footprint: Vec<(usize, usize)> = (0..3)
        .flat_map(|y| (0..4).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            matches!(
                game.inspect_region(RegionId(2), x, y).unwrap().details,
                Some(InspectDetailsView::Commercial { .. })
            )
        })
        .collect();
    assert!(
        footprint.len() >= 2,
        "workplace should be multi-cell: {footprint:?}"
    );

    // The commuter must show on every footprint cell (the bug: only the anchor did).
    for &(x, y) in &footprint {
        let workers = game.remote_workers_at(RegionId(2), x, y).unwrap();
        assert!(
            !workers.is_empty(),
            "remote commuter missing on footprint cell ({x},{y})"
        );
    }
}

#[test]
fn inspect_uses_published_remote_jobs_before_region_ticks() {
    let game = RegionalGame::two_region_default(4, 3).unwrap();
    build_connected_remote_job_fixture(&game);

    let inspect = game.inspect_region(RegionId(1), 1, 1).unwrap();

    assert!(
        !inspect
            .explanations
            .iter()
            .any(|note| note.contains("no jobs are available"))
    );
}

#[test]
fn remote_spare_jobs_without_road_link_do_not_unlock_population_growth() {
    let game = RegionalGame::two_region_default(4, 3).unwrap();
    build_disconnected_remote_job_fixture(&game);

    tick_region_for_one_day(&game, RegionId(1));

    let inspect = game.inspect_region(RegionId(1), 1, 1).unwrap();
    let Some(InspectDetailsView::Residential { population, .. }) = inspect.details else {
        panic!("expected residential inspect");
    };

    assert_eq!(population, 0);
    assert!(inspect.flags.contains(&InspectFlag::GrowthBlockedNoJobs));
}

#[test]
fn bridged_remote_workplace_is_not_double_counted_for_population_growth() {
    let game = RegionalGame::two_region_default(6, 4).unwrap();
    build_bridge_workplace_double_count_fixture(&game);

    tick_region_for_one_day(&game, RegionId(1));

    let inspect = game.inspect_region(RegionId(1), 1, 1).unwrap();
    let Some(InspectDetailsView::Residential {
        population,
        job_assignments,
        ..
    }) = inspect.details
    else {
        panic!("expected residential inspect");
    };

    assert_eq!(population, 2);
    assert_eq!(job_assignments.len(), 2);
    assert!(
        job_assignments
            .iter()
            .all(|assignment| { assignment.cell.region == RegionId(2) && assignment.is_remote })
    );
}

#[test]
fn cross_region_commuter_goes_to_work_and_returns_home() {
    let game = RegionalGame::two_region_default(4, 3).unwrap();
    build_connected_remote_job_fixture(&game);

    tick_city_for_hours(&game, 24);
    let inspect = game.inspect_region(RegionId(1), 1, 1).unwrap();
    let Some(InspectDetailsView::Residential {
        job_assignments, ..
    }) = inspect.details
    else {
        panic!("expected residential inspect");
    };
    assert!(
        job_assignments
            .iter()
            .any(|assignment| assignment.cell.region == RegionId(2) && assignment.is_remote),
        "fixture should assign a region-1 resident to a region-2 job"
    );

    tick_city_for_hours(&game, 9);
    assert!(
        step_until_traveler_seen(&game, RegionId(1), 8),
        "commuter should walk from home to region-1 border"
    );
    assert!(
        step_until_traveler_seen(&game, RegionId(2), 8),
        "commuter should cross into region 2 and walk to work"
    );
    for _ in 0..3 {
        game.step_travel_city().unwrap();
    }

    tick_city_for_hours(&game, 6);
    assert!(
        step_until_traveler_seen(&game, RegionId(2), 8),
        "commuter should leave region-2 workplace after work"
    );
    assert!(
        step_until_traveler_seen(&game, RegionId(1), 8),
        "commuter should cross back into region 1"
    );
    for _ in 0..8 {
        game.step_travel_city().unwrap();
    }
    assert!(
        travelers_in_region(&game, RegionId(1)).is_empty()
            && travelers_in_region(&game, RegionId(2)).is_empty(),
        "after walking home, no road traveler dot should remain"
    );

    tick_city_for_hours(&game, 18);
    assert!(
        step_until_traveler_seen(&game, RegionId(1), 8),
        "next workday departure proves the home-away record was cleared"
    );
}

fn region_cell_powered(game: &RegionalGame, region: RegionId, x: usize, y: usize) -> bool {
    game.view()
        .unwrap()
        .regions
        .into_iter()
        .find(|snapshot| snapshot.region_id == region)
        .expect("region snapshot")
        .view
        .map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.powered)
        .unwrap_or(false)
}

fn travelers_in_region(
    game: &RegionalGame,
    region: RegionId,
) -> Vec<small_city::interface::view::CitizenTravelView> {
    game.view()
        .unwrap()
        .regions
        .into_iter()
        .find(|snapshot| snapshot.region_id == region)
        .expect("region snapshot")
        .view
        .travelers
}

fn step_until_traveler_seen(game: &RegionalGame, region: RegionId, max_steps: usize) -> bool {
    for _ in 0..max_steps {
        game.step_travel_city().unwrap();
        if !travelers_in_region(game, region).is_empty() {
            return true;
        }
    }
    false
}

#[test]
fn paused_build_keeps_imported_power_until_next_tick() {
    // Region 2 imports power from region 1's plant across the border.
    let game = RegionalGame::two_region_default(3, 2).unwrap();
    assert!(
        game.build(RegionId(1), 2, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 2, 1, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 0, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 1, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );
    assert!(game.tick_region(RegionId(2)).unwrap().success);
    assert!(
        region_cell_powered(&game, RegionId(2), 1, 0),
        "residential should be imported-powered after the tick"
    );

    // Paused build (no tick): the derived refresh must not drop the import.
    assert!(
        game.build(RegionId(2), 1, 1, BuildingKind::Park)
            .unwrap()
            .success
    );
    assert!(
        region_cell_powered(&game, RegionId(2), 1, 0),
        "imported power must survive a paused build until the next tick"
    );
}

#[test]
fn two_region_default_wires_topology_for_cross_region_power_export() {
    let game = RegionalGame::two_region_default(3, 2).unwrap();

    assert!(
        game.build(RegionId(1), 2, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 2, 1, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 0, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 1, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );

    assert!(game.tick_region(RegionId(2)).unwrap().success);

    let view = game
        .view()
        .unwrap()
        .regions
        .into_iter()
        .find(|snapshot| snapshot.region_id == RegionId(2))
        .expect("region 2 snapshot")
        .view;
    let powered = view
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 1 && cell.y == 0)
        .and_then(|cell| cell.powered)
        .unwrap_or(false);
    assert!(powered);
}

fn build_connected_remote_job_fixture(game: &RegionalGame) {
    build(game, RegionId(1), 3, 1, BuildingKind::Road);
    build(game, RegionId(1), 2, 1, BuildingKind::Road);
    build(game, RegionId(1), 1, 1, BuildingKind::Residential);

    build(game, RegionId(2), 0, 1, BuildingKind::Road);
    build(game, RegionId(2), 1, 1, BuildingKind::Road);
    build(game, RegionId(2), 1, 0, BuildingKind::PowerPlant);
    build(game, RegionId(2), 1, 2, BuildingKind::Commercial);
}

fn build_disconnected_remote_job_fixture(game: &RegionalGame) {
    build(game, RegionId(1), 2, 1, BuildingKind::Road);
    build(game, RegionId(1), 1, 1, BuildingKind::Residential);
    build(game, RegionId(1), 2, 0, BuildingKind::PowerPlant);

    build(game, RegionId(2), 0, 1, BuildingKind::Road);
    build(game, RegionId(2), 1, 1, BuildingKind::Road);
    build(game, RegionId(2), 1, 0, BuildingKind::PowerPlant);
    build(game, RegionId(2), 1, 2, BuildingKind::Commercial);
}

fn build_bridge_workplace_double_count_fixture(game: &RegionalGame) {
    build(game, RegionId(1), 5, 0, BuildingKind::Road);
    build(game, RegionId(1), 5, 1, BuildingKind::Road);
    build(game, RegionId(1), 5, 2, BuildingKind::Road);
    build(game, RegionId(1), 4, 1, BuildingKind::Road);
    build(game, RegionId(1), 3, 1, BuildingKind::Road);
    build(game, RegionId(1), 2, 1, BuildingKind::Road);
    build(game, RegionId(1), 2, 0, BuildingKind::Road);
    build(game, RegionId(1), 1, 0, BuildingKind::PowerPlant);
    build(game, RegionId(1), 1, 1, BuildingKind::Residential);
    build(game, RegionId(1), 0, 1, BuildingKind::Park);

    build(game, RegionId(2), 0, 0, BuildingKind::Road);
    build(game, RegionId(2), 1, 0, BuildingKind::Road);
    build(game, RegionId(2), 2, 0, BuildingKind::Road);
    build(game, RegionId(2), 3, 0, BuildingKind::Road);
    build(game, RegionId(2), 0, 2, BuildingKind::Road);
    build(game, RegionId(2), 1, 2, BuildingKind::Road);
    build(game, RegionId(2), 2, 2, BuildingKind::Road);
    build(game, RegionId(2), 3, 2, BuildingKind::Road);
    build(game, RegionId(2), 2, 1, BuildingKind::PowerPlant);
    build(game, RegionId(2), 1, 1, BuildingKind::Commercial);
    build(game, RegionId(2), 3, 1, BuildingKind::Commercial);
    build(game, RegionId(2), 5, 3, BuildingKind::Road);
    build(game, RegionId(2), 5, 2, BuildingKind::PowerPlant);
    build(game, RegionId(2), 4, 3, BuildingKind::Commercial);
}

fn build(game: &RegionalGame, region: RegionId, x: usize, y: usize, kind: BuildingKind) {
    assert!(game.build(region, x, y, kind).unwrap().success);
}

fn build_goods_producer(game: &RegionalGame, region: RegionId) {
    build(game, region, 2, 0, BuildingKind::Road);
    build(game, region, 1, 0, BuildingKind::Road);
    build(game, region, 1, 1, BuildingKind::Road);
    build(game, region, 0, 0, BuildingKind::Industrial);
    build(game, region, 0, 1, BuildingKind::PowerPlant);
}

fn build_goods_consumer(game: &RegionalGame, region: RegionId) {
    build(game, region, 0, 0, BuildingKind::Road);
    build(game, region, 0, 1, BuildingKind::Road);
    build(game, region, 1, 1, BuildingKind::Road);
    build(game, region, 2, 1, BuildingKind::Road);
    build(game, region, 1, 0, BuildingKind::Commercial);
    build(game, region, 0, 2, BuildingKind::Residential);
    build(game, region, 2, 2, BuildingKind::PowerPlant);
}

fn build_goods_consumer_without_border_link(game: &RegionalGame, region: RegionId) {
    build(game, region, 1, 1, BuildingKind::Road);
    build(game, region, 2, 1, BuildingKind::Road);
    build(game, region, 1, 0, BuildingKind::Commercial);
    build(game, region, 0, 2, BuildingKind::Residential);
    build(game, region, 2, 2, BuildingKind::PowerPlant);
}

fn tick_region_for_one_day(game: &RegionalGame, region: RegionId) {
    for _ in 0..24 {
        assert!(game.tick_region(region).unwrap().success);
    }
}

fn tick_city_for_hours(game: &RegionalGame, hours: usize) {
    for _ in 0..hours {
        assert!(game.tick_city().unwrap().success);
    }
}

#[test]
fn old_generic_imported_resource_note_is_not_shown() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();

    assert!(
        game.build(RegionId(1), 1, 1, BuildingKind::Park)
            .unwrap()
            .success
    );
    assert!(
        !has_generic_imported_resource_note(&game, RegionId(2)),
        "CR6 retires the visibility-only imported-resource cache and note"
    );
}

#[test]
fn road_builds_do_not_create_cross_region_imports() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();

    assert!(
        game.build(RegionId(1), 1, 1, BuildingKind::Road)
            .unwrap()
            .success
    );

    assert!(
        !has_generic_imported_resource_note(&game, RegionId(2)),
        "common road placement should not fan out regional resource offers"
    );
}
