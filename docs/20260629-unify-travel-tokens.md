# 20260629 — Unify travel tokens (one `TravelToken`, one stepper)

Status: **plan** (not implemented). A **behaviour-preserving refactor** — no feature, no
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
of the same type — **with identical observable behaviour** (direct-neighbour commuting,
the dots, the timing all unchanged). Remove `world.visiting_travel`, `VisitingToken`,
`return_path`/`ReturnHop`, and `TravelPurpose{Outbound,Return}`. Keep `TravelState` (reused).

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
  one stepper, every token identical:
    phase  = schedule_phase(hour)                         // pure fn of hour (already exists)
    target = if phase == Work { token.work.unwrap_or(token.home) } else { token.home }  // jobless → home
    target.region == self ? walk to target.building (arrive, idle)
                          : walk to remote_exit_cells[target.region] → at border, MOVE the token
```

**Cross-region = a move, not a conversion.** The handoff carries the **same `TravelToken`
type** into the next region's `world.tokens` — identity (`TravelerId`) preserved, no
`TravelState`↔`VisitingToken` conversion. (Share-nothing ⇒ the active moving payload is sent
*by message*, no shared pointer; the home region keeps only a thin `Away` mark — the moving
token now lives in the next region.)

```text
  A.tokens[X] ──handoff (MOVE)──► B.tokens[X]      ── same TravelToken, identity preserved
       │ A's token for X stays, status = Away (away_generation guards stale returns)
       └── on return (phase → Home) the move runs in reverse → A clears Away, X idles AtHome
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
}
// world.tokens: HashMap<Entity, TravelToken>   // keyed by the CITIZEN entity (globally unique
//   across regions), one token per citizen — matches world.travel today. The generation lives in
//   away_generation (the guard), NOT the key; TravelerId{citizen,gen} is only the handoff identity.
```

- **Removed:** `world.travel`, `world.visiting_travel`, `VisitingToken`, `return_path`/`ReturnHop`,
  `TravelPurpose{Outbound,Return}`.
- **Kept:** `TravelState` (now `TravelToken.state`, incl. `building` for the idle location),
  `TravelerId`, `TravelStatus` (incl. `Away`), and **`away_generation`** (the stale-return
  guard — unchanged).
- **Handoff** (`components.rs`): `TravelerHandoff` carries the moved `TravelToken` +
  `traveler` + `to_region` + `entry_link: Option<BorderLinkId>` + `kind: {Move, Rollback}`
  (replaces the `Outbound`/`Return` purpose). `Move` = a normal crossing; `Rollback` = today's
  bounce-home fallback (a neighbour couldn't place an inbound token → it sends the citizen home
  to clear `Away`), kept, just renamed off the purpose enum. (Only the `state` field of the
  moved token is *not* preserved — receive rebuilds it; `home`/`work`/`traveler` carry meaning.)

### One stepper + a local-only front-end (`systems/travel.rs`)

- `step_travel` + `step_visiting`/`step_visiting_tokens` → **one** `step_tokens`. Reuses the
  walk primitives unchanged: `advance_to_building` (arrive/idle), `advance_to_exit` (border),
  `depart_to_cell` (building→road), and the **P7b dwell gate**.
- **Token lifecycle = citizen lifecycle (no spawn/despawn).** Just like `world.travel` today,
  a local citizen **always** has a token; its `state.status` tracks where it is —
  `AtHome` / `AtWork` (idle, `current_cell = None`, `building` = the occupied building) /
  `Travelling` / `Away`. The **initial token is added where the citizen is created** (the same
  spawn hook that seeds `world.travel` today, e.g. `citizens`/`population`) and pruned with it
  (`retain(|id| world.citizens.contains(id))`). **`building` is kept** so an idle citizen's
  location (home vs work) is remembered — *not* re-inferred from `home`/`work`, which would
  mis-place a citizen at work when the phase says Home.
- **`NoExit` (no reachable border exit) = stay put** (today's §4b no-teleport), emitting no
  handoff. The never-strand "teleport home" is a *multi-hop routing* feature, **not** this
  behaviour-preserving refactor.
- **Home-region front-end** = the *front half of today's `step_travel`*, only the home region
  has the `Citizen`: each tick, for each resident, read `schedule_phase` → set the token's
  target endpoint (`home`/`work` from `world.citizens`), unchanged from `resolve_target`
  today. **`away_generation` is KEPT** (its `TravelerId.generation` stale-return guard is
  unchanged): a new outbound bumps the generation, and a `Return`/`Rollback` is applied only
  if it matches the current `Away` mark — so a duplicate/stale return can't clear a newer trip.

### Receive / drain (`regions/mod.rs`)

- `receive_traveler_handoff`: `kind == Move` → place the token at the local entry cell and
  let the shared stepper continue it (no `Outbound`/`Return`/arrival branches — arrival is
  just "next step, `target.region == self`"); `kind == Rollback` → `apply_traveler_return`,
  which **guards internally** (still-`Away` + generation match, `travel.rs:655`) — *no second
  guard needed*. Stale entry cell → bounce a `Rollback` home (never drop). **Generation guard
  on `Move`:** a `Move` whose home is this region is accepted only if `traveler.generation`
  matches the current `away_generation` mark — a stale-generation return is **dropped** (else
  it would insert a duplicate token for the citizen).
- `drain_traveler_handoffs`: `Move`/`Rollback`, no `return_path` push/pop.

### Adapter (`interface/adapter.rs`)

- The traveller-dot builder reads **`world.tokens`** instead of `world.travel` +
  `world.visiting_travel`. Same `CitizenTravelView`/dot output (one source, not two).

## 4. Pseudocode / interaction

```rust
// systems/travel.rs — replaces step_travel + step_visiting. One pass, dwell-gated (P7b).
fn step_tokens(world) {
    refresh_resident_tokens(world);   // home region only (see below)

    let phase = schedule_phase(world.hour());
    for (citizen, token) in world.tokens (sorted by citizen.0) {
        if token.state.status == Away { continue }               // home-side Away mark (away_generation)
        // jobless / cleared-work in the Work phase → home (matches schedule_intent today).
        let target = if phase == Work { token.work.unwrap_or(token.home) } else { token.home };

        if target.region == self.region_id {
            advance_to_building(world, token, target.building);  // endpoint here → walk & idle (sets building)
        } else {
            match advance_to_exit(world, token, &world.remote_exit_cells[&target.region]) {
                Walking      => {}                               // still walking / re-picking (P7b dwell)
                NoExit       => {}                               // no reachable exit → STAY PUT (today's
                                                                 //   §4b no-teleport; never-strand is the
                                                                 //   multi-hop routing plan, not this refactor)
                Reached(rx)  => {
                    // BUMP FIRST, then both the handoff and the home Away mark use the new gen,
                    // so the legitimate return Move matches the guard. (As today's step_travel.)
                    let gen = bump(away_generation, citizen);
                    push PendingHandoff::Move { traveler: TravelerId{citizen, generation: gen},
                                                token: token.clone(), to_region: rx.to_region, exit_link: rx.link };
                    token.state = away_state();   // status = Away (the home mark; away_generation[citizen]=gen)
                }
            }
        }
    }
}

