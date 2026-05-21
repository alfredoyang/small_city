# Small City

Small City is a minimal SimCity-like simulation game written in Rust. The goal is to keep the simulation deterministic, testable, and easy to extend while using a small custom ECS instead of a full game engine.

## Architecture

The project is split into three layers:

- `core`: ECS data, resources, systems, grid, and the public `Game` API.
- `interface`: UI-safe input types, events, view models, and adapters from ECS state to renderable data.
- `ui`: terminal frontends, including the ratatui TUI and cursor-based ASCII fallback UI.

The main public API is `Game`, which owns the private ECS `World` and exposes operations such as `build`, `preview_build`, `replace`, `upgrade`, `bulldoze`, `tick`, `inspect`, `view`, `view_with_overlay`, `save_to_file`, and `load_from_file`.

## ECS Core

The ECS is intentionally small:

- `Entity`: stable ID for things placed in the city.
- `Components`: plain data such as `Position`, `Building` with level, `Population`, `Citizen`, `PowerProvider`, `PowerConsumer`, `PollutionSource`, and `HappinessEffect`.
- `World`: private storage for entities, components, grid, resources, and city stats.
- `Systems`: deterministic functions that operate on `World`, including build, replace, upgrade, bulldoze, power, road connectivity, citizens, local effects, stats, population, economy, pollution, and happiness.
- `Grid`: stores entity IDs for occupied map cells.
- `Resources`: global city state such as money, turn, population, jobs, pollution, unemployment, happiness, power stats, and derived local effects.

Citizens are ECS entities, but they do not occupy grid cells. Buildings remain the only grid occupants. A citizen has stable personal state: age, home residential building, optional workplace, happiness, and money. Residential population is kept as a cache derived from citizens so existing views can stay simple while future behavior can become more individual.

## UI Boundary

UI code must not access ECS internals. It must use the public `Game` API and render only from interface view models such as `GameView`, `CellView`, and `InspectView`.

The adapter in `src/interface/adapter.rs` is the boundary where private ECS data becomes UI-safe view data. Map overlays, demand, road-connected status, local effects, build previews, and inspect details are generated before the ASCII UI renders them.

## Terminal UI

Run the richer panel-based TUI with:

```sh
cargo run
```

You can choose a frontend explicitly:

```sh
cargo run -- tui
cargo run -- ascii
```

The TUI uses `ratatui` and `crossterm` for panels, styling, alternate-screen rendering, keyboard input, and raw terminal mode. The older ASCII UI remains available as a fallback/debug frontend.

