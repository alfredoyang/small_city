# Travel sub-tick — 10-minute movement with crossing dwell

Status: **plan** (not implemented). Builds on the committed traffic/pathfinding
work (P1 route Dijkstra, P2 route cache, P3 movement, P4 dots, P5 cross-region
handoff — see [traffic-pathfinding-plan.md](traffic-pathfinding-plan.md)).

---

## 1. Introduction — the problem

Today movement is **tick-cadence at the hourly clock**: `travel::run` is called once
per hourly economy tick (`simulation.rs:199`, inside `finish_tick_after_goods_phase`)
and advances every traveler **exactly one road cell per game hour**. Two consequences:

- **Coarse, jumpy motion.** A commuter teleports one cell per hour; the dot barely
  moves and a multi-cell commute takes many hours. The crossing penalty from P1
  only changes *which* path is chosen — it has **no effect on how long the trip
  takes**, because every cell, junction or not, costs exactly one hourly step.
- **Movement is welded to the heavy tick.** Each hourly tick runs power, jobs,
  goods exports, economy, happiness — and *also* steps travellers. You can't make
  movement smoother without re-running all of that, which is expensive and would
  distort the economy clock.

**Goal.** Introduce a cheap **10-minute sub-tick** that *only* moves travellers, so:

1. A traveller advances **one cell per 10-minute sub-tick** (6 per game hour) — smooth,
   visible motion without re-running the economy.
2. **Crossings cost time.** Entering a T-junction takes **2×** (20 min); a 4-way takes
   **4×** (40 min). The cost a traveller pays to cross a cell equals the same weight P1
   already uses to *route* — so the path the route cache prefers is a strong heuristic for
   the fastest one (not a strict guarantee — see §2b).
3. **All regions sub-tick in lockstep** (a `tick_city`-style barrier), so cross-region
   crossings stay deterministic and one-sub-tick-stale — the same staleness model P5
   already uses, just at finer granularity.

