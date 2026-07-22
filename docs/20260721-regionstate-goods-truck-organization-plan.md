# RegionState goods truck organization plan

Status: **P1 implemented; P2 deferred unless requested.**

## Problem

`src/core/regions/mod.rs` now owns too many goods-truck helpers. The ownership is
right: factory warehouse, truck fleet, shipment truth, and commercial orders all
belong to the region's private `World`. The problem is file organization, not
architecture.

```text
current: src/core/regions/mod.rs, 4703 lines

  lines    1- 380   shared region types + constants
  lines  421-1386   RegionState commands, routes, travel handoffs, power
  lines 1388-1986   >>> goods trucks (25 methods, ~600 lines) <<<
  lines 1988-2797   jobs / employment ledger, save/load
  lines 2799-4703   tests
```

The truck block is ~21% of production code in this file and sits wedged between
`apply_power_export_grant` (power settlement) and `spare_job_slots_on_network`
(employment). Neither neighbour is related to it, so a reviewer reading power or
jobs scrolls through six hundred lines of warehouse and shipment logic to get
from one to the other.

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
no move of shared types: GoodsTruckArrival, GOODS_PER_TRUCK, and
  GOODS_WAREHOUSE_CAPACITY stay declared in mod.rs beside the other
  region types, because runtime/mod.rs imports them from crate::core::regions
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

