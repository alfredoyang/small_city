# Regional Opaque Continuation Reply Handle

This document describes a focused design for cross-region follow-up work:

```text
Carry the continuation with the request as an opaque reply handle, then route it
back to the caller when the neighbor finishes.
```

This is a design note only. It does not change the current single-region game.

## Problem

One region may ask a neighboring region to process work. After the neighbor
finishes, the caller may need to run follow-up logic.

The important rule is:

```text
The follow-up runs in the caller region, not in the neighboring region.
```

For example:

```text
Region A sends work to Region B.
Region B processes the work.
Region A updates its own imported resource state after Region B completes.
```

Region B must not mutate Region A, call Region A directly, or execute Region
A's follow-up closure.

## Design Summary

The caller creates both:

- the request payload for the neighbor
- a caller-owned continuation that knows how to apply the result later

The continuation travels with the request as an opaque reply handle.

```text
Region A creates request + continuation.
Region A sends both to Region B.
Region B processes only the request payload.
Region B returns result + the same continuation.
Runtime routes the continuation back to Region A.
Region A executes the continuation in its own event loop.
```

The neighbor is allowed to carry the continuation around, but it should not have
an API that allows it to run the continuation.

## Rust Pseudocode

The continuation owns a closure and the caller region ID.

```rust
pub struct CallerContinuation<R> {
    caller_region: RegionId,
    apply: Box<dyn FnOnce(&mut RegionState, R) + Send + 'static>,
}
```

The public API exposes where the continuation must return:

```rust
impl<R> CallerContinuation<R> {
    pub fn caller_region(&self) -> RegionId {
        self.caller_region
    }
}
```

The `run` method should be private to the runtime module. Neighbor systems
should not be able to call it.

```rust
impl<R> CallerContinuation<R> {
    fn run(self, region: &mut RegionState, result: R) {
        (self.apply)(region, result);
    }
}
```

The neighbor request carries payload plus the continuation:

```rust
pub struct NeighborRequest<P, R> {
    pub caller_region: RegionId,
    pub payload: P,
    pub continuation: CallerContinuation<R>,
}
```

For imported resources, the payload and result could look like this:

```rust
pub struct ImportedOfferPayload {
    pub from_region: RegionId,
    pub offer: ImportedResourceOffer,
}

pub struct ImportedOfferResult {
    pub accepted: bool,
    pub remaining_capacity: u32,
    pub forwarded_offers: Vec<ImportedResourceOffer>,
}
```

Region events can then stay explicit:

```rust
pub enum RegionEvent {
    Tick,
    ProcessImportedOffer(
        NeighborRequest<ImportedOfferPayload, ImportedOfferResult>,
    ),
    RunImportedOfferContinuation {
        continuation: CallerContinuation<ImportedOfferResult>,
        result: ImportedOfferResult,
    },
}
```

## Sending Work To A Neighbor

Region A creates the continuation before sending work to Region B.

```rust
impl RegionRuntime {
    fn send_imported_offer_to_neighbor(
        &mut self,
        neighbor_region: RegionId,
        offer: ImportedResourceOffer,
    ) -> OutboundMessage {
        let caller_region = self.region.id();

        let continuation = CallerContinuation {
            caller_region,
            apply: Box::new(|region, result| {
                region.apply_neighbor_import_result(result);
            }),
        };

        let request = NeighborRequest {
            caller_region,
            payload: ImportedOfferPayload {
                from_region: caller_region,
                offer,
            },
            continuation,
        };

        OutboundMessage::ToRegion {
            target_region: neighbor_region,
            event: RegionEvent::ProcessImportedOffer(request),
        }
    }
}
```

The closure does not capture `&mut self` or any borrowed region state. It only
describes what to do once the caller region state is available again.

## Neighbor Processing

Region B processes the payload and returns the continuation with the result.

```rust
impl RegionRuntime {
    fn process_imported_offer(
        &mut self,
        request: NeighborRequest<ImportedOfferPayload, ImportedOfferResult>,
    ) -> OutboundMessage {
        let result = self
            .region
            .process_imported_offer(request.payload.from_region, request.payload.offer);

        OutboundMessage::ReturnContinuation {
            caller_region: request.continuation.caller_region(),
            continuation: request.continuation,
            result,
        }
    }
}
```

