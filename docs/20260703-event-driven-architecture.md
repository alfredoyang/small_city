# Event-driven state propagation — slim the tick down to the time pass

Status: **plan** (not implemented).

Mission (user direction): *"make the whole system event-driven — build a power
plant, dispatch a power update event; the power update affects building
status; a powered factory updates its region's resource hint and emits an
event so unemployed citizens local or external can find the job; commercial
workers and goods the same; the tick just progresses time, it doesn't update
the majority of game status like the current implementation."*

This plan turns that direction into the repo's own vocabulary. The honest
headline: **the codebase is already half event-driven** — the DT1/DT2 split
(`docs/derived-state-vs-time-pass-plan.md`), the `ResourceRegistryCache`
(compute-on-change power/job resolution), and the L1 repricing gate
(`road_topology_dirty`, `docs/20260630-l1-route-repricing-gate.md`) all
implement exactly the "dispatch an invalidation at the mutation, recompute at
the next read" pattern the user is asking for, *within one region*. What is
still tick-driven — and what this plan migrates — is everything **between**
regions: export reconciliation, availability-hint publishing, and the
consumer-flag churn that `power::run` inflicts every hour whether or not
anything changed. The tick that remains at the end is the time pass plus the
change-gated boundaries: exactly "the tick just progresses time."

---

## 1. Introduction — the problem

### What the hourly tick recomputes today, unconditionally

Every region, every game hour (`RegionEvent::Tick` →
`start_tick_power_phase`, `runtime/mod.rs:821` →
`begin_tick_power_demand_phase`, `regions/mod.rs:1013`):

1. **`power::run` churns every consumer** (`simulation.rs:107`, body at
   `systems/power.rs:10-49`): clears every consumer's `powered`/`source`
   (`power.rs:17-20`), then reapplies grants from the **cached** resolution.
   The resolution itself is compute-on-change (`resource_registry.rs:238-241`
   only recomputes when `power_dirty`); the per-tick cost is the
   clear-and-reapply — and its worst side effect is dropping cross-region
   **imported** grants that are still valid, which forced the
   capture/filter/restore dance in `begin_tick_power_demand_phase`
   (`regions/mod.rs:1013-1055`) and the denial-unwind in
   `apply_power_export_grant` (`regions/mod.rs:1126+`) — the entire fix in
   `docs/20260703-bug-cross-region-export-starvation-fix.md` is a workaround
   for per-tick clearing.
2. **Power export reconciliation releases everything and re-requests
   everything** (`reconcile_power_export_allocations`,
   `runtime/mod.rs:865-907`): `release` of all previous producer allocations
   plus one `PowerExportRequested` per unmet demand, **every hourly tick**,
   even when demand set, producer capacities, and topology are all unchanged.
   The `TODO(CR allocation lifecycle)` comments at `runtime/mod.rs:859` and
   `runtime/mod.rs:936` already name the wanted end state: *"trigger
   reconciliation from explicit demand, producer-capacity, or
   component-change events so it runs only when needed instead of every
   tick."* (Both TODOs point at a "Deferred optimizations" section of
   `docs/regional-multi-worker-plan.md` that doesn't actually exist there —
   this plan is where that deferred work now lives; the TODOs should be
   repointed here as they're absorbed.)
3. **Job export reconciliation, same shape, every daily tick**
   (`reconcile_job_export_allocations`, `runtime/mod.rs:942-980`), and goods
   (`reconcile_goods_export_allocations`, `runtime/mod.rs:1001+`).
4. **Availability hints recomputed per worker pass** for every region that
   processed any event (`worker.rs:578-582` pushes
   `runtime.state().availability_hints()` unconditionally into
   `changed_summaries`; `availability_hints`, `regions/mod.rs:948`, runs
   road-network discovery + spare-slot scans each call). The directory's
   idempotence check (`directory.rs:189-211`, `publish_region` returns
   `false` on no change) prevents redundant *rebuilds*, but the *compute* is
   spent every pass — and, conversely, a region that processed **no** events
   this pass never re-publishes at all (`worker.rs:530-533` skips it) — a
   real stale-hint gap. (The starvation investigation initially suspected
   this edge as the root cause; that hypothesis was **wrong** — the diagnosed
   cause was the import-clearing in `begin_tick_power_phase` — but the gap
   itself is real, and P-1 closes it.)

### Why it matters

- **Correctness pressure**: the export-starvation bug existed *because*
  per-tick clearing creates hourly transient windows. Its fix is three
  coordinated pieces of restore/unwind/filter logic that all exist only to
  survive a churn the steady state never needed. Event-driven reconciliation
  shrinks that window class from "every hour" to "only when config actually
  changed."
- **Wasted work**: a quiet city pays full reconcile + hint-compute cost per
  region per tick. The registry cache patch already proved the win of
  compute-on-change (7.3s → 1.9s on a scenario suite, per the comment at
  `resource_registry.rs:205-209`); this extends it across the region
  boundary.
- **Architecture debt**: two live TODOs and the status report's §2.4 all
  point here. Goods will pile onto the same lifecycle; better to fix the
  pattern before the third resource hardens it.

### Goal (success looks like)

1. A quiet tick (no config changes anywhere) performs **zero** export
   releases/requests, zero hint recomputes, zero directory publishes, and no
   consumer-flag churn — it advances time and runs the DT2 systems only.
2. Building a power plant propagates: registry invalidation → derived pass →
   consumers powered → jobs registry invalidated → hint republished →
   consumer regions reconcile — all within the existing one-(sub-)tick-stale
   discipline, converging in a bounded number of passes.
3. The starvation-bug workaround stops executing on quiet ticks entirely
   (and its transient window only exists on config-change ticks).
4. Determinism, save/load, and the parity suite are untouched: same inputs →
   same outputs; only *when work happens* changes, not *what it computes*.

---

## 2. Proposal

### The design in one sentence

Generalize the `road_topology_dirty` → L1-repricing-gate precedent
(`worker.rs:558-577`) to the other three per-tick recomputes — hints, export
reconciliation, and consumer-flag churn — using (a) the invalidation
chokepoints that already exist (`invalidate_resource_registry`,
`world.rs:293`; `invalidate_jobs_registry`, `world.rs:298`;
`mark_road_topology_dirty`, `world.rs:415`) as the local "events", and (b) a
new monotonic **discovery generation** on the shared `RegionDirectory` as the
cross-region "event", so a consumer region reconciles exactly when something
it can see has changed.

No new event *bus* is invented: locally the "event" is a dirty flag set at
the existing chokepoints (the repo's established idiom); across regions the
"event" is the directory snapshot generation moving (read at the same
start-of-slice points where the worker already reads the snapshot,
`worker.rs:536-547` — same staleness discipline, no new races).

### Where every piece lives — the layer map

Everything new (marked `◆`) attaches to a seam that already exists; nothing
crosses a layer it doesn't cross today:

```text
 LAYER                     EXISTING (reused as-is)              ◆ NEW in this plan
 ─────                     ───────────────────────              ──────────────────
 World                     derived_dirty, road_topology_dirty   ◆ hints_dirty
 (one region's ECS)        registry_cache (power/jobs dirty)    ◆ power/jobs/goods_
                           invalidate_* / mark_* chokepoints      exports_dirty (set INSIDE
                              ▲ set by attach_*/remove/upgrade     the existing chokepoints;
                              │ (world.rs:217-258, cleanup,        goods stock is the one
                              │  placement, business_growth)       exception — see P-1)
 ──────────────────────────┼──────────────────────────────────────────────────────
 RegionState               │ begin_tick_power_demand_phase      (unchanged seams;
 (command + tick surface)  │ pending_*_demands                   P-3 deletes the
                           │ availability_hints()                import capture/
                           │                                     restore dance here)
 ──────────────────────────┼──────────────────────────────────────────────────────
 RegionRuntime             │ TickState machine                  ◆ seen_power/jobs/goods_
 (event loop, protocol)    │ reconcile_*_export_allocations       generation: u64 (×3)
                           │ ExportAllocations ledger           ◆ reconcile gates in
                           │                                      start_tick_power_phase,
                           │                                      enter_job/goods_phase
 ──────────────────────────┼──────────────────────────────────────────────────────
 RegionWorker              │ per-region slice: snapshot reads,  ◆ hint publish gated
 (scheduler, routing)      │ road-report gate (the precedent),    on hints_dirty +
                           │ publish_region_summary               second sweep over
                           │ ForwardedEventOrderKey barrier       event-idle regions
 ──────────────────────────┼──────────────────────────────────────────────────────
 RegionDirectory           │ publish idempotence check,         ◆ generation: u64 on
 (shared, Arc)             │ rebuild_discovery, components        CrossRegionDiscovery
                           ▼                                      (bumped per rebuild)
```

Read the left column top-to-bottom and you have the whole propagation path: a
mutation fires a chokepoint → flags flip → the worker's next slice notices →
publish → generation bump → every runtime's next tick gate notices.

### The hourly tick — anatomy before vs after

```text
 TODAY (every region, every game hour,        AFTER P-1..P-6 (same hour,
 regardless of whether anything changed)      nothing changed anywhere)
 ═══════════════════════════════════════     ═══════════════════════════════════
 ensure_derived_state      (dirty-gated ✓)    ensure_derived_state   → no-op ✓
 time += 1h                                   time += 1h                       ✓ kept
 power::run                                   power::run             → no-op
   clear ALL consumers  ✗ churn                 (diff-apply, cache clean:
   reapply local grants ✗ churn                  nothing to write)
   drop imports         ✗ THE bug source        imports simply survive
 capture/filter/restore imports ✗ workaround    (workaround deleted, P-3)
 release ALL power allocations  ✗ every hour  reconcile gate: clean → skip
 re-request ALL power demands   ✗ every hour    grants + ledger untouched
 [daily] release+re-request ALL job allocs    [daily] gate: clean → skip
 [daily] release+re-request ALL goods         [daily] gate: clean → skip
 [daily] happiness decay, population,         [daily] unchanged                ✓ kept
         local job assignment, economy
 [weekly] business growth                     [weekly] unchanged               ✓ kept
 stats / pollution / happiness refresh        unchanged                        ✓ kept
 per-pass availability_hints() compute  ✗     hints_dirty clean → not computed
 publish (idempotence catches no-op)    ✗     not published at all

 ✗ = unconditional work the refactor gates    "the tick just progresses time"
```

On a tick where something *did* change, the AFTER column's gated rows run
exactly today's logic — the refactor never invents a new dirty-path behavior,
it only earns the right to skip the clean path.

**"Grants + ledger untouched", spelled out.** One cross-region import is
*two* records in *two* regions: the consumer's **grant** (its
`PowerConsumer.powered/source: Imported{..}` flags) and the producer's
**ledger** entry (`RegionRuntime.power_export_allocations`,
`runtime/mod.rs:301/451` — the reservation that stops the producer
double-selling that capacity). The invariant this whole area lives by:
**grant held ⇔ reservation held** — both present or both gone, never one
without the other (every review finding on the starvation fix was a
violation of this in one direction or the other):

