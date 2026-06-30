# 20260630 L1/L2 Region Routing

## 1. Introduction / Problem

Cross-region travel has two pathfinding layers:

- **Layer 1 (L1):** region-to-region routing. It answers, "from region A, which
  border should I leave through to eventually reach region C?"
- **Layer 2 (L2):** local road-cell routing inside one region. It answers, "from
  this road cell, which next road cell gets me to that border or building?"

The split matters because a citizen token moves one local road cell at a time,
but the route target can be several regions away.

```text
final target = C

Layer 1:
  A --------> B --------> C
  "leave A via East/0 toward B"

Layer 2 inside A:
  home road -> road -> road -> East/0 border cell
```

The code keeps those responsibilities separate:

```text
RegionState::road_report        prices local border-to-border roads  (L2 data)
RegionDirectory::build_routes   assembles region graph + Dijkstra     (L1 map)
RegionState::set_region_routes  converts L1 border links to road cells
travel::advance_to_exit         walks the token to the chosen road cell
```

## 2. Proposal / Mental Model

This document is the readable map of the current implementation.

### Big Picture

```text
                 DATA / ROUTE PREPARATION
                 ------------------------

  Region A                  Region B                  Region C
  --------                  --------                  --------
  road_report()             road_report()             road_report()
      |                         |                         |
      | RegionRoadReport        | RegionRoadReport        | RegionRoadReport
      |                         |                         |
      +-------------------------+-------------------------+
                                |
                                v
                    RegionDirectory snapshot
                    ------------------------
                    build_region_routes(...)
                    - assemble weighted region graph
                    - run Dijkstra per destination
                    - produce RegionRoutes
                                |
                                v
                    RegionWorker, for each region
                    -----------------------------
                    exits_from(A)
                      C -> [ExitLink { link: East/0, to_region: B }]
                                |
                                v
                    RegionState::set_region_routes(...)
                    -----------------------------------
                    East/0 -> local road cell r42
                                |
                                v
                    World.remote_exit_cells
                    -----------------------
                    C -> [
                      RouteExit {
                        cell: r42,
                        link: East/0,
                        to_region: B,
                      }
                    ]
```

The route-preparation side converts published local road reports into the exact
local exits a token can walk toward.

```text
                    RUNTIME MOVEMENT
                    ----------------

Citizen token in A wants final workplace in C

  travel::advance_to_exit(...)
          |
          | reads world.remote_exit_cells[C]
          v
  chooses RouteExit { cell: r42, link: East/0, to_region: B }
          |
          | Layer 2 local walking
          v
  home road -> road -> road -> r42
          |
          | reached border cell
          v
  PendingHandoff::Move
    to_region = B
    exit_cell = r42
    exit_link = East/0
          |
          | worker routes message
          v
  Region B receives token at matching border cell
          |
          | repeat same mechanism for final target C
          v
  Region C
```

Same data, different jobs:

```text
RegionRoadReport
  "what roads/border prices does this region publish?"

RegionRoutes / ExitLink
  "for final target C, which border link leaves A on the best next hop?"

RouteExit
  "which local road cell should the token walk to, and where does it cross?"
```

### L2: Local Pricing Inside One Region

For one region, L2 is just normal road routing over local road cells.

```text
Region B

             North/0 -> D
                  |
        +---------+---------+
        |                   |
West/0  +--- local roads ---+  East/0
 -> A   |                   |   -> C
        +-------------------+

RegionBorderLink:
  West/0  -> A
  East/0  -> C
  North/0 -> D

RegionCrossCost:
  West/0  -> East/0  = 8
  West/0  -> North/0 = 3
  North/0 -> East/0  = 6
```

`RegionState::road_report(...)` produces that report. It uses
`World::road_distance_to(...)`, which calls `road_predecessors_with_dist(...)`;
it does **not** use `World::routes_to(...)` or the route cache.

### L1: Region Dijkstra

The directory turns local reports into a weighted region graph.

```text
Published border matches:

  A East/0  <->  B West/0
  B East/0  <->  C West/0
  A South/0 <->  D North/0
  D East/0  <->  C South/0

Region graph:

       B
      / \
     /   \
    A     C
     \   /
      \ /
       D
```

Each region edge gets a cost from the two reports:

