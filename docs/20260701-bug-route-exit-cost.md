# 20260701 Bug Route Exit Cost

## 1. Introduction / Problem

P-d fixed Layer-1 routing to use a border-node graph and to emit all valid exits
to the chosen next-hop region. That prevents a token on a disconnected local
network from freezing.

But the emitted exits have no per-exit remaining cost:

```rust
pub struct ExitLink {
    pub link: BorderLinkId,
    pub to_region: RegionId,
}

pub struct RouteExit {
    pub cell: Entity,
    pub link: BorderLinkId,
    pub to_region: RegionId,
}
```

The route summary used to keep only the best total cost for the source region,
not a cost attached to each exit. `travel::advance_to_exit(...)` then picks the first
reachable `RouteExit`, not the cheapest reachable one.

Bug example:

```text
Goal: A -> C

path 1:
  A exit 1 -> B entry 1 -> B inside cost 8  -> B exit 1 -> C

path 2:
  A exit 2 -> B entry 2 -> B inside cost 16 -> B exit 2 -> C

Both exits go to next-hop region B.
Both exits are valid.
Path 1 is cheaper.
```

Current code can choose path 2 if its local `RouteExit` appears first and is
reachable from the token's local road network.

## 2. Proposal

Carry the remaining Layer-1 cost on each emitted exit, then choose the cheapest
reachable candidate in Layer 2.

```text
Layer 1:

  ExitLink {
    link: A exit 1,
    to_region: B,
    cost: 10,
  }

  ExitLink {
    link: A exit 2,
    to_region: B,
    cost: 18,
  }

Layer 2:

  reachable local candidates:
    local_cost(home -> exit 1) + exit_1.cost
    local_cost(home -> exit 2) + exit_2.cost

  choose the minimum.
```

This keeps the P-d no-freeze behavior: all strict-decrease exits to the chosen
next-hop region are still emitted. The mover just ranks reachable exits by cost
instead of list order.

No new protocol, worker event, or UI boundary is needed.

## 3. Important Functions And Structures

`src/core/regions/mod.rs`
- Extend `ExitLink` with `cost: u32`.
- Extend `RouteExit` with `cost: u32`.
- `RegionRoutes::exits_from(...)` remains the same shape:
  `HashMap<RegionId, Vec<ExitLink>>`.
- `RegionState::set_region_routes(...)` copies `ExitLink.cost` into
  `RouteExit.cost`.

`src/core/regions/directory.rs`
- `build_region_routes(...)` already computes `dist[(N, matching_link)]`.
- When emitting a crossing `(R, link) -> (N, matching)`, set:

```rust
ExitLink {
    link,
    to_region: N,
    cost: 1 + dist[(N, matching)],
}
```

- Do not keep a separate `RouteHop.cost`; each `ExitLink.cost` is the value the
  mover needs.

`src/core/systems/travel.rs`
- `advance_to_exit(...)` currently picks first reachable candidate.
- Change candidate selection to pick the reachable `RouteExit` with minimum:

```text
local cost from current/origin to exit.cell + exit.cost
```

For an already committed destination, keep the committed exit if it is still in
the candidate list and reachable. That avoids mid-route oscillation when a
slightly cheaper exit appears after the token already committed.

`src/core/world.rs`
- Reuse `World::road_distance_to(dest, target, network)`, which calls
  `road_predecessors_with_dist(...)` and does not touch the route cache.
- This gives weighted local cost, not raw hop count, so local exit ranking uses
  the same turn/intersection costs as movement.

## 4. Pseudocode / Integration

Build L1 exits:

```rust
for (target, exit) in valid_crossings {
    let n_dist = dist[target];
    if n_dist < r_dist {
        valid_crossings.push(ExitLink {
            link: exit.link,
            to_region: exit.to_region,
            cost: 1 + n_dist,
        });
    }
}

best_next_total = min cost grouped by next-hop region;
chosen_regions = regions whose min cost == best_next_total;

all_exits = valid_crossings
    .filter(|exit| chosen_regions.contains(exit.to_region));

RouteHop {
    exits: all_exits,
}
```

Install routes:

```rust
for (target, exit_links) in exits_from_region {
    for exit in exit_links {
        for cell in cells_by_link[exit.link] {
            remote_exit_cells[target].push(RouteExit {
                cell,
                link: exit.link,
                to_region: exit.to_region,
                cost: exit.cost,
            });
        }
    }
}
```

Choose a remote exit:

```rust
fn route_cost_to_exit(world, networks, state, origin, exit) -> Option<u32> {
    let local_cost = if let Some(cell) = state.current_cell {
        if cell == exit.cell {
            0
        } else {
            distance_from_cell_to_exit_cell(world, networks, cell, exit.cell)?
        }
    } else {
        distance_from_building_to_exit_cell(world, networks, origin, exit.cell)?
    };

    Some(local_cost + exit.cost)
}

let exit = if committed_exit_still_valid_and_reachable {
    committed_exit
} else {
    candidates
        .iter()
        .filter_map(|exit| Some((route_cost_to_exit(..., exit)?, exit)))
        .min_by_key(|(cost, exit)| (*cost, exit.cell, exit.link, exit.to_region))
        .map(|(_, exit)| exit)
};
```

Tie-breaking stays deterministic by sorting/ordering on cost, cell, link, and
region.

## 5. Tests

Add focused routing tests:

- `region_routes_exit_links_carry_per_exit_cost`
  - A has two exits to B.
  - B entry 1 reaches C with cost 8.
  - B entry 2 reaches C with cost 16.
  - Assert both exits are emitted and their `ExitLink.cost` values differ.

- `remote_exit_choice_prefers_cheapest_reachable_exit`
  - Build one region with two reachable border exits for the same final target.
  - Give the later/sorted-second exit lower total cost.
  - Assert `advance_to_exit` commits to the cheaper exit, not the first exit.

- `remote_exit_choice_uses_weighted_local_distance`
  - Build one reachable one-hop 4-way exit and one two-hop straight exit.
  - Give both exits equal L1 `cost`.
  - Assert the selector uses weighted local road cost and chooses the straight
    exit, not the raw-hop closer 4-way.

- `remote_exit_keeps_committed_exit_when_still_valid`
  - Start a token already travelling to an exit.
  - Add another cheaper candidate in `remote_exit_cells`.
  - Assert the token continues to its committed exit.

Existing tests to update:

- `region_routes_emit_all_exits_to_chosen_next_hop_regardless_of_border_cost`
  should assert per-exit costs.
- `set_region_routes_populates_remote_exit_cells` should assert `RouteExit.cost`.

No UI test is needed; this does not change view data or the UI boundary.

## 6. Risks / Non-goals

- This does not add congestion, ETA UI, or per-citizen route planning.
- This does not change handoff routing; `to_region` and `entry_link` behavior
  stay the same.
- This keeps the lazy P-d guarantee: emit all valid exits to the chosen next-hop
  region, then let Layer 2 choose the cheapest reachable one locally.

## 7. Patch Explanation

This patch keeps the existing L1/L2 split:

```text
L1 directory route:
  "from region A, which border links move toward final region C?"

L2 local travel:
  "from this token's current road network, which of those exits is cheapest
   and reachable?"
```

The fix is small: L1 now attaches the remaining cost to every emitted exit, and
L2 ranks reachable exits by `local weighted road cost + remaining L1 cost`.

### Important Structures

`src/core/regions/mod.rs`

```rust
pub struct ExitLink {
    pub link: BorderLinkId,
    pub to_region: RegionId,
    pub cost: u32,
}

pub struct RouteExit {
    pub cell: Entity,
    pub link: BorderLinkId,
    pub to_region: RegionId,
    pub cost: u32,
}
```

`ExitLink` is the directory's L1 answer. It says:

```text
link      = which local border to leave through
to_region = immediate next-hop region
cost      = remaining L1 cost after using this exit
```

`RouteExit` is the movement-ready version installed into `World`:

```text
cell      = the local road cell the token can walk to
link      = same border link, later carried in PendingHandoff::Move
to_region = same next-hop region, used by the worker route
cost      = same per-exit remaining L1 cost
```

Diagram:

```text
RegionDirectory
  ExitLink { link: East/0, to_region: B, cost: 10 }
       |
       | RegionState::set_region_routes resolves East/0 to road cell r8
       v
World.remote_exit_cells[C]
  RouteExit { cell: r8, link: East/0, to_region: B, cost: 10 }
```

### Important Functions

`src/core/regions/directory.rs`

- `build_region_routes(...)`
  - Runs the border-node Dijkstra from P-d.
  - For every valid crossing `(R, link) -> (N, matching)`, computes:

```text
ExitLink.cost = 1 + dist[(N, matching)]
```

That means each exit carries its own remaining cost. Two exits to the same
next-hop region can now differ:

```text
Goal: A -> C

Path 1:
  A East/0 --cross 1--> B West/0 --inside 8--> B East/0 --cross 1--> C
  ExitLink.cost from A East/0 = 1 + (8 + 1) = 10

Path 2:
  A East/1 --cross 1--> B West/1 --inside 16--> B East/1 --cross 1--> C
  ExitLink.cost from A East/1 = 1 + (16 + 1) = 18
```

`RouteHop` stays as just the list of exits. The mover chooses from the
per-exit `ExitLink.cost` values, so there is no summary cost to keep in sync.

`src/core/regions/mod.rs`

- `RegionState::set_region_routes(...)`
  - Converts each `ExitLink` into one or more `RouteExit`s by matching
    `ExitLink.link` to local border road cells.
  - Copies `exit.cost` into `RouteExit.cost`.

```text
L1:
  C -> [ExitLink { link: East/0, to_region: B, cost: 10 }]

local border roads:
  East/0 -> [r8]

movement map:
  remote_exit_cells[C] = [
    RouteExit { cell: r8, link: East/0, to_region: B, cost: 10 }
  ]
```

`src/core/world.rs`

- `World::road_distance_to(dest, target, network)`
  - Computes weighted local road distance with `road_predecessors_with_dist`.
  - It is pure for this use: it does not write `World::routes_to` or the route
    cache.
  - The cost includes the same turn/intersection weights as movement:
    straight `1`, turn/T-junction `2`, four-way `4`.

`src/core/systems/travel.rs`

- `depart_toward(...)`
  - Used when a local citizen departs toward a remote workplace.
  - For every reachable `RouteExit`, computes:

```text
total = World::road_distance_to(exit.cell, entry_cell, network) + exit.cost
```

- `advance_to_exit(...)`
  - Used while a token is walking toward a remote target.
  - If the token already has a committed `state.destination` and that exit is
    still reachable, keep it. This avoids oscillation when route costs change
    while the token is already walking.
  - Otherwise, choose the reachable `RouteExit` with the smallest:

```text
total = World::road_distance_to(exit.cell, current_cell, network) + exit.cost
```

### End-To-End Flow

```text
Directory builds L1 routes
--------------------------

dist[(B, West/0)] = 9
dist[(B, West/1)] = 17

A East/0 -> B West/0:
  ExitLink { link: East/0, to_region: B, cost: 1 + 9 = 10 }

A East/1 -> B West/1:
  ExitLink { link: East/1, to_region: B, cost: 1 + 17 = 18 }

Both exits are emitted because both go to the chosen next-hop region B.
```

```text
Region A installs local exits
-----------------------------

East/0 -> road cell r8
East/1 -> road cell r2

remote_exit_cells[C] = [
  RouteExit { cell: r8, link: East/0, to_region: B, cost: 10 },
  RouteExit { cell: r2, link: East/1, to_region: B, cost: 18 },
]
```

```text
Token chooses an exit
---------------------

from current road cell:

  local cost to r8 = 4
  total via r8 = 4 + 10 = 14

  local cost to r2 = 1
  total via r2 = 1 + 18 = 19

choose r8, even though r2 is locally closer.
```

### Why This Fixes The Bug

Before P-e:

```text
RouteExit had no per-exit cost.
travel picked the first reachable exit.
```

So the local mover could choose the wrong exit when two exits reached the same
next-hop region with different remaining costs.

After P-e:

```text
RouteExit has per-exit remaining L1 cost.
travel picks min(local weighted road cost + remaining L1 cost).
```

That is enough. No new protocol, worker event, UI state, or extra route cache is
needed.