```text
 Consumer B (GRANT)                       Producer C (LEDGER)
 power_consumers[building]:               power_export_allocations:
   powered: true                            key: (caller B, gen, token)
   source: Imported { C }                   unit: 3  ← reserved for B

 TODAY: the arrangement is torn down and rebuilt EVERY hour —
   clear grant → Release (ledger wiped) → Request (ledger back) →
   grant reply (grant back) — 24 rebuilds/day of the SAME arrangement,
   each mid-rebuild moment a transient window.
 AFTER, quiet tick: no clear, no Release, no Request — both records
   simply persist, still matching. That is "grants + ledger untouched".
 AFTER, dirty tick: today's full teardown+rebuild, BOTH sides together —
   the other consistent option.
```

### Event flow: "user adds a power plant"

```text
 player Build(PowerPlant)                        ── the user's example, end to end
   │  RegionEvent::RunCommand → RegionState::build
   ▼
 attach_power_provider (world.rs:241)
   │  invalidate_resource_registry()   ← the local "power update event"
   │  mark_derived_dirty()             ← DT1 (regions/mod.rs:430)
   │  [NEW] marks hints_dirty          ← folded into the same chokepoints
   ▼
 same worker pass, after the command (worker.rs:557):
 ensure_derived_state → refresh_derived_state_for_world (simulation.rs:270)
   │  power::run: cache recomputes (power_dirty), consumers gain powered=true
   │  power.rs:40-48: power state changed → invalidate_jobs_registry
   │       ← "a factory has sufficient power" propagates to jobs *by existing code*
   ▼
 [NEW] hints_dirty → worker recomputes availability_hints() and publishes
   │  directory.publish_region → snapshot rebuilt → [NEW] generation += 1
   ▼                                            (one-(sub-)tick-stale from here)
 every region, at its next Tick:
   [NEW] reconcile gate: local_dirty? OR seen_generation < snapshot.generation?
   │        YES → release-all + re-request-all   (today's logic, now gated)
   │        NO  → skip: keep grants, keep ledger, touch nothing
   ▼
 unemployed citizens in region A get job export requests routed to the newly
 powered producer — same request/grant rails (ForwardedEventOrderKey barrier),
 same authoritative producer-side allocation, unchanged.
```

The user's cascade — power event → building status → hint → external job
seekers — is exactly this diagram. Steps 1–3 already exist; the plan adds the
two `[NEW]` gates and the generation counter.

### Event flow by building kind

Every build/bulldoze funnels through the same two chokepoint files —
`placement.rs:10-34` (`attach_*` per kind + the road special case) and
`entity_cleanup.rs:7-88` (`invalidate_resource_registry` always, road/evict
dispatch, `remove_citizens_for_home`) — so the per-kind flows differ only in
*which* flags fire and *how far* the cascade travels:

```text
 CHOKEPOINT MATRIX — what one add/remove fires (verified against placement.rs
 attach_building_components + entity_cleanup.rs remove_entity)
 ┌────────────┬──────────────────────────────┬─────────────────────────────────┐
 │ kind       │ add fires…                   │ cascade reach                    │
 ├────────────┼──────────────────────────────┼─────────────────────────────────┤
 │ PowerPlant │ power_provider → registry ALL│ cross-region: capacity hint ↑,   │
 │            │ + derived + hints◆           │ gen bump, importers re-power     │
 │ Commercial │ power_consumer → registry ALL│ cross-region: +2 job slots,      │
 │            │ + derived + hints◆           │ goods DEMAND (storage) appears   │
 │ Industrial │ power_consumer → registry ALL│ cross-region: +3 job slots,      │
 │            │ + pollution_src (derived     │ goods SUPPLY appears; pollution  │
 │            │   only) + derived + hints◆   │ stays local                      │
 │ Residential│ population → registry JOBS   │ TWO-STAGE: config now, demand    │
 │            │ + power_consumer → ALL       │ later via time pass (below)      │
 │            │ + derived + hints◆           │                                  │
 │ Road       │ no components, but:          │ WIDEST: power pooling, workplace │
 │            │ road_topology_dirty +        │ eligibility, L1 routes, goods    │
 │            │ route-cache clear + registry │ routes can ALL flip (below)      │
 │            │ ALL (attach_position/building│                                  │
 │            │ do it) + derived + hints◆    │                                  │
 │ Park       │ happiness_effect ONLY —      │ NONE cross-region: derived pass  │
 │            │ no registry invalidation     │ recomputes local effects/        │
 │            │ (world.rs:258-261) + derived │ happiness; hints unchanged; no   │
 │            │                              │ gen bump; zero wire traffic ✓    │
 ├────────────┼──────────────────────────────┼─────────────────────────────────┤
 │ remove     │ ALWAYS registry ALL          │ + road → topology dirty;         │
 │ (any kind) │ (entity_cleanup.rs:36/75)    │ + residential → citizens deleted │
 │            │ + derived + hints◆           │   with the home (line 74/102)    │
 └────────────┴──────────────────────────────┴─────────────────────────────────┘
 ◆ = the NEW flag, set inside the existing invalidate_*/mark_* bodies —
     which is why the matrix needs no new call sites: every row already
     funnels through them.
```

Park is the proof the gates are *correct*, not just fast: a change that
cannot affect any other region produces exactly zero cross-region traffic,
because it never touches a chokepoint that feeds hints.

**Residential — the two-stage flow.** Placing houses creates demand in two
steps, and the second is deliberately time-driven (DT2), not an event:

```text
 stage 1 — CONFIG (event-driven, this plan)     stage 2 — PEOPLE (time-driven, unchanged)
 Build(Residential)                             next DAILY boundary:
   │ attach_population    → jobs registry       population::run (simulation.rs:147)
   │ attach_power_consumer → registry ALL         │ growth conditions met →
   │ + derived + hints◆                           │ citizens::spawn_for_home
   ▼                                              │   └ attach_citizen → jobs registry
 building exists, power demand counted,           │     + hints◆ (unemployed = demand)
 may itself need imported power (the              ▼
 power-plant flow, in reverse) — but            citizens exist → next daily job phase:
 NO job seekers yet: houses don't               local assignment first, leftover
 spawn people, days do                          seekers → gated job export reconcile
                                                → remote producers discovered
```

This is the cleanest illustration of the plan's boundary: the *config* half
of "add housing" propagates as events immediately; the *demand* half arrives
at the pace the simulation's population growth defines. Event-driven does
not accelerate demographics.

**Workplace removal with remote workers — the convergence that must not be
skipped.** Bulldozing a Commercial/Industrial that employs cross-region
commuters exercises both sides of the gate at once:

```text
   Consumer region A                Directory              Producer region B
   (workers live here)              ─────────              (workplace bulldozed)
                                                     bulldoze Commercial
                                                       │ remove_entity:
                                                       │ registry ALL + hints◆
                                                       │ (entity_cleanup.rs:75)
                                                       ▼
                                    gen N ──► N+1 ◄── hint shrinks (its slots
                                                       leave spare_job_slot_ids)
   next DAILY tick:
   gate: N+1 > seen(A) → DIRTY
     │ release-all ────────────────────────────────►  B's ledger: A's old
     │ re-request per leftover seeker ─────────────►  reservations cleared
     │                                                process_job_export_request:
     │                                                the slot Entity no longer
     │                                                exists in remaining_workplaces
     ◄──────────────────────────────────────────────  JobExportGrant{granted:false}
     ▼
   denial → citizen's remote assignment cleared → unemployed again →
   jobs registry + hints◆ → A's own hint updates (gen N+1 ──► N+2) →
   OTHER producers with spare slots get discovered by A's next daily
   reconcile — the commuter finds a new job elsewhere if one exists.
```

Note the same-region case needs no wire traffic at all: local workers of the
bulldozed building are re-assigned by the next daily `assign_local_jobs`
(jobs registry was invalidated by the removal); only the cross-region ledger
needs the request/deny round trip. (Exact consumer-side seam for clearing a
denied remote assignment is P-4 implementation detail — verify against
`apply_job_export_grant` when it lands.)

