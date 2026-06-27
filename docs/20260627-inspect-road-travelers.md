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

**Goal — two tiers, split by cost.** The cheap summary is always on screen; the
expensive cross-region detail is fetched only on demand:

1. **Hover (cursor on the road)** — the inspect side panel lists every citizen on the
   cell using **only local data, no cross-region query**: each row shows where it is
   commuting **from**, its **destination**, and the destination building's **type**
   (Residential / Commercial / Industrial, queried locally). For a visiting token the
   local-only facts are: its destination workplace (here) + type, and its **home region
   id** — *not* its name/age/etc. (those live in the neighbour's `World`).

2. **Enter** — opens a **detail popup list**, exactly like pressing `Enter` on a
   building roster. Local citizens' full `CitizenDetailView` (age, happiness, money,
   where they work/live) is read locally; **if there is a visiting token on the road,
   its detail is fetched cross-region by querying `remote_workers_at` on its destination
   workplace** (`regional_game.rs:489`) — the same fan-out the building roster already
   uses for remote staff; the visitor's home region is among those that answer.

So hovering never crosses a region boundary; only `Enter` does, and only when a visitor
is present. Read-only, deterministic, no simulation/economy change.

---

## 2. Proposal

Two pieces: a **local-only inline list** (`InspectView.travelers`) shown on hover, and an
**Enter detail popup** that reuses the building roster's panel and its cross-region fetch.

### Tier 1 — hover: local-only inline list

```text
  cursor on a ROAD cell  →  inspect panel shows, with NO cross-region query:
  ┌─ Travellers (3) ──────────────────────────────┐
  │  from Home (0,1)   →  (3,1) [Commercial]       │   ◄─ local citizen → local workplace
  │  from Home (2,0)   →  Region 2                  │   ◄─ local citizen → remote (region only)
  │  from Region 2     →  (3,1) [Commercial]        │   ◄─ visiting token: dest+type exact,
  └───────────────────────────────────────────────┘       origin = home region id only
```

Every field here comes from the inspected region's own `World` (`world.travel`,
`world.visiting_travel`, `world.buildings`, `world.positions`) — **no neighbour query**.

### Tier 2 — Enter: detail popup, fetching the visitor cross-region

```text
  cursor on a ROAD cell with travellers
     │  Enter (TuiAction::EnterCell, tui.rs:939) — cell_has_roster → cell_has_panel
     ▼
  open the citizen panel (render_citizen_panel, tui.rs:1400) — SAME popup as a building
     │
     ├─ local citizens  → CitizenDetailView read locally (cheap)
     │
     └─ IF a visiting token is present → fetch detail via remote_workers_at on the
        visitor's DESTINATION WORKPLACE W (its home region is among those that answer):

        Host region (worker A)               every region with commuters to W
        ────────────────────────            ─────────────────────────────────
        Enter → remote_workers_at(W)  ──"your residents who work at W?"──►
              (per distinct visitor dest)            RegionState::remote_workers_for
                                                     → its residents commuting to W
              popup ◄────reply: Vec<CitizenDetailView>──◄  (incl. this visitor; maybe more)
        synchronous round-trip, exactly like `remote_workers_at` on a workplace cell.
        (v1 ceiling: a SUPERSET — all commuters to W, not strictly those on this road.)
```

