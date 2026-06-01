# Regional Multithread Implementation Plan

This document turns the regional threading design notes into a small,
human-reviewable implementation sequence. It is a plan only; it does not change
the current single-region game behavior.

Related design notes:

- `docs/regional-imported-resource-threading-model.md`
- `docs/regional-event-loop-continuation-design.md`
- `docs/regional-opaque-continuation-reply-handle.md`
- `docs/regional-shared-worker-thread-architecture.md`

## Guiding Rules

- Keep the current `Game` API as the public boundary.
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
Game API
  owns current single-city behavior
  later owns or delegates to a regional simulation facade

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

Goal: introduce a region-owned state wrapper while preserving the existing
single-city `Game` API.

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
  so `Game::tick` and `RegionState::tick_local` use the same order.
- Keep `World` private to core internals.
- Do not change UI code.

Tests:

- `RegionState::tick_local` advances the same deterministic state as
  `Game::tick` for a simple city
- imported offer processing does not expose or mutate another region's state
- `Game::view` and `Game::inspect` still use UI-safe view models

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
- Do not connect this to the default `Game` API yet unless there is a separate
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
- `src/core/regions/threaded.rs`
- `tests/regional_game_runner_test.rs`

Implementation:

- Add `RegionalGameRunner` as the public execution owner above `RegionalGame`.
- Keep it threaded internally from the start, but create only one
  `ThreadedRegionWorker` for now.
- Move one `RegionWorker` into that threaded worker. The `RegionWorker` owns the
  `RegionRuntime` values, and each runtime owns its `RegionState`.
- Keep UI and future UI-facing code talking only to `RegionalGameRunner` methods,
  not to `ThreadedRegionWorker`, `RegionWorker`, `RegionRuntime`, or `World`.
- Expose a narrow first API:
  - start from one or more `RegionState` values
  - process/tick one region through the worker
  - request an owned region snapshot
  - shut down and recover the worker/state
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
- Keep terminal UI code using `Game` or `RegionalGameRunner` only after the
  runner API reaches feature parity for the needed workflow.
- Route snapshot requests through the runner/facade:

```text
UI thread/process
  -> RegionalGameRunner
  -> ThreadedRegionWorker
  -> RegionWorker
  -> RegionRuntime
  -> RegionalGameRunner
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
