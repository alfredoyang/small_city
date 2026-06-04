# Remove Old Single-Thread Architecture Plan

This completed plan retires the old UI-facing single-city `Game` execution path after the
regional runtime is ready to become the default. The goal is not to delete the
deterministic simulation systems. Those systems remain the core rules. The goal
is to remove the duplicate frontend/backend path where UI can still drive
`Game` directly instead of the regional facade.

Status: complete. The production `Game` facade has been removed, terminal UIs
run through `CityDriver` backed by `RegionalGame`, and legacy single-city saves
load through the regional loader.

## Goals

- Make the regional facade the default UI execution path.
- Preserve deterministic single-region behavior through the regional runtime.
- Preserve compatibility with existing single-city save files.
- Keep UI code using one backend path instead of switching between `Game` and
  `RegionalGame`.
- Keep ECS `World` private to core simulation and region state.
- Remove old single-city UI/backend code only after parity and migration tests
  are in place.

## Non-Goals

- Do not remove core simulation systems such as economy, power, population,
  citizens, local effects, or road analysis.
- Do not expose `World` to UI, runner, worker, or coordinator code.
- Do not add multi-worker load balancing as part of this removal.
- Do not change regional imported resources from visibility-only cache into full
  economy inputs in this cleanup.

## Resolved Blockers

- Default launch uses the regional path.
- `CityDriver` owns one regional backend path.
- Regional ticks return real `CommandResult` data from the region runtime.
- Save/load uses `RegionalGame::load_from_file`, including legacy single-city
  conversion.
- Shared simulation helpers live in `src/core/simulation.rs`.
- Production `src/core/game.rs` has been removed.

## Patch 18: Regional Tick Result Parity

Goal: make regional ticks return the same player-visible `CommandResult` shape
as `Game::tick`.

Likely files:

- `src/core/regions/runtime/mod.rs`
- `src/core/regional_types.rs`
- `src/core/regional_game_runner.rs`
- `src/core/regional_game.rs`
- `tests/regional_game_parity_test.rs`

Implementation:

- Add a request ID-correlated tick reply path from `RegionRuntime` to the
  runner, such as `RegionTickCompleted`, mirroring `RegionCommandCompleted`.
- Return the real `RegionState::tick_local` result instead of fabricating a
  minimal `TurnAdvanced` event.
- Keep this as plumbing, not a reimplementation: `RegionState::tick_local`
  already calls the same `tick_world` helper used by `Game::tick`.
- Keep export-change emission after tick deterministic.
- Preserve command/tick ordering through the same worker pumping rules.

Tests:

- single-region regional tick result matches `Game::tick` for turn and summary
  event shape
- economy, population, power, and pollution tick summaries remain visible
- regional tick still propagates export changes after local tick work

Review focus:

- Tick behavior remains deterministic.
- UI still sees only `CommandResult` and view models.
- No ECS internals leak through tick replies.

## Patch 19: Single-City Save Compatibility Through Regional Loader

Goal: let the regional path load existing single-city saves so removing the old
UI backend does not strand player saves.

Likely files:

- `src/core/regional_game.rs`
- `src/core/game.rs`
- `src/ui/city_driver.rs`
- `tests/regional_save_load_test.rs`
- `tests/save_load_test.rs`

Implementation:

- Detect whether a save file is regional or legacy single-city by trying the
  regional shape first (`selected_region` plus `regions`), then falling back to
  the legacy bare-`World` save shape. Do not require a new version field for
  existing saves.
- Convert a legacy single-city save into a one-region `RegionalGame`.
- Preserve existing single-city save tests until the compatibility path is
  covered through `RegionalGame`.
- Keep imported resources as rebuildable cache, not saved truth.
- Region-ordering concerns from regional save/load are a known non-issue for
  converted legacy saves because they contain exactly one region. The two-region
  default remains explicitly ordered as `[RegionId(1), RegionId(2)]`.

Tests:

- regional loader accepts an existing single-city save
- converted save exposes the same selected region view as the old `Game` load
- converted game can continue ticking, building, saving, and loading again
- invalid save errors stay deterministic and user-readable

Review focus:

- Existing save files remain loadable.
- Regional save format stays authoritative per region.
- No imported-resource cache is persisted as permanent truth.

## Patch 20: Make Regional UI The Default

Goal: switch normal launch to the regional backend while keeping an emergency
legacy command only for one patch if needed.

Likely files:

- `src/main.rs`
- `src/ui/city_driver.rs`
- `src/ui/ascii.rs`
- `src/ui/tui.rs`
- `tests/ui_contract_test.rs`

Implementation:

- Change default TUI launch to create a regional game.
- Keep CLI arguments explicit and documented in error text.
- If a temporary legacy mode remains, name it clearly, such as `single` or
  `legacy-single`, and mark it for removal in the next patch.
- Ensure ASCII and ratatui paths both use the same regional driver mode by
  default.

Tests:

- default TUI launch uses regional mode
- regional launch does not import worker/runtime/ECS internals in UI modules
- save/load from the default UI path uses regional compatibility loading
- region label and switching still work after default launch changes

