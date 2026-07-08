# Retire TickState — settle each tick with whatever's already held

Status: **P-a, P-b, P-c, P-d, and P-e implemented** (Power, the eager
nudge, Jobs, Goods, and the now-dead `TickState` cleanup — see
"P-a, implemented", "P-b, implemented", "P-c, implemented", and
"P-d/P-e, implemented" at the end of this doc), P-f still plan-only. Reviewed three
times by codex before implementation. First pass (core tick-retirement;
P-a/P-c/P-d/P-e): 2 High + 2 Medium. Second pass (eager nudge P-b +
self-describing-reply redesign): 2 High — the denial-cleanup guard and the
per-worker id partition (item 4/6). Third pass: 1 High — a stale *granted*
reply must actively release the producer's stuck reservation, not just
drop (item 4); 1 Low — worker-id bit layout overlap (item 6). All findings
fixed here; see the "caught in review" callouts. P-a's own implementation
was then reviewed twice more by codex (one High: a power reply could
mutate state mid-tick while paused for jobs/goods — fixed, see P-a's
write-up). P-b's implementation was reviewed twice more (one Low: use a
real `assert!`/`checked_add` instead of `debug_assert!`/bare `+=` for the
worker-id scheme so release builds stay protected too — fixed, see P-b's
write-up). P-c's implementation was reviewed three times more (one High
found *during* implementation, not by codex — a real gameplay regression
in the daily job-wipe cadence, fixed by the user's own direction before
codex ever saw the patch; then one further High from codex on that same
fix's own edge case — fixed; see P-c's write-up, it's the most involved
of the three).

Both earlier plans this session made the tick *cheap* (`P-1..P-6`) and, in
an abandoned attempt, *faster to react*. Neither touched the one real piece
of complexity left: a tick that needs a cross-region reply **stops and
waits** for it. This plan removes the waiting.

## The problem

```text
 Tick asks for power it doesn't have yet
   → PARKS in TickState::WaitingForPowerExports
   → remembers what it's owed in a continuation struct
   → a second Tick that shows up now has to wait its turn
   → reply arrives → resume → chain into jobs → maybe park again → ...
```

Everything below exists *only* to make that parking possible:
`TickState`'s 4 waiting variants, the `is_waiting()` machinery, a special
"which events may jump the queue while paused" filter, and "was that the
last reply? then chain to the next phase" logic in three grant handlers.
None of it is wrong — it's just a lot of machinery whose one job is "let a
tick wait." If nothing waits, it all goes — continuation structs included:
the reply is made to carry its own context back, so there's nothing left to
remember between asking and hearing the answer (see the pseudocode).

## The fix, mentally

**A tick always finishes in one pass**, using whatever power/jobs/goods it
already holds from last time. It still fires off release/request messages
for what it needs *next* — just doesn't block on the reply. Whenever a
reply lands, it's written straight into world state for the *next* tick to
pick up.

