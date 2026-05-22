# Multithreaded Region Actor Proposal

## Purpose

This document proposes a future multithreading architecture for the city simulation.
The goal is to improve simulation performance without exposing ECS internals to the UI,
without allowing data races, and without losing deterministic behavior.

The proposal favors region-owned worker event loops and asynchronous message passing
over a single coordinator that repeatedly scans and snapshots all ECS `HashMap` data.

## Problem

The current ECS-style storage separates data into maps such as:

```text
Entity -> Component
```

That is simple and flexible, but it is not ideal for future multithreading.

Two common multithreading approaches have important drawbacks:

1. Central snapshot creation
   - A coordinator thread scans component maps.
   - It builds read-only snapshots for worker threads.
   - It later merges worker results back into the authoritative world.
   - This can become expensive if most regions need updates every tick.

2. Shared mutable world access
   - Worker threads directly read and write shared simulation state.
   - This is risky for determinism, borrowing, locking, and debugging.
   - This should be avoided.

The deeper issue is not only thread execution. It is ownership of simulation state and
how cross-region data dependencies are handled.

## Proposed Architecture

Use a region actor model.

Each region owns its local simulation data and runs inside its own event loop:

```text
RegionActor A owns Region A state
RegionActor B owns Region B state
RegionActor C owns Region C state
```

Only the owning region actor mutates its region state.

Cross-region communication happens through messages:

```text
Region C needs data from Region A and Region B
Region C sends async requests to A and B
Region C continues local work
Region A and B send responses later
Region C receives responses through its event loop
```

This avoids a central thread repeatedly rebuilding full-city snapshots. It also avoids
multiple worker threads mutating the same world state.

## Event Loop Model

Each region actor processes events from its inbox:

```text
Local tick event
Cross-region request
Cross-region response
Promise resolved event
Transfer entity event
```

Events should include deterministic metadata:

```text
tick
phase
source region
target region
sequence number
message kind
payload
```

This metadata prevents data from different ticks or phases from being mixed accidentally.

## Promise-Based Cross-Region Data

Cross-region requests should be non-blocking.

A region should not stop its event loop while waiting for another region. Instead:

```text
send request
register pending promise
keep processing other work
receive response later
resolve promise
enqueue deterministic local event
```

Promise callbacks should not directly commit simulation state. They should collect
responses and enqueue local deterministic events.

Recommended pattern:

```text
callback(response):
  store response in pending query
  if query is complete:
    enqueue PromiseResolved(query_id)

event loop:
  process PromiseResolved(query_id)
  apply result in deterministic order
```

This keeps thread scheduling from becoming simulation behavior.

## Promise Linking

If a calculation requires data in an exact order, promises can be linked:

```text
Promise A resolves
then Promise B is requested or allowed to resolve
then combined result is applied
```

Example:

```text
Region C needs Region A result before Region B result

1. C sends request to A
2. A responds
3. C records A result
4. C sends request to B or enables B continuation
5. B responds
6. C enqueues Apply(A, B)
```

This preserves local dependency order.

For independent data, prefer promise groups:

```text
send requests to A, B, and D in parallel
collect all responses
sort responses by stable dependency order
enqueue one local apply event
```

Promise groups avoid unnecessary serialization on hot paths.

## Ticks And Phases

The simulation should still use fixed ticks and phases.

Example:

```text
Tick 120 Phase 1: collect local demand
Tick 120 Phase 2: answer cross-region requests
Tick 120 Phase 3: apply resolved local events
Tick 120 Phase 4: publish view data
```

Async messages may arrive at any real time, but they should only affect the tick and
phase they belong to.

If a response arrives too late, the receiving actor should follow a clear rule:

```text
discard stale response
or apply it to a future tick
or log a deterministic warning event
```

It should not modify the current tick unexpectedly.

## Coordinator Role

This architecture can still use a coordinator, but the coordinator should not own all
simulation data.

The coordinator should act as a clock and phase controller:

```text
start tick
start phase
wait for region phase-complete messages
advance phase
request GameView assembly
```

The coordinator may also own global actors or reducers for city-wide systems.

Examples:

```text
BudgetActor
PowerNetworkActor
TrafficRouterActor
EconomyActor
ClockActor
```

These global actors should communicate through the same message system.

## Entity Ownership Across Regions

Entities that cross region boundaries need explicit transfer rules.

Recommended transfer flow:

```text
source region sends TransferRequest
target region replies TransferAccepted
source region removes local ownership
target region creates or adopts local ownership
```

At no point should two regions believe they both own the same mutable entity.

For deterministic behavior, transfers should include:

```text
entity id
source region
target region
tick
phase
transfer sequence
entity payload or migration data
```

## UI Boundary

The UI must continue to use only the Game API and render only from `GameView`.

The UI should not access:

```text
RegionActor internals
ECS World internals
message queues
worker state
component stores
```

The simulation should publish a view model after a deterministic phase boundary.

## Determinism Rules

To keep the simulation deterministic:

1. All messages must include tick and phase metadata.
2. Region actors should process committable local events in stable order.
3. Promise callbacks should collect responses, not directly mutate simulation state.
4. State commits should happen through deterministic local events.
5. Cross-region responses should not modify already-completed ticks.
6. Tests should run the same scenario with shuffled message delivery order.

## Testing Strategy

Before converting real systems, create a small prototype test:

```text
two or three region actors
one cross-region query
one promise group
one promise chain
shuffled response delivery
same final state asserted every run
```

Important tests:

```text
same messages in different arrival orders produce same final state
stale responses do not affect current tick
entity transfer has exactly one owner after completion
UI receives only GameView data
phase advancement waits for required region completion
```

## Migration Plan

Use small missions.

Suggested first mission:

```text
Create a single-threaded prototype of RegionActor event loops.
Use deterministic queues and fake message delivery.
Add tests for promise groups, promise linking, and shuffled delivery order.
```

Suggested second mission:

```text
Move one simple cross-region rule into actor messages.
Keep execution single-threaded.
Verify behavior with tests.
```

Suggested third mission:

```text
Add real worker threads behind the same actor/event-loop interface.
The public Game API and GameView boundary should not change.
```

This keeps the architecture testable before introducing actual thread scheduling.

## Risks

This architecture is more complex than central snapshots.

Main risks:

```text
message ordering bugs
stale responses
long promise chains serializing performance
harder debugging across actors
entity transfer edge cases
global systems becoming bottlenecks
```

The mitigation is to keep the model ticked, phased, and heavily tested before using
real worker threads.

## Recommendation

Keep this architecture as a serious option, but prototype it before committing the
whole simulation to it.

The best near-term experiment is:

```text
single-threaded deterministic actor simulation
fake async message passing
promise linking
promise grouping
shuffled delivery tests
```

If that prototype stays readable and deterministic, then region actors can become the
foundation for future multithreaded performance work.
