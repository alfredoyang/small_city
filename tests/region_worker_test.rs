//! Integration tests for the shared single-threaded region worker.

use small_city::core::regional_types::{RegionCommand, UiRequestId};
use small_city::core::regions::continuation::{CallerContinuation, NeighborRequest};
use small_city::core::regions::runtime::{ImportedResourcePayload, RegionEvent, RegionRuntime};
use small_city::core::regions::worker::{RegionWorker, WorkerId, WorkerRoutingError};
use small_city::core::regions::{
    BorderEdge, ImportedResource, ImportedResourceResult, RegionId, RegionNeighborLink,
    RegionRoadNetworkId, RegionState, ResourceId, ResourceKind,
};
use small_city::interface::input::BuildingKind;

#[test]
fn one_worker_processes_events_for_multiple_regions() {
    let mut worker = worker_with_regions(WorkerId(1), &[RegionId(1), RegionId(2)]);
    worker
        .push_event(
            RegionId(1),
            RegionEvent::Tick {
                request_id: UiRequestId(1),
            },
        )
        .unwrap();
    worker
        .push_event(
            RegionId(2),
            RegionEvent::Tick {
                request_id: UiRequestId(2),
            },
        )
        .unwrap();

    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(summary.processed_regions, 2);
    assert_eq!(summary.tick_replies.len(), 2);
    assert_eq!(turn(&worker, RegionId(1)), 1);
    assert_eq!(turn(&worker, RegionId(2)), 1);
}

#[test]
fn busy_region_cannot_starve_another_region_when_event_limit_is_set() {
    let mut worker = worker_with_regions(WorkerId(2), &[RegionId(3), RegionId(4)]);
    worker.push_event(RegionId(3), tick(3)).unwrap();
    worker.push_event(RegionId(3), tick(4)).unwrap();
    worker.push_event(RegionId(3), tick(5)).unwrap();
    worker.push_event(RegionId(4), tick(6)).unwrap();

    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(summary.processed_regions, 2);
    assert_eq!(turn(&worker, RegionId(3)), 1);
    assert_eq!(turn(&worker, RegionId(4)), 1);
    assert!(pending_events(&worker, RegionId(3)) >= 2);
    assert!(pending_events(&worker, RegionId(4)) <= 1);
}

#[test]
fn returned_continuation_is_routed_to_caller_region_inbox() {
    let caller = RegionId(5);
    let target = RegionId(6);
    let mut worker = worker_with_regions(WorkerId(3), &[caller, target]);

    worker
        .push_event(
            target,
            RegionEvent::ProcessImportedResource(request(
                caller,
                resource(20, ResourceKind::ShoppingAccess, 1),
            )),
        )
        .unwrap();

    let first_pass = worker.process_region_events(1);

    assert!(first_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, caller), 1);
    assert!(
        worker
            .region(caller)
            .expect("caller")
            .state()
            .neighbor_import_results()
            .is_empty()
    );

    let second_pass = worker.process_region_events(1);

    assert!(second_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, caller), 0);
    assert_eq!(
        worker
            .region(caller)
            .expect("caller")
            .state()
            .neighbor_import_results()
            .len(),
        1
    );
}

#[test]
fn missing_target_region_produces_deterministic_routing_error() {
    let missing_caller = RegionId(7);
    let target = RegionId(8);
    let mut worker = worker_with_regions(WorkerId(4), &[target]);

    worker
        .push_event(
            target,
            RegionEvent::ProcessImportedResource(request(
                missing_caller,
                resource(21, ResourceKind::ParkAccess, 1),
            )),
        )
        .unwrap();

    let summary = worker.process_region_events(1);

    assert_eq!(
        summary.routing_errors,
        vec![WorkerRoutingError::MissingTargetRegion {
            target_region: missing_caller,
        }]
    );
    assert_eq!(pending_events(&worker, target), 0);
}

#[test]
fn add_region_rejects_duplicate_region_id() {
    let mut worker = RegionWorker::new(WorkerId(5));

    assert!(
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(9), 2, 2)))
            .is_ok()
    );
    let error = worker
        .add_region(RegionRuntime::new(RegionState::new(RegionId(9), 3, 3)))
        .expect_err("duplicate region should be rejected");

    assert_eq!(
        error.routing_error(),
        WorkerRoutingError::DuplicateRegion {
            region_id: RegionId(9),
        }
    );
    assert_eq!(error.into_runtime().region_id(), RegionId(9));
}

