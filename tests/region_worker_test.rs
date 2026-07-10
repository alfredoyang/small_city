//! Integration tests for the shared single-threaded region worker.

use small_city::core::regional_types::{RegionCommand, RegionTickResponse, UiRequestId};
use small_city::core::regions::directory::RegionDirectory;
use small_city::core::regions::runtime::{
    ExportAllocationRequest, JobExportRequest, PowerExportRequest, RegionEvent, RegionRuntime,
};
use small_city::core::regions::worker::{
    RegionOwnerDirectory, RegionWorker, WorkerId, WorkerRoutingError,
    process_workers_with_deterministic_barrier,
};
use small_city::core::regions::{
    BorderEdge, BorderLinkId, JobExportGrant, NetworkBorderLink, PowerExportGrant, RegionId,
    RegionNeighborLink, RegionRoadNetworkId, RegionState, RegionalAvailabilityHint,
};
use small_city::interface::events::{EconomyBreakdownView, GameEventView};
use small_city::interface::input::BuildingKind;
use small_city::interface::view::{CitizenRelation, InspectDetailsView};
use std::sync::Arc;

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
fn add_region_rejects_duplicate_region_id() {
    let mut worker = test_worker(WorkerId(5));

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
    assert!(
        discovery.availability_hints[0]
            .spare_job_slot_ids
            .is_empty()
    );
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
fn discovery_reflects_authoritative_road_state_after_build_and_bulldoze() {
    let left = RegionId(75);
    let right = RegionId(76);
    let mut worker = worker_with_region_states(
        WorkerId(49),
        vec![RegionState::new(left, 2, 1), RegionState::new(right, 2, 1)],
    );
    let topology = vec![neighbor(75, BorderEdge::East, 76)];
    worker.set_region_topology(topology.clone());

    assert!(
        worker
            .cross_region_discovery(&topology)
            .component_of(network(75, 0))
            .is_none()
    );

    worker
        .push_event(
            left,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 1,
                    y: 0,
                    kind: BuildingKind::Road,
                },
            },
        )
        .unwrap();
    worker
        .push_event(
            right,
            RegionEvent::RunCommand {
                request_id: UiRequestId(2),
                command: RegionCommand::Build {
                    x: 0,
                    y: 0,
                    kind: BuildingKind::Road,
                },
            },
        )
        .unwrap();
    drain_worker(&mut worker);

    let connected = worker.cross_region_discovery(&topology);
    assert_component(
        &connected,
        network(75, 0),
        &[network(75, 0), network(76, 0)],
    );

    worker
        .push_event(
            right,
            RegionEvent::RunCommand {
                request_id: UiRequestId(3),
                command: RegionCommand::Bulldoze { x: 0, y: 0 },
            },
        )
        .unwrap();
    drain_worker(&mut worker);

    let after_bulldoze = worker.cross_region_discovery(&topology);
    assert_component(&after_bulldoze, network(75, 0), &[network(75, 0)]);
    assert!(after_bulldoze.component_of(network(76, 0)).is_none());
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
fn power_grant_continuation_runs_in_caller_region() {
    // Retire-tickstate, P-a: the tick completes in the SAME pass it asks
    // for exported power (no more pause), so `RegionTickCompleted` shows up
    // in `request_pass` now, not `apply_pass` -- applying a same-generation
    // grant later is a silent ECS write with no further tick completion.
    let caller = RegionId(70);
    let producer = RegionId(71);
    let consumer = power_export_consumer_region(caller);
    let producer_region = power_export_producer_region(producer);
    let mut worker = worker_with_region_states(WorkerId(46), vec![consumer, producer_region]);
    worker.set_region_topology(vec![neighbor(70, BorderEdge::East, 71)]);

    worker.push_event(caller, tick(1)).unwrap();

    let request_pass = worker.process_region_events(1);
    assert!(request_pass.routing_errors.is_empty());
    assert!(!cell_powered(&worker, caller, 0, 0));
    assert_eq!(pending_events(&worker, producer), 1);
    assert_eq!(
        request_pass.tick_replies.len(),
        1,
        "the tick completes in the same pass it asks for exported power"
    );
    assert_eq!(turn(&worker, caller), 1);

    let producer_pass = worker.process_region_events(1);
    assert!(producer_pass.routing_errors.is_empty());
    assert!(!cell_powered(&worker, caller, 0, 0));
    assert_eq!(pending_events(&worker, producer), 0);
    assert_eq!(pending_events(&worker, caller), 1);

    let apply_pass = worker.process_region_events(1);
    assert!(apply_pass.routing_errors.is_empty());
    assert!(cell_powered(&worker, caller, 0, 0));
    assert_eq!(
        apply_pass.tick_replies.len(),
        0,
        "applying the grant is a silent ECS write, not a second tick completion"
    );
    assert_eq!(
        turn(&worker, producer),
        0,
        "producer must not run the caller's tick"
    );
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
fn stale_spare_power_hint_routes_to_producer_but_denies_cleanly() {
    let caller = RegionId(80);
    let producer = RegionId(81);
    let directory = Arc::new(RegionDirectory::new(vec![neighbor(
        80,
        BorderEdge::East,
        81,
    )]));
    let mut worker = test_worker_with_directory(WorkerId(52), Arc::clone(&directory));
    worker
        .add_region(RegionRuntime::new(power_export_consumer_region(caller)))
        .unwrap();
    worker
        .add_region(RegionRuntime::new(region_with_roads(
            producer,
            2,
            1,
            &[(0, 0)],
        )))
        .unwrap();
    // Event-driven plan P-1: a freshly constructed region starts with
    // hints_dirty=true (every attach_* during setup sets it) and the worker's
    // event-idle sweep republishes any owned region with hints_dirty set on
    // its very next pass, regardless of pending events. Prime both regions
    // with a no-op pass first so that sweep runs and clears the flag on their
    // accurate state, THEN overwrite the directory with the deliberately
    // stale hint below — otherwise the sweep would immediately correct
    // producer's entry before `request_pass` ever reads it, defeating the
    // staleness this test exercises.
    assert_eq!(worker.process_region_events(1).processed_regions, 0);
    directory.publish_region(
        producer,
        vec![NetworkBorderLink {
            network: network(81, 0),
            link: BorderLinkId {
                edge: BorderEdge::West,
                offset: 0,
            },
        }],
        vec![RegionalAvailabilityHint {
            network: network(81, 0),
            has_spare_power: true,
            spare_job_slot_ids: Vec::new(),
            spare_goods_units: 0,
        }],
    );

    worker.push_event(caller, tick(1)).unwrap();

    let request_pass = worker.process_region_events(1);
    assert!(request_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, producer), 1);
    // Retire-tickstate, P-a: the tick completes in this same pass now, not
    // when the (denied) reply later applies.
    assert_eq!(request_pass.tick_replies.len(), 1);
    assert_eq!(turn(&worker, caller), 1);

    let producer_pass = worker.process_region_events(1);
    assert!(producer_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, caller), 1);

    let apply_pass = worker.process_region_events(1);
    assert!(apply_pass.routing_errors.is_empty());
    assert_eq!(
        apply_pass.tick_replies.len(),
        0,
        "a denial applying late is silent, not a second tick completion"
    );
    assert!(!cell_powered(&worker, caller, 0, 0));
    assert_eq!(turn(&worker, producer), 0);
}

