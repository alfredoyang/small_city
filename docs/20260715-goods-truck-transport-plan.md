# Goods truck transport plan

Status: **plan**. Replaces immediate factory-to-commercial goods transfer with
factory-owned trucks. Commercial stock increases only when a loaded truck
reaches the commercial building.

## Current baseline

```text
today

industrial_goods_production(factory, level)
    -> distribute_local_goods
    -> add_commercial_goods immediately

cross-region:
commercial free storage
    -> one-unit GoodsExportRequest
    -> producer-network reservation
    -> pending_goods_stock
    -> add_commercial_goods immediately
```

Current code has no factory inventory, truck entity, shipment reservation, or
arrival-gated goods transfer. `TravelToken` and `TravelerId` are citizen-shaped,
but the road mover, cross-region handoffs, coordinator routes, and road UI seam
already exist.

## Prefix patch: unify current goods requests

Before adding trucks, make local and foreign supply use the current goods-export
protocol. This is a refactor only: current stock timing and economic output stay
unchanged.

```text
commercial shortage
  -> PendingGoodsDemand
  -> GoodsExportRequest
  -> GoodsExportAllocationRequest
  -> ProcessGoodsExportRequest
  -> GoodsExportGrant

candidate producer networks
  local network first
  then connected foreign networks
```

No new request, attempt, candidate, allocation, or release type is introduced.
The local candidate is an existing `RegionRoadNetworkId` whose `region` equals
the caller region. It is delivered through the existing `ProcessGoodsExportRequest`
event to the same runtime; a foreign candidate uses the existing coordinator route.

The candidate walk keeps the current full-grant-or-deny rule. Goods requests stay
one-unit batches in P0, using distinct existing `GoodsExportRequest.token` values.
This is still the existing request/grant mechanism: the refactor only lets the
same mechanism target a local producer network before continuing to foreign
candidates.

```text
temporary settlement adapter -- removed by truck delivery

accepted local grant    -> add_commercial_goods in this goods phase
accepted foreign grant  -> pending_goods_stock, then existing next-phase apply
```

This temporary adapter is the only local/foreign distinction. It preserves the
current immediate local credit and staged foreign credit while all request,
allocation, retry, and release decisions become one path. The truck phases replace
both credit branches with arrival-gated delivery.

## Goal

```text
commercial needs goods
    |
    v
order reserves commercial inbound capacity
    |
    v
factory reserves warehouse stock + an idle truck
    |
    v
truck token follows roads and border handoffs
    |
    v
arrival confirms delivery -> commercial stock increases
    |
    v
empty truck returns to its factory
```

```text
 Factory A                         Commercial C
 +-------------------+             +-------------------+
 | warehouse: 12     |             | stored: 3 / 10     |
 | reserved: 3       | --truck-->  | inbound reserved:3 |
 | trucks: idle, out |             +-------------------+
 +-------------------+

 C storage changes from 3 to 6 only at truck arrival.
```

## Scope

```text
 included
   persistent factory warehouse inventory
   stable truck entities owned by factories
   commercial orders and endpoint reservations
   local and cross-region loaded truck trips
   delivery only at commercial arrival
   typed truck road traffic / inspect data
   save/load recovery without cargo duplication or loss

 excluded
   road congestion and travel-time traffic simulation
   one entity per individual good
   route cost / price negotiation
   worker-dependent industrial production changes
```

Initial production keeps the current balance formula:

```text
daily factory production = industrial_goods_production(factory)
```

This plan changes delivery timing, not the formula for how much a factory
produces. Making production depend on factory employment is a separate balance
change.

## Ownership

```text
factory region
  factory warehouse
  truck entities
  outbound cargo reservation
  dispatch and return decision

commercial region
  stored goods
  inbound-capacity reservation
  order identity / remaining demand

truck host region
  opaque moving token only
  no commercial stock mutation
```

```text
factory Entity
  |-- Truck Entity #0
  |-- Truck Entity #1
  `-- Truck Entity #N