Not a new idea here — goods already work exactly this way: a granted
import is stashed in `pending_goods_stock`, applied at the *start* of the
next goods phase, not the one that asked. And the code already argues this
is fine for the producer side of job tax ("settlement can lag by one daily
tick, by design... deterministic and self-correcting"). This plan just
applies the same idea to the consumer side, and to the whole tick.

```text
 TODAY — one Tick, several possible stops              AFTER — one Tick, one pass, always
 ───────────────────────────────────────              ───────────────────────────────────
 Tick → power  → [pause? WaitingForPowerExports]       Tick → power (use what's held)
      → resume → jobs   → [pause? WaitingForJob…]           → jobs  (use what's held)
      → resume → goods  → [pause? WaitingForGoods…]         → goods (use what's held)
      → resume → finish                                     → finish        ← same pass, always

                                                        separately, fire-and-forget:
                                                        release stale + request what's next
                                                          → reply lands whenever → written to
                                                            world state → ready for NEXT tick
```

```text
 A grant, before and after
 ──────────────────────────
 TODAY:  send "#12?"        → PAUSE, remember what #12 meant in a continuation
         reply "#12: yes"   → look up #12 in the continuation → apply
                            → last one? chain forward. else keep waiting.

 AFTER:  send the question  → move on NOW — remember nothing;
                              the reply will bring its own context back
         reply (whenever)   → echoes the full request it answers
                              ("batch N, House-1, 5 units: yes")
                            → my current batch?  apply it — everything
                                                 needed is in the reply
                            → an old batch?      stale, drop it
```

## What changes

```text
              DELETE                            KEEP                         ADD
 ──────────  ───────────────────────────────    ─────────────────────────    ──────────────────────
 State        TickState's 4 waiting variants     —  (nothing to keep:         one scalar per
              AND all 4 continuation structs —      the reply carries its     resource — the
              the reply now describes itself,       own context back)         current batch's
              so nothing needs parking                                        request_id
 Dispatch     pop_next_runnable_event's          plain FIFO always            —
              mid-wait allow-list
 Grant        "last reply? chain to next         the ECS write itself         one guard in the
 handlers     phase" branch                      (apply_power_export_grant,   denial branch —
                                                  etc.)                        see item 4
 Tick shape   the phase-by-phase pause/           local order power→jobs→     one straight-through
              resume chain                       goods→economy (DT4,         run_tick, no pauses
                                                  unchanged)
 Producer     —                                   completely untouched —     —
 side                                              it only ever answers,
                                                    never waits
 Discovery    —                                    the poll/gate, unchanged  PowerCapacityRecheck:
 (power)                                         (still the backstop)     fan out on hint
                                                                           republish, so a
                                                                           neighbor can ask
                                                                           right away instead
                                                                           of waiting for its
                                                                           own next tick
```

## The cost, plainly

**A grant now pays out starting next tick, not the tick that asked.** The
tick that asks finishes still using what it already had; the *next* tick
sees the reply. One tick behind — for power ~1 hour, for jobs/goods a full
day (they only settle daily anyway). Real, felt gameplay change; needs an
explicit yes, not a buried diff.

That's the **worst case**. For power, the eager nudge (next section) shrinks
it to "usually unnoticeable" — the neighbor is nudged the instant the change
happens, so its request is usually answered before its next tick even fires.
Jobs/goods get no such nudge and stay one daily tick behind by design (the
daily cadence is the point — see "The eager nudge" for why nudging them buys
nothing).

**Determinism is unaffected.** Same barrier-sorted delivery, same
event-log-in/state-out reproducibility — only *which tick* a reply lands in
shifts, never whether the result repeats.

## The eager nudge

A region only asks for power when its own tick's gate notices something
changed — that's the one-tick lag above. The gate itself is already a hard
guarantee: every tick checks `discovery_generation > seen_power_generation`
directly off the shared snapshot, no matter what else happened. **This
plan also nudges neighbors the instant a hint republishes** (already
immediate — P-1), instead of making them wait for their own next tick to
notice.

```text
 two clocks:        Tick(H) ───────────────────── Tick(H+1)      ← slow, external
                     pass·pass·pass·pass·pass·pass·pass·pass·pass  ← fast, nonstop

 common case:  hint updates → nudge → ask → grant lands
               ├── a handful of fast passes ──┤  ← usually done
               well before Tick(H+1) even fires

 worst case:   nudge too slow/lost — doesn't matter:
               Tick(H+1)'s gate checks anyway → notices → asks on its own
               Tick(H+2) → that request had a whole tick to round-trip → done
```

**Why this is safe, not just faster:** the gate never depends on the
nudge. Worst case stays exactly what this plan already promised — one
tick behind whichever tick first asks. The nudge only moves *when* that
first ask happens: usually right away instead of only guaranteed by next
tick. Better common case, same worst case, never worse.

**Why power only.** The nudge shrinks the gap between "change happened" and
"neighbor asks." That only helps if asking sooner lets the answer land
sooner — true for power (hourly). Jobs and goods settle on a *daily*
boundary by design (deliberate hysteresis — a citizen shouldn't rethink its
job every hour). A job/goods nudge mid-day has nothing to do: the demand
list is only recomputed at the daily boundary, and the answer couldn't be
applied before then anyway. So the nudge would add cost for zero latency
win. Power is the only resource where it pays off.

**The cost of adding it:** a new fire-and-forget event, and its own fresh
request id minted by the worker (this fan-out isn't triggered by any UI
request — see the worker id counter in the pseudocode). It fans out
coarsely (whole connected component, not just actual importers); see Risks
for why that's a safe, deliberate choice.

## Pseudocode

**1. Make the reply carry its own context — then there is nothing to
remember.** A grant today echoes only its `token`; the caller must keep a
list of what each token *meant*. That list (and a counter to keep tokens
unique) was this plan's original design — superseded: the worker already
holds the full original request at the moment it routes the result back
(`route_export_request_result` has it in hand and discards it, forwarding
only the bare grant). Forward the request *with* the grant instead, and the
caller-side memory disappears — the message *is* the memory:

```text
 TODAY:  reply = "#12: granted"
           → caller: "what was #12 about?" → must have written it down

 AFTER:  reply = "batch N, House-1, 5 units: granted, from region C"
           → apply directly — nothing to look up, nothing was written down
```

```rust
// Requests gain the ONE field needed to re-derive the demand on arrival:
pub struct PowerExportRequest {
    pub request_id: UiRequestId,   // already there — becomes the staleness check
    pub caller_region: RegionId,   // already there
    pub caller_network: RegionRoadNetworkId, // already there
    pub token: u32,                // already there — stays a batch position
    pub demand: i32,               // already there
    pub consumer: Entity,          // NEW — jobs echo the citizen instead,
}                                   //       goods echo the commercial building

// The caller-side apply event carries request + grant, not the grant alone
// (ExportResource::apply_grant_event gains the request parameter; the worker
//  already holds it at route_export_request_result — zero new plumbing):
RegionEvent::ApplyPowerExportGrant { request: PowerExportRequest, grant: PowerExportGrant }
```

**2. One scalar per resource replaces the continuation: "which batch is
current."** All four continuation structs are deleted outright. What
survives is the one field we originally *trimmed away* — `request_id`:

```rust
// RegionRuntime — the ONLY caller-side bookkeeping left, per resource:
current_power_request_id: UiRequestId,
// current_job_request_id / current_goods_request_id: same.
```

Why the floor is one scalar, not zero: a producer reserves capacity the
moment it grants. When the caller starts a new batch, its release wipes the
old batch's reservations — so a *late* grant from that old batch, if
applied, would mark a consumer powered with **no reservation backing it
anywhere**, and the quiet gate would never look at it again. Something must
distinguish "my current batch" from "an old one." The reply already carries
`request_id` (per item 1), so one comparison does it:

```text
 batch N (request_id 7):  tokens 0,1,2     batch N+1 (request_id 9):  tokens 0,1
                                            ▲ same token numbers — fine now!
 late reply for (id 7, token 0) arrives:
   7 ≠ current (9) → dropped before any token is even looked at ✓
```

Tokens stay exactly what they are today — batch-local positions — because
their only remaining job is telling demands apart *within* one batch. This
mirrors the producer's existing semantics precisely: reservations are
already keyed `(caller_region, request_id, token)`, and
`release_stale_for_caller` already treats `request_id` as the staleness
generation. Caller and producer now speak the same language. (An earlier
draft solved the cross-batch token collision with a never-resetting token
counter — caught in review as a real bug in the batch-position scheme; the
request_id comparison supersedes that fix by dropping whole stale batches
at once.)

**3. Release + request, no pausing** — stamp the new generation, send, done:

```rust
fn release_and_request_power(
    &mut self,
    request_id: UiRequestId,
    demands: &[PendingPowerDemand],
) -> Vec<OutboundMessage> {
    self.current_power_request_id = request_id; // everything older is now stale
    let producer_regions = std::mem::take(&mut self.power_export_producers);
    let mut outbound = vec![OutboundMessage::PowerExportAllocationsReleased(
        PowerExportAllocationRelease { caller_region: self.region_id(), request_id, producer_regions }
    )];
    outbound.extend(demands.iter().map(|demand| {
        OutboundMessage::PowerExportRequested(PowerExportRequest {
            request_id,
            caller_region: self.region_id(),
            caller_network: demand.caller_network,
            token: demand.token,       // batch position, unchanged from today
            demand: demand.demand,
            consumer: demand.consumer, // NEW: echoed back by the reply
        })
    }));
    outbound
}
// release_and_request_job / release_and_request_goods: identical shape,
// own current_*_request_id each, swap the message/demand types.
```

**4. Grant handlers — staleness check, then apply. No matching, no
chaining. A stale *granted* reply must actively release, not just drop:**

```rust
RegionEvent::ApplyPowerExportGrant { request, grant } => {
    self.remember_power_export_producer(&grant);
    if request.request_id != self.current_power_request_id {
        // Superseded batch — but if it was GRANTED, the producer reserved
        // capacity we will never apply, and our next real release may not
        // reach that producer (see below). Clear it NOW, keyed to the
        // current generation so the producer drops this old reservation and
        // keeps any current one.
        return release_stale_granted_power(self.region_id(),
                                           self.current_power_request_id, &grant);
    }
    let demand = PendingPowerDemand {
        token: request.token,
        consumer: request.consumer,
        demand: request.demand,
        caller_network: request.caller_network,
    };
    self.state.apply_power_export_grant(demand, grant); // ECS write — ONE
    Vec::new()                                          // guard added, below
}

RegionEvent::ApplyJobExportGrant { request, grant } => {
    self.remember_job_export_producer(&grant);
    if request.request_id != self.current_job_request_id {
        return release_stale_granted_job(self.region_id(),
                                         self.current_job_request_id, &grant);
    }
    self.state.apply_job_export_grant(demand_from(&request), grant); // unchanged ECS write
    Vec::new()
}

RegionEvent::ApplyGoodsExportGrant { request, grant } => {
    self.remember_goods_export_producer(&grant);
    if request.request_id != self.current_goods_request_id {
        return release_stale_granted_goods(self.region_id(),
                                           self.current_goods_request_id, &grant);
    }
    if grant.granted && grant.units > 0 {
        // UNCHANGED: goods already stage into pending_goods_stock,
        // applied at the next goods phase. Power/jobs now match this.
        self.pending_goods_stock.push((request.commercial, grant.units));
    }
    Vec::new()
}

// Same shape for all three (helper per resource, or one generic over the
// release message type): a granted-but-stale reply emits a targeted release
// to just that producer, stamped with the CURRENT generation.
fn release_stale_granted_power(
    caller: RegionId, current: UiRequestId, grant: &PowerExportGrant,
) -> Vec<OutboundMessage> {
    match grant.source_region {
        Some(producer) if grant.granted => {
            vec![OutboundMessage::PowerExportAllocationsReleased(PowerExportAllocationRelease {
                caller_region: caller,
                request_id: current,       // release_stale_for_caller drops
                producer_regions: vec![producer], // the old generation, keeps current
            })]
        }
        _ => Vec::new(), // a denial reserved nothing — nothing to release
    }
}
```

**Caught in review — why the stale-granted reply must actively release.**
Today `remember_*_export_producer` runs before the staleness check on the
theory that "the next release will reach that producer and clear it." That
holds under pausing: batch N+1 can't start until batch N settled, so N's
grant is always in `power_export_producers` when N+1's release fires.
Unpaused, it's a plain race:

```text
 batch N   → request to producer P
 batch N+1 → release (P not in the list yet!) + new request   ← supersedes N
 batch N's grant arrives late → remembered, but request_id stale → DROPPED
 region goes quiet → no next release → P's reservation for N stuck forever
```

The targeted release above closes it: the instant a stale *granted* reply
lands, fire a release to just that producer with the current generation —
`release_stale_for_caller` drops the old generation and keeps any current
one (`runtime/mod.rs:484`). A stale *denial* reserved nothing, so it needs
no release. This is the one place the old "remember, clear next time"
assumption doesn't survive un-pausing.

**Caught in review — one required change *inside* the power ECS write.**
`apply_power_export_grant`'s denial branch assumes any still-powered
consumer can only be the optimistic restore, and clears it unconditionally
(`regions/mod.rs:1197`). That assumption is *made true today by pausing* —
nothing can run between request and reply. Once nothing pauses, a later
tick or the nudge can give that consumer **local** power before a
same-batch denial lands; the unguarded clear would wipe real local power
and subtract it from the supplied stats. One guard fixes it — a denial
may only undo an *imported* source, because local power is never the
optimistic restore's doing:

```rust
// RegionState::apply_power_export_grant, denial branch
// was: if consumer.powered {
if consumer.powered && matches!(consumer.source, Some(PowerSource::Imported { .. })) {
    /* clear + stats rollback — unchanged */
}
```

The job and goods ECS writes have no such denial cleanup — they really are
unchanged.

**5. The nudge itself — a fire-and-forget event, modeled on
`RegionEvent::ReceiveTraveler`** (no grant, no tick pause, no reply):

```rust
RegionEvent::PowerCapacityRecheck { request_id, .. } => {
    let demands = self.state.power_demand_recheck(); // time-neutral, below
    self.release_and_request_power(request_id, &demands) // same helper as run_tick
}
```

Unlike a normal tick, this must **not** advance the game clock — a random
cross-region nudge shouldn't tick the hour along as a side effect. So it
needs its own time-neutral way to collect fresh demand, instead of
`begin_tick_power_demand_phase` (which does, via `begin_tick_power_phase`):

```rust
// regions/mod.rs, RegionState — new:
pub(crate) fn power_demand_recheck(&mut self) -> Vec<PendingPowerDemand> {
    ensure_derived_state(&mut self.world, self.id); // catch up any pending
                                                      // config change, no time advance
    let imported = imported_power_grants(&self.world);
    clear_imported_power(&mut self.world, &imported);
    power::run(&mut self.world); // NOT begin_tick_power_phase — no advance_hours
    let power_demands = self.pending_power_demands();
    let requestable: HashSet<Entity> = power_demands.iter().map(|d| d.consumer).collect();
    let restorable = imported.into_iter()
        .filter(|(e, _, _)| requestable.contains(e))
        .collect::<Vec<_>>();
    reapply_imported_power(&mut self.world, &restorable);
    power_demands
}
```

**6. The worker-level fan-out**, right after the existing P-1 hint-publish
sweep — gated on `publish_region`'s own idempotence check (only fans out on
a *real* change), minting a fresh id per republish so the request doesn't
collide with anything a UI-driven tick already assigned:

```rust
for (region_id, links, hints) in &changed_summaries {
    let republished = self.directory.publish_region(region_id, links.clone(), hints.clone());
    if !republished { continue; } // nothing actually changed — nothing to nudge
    let recheck_id = self.next_worker_request_id(); // see below
    let discovery = self.directory.discovery_snapshot();
    let mut notified = HashSet::new();
    for hint in &hints {
        let Some(component) = discovery.component_of(hint.network) else { continue };
        for network in component {
            if network.region == *region_id || !notified.insert(network.region) {
                continue; // skip self and duplicates
            }
            let order_key = ForwardedEventOrderKey {
                target_region: network.region, source_region: *region_id,
                request_id: recheck_id, token: hint.network.road_network,
                resource_rank: 0, event_rank: 3, // after release/request/reply
            };
            if let Ok(WorkerRoutedMessage::Forwarded(event)) = self.route_region_event(
                network.region, *region_id,
                RegionEvent::PowerCapacityRecheck { request_id: recheck_id, source_region: *region_id },
                order_key, routing_mode,
            ) {
                forwarded_events.push(event);
            }
        }
    }
}
```

**Why the worker needs its own id counter.** This fan-out isn't triggered
by any UI request, so there's no `UiRequestId` to borrow — but the
resulting requests still need a fresh generation (both the producer's
`release_stale_for_caller` and the caller's own
`current_power_request_id` staleness check depend on batch ids actually
changing between batches). Caught in review: "top bit set" alone isn't
enough — a multi-worker game has several `RegionWorker`s, and two workers
each running their own counter *can* mint the same id and nudge the same
target with it, defeating both staleness checks. So the id must encode
*who* minted it. Every worker already knows its `WorkerId` (a `u32`):

