# 20260630 L1 Route Repricing Gate

## 1. Introduction / Problem

`RegionWorker::process_region_events_with_mode` currently recomputes a region's
post-event border links, `border_neighbor_map`, `RegionRoadReport`, and Layer-1
route snapshot after every processed region event batch.

The directory publish is idempotent, so unchanged reports do not always rebuild
Layer-1:

```text
process event
  -> road_report(...)
  -> publish_region_road_report(...)
      -> returns early if unchanged
```

But the expensive part before publish still runs for pure runtime events:

```text
StepTravel / ReceiveTraveler / export grants
  -> ensure_derived_state()
  -> network_border_links()
  -> road_report()
  -> publish no-op
```

That is wasted work. Layer-1 road prices only need recompute when local road
topology may have changed.

## 2. Proposal

Keep the current idempotent directory publish as the safety net, but gate the
post-event road-report work with a `World` dirty flag.

Add `road_topology_dirty` beside `derived_dirty`:

```text
World
  derived_dirty        = applied derived state is stale
  road_topology_dirty  = local road graph / border links may be stale

TODO: both flags are coarse. If config mutation grows, split/optimize them by
affected subsystem.
```

Mark `road_topology_dirty` at the same chokepoints that already invalidate the
route cache:

- placing a road
- removing a road
- replacing a road or replacing something with a road
- road-affecting upgrade only if one is introduced later

Current flow:

```text
Worker
  for each runtime with pending events
    process_some_events(...)
    ensure_derived_state()
    always recompute border links + road_report
    publish report (maybe no-op)
```

Proposed flow:

```text
Worker
  for each runtime with pending events
    process_some_events(...)
    ensure_derived_state()
    if runtime.state().is_road_topology_dirty():
      recompute border links + road_report
      publish report
      refresh region_routes exits
      runtime.state().clear_road_topology_dirty()
    else:
      keep current multi-hop exit map
```

This avoids worker-side guessing. The region's ECS mutation code already knows
whether the road graph changed.

## 3. Important Functions And Structures

`src/core/regions/worker.rs`
- `RegionWorker::process_region_events_with_mode(...)` — extend. It should decide
  whether to recompute road reports by reading the region state's
  `road_topology_dirty` flag after processing events.
- `RegionWorker::publish_region_summary(...)` — unchanged.

`src/core/regions/runtime/mod.rs`
- `RegionRuntime::process_some_events(...)` — unchanged.

`src/core/world.rs`
- `World::road_topology_dirty: Cell<bool>` — new sibling of `derived_dirty`.
- `World::{mark,is,clear}_road_topology_dirty()` — new tiny accessors.
- Add a TODO near `derived_dirty` / `road_topology_dirty`: both flags are coarse
  and should be split by subsystem if config mutation grows.

`src/core/regions/mod.rs`
- `RegionState::{is,clear}_road_topology_dirty()` — new thin wrappers for the
  worker. No UI exposure.

`src/core/systems/placement.rs`
- `place_building(...)` — already clears route cache when `kind == Road`; also
  call `world.mark_road_topology_dirty()` there.

`src/core/systems/entity_cleanup.rs`
- `remove_entity(...)` — already clears route cache when the removed kind is
  `Road`; also call `world.mark_road_topology_dirty()` there.

`src/core/regions/directory.rs`
- `RegionDirectory::publish_region_road_report(...)` — unchanged idempotent guard.
  It remains the last line of defense against unnecessary Layer-1 rebuilds.

## 4. Pseudocode / Integration

Add the flag in `World`:

```rust
pub(crate) struct World {
    derived_dirty: Cell<bool>,
    road_topology_dirty: Cell<bool>,
}

// TODO: derived_dirty and road_topology_dirty are coarse command-side
// invalidation flags. Split by affected subsystem if config mutation grows.
```

Mark it at road mutation chokepoints:

```rust
// placement.rs
if kind == BuildingKind::Road {
    world.clear_route_cache();
    world.mark_road_topology_dirty();
}

// entity_cleanup.rs
match removed_kind {
    Some(BuildingKind::Road) => {
        world.clear_route_cache();
        world.mark_road_topology_dirty();
    }
    Some(_) => world.evict_route_cache(entity),
    None => {}
}
```

Then in `process_region_events_with_mode`:

```rust
outbound.extend(runtime.process_some_events(max_events_per_region));
runtime.ensure_derived_state();

if runtime.state().is_road_topology_dirty() {
    let post_links = runtime.state().network_border_links();
    let post_border_neighbor_map = border_neighbor_map_for_region(..., &post_links, ...);
    runtime.set_border_neighbor_map(post_border_neighbor_map.clone());
    let road_report = runtime.state().road_report(&post_border_neighbor_map);
    self.directory.publish_region_road_report(road_report);
    if let Some(exits) = self.directory.exits_from(source_region) {
        runtime.set_region_routes(&exits);
    }
    runtime.state().clear_road_topology_dirty();
}
```

Do not clear the flag before a successful publish. If publish stays idempotent
and returns `false`, clearing is still fine because the report matched the
snapshot.

## 5. Tests

Add focused worker/directory tests:

- `step_travel_does_not_republish_road_report`
  - enqueue `RegionEvent::StepTravel`
  - process one barrier pass
  - assert `directory.rebuild_count()` does not increase from road-report publish

- `build_road_republishes_road_report`
  - enqueue `RegionCommand::Build { kind: Road, .. }`
  - process one pass
  - assert road report publish/rebuild happens

- `build_house_does_not_republish_road_report`
  - enqueue `RegionCommand::Build { kind: Residential, .. }`
  - assert no road-report rebuild

- `preview_build_does_not_republish_road_report`
  - enqueue `RegionCommand::PreviewBuild`
  - assert no road-report rebuild

- `bulldoze_road_republishes_road_report`
  - create and then bulldoze a road
  - assert road report publish/rebuild happens

No UI tests: no UI/view contract changes.

## 6. Risks / Non-goals

- The flag only tracks road topology. If a future non-road building changes
  border route pricing, that mutation must mark `road_topology_dirty` too.
- This does not change Layer-1 route semantics, only when reports are recomputed.
- Do not expose `World` or ECS state to the UI.
- Keep `publish_region_road_report` idempotent; it protects against false
  positives and stale assumptions.
