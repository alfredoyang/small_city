# Multithreaded Region Actor Implementation Plan

## Purpose

This document breaks the region actor proposal into small implementation missions.
The plan is intentionally incremental so each change stays deterministic, reviewable,
and compatible with the existing `Game` API and `GameView` UI boundary.

The proposal document is:

```text
docs/multithreaded-region-actor-proposal.md
```

## Non-Goals

Do not start by converting the whole simulation to real multithreading.

Do not expose ECS `World` to UI.

Do not make UI render from actor state, component stores, queues, or worker internals.

Do not replace the custom ECS with an external ECS crate during this plan.

Do not introduce nondeterministic scheduling behavior into simulation rules.

## Target Architecture

The long-term target is:

```text
Game API
  owns simulation runtime

Simulation runtime
  owns clock and phase progression
  owns region actors
  owns optional global actors

Region actors
  own mutable local region state
  process deterministic event queues
  communicate through ticked and phased messages

Interface adapter
  builds UI-safe GameView after deterministic phase boundaries

UI
  uses Game API and GameView only
```

## Implementation Principles

1. Keep execution single-threaded until message ordering and determinism are proven.
2. Add real worker threads only behind an already-tested actor/event-loop interface.
3. Every message must carry tick and phase metadata.
4. Promise callbacks may collect data, but state commits must happen through ordered local events.
5. Actor events must be processable in stable order.
6. Tests must be able to shuffle message delivery and still get the same final state.
7. Every mission should touch five files or fewer unless explicitly split first.
8. Every mission should include tests unless it is documentation-only.

## Mission 1: Prototype Deterministic Actor Runtime

### Goal

Create a small single-threaded actor runtime prototype that does not change current
game behavior.

This runtime should prove:

```text
region actors have inboxes
messages include tick and phase
events are processed in stable order
message delivery can be shuffled without changing final output
```

### Likely Files

```text
src/core/region_actor.rs
src/core/mod.rs
tests/region_actor_runtime_test.rs
```

If the project prefers unit tests beside the module, keep tests in `region_actor.rs`
instead of adding an integration test.

### Design

Add basic types:

```rust
RegionId
SimTick
SimPhase
MessageSequence
RegionMessage
RegionEvent
RegionActor
ActorRuntime
```

The prototype actor state can be deliberately tiny, such as a counter or local metric.
It should not own real city ECS data yet.

### Tests

Add tests for:

```text
messages sort by tick, phase, source region, sequence
shuffled delivery produces the same actor state
actor only commits state through local events
stale tick messages are rejected or deferred according to one clear rule
```

### Stop Condition

Stop after the prototype runtime passes tests. Do not connect it to real systems yet.

## Mission 2: Add Promise Group And Promise Chain Primitives

### Goal

Add deterministic async request helpers on top of the single-threaded actor runtime.

### Likely Files

```text
src/core/region_actor.rs
tests/region_actor_runtime_test.rs
```

If `region_actor.rs` grows too large, split promise logic into:

```text
src/core/region_promise.rs
```

### Design

Add:

```rust
PromiseId
PromiseGroup
PromiseChain
PromiseResponse
PromiseResolved event
```

Promise callbacks should only record responses and enqueue local events.
They should not directly mutate actor simulation state.

Promise groups should support:

```text
parallel request collection
stable response ordering before apply
complete only when all required responses arrive
```

Promise chains should support:

```text
request A
after A response, request or enable B
after B response, enqueue one deterministic apply event
```

### Tests

Add tests for:

```text
promise group resolves after all responses arrive
promise group applies responses in stable dependency order
promise chain preserves A before B order
promise callback does not directly mutate committed state
late response cannot modify an already-completed tick
```

### Stop Condition

Stop once promises are deterministic in the prototype runtime. Do not add threads.

## Mission 3: Introduce Region Partition Metadata

### Goal

Define region identity and map partitioning without moving simulation state yet.

### Likely Files

```text
src/core/grid.rs
src/core/region.rs
src/core/mod.rs
tests/region_partition_test.rs
```

### Design

Add a deterministic map-to-region helper:

```text
GridPos -> RegionId
RegionId -> region bounds
RegionId -> neighboring regions
```

Keep this separate from actor execution.

### Tests

Add tests for:

```text
every map cell maps to exactly one region
region mapping is deterministic
border cells identify expected neighbor regions
small maps still produce valid regions
```

### Stop Condition

Stop after region metadata exists and is tested. Do not move ECS storage.

## Mission 4: Build A Fake Cross-Region Query

### Goal

Connect the actor runtime and region metadata with one fake query that resembles a
future simulation dependency but does not change game rules.

### Likely Files

```text
src/core/region_actor.rs
src/core/region.rs
tests/region_actor_runtime_test.rs
```

### Design

Example fake query:

```text
Region C asks neighboring regions for a numeric border metric.
Neighbors respond with deterministic values.
Region C collects responses through a promise group.
Region C commits a local derived metric at a phase boundary.
```

### Tests

Add tests for:

```text
neighbor query requests only expected regions
responses delivered in different orders produce same derived metric
promise result is committed only at the expected phase
```

### Stop Condition

Stop after fake cross-region query behavior is deterministic.

## Mission 5: Move One Read-Only Derived Rule Into Actor Prototype

### Goal