#[test]
fn quiet_power_tick_skips_reconcile_after_first_grant() {
    // Event-driven plan, P-2: once a consumer's import is granted and nothing
    // local or cross-region changes afterward, its NEXT tick's power gate must
    // stay quiet — no release, no fresh request — even though the import
    // still needs to keep working every tick. This is the positive proof for
    // the property `tick_without_exportable_demand_finishes_immediately` (a
    // unit test on a bare, empty `RegionRuntime`) can't cover: a REAL,
    // ongoing cross-region import, routed through a real worker + directory,
    // that keeps its grant without a repeated round trip.
    let caller = RegionId(82);
    let producer = RegionId(83);
    let directory = Arc::new(RegionDirectory::new(vec![neighbor(
        82,
        BorderEdge::East,
        83,
    )]));
    let mut worker = test_worker_with_directory(WorkerId(62), Arc::clone(&directory));
    worker
        .add_region(RegionRuntime::new(power_export_consumer_region(caller)))
        .unwrap();
    worker
        .add_region(RegionRuntime::new(power_export_producer_region(producer)))
        .unwrap();

    worker.push_event(caller, tick(1)).unwrap();
    for _ in 0..3 {
        assert!(worker.process_region_events(1).routing_errors.is_empty());
    }
    assert!(
        cell_powered(&worker, caller, 0, 0),
        "setup: first tick's dirty reconcile must grant the import"
    );
    assert_eq!(turn(&worker, caller), 1);

    // Second tick, nothing changed anywhere: the gate must go quiet. Same-
    // worker routing is immediate (`route_region_event`), so a request would
    // show up as a pending event on the producer within this single pass.
    worker.push_event(caller, tick(2)).unwrap();
    let second_pass = worker.process_region_events(1);
    assert!(second_pass.routing_errors.is_empty());
    assert_eq!(
        second_pass.tick_replies.len(),
        1,
        "tick must still complete"
    );
    assert_eq!(
        pending_events(&worker, producer),
        0,
        "quiet tick must not re-request power from the producer"
    );
    assert!(
        cell_powered(&worker, caller, 0, 0),
        "the existing grant must persist untouched through a quiet tick"
    );
    assert_eq!(turn(&worker, caller), 2);
}

#[test]
fn eager_nudge_powers_neighbor_before_its_own_first_tick() {
    // Retire-tickstate, P-b: a neighbor is nudged the instant a hint
    // republishes, instead of waiting for its own next tick's discovery-
    // generation gate to notice. Prove the common-case payoff directly: the
    // consumer never receives a single Tick, yet ends up powered once the
    // producer's hint changes and the fan-out + request/grant round trip
    // settles.
    let consumer = RegionId(92);
    let producer = RegionId(93);
    let mut worker = worker_with_region_states(
        WorkerId(63),
        vec![
            power_export_consumer_region(consumer),
            // Same shape as `power_export_producer_region`, minus the plant:
            // the producer starts with no spare power at all.
            region_with_roads(producer, 2, 2, &[(0, 0)]),
        ],
    );
    worker.set_region_topology(vec![neighbor(92, BorderEdge::East, 93)]);

    // Prime a no-op pass so the initial (no-spare-power) hints settle, same
    // setup as `stale_spare_power_hint_routes_to_producer_but_denies_cleanly`.
    assert_eq!(worker.process_region_events(1).processed_regions, 0);
    assert_eq!(turn(&worker, consumer), 0);

    // The producer builds a power plant -- its availability hint flips
    // false -> true. This alone must fan out a PowerCapacityRecheck to the
    // consumer (the connected neighbor), with no Tick involved anywhere.
    worker
        .push_event(
            producer,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 0,
                    y: 1,
                    kind: BuildingKind::PowerPlant,
                },
            },
        )
        .unwrap();
    for _ in 0..8 {
        assert!(worker.process_region_events(1).routing_errors.is_empty());
    }

    assert!(
        cell_powered(&worker, consumer, 0, 0),
        "the nudge should have powered the consumer"
    );
    assert_eq!(
        turn(&worker, consumer),
        0,
        "the consumer must never have run its own tick to get here"
    );
}

