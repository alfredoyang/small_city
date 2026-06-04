//! Integration tests for opaque caller-owned regional continuations.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use small_city::core::regions::continuation::{CallerContinuation, NeighborRequest};
use small_city::core::regions::runtime::{
    ImportedResourcePayload, OutboundMessage, RegionEvent, RegionRuntime, RegionRuntimeError,
};
use small_city::core::regions::{
    ImportDecision, ImportedResource, ImportedResourceResult, RegionId, RegionState, ResourceId,
    ResourceKind,
};

#[test]
fn target_region_returns_continuation_with_result() {
    let mut target = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
    let continuation_ran = Arc::new(AtomicBool::new(false));

    target.push_event(RegionEvent::ProcessImportedResource(request(
        RegionId(1),
        resource(9, ResourceKind::Jobs, 1),
        continuation_flag(RegionId(1), Arc::clone(&continuation_ran)),
    )));

    let outbound = target.process_next_event();

    let [
        OutboundMessage::ReturnImportedResourceContinuation {
            caller_region,
            result,
            ..
        },
    ] = outbound.as_slice()
    else {
        panic!("expected returned continuation");
    };

    assert_eq!(*caller_region, RegionId(1));
    assert_eq!(result.decision, ImportDecision::Accepted);
    assert!(!continuation_ran.load(Ordering::SeqCst));
}

#[test]
fn continuation_does_not_run_while_target_region_handles_request() {
    let caller = RegionRuntime::new(RegionState::new(RegionId(3), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(4), 2, 2));
    let continuation_ran = Arc::new(AtomicBool::new(false));

    target.push_event(RegionEvent::ProcessImportedResource(request(
        caller.region_id(),
        resource(10, ResourceKind::ParkAccess, 1),
        continuation_flag(caller.region_id(), Arc::clone(&continuation_ran)),
    )));

    let outbound = target.process_next_event();

    assert_eq!(outbound.len(), 1);
    assert!(!continuation_ran.load(Ordering::SeqCst));
    assert!(caller.state().neighbor_import_results().is_empty());
}

#[test]
fn continuation_runs_only_when_caller_region_processes_event() {
    let mut caller = RegionRuntime::new(RegionState::new(RegionId(5), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(6), 2, 2));
    let continuation_ran = Arc::new(AtomicBool::new(false));

    target.push_event(RegionEvent::ProcessImportedResource(request(
        caller.region_id(),
        resource(11, ResourceKind::ShoppingAccess, 1),
        continuation_flag(caller.region_id(), Arc::clone(&continuation_ran)),
    )));

    let (continuation, result) = returned_continuation(target.process_next_event());
    assert!(!continuation_ran.load(Ordering::SeqCst));

    caller.push_event(RegionEvent::RunImportedResourceContinuation {
        continuation,
        result: result.clone(),
    });

    assert!(caller.process_next_event().is_empty());
    assert!(continuation_ran.load(Ordering::SeqCst));
    assert_eq!(caller.state().neighbor_import_results(), &[result]);
}

#[test]
fn continuation_can_modify_caller_region_state() {
    let mut caller = RegionRuntime::new(RegionState::new(RegionId(12), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(13), 2, 2));

    target.push_event(RegionEvent::ProcessImportedResource(request(
        caller.region_id(),
        resource(14, ResourceKind::ServiceAccess, 1),
        CallerContinuation::new(caller.region_id(), |region, _result| {
            region.tick_local();
        }),
    )));

    let (continuation, result) = returned_continuation(target.process_next_event());

    assert_eq!(caller.state().view().status.turn, 0);
    assert_eq!(target.state().view().status.turn, 0);

    caller.push_event(RegionEvent::RunImportedResourceContinuation {
        continuation,
        result,
    });

    assert!(caller.process_next_event().is_empty());
    assert_eq!(caller.state().view().status.turn, 1);
    assert_eq!(target.state().view().status.turn, 0);
    assert!(caller.state().neighbor_import_results().is_empty());
}