Both UIs keep cursor state locally. The cursor is not stored in the ECS core.

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
R                       replace selected cell with selected build type
U                       upgrade selected cell
X                       bulldoze selected cell
I                       inspect selected cell
N                       next turn when paused
Space                   pause/resume automatic ticks in TUI
V                       cycle overlay in ASCII UI
O                       cycle overlay in TUI
H                       open help screen in TUI
S                       prompt for save filename
L                       prompt for load filename
Q                       quit
```

Note: lowercase `s` moves down; uppercase `S` saves.

ASCII fallback map symbols:

```text
. empty
= road
R residential
C commercial
I industrial
T power plant
P park
```

Ratatui TUI map tiles use an ASCII-2 theme by default. Every map cell is a fixed two-character tile:

```text
.. empty
== road
R1 residential level 1
R2 residential level 2
C1 commercial level 1
I1 industrial level 1
T1 power plant level 1
P1 park level 1
R- unpowered residential
R! blocked residential
```

Power overlay symbols:

```text
T* active power plant
=* powered road network
R+ powered residential
R- unpowered residential
C+ powered commercial
C- unpowered commercial
I+ powered industrial
I- unpowered industrial
.. no power overlay data
```

Local overlays:

```text
pollution       .. clean | -- low | ++ medium | ** high | ## severe
land value      .. none  | -- low | ++ medium | ** high | ## very high
desirability    !! bad   | -- low | ++ medium | ** good | ## excellent
population      R0-R9 for residential population where available
```

The TUI presents the same view data in panels: city map, selected cell, city status, build preview/actions, and messages/tick summary. Status panels show turn, money, population, citizen count, jobs, happiness, pollution, power capacity/supply/shortage, zone demand, current build tool and cost, current overlay, overlay legend, demand notes, selected cell details, inspect notes, build preview explanations, run/pause state, and the latest command message. TUI messages use `OK:`, `WARN:`, `ERR:`, or `INFO:` prefixes. New and loaded TUI games start paused; pressing Space resumes automatic one-second ticks.

The TUI needs at least a 100x30 terminal. Smaller terminals show a resize warning and suggest `cargo run -- ascii`.

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

Tests cover core simulation rules, citizen economy, road connectivity, demand, bulldoze, build previews, save/load behavior, inspect output, map overlays, cursor/action parsing, TUI key mapping, and UI boundary contracts.

Scenario-style integration tests cover longer multi-turn cities that combine power networks, demand-driven growth, citizen salary/rent/shopping economy, land-value effects, upgrades, replace, bulldoze, overlays, and save/load.

## v0.1 Completed Scope

- Fixed-size grid with one entity per occupied cell.
- Buildable road, residential, commercial, industrial, power plant, and park cells.
- Building costs and money tracking.
- Deterministic tick order and structured tick summary events.
- Inspect output with building-specific details.

## v0.2 Completed Scope

- Cursor-based ASCII UI using only `Game` and view models.
- Panel-based ratatui TUI using only `Game` and view models, with ASCII UI preserved as fallback.
- UI-local cursor, selected build tool, and current overlay state.
- Bulldoze support with component cleanup and derived-state refresh.
- Replace support for swapping an occupied cell to the selected build type.
- Upgrade support for residential, power plant, and park cells.
- Build preview explanations for selected cursor cell and build tool.
- Prompted save/load filenames with default `city1`.
- Road connectivity requirement for residential growth and effective jobs.
- Network-based power: roads form networks, power plants supply adjacent networks, and buildings draw power from adjacent powered roads.
- Limited power capacity and consumer demand: power plants provide 10 capacity, residential uses 1, commercial uses 2, and industrial uses 3.
- Deterministic power shortage handling by map position, y first then x.
- Power status totals for capacity, demand, supplied power, and shortage.
- Population growth only when residential buildings are powered, road-connected, jobs are available, and desirability is not low; high desirability grows faster, medium desirability grows normally, and low desirability blocks growth.
- Commercial and industrial effective job counts only when powered and road-connected.
- Citizen economy foundation: citizens are assigned to powered, road-connected commercial or industrial jobs, earn salary, pay rent when they can afford it, and spend money at powered, road-connected commercial buildings for a happiness gain.
- City income now comes from workplace tax, residential rent, commercial sales tax, manufacturing tax, and export tax. Commercial buildings pay sales tax only when citizens actually shop there, and disconnected shops receive no shoppers.
- Ongoing economy balance: commercial, industrial, power plant, and park buildings each cost 1 maintenance per turn; roads and residential buildings have no upkeep. Tick summaries include salaries paid, workplace tax, rent, sales tax, goods produced/stored/sold/imported/exported, manufacturing tax, export tax, shoppers served, rent failures, maintenance, and net money change.
- Industrial pollution and park happiness effects.
- Basic residential, commercial, and industrial demand levels.
- Basic map overlays for normal, power, pollution, population, land value, and desirability views.
- In-game overlay legends and short demand explanations in the ASCII UI.
- Inspect notes explain blockers and local effects such as missing roads, unpowered networks, power shortage, no available jobs, pollution, and happiness.
- Building levels start at 1 and currently max at 2.
- Upgrade effects at level 2: residential max population increases from 5 to 8, power plant capacity increases from 10 to 15, and park happiness effect increases from +3 to +5.

## v0.3 Completed Scope

- Deterministic local effects system for every map cell.
- Derived cell values for land value, pollution pressure, accessibility, and desirability.
- Parks improve nearby land value and desirability.
- Industrial buildings increase nearby pollution pressure and reduce nearby land value.
- Commercial buildings slightly improve nearby land value.
- Roads improve accessibility for adjacent cells.
- Residential growth now considers desirability.
- Residential growth now spawns citizen entities instead of only incrementing a counter.
- Residential population is derived from citizens assigned to that home.
- Citizens have individual happiness derived from home conditions and local effects.
- City happiness uses average citizen happiness once citizens exist.
- Happy or unhappy citizens can slightly affect nearby local effects.
- Inspect and cell views expose local effects through UI-safe view models.
- Land value and desirability map overlays.
- Integration tests cover local effects, citizen spawning, citizen cleanup, citizen economy behavior, growth behavior, overlays, and save/load refresh.
- Implemented the new TUI panel-based terminal with ratatui + crossterm.

## v0.4 Completed Scope

- Connected economy, happiness, land value, and building level into one deterministic loop.
- Residential rent now depends on local land value and building level.
- Citizens who cannot afford rent receive rent stress, which lowers future happiness.
- Commercial and industrial workplaces pay level-based tax, with industrial paying more.
- Commercial sales tax now increases with local land value and building level.
- Industrial buildings produce local goods when powered and road-connected.
- Productive commercial buildings store local goods up to capacity and sell local goods before importing.
- Surplus industrial goods are exported for export tax.
- Local goods give citizens cheaper shopping and better happiness than imported goods.
- Building maintenance increases with building level.
- Citizen happiness considers home desirability, pollution pressure, unemployment, and rent stress.
- Residential growth is blocked by low average happiness and gets a small bonus from high average happiness.
- Inspect output exposes UI-safe economy details such as rent per citizen, maintenance, sales tax per shopper, commercial goods storage, industrial goods production, power demand, level, local effects, and happiness blockers.
- Tick summaries include rent collected, rent failures, commercial sales tax, local goods flow, manufacturing tax, export tax, maintenance, and net money change.

## V0.4 Business Reinvestment Scope

Commercial and industrial buildings can level up automatically when they earn enough money. This uses building-level business profit, not city money, so successful districts grow from their own performance.

Implemented business state:

- `business_cash`
- `lifetime_profit`
- `days_profitable`
- `last_period_profit`

Profit comes from the existing economy loop:

- Commercial earns from shoppers and sales.
- Industrial earns from local goods sold, manufacturing value, and exports.
- Maintenance, import cost, export distance cost, and route distance penalties reduce profit.

Upgrade checks run weekly after daily economy has accumulated profit:

- Level 1 to 2 requires enough `business_cash`.
- Building must be powered.
- Building must be road-connected.
- Recent demand should not be low.
- Commercial should have customers or goods flow.
- Industrial should have workers, production, or export flow.

Implemented commercial upgrade effects:

- More jobs.
- More shopper capacity.
- More goods storage.
- Higher sales tax potential.
- Higher maintenance.

Implemented industrial upgrade effects:

- More jobs.
- More goods production.
- Better export potential.
- Higher manufacturing tax potential.
- Higher maintenance.
- Higher pollution pressure.

Inspect output exposes UI-safe business notes:

- Business cash progress toward upgrade.
- Recent profit.
- Upgrade ready.
- Blocked by no power.
- Blocked by missing road access.
- Blocked by low demand.
- Blocked by weak goods/customer flow.

Implementation status:

- Phase A: Done. Commercial and industrial business cash/profit are tracked and exposed through inspect notes.
- Phase B: Done. Profitable commercial and industrial buildings can auto-upgrade from level 1 to 2 at weekly boundaries.
- Phase C: Remaining. Add level 3, stronger effects, and tick events such as `Commercial upgraded from reinvestment`.