idle truck      = no token, at its factory
travelling truck = one shared TravelToken keyed by truck Entity
```

Truck count comes from the existing save-stamped building-rules file. P2 extends
only the Industrial entry in the embedded ruleset and optional
`config/game_settings.json` override:

```json
{
  "buildings": {
    "Industrial": {
      "footprint_area_per_level": [1, 2, 4],
      "truck_count_per_level": [1, 2, 4]
    }
  }
}
```

`truck_count_per_level` is positive and has at least one entry for every supported
upgrade level. The embedded default `[1, 2, 4]` gives factories one, two, and four trucks
at levels 1, 2, and 3. A legacy ruleset without this optional Industrial field
uses that same default table.

Cargo and warehouse capacity continue to use building level as their size
measure:

```text
truck_count(factory)    = building_rules.industrial_truck_count(factory.level)
truck_capacity(factory) = GOODS_PER_TRUCK * max(1, factory.level)
warehouse_capacity      = GOODS_WAREHOUSE_CAPACITY * max(1, factory.level)
```

Fleet reconciliation runs after an upgrade, downgrade, load, and truck return.
It appends stable truck entities up to the configured target, then retires excess
idle trucks in descending Entity order. A loaded truck returns before it can
retire.

## Durable state

```rust
pub struct FactoryGoodsState {
    pub stored_units: i32,
    pub reserved_outbound_units: i32,
}

pub struct Truck {
    pub id: Entity,
    pub factory: Entity,
    pub cargo_capacity: i32,
    pub arrival_action: ArrivalAction,
    pub trip_generation: u32,
    pub shipment: Option<Shipment>,
}

pub enum ArrivalAction {
    StartWorkShift,
    DeliverGoods,
    ReturnHome,
}

pub struct GoodsOrderId {
    pub commercial: Entity,
    pub request_id: UiRequestId,
    pub token: u32,
}

pub struct GoodsOrder {
    pub id: GoodsOrderId,
    pub commercial: Entity,
    pub requested_units: i32,
    pub inbound_reserved_units: i32,
    pub remaining_units: i32,
}

pub struct Shipment {
    pub order: GoodsOrderId,
    pub allocation_key: ExportAllocationKey,
    pub producer_network: RegionRoadNetworkId,
    pub commercial: PlaceRef,
    pub units: i32,
}
```

`Truck.id` is the same stable non-grid identity pattern as `Citizen.id`. It is
also the key of the factory region's truck table, `World.tokens`, and
`TravelerId` while that truck is moving.

```text
factory available stock = stored_units - reserved_outbound_units
commercial orderable    = storage_capacity - stored_goods - inbound_reserved_units
```

Both values are clamped at zero. A reservation is a real economic claim, not a
display counter: an accepted grant decreases `order.remaining_units` and
increases only the reservation totals. Successful delivery consumes the factory stock, the
commercial inbound reservation, and the producer-network allocation. A rejected
or load-recovered shipment releases the producer-network allocation and factory
reservation, then restores the order's remaining units; it never adds stock a
second time.

## Shared travel token

Do not add `TruckToken`, a truck-only handoff, or a second road stepper.
Generalize the current citizen-specific payload once.

```rust
pub struct TravelerId {
    pub entity: Entity,       // citizen or truck
    pub generation: u32,
}

pub struct TravelToken {
    pub state: TravelState,   // unchanged movement payload and dwell logic
    pub home: PlaceRef,       // residence or factory
    pub kind: TravelKind,
    pub trip_generation: u32,
}

pub enum TravelKind {
    Citizen { work: Option<PlaceRef> },
    Truck { shipment: Shipment }, // moving snapshot; Truck owns the truth
}
```

```text
step_tokens(world)
  citizen -> current schedule-driven departure and work/home movement
  truck   -> dispatch-driven outbound or return target

both -> same route cache, dwell cost, border handoff, rollback guard,
        deterministic Entity-key sort, and road rendering source
```

### Shared arrival event

Truck delivery reuses the citizen arrival path. Do not add a freight queue,
direct runtime callback, or second coordinator drain.

```text
core step_tokens
  -> edge into endpoint building
  -> World.outgoing_destination_arrivals
  -> RegionState::take_pending_destination_arrivals
  -> runtime routes existing RegionEvent::DestinationArrived
  -> receiving RegionState validates and applies the arrival