Region B never calls:

```rust
request.continuation.run(...)
```

Only the runtime path that delivers events to the caller region should call
`run`.

## Routing Back To The Caller

The worker or coordinator turns the completed neighbor result into a caller
event.

```rust
impl Worker {
    fn route_outbound_message(&mut self, message: OutboundMessage) {
        match message {
            OutboundMessage::ToRegion {
                target_region,
                event,
            } => {
                self.inbox_for(target_region).push(event);
            }
            OutboundMessage::ReturnContinuation {
                caller_region,
                continuation,
                result,
            } => {
                self.inbox_for(caller_region)
                    .push(RegionEvent::RunImportedOfferContinuation {
                        continuation,
                        result,
                    });
            }
        }
    }
}
```

The worker routes messages only. It does not inspect or mutate any ECS world.

## Running The Follow-Up

Region A receives the continuation event and executes it inside Region A's
event loop.

```rust
impl RegionRuntime {
    fn process_next_event(&mut self, event: RegionEvent) -> Vec<OutboundMessage> {
        match event {
            RegionEvent::Tick => self.run_tick(),

            RegionEvent::ProcessImportedOffer(request) => {
                vec![self.process_imported_offer(request)]
            }

            RegionEvent::RunImportedOfferContinuation {
                continuation,
                result,
            } => {
                continuation.run(&mut self.region, result);
                Vec::new()
            }
        }
    }
}
```

At this point, the follow-up is running in the caller region because
`self.region` belongs to Region A's runtime.

## Why This Avoids A Pending Map

An alternative design stores follow-ups in a map:

```text
pending_followups: HashMap<RequestId, FollowUp>
```

That works, but it adds lookup and lifecycle bookkeeping. The opaque
continuation handle avoids that by moving the follow-up through the request
flow:

```text
request owns continuation
completion owns same continuation
caller event owns same continuation
caller event loop consumes continuation
```

The continuation is `FnOnce`, so it naturally runs at most one time.

## Safety Rules

The continuation closure should be:

```rust
Box<dyn FnOnce(&mut RegionState, ResultData) + Send + 'static>
```

This means:

- `FnOnce`: the follow-up is consumed when it runs.
- `Send`: the request can move between worker threads.
- `'static`: the closure cannot borrow temporary caller state.

The closure must not capture:

```rust
&mut RegionState
&World
&mut World
&mut Game
```

It may capture small owned values:

```rust
let source_neighbor = neighbor_region;
let original_offer_id = offer.id;

let continuation = CallerContinuation {
    caller_region,
    apply: Box::new(move |region, result| {
        region.record_import_result(source_neighbor, original_offer_id, result);
    }),
};
```

## Module Boundary

To keep the continuation opaque, put the executable part behind a narrow module
boundary.

```rust
// runtime/continuation.rs
pub struct CallerContinuation<R> {
    caller_region: RegionId,
    apply: Box<dyn FnOnce(&mut RegionState, R) + Send + 'static>,
}

impl<R> CallerContinuation<R> {
    pub fn new(
        caller_region: RegionId,
        apply: impl FnOnce(&mut RegionState, R) + Send + 'static,
    ) -> Self {
        Self {
            caller_region,
            apply: Box::new(apply),
        }
    }

    pub fn caller_region(&self) -> RegionId {
        self.caller_region
    }

    pub(super) fn run(self, region: &mut RegionState, result: R) {
        (self.apply)(region, result);
    }
}
```

With `pub(super)`, only the parent runtime module can execute the continuation.
Neighbor processing code outside that module can pass it around but cannot run
it.

## Determinism Notes

This design does not make cross-region message arrival globally deterministic.
It only preserves local region ownership and local event order.

To keep each region predictable:

- process each region's inbox in a stable order
- run local systems in a fixed order
- execute returned continuations as normal region events
- apply imported resource state at clear event or tick boundaries

## First Tests To Add

When this becomes code, the first tests should verify:

- Region B can process a request from Region A without mutating Region A.
- Region B returns the same continuation with the result.
- The worker routes the returned continuation to Region A.
- The continuation runs only when Region A processes its event.
- The continuation is consumed once and cannot run twice.

These tests protect the main design rule: neighbor work may complete elsewhere,
but caller follow-up belongs to the caller region's event loop.
