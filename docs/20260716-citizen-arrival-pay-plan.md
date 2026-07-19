# Citizen arrival-pay plan

Status: **plan**.

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
home region                         workplace region
-----------                         ----------------
Citizen purpose                     travel token moves
attendance                          detects destination
payroll                             emits DestinationArrived
```

## Ownership

```text
Citizen home region owns:
  workplace_assignment
  CitizenArrivalAction
  attendance for the current payroll day
  salary payment

Token host region owns:
  road position, dwell, route endpoints, trip generation
  no salary or attendance mutation
```

The token retains navigation endpoints (`home`, `work`) because a host region
needs them to move a visiting citizen. It does not own the meaning of arrival.

```rust
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

## Arrival flow

```text
home sets CitizenArrivalAction::StartWorkShift
        |
        v
token reaches workplace in local or remote region
        |
        v
travel::step_tokens buffers DestinationArrived
        |
        v
worker barrier sorts it with same-region targets included
        |
        v
home FIFO receives RegionEvent::DestinationArrived
        |
        v
home validates purpose + assignment + destination
        |
        v
attendance = true for current payroll day
```

Local and remote arrival use the same path:

```text
local:   workplace region -> barrier -> same home-region FIFO
remote:  workplace region -> barrier -> home-region FIFO
```

```rust
fn apply_destination_arrived(home: &mut RegionState, event: DestinationArrived) {
    let citizen = home.citizen(event.traveler.citizen)?;
    let assignment = citizen.workplace_assignment?;

    if citizen.arrival_action != CitizenArrivalAction::StartWorkShift {
        return;
    }
    if assignment.workplace != event.destination.building {
        return;
    }

    citizen.attended_payroll_day = Some(home.payroll_day());
}
```

Repeated arrival events are harmless: recording the same payroll day again has
no additional effect.

## StepTravel-owned event drain

`step_travel_city()` already owns the worker barrier for one movement subtick.
It also drains the `DestinationArrived` events produced by that movement before
returning. There is no background coordinator and no second event queue.

```text
step_travel_city()
  1. lock the runner operation
  2. drain current FIFO work; run StepTravel once
  3. collect and sort outbound messages
  4. partition:
       arrivals = DestinationArrived events
       handoffs = TravelerHandedOff events
  5. drain_events(arrivals)
       a. deliver arrivals to existing target FIFOs
       b. run one full-inbox worker barrier
       c. collect any resulting outbound messages
  6. deliver handoffs and resulting outbound messages normally
  7. unlock and return

DestinationArrived handling
  1. home validates traveler + purpose + assignment + destination
  2. home records attendance
  3. handler emits no follow-up arrival event
```

```rust
fn step_travel_city(&self) -> Result<(), RegionalGameRunnerError> {
    let _operation = self.operation_lock.lock()?;

    let forwarded = run_step_travel_barrier()?;
    let (arrivals, deferred) = partition_travel_outputs(forwarded);

    let drain_outputs = if arrivals.is_empty() {
        Vec::new()
    } else {
        self.drain_events(arrivals)?
    };
    self.deliver_forwarded_events(merge_and_sort(deferred, drain_outputs))
}

fn drain_events(
    &self,
    events: Vec<ForwardedRegionEvent>,
) -> Result<Vec<ForwardedRegionEvent>, RegionalGameRunnerError> {
    // deliver_forwarded_events performs the deterministic merge sort.
    self.deliver_forwarded_events(events)?;

    let mut forwarded = Vec::new();
    for worker in &self.workers {
        let mut summary = worker.process_region_events_for_barrier(usize::MAX)?;
        check_routing_errors(&summary)?;
        forwarded.append(&mut summary.forwarded_events);
    }
    Ok(forwarded)
}
```

`drain_events` is generic: it receives actual forwarded events, not an event
kind. P1 calls it only with `DestinationArrived` events.

The second barrier does not broadcast `StepTravel`:

```text
movement barrier:  token may move one road step
arrival barrier:   home processes attendance only

therefore:
  token moves at most one road step per step_travel_city() call
  arrival is applied before step_travel_city() returns
  Tick/save cannot overtake the arrival because they share operation_lock
```

