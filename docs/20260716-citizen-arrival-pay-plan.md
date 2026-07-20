# Citizen arrival-pay plan

Status: **P1/P2 implemented; P3 deferred**.

## Goal

```text
assigned job != paid job

citizen must physically reach assigned workplace
  -> home region records attendance
  -> daily payroll pays that citizen

cannot reach workplace
  -> no attendance
  -> no salary
  -> job lease stays valid unless existing loss rules end it
```

```text
home region                         token host region
-----------                         -----------------
Citizen purpose                     travel token moves
attendance                          detects destination
payroll                             routes DestinationArrived
```

## Ownership

```text
Citizen home region owns:
  workplace_assignment
  CitizenArrivalAction
  work-trip generation
  attendance since the last daily settlement
  salary payment

Token host region owns:
  road position, dwell, route endpoints
  carries the trip stamp without owning it
  no salary or attendance mutation
```

The token retains navigation endpoints (`home`, `work`) because a host region
needs them to move a visiting citizen. It does not own the meaning of arrival.

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CitizenArrivalAction {
    StartWorkShift,
    ReturnHome,
}

pub struct DestinationArrived {
    pub traveler: TravelerId,
    pub destination: PlaceRef,
}
```

`CitizenArrivalAction` is intentionally small. The home region obtains the
workplace and salary from the citizen's existing `workplace_assignment`.

The action and trip stamp are separate citizen fields:

```rust
pub struct Citizen {
    // Existing fields omitted.
    pub arrival_action: CitizenArrivalAction,
    pub work_trip_generation: u32,
    pub attended_since_daily_settlement: bool,
}
```

```text
new/unemployed/loaded citizen      ReturnHome
home starts a work commute         increment work_trip_generation
                                  + StartWorkShift
work arrival accepted              attended_since_daily_settlement = true
                                  + ReturnHome
job cleared or lost                ReturnHome
```

Every work departure mints a new generation, including a local commute. A
cross-region handoff carries that same generation in its existing `TravelerId`.
The existing home-phase return trip remains travel-only in P1; it does not emit
an arrival event or mutate salary state.

There is one counter, not two:

```text
Citizen.work_trip_generation
  authoritative home-side generation for local and cross-region work trips
  persisted with Citizen
  incremented when the home region creates the work token
  copied into TravelToken.trip_gen and TravelerId.generation

World.away_generation
  removed by P1
  cross-out no longer increments a generation

World.away_residents
  transient active-commute state for local and cross-region travelers
  work-token creation inserts the citizen
  validated home arrival, or ghost cleanup for a vanished citizen, removes it

home_accepts
  requires away_residents membership
  compares TravelerId.generation with Citizen.work_trip_generation
  is used by local home arrival and cross-region return
```

The current `trip_gen_for_home` helper is split deliberately:

```text
work-token creation
  increment Citizen.work_trip_generation
  create TravelToken { trip_gen: citizen.work_trip_generation }
  insert World.away_residents

cross-out
  emit TravelerId with the token's unchanged trip_gen

home return
  require away_residents + matching Citizen.work_trip_generation
  applies to local token arrival and cross-region handoff/rollback
  remove away_residents on acceptance
```

```rust
fn home_accepts(world: &World, traveler: TravelerId) -> bool {
    world.citizens.contains_key(&traveler.citizen)
        && world.away_residents.contains(&traveler.citizen)
        && world.citizens[&traveler.citizen].work_trip_generation
            == traveler.generation
}
```

```rust
if home_accepts(world, traveler) {
    world.tokens.remove(&traveler.citizen); // local path only
    world.away_residents.remove(&traveler.citizen);
} else {
    // A rejected local completion must not leave a parked home token forever.
    world.tokens.remove(&traveler.citizen); // local path only
    if !world.citizens.contains_key(&traveler.citizen) {
        world.away_residents.remove(&traveler.citizen);
    }
}
```

The cross-region return path never removes a local token. `home_accepts` does
not require token absence because the valid local completion has one. The old
absence check guarded a repeated cross-region return while its matching token
was walking home; with the unified guard, that return cannot remove the token,
and the eventual local completion either validates or drops the stale token.

## Arrival flow

```text
home starts work trip generation G
        |
        v