#[test]
fn process_region_events_with_zero_event_limit_reports_no_processed_regions() {
    let mut worker = worker_with_regions(WorkerId(6), &[RegionId(10), RegionId(11)]);
    worker.push_event(RegionId(10), tick(10)).unwrap();
    worker.push_event(RegionId(11), tick(11)).unwrap();

    let summary = worker.process_region_events(0);

    assert_eq!(summary.processed_regions, 0);
    assert!(summary.routing_errors.is_empty());
    assert_eq!(turn(&worker, RegionId(10)), 0);
    assert_eq!(turn(&worker, RegionId(11)), 0);
    assert_eq!(pending_events(&worker, RegionId(10)), 1);
    assert_eq!(pending_events(&worker, RegionId(11)), 1);
}

#[test]
fn worker_routes_export_change_to_neighbor_import_cache() {
    let source = RegionId(12);
    let target = RegionId(13);
    let mut worker = worker_with_regions(WorkerId(7), &[source, target]);

    worker
        .push_event(
            source,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::Park,
                },
            },
        )
        .unwrap();

    drain_worker(&mut worker);

    let imported = worker
        .region(target)
        .expect("target")
        .state()
        .imported_resources();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].id.origin_region, source);
    assert_eq!(imported[0].id.resource_kind, ResourceKind::ParkAccess);
    assert_eq!(imported[0].remaining_capacity, 1);
}

#[test]
fn worker_routes_export_removal_to_neighbor_import_cache() {
    let source = RegionId(14);
    let target = RegionId(15);
    let mut worker = worker_with_regions(WorkerId(8), &[source, target]);

    worker
        .push_event(
            source,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::Park,
                },
            },
        )
        .unwrap();
    drain_worker(&mut worker);
    assert_eq!(
        worker
            .region(target)
            .expect("target")
            .state()
            .imported_resources()
            .len(),
        1
    );

    worker
        .push_event(
            source,
            RegionEvent::RunCommand {
                request_id: UiRequestId(2),
                command: RegionCommand::Bulldoze { x: 1, y: 1 },
            },
        )
        .unwrap();
    drain_worker(&mut worker);

    assert!(
        worker
            .region(target)
            .expect("target")
            .state()
            .imported_resources()
            .is_empty()
    );
}

#[test]
fn discovery_joins_complementary_border_road_networks() {
    let left = region_with_roads(RegionId(16), 2, 1, &[(1, 0)]);
    let right = region_with_roads(RegionId(17), 2, 1, &[(0, 0)]);
    let worker = worker_with_region_states(WorkerId(9), vec![left, right]);

    let discovery = worker.cross_region_discovery(&[neighbor(16, BorderEdge::East, 17)]);

    assert_component(
        &discovery,
        network(16, 0),
        &[network(16, 0), network(17, 0)],
    );
}

#[test]
fn discovery_does_not_join_mismatched_border_offsets() {
    let left = region_with_roads(RegionId(18), 2, 2, &[(1, 0)]);
    let right = region_with_roads(RegionId(19), 2, 2, &[(0, 1)]);
    let worker = worker_with_region_states(WorkerId(10), vec![left, right]);

    let discovery = worker.cross_region_discovery(&[neighbor(18, BorderEdge::East, 19)]);

    assert_component(&discovery, network(18, 0), &[network(18, 0)]);
    assert_component(&discovery, network(19, 0), &[network(19, 0)]);
}

#[test]
fn discovery_keeps_one_regions_disconnected_networks_in_separate_components() {
    let left = region_with_roads(RegionId(20), 2, 5, &[(1, 1)]);
    let middle = region_with_roads(RegionId(21), 3, 5, &[(0, 1), (2, 3)]);
    let right = region_with_roads(RegionId(22), 2, 5, &[(0, 3)]);
    let worker = worker_with_region_states(WorkerId(11), vec![left, middle, right]);

    let discovery = worker.cross_region_discovery(&[
        neighbor(20, BorderEdge::East, 21),
        neighbor(21, BorderEdge::East, 22),
    ]);

    assert_component(
        &discovery,
        network(20, 0),
        &[network(20, 0), network(21, 0)],
    );
    assert_component(
        &discovery,
        network(21, 1),
        &[network(21, 1), network(22, 0)],
    );
}

