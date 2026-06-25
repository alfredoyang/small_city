# Citizen Daily Schedule

Status: **Greenfield; v1 commute-only.** Targets `master`'s `Entity(u64)` model.
Defines *when* and *why* citizens travel — the daily schedule that emits a semantic
**intent** (Home, Work, Leisure) for each citizen by hour. The movement system
resolves the intent to a target. The *how* of movement
(routing, Dijkstra with crossing penalty, route cache, cross-region token handoff,
and intent-to-target resolution) is in
[`docs/traffic-pathfinding-plan.md`](traffic-pathfinding-plan.md).

## Goal

A simple, shared daily schedule keyed off the `GameTime` hour. Each region owns its
own `GameTime` (per-region `World.resources.time`), synchronized by the city-wide
tick — all regions advance one hour per tick, so all schedules agree. **Cross-region
travel (P5) requires `tick_city`** (all regions tick together); `tick_region` (single
region) can desynchronize clocks and must not be used when cross-region tokens are in
flight. Each tick, **after the hour is advanced**, the movement system asks the
schedule "what does this citizen want to do now?", then resolves the intent to a
target and routes the citizen toward it (the
routing is the pathfinding plan's job).

```text
 hour:  00 ───── 09 ─────────── 15 ─────────── 22 ──── 24
 want:  [   HOME    ][   WORK      ][  FREE TIME  ][  HOME  ]
 dest:   home         workplace      a commercial   home
```

## v1 — commute-only

**v1 implements only the home ↔ work commute.** The 15–22 "free time" /
leisure→commercial hop is **deferred** — a `ponytail:` note in `travel.rs` says it
collapses to home in v1.

```text
 hour:  00 ───── 09 ─────────── 15 ──── 24
 want:  [   HOME    ][   WORK      ][  HOME  ]
 dest:   home         workplace      home
        [22:00, 09:00)  [09:00, 15:00)  [15:00, 22:00)→home  [22:00, 24:00)
```

- **[09:00, 15:00) → WORK** (intent = `Work(workplace)`; movement resolves local
  workplace or border-exit cell if remote — see pathfinding plan §5)
- **else → HOME** (intent = `Home`; movement resolves to `citizen.home`)

Remote workers idle at home in v1 (the pathfinding plan's P3); P5 adds border-exit
routing for them (pathfinding plan §5d).

The **desired phase** changes at 09:00 (home→work) and 15:00 (work→home). The
movement system reconciles per-tick: a trip may start later than the boundary after
load, a delayed handoff, or an assignment change — the schedule simply re-derives the
intent each tick and the movement system departs if the resolved target differs from
the current location.

## Target selection (full design)

### Work [09:00, 15:00)
- The schedule emits `ScheduleIntent::Work(workplace_entity)` — the workplace
  `Entity` (local or remote). The **movement system** resolves it:
  - **Local job** (movement calls `workplace.as_local(world.region_id)` → `Some`):
    target = the workplace building (route cache routes to its adjacent road cells).
  - **Remote job** (`as_local` → `None`): P3 idles at home; P5 resolves to the
    **border-exit cell** — a specific road cell `Entity`, not a building (pathfinding
    plan §5d). The border-exit selection uses road/hint data (`border_neighbor_map`)
    that lives in the pathfinding layer, not the schedule. The route cache (P2) must
    support routing to an explicit road cell as the destination root — see
    pathfinding plan §5d.

### Free time [15:00, 22:00) — deferred
- The schedule emits a **Leisure** intent (not a specific building). A future
  destination resolver (in the pathfinding/movement layer, not here) picks the nearest
  reachable commercial from the citizen's current location, deterministic tie-break by
  `sort_entities_by_position`; none reachable → home.
- This keeps road/pathfinding logic out of the schedule; the schedule only says
  "this citizen wants leisure," not "go to building X."
- Future: richer leisure errands, multiple stops, morale-influenced choices.

### Home [22:00, 09:00)
- Target = home. If the citizen is already home, it stays. If the citizen is stranded
  elsewhere (e.g. at work with no road home — the road was torn up), the pathfinding
  plan's §4b keeps it **at its current location** (no teleport); the schedule continues
  to emit "home" each tick, and the movement system routes home as soon as a road
  reconnects.

## Movement interface (how the schedule meets the pathfinding system)

The schedule has two layers:

```rust
/// Pure phase from the hour alone — no citizen data needed.
/// Used by visiting tokens (P5) in a host region that doesn't own the citizen.
enum SchedulePhase { Work, Home, Leisure }

fn schedule_phase(hour: u8) -> SchedulePhase {
    if (9..15).contains(&hour) { SchedulePhase::Work }
    else { SchedulePhase::Home }                      // v1: Leisure collapses to Home
}

/// Semantic intent for a local citizen — the movement system resolves it to a route.
/// The schedule does NOT pick a border-exit cell or a commercial building; those
/// are pathfinding-side decisions (road/hint data).
enum ScheduleIntent {
    Home,
    Work(Entity),    // the workplace Entity (local or remote); movement system resolves
    Leisure,         // deferred — a destination resolver picks the building
}

fn schedule_intent(hour: u8, citizen: &Citizen) -> ScheduleIntent {
    match schedule_phase(hour) {
        SchedulePhase::Work => match &citizen.workplace_assignment {
            Some(a) => ScheduleIntent::Work(a.workplace),  // local or remote — movement resolves
            None => ScheduleIntent::Home,                   // jobless → home
        },
        SchedulePhase::Home => ScheduleIntent::Home,
        SchedulePhase::Leisure => ScheduleIntent::Leisure,  // unreachable in v1
                                                                // (schedule_phase never returns Leisure)
    }
}
```

**How the movement system resolves `ScheduleIntent`:**

```text
Home         → target = citizen.home; route via P2 route cache
Work(entity) → entity.as_local(world.region_id):
                 Some(local) → target = local workplace; route via P2
                 None(remote)→ P3: idle at home (no depart)
                               P5: target = border-exit cell (§5d, pathfinding-side)
Leisure      → deferred; a destination resolver picks a commercial (pathfinding-side)
```

A **visiting token** (P5) lives in a host region that doesn't own the citizen's
`Citizen` data. The host calls `schedule_phase(hour)` to detect the workday end
(phase → Home at 15:00) and emits `Return` — it never needs the full
`schedule_intent` because the token already carries its destination. A **local
citizen** uses `schedule_intent(hour, citizen)` which produces the semantic
intent; the movement system resolves it to a concrete target.

The movement system consumes the intent each tick:

```text
at target building?  ─► status = AtHome / AtWork ; current_cell = None (idle)
target changed?      ─► depart: current_cell = an entry road cell of the current
                          building; status = Traveling; destination = target
en route?            ─► step current_cell = came_from[current_cell] (one cell);
                          arrived when current_cell is a destination root
```

The `came_from`, route cache, and Dijkstra are all in the pathfinding plan — the
schedule only decides *which intent*, not *how* to get there. The movement system resolves the intent to a target.

## Future daily-life sections (placeholder)

The wider scope of this doc leaves room for future citizen-behavior sections beyond
the schedule:

- **Leisure/commercial selection** — a destination resolver that turns a `Leisure`
  intent into a specific commercial building (nearest reachable, preference weights,
  multiple errand stops). This lives in the movement/pathfinding layer, not the
  schedule.
- **Morale effects on travel** — unhappy citizens travel less, skip leisure, or
  commute irregularly.
- **Errands** — shopping, services, multi-stop trips.
- **Schedule variation** — shift work, part-time, weekends (`day_of_week()` already
  exists on `GameTime`, `resources.rs:47`).

Each is a separately balanced mission; none is built yet.

## Architecture view

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                         CITIZEN DAILY SCHEDULE                              │
│                     docs/citizen-schedule-plan.md                           │
│                                                                             │
│  "WHEN and WHY a citizen travels" — emits INTENT; never the ROUTE          │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  INPUTS                         SCHEDULE LOGIC              OUTPUT          │
│  ─────────                      ──────────────               ───────         │
│  world.resources.time           schedule_phase(hour)        SchedulePhase   │
│    .total_hours (u64)             → Work | Home | Leisure    (for tokens)    │
│    (advanced BEFORE               ─┐                                        │
│     travel runs)                   │                                        │
│                                  schedule_intent(hour,         ScheduleIntent│
│  citizen.home (Entity)            citizen)          ─►     {Home,            │
│  citizen.workplace_assignment     │                            Work(Entity),  │
│    .workplace                     │  phase → intent:           Leisure}       │
│      → local or remote            │  Work: Work(wp_entity)     (movement      │
│                                  │  Home: Home                 resolves to    │
│                                  │  Leisure: Leisure           target)        │
│                                  ─┘                                         │
│                                                                             │
│  BOUNDARY: the schedule knows nothing about roads, Dijkstra, route caches,  │
│  or token handoffs. It only answers: "what does this citizen want to do?"   │
│                                                                             │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   │  intent (ScheduleIntent) / phase (SchedulePhase)
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                    PATHFINDING / MOVEMENT SYSTEM                            │
│                  docs/traffic-pathfinding-plan.md                           │
│                                                                             │
│  "HOW to get there" — Dijkstra + crossing penalty (layer 2), route cache,   │
│  cross-region token handoff (layer 1 weighted Dijkstra)                     │
│                                                                             │
│  resolves intent → target → depart / step / arrive state machine            │
│  consumes phase → visiting token workday-end detection (P5)                │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Structure / function interaction map

```text
TICK PIPELINE (src/core/simulation.rs) — where the schedule plugs in
═══════════════════════════════════════════════════════════════════════

  RegionEvent::Tick { request_id }                     runtime/mod.rs:665
    └─ RegionRuntime::start_tick_power_phase            runtime/mod.rs:767
         └─ begin_tick_power_phase (simulation.rs:89)
              ├─ ensure_derived_state(world)             simulation.rs:102
              ├─ world.resources.time.advance_hours(1)  simulation.rs:105  ← HOUR ADVANCES HERE
              ├─ power::run(world)                       simulation.rs:107
              │
              └─ continue_to_job_phase (simulation.rs:135)
                   ├─ stats::run                                    ─┐
                   ├─ [daily] citizens::apply_daily_happiness_decay  │ derived + time phase
                   ├─ [daily] population::run                        │ (travel observes the
                   ├─ citizens::update_happiness_targets             │  NEW hour, advanced
                   ├─ citizens::update_happiness                     │  above at :105)
                   ├─ [daily] economy::assign_local_jobs_for_daily_tick  ─┘
                   │        └─ job_requests_from_world (resource_registry.rs:470)
                   │             excludes citizens whose workplace_assignment.workplace
                   │             .region() != world.region_id (already-remote = not a
                   │             local seeker; they don't compete for local jobs)
                   │
                   └─ [cross-region job export: worker routes grants]
                        └─ finish_tick_after_job_phase (simulation.rs:166)
                             └─ finish_tick_after_goods_phase (simulation.rs:175)
                                  ├─ economy::run_with_goods_exports
                                  ├─ [weekly] business_growth::run
                                  ├─ stats::refresh_population_and_jobs
                                  ├─ pollution::run
                                  ├─ happiness::run
                                  │
                                  ├─ ★ travel::run(world)  ◄── NEW (P3, not yet built)
                                  │    │  reads: world.resources.time.hour_of_day()
                                  │    │         (the NEW hour, advanced at :105)
                                  │    │         world.citizens[citizen].home
                                  │    │         world.citizens[citizen].workplace_assignment
                                  │    │  calls: schedule_intent(hour, citizen)  ── THE SCHEDULE
                                  │    │         schedule_phase(hour)  (for visiting tokens)
                                  │    │  resolves intent → target:
                                  │    │         Home → citizen.home
                                  │    │         Work(entity) → entity.as_local(world.region_id):
                                  │    │           Some(local) → local workplace
                                  │    │           None(remote)→ P3: idle at home; P5: border-exit (§5d)
                                  │    │         Leisure → deferred (destination resolver)
                                  │    │  uses:  world.routes_to(target, network)  (P2 cache)
                                  │    │         road_predecessors  (P1 Dijkstra + crossing penalty)
                                  │    │  writes: world.travel[citizen] = TravelState {
                                  │    │            status, current_cell, destination }
                                  │    │
                                  │    │  ┌─ schedule_intent(hour, citizen) ─────────┐
                                  │    │  │  THIS DOC'S CORE FUNCTION                         │
                                  │    │  │                                                     │
                                  │    │  │  phase = schedule_phase(hour)                     │
                                  │    │  │    [09,15) → Work;  else → Home (v1)              │
                                  │    │  │                                                     │
                                  │    │  │  Work:                                             │
                                  │    │  │    Some(assignment) → Work(assignment.workplace)  │
                                  │    │  │    None             → Home  (jobless)            │
                                  │    │  │  Home:  Home                                       │
                                  │    │  │  Leisure: Leisure  (deferred)                    │
                                  │    │  │                                                     │
                                  │    │  │  returns: ScheduleIntent                          │
                                  │    │  │    (movement system resolves to target Entity)    │
                                  │    │  └─────────────────────────────────────────────────────┘
                                  │    │
                                  │    └─ for each citizen (sorted by entity.0):
                                  │         prev = world.travel[citizen] or default
                                  │         intent = schedule_intent(hour, citizen)
                                  │         target = resolve(intent, citizen, world)  [movement-side]
                                  │         match prev.current_cell:
                                  │           Some(cell) → step(cell, target)  [§4 state machine]
                                  │           None → depart or idle            [§4 state machine]
                                  │
                                  └─ world.resources.turn += 1          simulation.rs:197


DATA TYPES (src/core/components.rs, src/core/resources.rs, src/core/entity.rs)
══════════════════════════════════════════════════════════════════════════════

  GameTime { total_hours: u64 }                         resources.rs:34
    .hour_of_day() -> u8  (total_hours % 24)            resources.rs:43
    .day_of_week() -> u8  (already exists)              resources.rs:47
      └─ schedule variation can use this now (shifts, weekends)

  Citizen {                                             components.rs:151
    id: Entity,                  #[serde(skip)]          :160
    age: u32,                                            :162
    home: Entity,                 (always local)         :166
    workplace_assignment: Option<WorkplaceAssignment>,   :173
      #[serde(default, skip)]  (derived, rebuilt daily)
    morale: Morale,                                      :175
    money: i32,                                          :177
  }
    Citizens are off-grid (no Position component); sorted by entity.0 for determinism.

  WorkplaceAssignment {                                 components.rs:220
    workplace: Entity,           (city-wide; region() = producer)
    location: CityCellRef,       (self-describing cell)
    salary: i32,
  }
    local iff workplace.as_local(world.region_id).is_some()   :215

  Entity(pub u64)                                       entity.rs:26
    .region() -> RegionId  (high 32 bits)               :35
    .local()  -> u32       (low 32 bits)                :40
    .as_local(region) -> Option<Entity>  (guard)        :48

  TravelState { (NEW — P3, not yet built)               pathfinding §2
    status: TravelStatus,                               P3: AtHome|AtWork|Traveling
                                                      P5 adds: Away
    current_cell: Option<Entity>,
    destination: Option<Entity>,
  }
    stored in: world.travel: HashMap<Entity, TravelState>  #[serde(skip)]
    P5 adds: world.visiting_travel: HashMap<TravelerId, TravelState>  #[serde(skip)]


CROSS-DOC INTERACTION
══════════════════════

  citizen-schedule-plan.md           traffic-pathfinding-plan.md
  ─────────────────────────           ────────────────────────────
  schedule_intent(hour, citizen)     travel::run(world)
    picks INTENT ──────────────────►  resolves intent → target
  schedule_phase(hour)                   depart / step / arrive (§4a)
    picks PHASE ────────────────────►  visiting tokens (P5): host detects
                                       workday end (phase→Home) → emit Return
                                        │
                                        ├─ local route: Dijkstra + crossing
                                        │    penalty (P1) → route_cache (P2)
                                        │    → came_from → O(1) step/tick
                                        │
                                        └─ remote route (P5):
                                             border-exit (§5d hint)
                                             → token handoff (§5c)
                                             → visiting_travel (§5e)
                                             → multi-hop: layer 1 weighted
                                               Dijkstra + layer 2 Dijkstra (§5f)

  ◄──────────────────── ───────────  pathfinding §4b: unreachable → stay put
   schedule says "intent=Work"         (the schedule emitted a Work intent the
   but if no route exists, the          movement system couldn't route to; the
   movement system keeps the citizen     movement system keeps the citizen
   at its current location (no          at its current building/cell, not
   teleport); re-derives "home"         home — until a road reconnects)
   each tick; routes home when
   a road reconnects
```

### Key takeaways

1. **The schedule is two functions**: `schedule_phase(hour) -> SchedulePhase`
   (pure, no citizen data — used by visiting tokens in P5) and
   `schedule_intent(hour, citizen) -> ScheduleIntent` (semantic intent — used
   by local citizens). The movement system resolves `ScheduleIntent` to a target
   `Entity` (local workplace, home, or border-exit). The schedule never picks a
   border-exit or commercial building — those are pathfinding-side decisions.
2. **It plugs into the tick pipeline** at `travel::run(world)` (P3, not yet built),
   which runs after `happiness::run` and before `world.resources.turn += 1`
   (`simulation.rs:197`). The hour was advanced earlier at `simulation.rs:105`, so
   travel observes the **new** hour.
3. **The boundary is clean**: the schedule never touches roads, Dijkstra, route
   caches, or token handoffs. The pathfinding plan's §4b (unreachable → stay at
   current location, no teleport) is the one place the movement system doesn't reach
   the schedule's intent — and that's a movement-side decision, not a schedule-side
   one.
4. **The `as_local()` guard** (`entity.rs:48`) is the single mechanism that tells the
   **movement system** whether a job is local (target = workplace) or remote (v1: idle
   at home; P5: target = border-exit cell). The schedule itself never calls `as_local`.
5. **Future daily-life scope** (leisure, morale effects, errands, schedule variation)
   all plug into the same `schedule_intent` function — the function just gets richer,
   the movement system stays unchanged. Leisure emits an intent; a destination resolver
   in the pathfinding layer picks the building.

## Decisions locked

- **Daily schedule** (shared, by hour): `[09:00, 15:00)` emits `Work(workplace_entity)`
  intent (movement resolves local workplace or border-exit for remote), else emits
  `Home`. Free-time/leisure deferred (commute-only in v1). The desired phase changes
  at 09:00 and 15:00; per-tick reconciliation handles delayed departures.
- **Two schedule functions**: `schedule_phase(hour)` (pure, for visiting tokens) and
  `schedule_intent(hour, citizen)` (semantic `ScheduleIntent`, for local
  citizens). The schedule returns `Work(workplace_entity)` — the movement system
  resolves local vs remote (`as_local`) and picks the border-exit (P5, pathfinding
  data). A visiting token uses `schedule_phase` to detect the workday end without
  needing the citizen's `Citizen` data.
- **Remote detection** is movement-side: the movement system calls
  `workplace.as_local(world.region_id)` on the `Work(entity)` intent — `Some` = local,
  `None` = remote (master's `Entity(u64)` model). The schedule itself never calls
  `as_local`; it just emits `Work(workplace_entity)`.
- **v1 is commute-only** — two targets (home, workplace), two phase boundaries. The
  free-time block collapses to home; leisure→commercial emits a `Leisure` intent
  resolved by a future destination resolver in the pathfinding layer.
- **The schedule emits intent; the pathfinding system resolves and routes.** The
  schedule knows nothing about roads, Dijkstra, or route caches — it only answers
  "what does this citizen want to do?"
- **Unreachable target → stay put** (no teleport). The movement system keeps the
  citizen at its current building/cell if no route exists; the schedule re-derives
  "home" each tick, and the movement system routes home as soon as a road reconnects.

---

## Implemented — v1 schedule layer (`src/core/systems/schedule.rs`)

The commute-only schedule shipped as a standalone, pure core patch (no tick
wiring yet — pathfinding **P3 (movement)** is the consumer that wires it in).

### What it adds

Two pure functions and two enums, nothing else. The module owns *only* the
hour→intent decision; it never touches roads, the route cache, `as_local`, or
border-exit selection (those are movement-side, per "Decisions locked").

```text
  ┌──────────────────────── schedule.rs (pure, deterministic) ────────────────────────┐
  │                                                                                    │
  │  schedule_phase(hour: u8) ─────────────► SchedulePhase { Work, Home, Leisure }     │
  │     (9..15) → Work ; else → Home          (Leisure reserved/deferred; never        │
  │     no citizen data needed                 constructed in v1 → #[allow(dead_code)]) │
  │     └─ used by P5 visiting tokens that don't own the Citizen record                │
  │                                                                                    │
  │  schedule_intent(hour, &Citizen) ──────► ScheduleIntent { Home, Work(Entity),      │
  │     Work phase + employed → Work(a.workplace)                          Leisure }    │
  │     Work phase + jobless  → Home          (Work carries the workplace Entity        │
  │     else                  → Home           verbatim — local OR remote; the          │
  │     └─ #[allow(dead_code)] until P3        movement system calls as_local, not us)  │
  │                                                                                    │
  └────────────────────────────────────────────────────────────────────────────────────┘
                                    │ emits intent
                                    ▼
                    movement system (pathfinding P3, NOT in this patch)
                    resolves intent → target → route via the P2 route cache
```

### Where the boundary sits (why two functions)

```text
  WHO ASKS                        CALLS                  WHY
  ───────────────────────────────────────────────────────────────────────────
  local citizen (home region)     schedule_intent(h,c)   needs the semantic
                                                          intent incl. workplace
  P5 visiting token (host region,  schedule_phase(h)      only needs to detect
  no Citizen record on hand)                              workday end (→ Home)
```

`schedule_phase` is the hour-only half so a host region can run it without the
traveler's `Citizen` data; `schedule_intent` layers the employed/jobless lookup
on top for a locally-owned citizen.

### Determinism / balance

Pure functions of `(hour, &citizen.workplace_assignment)` — no time read, no RNG,
no hidden state, no resource mutation. Balance-neutral (decision layer only).
`Leisure` in both enums is forward-declared for the deferred 15:00–22:00 block;
v1 folds that window into `Home`, so the `Leisure` *phase* is never constructed.

### Reviewed via `claude-city-dev`

codex (`reviewer`) + opencode (`ses_108ae15e8ffel8UmUzUFJQ94IF`) both clean after
two low-severity codex fixes: (1) the remote-workplace test cell now satisfies
`location.region == workplace.region()`; (2) the module's ascii timeline was made
non-overlapping. Gates: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
`cargo test -q` all green (6 new schedule tests, full suite unaffected).