#[test]
fn eager_nudge_does_not_refire_on_an_unchanged_pass() {
    // Retire-tickstate, P-b: the fan-out is gated on `publish_region`'s own
    // idempotence check. Once a real change has been nudged and settled,
    // a further pass where nothing actually changed must not re-nudge --
    // no repeated request/release traffic, nothing left to process.
    let consumer = RegionId(94);
    let producer = RegionId(95);
    let mut worker = worker_with_region_states(
        WorkerId(64),
        vec![
            power_export_consumer_region(consumer),
            region_with_roads(producer, 2, 2, &[(0, 0)]),
        ],
    );
    worker.set_region_topology(vec![neighbor(94, BorderEdge::East, 95)]);
    assert_eq!(worker.process_region_events(1).processed_regions, 0);

    // A real change: the producer builds a power plant, nudging the
    // consumer. Drain to a fixed point.
    worker
        .push_event(
            producer,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 0,
                    y: 1,
                    kind: BuildingKind::PowerPlant,
                },
            },
        )
        .unwrap();
    for _ in 0..8 {
        assert!(worker.process_region_events(1).routing_errors.is_empty());
    }
    assert!(
        cell_powered(&worker, consumer, 0, 0),
        "setup: nudge settled"
    );

    // Nothing changes now. A further no-op pass must produce zero
    // additional pending events anywhere -- no repeated nudge, no repeated
    // request/release traffic.
    for _ in 0..3 {
        let summary = worker.process_region_events(1);
        assert!(summary.routing_errors.is_empty());
        assert_eq!(summary.processed_regions, 0, "nothing left to process");
    }
}

#[test]
fn event_idle_region_republishes_dirty_hints_on_next_pass() {
    // P-1 (event-driven plan): a region's hints_dirty flag is set by every
    // attach_* chokepoint, including the ones `RegionState::build` runs during
    // test setup — and `add_region`'s one-time initial publish does NOT clear
    // it, so the flag stays true even though the hint it just published is
    // accurate. Simulate the directory drifting out of sync afterward (as if
    // published by a stale process) and confirm the worker's event-idle sweep
    // self-corrects it on the very next pass, even though this region never
    // receives a single event. Before P-1, only regions with
    // `pending_event_count() > 0` republished, so this correction would never
    // happen — the stale, manually-corrupted entry would persist forever.
    let region_id = RegionId(91);
    let mut region = RegionState::new(region_id, 3, 2);
    assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(region.build(0, 1, BuildingKind::Road).success);
    let directory = Arc::new(RegionDirectory::new(Vec::new()));
    let mut worker = test_worker_with_directory(WorkerId(61), Arc::clone(&directory));
    worker.add_region(RegionRuntime::new(region)).unwrap();

    // Corrupt the directory as if a stale process published the wrong hint.
    let links = worker
        .region(region_id)
        .unwrap()
        .state()
        .network_border_links();
    directory.publish_region(
        region_id,
        links,
        vec![RegionalAvailabilityHint {
            network: network(91, 0),
            has_spare_power: false,
            spare_job_slot_ids: Vec::new(),
            spare_goods_units: 0,
        }],
    );
    assert!(!directory.discovery_snapshot().availability_hints[0].has_spare_power);

    // No event pushed anywhere: the region is event-idle this pass.
    let summary = worker.process_region_events(1);
    assert_eq!(
        summary.processed_regions, 0,
        "region had zero pending events"
    );

    let discovery = directory.discovery_snapshot();
    assert!(
        discovery.availability_hints[0].has_spare_power,
        "event-idle sweep must self-correct the region's real (accurate) hint"
    );
}

#[test]
fn cross_worker_power_export_routes_through_deterministic_barrier() {
    let directory = Arc::new(RegionDirectory::new(vec![neighbor(
        82,
        BorderEdge::East,
        83,
    )]));
    let owners = Arc::new(RegionOwnerDirectory::new());
    let mut consumer_worker = RegionWorker::with_directory_and_owners(
        WorkerId(53),
        Arc::clone(&directory),
        Arc::clone(&owners),
    );
    let mut producer_worker = RegionWorker::with_directory_and_owners(
        WorkerId(54),
        Arc::clone(&directory),
        Arc::clone(&owners),
    );
    consumer_worker
        .add_region(RegionRuntime::new(power_export_consumer_region(RegionId(
            82,
        ))))
        .unwrap();
    producer_worker
        .add_region(RegionRuntime::new(power_export_producer_region(RegionId(
            83,
        ))))
        .unwrap();

    consumer_worker.push_event(RegionId(82), tick(1)).unwrap();

    // Retire-tickstate, P-a: the tick's own RegionTickCompleted now arrives
    // on the FIRST pass (no more pause) -- it no longer signals that the
    // cross-worker grant has round-tripped back too. Run every pass to a
    // fixed point instead of breaking out as soon as one reply appears.
    let mut tick_replies = Vec::new();
    for _ in 0..8 {
        let summary = process_workers_with_deterministic_barrier(
            &mut [&mut consumer_worker, &mut producer_worker],
            1,
        );
        assert!(summary.routing_errors.is_empty());
        tick_replies.extend(
            summary
                .worker_summaries
                .into_iter()
                .flat_map(|summary| summary.tick_replies),
        );
    }

    assert_eq!(tick_replies.len(), 1);
    assert!(cell_powered(&consumer_worker, RegionId(82), 0, 0));
    assert_eq!(turn(&consumer_worker, RegionId(82)), 1);
    assert_eq!(
        turn(&producer_worker, RegionId(83)),
        0,
        "producer worker must not run the caller continuation"
    );
}