**Road add/remove — the widest cascade.** A road is componentless but
touches the most derived systems, because three different graphs hang off
road connectivity:

```text
 Build/Bulldoze(Road)
   │ mark_road_topology_dirty + route-cache clear (placement.rs:30-33 /
   │ entity_cleanup.rs:40-46) + registry ALL + derived + hints◆
   ▼
 one chokepoint, four consumers — all already change-gated after this plan:
   ├─► power networks re-pool          (registry ALL → derived pass; a road
   │                                    can merge two networks = one plant
   │                                    suddenly covers more consumers)
   ├─► workplace eligibility flips     (is_effective_workplace = powered &&
   │                                    ROAD_CONNECTED → jobs registry →
   │                                    hints◆ → gen bump → job reconciles)
   ├─► L1 route repricing              (road_topology_dirty → the EXISTING
   │                                    repricing gate, worker.rs:558-577 —
   │                                    the precedent this plan generalizes)
   └─► goods routes refresh            (border networks / supplier
                                        discovery re-derived)
 Cross-region blast radius: everything — which is fine, because it is the
 SAME blast radius a road change has today; the gates only stop paying it
 on the hours when no road changed.
```

### Why a generation counter instead of targeted notify events

A producer whose capacity *shrinks* must also reach holders of its grants
(otherwise a gated consumer keeps a stale grant forever — this killed the
naive version of the starvation fix in review). Explicit
`ProducerCapacityChanged`/revocation wire events would need new
`RegionEvent`/`OutboundMessage` variants, order-key ranks, and producer-side
holder tracking. The generation counter gets the same convergence with none
of that: **any** hint/topology change anywhere bumps the generation; every
region's next tick notices and runs the *existing* release-all +
re-request-all reconcile; the producer authoritatively grants or denies
against its *current* capacity; the existing denial path
(`apply_power_export_grant`'s unwind, `regions/mod.rs:1126+`) clears
lost grants. Capacity loss and capacity gain ride the same mechanism.

Cost: coarse — one region's change wakes every region's next reconcile. But
that worst case **equals today's every-tick behavior**, and the steady state
(no changes) drops to zero. **ponytail ceiling**: one global generation; if
profiling ever shows cross-talk between unrelated components, upgrade to
per-discovery-component generations (the component graph already exists,
`directory.rs`, `build_component_graph`).

### The generation gate over time — quiet hours cost nothing

Three regions, five game hours. `gen` is the directory snapshot generation;
`seen(X)` is region X's per-resource seen marker (power's, in this hourly
timeline; jobs/goods have their own — see §3). The only config change is
region B building a road at hour 3:

```text
 game hour        1        2        3          4        5
 directory gen    7        7        7 ──► 8    8        8
                                      ▲ B's road → road report +
                                        hints republished → bump

 region A tick   gate:    gate:    gate:      gate:      gate:
 seen(A)=7       7=7,     7=7,     7=7,       8>7 DIRTY  8=8,
                 clean →  clean →  clean →    reconcile: clean →
                 SKIP     SKIP     SKIP       release+   SKIP
                 (keep    (keep    (keep      re-request (keep
                  grants)  grants)  grants)   seen(A)=8   grants)

 region B tick   SKIP     SKIP     local      8=8 but…   SKIP
 (the builder)                     exports_   flag was
                                   dirty →    cleared →
                                   RECONCILE  SKIP
                                   seen(B)=8

 region C tick   SKIP     SKIP     SKIP       8>7 DIRTY  SKIP
 seen(C)=7                         (bump      reconcile
                                   lands      seen(C)=8
                                   NEXT pass:
                                   one-tick-
                                   stale ✓)

 TODAY, same 5 hours: every cell above is a full release-all + re-request-all.
 AFTER: 3 reconciles total (B once locally, A and C once each on the bump),
        12 skips. Convergence: everyone consistent with gen 8 by hour 4.
```

Note hour 3 vs hour 4 for regions A/C: B's own tick sees its *local* flag
immediately, but A and C see the generation bump only on their next tick —
that is the documented one-(sub-)tick-stale model, unchanged.

### Capacity loss — the direction that forced this design

Gain is easy (a new producer just gets discovered). Loss is the direction
that kills naive gating: consumer A holds a power grant from producer C, C's
plant gets bulldozed, and A — being "clean" — would keep a grant backed by
nothing. The generation closes the loop without any revocation event:

```text
   Region A (grant holder)      RegionDirectory           Region C (producer)
   ───────────────────────      ───────────────           ───────────────────
                                                    bulldoze PowerPlant
                                                      │ invalidate_resource_registry
                                                      │ + hints_dirty   (chokepoint)
                                                      ▼
                                gen 8 ──► 9  ◄─── worker publishes C's shrunken
                                                  hint on its next slice
   next Tick:
   gate: 9 > seen(A)=8 → DIRTY
     │ release-all  ──────────────────────────────►  C's ledger: A's old
     │ re-request demand ─────────────────────────►  reservation cleared
     │                                               process_power_export_request:
     │                                               remaining capacity = 0
     ◄───────────────────────────────────────────── PowerExportGrant{granted:false}
     ▼
   apply_power_export_grant (denial path, regions/mod.rs:1126+):
   consumer → unpowered, supplied stat unwound   ← the EXISTING unwind,
   jobs registry invalidated → hints_dirty →        added by the starvation
   A's own hint republishes (gen 9 ──► 10) →        fix, reused verbatim
   A's job-holding neighbours reconcile in turn…
                                                  …and the cascade dies out:
                                                  gen 10's reconciles find
                                                  nothing further to change,
                                                  publish nothing, no bump.
```

Latency: A discovers the loss at its next hourly tick — identical to today,
where A re-requests every hour anyway. The difference is only that the 999
hours where nothing was lost now cost nothing.

### Cascade termination and oscillation

The event edges form a DAG per cause, each stage bumping the generation at
most once per underlying config change:

```text
 config change ──► power resolution ──► consumer powered flags
                        │                     │ (power.rs:46-48)
                        ▼                     ▼
                  hints publish ◄──── jobs registry invalidated
                        │
                        ▼  generation += 1
                  consumers reconcile (power: next hourly tick;
                                        jobs/goods: next DAILY tick)
                        │
                        ▼
                  grants/denials applied — these do NOT touch hints
                  (reservations are not part of availability_hints,
                   regions/mod.rs:948; granting cannot re-bump)
```

The genuinely cyclic feedback loops in the sim — job assignment → happiness →
population growth → job demand → assignment — are **already cut by time
boundaries**, and the plan deliberately keeps them there: daily happiness
decay, daily population growth, and daily local job assignment
(`simulation.rs:143-154`) stay on the daily gate; weekly business growth
(`simulation.rs:186-190`) stays weekly. **This is design, not tick debt**: a
citizen changing jobs at most once per game-day is hysteresis that prevents
assignment thrash, and the daily economy settlement is a gameplay cadence.
Event-driven applies to *derived state and reconciliation triggers*; it does
not make hiring instant. A new producer appearing at 09:00 is discoverable by
external job seekers at their next daily job phase — same latency as today,
minus the wasted work in between.

### What stays time-driven forever

- Time itself (`world.resources.time.advance_hours(1)`, `simulation.rs:105`)
  and `turn` (`simulation.rs:201`).
- DT2: actual morale convergence (`citizens::update_happiness`), daily decay,
  daily population growth, daily economy settlement
  (`economy::run_with_goods_exports`), weekly business reinvestment.
- Movement sub-ticks (`step_travel`, 6× per hour via
  `RegionalGame::advance`, `regional_game.rs:605`) — already decoupled.

### Determinism and staleness (unchanged guarantees)

- All cross-region traffic keeps riding `OutboundMessage` →
  `ForwardedEventOrderKey` barrier sort (`worker.rs:145-163`) — no new wire
  event kinds, so no new ordering ranks at all.
- The generation is read once per region per pass at the same point the
  worker already reads `discovery_snapshot()` for routing/importable-jobs
  (`worker.rs:536-547`); it inherits that read's staleness discipline
  exactly (one pass stale at most). If a cross-worker snapshot-read timing
  edge exists today, this plan neither fixes nor worsens it — explicitly out
  of scope.
- Dirty flags are `Cell<bool>` `#[serde(skip)]` like `derived_dirty`
  (`world.rs:110-117`); generation/seen-generation live on runtime-side
  state that is never serialized. **Load = everything dirty once**: the
  existing load path already forces a full derived refresh
  (`from_world` → `refresh_derived_state_for_world`, `regions/mod.rs:1299+`)
  and a power-import renegotiation (`settle_power_imports`,
  `regional_game_runner.rs:372`); new flags default to dirty / generation 0
  so the first pass after load republishes and reconciles everything. No
  in-flight event ever needs to be durable.

---

## 3. Important structures / functions

### `World::hints_dirty: Cell<bool>` — new field, `src/core/world.rs`

