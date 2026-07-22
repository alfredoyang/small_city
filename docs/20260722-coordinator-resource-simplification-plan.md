# Coordinator resource simplification plan

Status: **proposal only**.

## Goal

Use the coordinator event loop as the single region-to-region transport for
citizen travel arrivals, goods deliveries, and power allocation messages.

The goal is not to make every resource use the same simulation model. The goal
is to make every cross-owner interaction follow the same ownership rule:

```text
owner region keeps truth
sender emits RegionEvent
coordinator routes RegionEvent
target owner applies truth change
```

This keeps same-region and cross-region behavior aligned without exposing
`World` outside its owning region.

## Assumptions

```text
1. Local-region deterministic behavior still matters.
2. Cross-region timing may be stale because it reads snapshots and event queues.
3. Same-region events may use the coordinator path when that removes duplicate
   logic, even if a direct call would be faster.
4. The coordinator must not inspect World or decide domain policy.
5. Power remains allocation-based; it does not become a traveler/token system.
```

## Current Design

```text
Citizen travel

  host region travel step
        |
        | local and remote workplace arrivals both produce
        | PendingDestinationArrival
        v
  runtime routes DestinationArrived to traveler.entity.region()
        |
        v
  home region validates ArrivalAction and records attendance
```

```text
Goods

  commercial demand
        |
        v
  GoodsSupplyRequest / ProcessGoodsSupplyRequest
        |
        v
  producer dispatches factory-owned truck
        |
        v
  DestinationArrived -> ApplyGoodsDelivery -> Confirm/RejectGoodsDelivery

  Remaining complexity:
    - export-era naming is still visible in allocation helpers
    - local/remote behavior must stay unified as cleanup continues
    - producer-side revenue-before-dispatch was fixed outside this plan by
      settling confirmed delivery revenue through `World.pending_goods_delivery_revenue`
```

```text
Power

  consumer runtime
        |
        v
  RegionRuntime builds producer candidates from discovery snapshot
        |
        v
  producer runtime owns allocation truth
        |
        v
  consumer runtime applies grant or tries the next candidate

  Remaining complexity:
    - power recheck target policy already lives in the directory helper
    - worker still owns transport selection through `route_region_event`, which
      is intentional because it preserves Immediate vs Coordinator routing mode
```

The repeated shape is "a caller asks a different owner to mutate owner truth",
but each domain still carries some local/remote or worker/runtime differences.

## Improved Design

```text
                         +----------------------+
                         | RegionEventCoordinator|
                         | route only            |
                         | no World access       |
                         +----------+-----------+
                                    |
          +-------------------------+-------------------------+
          |                         |                         |
          v                         v                         v
  +---------------+         +---------------+         +---------------+
  | region A      |         | region B      |         | region C      |
  | owns World A  |         | owns World B  |         | owns World C  |
  +-------+-------+         +-------+-------+         +-------+-------+
          ^                         ^                         ^
          |                         |                         |
          +------------- routed RegionEvent values -----------+
```

```text
same-region event:

  region A runtime
      -> RoutedRegionEvent { One(A), event }
      -> coordinator
      -> region A FIFO

cross-region event:

  region A runtime
      -> RoutedRegionEvent { One(B), event }
      -> coordinator
      -> region B FIFO
```

Same-region routing through the coordinator is allowed only where it removes a
real duplicate path. Hot local-only computations stay direct.

## Ownership Table

```text
domain                  truth owner              routed event changes
------                  -----------              --------------------
citizen                 citizen home region      attendance / arrival action
travel                  token host region        movement, handoff, arrival
goods order             commercial region        inbound reservation, stock
shipment                factory region           truck assignment, reservation
goods supply allocation producer RegionRuntime   producer-network allocation
power                   producer RegionRuntime   exported capacity allocation
```

## Patch Split

### P1: Citizen Arrival Routing Audit

Audit result:

```text
no production change expected
current code already routes local and remote work arrivals through the shared
DestinationArrived event path, with truck validation checked before citizen
attendance
```

Scope:

```text
citizen travel arrival only
verify same coordinator path for local and remote DestinationArrived
no salary formula change
no goods or power changes except preserving shared DestinationArrived dispatch
no new queue type
```

Expected patch shape:

```text
tests/comments only unless review finds a real direct-arrival branch
```

Behavior allowed:

```text
local workplace arrival continues to enqueue DestinationArrived through coordinator
duplicate/stale DestinationArrived remains harmless
citizen home region remains the only attendance owner
remove stale tests/comments that imply a direct local-pay path
```