#[test]
fn deterministic_barrier_orders_competing_cross_worker_power_requests() {
    let directory = Arc::new(RegionDirectory::new(vec![
        neighbor(84, BorderEdge::East, 86),
        neighbor(85, BorderEdge::East, 86),
    ]));
    let owners = Arc::new(RegionOwnerDirectory::new());
    let mut mixed_worker = RegionWorker::with_directory_and_owners(
        WorkerId(55),
        Arc::clone(&directory),
        Arc::clone(&owners),
    );
    let mut low_caller_worker = RegionWorker::with_directory_and_owners(
        WorkerId(56),
        Arc::clone(&directory),
        Arc::clone(&owners),
    );
    mixed_worker
        .add_region(RegionRuntime::new(power_export_consumer_region(RegionId(
            85,
        ))))
        .unwrap();
    mixed_worker
        .add_region(RegionRuntime::new(one_spare_power_producer_region(
            RegionId(86),
        )))
        .unwrap();
    low_caller_worker
        .add_region(RegionRuntime::new(power_export_consumer_region(RegionId(
            84,
        ))))
        .unwrap();

    mixed_worker.push_event(RegionId(85), tick(1)).unwrap();
    low_caller_worker.push_event(RegionId(84), tick(2)).unwrap();
    drain_workers_with_barrier(&mut [&mut mixed_worker, &mut low_caller_worker]);

    assert!(
        cell_powered(&low_caller_worker, RegionId(84), 0, 0),
        "lower remote caller wins even when the higher caller shares a worker with the producer"
    );
    assert!(!cell_powered(&mixed_worker, RegionId(85), 0, 0));
}

#[test]
fn stale_granted_reply_immediately_releases_producer_reservation() {
    // Retire-tickstate, P-a: nothing pauses anymore, so a granted reply for a
    // batch this caller has already moved past (or, as here, never even
    // requested) can no longer rely on "the next release will reach the
    // producer" -- there may never be a next release if this caller's own
    // demand stays quiet. A stale-but-granted reply must release the
    // producer's reservation right away instead of leaving it stuck.
    let caller = RegionId(87);
    let producer = RegionId(88);
    let unrelated = RegionId(89);
    let mut worker = worker_with_region_states(
        WorkerId(58),
        vec![
            RegionState::new(caller, 2, 2),
            RegionState::new(producer, 2, 2),
            RegionState::new(unrelated, 2, 2),
        ],
    );

    worker
        .push_event(
            caller,
            RegionEvent::ApplyPowerExportGrant {
                request: PowerExportRequest {
                    request_id: UiRequestId(999), // superseded: caller's current is 0
                    caller_region: caller,
                    caller_network: network(87, 0),
                    token: 0,
                    demand: 1,
                    consumer: small_city::core::entity::Entity::new(caller, 0),
                },
                grant: PowerExportGrant {
                    token: 0,
                    granted: true,
                    source_region: Some(producer),
                },
            },
        )
        .unwrap();
    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(
        pending_events(&worker, producer),
        1,
        "a stale granted reply must immediately release the producer's reservation"
    );
    assert_eq!(
        pending_events(&worker, unrelated),
        0,
        "M3 release routing should not broadcast to unrelated regions"
    );
}

#[test]
fn missing_caller_for_power_grant_result_is_deterministic_routing_error() {
    let producer = RegionId(72);
    let mut worker =
        worker_with_region_states(WorkerId(47), vec![power_export_producer_region(producer)]);

    worker
        .push_event(
            producer,
            RegionEvent::ProcessPowerExportRequest(ExportAllocationRequest {
                request: PowerExportRequest {
                    request_id: UiRequestId(1),
                    caller_region: RegionId(999),
                    caller_network: network(999, 0),
                    token: 0,
                    demand: 1,
                    consumer: small_city::core::entity::Entity::new(RegionId(999), 0),
                },
                candidates: vec![network(72, 0)],
                candidate_index: 0,
            }),
        )
        .unwrap();

    let summary = worker.process_region_events(1);

    assert_eq!(
        summary.routing_errors,
        vec![WorkerRoutingError::MissingTargetRegion {
            target_region: RegionId(999),
        }]
    );
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
fn job_export_request_completed_routes_apply_event_back_to_caller() {
    let caller = RegionId(73);
    let producer = RegionId(74);
    let mut worker = worker_with_region_states(
        WorkerId(48),
        vec![
            job_seeker_region(caller),
            job_slot_producer_region(producer),
        ],
    );

    worker
        .push_event(
            producer,
            RegionEvent::ProcessJobExportRequest(ExportAllocationRequest {
                request: JobExportRequest {
                    request_id: UiRequestId(1),
                    caller_region: caller,
                    caller_network: network(73, 0),
                    token: 0,
                    citizen: small_city::core::entity::Entity::new(caller, 0),
                },
                candidates: vec![network(74, 0)],
                candidate_index: 0,
            }),
        )
        .unwrap();

    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, caller), 1);
    assert_eq!(imported_job_count(&worker, caller), 0);
}