This is the finer-granularity movement that pathfinding §4c deferred ("v1 granularity:
tick-cadence … smooth tween deferred"). It is additive: economy stays hourly.

---

## 2. Proposal

### 2a. Split "decide" (hourly) from "move" (sub-tick)

`travel::run` currently does *both* — it resolves each citizen's schedule target **and**
steps positions. We split it:

```text
  ┌─────────────────────── 1 GAME HOUR ───────────────────────┐
  │ hourly tick (heavy, authoritative clock)                  │
  │   power → … → economy → happiness                         │
  │   travel::resolve(world)   ◄── GOAL ONLY: schedule →      │
  │     (set destination + prune; no motion)     destination   │
  │   turn += 1                                                │
  │                                                           │
  │   then 6 × step_travel(world)  ◄── ALL MOTION: one cell    │
  │     depart / step / arrive / cross   each, dwell-gated     │
  └───────────────────────────────────────────────────────────┘
```

- **`travel::resolve`** (hourly) sets each citizen's **goal** — the schedule→target
  `destination` (home / local work / border-exit) — skips `Away` citizens, and prunes dead
  ones. It **does not move, depart, cross, or reset** in-flight state; if a traveller is en
  route to an unchanged target it is left exactly as-is.
- **`step_travel`** (sub-tick) owns **all motion**: depart an idle citizen onto an entry
  road, walk `current_cell` one cell toward `destination` along the `came_from` tree (gated
  by the per-traveller dwell counter), detect **arrival** (idle) and the **cross-region
  crossing** (reach the exit cell → buffer the handoff, mark `Away`), and step visiting
  tokens the same way. It re-resolves no schedule.

Targets only change at 09:00 / 15:00, so resolving the goal once per hour loses nothing;
all movement is smooth within the hour.

### 2b. Dwell-cost stepping (no sub-cell positions)

We keep "one cell per step" but make a traveller **sit on a cell for `cost` sub-ticks**
before advancing. `cost` is the *same* function P1 uses for the Dijkstra edge weight, so the
router **minimises the same per-cell cost the mover pays** — the route it picks is a strong
heuristic for the fastest one, and crossings/turns visibly cost time. It is **not a strict
guarantee**, for two reasons worth stating up front (both acceptable for a city grid):

- **Endpoint convention.** Destination-rooted `dist[origin]` excludes the origin road cell
  and includes the destination root; the mover pays the origin cell and *arrives before*
  paying the root (§4a). With a building destination touching several root roads of
  different cost, the root charged can differ by candidate, so the cheapest-route ≠
  guaranteed-fastest in that corner.
- **Single-tree turns.** Turn cost is charged against each cell's one forward direction in
  the `came_from` tree, the standard approximation of exact `(cell, direction)` turn-penalty
  routing (§3 ceiling).

**Cost is geometric, not just degree — a turn costs as much as a T-junction.** A degree-2
cell is *not* always free: going **straight through** costs 1, but **turning 90°** at it
costs 2× (you slow to take the corner), the same as a T-junction. So the cost depends on
the **incoming and outgoing direction**, not only the cell:

```text
  cost(in_dir, out_dir, degree):
     degree ≥ 4                          → 4   4-way                 40 min (4×)
     degree = 3   OR   in ⊥ out (a turn) → 2   T-junction / corner   20 min (2×)
     else (straight pass, in ∥ out)      → 1   straight              10 min

  one traveller, path  H ─ a ─ X ─ b ─ W   (a straight, X a 4-way, b a corner-turn):
   on a (straight, 1): step → X
   on X (4-way,    4): dwell·3 → step → b     (4 sub-ticks on the crossing)
   on b (turn,     2): dwell·1 → step → arrive at W
```

Because the cost needs *where you came from*, the mover stores the previous cell
(`prev_cell`, §3) and the router charges the turn against each cell's fixed forward
direction (§4a) — so both use the **same** rule and can't disagree.

### 2c. Lockstep across regions (the cross-worker part)

Cross-region crossings (P5) must stay deterministic. Today a handoff emitted on an
**hourly** tick is delivered to the neighbour for its **next** tick (one-tick-stale). We
make the *same* guarantee at sub-tick granularity by running **one sub-tick across every
region as a barrier**: all regions step, *then* all handoffs are routed, *then* the next
sub-tick runs.

```text
  RegionalGame.step_travel_city()  — one 10-minute sub-tick for the whole map
        │
        ▼
  RegionWorker.step_travel_pass():
    ── phase 1: STEP every region (deterministic, no routing yet) ──
       for each region R (owned):
         push RegionEvent::StepTravel ─► R.process ─► step_travel(world)
              ├─ move local travellers + visiting tokens one dwell-step
              ├─ a token reaching its exit cell buffers a crossing (P5a)
              └─ drain → OutboundMessage::TravelerHandedOff   (P5b path, reused)
    ── phase 2: BARRIER — collect ALL handoffs, sort by order key ──
    ── phase 3: ROUTE to neighbour inboxes as RegionEvent::ReceiveTraveler ──
       (consumed at the NEXT sub-tick → one-sub-tick-stale, like P5's one-tick)
        │
        ▼
  mechanism: like Tick, the runner broadcasts a RegionEvent::StepTravel into every
             region's mailbox (handle.send), then drives ONE existing ProcessBarrier
             pass at a full-inbox budget (usize::MAX) so each region drains its pending
             ReceiveTravelers + the one StepTravel in a single pass — NOT looped (a loop
             would re-consume StepTravel's freshly-delivered handoffs same sub-tick and
             break staleness, §4c). Only new code: the StepTravel event body + a new
             runner method; the ProcessBarrier command/worker path is unchanged.

  cadence:  hourly Tick ─► 6 × step_travel_city ─► hourly Tick ─► 6 × … 
            (driver/runner orchestrates; single-region game = same path, 1 region)
```

The barrier is what makes it lockstep: a token handed off at sub-tick *N* lands in the
neighbour's inbox and is stepped at sub-tick *N+1*, never within the same sub-tick (which
would let it skip two regions in one step and depend on region iteration order). This is
exactly P5's `process_workers_with_deterministic_barrier` discipline, reused for travel.

**Constraint (inherited from P5):** sub-ticking a single region while cross-region tokens
are in flight is forbidden — `step_travel_city` always steps *all* regions, so clocks stay
aligned.

---

## 3. Important structures / functions

### `step_cost(in, current, out, degree) -> u32` — the unifying cost (NEW, `road_network_analysis.rs`)

The single source of truth for "how many 10-min ticks to traverse this cell," used by
**both** the router and the mover so they can never drift. It is **geometric**: a turn is
charged like a junction.