```text
w(A,B) = A cost to reach East/0 + B cost from West/0
w(B,C) = B cost to reach East/0 + C cost from West/0
w(A,D) = A cost to reach South/0 + D cost from North/0
w(D,C) = D cost to reach East/0 + C cost from South/0
```

Example weights:

```text
         B
      4 / \ 8
       /   \
      A     C
       \   /
     2  \ / 5
         D
```

For destination `C`, Dijkstra computes:

```text
cost_to_C[C] = 0
cost_to_C[B] = 8
cost_to_C[D] = 5
cost_to_C[A] = min(A->B->C, A->D->C)
             = min(4 + 8, 2 + 5)
             = 7
```

So A chooses D, not B:

```text
Route field for final target C:

  from A -> ExitLink { link: South/0, to_region: D }, cost 7
  from D -> ExitLink { link: East/0,  to_region: C }, cost 5
  from B -> ExitLink { link: East/0,  to_region: C }, cost 8
  from C -> no exit, cost 0
```

This is also the loop-safety rule: every hop must strictly decrease the Dijkstra
distance.

```text
A(7) -> D(5) -> C(0)     valid: 7 > 5 > 0
D(5) -> A(7)             rejected: 7 is not lower than 5
```

### L1 To L2 Adapter: `set_region_routes`

L1 returns border links. Local movement needs road cells.

```text
L1 answer for source A:

  final target C -> ExitLink { link: South/0, to_region: D }

Local border roads in A:

  South/0 -> [road cell r42]

Stored movement answer:

  remote_exit_cells[C] = [
    RouteExit { cell: r42, link: South/0, to_region: D }
  ]
```

`RouteExit` deliberately carries all three pieces:

```text
cell      = local L2 walking target
link      = sender-side border link for handoff placement
to_region = immediate next-hop region for worker routing
```

That prevents the old duplicate routing path. The border crossing uses the same
L1 choice the directory made; it does not re-derive a neighbour from a separate
direct-neighbour map.

### Concrete Sample: A Can Reach C Via B Or D

```text
Goal: citizen in A works in C

Weights:

  A -> B = 4
  B -> C = 8

  A -> D = 2
  D -> C = 5

Totals:

  A -> B -> C = 12
  A -> D -> C = 7

Chosen:

  A exits to D
```

Flow:

```text
Directory route:
  to[C].from[A].exits = [ExitLink { link: South/0, to_region: D }]

Region A install:
  South/0 resolves to road cell r42
  remote_exit_cells[C] = [RouteExit { cell: r42, link: South/0, to_region: D }]

Travel step:
  token walks home -> ... -> r42
  at r42:
    PendingHandoff::Move {
      to_region: D,
      exit_cell: r42,
      exit_link: South/0,
      token,
    }

Region layer:
  TravelerHandoff { to_region: D, entry_link: South/0, ... }

Worker:
  routes handoff to region D's owner
```

Then D repeats the same mechanism:

```text
Region D has:
  remote_exit_cells[C] = [RouteExit { cell: d_exit, link: East/0, to_region: C }]

Token in D walks to d_exit and crosses to C.
```

## 3. Important Functions And Structures

`src/core/regions/mod.rs`
- `RegionBorderLink` — one local `BorderLinkId` plus the neighbour it reaches.
- `RegionCrossCost` — L2 cost from one local border link to another.
- `RegionRoadReport` — one region's published L2 border pricing.
- `RegionRoutes` / `RouteField` / `RouteHop` / `ExitLink` — L1 output.
- `RouteExit` — local movement-ready exit: `{ cell, link, to_region }`.
- `RegionState::road_report(...)` — builds the per-region L2 report.
- `RegionState::set_region_routes(...)` — converts L1 `ExitLink`s into local
  `RouteExit`s.

`src/core/regions/directory.rs`
- `RegionDirectory::publish_region_road_report(...)` — stores one region report
  and rebuilds the snapshot when it changes.
- `build_region_routes(...)` — assembles the weighted region graph and runs one
  destination-rooted Dijkstra per target region.
- `RegionDirectory::exits_from(region)` — returns this region's
  `HashMap<target_region, Vec<ExitLink>>`.

`src/core/regions/worker.rs`
- `border_neighbor_map_for_region(...)` — builds direct border-neighbour facts
  for `RegionState::road_report(...)` pricing only. It is not the travel route.
