# Implementation Plan: Minimal ECS City Simulation in Rust

## Goal

Implement a minimal SimCity-like simulation core in Rust.

The project must use a simple ECS-style architecture from the beginning, but stay small and easy to test.

The first UI is ASCII-based, but the UI must not access ECS internals directly.

## Architecture Rules

1. Core engine must be written in Rust.
2. Use a minimal custom ECS:
   - Entity = ID
   - Components = plain data
   - Systems = functions operating on World
   - Resources = global city state
3. The grid stores Entity IDs, not full tile data.
4. UI must only access the game through the public Game API.
5. UI must render from view models, not from ECS components.
6. Do not expose World directly to the UI.
7. Keep all rules deterministic.
8. Add unit tests for systems and UI contract.

## Version 1 Scope

Map:
- Fixed grid size, default 10x10.
- One entity per occupied grid cell.

Buildings:
- Road
- Residential
- Commercial
- Industrial
- PowerPlant
- Park

Components:
- Position
- Building
- Population
- PowerProvider
- PowerConsumer
- PollutionSource
- HappinessEffect

Resources:
- CityResources
- CityStats

Systems:
- BuildSystem
- PowerSystem
- StatsSystem
- PopulationSystem
- EconomySystem
- PollutionSystem
- HappinessSystem

Public Game API:
- Game::new(width, height)
- Game::view()
- Game::build(x, y, kind)
- Game::tick()
- Game::inspect(x, y)

UI-facing models:
- GameView
- MapView
- CellView
- CityStatusView
- BuildOptionView
- InspectView
- CommandResult
- GameEventView

## Simulation Rules

Starting money:
- 100

Building costs:
- Road: 1
- Residential: 5
- Commercial: 8
- Industrial: 10
- PowerPlant: 20
- Park: 6

Jobs:
- Commercial: 2
- Industrial: 3

Income per tick:
- Each citizen: +1
- Powered commercial: +2
- Powered industrial: +3

Population:
- Residential has max population 5.
- Residential gains +1 population per tick if powered and city has available jobs.
- Jobs may be counted globally in Version 1.

Power:
- PowerPlant provides power to all PowerConsumers within Manhattan distance <= 3.
- No power lines in Version 1.

Pollution:
- Industrial produces 2 pollution.
- Park reduces pollution by 1.
- Pollution cannot go below 0.

Happiness:
- happiness = 50 + park_count * 3 - pollution - unemployment * 2
- Clamp happiness between 0 and 100.

Tick order:
1. PowerSystem
2. StatsSystem
3. PopulationSystem
4. EconomySystem
5. PollutionSystem
6. HappinessSystem
7. Increment turn

## ASCII UI

Commands:
- build road x y
- build residential x y
- build commercial x y
- build industrial x y
- build power x y
- build park x y
- next
- inspect x y
- status
- quit

Symbols:
- Empty: .
- Road: =
- Residential: R
- Commercial: C
- Industrial: I
- PowerPlant: T
- Park: P

The ASCII UI must render only from GameView.
It must not read World, components, or systems directly.

## Required Project Structure

src/
- main.rs
- core/
  - mod.rs
  - entity.rs
  - components.rs
  - world.rs
  - grid.rs
  - resources.rs
  - game.rs
  - systems/
    - mod.rs
    - build.rs
    - power.rs
    - stats.rs
    - population.rs
    - economy.rs
    - pollution.rs
    - happiness.rs
- interface/
  - mod.rs
  - input.rs
  - view.rs
  - events.rs
  - adapter.rs
- ui/
  - mod.rs
  - ascii.rs

tests/
- build_test.rs
- power_test.rs
- population_test.rs
- economy_test.rs
- game_api_test.rs
- ui_contract_test.rs

## Testing Requirements

Add tests for:

1. Building cost is deducted correctly.
2. Cannot build outside the map.
3. Cannot build on occupied cell.
4. Cannot build without enough money.
5. PowerPlant powers nearby Residential.
6. Residential population grows when powered and jobs are available.
7. Industrial creates pollution.
8. Park reduces pollution effect.
9. Happiness is clamped between 0 and 100.
10. GameView contains width * height cells.
11. Empty cells are buildable.
12. Occupied cells are not buildable.
13. Residential CellView includes population data.
14. UI contract does not expose World.

## Deliverable

Implement the first playable version.

After implementation:
- Run cargo fmt
- Run cargo test
- Run cargo run and verify ASCII UI starts