The full move list — 25 methods, current as of `4fa2c88`. `→` marks a method
mod.rs still calls after the move; see [Visibility](#visibility) for why those
need `pub(super)`.

### Factory Inventory (mod.rs 1388-1461)

```text
    factory_entities
    factory_warehouse_capacity
    factory_goods_mut
    factory_available_goods
  → factory_goods_units_on_network      called by availability_hints
    is_effective_factory
```

Reason: these all read factory identity or warehouse state.

### Fleet (mod.rs 1463-1542)

```text
    reconcile_factory_trucks
    first_idle_truck
```

Reason: these manage stable truck entities, per-level fleet size and cargo
capacity, and idle/busy state.

### Dispatch (mod.rs 1544-1683)

```text
    choose_factory_for_shipment
    produce_factory_goods_for_daily_tick   pub
    dispatch_goods_shipment                pub(crate)
    record_goods_order                     pub(crate)
```

Reason: these turn an accepted goods allocation into producer-owned shipment
truth (warehouse reservation, truck assignment, travel token).

### Delivery (mod.rs 1716-1898)

```text
    goods_truck_shipment                   pub(crate)
    validate_goods_truck_arrival           pub(crate)
    apply_goods_delivery                   pub(crate)
    reject_goods_delivery_at_host          pub(crate)
    retarget_goods_truck_home              pub(crate)
    confirm_goods_delivery                 pub(crate)
    cancel_goods_delivery                  pub(crate)
```

Reason: these apply the arrival protocol:

```text
DestinationArrived -> ApplyGoodsDelivery -> Confirm/RejectGoodsDelivery
```

`goods_truck_shipment` is the read-only snapshot the runtime takes *before*
`receive_traveler_handoff` clears a rolled-back shipment, so it can route the
release to the consumer region. It belongs with delivery, not with cleanup.

Important boundary:

```text
apply_truck_rollback only clears producer-side shipment truth.
It cannot clear a remote consumer's GoodsOrder.

remote rollback flow:
  runtime snapshots goods_truck_shipment(traveler)
  runtime delivers/receives the Rollback handoff
  receive_traveler_handoff -> apply_truck_rollback
  runtime routes RejectGoodsDelivery to shipment.commercial.region
```

Do not "simplify" this by calling `apply_truck_rollback` directly from every
rollback site. That was the orphaned-order bug: producer cleanup succeeded while
the consumer's inbound reservation stayed forever.

### Return / Rollback (mod.rs 1685-1714)

```text
  → accepts_truck_return                 called by accepts_inbound_home_traveler
    apply_truck_return
  → apply_truck_rollback                 called by receive_traveler_handoff
```

Reason: these are the truck-side halves of the generic traveler home-arrival
guard. The generic halves stay in mod.rs (see Stay-Put Boundary).

### Cleanup / Recovery (mod.rs 1843-1986)

```text
  → clear_goods_orders_for_commercial    called by bulldoze and replace
    clear_goods_shipment_reservation
  → resume_persisted_goods_shipments     called by set_region_routes
                                         and from_world_with_employer_state
```

Reason: these release reservations when delivery cannot complete, when an
endpoint is destroyed, or when load rebuilds transient tokens.

## Patch Split

### P1: Create module and move methods

Scope:

```text
add src/core/regions/goods_trucks.rs
add `mod goods_trucks;` in src/core/regions/mod.rs
move the 25 methods listed in Function Groups as-is
widen exactly the five `→` methods to pub(super); leave every other
  visibility unchanged
keep tests in mod.rs unchanged
```

Tests need no change: the truck tests call existing public/package APIs such as
`produce_factory_goods_for_daily_tick`, `dispatch_goods_shipment`, and
`record_goods_order`, plus direct `region.world.*` reads and generic
RegionState/runtime methods that stay in mod.rs. Those remain legal because the
test module is a child of `regions` (see Visibility).

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

## Visibility

Rust module privacy is asymmetric, and this decides the whole patch:

```text
  regions (mod.rs)
      |
      +-- goods_trucks (child)

  child  -> parent : private items ARE visible   (RegionState.world, is_effective_factory)
  parent -> child  : private items are NOT visible
```

So the move is free in one direction and costs `pub(super)` in the other:

```text
free
  goods_trucks.rs reads RegionState.world directly
  goods_trucks.rs calls anything still private in mod.rs
  mod.rs tests keep reaching into region.world.trucks

costs pub(super)   -- exactly these five, because mod.rs calls them
  factory_goods_units_on_network      <- availability_hints
  accepts_truck_return                <- accepts_inbound_home_traveler
  apply_truck_rollback                <- receive_traveler_handoff
  clear_goods_orders_for_commercial   <- bulldoze, replace
  resume_persisted_goods_shipments    <- set_region_routes,
                                         from_world_with_employer_state
```

Prefer `pub(super)` over `pub(crate)` for all five: the callers are all in the
parent module, so `pub(crate)` would advertise them to `runtime/mod.rs` and the
worker, which have no business calling them.

## Suggested File Layout

```rust
// goods_trucks.rs
use super::{GoodsTruckArrival, RegionRoadNetworkId, RegionState, GOODS_PER_TRUCK, ...};

impl RegionState {
    // Factory inventory
    fn factory_entities(&self) -> Vec<Entity> { ... }
    fn factory_warehouse_capacity(&self, factory: Entity) -> i32 { ... }
    fn factory_goods_mut(&mut self, factory: Entity) -> Option<&mut FactoryGoodsState> { ... }
    fn factory_available_goods(&self, factory: Entity) -> i32 { ... }
    pub(super) fn factory_goods_units_on_network(&self, network: RegionRoadNetworkId) -> u32 { ... }
    fn is_effective_factory(&self, factory: Entity) -> bool { ... }

    // Fleet
    fn reconcile_factory_trucks(&mut self) { ... }
    fn first_idle_truck(&self, factory: Entity, units: i32) -> Option<Entity> { ... }

    // Dispatch
    fn choose_factory_for_shipment(...) -> Option<Entity> { ... }
    pub fn produce_factory_goods_for_daily_tick(&mut self) { ... }
    pub(crate) fn dispatch_goods_shipment(...) -> GoodsSupplyGrant { ... }
    pub(crate) fn record_goods_order(...) { ... }

    // Delivery
    pub(crate) fn goods_truck_shipment(&self, traveler: TravelerId) -> Option<Shipment> { ... }
    pub(crate) fn validate_goods_truck_arrival(...) -> Option<GoodsTruckArrival> { ... }
    pub(crate) fn apply_goods_delivery(...) -> bool { ... }
    pub(crate) fn reject_goods_delivery_at_host(...) { ... }
    pub(crate) fn retarget_goods_truck_home(&mut self, traveler: TravelerId) { ... }
    pub(crate) fn confirm_goods_delivery(&mut self, traveler: TravelerId, units: i32) { ... }
    pub(crate) fn cancel_goods_delivery(&mut self, traveler: TravelerId, units: i32) { ... }

    // Return / rollback
    pub(super) fn accepts_truck_return(&self, traveler: TravelerId) -> bool { ... }
    fn apply_truck_return(&mut self, traveler: TravelerId) { ... }
    pub(super) fn apply_truck_rollback(&mut self, traveler: TravelerId) { ... }

    // Cleanup / recovery
    pub(super) fn clear_goods_orders_for_commercial(&mut self, commercial: Entity) { ... }
    fn clear_goods_shipment_reservation(...) { ... }
    pub(super) fn resume_persisted_goods_shipments(&mut self) { ... }
}
```

## Explicit Stay-Put Boundary

These stay in `mod.rs`. They are generic traveler or region-command logic that
happens to branch on "is this entity a truck":

```text
resolve_pending_traveler_handoffs     emits the self-addressed truck Rollback
receive_traveler_handoff              dispatches to apply_truck_rollback
bounce_to_home
accepts_inbound_home_traveler         dispatches to accepts_truck_return
bulldoze / replace                    call clear_goods_orders_for_commercial
set_region_routes                     calls resume_persisted_goods_shipments
from_world_with_employer_state        calls resume_persisted_goods_shipments
availability_hints                    calls factory_goods_units_on_network
```

They may call truck helpers from `goods_trucks.rs`, but they should not move in
this refactor. Note the seam has grown since this plan was first written:
generic traveler code, command code, hint publishing, and save/load now all call
into the goods-truck helper block. `set_region_routes` in particular is a
route-installation setter that also resumes persisted shipments after load. That
coupling is deliberate and documented at the call site — do not "clean it up"
while moving files.

Rollback detail:

```text
resolve_pending_traveler_handoffs
  stale producer exit for local-home truck
    -> emit self-addressed HandoffKind::Rollback
    -> runtime routes ReceiveTraveler back to this region
    -> runtime snapshots goods_truck_shipment before receive_traveler_handoff
    -> receive_traveler_handoff calls apply_truck_rollback
    -> runtime routes RejectGoodsDelivery to the consumer
```

The self-addressed rollback looks odd, but it keeps every rollback door on the
same runtime-owned release path. If this is changed, add or preserve a test for
remote shipment rollback after the producer border road is deleted before
crossing.

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
private helper visibility widened more than needed (pub(crate) where
  pub(super) suffices -- see Visibility)
tests accidentally moved into production helper APIs
imports become broad and hide dependencies
the move lands while goods-truck behavior is still being fixed, so the
  diff stops being reviewable as a pure move
```

Sequencing: this refactor is only worth doing on a quiet base. If goods-truck
behavior work is still open, finish it first — a mechanical move mixed with a
behavior fix defeats the one review check that makes this patch cheap.

Mitigation:

```text
use mechanical move first
run cargo fmt
run cargo clippy -- -D warnings
run cargo test -q
review diff with whitespace ignored if needed
```