- `RegionWorker::process_region_events_with_mode(...)` — publishes reports and
  installs `directory.exits_from(source_region)` into the runtime.

`src/core/regions/runtime/mod.rs`
- `RegionRuntime::set_region_routes(...)` — passes L1 exits to `RegionState`.

`src/core/systems/travel.rs`
- `depart_toward(...)` — for remote targets, picks a reachable
  `remote_exit_cells[target.region]` candidate.
- `advance_to_exit(...)` — walks toward the selected `RouteExit.cell`; on the
  border cell, emits a `PendingHandoff::Move` carrying `exit.link` and
  `exit.to_region`.

`src/core/components.rs`
- `PendingHandoff::Move { exit_cell, exit_link, to_region, ... }` — buffered
  crossing decision.
- `TravelerHandoff { entry_link, to_region, ... }` — worker-routed crossing
  message.

## 4. Pseudocode / Integration

Report publishing:

```rust
// RegionWorker, after event processing / derived-state refresh
let border_neighbours = border_neighbor_map_for_region(topology, region, links, is_owned);
let report = runtime.state().road_report(&border_neighbours);
directory.publish_region_road_report(report);
```

L1 route assembly:

```rust
// RegionDirectory snapshot rebuild
for report in reports {
    collect RegionBorderLink and RegionCrossCost;
}

for destination in owned_regions {
    cost_to_destination = dijkstra(destination, region_graph);

    for source in owned_regions {
        choose neighbour where:
            cost_to_destination[neighbour] < cost_to_destination[source]
            and total edge cost is minimal;

        routes.to[destination].from[source] = RouteHop { exits, cost };
    }
}
```

Install routes into one region:

```rust
let exits_from_a = directory.exits_from(A).unwrap_or_default();
runtime.set_region_routes(&exits_from_a);

// RegionState::set_region_routes
cells_by_link = border_road_links(); // BorderLinkId -> local road cells

for (target, exit_links) in exits_from_a {
    for exit in exit_links {
        for cell in cells_by_link[exit.link] {
            remote_exit_cells[target].push(RouteExit {
                cell,
                link: exit.link,
                to_region: exit.to_region,
            });
        }
    }
}
```

Move a token:

```rust
let candidates = world.remote_exit_cells[target.region];
let exit = first candidate reachable by local L2 roads;

walk toward exit.cell;

if current_cell == exit.cell {
    emit PendingHandoff::Move {
        to_region: exit.to_region,
        exit_cell: exit.cell,
        exit_link: exit.link,
        token,
    };
}
```

Cross the border:

```rust
// Sender region
drain_traveler_handoffs:
    assert exit_link still resolves to exit_cell;
    send TravelerHandoff {
        to_region,
        entry_link: Some(exit_link),
        token,
    };

// Receiver region
receive_traveler_handoff:
    local_link = entry_link.matching_neighbor_link();
    entry_cell = cell_at_border_link(local_link);
    receive_traveler(token, entry_cell);
```

## 5. Tests

Existing tests that anchor this routing model:

- `region_routes_map_multihop_destination_to_first_hop`
- `region_routes_pick_only_cost_decreasing_neighbour`
- `region_routes_prefer_lower_cost_corridor`
- `region_routes_skip_roadless_border`
- `set_region_routes_populates_remote_exit_cells`
- `set_region_routes_empty_routes_clear_remote_exit_cells`
- `drain_move_resolves_border_link`
- `cross_region_commuter_goes_to_work_and_returns_home`

Useful future test if the sample above is not already covered end-to-end:

- `multi_hop_commute_prefers_cheaper_a_d_c_over_a_b_c`
  - Build A/B/C/D reports where A-B-C is valid but costlier than A-D-C.
  - Assert `RegionRoutes::exits_from(A)[C]` points to D.
  - Then step a token and assert the first handoff routes to D.

## 6. Risks / Non-goals

- L1 edge weights are region-level approximations. The current model is good for
  choosing a next hop; exact border-to-border global cost would require a
  border-node graph keyed by `(region, border_link)`.
- The direct border-neighbour facts still exist for report pricing. They are not
  a travel-routing source.
- Cross-region route snapshots are stale-tolerant. If a road disappears after a
  token selected an exit, the handoff path rolls back instead of stranding.
- This doc does not propose new routing behavior; it explains the current L1/L2
  interaction.
