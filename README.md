# Small City

Small City is a minimal SimCity-like simulation game written in Rust. The goal is to keep the simulation deterministic, testable, and easy to extend while using a small custom ECS instead of a full game engine.

## Architecture

The project is split into three layers:

- `core`: ECS data, resources, systems, grid, and the public `Game` API.
- `interface`: UI-safe input types, events, view models, and adapters from ECS state to renderable data.
- `ui`: cursor-based ASCII terminal UI.

The main public API is `Game`, which owns the private ECS `World` and exposes operations such as `build`, `preview_build`, `bulldoze`, `tick`, `inspect`, `view`, `view_with_overlay`, `save_to_file`, and `load_from_file`.

## ECS Core

The ECS is intentionally small:

- `Entity`: stable ID for things placed in the city.
- `Components`: plain data such as `Position`, `Building`, `Population`, `PowerProvider`, `PowerConsumer`, `PollutionSource`, and `HappinessEffect`.
- `World`: private storage for entities, components, grid, resources, and city stats.
- `Systems`: deterministic functions that operate on `World`, including build, bulldoze, power, road connectivity, stats, population, economy, pollution, and happiness.
- `Grid`: stores entity IDs for occupied map cells.
- `Resources`: global city state such as money, turn, population, jobs, pollution, unemployment, and happiness.

## UI Boundary

UI code must not access ECS internals. It must use the public `Game` API and render only from interface view models such as `GameView`, `CellView`, and `InspectView`.

The adapter in `src/interface/adapter.rs` is the boundary where private ECS data becomes UI-safe view data. Map overlays, demand, road-connected status, build previews, and inspect details are generated before the ASCII UI renders them.

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
X                       bulldoze selected cell
I                       inspect selected cell
N                       next turn
V                       cycle overlay
S                       prompt for save filename
L                       prompt for load filename
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
* powered road network
+ powered consumer
- unpowered consumer
. no power overlay data
```

Status panels show turn, money, population, jobs, happiness, pollution, power capacity/supply/shortage, zone demand, current build tool and cost, current overlay, overlay legend, demand notes, selected cell details, inspect notes, build preview explanations, and the latest command message.

## Save And Load

Save the current city from the ASCII UI:

```text
S
```

Load a saved city from the ASCII UI:

```text
L
```

Save/load prompts for a filename. Press Enter at the prompt to use the default `city1`.

Save files are JSON snapshots of the private game state. Loading refreshes derived state before the game continues and resets the UI cursor to the loaded map.

## Tests

Run the standard checks:

```sh
cargo fmt
cargo test
cargo clippy -- -D warnings
```

Tests cover core simulation rules, road connectivity, demand, bulldoze, build previews, save/load behavior, inspect output, map overlays, cursor/action parsing, and UI boundary contracts.

## v0.1 Completed Scope

- Fixed-size grid with one entity per occupied cell.
- Buildable road, residential, commercial, industrial, power plant, and park cells.
- Building costs and money tracking.
- Deterministic tick order and structured tick summary events.
- Inspect output with building-specific details.

## Current v0.2 Scope

- Cursor-based ASCII UI using only `Game` and view models.
- UI-local cursor, selected build tool, and current overlay state.
- Bulldoze support with component cleanup and derived-state refresh.
- Build preview explanations for selected cursor cell and build tool.
- Prompted save/load filenames with default `city1`.
- Road connectivity requirement for residential growth and effective jobs.
- Network-based power: roads form networks, power plants supply adjacent networks, and buildings draw power from adjacent powered roads.
- Limited power capacity and consumer demand: power plants provide 10 capacity, residential uses 1, commercial uses 2, and industrial uses 3.
- Deterministic power shortage handling by map position, y first then x.
- Power status totals for capacity, demand, supplied power, and shortage.
- Population growth only when residential buildings are powered, road-connected, and jobs are available.
- Commercial and industrial effective job counts and income only when powered and road-connected.
- Ongoing economy balance: commercial, industrial, power plant, and park buildings each cost 1 maintenance per turn; roads and residential buildings have no upkeep.
- Industrial pollution and park happiness effects.
- Basic residential, commercial, and industrial demand levels.
- Basic map overlays for normal, power, pollution, and population views.
- In-game overlay legends and short demand explanations in the ASCII UI.
- Inspect notes explain blockers and local effects such as missing roads, unpowered networks, power shortage, no available jobs, pollution, and happiness.

## Proposed v0.2 Roadmap

- Add stronger demand-driven growth behavior.
- Add replace/upgrade commands.
- Add more scenario-style integration tests for longer simulations.