#[test]
fn job_grant_continuation_runs_in_caller_region() {
    let caller = RegionId(77);
    let producer = RegionId(78);
    let mut worker = worker_with_region_states(
        WorkerId(50),
        vec![
            job_seeker_region(caller),
            job_slot_producer_region(producer),
        ],
    );
    worker.set_region_topology(vec![neighbor(77, BorderEdge::East, 78)]);

    for request_id in 1..=240 {
        worker.push_event(caller, tick(request_id)).unwrap();
        drain_worker(&mut worker);
        if imported_job_count(&worker, caller) > 0 {
            assert_eq!(turn(&worker, caller), request_id as u32);
            assert_eq!(
                turn(&worker, producer),
                0,
                "producer must not run the caller's job continuation"
            );
            assert_eq!(imported_job_count(&worker, producer), 0);
            return;
        }
    }

    panic!("caller never recorded a remote workplace from the job export grant");
}

#[test]
fn wrong_region_export_grants_are_ignored_without_mutating_state() {
    let region = RegionId(79);
    let mut worker =
        worker_with_region_states(WorkerId(51), vec![power_export_consumer_region(region)]);

    worker
        .push_event(
            region,
            RegionEvent::ApplyPowerExportGrant {
                // request_id 0 matches a never-ticked runtime's current
                // generation, so this is NOT dropped as stale -- it reaches
                // the ECS write, which must still no-op: `consumer` names an
                // entity that does not exist in `region`'s world.
                request: PowerExportRequest {
                    request_id: UiRequestId(0),
                    caller_region: region,
                    caller_network: network(79, 0),
                    token: 0,
                    demand: 1,
                    consumer: small_city::core::entity::Entity::new(RegionId(80), 0),
                },
                grant: PowerExportGrant {
                    token: 0,
                    granted: true,
                    source_region: Some(RegionId(80)),
                },
            },
        )
        .unwrap();
    worker
        .push_event(
            region,
            RegionEvent::ApplyJobExportGrant {
                // Same reasoning as the power grant above: request_id 0
                // matches current, so this reaches the ECS write, which
                // must still no-op since no citizen with this id exists.
                request: JobExportRequest {
                    request_id: UiRequestId(0),
                    caller_region: region,
                    caller_network: network(79, 0),
                    token: 0,
                    citizen: small_city::core::entity::Entity::new(RegionId(80), 0),
                },
                grant: JobExportGrant {
                    token: 0,
                    granted: true,
                    workplace: Some(small_city::core::entity::Entity::new(RegionId(80), 0)),
                    location: Some(small_city::core::city_refs::CityCellRef::local(
                        RegionId(80),
                        0,
                        0,
                    )),
                    salary: 4,
                },
            },
        )
        .unwrap();

    let summary = worker.process_region_events(2);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(turn(&worker, region), 0);
    assert!(!cell_powered(&worker, region, 0, 0));
    assert_eq!(imported_job_count(&worker, region), 0);
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
fn derived_read_during_a_paused_power_handshake_does_not_wipe_imported_power() {
    // DT1 mid-tick-safety regression. While a consumer's tick is paused waiting on
    // a cross-region power grant -- imported power already applied to its
    // residential -- a derived-state read (here `inspect`, also the worker's own
    // between-pass hint publish) must NOT recompute the derived pass: `power::run`
    // resolves only LOCAL power and would wipe the imported grant, leaving the
    // residential unpowered so population never grows.
    //
    // The protection is that `derived_dirty` is set only by out-of-tick commands,
    // so it stays false throughout a tick and the read is a no-op. If a future
    // change marks it dirty inside a tick (e.g. from an applied grant or an
    // `invalidate_*` call), this test fails where the plain population-growth test
    // might still pass on timing.
    let consumer = power_export_growth_region(RegionId(50));
    let producer = power_export_producer_region(RegionId(51));
    let mut worker = worker_with_region_states(WorkerId(60), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(50, BorderEdge::East, 51)]);

    for request_id in 1..=48 {
        worker.push_event(RegionId(50), tick(request_id)).unwrap();
        worker
            .push_event(RegionId(51), tick(request_id + 100))
            .unwrap();
        // Drain pass-by-pass, forcing a derived read between every pass so one lands
        // while the consumer tick is mid-handshake.
        for _ in 0..16 {
            if worker.process_region_events(1).processed_regions == 0 {
                break;
            }
            let _ = worker
                .region_mut(RegionId(50))
                .expect("consumer")
                .inspect(0, 0);
        }
    }

    assert!(
        worker
            .region(RegionId(50))
            .expect("consumer")
            .state()
            .view()
            .status
            .population
            > 0,
        "imported power must survive derived reads taken mid-handshake"
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
}

#[test]
fn fired_remote_worker_finds_another_remote_job() {
    let consumer = RegionId(80);
    let first_producer = RegionId(81);
    let second_producer = RegionId(82);
    let mut worker = worker_with_region_states(
        WorkerId(65),
        vec![
            job_seeker_region(consumer),
            job_slot_producer_region(first_producer),
            job_slot_producer_region(second_producer),
        ],
    );
    worker.set_region_topology(vec![
        neighbor(80, BorderEdge::East, 81),
        neighbor(81, BorderEdge::East, 82),
    ]);

    for hour in 1..=(10 * 24) {
        worker.push_event(consumer, tick(hour)).unwrap();
        worker
            .push_event(first_producer, tick(100_000 + hour))
            .unwrap();
        worker
            .push_event(second_producer, tick(200_000 + hour))
            .unwrap();
        drain_worker(&mut worker);
        if first_resident_job_region(&worker, consumer) == Some(first_producer) {
            break;
        }
    }
    assert_eq!(
        first_resident_job_region(&worker, consumer),
        Some(first_producer),
        "the lower-id reachable producer should employ the first resident"
    );

    worker
        .push_event(
            first_producer,
            RegionEvent::RunCommand {
                request_id: UiRequestId(900_000),
                command: RegionCommand::Bulldoze { x: 0, y: 1 },
            },
        )
        .unwrap();
    drain_worker(&mut worker);

    for hour in (10 * 24 + 1)..=(20 * 24) {
        worker.push_event(consumer, tick(hour)).unwrap();
        worker
            .push_event(first_producer, tick(100_000 + hour))
            .unwrap();
        worker
            .push_event(second_producer, tick(200_000 + hour))
            .unwrap();
        drain_worker(&mut worker);
        if first_resident_job_region(&worker, consumer) == Some(second_producer) {
            break;
        }
    }

    assert_eq!(
        first_resident_job_region(&worker, consumer),
        Some(second_producer),
        "after losing producer A's job, the same resident should find producer B"
    );
}

#[test]
fn cross_region_job_export_is_visible_as_producer_workplace_tile() {
    let consumer = job_seeker_region(RegionId(60));
    let producer = job_slot_producer_region(RegionId(61));
    let mut worker = worker_with_region_states(WorkerId(64), vec![consumer, producer]);
    worker.set_region_topology(vec![neighbor(60, BorderEdge::East, 61)]);

    run_job_growth_days(&mut worker, RegionId(60), RegionId(61), 10);

    let region = worker.region(RegionId(60)).expect("consumer region");
    let inspect = region.state().inspect(0, 0);
    let assignment = match &inspect.details {
        Some(InspectDetailsView::Residential {
            job_assignments, ..
        }) => job_assignments.first().copied().expect("remote assignment"),
        details => panic!("expected residential inspect, got {details:?}"),
    };
    let cell_assignment = region
        .state()
        .view()
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 0 && cell.y == 0)
        .and_then(|cell| cell.job_assignments.first().copied())
        .expect("cell remote assignment");

    assert_eq!(assignment.cell.region, RegionId(61));
    assert_eq!((assignment.cell.x, assignment.cell.y), (0, 1));
    assert_eq!(assignment.salary, 4);
    assert!(assignment.is_remote);
    assert_eq!(cell_assignment, assignment);
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

#[test]
fn road_connected_region_uses_neighbor_goods_before_edge_imports() {
    let consumer = RegionId(70);
    let producer = RegionId(71);
    let mut worker = goods_trade_worker(true, consumer, producer);

    let (first_day_consumer, first_day_producer) =
        run_goods_trade_days(&mut worker, consumer, producer, 1);
    assert_eq!(
        first_day_consumer.local_goods_sold, 0,
        "{first_day_consumer:?}"
    );
    assert_eq!(
        first_day_producer.exported_goods, 4,
        "{first_day_producer:?}"
    );

    let (consumer_economy, producer_economy) =
        run_goods_trade_days(&mut worker, consumer, producer, 1);

    assert!(
        consumer_economy.local_goods_sold > 0,
        "{consumer_economy:?}"
    );
    assert_eq!(
        consumer_economy.imported_goods_sold, 0,
        "{consumer_economy:?}"
    );
    assert_eq!(producer_economy.exported_goods, 0, "{producer_economy:?}");
    assert_eq!(producer_economy.export_tax, 0, "{producer_economy:?}");
    assert!(producer_economy.manufacturing_tax > 0);
}

#[test]
fn disconnected_region_still_uses_edge_goods_market() {
    let consumer = RegionId(72);
    let producer = RegionId(73);
    let mut worker = goods_trade_worker(false, consumer, producer);

    let (consumer_economy, producer_economy) =
        run_goods_trade_days(&mut worker, consumer, producer, 2);

    assert_eq!(consumer_economy.local_goods_sold, 0);
    assert_eq!(consumer_economy.imported_goods_sold, 1);
    assert_eq!(producer_economy.exported_goods, 4);
    assert!(producer_economy.export_tax > 0);
}

#[test]
fn save_restart_drops_in_flight_goods_stock_without_double_counting() {
    let consumer = RegionId(74);
    let producer = RegionId(75);
    let mut worker = goods_trade_worker(true, consumer, producer);

    let _ = run_goods_trade_days(&mut worker, consumer, producer, 1);
    worker.restart_region_from_save_record(consumer).unwrap();
    worker.restart_region_from_save_record(producer).unwrap();

    let (consumer_economy, producer_economy) =
        run_goods_trade_days(&mut worker, consumer, producer, 1);

    assert_eq!(consumer_economy.local_goods_sold, 0, "{consumer_economy:?}");
    assert_eq!(producer_economy.exported_goods, 4, "{producer_economy:?}");
    assert!(producer_economy.export_tax > 0, "{producer_economy:?}");
}

#[test]
fn two_worker_barrier_matches_single_worker_for_power_and_jobs_script() {
    let consumer = RegionId(90);
    let producer = RegionId(91);
    let topology = vec![neighbor(90, BorderEdge::East, 91)];
    let mut single = worker_with_region_states(
        WorkerId(59),
        vec![
            RegionState::new(consumer, 6, 3),
            RegionState::new(producer, 2, 2),
        ],
    );
    single.set_region_topology(topology.clone());

    let directory = Arc::new(RegionDirectory::new(topology));
    let owners = Arc::new(RegionOwnerDirectory::new());
    let mut consumer_worker = RegionWorker::with_directory_and_owners(
        WorkerId(60),
        Arc::clone(&directory),
        Arc::clone(&owners),
    );
    let mut producer_worker = RegionWorker::with_directory_and_owners(
        WorkerId(61),
        Arc::clone(&directory),
        Arc::clone(&owners),
    );
    consumer_worker
        .add_region(RegionRuntime::new(RegionState::new(consumer, 6, 3)))
        .unwrap();
    producer_worker
        .add_region(RegionRuntime::new(RegionState::new(producer, 2, 2)))
        .unwrap();

    let build_script = [
        BuildStep::new(consumer, 0, 0, BuildingKind::Residential),
        BuildStep::new(consumer, 0, 1, BuildingKind::Park),
        BuildStep::new(consumer, 1, 0, BuildingKind::Road),
        BuildStep::new(consumer, 2, 0, BuildingKind::Road),
        BuildStep::new(consumer, 3, 0, BuildingKind::Road),
        BuildStep::new(consumer, 4, 0, BuildingKind::Road),
        BuildStep::new(consumer, 5, 0, BuildingKind::Road),
        BuildStep::new(consumer, 1, 2, BuildingKind::Industrial),
        BuildStep::new(consumer, 2, 2, BuildingKind::Road),
        BuildStep::new(consumer, 3, 2, BuildingKind::Road),
        BuildStep::new(consumer, 4, 2, BuildingKind::PowerPlant),
        BuildStep::new(producer, 0, 0, BuildingKind::Road),
        BuildStep::new(producer, 1, 0, BuildingKind::Road),
        BuildStep::new(producer, 0, 1, BuildingKind::Industrial),
        BuildStep::new(producer, 1, 1, BuildingKind::PowerPlant),
    ];

    for (index, step) in build_script.into_iter().enumerate() {
        run_build_step(
            &mut single,
            &mut consumer_worker,
            &mut producer_worker,
            UiRequestId(10_000 + index as u64),
            step,
        );
        assert_worker_region_parity(&single, &consumer_worker, consumer);
        assert_worker_region_parity(&single, &producer_worker, producer);
    }

    for request_id in 1..=(14 * 24) {
        single.push_event(consumer, tick(request_id)).unwrap();
        single
            .push_event(producer, tick(request_id + 100_000))
            .unwrap();
        drain_worker(&mut single);

        consumer_worker
            .push_event(consumer, tick(request_id))
            .unwrap();
        producer_worker
            .push_event(producer, tick(request_id + 100_000))
            .unwrap();
        drain_workers_with_barrier(&mut [&mut consumer_worker, &mut producer_worker]);

        assert_worker_region_parity(&single, &consumer_worker, consumer);
        assert_worker_region_parity(&single, &producer_worker, producer);

        if request_id == 7 * 24 {
            restart_parity_regions_from_save(
                &mut single,
                &mut consumer_worker,
                &mut producer_worker,
                consumer,
                producer,
            );
            assert_worker_region_parity(&single, &consumer_worker, consumer);
            assert_worker_region_parity(&single, &producer_worker, producer);
        }
    }

    assert_eq!(
        cell_powered(&consumer_worker, consumer, 0, 0),
        cell_powered(&single, consumer, 0, 0)
    );
    assert_eq!(
        imported_job_count(&consumer_worker, consumer),
        imported_job_count(&single, consumer)
    );
    assert_eq!(
        consumer_worker
            .region(consumer)
            .expect("multi consumer")
            .state()
            .view()
            .status,
        single
            .region(consumer)
            .expect("single consumer")
            .state()
            .view()
            .status
    );
}

fn worker_with_regions(id: WorkerId, regions: &[RegionId]) -> RegionWorker {
    let mut worker = test_worker(id);
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

fn goods_trade_worker(link_regions: bool, consumer: RegionId, producer: RegionId) -> RegionWorker {
    let mut worker = worker_with_region_states(
        WorkerId(70),
        vec![
            goods_consumer_region(consumer),
            goods_producer_region(producer),
        ],
    );
    if link_regions {
        worker.set_region_topology(vec![
            RegionNeighborLink::new(consumer, BorderEdge::West, producer),
            RegionNeighborLink::new(producer, BorderEdge::East, consumer),
        ]);
    }
    worker
}

fn goods_consumer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 3, 3);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(0, 1, BuildingKind::Road).success);
    assert!(region.build(1, 1, BuildingKind::Road).success);
    assert!(region.build(2, 1, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Commercial).success);
    assert!(region.build(0, 2, BuildingKind::Residential).success);
    assert!(region.build(2, 2, BuildingKind::PowerPlant).success);
    region
}

