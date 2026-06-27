# Inspect road travelers — who is on this road cell

Status: **plan** (not implemented). UI/interface feature on top of the committed
travel work (P3 dots, P5 cross-region tokens, P7 sub-tick movement). Pairs with
`docs/travel-subtick-plan.md`.

---

## 1. Introduction — the problem

Today the inspect panel shows a per-**building** roster (`InspectView.roster`,
`view.rs:234`): residents of a home, or local workers of a workplace
(`adapter.rs:195 citizen_roster`). A **road** cell, however, inspects to
`InspectDetailsView::Road` (`view.rs` variant) with **no information about the
travellers standing on it** — the P4/P7 dots show *that* someone is commuting, but
not *who*, *where they're going*, or *where they came from*.

The data already exists in the ECS: `world.travel: HashMap<Entity, TravelState>`
(local commuters) and `world.visiting_travel: HashMap<TravelerId, VisitingToken>`
(cross-region tokens), each with a `current_cell`. The inspect adapter just doesn't
surface it.

**Goal.** Hovering the cursor onto a road cell lists the citizens currently on it,
each showing:

1. **Activity** — heading to *Work* or *Home* (classified from where they're walking;
   *Leisure* is a reserved label, unreachable in v1).
2. **Destination** — the building/region they're walking to (from `TravelState`).
3. **Origin** — where they came from (the inferred other endpoint of the trip).

Read-only, deterministic, no simulation/economy change.

---

## 2. Proposal

A new `InspectView.travelers: Vec<RoadTravelerView>`, populated by the adapter only
when the inspected cell is a road, rendered by the TUI as a second list below the
(empty, for roads) building roster.

### Data flow — reuses the existing inspect path, no new event

```text
  TUI cursor on (x,y)
     │  CityDriver::inspect (city_driver.rs:193)
     ▼
  RegionalGame::inspect_selected_region (regional_game.rs:478)
     │  → RegionalGame::inspect_region (regional_game.rs:467)
     │  ── runner ── RegionalGameRunner::inspect_region (regional_game_runner.rs:291)
     ▼
  RegionState::inspect (regions/mod.rs:402)
     │
     ▼
  interface::adapter::inspect_world (adapter.rs:164)   ◄── the ONLY ECS→view boundary
     ├─ existing: roster = citizen_roster(world, x, y)          (building roster)
     └─ NEW:      travelers = road_travelers(world, x, y)       (road traveller list)
                  │ reads world.travel + world.visiting_travel filtered to this cell
                  │ classifies activity + origin/destination from TravelState
                  ▼
              InspectView { …, roster, travelers }  → TUI renders both
```

This is the **same synchronous inspect path** the roster already uses. It *does* cross
to the worker thread (`ThreadedRegionWorker::inspect_region`, `threaded.rs:128` sends an
inspect command and blocks for the reply) — but it adds **no new `RegionEvent` and no
barrier pass**. The data is read from the inspected region's own `World`, so there is no
cross-region staleness to reason about for the gathering itself.

### The cross-region wrinkle (share-nothing)

Two kinds of traveller can stand on a local road:

```text
  world.travel[citizen]            world.visiting_travel[TravelerId]
  ───────────────────────          ────────────────────────────────────────
  a LOCAL citizen                  a NEIGHBOUR's citizen, here as a token (P5)
  full Citizen data available      NO Citizen data (share-nothing — it lives in
   → home, workplace + TravelState   the home region's World)
   → dest from state.destination     → only TravelerId.citizen.region() (home region)
     (local exact; remote region        and token.destination (the local workplace)
      best-effort), origin inferred
                                      → activity = Work, dest = local cell, origin =
                                        "region R" (id only, no coords)
```

So a **local** traveller renders fully; a **visiting** token renders partially
(Work, to a local workplace cell, from a named neighbour region but no home coords).
That asymmetry is inherent to share-nothing and is shown honestly, not faked.

### Per-traveller resolution (local citizen)

The **destination is read from `TravelState.destination`** — the building/cell the
citizen is *actually* walking toward this trip — **not** re-derived from
`schedule_intent`. That matters: with P7 sub-ticks a commuter can still be mid-route
when the hour flips (e.g. 15:00 Work→Home), so the live intent can disagree with the
trip in progress; `destination` is the truth. The **activity** is then *classified
from the destination*, and the **origin is the inferred complementary endpoint**:

```text
  dest = state.destination?                             // None → skip (a traveller always has one)
  classify dest BY ITS OWN NATURE (robust to a mid-trip assignment change):
     ├─ dest == citizen.home          → activity Home, origin ≈ workplace
     ├─ dest is a border-exit road cell → activity Work (remote), origin ≈ home,
     │     destination = RemoteRegion(workplace.region()) or Unknown  (region best-effort)
     └─ else (a local building cell)  → activity Work, origin ≈ home,
           destination = its coords (or Unknown if bulldozed mid-trip)
  endpoint → Local{x,y} via world.positions, RemoteRegion(r), or Unknown (no position)
```

