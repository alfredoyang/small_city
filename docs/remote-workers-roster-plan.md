# Remote Workers in the Workplace Roster — Plan

## Goal

A workplace roster (Commercial/Industrial) currently lists **local workers only**
— citizens who live in the same region. But a workplace can be staffed by
**remote** workers who commute in from another region (cross-region job export).
Those workers fill real jobs yet appear nowhere on the workplace's own roster, so
a busy industrial can show *"No local workers"* while every slot is taken.

This plan adds remote workers to the workplace roster.

```text
 Industrial @ region 4, cell (2,6) — 12 jobs, all filled by region-1 commuters
   ┌ Workers at (2,6) ─ 12 worker(s) ──────────────┐
   │ #   Age  Happy  $    Lives at                  │
   │ #1  31   70    $9    region 1 (4,11)           │  ← remote (NEW)
   │ #2  27   64    $7    region 1 (4,12)           │  ← remote (NEW)
   │ …                                              │
   └ local + remote ───────────────────────────────┘
```

## Why this is possible (and why it isn't a producer-side read)

The cross-region job link is recorded on the **consumer** citizen, not on the
producer building. When a region-1 resident takes a remote job at region-4's
industrial, the daily job-export phase writes onto that **region-1 citizen**:

```rust
Citizen.workplace_assignment = Some(WorkplaceAssignment {
    region:   RegionId(4),          // ← the producer region
    position: Position { x: 2, y: 6 },  // ← the producer cell
    salary,
    source:   WorkplaceSource::Remote { slot_id },
});
```

So the worker's identity **and full attributes** (`age`, `morale.actual`,
`money`, `home`) live in region 1's `World`, tagged with exactly which
`(region, position)` they commute to. The producer (region 4) holds only an
opaque slot reservation in the runtime export ledger — a *count*, not a *who*.

Therefore the only way to enumerate remote workers is a **reverse lookup into
the regions where the workers live**, keyed on `(producer_region, position)`:

```text
local workers (today):
   workplace @ region R, entity E
   = citizens in R where source == Local{ E }

remote workers (this plan):
   workplace @ region R, position P
   = citizens in ANY OTHER region where
       assignment.region == R && assignment.position == P && source == Remote{..}
```

The match key is `(region, position)` — both stored on the consumer citizen — so
no shared entity id is needed across regions.

## Architecture: a lighter cross-region read

`inspect_region` is a **synchronous direct read** under the runner's
`operation_lock` (`regional_game_runner.rs` → `worker.region_mut(id)`), **not** a
mailbox event. The new query rides that same synchronous path, so it needs **no
new tick event, no `TickState`/`RegionEvent` change, and no `UiRequest`/`UiReply`
variant** (those are only the async snapshot path).

The "lighter query" decision (vs a full `inspect_region` fan-out per frame):

- **Frequency:** fetch only when the workplace panel is **opened** (and re-fetch
  on tick while it stays open), never per cursor-move / per frame.
- **Payload:** a dedicated `remote_workers_at(region, pos) -> Vec<CitizenDetailView>`
  that computes only the roster slice — not the whole `InspectView`
  (cell/details/effects/flags/explanations).
- **Per-call work:** a plain citizen scan in M1/M2; add an
  `(target_region, target_pos) -> [worker]` reverse index only if profiling
  demands it (ponytail: skip the index until then).

```text
UI open workplace panel
  └─ RegionalGame.remote_workers_at(producer_region, pos)            [facade]
       └─ RegionalGameRunner.remote_workers_at(...)                  [lock, fan out]
            └─ each ThreadedRegionWorker: "remote_workers_at" command [thread boundary]
                 └─ RegionWorker iterates its regions (skip producer) [worker]
                      └─ RegionRuntime.remote_workers_for(prod, pos)  [ensure derived, read]
                           └─ RegionState: citizens where
                                assignment.region==prod && position==pos && Remote
                                → project to CitizenDetailView         [core + reuse adapter]
  └─ merge into the workplace roster, render (region-tagged)         [UI]
```

## Layered scope

`(+)` = added/changed. The change is read-only and spans **all five layers**, but
touches **no** simulation, balance, tick, or export-protocol code.

```text
┌─ src/core  (SIMULATION) ───────────────────────────────────────────────────┐
│  Citizen.workplace_assignment already carries { region, position, Remote }  │
│  (+) RegionState::remote_workers_for(region, pos) -> Vec<CitizenDetailView> │
│      filter citizens (assignment.region==R && position==P && Remote), project│
│  (+) RegionRuntime accessor: ensure derived state, then read (mirrors inspect)│
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/interface  (ECS→VIEW BOUNDARY) ────────────▼───────────────────────────┐
│  (+) CitizenRelation::LivesAt gains `region: Option<RegionId>`              │
│      (None = inspected/local region; Some(r) = remote commuter's home).     │
│      The bare World is region-agnostic and cannot name "this region", so a  │
│      non-optional id would force threading a RegionId through inspect_world.│
│  (+) adapter projection reused for a remote worker (home in its own region)  │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │  synchronous read path
┌─ src/core/regions  (THREADING) ─────────────────▼───────────────────────────┐
│  (+) RegionWorker::remote_workers_at(prod, pos): iterate owned regions       │
│  (+) ThreadedRegionWorker command (mirrors inspect_region)                   │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/core/regional_game{,_runner}.rs  (FACADE) ─▼───────────────────────────┐
│  (+) Runner.remote_workers_at: lock, fan out to ALL workers, merge          │
│  (+) RegionalGame.remote_workers_at(region, pos) -> Vec<CitizenDetailView>  │
└───────────────────────────────────────────────┬───────────────────────────┘
                                                  │
┌─ src/ui/tui.rs  (FRONTEND) ─────────────────────▼───────────────────────────┐
│  (+) on panel open: fetch + cache remote workers in TuiState (not per frame) │
│  (+) render them under the local workers, region-tagged                      │
└────────────────────────────────────────────────────────────────────────────┘
```

