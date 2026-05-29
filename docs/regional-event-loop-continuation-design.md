# Regional Event Loop Continuation Design

This document records the current implementation design for future regional
simulation work. It narrows the idea in
`docs/regional-imported-resource-threading-model.md` into a simple actor-style
runtime model.

This is a design note only. It does not change the current single-region game.

## Goals

- Each region owns its own authoritative simulation state.
- Each region has an event loop that processes local work and neighbor work.
- A region can send work to a neighboring region and receive a completion.
- Any follow-up for a completed neighbor event runs in the caller region.
- More than one region can run on the same worker thread.
- The design stays simple enough to test without requiring async or a global
  ECS world.

## Ownership Rules

- A region never reads or mutates another region's ECS world.
- Messages contain owned data, IDs, and compact resource summaries.
- Messages do not contain references to `World`, entities from another region,
  or UI state.
- The UI still uses only the public `Game` API and view models.
- Cross-region imported resources are treated as rebuildable cache, not
  permanent authoritative state.

## Runtime Shape

The core unit is a region runtime:

```text
RegionRuntime
  region state
  event inbox
  process_next_event()
```

A worker thread may own more than one `RegionRuntime`:

```text
Worker thread
  Region A event loop
  Region B event loop
  Region C event loop
```

The worker repeatedly gives each assigned region a small amount of work. This
allows one operating-system thread to run multiple regions. Later, a coordinator
can assign regions to different workers depending on load.

The first implementation should not require async. Plain queues or a small
channel library such as `crossbeam-channel` are enough. The messages should
still be owned and explicit so an async runtime can drive the same design later
if the project needs network or external I/O.

## Events

Local work and neighbor work use the same event loop. A local tick is just an
event sent to the region itself.

Example event shape:

```text
RegionEvent
  Tick
  ProcessImportedOffer(request)
  RunContinuation(continuation, result)
```

`Tick` runs local deterministic systems.

`ProcessImportedOffer` asks this region to process imported resource work for a
neighbor.

`RunContinuation` runs a caller-owned follow-up inside the caller region's event
loop.

## Caller-Owned Continuations

Some cross-region requests need follow-up work. The follow-up should be a
function or closure created by the caller region, but it must execute in the
caller region, not in the neighbor region.

The design is to carry the continuation with the request as an opaque reply
handle:

```text
NeighborRequest
  caller_region
  payload
  continuation
```

The neighbor may carry the continuation and return it with a result, but it must
not execute the continuation.

```text
CallerContinuation
  caller_region
  apply(region_state, result)
```

The continuation's `apply` function should be private to the runtime module.
Neighbor systems should not have an API that lets them call it directly.

## Request Flow

Example: Region A asks Region B to process an imported resource event.

```text
1. Region A creates the request payload.
2. Region A creates a caller-owned continuation.
3. Region A sends NeighborRequest to Region B.
4. Region B processes only the payload.
5. Region B returns result + the same continuation.
6. The worker or coordinator posts RunContinuation to Region A.
7. Region A executes the continuation inside Region A's event loop.
```

The follow-up function travels with the request and comes back with the result.
This avoids a pending-follow-up lookup table while preserving the rule that the
caller region owns follow-up execution.

## Important Constraint

The continuation may move across worker threads if the caller and neighbor run
on different workers. Therefore, a closure-based continuation should be:

```text
FnOnce(&mut RegionState, ResultData) + Send + 'static
```

The closure must not capture references to caller state. It should capture only
small owned values needed to apply the result later. The mutable caller region
state is provided only when the continuation runs inside the caller's event
loop.

## Worker Responsibilities

Workers and coordinators move messages. They do not inspect or mutate ECS
worlds.

```text
Worker
  poll assigned region inboxes
  call process_next_event()
  route outbound messages
  post returned continuations to caller region inboxes
```

If a worker receives a completed neighbor request, it routes the continuation
back to the caller region as a `RunContinuation` event.

## Determinism Rules

Inside one region:

- Process events in a stable order.
- Run local systems in a fixed order.
- Apply imported resource changes at explicit event or tick boundaries.
- Keep resource propagation formulas integer-based and deterministic.

Across regions:

- Message arrival may be eventually consistent.
- Neighbor results do not need to resolve on the same tick everywhere.
- The same imported resource identity must not be rewritten by forwarding
  regions.

## First Implementation Mission

The first code mission should be smaller than full multithreading:

```text
Add a single-threaded RegionRuntime that can process local events, neighbor
imported-offer events, and caller-region continuation events.
```

Suggested tests:

- A local tick is processed through the region event loop.
- A neighbor request processes only its payload in the target region.
- A completed neighbor request posts a continuation back to the caller region.
- The continuation runs in the caller region, not the neighbor region.
- A continuation cannot borrow caller region state across the request boundary.

After that works, a later mission can add a worker that runs multiple region
runtimes on one thread.

## Risks

- Closure continuations are opaque, so they are harder to inspect than explicit
  follow-up action enums.
- Continuations must not capture non-owned references.
- Long chains of region messages can make debugging difficult without good
  tracing.
- Load-based region reassignment should wait until the single-threaded event
  model is tested.

For gameplay rules, an explicit `FollowUpAction` enum may still be easier to
debug than arbitrary closures. Closure continuations are best used where the
runtime needs flexible caller-owned follow-up behavior.