fn goods_producer_region(region_id: RegionId) -> RegionState {
    let mut region = RegionState::new(region_id, 3, 2);
    assert!(region.build(2, 0, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Road).success);
    assert!(region.build(1, 1, BuildingKind::Road).success);
    assert!(region.build(0, 0, BuildingKind::Industrial).success);
    assert!(region.build(0, 1, BuildingKind::PowerPlant).success);
    region
}

fn run_goods_trade_days(
    worker: &mut RegionWorker,
    consumer: RegionId,
    producer: RegionId,
    days: u64,
) -> (EconomyBreakdownView, EconomyBreakdownView) {
    let mut consumer_economy = empty_economy();
    let mut producer_economy = empty_economy();
    for request_id in 1..=(days * 24) {
        worker.push_event(consumer, tick(request_id)).unwrap();
        worker
            .push_event(producer, tick(request_id + 100_000))
            .unwrap();
        for reply in drain_worker_tick_replies(worker) {
            if reply.region_id == consumer {
                consumer_economy = tick_economy(&reply.result.events[0]);
            } else if reply.region_id == producer {
                producer_economy = tick_economy(&reply.result.events[0]);
            }
        }
    }
    (consumer_economy, producer_economy)
}

fn tick_economy(event: &GameEventView) -> EconomyBreakdownView {
    match event {
        GameEventView::TickSummary { economy, .. } => *economy,
        _ => panic!("expected tick summary"),
    }
}