```rust
// RegionWorker — new field: recheck_counter: u32 (starts at 0)
fn next_worker_request_id(&mut self) -> UiRequestId {
    // WorkerId starts at 1 and stays tiny (INITIAL_WORKER_ID = 1, +index);
    // 31 bits is astronomically more than any real deployment, and the
    // assert makes the ceiling explicit rather than a silent wraparound.
    debug_assert!(self.worker_id.0 < (1 << 31));
    self.recheck_counter += 1;
    //   bit 63: "worker-minted"   bits 32..62: which worker (31 bits)   bits 0..31: counter
    UiRequestId((1u64 << 63) | (u64::from(self.worker_id.0) << 32) | u64::from(self.recheck_counter))
}
```

`RegionalGame`'s UI counter starts at 1 and increments per player action —
it never reaches bit 63. Disjoint bit ranges structurally cannot collide:
not UI vs. worker, and (given the assert) not worker vs. worker.

**7. One straight-through tick.** Caught in review: an earlier draft ran
goods every hour, unconditionally — but goods (like jobs) only ever
resolve on a daily boundary; an hourly tick must skip them entirely,
exactly like today's `enter_job_phase` early-return does:

```rust
fn run_tick(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
    let mut outbound = Vec::new();

    // ---- power: always runs, gated release/request same as today ----
    let power_dirty = self.state.is_power_exports_dirty()
        || self.discovery_generation > self.seen_power_generation;
    let power_phase = if power_dirty {
        self.seen_power_generation = self.discovery_generation;
        self.state.clear_power_exports_dirty();
        self.state.begin_tick_power_demand_phase() // P-3's dirty path, unchanged
    } else {
        self.state.begin_tick_power_phase_quiet() // P-6's quiet path, unchanged
    };
    if power_dirty {
        outbound.extend(self.release_and_request_power(request_id, &power_phase.power_demands));
    }

    // ---- jobs + goods: ONLY on a daily boundary, exactly like today ----
    let job_phase = self.state.continue_tick_to_job_demand_phase(power_phase); // always runs
    let result = if !job_phase.is_daily() {
        // hourly: finish right after jobs, exported_goods_units = 0,
        // same as today's finish_tick_after_job_phase — no goods touched
        let exported_job_slots = self.job_export_allocations.units().collect::<Vec<_>>();
        self.state.finish_tick_job_demand_phase(job_phase, &exported_job_slots)
    } else {
        let jobs_dirty = self.state.is_jobs_exports_dirty()
            || self.discovery_generation > self.seen_jobs_generation;
        if jobs_dirty {
            self.seen_jobs_generation = self.discovery_generation;
            self.state.clear_jobs_exports_dirty();
            outbound.extend(self.release_and_request_jobs(request_id, &job_phase.job_demands));
        }

        self.apply_pending_goods_stock(); // last tick's granted goods land now
        let goods_phase = self.state.continue_tick_to_goods_demand_phase(job_phase);
        let goods_dirty = self.state.is_goods_exports_dirty()
            || self.discovery_generation > self.seen_goods_generation;
        if goods_dirty {
            self.seen_goods_generation = self.discovery_generation;
            self.state.clear_goods_exports_dirty();
            outbound.extend(self.release_and_request_goods(request_id, &goods_phase.goods_demands));
        }

        let exported_job_slots = self.job_export_allocations.units().collect::<Vec<_>>();
        let exported_goods_units = self.goods_export_allocations.units().sum();
        self.state.finish_tick_goods_demand_phase(goods_phase, &exported_job_slots, exported_goods_units)
    };

    outbound.push(OutboundMessage::RegionTickCompleted(
        RegionTickResponse { request_id, region_id: self.region_id(), result }
    ));
    outbound.extend(self.drained_traveler_handoff_messages()); // unchanged
    outbound
}

// process_event's Tick arm:
RegionEvent::Tick { request_id } => self.run_tick(request_id),
```

**8. Dispatch collapses to plain FIFO** — nothing waits, so nothing needs
the mid-wait filter:

```rust
fn pop_next_runnable_event(&mut self) -> Option<RegionEvent> {
    self.receiver.pop_event() // TickState::is_waiting() is gone
}
```

**9. `SettlePowerImports`** (load-time re-negotiation) reuses the same
helper — becomes a plain fire-and-forget call:

```rust
fn start_power_import_settlement(&mut self, request_id: UiRequestId) -> Vec<OutboundMessage> {
    let demands = self.state.power_import_settlement_demands(); // unchanged, time-neutral
    self.release_and_request_power(request_id, &demands)
}
```

This only works because `release_and_request_power` doesn't care how the
demands were collected — settlement's collector is time-neutral (calls
`power::run` directly); a normal tick's collector advances time. **Don't
merge the two collectors later** — keeping `release_and_request_power`
demand-agnostic is what keeps that distinction safe.

## Decisions locked

- Determinism unaffected — same barrier-sorted delivery either way; only
  *which tick* a reply lands in shifts, never reproducibility.
- The gates (`*_exports_dirty`, `discovery_generation`/`seen_*_generation`)
  don't change — this plan removes the pause *after* that decision, not
  the decision.
- Goods' `pending_goods_stock` pattern is the template for power and jobs,
  not a new invention.
- **Replies are self-describing** — the apply event carries the original
  request (the worker already holds it when routing the result), so the
  caller keeps no demand list and no token counter. Caller-side state is
  exactly one `request_id` scalar per resource, mirroring the producer's
  existing `(caller_region, request_id, token)` reservation key semantics.
- The one-tick-later gameplay cost gets called out explicitly in review,
  not left implicit in the diff.