#[test]
fn discovery_publishes_owned_availability_hints() {
    let mut source = RegionState::new(RegionId(23), 3, 2);
    assert!(source.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(source.build(0, 1, BuildingKind::Road).success);
    let worker = worker_with_region_states(WorkerId(12), vec![source]);

    let discovery = worker.cross_region_discovery(&[]);

    assert_eq!(discovery.availability_hints.len(), 1);
    assert_eq!(discovery.availability_hints[0].network, network(23, 0));
    assert!(discovery.availability_hints[0].has_spare_power);
    assert!(!discovery.availability_hints[0].has_spare_jobs);
}

#[test]
fn discovery_does_not_join_unrelated_regions_with_matching_border_links() {
    let left = region_with_roads(RegionId(24), 2, 1, &[(1, 0)]);
    let right = region_with_roads(RegionId(25), 2, 1, &[(0, 0)]);
    let worker = worker_with_region_states(WorkerId(13), vec![left, right]);

    let discovery = worker.cross_region_discovery(&[]);

    assert_component(&discovery, network(24, 0), &[network(24, 0)]);
    assert_component(&discovery, network(25, 0), &[network(25, 0)]);
}

#[test]
fn cross_region_power_export_powers_same_component_consumer() {
    let consumer = power_export_consumer_region(RegionId(26));
    let producer = power_export_producer_region(RegionId(27));
    let mut worker = worker_with_region_states(WorkerId(14), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(26, BorderEdge::East, 27)]);

    worker.push_event(RegionId(26), tick(1)).unwrap();
    drain_worker(&mut worker);

    assert!(cell_powered(&worker, RegionId(26), 0, 0));
}

#[test]
fn cross_region_power_export_does_not_cross_separate_components() {
    let consumer = power_export_consumer_region(RegionId(28));
    let producer = power_export_producer_region(RegionId(29));
    let mut worker = worker_with_region_states(WorkerId(15), vec![consumer, producer]);

    worker.push_event(RegionId(28), tick(1)).unwrap();
    drain_worker(&mut worker);

    assert!(!cell_powered(&worker, RegionId(28), 0, 0));
}

#[test]
fn cross_region_power_export_allocation_prevents_double_spend() {
    let first = power_export_consumer_region(RegionId(30));
    let second = power_export_consumer_region(RegionId(31));
    let producer = one_spare_power_producer_region(RegionId(32));
    let mut worker = worker_with_region_states(WorkerId(16), vec![first, second, producer]);
    worker.set_region_topology(vec![
        neighbor(30, BorderEdge::East, 32),
        neighbor(31, BorderEdge::East, 32),
    ]);

    worker.push_event(RegionId(30), tick(1)).unwrap();
    worker.push_event(RegionId(31), tick(2)).unwrap();
    drain_worker(&mut worker);

    let powered_consumers = [RegionId(30), RegionId(31)]
        .into_iter()
        .filter(|region| cell_powered(&worker, *region, 0, 0))
        .count();
    assert_eq!(powered_consumers, 1);
    assert!(cell_powered(&worker, RegionId(30), 0, 0));
    assert!(!cell_powered(&worker, RegionId(31), 0, 0));
}

#[test]
fn cross_region_power_export_resolves_before_population_growth() {
    let consumer = power_export_growth_region(RegionId(33));
    let producer = power_export_producer_region(RegionId(34));
    let mut worker = worker_with_region_states(WorkerId(17), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(33, BorderEdge::East, 34)]);

    for request_id in 1..=48 {
        worker.push_event(RegionId(33), tick(request_id)).unwrap();
        worker
            .push_event(RegionId(34), tick(request_id + 100))
            .unwrap();
        drain_worker(&mut worker);
    }

    assert!(
        worker
            .region(RegionId(33))
            .expect("consumer")
            .state()
            .view()
            .status
            .population
            > 0
    );
}

