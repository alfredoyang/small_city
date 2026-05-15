# Small City

Small City is a minimal SimCity-like simulation game written in Rust. The goal is to keep the simulation deterministic, testable, and easy to extend while using a small custom ECS instead of a full game engine.

## Architecture

The project is split into three layers:

- `core`: ECS data, resources, systems, grid, and the public `Game` API.
- `interface`: UI-safe input types, events, view models, and adapters from ECS state to renderable data.
- `ui`: cursor-based ASCII terminal UI.

The main public API is `Game`, which owns the private ECS `World` and exposes operations such as `build`, `tick`, `inspect`, `view`, `view_with_overlay`, `save_to_file`, and `load_from_file`.

## ECS Core

The ECS is intentionally small:

- `Entity`: stable ID for things placed in the city.
- `Components`: plain data such as `Position`, `Building`, `Population`, `PowerProvider`, `PowerConsumer`, `PollutionSource`, and `HappinessEffect`.
- `World`: private storage for entities, components, grid, resources, and city stats.
- `Systems`: deterministic functions that operate on `World`, including build, power, road connectivity, stats, population, economy, pollution, and happiness.
- `Grid`: stores entity IDs for occupied map cells.
- `Resources`: global city state such as money, turn, population, jobs, pollution, unemployment, and happiness.

## UI Boundary

UI code must not access ECS internals. It must use the public `Game` API and render only from interface view models such as `GameView`, `CellView`, and `InspectView`.

The adapter in `src/interface/adapter.rs` is the boundary where private ECS data becomes UI-safe view data. Map overlays, demand, road-connected status, and inspect details are generated there, not in the ASCII UI.

## ASCII UI

Run the terminal UI with:

```sh
cargo run
```

The UI keeps cursor state locally. The cursor is not stored in the ECS core.

Controls:

```text
W/A/S/D or Arrow Keys  move cursor
1                       select Road
2                       select Residential
3                       select Commercial
4                       select Industrial
5                       select Power Plant
6                       select Park
B or Enter              build selected type at cursor
I                       inspect selected cell
N                       next turn
V                       cycle overlay
S                       save to city1
L                       load from city1
Q                       quit
```

Note: lowercase `s` moves down; uppercase `S` saves.

Normal map symbols:

```text
. empty
= road
R residential
C commercial
I industrial
T power plant
P park
```

Power overlay symbols:

```text
P power plant
* inside power plant radius
+ powered consumer
- unpowered consumer
. no power overlay data
```

Status panels show turn, money, population, jobs, happiness, pollution, zone demand, current build tool and cost, current overlay, selected cell details, and the latest command message.

## Save And Load

Save the current city from the ASCII UI:

```text
S
```

Load the default save from the ASCII UI:

```text
L
```

The cursor UI currently saves to and loads from `city1`. Save files are JSON snapshots of the private game state. Loading refreshes derived state before the game continues.

## Tests

Run the standard checks:

```sh
cargo fmt
cargo test
cargo clippy -- -D warnings
```

Tests cover core simulation rules, road connectivity, demand, save/load behavior, inspect output, map overlays, cursor/action parsing, and UI boundary contracts.

## v0.1 Completed Scope

- Fixed-size grid with one entity per occupied cell.
- Buildable road, residential, commercial, industrial, power plant, and park cells.
- Building costs and money tracking.
- Power plant radius using Manhattan distance.
- Road connectivity requirement for residential growth and effective jobs.
- Population growth when residential buildings are powered, road-connected, and jobs are available.
- Commercial and industrial effective job counts and income when powered and road-connected.
- Industrial pollution and park happiness effects.
- Basic residential, commercial, and industrial demand levels.
- Deterministic tick order and structured tick summary events.
- Cursor-based ASCII UI using only `Game` and view models.
- Save/load support.
- Inspect output with building-specific details.
- Basic map overlays for normal, power, pollution, and population views.

## Proposed v0.2 Roadmap

- Add richer overlay legends and demand explanations.
- Add richer inspect details for nearby power coverage, road blockers, and local effects.
- Add stronger demand-driven growth behavior.
- Add bulldoze or replace commands.
- Add configurable save/load filenames from the cursor UI.
- Add per-building maintenance costs and stronger economy balancing.
- Add more scenario-style integration tests for longer simulations.
