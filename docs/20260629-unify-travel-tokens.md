# 20260629 — Unify travel tokens (one `TravelToken`, one stepper)

Status: **plan** (not implemented). A token-model **refactor** — one intentional behaviour
change (the cross-region **return now animates**: at off-work a visitor walks workplace→border
home instead of returning instantly, `travel.rs:180`); everything else is behaviour-preserving.
No feature beyond that, no
multi-hop. It collapses the two parallel travel representations into one. The multi-hop
routing work is separate (`docs/20260627-multi-region-return-home.md`) and is *not* part
of this patch; this refactor only settles the token model it will build on.

## 1. Problem

Travel has **two parallel representations** for the same thing — a moving citizen:

```text
  LOCAL citizens                              VISITING tokens (a neighbour's citizen here)
  ─────────────────────────────────          ──────────────────────────────────────────────
  world.travel: HashMap<Entity,TravelState>  world.visiting_travel: HashMap<TravelerId,VisitingToken>
  driven by schedule_intent(citizen)         driven by VisitingToken { token, return_path }
  step_travel()                              step_visiting() / step_visiting_tokens()
```

Every cross-region step **converts** between them (`TravelState` → `VisitingToken` →
`TravelState`), and the return trip rides a stored `return_path` stack. Two maps, two
steppers, a stored route, and a type conversion at every border — all to move one citizen.
(Blast radius today: `components.rs`, `world.rs`, `systems/travel.rs`, `regions/mod.rs`,
`regions/worker.rs`, `interface/adapter.rs`.)

**Goal.** One `TravelToken` in one map, one stepper, the cross-region step a plain *move*
of the same type. Observable behaviour is unchanged **except one intentional fix**: the
cross-region **return now animates** (a visitor walks workplace→border home at off-work
instead of returning instantly, `travel.rs:180`) — the symmetric stepper gives it for free.
Remove `world.visiting_travel`, `VisitingToken`, `return_path`/`ReturnHop`, and
`TravelPurpose{Outbound,Return}`. Keep `TravelState` (reused).

## 2. Proposal

```text
  BEFORE                                       AFTER
  ──────                                       ─────
  world.travel        (Entity → TravelState)   world.tokens (Entity → TravelToken)
  world.visiting_travel (TravelerId → Visiting…)
  step_travel + step_visiting                  ONE stepper
  schedule_intent vs VisitingPurpose           schedule_phase + the token's home/work
  TravelState↔VisitingToken at each border     MOVE the TravelToken (same type)
  return_path stack for the return trip        re-target home (schedule_phase → Home)
```

A `TravelToken` carries the citizen's two endpoints and follows the **city-wide**
`schedule_phase` clock; "go to work" vs "go home" is just which endpoint the phase points at.

```text
  one stepper, two passes (the two halves of today's step_travel):
    DEPART: a resident idle AT HOME (no token, not away) whose phase points elsewhere → new token
    MOVE:   every present token, identical logic —
      phase  = schedule_phase(hour)                       // pure fn of hour (already exists)
      target = if phase == Work { token.work.unwrap_or(token.home) } else { token.home }  // jobless → home
      target.region == self ? walk to target.building (arrive home → remove token; arrive work → idle)
                            : walk to remote_exit_cells[target.region] → at border, MOVE the token
```

**Cross-region = a move, not a conversion.** The handoff carries the **same `TravelToken`
type** into the next region's `world.tokens` — identity (`TravelerId`) preserved, no
`TravelState`↔`VisitingToken` conversion. (Share-nothing ⇒ the moving payload is sent *by
message*, no shared pointer.) A token exists **only while a citizen is away from home** and
lives in the region where the body physically is — so **"away" = a token exists**, "home" = no
token; no `Away` status flag.

```text
  A.tokens[X] ──handoff (MOVE)──► B.tokens[X]      ── X removed from A; body now lives in B
       │ A: X away → tracked by away_residents{X} (the home region's away-record)
       └── on return (phase → Home) it walks home; on arriving, A removes the token (idle, no token)
   away_generation[X] = monotonic trip stamp (bumped on cross-out, NEVER cleared);
   away_residents{X}  = the active-away record (inserted on cross-out, removed on home-arrival).
```

### Explicitly OUT of scope (preserve today's behaviour)

