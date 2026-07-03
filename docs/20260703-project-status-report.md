# Project Status Report — 2026-07-03

Branch `multi-region-return`. Local gate green at HEAD (`92ae242`):
`cargo fmt --check` clean, `cargo clippy --all-targets -- -D warnings` clean,
**583 tests passing**. Recent work landed the Layer-1 border-node routing
rewrite (interior cost paid once), the parallel-exit preservation fix, the
road-topology repricing gate, and route-report refresh on region-owner changes.

Codebase size: ~39,000 lines of Rust. Largest files: `src/ui/tui.rs` (5,374),
`src/core/regions/directory.rs` (2,129), `src/core/regions/mod.rs` (1,979),
`src/core/regions/runtime/mod.rs` (1,927), `src/core/regions/worker.rs` (1,669).

---

## 1. Redundant code

Ranked by confidence that deletion is safe.

### 1.1 Stale `#[allow(dead_code)]` on code that is now live
- `src/core/systems/road_network_analysis.rs:277` — `road_predecessors` still
  says *"P1 is a standalone patch; P2 wires this into the route cache."* P2
  landed: it is called in production at `src/core/world.rs:354`. Remove the
  attribute and comment.
- `src/core/world.rs:325` — `routes_to` still says *"P2 standalone; P3 wires
  this into the movement system."* P3 landed: `src/core/systems/travel.rs`
  calls it at lines 347, 486, 599, 657, 674. Remove the attribute.

These are not just cosmetic — a stale `allow(dead_code)` will silently hide a
regression if a future refactor genuinely orphans the function.

### 1.2 Genuinely dead function
- `src/ui/tui.rs:3114` — `inspect_title` has no callers anywhere (only its own
  definition matches a repo-wide grep). Delete it.

### 1.3 Launch-mode plumbing collapsed to one path but the scaffolding remains
- `CityLaunchMode` (`src/ui/city_driver.rs:23`) is a **one-variant enum**
  (`RegionalMultiRegion`).
- Both UIs expose `run()` and `run_regional()` that are byte-identical
  (`src/ui/tui.rs:1136/1141`, `src/ui/ascii.rs:86/91`).
- `src/main.rs` advertises four frontends (`tui`, `ascii`, `regional`,
  `regional-ascii`) that map to only two real behaviors.

Suggested cut: delete `run_regional`, the `regional`/`regional-ascii` arms in
`main.rs`, and pass nothing (or keep the enum only if a second backend is
genuinely imminent). ~40 lines gone, one less concept.

### 1.4 Stale documentation
- `CLAUDE.md` (architecture section) still lists
  `src/core/regions/load_manager.rs` / `WorkerLoad`, which no longer exist —
  a repo-wide grep finds zero hits. Remove the bullet or mark it "planned".

### 1.5 Wrapper structs that thinned out after the cost removal
- After commit `40a0370` removed `RouteHop.cost`, `RouteHop` is a single-field
  struct (`exits: Vec<ExitLink>`) and `RouteField` a single-field wrapper over
  a `HashMap`. `RegionRoutes.to[T].from[R]` is now morally
  `HashMap<RegionId, HashMap<RegionId, Vec<ExitLink>>>`. Keeping the named
  types is defensible for documentation value; if they stay, they should stay
  deliberately, not by inertia. Low priority.

### 1.6 Deliberate (fine) — for completeness, not deletion
- `schedule.rs` Leisure phase, `components.rs:156` unread `Citizen.id`,
  `economy.rs:430` proxy distance — all carry `ponytail:` markers naming the
  upgrade path. These are tracked shortcuts, not rot.

---

## 2. Code that could be improved

### 2.1 `src/ui/tui.rs` is a 5,374-line single file
It holds the runtime state machine, input handling, every panel renderer, the
tile theme, and ~2,200 lines of tests. Split into a `src/ui/tui/` module
(`state.rs`, `render/*.rs`, `theme.rs`, `input.rs`) the next time a TUI
feature lands — do it as its own no-behavior-change patch, not bundled with a
feature.