```

Keep the existing pending arrival shape. The regions runtime already routes an
arrival from its traveler identity; P1 changes that identity from a citizen to a
generic owner entity.

```rust
pub struct PendingDestinationArrival {
    pub traveler: TravelerId,
    pub destination: PlaceRef,
}
```

The pending record does not store the host region. The runtime that drains the
arrival buffer passes its own `region_id` as `host_region` when routing or
handling `DestinationArrived`.

```text
Citizen reaches workplace
  route recipient = citizen Entity birth region
  -> existing attendance / salary-eligibility handler

Truck reaches commercial
  route recipient = truck Entity birth region = factory region
  -> same DestinationArrived event
  -> factory validates its owned Truck state
  -> factory sends ApplyGoodsDelivery through the same event queue
  -> commercial applies stock and retargets parked token home
```

The truck's factory owns `arrival_action`, `trip_generation`, and `shipment`,
just as a residential region owns those citizen-side commute fields. The token
only carries a shipment snapshot so host regions can move and render it.

```text
Citizen truth                         Truck truth

residential -> Citizen                factory -> Truck
  StartWorkShift                        DeliverGoods
  work_trip_generation                  trip_generation
  workplace_assignment                  shipment
```

The factory accepts a truck arrival only when its owned truck still has the
matching generation, `ArrivalAction::DeliverGoods`, and shipment destination.
It changes the action to `ReturnHome` before routing `ApplyGoodsDelivery` to the
commercial. A duplicate or stale arrival is rejected before it can create a
second delivery. A rejected arrival routes `ReturnTruckHome` to the token host,
which retargets the parked truck without changing stock. The commercial applies
delivery only after the factory has authorized it through the existing coordinator
event queue.

The current cross-region `away_residents` guard becomes `active_travelers`, which
covers both local and cross-region citizen and truck bodies. An idle owner has no
token and no active-traveler entry. The persisted citizen work-trip generation
remains the citizen attendance guard. A persisted truck trip generation is the
equivalent factory-owned stale-arrival guard.

## Unified order and dispatch

```text
daily goods phase
  1. consume commercial goods as today
  2. compute commercial free capacity excluding inbound reservations
  3. create / extend GoodsOrder
  4. create existing GoodsExportRequest batches from order.remaining_units
  5. use the existing candidate walk: local networks, then foreign networks
  6. accepted producer reserves factory stock and an idle truck; consumer marks
     the accepted batch as dispatched against its inbound reservation
  7. producer spawns one loaded truck token per accepted batch
```

```rust
fn create_goods_orders(world: &mut World) {
    for commercial in commercial_entities_in_entity_order(world) {
        let units = orderable_capacity(world, commercial);
        if units > 0 {
            reserve_inbound_capacity(world, commercial, units);
            create_or_extend_order(world, commercial, units);
        }
    }
}

fn dispatch_goods_export(request: GoodsExportRequest) {
    // Existing candidate walk; local target is routed to this runtime.
    begin_goods_export(request);
}
```

The existing producer handler selects the factory deterministically:

```text
reachable road distance, then factory Entity id, then truck Entity id
```

One commercial order may be split across factories and trucks.

```text
order C: 8 units
  factory A / truck 1: 3
  factory A / truck 2: 3
  factory B / truck 1: 2
```

## Arrival and return

```text
loaded truck reaches commercial region
  |
  v
route existing DestinationArrived to factory region
  |
  v
factory validates Truck { generation, DeliverGoods, shipment }
  |
  +-- valid   -> Truck action = ReturnHome; route ApplyGoodsDelivery
  |
  `-- invalid -> route ReturnTruckHome to token host; preserve cargo
  |
  v
commercial applies stock + consumes inbound reservation + retargets token home
  |
  +-- success -> ConfirmGoodsDelivery; consume reserved factory stock
  `-- reject  -> restore current order batch, then RejectGoodsDelivery;
                 release factory reservation without adding stock