- **The eager nudge (`PowerCapacityRecheck`) is part of this plan, not an
  optional extra** — power only (see "The eager nudge" for why jobs/goods
  gain nothing from it). It shrinks the *common* case; it never changes the
  *worst* case, which the gate alone already bounds.

## Risks / notes

- **Biggest re-baselining risk of any plan this session.** Confirmed in
  review: ~9 tests in `runtime/mod.rs`'s `tick_state_tests` assert a
  specific wait state or continuation directly (enter-wait,
  last-grant-resumes, second-tick-deferred, unknown-token-keeps-waiting,
  job wait-state coverage), plus ~4 paused-handshake tests in
  `tests/region_worker_test.rs`. All need real rewrites, not edits —
  expect this to be the largest part of the work, bigger than the code
  change itself.
- **Open question, not a decided removal**: whether the starvation-fix
  capture/clear/restore dance in `begin_tick_power_demand_phase`
  (`docs/20260703-bug-cross-region-export-starvation-fix.md`) can also
  simplify once nothing pauses. Needs its own trace — don't assume it
  falls out for free.
- The mid-wait deadlock-avoidance reasoning (why some events had to jump
  the queue while paused) disappears entirely once nothing pauses — worth
  confirming nothing else still assumes it's needed.
- Perf: strictly less work per tick — no pause bookkeeping, no per-pause
  `TickState` transitions, no deferred second-`Tick` handling, no
  caller-side demand list at all. A simplification in both code size and
  per-tick cost, not a tradeoff.
- **Event-shape ripple**: the three `Apply*ExportGrant` events change from
  carrying a bare grant to `{ request, grant }`, and each request type
  gains one echoed entity field (`consumer`/citizen/commercial). That
  touches the `ExportResource` trait's `apply_grant_event` and every
  constructor of those events — mechanical, but wide.
- **Dead-entity echo must be a no-op**: a reply can echo a consumer that
  was bulldozed after the request went out (same batch, so the staleness
  check passes). `RegionState::apply_power_export_grant` (and the job/goods
  equivalents) must tolerate an entity that no longer exists — verify this
  explicitly in P-a, don't assume it.
- The nudge fans out to a whole connected component, not just actual
  importers — coarse on purpose, same "false positives are free" tradeoff
  every dirty flag in this codebase already makes. A precise,
  holder-only version is a possible later refinement, not required here.

## Patch split

Caught in review: "change the bookkeeping, keep pausing" as a first patch
doesn't work — while pausing exists, something still has to know when the
*last* reply landed to chain phases forward. So reply-shape change and
pause-removal land together, one resource at a time.

```text
 dependency order:

   P-a ──► P-b          (nudge reuses P-a's helper + generation scalar)
    │
    ├──► P-c ─┐
    ├──► P-d ─┼──► P-e ──► P-f
    └─────────┘     (delete enum      (optional; may
                     once nothing       be a no-op)
                     pauses)
```

| patch | does | verify |
|-------|------|--------|
| **P-a** Power | self-describing replies + `current_power_request_id` + no-pause `run_tick`; delete `WaitingForPower*`/`*Continuation`; `SettlePowerImports` → `release_and_request_power`; denial guard (item 4) + stale-granted release (item 4) | superseded batch → dropped; stale *granted* → emits release (no stuck reservation); bulldozed-consumer echo → no-op; denial after LOCAL power regained → local power untouched |
| **P-b** Nudge | `PowerCapacityRecheck` from the hint-publish sweep → connected component; `power_demand_recheck` (time-neutral); worker's disjoint id counter | worst case with nudge dropped/delayed == P-a alone (never worse) |
| **P-c** Jobs | same cutover, citizen echoed | hourly ticks still never touch jobs/goods |
| **P-d** Goods | same cutover, commercial echoed | `apply_pending_goods_stock` + goods phase daily-only |
| **P-e** Delete `TickState` | only `Idle` left → drop the enum; `pop_next_runnable_event` → plain FIFO | zero `Waiting*`/continuation refs left |
| **P-f** Starvation-fix | *(exploratory)* can the capture/restore dance simplify now? | delete only if the guarded race is confirmed gone |
```text
 P-a first (everything builds on the power cutover). P-c/P-d any order
 after it. P-e once nothing pauses. P-f optional — plan is done without it.
```

## P-a, implemented

Landed on `retire_tick_statemachine`: `src/core/regions/runtime/mod.rs`,
`src/core/regions/mod.rs`, `src/core/regions/worker.rs`,
`tests/region_worker_test.rs`. Power only — jobs/goods untouched except a
mechanical trait-signature ripple (below). `cargo fmt` / `clippy -D
warnings` / `test -q` all green; reviewed twice by codex (one High found
and fixed, see below).

### What changed, structurally

```text
 RegionEvent::ApplyPowerExportGrant
   BEFORE: (PowerExportGrant)                       -- bare grant, token only
   AFTER:  { request: PowerExportRequest, grant }    -- carries its own context

 RegionRuntime (caller-side state for power)
   BEFORE: tick_state: TickState  (has WaitingForPowerExports/Settlement)
           power_export_producers: Vec<RegionId>
   AFTER:  tick_state: TickState  (Idle | WaitingForJobExports | WaitingForGoodsExports)
           power_export_producers: Vec<RegionId>            -- unchanged
           current_power_request_id: UiRequestId             -- NEW, the only addition
```

`TickPowerContinuation` and `PowerSettlementContinuation` are gone
entirely — deleted, not deprecated. The one thing that survives is
`request_id`, promoted from "a field inside a struct that gets thrown
away" to "the caller's whole memory of what it's doing."

### How `ApplyPowerExportGrant` works, end to end

Where the event comes from — the worker already holds `request` at the
moment it routes the result back; it used to throw it away, now it
forwards it:

```text
 Caller region                  Worker                    Producer region
 ─────────────                  ──────                    ───────────────
 release_and_request_power
   PowerExportRequested ────────► route_export_request
                                    ├─ candidate found?
                                    │    yes → ProcessPowerExportRequest ─────►
                                    │                                          process_power_export_request
                                    │                                              grant / deny
                                    │◄──────────────────────────────────────── PowerExportRequestCompleted
                                    │                                          { request, grant }
                                    │
                                    │  worker ALREADY holds `request` here —
                                    │  forwards it instead of just the grant:
                                    ▼
                              ApplyPowerExportGrant { request, grant } ──────►
                                                                              (back to caller region's inbox)
```