Review focus:

- Default behavior changes only in this explicit patch.
- UI boundary remains `CityDriver` and view models.
- Existing frontends do not talk to worker or runtime types.

## Patch 21: Collapse CityDriver To One Backend

Goal: remove the duplicate UI backend branch and make `CityDriver` regional-only.

Likely files:

- `src/ui/city_driver.rs`
- `src/ui/ascii.rs`
- `src/ui/tui.rs`
- `tests/ui_contract_test.rs`
- `tests/regional_multi_region_play_test.rs`

Implementation:

- Remove `CityBackend::SingleCity`.
- Remove single-city-specific driver constructors.
- Keep one driver command/view/save/load path backed by `RegionalGame`.
- Update UI tests to assert the regional facade is the only backend.

Tests:

- driver commands route through `RegionalGame`
- driver save/load accepts legacy and regional saves
- unavailable backend behavior still protects UI after unrecoverable save errors
- non-test UI code does not import `Game`, ECS, worker, or runtime types
- either migrate UI test modules off `Game` parity helpers, or make the boundary
  test explicitly ignore `#[cfg(test)]` imports until Patch 22/23 retires or
  re-scopes `Game`

Review focus:

- UI no longer has two execution paths.
- Error handling remains user-readable.
- The driver still renders from `GameView`/`InspectView` only.

## Patch 22: Move Shared Simulation Helpers Out Of `game.rs`

Goal: remove the regional runtime's dependency on helper functions that live in
the UI-facing single-city `Game` module.

Likely files:

- `src/core/game.rs`
- `src/core/regions/mod.rs`
- new neutral module such as `src/core/simulation.rs` or `src/core/tick.rs`
- `src/core/mod.rs`
- core tests that import tick or derived-state helpers indirectly

Implementation:

- Move `tick_world` out of `game.rs` into a neutral core module.
- Move `refresh_derived_state_for_world` out of `game.rs` into the same neutral
  core module, or another clearly named simulation helper module.
- Update both `Game` and `RegionState` to import these helpers from the neutral
  module.
- Check for any other helper in `game.rs` that regional code imports or would
  need before `Game` can be retired.
- Keep behavior unchanged. This patch is a relocation only.

Tests:

- existing `Game` tests still pass
- regional tick, command, save/load, and parity tests still pass
- no regional module imports `crate::core::game` after the relocation

Review focus:

- No simulation order changes.
- The helper module does not expose ECS `World` to UI.
- `game.rs` becomes a facade around shared simulation helpers, not the owner of
  logic required by regional runtime.

## Patch 23: Retire Or Re-scope `Game`

Goal: decide whether `Game` should be removed from public UI usage entirely or
kept as a small compatibility/test facade over core simulation.

Likely files:

- `src/core/game.rs`
- `src/lib.rs`
- core integration tests that still instantiate `Game`
- regional parity tests

Implementation options:

- Preferred compatibility option: keep `Game` as a core single-world test and
  save compatibility facade, but remove all UI usage.
- Full removal option: migrate tests to `RegionalGame` or lower-level system
  helpers, then delete the `Game` facade. This option is blocked until Patch 22
  moves shared simulation helpers out of `game.rs`.

Tests:

- if `Game` remains, tests prove it is not imported by non-test UI code
- if `Game` is removed, all behavior tests have equivalent regional or system
  coverage
- save/load compatibility remains covered after any public API change

Review focus:

- Do not remove useful deterministic core test coverage accidentally.
- Do not force UI or regional code to access ECS internals.
- Keep public API changes intentional and documented.
- If `Game` remains, make its role explicit: compatibility/test facade over
  shared simulation helpers, not the architecture used by UI.

## Patch 24: Documentation And Cleanup

Goal: remove stale documentation and tests that describe regional mode as
experimental or opt-in after it becomes the only UI path.

Likely files:

- `README.md`
- `docs/regional-multithread-implementation-plan.md`
- `docs/remove-old-single-thread-architecture-plan.md`
- UI contract tests

Implementation:

- Update launch instructions.
- Update architecture docs to say regional runtime is the default UI execution
  path.
- Remove obsolete references to keeping the old single-city UI path as default.
- Keep design notes that still explain why the region runtime owns isolation and
  deterministic event flow.

Tests:

- documentation-only changes do not require Rust tests unless examples or CLI
  behavior change in the same patch

Review focus:

- Docs match actual launch behavior.
- Removed docs are truly stale, not still useful design rationale.

Status: complete. README launch and architecture sections now describe the
regional facade as the default path, and this plan records the completed removal
state.

## Final Removal Checklist

- [x] `cargo run` launches the regional path by default.
- [x] ASCII and ratatui frontends share one regional driver path.
- [x] UI modules do not import `Game`, ECS, worker, or runtime internals.
- [x] Regional tick returns real `CommandResult` data.
- [x] Existing single-city saves load through the regional path.
- [x] Multi-region saves still round trip.
- [x] Production `Game` has been removed; remaining single-region behavioral
  coverage uses the regional facade or test-only wrappers over it.
- [x] `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass.