token reaches workplace in local or remote region
        |
        v
core travel step records PendingDestinationArrival in World
        |
        v
regions layer drains it and routes
RoutedRegionEvent { One(traveler.citizen.region()), DestinationArrived }
        |
        v
RegionEventCoordinator wakes the home worker
        |
        v
home validates purpose + assignment + destination
        |
        v
attendance = true since the last daily settlement
```

Local and remote arrival use the same coordinator path:

```text
local:   host region -> coordinator -> home-region FIFO
remote:  host region -> coordinator -> home-region FIFO
```

```rust
fn apply_destination_arrived(home: &mut RegionState, event: DestinationArrived) {
    let citizen = home.citizen(event.traveler.citizen)?;
    let assignment = citizen.workplace_assignment?;

    if citizen.arrival_action != CitizenArrivalAction::StartWorkShift {
        return;
    }
    if citizen.work_trip_generation != event.traveler.generation {
        return;
    }
    if assignment.workplace != event.destination.building {
        return;
    }

    citizen.attended_since_daily_settlement = true;
    citizen.arrival_action = CitizenArrivalAction::ReturnHome;
}
```

Repeated or stale arrival events are harmless: the action and generation guard
reject them after the first accepted arrival.

## Delivery

`DestinationArrived` is an ordinary `RegionEvent` inside the existing
`RoutedRegionEvent` envelope. The sender derives
`One(traveler.citizen.region())` and the coordinator wakes that worker.

The core travel system does not construct `OutboundMessage` or import a runtime
type. It records a value-only pending fact in `World`, just as it already does
for `PendingHandoff`; the regions layer drains that buffer and calls the route
helper.

```rust
pub struct PendingDestinationArrival {
    pub traveler: TravelerId,
    pub destination: PlaceRef,
}

// World, transient like outgoing_handoffs.
pub(crate) outgoing_destination_arrivals: Vec<PendingDestinationArrival>,

impl RegionState {
    fn drain_destination_arrivals(&mut self) -> Vec<PendingDestinationArrival> {
        std::mem::take(&mut self.world.outgoing_destination_arrivals)
    }
}

fn drained_destination_arrival_messages(
    runtime: &mut RegionRuntime,
) -> Vec<OutboundMessage> {
    runtime.state.drain_destination_arrivals()
        .into_iter()
        .map(|arrival| route_destination_arrived(arrival.traveler, arrival.destination))
        .collect()
}
```

The `RegionEvent::StepTravel` arm calls `state.step_travel()`, then drains both
`drained_traveler_handoff_messages(step)` and
`drained_destination_arrival_messages()`. The duplicate-step early return is
safe: it happens before `state.step_travel()`, so it cannot leave a newly
produced arrival buffered.

P1 does not change `step_travel_city()`, its coordinator drain, or traveler
handoff cadence.

```rust
fn route_destination_arrived(
    traveler: TravelerId,
    destination: PlaceRef,
) -> OutboundMessage {
    OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
        recipients: RegionRecipients::One(traveler.citizen.region()),
        event: RegionEvent::DestinationArrived {
            traveler,
            destination,
        },
    })
}
```

Travel handoffs remain one movement subtick stale. `ReceiveTraveler` retains
its `eligible_step`; a handoff created for step `N` is not consumed until step
`N + 1`, even though coordinator delivery itself is immediate.

```text
subtick N
  move token -> emit handoff -> queue at neighbor

subtick N+1
  neighbor consumes handoff -> moves token
```

## Arrival Detection

P1 does not invent another arrival state. It emits `DestinationArrived` only
on the existing transition into:

```text
TravelState { status: Traveling }
  -> TravelState { status: AtWork, building: Some(token.work.building) }
```

`TokenArrival::ArrivedWork` is not itself a work-arrival fact. It is also
returned for an already parked token and for a stranded token outside home.
P1 requires a current work endpoint and verifies the resulting parked building
matches that endpoint. Arrival at home emits no `DestinationArrived` in this
version.

```rust
let Some(work) = token.work else {
    // Assignment was cleared while this token was away or parked.
    // This cannot be a work arrival.
    return no_destination_arrival;
};

