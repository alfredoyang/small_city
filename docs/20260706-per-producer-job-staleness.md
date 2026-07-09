# Per-producer staleness for remote jobs — stop wiping workers whose producer never changed

Status: **plan** (not implemented). Builds on P-c of
[20260704-retire-tickstate.md](20260704-retire-tickstate.md). Not a bug
fix — P-c's rule is correct, just blunt.

## The problem, in one picture

Today, "did anything change?" is answered by one shared switch for the
whole world:

```text
 region A imports remote workers from B (5 people) and C (2 people)

 ANYTHING changes ANYWHERE  →  one shared switch flips to "changed"
   - C builds a road                    ┐
   - a random region sells goods        ├─ all flip the SAME switch
   - power shifts in a region far away  ┘

 A sees the switch flipped → A can't tell WHAT changed → A assumes the worst
   → wipes ALL 7 workers, asks again for all 7
   → the 5 people who work for B (who never changed) go jobless-for-a-day
     for absolutely no reason
```

It's one alarm bell for the whole city. Any building anywhere ringing it
wipes every region's remote workers, everywhere — even the ones whose real
employer never touched a thing.

**Why this matters more than it sounds:** goods-trading regions ring this
bell *every single day* automatically (selling goods changes a public
number). So in a lively city, the bell never really goes quiet — remote
workers get wiped and re-hired daily, forever, exactly the thing P-c was
supposed to stop.

## The fix, in one picture

Give every region its **own** small bell, that only rings for things that
actually matter to jobs:

```text
 TODAY:  one bell for the whole city, rings for anything

 AFTER:  one small bell PER region, rings only for:
           - a border road appears/disappears (can I even reach them?)
           - their list of open jobs changes (did a slot open or close?)
         does NOT ring for:
           - goods stock changing
           - power capacity changing

 A checks only the bells of the regions it actually depends on:
   B's bell rang since I last checked?  → re-verify B's workers only
   C's bell never rang?                 → leave C's workers alone
```

That's the whole idea. A only listens to the producers it actually has
workers at, not to the whole city.

## What still happens the same way

When a bell *does* ring, the response is unchanged: a full wipe, re-match
locally, re-ask remotely — exactly like today. This plan only makes the
**decision** ("should I bother?") sharper. It does not try to make the
**response** partial (wipe only the affected worker). That's tempting but
turns out to be unsafe — see below.

```text
 dumber question, expensive answer          sharper question, SAME answer
 ───────────────────────────────           ──────────────────────────────
 "did anything change?"                     "did MY producers change?"
 yes → wipe everyone                        yes → wipe everyone (unchanged)
 (fires constantly)                         (fires rarely — the whole point)
```

## Why we can't ALSO make the wipe partial (the trap)

It's tempting to go further: "just re-verify the ONE producer that
changed, leave the rest completely alone, don't even wipe them." That
sounds strictly better. It isn't — here's the trap:

```text
 the producer's own bookkeeping works like this:
   "whenever caller A sends me a NEW request, throw away ALL of A's
    OLD reservations, no matter which one this new request is about"

 so if A tries to keep some workers and only re-ask about one:

   day 1:  A has workers at B and C, both "generation 5"
   day 9:  A new citizen shows up, A sends ONE new request (generation 9)
           — but A doesn't get to choose who answers it; it happens to
             route to C
   C sees a generation-9 request from A
     → C throws away ALL of A's generation-5 reservations at C
     → including the untouched worker C was supposed to keep!
   that worker is now employed with NOTHING reserving their slot
     → C can hand that same slot to somebody else too → two people, one job
```

Today this can't happen, because EVERY wipe re-asks about EVERYONE, so
every reservation always gets renewed together, as one batch. Keeping some
workers while asking about others breaks that "all-or-nothing, together"
rule and causes silent double-booking. Fixing that properly would mean
rebuilding a fair amount of what P-a/P-b/P-c just finished simplifying —
too big for this plan. So: **smarter about when to ring the bell, same
answer once it rings.**

## What this buys, concretely

```text
 producer B changes           → only regions that hire from B re-check
 producer C changes            → only regions that hire from C re-check
 goods price/stock changes      → NOBODY's job-bell rings, ever
 power capacity changes         → NOBODY's job-bell rings, ever
 a jobless citizen still exists → still retries whenever ANY region's
                                   jobs situation changes anywhere
                                   (unchanged — nobody gets stuck forever)
```

## The moving parts

```text
 producer B republishes what it offers (as it already does today)
        │
        ▼
 the directory checks: did B's ACTUAL JOB OFFER change?
   (a border road, or the list of open slots)          → yes: ring B's bell
   (just goods stock or power number)                   → no: stay silent
        │
        ▼
 every region, once a day, asks:
   "for each producer I currently hire from — has THEIR bell rung
    since I last checked?"                              → re-verify just them
   "is anyone at home still jobless, and did ANY bell     → try again
    ring anywhere?"
        │
        ▼
 if yes to either → do exactly what happens today: wipe, re-match
                     locally, ask remotely for everyone
 if no to both    → change nothing, leave every job as-is
```

One number still does the trick for "did I miss anything": each region
remembers the city-wide bell count *as of my last check*. A producer's
bell count higher than that means "rang after I last looked" — no need to
remember a separate number per producer.

```text
 I checked when the city-wide count was 40
 producer B's own count is now 41  →  41 > 40 → B rang since I checked ✓
 producer C's own count is still 40 →  40 > 40? no → C stayed quiet ✓
```

## Pseudocode

**The directory decides whether to ring the bell** (today it only checks
"did anything change" for one region as a whole — this splits that check):