Set (never cleared) by the same chokepoints that already invalidate the
inputs hints are derived from: `invalidate_resource_registry`
(`world.rs:293`), `invalidate_jobs_registry` (`world.rs:298`),
`mark_road_topology_dirty` (`world.rs:415`). **Goods stock is the one
exception and needs explicit new marks** (codex review finding): all stock
mutations happen inside `economy::run_with_goods_exports` — via
`distribute_local_goods` → `add_commercial_goods` (`economy.rs:415/778`)
and `consume_local_good` (`economy.rs:229/759`) — plus the cross-region
delivery entry `RegionState::add_commercial_goods` (`regions/mod.rs:1215`).
None passes any chokepoint, yet `spare_goods_units` feeds hints via
`economy::exportable_goods_units_on_network` (`regions/mod.rs:971-974`).
Mark strategy (two sites, not one per mutation): once after the daily
economy settlement (goods flow may have mutated stock), and once in
`RegionState::add_commercial_goods` — see P-1's pseudocode.
Cleared only by the worker after a successful publish. `#[serde(skip),
default = true-on-load]` so the first pass after load republishes.
Invariant: `hints_dirty == false` ⇒ the directory's stored hints for this
region equal what `availability_hints()` would compute now. **ponytail
ceiling**: this is the third-or-fourth independent `Cell<bool>` on `World`
(the TODO at `world.rs:114` already says "split by subsystem if config
mutation grows") — consolidation into one named bitset is the upgrade path,
deliberately not bundled into this plan.

### `RegionDirectory` snapshot generation — new, `src/core/regions/directory.rs`

A `u64` bumped inside `rebuild_discovery` (`directory.rs:234+`, under the
existing publish-state lock) and stored on `CrossRegionDiscovery` so readers
get it atomically with the snapshot they already clone. Monotonic;
deterministic because rebuilds only happen from deterministic publish calls.
The existing test-only `rebuild_count` is almost this already — the
generation is its production twin.

### Per-resource seen generations — new, `src/core/regions/runtime/mod.rs`

`seen_power_generation` / `seen_jobs_generation` / `seen_goods_generation:
u64` — the reconcile gates' memory: the last directory generation each
resource's reconcile ran against. Not serialized (runtime state never is);
all start at 0 so a fresh/loaded runtime always reconciles once.

**Why one shared marker is NOT enough** (codex review finding): the three
gates run at different cadences — power hourly, jobs/goods only on daily
boundaries — so whichever gate runs first consumes the bump for all three:

```text
  the ABSORPTION bug a shared seen-marker would have
  ──────────────────────────────────────────────────
  hour 5    producer B loses a job slot → hint publish → gen N ──► N+1
  hour 6    A's POWER gate (runs hourly):
              N+1 > seen(N) → dirty → power reconcile
              shared seen = N+1                    ◄── bump CONSUMED
  hour 24   A's daily JOB gate:
              generation "not moved" (seen == N+1) → quiet daily
              → job reconcile SKIPPED
            B's job-slot change is never seen by A's job phase —
            the workplace-removal convergence silently broken.

  per-resource markers: hour 6 advances only seen_power; hour 24's
  job gate still sees N+1 > seen_jobs(N) → dirty → converges.
```

The same cadence argument applies to the dirty **flags**: `exports_dirty`
must be per-resource too (`power_exports_dirty` / `jobs_exports_dirty` /
`goods_exports_dirty`) — set together at the chokepoints, but each cleared
only by its own phase's dirty reconcile. A single shared flag would be
cleared by the hourly power gate before the daily job/goods gates ever read
it (the same absorption, local edition). The directory-side `generation`
itself stays one global counter; only the consumer-side memory splits.

### Reconcile gate — changed: `start_tick_power_phase` / `enter_job_phase` / `enter_goods_phase`, `runtime/mod.rs:821/914/982`

Each phase gains the same guard: reconcile (today's release-all +
re-request-all, unchanged) **only if** `local_inputs_dirty ||
seen_generation < snapshot_generation`; otherwise skip the release *and* the
requests entirely — grants persist, the producer ledger persists, the tick
proceeds to the next phase immediately. "Local inputs dirty" reuses what
exists: the registry cache's `power_dirty`/`jobs_dirty` can't be read
directly without recomputing, so each gate reads its own sibling
`power/jobs/goods_exports_dirty` flag set at the identical chokepoints and
cleared only by its own phase (see the absorption diagram above)
(**heuristic-not-guarantee caveat**: the flag is deliberately coarser than
"my demand set changed" — false positives are safe because a spurious
reconcile is exactly today's behavior; false negatives are prevented by
setting the flag at every chokepoint that can affect demand: config attach/
remove, citizen add/remove, power-state change, road topology). Jobs and
goods keep their **daily cadence** on top of the gate (the `is_daily()`
check at `runtime/mod.rs:920` stays) — the gate decides *whether* the daily
reconcile does work, the calendar decides *when* it may.

### `power::run` diff-apply — changed, `src/core/systems/power.rs:10-49`

Stops clearing `powered`/`source` up front. Instead: compute the cached
resolution, then set each consumer's state to (local grant if resolved,
else **keep** an existing `Imported` source, else unpowered). The
`before`-diff → `invalidate_jobs_registry` edge (`power.rs:40-48`) stays.
Effect: imported grants survive local recomputes *structurally*, so the
capture/restore in `refresh_derived_state_for_world`
(`simulation.rs:283-285`) and the capture/filter/restore in
`begin_tick_power_demand_phase` (`regions/mod.rs:1030-1050`) become
deletable — **but only after the reconcile gate exists**, because on a
*dirty* reconcile the caller must still drop-and-re-request its imports
(release-all makes a kept import unbacked; that is the round-1 review
finding of the starvation fix, now a locked constraint). Sequencing handled
in the patch split. **ponytail ceiling**: on dirty ticks the
release-everything reconcile remains (with its shrunken transient window);
fully windowless renegotiation would need per-grant renewal instead of
release-all — named upgrade path, not in this plan.

### Hint publish loop — changed: `process_region_events_with_mode`, `src/core/regions/worker.rs:517-590`

Two changes. (a) The `changed_summaries` push (`worker.rs:578-582`) is gated
on `hints_dirty` — no per-pass `availability_hints()` compute for clean
regions. (b) The loop stops being limited to regions with pending events:
after the per-region event slice, a second sweep publishes any **owned**
region with `hints_dirty` set even if it processed nothing this pass —
closing the stale-hint gap where an event-idle region's directory entry
never catches up. (The road-report gate at `worker.rs:558-577` already
proved this exact structure.)

---

## 4. Pseudocode + interaction with current code

### P-1: hint gating (worker.rs + world.rs)

```rust
// world.rs — new flag, set inside the EXISTING chokepoints (no new call sites):
pub(crate) fn invalidate_resource_registry(&self) {
    self.registry_cache.borrow_mut().invalidate_all();
    self.hints_dirty.set(true);                       // NEW line
}
pub(crate) fn invalidate_jobs_registry(&self) { /* same NEW line */ }
pub(crate) fn mark_road_topology_dirty(&self) { /* same NEW line */ }

// regions/mod.rs — goods stock BYPASSES all three chokepoints, so it gets
// explicit marks (the one exception to "no new call sites"):
pub(crate) fn add_commercial_goods(&mut self, commercial, units) {   // mark site 1:
    economy::add_commercial_goods(&mut self.world, commercial, units as i32);
    self.world.mark_hints_dirty();          // NEW — cross-region delivery path
}
// mark site 2: once after the daily economy settlement, where
// run_with_goods_exports may have mutated stock (distribute_local_goods →
// add_commercial_goods, economy.rs:415; consume_local_good, economy.rs:229/759)
// — one coarse mark per settlement, not one per internal mutation.

// worker.rs process_region_events_with_mode — replace the unconditional push:
for runtime in &mut self.regions {
    // ... existing event slice + ensure_derived_state + road-report gate ...
}
// NEW second sweep: ALL owned regions, not only event-active ones.
for runtime in &mut self.regions {
    if runtime.state().hints_dirty() {
        runtime.ensure_derived_state();               // hints read derived state
        changed_summaries.push((region, links(), availability_hints()));
        runtime.state().clear_hints_dirty();
    }
}
// existing publish loop unchanged (worker.rs:585-587); publish_region's
// idempotence check stays as the safety net for false positives.
```

Why goods is the exception — the four hint inputs and their mutation paths:

```text
  hint input      mutated via                          fires a chokepoint?
  ──────────      ───────────                          ───────────────────
  power spare     attach/remove provider or consumer → invalidate_registry  ✓
  job slots       attach/remove citizen or workplace → invalidate_jobs      ✓
  road networks   build/bulldoze road                → mark_road_topology   ✓
  goods stock     economy settlement internals       → building.data write  ✗
                  (distribute_local_goods → add_commercial_goods,
                   economy.rs:415; consume_local_good, economy.rs:229/759;
                   cross-region delivery, regions/mod.rs:1215 — all write
                   `local_goods_stored` on the building directly; no
                   chokepoint fires; without the explicit marks, a
                   goods-only change never republishes and remote shoppers
                   keep seeing yesterday's availability)
```

Reuses verbatim: `publish_region_summary`, directory idempotence, the
road-report-gate structure. Behavior-identical outputs; publish *timing*
strictly improves (event-idle regions now catch up).

### P-2: discovery generation + power reconcile gate (directory.rs + runtime/mod.rs)