```

```rust
fn apply_destination_arrived(
    factory: &mut RegionState,
    host_region: RegionId,
    arrived: PendingDestinationArrival,
) {
    let Some(truck) = factory.matching_truck(arrived.traveler) else {
        factory.route_return_truck_home(host_region, arrived.traveler);
        return;
    };
    let Some(shipment) = truck.shipment else {
        factory.route_return_truck_home(host_region, arrived.traveler);
        return;
    };
    if truck.arrival_action != ArrivalAction::DeliverGoods
        || shipment.commercial != arrived.destination
    {
        factory.route_return_truck_home(host_region, arrived.traveler);
        return;
    }

    truck.arrival_action = ArrivalAction::ReturnHome;
    factory.route_apply_goods_delivery(arrived.traveler, shipment);
}

fn apply_goods_delivery(commercial: &mut RegionState, delivery: GoodsDelivery) {
    let Some(order) = commercial.matching_order(delivery.order) else {
        commercial.retarget_parked_truck_home(delivery.traveler);
        commercial.route_reject_goods_delivery(delivery);
        return;
    };
    if delivery.units > order.inbound_reserved_units {
        commercial.restore_order_remaining(order.id, delivery.units);
        commercial.retarget_parked_truck_home(delivery.traveler);
        commercial.route_reject_goods_delivery(delivery);
        return;
    }
    commercial.add_commercial_goods(order.commercial, delivery.units as u32);
    commercial.consume_inbound_reservation(order.id, delivery.units);
    commercial.retarget_parked_truck_home(delivery.traveler);
    commercial.route_confirm_goods_delivery(delivery);
}

fn confirm_goods_delivery(factory: &mut RegionState, confirmation: GoodsDeliveryConfirmation) {
    let truck = factory.matching_truck(confirmation.traveler)?;
    factory.release_export_allocation(confirmation.allocation_key);
    factory.consume_reserved_stock(truck.factory, confirmation.units);
    truck.shipment = None;
}

fn reject_goods_delivery(factory: &mut RegionState, rejection: GoodsDeliveryRejection) {
    let truck = factory.matching_truck(rejection.traveler)?;
    factory.release_export_allocation(rejection.allocation_key);
    factory.release_outbound_reservation(truck.factory, rejection.units);
    truck.shipment = None;
}
```

`add_commercial_goods` is called only by this delivery path. The current local
immediate distributor and cross-region `pending_goods_stock` apply path are
removed when their corresponding truck phase becomes live.

## Producer dispatch through the existing goods-export protocol

The current protocol reserves aggregate producer-network surplus and immediately
applies `GoodsExportGrant` to consumer stock. Keep producer authority and the
same request/grant types, but turn an accepted grant into a producer truck
dispatch instead.

```text
consumer commercial
  -> reserve inbound capacity / send existing GoodsExportRequest batch
  -> candidate producer networks

producer region
  -> validate network capacity
  -> choose a reachable physical factory and idle truck
  -> reserve network capacity and factory cargo
  -> send existing GoodsExportGrant as an acknowledgement only
  -> move loaded truck across RegionEvent::ReceiveTraveler handoffs

commercial region
  -> receive ApplyGoodsDelivery only after factory validates truck arrival
```

```rust
fn process_goods_export_request(request: GoodsExportAllocationRequest) -> Option<GoodsExportGrant> {
    let export = request.request;
    let allocation_key = export.allocation_key();
    let producer_network = request.candidates[request.candidate_index];
    let factory = choose_factory(export.caller_network, export.units)?;
    let truck = first_idle_truck(factory)?;
    if export.units > truck.cargo_capacity || export.units > factory_available_stock(factory) {
        return None;
    }
    reserve_network_capacity(request, export.units)?;
    reserve_factory_stock(factory, export.units);
    dispatch_loaded_truck(factory, truck, allocation_key, producer_network, export);
    Some(GoodsExportGrant { granted: true, units: export.units, ..from(export) })
}
```

The producer chooses the physical factory. A consumer never selects or mutates a
foreign factory. A rejected producer result uses the existing candidate
continuation and token/request identity, then proceeds to the next candidate or
remains pending for a later daily phase.

`ConfirmGoodsDelivery`, `RejectGoodsDelivery`, commercial destruction, factory
destruction, and load recovery all release the matching producer-network
allocation named by the shipment's `ExportAllocationKey`. Confirmation consumes
reserved factory stock; every other terminal path releases only the factory
reservation and, when the order still exists, restores its remaining units for a
later dispatch.

All truck crossings use the existing coordinator route for `TravelerHandoff`.
The destination receives the truck on the next eligible travel step, exactly as
the current citizen token flow does.

## Road break and destruction

```text
route changes while loaded truck is outbound
  |
  +-- commercial still reachable -> reroute toward commercial
  |
  +-- factory reachable           -> retarget home, return cargo
  |
  `-- neither reachable           -> stranded with reserved cargo
                                      retry route on later travel steps
```