Activity is therefore always `Home` or `Work` — never `Unknown`; an entry that can't
be classified (no `destination`) is **skipped**, not shown blank. `Unknown` only
appears as a *`TripEndpointView`* when a building's position can't be resolved.

`origin` is **best-effort** (the complementary home↔work endpoint), not the literal
departed-from building — `TravelState` doesn't store one (see §3 ceiling).

---

## 3. Important structures / functions

### `RoadTravelerView` (NEW, `interface/view.rs`)

```rust
pub struct RoadTravelerView {
    pub activity: TravelActivity,      // Home | Work | Leisure (where they're headed)
    pub origin: TripEndpointView,      // where they came from
    pub destination: TripEndpointView, // where they're going
}
pub enum TravelActivity { Home, Work, Leisure }
pub enum TripEndpointView {
    Local { x: usize, y: usize },      // a building cell in this region
    RemoteRegion(RegionId),            // a neighbour region (no local coords)
    Unknown,                           // position unresolvable (e.g. bulldozed mid-trip)
}
```

UI-safe (coords + tags only, no `Entity`). Lives in the **interface** layer beside
`CitizenDetailView`. `Copy`, `Eq` like the other view models.

### `road_travelers(world, x, y) -> Vec<RoadTravelerView>` (NEW, `interface/adapter.rs`)

The sole ECS→view builder for the list. Contract: returns `[]` unless `(x,y)` is a
road cell; otherwise one entry per local citizen *and* per visiting token whose
`current_cell == grid.get(x,y)`, **deterministically ordered** (locals by
`entity.0`, then visiting by `(citizen.0, generation)`). It reads `TravelState`
(`current_cell`, `destination`), `citizen.home`, and `workplace_assignment`, and
resolves coordinates via `world.positions` exactly as `citizen_roster` does — no
schedule/hour read is needed (activity is classified from the destination, not the
live intent). Reuses `road_connectivity::is_road_entity` (`road_connectivity.rs:120`).

> **Ponytail ceiling — what's exact vs. best-effort.**
> - **Local destinations are exact.** When `TravelState.destination` is a local building,
>   it is the committed target and resolves to its real coordinates (or `Unknown` if the
>   building was bulldozed mid-trip).
> - **A remote destination's *region* is best-effort.** Before crossing, `TravelState.
>   destination` is only the *border-exit road cell*, not the remote workplace — so the
>   shown `RemoteRegion(r)` is read from the *current* `workplace_assignment` and can be
>   wrong (or `Unknown`) if that assignment changed/cleared mid-trip. Upgrade path:
>   derive the region from `remote_exit_cells`/the border hint, or carry the remote
>   workplace in the token.
> - **`origin` is best-effort.** It is **inferred** as the complementary home↔work
>   endpoint — `TravelState` stores no trip origin (`building` is `None` while
>   travelling, `prev_cell` is the last *road* cell). It assumes v1's home↔work commute.
>   Upgrade path: store the departed-from building in `TravelState` and read it here.

> **Honest gap — visiting tokens render partially.** A `VisitingToken` carries no
> `Citizen`, so its `activity` is fixed to `Work`, `destination` is the local
> workplace cell (`token.destination` via `positions`), and `origin` is
> `RemoteRegion(traveler.citizen.region())` — the home region id, no coords. This is
> the share-nothing boundary, not a bug.

### `InspectView.travelers: Vec<RoadTravelerView>` (CHANGED, `view.rs:221`)

New field beside `roster`. Empty for every non-road cell (and for roads with no
travellers). All existing `InspectView { … }` constructors gain `travelers`.

### TUI render (CHANGED, `ui/tui.rs` inspect panel)

A second list under the (road-empty) roster: one row per traveller,
`activity → destination  (from origin)`. v1 is a plain non-selectable list (the
roster's selectable-table machinery is not reused — ponytail: add selection only if
asked).

---

## 4. Pseudocode + interaction with current code

### Adapter (the new builder + the wiring)

```rust
// interface/adapter.rs — NEW, mirrors citizen_roster's shape
fn road_travelers(world: &World, x: usize, y: usize) -> Vec<RoadTravelerView> {
    let Some(cell) = world.grid.get(x, y) else { return Vec::new() };
    if !road_connectivity::is_road_entity(world, cell) { return Vec::new() }   // reuse
    let mut out = Vec::new();

    // Local citizens standing on this cell (sorted by entity.0).
    let mut locals: Vec<(&Entity, &TravelState)> = world.travel.iter()
        .filter(|(_, s)| s.current_cell == Some(cell)).collect();
    locals.sort_by_key(|(id, _)| id.0);
    for (id, state) in locals {
        let Some(citizen) = world.citizens.get(id) else { continue };          // skip pruned
        if let Some(v) = local_traveler_view(world, citizen, state) {          // §2 resolution
            out.push(v);                                                       // skip if no dest
        }
    }

    // Cross-region tokens standing on this cell (sorted by (citizen.0, generation)).
    let mut visiting: Vec<(&TravelerId, &VisitingToken)> = world.visiting_travel.iter()
        .filter(|(_, v)| v.token.current_cell == Some(cell)).collect();
    visiting.sort_by_key(|(t, _)| (t.citizen.0, t.generation));
    for (traveler, v) in visiting {
        out.push(RoadTravelerView {
            activity: TravelActivity::Work,                                     // a token is a commuter
            destination: endpoint_for(world, v.token.destination),             // local workplace cell
            origin: TripEndpointView::RemoteRegion(traveler.citizen.region()),  // home region only
        });
    }
    out
}