```rust
// directory.rs rebuild_discovery — one line inside the existing lock:
discovery.generation = self.next_generation();        // monotonic u64

// runtime/mod.rs start_tick_power_phase (821):
// ORDER MATTERS (codex review finding): decide the gate BEFORE collecting
// demands. After P-3's diff-apply a kept import stays `powered`, so a demand
// scan taken first would NOT list it; the dirty path's release-all would then
// strip its producer reservation with no replacement request — the round-1
// desync again. The gate's inputs (flag + generation) need no demand scan.
fn start_tick_power_phase(&mut self, request_id) -> Vec<OutboundMessage> {
    let snapshot_generation = /* worker installs per-slice, like set_region_routes */;
    let dirty = self.state.power_exports_dirty()       // local inputs changed
        || snapshot_generation > self.seen_power_generation;
    if !dirty {
        // NEW quiet path: time advance + power::run only; demands are not
        // collected. Grants + producer ledger untouched; no release, no
        // requests. Straight to the job phase (own gate).
        //
        // P-2 lands BEFORE P-3, so power::run still clears imports here: the
        // quiet variant preserves them with the EXISTING helpers
        // (imported_power_grants / reapply_imported_power, pub(crate) since
        // the starvation fix). UNFILTERED reapply is safe on THIS path —
        // nothing is released, so every kept grant still has its producer
        // reservation (the round-4 filter existed only because the dirty
        // path releases). P-3 later swaps this for diff-apply and deletes
        // the capture/reapply dance.
        let phase = self.state.begin_tick_power_phase_quiet();
        return self.enter_job_phase(request_id, phase);
    }
    self.seen_power_generation = snapshot_generation;
    self.state.clear_power_exports_dirty();
    // DIRTY path: clear imported sources FIRST, then collect demands — every
    // import-needing consumer now shows up and gets a re-request.
    let phase = self.state.begin_tick_power_demand_phase_dirty();
    self.reconcile_power_export_allocations(request_id, phase)  // today's code
}
```

Why gate-then-collect and not collect-then-gate (the ordering both P-2 and
P-3 must honor once imports persist):

```text
  WRONG — collect first (original draft):    RIGHT — gate first:
  ─────────────────────────────────────      ──────────────────────────────
  begin_..._demand_phase()                   dirty? ──no──► quiet: time +
    power::run keeps import (powered ✓)                     diff-apply only,
    pending_demands: imported consumer            │yes      no demand scan
    is powered → NOT LISTED ✗                     ▼
        │                                    clear imported sources
        ▼ gate says dirty                         ▼
  release ALL producer reservations          collect demands — the import-
  re-request only the LISTED demands         needing consumers are unpowered
        │                                    now, so they ARE listed ✓
        ▼                                         ▼
  kept import: grant still held,             release-all + re-request covers
  reservation GONE — unbacked power,         every import: grant ⇔ ledger
  the round-1 desync reintroduced ✗          torn down + rebuilt together ✓
```

Interaction note: the quiet path still advances time and runs `power::run`
through `begin_tick_power_phase_quiet` — only the demand scan and
release/request traffic are gated. The quiet variant has two eras: **at P-2**
it wraps today's raw `power::run` in the existing capture/reapply-all import
preservation (so P-2 is independently green); **after P-3** the diff-apply
makes that wrapper redundant and P-3 deletes it, leaving `power::run` an
effective no-op on clean cache. The two `TODO(CR allocation lifecycle)`
comments (`runtime/mod.rs:859/936`) get repointed to this doc as each lands.

### P-4 (jobs; goods P-5 identical shape)

```rust
// enter_job_phase (runtime/mod.rs:914): the is_daily() early-out stays.
// Own flag + own seen marker — the hourly power gate can never absorb a
// bump or a local change destined for the daily job gate (§3 diagram).
if phase.is_daily()
    && (self.state.jobs_exports_dirty()
        || snapshot_generation > self.seen_jobs_generation)   // NEW gate
{
    self.reconcile_job_export_allocations(request_id, phase)   // today's code
} else if phase.is_daily() {
    // NEW quiet daily: grants persist; finish the phase without release/requests
    self.enter_goods_phase(request_id, /* job phase folded through */)
}
```

Kept deliberately: daily cadence (hysteresis, gameplay); release-all +
re-request-all as the *dirty-path* policy (simple, already reviewed); the
producer's authoritative grant/deny and the existing denial-unwind path.

### P-3: diff-apply `power::run` + delete the starvation workaround

```rust
// power.rs run(): replace the clear-loop (17-20) with per-consumer diff:
for (entity, consumer) in world.power_consumers.iter_mut() {
    let local = resolution_grant_for(entity);
    match (local, consumer.source) {
        (Some(grant), _) => set(consumer, powered, Local(grant.source)),
        (None, Some(PowerSource::Imported {..})) => { /* KEEP — do not clear */ }
        (None, _) => set(consumer, unpowered, None),
    }
}
// simulation.rs refresh_derived_state_for_world: delete the
// imported_power_grants / reapply_imported_power capture-restore (283-285).
// regions/mod.rs begin_tick_power_demand_phase: delete the capture +
// requestable-filter + restore block (1030-1050), AND the capture/reapply-all
// wrapper P-2 put inside begin_tick_power_phase_quiet (diff-apply makes both
// redundant). On a DIRTY reconcile the
// runtime clears imported sources BEFORE collecting demands (P-2's
// begin_tick_power_demand_phase_dirty): clear-then-collect, never
// collect-then-clear — a demand set taken while imports are still powered
// cannot list them, and release-all would leave them unbacked (see the
// ordering diagram in P-2).
```

