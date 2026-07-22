# Coordinator-only resource refactor plan

Status: **proposal only**.

## Goal

Remove the worker's same-thread routing special case, then split resource event
logic into smaller modules.

The target shape:

```text
RegionRuntime emits RoutedRegionEvent
        |
        v
Coordinator / test harness delivers it
        |
        v
target RegionRuntime processes RegionEvent
```

No resource should have a hidden local shortcut just because the target region
happens to live on the same worker.

## Non-goals

```text
no power balance changes
no goods delivery behavior changes
no citizen payroll behavior changes
no new resource protocol
no deterministic-winner guarantee for cross-region contention
```

## Current Problem

`RegionWorker` has two routing modes:

```text
Immediate mode
  local target owned by this worker -> push directly into runtime inbox
  missing target                   -> WorkerRoutingError

Coordinator mode
  any target -> RoutedRegionEvent
```

That means tests and production can observe different route shapes.

```text
same-worker target today

  worker
    |
    +-- Immediate mode: direct push_event(target)
    |
    +-- Coordinator mode: RoutedRegionEvent -> coordinator -> target
```

The direct path is useful for old tests, but it makes ownership harder to
review:

```text
runtime event handlers
  power request/retry
  goods delivery terminal events
  citizen/truck travel arrivals
  employment directory wakes
  all inside one large process_event match
```

## Target Design

Use one route shape:

```text
all worker-routed events
  -> WorkerRoutedMessage::Coordinator(RoutedRegionEvent)
```

Tests that need same-worker delivery use a harness:

```text
process worker pass
  collect coordinator_events
  deliver each route to target worker/runtime
  repeat until idle
```

This keeps unit tests cheap without preserving production-only special cases.

## Patch Split

### P0-a: Coordinator-shaped test harness

Scope:

```text
test support seam plus test helper only
no production routing behavior change
no resource behavior change
```

Add helpers for tests that currently rely on Immediate mode.

Expose the coordinator-shaped pass only to unit tests before deleting Immediate
mode:

```rust
#[cfg(test)]
impl RegionWorker {
    fn process_region_events_for_coordinator(
        &mut self,
        max_events_per_region: usize,
    ) -> WorkerRunSummary {
        self.process_region_events_with_mode(
            max_events_per_region,
            RegionRoutingMode::Coordinator,
        )
    }
}
```

Pseudo-code:

```rust
fn drain_worker_coordinator_events(worker: &mut RegionWorker) {
    for _ in 0..MAX_TEST_DRAIN_ROUNDS {
        let summary = worker.process_region_events_for_coordinator(usize::MAX);
        assert!(summary.routing_errors.is_empty());

        if summary.coordinator_events.is_empty() && summary.processed_regions == 0 {
            return;
        }

        for route in summary.coordinator_events {
            deliver_route_to_same_worker(worker, route);
        }
    }

    panic!("test coordinator drain did not become idle");
}

fn deliver_route_to_same_worker(worker: &mut RegionWorker, route: RoutedRegionEvent) {
    match route.recipients {
        RegionRecipients::One(region) => worker.push_event(region, route.event).unwrap(),
        RegionRecipients::Many(regions) => {
            for region in regions {
                worker.push_event(region, route.event.clone()).unwrap();
            }
        }
        RegionRecipients::All => {
            for region in worker.owners.region_ids() {
                worker.push_event(region, route.event.clone()).unwrap();
            }
        }
    }
}
```

The helper lives with `worker.rs` unit tests, so it can use private worker
internals without adding production API. It deliberately delivers through
`RoutedRegionEvent`. It must not use `process_region_events`, because that is
the Immediate-mode API until P0-b.

Tests to migrate:

```text
worker tests expecting pending_events after same-worker direct routing
power eager nudge tests
goods/travel same-worker route tests if any depend on Immediate delivery
```

Review checks:

```text
helper consumes RoutedRegionEvent, not private ECS state
helper preserves FIFO enough for unit tests
helper has a bounded drain loop
live runner behavior unchanged
no new non-test public worker API
```

### P0-b: Remove Immediate routing mode

Scope:

```text
worker routing only
delete RegionRoutingMode::Immediate
delete direct local push_event routing branch
no resource logic changes
```

Target code:

```rust
fn route_region_event(
    &mut self,
    target_region: RegionId,
    event: RegionEvent,
) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
    // Worker-minted routes such as PowerCapacityRecheck keep the old
    // missing-target behavior: do not manufacture a coordinator event that
    // would later fault the whole runner.
    if self.owners.owner_of(target_region).is_none() {
        return Err(WorkerRoutingError::MissingTargetRegion { target_region });
    }

    Ok(WorkerRoutedMessage::Coordinator(RoutedRegionEvent {
        recipients: RegionRecipients::One(target_region),
        event,
    }))
}
```

Worker pass:

```rust
fn process_region_events(...) -> WorkerRunSummary {
    let summary = process_owned_runtime_events(...);

    // No local coordinator delivery here.
    // Caller/coordinator owns delivery of summary.coordinator_events.
    summary
}
```

Behavior allowed:

```text
same-worker events now appear as coordinator_events
tests use the harness to deliver them
```