let was_parked_at_work = token.state.status == TravelStatus::AtWork
    && token.state.building == Some(work.building);

let (next_state, arrival) = advance_to_building(...);

let reached_current_work = next_state.status == TravelStatus::AtWork
    && next_state.building == Some(work.building);

if arrival == TokenArrival::ArrivedWork
    && !was_parked_at_work
    && reached_current_work
{
    let traveler = TravelerId {
        citizen,
        generation: token.trip_gen,
    };
    world.outgoing_destination_arrivals.push(PendingDestinationArrival {
        traveler,
        destination: work,
    });
}
```

## Payroll

The first version settles the attendance interval that ended at the daily
boundary:

```text
daily boundary D
  pay attendance recorded since boundary D - 1
  clear attendance
  movement during interval D records next payment's attendance
```

`advance()` runs the daily economy before the first movement sub-tick of that
hour. Therefore the boundary must pay the already-recorded flag, never compare
attendance to a newly invented "current day" number.

At daily settlement:

```text
inside the existing entity-sorted economy loop:
  derive salary and local workplace tax exactly as today
  salary = 0 when attended_since_daily_settlement is false
  keep local workplace tax based on the effective workplace
  keep exported workplace tax contract-based in the producer region
  run existing salary/breakdown/rent/shopping bookkeeping
  attended_since_daily_settlement = false
```

This intentionally does not prorate a late arrival. A later work-credit plan
can replace the boolean attendance record with worked 10-minute subticks while
keeping the same `DestinationArrived` and home-owned-purpose protocol.

For a remote worker, the token host cannot mutate the home-owned `Citizen`.
P3 must therefore report compact worked intervals to the home region, never one
credit event per movement subtick. The host accumulates credit while its token
is parked `AtWork`, then routes an interval on work departure and on any daily
settlement cut that would otherwise cross a payroll boundary.

## Boundaries

```text
in scope
  local and remote citizen arrival
  attendance-gated citizen salary
  same coordinator/FIFO path for local and remote arrival

out of scope
  firing citizens merely because a route is temporarily unavailable
  changing employment claim/release/loss behavior
  changing goods or truck travel
  prorated wages
  changing producer-side remote workplace tax policy
```

Remote workplace tax is currently contract-based. This patch changes only
citizen salary: local workplace tax remains derived from the effective local
workplace, and exported workplace tax remains contract-based in the producer
region. An unpaid worker therefore does not change either producer-side tax
path. A later balance decision may gate producer tax on attendance too.

```text
startup balance risk
  newly assigned citizens have no attendance until they complete one commute
  -> first rent settlement can fail
  -> missed-rent morale can temporarily lower happiness

P2 accepts this bounded startup pressure. Scenario coverage limits it in a
healthy starter city; changing rent timing, morale, or starting money is a
separate balance decision.
```

## Save/load

```text
persist:
  work_trip_generation
  attended_since_daily_settlement

do not persist:
  travel token position (existing transient behavior)
  CitizenArrivalAction (reset to ReturnHome on load)
```

Save compatibility:

```text
old save -> work_trip_generation = 0
  harmless: tokens and handoffs are transient, so the next work departure
  creates a fresh generation.

old save -> attended_since_daily_settlement = false
  conservative: a citizen who had already arrived during the pre-upgrade
  settlement interval can miss that one payment.

old save -> arrival_action = ReturnHome
  required: no in-flight token or queued arrival survives load.
