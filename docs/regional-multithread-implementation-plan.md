# Regional Multithread Implementation Plan

This document turns the regional threading design notes into a small,
human-reviewable implementation sequence. Earlier patch descriptions preserve
the historical migration context; the current UI path now runs through the
regional facade by default.

Related design notes:

- `docs/regional-imported-resource-threading-model.md`
- `docs/regional-event-loop-continuation-design.md`
- `docs/regional-opaque-continuation-reply-handle.md`
- `docs/regional-shared-worker-thread-architecture.md`

## Guiding Rules

- Keep `RegionalGame` and UI-safe view models as the public boundary.
- Do not expose `World` to UI, worker, or coordinator code.
- Keep each region authoritative over its own local simulation state.
- Keep local simulation deterministic: stable event order, fixed system order,
  and integer formulas.
- Treat imported cross-region resource data as rebuildable cache.
- Do not introduce true multithreading until the single-threaded model is
  tested.
- Keep every patch under the project limit of roughly five files and 400 changed
  lines where possible.

## Target Architecture

The final shape should be:

```text
RegionalGame facade
  owns/delegates to the regional runner
  exposes UI-safe commands, snapshots, inspect results, and save/load

RegionRuntime
  owns one RegionState
  owns one event inbox
  processes local and neighbor events

RegionWorker
  owns one or more RegionRuntime values
  gives each region bounded work per pass
  routes outbound messages without inspecting ECS internals

RegionHandle
  stable sender endpoint for a region mailbox
  can be cloned and kept by neighboring regions

LoadManager
  optional later layer
  moves whole RegionRuntime values between workers at safe points
```

The first implementation should stay single-threaded. The worker abstraction is
introduced before OS threads so ownership, routing, and deterministic event
processing can be tested without timing noise.

## Patch 1: Region Resource Model

Goal: add data-only imported resource types and propagation rules without a
runtime or worker.

Likely files:

- `src/core/region.rs` or `src/core/regions/mod.rs`
- `src/core/mod.rs`
- `tests/regional_resource_test.rs`

Implementation:

- Add `RegionId`.
- Add `ResourceKind`.
- Add `ResourceId` with `origin_region`, `resource_kind`, and `generation`.
- Add `ImportedResource` with remaining capacity, hop count, max hops, travel
  cost, and source neighbor.
- Add a small cache type that accepts or rejects imported offers.
- Use the first-version duplicate rule: same `ResourceId` is rejected.
- Keep newer generation handling explicit. Recommended first behavior: a newer
  generation for the same origin and kind replaces older cached generations.
- Add deterministic forwarding helpers that:
  - subtract local used capacity
  - increment hop count
  - increment travel cost
  - stop when capacity is zero or max hops is reached
  - skip forwarding back to the sender

Tests:

- accepts a new imported offer
- rejects the same `ResourceId`
- replaces older generation for the same origin and kind
- forwards only remaining capacity
- does not forward back to the sender
- stops forwarding at `max_hops`

Review focus:

- Data types are owned and do not contain ECS references.
- Duplicate and generation behavior is easy to explain.
- Formulas are deterministic integer rules.

## Patch 2: Region State Wrapper

Goal: introduce a region-owned state wrapper while preserving the then-existing
single-city facade during migration.

Likely files:

- `src/core/region.rs` or `src/core/regions/state.rs`
- `src/core/game.rs`
- `tests/regional_state_test.rs`

Implementation:

- Add `RegionState` that owns a private `World` plus imported resource cache.
- Add methods needed by the future runtime:
  - `tick_local`
  - `process_imported_offer`
  - `apply_neighbor_import_result`
  - `view`
  - `inspect`
- Move the current deterministic tick sequence behind a reusable internal helper
  so the facade tick path and `RegionState::tick_local` use the same order.
- Keep `World` private to core internals.
- Do not change UI code.

Tests:

- `RegionState::tick_local` advances the same deterministic state as the facade
  tick path for a simple city
- imported offer processing does not expose or mutate another region's state
- facade view and inspect operations still use UI-safe view models

Review focus:

- No UI access to `World`.
- Tick order remains explicit.
- Any helper extraction is minimal and local to core.

## Patch 3: Single-Threaded Region Runtime

Goal: add the actor-style event loop without worker threads.

Likely files:

- `src/core/regions/runtime.rs`
- `src/core/regions/mod.rs`
- `tests/region_runtime_test.rs`