What the caller does with it on arrival — one unconditional step, then one
branch:

```text
 RegionEvent::ApplyPowerExportGrant { request, grant } arrives
        │
        ▼
 remember_power_export_producer(&grant)     ← ALWAYS runs first, regardless
        │                                      of staleness (producer reserved
        │                                      capacity the moment it granted;
        │                                      this caller's NEXT release must
        │                                      still reach it eventually)
        ▼
 request.request_id == current_power_request_id ?
        │
        ├─ NO  (stale — a newer batch already superseded this one)
        │       │
        │       ├─ grant.granted? ──yes──► fire ONE targeted release to
        │       │                           grant.source_region, stamped
        │       │                           with the CURRENT generation
        │       │                           (see "the stale-granted-release
        │       │                           fix" below)
        │       └─ grant.granted? ──no───► nothing reserved, nothing to do
        │
        └─ YES (this is my current batch)
                │
                ▼
        rebuild PendingPowerDemand from the echoed request
        (token, consumer, demand, caller_network)
                │
                ▼
        RegionState::apply_power_export_grant(demand, grant)
          — the ECS write: power the consumer (or undo an
            Imported-only optimistic restore on denial)
```

No token lookup in a Vec, no continuation, no "is this the last one, chain
forward" — the match/mismatch against one scalar is the entire decision.

### The tick, before and after

```text
 BEFORE                                    AFTER
 ──────                                    ─────
 Tick → power::run                         Tick → power::run
      → demand? → release+request              → demand? → release+request (fire-and-forget)
      → PARK (WaitingForPowerExports)           → keep going, same pass
      → ... (later pass) ...                    → jobs (still pauses if daily+demand)
      → ApplyPowerExportGrant arrives             → RegionTickCompleted (already sent!)
      → last demand? → unpark → jobs
      → ...
      → RegionTickCompleted
```

`RegionTickCompleted` now shows up on the **first** pass a power-only tick
runs, not the last. Three integration tests asserted the opposite
(`tick_replies.len() == 1` on the pass that used to apply the grant) and
needed that assertion moved — not a behavior bug, a test that was watching
the wrong pass.

### The stale-granted-release fix (caught in the plan's own 3rd review)

```text
 batch N:  request sent to producer P --------------------.
 batch N+1 supersedes N (release + new request)            |
   producer P's copy of N is now orphaned in this caller's  |
   own bookkeeping -- P still thinks it granted N           |
                                                             v
 N's grant arrives late, GRANTED:  request_id(N) != current(N+1)
   -> stale. Old code: just drop it. BUG: P's reservation for N
      is never released if this caller then goes quiet.
   -> New code: drop it AND fire one targeted release to P,
      stamped with the CURRENT generation (N+1). P's
      release_stale_for_caller keeps anything tagged N+1,
      drops anything tagged older -- so this is safe even if
      N+1 also granted something at P.
```

A stale *denial* needs no release (it reserved nothing at the producer).

### The mid-tick interleave codex caught

The first review round found a real gap: the shared dispatch allow-list
(`pop_next_runnable_event`) still let `ApplyPowerExportGrant` jump the
queue while a tick was paused for jobs/goods — a leftover from when power
itself used to pause. Since power's own apply-grant no longer needs to
unblock anything, letting it jump the queue meant a power reply could
mutate world state **while that same tick's job/goods phases were still
in flight**, contradicting the whole point (grant lands for the *next*
tick, not this one):

```text
 Tick T: power dirty -> release+request -> enter job phase
                                              -> daily + demand -> PARK (jobs)
 (later pass) power's reply for Tick T arrives, GRANTED, still tick T's batch
   BEFORE fix: allow-listed -> jumps queue -> applies mid-T, ahead of T's
              own job/goods/economy phases reading power state
   AFTER fix:  not allow-listed -> waits its turn like an ordinary event ->
              applied only once T fully finishes and returns to Idle
```

Fix: removed `ApplyPowerExportGrant` from the allow-list. Kept
`ProcessPowerExportRequest`/`ReleasePowerExportAllocations` (producer-side
— still needed so two mutually-exporting regions can't deadlock each
other) and `ApplyJobExportGrant`/`ApplyGoodsExportGrant` (still the only
thing that unblocks their own pause).

### Tests

Re-baselining was the bulk of the diff, as the plan warned. Three shapes:
- **Power tests rewritten for the new contract**: a tick with demand now
  completes immediately (was: enters a wait state); a matching-generation
  grant applies silently with no second `RegionTickCompleted`; a stale
  granted reply is dropped *and* releases the producer; an unmatched/since
  bulldozed consumer is a no-op; a denial never touches `PowerSource::Local`.
- **One test moved from power to jobs**: `second_tick_is_deferred_while_waiting`
  tested the *shared* deferred-dispatch mechanism using a power fixture;
  since power no longer pauses, it's rebuilt on `job_seeker_region` (jobs
  still pause, code unchanged) — same mechanism, still covered.
- **Three integration tests' pass-by-pass assertions shifted** from "grant
  pass" to "request pass" to match `RegionTickCompleted`'s new timing (see
  above).

## P-b, implemented

Landed on `retire_tick_statemachine`, depends on P-a. `src/core/regions/mod.rs`,
`src/core/regions/runtime/mod.rs`, `src/core/regions/worker.rs`,
`tests/region_worker_test.rs`. `cargo fmt` / `clippy -D warnings` / `test -q`
all green; reviewed twice by codex (one Low found and fixed).

### The two clocks, made concrete

```text
 Tick(H) ─────────────────────────────────────────────── Tick(H+1)
   caller's gate checks discovery_generation, only here

 fast worker passes:  pass · pass · pass · pass · pass · pass · pass

 WITHOUT the nudge:  hint changes → (nothing happens until Tick(H+1))
                                     → gate notices → asks → Tick(H+2) applies
                                     worst case: exactly what P-a already promised

 WITH the nudge:     hint changes → PowerCapacityRecheck fires THIS pass
                                     → ask → grant lands a few passes later
                                     → usually done well before Tick(H+1)
```

The gate (`discovery_generation > seen_power_generation`, checked every
tick — unchanged since P-2) is still the only thing the *worst* case
depends on. The nudge never touches it; it only sometimes lets a region
ask sooner than its own next tick would have.

### Where the nudge is triggered from