#[test]
fn continuation_event_is_consumed_once() {
    let mut caller = RegionRuntime::new(RegionState::new(RegionId(7), 2, 2));
    let result = ImportedResourceResult {
        decision: ImportDecision::Accepted,
        forwarded_resources: Vec::new(),
    };

    caller.push_event(RegionEvent::RunImportedResourceContinuation {
        continuation: record_import_result(caller.region_id()),
        result: result.clone(),
    });

    assert!(caller.process_next_event().is_empty());
    assert!(caller.process_next_event().is_empty());

    assert_eq!(caller.state().neighbor_import_results(), &[result]);
}

#[test]
fn continuation_for_one_region_cannot_run_in_another_region() {
    let mut wrong_runtime = RegionRuntime::new(RegionState::new(RegionId(9), 2, 2));
    let continuation_ran = Arc::new(AtomicBool::new(false));
    let result = ImportedResourceResult {
        decision: ImportDecision::Accepted,
        forwarded_resources: Vec::new(),
    };

    wrong_runtime.push_event(RegionEvent::RunImportedResourceContinuation {
        continuation: continuation_flag(RegionId(8), Arc::clone(&continuation_ran)),
        result,
    });

    let outbound = wrong_runtime.process_next_event();

    assert!(!continuation_ran.load(Ordering::SeqCst));
    assert!(wrong_runtime.state().neighbor_import_results().is_empty());
    assert_eq!(
        runtime_errors(&outbound),
        vec![RegionRuntimeError::ContinuationRoutedToWrongRegion {
            expected_region: RegionId(8),
            actual_region: RegionId(9),
        }]
    );
}

fn request(
    caller_region: RegionId,
    resource: ImportedResource,
    continuation: CallerContinuation<ImportedResourceResult>,
) -> NeighborRequest<ImportedResourcePayload, ImportedResourceResult> {
    NeighborRequest {
        payload: ImportedResourcePayload {
            resource,
            local_used_capacity: 0,
            border_crossing_cost: 1,
            target_neighbors: Vec::new(),
        },
        continuation: {
            assert_eq!(continuation.caller_region(), caller_region);
            continuation
        },
    }
}

fn continuation_flag(
    caller_region: RegionId,
    ran: Arc<AtomicBool>,
) -> CallerContinuation<ImportedResourceResult> {
    CallerContinuation::new(caller_region, move |region, result| {
        ran.store(true, Ordering::SeqCst);
        region.apply_neighbor_import_result(result);
    })
}

fn record_import_result(caller_region: RegionId) -> CallerContinuation<ImportedResourceResult> {
    CallerContinuation::new(caller_region, |region, result| {
        region.apply_neighbor_import_result(result);
    })
}

fn returned_continuation(
    mut outbound: Vec<OutboundMessage>,
) -> (
    CallerContinuation<ImportedResourceResult>,
    ImportedResourceResult,
) {
    let message = outbound.pop().expect("outbound message");
    match message {
        OutboundMessage::ReturnImportedResourceContinuation {
            continuation,
            result,
            ..
        } => (continuation, result),
        OutboundMessage::RegionCommandCompleted(reply) => {
            panic!("unexpected command reply: {reply:?}")
        }
        OutboundMessage::RegionTickCompleted(reply) => {
            panic!("unexpected tick reply: {reply:?}")
        }
        OutboundMessage::RegionSnapshotReady(reply) => {
            panic!("unexpected snapshot reply: {reply:?}")
        }
        OutboundMessage::RegionExportsChanged(change) => {
            panic!("unexpected export change: {change:?}")
        }
        OutboundMessage::RuntimeError(error) => panic!("unexpected runtime error: {error:?}"),
    }
}

fn runtime_errors(outbound: &[OutboundMessage]) -> Vec<RegionRuntimeError> {
    outbound
        .iter()
        .filter_map(|message| match message {
            OutboundMessage::RuntimeError(error) => Some(*error),
            OutboundMessage::ReturnImportedResourceContinuation { .. } => None,
            OutboundMessage::RegionCommandCompleted(_) => None,
            OutboundMessage::RegionTickCompleted(_) => None,
            OutboundMessage::RegionSnapshotReady(_) => None,
            OutboundMessage::RegionExportsChanged(_) => None,
        })
        .collect()
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