Travel handoffs remain one movement subtick stale. They are delivered after the
arrival barrier and are not consumed until the next `step_travel_city()` call.

```text
subtick N
  move token -> emit handoff -> queue at neighbor

subtick N+1
  neighbor consumes handoff -> moves token
```

## Payroll

The first version is daily attendance payroll:

```text
arrival at assigned workplace during payroll day -> full daily salary
no arrival during payroll day                   -> zero salary
```

```rust
fn salary_for_daily_settlement(citizen: &Citizen, payroll_day: u64) -> i32 {
    if citizen.attended_payroll_day == Some(payroll_day) {
        citizen.workplace_assignment.map(|job| job.salary).unwrap_or(0)
    } else {
        0
    }
}
```

At daily settlement:

```text
for each citizen in Entity order:
  salary = salary_for_daily_settlement(citizen, current_day)
  pay salary
  clear attendance for the settled day
```

This intentionally does not prorate a late arrival. A later work-credit plan
can replace the boolean attendance record with worked 10-minute subticks while
keeping the same `DestinationArrived` and home-owned-purpose protocol.

## Boundaries

```text
in scope
  local and remote citizen arrival
  attendance-gated citizen salary
  same FIFO/barrier path for local and remote arrival

out of scope
  firing citizens merely because a route is temporarily unavailable
  changing employment claim/release/loss behavior
  changing goods or truck travel
  prorated wages
  changing producer-side remote workplace tax policy
```

Remote workplace tax is currently contract-based. This patch changes only
citizen salary; it must document the resulting temporary difference between
unpaid remote workers and producer-side tax. A later balance decision may gate
that tax on attendance too.

## Save/load

```text
persist:
  CitizenArrivalAction
  attended_payroll_day

do not persist:
  travel token position (existing transient behavior)
```

On load, tokens rebuild through normal travel behavior. Attendance already
recorded for the current payroll day remains valid and idempotent; a citizen
must reach work again before a later payroll day can record attendance.

## Patch split

### P1: Destination arrival event and home purpose

```text
Scope
  CitizenArrivalAction; DestinationArrived; travel detection; barrier routing;
  StepTravel-owned generic drain_events(arrivals); one immediate arrival barrier;
  home-side validation and attendance record.

Forbidden
  no salary formula change; no background drain thread; no second StepTravel;
  no local direct-apply path; no same-subtick traveler handoff consumption.

Tests
  local and remote arrival are applied before step_travel_city() returns
  remote arrival follows the same path
  a duplicate destination event records attendance once
  token moves no more than one road step during the operation
  a cross-region handoff is still consumed on the next movement subtick
```

```rust
match token_arrival {
    ArrivedWork => pending_arrivals.push(DestinationArrived { traveler, destination }),
    _ => {}
}

let (arrivals, handoffs) = partition_travel_outputs(outbound);
let drain_outputs = if arrivals.is_empty() {
    Vec::new()
} else {
    drain_events(arrivals)?
};
deliver_forwarded_events(merge_and_sort(handoffs, drain_outputs));
```

### P2: Attendance-gated daily salary

```text
Scope
  replace assignment-only salary eligibility with attendance for the current
  payroll day; clear settled attendance.

Forbidden
  no employment lease release on missed arrival; no producer-tax change.

Tests
  assigned but unreachable citizen receives zero salary
  citizen receives salary after reaching the assigned workplace
  a stable worker remains paid on later days after reaching work each day
  remote and local workers follow the same salary eligibility rule
  save/load preserves already-recorded current-day attendance exactly once
```

```rust
let salary = citizen
    .attended_payroll_day
    .filter(|day| *day == current_day)
    .and_then(|_| citizen.workplace_assignment)
    .map(|assignment| assignment.salary)
    .unwrap_or(0);
```

### P3: Worked-time credit (later)

```text
Scope
  replace daily boolean attendance with 10-minute work credit and prorated pay.

Tests
  late arrival earns less than full-day arrival
  unreachable citizen earns zero
  fractional-credit rounding is deterministic
```