Behavior forbidden:

```text
do not drop same-worker events
do not push directly into target runtime from route_region_event
do not make coordinator inspect World
do not change power/goods/job/citizen event payloads
```

Review checks:

```text
RegionRoutingMode removed
deliver_coordinator_event_locally removed if unused
P0-a process_region_events_for_coordinator test alias removed
same-worker power nudge still eventually powers target through harness/coordinator
worker-minted missing targets remain WorkerRoutingError before coordinator route creation
runtime-emitted CoordinatorRoute messages still go through normal coordinator validation
```

### P1: Split travel arrival handling

Scope:

```text
RegionRuntime travel-related events only
no behavior changes
```

Extract:

```text
StepTravel
ReceiveTraveler
DestinationArrived routing shell
```

Target shape:

```rust
impl RegionRuntime {
    fn handle_travel_event(&mut self, event: TravelEvent) -> Vec<OutboundMessage> {
        match event {
            TravelEvent::StepTravel { step } => self.handle_step_travel(step),
            TravelEvent::ReceiveTraveler { eligible_step, handoff } => {
                self.handle_receive_traveler(eligible_step, handoff)
            }
            TravelEvent::DestinationArrived { traveler, destination } => {
                self.handle_destination_arrived(traveler, destination)
            }
        }
    }
}
```

Diagram:

```text
StepTravel
  -> core moves tokens
  -> route traveler handoffs
  -> route destination arrivals

DestinationArrived
  -> goods truck validation first
  -> else citizen attendance
```

Review checks:

```text
truck validation remains before citizen attendance
core travel still emits value-only pending arrivals
no new queue
```

### P2: Extract goods delivery handling

Scope:

```text
goods supply request/grant/delivery/confirm/reject only
no economy formula change
no truck behavior change
```

Create:

```text
src/core/regions/runtime/goods_delivery.rs
```

Use a `runtime` child module so the extracted handlers can keep using
`RegionRuntime` internals without widening visibility.

Move handler bodies:

```rust
fn handle_process_goods_supply_request(...)
fn handle_apply_goods_supply_grant(...)
fn handle_apply_goods_delivery(...)
fn handle_confirm_goods_delivery(...)
fn handle_reject_goods_delivery(...)
```

Ownership diagram:

```text
factory region owns
  truck.shipment
  factory reserved_outbound_units
  goods supply allocation
  delivery revenue queue

commercial region owns
  GoodsOrder
  inbound_reserved_units
  commercial local_goods_stored
  parked truck token while truck is at commercial
```

Review checks:

```text
commercial stock changes only in ApplyGoodsDelivery
factory stock/shipment cleanup only in Confirm/Reject at factory owner
same-region and remote delivery use same handler
duplicate DestinationArrived cannot double-deliver
```

### P3: Extract power allocation handling

Scope:

```text
power request/release/grant/recheck only
no allocation math change
no candidate ordering change
```

Create:

```text
src/core/regions/power_allocation.rs
```

Move:

```rust
begin_power_export
process_power_export_request
apply_power_export_result
apply_power_export_grant
release_stale_granted_power
power_capacity_recheck
power_release_routes
power_candidates
```

Target ownership:

```text
consumer runtime owns
  candidate retry index
  current_power_request_id
  applied grant state

producer runtime owns
  power_export_allocations
  spare capacity measurement

directory owns
  target computation for capacity recheck

coordinator owns
  route delivery
```

Review checks:

```text
candidate retry remains in runtime
producer allocation math unchanged
stale granted reply still releases producer allocation
PowerCapacityRecheck remains time-neutral
```

### P4: Extract employment directory handling

Scope:

```text
employment directory runtime glue only
no job allocation behavior change
```

Create:

```text
src/core/regions/employment_runtime.rs
```

Split this patch if the diff becomes hard to review:

```text
P4-a: EmploymentDirectoryReady handler
P4-b: daily_employment_phase and home claim submission shell
```

Move first:

```rust
handle_employment_directory_ready
employer_validate_claims
home_apply_accepted_employment
employer_apply_releases
home_apply_losses
```

Then move:

```rust
daily_employment_phase
home_region_daily_jobs
```

Target diagram:

```text
EmploymentDirectoryReady
  -> employer validates claims
  -> employer applies releases/losses
  -> home applies accepted jobs
  -> home applies losses
```

Review checks:

```text
accepted assignment still applied only by home region
contract truth still owned by employer region
route invalidation still uses installed discovery snapshot
no old job-export path reappears
```

## Migration Notes

Do P0 first. P1-P4 become easier after all event routing has one shape.

```text
P0-a prepares tests
P0-b removes Immediate mode
P1-P4 split runtime handler code by domain
```

Each patch should pass:

```sh
cargo fmt
cargo clippy -- -D warnings
cargo test -q
```

## Risk

```text
P0-b may expose tests that were relying on same-pass direct delivery
coordinator/test harness drain loops must be bounded
handler extraction can accidentally move ownership checks if done too broadly
```

Mitigation:

```text
small patches
test harness first
no renames during extraction
compare behavior with existing scenario tests after each split
```