#[test]
fn cross_region_power_export_does_not_overwrite_a_paused_tick() {
    let consumer = power_export_consumer_region(RegionId(35));
    let producer = power_export_producer_region(RegionId(36));
    let mut worker = worker_with_region_states(WorkerId(18), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(35, BorderEdge::East, 36)]);

    worker.push_event(RegionId(35), tick(1)).unwrap();
    worker.push_event(RegionId(35), tick(2)).unwrap();
    let mut tick_replies = worker.process_region_events(2).tick_replies;

    for _ in 0..16 {
        let summary = worker.process_region_events(2);
        tick_replies.extend(summary.tick_replies);
        if summary.processed_regions == 0 {
            break;
        }
    }

    let reply_ids = tick_replies
        .into_iter()
        .map(|reply| reply.request_id)
        .collect::<Vec<_>>();
    assert_eq!(reply_ids, vec![UiRequestId(1), UiRequestId(2)]);
}

#[test]
fn paused_region_can_still_process_producer_side_power_requests() {
    let caller = power_export_consumer_region(RegionId(37));
    let middle = power_consumer_and_exporter_region(RegionId(38));
    let upstream = power_export_producer_region(RegionId(39));
    let mut worker = worker_with_region_states(WorkerId(19), vec![middle, caller, upstream]);
    worker.set_region_topology(vec![
        neighbor(37, BorderEdge::East, 38),
        neighbor(38, BorderEdge::East, 39),
    ]);

    worker.push_event(RegionId(38), tick(1)).unwrap();
    worker.push_event(RegionId(37), tick(2)).unwrap();
    drain_worker(&mut worker);

    assert!(cell_powered(&worker, RegionId(37), 0, 0));
    assert!(cell_powered(&worker, RegionId(38), 2, 0));
}

#[test]
fn repeated_selected_region_export_does_not_consume_stale_producer_allocation() {
    let consumer = power_export_consumer_region(RegionId(40));
    let producer = one_spare_power_producer_region(RegionId(41));
    let mut worker = worker_with_region_states(WorkerId(20), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(40, BorderEdge::East, 41)]);

    worker.push_event(RegionId(40), tick(1)).unwrap();
    drain_worker(&mut worker);
    assert!(cell_powered(&worker, RegionId(40), 0, 0));

    worker.push_event(RegionId(40), tick(2)).unwrap();
    drain_worker(&mut worker);
    assert!(cell_powered(&worker, RegionId(40), 0, 0));
}

#[test]
fn caller_tick_without_export_request_releases_previous_producer_allocation() {
    let first = power_export_consumer_region(RegionId(42));
    let second = power_export_consumer_region(RegionId(43));
    let producer = one_spare_power_producer_region(RegionId(44));
    let mut worker = worker_with_region_states(WorkerId(21), vec![first, second, producer]);
    worker.set_region_topology(vec![
        neighbor(42, BorderEdge::East, 44),
        neighbor(43, BorderEdge::East, 44),
    ]);

    worker.push_event(RegionId(42), tick(1)).unwrap();
    drain_worker(&mut worker);
    assert!(cell_powered(&worker, RegionId(42), 0, 0));

    worker.push_event(RegionId(43), tick(2)).unwrap();
    drain_worker(&mut worker);
    assert!(!cell_powered(&worker, RegionId(43), 0, 0));

    worker
        .push_event(
            RegionId(42),
            RegionEvent::RunCommand {
                request_id: UiRequestId(3),
                command: RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::PowerPlant,
                },
            },
        )
        .unwrap();
    drain_worker(&mut worker);
    worker.push_event(RegionId(42), tick(4)).unwrap();
    drain_worker(&mut worker);

    worker.push_event(RegionId(43), tick(5)).unwrap();
    drain_worker(&mut worker);
    assert!(cell_powered(&worker, RegionId(43), 0, 0));
}