Implementation:

- Add `RegionEvent`:
  - `Tick`
  - `ProcessImportedOffer`
  - `RunImportedOfferContinuation`
- Add `RegionRuntime` with a `RegionState` and FIFO inbox.
- Add `process_next_event` and `process_some_events(max_events)`.
- Add `OutboundMessage` for messages that must be routed by the caller.
- Keep the inbox implementation simple. Start with `VecDeque`; channels can
  wait until worker handles are needed.

Tests:

- local tick is processed through the runtime
- events are processed in insertion order
- `process_some_events` respects `max_events`
- neighbor imported-offer event processes only target-region payload
- outbound continuation message is returned instead of directly mutating caller

Review focus:

- Region event handling is deterministic.
- Runtime owns one region only.
- No worker or runtime code reads another region's ECS state.

Transition point:

- After this patch, the current implementation can start delegating a local
  single-region tick through `RegionRuntime` in a later small patch if review
  confirms the behavior matches `Game::tick`.
- This is not the full threading model yet. Cross-region caller follow-up still
  needs Patch 4, shared worker scheduling needs Patch 5, stable mailbox handles
  need Patch 6, and actual OS threads should wait until Patch 8.

## Patch 4: Opaque Caller Continuation

Goal: implement caller-owned follow-up work as an opaque reply handle.

Likely files:

- `src/core/regions/continuation.rs`
- `src/core/regions/runtime.rs`
- `tests/region_continuation_test.rs`

Implementation:

- Add `CallerContinuation<R>` with:
  - caller region ID
  - boxed `FnOnce(&mut RegionState, R) + Send + 'static`
- Expose `new` and `caller_region`.
- Keep `run` private to the runtime module.
- Add `NeighborRequest<P, R>` carrying payload plus continuation.
- Add imported-offer request and result types.
- Route returned continuations as events rather than executing them in the
  neighbor.

Tests:

- target region returns the same continuation with a result
- continuation does not run while target region handles the request
- continuation runs only when caller region processes its event
- continuation is consumed once

Review focus:

- Neighbor systems cannot call the continuation.
- Continuations cannot borrow region state across event boundaries.
- The API shape favors owned values over references.

## Patch 5: Shared Single-Thread Worker

Goal: add a worker that schedules multiple region runtimes on one thread.

Likely files:

- `src/core/regions/worker.rs`
- `src/core/regions/runtime.rs`
- `tests/region_worker_test.rs`

Implementation:

- Add `WorkerId`.
- Add `RegionWorker` containing multiple `RegionRuntime` values.
- Add `run_once(max_events_per_region)`.
- Route `OutboundMessage` values through region IDs.
- Keep routing inside the worker as message delivery only. The worker must not
  inspect or mutate `RegionState` directly.

Tests:

- one worker processes events for multiple regions
- busy region cannot starve another region when event limits are set
- returned continuation is routed to the caller region inbox
- missing target region produces a deterministic routing error

Review focus:

- Fair scheduling is bounded and simple.
- Worker logic remains independent of ECS details.
- Error handling is explicit enough for debugging.

## Patch 6: Stable Region Handles

Goal: replace direct region lookup with stable mailbox handles so worker
ownership can move later.

Likely files:

- `src/core/regions/handle.rs`
- `src/core/regions/runtime.rs`
- `tests/region_handle_test.rs`

Implementation:

- Add `RegionHandle` or `RegionPort` with region ID and sender endpoint.
- Start with an in-process sender abstraction if avoiding channels keeps the
  patch smaller.
- Make neighbor sends target handles, not workers.
- Keep receivers owned by `RegionRuntime`.

Tests:

- region can send an event through a neighbor handle
- sender can be cloned without cloning the receiver
- receiver remains owned by the target runtime

Review focus:

- Neighbor regions know mailbox endpoints, not worker identity.
- This patch does not start OS threads.

## Patch 7: Worker Reassignment Safe Point

Goal: prove a whole region runtime can move between workers without changing
neighbor communication.

Likely files:

- `src/core/regions/worker.rs`
- `tests/region_worker_reassignment_test.rs`

Implementation:

- Add `remove_region(region_id) -> Option<RegionRuntime>`.
- Add `add_region(region_runtime)`.
- Move only at safe points outside `run_once`.
- Do not add automatic load balancing yet.