```text
commercial destroyed
  -> cancel inbound reservation
  -> release the matching network and factory reservations
  -> truck returns without delivery

factory destroyed
  -> reject its dispatched shipment batches
  -> release its network and factory reservations
  -> loaded truck is cancelled; cargo is not delivered
```

No path teleports goods to a commercial. A disconnected route therefore stops
further imports even if an allocation existed before the road break.

## UI

```rust
pub struct RoadTrafficView {
    pub citizens: usize,
    pub trucks: usize,
}

pub struct TruckTrafficView {
    pub source: CityCellRef,
    pub destination: CityCellRef,
    pub cargo_units: i32,
    pub returning: bool,
}
```

```text
road cell
  citizens only -> citizen marker
  trucks only   -> truck marker
  both          -> mixed marker / count
```

The adapter derives these views from `World.tokens`; TUI and ASCII consume only
`GameView` and inspect view models. The road panel may show source, destination,
cargo, and return state, but never exposes ECS objects.

## Save/load recovery

```text
persist
  factory warehouse state
  truck identities and factory ownership
  orders and endpoint reservations

transient
  TravelToken road position and handoff inbox

load reconciliation
  1. release every persisted outbound and producer-network reservation
  2. clear its commercial inbound reservation
  3. restore its order remaining when the order still exists
  4. mark the truck idle at its factory
  5. re-dispatch normally after derived roads are rebuilt
```

This restarts animation after load but cannot duplicate goods, leak a reservation,
or credit an undelivered shipment.

## Patch split

### P0: Unify current goods requests

```text
Scope
  Refactor local goods supply to use the existing goods-export request,
  allocation, grant, continuation, and release machinery.
  Candidate networks include local producer networks first, then connected
  foreign producer networks.
  Keep current timing: local accepted grants credit stock in the same goods
  phase; foreign accepted grants use pending_goods_stock and the existing
  next-phase apply.

Forbidden
  no factory warehouse, truck entity, shipment, ArrivalAction::DeliverGoods,
  or arrival-gated stock change.
  no new order-attempt, supplier-candidate, allocation, or result type.
  no change to commercial consumption, industrial production formula, or
  external-import fallback balance.

Tests
  local commercial stock result matches the pre-refactor distributor
  foreign commercial stock timing matches the pre-refactor pending_goods_stock
  local candidate is tried before foreign candidate for the same shortage
  denial from a local candidate continues to a foreign candidate
  producer-side allocation release still frees capacity
```

```text
Structures

PendingGoodsDemand                    existing consumer-side shortage record

GoodsExportRequest                    existing batch request
  request_id: UiRequestId
  caller_region: RegionId
  caller_network: RegionRoadNetworkId
  token: u32                          distinct per split batch
  units: u32                           convert at stock mutation boundary
  commercial: Entity

GoodsExportAllocationRequest          existing candidate-walk envelope
  request: GoodsExportRequest
  candidates: Vec<RegionRoadNetworkId>
  candidate_index: usize

GoodsExportGrant                      existing producer reply
  token: u32
  granted: bool
  source_region: Option<RegionId>
  units: u32

ExportAllocationKey                   existing producer reservation key
  caller_region: RegionId
  request_id: UiRequestId
  token: u32
```

