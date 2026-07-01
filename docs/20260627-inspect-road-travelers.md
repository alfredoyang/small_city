# Inspect road travelers — how many citizens are on this road cell

Status: **plan** (not implemented). UI/interface feature on top of the current
travel-token implementation: `world.tokens: HashMap<Entity, TravelToken>` with
one token in the region where the body physically is.

---

## 1. Introduction — the problem

Today the inspect panel shows a per-**building** roster (`InspectView.roster`,
`src/interface/view.rs:221`): residents of a home, or local workers of a workplace
(`citizen_roster`, `src/interface/adapter.rs`). A **road** cell inspects to
`InspectDetailsView::Road` with no information about how many travelers are
standing on it. The map dot says *someone* is moving, but not how crowded that
road cell is.

The data already exists locally:

```text
World
 └─ tokens: HashMap<Entity, TravelToken>
      key   = citizen Entity (home-region identity, globally unique)
      value = body token currently in THIS region
              state.current_cell = road cell, when travelling
              home/work          = final endpoints
              trip_gen           = stale-handoff guard
```

`world.tokens` now covers both local moving residents and foreign bodies visiting
or transiting through this region. There is no `world.travel`,
`world.visiting_travel`, `VisitingToken`, or `return_path` in the current model.

**Goal — two tiers, split by cost.**

1. **Hover / inspect** shows only a **count** of tokens on the road using this
   region's local `World`.
2. **Enter** opens the existing citizen-panel popup with full details. Local
   details come from this region. Visitor details reuse the existing
   remote-worker lookup only when the visitor's final workplace is local to this
   region; transit visitors remain count-only in v1 unless we add a precise
   by-citizen remote query.

Read-only, deterministic, no simulation or economy change.

---

## 2. Proposal

Add a local-only road-traveler count to `InspectView`, then let the TUI render
only that count in the inspect panel. Reuse the existing citizen panel for
`Enter`.

### Tier 1 — hover: local-only count

```text
cursor on a ROAD cell
  |
  v
inspect_world(...)
  |
  +-- citizen_roster(...)       existing building roster
  |
  +-- road_traveler_count(...)  NEW, local-only
        |
        +-- scan world.tokens
        +-- keep tokens whose state.current_cell == inspected road cell
```

Example inspect text:

```text
Road
Travellers: 3
```

The count comes from the inspected region's own `World.tokens`. No neighbour
query, no endpoint resolution, and no citizen detail rows on hover.

### Tier 2 — Enter: existing citizen panel

```text
Enter on road with travelers
  |
  v
open render_citizen_panel(...)      existing popup/table
  |
  +-- local traveler details         NEW local facade call, same inspect-style chain
  |
  +-- visitor details, v1            only when a token's work is local to this region:
        remote_workers_at(workplace)
```

Current TUI touchpoints:

- `TuiAction::EnterCell` is handled around `src/ui/tui.rs:939`.
- `cell_has_roster` is currently building-only at `src/ui/tui.rs:1365`.
- `render_citizen_panel` is the reusable popup at `src/ui/tui.rs:1400`.
- `fetch_citizen_remote` at `src/ui/tui.rs:761` is workplace-only; road travelers
  should call `remote_workers_at` directly only for local workplace destinations
  discovered by the Enter-only facade call.
- The state is still `citizen_panel: bool`; a small `PanelMode` enum is optional,
  not required for P-a.

Visitor-detail ceiling:

```text
Region 4 road token:
  home = Region 1
  work = Region 7

remote_workers_at(...) can answer "who works at this local workplace cell?"
It cannot answer "give me Entity(Region1, n) from its home region" today.
```

So, with the current protocol:

- Visitor already in its workplace region: `remote_workers_at(workplace)` can return
  a useful, possibly superset, detail list.
- Visitor transiting through a middle region: hover shows only the count; full
  detail needs a new direct by-citizen/home-region query. Do not fake it.

### Existing inspect data flow

