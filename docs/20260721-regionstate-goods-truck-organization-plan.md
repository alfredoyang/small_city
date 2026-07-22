# RegionState goods truck organization plan

## Problem

`src/core/regions/mod.rs` now owns too many goods-truck helpers. The ownership is
right: factory warehouse, truck fleet, shipment truth, and commercial orders all
belong to the region's private `World`. The problem is file organization, not
architecture.

```text
current:

  regions/mod.rs
    RegionState core commands
    discovery summaries
    employment ledger helpers
    goods truck fleet helpers
    goods shipment dispatch
    goods delivery confirmation
    save/load repair helpers
    tests
```

The truck functions are hard to review because they sit between unrelated
`RegionState` behavior.

## Goal

Move goods truck and goods shipment methods into a focused module while keeping
the same owner, signatures, behavior, and tests.

```text
target:

  src/core/regions/mod.rs
    declares RegionState
    declares shared region types
    keeps non-truck region behavior

  src/core/regions/goods_trucks.rs
    impl RegionState {
      factory inventory helpers
      fleet reconciliation
      shipment dispatch
      delivery / rollback / recovery
    }
```

## Non-goals

```text
no behavior changes
no new TruckManager / service object
no new actor, queue, or coordinator route
no change to local vs remote shipment semantics
no change to save format
no UI access to World
no test expectation updates except module paths if needed
```

## Ownership

```text
RegionState still owns World
        |
        +-- factories
        +-- trucks
        +-- goods_orders
        +-- travel tokens
        +-- shipment reservations

goods_trucks.rs is only an impl block location.
It does not own state.
```

## Function Groups

### Factory Inventory

```text
factory_entities
factory_warehouse_capacity
factory_goods_mut
factory_available_goods
factory_goods_units_on_network
produce_factory_goods_for_daily_tick
```

Reason: these all read or mutate factory warehouse state.

### Fleet

```text
reconcile_factory_trucks
first_idle_truck
```

Reason: these manage stable truck entities and idle/busy state.

### Dispatch

```text
choose_factory_for_shipment
dispatch_goods_shipment
record_goods_order
```

Reason: these turn accepted goods allocation into producer-owned shipment truth.

### Delivery

```text
validate_goods_truck_arrival
apply_goods_delivery
confirm_goods_delivery
cancel_goods_delivery
```

Reason: these apply the arrival protocol:

```text
DestinationArrived -> ApplyGoodsDelivery -> Confirm/RejectGoodsDelivery
```

### Cleanup / Recovery

```text
clear_goods_shipment_reservation
apply_truck_rollback
resume_persisted_goods_shipments
```

Reason: these release reservations when delivery cannot complete or when load
rebuilds transient tokens.

## Patch Split

### P1: Create module and move methods

Scope:

```text
add src/core/regions/goods_trucks.rs
add `mod goods_trucks;` in src/core/regions/mod.rs
move goods-truck-related impl RegionState methods as-is
keep method visibility unchanged
keep tests unchanged
```

Pseudo-code:

```rust
// src/core/regions/mod.rs
mod goods_trucks;

pub struct RegionState {
    world: World,
    ...
}

// src/core/regions/goods_trucks.rs
impl RegionState {
    pub(crate) fn dispatch_goods_shipment(...) -> GoodsSupplyGrant {
        // moved body, no logic change
    }
}
```

Review checks:

```text
git diff shows moved code, not rewritten logic
no signature changes except private helper placement
no new abstractions
no behavior tests changed
cargo test -q passes
```

### P2: Deferred optional test helper cleanup

Scope:

```text
do not implement unless requested separately
only if P1 leaves duplicated runtime-test movement loops hard to read
move repeated test-only truck-delivery stepping into local test helpers
no production changes
```

Pseudo-code:

```rust
#[cfg(test)]
fn collect_destination_arrivals(runtime: &mut RegionRuntime, expected: usize) -> Vec<RegionEvent> {
    for step in 1..=32 {
        ...
    }
}
```

Review checks:

```text
P2 is absent from the P1 diff unless explicitly requested
test helper lives in the test module
helper does not hide different behavior paths
no production code touched in P2
focused tests still describe the scenario
```

## Suggested File Layout

```rust
// goods_trucks.rs
use super::{RegionRoadNetworkId, RegionState, GOODS_PER_TRUCK, ...};

impl RegionState {
    // Factory inventory
    fn factory_entities(&self) -> Vec<Entity> { ... }
    fn factory_warehouse_capacity(&self, factory: Entity) -> i32 { ... }
    fn factory_goods_mut(&mut self, factory: Entity) -> Option<&mut FactoryGoodsState> { ... }
    fn factory_available_goods(&self, factory: Entity) -> i32 { ... }
    pub(crate) fn produce_factory_goods_for_daily_tick(&mut self) { ... }

    // Fleet
    fn reconcile_factory_trucks(&mut self) { ... }
    fn first_idle_truck(&self, factory: Entity, units: i32) -> Option<Entity> { ... }

    // Dispatch
    fn choose_factory_for_shipment(...) -> Option<Entity> { ... }
    pub(crate) fn dispatch_goods_shipment(...) -> GoodsSupplyGrant { ... }
    pub(crate) fn record_goods_order(...) { ... }

    // Delivery
    pub(crate) fn validate_goods_truck_arrival(...) -> Option<Shipment> { ... }
    pub(crate) fn apply_goods_delivery(...) -> bool { ... }
    pub(crate) fn confirm_goods_delivery(...) { ... }
    pub(crate) fn cancel_goods_delivery(...) { ... }

    // Cleanup / recovery
    fn clear_goods_shipment_reservation(...) { ... }
    fn apply_truck_rollback(...) { ... }
    fn resume_persisted_goods_shipments(&mut self) { ... }
}
```

Keep `resume_persisted_goods_shipments` private if possible. It is called by
the save/load rebuild code that remains in `mod.rs`; only widen it to
`pub(super)`/`pub(crate)` if the compiler requires it after the mechanical move.

## Explicit Stay-Put Boundary

These methods stay in `mod.rs` because they are general traveler handoff logic,
not goods-truck logic:

```text
resolve_pending_traveler_handoffs
receive_traveler_handoff
bounce_to_home
accepts_inbound_home_traveler
```

They may call truck helpers from `goods_trucks.rs`, but they should not move in
this refactor.

## Diagrams

### Before

```text
regions/mod.rs
  |
  +-- RegionState command methods
  +-- route/discovery helpers
  +-- employment helpers
  +-- goods truck helpers
  +-- save/load helpers
  +-- tests
```

### After

```text
regions/mod.rs
  |
  +-- RegionState definition
  +-- non-truck RegionState impls
  +-- mod goods_trucks

regions/goods_trucks.rs
  |
  +-- impl RegionState
        |
        +-- factory inventory
        +-- fleet
        +-- dispatch
        +-- delivery
        +-- cleanup / recovery
```

### Behavior Boundary

```text
runtime event
    |
    v
RegionRuntime
    |
    v
RegionState::dispatch_goods_shipment / confirm_goods_delivery
    |
    v
World mutation

Only the file location changes.
The call graph and ownership stay the same.
```

## Risks

```text
accidental behavior change while moving code
private helper visibility widened more than needed
tests accidentally moved into production helper APIs
imports become broad and hide dependencies
```

Mitigation:

```text
use mechanical move first
run cargo fmt
run cargo clippy -- -D warnings
run cargo test -q
review diff with whitespace ignored if needed
```