#[test]
fn same_pass_release_is_routed_before_another_caller_power_request() {
    let first = power_export_consumer_region(RegionId(45));
    let second = power_export_consumer_region(RegionId(46));
    let producer = one_spare_power_producer_region(RegionId(47));
    let mut worker = worker_with_region_states(WorkerId(22), vec![second, first, producer]);
    worker.set_region_topology(vec![
        neighbor(45, BorderEdge::East, 47),
        neighbor(46, BorderEdge::East, 47),
    ]);

    worker.push_event(RegionId(45), tick(1)).unwrap();
    drain_worker(&mut worker);
    assert!(cell_powered(&worker, RegionId(45), 0, 0));

    worker
        .push_event(
            RegionId(45),
            RegionEvent::RunCommand {
                request_id: UiRequestId(2),
                command: RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::PowerPlant,
                },
            },
        )
        .unwrap();
    drain_worker(&mut worker);

    worker.push_event(RegionId(46), tick(3)).unwrap();
    worker.push_event(RegionId(45), tick(4)).unwrap();
    drain_worker(&mut worker);

    assert!(cell_powered(&worker, RegionId(45), 0, 0));
    assert!(cell_powered(&worker, RegionId(46), 0, 0));
}

#[test]
fn cross_region_job_export_employs_jobless_citizen() {
    let consumer = job_seeker_region(RegionId(60));
    let producer = job_slot_producer_region(RegionId(61));
    let mut worker = worker_with_region_states(WorkerId(40), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(60, BorderEdge::East, 61)]);

    run_job_growth_days(&mut worker, RegionId(60), RegionId(61), 10);

    let imported = imported_job_count(&worker, RegionId(60));
    assert!(
        imported >= 1,
        "a jobless citizen should import a remote job"
    );
    // The recorded reference is owned data pointing at the producer region.
    let slots = worker
        .region(RegionId(60))
        .expect("consumer")
        .state()
        .imported_job_slots();
    assert!(
        slots
            .iter()
            .all(|(region, _slot_id)| *region == RegionId(61))
    );
}

#[test]
fn cross_region_job_export_does_not_cross_separate_components() {
    let consumer = job_seeker_region(RegionId(62));
    let producer = job_slot_producer_region(RegionId(63));
    // No topology: the regions are in separate components (the trap).
    let mut worker = worker_with_region_states(WorkerId(41), vec![consumer, producer]);

    run_job_growth_days(&mut worker, RegionId(62), RegionId(63), 10);

    assert_eq!(imported_job_count(&worker, RegionId(62)), 0);
}

#[test]
fn cross_region_job_export_reservation_prevents_double_spend() {
    // The producer has two spare commercial slots; the consumer grows three
    // jobless citizens. Only two may import a job: no slot is granted twice.
    let consumer = job_seeker_region(RegionId(64));
    let producer = limited_job_slot_producer_region(RegionId(65));
    let mut worker = worker_with_region_states(WorkerId(42), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(64, BorderEdge::East, 65)]);

    run_job_growth_days(&mut worker, RegionId(64), RegionId(65), 12);

    assert_eq!(imported_job_count(&worker, RegionId(64)), 2);
}

#[test]
fn cross_region_job_export_tax_accrues_to_exporter_without_remote_entity() {
    // Same producer run twice: once connected (exports a job) and once isolated
    // (no export). The connected producer must end richer by the exported job's
    // workplace tax, and the consumer stores only an owned slot reference.
    let mut connected = worker_with_region_states(
        WorkerId(43),
        vec![
            job_seeker_region(RegionId(66)),
            job_slot_producer_region(RegionId(67)),
        ],
    );
    connected.set_region_topology(vec![neighbor(66, BorderEdge::East, 67)]);
    run_job_growth_days(&mut connected, RegionId(66), RegionId(67), 10);

    let mut isolated = worker_with_region_states(
        WorkerId(44),
        vec![
            job_seeker_region(RegionId(66)),
            job_slot_producer_region(RegionId(67)),
        ],
    );
    run_job_growth_days(&mut isolated, RegionId(66), RegionId(67), 10);

    assert!(imported_job_count(&connected, RegionId(66)) >= 1);
    assert_eq!(imported_job_count(&isolated, RegionId(66)), 0);
    assert!(
        region_money(&connected, RegionId(67)) > region_money(&isolated, RegionId(67)),
        "exported job workplace tax should accrue to the producer region"
    );
}