- **No multi-hop.** `remote_exit_cells` stays the current **direct-neighbour** map. The
  stepper routes to a direct neighbour exactly as `step_travel` does today. (Multi-hop
  `region_routes`/`RouteExit`/Dijkstra is the separate routing plan.)
- **No rendering change in meaning** — the adapter reads one map instead of two; dots are
  identical.
- **No schedule change** — `schedule_phase`/`schedule_intent` semantics are unchanged; the
  stepper just reads `schedule_phase` (which `step_visiting` already does for workday-end).

## 3. Important structures / functions

### Removed → one type (`components.rs`, `world.rs`)

```rust
pub struct PlaceRef { pub region: RegionId, pub building: Entity }
pub struct TravelToken {
    pub state: TravelState,        // KEPT verbatim — movement payload the adapter already renders
    pub home: PlaceRef,            // where this citizen lives
    pub work: Option<PlaceRef>,    // where it works (None = jobless → always home)
    pub gen: u32,                  // the active-trip stamp. The HOME region sets it (= bumped
}                                  //   away_generation) on departure; hosts CARRY it unchanged
                                   //   (it was the TravelerId-key on visiting_travel today — Entity-
                                   //   keying moves it onto the token). Handoff TravelerId = {citizen, gen}.
// world.tokens: HashMap<Entity, TravelToken>   // keyed by the CITIZEN entity (globally unique
//   across regions). A token exists ONLY while the citizen is away from home, in the one region
//   where the body is. Idle-at-home = no token. away_residents{} is the home region's active-away
//   record; away_generation is the monotonic trip-stamp guard. No Away/AtHome status.
```

- **Removed:** `world.travel`, `world.visiting_travel`, `VisitingToken`, `return_path`/`ReturnHop`,
  `TravelPurpose{Outbound,Return}`, and **`TravelStatus::{Away, AtHome}`** — a token exists **only
  while a citizen is away from home**, so "away" = *token exists* (no flag) and "at home" = *no token*.
- **Kept:** `TravelState` (now `TravelToken.state`, incl. `building` for the at-work location),
  `TravelStatus::{AtWork, Travelling}`, `TravelerId`, and **`away_generation`** — the **monotonic**
  per-citizen trip counter (bumped on each cross-out, **never cleared**), the stale-return guard.
- **Added:** `away_residents: HashSet<Entity>` — the home region's record of residents currently
  away **across a region boundary** (inserted on cross-out, **removed on home-arrival**). This is
  the "don't re-spawn / is on an active trip" half that `away_generation` (which must stay
  monotonic) can't be; together they disambiguate a cross-region-away resident from an idle/new one.
  *(This is the `Away` status, relocated from a token flag to a home-side set — the home region
  has no token while the body is in the neighbour.)*