Tests:

- a runtime can be removed from one worker and added to another
- only one worker owns the runtime after movement
- existing send handles still deliver to the moved region

Review focus:

- Ownership movement is explicit.
- No runtime is polled by two workers.

## Patch 8: Optional OS Thread Runner

Goal: add an opt-in threaded runner after the single-threaded worker is stable.

Likely files:

- `src/core/regions/threaded.rs`
- `src/core/regions/handle.rs`
- `tests/region_threaded_test.rs`

Implementation:

- Introduce standard-library channels or a small channel dependency only if the
  need is clear.
- Spawn worker threads that each own one `RegionWorker`.
- Keep `RegionHandle` as the only cross-thread send surface.
- Keep message types `Send + 'static`.
- Do not connect this to the UI-facing facade yet unless there is a separate
  reviewed mission for a regional game facade.

Tests:

- threaded worker processes a tick request and returns a result
- neighbor message can cross worker threads
- shutdown drains or rejects pending work deterministically

Review focus:

- No ECS references cross threads.
- Thread lifecycle and shutdown are explicit.
- The non-threaded tests still cover deterministic behavior.

## Patch 9: Regional Game Facade And Snapshot Requests

Goal: expose regional simulation through a high-level API without changing UI
boundaries, and make the UI snapshot request/reply path explicit.

Likely files:

- `src/core/regional_game.rs`
- `src/core/regions/runtime.rs` if a snapshot event or outbound message is added
- `src/core/game.rs` if shared behavior is needed
- `src/interface/adapter.rs` only if a new view model is needed
- `tests/regional_game_api_test.rs`

Implementation:

- Add a separate facade first, such as `RegionalGame`, instead of changing
  `Game` directly.
- Treat `RegionalGame` as the UI-facing region manager/owner. The UI should
  send commands and snapshot requests to this facade, not to `RegionRuntime`.
- Keep UI rendering from view models only.
- Add methods for regional tick and regional inspect/view composition.
- Add owned request/reply data for snapshots. For example:

```text
UiRequest::GetRegionSnapshot { request_id, region_id }

RegionEvent::BuildSnapshot { request_id }

OutboundMessage::RegionSnapshotReady {
  request_id,
  region_id,
  snapshot,
}
```

- Add a UI-safe owned snapshot type if `GameView` is not enough:

```text
RegionViewSnapshot
  region_id
  revision
  view: GameView or region-specific view model
  recent_events

RegionalGameView
  regions: Vec<RegionViewSnapshot>
  selected_region
```

- In the first single-threaded facade, `RegionalGame` can own
  `Vec<RegionRuntime>` directly and dispatch snapshot events by calling runtime
  methods.
- In the later threaded facade, `RegionalGame` should own region handles or
  mailboxes while worker threads own the actual `RegionRuntime` values.
- Keep all UI request and snapshot reply payloads owned. Do not use references,
  closures, `World`, or ECS entity storage in UI-facing messages.
- Design these request/reply payloads so they can later become serializable for
  another process boundary.
- Avoid save/load integration until the authoritative state and cache rebuild
  rules are proven.

Tests:

- regional facade exposes view data without exposing `World`
- regional tick advances each region through its runtime
- UI snapshot request reaches the requested region and returns the matching
  region snapshot
- snapshot request for an unknown region returns a deterministic error
- UI-facing snapshot data is owned and does not expose ECS internals
- imported resource cache can be rebuilt from authoritative region state

Review focus:

- Public API remains clear.
- UI boundary remains protected.
- UI talks only to `RegionalGame`, never directly to `RegionRuntime` or `World`.
- Snapshot request/reply messages are safe to move across threads and can be
  adapted for process boundaries later.
- Save/load compatibility is not accidentally changed.

## Patch 10: Regional Game Runner With One Threaded Worker

Goal: add the production execution owner that starts and stops the threaded
regional worker path, while keeping the initial runtime topology to one
`ThreadedRegionWorker` and one OS thread.

Likely files:

- `src/core/regional_game_runner.rs`
- `src/core/regional_game.rs`
- `src/core/regional_types.rs`
- `src/core/regions/threaded.rs`
- `tests/regional_game_runner_test.rs`

Implementation:

- Add `RegionalGameRunner` as the threaded execution owner that sits below the
  `RegionalGame` facade and above the worker layer. `RegionalGame` stays the
  UI-facing facade and owns/delegates to one `RegionalGameRunner`; the runner
  owns the thread lifecycle, region handles, and the single threaded worker.
- Keep it threaded internally from the start, but create only one
  `ThreadedRegionWorker` for now.
- Move one `RegionWorker` into that threaded worker. The `RegionWorker` owns the
  `RegionRuntime` values, and each runtime owns its `RegionState`.
- Keep UI and future UI-facing code talking only to the `RegionalGame` facade,
  which delegates to `RegionalGameRunner`. UI code must not touch
  `ThreadedRegionWorker`, `RegionWorker`, `RegionRuntime`, or `World` directly.
- Keep shared UI-safe request/reply/snapshot values (`UiRequest`, `UiReply`,
  `RegionViewSnapshot`, `RegionalGameView`, `UiRequestId`) in a neutral
  `regional_types` leaf module so the runner and threaded worker do not depend
  upward on the `RegionalGame` facade module.
- Expose a narrow first API:
  - start from one or more `RegionState` values
  - process/tick one region through the worker
  - request an owned region snapshot
  - shut down and recover the worker/state
- In Patch 10, `tick_region` sends `RegionEvent::Tick` only to the requested
  region, then asks the worker for one fair scheduling pass across all owned
  regions. That can drain already queued work from another region. This is
  acceptable for the first one-worker runner, but target-only event processing
  should be a separate worker command if later APIs need that guarantee.
- For Patch 10, snapshot and inspect requests are synchronous worker control
  commands. They read the selected runtime on the worker thread and return a
  blocking reply; they do not enqueue `RegionEvent::BuildSnapshot` through the
  region inbox. If a caller needs a snapshot after pending region events, it must
  first request bounded event processing. The `UiRequestId` remains part of the
  UI-facing reply contract so a later asynchronous/event-routed snapshot path can
  use it for correlation without changing facade payloads.
- Use explicit request/reply data and deterministic errors. Do not expose raw
  channels, handles, worker internals, or ECS storage through the runner API.
- Do not add multi-worker routing, load balancing, UI migration, or save/load in
  this patch.

Tests:

- runner starts one threaded worker and processes a regional tick
- runner returns an owned snapshot for the requested region
- runner shutdown recovers authoritative region state
- unknown region requests return deterministic errors
- UI-facing code can use runner APIs without importing worker/runtime/ECS types

Review focus:

- `RegionalGameRunner` owns thread lifecycle.
- `RegionalGame` remains the single UI-facing entry point and delegates to the
  runner; UI code never calls the runner, worker, or runtime directly.
- Lower-level modules (`regional_game_runner`, `regions::threaded`) do not import
  the `RegionalGame` facade module; shared payload types live in `regional_types`.
- There is exactly one `ThreadedRegionWorker` in this patch.
- UI boundary remains protected.
- Shutdown is explicit and recovers state for later save/load or handoff.
- No multi-thread load balancing is introduced early.

## Patch 11: Load Manager

Goal: add optional load-based movement after stable handles, safe movement, and
the runner-owned execution boundary are tested.

Likely files:

- `src/core/regions/load_manager.rs`
- `src/core/regions/worker.rs`
- `tests/region_load_manager_test.rs`

Implementation:

- Add `WorkerLoad` with worker ID, region count, queued events, and optional
  frame time.
- Add a deterministic policy for choosing a move.
- Keep the load manager separate from normal message routing.
- Start with static assignment as the default.
- Do not connect this to `RegionalGameRunner` until the deterministic movement
  policy is reviewed independently.

Tests:

- no move is chosen below threshold
- busiest worker moves one region to quietest worker above threshold
- tie handling is deterministic

Review focus:

- Movement policy is understandable and stable.
- Load manager does not inspect region ECS state.

## Player-Facing Gameplay Track

Patches 1 to 11 build a tested threading skeleton, but the regional path can only
tick, view, and inspect. It cannot accept a single player action, so it is not
yet a game. The goal of this track is to make multi-region gameplay real for
players: a player can build, change, and grow more than one region and see the
results, with the terminal UI talking only to the `RegionalGame` facade.

These patches keep the same rules as the earlier ones: one mission per patch, no
UI access to `World`, deterministic local formulas, owned request/reply payloads,
and roughly five files or 400 lines per patch.