### 2.2 Coarse invalidation flags
`world.rs` now has two command-side `Cell<bool>` flags (`derived_dirty`,
`road_topology_dirty`) with a `TODO` acknowledging they should split by
subsystem if config mutation grows. Fine today; the risk to watch is a new
mutation chokepoint forgetting to set the right flag. A single
`InvalidationFlags` bitset with one `mark(reason)` chokepoint would make the
next flag cheap and un-forgettable.

### 2.3 Startup road-report republish is O(N²)
`worker.rs:294` (`publish_current_road_reports` called from every
`add_region`) republishes **all** regions' reports per add. Already
ponytail-marked; narrow to the new region + its neighbours when region counts
grow past a few dozen.

### 2.4 Cross-region allocation lifecycle TODOs
`runtime/mod.rs:848/925/1084` — reconciliation is tick-driven rather than
demand-driven, and reservations clear on next-tick-start rather than on
explicit release. This is the biggest known correctness-adjacent debt in the
regions layer; it should be the subject of its own plan doc before more
export resources (goods) pile onto the same lifecycle.

### 2.5 Visibility/reporting gaps
- `regions/mod.rs:1106` — exported power demand counted as supplied in the
  exporter's stats (TODO CR4): balance-report distortion.
- `adapter.rs:492` — commute note is local-only; cross-region commuters show
  no commute info in inspect.

### 2.6 Minor polish
- `CityDriverError::Regional` formats with `{error:?}` (debug) in a
  user-facing `Display` — give `RegionalGameError` a real `Display`.
- The new worker tests build `RegionCommand`s with fully-qualified paths
  inline; a 5-line test helper (`build_cmd(x, y, kind)`) would cut ~60 lines
  of noise in `worker.rs` tests.
- `World` → `RegionWorld` rename (TODO at `world.rs:40`) is worth doing while
  the type is `pub(crate)` and the rename is mechanical.

---

## 3. Future development suggestions

In rough priority order:

1. **Finish the routing arc.** The in-flight plan
   (`docs/20260627-inspect-road-travelers.md`, uncommitted edits pending) plus
   the CR allocation lifecycle TODOs (§2.4) close out the current mission
   cleanly before anything new starts.
2. **Goods transfer completion.** `docs/cross-region-goods-transfer-plan.md`
   exists and the `ExportResource` machinery is generic; goods is the natural
   third resource after power/jobs, and will stress-test the allocation
   lifecycle — do §2.4 first.
3. **Save format versioning.** Saves are serde JSON with no version stamp.
   One `save_version: u32` field plus a load-time check is cheap now and very
   expensive to retrofit after players have saves.
4. **Determinism/replay harness.** Determinism is the repo's core invariant
   but is only spot-checked. A test that runs the same seed twice across a
   2-worker split and diffs full snapshots per tick would turn the invariant
   into a guard. (The parity test covers single-vs-regional; this covers
   thread-timing independence.)
5. **Perf counters for the repricing gate.** The gate just landed on the
   claim that road edits are rare relative to ticks. A debug-build counter
   (reports published / ticks) surfaced in the TUI debug panel would verify
   the claim as maps grow.
6. **Multi-worker load balancing** — deliberately deferred (load_manager was
   removed). Reintroduce only when a profile shows one worker saturated;
   until then it is speculative.

---

## 4. New UI recommendation — browser

The architecture is already browser-ready in the way that matters: **UI code
renders exclusively from plain-data view models** (`GameView`, `CellView`,
`InspectView`) behind `CityDriver`/`RegionalGame`, and never touches the ECS.
A browser frontend is "just" a third renderer.

Two viable paths:

### Option A (recommended first): local web server + TS/Canvas client
```text
 browser (TS + Canvas)                    existing native process
 ┌──────────────────────┐   WebSocket    ┌───────────────────────────┐
 │ grid renderer        │◄── JSON ───────│ axum ── CityDriver        │
 │ overlays / inspect   │── commands ───►│         └─ RegionalGame   │
 │ rAF traveler anim    │                │             └─ worker thread │
 └──────────────────────┘                └───────────────────────────┘
```
- Add `#[derive(Serialize)]` to the view/input types in `src/interface/`
  (they are plain data; this is a one-line change per type and does not leak
  ECS internals — the adapter boundary is unchanged).
- A thin `axum` server (one new binary target, feature-gated so the core
  keeps zero new mandatory deps) pushes a `GameView` snapshot per tick over a
  WebSocket and accepts the same commands `CityDriver` already exposes.
- Client: TypeScript + Canvas 2D. The grid is small (20×15 default), so
  naive full-redraw per frame is fine; overlays become alpha-blended color
  layers; `travelers` in the view already provide dot positions for
  `requestAnimationFrame` interpolation.
- Why first: no wasm toolchain, threads keep working, the native TUI/ASCII
  frontends are untouched, and the serde derives are the only core change.

### Option B (later): WASM in-browser sim
Compile `core` + `interface` to `wasm32-unknown-unknown`; no server, works as
a static page. Two real blockers to plan for:
- `regions/threaded.rs` uses `std::thread` — wasm needs the worker to run on
  the non-threaded path (`RegionWorker` already supports bounded scheduling
  passes; feature-gate `ThreadedRegionWorker` out).
- `crossterm`/`ratatui` must move behind a `tui` feature so the core builds
  without them.
Both are healthy refactors even if wasm never ships. Do B only after A proves
what the browser UI should look like.

### Terminal note (COLORTERM=truecolor)
Your terminal advertises truecolor. ratatui supports `Color::Rgb`, so the
existing TUI overlays (pollution, land value, desirability) could upgrade
from the 256-color palette to smooth 24-bit gradients, gated on
`COLORTERM=truecolor` with the current theme as fallback. Small,
self-contained patch; a nice precursor to the browser overlay renderer since
both need the same value→color ramp function.

---

## Suggested next patches (each independently green + reviewable)
1. Cleanup: §1.1 + §1.2 + §1.3 + §1.4 in one small "remove dead scaffolding"
   patch (~-80 lines, no behavior change). **Implemented — see below.**
2. `serde::Serialize` derives on `src/interface/` view models + a
   round-trip test (pre-work for the browser UI, useful for debugging today).
3. Save version stamp (§3.3).
4. `axum` server binary behind a `web` feature (Option A skeleton).

---

## Patch 1 implemented — "Cleanup: remove dead scaffolding" (2026-07-03)

