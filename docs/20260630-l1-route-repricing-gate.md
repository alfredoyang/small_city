# 20260630 L1 Route Repricing Gate

## 1. Introduction / Current State

This document tracks the Layer-1 route repricing gate idea against the **current
implementation**.

Current code has a `road_topology_dirty` flag. After every processed region event
batch, `RegionWorker::process_region_events_with_mode(...)` always installs the
current Layer-1 exits before events, but it recomputes and republishes the road
report **only when local road topology changed**:

```text
RegionWorker::process_region_events_with_mode
  |
  +-- install current directory.exits_from(region)
  |
  +-- runtime.process_some_events(...)
  |
  +-- runtime.ensure_derived_state()
  |
  +-- if runtime.state().is_road_topology_dirty()
        |
        +-- runtime.state().network_border_links()
        +-- border_neighbor_map_for_region(...)
        +-- runtime.state().road_report(...)
        +-- directory.publish_region_road_report(...)
        |
        +-- directory.exits_from(region)
        +-- runtime.set_region_routes(...)
        +-- runtime.state().clear_road_topology_dirty()
```

`RegionDirectory::publish_region_road_report(...)` is idempotent:

```text
same report as last snapshot -> return false, no rebuild
changed report              -> rebuild discovery + RegionRoutes
```

So the directory still avoids rebuilding when the report is unchanged, and the
worker now also skips the pre-publish work when the local road graph is clean:

```text
network_border_links()
border_neighbor_map_for_region(...)
road_report(...)
```

That means pure movement events (`StepTravel`, handoff receive, grants) keep the
installed route exits and do not reprice local roads.

## 2. Current Routing Data Flow

The current Layer-1/Layer-2 routing shape is:

```text
RegionState::road_report
  -> RegionRoadReport {
       border_links,
       crossing_costs,
     }

RegionDirectory::publish_region_road_report
  -> build_region_routes(...)
  -> RegionRoutes

RegionDirectory::exits_from(A)
  -> HashMap<final_target_region, Vec<ExitLink>>

RegionState::set_region_routes
  -> World.remote_exit_cells:
       HashMap<final_target_region, Vec<RouteExit>>
```

Important distinction:

```text
ExitLink  = which border to use + next region + remaining route cost
RouteExit = ExitLink + concrete local road cell to walk to
```

Diagram:

```text
Directory / L1
  ExitLink { link: East/0, to_region: B, cost: 10 }
        |
        | RegionState::set_region_routes maps East/0 to local road cell r42
        v
World.remote_exit_cells / L2
  RouteExit { cell: r42, link: East/0, to_region: B, cost: 10 }
        |
        | travel picks by local weighted distance + RouteExit.cost
        v
PendingHandoff::Move { exit_cell: r42, exit_link: East/0, to_region: B }
```

`RouteHop` now contains only `exits: Vec<ExitLink>`. There is no separate
`RouteHop.cost`; each `ExitLink.cost` is the value the mover uses.

## 3. Important Functions And Structures

`src/core/regions/worker.rs`

- `RegionWorker::process_region_events_with_mode(...)`
  - Installs `directory.exits_from(source_region)` both before and after event
    processing when roads changed.
  - Gates the post-event `RegionRoadReport` publish behind
    `runtime.state().is_road_topology_dirty()`.

`src/core/regions/mod.rs`

- `RegionState::road_report(...)`
  - Builds `RegionRoadReport` from current local road graph and border neighbour
    facts.
- `RegionState::set_region_routes(...)`
  - Converts `ExitLink` values from the directory into local `RouteExit` cells.
- `RegionState::{is,clear}_road_topology_dirty()`
  - Thin wrappers used by the worker gate.
- `ExitLink`
  - L1 route answer: border link, immediate next region, remaining route cost.
- `RouteExit`
  - L2 movement answer: `ExitLink` plus local road cell.

`src/core/regions/directory.rs`

- `RegionDirectory::publish_region_road_report(...)`
  - Idempotent publish.
  - Rebuilds discovery/routes only when the report changed.