```text
CityDriver::inspect                         src/ui/city_driver.rs:193
  -> RegionalGame::inspect_selected_region  src/core/regional_game.rs:478
  -> RegionalGame::inspect_region           src/core/regional_game.rs:467
  -> RegionalGameRunner::inspect_region     src/core/regional_game_runner.rs:291
  -> ThreadedRegionWorker::inspect_region   src/core/regions/threaded.rs:128
  -> RegionRuntime::inspect                 src/core/regions/runtime/mod.rs:595
  -> RegionState::inspect                   src/core/regions/mod.rs:486
  -> adapter::inspect_world                 src/interface/adapter.rs:170
```

`adapter::inspect_world` is still the ECS-to-view boundary. The UI must not read
`World`, `Entity`, or `TravelToken` directly.

---

## 3. Important structures / functions

### Existing travel data

`src/core/components.rs`

- `TravelToken { state, home, work, trip_gen }`
  - Reused. This is the single source for road bodies.
- `PlaceRef { region, building }`
  - Reused for final home/work endpoints.
- `TravelState { status, current_cell, destination, building, dwell, prev_cell }`
  - Reused only for current road cell and current local leg state.
- `TravelStatus::{AtWork, Traveling}`
  - No `AtHome`, no `Away`.

`src/core/world.rs`

- `tokens: HashMap<Entity, TravelToken>`
  - Reused. Scan this for road travelers.
- `away_residents`, `away_generation`
  - Not used by inspect.

### New view models

`src/interface/view.rs`

```rust
pub struct InspectView {
    ...
    pub road_traveler_count: usize,
}
```

Keep hover inspect intentionally small: a count only, no citizen list and no
endpoint rows.

For Enter, use a separate UI-safe seed instead of storing details in
`InspectView`:

```rust
pub struct RoadTravelerPanelSeedView {
    pub local_details: Vec<CitizenDetailView>,
    pub local_visitor_workplaces: Vec<CityCellRef>,
}
```

`local_visitor_workplaces` is coarse on purpose. The TUI can call the existing
`remote_workers_at(workplace)` for those local workplace cells. Transit visitors
have no local workplace in this region, so they do not get detail rows in v1.

### New adapter helpers

`src/interface/adapter.rs`

- `road_traveler_count(world, x, y) -> usize`
  - New local-only ECS-to-view builder.
  - Returns `0` unless `(x,y)` is a road.
  - Scans `world.tokens`, filters `state.current_cell == cell`.
  - Skips stale local tokens whose citizen no longer exists, matching traveler
    dot rendering.
- `road_traveler_panel_seed(world, x, y) -> RoadTravelerPanelSeedView`
  - Enter-only detail builder.
  - Local citizens: returns `CitizenDetailView` rows.
  - Visitors whose `token.work.region == world.region_id`: returns that local
    workplace cell in `local_visitor_workplaces` so the UI can call
    `remote_workers_at`.
  - Transit visitors: no detail in v1.
  - Same residential perspective as `citizen_roster`: local citizen detail, no
    direct ECS leak.

Target rule for the Enter-only seed:

```text
target = if schedule_phase(hour) == Work {
    token.work.unwrap_or(token.home)
} else {
    token.home
}

is_visitor = token.home.region != world.region_id

if token.home.region == world.region_id:
  local_details.push(citizen_relation(...))

if is_visitor && token.work == Some(local workplace):
  local_visitor_workplaces.push(workplace coords)
```

This mirrors the current stepper in `src/core/systems/travel.rs`: tokens retarget by
`schedule_phase(hour)` and their stored `home/work` endpoints.

### Facade for Enter details

Add the same narrow chain used by inspect:

```text
CityDriver::road_traveler_panel_seed
  -> RegionalGame::road_traveler_panel_seed_selected_region
  -> RegionalGameRunner / ThreadedRegionWorkerCommand
  -> RegionRuntime
  -> RegionState::road_traveler_panel_seed
  -> adapter::road_traveler_panel_seed
```

Visitor detail fetch reuses existing calls only when possible:

- `RegionalGame::remote_workers_at` (`src/core/regional_game.rs:489`)
- `RegionState::remote_workers_for` (current worker-side remote staff path)

No new cross-region protocol is required for P-a/P-b. Precise transit visitor detail is
deferred.

---

## 4. Pseudocode / integration

### Adapter