Sequencing rationale: the command path (Patch 12) must come first because every
later step depends on a player being able to act on a region. View parity
(Patch 13) proves the regional path matches the prior single-city behavior.
Save/load (Patch 14) protects player progress. Only then is it safe to point the
UI at the facade (Patch 15) and make a second region reachable in play
(Patch 16).

## Patch 12: Regional Command Path

Goal: let a player act on one region through the facade with the same command
surface the game already exposes. This is the missing player-action
layer and the first step toward real gameplay.

Likely files:

- `src/core/regions/mod.rs` (add command methods to `RegionState`)
- `src/core/regions/runtime/mod.rs` (add a command event and owned result routing)
- `src/core/regional_types.rs` (owned command request/reply payloads)
- `src/core/regional_game.rs` and `src/core/regional_game_runner.rs`
- `tests/regional_command_test.rs`

Implementation:

- Add `RegionState` command methods for `build`, `preview_build`,
  `bulldoze`, `replace`, `upgrade`. Reuse the existing core systems; do not fork
  building logic.
- Add an owned `RegionCommand` request type in `regional_types` covering build,
  bulldoze, replace, upgrade, and preview. Keep it `Send + 'static`, with no
  references, closures, `World`, or ECS entity storage. Use `BuildingKind` and
  coordinate fields only.
- Add `RegionEvent::RunCommand { request_id, command }` (or similar) and route the
  owned `CommandResult` / `BuildPreviewView` back as a reply, not as a direct
  mutation of any other region.
- Decide explicitly how command replies reach the caller. Prefer routing through
  the region event loop so command ordering and snapshot ordering share one
  deterministic path. If a synchronous worker command is used instead, document
  why and confirm it cannot read partial state. This is the right time to resolve
  the snapshot-vs-event-loop divergence noted for Patch 9.
- Expose `RegionalGame::build/preview_build/bulldoze/replace/upgrade(region_id, ...)`
  that delegate to the runner. Return deterministic errors for unknown regions.
- Do not add overlays, save/load, or UI wiring in this patch.

Tests:

- a build command applied to a region changes only that region's view
- preview returns owned data and does not mutate the region
- bulldoze, replace, and upgrade route through the facade and produce the same
  `CommandResult` shape as the prior facade
- a command for an unknown region returns a deterministic error
- command payloads and replies are owned and expose no ECS internals
- commands and ticks on one region are processed in a deterministic order

Review focus:

- Building logic is reused from core systems, not duplicated.
- Command request/reply payloads are owned and safe to move across threads.
- One region's command never mutates another region.
- Command and snapshot ordering use one explained path.

## Patch 13: Regional View Parity

Goal: prove that driving a single region through the regional path produces the
same player-visible state as the prior single-city path, before any UI migration.

Likely files:

- `tests/regional_game_parity_test.rs`
- small core additions only if a shared scripted-input helper is needed

Implementation:

- Add a deterministic scripted sequence of commands and ticks.
- Run it through the prior single-city facade and through a single-region `RegionalGame`.
- Compare resulting `GameView` values turn for turn and after each command.
- Add `view_with_overlay` parity once the overlay path exists on the facade. If
  overlays are not yet exposed, add the facade overlay method here as a small,
  isolated addition.

Tests:

- identical command and tick scripts yield identical views from the prior path
  and the single-region facade
- divergence anywhere in the script fails loudly with the first differing turn
- overlay views match when overlays are exposed

Review focus:

- The parity test is strict and deterministic.
- Any facade additions stay minimal and do not change existing behavior.

## Patch 14: Regional Save And Load

Goal: protect player progress by saving and loading authoritative regional state,
following the existing Save And Load Plan.

Likely files:

- `src/core/regional_game.rs` and `src/core/regional_game_runner.rs`
- `src/core/regions/mod.rs` (per-region serialization of authoritative state)
- `tests/regional_save_load_test.rs`

Implementation:

- Save authoritative per-region state and local resource generation counters.
- Do not save imported offers as permanent truth.
- Rebuild imported offers after load by regenerating exports and propagating them
  again, using `rebuild_imported_resource_cache`.
- Recover state at a safe point through the runner shutdown/handoff path rather
  than reading ECS across threads.
- Add compatibility tests before changing any existing single-city save format.

Tests:

- a multi-region game round-trips through save and load with identical views
- imported resource cache is rebuilt, not loaded as truth
- loading does not corrupt or read another region's authoritative state
- existing single-city saves remain loadable

Review focus:

- Authoritative state is the only saved truth.
- Cache rebuild is deterministic and matches a freshly propagated game.
- Save/load happens at explicit safe points.

## Patch 15: UI On The Regional Facade Behind A Flag

Goal: let the terminal UI run on the regional facade without removing the then-working
single-city path, so the migration is reversible.

Likely files:

- `src/main.rs` (launch flag/mode selection)
- `src/ui/tui.rs` and `src/ui/ascii.rs` (drive `RegionalGame` for one region)
- `tests/ui_regional_smoke_test.rs` or an interface-level boundary test

Implementation:

- Add a launch mode, for example `cargo run -- regional`, that drives a
  single-region `RegionalGame`. At this historical step, keep `cargo run` on the
  old default path as the fallback.
- Render only from view models. The UI must not import worker, runtime, or
  `World` types.
- Map existing UI inputs to facade commands and snapshot requests.
- Do not flip the default to regional and do not add a second player-visible
  region in this historical patch.

Tests:

- a UI boundary test drives the regional mode through facade commands and view
  snapshots only
- the UI builds no dependency on ECS, worker, or runtime types
- the default path is unchanged

Review focus:

- UI talks only to `RegionalGame`.
- The old path still works at this migration step.
- Every new public view path has a boundary test.

## Patch 16: Multi-Region Play

Goal: make a second region reachable in normal play so multi-region gameplay is
real for players: build in more than one region, move between regions, and see
cross-region resources take effect.

Likely files:

- `src/core/regional_game.rs` (selected-region navigation, composed view)
- `src/ui/tui.rs` and `src/ui/ascii.rs` (region switching, region indicator)
- `tests/regional_multi_region_play_test.rs`

Implementation:

- Start the regional mode with two or more regions on the one threaded worker.
- Add region selection and switching to `RegionalGameView` and the UI, using the
  existing `selected_region` field.
- Let a player build and tick in each region and see per-region results.
- Surface cross-region imported resources in the view models so their effect is
  visible, reusing the propagation already built in Patches 1 and 4.
- Only after this path is stable, consider flipping the default launch mode to
  regional in a separate reviewed change. That later removal is recorded in
  "Completed Follow-Up: Old Single-Thread Architecture Removal" below.

Tests:

- a player can build in two regions and each region reflects only its own builds
- switching the selected region changes the composed view deterministically
- a cross-region imported resource produced in one region affects the intended
  neighbor and is visible in view models

Review focus:

- Multiple regions are genuinely playable, not just present.
- Cross-region effects are deterministic and visible only through view models.
- Flipping the default launch mode stays a separate, explicit decision.

## Patch 17: Region-Owned Export Propagation

Goal: move cross-region resource propagation ownership into the region runtime
and worker routing layers. `RegionalGame` should issue player-facing commands and
compose view models; `RegionalGameRunner` should own execution and lifecycle.
Neither layer should contain domain-specific imported-resource send APIs.

Likely files:

- `src/core/regions/mod.rs` (region-owned export summary and import cache
  updates)
- `src/core/regions/runtime/mod.rs` (emit export-change outbound messages after
  region events mutate exports)
- `src/core/regions/worker.rs` (route export-change messages to neighbor
  region inboxes)
- `src/core/regional_game.rs` (remove facade-owned export scanning/sync)
- `src/core/regional_game_runner.rs` (remove runner-owned
  `send_imported_resource` domain API)
- `tests/regional_multi_region_play_test.rs`
- `tests/region_worker_test.rs`
- `tests/region_runtime_test.rs`

Implementation:

- Add a region-owned export summary/cache to `RegionState` or `RegionRuntime`.
- After successful build, bulldoze, replace, upgrade, or tick events, detect
  export changes inside the region runtime.
- Emit an outbound message such as `RegionExportsChanged` that carries the
  source region, current exports, and removed export kinds.
- Let the worker route export-change messages to neighboring region inboxes as
  imported-resource events.
- Keep the worker as a message router only. It must not inspect or mutate ECS
  world state.
- Keep `RegionalGameRunner` responsible for pumping worker passes, command
  requests, snapshot/inspect requests, save handoff, and shutdown only.
- Remove or make private any temporary runner/facade method that directly sends
  imported resources, such as `send_imported_resource`.