- `build_region_routes(...)`
  - Builds the border-node graph and emits per-exit `ExitLink.cost`.

`src/core/world.rs`

- `derived_dirty: Cell<bool>`
  - Exists today.
  - Tracks applied derived state after config changes.
- `road_topology_dirty`
  - Exists today.
  - Set by road placement/removal, cleared after the worker successfully
    republishes and reinstalls routes.

`src/core/systems/placement.rs`

- `place_building(...)`
  - If `kind == BuildingKind::Road`, clears route cache and marks
    `road_topology_dirty`.

`src/core/systems/entity_cleanup.rs`

- `remove_entity(...)`
  - If the removed entity was a road, clears route cache and marks
    `road_topology_dirty`.

## 4. Implemented Gate

The implementation adds a road-topology flag next to `derived_dirty` and gates
only the road-report publish path.

```text
World
  derived_dirty        = applied derived state is stale
  road_topology_dirty  = local road graph / border links may be stale

TODO: both flags are coarse. If config mutation grows, split/optimize them by
affected subsystem.
```

Mark `road_topology_dirty` at the same chokepoints that already invalidate the
route cache:

```text
place road
remove road
replace road <-> non-road
future road-affecting upgrade, if one appears
```

Before the gate:

```text
event batch
  -> ensure_derived_state()
  -> always recompute road_report()
  -> publish_region_road_report()
  -> set_region_routes()
```

Current gated flow:

```text
event batch
  -> ensure_derived_state()
  -> if road_topology_dirty:
       recompute road_report()
       publish_region_road_report()
       set_region_routes(directory.exits_from(region))
       clear road_topology_dirty
     else:
       keep installed route exits
```

`publish_region_road_report(...)` remains idempotent. It is the safety net for
false positives.

## 5. Pseudocode / Integration

Flag in `World`:

```rust
pub(crate) struct World {
    derived_dirty: Cell<bool>,
    road_topology_dirty: Cell<bool>,
}

// TODO: derived_dirty and road_topology_dirty are coarse command-side
// invalidation flags. Split by affected subsystem if config mutation grows.
```

Marked at road mutation chokepoints:

```rust
if a road was placed/removed/replaced {
    world.clear_route_cache();
    world.mark_road_topology_dirty();
}
```

Worker publish path:

```rust
outbound.extend(runtime.process_some_events(max_events_per_region));
runtime.ensure_derived_state();

if runtime.state().is_road_topology_dirty() {
    let links = runtime.state().network_border_links();
    let neighbours = border_neighbor_map_for_region(..., &links, ...);
    let report = runtime.state().road_report(&neighbours);

    self.directory.publish_region_road_report(report);

    let exits = self.directory.exits_from(source_region).unwrap_or_default();
    runtime.set_region_routes(&exits);

    runtime.state().clear_road_topology_dirty();
}
```

The worker clears the flag after publishing and reinstalling `region_routes`.

## 6. Tests

Focused tests:

- `step_travel_does_not_republish_road_report`
  - enqueue `RegionEvent::StepTravel`
  - process one barrier pass
  - assert directory route rebuild count does not increase

- `build_road_republishes_road_report`
  - build a road
  - process one pass
  - assert road report publish/rebuild happens

- `build_house_does_not_republish_road_report`
  - build a non-road building
  - assert no road-report rebuild

- `preview_build_does_not_republish_road_report`
  - preview only
  - assert no road-report rebuild

- `bulldoze_road_republishes_road_report`
  - create and then bulldoze a road
  - assert road report publish/rebuild happens

No UI tests: this is worker/core routing maintenance only.

## 7. Risks / Non-goals

- This does not change Layer-1 route semantics.
- This does not change `ExitLink`, `RouteExit`, or handoff routing.
- The flag must be marked at every road-topology mutation; missing one can leave
  stale route prices installed.
- Keep the idempotent directory publish. It protects against false positives and
  stale assumptions.