#[test]
fn tick_short_on_power_and_jobs_resolves_both_phases() {
    // The consumer imports power (its residential network has no local plant) and,
    // once powered, grows citizens that are locally jobless and import a job. Both
    // export phases must resolve in the same daily ticks.
    let consumer = power_and_job_seeker_region(RegionId(68));
    let producer = power_and_job_producer_region(RegionId(69));
    let mut worker = worker_with_region_states(WorkerId(45), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(68, BorderEdge::East, 69)]);

    run_job_growth_days(&mut worker, RegionId(68), RegionId(69), 14);

    assert!(
        cell_powered(&worker, RegionId(68), 0, 0),
        "residential should be powered by imported power"
    );
    assert!(
        imported_job_count(&worker, RegionId(68)) >= 1,
        "a jobless citizen should import a remote job in the same run"
    );
}

fn worker_with_regions(id: WorkerId, regions: &[RegionId]) -> RegionWorker {
    let mut worker = RegionWorker::new(id);
    for region_id in regions {
        worker
            .add_region(RegionRuntime::new(RegionState::new(*region_id, 2, 2)))
            .unwrap();
    }
    worker
}

fn power_export_consumer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 2, 2);
    assert!(region.build(0, 0, BuildingKind::Residential).success);
    assert!(region.build(1, 0, BuildingKind::Road).success);
    region
}

fn power_export_growth_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 6, 3);
    assert!(region.build(0, 0, BuildingKind::Residential).success);
    assert!(region.build(2, 1, BuildingKind::Commercial).success);
    assert!(region.build(0, 1, BuildingKind::Park).success);
    for x in 1..=5 {
        assert!(region.build(x, 0, BuildingKind::Road).success);
    }
    region
}

fn power_export_producer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 2, 2);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
    region
}

fn power_consumer_and_exporter_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 4, 2);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
    assert!(region.build(2, 0, BuildingKind::Residential).success);
    assert!(region.build(3, 0, BuildingKind::Road).success);
    region
}

fn one_spare_power_producer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 5, 2);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Road).success);
    assert!(region.build(2, 0, BuildingKind::Road).success);
    assert!(region.build(3, 0, BuildingKind::Road).success);
    assert!(region.build(4, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
    assert!(region.build(1, 1, BuildingKind::Industrial).success);
    assert!(region.build(2, 1, BuildingKind::Industrial).success);
    assert!(region.build(3, 1, BuildingKind::Industrial).success);
    region
}

// A residential on a locally-powered border network whose only local workplace
// sits on a SEPARATE (unreachable) road network. Jobs are counted so population
// grows, but those citizens cannot reach a local slot and seek a remote one east.
fn job_seeker_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 6, 3);
    assert!(region.build(0, 0, BuildingKind::Residential).success);
    assert!(region.build(0, 1, BuildingKind::Park).success);
    for x in 1..=5 {
        assert!(region.build(x, 0, BuildingKind::Road).success);
    }
    // One plant powers both networks (adjacent to a road in each).
    assert!(region.build(4, 1, BuildingKind::PowerPlant).success);
    assert!(region.build(3, 2, BuildingKind::Road).success);
    assert!(region.build(4, 2, BuildingKind::Road).success);
    assert!(region.build(5, 2, BuildingKind::Industrial).success);
    region
}

// Producer with three spare industrial slots reachable from the west border.
fn job_slot_producer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 2, 2);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::Industrial).success);
    assert!(region.build(1, 1, BuildingKind::PowerPlant).success);
    region
}

// Producer with only two spare commercial slots, for the double-spend test.
fn limited_job_slot_producer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 2, 2);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::Commercial).success);
    assert!(region.build(1, 1, BuildingKind::PowerPlant).success);
    region
}

// Like `job_seeker_region` but its residential network has NO local plant, so it
// must import power before it can grow the citizens that then import jobs.
fn power_and_job_seeker_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 6, 3);
    assert!(region.build(0, 0, BuildingKind::Residential).success);
    assert!(region.build(0, 1, BuildingKind::Park).success);
    for x in 1..=5 {
        assert!(region.build(x, 0, BuildingKind::Road).success);
    }
    // The job-count network on row 2 is powered locally; the residential network
    // on row 0 has no plant and imports power from the east neighbor.
    assert!(region.build(1, 2, BuildingKind::Industrial).success);
    assert!(region.build(2, 2, BuildingKind::Road).success);
    assert!(region.build(3, 2, BuildingKind::Road).success);
    assert!(region.build(4, 2, BuildingKind::PowerPlant).success);
    region
}