- **Handoff** (`components.rs`): `TravelerHandoff` carries the moved `TravelToken` +
  `traveler` + `to_region` + `entry_link: Option<BorderLinkId>` + `kind: {Move, Rollback}`
  (replaces the `Outbound`/`Return` purpose). `Move` = a normal crossing; `Rollback` = today's
  bounce-home fallback (a neighbour couldn't place an inbound token → it sends the citizen home),
  kept, renamed off the purpose enum. (Only the `state` field of the moved token is *not*
  preserved — receive rebuilds it; `home`/`work`/`traveler` carry meaning.)

### One stepper + a local-only front-end (`systems/travel.rs`)

- `step_travel` + `step_visiting`/`step_visiting_tokens` → **one** `step_tokens`. Reuses the
  walk primitives `advance_to_exit` (border) and `depart_to_cell` (building→road) verbatim.
  `advance_to_building` changes its *result only*: since `AtHome` is gone it returns
  `TokenArrival { Walking, ArrivedHome, ArrivedWork }` — `ArrivedHome` ⇒ caller **removes** the
  token + **removes** the citizen from `away_residents`; `ArrivedWork` ⇒ idle in place (`AtWork`,
  `building` set); `Walking` ⇒ keep going. (P7b dwell gate unchanged.)
- **A token exists only while a citizen is away from home** (departed → not yet returned), in
  the region where the body physically is. Idle-at-home = **no token** (today's lazy default).
  Statuses: `AtWork` (idle/parked at a workplace, `current_cell = None`, `building` = workplace)
  and `Travelling`. When a citizen arrives **home**, its token is **removed** (back to idle).
  `building` is kept so a parked-at-work citizen departs *from work* on the Home phase.
- **Stepper = a depart pass + a move pass** (the two halves of today's `step_travel`):
  - **Depart pass** (home region, over `world.citizens`): a resident that is *idle at home* —
    has **no token here** and is **not in `away_residents`** — whose `schedule_phase` target is
    *not* its home building **departs**, taking its first road step (`depart` for a local building
    target, `depart_to_cell` over a reachable border candidate for a remote one). The token is
    created **only if that step succeeds** (a route exists) — no route ⇒ no token (idle at home,
    retried next sub-tick, matching today). A just-departed id is recorded so the move pass skips
    it this sub-tick (one advance). An away resident (in `away_residents`) is *not* re-spawned.
  - **Move pass** (over `world.tokens`, skipping the just-departed): step every present token
    toward its endpoint; on arriving home, **remove** the token (+ `away_residents`); on reaching
    a border exit, **move** it to the neighbour (handoff), and — *if this is the home region* —
    bump `away_generation` (monotonic) and insert into `away_residents`.
- **`NoExit` (no reachable border exit) = stay put** (today's §4b no-teleport), no handoff. The
  never-strand "teleport home" is a *multi-hop routing* feature, not this refactor.
- **Away record + guard (home region):** cross-out **bumps** `away_generation` (monotonic, the
  trip stamp carried on the token as `gen`) and **inserts** `away_residents`; home-arrival
  **removes** `away_residents` (`away_generation` stays — never reused, so a stale older trip
  can't match). A returning `Move`/`Rollback` is accepted only if the citizen **exists**, is
  **absent** here, is **in `away_residents`** (an active trip, not an already-completed one), and
  `traveler.generation == away_generation` — so neither a stale older trip nor a post-completion
  duplicate can re-insert or clobber.

### Receive / drain (`regions/mod.rs`)

- `receive_traveler_handoff`: `kind == Move` at a **host** (foreign home) → just place the token
  at the entry cell into `world.tokens` (the stepper continues it; arrival is "next step,
  `target.region == self`"). `kind == Move` **completing at home** *or* `kind == Rollback` →
  apply the home guard: accept only if the citizen **exists** + is **absent** (no token here) +
  is **in `away_residents`** + `traveler.generation == away_generation` (`apply_traveler_return`
  guards the same, `travel.rs:655`); otherwise **drop** (dead-while-away, post-completion
  duplicate, present token, or stale older trip → no ghost, no clobber). Stale entry cell →
  bounce a `Rollback` home (never drop). `away_residents` is cleared on the eventual home-arrival,
  not on receive (a returning `Move` is placed at the border and still walks home).
- `drain_traveler_handoffs`: `Move`/`Rollback`, no `return_path` push/pop.

### Adapter (`interface/adapter.rs`)

- The traveller-dot builder reads **`world.tokens`** instead of `world.travel` +
  `world.visiting_travel`. Same `CitizenTravelView`/dot output (one source, not two).

## 4. Pseudocode / interaction

```rust
// systems/travel.rs — replaces step_travel + step_visiting. Two passes, dwell-gated (P7b).
fn step_tokens(world) {
    let phase = schedule_phase(world.hour());

    // ── DEPART pass (every region, over its OWN residents in world.citizens): an idle-at-home
    //    resident whose phase target is elsewhere tries to leave NOW. ("Home region" only in the
    //    sense that a region holds the Citizen for its own residents.)
    //    "Idle at home" = no token here AND not in away_residents (so an away resident
    //    — body in the neighbour — is NOT re-spawned; a brand-new/just-returned one IS). The token
    //    is created ONLY if its FIRST step succeeds (a route exists) — matching today: an
    //    unreachable workplace = idle at home, no token, retried next sub-tick. just_departed lets
    //    the move pass skip it this sub-tick (one advance/sub-tick).
    let mut just_departed = HashSet::new();
    for (id, citizen) in world.citizens {
        if world.tokens.contains(id) || away_residents.contains(id) { continue }   // busy / away
        let home = PlaceRef{ region: self.region_id, building: citizen.home };
        let work = citizen.workplace_assignment.map(|a| PlaceRef{ region: a.workplace.region(), building: a.workplace });
        let target = if phase == Work { work.unwrap_or(home) } else { home };      // jobless → home
        if target == home { continue }                                            // stays home → no token
        // first step toward the target: a LOCAL building target → `depart` (handles the entry road
        // adjacent to the building, travel.rs:475); a REMOTE target → `depart_to_cell` over a
        // reachable remote_exit_cells[target.region] candidate. None ⇒ no route ⇒ no token.
        let Some(state) = depart_toward(world, citizen.home, target) else { continue };
        world.tokens.insert(id, TravelToken { state, home, work, gen: 0 });        // already on its first road cell
        just_departed.insert(id);
    }

    // ── MOVE pass: step every present token (except the just-departed, already advanced this sub-tick).
    let mut done = Vec::new();   // (citizen, arrived_home?) — removed after the loop
    for (citizen, token) in world.tokens (sorted by citizen.0) {
        if just_departed.contains(citizen) { continue }
        token.refresh_endpoints_from(world.citizens.get(citizen));   // own resident: re-read home/work
                                                                     // (None for a foreign visitor → no-op)
        let target = if phase == Work { token.work.unwrap_or(token.home) } else { token.home };
        if target.region == self.region_id {
            match advance_to_building(world, token, target.building) {  // → TokenArrival
                Walking | ArrivedWork => {}                            // keep / idle in place (AtWork)
                ArrivedHome           => done.push((citizen, /*home*/ true)),
            }
        } else {
            match advance_to_exit(world, token, &world.remote_exit_cells[&target.region]) {
                Walking | NoExit => {}    // walking / re-picking; NoExit → STAY PUT (§4b no-teleport)
                Reached(rx)  => {
                    // HOME owns the trip stamp: bump the monotonic counter + record away. A host
                    // (foreign home) just CARRIES token.gen and touches no record.
                    let gen = if token.home.region == self.region_id {
                        away_residents.insert(citizen);
                        bump(away_generation, citizen)            // monotonic, never cleared
                    } else { token.gen };
                    let mut moved = token.clone(); moved.gen = gen;
                    push PendingHandoff::Move { traveler: TravelerId{citizen, generation: gen},
                                                token: moved, to_region: rx.to_region, exit_link: rx.link };
                    done.push((citizen, /*home*/ false));   // body left this region → remove the local token
                }
            }
        }
    }
    for (c, arrived_home) in done {
        world.tokens.remove(c);
        if arrived_home { away_residents.remove(c); }   // home → no token; away_generation stays (monotonic)
    }
    // prune a dead local resident's token; keep foreign visitors (home elsewhere)
    world.tokens.retain(|id, t| world.citizens.contains(id) || t.home.region != self.region_id);
    away_residents.retain(|c| world.citizens.contains(c));   // a resident that died WHILE away → drop the record
}

fn home_accepts(c, gen) -> bool {              // the home-completion guard (also used by apply_traveler_return)
    world.citizens.contains(c)                 // not dead-while-away (no ghost)
        && !world.tokens.contains(c)           // not already placed/walking home (no clobber)
        && away_residents.contains(c)          // on an ACTIVE trip (drops a post-completion duplicate)
        && away_generation.get(&c) == Some(&gen)   // the CURRENT trip (monotonic ⇒ drops a stale older trip; .get ⇒ no panic)
}

// apply_traveler_return: the teleport-home fallback. If home_accepts(c, gen) → remove from
// away_residents (the citizen is now home, idle, NO token, no body placed); else no-op (drop).
fn apply_traveler_return(world, traveler) {
    if home_accepts(traveler.citizen, traveler.generation) { away_residents.remove(traveler.citizen); }
}

fn receive_traveler_handoff(h) {
    let c = h.traveler.citizen;
    // RECEIVE-side Rollback = a neighbour bounced this citizen home (its outbound couldn't place).
    if h.kind == Rollback { apply_traveler_return(world, h.traveler); return }
    // A Move RETURNING to the HOME region uses the guard; a Move at a HOST (foreign home) just places.
    if h.token.home.region == self.id && !home_accepts(c, h.traveler.generation) { return }   // drop
    let Some(entry) = h.entry_link.and_then(|l| cell_at_border_link(l.matching_neighbor_link()))
        // entry road gone → bounce a Rollback to the home region. If THIS is the home region (its own
        // entry vanished), it self-bounces: next sub-tick apply_traveler_return clears away_residents,
        // so the abandoned trip is re-departable. (Never drop the traveller.)
        else { push PendingHandoff::Rollback { traveler: h.traveler, to_region: h.token.home.region };
               return };
    // (away_residents stays set for a returning Move — the token is placed at the border and still
    //  walks home; it is removed only on the eventual home-arrival in the move pass.)
    let mut t = h.token;                                         // the moved TravelToken (home/work/gen carry meaning)
    t.gen = h.traveler.generation;                              // remember the trip stamp for the eventual return
    t.state = TravelState { status: Travelling, current_cell: Some(entry),          // state is REBUILT here,
                            destination: None, building: None, dwell: 0, prev_cell: None };  // not preserved
    world.tokens.insert(c, t);   // body now present here; next step_tokens targets from t.home/t.work
}
```

Interaction: the routing (`remote_exit_cells`, direct-neighbour), the transport
(`RegionEvent::ReceiveTraveler`/`StepTravel`, `route_traveler_handoff`, the barrier), and
the walk primitives are **reused unchanged**. Only the token *type/map/stepper* change.

## 5. Tests

- `local_commute_unchanged` — a home↔local-work commute walks/arrives identically to P3
  (golden: same cell sequence, same arrival tick).
- `direct_neighbour_outbound_unchanged` — A→B *outbound* commute crosses and arrives at work
  with the same timing as today (the behaviour-preserving anchor for the unchanged half).
- `return_now_animates` — at off-work a visitor in B walks workplace→B/A border before crossing
  (the one intentional change), instead of today's instant `Return`; the home-side walk is as today.
- `move_handoff_carries_token_no_return_path` — the crossing emits `kind: Move` carrying the
  `TravelToken`; assert no `return_path`.
- `phase_flip_retargets_home` — a token AtWork in B, `schedule_phase` flips to Home → the same
  stepper departs it toward home (no special workday-end path).
- `crossed_out_token_removed_from_home` — when a resident crosses out, its token is **removed**
  from the home region (absent, not an `Away` stub) and present in the neighbour; the home
  stepper/adapter no longer sees it (and a returning `Move` re-inserts it).
- `stale_older_trip_dropped_by_monotonic_generation` — after trip 1 returns, trip 2 bumps
  `away_generation` (monotonic, not cleared); a stale trip-1 `Move`/`Rollback` (gen 1) is dropped.
- `post_completion_duplicate_dropped_by_away_residents` — a duplicate of the *current* trip
  arriving after the citizen reached home (removed from `away_residents`) is dropped — generation
  alone would match; `away_residents` membership is what rejects it.
- `jobless_goes_home_in_work_phase` — a token with `work == None` (or cleared mid-day) targets
  `home` during the Work phase (matches `schedule_intent` today), not skipped/stranded.
- `mid_day_job_reassign_reroutes` — the move pass re-reads `work` from `world.citizens` each
  tick (`refresh_endpoints_from`), so a reassignment mid-day routes to the new workplace.
- `depart_creates_token_only_if_route` — an idle-at-home resident (no token, not in
  `away_residents`) whose phase points at work departs and gets a token that advances **once**
  this sub-tick (no double-step); an **unreachable** workplace creates **no token** (idle, retried);
  an **away** resident (in `away_residents`) is **not** re-spawned.
- `arrive_home_removes_token_and_away_residents` — `TokenArrival::ArrivedHome` removes the token
  and removes the citizen from `away_residents` (idle-at-home = no token; `away_generation` stays);
  `ArrivedWork` keeps it idle (`AtWork`).
- `no_exit_stays_put_not_teleport` — a token whose only border exit became unreachable stays on
  its cell (no handoff, no teleport home) — today's §4b behaviour.
- `idle_at_work_remembers_location` — a token idle `AtWork` keeps `building = workplace`, so the
  Home phase departs *from work*, not from home (the `building`-retention regression).
- `rollback_re_homes_by_presence_and_generation` — a `Rollback` reaching home re-inserts the
  citizen at home only if it exists + was absent + the generation matches (no ghost, no clobber).
- `host_carries_generation_on_return` — a token's `gen` is set by the home region on departure
  and carried unchanged by the host, so the return `Move` matches the home `away_generation` guard.
- `dead_while_away_prunes_away_residents` — a resident that dies while cross-region-away is
  dropped from `away_residents` (paired retain), and a late return for it is dropped by `home_accepts`.
- `dots_render_from_one_map` — the adapter draws the same traveller dots reading `world.tokens`.
- Re-baseline the existing `step_visiting`/`return_path` tests onto the unified stepper.

## 6. Risks / non-goals

- **One intentional behaviour change: the return now animates** (a visitor walks workplace→
  border home at off-work, rather than today's instant `Return`). This is the long-standing
  "return has no animation" gap, folded in because the symmetric stepper gives it for free.
  Everything else is the **bar = behaviour-preserving** — anchor the *outbound* + local commute
  with golden cell/timing tests; the return test asserts the new walk-home.
- It refactors *working* direct-neighbour code; its larger payoff is being the clean base for
  the deferred multi-hop routing. Worth it for one model; flagged as churn on working code.
- **Dead-while-away cleanup:** `away_residents` is pruned with `world.citizens` (above), but a
  *foreign visitor's* token in the host leaks if its home citizen dies while it is `AtWork` there
  (the host can't see the foreign `Citizen`). This already exists today (`visiting_travel` has no
  cross-region death signal); leave it as a pre-existing ceiling — *(`ponytail:` host can't see a
  foreign citizen's death; reap on a return that fails `home_accepts`, or in the routing patch.)*
- **Out of scope:** multi-hop routing, the L1 registry, cost-weighting, the directory locking
  change — all in `docs/20260627-multi-region-return-home.md` (routing-only after this lands).
- No new worker command/protocol; no new dependency; determinism unchanged (tokens stepped in
  citizen `Entity` order); population untouched (derived from `world.citizens`, never the token map).

## Suggested patch split

- **R-a:** introduce `TravelToken`/`PlaceRef`/`world.tokens` + `away_residents` + the two-pass
  stepper + the home-region front-end (*behaviour-preserving except the animated return*);
  adapter reads `world.tokens`; golden + unified tests.
- **R-b:** delete `world.visiting_travel` / `VisitingToken` / `return_path` / `ReturnHop` /
  `TravelPurpose{Outbound,Return}` and their construction sites once nothing references them.

---

## 7. R-a implementation record

R-a was implemented in commit `[r-a]`. The patch collapses `world.travel` and
`world.visiting_travel` into one `world.tokens: HashMap<Entity, TravelToken>`,
backed by `away_residents: HashSet<Entity>` (the home region's active-away
record) and `away_generation: HashMap<Entity, u32>` (the monotonic trip
stamp). The two maps + two steppers + one type conversion at every border are
gone; the new model is one map + one stepper + one move at the border.

### 7a. The new types (in `components.rs`)

```text
  TravelState { AtWork, Travelling }             // no Away, no AtHome
  TravelToken { state, home: PlaceRef, work: Option<PlaceRef>, trip_gen: u32 }
  PlaceRef { region: RegionId, building: Entity }
  TravelerId { citizen: Entity, generation: u32 }    // still the handoff identity
  PendingHandoff::{ Move { traveler, token, to_region, exit_cell },
                    Rollback { traveler, to_region } }
  TravelerHandoff { token, traveler, to_region, entry_link: Option<BorderLinkId>,
                    kind: HandoffKind::{ Move, Rollback } }
  HandoffKind::{ Move, Rollback }                  // replaces TravelPurpose
```

Removed: `world.travel`, `world.visiting_travel`, `VisitingToken`, `return_path`/
`ReturnHop`, `TravelPurpose{Outbound,Return}`, `TravelStatus::{Away, AtHome}`.
`TravelState` is reused (its `building` field still records the at-work
location). `TravelerId` is reused. `HandoffKind` replaces `TravelPurpose`.

### 7b. The two-pass stepper (in `systems/travel.rs`)

```text
  step_tokens(world):
    // ── DEPART pass: every resident in this region with no token here AND not
    //    in away_residents whose phase target is elsewhere → first-step toward
    //    target. Token created ONLY if a route exists (no route ⇒ no token).
    for (id, citizen) in world.citizens:
        if world.tokens.contains_key(id) || away_residents.contains(id): continue
        if target == home: continue
        let Some(state) = depart_toward(world, &networks, citizen.home, target) else continue
        world.tokens.insert(id, TravelToken { state, home, work, trip_gen: 0 })
        just_departed.insert(id)

    // ── MOVE pass: every present token (except the just-departed).
    //    dwell gate is checked INSIDE advance_*_building / advance_to_exit
    //    BEFORE advancing — remote trips get the same dwell as local ones.
    for (citizen, token) in world.tokens (sorted by citizen.0):
        if just_departed.contains(citizen): continue
        refresh_endpoints_from(&mut token, world.citizens.get(&citizen))  // None for foreign
        let target = if phase == Work { token.work.unwrap_or(token.home) } else { token.home }
        match target.region == self_region {
            true => apply advance_to_building,
            false => apply advance_to_exit,
        }

    // Cleanup: remove arrived-home, update others, drain handoffs.
    for c in removes: world.tokens.remove(c); if arrived_home: away_residents.remove(c)
    for h in handoffs: world.outgoing_handoffs.push(h)
    world.tokens.retain(|id, t| world.citizens.contains_key(id) || t.home.region != self_region)
    away_residents.retain(|c| world.citizens.contains_key(c))  // dead-while-away cleanup
```

The depart pass collects `fresh_tokens` first and writes them at the end (to
avoid borrow conflicts on `world.tokens` mid-iteration); the move pass similarly
collects `updates` / `removes` / `handoffs` and applies them after the loop.

### 7c. The `home_accepts` guard (used by `apply_traveler_return` and the receive side)

```rust
fn home_accepts(c, gen) -> bool {
    world.citizens.contains(c)     // not dead-while-away
        && !world.tokens.contains(c)  // not already walking home
        && away_residents.contains(c) // on an active trip (not post-completion)
        && away_generation.get(&c) == Some(&gen)  // current trip, not stale older
}
```

The paired `retain` after the move pass keeps `away_residents` and
`world.tokens` consistent with `world.citizens`. A resident that dies WHILE
away is dropped from both, and a late `Move`/`Rollback` for it is dropped by
`home_accepts` (no ghost).

### 7d. Cross-region: one move, no conversion

```text
  A: token reaches exit_cell → emit PendingHandoff::Move { traveler, token, ... }
  Worker: route_traveler_handoff → emit TravelerHandoff { kind: Move, ... } (or Rollback)
  B: receive_traveler_handoff (kind == Move) → place token at entry cell → stepper continues
  B: receive_traveler_handoff (kind == Rollback) → apply_traveler_return → home_accepts → if true, clear away_residents
```

The `Rollback` kind handles two cases: the home's own entry vanished (self-bounce,
clear `away_residents` locally) and a neighbour's exit became unroutable
(emit a `Rollback` handoff to the home, which clears `away_residents` there).

### 7e. The animated return (the one intentional change)

Today (P5a), a visiting token in B at workday end emits an instant `Return`
handoff (the token disappears from B's `visiting_travel`, the home applies
`apply_traveler_return`). In R-a, the visitor walks workplace → border home
sub-tick by sub-tick: B's `step_tokens` calls `advance_to_exit` on the
foreign token with `target = token.home`, which walks the token toward a
border-exit cell; on the border cell, a `Move` handoff is emitted to A; A
places the token at its entry cell and walks it home in subsequent sub-ticks.
The home-phase stepper in A walks the token from border → home. All `dwell`
gating applies (a 4-way on the return path holds for 4 sub-ticks).

### 7f. Tests added (276 total, 14 new for R-a)

- `local_commute_unchanged` — home→work walks exact r0..r3, arrives AtWork.
- `dwell_holds_a_traveller_on_a_4way` — local 4-way holds 4 sub-ticks.
- `remote_dwell_holds_a_visitor_on_a_4way` — foreign 4-way holds 3 sub-ticks (cost-1; the placement sub-tick isn't counted because the test pre-places the token).
- `phase_flip_retargets_home` — Home phase re-targets the token (the P3 behaviour is preserved).
- `crossed_out_token_removed_from_home` — cross-out removes the local token (no Away stub).
- `stale_older_trip_dropped_by_monotonic_generation` — stale older trip is dropped.
- `post_completion_duplicate_dropped_by_away_residents` — post-completion duplicate is dropped (generation alone would match; `away_residents` rejects).
- `jobless_goes_home_in_work_phase` — jobless → home in Work phase.
- `mid_day_job_reassign_reroutes` — reassigned workplace routes from the OLD workplace (not teleport to new).
- `depart_creates_token_only_if_route` — no token for an unreachable workplace; away resident NOT re-spawned.
- `arrive_home_removes_token_and_away_residents` — ArrivedHome removes token + `away_residents`.
- `no_exit_stays_put_not_teleport` — disconnected road network → no token, no handoff.
- `dead_while_away_prunes_away_residents` — `away_residents` pruned for dead citizens.
- `host_walks_visitor_home_animation` — foreign visitor walks workplace → border at off-work.
- `return_now_animates` — home-side walk after a Move is received.
- `idle_at_work_remembers_location` — reassigned worker departs from OLD workplace, not new.
- Adapter: `traveler_views_includes_visiting_tokens`, `traveler_views_excludes_removed_citizen`.
- Regions: `drain_move_resolves_border_link`, `drain_move_rolls_back_when_unroutable`, `receive_move_at_host_places_token`, `receive_rollback_clears_away`, `traveler_handoff_routes_to_destination_inbox`, `border_neighbor_map_maps_edge_to_neighbor`.

### 7g. Diagrams

**Before (P5a, two maps + two steppers + type conversion at every border):**

```text
  A (home):  world.travel[citizen] = TravelState  (idle/Away/Travelling)
            step_travel()  →  schedule_intent + walk + cross
            on cross: insert TravelState{Away, ...} into world.travel
                       push PendingHandoff::Outbound { ..., return_path = [] }

  Wire:     TravelerHandoff { token: TravelState, return_path: Vec<ReturnHop>,
                             purpose: TravelPurpose::Outbound }

  B (host): visiting_travel[TravelerId] = VisitingToken { token, return_path }
            step_visiting()  →  walk to workplace
            on workday end: push PendingHandoff::Return { return_path }
                            remove from visiting_travel  (instant!)
            on receive Return: apply_traveler_return (Away + gen check, clear)
```

**After (R-a, one map + one stepper + one move at the border):**

```text
  A (home):  world.tokens[citizen] = TravelToken { state, home, work, trip_gen }
            (present iff away-from-home; idle-at-home = no token)
            away_residents[citizen] = true iff on an active cross-region trip
            away_generation[citizen] = monotonic trip stamp
            step_tokens()  →
                DEPART pass: resident without token & not in away_residents
                             whose phase target ≠ home → depart (or no-op)
                MOVE pass:   every present token → walk to target (one cell)
                             dwell gate (cost = step_cost) before advancing
                on ArrivedHome: remove token, remove from away_residents
                on Reached(exit): emit PendingHandoff::Move { ..., trip_gen =
                                bumped away_generation, away_residents inserted }

  Wire:     TravelerHandoff { token: TravelToken, traveler, to_region,
                             entry_link: Option<...>, kind: HandoffKind::Move }

  B (host): world.tokens[citizen] = same TravelToken (placed at entry cell)
            step_tokens()  →
                MOVE pass:   token with home.region != self
                             → advance_to_exit (target = token.home)
                             → walk to border-exit, then Reached(exit) → Move
            on receive Move: home_accepts (false for post-completion dup)
                or place at entry cell (host case)

  Animated return (one intentional change):
    B: visitor's target = token.home; B's stepper walks the visitor
       workplace → border (sub-tick by sub-tick, with dwell gating).
    B: at the border cell → Reached(exit) → emit Move to A.
    A: receive_traveler_handoff (Move) → home_accepts check → place at entry.
    A: phase = Home → MOVE pass walks border → home.
    A: at home → ArrivedHome → remove token, remove from away_residents.
```

**The stale-return guard (home_accepts, four-part check):**

```text
  home_accepts(c, gen) =
      c ∈ world.citizens                          (not dead-while-away)
   && c ∉ world.tokens.keys()                    (not already placed/walking home)
   && c ∈ away_residents                          (on an ACTIVE trip)
   && away_generation[c] == Some(gen)             (the CURRENT trip, not stale older)
```

- Dead-while-away: c removed from `world.citizens` → guard rejects.
- Stale older trip: gen doesn't match the current `away_generation` → guard rejects.
- Post-completion duplicate: c removed from `away_residents` on home-arrival → guard rejects.
- Already-walking-home: c in `world.tokens` (a Move just placed it) → guard rejects.

Paired `retain` after each `step_tokens` keeps `away_residents` and
`world.tokens` consistent with `world.citizens`.