fn empty_economy() -> EconomyBreakdownView {
    EconomyBreakdownView {
        salaries_paid: 0,
        workplace_tax: 0,
        rent_income: 0,
        commercial_sales_tax: 0,
        shoppers_served: 0,
        local_goods_produced: 0,
        local_goods_stored: 0,
        local_goods_sold: 0,
        imported_goods_sold: 0,
        exported_goods: 0,
        manufacturing_tax: 0,
        export_tax: 0,
        rent_failures: 0,
        maintenance_cost: 0,
        net: 0,
    }
}

fn imported_job_count(worker: &RegionWorker, region_id: RegionId) -> usize {
    worker
        .region(region_id)
        .expect("region")
        .state()
        .imported_job_count()
}

fn first_resident_job_region(worker: &RegionWorker, region_id: RegionId) -> Option<RegionId> {
    let resident = worker
        .region(region_id)
        .expect("consumer region")
        .state()
        .inspect(0, 0)
        .roster
        .into_iter()
        .next()?;
    match resident.relation {
        CitizenRelation::WorksAt { cell, .. } => Some(cell.region),
        CitizenRelation::Unemployed | CitizenRelation::LivesAt { .. } => None,
    }
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
    let mut worker = test_worker(id);
    for region in regions {
        worker.add_region(RegionRuntime::new(region)).unwrap();
    }
    worker
}