Prototype one real but read-only derived calculation through region actors.

Good candidates:

```text
local effect sampling
border pollution summary
border desirability summary
road-neighbor summary
```

Avoid first candidates that own money, population growth, entity creation, or save/load
state.

### Likely Files

Depends on the chosen rule, but keep the mission to five files or fewer.

Possible files:

```text
src/core/region_actor.rs
src/core/local_effects.rs
src/core/region.rs
tests/local_effects_integration_test.rs
tests/region_actor_runtime_test.rs
```

### Design

Keep the existing system as the source of truth. The actor path should either:

```text
run in shadow mode and compare results
or compute an isolated derived metric not yet used by gameplay
```

### Tests

Add tests for:

```text
actor-derived result matches existing deterministic system result
shuffled message delivery does not change result
UI still uses GameView only
```

### Stop Condition

Stop after one derived read-only rule is validated. Do not remove the existing system yet.

## Mission 6: Add Runtime Integration Behind Game API

### Goal

Add the actor runtime as private core infrastructure owned behind `Game`, while keeping
public API behavior unchanged.

### Likely Files

```text
src/core/game.rs
src/core/region_actor.rs
src/core/mod.rs
tests/game_api_test.rs
tests/ui_contract_test.rs
```

### Design

`Game` may own a private runtime field, but callers should not see it.

The API must remain:

```text
Game::tick
Game::view
Game::inspect
Game::save_to_file
Game::load_from_file
```

Do not expose actors, messages, or region state in interface or UI layers.

### Tests

Add or update tests for:

```text
GameView output is unchanged for existing scenarios
UI contract still prevents ECS or actor internals from leaking
save/load still round-trips and refreshes derived state
```

### Stop Condition

Stop when runtime ownership is private and existing behavior remains unchanged.

## Mission 7: Add Real Worker Threads Behind Runtime Interface

### Goal

Replace the single-threaded runtime executor with an optional threaded executor.

This should happen only after deterministic actor behavior is proven.

### Likely Files

```text
src/core/region_actor.rs
src/core/actor_executor.rs
src/core/mod.rs
tests/region_actor_runtime_test.rs
```

### Design

Create an executor boundary:

```rust
trait ActorExecutor {
    fn run_phase(&mut self, phase_work: PhaseWork) -> PhaseResult;
}
```

Implement:

```text
SingleThreadActorExecutor
ThreadedActorExecutor
```

Use the single-thread executor as the default until the threaded executor passes
determinism tests.

### Tests

Add tests for:

```text
single-thread executor and threaded executor produce identical results
shuffled delivery stays deterministic
threaded executor cannot expose actor internals to UI
```

### Stop Condition

Stop after threaded execution is optional and behavior matches single-thread execution.

## Mission 8: Convert One Real System

### Goal

Convert one real simulation system to region actors.

Pick a system with low ownership risk.

Recommended first candidates:

```text
local effects
pollution pressure
land value / desirability derivation
```

Avoid first candidates:

```text
economy money flow
citizen creation/deletion
save/load
power networks spanning many regions
road connectivity/pathfinding
```

### Likely Files

Depends on selected system. Keep the mission split if it would exceed five files.

### Tests

Add or update tests for:

```text
converted system preserves existing behavior
converted system is deterministic under shuffled delivery
existing scenario tests still pass
UI views remain unchanged
```

### Existing Test Modification Policy

Only modify existing tests when the actor conversion intentionally changes internal
timing or event details.

If an existing test must change, explain:

```text
which assertion changed
why the previous assertion no longer describes the public behavior
how the new assertion preserves the same user-facing guarantee
```

## Mission 9: Add Performance Measurements

### Goal

Measure whether the actor model helps before converting more systems.

### Likely Files

```text
benches/
or tests/scenario-style measurement helper
or a small dev-only example
```

Avoid adding benchmark dependencies unless clearly needed.

### Measurements

Track:

```text
tick time for small city
tick time for large city
message count per tick
promise count per tick
phase duration
actor queue length
```

### Stop Condition

Stop if measurements show message overhead is larger than the system cost.
Revisit storage layout or dense region stores before converting more systems.

## Suggested Mission Order

Recommended order:

```text
1. Deterministic actor runtime prototype
2. Promise group and promise chain primitives
3. Region partition metadata
4. Fake cross-region query
5. One read-only derived rule in shadow mode
6. Private Game-owned runtime integration
7. Optional threaded executor
8. Convert one real low-risk system
9. Measure performance
```

## Review Checklist For Each Mission

Before marking a mission complete, review:

```text
Did this mission implement only one thing?
Did it avoid unrelated refactors?
Did UI avoid ECS and actor internals?
Are tick and phase rules explicit?
Are message and promise orders deterministic?
Are tests meaningful?
Were stale or late responses handled?
Are there hidden balance risks?
Did save/load compatibility remain intact?
```

## Required Commands

Run after each mission:

```sh
cargo fmt
cargo clippy -- -D warnings
cargo test
```

## Recommendation

Start with Mission 1 only.

Do not implement worker threads until the single-threaded actor model has tests for:

```text
stable event ordering
shuffled message delivery
promise grouping
promise linking
stale response handling
phase-boundary commits
```

This keeps the risky part of the architecture visible and testable before real thread
scheduling is involved.