```text
daily goods phase:
    demands = pending_goods_demands()

    for demand in demands.sorted_by_commercial():
        for batch in split_into_request_tokens(demand):
            request = GoodsExportRequest {
                request_id,
                caller_region,
                caller_network: demand.consumer_network,
                token: batch.token,
                units: batch.units,
                commercial: demand.commercial,
            }
            candidates = local_producer_networks(caller_region)
                       + connected_foreign_producer_networks(discovery)
            route ProcessGoodsExportRequest { request, candidates }

producer receives ProcessGoodsExportRequest:
    if available_units_for(candidate_network) >= request.units:
        reserve allocation by ExportAllocationKey for request.units
        return GoodsExportGrant { granted: true, units: request.units }
    else:
        continue to next candidate using the existing allocation request

consumer applies grant:
    request = pending request matched by request_id/token
    if grant.source_region == Some(caller_region):
        add_commercial_goods(request.commercial, grant.units) now
    else:
        pending_goods_stock.push(grant)
```

### P1: Generalize travel identity

```text
Scope
  TravelerId becomes entity-based.
  TravelToken gains TravelKind::Citizen.
  active_travelers replaces away_residents.
  CitizenArrivalAction becomes ArrivalAction.
  Existing PendingDestinationArrival and DestinationArrived routing stay the
  only arrival event path; recipient is derived from TravelerId.entity.region().

Forbidden
  no Truck variant, goods reservation, UI, or citizen behaviour change.

Tests
  existing local and cross-region citizen travel is unchanged
  stale citizen handoff cannot complete a newer trip
  citizen work arrival still routes to the citizen home region
  CitizenArrivalAction rename leaves citizen behaviour unchanged
  token stepping remains Entity-ordered
```

```text
Structures

TravelerId                         changed; transient wire identity
  entity: Entity                   citizen or truck Entity
  generation: u32                  owner-issued stale-event guard

ArrivalAction                      renamed from CitizenArrivalAction
  StartWorkShift                   citizen work arrival effect
  DeliverGoods                     factory truck delivery effect; unused until P2
  ReturnHome                       arrival effect already completed

TravelToken                        changed; transient host-side moving body
  state: TravelState               unchanged road position / dwell data
  home: PlaceRef                   residence or factory
  kind: TravelKind                 Citizen only in P1
  trip_generation: u32             copied across handoffs

TravelKind::Citizen
  work: Option<PlaceRef>           current citizen endpoint

PendingDestinationArrival          changed; transient World buffer item
  traveler: TravelerId
  destination: PlaceRef

World.active_travelers             renamed from away_residents
  HashSet<Entity>                  active local/cross-region citizen-trip guard in P1
```

```text
// Same citizen behavior; only identity and names become traveler-generic.

for traveler in tokens.sorted_by_entity() {
    step(traveler.token)
}

on_endpoint_edge(traveler, destination):
    outgoing_destination_arrivals.push({
        traveler,
        destination,
    })

runtime:
    route_destination_arrivals()
        -> RegionEvent::DestinationArrived
        -> traveler.entity.region() applies ArrivalAction::StartWorkShift
```

### P2: Local factory inventory and truck delivery

```text
Scope
  Industrial truck_count_per_level building rule; warehouse state, stable trucks,
  local orders/reservations, Truck tokens,
  arrival-gated local restock, return flow, typed road traffic UI.

Forbidden
  no foreign truck handoff.
  foreign grants still use P0 pending_goods_stock until P3.

Tests
  commercial stock remains unchanged before truck arrival
  arrival increases stock exactly once
  truck arrival is emitted through DestinationArrived, not a new queue
  factory-owned DeliverGoods action authorizes exactly one delivery event
  duplicate truck arrival cannot deliver cargo twice
  busy truck prevents a second shipment from using it
  configured truck count creates the exact factory fleet size
  two factories can satisfy one commercial order
  road disconnection prevents delivery and returns or strands cargo safely
  UI renders truck traffic without UI-to-World access
```

