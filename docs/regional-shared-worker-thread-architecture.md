# Regional Shared Worker Thread Architecture

This document describes the architecture for running more than one region on the
same thread while keeping each region's event loop independent.

This is a design note only. It does not change the current single-region game.

## Core Idea

Region communication should be stable. Worker ownership should be movable.

Neighboring regions should not know which worker thread currently runs a region.
They should only know a stable send handle to the target region's mailbox.

```text
Regions talk to region mailboxes, not to threads.
```

This means a region can move from one worker thread to another without changing
neighbor communication.

## Region Handle

A region handle is the stable communication endpoint other regions can keep.

```rust
pub struct RegionHandle {
    pub region_id: RegionId,
    pub sender: Sender<RegionEvent>,
}
```

Neighboring regions should hold `RegionHandle` values, or a smaller
`RegionPort` with the same meaning.

Important naming point:

```text
Neighboring regions should hold a sender, not the receiver.
```

The receiver belongs to the target region runtime. The sender can be cloned and
shared with neighboring regions.

## Region Runtime

Each region runtime owns one region's state and event receiver.

```rust
pub struct RegionRuntime {
    id: RegionId,
    state: RegionState,
    receiver: Receiver<RegionEvent>,
    neighbors: Vec<RegionHandle>,
}
```

The runtime processes its own event loop:

```rust
impl RegionRuntime {
    pub fn process_some_events(&mut self, max_events: usize) -> Vec<OutboundMessage> {
        let mut outbound = Vec::new();

        for _ in 0..max_events {
            let Some(event) = self.try_recv_event() else {
                break;
            };

            outbound.extend(self.process_event(event));
        }

        outbound
    }

    fn process_event(&mut self, event: RegionEvent) -> Vec<OutboundMessage> {
        match event {
            RegionEvent::Tick => self.run_tick(),
            RegionEvent::ProcessImportedOffer(request) => {
                vec![self.process_imported_offer(request)]
            }
            RegionEvent::RunImportedOfferReply { reply, result } => {
                reply.run(&mut self.state, result);
                Vec::new()
            }
        }
    }
}
```

The region state and receiver move together if the region is reassigned to a
different worker.

## Direct Neighbor Send

Region A can send an event directly to Region B's stable mailbox.

```rust
impl RegionRuntime {
    pub fn send_to_neighbor(
        &self,
        neighbor: &RegionHandle,
        event: RegionEvent,
    ) -> Result<(), SendError<RegionEvent>> {
        neighbor.sender.send(event)
    }
}
```

Region A does not need to know where Region B is currently running.

```text
Region A knows:
  Region B's RegionHandle

Region A does not know:
  Region B's worker thread
```

## Worker Thread

A worker thread owns and schedules multiple region runtimes.

```rust
pub struct RegionWorker {
    id: WorkerId,
    regions: Vec<RegionRuntime>,
}
```

The worker loop gives each region a small amount of work. This keeps one busy
region from starving the others on the same thread.

```rust
impl RegionWorker {
    pub fn run_once(&mut self) {
        for region in &mut self.regions {
            let outbound = region.process_some_events(MAX_EVENTS_PER_REGION);
            self.handle_outbound(outbound);
        }
    }
}
```

For a first version, `handle_outbound` can send directly through region handles
or local worker queues. It should not inspect or mutate another region's state.

## Example Layout

One worker thread can run many independent region event loops:

```text
Worker 1 thread
  Region A runtime
    owns Region A state
    owns Region A receiver
    has send handles to B and C

  Region B runtime
    owns Region B state
    owns Region B receiver
    has send handles to A and D

Worker 2 thread
  Region C runtime
    owns Region C state
    owns Region C receiver
    has send handles to A and D

  Region D runtime
    owns Region D state
    owns Region D receiver
    has send handles to B and C
```

Region A sends to Region B like this:

```rust
neighbor_b.sender.send(RegionEvent::ProcessImportedOffer(request))?;
```

This works whether Region B currently runs on Worker 1 or Worker 2.

## Moving A Region Between Workers

Because the sender is stable and the receiver is owned by `RegionRuntime`, load
balancing can move the whole runtime.

```text
Before:
  Worker 1 owns Region B runtime

Move:
  remove Region B runtime from Worker 1
  push Region B runtime into Worker 2

After:
  Worker 2 owns Region B runtime
```

Existing sender handles remain valid:

```text
Region A -> Region B sender
Region C -> Region B sender
Region D -> Region B sender
```

No neighbor port needs to change.

Rust-like pseudocode:

```rust
impl RegionWorker {
    pub fn remove_region(&mut self, region_id: RegionId) -> Option<RegionRuntime> {
        let index = self
            .regions
            .iter()
            .position(|region| region.id == region_id)?;

        Some(self.regions.remove(index))
    }

    pub fn add_region(&mut self, region: RegionRuntime) {
        self.regions.push(region);
    }
}
```

The move must happen at a safe point where only one worker owns and polls that
runtime.

## Load Manager

Dynamic load balancing can be handled by a small load manager. This is not a
message coordinator. It does not route normal region-to-region events.

```rust
pub struct LoadManager {
    workers: Vec<WorkerHandle>,
}
```

The load manager observes worker pressure and decides when to move a region.

```rust
pub struct WorkerLoad {
    pub worker_id: WorkerId,
    pub region_count: usize,
    pub queued_events: usize,
    pub last_frame_time_ms: u32,
}
```

Example policy for a later version:

```rust
impl LoadManager {
    pub fn choose_region_move(&self, loads: &[WorkerLoad]) -> Option<RegionMove> {
        let busiest = loads.iter().max_by_key(|load| load.queued_events)?;
        let quietest = loads.iter().min_by_key(|load| load.queued_events)?;

        if busiest.queued_events < quietest.queued_events + MOVE_THRESHOLD {
            return None;
        }

        Some(RegionMove {
            region_id: self.pick_region_from_worker(busiest.worker_id)?,
            from_worker: busiest.worker_id,
            to_worker: quietest.worker_id,
        })
    }
}
```

For the first implementation, dynamic movement should wait. Start with static
assignment so the event model is easy to test.

```rust
fn assign_worker(region_index: usize, worker_count: usize) -> WorkerId {
    WorkerId(region_index % worker_count)
}
```

## Architecture Summary

```text
Stable RegionHandle
  region_id + Sender<RegionEvent>

RegionRuntime
  owns region state
  owns Receiver<RegionEvent>
  owns event loop
  has neighbor RegionHandles

RegionWorker
  owns many RegionRuntimes
  runs them fairly on one thread

LoadManager
  optionally moves whole RegionRuntimes between workers
  does not route normal neighbor messages
```

This solves the shared-thread requirement:

```text
One thread can run many regions.
Each region keeps its own event loop.
Regions communicate through stable mailboxes.
Moving a region between workers does not break neighbor communication.
```

## First Tests To Add

When this becomes code, the first tests should verify:

- one worker can process events for multiple regions
- a region can send an event through a neighbor handle
- sending still works after moving a runtime between workers
- only one worker owns a moved runtime at a time
- a busy region does not starve another region on the same worker

These tests protect the main design rule: region mailboxes are stable, while
worker ownership is only a scheduling detail.