- Remove export scanning and cross-region resource sync orchestration from
  `RegionalGame`.

Tests:

- a successful build in Region A emits an export-change outbound message from
  Region A's runtime
- the worker routes Region A's export-change message to neighboring Region B
- Region B stores the imported resource in its imported warehouse/cache
- removing the source building emits a tombstone/removal and clears Region B's
  imported cache entry
- `RegionalGame` can build in Region A and observe Region B imported-resource
  visibility without calling a resource-send method
- the runner API no longer exposes a domain-specific imported-resource send
  method

Review focus:

- Region runtime/state owns export detection.
- Imported resources enter a neighbor through normal region events.
- `RegionalGame` does not know how regional resources are propagated.
- `RegionalGameRunner` does not expose domain-specific resource delivery APIs.
- Worker routing remains deterministic and does not access ECS internals.

## Save And Load Plan

Save/load should wait until the regional facade exists. When implemented:

- Save authoritative per-region state.
- Save local resource generation counters.
- Do not save imported offers as permanent truth.
- Rebuild imported offers after load by regenerating exports and propagating
  them again.
- Add compatibility tests before changing existing save files.

## UI Plan

No UI work is needed for the first runtime patches. When regional state becomes
player-visible:

- Add view-model fields before adding UI rendering.
- Keep terminal UI code using `Game` or the `RegionalGame` facade only after the
  facade/runner API reaches feature parity for the needed workflow.
- Current Patch 10 snapshot/inspect requests stop at the worker control surface
  and synchronously read the target runtime after whatever processing the caller
  has explicitly requested. A future asynchronous snapshot path can replace that
  with event-routed `RegionEvent::BuildSnapshot` /
  `OutboundMessage::RegionSnapshotReady` correlation.
- Route snapshot requests down through the facade and runner:

```text
UI thread/process
  -> RegionalGame (facade)
  -> RegionalGameRunner
  -> ThreadedRegionWorker
  -> RegionWorker
  -> RegionalGameRunner
  -> RegionalGame (facade)
  -> UI-safe snapshot reply
```

- Use explicit request IDs for async/threaded/process snapshot replies.
- Add UI boundary tests for every new public view path.

## Dependency Plan

Start with the Rust standard library:

- `VecDeque` for single-threaded inboxes.
- `std::sync::mpsc` only when OS threads are introduced.

Avoid external dependencies unless standard channels become a proven problem.

## Recommended First Mission

Start with Patch 1 only:

```text
Add single-threaded region resource propagation data structures and tests.
```

This is the smallest useful step because it validates the cross-region data
contract before introducing runtimes, continuations, workers, or threads.

## Completed Follow-Up: Old Single-Thread Architecture Removal

This follow-up retired the old UI-facing single-city `Game` execution path after
the regional runtime became ready to serve as the default. It did not remove the
deterministic simulation systems. Those systems remain the core rules in shared
core modules. The cleanup removed the duplicate frontend/backend path where UI
could still drive `Game` directly instead of the regional facade.

Status: complete. The production `Game` facade has been removed, terminal UIs
run through `CityDriver` backed by `RegionalGame`, and legacy single-city saves
load through the regional loader.

Goals:

- Make the regional facade the default UI execution path.
- Preserve deterministic single-region behavior through the regional runtime.
- Preserve compatibility with existing single-city save files.
- Keep UI code using one backend path instead of switching between `Game` and
  `RegionalGame`.
- Keep ECS `World` private to core simulation and region state.
- Remove old single-city UI/backend code only after parity and migration tests
  are in place.

Non-goals:

- Do not remove core simulation systems such as economy, power, population,
  citizens, local effects, or road analysis.
- Do not expose `World` to UI, runner, worker, or coordinator code.
- Do not add multi-worker load balancing as part of this removal.
- Do not change regional imported resources from visibility-only cache into full
  economy inputs in this cleanup.

Resolved blockers:

- Default launch uses the regional path.
- `CityDriver` owns one regional backend path.
- Regional ticks return real `CommandResult` data from the region runtime.
- Save/load uses `RegionalGame::load_from_file`, including legacy single-city
  conversion.
- Shared simulation helpers live in `src/core/simulation.rs`.
- Production `src/core/game.rs` has been removed.

### Patch 18: Regional Tick Result Parity