```rust
/// Cost to traverse `current` entering from `in_cell` and leaving toward `out_cell`.
/// `out_cell == None` means `current` is the destination root (arrival, no exit turn).
pub(crate) fn step_cost(
    world: &World, in_cell: Option<Entity>, current: Entity, out_cell: Option<Entity>, degree: u32,
) -> u32 {
    if degree >= 4 { return 4; }                       // 4-way
    let turns = match (in_cell, out_cell) {
        (Some(i), Some(o)) => !collinear(world, i, current, o), // 90° corner
        _ => false,                                    // entering from a building / arriving
    };
    if degree == 3 || turns { 2 } else { 1 }           // T-junction or corner, else straight
}
```

- `collinear(world, a, b, c)` is a tiny position check: `b` between `a` and `c` on one axis.
- P1's `road_predecessors_inner` changes `nd = cost + 1 + crossing_penalty` →
  `nd = cost + step_cost(world, neighbor, current, came_from[current], degree)` (see §4a —
  the parent gives `current`'s fixed forward direction). Routing weights move from `1/3` to
  `1/2/4`; this feeds **only** travel routing (`road_distances` stays plain BFS), so the
  economy is untouched.
- `step_travel` calls the same `step_cost` with the traveller's `(prev_cell, current,
  came_from[current])`. `road_degree_in_network` already exists (`road_network_analysis.rs:356`)
  but is **private** — make it `pub(crate)` (or expose `step_cost` as the one public helper
  that takes a cell and computes degree internally), since `travel.rs` now needs the degree.

> **Ceiling (ponytail):** charging the turn against each cell's *single* forward direction
> in the `came_from` tree is the standard approximation of turn-penalty routing; the exact
> version needs `(cell, incoming-direction)` search states. Fine for a city grid — upgrade
> only if a measured route looks wrong.

### `TravelState.dwell: u16` + `prev_cell: Option<Entity>` — per-traveller crossing state (NEW)

```text
  TravelState { status, current_cell, destination, building, dwell, prev_cell, goal }
                                                              ^^^^^  ^^^^^^^^^  ^^^^
   dwell:       sub-ticks already spent on current_cell; advance when dwell+1 == cost
   prev_cell:   the cell stepped from last (so the turn at current_cell is known)
   goal:        Goal { Building(e) | BorderExit(region) } — set hourly by resolve
   destination: the committed CONCRETE cell for the goal (a building, or the exit road
                cell chosen on depart for a BorderExit); set by step_travel, not resolve
```

All `#[serde(skip)]` like the rest of travel state → **no save-format change**, stays
`Copy` (`Goal` is two words). `VisitingToken.token` reuses the same fields. (A visiting
token's goal is implicit — its workplace destination is fixed — so it just steps toward
`destination`.)

### `travel::resolve(world)` — set the GOAL (hourly), the trimmed half of `run`

Schedule → `Goal` for each non-`Away` citizen, plus the dead-citizen prune.
No depart, no stepping, no cross, no reset of in-flight state. Since crossings are now
**buffered** during `step_travel` (mid-hour), the P5b **drain** also moves to the
`StepTravel` handler (§4c) — `resolve` emits no handoffs, so the hourly tick no longer
drains.

### `travel::step_travel(world)` — all MOTION (sub-tick), the new cheap pass

For every local traveller and visiting token (in `entity.0` order), apply one dwell-step
toward `destination` via `World::routes_to`, handle arrival/cross, buffer crossings.

### `RegionEvent::StepTravel` + `RegionWorker::step_travel_pass` + `RegionalGame::step_travel_city`

The lockstep sub-tick: a broadcast event, a barrier pass that reuses the P5b
`TravelerHandedOff → ReceiveTraveler` routing, and the UI-facing entry point.

---

## 4. Pseudocode + interaction with the current code

### 4a. Unify the cost (P1 routing, turn-aware)