```text
Structures

ZoneRules                             existing shared JSON entry shape
  footprint_area_per_level: Vec<u32>
  #[serde(default)]
  truck_count_per_level: Option<Vec<u16>>
                                     absent for legacy/non-Industrial entries

BuildingRules::Industrial            existing persisted game-settings entry
  truck_count_per_level              validates positive, enough entries for levels

BuildingRules::industrial_truck_count(level) -> u16
  uses [1, 2, 4] when absent; otherwise clamps like footprint_area

BuildingData::Industrial::goods    persisted factory-owned state
  FactoryGoodsState {
    stored_units: i32
    reserved_outbound_units: i32
  }

World.trucks                        persisted, factory-region-owned
  BTreeMap<Entity, Truck>           key is Truck.id; stable Entity order

Truck                               persisted non-grid entity record
  id: Entity
  factory: Entity
  cargo_capacity: i32
  arrival_action: ArrivalAction     DeliverGoods -> ReturnHome
  trip_generation: u32
  shipment: Option<Shipment>        factory-owned shipment truth

World.goods_orders                  persisted, commercial-region-owned
  BTreeMap<GoodsOrderId, GoodsOrder>

GoodsOrderId
  commercial: Entity
  request_id: UiRequestId
  token: u32

GoodsOrder
  id: GoodsOrderId
  commercial: Entity
  requested_units: i32
  inbound_reserved_units: i32
  remaining_units: i32

Shipment                            persisted only in Truck; token carries a copy
  order: GoodsOrderId
  allocation_key: ExportAllocationKey
  producer_network: RegionRoadNetworkId
  commercial: PlaceRef
  units: i32

TravelKind::Truck                   transient token payload
  shipment: Shipment                movement / rendering snapshot, never truth

RegionEvent::ApplyGoodsDelivery     existing runtime queue, local or routed
  traveler: TravelerId
  order: GoodsOrderId
  allocation_key: ExportAllocationKey
  commercial: Entity
  units: i32

RegionEvent::ConfirmGoodsDelivery / RejectGoodsDelivery
  traveler: TravelerId
  order: GoodsOrderId
  allocation_key: ExportAllocationKey
  units: i32

RegionEvent::ReturnTruckHome        new event on the existing runtime queue
  traveler: TravelerId
  token_host: RegionId

RoadTrafficView                     UI-safe derived view
  citizens: usize
  trucks: usize

TruckTrafficView                    UI-safe truck row for road inspect
  source: CityCellRef
  destination: CityCellRef
  cargo_units: i32
  returning: bool
```

```text
daily goods phase:
    for factory in factories.sorted_by_entity():
        factory.warehouse += current_industrial_production(factory)
        factory.warehouse = min(factory.warehouse, factory.capacity)

    for commercial in commercials.sorted_by_entity():
        missing = capacity - stored - inbound_reserved
        if missing > 0:
            order.reserve_inbound(commercial, missing)
            create GoodsExportRequest batches using order.id request_id/token
            use P0 candidate walk: local candidates, then foreign candidates

local producer receives ProcessGoodsExportRequest:
    factory = nearest_reachable_factory_with_idle_truck(request)
    if no factory:
        continue to next candidate
    truck = factory.first_idle_truck()
    if request.units > truck.capacity or request.units > factory.available_stock:
        continue to next candidate
    reserve existing ExportAllocationKey
    factory.reserve_outbound(request.units)
    order = GoodsOrderId { request.commercial, request.request_id, request.token }
    truck.shipment = {
        order,
        allocation_key,
        producer_network,
        commercial: request.commercial,
        units: request.units,
    }
    truck.arrival_action = DeliverGoods
    spawn_truck_token(truck)
    return GoodsExportGrant acknowledgement only

consumer receives local GoodsExportGrant:
    record grant/release bookkeeping
    order.remaining -= grant.units as i32
    do not add stock now
    wait for ApplyGoodsDelivery after truck arrival

truck reaches commercial:
    DestinationArrived -> factory
    factory validates { truck generation, DeliverGoods, shipment }
    factory.arrival_action = ReturnHome
    enqueue ApplyGoodsDelivery(commercial, shipment)

commercial applies delivery:
    stored += shipment.units
    inbound_reserved -= shipment.units
    enqueue ConfirmGoodsDelivery(factory, truck)
    retarget parked token to factory
```

### P3: Foreign truck dispatch through goods-export grants