Net diff: 8 files, +21/-76 lines. No behavior change; verified by the full
gate (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test -q` — 583 tests green before and after).

### What changed and why

- **`road_network_analysis.rs`, `world.rs`** — dropped two
  `#[allow(dead_code)]` attributes whose comments still said "P1/P2 is a
  standalone patch, P3 wires this in." Both wiring patches landed long ago
  (`road_predecessors` is called from `world.rs:354`; `routes_to` is called
  five times from `travel.rs`). The attributes were pure noise that could
  mask a real future regression.
- **`tui.rs`** — deleted `inspect_title`, a function with zero callers
  anywhere in the repo.
- **`city_driver.rs`, `tui.rs`, `ascii.rs`, `main.rs`** — collapsed
  `CityLaunchMode`, a one-variant enum (`RegionalMultiRegion` only), and
  everything that existed only to thread it through:
  `CityDriver::new(mode)` (redundant with `CityDriver::regional_multi_region()`),
  `TuiRuntime::with_mode(now, mode)` → `TuiRuntime::launch(now)`,
  `run_regional()` in both UI modules (byte-identical to `run()`), and the
  `"regional"`/`"regional-ascii"` CLI arms in `main.rs` (now `"tui"`/`"ascii"`
  only).
- **`tests/ui_contract_test.rs`, `tests/regional_multi_region_play_test.rs`**
  — fixed call sites, and rewrote `default_launch_uses_regional_mode_
  without_legacy_escape_hatch` to assert the new reality (no
  `CityLaunchMode`, no `run_regional`, direct `CityDriver::
  regional_multi_region()` calls) instead of source strings that no longer
  exist. The test still guards the same property — no legacy escape hatch —
  against the current code shape.
- **`CLAUDE.md`** — removed the architecture bullet documenting
  `src/core/regions/load_manager.rs` / `WorkerLoad`, which do not exist in
  the repo (confirmed via repo-wide grep, zero hits).
- **`README.md`** — codex's review of this patch caught that the README
  still advertised `cargo run -- regional` / `cargo run -- regional-ascii`
  as "compatibility aliases," which would now fail with "Unknown frontend"
  since `main.rs` no longer has those arms. Removed both lines.

### Diagram — before/after launch surface

```text
BEFORE                                   AFTER
main.rs                                  main.rs
 ├─ "tui"          → tui::run()           ├─ "tui" | None → tui::run()
 ├─ "ascii"        → ascii::run()         └─ "ascii"      → ascii::run()
 ├─ "regional"     → tui::run_regional()        │
 └─ "regional-ascii"→ ascii::run_regional()     │
        │                  │                    ▼
        ▼                  ▼            CityDriver::regional_multi_region()
run_with_mode(CityLaunchMode)   (both UIs, direct call — one path, one name)
        │
        ▼
CityDriver::new(CityLaunchMode::RegionalMultiRegion)
        │
        ▼
CityDriver::regional_multi_region()   ← the only real behavior, always
```

The enum, the `new(mode)` indirection, and the two aliased entry points all
existed to select between launch modes that had already collapsed to one.
Nothing downstream of `CityDriver::regional_multi_region()` changed.

### Review packet

1. **Files changed & why**: see bullets above — each change removes scaffolding
   left over from a prior refactor phase (P1/P2/P3 patch markers, a launch-mode
   enum that lost its second variant) rather than touching live behavior.
2. **Behavior changed**: none for players. The CLI surface shrinks from four
   documented frontend names to two (`tui`, `ascii`); the two removed names
   (`regional`, `regional-ascii`) were undocumented-as-primary aliases that
   pointed at the exact same code path as the primary names.
3. **Tests added**: none new; this is a deletion-only patch per its mission.
4. **Tests modified & why**: `ui_contract_test.rs::
   default_launch_uses_regional_mode_without_legacy_escape_hatch` rewritten to
   assert against the post-cleanup source shape (see above); three
   `CityDriver::new(CityLaunchMode::...)` call sites across two test files
   updated to `CityDriver::regional_multi_region()`.
5. **Risks remaining**: none identified. Codex (round 1) flagged the stale
   README lines, which are now fixed; a second codex pass and the self-review
   checklist found nothing further. Opencode review was skipped at the user's
   instruction for this patch.
6. **Assumptions made**: that `regional`/`regional-ascii` were never load-bearing
   for any external script or CI job — confirmed via repo-wide grep (only
   README documented them, now fixed) and there is no `.github/` workflow
   invoking them.
7. **Commands run**: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test -q` (583 tests, all passing, run twice — before and after the
   README fix).
8. **Patch diagram**: see above.
9. **Problem diagram** — what the scaffolding looked like before this patch
   existed to solve (a single-mode CLI still shaped like a multi-mode one):

```text
CityLaunchMode { RegionalMultiRegion }   ← one variant, exhaustive match
        │
        ▼
CityDriver::new(mode) { match mode { RegionalMultiRegion => regional_multi_region() } }
        │
        ▼
   always the same call — the match and the enum add nothing
```
