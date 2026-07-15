# Goods truck transport plan

Status: **plan**. Adds a visible truck-transport layer over the *existing*
factory-to-commercial goods flow. Trucks mirror the transfers immediate
distribution already performs — they carry a **display cargo** and touch no
stock, so the economy stays byte-for-byte identical to today. Builds on
[travel-subtick-plan.md](travel-subtick-plan.md) and the existing cross-region
goods allocation path. Real cargo reservation, arrival-gated restock (commercial
restocks only when a truck arrives), and citizen wages-on-arrival are a separate
**future** plan.

## Goal

```text
 factory owns trucks and stock (stock grows with workers x building size)
 trucks move visibly on the road graph; the live economy is unchanged
 commercial keeps immediate restock this plan (arrival-gating is future)
 trucks use the same roads, dwell costs, and border barriers as citizens
 a road break never teleports cargo to its destination
```

```text
 Factory                                      Commercial
 +---------------+                         +----------------+
 | stock: 12     | -- truck, cargo 3 ---> | stock: +3       |
 | idle trucks:2 |                         | sells on arrival|
 +---------------+ <--- empty truck ------ +----------------+
```

## Staging

This plan is the transport + inventory layer only. The arrival-gated economics
in the illustrations throughout this doc (goods delivered on truck arrival,
"sells on arrival", "stock changes only on arrival", cross-region "order/dispatch
not an immediate grant") describe the **future** arrival-economics plan and are
kept here as forward context. This plan keeps the existing immediate restock and
adds only:

```text
 real factory inventory (workers x size) feeding immediate distribution
 visible truck movement + typed road-traffic UI
 the generalized one-token travel machinery (P1)
```

so the live economy is byte-for-byte unchanged from today.

## Non-goals

```text
 no one-entity-per-good simulation
 no traffic congestion or road capacity in the first version
 no citizen work-schedule behaviour on trucks
 no UI access to World or region runtime state
 no arrival-gated restock this plan (commercial keeps immediate restock)
 no citizen wages-on-arrival (both land in the future arrival-economics plan)
```

## Ownership

```text
 Factory region owns:
   factory inventory (accrues from workers x building size; capped; persisted)
   Truck entities belonging to its factories
   (real outbound cargo reservation is future — this plan's cargo is display-only)

 Commercial region owns:
   commercial inventory
   demand request

 Truck in transit:
   origin truck Entity remains authoritative
   host region holds only an opaque token while the truck is visiting
```

```text
 factory Entity
     | owns N truck Entities (building level decides N and capacity)
     v
 Truck { factory, cargo_capacity }
     |
     +- no token -> idle at factory
     `- TravelToken::Truck { cargo } -> away
```

`Truck` is like a citizen belonging to a residential building. An idle truck is
at its factory. A shared `TravelToken` is the travelling body for either a
citizen or a truck.

## Factory inventory

The one economically real addition this plan; the truck layer on top of it is
display-only (see Staging).

```text
 inventory_cap(factory) = f(building footprint / size)
 production_per_tick    = per_worker_output * workers_at(factory)
 inventory += production_per_tick each economy tick, clamped to inventory_cap
```

- `workers_at(factory)` is the employed headcount assigned to the factory
  (local + remote, from the employment ledger) — derived, deterministic. A
  factory with no workers accrues nothing and supplies nothing.
- The existing immediate `distribute_local_goods` is repointed to draw from
  inventory instead of an abstract production rate, so supply is now capped by
  what the factory actually stocked.
- Persisted in the region save record and rebuilt on load, like commercial
  stock.

Decision (P2): trucks carry a **display quantity** that mirrors the transfer
immediate distribution just performed — they touch no stock and add nothing on
arrival, so the economy is byte-for-byte unchanged. Real cargo reservation and
arrival-gated delivery are deferred to the future arrival-economics plan.

## One travel token

Generalize the existing `TravelToken`, `TravelerId`, `away_residents`, and
`TravelerHandoff`. Do not add a parallel truck token, truck handoff, or road
stepper.

```rust
pub struct Truck {
    pub factory: Entity,
    pub cargo_capacity: i32,
}

pub struct TravelerId {
    pub entity: Entity,
    pub generation: u32,
}

pub struct TravelToken {
    pub state: TravelState,
    pub home: PlaceRef,
    pub trip_gen: u32,
    pub kind: TravelKind,
}

pub enum TravelKind {
    Citizen { work: Option<PlaceRef> },
    Truck { cargo: Option<GoodsCargo> },
}

pub struct TravelerHandoff {
    pub traveler: TravelerId,
    pub token: TravelToken,
    pub to_region: RegionId,
    pub entry_link: Option<BorderLinkId>,
    pub kind: HandoffKind,
}

pub struct GoodsCargo {
    pub source: Entity,
    pub destination: PlaceRef,
    pub units: i32,
}
```

```text
 World token storage

 tokens: HashMap<Entity, TravelToken>
 away_travelers: HashSet<Entity>
 away_generation: HashMap<Entity, u32>

 token.kind = Citizen { work }  -> resident schedule controls departure
 token.kind = Truck { cargo }   -> factory dispatch controls departure

away_travelers is cross-region-only. A local truck trip has a token but is
not away; a truck crossing a border enters away_travelers until it returns to
its owning factory region.

away_generation remains the monotonic stale-handoff guard for both citizens
and trucks.
```

`TravelState` remains unchanged and drives both kinds of road movement:

```text
 current_cell / destination / building / dwell / prev_cell
       |
       +-- route cache lookup
       +-- one sub-tick road step
       +-- border crossing through TravelerHandoff
```

Arrival and cleanup branch only after common movement:

```text
 Citizen -> arrive at work, or arrive home and remove token
 Truck   -> unload at commercial, then retarget home; arrive home and remove token
```

`TravelStatus::AtWork` means "parked at a building" for a truck. Keep that
internal name initially; rename it only if a later change needs a neutral name.
Citizen schedule semantics remain unchanged.

```text
 existing citizen-only flow             generalized flow
 --------------------------             ----------------
 TravelToken { home, work }             TravelToken { home, kind }
 TravelerId { citizen, generation }     TravelerId { entity, generation }
 away_residents                          away_travelers
 TravelerHandoff                          TravelerHandoff
```

Keep the existing determinism mechanism: collect token Entity keys, sort them,
then step in that order. Do not rely on `HashMap` iteration order or change the
map solely to obtain ordering.

```rust
let mut token_ids: Vec<Entity> = world.tokens.keys().copied().collect();
token_ids.sort_unstable_by_key(|entity| entity.0);
for entity in token_ids {
    step_token(entity, world);
}
```

## Cargo lifecycle

```text
 1. immediate distribution moves N units factory -> commercial (as today)
 2. choose an idle truck for that factory deterministically
 3. create TravelToken::Truck with a display cargo of N units (no stock touched)
 4. spawn the token at the factory entry road
 5. move one road step per travel sub-tick
 6. on arrival: clear the display cargo (no stock change — already delivered)
 7. retarget the empty truck to its factory
 8. on factory arrival: remove token; truck is idle at its factory
```

```rust
fn depart_truck(factory: Entity, commercial: PlaceRef, units: i32) {
    let Some(truck) = first_idle_truck(factory) else { return };
    // Display cargo mirrors the transfer immediate distribution just made;
    // no stock is reserved or moved here.
    let cargo = units.min(truck.cargo_capacity);
    spawn_travel_token(
        truck,
        factory,
        TravelKind::Truck { cargo: Some(GoodsCargo { .. }) },
    );
}

fn arrive_truck(token: &mut TravelToken) {
    let TravelKind::Truck { cargo } = &mut token.kind else { return };
    if cargo.take().is_none() {
        return;
    }
    // Visual-only: the goods were already delivered immediately, so arrival
    // adds nothing. The future plan replaces this with add_commercial_goods.
    token.state.destination = Some(token.home.building);
}
```

The display cargo is bookkeeping for rendering only; it is never usable or
double-counted at either endpoint. The future plan turns it into a real
reservation that is consumed at commercial arrival.

## Road break policy

```text
 road changes
     |
     v
 next truck sub-tick cannot find a route
     |
     +- destination still reachable by another route -> reroute and continue
     `- destination unreachable -> turn around and drive back to its factory
```

The truck carries only display cargo, so a road break has no economic effect —
it simply reroutes or drives home. When the future plan gives trucks real cargo,
this same turn-around path returns that stock to the factory on arrival (never
cargo loss); a later disaster system may explicitly destroy cargo instead.

## UI

The map exposes typed road traffic, not only one traveller count.

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
 road cell rendering

 citizens = 2, trucks = 0  -> citizen marker
 citizens = 0, trucks = 1  -> truck marker
 citizens > 0, trucks > 0  -> mixed-traffic marker / count
```

The UI reads only `GameView` and inspect view models. A road inspect panel can
show truck source, destination, cargo, and return state; it never queries the
ECS directly.

## Cross-region trip

```text
 producer factory                 border               consumer commercial
      truck token -- step --> TravelerHandoff -- step --> arrival (no stock)
                              barrier
```

Use the same all-region travel barrier as citizen handoffs:

```text
 step every region
   -> collect TravelerHandoff for citizens and trucks
   -> sort by stable order key
   -> deliver to next-region inbox
   -> next sub-tick moves the received token
```

Keep the existing `HandoffKind`:

```text
 Move     normal border crossing, including an empty truck returning to factory
 Rollback destination cannot accept the crossing
            citizen: existing return handling
            truck: preserve cargo, retarget its factory, and continue/return there
```

Visual-only this plan: the cross-region goods economics still run immediately
over the existing allocation path; the truck only animates that flow across the
border and grants no stock on arrival. The future plan makes the producer own a
real freight reservation until commercial arrival (order/dispatch, not an
immediate grant).

## Findings To Resolve Before Implementation

These are unresolved design conflicts, not implementation tasks. Do not start
P1 until each has an agreed resolution.

```text
 F1. Economy scope conflicts with factory inventory.

     "byte-for-byte unchanged goods economy" conflicts with persisted,
     worker-dependent, capped factory inventory feeding distribution.

     Decide one:
       A. visual-only trucks; leave current production/distribution untouched
       B. arrival/inventory economics; explicitly accept an economy change

 F2. Existing Rollback cannot return a truck with cargo.

     Current PendingHandoff::Rollback carries only TravelerId; its receiver
     clears the citizen's away state and drops the trip. It cannot preserve a
     truck token/cargo or retarget it to the factory.

     Decide one:
       A. add a truck rollback payload and truck-specific receive behavior
       B. retarget the truck to its factory and use normal Move crossings only

 F3. Cross-region grants do not name a source factory.

     GoodsExportGrant names only source region + units. Allocation and economy
     consume network/region-wide capacity, so P3 cannot choose a physical
     producer factory for the display trip.

     Decide one:
       A. producer records deterministic factory allocation per granted batch
       B. define a deterministic visual-source factory independent of allocation

 F4. One truck cannot mirror every immediate transfer.

     A transfer can exceed truck capacity or occur while every truck is busy.
     The current P2 pseudocode truncates display cargo and drops the remainder.

     Decide one:
       A. split and queue display batches
       B. permit unanimated excess delivery and state that limit explicitly
```

## Patch split

### P1: Generalize travel token

```text
 Scope
   TravelToken gains TravelKind::Citizen; TravelerId becomes entity-based;
   away_residents becomes away_travelers; explicit Entity-key sorting stays.

 Forbidden
   no Truck variant, no cargo, no changed citizen movement or UI behaviour.

 Tests
   existing citizen local/cross-region travel behavior is unchanged
   stale citizen handoff still cannot clear a newer trip
   token stepping order remains deterministic
```

```rust
// Same inputs and same citizen output as before.
for entity in sorted_token_ids(world) {
    let token = world.tokens.get_mut(&entity).expect("key collected above");
    let TravelKind::Citizen { work } = &mut token.kind else { continue };
    step_citizen_token(entity, token, work, world);
}

fn accept_handoff(handoff: TravelerHandoff) {
    accept_citizen_handoff(handoff);
}
```

### P2: Local trucks, delivery, and UI

```text
 Scope
   factory inventory (workers x size) feeding immediate distribution;
   truck entities from factory level; local dispatch; TravelKind::Truck
   movement; local truck return; typed road traffic UI.

 Forbidden
   no border handoff; no citizen travel behaviour change;
   no arrival-gated restock (trucks add no commercial stock this plan).

 Tests
   commercial economy is unchanged from the pre-truck baseline
   a factory with no workers accrues no inventory and supplies no goods
   factory inventory persists across save/load
   truck follows road route and dwell costs
   disconnected route sends the truck back to its factory
   map and road inspect show the truck without UI-to-World access
```

```rust
fn dispatch_local_goods(factory: Entity, commercial: Entity, delivered: i32) {
    let Some(truck) = first_idle_truck(factory) else { return };
    // `delivered` = units immediate distribution just moved; the truck only
    // mirrors it for display. No stock is reserved.
    let units = delivered.min(truck.cargo_capacity);
    world.tokens.insert(truck, TravelToken {
        home: PlaceRef::local(factory),
        state: depart_from(factory),
        kind: TravelKind::Truck { cargo: Some(GoodsCargo::to(commercial, units)) },
        ..new_trip()
    });
}

fn on_arrival(token: &mut TravelToken) {
    match &mut token.kind {
        TravelKind::Citizen { .. } => finish_citizen_arrival(token),
        // Visual-only: drop the display cargo and head home; add no stock.
        TravelKind::Truck { cargo } if cargo.is_some() => {
            token.kind = TravelKind::Truck { cargo: None };
            retarget_to_home(token);
        }
        TravelKind::Truck { cargo: None } => finish_truck_return(token),
    }
}

fn road_traffic(tokens: &HashMap<Entity, TravelToken>, cell: Entity) -> RoadTrafficView {
    count_citizens_and_trucks(tokens.values(), cell)
}
```

### P3: Cross-region truck trips (visual-only)

```text
 Scope
   route the truck display token across a border via TravelerHandoff and the
   shared barrier; animate remote delivery. Cross-region goods economics keep
   running immediately over the existing allocation path.

 Forbidden
   never add consumer stock on border crossing or arrival (display-only);
   no real cargo reservation (future arrival-economics plan).

 Tests
   a truck token crosses one region boundary one sub-tick at a time
   disconnected border drives the truck back toward its factory
   cross-region goods economy is unchanged from the pre-truck baseline
```

```rust
fn cross_out(token: TravelToken, exit: BorderLinkId) -> TravelerHandoff {
    TravelerHandoff { token, entry_link: Some(exit), .. }
}

fn receive_handoff(handoff: TravelerHandoff) {
    // Shared barrier already orders this with citizen handoffs.
    install_token_at_border_entry(handoff);
}

// Visual-only: arrival drops the display cargo and heads home; no stock grant.
// The future plan replaces this with add_commercial_goods at the destination.
TravelKind::Truck { cargo: Some(_) } if at_destination(token)
    => { token.kind = TravelKind::Truck { cargo: None }; retarget_to_home(token); }
```

### P4: Throughput and congestion (later)

```text
 Scope
   truck count/capacity balance; optional road throughput queues.

 Tests
   factory level changes dispatch throughput deterministically
   queue order is stable by truck Entity
```

```rust
for truck in idle_trucks(factory).take(factory_dispatch_limit()) {
    dispatch_next_demand(factory, truck);
}
```

## Invariants

```text
 the live goods economy is unchanged from the pre-truck baseline this plan
 trucks carry display cargo only and grant no stock on arrival this plan
 factory inventory never exceeds its cap and never goes negative
 one truck has at most one token and one cargo batch
 one cargo batch has one destination
 every local decision is ordered by stable Entity ids
 all cross-region truck handoffs pass through the existing barrier
```