// home region only: read each RESIDENT's home/work from world.citizens (so a mid-day job
// reassignment routes correctly), and prune a token whose Citizen died. Away/foreign tokens
// are not refreshed (their host has no Citizen). NO spawn/despawn — the initial token is added
// when the citizen is created (citizens::spawn) and removed with it; this only refreshes.
fn refresh_resident_tokens(world) {
    for (id, citizen) in world.citizens {
        let t = world.tokens.entry(id);
        t.home = PlaceRef{ region: home_region, building: citizen.home };
        t.work = citizen.workplace_assignment.map(|a| PlaceRef{ region: a.workplace.region(), building: a.workplace });
    }
    // keep a local resident's token (Citizen still alive) OR a foreign visitor's (home elsewhere)
    world.tokens.retain(|id, t| world.citizens.contains(id) || t.home.region != self.region_id);
}

fn receive_traveler_handoff(h) {
    let c = h.traveler.citizen;
    // RECEIVE-side Rollback = a neighbour bounced this citizen home (its outbound couldn't place):
    // apply_traveler_return already guards internally (still-Away + generation match) — do NOT add
    // a second guard. (Distinct from the LOCAL stay-put above, which emits no handoff.)
    if h.kind == Rollback { apply_traveler_return(world, h.traveler); return }
    // A Move completing at the HOME region: drop a stale-generation one (it would insert a duplicate).
    if h.token.home.region == self.id && away_generation[c] != h.traveler.generation { return }
    let Some(entry) = h.entry_link.and_then(|l| cell_at_border_link(l.matching_neighbor_link()))
        else { // entry road gone → bounce a Rollback to the HOME region (never drop)
               push PendingHandoff::Rollback { traveler: h.traveler, to_region: h.token.home.region };
               return };
    let mut t = h.token;                                         // the moved TravelToken (home/work/traveler matter)
    t.state = TravelState { status: Travelling, current_cell: Some(entry),          // state is REBUILT here,
                            destination: None, building: None, dwell: 0, prev_cell: None };  // not preserved
    world.tokens.insert(c, t);   // keyed by citizen entity; next step_tokens targets from t.home/t.work
}
```

Interaction: the routing (`remote_exit_cells`, direct-neighbour), the transport
(`RegionEvent::ReceiveTraveler`/`StepTravel`, `route_traveler_handoff`, the barrier), and
the walk primitives are **reused unchanged**. Only the token *type/map/stepper* change.

## 5. Tests

- `local_commute_unchanged` — a home↔local-work commute walks/arrives identically to P3
  (golden: same cell sequence, same arrival tick).
- `direct_neighbour_outbound_and_return_unchanged` — A↔B commute crosses, works, and returns
  home with the same timing as today (the behaviour-preserving anchor).
- `move_handoff_carries_token_no_return_path` — the crossing emits `kind: Move` carrying the
  `TravelToken`; assert no `return_path`.
- `phase_flip_retargets_home` — a token AtWork in B, `schedule_phase` flips to Home → the same
  stepper departs it toward home (no special workday-end path).
- `away_token_marked_not_stepped` — a resident whose token crossed out has `status == Away` in
  the home region and is skipped by the stepper until it returns.
- `stale_return_does_not_clear_newer_trip` — re-baseline today's `away_generation` guard onto
  `world.tokens`: a duplicate/stale `Return`/`Move` (old generation) at the home region is
  dropped — it must not reset an active trip or insert a duplicate token.
- `jobless_goes_home_in_work_phase` — a token with `work == None` (or cleared mid-day) targets
  `home` during the Work phase (matches `schedule_intent` today), not skipped/stranded.
- `mid_day_job_reassign_reroutes` — `refresh_resident_tokens` re-reads `work` from
  `world.citizens` each tick, so a reassignment mid-day routes to the new workplace (the
  behaviour-preserving anchor for the work-refresh).
- `no_exit_stays_put_not_teleport` — a token whose only border exit became unreachable stays on
  its cell (no handoff, no teleport home) — today's §4b behaviour.
- `idle_at_work_remembers_location` — a token idle `AtWork` keeps `building = workplace`, so the
  Home phase departs *from work*, not from home (the `building`-retention regression).
- `severed_route_rolls_back_not_strands` — no progressing exit → `Rollback` → home clears Away.
- `dots_render_from_one_map` — the adapter draws the same traveller dots reading `world.tokens`.
- Re-baseline the existing `step_visiting`/`return_path` tests onto the unified stepper.

## 6. Risks / non-goals

- **Behaviour-preserving is the bar.** Anchor with golden tests for the direct-neighbour
  commute (cells + timing) before/after.
- It refactors *working* direct-neighbour code; its larger payoff is being the clean base for
  the deferred multi-hop routing. Worth it for one model; flagged as churn on working code.
- **Out of scope:** multi-hop routing, the L1 registry, cost-weighting, the directory locking
  change — all in `docs/20260627-multi-region-return-home.md` (routing-only after this lands).
- No new worker command/protocol; no new dependency; determinism unchanged (tokens stepped in
  citizen `Entity` order); population untouched (derived from `world.citizens`, never the token map).

## Suggested patch split

- **R-a:** introduce `TravelToken`/`PlaceRef`/`world.tokens` + the one stepper + the home-region
  front-end, *behaviour-preserving*; adapter reads `world.tokens`; golden + unified tests.
- **R-b:** delete `world.visiting_travel` / `VisitingToken` / `return_path` / `ReturnHop` /
  `TravelPurpose{Outbound,Return}` and their construction sites once nothing references them.