fn test_worker(id: WorkerId) -> RegionWorker {
    let owners = Arc::new(RegionOwnerDirectory::new());
    let directory = Arc::new(RegionDirectory::with_owners(
        Vec::new(),
        Arc::clone(&owners),
    ));
    RegionWorker::with_directory_and_owners(id, directory, owners)
}

fn test_worker_with_directory(id: WorkerId, directory: Arc<RegionDirectory>) -> RegionWorker {
    RegionWorker::with_directory_and_owners(id, directory, Arc::new(RegionOwnerDirectory::new()))
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

fn drain_worker_tick_replies(worker: &mut RegionWorker) -> Vec<RegionTickResponse> {
    let mut replies = Vec::new();
    for _ in 0..16 {
        let summary = worker.process_region_events(1);
        replies.extend(summary.tick_replies);
        if summary.processed_regions == 0 {
            return replies;
        }
    }

    panic!("worker did not drain");
}

#[derive(Clone, Copy)]
struct BuildStep {
    region: RegionId,
    x: usize,
    y: usize,
    kind: BuildingKind,
}

impl BuildStep {
    fn new(region: RegionId, x: usize, y: usize, kind: BuildingKind) -> Self {
        Self { region, x, y, kind }
    }
}

fn run_build_step(
    single: &mut RegionWorker,
    consumer_worker: &mut RegionWorker,
    producer_worker: &mut RegionWorker,
    request_id: UiRequestId,
    step: BuildStep,
) {
    let event = RegionEvent::RunCommand {
        request_id,
        command: RegionCommand::Build {
            x: step.x,
            y: step.y,
            kind: step.kind,
        },
    };
    single.push_event(step.region, event).unwrap();
    drain_worker(single);

    let event = RegionEvent::RunCommand {
        request_id,
        command: RegionCommand::Build {
            x: step.x,
            y: step.y,
            kind: step.kind,
        },
    };
    if consumer_worker.region(step.region).is_some() {
        consumer_worker.push_event(step.region, event).unwrap();
    } else {
        producer_worker.push_event(step.region, event).unwrap();
    }
    drain_workers_with_barrier(&mut [consumer_worker, producer_worker]);
}

fn assert_worker_region_parity(
    single_worker: &RegionWorker,
    multi_worker: &RegionWorker,
    region: RegionId,
) {
    let single_view = single_worker
        .region(region)
        .expect("single-worker region")
        .state()
        .view();
    let multi_view = multi_worker
        .region(region)
        .expect("multi-worker region")
        .state()
        .view();

    assert_eq!(multi_view.status, single_view.status);
    assert_eq!(multi_view.map.cells, single_view.map.cells);
    assert_eq!(
        multi_worker
            .region(region)
            .expect("multi-worker region")
            .state()
            .stats_snapshot(),
        single_worker
            .region(region)
            .expect("single-worker region")
            .state()
            .stats_snapshot()
    );
}

fn restart_parity_regions_from_save(
    single: &mut RegionWorker,
    consumer_worker: &mut RegionWorker,
    producer_worker: &mut RegionWorker,
    consumer: RegionId,
    producer: RegionId,
) {
    single.restart_region_from_save_record(consumer).unwrap();
    single.restart_region_from_save_record(producer).unwrap();
    consumer_worker
        .restart_region_from_save_record(consumer)
        .unwrap();
    producer_worker
        .restart_region_from_save_record(producer)
        .unwrap();
}

fn drain_workers_with_barrier(workers: &mut [&mut RegionWorker]) {
    for _ in 0..24 {
        let summary = process_workers_with_deterministic_barrier(workers, 1);
        assert!(summary.routing_errors.is_empty());
        if summary
            .worker_summaries
            .iter()
            .all(|summary| summary.processed_regions == 0)
        {
            return;
        }
    }

    panic!("workers did not drain");
}