```text
 process_region_events_with_mode
   │
   ├─ per-region event processing (unchanged)
   │
   ├─ P-1 sweep: every region with hints_dirty → changed_summaries
   │
   ▼
 for (region_id, links, hints) in changed_summaries:
     republished = directory.publish_region(region_id, links, hints.clone())
       │
       ├─ false (no real change) ─────────────────────► nothing to nudge
       │
       └─ true (a real change) ──► recheck_id = next_worker_request_id()
                                     │
                                     ▼
                              for each OTHER network in this network's
                              connected component (discovery.component_of):
                                  route PowerCapacityRecheck{recheck_id, region_id}
```

`publish_region`'s own idempotence check (it already compared old vs. new
links/hints to decide whether to rebuild the discovery snapshot) is now
also the nudge's gate — a hint re-published unchanged fans out nothing.

### What a target region does with the nudge

```text
 RegionEvent::PowerCapacityRecheck { request_id, .. } arrives
        │
        ▼
 RegionState::power_demand_recheck()   ← time-neutral: NO advance_hours
        │  (same capture/clear/restore dance as begin_tick_power_demand_phase,
        │   for the identical reason: release_and_request_power is about to
        │   release every producer reservation and request only what THIS
        │   scan finds, so an already-imported consumer must be re-included
        │   or its reservation is orphaned)
        ▼
 release_and_request_power(request_id, demands)   ← the EXACT SAME P-a
                                                      helper a dirty tick uses
        │
        ▼
 fire release + request, return immediately — no tick, no pause, no reply
```

Nothing about applying the eventual grant changes: it comes back through
the ordinary `ApplyPowerExportGrant { request, grant }` path from P-a,
compared against `current_power_request_id` exactly the same way whether
it was asked for by a tick or by a nudge.

### The worker-minted id

```text
 UI-minted ids (RegionalGame's AtomicU64):  1, 2, 3, 4, ...  ─┐
                                                                ├─ never collide:
 worker-minted ids (bit 63 always set):                        │  disjoint bit
   bit 63 | WorkerId (bits 32..62) | counter (bits 0..31)  ────┘  ranges

 worker 1's 1st nudge:  1_000...0001 << 32 | 1
 worker 2's 1st nudge:  1_000...0010 << 32 | 1     ← same counter value,
                                                       different WorkerId bits,
                                                       structurally cannot collide
```

Both the `WorkerId` ceiling (`< 2^31`) and the counter are checked with a
real `assert!`/`checked_add` (not `debug_assert!`) — caught in review: a
release build silently wrapping either would reintroduce exactly the
collision this scheme exists to rule out.

### Tests

- **`power_capacity_recheck_requests_export_without_advancing_time`**
  (unit, `runtime/mod.rs`): a nudge on a region with real power demand
  fires a request and never advances `turn` — proves time-neutrality
  directly.
- **`eager_nudge_powers_neighbor_before_its_own_first_tick`** (integration):
  the money test. A producer builds a power plant; the consumer — which
  never receives a single `Tick` — ends up powered purely from the nudge
  fan-out + request/grant round trip.
- **`eager_nudge_does_not_refire_on_an_unchanged_pass`** (integration):
  after the above settles, a further no-op pass produces zero additional
  pending events anywhere — the idempotence gate holds.
- **`worker_minted_ids_are_disjoint_from_ui_ids_and_other_workers`** (unit,
  `worker.rs`): two workers' first ids, and one worker's first two ids, are
  all distinct and all carry bit 63.

No test targets the gate-only worst case directly — it doesn't need one:
every P-a test already exercises ticks with the nudge never firing (none
of them run the worker's hint-publish sweep with a genuine change), and
they all still pass unchanged, which *is* the proof that P-a's worst-case
guarantee holds with P-b layered on top.

## P-c, implemented

Landed on `retire_tick_statemachine`, depends on P-a. Same mechanical
cutover as power (self-describing replies, `current_job_request_id`,
delete `WaitingForJobExports`/`TickJobContinuation`), but this one
uncovered a real gameplay bug the plan text never anticipated, plus a
sharp edge in the fix for it. `cargo fmt` / `clippy -D warnings` /
`test -q` all green; three codex review rounds (2 High found and fixed
across the two rounds after the mechanical part; the mechanical part
itself was clean).

### The mechanical part (same shape as P-a)

```text
 JobExportRequest gains:  citizen: Entity            (echoed back, like power's consumer)
 ApplyJobExportGrant:     (JobExportGrant)  ->  { request: JobExportRequest, grant }
 RegionRuntime:           + current_job_request_id: UiRequestId
                          - TickJobContinuation, WaitingForJobExports (deleted)
 reconcile_job_export_allocations  ->  release_and_request_job  (fire-and-forget)
 apply_job_export_grant:  staleness check against current_job_request_id,
                          release_stale_granted_job on a stale-but-granted reply
 pop_next_runnable_event: ApplyJobExportGrant removed from the mid-wait
                          allow-list (jobs no longer pauses; goods still
                          does, P-d not done, so goods keeps its slot)
```

Nothing here surprised review. The surprise was underneath it.

### The bug this cutover woke up

```text
 EVERY daily tick, today, already does this, back to back:
   1. WIPE     every citizen's job (workplace_assignment = None)
   2. re-match local slots first
   3. request  still-jobless citizens to a remote region
   4. ECONOMY  pay salary, let citizens shop      ← reads job state RIGHT NOW

 OLD (paused):   1 → 2 → 3 → [ freeze until reply lands ] → 4
                                                   economy always sees FRESH state

 P-c as first written:  1 → 2 → 3 → 4 (immediately, reply not back yet)
                                       economy sees "just wiped" — salary 0
                                       EVERY day, forever (the wipe recurs daily)
```

Power didn't have this problem because a granted consumer's `powered` flag
is diff-applied and *sticks* across ticks. Jobs *actively* wipe and rebuild
every day, by design (so a citizen can grab a newly-opened local slot
instead of staying stuck remote) — removing the pause turned a one-time
transient delay into a permanent recurring one. Caught by
`regional_view_reports_city_goods_and_city_aware_inspect_notes` (an
existing integration test asserting a commercial building keeps selling
goods after 7 days) flipping from pass to fail.

### The fix: gate the wipe with the gate that already exists

```text
 Today, split-brain: the WIPE is unconditional; only the re-request that
 could repair it is gated.

 assign_local_jobs_for_daily_tick  ← wipes everyone           (today: always)
 assign_local_jobs                 ← matches jobless only,     (today: never
                                      preserves remote           called on its
                                      assignments — its OWN       own on a daily
                                      doc comment already          tick)
                                      says so
 jobs_exports_dirty gate           ← already exists, already decides
                                      "did anything change?" every daily tick
                                      — today only gates the re-request

 P-c: one decision, not two.
   quiet day  → skip the wipe entirely, leave every assignment alone
   dirty day  → wipe → match → request, exactly like before
```

A stable remote worker is left alone not by luck: every chokepoint that
could make the wipe *worth doing* already flips the SAME flag —
`attach_citizen`/`attach_population` (new job seeker), `invalidate_resource_registry`
(building built/bulldozed/replaced — local slots may have changed),
discovery-generation moving (the remote side changed). The gate was
already trustworthy; P-c just also asks it about the wipe.

**The self-dirtying loop.** One thing was NOT trustworthy at first: a
granted remote assignment landing was itself flagged as "something
changed" (`apply_job_export_grant`'s ECS write called the general
`World::invalidate_jobs_registry()`, which sets `jobs_exports_dirty`). That
re-opens the gate the very next day, wiping the assignment right back out
— the bug, recreated even after gating the wipe. Fix: a narrower
`World::refresh_jobs_cache_after_grant_applied()` that refreshes the job
cache and hints (still needed — the cache is stale, and a slot got filled)
but does **not** re-flag `jobs_exports_dirty`. Applying a grant is the
gate's own answer arriving, not new information for it to notice.