**Convention — each cell's cost is charged once, to that cell**, using its two on-path
neighbours `(incoming, cell, outgoing)`. The reverse (destination-rooted) search relaxes
`current → neighbor` to extend the forward path `neighbor → current → came_from[current]`,
so it charges **`current`'s** cost there: incoming `= neighbor`, outgoing `= came_from
[current]` (the latter already settled when `current` is popped). A **source/root** cell
(a destination entry road) has no `came_from` → outgoing `= None` → no exit turn, base cost
only; the **origin** cell is never a `current` for a farther neighbour → it is charged only
when the mover dwells on it, with incoming `= None` (no entry turn).

```rust
// road_network_analysis.rs — road_predecessors_inner, replacing the inline penalty.
let degree = neighbors.len() as u32;
let forward = came_from.get(&current).copied();        // current's fixed exit; None at a root
let nd = cost + step_cost(world, Some(neighbor), current, forward, degree);
//        was: cost + 1 + crossing_penalty
```
Interaction: the route cache (P2) is unchanged — it still stores `came_from`; only the
weights that built it change. `road_distances` (economy) untouched. The mover walks this
same tree and re-derives the **same** per-cell cost from `(prev_cell, cell, came_from
[cell])` (`prev_cell = None` on the first cell, and it **checks arrival before** looking up
a forward cell — §4b), so router and mover charge the same turn at every **interior** cell.
The two **endpoints** are where they can differ (above): the origin cell is charged only by
the mover, the destination-root only by the router — fine as a heuristic, not a proof of
the absolute-fastest route.

### 4b. Split `travel::run`

The split is **resolve sets the *goal*; step_travel does *all motion*** (depart, step,
arrive, **and the cross-region crossing**, since a crossing only happens when a traveller
reaches its exit cell — which is movement, not decision).

The goal is an **owned** descriptor, *not* a single pre-picked cell — a remote workplace's
exit cell can't be chosen until movement time (P5a's `advance_to_exit` picks the first
candidate reachable from the traveller's position). So `resolve` records a `Goal` and
`step_travel` owns the concrete cell choice:

```rust
// systems/travel.rs

// Owned goal stored in TravelState (no borrowed candidate slice). step_travel turns it
// into a concrete next cell each motion step.
enum Goal { Building(Entity), BorderExit(RegionId) }   // home / local work / a remote neighbour

// HOURLY — wired where travel::run is today (simulation.rs:199), inside
// finish_tick_after_goods_phase. Sets each traveller's Goal; NEVER steps,
// crosses, picks an exit candidate, or resets in-flight motion.
pub(crate) fn resolve(world: &mut World) {
    let hour = world.resources.time.hour_of_day();
    for id in citizens_sorted_by_entity {
        if state.status == Away { continue; }                    // out of region; skip
        // resolve_target stays P5a's: Home|Work→Building(home|local), remote-reachable→
        // BorderExit(region), remote-unreachable→Building(home).
        state.goal = Some(resolve_target(world, region, home, schedule_intent(hour, citizen)));
        // Only (re)point the goal. En route + unchanged goal is a no-op — current_cell /
        // destination / dwell / prev_cell are left intact so the hour never restarts a trip.
        travel[id] = state;
    }
    prune dead citizens;            // crossings buffer in step_travel, so the P5b drain
                                    // moves to the StepTravel handler (§4c); resolve emits none
}