```rust
pub fn publish_region(&self, region, links, hints) -> bool {
    let links = normalize_links(links);
    let hints = normalize_hints(hints);
    let mut state = self.publish_state.lock()...;

    let current_links = state.region_links.get(&region)...;
    let current_hints = state.region_hints.get(&region)...;
    if current_links == links && current_hints == hints {
        return false; // nothing changed at all — unchanged behavior
    }

    // NEW: does this change actually matter to jobs?
    //   a border road appearing/disappearing → yes (reachability)
    //   the list of open job slots changing  → yes
    //   just goods stock or power numbers    → no (ignored on purpose)
    let job_offer = |hints: &[RegionalAvailabilityHint]|
        hints.iter().map(|h| (h.network, h.spare_job_slot_ids.clone()))
             .collect::<Vec<_>>();
    if current_links != links || job_offer(&current_hints) != job_offer(&hints) {
        // Ring the city bell first, then stamp THIS producer with that same
        // global value. `jobs_generations[region]` is the global generation as
        // of the region's last job-offer change (a timestamp), NOT a per-region
        // change count — it must live in the same number space as the
        // `seen_jobs_generation` it is later compared against. An independent
        // `+= 1` per producer would fall behind the global count and make real
        // changes read as "older than my last check" (missed re-verification).
        state.global_jobs_generation += 1;                       // ring the city bell
        state.jobs_generations.insert(region, state.global_jobs_generation); // stamp B with it
    }

    set_or_remove(&mut state.region_links, region, links);
    set_or_remove(&mut state.region_hints, region, hints);
    self.rebuild_discovery(&mut state);
    true
}
```

**Each region checks its own producers before deciding to wipe** (replaces
today's one blanket "did anything change" check):

```rust
fn enter_job_phase(&mut self, request_id, power_phase) -> Vec<OutboundMessage> {
    // Do any of MY CURRENT producers have a bell that rang since I last checked?
    let remote_dirty = self.state.remote_producer_regions()
        .iter()
        .any(|producer| self.jobs_generation_of(*producer) > self.seen_jobs_generation);

    // Am I still missing a worker, and did ANYTHING job-relevant ring anywhere?
    let seeker_dirty = self.state.has_unassigned_citizen()
        && self.global_jobs_generation > self.seen_jobs_generation;

    let snapshot_dirty = remote_dirty || seeker_dirty;

    let phase = self.state.continue_tick_to_job_demand_phase(power_phase, snapshot_dirty);
    if !phase.is_daily() { /* unchanged: hourly ticks skip jobs entirely */ }

    // Everything past this point is UNCHANGED from today once "dirty" is decided.
    let mut outbound = if phase.jobs_dirty() {
        self.seen_jobs_generation = self.global_jobs_generation; // remember I just checked
        self.state.clear_jobs_exports_dirty();
        self.release_and_request_job(request_id, &phase.job_demands) // full wipe + re-ask, same as today
    } else {
        Vec::new() // quiet: leave every job exactly as it is
    };
    outbound.extend(self.enter_goods_phase(request_id, phase));
    outbound
}

fn jobs_generation_of(&self, region: RegionId) -> u64 {
    // A producer I've never heard from counts as 0 — can never look "newer"
    // than something I've already checked, so it's always safely ignored.
    self.jobs_generations.get(&region).copied().unwrap_or(0)
}
```

**Two small helpers, reading only what's already in the region:**

```rust
pub(crate) fn remote_producer_regions(&self) -> BTreeSet<RegionId> {
    // Who do I currently have a worker with, outside my own region?
    self.world.citizens.values()
        .filter_map(|c| c.workplace_assignment)
        .map(|a| a.workplace.region())
        .filter(|r| *r != self.id)
        .collect()
}

pub(crate) fn has_unassigned_citizen(&self) -> bool {
    self.world.citizens.values().any(|c| c.workplace_assignment.is_none())
}
```

Nothing about how a wipe actually happens changes at all — only the
question "should I even bother wiping today?" gets sharper.

## Decisions locked

- Make the *question* sharper. Leave the *answer* (full wipe) exactly as
  it is today — a partial wipe is unsafe, see "the trap" above.
- A bell only rings for border-road changes and open-job-slot changes.
  Goods and power numbers never ring it.
- One remembered number per region ("what was the city bell count when I
  last checked") is enough — no need to remember a number per producer.
- A jobless citizen still keeps retrying whenever anything job-relevant
  changes anywhere — nobody gets permanently stuck.

## Risks / notes

- Some existing tests currently force a re-check by bumping the one old
  shared counter. Those need to bump the new job-specific bell instead.
- If workplaces ever get more interesting than "does it have an open
  slot" (say, different pay per level), that new detail needs to also
  ring the bell — otherwise a real change could hide from this check. Not
  a problem today; flagged for whoever adds that later.
- Small memory cost: one extra number stored per region that has ever
  published anything. Bounded by how many regions exist — never grows
  unbounded.

## Patch split

```text
 P-i    Give the directory its per-region bell + the city-wide bell,
        ringing only for border-road/open-slot changes. Test: a goods-only
        change rings nothing; a slot/road change rings that region + the
        city bell.
 P-ii   Wire the bell counts through to each region (no behavior change
        yet — still using the old switch). Test: the wiring matches the
        existing pattern used for other numbers like this.
 P-iii  Switch the actual decision over to the new bells. Test: changing
        producer B only wakes up regions that hire from B; nobody wakes up
        for goods/power noise; a jobless citizen still keeps trying.
```

Each patch stays green on its own. Making the *response* partial too (not
just the question) is a bigger, separate project — only worth it if the
"full wipe once in a while" cost still shows up as a real problem later.