```

On load, tokens rebuild through normal travel behavior. Persisted attendance
remains payable at the next daily boundary. Because no token position is saved,
a citizen saved mid-commute restarts at home. If the remaining interval is too
short to reach work, that citizen can miss one payment they would otherwise
have earned. P2 accepts this existing transient-token limitation; persisting or
reconstructing mid-commute progress is a separate travel/save mission.

After load, `work_trip_generation` may be nonzero while transient
`away_residents` is empty. That is intentional: `away_residents` describes an
active local-or-remote commute, while the persisted counter only rejects stale
arrivals and returns. The next work departure establishes a new live trip.

## Patch split

### P1: Destination arrival event and home purpose

```text
Scope
  CitizenArrivalAction; DestinationArrived; travel detection; coordinator route
  to the home region; one persisted per-work-departure generation replacing
  World.away_generation; generalized World.away_residents and home_accepts for
  local and cross-region travelers; transient World pending-arrival buffer;
  home-side validation and attendance record.

Forbidden
  no salary formula change; no new queue or drain API; no change to
  step_travel_city(); no second StepTravel; no local direct-apply path; no
  same-subtick traveler handoff consumption.

Tests
  local and remote arrival reach the home-region FIFO
  remote arrival follows the same path
  only the Traveling -> AtWork transition emits an arrival
  parked AtWork token emits no duplicate coordinator route
  cleared assignment at a stranded former workplace emits no arrival or panic
  a stale generation cannot record attendance for a new work trip
  rejected local home completion drops its token instead of parking forever
  a duplicate destination event records attendance once
  a cross-region handoff is still consumed on the next movement subtick
```

`CitizenArrivalAction` and the existing `TokenArrival` both derive
`PartialEq, Eq`; P1's edge predicate compares their variants directly.

```rust
fn begin_work_trip(
    world: &mut World,
    citizen_id: Entity,
    state: TravelState,
    home: PlaceRef,
    work: PlaceRef,
) -> Option<TravelToken> {
    let Some(citizen) = world.citizens.get_mut(&citizen_id) else {
        return None;
    };
    citizen.work_trip_generation += 1;
    citizen.arrival_action = CitizenArrivalAction::StartWorkShift;
    let trip_gen = citizen.work_trip_generation;
    world.away_residents.insert(citizen_id);

    Some(TravelToken {
        state,
        home,
        work: Some(work),
        trip_gen,
    })
}

// The existing immutable planning pass first collects only departures whose
// depart_toward call succeeded. P1 applies those candidates after that pass,
// before the move pass, so it can mutate Citizen and World safely.
for (citizen, state, home, work) in planned_work_departures {
    let Some(token) = begin_work_trip(world, citizen, state, home, work) else {
        continue;
    };
    world.tokens.insert(citizen, token);
    just_departed.insert(citizen);
}

fn destination_arrived_after_step(
    citizen: Entity,
    token: &TravelToken,
    next_state: &TravelState,
    arrival: TokenArrival,
) -> Option<PendingDestinationArrival> {
    let work = token.work?;
    let was_parked_at_work = token.state.status == TravelStatus::AtWork
        && token.state.building == Some(work.building);
    let reached_current_work = next_state.status == TravelStatus::AtWork
        && next_state.building == Some(work.building);

    (arrival == TokenArrival::ArrivedWork
        && !was_parked_at_work
        && reached_current_work)
        .then(|| PendingDestinationArrival {
            traveler: TravelerId { citizen, generation: token.trip_gen },
            destination: work,
        })
}

fn apply_destination_arrived(home: &mut RegionState, event: DestinationArrived) {
    let Some(citizen) = home.world.citizens.get_mut(&event.traveler.citizen)
    else {
        return;
    };
    let Some(assignment) = citizen.workplace_assignment else {
        return;
    };

    if citizen.arrival_action != CitizenArrivalAction::StartWorkShift
        || citizen.work_trip_generation != event.traveler.generation
        || assignment.workplace != event.destination.building
    {
        return;
    }

    citizen.attended_since_daily_settlement = true;
    citizen.arrival_action = CitizenArrivalAction::ReturnHome;
}
```

### P2: Attendance-gated daily salary

```text
Scope
  replace assignment-only salary eligibility with attendance since the previous
  daily settlement; pay then clear that attendance at the next daily boundary.

Forbidden
  no employment lease release on missed arrival; no producer-tax change.