// SUB-TICK — new; called 6× per hour by the lockstep driver. ALL motion lives here.
pub(crate) fn step_travel(world: &mut World) {
    let networks = discover_road_networks(world);
    for id in travellers_sorted_by_entity {
        if state.status == Away { continue; }
        let Some(goal) = state.goal else { continue };
        match state.current_cell {
            // Idle in a building → depart. step_travel owns the concrete-cell choice:
            //   Building(b)     → route toward b's adjacent roads;
            //   BorderExit(reg) → P5a advance_to_exit picks the FIRST candidate in
            //                     remote_exit_cells[reg] reachable from here, records it as
            //                     `state.destination` (the committed exit), and walks to it.
            None => travel[id] = depart_or_idle(world, &networks, &mut state, goal),
            Some(cell) => {
                // The committed concrete target this sub-tick (set on depart): a building
                // for Building, or the chosen exit road cell for BorderExit.
                let dest = state.destination.expect("set on depart");
                // arrival / cross are checked BEFORE any forward/cost lookup, because a
                // source cell has no came_from entry.
                if reached(world, cell, dest) {                  // adjacent to building, or == exit cell
                    travel[id] = arrive_or_cross(world, &mut state, cell, goal, dest);
                    //   arrive (Building) → idle;
                    //   exit cell (BorderExit) → buffer Outbound, away_generation++, Away
                    continue;
                }
                let next = routes_to(world, dest)[cell];          // safe: not a source here
                let degree = road_degree_in_network(world, cell, network_of(cell));
                let cost = step_cost(world, state.prev_cell, cell, Some(next), degree);
                if state.dwell + 1 < cost { state.dwell += 1; }   // still crossing this cell
                else { state.prev_cell = Some(cell); state.current_cell = Some(next); state.dwell = 0; }
                travel[id] = state;
            }
        }
    }
    step_visiting_tokens(world, &networks);   // same dwell rule + Return at workday end
    // crossings reached this sub-tick are buffered in world.outgoing_handoffs (P5a, unchanged)
}
```
Interaction: P5a's `advance` is **partitioned by phase**, not by concern — `resolve` keeps
only the *goal* selection (`resolve_target`, the `Away` skip, prune); **every** motion path
P5a already has (depart, the came_from step, arrival, and the exit-cell `Cross` that buffers
the Outbound handoff + marks `Away`) moves into `step_travel`. `routes_to`, the adjacency
`reached` check (today `advance`'s `is_adjacent` / `current_cell == exit_cell`),
`step_visiting`, and the P5 buffers are reused verbatim.

### 4c. Lockstep sub-tick across regions

```rust
// regions/runtime/mod.rs — process_event. ReceiveTraveler already inserts a visiting
// token (P5b, unchanged); StepTravel then steps it. They must be processed in that
// order within a sub-tick — see the budget note below.
RegionEvent::StepTravel => {
    self.state.step_travel();                       // move + buffer crossings
    self.drained_traveler_handoff_messages()        // drain → TravelerHandedOff (reused P5b)
}

// regional_game_runner.rs — NEW runner method; the ONLY new code on the worker path.
// No new ThreadedWorkerCommand and no new worker method: it reuses the existing
// ProcessBarrier entry (process_region_events_for_barrier), just at a full-inbox budget
// and as a SINGLE pass (the tick path loops it at budget 1; this does not loop).
fn step_travel_city(&self) {
    let _op = self.operation_lock.lock();
    for handle in &self.handles {                    // broadcast like tick_regions (runner:231)
        handle.send(RegionEvent::StepTravel);
    }
    // ONE barrier pass at a budget large enough to drain each region's whole inbox:
    // the pending ReceiveTravelers from the previous sub-tick (FIFO, processed first →
    // their visiting tokens are inserted) PLUS the one StepTravel just pushed. Budget 1
    // (the tick value) would stop after a single ReceiveTraveler and leave StepTravel
    // queued. process_region_events_with_mode already refreshes border_neighbor_map per
    // region (P5b), so the hint is current for free.
    let mut forwarded = Vec::new();
    for worker in &self.workers {
        let summary = worker.process_region_events_for_barrier(usize::MAX);  // single pass
        forwarded.extend(summary.forwarded_events);   // the handoffs StepTravel emitted
    }
    // Sort by the rank-3 order key + deliver as ReceiveTraveler to inboxes — exactly as
    // process_one_reply_pass does (runner:447). These are consumed on the NEXT sub-tick,
    // NOT re-processed here (produced by StepTravel, delivered post-pass): that is the
    // one-sub-tick-stale, can't-skip-two-regions guarantee. Looping the barrier until
    // empty WOULD re-consume them and break it — hence a single pass.
    self.deliver_forwarded_events(forwarded);
}
```
Interaction: the **barrier ordering** (collect → sort by order key → deliver to inboxes for
the next pass), `ReceiveTraveler`, `TravelerHandedOff`, the rank-3 order key, the
owner-filtered hint refresh, **and** the `ProcessBarrier` command itself are all reused
**unchanged**. The only deltas are (a) the new `RegionEvent::StepTravel` body, and (b) the
new runner method `step_travel_city`, which drives **one** `process_region_events_for_barrier`
pass at `usize::MAX` budget (vs. the tick path's looped budget-1). No new
`ThreadedWorkerCommand`, no new worker method.

### 4d. Cadence — the UI progresses the timeline; the runner owns tick vs sub-tick

**The UI asks for one thing — "progress the timeline" — and never distinguishes ticks from
sub-ticks.** The cadence (the 6:1 ratio, and firing the hourly tick automatically) lives in
`RegionalGameRunner`, between the UI facade and the worker:

```text
  UI  ──advance()──►  RegionalGame  ──►  RegionalGameRunner          ──►  RegionWorker
  (one call;                              owns the 0..6 sub-tick counter    (pure executor:
   renders dots;                          ───────────────────────           runs the pass
   no tick concept)                       fn advance(&self):                 it is told to)
                                            let _op = operation_lock.lock();
                                            let n = sub_tick.get();          // interior-mutable
                                            if n == 0 {
                                              tick_city()      // hour start: economy + travel::resolve
                                            }
                                            step_travel_city()  // 10 min movement (lockstep)
                                            sub_tick.set((n + 1) % 6)