// activity + origin/destination CLASSIFIED from the committed destination — not the
// live schedule intent, which can disagree mid-route after an hour flip. Returns None
// (skip) for the impossible no-destination case rather than showing a blank entry.
fn local_traveler_view(world, citizen, state: &TravelState) -> Option<RoadTravelerView> {
    let dest = state.destination?;                  // None → skip (a traveller has one)
    let workplace = citizen.workplace_assignment.map(|a| a.workplace);
    Some(if dest == citizen.home {
        RoadTravelerView { activity: Home,
            destination: endpoint_for(world, Some(citizen.home)),
            origin: workplace_endpoint(world, workplace) }          // origin ≈ workplace
    } else if road_connectivity::is_road_entity(world, dest) {      // a border-exit road cell
        // remote commute, pre-cross: name the workplace region (best-effort).
        RoadTravelerView { activity: Work,
            destination: workplace.map_or(Unknown, |w| RemoteRegion(w.region())),
            origin: endpoint_for(world, Some(citizen.home)) }       // origin ≈ home
    } else {
        // a local building cell (workplace); endpoint_for → coords, or Unknown if
        // it was bulldozed mid-trip (a missing local building, NOT remote).
        RoadTravelerView { activity: Work,
            destination: endpoint_for(world, Some(dest)),
            origin: endpoint_for(world, Some(citizen.home)) }       // origin ≈ home
    })
    // No Leisure branch: v1's schedule never routes to a leisure building, so a local
    // destination is always home or a workplace. Leisure stays a reserved variant.
}
// endpoint_for(world, Option<Entity>): positions.get → Local{x,y}, else Unknown.
// workplace_endpoint(world, Option<Entity>): Local{x,y} if local & resolvable,
//   RemoteRegion(region) if remote, else Unknown.
```

Interaction: `road_travelers` is added to `inspect_world` (`adapter.rs:164`) next to
the existing `roster: citizen_roster(world, x, y)` call — `InspectView` gains the
field, every constructor updates. No core/systems change; `is_road_entity` and
`positions` are reused verbatim (no schedule/hour read). The fallback inspect
(`city_driver.rs` `fallback_inspect`) sets `travelers: Vec::new()`.

### TUI

```rust
// ui/tui.rs — inspect panel, after the roster section
if !inspect.travelers.is_empty() {
    // header "Travellers (N)"; one line per traveller:
    //   "→ Work  @ (3,1)   from Home @ (0,1)"
    //   "→ Work  @ (2,1)   from region 2"          (visiting token)
}
```

Interaction: pure render from the view model (UI never reads ECS). P4's dots already
prove a traveller is on the cell; this panel names them.

---

## Decisions locked

- **Travellers are a new top-level `InspectView.travelers` field**, parallel to
  `roster`, populated only for road cells. Mirrors the existing roster path.
- **Destination = `TravelState.destination`**, **classified by the cell's own nature**
  (home / border-exit road → remote Work / local building → local Work). Local
  destinations are exact (coords, or `Unknown` if bulldozed); a remote trip's *region*
  is best-effort pre-cross (read from the current assignment). **Origin = the inferred
  complementary endpoint** (best-effort, not stored).
- **Visiting tokens render partially** (Work · local cell · home region id) — the
  share-nothing boundary.
- **Deterministic order**: locals by `entity.0`, then visiting by
  `(citizen.0, generation)`. Read-only; no economy/balance change.

## Risks / notes

- The `origin` inference and the leisure branch are tied to v1's commute-only
  schedule; revisit when richer schedules land (store the real origin then).
- Perf: `road_travelers` scans `world.travel` + `world.visiting_travel` per inspect
  (on cursor move, not per frame). Fine at v1 sizes; ponytail upgrade is a
  cell→travellers index if a huge map ever makes it show.
- No staleness concern for gathering (local-world read). A remote *destination* is
  shown as a region id, consistent with how the roster shows remote workplaces.

## Suggested patch split

- **P-a (interface):** `RoadTravelerView`/`TravelActivity`/`TripEndpointView` +
  `road_travelers` + `InspectView.travelers` + adapter tests (local citizen to local
  work; resident going home; remote-workplace destination; a visiting token; empty for
  non-road / road-with-no-travellers).
- **P-b (ui/tui):** render the list in the inspect panel + a render test.
- **P-c (optional, ui/ascii):** the same list in the ASCII frontend.