Behavior forbidden:

```text
do not pay directly from travel step
do not let token host mutate foreign citizen payroll state
do not change return-home handoff semantics
do not make StepTravel depend on a new blocking drain
```

Design:

```text
current target shape

  RegionEvent::StepTravel in any host region
      -> token reaches workplace
      -> World.outgoing_destination_arrivals.push(...)
      -> runtime routes DestinationArrived to citizen home
      -> coordinator wakes home region
      -> home applies attendance gate
```

Pseudo-code:

```rust
// core travel step: value-only fact, no runtime imports.
fn on_token_reaches_work(token: &TravelToken, traveler: TravelerId, world: &mut World) {
    if token.kind.is_work_trip() && token.reached_current_work_endpoint() {
        world.outgoing_destination_arrivals.push(PendingDestinationArrival {
            traveler,
            destination: token.current_work_endpoint(),
        });
    }
}

// regions layer: same local/remote route shape.
fn route_destination_arrivals(runtime: &mut RegionRuntime) -> Vec<RoutedRegionEvent> {
    runtime
        .state
        .take_pending_destination_arrivals()
        .into_iter()
        .map(|arrival| {
            runtime.destination_arrival_route(arrival.traveler, arrival.destination)
        })
        .collect()
}

// citizen home region.
fn apply_destination_arrived(
    home: &mut RegionState,
    traveler: TravelerId,
    destination: PlaceRef,
) {
    let Some(citizen) = home.world.citizens.get_mut(&traveler.entity) else {
        return;
    };
    if citizen.arrival_action != ArrivalAction::StartWorkShift {
        return;
    }
    if citizen.work_trip_generation != traveler.generation {
        return;
    }
    if !citizen.assignment_matches(destination) {
        return;
    }

    citizen.attended_since_daily_settlement = true;
    citizen.arrival_action = ArrivalAction::ReturnHome;
}
```

Review checks:

```text
local and remote work arrivals both emit RegionEvent::DestinationArrived
travel.rs still does not import runtime/worker/coordinator types
duplicate arrival cannot pay twice
old local arrival behavior has an equivalent test
no production diff is needed if current tests already prove these invariants
DestinationArrived dispatch keeps the truck path first, then citizen attendance
```

Diagram:

```text
before

  old risk: local token arrival could grow a direct attendance shortcut
  old risk: remote token arrival used routed event

after

  local token arrival  \
                       +--> DestinationArrived --> coordinator --> citizen home
  remote token arrival /
```

The same pending-arrival and routed-event mechanism is reused by goods trucks.
Citizens interpret `ArrivalAction::StartWorkShift`; trucks interpret
`ArrivalAction::DeliverGoods`.

### P2: Goods Local/Remote Delivery Guardrails

Scope:

```text
goods request, dispatch, delivery, confirm/reject only
verify same RegionEvent path for local and remote commercial orders
factory remains shipment/truck owner
commercial remains order/storage owner
no power or citizen changes
do not rename public types in this patch unless required by cleanup
```

Expected patch shape:

```text
mostly tests/comments plus removal of any remaining local stock-credit shortcut
found during review
```

Behavior allowed:

```text
same-region goods request may route through coordinator
GoodsSupplyGrant remains an acknowledgement, not stock credit
commercial stock changes only from delivery arrival apply
factory dispatch path handles local and remote commercial PlaceRef uniformly
```

Behavior forbidden:

```text
do not reintroduce direct local commercial stock refill
do not let commercial mutate factory warehouse/truck truth
do not let factory mutate foreign commercial storage directly
do not split local and remote dispatch algorithms
```

Design:

```text
current mixed shape

  export-era names remain, but live local and remote grants already go through
  GoodsSupplyRequest and truck delivery

target shape

  commercial order, local or remote
      -> GoodsSupplyRequest
      -> coordinator routes ProcessGoodsSupplyRequest to factory region
      -> factory chooses stock + idle truck
      -> truck travels
      -> DestinationArrived routes to factory owner
      -> factory emits ApplyGoodsDelivery to commercial owner
      -> commercial applies stock or rejects
      -> factory confirms cleanup or rejection cleanup
```

Pseudo-code:

```rust
fn commercial_needs_goods(consumer: &mut RegionRuntime, order: GoodsOrder) -> Vec<RoutedRegionEvent> {
    order
        .batches()
        .map(|batch| RoutedRegionEvent {
            recipients: RegionRecipients::One(batch.producer_region),
            event: RegionEvent::ProcessGoodsSupplyRequest(batch),
        })
        .collect()
}

fn apply_goods_supply_grant(
    consumer: &mut RegionRuntime,
    request: GoodsSupplyAllocationRequest,
    grant: GoodsSupplyGrant,
) -> Vec<RoutedRegionEvent> {
    if grant.granted {
        // This is where inbound capacity is reserved. Denied requests must not
        // reserve shelf space that later cleanup has to undo.
        consumer.state.record_goods_order(&request.request, grant.units);
    }

    consumer.apply_goods_supply_result(request, grant)
}

fn process_goods_supply_request(
    factory_region: &mut RegionRuntime,
    request: GoodsSupplyAllocationRequest,
) -> RoutedRegionEvent {
    let allocation_key = export_allocation_key(&request.request);
    let grant = factory_region.state.dispatch_goods_shipment(
        &request.request,
        request.producer_network(),
        allocation_key,
    );

    RoutedRegionEvent {
        recipients: RegionRecipients::One(request.request.caller_region),
        event: RegionEvent::ApplyGoodsSupplyGrant {
            request,
            grant,
        },
    }
}

fn validate_goods_truck_arrival(
    factory_region: &mut RegionState,
    traveler: TravelerId,
    destination: PlaceRef,
) {
    match factory_region.validate_goods_truck_arrival(traveler, destination) {
        GoodsTruckArrival::Deliver(shipment) => route_apply_goods_delivery(shipment),
        GoodsTruckArrival::Reject(shipment) => route_reject_goods_delivery(shipment),
    }
}

fn apply_goods_delivery(commercial_region: &mut RegionState, delivery: GoodsDelivery) {
    if commercial_region.order_accepts(delivery.order, delivery.units) {
        commercial_region.add_commercial_goods(delivery.commercial, delivery.units);
        // Retarget is in the token-holding region. For a remote delivery the
        // token is parked in the commercial region, not the factory region.
        commercial_region.retarget_goods_truck_home(delivery.traveler);
        route_confirm_goods_delivery(delivery);
    } else {
        // Commercial owns only the parked token/order decision. Factory-owned
        // shipment cleanup happens after the routed RejectGoodsDelivery reaches
        // the factory region.
        commercial_region.retarget_goods_truck_home(delivery.traveler);
        route_reject_goods_delivery(delivery);
    }
}

fn confirm_goods_delivery(factory_runtime: &mut RegionRuntime, confirm: GoodsDeliveryConfirm) {
    // Revenue queueing was fixed outside this coordinator plan: confirmation
    // records pending delivery revenue for the next economy settlement.
    factory_runtime.state.queue_confirmed_goods_delivery_revenue(confirm.truck, confirm.units);
    factory_runtime.state.consume_reserved_factory_goods(confirm.truck, confirm.units);
    factory_runtime.goods_supply_allocations.release_key(confirm.allocation_key);
    // Do not retarget here: the token may be in the commercial region.
}

fn reject_goods_delivery(runtime: &mut RegionRuntime, reject: GoodsDeliveryReject) {
    runtime.goods_supply_allocations.release_key(reject.allocation_key);

    if reject.traveler.entity.region() == runtime.region_id() {
        // Factory-owned truth: truck shipment and reserved factory goods.
        runtime
            .state
            .cancel_goods_delivery(reject.traveler, reject.units);

        if reject.order.commercial.region() == runtime.region_id() {
            // Same-region order: this runtime is also the consumer owner.
            runtime.state.reject_goods_delivery_at_host(reject.order, reject.units);
        }
    } else {
        // Remote order: consumer-owned truth only. This releases inbound shelf
        // reservation when the factory rejects/cancels a shipment.
        runtime.state.reject_goods_delivery_at_host(reject.order, reject.units);
    }
}
```

Rule:

```text
retarget loaded trucks in the token-holding region
release supply allocation in the producer RegionRuntime
never call retarget_goods_truck_home from the factory confirmation path
confirm/reject shipment cleanup happens only in the factory region
```

Review checks:

```text
local and remote orders use the same ProcessGoodsSupplyRequest path
commercial stock changes only in ApplyGoodsDelivery
factory reservation is released on confirm, reject, destruction, rollback, load cleanup
one-truck/two-order tests still prove turn-taking
remote rollback releases consumer inbound reservation
producer-side revenue-before-dispatch stays resolved by the economy settlement ledger
duplicate truck DestinationArrived cannot deliver cargo twice
```

Diagram:

```text
before

  old risk:   local and remote goods paths diverge again during cleanup

after

  local:      commercial \
                         +-> routed producer -> truck -> routed delivery
  remote:     commercial /
```

### P3: Power Recheck Routing Audit

Audit result:

```text
no production move expected
target computation already lives in the directory helper
worker correctly owns transport selection through route_region_event
```

Scope:

```text
power recheck routing audit only
preserve RegionRuntime-owned candidate retry
producer allocation truth unchanged
no goods/citizen changes
no power balance formula change
```

Expected patch shape:

```text
tests/comments only; no production move unless worker owns policy beyond
calling the directory helper
```

Behavior allowed:

```text
consumer runtime keeps candidate_index continuation
producer runtime keeps grant/denial replies
worker keeps route_region_event transport selection
directory helper keeps target policy
```

Behavior forbidden:

```text
do not turn power into traveler/token simulation
do not let coordinator inspect topology or capacity
do not let worker reserve/release power
do not change producer allocation math
do not move power candidate retry out of RegionRuntime
do not bypass route_region_event
do not introduce directory -> runtime/coordinator imports
```

Design:

```text
current shape

  consumer runtime
      -> builds candidates and owns retry
      -> producer allocation
      -> applies grant or retries

  worker sweep
      -> detects changed directory publish
      -> calls directory-owned power_capacity_recheck_targets helper
      -> routes each nudge through route_region_event

target shape

  same as current, with comments/tests making the ownership explicit
```

Pseudo-code:

```rust
fn worker_publish_changed_summary(...) -> Vec<WorkerRoutedMessage> {
    if !directory.publish_region(region_id, links, hints.clone()) {
        return Vec::new();
    }

    let discovery = directory.discovery_snapshot();
    let request_id = next_worker_request_id();
    power_capacity_recheck_targets(&discovery, region_id, &hints)
        .into_iter()
        .filter_map(|target| {
            route_region_event(
                target,
                RegionEvent::PowerCapacityRecheck {
                    request_id,
                    source_region: region_id,
                },
                routing_mode,
            )
            .ok()
        })
        .collect()
}
```

Review checks:

```text
worker only calls the directory-owned target helper; it owns transport
coordinator only routes RegionRecipients
producer allocation tests remain unchanged
stale caller release still happens before producer capacity measurement
cross-caller stale reservation is eventual, not single-tick guaranteed
begin_power_export / apply_power_export_result stay in RegionRuntime
Immediate routing mode still uses local push_event
missing target behavior does not become a coordinator hard fault
```

Diagram:

```text
before

  directory publish changed
       |
       v
  worker calls directory helper for targets
       |
       v
  worker route_region_event preserves routing mode

after

  directory publish changed
       |
       v
  same flow, documented and tested
       |
       v
  no new directory -> runtime/coordinator dependency

  request/grant retry decision stays in RegionRuntime
```

## Combined Future Picture

```text
citizen:
  token arrival -> DestinationArrived -> home applies attendance

goods:
  commercial order -> producer dispatches truck -> delivery arrival -> stock apply

power:
  consumer request -> producer allocation -> consumer apply/retry

all:
  RegionRuntime emits RoutedRegionEvent
  coordinator resolves recipients
  owning RegionRuntime applies truth
```

## Test Strategy

```text
P1 citizen
  local worker reaches workplace and gets paid
  remote worker reaches workplace and gets paid
  stale duplicate DestinationArrived does not pay twice

P2 goods
  local commercial receives goods only after truck arrival
  remote commercial receives goods only after truck arrival
  one factory one truck serves two orders by turns
  duplicate truck DestinationArrived cannot deliver cargo twice
  destruction cleanup releases factory reservation and consumer inbound reservation
  load cleanup releases factory reservation and consumer inbound reservation
  rollback releases consumer inbound reservation
  same-region path emits no direct stock credit

P3 power
  changed directory publish emits PowerCapacityRecheck routes
  unchanged publish emits no recheck routes
  Immediate routing mode is preserved
  missing target does not become a coordinator hard fault
  existing grant/denial retry tests continue to pass unchanged
```

## Risks

```text
more same-region routing may add overhead
  Mitigation: use it only for ownership-boundary events, not local math loops.

event cycles can become easier to create
  Mitigation: keep coordinator drain/worker round limits and add cycle tests.

power eventual behavior can surprise tests
  Mitigation: tests assert capacity invariants and eventual retry, not exact
  same-tick winner under cross-caller contention.

goods naming may lag behavior
  Mitigation: rename export-specific goods terms only after behavior is stable.
```