```

The runner's facade is `&self` and serialises operations under `operation_lock`
(`regional_game_runner.rs:218`), so the counter is **interior-mutable state guarded by that
same lock** (a `Cell<u8>`/`Mutex<u8>`), not a `&mut self` field — `advance` keeps the
existing `&self` signature.

So **the answer to "who handles tick and sub-tick": the runner — automatically.** The UI
only calls `advance()`; the runner runs a movement sub-tick every call and slips in the
heavy hourly `tick_city` at each hour boundary (every 6th). One game hour = 1 hourly tick
(economy + `resolve` sets goals) + 6 movement sub-ticks (which do all depart/step/cross).
The counter is **not** in the UI
(stays ignorant) and **not** in the worker (stays a passive executor).

Interaction: P4's `traveler_views` already renders from `travel` + `visiting_travel`, so
dots move every `advance()` with no UI change. Today's `tick_city` button maps to "call
`advance()` 6 times" (one whole hour) if you want the old one-press-one-hour feel; a faster
animation just calls `advance()` more often. Paused ⇒ the UI stops calling `advance()` ⇒
frozen (unchanged policy).

---

## Decisions locked

- **Geometric cost via `step_cost`: 2× for a T-junction *or a turn* (a 90° corner), 4× for a
  4-way, 1× for a straight pass.** The same function weights P1 routing (turn charged against
  each cell's fixed forward direction in the `came_from` tree), so the routed path is a strong
  **heuristic** for the fastest — not a strict guarantee (endpoint convention + single-tree
  turns, §2b/§4a). Exact turn-penalty routing (`(cell, dir)` states) is the named ceiling.
- **The mover stores `prev_cell`** so the turn at the current cell is known; both `prev_cell`
  and `dwell` are `#[serde(skip)]`.
- **The UI just calls `advance()`; the runner owns the tick/sub-tick cadence.** Every
  `advance()` is one movement sub-tick; the runner auto-fires the hourly `tick_city` at each
  hour boundary (every 6th). The counter lives in `RegionalGameRunner`, never the UI or worker.
- **All regions sub-tick in lockstep** (`step_travel_city` always steps every region);
  single-region sub-ticking with tokens in flight is forbidden (P5 constraint).
- **Split `travel::run`** into `resolve` (hourly: set destination goal + prune) and
  `step_travel` (sub-tick: all motion — depart/step/arrive/cross).
- **6 sub-ticks per game hour**; economy stays hourly and authoritative.
- **No save-format change** — `dwell`, `prev_cell`, and all travel state remain `#[serde(skip)]`.

## Risks / notes

- Changing P1 weights (`1/3` → geometric `1/2/4` with turns) alters route choices on existing
  maps — re-baseline any P1 routing tests. Economy unaffected (`road_distances` is separate).
- Lockstep adds a per-sub-tick barrier (6× the routing passes per hour). Cheap: `step_travel`
  touches only travellers, not the economy; discovery is the main per-pass cost (cache later
  if profiling shows it).
- Cross-region crossings now resolve mid-hour but remain **one-sub-tick-stale** — the same
  guarantee as P5, finer. A handoff still cannot skip two regions in one sub-tick (barrier).

---

## Implemented — P7a (turn-aware cost unification) · `systems/road_network_analysis.rs`

The routing half of the cost change: P1's degree-only `1 + crossing_penalty` becomes
the geometric `step_cost` that P7b's mover will also use.

