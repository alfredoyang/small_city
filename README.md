# Small City

Small City is a minimal SimCity-like simulation game written in Rust. The goal is to keep the simulation deterministic, testable, and easy to extend while using a small custom ECS instead of a full game engine.

## Architecture

The project is split into three layers:

- `core`: ECS data, resources, systems, grid, and the public `Game` API.
- `interface`: UI-safe input types, events, view models, and adapters from ECS state to renderable data.
- `ui`: ASCII terminal UI.

The main public API is `Game`, which owns the private ECS `World` and exposes operations such as `build`, `tick`, `inspect`, `view`, `view_with_overlay`, `save_to_file`, and `load_from_file`.

## ECS Core

The ECS is intentionally small:

- `Entity`: stable ID for things placed in the city.
- `Components`: plain data such as `Position`, `Building`, `Population`, `PowerProvider`, `PowerConsumer`, `PollutionSource`, and `HappinessEffect`.
- `World`: private storage for entities, components, grid, resources, and city stats.
- `Systems`: deterministic functions that operate on `World`, including build, power, stats, population, economy, pollution, and happiness.
- `Grid`: stores entity IDs for occupied map cells.
- `Resources`: global city state such as money, turn, population, jobs, pollution, unemployment, and happiness.

## UI Boundary

UI code must not access ECS internals. It must use the public `Game` API and render only from interface view models such as `GameView`, `CellView`, and `InspectView`.

The adapter in `src/interface/adapter.rs` is the boundary where private ECS data becomes UI-safe view data. Map overlays and inspect details are generated there, not in the ASCII UI.

## ASCII UI

Run the terminal UI with:

```sh
cargo run
```

Commands:

```text
build road x y
build residential x y
build commercial x y
build industrial x y
build power x y
build park x y
next
inspect x y
status
view normal
view power
view pollution
view population
save filename
load filename
quit
```

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

## Save And Load

Save the current city:

```text
save city1
```

Load a saved city:

```text
load city1
```

Save files are JSON snapshots of the private game state. Loading refreshes derived state before the game continues.

## Tests

Run the standard checks:

```sh
cargo fmt
cargo test
cargo clippy -- -D warnings
```

Tests cover core simulation rules, save/load behavior, inspect output, map overlays, and UI boundary contracts.

## v0.1 Completed Scope

- Fixed-size grid with one entity per occupied cell.
- Buildable road, residential, commercial, industrial, power plant, and park cells.
- Building costs and money tracking.
- Power plant radius using Manhattan distance.
- Population growth when residential buildings are powered and jobs are available.
- Commercial and industrial job counts and income.
- Industrial pollution and park happiness effects.
- Deterministic tick order and structured tick summary events.
- ASCII UI using only `Game` and view models.
- Save/load support.
- Inspect output with building-specific details.
- Basic map overlays for normal, power, pollution, and population views.

## Proposed v0.2 Roadmap

- Improve overlay legends and make current overlay mode visible in the UI.
- Add richer inspect details for nearby power coverage and local effects.
- Add roads as a requirement for growth or service reach.
- Add zoning or demand pressure for residential, commercial, and industrial growth.
- Add bulldoze or replace commands.
- Add per-building maintenance costs and stronger economy balancing.
- Add more scenario-style integration tests for longer simulations.