```text
 day D:   dirty (new seeker) → wipe → request
 async:    grant lands → apply → refresh_jobs_cache_after_grant_applied
                                   (cache refreshed, gate STAYS clear)
 day D+1: gate reads clear → QUIET → assignment left alone → citizen paid
```

### The edge codex caught in the fix itself

```text
 continue_to_job_phase, in order:
   ... → population::run (can spawn a citizen THIS tick, which itself
                           calls attach_citizen → sets jobs_exports_dirty)
       → [ decide: wipe or not? ]
```

My first pass read the dirty gate BEFORE calling into this function at
all — meaning a citizen born partway through it could never be reflected
in a decision already made. Fix: split the gate into two halves.
`discovery_dirty` (the runtime's own generation check) genuinely can't
change mid-tick, so it's still snapshotted before and passed in. But
`jobs_exports_dirty` is now read **fresh, after `population::run`**,
inside the function — combined into one effective answer, `jobs_dirty()`,
carried back out on `TickJobPhase`/`RegionalTickJobPhase` for the caller
to act on:

```text
 BEFORE (caught in review):                AFTER:
 read dirty → call function                call function → population::run
   → population::run (too late             → NOW read jobs_exports_dirty
     to matter, already decided)              (population's spawn already
                                               counted) → return jobs_dirty()
```

Without this, a citizen spawned by growth would wait a full extra day for
its first job attempt even on an otherwise-quiet day — not the permanent
bug above, but a real, avoidable one-day miss the cheap fix closes
outright.

### Tests

- **`apply_job_export_grant_does_not_redirty_jobs_exports`** (regions/mod.rs):
  the self-dirtying-loop regression guard — verified red without the fix
  (reverted one line, confirmed failure, restored).
- **`jobs_dirty_is_rechecked_after_population_spawns_a_citizen_same_tick`**
  (simulation.rs): the population-timing edge — drives `continue_to_job_phase`
  directly (not `tick_world`, which forces dirty unconditionally) to reach a
  genuinely quiet day-24 boundary, spawns a citizen via real growth, asserts
  `jobs_dirty()` is true anyway. Also verified red-then-green.
- **Rewritten in runtime/mod.rs's `tick_state_tests`** (same shapes as
  P-a's): a daily tick with a jobless seeker now completes immediately
  instead of entering a wait state; a stale job reply is dropped and
  releases the producer; a since-removed citizen's grant is a no-op;
  `second_tick_is_deferred_while_waiting` moved from a jobs fixture to a
  goods one, since jobs no longer pauses (goods still does — P-d).
- The existing integration test that caught the original bug
  (`regional_view_reports_city_goods_and_city_aware_inspect_notes`) is
  itself now the regression guard for the full gameplay path — it went
  fail → pass across this work and needed no changes of its own.

### Risk noted, not chased

This patch touches 6 files (~450 changed lines) across two review passes
— over this repo's usual 5-file/400-line guideline for one patch. Codex
was asked directly whether the self-dirtying-loop fix should have split
out separately and agreed it couldn't have: it's a necessary correctness
fix for P-c to work at all, not an independent improvement, so there was
no prior point where it could have landed on its own.

## P-d/P-e, implemented

Landed on `retire_tick_statemachine`, depends on P-a/P-c. Goods now uses
the same self-describing, fire-and-forget flow as power and jobs:

```text
 GoodsExportRequest gains: commercial: Entity
 ApplyGoodsExportGrant:    (GoodsExportGrant) -> { request: GoodsExportRequest, grant }
 RegionRuntime:            + current_goods_request_id
                           - TickGoodsContinuation, WaitingForGoodsExports
 reconcile_goods_export_allocations -> release_and_request_goods
 apply_goods_export_grant: staleness check against current_goods_request_id,
                           release_stale_granted_goods on stale-but-granted,
                           granted units still stage into pending_goods_stock
 pop_next_runnable_event:  plain FIFO
```

The user-facing cadence stays the same where it matters: goods are still
daily-only. `enter_job_phase` returns before goods on hourly ticks, so no
hourly goods release/request traffic is emitted. On a dirty daily goods
phase, the runtime applies last tick's `pending_goods_stock`, collects
current commercial demand, releases old producer allocations, requests the
new batch, and finishes the tick immediately. A grant that lands later
stages stock for the next daily goods phase.

P-e was included because P-d removed the last waiting state. Keeping an
`Idle`-only `TickState` enum and the mid-wait mailbox filter made the code
dead under `clippy -D warnings`, so the cleanup is mechanical fallout from
P-d, not a separate gameplay change.

### Tests

- **`daily_tick_with_goods_demand_completes_immediately_and_requests_export`**
  (`runtime/mod.rs`): a daily commercial goods demand emits release/request
  and `RegionTickCompleted` in the same pass.
- **`matching_goods_grant_applies_on_next_daily_goods_phase`**
  (`runtime/mod.rs`): a granted import is not stored immediately; it lands
  through `apply_pending_goods_stock` on the next daily goods phase.
- **`stale_goods_reply_is_dropped_and_releases_the_producer`**
  (`runtime/mod.rs`): a stale-but-granted goods reply emits a targeted
  release for the producer reservation it would otherwise strand.
- Existing goods quiet-path and worker routing tests were updated through
  the new self-describing request shape.