```text
  road_predecessors_inner relax (current → neighbor), per neighbor:
    forward = came_from[current]            // current's fixed exit (None at a root)
    nd = cost + step_cost(world, Some(neighbor), current, forward, degree)

  step_cost(in, current, out, degree):
    degree ≥ 4                         → 4   4-way
    degree = 3  OR  in ⊥ out (a turn)  → 2   T-junction or 90° corner
    else (straight, in ∥ out)          → 1
    turn ⇔ !collinear(in, current, out)      // midpoint check: in.x+out.x == 2·cur.x
                                             //               && in.y+out.y == 2·cur.y
```

The turn is charged against `current`'s **single** forward direction in the `came_from`
tree (the standard approximation of `(cell, direction)` turn-penalty routing — see §3
ceiling). Weights move from `1/3` to `1/2/4`-with-turns; this feeds **only** the travel
route cache — `road_distances` (economy/happiness/inspect) stays plain BFS, so gameplay
balance is untouched.

Re-baselined `road_predecessors_crossing_penalty_charged` (an arm reached through the
4-way now costs 5, was 4) and added `road_predecessors_turn_costs_like_a_junction` (a
corner costs 2 vs a straight pass 1). codex clean; gates green.

---

## Implemented — P7b (dwell mechanism) · `components.rs`, `systems/travel.rs`

Makes crossings and turns cost **time**. `TravelState` gains `dwell: u16` (sub-ticks
spent on the current cell) and `prev_cell: Option<Entity>` (the entry, so the turn at
the current cell is known) — both `#[serde(skip)]` via the travel map.

```text
  travel::run, per en-route traveller (before advance):
    cost = dwell_cost_for(target, state)   = step_cost(prev_cell, cell, came_from-next, degree)
           └─ None when idle / off-graph / unreachable / about-to-arrive-or-cross
              (advance handles those instantly, ungated)
    if dwell + 1 < cost:  dwell += 1; stay on cell        // still traversing
    else:                 advance one cell; prev_cell = cell just left; dwell = 0

  straight cell (cost 1):  0+1 < 1 false → advance every tick   (unchanged)
  4-way     (cost 4):      dwell 0,1,2,3 → advance on the 4th    (held 4 ticks)
```

`advance()`'s state machine is **unchanged** — the gate just decides *whether* to call
it this tick. The same gate runs for visiting tokens in `step_visiting` (host side). A
stale committed border-exit (no longer a candidate) is left ungated so `advance` re-picks
immediately.

Granularity note: `run` is still the hourly pass, so dwell is in **hours** here (a 4-way
takes 4 hourly ticks); **P7c** rebases `run` → `resolve` (hourly) + `step_travel` (6×
10-min sub-ticks), turning each dwell unit into 10 minutes. Display-only — `road_distances`
(economy) is untouched, so balance is unaffected.

`road_degree_in_network` is now `pub(crate)`. New test `dwell_holds_a_traveller_on_a_4way`
(the 4-way holds for 4 sub-ticks); the 23 existing travel tests pass unchanged (their
roads are straight, cost 1). codex clean; gates green.

---

## Implemented — P7c (lockstep sub-tick driver + cadence) · `simulation.rs`, `travel.rs`, `regions/{mod,runtime}.rs`, `regional_game_runner.rs`, `regional_game.rs`, `ui/city_driver.rs`

Decouples movement from the hourly economy tick and drives it as a 10-minute
sub-tick across all regions in lockstep.

```text
  UI: CityDriver::tick()  (one key press = one game hour)
        │  calls 6×
        ▼
  RegionalGame::advance()            sub_tick 0..6 under a Mutex (serialised cadence)
        ├─ if sub_tick == 0 → tick_city()      // hourly economy (unchanged)
        ├─ step_travel_city()                  // one movement sub-tick (all regions)
        └─ sub_tick = (sub_tick + 1) % 6
        ▼
  RegionalGameRunner::step_travel_city()       ONE barrier pass (not looped):
        ├─ broadcast RegionEvent::StepTravel to every region mailbox
        ├─ each worker: process_region_events_for_barrier(usize::MAX)
        │     └─ region: [pending ReceiveTravelers] then StepTravel
        │            RegionEvent::StepTravel → RegionState::step_travel()  (travel::step_travel)
        │                                    + drain crossings → TravelerHandedOff
        └─ collect forwarded handoffs → sort by order key → deliver_forwarded_events
               (delivered to neighbour inboxes for the NEXT sub-tick — one-sub-tick-stale)
```