Constraint honored (from the starvation fix's review rounds): imports are
never left "kept" while their reservation is released — the dirty path clears
exactly the consumers it re-requests, the quiet path releases nothing. The
three starvation-fix regression tests are retargeted, not deleted: the
integration repro must stay green throughout; the two unit tests assert the
new invariants at the same seams.

### P-6: slim the tick (simulation.rs)

After P-1..P-5, `begin_tick_power_phase` (`simulation.rs:89-114`) keeps
`ensure_derived_state` (already dirty-gated) + time advance; its
`power::run` call becomes a cheap no-op on clean cache after P-3's
diff-apply — P-6 makes the skip explicit (guard on the registry's dirty
state), updates `CLAUDE.md`'s architecture section and the DT4 dependency
comment (`simulation.rs:7-16`), and closes out the absorbed TODOs. Mostly
symbolic; the real wins land earlier — kept last so every system the tick
currently drives has its event-driven replacement in place first.

---

## Decisions locked

- **No new event bus / no new wire event kinds.** Local events = existing
  invalidation chokepoints + new sibling flags; cross-region events = the
  directory snapshot generation. New `RegionEvent` variants: zero.
- **Coarse over clever**: one global directory generation (not
  per-component) — but consumer-side memory is **per-resource**
  (`seen_power/jobs/goods_generation` + sibling per-resource dirty flags):
  the gates run at different cadences, and a shared marker is absorbed by
  whichever gate runs first (§3). Flag false-positives allowed (a spurious
  reconcile = today's behavior); release-all reconcile kept as the
  dirty-path policy.
- **Daily/weekly cadences are design, not debt** — job assignment, economy
  settlement, population growth, happiness decay, business growth stay
  time-driven, permanently.
- **Tick-slimming lands last** (P-6), after every gated system is proven.
- **Load = all-dirty**: no durable events; first pass after load republishes
  and reconciles everything (matches the existing
  `settle_power_imports` philosophy).
- **`TickState` survives this plan — its retirement is the named follow-up,
  not part of it.** The state machine (`runtime/mod.rs:338-344`) is this
  codebase's async/await: on a *dirty* reconcile, the tick genuinely must
  pause for grants because downstream phases read upstream results the same
  day (`continue_to_job_phase` needs final powered state; the daily economy
  settles salaries with today's job/goods grants). The gates make `Idle` the
  norm — the machine leaves it only on change ticks — but the dirty path
  stays byte-identical, so the machine stays. See "Beyond P-6" below for
  what deleting it would take.

## Beyond P-6 (explicitly out of scope): retiring `TickState`

`TickState` is easiest to understand as this codebase's async/await: a dirty
tick is not one function call, it is **sliced into pieces by grant
round-trips**, and `TickState` is the bookmark between the slices. The
economy is deliberately the *last* slice — that ordering is the whole reason
the machine exists:

```text
 A — WHAT TICKSTATE IS (one dirty daily tick in region B, today + this plan)
 ════════════════════════════════════════════════════════════════════════════
 B's inbox (FIFO)             TickState                what actually runs
 ────────────────             ─────────                ───────────────────
 [Tick] ──────────── pop ──►  Idle
                              ▼
                              power phase: B needs imported power
                              send PowerExportRequested ──► producer C
                              ╔════════════════════════════════╗
                              ║ PARK: WaitingForPowerExports   ║ ◄── the "await":
                              ║ continuation = REST OF THE TICK║     rest of the tick
                              ╚════════════════════════════════╝     saved for later
 [Tick #2 arrives]            held — NOT runnable while waiting
 [ProcessJobExportRequest     allowed through the filter (producer-side
  from region A arrives]      work must run mid-wait or two mutually-
                              importing regions deadlock) — B answers A
                              with whatever state B has RIGHT NOW ◄── the
                                                                      starvation
 [ApplyPowerExportGrant] pop► control event: allowed                  bug's window
                              ▼ RESUME the parked continuation
                              job phase: citizen needs a remote slot
                              send JobExportRequested ──► producer C
                              ╔════════════════════════════════╗
                              ║ PARK: WaitingForJobExports     ║
                              ╚════════════════════════════════╝
 [ApplyJobExportGrant]  pop─► ▼ RESUME
                              goods phase … (same park/resume pattern) …
                              ▼
                              ECONOMY SETTLES ◄─── WHY the parking exists:
                                salary for the job granted TODAY  │ economy is
                                power granted TODAY counts TODAY ─┘ the LAST slice
                              ▼
                              Idle  ──► held Tick #2 becomes runnable
```

This plan (P-2/P-4/P-5) does not touch that dance — it only collapses the
machine's duty cycle from "every hour, every importing region" to "only on
the tick where something changed":

```text
 B — DUTY CYCLE UNDER THE GATES
              h1   h2   h3   h4   h5   h6      h7 (road built)      h8   h9
 gate:       clean clean clean clean clean clean   DIRTY            clean clean
 TickState:   I    I    I    I    I    I     I→W→W→W→I               I    I
              └──────── machine dormant ─────┘    └─ diagram A ┘└─ dormant ─┘
 TODAY (no gates): the I→W→…→I dance runs at EVERY column, every hour —
 that hourly churn is the starvation bug's home.
```

Once grants are persistent (P-2/P-3), a further plan could stop slicing the
tick at all — *fire-and-forget*:

```text
 C — THE FUTURE PLAN THAT DELETES IT (fire-and-forget)
 ═══════════════════════════════════════════════════════
 B's inbox                    (TickState: gone)       what runs
 ─────────                                            ─────────
 [Tick] ──────────── pop ──►  power phase: send PowerExportRequested ──►
                              …don't wait — keep the grant already held
                              job phase: send JobExportRequested ──►
                              …don't wait — settle with YESTERDAY's assignments
                              ECONOMY SETTLES (held/old grants)
                              tick complete — ONE uninterrupted run
 [ProcessJobExportRequest] ►  just a FIFO event between ticks (no mid-wait
                              window exists — the starvation bug's entire
                              hazard class is structurally gone)
 [ApplyPowerExportGrant]  ──► plain event: update ledger/world ─┐ take effect
 [ApplyJobExportGrant]    ──► plain event: record assignment  ──┤ at TOMORROW's
                                                                └ settlement
 deleted: both PARK boxes, the runnable-event filter
          (pop_next_runnable_event, runtime/mod.rs:796-819), the Tick #2
          deferral, the mid-wait deadlock rule, all 4 WaitingFor*
          continuation types; WaitingForPowerSettlement (load) collapses
          into the runner's existing drain-until-quiet
 price:   granted TODAY → paid TOMORROW; powered TODAY → effective next
          pass (a region's OWN change becomes one boundary stale — the
          cross-region staleness model, now applied uniformly)
```

The price, and why it is not bundled here: a region's **own** change also
lands one boundary late (a remote job granted today is salaried from
tomorrow's settlement; an imported-power grant applies next pass). That
extends the accepted cross-region staleness to the caller's own tick —
deterministic and likely imperceptible, but a real observable change that
breaks this plan's "dirty path stays identical" promise and re-baselines
cross-region scenario tests. It deserves its own plan, its own review, and
a deliberate gameplay sign-off on losing same-day convergence.

## Risks / notes

- **Re-baselining**: P-1/P-2 should be output-identical (gates only skip
  no-op work). P-3 changes *transient* mid-tick states (fewer flicker
  windows) — any test asserting an intermediate unpowered flash would need
  rework (none known). P-2/P-4/P-5 change how *often* requests/releases
  cross the wire: tests counting per-tick export messages (e.g.
  `region_worker_test.rs` request assertions) will need review; the parity
  suite (`tests/regional_game_parity_test.rs`) is unaffected in principle
  (single-region has no exports) but must be re-run per patch.
- **The gate's false-negative risk is the real one**: a mutation path that
  affects export demand but misses the flag chokepoints re-creates a
  starvation-class bug. Mitigation: flags are set inside
  `invalidate_*`/`mark_*` themselves (not at their call sites), so any
  future mutation that correctly invalidates the registry gets the export
  flag for free; plus one watchdog test per resource that mutates via every
  chokepoint class and asserts the next tick reconciles.
- **Producer-loss convergence latency**: a producer losing capacity reaches
  grant holders via generation-bump → holder's next (hourly power / daily
  job) reconcile → authoritative denial. Power: ≤1 game hour, same as today.
  Jobs: ≤1 game day, same as today.
- **Perf ceiling**: generation coarseness means one busy region keeps all
  regions reconciling — never worse than today's unconditional behavior;
  per-component generations are the named upgrade.
- **Pre-existing snapshot-read timing across workers** is inherited, not
  addressed.

## Patch split (each independently green + reviewable)

```text
P-1  hints_dirty + gated, all-owned-regions hint publishing        (worker.rs, world.rs)
       fixes the stale-hint gap; strictly less compute; output-identical.
P-2  discovery generation + power reconcile gate                    (directory.rs, runtime/mod.rs)
       quiet hourly ticks stop releasing/re-requesting power.
P-3  diff-apply power::run + delete the starvation workaround dance (power.rs, simulation.rs, regions/mod.rs)
       depends on P-2 (kept imports must never coexist with an
       unconditional release); retargets the starvation regression tests.
P-4  job reconcile gate (daily cadence kept)                        (runtime/mod.rs)
P-5  goods reconcile gate                                           (runtime/mod.rs)
P-6  slim begin_tick_power_phase + docs + absorb the two TODOs      (simulation.rs, CLAUDE.md)
```

Dependency graph — arrows mean "must land first"; the annotation on each
node is what a reviewer should verify still holds after that patch:

```text
                 P-1 hints_dirty publishing
                 (verify: quiet pass computes/publishes no hints;
                  event-idle region with dirty hints DOES publish)
                        │
                        │        P-2 generation + power gate
                        │        (verify: quiet tick sends zero
                        │         Release/Request messages; any hint
                        │         publish forces every region exactly
                        │         ONE reconcile)
                        │               │
                        └───────┬───────┘
                                ▼
                 P-3 diff-apply power::run, delete workaround
                 (needs BOTH: P-1 so hint state is trustworthy while
                  imports persist, P-2 so a kept import can never
                  coexist with an unconditional release — the exact
                  desync codex round 1 caught in the starvation fix.
                  does NOT need P-4/P-5: the workaround serves the
                  producer-side process_job_export_request path, which
                  no patch touches — P-4/P-5 only re-gate the
                  CONSUMER-side reconcile cadence. Internally atomic:
                  diff-apply and delete land together (delete without
                  diff-apply re-opens the starvation bug; diff-apply
                  without delete leaves dead restore code).
                  verify: starvation integration test still green;
                  no capture/restore code remains on the tick path)
                                │
                        ┌───────┴────────┐
                        ▼                ▼
                 P-4 jobs gate      P-5 goods gate
                 (mechanical repeats of P-2's shape at
                  enter_job_phase / enter_goods_phase;
                  verify: daily cadence unchanged, only
                  clean-day work is skipped)
                        └───────┬────────┘
                                ▼
                 P-6 slim the tick + docs + absorb TODOs
                 (pure cleanup; verify: a fully quiet tick's
                  only writes are time, turn, and DT2 outputs)
```

Recommended order: exactly as numbered — P-1 and P-2 are independent of each
other but both precede P-3; P-4/P-5 are mechanical repeats of P-2's shape;
P-6 is the victory lap. Estimated sizes: P-1 ~120 lines, P-2 ~150, P-3 ~200
(mostly deletions + test retargeting), P-4/P-5 ~80 each, P-6 ~60.

---

## P-1 — implemented (commit follows this section)

**Files changed:** `src/core/world.rs`, `src/core/regions/mod.rs`,
`src/core/simulation.rs`, `src/core/regions/worker.rs`,
`tests/region_worker_test.rs`. 5 files, ~137 lines.

**What changed, mapped to the design above:**

- `World::hints_dirty: Cell<bool>` added next to `derived_dirty` /
  `road_topology_dirty` (`world.rs`). Set inside `invalidate_resource_registry`,
  `invalidate_jobs_registry`, `mark_road_topology_dirty` — zero new call sites
  for those three, exactly as designed. Accessors `mark_hints_dirty` /
  `is_hints_dirty` / `clear_hints_dirty` mirror the existing
  `*_road_topology_dirty` trio.
- Goods-stock explicit marks at both bypass sites identified in the review
  round: `RegionState::add_commercial_goods` (`regions/mod.rs`) and the daily
  economy settlement in `finish_tick_after_goods_phase` (`simulation.rs`,
  guarded by `phase.is_daily`, matching "one coarse mark per settlement").
- `RegionState::from_world` (the load path) calls `mark_hints_dirty()`
  explicitly right after `refresh_derived_state_for_world` — the plan's
  "load = all-dirty" decision, implemented directly rather than via a custom
  serde default (matching how `derived_dirty` is already forced by
  `from_world` calling the refresh unconditionally, not by its serde default).
- `worker.rs`'s `process_region_events_with_mode`: the old unconditional
  `changed_summaries.push` (fired only for regions with
  `pending_event_count() > 0`) is deleted and replaced with a second sweep
  over **every** owned region, gated on `is_hints_dirty()`, calling
  `ensure_derived_state()` before reading hints (hints read derived state) and
  `clear_hints_dirty()` after publish.

**Anatomy of the change:**

```text
 BEFORE                                    AFTER (P-1)
 ══════                                    ═══════════
 for region in owned:                      for region in owned:
   if 0 pending events: skip                 if 0 pending events: skip
   process events                            process events
   ensure_derived_state()                    ensure_derived_state()
   road-topology gate (unchanged)            road-topology gate (unchanged)
   publish hints UNCONDITIONALLY  ◄─ removed
                                            for region in owned:        ◄ NEW
                                              if !hints_dirty: skip
                                              ensure_derived_state()
                                              publish hints
                                              clear_hints_dirty()

 processed-with-events region:             processed-with-events region:
   always published (whether or not          published ONLY if something it
   anything relevant changed)                did actually dirtied a hint input
                                              (no change ⇒ no publish, no
                                               redundant compute)

 event-idle region (0 events this pass):   event-idle region:
   NEVER published, even if its hints        published if hints_dirty carried
   went stale from an earlier pass           over from an earlier pass —
   (the gap)                                 the gap is closed
```

**Why the fix needed a rewritten test, not just an adapted one.** My first
regression-test draft asserted "the directory ends up with an accurate hint,"
which passed even with the `worker.rs` half of the patch reverted — because
`add_region`'s own one-time initial publish (pre-existing code, unrelated to
P-1) already produces correct content. A tautological test. The corrected
version manually corrupts the directory *after* `add_region` (simulating
drift), then proves the region — event-idle, zero pending events, never
touched by loop 1 — self-corrects the directory on its very next pass. Confirmed
failing on `git stash` of the `worker.rs` half alone, passing restored.

```text
  event_idle_region_republishes_dirty_hints_on_next_pass — what it proves
  ────────────────────────────────────────────────────────────────────────
  add_region(power-plant region)  →  hints_dirty stays true (never cleared
                                      by add_region's own publish)
  directory.publish_region(region, [has_spare_power: false])   ◄ corrupt it
  process_region_events(1)   with ZERO events pushed anywhere
    → processed_regions == 0  (region genuinely event-idle)
    → second sweep still finds hints_dirty == true → recomputes from the
      REAL world state (has_spare_power: true, unchanged) → differs from
      the corrupted directory entry → republishes → self-corrects
  assert: discovery.availability_hints[0].has_spare_power == true  ✓

  Without the fix: nothing ever touches this region again (0 events forever,
  loop 1 skips it) → the corrupted entry would persist. Confirmed via
  git stash on worker.rs alone: test fails without the gate, passes with it.
```

**The `stale_spare_power_hint_routes_to_producer_but_denies_cleanly` test
fix.** A pre-existing test manually publishes a *deliberately* wrong hint to
exercise the architecture's stale-tolerant-discovery / authoritative-grant
property (CLAUDE.md: "a stale-tolerant component graph plus availability hint
picks a candidate producer, and an authoritative request/grant ... reserves
the producer's spare"). Under P-1, a **freshly constructed** region starts
with `hints_dirty = true` (every `attach_*` during test setup sets it), so
the very first worker pass — via the new second sweep — would immediately
correct the producer's hint before the test's deliberately-stale value is
ever read by routing, defeating the scenario the test exists to check. Fixed
by priming the worker with one no-op `process_region_events(1)` pass *before*
manually overwriting the directory, so `hints_dirty` is already clear when
the interesting part of the test begins. Codex reviewed this as a faithful
adaptation, not a workaround: "the new event-idle sweep is expected to
correct dirty initial hints, so priming the worker before manually injecting
a stale directory entry preserves what that test is actually exercising."

**Review:** codex `reviewer` session, two passes (initial patch, then the
strengthened test) — both "No findings." Confirmed stale-tolerant discovery
semantics unchanged: hints remain non-authoritative input; producer-side
grant/deny remains the only authority. `cargo fmt`, `cargo clippy --all-targets
-- -D warnings`, `cargo test -q` all green throughout, including the new
regression test's confirmed fail-before/pass-after cycle.

**Risks / notes carried forward:** none new. This patch only changes
publish *timing* (strictly more frequent/prompt, never less) — it cannot
regress the existing "clean tick does nothing new" property since nothing in
this patch skips previously-mandatory work; it only stops skipping a publish
that should have happened. P-2 is next: the discovery generation counter and
the first reconcile gate that actually skips work on a quiet tick.

---

## P-2 — implemented (commit follows this section)

**Files changed:** `src/core/regions/directory.rs`, `src/core/regions/mod.rs`,
`src/core/regions/runtime/mod.rs`, `src/core/regions/worker.rs`,
`src/core/world.rs`, `tests/region_worker_test.rs`. 6 files, ~216 lines
(slightly over the 5-file soft guideline; directory generation, the gate that
reads it, and the tests proving both are too tightly coupled to split without
leaving broken intermediate states).

**What changed, mapped to the design above:**

- `CrossRegionDiscovery::generation: u64` — bumped once per `rebuild_discovery`
  call, stored on the snapshot itself. `rebuild_discovery`'s signature changed
  from `&DirectoryPublishState` to `&mut DirectoryPublishState` so the counter
  (added to `DirectoryPublishState`, protected by the existing `publish_state`
  mutex — no new lock) can be mutated under the SAME lock all three callers
  already hold; all three call sites (`set_topology`, `publish_region`,
  `publish_region_road_report`) updated to pass `&mut state`.
- `World::power_exports_dirty: Cell<bool>` — set inside
  `invalidate_resource_registry` and `mark_road_topology_dirty` only.
  Deliberately **not** `invalidate_jobs_registry`: jobs and power are
  orthogonal registries (a citizen spawning doesn't change power demand), so a
  per-resource dirty flag must only be set by chokepoints that can actually
  affect that resource — the exact bug the "per-resource seen generation"
  review finding warned about, now honored by construction, not just by the
  seen-marker split.
- `RegionRuntime::discovery_generation` (installed per-slice by the worker,
  mirroring the existing `set_region_routes` pattern) and
  `seen_power_generation` (persistent gate memory, survives across passes,
  starts at 0 so a fresh/loaded runtime always reconciles once).
- `start_tick_power_phase` gate: `dirty = power_exports_dirty ||
  discovery_generation > seen_power_generation`, decided **before** any
  demand collection (the ordering codex's plan-review flagged: once P-3 lands,
  collecting demands before the gate would make a kept import invisible to
  `pending_power_demands`, and a dirty reconcile's release-all would then
  strip its producer reservation with nothing to replace it — the starvation
  fix's round-1 desync, reintroduced). Quiet path:
  `RegionState::begin_tick_power_phase_quiet` — still advances time and runs
  today's raw `power::run` (P-3 makes this a no-op via diff-apply), but wraps
  it in the *existing* capture/reapply-all import-preservation helpers,
  UNFILTERED (safe here because nothing is released on this path, unlike the
  dirty path's filter-by-requestable restore) — then goes straight to
  `enter_job_phase` with zero demand collection and zero release/request
  traffic.

**Anatomy of the gate:**

```text
                         RegionEvent::Tick
                                │
                                ▼
                 start_tick_power_phase (runtime/mod.rs)
                                │
              dirty = power_exports_dirty
                      || discovery_generation > seen_power_generation
                                │
              ┌─────────────────┴─────────────────┐
              │ NO (quiet)                         │ YES (dirty)
              ▼                                    ▼
  begin_tick_power_phase_quiet          seen_power_generation = discovery_generation
    time advance + power::run                clear_power_exports_dirty
    capture imports → reapply ALL         begin_tick_power_demand_phase (unchanged)
    (unfiltered — nothing released)         capture → power::run → collect demands
    power_demands: empty                    → filter-by-requestable → restore
              │                                    │
              ▼                                    ▼
       enter_job_phase directly          reconcile_power_export_allocations
       (no release, no request)            (release-all + request-per-demand,
                                             today's unchanged code)
```

**The "denied import never retries" question — the crux of this patch's test
adaptations.** A consumer whose import request was denied (no candidate
producer had spare capacity) does not get its own independent retry-every-tick
mechanism beyond the gate. This is deliberate, not an oversight: repeating an
identical request against unchanged producer state is guaranteed by
determinism to produce an identical (denied) outcome — so skipping the retry
loses no correctness, only redundant work that could never have changed the
answer. The only two ways the answer *could* change are covered by the gate's
two inputs: this region's own demand/capacity changing
(`power_exports_dirty`), or the discoverable producer landscape changing
(`discovery_generation` moving — bumped by *any* region's hint republish,
including the producer later freeing capacity). Codex confirmed this
reasoning independently after a targeted re-check: "unchanged producer state
plus unchanged caller demand gives the same denial; capacity becoming
available requires some local/cross-region change that bumps the directory
generation or local dirty flag, which reopens the gate."

**Test adaptations (3 pre-existing, all encoding the old "reconcile every
tick unconditionally" assumption) plus 1 new regression test:**

```text
  tick_without_exportable_demand_finishes_immediately
    OLD: asserted outbound[0] == PowerExportAllocationsReleased
    NEW: asserts NO power export traffic at all (an empty, never-touched
         region's first tick correctly goes quiet — even more "finishes
         immediately" than the name originally claimed)

  daily_tick_without_job_demands_releases_job_allocations_before_finishing
    OLD: asserted power_release < job_release < completed across 24 ticks
    NEW: drops the power assertion (power's gate goes quiet after tick 1 in
         this steady-state region; nothing to release starting tick 2) and
         keeps job_release < completed (jobs are ungated until P-4, still
         unconditional every daily tick)

  second_tick_is_deferred_while_waiting
    Tests TickState's deferred-event mechanics directly on a bare
    RegionRuntime, bypassing the worker (so discovery_generation is never
    installed). Without adaptation the second tick would (correctly) go
    quiet, defeating the test's actual subject. Fixed with one line —
    runtime.set_discovery_generation(1) before the second tick — simulating
    what a real worker pass would install if something discoverable changed,
    explicitly isolating tick-state mechanics from the new gate.

  NEW: quiet_power_tick_skips_reconcile_after_first_grant (region_worker_test.rs)
    Real consumer/producer pair via a single worker + directory (not a bare
    RegionRuntime): ticks to a GRANTED import, ticks again with nothing
    changed, asserts pending_events(producer) == 0 (same-worker routing is
    immediate, so a fresh request would show up as a pending event) and the
    grant persists. Confirmed failing (gate forced dirty=true unconditionally)
    before the fix, passing after.
```

**Review:** codex `reviewer` session. First pass: "No findings," though its
summary paragraph initially mis-described the new test (echoing P-1's
test instead) — caught and re-verified with a targeted follow-up naming the
exact test and gate functions; codex re-checked file contents directly and
confirmed "No findings on that P-2 test or the gate path," independently
validating both the gate-ordering and the denied-import reasoning above.
`cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test -q` all
green throughout.

**Risks / notes carried forward:** the `begin_tick_power_demand_phase` name
is intentionally left unchanged even though it's now the *dirty-path-only*
entry point — P-3 is where diff-apply lands and the capture/restore dance
this function still does becomes deletable; renaming now would be premature
churn ahead of that deletion. P-3 is next: diff-apply `power::run` itself,
which removes the need for `begin_tick_power_phase_quiet`'s capture/reapply
wrapper entirely and deletes the starvation-fix workaround on the dirty path
too.

---

## P-3 — implemented (commit follows this section)

**Files changed:** `src/core/systems/power.rs`, `src/core/regions/mod.rs`,
`src/core/simulation.rs`, `src/core/resource_registry.rs`. 4 files, ~212
lines.

**Correction to P-2's closing note:** it said P-3 "deletes the starvation-fix
workaround on the dirty path too." That turned out to be wrong once the
ordering hazard was worked through in full — the dirty path's clear + collect
+ filter-restore dance **survives**, adapted for diff-apply; only the
*quiet*-path wrapper (`begin_tick_power_phase_quiet`) and the *paused-refresh*
capture/restore (`refresh_derived_state_for_world`) are fully deleted. See
below for why the dirty path still needs it.

**What changed:**

- `power::run` (`power.rs`) diff-applies: for each consumer, a fresh matching
  local grant always overwrites (imported or not); otherwise an existing
  `Imported` source is **kept** untouched; otherwise unpowered/`None`. An
  import now survives a raw `power::run` call structurally — no capture
  needed to protect it from a blanket clear that no longer happens.
- `begin_tick_power_phase_quiet` (P-2's quiet path, `regions/mod.rs`): the
  capture/reapply-all wrapper is deleted — it now just calls
  `begin_tick_power_phase` directly, relying on diff-apply.
- `refresh_derived_state_for_world` (the paused build/bulldoze read path,
  `simulation.rs`): same deletion, same reason — a paused command's derived
  refresh no longer needs to protect imports from `power::run`.
- `begin_tick_power_demand_phase` (the **dirty** reconcile path,
  `regions/mod.rs`) **keeps** the capture → clear → collect → filter-restore
  shape, with one change: it now calls the new `clear_imported_power`
  (`simulation.rs`) explicitly, instead of relying on `power::run`'s old
  blanket clear to do it as a side effect.

**Why the dirty path still needs an explicit clear — the ordering hazard
that survived diff-apply.** A dirty reconcile is about to release every
producer reservation and request only what this tick's fresh demand scan
finds. `pending_power_demands` skips any consumer already reading as
`powered`. If diff-apply's "keep" rule were left to run unmodified here, an
existing import would stay `powered=true` straight through `power::run`,
`pending_power_demands` would never include it in the fresh batch, and its
old reservation would still be released moments later — one-sided again,
this time the ledger vanishing but the *local* grant surviving instead of
the reverse (the starvation fix's round-1 shape, just flipped). So
`begin_tick_power_demand_phase` calls `clear_imported_power` on the captured
imports **before** `begin_tick_power_phase` (hence before `power::run`
diff-applies), making them correctly appear as needing a request; the
existing filtered restore afterward — unchanged, still only restoring
consumers that made it into the fresh demand batch (the round-4 fix) —
protects mid-wait reads while the fresh request is in flight, exactly as
before.

```text
  dirty tick, before → after, step by step:

  imported_power_grants(&world)        capture: [(X, demand, region_B)]
       │
       ▼
  clear_imported_power(&mut world, …)   X: powered=false, source=None
       │                                (NEW — power::run no longer
       │                                 does this as a side effect)
       ▼
  begin_tick_power_phase (power::run)   diff-apply sees X with source=None:
                                         no local grant either → stays
                                         unpowered (correctly — nothing
                                         to "keep", nothing local covers it)
       │
       ▼
  pending_power_demands()               X now shows unpowered → included
                                         in this tick's fresh demand batch ✓
       │
       ▼
  filter by requestable, restore        X is in the batch → restored:
                                         powered=true, source=Imported again
                                         (mid-wait reads see X as still
                                         effective while the fresh request
                                         round-trips back to producer B)
```

**The stats gap codex caught.** `power::run`'s stats block computes
`total_power_supplied`/`total_power_shortage` from `resolution` alone —
`ResourceRegistryCache`'s local-only power resolution, which has no concept
of cross-region imports at all (`total_demand` sums every consumer's demand
regardless of source, but `total_supplied` only sums *local* grants). Under
the old code, this was masked: `reapply_imported_power`'s restore step
explicitly added the import's demand back into `total_power_supplied` every
time it ran, and it ran on every call site. After P-3, two of those three
call sites (quiet path, paused refresh) lost their restore step entirely —
so a kept import's demand would silently vanish from citywide stats, showing
a shortage that doesn't exist. Fixed inside `power::run` itself: the
diff-apply loop now accumulates `kept_imported_demand` and adds it into
`total_power_supplied` before the shortage computation — the one place that
now has full visibility into which consumers are kept-imported, regardless
of which call site invoked it.

```text
  BEFORE (bug):                          AFTER (fixed):
  resolution.total_supplied              resolution.total_supplied
    (local grants only)                    + kept_imported_demand
         │                                      │
         ▼                                      ▼
  total_power_supplied UNDER-counts      total_power_supplied correctly
  kept imports → shortage OVER-counts    reflects imports too → shortage
  (visible in UI stats on every quiet    accurate on quiet ticks and the
   tick and every paused refresh)         paused-refresh path
```

**Test changes:**

```text
  resource_registry.rs: cached_jobs_rebuild_when_imported_power_is_lost
    OLD: relied on power::run's own blanket clear to observe an Imported→
         unpowered transition (manually set up, no power plant in the world)
    NEW: calls the real production mechanism directly —
         imported_power_grants + clear_imported_power — before power::run,
         so the test exercises actual code paths instead of a side effect
         of the old (now-gone) unconditional clear

  power.rs: two new unit tests
    diff_apply_keeps_imported_source_when_no_local_grant_exists
      — the core P-3 property. Confirmed fails if power::run is reverted to
        clear-all-then-reapply-local (manually verified via revert/rerun).
        Extended per codex's finding to also assert total_power_supplied ==
        total_power_demand and shortage == 0 — confirmed THIS assertion
        specifically fails if the kept_imported_demand accounting is
        reverted alone, everything else kept
    diff_apply_overwrites_imported_source_with_a_fresh_local_grant
      — a consumer gaining local coverage transitions to Local, not stuck
        showing a stale import. Passes under old code too (this property
        was never broken) — a characterization test, not a P-3-discriminating
        regression proof; noted as such rather than overclaiming
```

**The jobs-registry-invalidation gap found mid-implementation.**
`clear_imported_power`'s clearing now happens *before* `power::run`'s own
before/after snapshot — under the old code, `power::run` was both the
clearer and the detector in one call, so its `power_state_changed` check
always saw the transition. Splitting the clearing out means that check no
longer observes it. A consumer that ends up restored right back nets to no
observable change (and `apply_power_export_grant` invalidates jobs again
regardless, once its round trip resolves) — but one that is *not* restored
(lost its border connection, no longer requestable) has a real, lasting
transition that nothing else would ever flag, leaving a stale "still
effective workplace" answer in the jobs cache indefinitely. Fixed by having
`clear_imported_power` call `world.invalidate_jobs_registry()` itself, once,
whenever it actually clears something — coarse (a restored consumer pays a
redundant invalidation) but correct, matching this plan's established
"false positives are safe, false negatives are not" philosophy for dirty
flags. Codex confirmed this reasoning independently: "closes the real
cache-staleness gap when an import is cleared before `power::run` can
observe the transition."

**Review:** codex `reviewer` session, two passes. First pass: one High
(the stats gap above) and one Low (a comment left describing the dirty
path's dance as deleted when it in fact survives, adapted) — both fixed.
Second pass, after the fixes: "No findings," including explicit
confirmation that the dirty-path clear-before-`power::run` ordering doesn't
double-count a cleared-then-possibly-restored consumer in
`kept_imported_demand`. `cargo fmt`, `cargo clippy --all-targets -- -D
warnings`, `cargo test -q` green throughout, including the full pre-existing
cross-region starvation-fix regression suite (`regional_multi_region_play_test.rs`
and the 3 dedicated unit tests in `regions/mod.rs`), all passing with their
original *meaning* intact, not just green.

**Risks / notes carried forward:** none new. P-4 is next: the job reconcile
gate, mechanically repeating P-2's shape at `enter_job_phase` — no diff-apply
equivalent needed there (jobs have no analogous "clear-then-reapply" system;
`assign_local_jobs_for_daily_tick` already only runs on daily boundaries).