Goal: make regional ticks return the same player-visible `CommandResult` shape
as the prior single-city tick path.

Implementation:

- Add a request ID-correlated tick reply path from `RegionRuntime` to the
  runner, such as `RegionTickCompleted`, mirroring `RegionCommandCompleted`.
- Return the real `RegionState::tick_local` result instead of fabricating a
  minimal `TurnAdvanced` event.
- Keep this as plumbing, not a reimplementation: `RegionState::tick_local`
  already calls the shared `tick_world` helper.
- Keep export-change emission after tick deterministic.
- Preserve command/tick ordering through the same worker pumping rules.

Tests:

- single-region regional tick result matches the prior tick behavior for turn
  and summary event shape
- economy, population, power, and pollution tick summaries remain visible
- regional tick still propagates export changes after local tick work

### Patch 19: Single-City Save Compatibility Through Regional Loader

Goal: let the regional path load existing single-city saves so removing the old
UI backend does not strand player saves.

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
- converted save exposes the same selected region view as the old load path
- converted game can continue ticking, building, saving, and loading again
- invalid save errors stay deterministic and user-readable

### Patch 20: Make Regional UI The Default

Goal: switch normal launch to the regional backend while keeping an emergency
legacy command only for one patch if needed.

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

### Patch 21: Collapse CityDriver To One Backend

Goal: remove the duplicate UI backend branch and make `CityDriver`
regional-only.

Implementation:

- Remove `CityBackend::SingleCity`.
- Remove single-city-specific driver constructors.
- Keep one driver command/view/save/load path backed by `RegionalGame`.
- Update UI tests to assert the regional facade is the only backend.
- Either migrate UI test modules off `Game` parity helpers, or make the boundary
  test explicitly ignore `#[cfg(test)]` imports until `Game` is retired or
  re-scoped.

Tests:

- driver commands route through `RegionalGame`
- driver save/load accepts legacy and regional saves
- unavailable backend behavior still protects UI after unrecoverable save errors
- non-test UI code does not import `Game`, ECS, worker, or runtime types

### Patch 22: Move Shared Simulation Helpers Out Of `game.rs`

Goal: remove the regional runtime's dependency on helper functions that lived in
the UI-facing single-city `Game` module.

Implementation:

- Move `tick_world` out of `game.rs` into a neutral core module.
- Move `refresh_derived_state_for_world` out of `game.rs` into the same neutral
  core module, or another clearly named simulation helper module.
- Update both the old facade and `RegionState` to import these helpers from the
  neutral module during the transition.
- Check for any other helper in `game.rs` that regional code imports or would
  need before `Game` can be retired.
- Keep behavior unchanged. This patch is a relocation only.

Tests:

- existing behavior tests still pass
- regional tick, command, save/load, and parity tests still pass
- no regional module imports `crate::core::game` after the relocation

### Patch 23: Retire `Game`

Goal: remove `Game` from public UI usage and then delete the production facade
once shared simulation helpers no longer live in `game.rs`.

Implementation:

- Migrate tests to `RegionalGame` or lower-level system helpers.
- Use test-only wrappers over `RegionalGame` where single-region scenario tests
  remain clearer with simple city method names.
- Delete the production `Game` facade.
- Keep public API changes intentional and documented.

Tests:

- all behavior tests have equivalent regional or system coverage
- save/load compatibility remains covered after the public API change
- production core no longer exports the old game facade

### Patch 24: Documentation And Cleanup

Goal: remove stale documentation and tests that describe regional mode as
experimental or opt-in after it becomes the only UI path.

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

Final removal checklist:

- [x] `cargo run` launches the regional path by default.
- [x] ASCII and ratatui frontends share one regional driver path.
- [x] UI modules do not import `Game`, ECS, worker, or runtime internals.
- [x] Regional tick returns real `CommandResult` data.
- [x] Existing single-city saves load through the regional path.
- [x] Multi-region saves still round trip.
- [x] Production `Game` has been removed; remaining single-region behavioral
  coverage uses the regional facade or test-only wrappers over it.
- [x] `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass.

## Review Checklist For Each Patch

Before requesting human review, check:

- Did the patch implement only one mission?
- Did it avoid unrelated refactors?
- Did UI avoid accessing ECS internals?
- Are local formulas deterministic?
- Are tests meaningful?
- Are hidden balance risks documented?
- Did `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` pass for code
  changes?