// Producer that exports both spare power and spare job slots from the west border.
fn power_and_job_producer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 2, 2);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::Industrial).success);
    assert!(region.build(1, 1, BuildingKind::PowerPlant).success);
    region
}

fn run_job_growth_days(
    worker: &mut RegionWorker,
    consumer: RegionId,
    producer: RegionId,
    days: u64,
) {
    // Twenty-four hourly ticks per day cross the daily boundary where population
    // grows and jobs (local then remote) resolve.
    for request_id in 1..=(days * 24) {
        worker.push_event(consumer, tick(request_id)).unwrap();
        worker
            .push_event(producer, tick(request_id + 100_000))
            .unwrap();
        drain_worker(worker);
    }
}

fn imported_job_count(worker: &RegionWorker, region_id: RegionId) -> usize {
    worker
        .region(region_id)
        .expect("region")
        .state()
        .imported_job_count()
}

fn region_money(worker: &RegionWorker, region_id: RegionId) -> i32 {
    worker
        .region(region_id)
        .expect("region")
        .state()
        .view()
        .status
        .money
}

fn cell_powered(worker: &RegionWorker, region_id: RegionId, x: usize, y: usize) -> bool {
    worker
        .region(region_id)
        .expect("region")
        .state()
        .view()
        .map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.powered)
        .unwrap_or(false)
}

fn worker_with_region_states(id: WorkerId, regions: Vec<RegionState>) -> RegionWorker {
    let mut worker = RegionWorker::new(id);
    for region in regions {
        worker.add_region(RegionRuntime::new(region)).unwrap();
    }
    worker
}

fn assert_component(
    discovery: &small_city::core::regions::worker::CrossRegionDiscovery,
    member: RegionRoadNetworkId,
    expected: &[RegionRoadNetworkId],
) {
    assert_eq!(discovery.component_of(member), Some(expected));
}

fn region_with_roads(
    region_id: RegionId,
    width: usize,
    height: usize,
    roads: &[(usize, usize)],
) -> RegionState {
    let mut region = RegionState::new(region_id, width, height);
    for (x, y) in roads {
        assert!(region.build(*x, *y, BuildingKind::Road).success);
    }
    region
}

fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
    RegionRoadNetworkId {
        region: RegionId(region),
        road_network,
    }
}

fn neighbor(region: u32, edge: BorderEdge, neighbor: u32) -> RegionNeighborLink {
    RegionNeighborLink::new(RegionId(region), edge, RegionId(neighbor))
}

fn tick(request_id: u64) -> RegionEvent {
    RegionEvent::Tick {
        request_id: UiRequestId(request_id),
    }
}

fn request(
    caller_region: RegionId,
    resource: ImportedResource,
) -> NeighborRequest<ImportedResourcePayload, ImportedResourceResult> {
    NeighborRequest {
        payload: ImportedResourcePayload {
            resource,
            local_used_capacity: 0,
            border_crossing_cost: 1,
            target_neighbors: Vec::new(),
        },
        continuation: CallerContinuation::new(caller_region, |region, result| {
            region.apply_neighbor_import_result(result);
        }),
    }
}

fn turn(worker: &RegionWorker, region_id: RegionId) -> u32 {
    worker
        .region(region_id)
        .expect("region")
        .state()
        .view()
        .status
        .turn
}

fn pending_events(worker: &RegionWorker, region_id: RegionId) -> usize {
    worker
        .region(region_id)
        .expect("region")
        .pending_event_count()
}

fn drain_worker(worker: &mut RegionWorker) {
    for _ in 0..16 {
        if worker.process_region_events(1).processed_regions == 0 {
            return;
        }
    }

    panic!("worker did not drain");
}

fn resource(origin_region: u32, resource_kind: ResourceKind, generation: u64) -> ImportedResource {
    ImportedResource {
        id: ResourceId {
            origin_region: RegionId(origin_region),
            resource_kind,
            generation,
        },
        remaining_capacity: 5,
        hop_count: 0,
        max_hops: 2,
        travel_cost: 0,
        source_neighbor: RegionId(origin_region),
    }
}