### Key changes

- **`simulation.rs`**: `travel::run` removed from `finish_tick_after_goods_phase` — the
  hourly tick no longer moves travellers.
- **`travel.rs`**: `run` → `step_travel` (the sub-tick pass). It **re-resolves the
  schedule each call** — cheap, and `advance` already no-ops on an unchanged target, so
  no separate `resolve`/`goal` was needed (a simplification over §4b). dwell (P7b) is now
  in 10-minute units (6 calls/hour).
- **`RegionEvent::StepTravel`** + **`RegionState::step_travel`**: one movement sub-tick,
  then the P5b drain emits the crossings it buffered.
- **`RegionalGameRunner::step_travel_city`**: the single-pass, full-inbox barrier (reuses
  `process_region_events_for_barrier` + `deliver_forwarded_events` — no new worker command).
- **`RegionalGame::advance`** + `sub_tick: Mutex<u8>`: the runner-owned cadence; the whole
  read→tick→step→bump sequence is held under the mutex so concurrent `Arc` callers can't
  interleave (an `AtomicU8` would protect the byte but not the sequence).
- **`CityDriver::tick`**: now `advance()` ×6 — one press = one hour = 1 economy tick + 6
  movement sub-ticks, preserving the existing UI feel while routing all movement through
  the sub-tick path.

### Determinism / balance

The cross-worker barrier sort (`sort_forwarded_events` by the rank-3 order key) is
independent of thread timing; cross-region crossings are one-**sub**-tick-stale (the same
model as P5, finer). The economy still runs exactly once per game hour. Movement is
display-only (`road_distances`/economy untouched), so balance is unaffected.

New test `advance_fires_economy_once_per_six_subticks`; the dwell/travel/cross-region
unit tests cover movement; full suite green. codex clean (opencode skipped by request).

**P7 is complete** (P7a cost · P7b dwell · P7c sub-tick driver). A smooth per-frame
render (calling `advance` once per animation frame instead of 6× per press) is the only
remaining polish, deferable to a future patch.

---

## Implemented — P7d (smooth cell-by-cell animation, UI-only) · `ui/city_driver.rs`, `ui/tui.rs`

Renders the sub-ticks instead of jumping a whole hour per frame. **No core change** —
`RegionalGame::advance()` (P7c) is the public seam; this patch only changes *how often*
the TUI calls it.

```text
  before (P7c): auto step → game.tick() = advance() ×6 → ONE render   (dot jumps ≤6 cells)
  after  (P7d): auto step → game.advance() = 1 sub-tick → render      (dot steps 1 cell)
                fired every RunSpeed::interval(): 1× 500ms · 2× 250ms · 4× 125ms
```

- **`CityDriver::advance() -> Option<CommandResult>`**: passthrough to
  `RegionalGame::advance()`. `Some` = the hourly economy result (every 6th sub-tick) or an
  error, to surface on the status line; `None` = a movement-only sub-tick (leave the line).
  `CityDriver::tick()` (= `advance()` ×6 = one hour) is unchanged, so `ascii.rs` keeps its
  one-press-one-hour behaviour.
- **`tui.rs`**: `apply_due_auto_tick` calls `advance()` (one sub-tick); `manual_tick` (the
  `N` key) advances **one 10-minute sub-tick** (option a). `RunSpeed::interval()` is now the
  auto-advance cadence (`AUTO_ADVANCE_INTERVAL = 500ms` at 1×, ~2 sub-ticks/sec); the speed
  labels/help text say "advancing every 500ms/250ms/125ms" and "Advance 10 min".

**Pacing implication (by design):** at 1× the game runs ~1 hour per 3 real-seconds (6
sub-ticks at 2/sec) — slower than the old 1 hour/sec, in exchange for watchable cell-by-cell
motion; 2×/4× scale it. The **economy cadence is unchanged** (exactly one tick per game
hour), so balance is unaffected — only wall-clock pacing changed.

New test `auto_advance_runs_economy_once_per_six_subticks`; 5 label/interval/help tests
updated. codex clean (opencode skipped by request). The remaining sub-cell pixel tween
(§"Two levels of smooth", level 2) stays deferred.