Today `Enter` on a road falls through to *Build* (test `enter_does_not_open_roster_on_a_road`,
`tui.rs:4081`); the change opens the panel when `travelers` is non-empty, in a new
`PanelMode::Travellers`. The building roster's own behaviour is **unchanged**.

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
                  │ resolves origin + destination + queried type from TravelState
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
  full Citizen data available      NO Citizen data locally (share-nothing — it lives
   → home, workplace + TravelState   in the home region's World)
   → inline row from state.destination → inline row from only TravelerId.citizen.region()
     (local dest exact; remote region    (home region id) + token.destination (local
      best-effort), origin inferred       workplace) → dest+type exact, origin = region id
   → Enter detail read LOCALLY        → Enter detail FETCHED via remote_workers_at on the
                                          destination workplace (P5 worker-boundary fan-out)
```

Both kinds appear in the inline list from **local data only**. The difference is the
`Enter` detail: a local citizen's `CitizenDetailView` is read from this `World`; a
visiting token's is **fetched via a destination-workplace remote-worker lookup**
(`remote_workers_at(W)`, the one cross-region step, only on `Enter`). That asymmetry is
inherent to share-nothing, handled honestly.

### Per-traveller resolution (local citizen)

The **destination is read from `TravelState.destination`** — the building/cell the
citizen is *actually* walking toward this trip — **not** re-derived from
`schedule_intent` (with P7 sub-ticks a commuter can still be mid-route when the hour
flips, so the live intent can disagree; `destination` is the truth). Three things come
off it — the destination endpoint, the **queried destination type**, and the inferred
origin:

```text
  dest = state.destination?                             // None → skip (a traveller always has one)
  classify dest BY ITS OWN NATURE (robust to a mid-trip assignment change):
     ├─ dest is a border-exit road cell → destination = RemoteRegion(workplace.region())
     │                                     or Unknown  (region best-effort), kind = None (remote)
     └─ else (a local building cell)    → destination = its coords (Unknown if bulldozed),
                                           kind = world.buildings[dest].kind  (the queried type)
  origin = if dest == citizen.home { workplace endpoint } else { home endpoint }   // best-effort
```

- **`destination_kind`** is the queried building type (`Residential` / `Commercial` /
  `Industrial`) — `None` for a remote destination (the workplace is in another region —
  share-nothing) or an unresolvable cell. This is the type the user wants on each row;
  it also tells home (Residential) from work (Commercial/Industrial) without a separate
  activity field.
- **`origin`** is the inferred complementary endpoint — `dest == home` ⇒ from the
  workplace, otherwise ⇒ from home. Best-effort (no stored trip origin; see §3 ceiling).

The inline row carries **no citizen detail** — that is fetched only on `Enter` (Tier 2).
An entry with no `destination` is **skipped**, not shown blank.

---

## 3. Important structures / functions

### `RoadTravelerView` (NEW, `interface/view.rs`)

```rust
pub struct RoadTravelerView {              // the INLINE row only — local-only, no detail
    pub origin: TripEndpointView,             // where they're commuting from
    pub destination: TripEndpointView,        // where they're going
    pub destination_kind: Option<BuildingKind>, // queried type; None if remote/unresolvable
    pub is_visitor: bool,                     // a cross-region token → Enter fetches its detail
}
pub enum TripEndpointView {
    Local { x: usize, y: usize },      // a building cell in this region
    RemoteRegion(RegionId),            // a neighbour region (no local coords)
    Unknown,                           // position unresolvable (e.g. bulldozed mid-trip)
}
```

UI-safe (coords + tags + the existing `BuildingKind` view type, no `Entity`, no detail).
Lives in the **interface** layer beside `CitizenDetailView`. `Copy`, `Eq` like the other
small view models. `is_visitor` tells the TUI whether `Enter` needs a cross-region fetch
(Tier 2); the detail itself is **not** carried inline.

### `road_travelers(world, x, y) -> Vec<RoadTravelerView>` (NEW, `interface/adapter.rs`)

The sole ECS→view builder for the list. Contract: returns `[]` unless `(x,y)` is a
road cell; otherwise one entry per local citizen *and* per visiting token whose
`current_cell == grid.get(x,y)`, **deterministically ordered** (locals by
`entity.0`, then visiting by `(citizen.0, generation)`). It reads `TravelState`
(`current_cell`, `destination`), `citizen.home`, and `workplace_assignment`; resolves
coordinates via `world.positions`; and **queries the destination type** via
`world.buildings[dest].kind`. **All local** — no `Citizen` detail, no schedule/hour, no
cross-region read. Reuses `road_connectivity::is_road_entity` (`road_connectivity.rs:120`).

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

> **Visiting tokens in the inline list.** A `VisitingToken` carries no `Citizen`, so its
> inline row is built from local facts only: `destination` + `destination_kind` are
> **exact** (`token.destination` is a local workplace here → coords + `Commercial`/
> `Industrial`); `origin` is `RemoteRegion(traveler.citizen.region())` (home region id,
> no coords); `is_visitor = true`. Its full detail is fetched on `Enter` (next).

### Enter-time detail fetch (the one cross-region step)

The popup detail has two halves. The UI cannot build either from the inline
`RoadTravelerView` (no `Entity`, no ECS access), so **both come through facade calls**:

- **Local citizens** → a NEW ECS→view builder, `road_traveler_details(world, x, y) ->
  Vec<CitizenDetailView>` (`interface/adapter.rs`), building each local citizen on the
  cell with the residential `citizen_relation` perspective (`WorksAt`/`Unemployed`,
  `adapter.rs:235`). Local and synchronous — but, like `inspect`, the UI cannot call the
  adapter directly; it **threads the whole `inspect` facade/worker chain** (the cost of
  not crossing the ECS boundary from the UI):

  ```text
  CityDriver::road_traveler_details → RegionalGame::road_traveler_details_selected_region
    → RegionalGameRunner (ThreadedRegionWorkerCommand::RoadTravelerDetails) → worker thread
    → RegionRuntime → RegionState::road_traveler_details → adapter::road_traveler_details
  ```
  Mechanical, mirrors the existing `inspect`/`remote_workers_at` plumbing exactly.
- **Visiting tokens** → `RegionalGame::remote_workers_at` (`regional_game.rs:489`) on the
  visitor's **destination workplace W**, which fans out to every region's
  `RegionState::remote_workers_for` (`regions/mod.rs:413`); the region that owns the
  visitor (its home) returns its `CitizenDetailView`. Synchronous, like the building
  roster's remote staff. (`fetch_citizen_remote` `tui.rs:767` is workplace-only, so the
  `Travellers` mode calls `remote_workers_at(W)` directly.)

The TUI concatenates the two into the popup. (A single combined facade
`road_traveler_details_at(x,y)` returning local + remote is the obvious convenience, but
keeping the local builder and the existing `remote_workers_at` separate reuses more.)

> **Ponytail ceiling — visitor fetch granularity.** v1 keys the fetch by the visitor's
> **destination workplace** (reusing `remote_workers_at` verbatim — zero new cross-region
> protocol), so it returns every remote commuter to that workplace, which can be a
> *superset* of the visitors on this one road cell. Upgrade path: a precise per-`TravelerId`
> query (route by `traveler.citizen.region()` + entity) when exactness matters. Decide
> P-b vs. this reuse at implementation time (see split).

### `InspectView.travelers: Vec<RoadTravelerView>` (CHANGED, `view.rs:221`)

New field beside `roster`. Empty for every non-road cell (and for roads with no
travellers). All existing `InspectView { … }` constructors gain `travelers`.

### TUI (CHANGED, `ui/tui.rs`)

- **Hover** renders `inspect.travelers` as a small inline list in the inspect panel
  (`from {origin} → {destination} [{kind}]`) — no panel, no fetch.
- **`Enter`** on a road with travellers opens the existing **citizen panel popup**
  (`render_citizen_panel`, `tui.rs:1400`) — the same control a building uses — populated
  with `CitizenDetailView`s: local citizens read locally, plus a **cross-region fetch for
  any visitors** via `remote_workers_at` on each visitor destination (cached on open and
  refreshed on tick like the building roster). `cell_has_roster` → `cell_has_panel`
  so the panel opens on a road too. No new widget — `citizen_selected` /
  `handle_citizen_panel_key` / `render_citizen_panel` are reused; the road path calls
  `remote_workers_at` directly rather than `fetch_citizen_remote` (which is workplace-only).

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
        let dest = v.token.destination;                                        // local workplace
        out.push(RoadTravelerView {
            origin: TripEndpointView::RemoteRegion(traveler.citizen.region()),  // home region only
            destination: endpoint_for(world, dest),                            // local cell, exact
            destination_kind: kind_of(world, dest),                            // Commercial/Industrial
            is_visitor: true,                                                  // Enter fetches detail
        });
    }
    out
}

// destination + queried type + inferred origin, from the committed destination — not
// the live schedule intent (which can disagree mid-route after an hour flip). Returns
// None (skip) for the impossible no-destination case rather than a blank row.
fn local_traveler_view(world, citizen, state: &TravelState) -> Option<RoadTravelerView> {
    let dest = state.destination?;                  // None → skip (a traveller has one)
    let workplace = citizen.workplace_assignment.map(|a| a.workplace);
    // origin ≈ the OTHER endpoint: leaving home ⇒ came from work, else came from home.
    let origin = if dest == citizen.home {
        workplace_endpoint(world, workplace)
    } else {
        endpoint_for(world, Some(citizen.home))
    };
    let (destination, destination_kind) = if road_connectivity::is_road_entity(world, dest) {
        // border-exit road cell → remote commute, pre-cross: name the region (best-effort),
        // type unknown (the workplace is in another region — share-nothing).
        (workplace.map_or(Unknown, |w| RemoteRegion(w.region())), None)
    } else {
        // a local building cell (home or workplace); coords + its queried type, or
        // Unknown if bulldozed mid-trip (a missing local building, NOT remote).
        (endpoint_for(world, Some(dest)), kind_of(world, Some(dest)))
    };
    Some(RoadTravelerView { origin, destination, destination_kind, is_visitor: false })
}
// endpoint_for(world, Option<Entity>): positions.get → Local{x,y}, else Unknown.
// workplace_endpoint(world, Option<Entity>): Local{x,y} if local & resolvable,
//   RemoteRegion(region) if remote, else Unknown.
// kind_of(world, Option<Entity>): world.buildings[e].kind → view BuildingKind, else None.
```

Interaction: `road_travelers` is added to `inspect_world` (`adapter.rs:164`) next to
the existing `roster: citizen_roster(world, x, y)` call — `InspectView` gains the
field, every constructor updates. No core/systems change; `is_road_entity`,
`positions`, `buildings`, and `citizen_relation` are reused verbatim (no schedule/hour
read). The fallback inspect (`city_driver.rs` `fallback_inspect`) sets `travelers: Vec::new()`.

### TUI — inline list on hover, popup (with fetch) on Enter

```rust
// ui/tui.rs

// Tier 1 — hover: render inspect.travelers inline in the inspect panel, no fetch:
//   "from Home (0,1)   →  (3,1) [Commercial]"
//   "from Region 2     →  (3,1) [Commercial]"     (is_visitor = true)

// Tier 2 — Enter: open the SAME citizen panel popup a building uses, and fetch visitors.
fn cell_has_panel(inspect: &InspectView) -> bool {            // was cell_has_roster
    cell_has_roster(inspect) || !inspect.travelers.is_empty()
}
// EnterCell (tui.rs:939): on a road with travellers, open citizen_panel in PanelMode::
// Travellers (a NEW small enum replacing the citizen_panel: bool flag) and populate the
// detail rows like a building roster — BOTH halves via the facade (the UI has no ECS):
let local = self.game.road_traveler_details(x, y)?;     // NEW local builder (§3) — like inspect
let visitors = if inspect.travelers.iter().any(|t| t.is_visitor) {
    self.game.remote_workers_at(W)?    // per distinct visitor destination W — like the roster
} else { Vec::new() };
self.state.panel_mode = Some(PanelMode::Travellers);   // replaces citizen_panel: bool;
                                       // reuse render_citizen_panel + citizen_selected, rows = local ++ visitors
// Flips the test `enter_does_not_open_roster_on_a_road` → opens iff travellers present.
```

Interaction: hover renders from the view model only (UI never reads ECS). `Enter` reuses
the `citizen_panel` machinery (`citizen_selected`, `handle_citizen_panel_key`,
`render_citizen_panel`) and calls `RegionalGame::remote_workers_at` for the visitor
detail (the road path calls it directly rather than `fetch_citizen_remote`, which is
workplace-only) — the same cross-region round-trip, keyed by the road's visitor
destinations (see the §3 granularity ceiling).

---

## Decisions locked

- **Two tiers split by cost.** Hover → a **local-only** inline list (`InspectView.travelers`);
  `Enter` → the existing citizen-panel popup with full detail, fetching visitors
  cross-region. Hovering never crosses a region boundary.
- **Inline row = origin + destination + queried `destination_kind`** (the building type)
  + `is_visitor`. No "activity" field — the type (Residential⇒home, Commercial/Industrial⇒
  work) carries it. Local destinations + types are exact; a remote trip's region is
  best-effort pre-cross, its type `None` (share-nothing). **No `detail` carried inline.**
- **`Enter` reuses the building roster's panel *and* its remote fetch.** Both halves come
  through the facade (the UI has no ECS): local citizens via a NEW `road_traveler_details`
  builder, visitors via `remote_workers_at(W)` on the destination workplace (superset; see
  ceiling). A new `PanelMode::Travellers` enum replaces the `citizen_panel: bool` flag.
- **Origin = the inferred complementary endpoint** (best-effort, not stored).
- **Deterministic order**: locals by `entity.0`, then visiting by
  `(citizen.0, generation)`. Read-only; no economy/balance change.

## Risks / notes

- The `origin` inference is tied to v1's commute-only schedule; revisit when richer
  schedules land (store the real origin then).
- The visitor `Enter` fetch is **the only cross-region step** and reuses the existing
  synchronous remote-worker round-trip (same one-tick-staleness profile as the building
  roster's remote staff — it reads the neighbour's last published state). v1's
  destination-keyed fetch can show a superset of the road's visitors (§3 ceiling).
- The panel gains a `Travellers` mode beside `Roster`; keep the branch obvious and
  re-baseline the panel tests (`enter_does_not_open_roster_on_a_road` flips).
- Perf: `road_travelers` scans `world.travel` + `world.visiting_travel` per inspect (on
  cursor move, not per frame); the visitor fetch happens only on `Enter`. Fine at v1
  sizes; ponytail upgrade is a cell→travellers index if a huge map makes it show.

## Suggested patch split

- **P-a (interface):** `RoadTravelerView` (`origin` / `destination` / `destination_kind`
  / `is_visitor`) + `TripEndpointView` + `road_travelers` + `InspectView.travelers`, plus
  the `road_traveler_details(world, x, y) -> Vec<CitizenDetailView>` local builder exposed
  via `RegionState`/`RegionalGame` (like `inspect`) + adapter tests (local→local-work with
  kind; resident going home; remote-workplace destination → region + `kind None`; a
  visiting token → exact dest+kind, `is_visitor`; the local-detail builder; empty for
  non-road / road-with-no-travellers). **Local-only, no cross-region code.**
- **P-b (ui/tui):** Tier-1 inline render of `inspect.travelers`; Tier-2 `Enter` →
  `cell_has_panel` opens the popup in `PanelMode::Travellers`, rows = `road_traveler_details`
  (local) ++ `remote_workers_at(W)` per visitor destination (the superset fetch); flip
  `enter_does_not_open_roster_on_a_road` + open/render/fetch tests. (If precise
  per-`TravelerId` fetch is wanted, that core query is a separate P-b′ before this.)
- **P-c (optional, ui/ascii):** the same in the ASCII frontend.