```rust
fn road_traveler_count(world: &World, x: usize, y: usize) -> usize {
    let Some(cell) = world.grid.get(x, y) else { return 0 };
    if !road_connectivity::is_road_entity(world, cell) {
        return 0;
    }

    world.tokens.iter()
        .filter(|(_, t)| t.state.current_cell == Some(cell))
        .filter(|(citizen, token)| {
            token.home.region != world.region_id || world.citizens.contains_key(citizen)
        })
        .count()
}
```

```rust
fn road_traveler_panel_seed(world: &World, x: usize, y: usize) -> RoadTravelerPanelSeedView {
    let Some(cell) = road_cell(world, x, y) else { return default_seed() };
    let phase = schedule_phase(world.resources.time.hour_of_day());
    let mut seed = RoadTravelerPanelSeedView::default();

    let mut tokens: Vec<_> = world.tokens.iter()
        .filter(|(_, t)| t.state.current_cell == Some(cell))
        .collect();
    tokens.sort_by_key(|(citizen, _)| citizen.0);

    for (citizen, token) in tokens {
        if token.home.region == world.region_id {
            if let Some(detail) = citizen_relation(world, *citizen) {
                seed.local_details.push(detail);
            }
            continue;
        }

        // Visitor details can be fetched with existing remote_workers_at only
        // when the visitor's workplace is local to this region.
        if phase == SchedulePhase::Work {
            if let Some(work) = token.work.filter(|w| w.region == world.region_id) {
                if let Some(pos) = world.positions.get(&work.building) {
                    seed.local_visitor_workplaces.push(CityCellRef { x: pos.x, y: pos.y });
                }
            }
        }
    }

    seed.local_visitor_workplaces.sort();
    seed.local_visitor_workplaces.dedup();
    seed
}
```

### Inspect wiring

```rust
InspectView {
    ...
    roster: citizen_roster(world, x, y),
    road_traveler_count: road_traveler_count(world, x, y),
}
```

Every existing `InspectView` constructor in tests/fallback UI gets
`road_traveler_count: 0`.

### TUI

Smallest useful implementation:

```rust
fn cell_has_panel(inspect: &InspectView) -> bool {
    cell_has_roster(inspect) || inspect.road_traveler_count > 0
}

match action {
    TuiAction::EnterCell if inspect.road_traveler_count > 0 => {
        let seed = self.game.road_traveler_panel_seed(x, y);
        self.state.citizen_panel = true;
        self.state.citizen_selected = 0;
        self.state.citizen_roster = seed.local_details;
        self.state.citizen_remote = self.fetch_road_traveler_remote(seed.local_visitor_workplaces);
        self.state.message = "Travelers (↑/↓ select · Esc close)".to_string();
    }
    ...
}
```

`fetch_road_traveler_remote`:

```text
for each workplace cell from road_traveler_panel_seed:
    call remote_workers_at(workplace)
dedupe/append results

for transit visitor with no local workplace:
    no detail fetch in v1
```

This is deliberately boring. Add `PanelMode` only if the bool starts making the code
awkward.

---

## 5. Tests

P-a interface/adapter:

- road with no tokens -> `InspectView.road_traveler_count == 0`.
- road with local + visitor tokens -> count equals visible tokens.
- removed local citizen token is skipped; foreign token is counted, matching
  `traveler_views`.
- `road_traveler_panel_seed` returns local `CitizenDetailView` rows for local
  travelers.
- visitor in workplace region adds that local workplace to
  `local_visitor_workplaces`.
- transit visitor does not add a fake detail row or remote workplace.

P-b UI:

- inspect panel renders only `Travellers: N` for a road.
- `Enter` on road with travelers opens the existing citizen panel.
- `Enter` on road without travelers still builds.
- local traveler details come through the facade, not direct ECS access.
- visitor in workplace region triggers `remote_workers_at`; transit visitor does not.

P-c optional:

- ASCII inspect renders the same count.

---

## 6. Risks / non-goals

- Full detail for a transit visitor is **not** solved by existing
  `remote_workers_at`; it needs a new by-home-region/by-citizen query. Do not hide
  that with a fake row.
- Hover deliberately does **not** show origin/destination/detail rows. Press Enter
  for details.
- `road_traveler_count` is an O(tokens) scan on inspect/cursor movement. Fine for
  now. Add a cell index only if this shows up in profiling.
- No core movement, routing, worker barrier, or economy behavior changes.