```text
Scope
  Foreign candidate batches use the same GoodsExportRequest,
  GoodsExportAllocationRequest, GoodsExportGrant, and ExportAllocationKey as P0.
  Producer-side factory selection, cargo/network reservation, truck handoff,
  factory-authorized ApplyGoodsDelivery event, release/cancel paths.

Forbidden
  no immediate pending_goods_stock credit after a grant.

Tests
  producer selects the factory and owns the cargo reservation
  commercial stock changes only after cross-region truck arrival
  a disconnected border releases or preserves cargo without import
  one commercial order can receive batches from multiple producer regions
```

```text
Structures

GoodsExportRequest                  existing consumer batch identity
  request_id / caller_region / caller_network / token / units / commercial

GoodsExportAllocationRequest        existing candidate-walk envelope
  request
  candidates
  candidate_index

GoodsExportGrant                    existing producer acknowledgement
  token
  granted
  source_region                    Option<RegionId>
  units

ExportAllocationKey                 retained producer-network reservation key
  caller_region: RegionId
  request_id: UiRequestId
  token: u32

TravelerHandoff                     existing transient cross-region carrier
  token: TravelToken                now may contain TravelKind::Truck
  traveler: TravelerId              truck Entity + generation
  to_region / entry_link / kind     unchanged routing fields
```

```text
commercial order has remaining units:
    create existing GoodsExportRequest batch
    route existing GoodsExportAllocationRequest through candidate walk

producer receives ProcessGoodsExportRequest:
    if network reservation or factory/truck capacity is unavailable:
        continue to next candidate or reject
    else:
        factory = producer.choose_factory()
        truck = factory.first_idle_truck()
        reserve network + factory cargo
        assign Truck.shipment
        spawn loaded token
        return existing GoodsExportGrant as acknowledgement only

consumer receives GoodsExportGrant:
    record producer/release bookkeeping
    do not add stock and do not push pending_goods_stock
    wait for ApplyGoodsDelivery after truck arrival

truck crosses border:
    token -> TravelerHandoff -> coordinator route -> ReceiveTraveler
    next eligible StepTravel continues the same token

truck arrival:
    DestinationArrived -> factory owner
    ApplyGoodsDelivery -> commercial owner
    ConfirmGoodsDelivery / RejectGoodsDelivery -> factory owner
```

### P4: Save/load and balancing

```text
Scope
  persist warehouse, truck, order, and reservation state; load recovery;
  configured truck-count fleet reconciliation and cargo-capacity balance constants.

Tests
  save during shipment does not duplicate or lose goods
  load restarts undelivered shipment from factory state
  upgrade/downgrade changes fleet size deterministically
  legacy settings without truck_count_per_level use [1, 2, 4]
```

```text
Structures

RegionStateSaveRecord               extended persisted save payload
  trucks: Vec<Truck>                 factory-owned non-grid truck records
  goods_orders: Vec<GoodsOrder>      commercial-owned outstanding orders

BuildingData::Industrial::goods     already persisted in P2
  stored_units
  reserved_outbound_units

BuildingRules                        existing save-stamped game settings
  Industrial.truck_count_per_level   persisted with the city like other rules

Not persisted
  World.tokens                       moving truck/citizen bodies
  pending handoffs
  pending destination arrivals
  queued delivery confirmations
```

```text
save:
    persist factories, trucks, warehouse, orders, and reservations
    omit moving tokens and handoff inboxes

load:
    for truck with shipment:
        release factory outbound and producer-network reservations
        clear commercial inbound reservation
        restore order remaining when the order still exists
        clear truck shipment; truck becomes idle
    rebuild roads and derived state
    reconcile every factory fleet against its configured truck count
    next goods phase dispatches remaining orders normally

factory upgrade:
    target = building_rules.industrial_truck_count(factory.level)
    append stable truck entities until target is met

factory downgrade:
    reconcile to the configured target
    retire idle excess trucks in descending truck Entity order
    leave loaded excess trucks until they return, then reconcile again
```

## Invariants

```text
commercial stock increases only at a successful truck-arrival delivery
factory available stock and commercial orderable capacity never go negative
one truck has at most one active token and one shipment
one shipment has one factory, one commercial, and one order identity
every reservation is consumed by delivery or released by cancellation/recovery
local matching and token stepping use stable Entity order
cross-region trucks use coordinator-routed TravelerHandoff messages
the UI never reads ECS World directly
```