Tests
  assigned but unreachable citizen receives zero salary
  citizen receives salary after reaching the assigned workplace
  a stable worker remains paid on later days after reaching work each day
  remote and local workers follow the same salary eligibility rule
  an unattended local worker receives zero salary while its effective workplace
  still contributes the same local workplace tax
  save/load preserves already-recorded attendance exactly once
  a mid-commute save can miss the next payment when the restarted commute cannot
  reach work before settlement
```

```rust
fn settle_daily_payroll(world: &mut World) {
    // This is the existing payroll loop's order and read-before-mutate shape.
    let mut citizen_entities: Vec<_> = world.citizens.keys().copied().collect();
    citizen_entities.sort_by_key(|citizen| citizen.0);

    for citizen_entity in citizen_entities {
        let (home, assignment, attended) = world.citizens
            .get(&citizen_entity)
            .map(|citizen| (
                citizen.home,
                citizen.workplace_assignment,
                citizen.attended_since_daily_settlement,
            ))
            .unwrap_or((Entity(u64::MAX), None, false));
        let (ungated_salary, local_workplace_tax) = {
            match assignment {
                Some(job) if job.workplace.as_local(world.region_id).is_some() => {
                    let workplace = job.workplace;
                    salary_for_workplace(world, workplace)
                        .map(|pay| (pay, workplace_tax_for_workplace(world, workplace)))
                        .unwrap_or((0, 0))
                }
                Some(job) => (job.salary, 0),
                None => (0, 0),
            }
        };
        let salary = attended.then_some(ungated_salary).unwrap_or(0);

        let rent = rent_per_citizen(world, home);
        let shopping = next_shopping_offer(world, home, &shopping_slots);

        // Existing code keeps its positive-pay guard and breakdown updates,
        // but uses salary rather than the former salary.0 tuple field.
        // if salary > 0 { citizen.money += salary; salaries_paid += salary; }
        // if ungated_salary > 0 { workplace_tax += local_workplace_tax; }
        // ...rent and shopping using rent and shopping...
        let Some(citizen) = world.citizens.get_mut(&citizen_entity) else {
            continue;
        };
        citizen.attended_since_daily_settlement = false;
    }
}
```

### P3: Worked-time credit (later)

```text
Scope
  replace daily boolean attendance with 10-minute work credit and prorated pay;
  report remote worked intervals to the citizen's home region at departure and
  at a daily settlement cut, not once per work subtick.

Tests
  late arrival earns less than full-day arrival
  unreachable citizen earns zero
  fractional-credit rounding is deterministic
```

```rust
// Token host: keeps this transient counter with the token while it is parked.
fn record_work_subtick(token: &mut TravelToken) {
    let Some(work) = token.work else {
        return;
    };
    if token.state.status == TravelStatus::AtWork
        && token.state.building == Some(work.building)
    {
        token.worked_subticks_since_report += 1;
    }
}

// Token host: emit once at work departure, or split at the daily boundary.
fn report_worked_interval(
    token: &mut TravelToken,
    citizen: Entity,
) -> Option<PendingWorkedInterval> {
    let workplace = token.work?;
    let worked_subticks = std::mem::take(&mut token.worked_subticks_since_report);
    Some(PendingWorkedInterval {
        traveler: TravelerId { citizen, generation: token.trip_gen },
        workplace,
        worked_subticks,
    })
}

// Home region: after P1 accepted arrival, the action is ReturnHome. Validate
// that current trip stamp and workplace, then add the compact interval credit.
fn apply_worked_interval(home: &mut RegionState, interval: WorkedInterval) {
    if !current_work_interval_is_valid(home, interval.traveler, interval.workplace) {
        return;
    }
    let Some(citizen) = home.world.citizens.get_mut(&interval.traveler.citizen)
    else {
        return;
    };
    citizen.worked_subticks_since_daily_settlement += interval.worked_subticks;
}

// full_daily_salary comes from the same local-authoritative / remote-captured
// salary lookup P2 preserves; this helper only prorates that result.
fn prorated_salary(citizen: &Citizen, full_daily_salary: i32) -> i32 {
    full_daily_salary * citizen.worked_subticks_since_daily_settlement
        / WORK_SUBTICKS_PER_DAY
}
```