## Mission boundary

~9 files total — past the 5-file / ~400-line single-patch cap — so split into
three one-patch missions, each green through the dev loop before the next.

### M1 — core read + view model (core/interface)

- `view.rs`: `CitizenRelation::LivesAt { x, y }` → `LivesAt { region: Option<RegionId>, x, y }`.
  Update every existing roster test/literal (same churn as the original roster
  feature). `None` = a *local* worker (lives in the inspected region — the bare
  region-agnostic World cannot name itself); `Some(r)` = a remote commuter whose
  home is in region `r`.
- `regions/mod.rs` (`RegionState`): `remote_workers_for(producer_region, pos)`
  — filter `citizens` by `assignment.region == producer_region && position == pos
  && matches!(source, Remote{..})`, project each to `CitizenDetailView` (reuse the
  adapter's citizen mapping), deterministic order by `Entity.0`.
- Tests: a hand-built two-region `World`/`RegionState` (no threading) — a citizen
  with a `Remote` assignment to `(R, P)` is returned for that key, local workers
  and non-matching cells are excluded, order is stable.

Size: ~2–3 files + tests. UI and threading untouched.

### M2 — cross-region fan-out (regions/facade)

- `regions/runtime/mod.rs`: `RegionRuntime::remote_workers_for(prod, pos)` —
  ensure derived state (like inspect), then call the M1 `RegionState` read.
- `worker.rs`: `RegionWorker::remote_workers_at(prod, pos)` — iterate
  `self.regions`, skip the producer region, concatenate matches.
- `threaded.rs`: new worker command + response (mirror `inspect_region`).
- `regional_game_runner.rs`: `remote_workers_at` — take `operation_lock`, fan out
  to **all** workers, merge (deterministic region/entity order).
- `regional_game.rs`: public `remote_workers_at(region, pos)`.
- Tests: a real multi-region game with a `city1`-style remote-commuter setup —
  residents in region A working remotely at region B's workplace are returned by
  `remote_workers_at(B, pos)`; producer-region citizens are not double-counted.

Size: ~5 files + tests. No UI.

### M3 — TUI render (ui)

- `tui.rs`: when the panel opens on a workplace, call `remote_workers_at`, cache
  the result in `TuiState` (so it is not refetched per frame; refresh on tick
  while open), and render remote workers under the local ones — region-tagged via
  the new `LivesAt { region, .. }`, e.g. `region 1 (4,11)`.
- The empty-state and footnote adjust: a workplace with only remote workers shows
  them instead of "No citizens yet"; the "(local workers only)" footnote is
  dropped or reworded now that remote workers are listed.
- Tests: render a workplace roster mixing a local and a remote worker; assert both
  appear and the remote one carries its home-region tag.

Size: 1 file + tests.

## Risks / notes

- **Cross-worker locking (medium):** `inspect_region` hits one worker; this hits
  all of them on panel open. Follow the existing command-channel pattern; bounded
  by "only on open."
- **View-model ripple (low-med):** adding `region` to `LivesAt` touches every
  existing roster test/literal.
- **Cost (low):** O(citizens in other regions at that cell), only on open. Add an
  `outbound_remote` reverse index only if profiling says so.
- **Determinism:** intact — a pure function of each region's state in a fixed
  (region, entity) order. Cross-region reads are **one-tick-stale**, consistent
  with the documented model (within-tick synchronicity is the only thing relaxed
  across regions).
- **No simulation/balance/tick/export-protocol change.** The producer export
  ledger still holds only a count; identities come from the consumer regions where
  the citizens already live.

## Open question (decided)

How to present a mixed roster: a single table tagging each row local/remote, or
two sections? **Resolved: a single table.** Local workers render first, then the
remote commuters; the `Lives at` column shows `(x,y)` for a local worker and
`region N (x,y)` for a remote one. The bottom footnote (`N local · M remote`)
gives the breakdown. This was the lightest option and reuses the existing column
model unchanged.

---

## Implemented architecture (M1–M3)

Shipped in three patches: `e87646d` (M1), `6a90205` (M2), `2e43f13` (M3).

### End-to-end read path

```text
TUI: Enter on a Commercial/Industrial cell  (or a tick while the panel is open)
  │   (residential / other cells: skip — no fan-out)
  ▼
CityDriver::remote_workers_at(x,y)                                  [src/ui]
  ▼
RegionalGame::remote_workers_at_selected_region → remote_workers_at(region,x,y)
  ▼
RegionalGameRunner::remote_workers_at(producer_region, pos)         [FACADE]
  │  • take operation_lock
  │  • validate producer_region exists (parity with inspect_region)
  │  • FAN OUT to EVERY ThreadedRegionWorker (commuters may live on any worker)
  │  • STABLE-sort merged list by home region id  ──► deterministic order
  ▼
ThreadedRegionWorker::remote_workers_at  ── RemoteWorkersAt cmd ──► worker thread
  ▼
RegionWorker::remote_workers_at(producer_region, pos)              [WORKER]
  │  • iterate owned regions, SKIP producer (its workers there are Local)
  │  • concatenate per-region results (each Entity.0-ordered, contiguous)
  ▼
RegionRuntime::remote_workers_for  (ensure_derived_state, mirrors inspect)
  ▼
RegionState::remote_workers_for(producer_region, pos)              [CORE]
  ▼
adapter::remote_workers_for(world, home_region, producer_region, pos)
       scan THIS region's citizens:
         assignment.region == producer_region
         && assignment.position == pos
         && source == Remote
       → project to CitizenDetailView { LivesAt { region: Some(home_region) } }
```

### Why the producer can't answer for itself

```text
       region 1 (consumer)                    region 4 (producer)
  ┌──────────────────────────┐          ┌──────────────────────────┐
  │ Citizen #7                │          │ Industrial @ (2,6)        │
  │  home (4,11)              │          │  jobs: 12                 │
  │  workplace_assignment ────┼────────► │  export ledger: count=12  │
  │   { region: 4,            │  reverse │   (opaque slots, NO who)  │
  │     position: (2,6),      │  lookup  │                           │
  │     Remote { slot 3 } }   │ ◄────────┤  asks every OTHER region: │
  └──────────────────────────┘          │  "who points at (4,(2,6))?"│
        identity + attributes lives here └──────────────────────────┘
```

The match key `(region, position)` is recorded only on the **consumer** citizen,
so the roster is assembled by scanning consumer regions — never by reading the
producer's ledger (which holds a count, not identities).

### Determinism of the cross-worker merge

```text
worker A owns regions [1, 3]   worker B owns region [2]
   1: [c1, c4]  (Entity.0)         2: [c9]
   3: [c2]
fan-out (any worker order) → flat list, each region's run CONTIGUOUS
   e.g. [c1,c4, c2,  c9]   (regions 1,3 then 2)
stable sort by home-region id (key = LivesAt.region) keeps within-region order
   → [c1,c4 (r1),  c9 (r2),  c2 (r3)]   deterministic regardless of layout
```

Each region is owned by exactly one worker, so its workers form one contiguous,
already-`Entity.0`-ordered run; a *stable* sort by region id therefore yields a
fixed `(region, entity)` order no matter how regions map to workers. Cross-region
reads remain one-tick-stale by design — only within-tick synchronicity is relaxed.

### TUI render (single combined table)

```text
┌ Workers at (2,6) — 13 worker(s) · ↑/↓ · Esc close ──┐
│ #   Age  Happy  $    Lives at                       │
│ #1  29   60     $5   (3,7)               ← local    │
│>#2  31   70     $9   region 1 (4,11)     ← remote   │
│ …                                                   │
│ 1 local · 12 remote                                 │
└─────────────────────────────────────────────────────┘
```

`citizen_remote` is cached in `TuiState`, filled on panel open and refreshed on
tick (never per frame). The panel is modal, so the cursor cannot move to another
cell while it is open and the cache stays consistent with the inspected cell.

### Follow-up fix — multi-cell footprints (`463a363`)

The position-keyed match broke for **multi-cell** workplaces: a building grown to
e.g. 4×4 is one entity with a single anchor `Position`, and each commuter's
assignment records that anchor. Matching `assignment.position == clicked(x,y)`
therefore lit up only the anchor cell; the other footprint cells showed an empty
roster. (Local workers were unaffected — they match by *entity*, which
`set_footprint` maps across every cell.)

Fix: normalize the clicked cell to the anchor before the fan-out.

```text
grid:  (1,2)->E  (2,2)->E        positions: E -> (1,2)   (anchor)
click (2,2) ─► grid.get ─► E ─► positions.get ─► anchor (1,2)
                                      │
                                      ▼  fan out scan with the ANCHOR, not (2,2)
   assignment.position == (1,2) == anchor   ► matches on every footprint cell
```

`RegionState::workplace_anchor_at(x,y) = positions.get(grid.get(x,y))` resolves any
footprint cell to the current anchor; the runner reads it from the producer
region's worker (new `WorkplaceAnchorAt` command) before fanning out the scan with
the anchor. Producer anchor and consumer `assignment.position` both derive from
`positions.get(entity)`, so they agree regardless of the direction the building
grew. An empty cell (no anchor) short-circuits to an empty roster.
