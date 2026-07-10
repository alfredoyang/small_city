//! P1 (data model only) of the directory employment ledger plan
//! (`docs/20260708-directory-employment-ledger-plan.md`).
//!
//! This module adds the types the plan's later patches (P2-P7) will wire up:
//! employer-published job pools, home-submitted claims, and the
//! `EmploymentDirectory` broker/snapshot storage shape. Nothing here is
//! called by the existing job path yet — no daily-tick behavior changes, no
//! new claim submission or acceptance flow, no save-format change. That
//! wiring is explicitly out of scope for P1; see the plan's "Patch split"
//! section.
//!
//! ```text
//! Home region owns:      citizen body, applied WorkplaceAssignment
//! Employer region owns:  real pool validity, EmployerState contracts
//! EmploymentDirectory:   published pool snapshot, pending claims,
//!                        committed-employment read cache (not truth)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};

use crate::core::components::WorkplaceAssignment;
use crate::core::entity::Entity;
use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::{RegionId, RegionRoadNetworkId};

#[derive(Debug, Clone, Copy)]
/// One employer-published workplace pool, as the directory sees it.
///
/// `workplace` is the pool identity — no separate `JobPoolId` wrapper: the
/// directory does not need to know the employer's internal seat numbering,
/// and a wrapper containing only `workplace` would just be `Entity` with
/// extra ceremony (see "Stable Job Pool Identity" in the plan).
///
/// `generation` is directory-owned metadata, bumped only by
/// `EmploymentDirectory::publish_pools` when this pool's facts actually
/// change (P2). Deliberately **not** `PartialEq`/`Eq` — comparing two whole
/// `JobPool`s would fold `generation` into the comparison and make every
/// republish look like a change, even when nothing an employer controls
/// moved. Use [`same_pool_facts`] instead.
pub struct JobPool {
    pub region: RegionId,
    pub workplace: Entity,
    pub open_count: u16,
    pub network: RegionRoadNetworkId,
    pub salary: i32,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// A citizen, named by its home region and its home-local entity id.
///
/// The directory coordinates citizens across regions, so a bare `Entity`
/// (birth-region-scoped) is not enough context on its own here — pairing it
/// with `region` makes the ref self-describing without a lookup.
pub struct CitizenRef {
    pub region: RegionId,
    pub citizen: Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// Directory-minted id for one pending claim. Newtype, same shape as
/// `RegionId`/`UiRequestId` elsewhere in this codebase.
pub struct JobClaimId(pub u64);

#[derive(Debug, Clone, Copy)]
/// One home region's pending bid for one workplace pool seat.
///
/// `generation` is the pool generation this claim was chosen against
/// (`EmploymentSnapshot`'s view at submit time) — carried so an employer-side
/// re-check can tell a still-valid claim from one whose target pool moved
/// underneath it since the snapshot was read.
pub struct JobClaim {
    pub claim_id: JobClaimId,
    pub citizen: CitizenRef,
    pub workplace: Entity,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy)]
/// An employer's answer to one pending claim.
pub enum JobClaimDecision {
    Accepted {
        claim_id: JobClaimId,
        assignment: WorkplaceAssignment,
    },
    Rejected {
        claim_id: JobClaimId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Names one accepted employment relationship: this citizen, at this
/// workplace. Used to address a specific release/loss without carrying the
/// rest of a `JobClaim`/`EmploymentContract`.
pub struct EmploymentLeaseRef {
    pub citizen: CitizenRef,
    pub workplace: Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Why an employer reported a lease as lost (P5). `PoolInvalid` covers a
/// removed/shrunk/unreachable pool; `EmployerMissing` covers the employer
/// region itself disappearing from a rebuild.
pub enum JobLossReason {
    PoolInvalid,
    EmployerMissing,
}

#[derive(Debug, Clone, Copy)]
/// An employer-confirmed loss, addressed to the home region that must clear
/// its assignment.
pub struct JobLoss {
    pub lease: EmploymentLeaseRef,
    pub reason: JobLossReason,
}

#[derive(Debug, Clone, Copy)]
/// The employer's own record of one accepted seat. `accepted_generation` is
/// the pool generation in effect when the contract was created — kept for
/// the employer's own bookkeeping, not compared by the directory.
pub struct EmploymentContract {
    pub salary: i32,
    pub accepted_generation: u64,
}

#[derive(Debug, Clone, Default)]
/// Employer-side (workplace-owning region) contract bookkeeping.
///
/// "Employer" here means the workplace-owning region's own state, not a
/// separate actor — see "Stable Job Pool Identity" in the plan. Not yet
/// embedded into `RegionState`; P1 is data-shape only.
pub struct EmployerState {
    pub contracts_by_workplace: BTreeMap<Entity, BTreeMap<CitizenRef, EmploymentContract>>,
    pub pool_generations: BTreeMap<Entity, u64>,
}

#[derive(Debug, Default)]
/// The directory's own coordination state. Lock-held only long enough to
/// mutate one of these maps and rebuild the snapshot (P2+); never held
/// while scanning citizens, pathfinding, or reading `World`.
///
/// Each pending index has one clear owner:
/// - `claims_by_id` — the pending claim itself, by id. Accepted/rejected
///   decisions remove the entry; this is live coordination state, not an
///   audit log (see "Claim Retention" in the plan).
/// - `pending_by_workplace` — which claims are contending for one pool, so
///   a snapshot rebuild can subtract contended seats from `open_count`
///   without scanning every claim.
/// - `pending_by_citizen` — at most one pending claim per citizen, so
///   `submit_claims` (P3) can reject a second claim for a citizen who
///   already has one in flight.
/// - `pending_by_employer` — which claims one employer region still needs
///   to validate, so `take_pending_claims_for_employer` (P3) can pull its
///   own batch without scanning every claim.
#[allow(dead_code)] // P1: data model only; P2/P3 read and mutate these fields.
struct EmploymentBrokerState {
    next_claim_id: u64,
    pools_by_workplace: BTreeMap<Entity, JobPool>,
    // Pending claims only. Accepted/rejected decisions remove the claim.
    claims_by_id: BTreeMap<JobClaimId, JobClaim>,
    pending_by_workplace: BTreeMap<Entity, BTreeSet<JobClaimId>>,
    pending_by_citizen: BTreeMap<CitizenRef, JobClaimId>,
    pending_by_employer: BTreeMap<RegionId, BTreeSet<JobClaimId>>,
    releases_by_employer: BTreeMap<RegionId, Vec<EmploymentLeaseRef>>,
    losses_by_home: BTreeMap<RegionId, Vec<JobLoss>>,
    // Optional read cache of accepted claims. This mirrors region truth so
    // home regions can discover accepted employment cheaply; it is not the
    // authority for whether the employer contract actually exists.
    accepted_by_citizen: BTreeMap<CitizenRef, WorkplaceAssignment>,
    accepted_by_workplace: BTreeMap<Entity, BTreeSet<CitizenRef>>,
    pool_generation_by_workplace: BTreeMap<Entity, u64>,
    global_generation: u64,
}

#[derive(Debug, Default)]
/// Read-optimized copy of `EmploymentBrokerState`, published behind an
/// `Arc` so a region can clone the pointer under a short lock and scan it
/// with no lock held (P2+; see "Fast Snapshot Exchange" in the plan).
pub struct EmploymentSnapshot {
    pub generation: u64,
    pub open_pools_by_network: BTreeMap<RegionRoadNetworkId, Vec<JobPool>>,
    pub accepted_by_home_region: BTreeMap<RegionId, Vec<(CitizenRef, WorkplaceAssignment)>>,
    pub pending_claims_by_employer: BTreeMap<RegionId, Vec<JobClaim>>,
    pub active_citizens_by_home_region: BTreeMap<RegionId, BTreeSet<Entity>>,
}

#[derive(Debug, Default)]
/// Cross-region employment broker.
///
/// Not allowed to expose `World` to UI and does not mutate a region's ECS
/// directly. Owns claim coordination and read snapshots, not the final
/// employment truth: the employer region remains the source of truth for
/// whether a worker is really reserved; the home region remains the source
/// of truth for whether a citizen has applied the assignment.
pub struct EmploymentDirectory {
    broker: Mutex<EmploymentBrokerState>,
    active_snapshot: RwLock<Arc<EmploymentSnapshot>>,
}

/// Stable-fact equality for a `JobPool`: everything the employer controls,
/// excluding the directory-owned `generation`. Comparing whole `JobPool`
/// values would make an unrelated republish look like a change to every
/// pool from that employer — see "Publishing Pools" in the plan.
fn same_pool_facts(a: &JobPool, b: &JobPool) -> bool {
    a.region == b.region
        && a.workplace == b.workplace
        && a.open_count == b.open_count
        && a.network == b.network
        && a.salary == b.salary
}

/// Sorts and dedups one employer's republished pool list by workplace (the
/// pool identity), so publish order never matters and one employer never
/// lists the same workplace twice in a single call. Mirrors
/// `RegionDirectory`'s `normalize_links`/`normalize_hints`
/// (`directory.rs`) for the same reason.
fn normalize_pools(mut pools: Vec<JobPool>) -> Vec<JobPool> {
    pools.sort_by_key(|pool| pool.workplace);
    pools.dedup_by_key(|pool| pool.workplace);
    pools
}

#[derive(Debug, Default)]
/// What one employer's republish actually needs to change in the broker.
/// Unchanged pools are not collected anywhere — the whole point is that
/// they need no action, so their existing `pool_generation_by_workplace`
/// entry (and any pending claim targeting them) stays untouched.
struct PoolDelta {
    added: Vec<JobPool>,
    changed: Vec<JobPool>,
    removed: Vec<JobPool>,
}

impl PoolDelta {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Splits one employer's normalized republish against the broker's current
/// state. `generation` is directory-owned metadata, not an employer fact,
/// so pools are compared with [`same_pool_facts`], never with `JobPool`
/// equality — see "Publishing Pools" in the plan.
///
/// Every incoming pool is filtered to rows this `employer` actually owns.
/// The plan's rule is "the directory updates only that employer's pools",
/// and `publish_pools` is a public API: without this guard, employer A
/// could add or overwrite a pool owned by employer B.
///
/// Ownership is decided by `workplace.region()` — the birth region packed
/// into the workplace `Entity` id, which is the same authority
/// `invalidate_pending_claims_for_pool` uses to find an employer. The
/// `pool.region` field is a *self-declared* copy of that, so trusting it
/// alone would let A name B's workplace under `region: A` and still
/// overwrite `pools_by_workplace[B_workplace]`. Both must agree, which also
/// keeps every stored row self-consistent for the removal pass below.
fn diff_pools_for_employer(
    state: &EmploymentBrokerState,
    employer: RegionId,
    pools: &[JobPool],
) -> PoolDelta {
    let mut delta = PoolDelta::default();
    let owned = pools
        .iter()
        .filter(|pool| pool.region == employer && pool.workplace.region() == employer);
    let incoming_workplaces: BTreeSet<Entity> = owned.clone().map(|pool| pool.workplace).collect();

    for &incoming in owned {
        match state.pools_by_workplace.get(&incoming.workplace) {
            None => delta.added.push(incoming),
            Some(existing) if !same_pool_facts(existing, &incoming) => {
                delta.changed.push(incoming);
            }
            Some(_existing) => {
                // Facts are unchanged. Keep the existing directory-owned generation.
            }
        }
    }

    for existing in state.pools_by_workplace.values() {
        if existing.workplace.region() == employer
            && !incoming_workplaces.contains(&existing.workplace)
        {
            delta.removed.push(*existing);
        }
    }

    delta
}

/// Drop every pending claim against one workplace pool, because the facts it
/// was chosen from no longer hold — the pool was removed, or republished with
/// changed facts (and therefore a new generation).
///
/// The plan names this `mark_pool_missing_for_validation` and calls it only
/// for removed pools. P3 generalizes it: a *changed* pool invalidates its
/// pending claims for the same reason a removed one does, which is the direct
/// contrapositive of the plan's own "pending claims against untouched pools
/// stay valid". Keeping the generation authority here (rather than asking the
/// employer to re-check it) is what lets employer-side validation be a pure
/// capacity question.
///
/// Pending coordination state only. Accepted employment stays active until the
/// employer confirms loss with `report_lost_employment` (P5): this never
/// touches `accepted_by_citizen`/`accepted_by_workplace`.
///
/// No home is woken — the citizen is simply un-pended and retries on its next
/// pass, exactly as it does for a removed pool.
fn invalidate_pending_claims_for_pool(state: &mut EmploymentBrokerState, workplace: Entity) {
    let Some(claim_ids) = state.pending_by_workplace.remove(&workplace) else {
        return;
    };

    let employer = workplace.region();
    for claim_id in claim_ids {
        let mut remove_employer_entry = false;
        if let Some(ids) = state.pending_by_employer.get_mut(&employer) {
            ids.remove(&claim_id);
            remove_employer_entry = ids.is_empty();
        }
        if remove_employer_entry {
            state.pending_by_employer.remove(&employer);
        }

        let Some(claim) = state.claims_by_id.remove(&claim_id) else {
            continue;
        };

        state.pending_by_citizen.remove(&claim.citizen);
    }
}

/// Every citizen this directory currently considers spoken-for: holding a
/// pending claim, or already accepted. Lets a home region filter its own
/// unemployed list without a per-citizen directory lock (P3); the
/// directory still re-checks inside `submit_claims` because this snapshot
/// view may be stale by the time a claim actually lands.
fn group_active_citizens_by_home(
    state: &EmploymentBrokerState,
) -> BTreeMap<RegionId, BTreeSet<Entity>> {
    // Explicit type: `.or_default()` below needs the entry's value type
    // pinned before it can pick which `Default` impl to call; the doc's
    // pseudocode elides this, but plain `BTreeMap::new()` doesn't compile
    // without it.
    let mut active: BTreeMap<RegionId, BTreeSet<Entity>> = BTreeMap::new();

    for (citizen, _assignment) in state.accepted_by_citizen.iter() {
        active
            .entry(citizen.region)
            .or_default()
            .insert(citizen.citizen);
    }

    for claim in state.claims_by_id.values() {
        active
            .entry(claim.citizen.region)
            .or_default()
            .insert(claim.citizen.citizen);
    }

    active
}

/// Accepted employment, grouped by the home region that must apply it.
/// Iterates `accepted_by_citizen` (a `BTreeMap`), so each home's list comes
/// out already sorted by citizen — no separate sort needed.
fn group_accepted_by_home(
    state: &EmploymentBrokerState,
) -> BTreeMap<RegionId, Vec<(CitizenRef, WorkplaceAssignment)>> {
    let mut accepted: BTreeMap<RegionId, Vec<(CitizenRef, WorkplaceAssignment)>> = BTreeMap::new();

    for (&citizen, &assignment) in state.accepted_by_citizen.iter() {
        accepted
            .entry(citizen.region)
            .or_default()
            .push((citizen, assignment));
    }

    accepted
}

/// Pending claims, grouped by the employer that must validate them. Reuses
/// `pending_by_employer` (already grouped by employer) rather than
/// re-deriving employer identity from each claim, mirroring
/// `take_pending_claims_for_employer`'s (P3) own lookup pattern.
fn group_pending_claims_by_employer(
    state: &EmploymentBrokerState,
) -> BTreeMap<RegionId, Vec<JobClaim>> {
    let mut pending: BTreeMap<RegionId, Vec<JobClaim>> = BTreeMap::new();

    for (&employer, claim_ids) in state.pending_by_employer.iter() {
        for claim_id in claim_ids {
            if let Some(&claim) = state.claims_by_id.get(claim_id) {
                pending.entry(employer).or_default().push(claim);
            }
        }
    }

    pending
}

impl EmploymentDirectory {
    /// Cheap read: clones the `Arc`, never blocks on the broker's rebuild
    /// lock. See "Fast Snapshot Exchange" in the plan.
    pub fn snapshot(&self) -> Arc<EmploymentSnapshot> {
        Arc::clone(&self.active_snapshot.read().unwrap())
    }

    fn rebuild_snapshot_locked(state: &EmploymentBrokerState) -> EmploymentSnapshot {
        let mut open_pools_by_network = BTreeMap::new();

        for pool in state.pools_by_workplace.values() {
            let pending_count = state
                .pending_by_workplace
                .get(&pool.workplace)
                .map_or(0, BTreeSet::len) as u16;
            if pool.open_count <= pending_count {
                continue;
            }
            // *pool, not pool.clone(): JobPool is Copy, and clippy's
            // clone_on_copy is a hard error under this project's required
            // `-D warnings` gate — the doc's pseudocode predates that check.
            let mut claimable_pool = *pool;
            claimable_pool.open_count -= pending_count;

            open_pools_by_network
                .entry(pool.network)
                .or_insert_with(Vec::new)
                .push(claimable_pool);
        }

        for pools in open_pools_by_network.values_mut() {
            pools.sort_by_key(|pool| (pool.region, pool.workplace));
        }

        EmploymentSnapshot {
            generation: state.global_generation,
            open_pools_by_network,
            accepted_by_home_region: group_accepted_by_home(state),
            pending_claims_by_employer: group_pending_claims_by_employer(state),
            active_citizens_by_home_region: group_active_citizens_by_home(state),
        }
    }

    /// Employer regions publish job pools after their own derived state is
    /// current. Updates only that employer's pools, per-pool (not "stamp
    /// everything from this employer") — see "Publishing Pools" in the
    /// plan. Returns `false` (no rebuild, no swap) when nothing changed.
    pub fn publish_pools(&self, employer: RegionId, pools: Vec<JobPool>) -> bool {
        let mut state = self.broker.lock().unwrap();
        let pools = normalize_pools(pools);
        let delta = diff_pools_for_employer(&state, employer, &pools);
        if delta.is_empty() {
            return false;
        }

        let next_generation = state.global_generation + 1;

        for removed in delta.removed {
            state.pools_by_workplace.remove(&removed.workplace);
            state
                .pool_generation_by_workplace
                .insert(removed.workplace, next_generation);
            invalidate_pending_claims_for_pool(&mut state, removed.workplace);
        }

        for mut pool in delta.added {
            pool.generation = next_generation;
            state
                .pool_generation_by_workplace
                .insert(pool.workplace, next_generation);
            state.pools_by_workplace.insert(pool.workplace, pool);
        }

        for mut pool in delta.changed {
            pool.generation = next_generation;
            state
                .pool_generation_by_workplace
                .insert(pool.workplace, next_generation);
            state.pools_by_workplace.insert(pool.workplace, pool);
            // A pending claim was chosen against this pool's OLD facts, and its
            // `generation` no longer matches. The plan states the rule from the
            // other side — "pending claims against untouched pools stay valid"
            // — so a *touched* pool must drop them, exactly as a removed pool
            // does. Without this, an employer would validate a claim whose
            // salary/capacity/network it has since changed; the worst case is a
            // citizen hired into a pool whose `network` moved out of the home's
            // reachable component. The home retries on its next pass, against
            // the fresh facts.
            invalidate_pending_claims_for_pool(&mut state, pool.workplace);
        }

        state.global_generation = next_generation;

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        true
    }

    /// Reserve a batch of claims. Reserves **both** pool capacity and citizen
    /// identity immediately, so two home regions reading the same snapshot
    /// cannot overclaim one pool, and one citizen cannot end up holding two
    /// pending cross-region claims. Returns the employer regions that now
    /// have pending work — wake targets only, never claim payloads.
    ///
    /// Per "Bounded Nondeterminism" in the plan: *which* valid citizen wins a
    /// contested pool may vary, but the invariants below hold exactly.
    pub fn submit_claims(&self, requests: Vec<(CitizenRef, Entity, u64)>) -> Vec<RegionId> {
        let mut state = self.broker.lock().unwrap();
        let mut employers_to_wake = BTreeSet::new();

        for (citizen, workplace, generation) in normalize_claim_requests(requests) {
            let Some(pool) = state.pools_by_workplace.get(&workplace) else {
                continue;
            };
            if pool.generation != generation {
                continue; // snapshot was stale; try again on a later tick
            }
            let pending_count = state
                .pending_by_workplace
                .get(&workplace)
                .map_or(0, BTreeSet::len) as u16;
            if pending_count >= pool.open_count {
                continue;
            }
            if state.accepted_by_citizen.contains_key(&citizen) {
                continue;
            }
            if state.pending_by_citizen.contains_key(&citizen) {
                continue;
            }
            let region = pool.region;

            let claim_id = JobClaimId(state.next_claim_id);
            state.next_claim_id += 1;

            let claim = JobClaim {
                claim_id,
                citizen,
                workplace,
                generation,
            };

            state
                .pending_by_workplace
                .entry(workplace)
                .or_default()
                .insert(claim_id);
            state.pending_by_citizen.insert(citizen, claim_id);
            state
                .pending_by_employer
                .entry(region)
                .or_default()
                .insert(claim_id);
            state.claims_by_id.insert(claim_id, claim);
            employers_to_wake.insert(region);
        }

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        employers_to_wake.into_iter().collect()
    }

    /// One employer's current pending claims. Reads, does not drain: the
    /// claims are removed by `apply_claim_decisions` once the employer has
    /// actually decided them, so a wake that arrives while the employer is
    /// mid-validation cannot lose a claim.
    pub fn take_pending_claims_for_employer(&self, employer: RegionId) -> Vec<JobClaim> {
        let state = self.broker.lock().unwrap();
        let claim_ids = state
            .pending_by_employer
            .get(&employer)
            .cloned()
            .unwrap_or_default();
        claim_ids
            .into_iter()
            .filter_map(|claim_id| state.claims_by_id.get(&claim_id).copied())
            .collect()
    }

    /// Apply one employer's decisions. Every decided claim leaves *all* four
    /// pending indexes; an accepted one additionally decrements the cached
    /// `open_count` and lands in the accepted read cache. Returns the home
    /// regions to wake — for accepted **and** rejected claims alike, because a
    /// rejection is what releases the home's citizen-side pending guard so it
    /// can retry.
    pub fn apply_claim_decisions(
        &self,
        employer: RegionId,
        decisions: Vec<JobClaimDecision>,
    ) -> Vec<RegionId> {
        let mut state = self.broker.lock().unwrap();
        let mut homes_to_wake = BTreeSet::new();

        for decision in normalize_claim_decisions(decisions) {
            let claim_id = claim_id_of(&decision);
            let Some(claim) = state.claims_by_id.get(&claim_id).copied() else {
                continue;
            };
            // Same ownership rule publish_pools enforces: an employer may only
            // decide claims against workplaces it owns. Without this, employer A
            // could accept a claim targeting employer B's workplace and corrupt
            // B's pool `open_count` and accepted cache.
            if claim.workplace.region() != employer {
                continue;
            }
            state.claims_by_id.remove(&claim_id);

            let mut remove_pending_pool_entry = false;
            if let Some(ids) = state.pending_by_workplace.get_mut(&claim.workplace) {
                ids.remove(&claim.claim_id);
                remove_pending_pool_entry = ids.is_empty();
            }
            if remove_pending_pool_entry {
                state.pending_by_workplace.remove(&claim.workplace);
            }
            state.pending_by_citizen.remove(&claim.citizen);
            if let Some(ids) = state.pending_by_employer.get_mut(&employer) {
                ids.remove(&claim.claim_id);
            }
            homes_to_wake.insert(claim.citizen.region);

            if let JobClaimDecision::Accepted { assignment, .. } = decision {
                if let Some(pool) = state.pools_by_workplace.get_mut(&claim.workplace) {
                    pool.open_count = pool.open_count.saturating_sub(1);
                }
                state
                    .accepted_by_workplace
                    .entry(claim.workplace)
                    .or_default()
                    .insert(claim.citizen);
                state.accepted_by_citizen.insert(claim.citizen, assignment);
            }
        }

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        homes_to_wake.into_iter().collect()
    }

    /// P4: the home region reports which accepted assignments it has now
    /// written into its own durable `Citizen.workplace_assignment`.
    ///
    /// This does nothing to broker state, and that is the point: once the home
    /// has applied it, the durable copy lives in the region, and
    /// `accepted_by_citizen` / `accepted_by_workplace` are only a read cache.
    /// There is no terminal `JobClaim` to retain or GC — `apply_claim_decisions`
    /// already removed it from every pending index. The accepted cache is
    /// cleared later, by an explicit release or employer-confirmed loss (P5).
    ///
    /// The method exists as the seam P6's restart/rebuild reconciliation needs:
    /// after a rebuild the directory must learn which assignments a home has
    /// really applied, rather than assume its own cache is truth.
    ///
    /// Deviation: the plan's body also takes the broker lock and rebuilds/swaps
    /// the snapshot. Since nothing is mutated, that rebuild would produce a
    /// byte-identical snapshot at `O(pools + claims + accepted)` cost on *every*
    /// home wake. Skipped deliberately; behaviour is unchanged.
    pub fn acknowledge_home_applied(&self, _citizens: Vec<CitizenRef>) {}
}

fn claim_id_of(decision: &JobClaimDecision) -> JobClaimId {
    match decision {
        JobClaimDecision::Accepted { claim_id, .. } | JobClaimDecision::Rejected { claim_id } => {
            *claim_id
        }
    }
}

/// Deterministic order for one submit batch, and exact-duplicate removal.
/// Same shape as [`normalize_pools`]: sort by identity, then dedup. The sort
/// key is `(workplace, citizen, generation)` so the batch is processed in an
/// order that does not depend on the caller's iteration order.
///
/// A citizen appearing twice against *different* workplaces is not removed
/// here — `submit_claims`' `pending_by_citizen` guard rejects the second one
/// once the first is reserved, which is the same rule applied to a citizen
/// who already had a claim from an earlier batch.
fn normalize_claim_requests(
    mut requests: Vec<(CitizenRef, Entity, u64)>,
) -> Vec<(CitizenRef, Entity, u64)> {
    requests.sort_by_key(|(citizen, workplace, generation)| (*workplace, *citizen, *generation));
    requests.dedup();
    requests
}

/// Deterministic order for one employer's decision batch, and duplicate
/// removal by `claim_id` (a claim can only be decided once).
fn normalize_claim_decisions(mut decisions: Vec<JobClaimDecision>) -> Vec<JobClaimDecision> {
    decisions.sort_by_key(claim_id_of);
    decisions.dedup_by_key(|decision| claim_id_of(decision));
    decisions
}

/// Pick one open pool for a home region's citizen.
///
/// The plan writes this as `choose_best_pool(&snapshot, citizen)`, but the
/// snapshot alone cannot answer *reachability*: `open_pools_by_network` is
/// keyed by `RegionRoadNetworkId` and carries no component graph. Jobs are
/// network-scoped across regions exactly like power, so a pool is only a
/// candidate when its network sits in the same `CrossRegionDiscovery`
/// component as one of the home region's own border networks — the same
/// reachability rule the old job-export path already uses. The caller
/// therefore supplies the discovery snapshot and the home's networks.
///
/// Job *quality* matching is an explicit non-goal of the plan, so the pick is
/// simply the lowest `(region, workplace)` among reachable open pools. That is
/// deterministic; per "Bounded Nondeterminism" only the contested-winner
/// identity is allowed to vary, and that is decided later in `submit_claims`.
pub(crate) fn choose_best_pool(
    snapshot: &EmploymentSnapshot,
    discovery: &CrossRegionDiscovery,
    home: RegionId,
    home_networks: &[RegionRoadNetworkId],
) -> Option<JobPool> {
    let mut reachable = BTreeSet::new();
    for home_network in home_networks {
        let Some(component) = discovery.component_of(*home_network) else {
            continue;
        };
        for network in component {
            if network.region != home {
                reachable.insert(*network);
            }
        }
    }

    reachable
        .into_iter()
        .filter_map(|network| snapshot.open_pools_by_network.get(&network))
        .flatten()
        .min_by_key(|pool| (pool.region, pool.workplace))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::city_refs::CityCellRef;

    fn pool(region: u32, workplace: u32, open_count: u16, salary: i32, generation: u64) -> JobPool {
        JobPool {
            region: RegionId(region),
            // Entity::new, not a bare Entity(..): invalidate_pending_claims_for_pool
            // derives the employer from workplace.region() (the entity's packed
            // birth region), so a fixture whose workplace id doesn't actually
            // encode `region` would silently break that lookup.
            workplace: Entity::new(RegionId(region), workplace),
            open_count,
            network: RegionRoadNetworkId {
                region: RegionId(region),
                road_network: 0,
            },
            salary,
            generation,
        }
    }

    #[test]
    fn same_pool_facts_ignores_generation() {
        let a = pool(1, 7, 3, 50, 5);
        let b = pool(1, 7, 3, 50, 9);
        assert!(same_pool_facts(&a, &b));
    }

    #[test]
    fn same_pool_facts_catches_a_real_change() {
        let a = pool(1, 7, 3, 50, 5);
        let changed_open_count = pool(1, 7, 4, 50, 5);
        let changed_salary = pool(1, 7, 3, 60, 5);
        assert!(!same_pool_facts(&a, &changed_open_count));
        assert!(!same_pool_facts(&a, &changed_salary));
    }

    #[test]
    fn employment_directory_default_is_empty() {
        let directory = EmploymentDirectory::default();
        let broker = directory.broker.lock().unwrap();
        assert!(broker.pools_by_workplace.is_empty());
        assert!(broker.claims_by_id.is_empty());
        assert!(broker.pending_by_workplace.is_empty());
        assert!(broker.pending_by_citizen.is_empty());
        assert!(broker.pending_by_employer.is_empty());
        assert_eq!(broker.global_generation, 0);
        drop(broker);

        let snapshot = directory.active_snapshot.read().unwrap();
        assert_eq!(snapshot.generation, 0);
        assert!(snapshot.open_pools_by_network.is_empty());
    }

    #[test]
    fn citizen_ref_ordering_is_deterministic() {
        // BTreeSet, not HashSet: the plan requires no HashMap/HashSet
        // iteration order in allocation decisions. Insert out of order,
        // expect (region, citizen) sorted iteration back.
        let mut refs = BTreeSet::new();
        refs.insert(CitizenRef {
            region: RegionId(2),
            citizen: Entity(1),
        });
        refs.insert(CitizenRef {
            region: RegionId(1),
            citizen: Entity(9),
        });
        refs.insert(CitizenRef {
            region: RegionId(1),
            citizen: Entity(3),
        });

        let ordered: Vec<CitizenRef> = refs.into_iter().collect();
        assert_eq!(
            ordered,
            vec![
                CitizenRef {
                    region: RegionId(1),
                    citizen: Entity(3),
                },
                CitizenRef {
                    region: RegionId(1),
                    citizen: Entity(9),
                },
                CitizenRef {
                    region: RegionId(2),
                    citizen: Entity(1),
                },
            ]
        );
    }

    #[test]
    fn job_claim_decision_and_lease_types_compile_and_hold_expected_fields() {
        // Compile-only check for P1's remaining types: constructible, and
        // the WorkplaceAssignment/CitizenRef/Entity plumbing lines up.
        let citizen = CitizenRef {
            region: RegionId(1),
            citizen: Entity(3),
        };
        let workplace = Entity(7);
        let decision = JobClaimDecision::Accepted {
            claim_id: JobClaimId(1),
            assignment: WorkplaceAssignment {
                workplace,
                location: CityCellRef {
                    region: RegionId(2),
                    x: 0,
                    y: 0,
                },
                salary: 40,
            },
        };
        let JobClaimDecision::Accepted { assignment, .. } = decision else {
            panic!("expected Accepted");
        };
        assert_eq!(assignment.workplace, workplace);

        let lease = EmploymentLeaseRef { citizen, workplace };
        let loss = JobLoss {
            lease,
            reason: JobLossReason::PoolInvalid,
        };
        assert_eq!(loss.lease.citizen, citizen);

        let contract = EmploymentContract {
            salary: 40,
            accepted_generation: 5,
        };
        let mut employer = EmployerState::default();
        employer
            .contracts_by_workplace
            .entry(workplace)
            .or_default()
            .insert(citizen, contract);
        assert_eq!(
            employer.contracts_by_workplace[&workplace][&citizen].salary,
            40
        );
    }

    #[test]
    fn publish_pools_bumps_generation_only_for_changed_pools_leaves_unchanged_pools_alone() {
        let directory = EmploymentDirectory::default();
        let employer = RegionId(9);

        let pool_a = pool(9, 1, 3, 50, 0);
        let pool_b = pool(9, 2, 1, 70, 0);
        assert!(directory.publish_pools(employer, vec![pool_a, pool_b]));

        let network = pool_a.network;
        let first = directory.snapshot();
        let rows = &first.open_pools_by_network[&network];
        let gen_a_first = rows
            .iter()
            .find(|p| p.workplace == pool_a.workplace)
            .unwrap()
            .generation;
        let gen_b_first = rows
            .iter()
            .find(|p| p.workplace == pool_b.workplace)
            .unwrap()
            .generation;
        assert_eq!(
            gen_a_first, gen_b_first,
            "both minted in the same publish call"
        );

        // Republish: A unchanged, B changed (salary), C added.
        let pool_b_changed = pool(9, 2, 1, 99, 0);
        let pool_c = pool(9, 3, 2, 40, 0);
        assert!(directory.publish_pools(employer, vec![pool_a, pool_b_changed, pool_c]));

        let second = directory.snapshot();
        let rows = &second.open_pools_by_network[&network];
        let gen_a_second = rows
            .iter()
            .find(|p| p.workplace == pool_a.workplace)
            .unwrap()
            .generation;
        let gen_b_second = rows
            .iter()
            .find(|p| p.workplace == pool_b.workplace)
            .unwrap()
            .generation;
        let gen_c = rows
            .iter()
            .find(|p| p.workplace == pool_c.workplace)
            .unwrap()
            .generation;

        assert_eq!(
            gen_a_second, gen_a_first,
            "A's facts never changed; an unrelated republish must not bump its generation"
        );
        assert!(
            gen_b_second > gen_a_first,
            "B's salary changed; it must get a fresh generation"
        );
        assert_eq!(
            gen_c, gen_b_second,
            "B and C were minted in the same publish call"
        );
    }

    #[test]
    fn publish_pools_returns_false_when_republish_is_identical() {
        let directory = EmploymentDirectory::default();
        let employer = RegionId(9);
        let pool_a = pool(9, 1, 3, 50, 0);

        assert!(directory.publish_pools(employer, vec![pool_a]));
        assert!(
            !directory.publish_pools(employer, vec![pool_a]),
            "an identical republish must be a no-op"
        );
    }

    #[test]
    fn publish_pools_removed_pool_drops_from_snapshot_and_clears_its_pending_indexes() {
        let directory = EmploymentDirectory::default();
        let employer = RegionId(9);
        let pool_a = pool(9, 1, 3, 50, 0);
        assert!(directory.publish_pools(employer, vec![pool_a]));

        // Synthesize a pending claim against A, as P3's submit_claims would.
        let claim_id = JobClaimId(1);
        let citizen = CitizenRef {
            region: RegionId(1),
            citizen: Entity(50),
        };
        {
            let mut state = directory.broker.lock().unwrap();
            let generation = state.pools_by_workplace[&pool_a.workplace].generation;
            state.claims_by_id.insert(
                claim_id,
                JobClaim {
                    claim_id,
                    citizen,
                    workplace: pool_a.workplace,
                    generation,
                },
            );
            state
                .pending_by_workplace
                .entry(pool_a.workplace)
                .or_default()
                .insert(claim_id);
            state.pending_by_citizen.insert(citizen, claim_id);
            state
                .pending_by_employer
                .entry(employer)
                .or_default()
                .insert(claim_id);
        }

        // Republish without A: it's removed.
        assert!(directory.publish_pools(employer, Vec::new()));

        let snapshot = directory.snapshot();
        assert!(
            snapshot
                .open_pools_by_network
                .values()
                .flatten()
                .all(|p| p.workplace != pool_a.workplace)
        );

        let state = directory.broker.lock().unwrap();
        assert!(state.pools_by_workplace.get(&pool_a.workplace).is_none());
        assert!(
            state.claims_by_id.is_empty(),
            "invalidate_pending_claims_for_pool must drop the pending claim"
        );
        assert!(state.pending_by_workplace.get(&pool_a.workplace).is_none());
        assert!(state.pending_by_citizen.is_empty());
        assert!(
            state.pending_by_employer.get(&employer).is_none(),
            "P2 review check: pending_by_employer must be cleared too"
        );
    }

    #[test]
    fn rebuild_snapshot_subtracts_pending_capacity_and_hides_fully_pending_pools() {
        let directory = EmploymentDirectory::default();
        let employer = RegionId(9);
        let pool_a = pool(9, 1, 2, 50, 0); // 2 open seats
        assert!(directory.publish_pools(employer, vec![pool_a]));

        let mut state = directory.broker.lock().unwrap();
        state
            .pending_by_workplace
            .entry(pool_a.workplace)
            .or_default()
            .insert(JobClaimId(1));
        let snapshot = EmploymentDirectory::rebuild_snapshot_locked(&state);
        let rows = &snapshot.open_pools_by_network[&pool_a.network];
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].open_count, 1,
            "one of two seats is pending, one still claimable"
        );

        state
            .pending_by_workplace
            .get_mut(&pool_a.workplace)
            .unwrap()
            .insert(JobClaimId(2));
        let fully_pending = EmploymentDirectory::rebuild_snapshot_locked(&state);
        assert!(
            !fully_pending
                .open_pools_by_network
                .contains_key(&pool_a.network),
            "a pool with zero claimable seats left must not appear in open_pools_by_network"
        );
    }

    #[test]
    fn publish_pools_from_one_employer_cannot_touch_another_employers_pools() {
        // "The directory updates only that employer's pools." publish_pools is
        // a public API, so a caller naming another region's workplace must not
        // be able to add, change, or remove that region's row.
        let directory = EmploymentDirectory::default();
        let employer_a = RegionId(9);
        let employer_b = RegionId(4);

        let a_pool = pool(9, 1, 3, 50, 0);
        let b_pool = pool(4, 1, 5, 70, 0);
        assert!(directory.publish_pools(employer_a, vec![a_pool]));
        assert!(directory.publish_pools(employer_b, vec![b_pool]));

        let b_generation_before = {
            let state = directory.broker.lock().unwrap();
            state.pools_by_workplace[&b_pool.workplace].generation
        };

        // A republishes its own pool unchanged, but also tries to CHANGE B's
        // pool (different salary) and ADD a brand-new pool owned by B.
        let b_pool_hijacked = pool(4, 1, 5, 999, 0);
        let b_pool_invented = pool(4, 77, 8, 123, 0);
        assert!(
            !directory.publish_pools(employer_a, vec![a_pool, b_pool_hijacked, b_pool_invented]),
            "A's republish carries no change to A's own pools, so it must be a no-op"
        );

        let state = directory.broker.lock().unwrap();
        let b_now = state.pools_by_workplace[&b_pool.workplace];
        assert_eq!(b_now.salary, 70, "A must not change B's pool facts");
        assert_eq!(
            b_now.generation, b_generation_before,
            "A must not bump B's pool generation"
        );
        assert!(
            !state
                .pools_by_workplace
                .contains_key(&b_pool_invented.workplace),
            "A must not add a pool owned by B"
        );
        drop(state);

        // Spoofed row: A names B's WORKPLACE entity but self-declares
        // `region: A`. Ownership must be decided by workplace.region(), not by
        // the caller-supplied `region` field, or this overwrites B's row.
        let b_pool_spoofed = JobPool {
            region: employer_a,
            workplace: b_pool.workplace,
            open_count: 5,
            network: b_pool.network,
            salary: 4242,
            generation: 0,
        };
        assert!(
            !directory.publish_pools(employer_a, vec![a_pool, b_pool_spoofed]),
            "a spoofed-region row must be filtered out, leaving no change to publish"
        );

        let state = directory.broker.lock().unwrap();
        let b_now = state.pools_by_workplace[&b_pool.workplace];
        assert_eq!(
            b_now.salary, 70,
            "A must not overwrite B's pool by spoofing pool.region"
        );
        assert_eq!(b_now.region, employer_b, "B's row must still be owned by B");
        assert_eq!(
            b_now.generation, b_generation_before,
            "a spoofed row must not bump B's generation"
        );
        drop(state);

        // A republishes nothing at all: only A's pools are removed, B's survive.
        assert!(directory.publish_pools(employer_a, Vec::new()));
        let state = directory.broker.lock().unwrap();
        assert!(
            !state.pools_by_workplace.contains_key(&a_pool.workplace),
            "A's own pool is removed by its empty republish"
        );
        assert!(
            state.pools_by_workplace.contains_key(&b_pool.workplace),
            "A's empty republish must not remove B's pool"
        );
    }

    #[test]
    fn publish_pools_removing_a_pool_does_not_clear_accepted_employment() {
        // P2 "Behavior forbidden": do not clear accepted employment when a pool
        // disappears. An accepted worker keeps their job until the employer
        // explicitly reports loss via report_lost_employment (P5); a pool simply
        // vanishing from a republish must only reject PENDING claims.
        let directory = EmploymentDirectory::default();
        let employer = RegionId(9);
        let pool_a = pool(9, 1, 3, 50, 0);
        assert!(directory.publish_pools(employer, vec![pool_a]));

        let citizen = CitizenRef {
            region: RegionId(1),
            citizen: Entity::new(RegionId(1), 50),
        };
        let assignment = WorkplaceAssignment {
            workplace: pool_a.workplace,
            location: CityCellRef {
                region: employer,
                x: 1,
                y: 0,
            },
            salary: 50,
        };
        {
            let mut state = directory.broker.lock().unwrap();
            state.accepted_by_citizen.insert(citizen, assignment);
            state
                .accepted_by_workplace
                .entry(pool_a.workplace)
                .or_default()
                .insert(citizen);
        }

        // The workplace disappears from the employer's republish.
        assert!(directory.publish_pools(employer, Vec::new()));

        let state = directory.broker.lock().unwrap();
        assert!(
            !state.pools_by_workplace.contains_key(&pool_a.workplace),
            "the pool row itself is gone"
        );
        assert_eq!(
            state.accepted_by_citizen.get(&citizen).map(|a| a.workplace),
            Some(pool_a.workplace),
            "accepted employment must survive the pool disappearing"
        );
        assert!(
            state
                .accepted_by_workplace
                .get(&pool_a.workplace)
                .is_some_and(|workers| workers.contains(&citizen)),
            "accepted_by_workplace must survive the pool disappearing"
        );
    }

    #[test]
    fn employment_directory_never_reads_private_world_storage() {
        // P2 review check: "snapshot rebuild does not read private World
        // storage." The directory coordinates owned summaries only; regions
        // publish into it. Same contract-test shape as
        // `regional_state_imports_shared_simulation_helpers_not_game_facade`.
        let source = std::fs::read_to_string("src/core/regions/employment_directory.rs")
            .expect("employment directory source");

        // Scan only the production half: this test module's own assertion
        // messages would otherwise match the very strings being forbidden.
        let production = source
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .expect("test module marker");
        let code = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<String>();

        // Built at runtime so this source file never contains the literals.
        let forbidden_import = ["crate::core::", "world"].concat();
        let forbidden_type = ["Wor", "ld"].concat();

        assert!(
            !code.contains(&forbidden_import),
            "the employment directory must never import the private ECS storage module"
        );
        assert!(
            !code.contains(&forbidden_type),
            "the employment directory must never name the private ECS storage type outside comments"
        );
    }

    // ---- P3: claim flow ----

    fn citizen(region: u32, local: u32) -> CitizenRef {
        CitizenRef {
            region: RegionId(region),
            citizen: Entity::new(RegionId(region), local),
        }
    }

    fn assignment_for(pool: JobPool) -> WorkplaceAssignment {
        WorkplaceAssignment {
            workplace: pool.workplace,
            location: CityCellRef::local(pool.region, 1, 0),
            salary: pool.salary,
        }
    }

    /// One pool with `open_count` seats, published by its employer.
    fn directory_with_pool(open_count: u16) -> (EmploymentDirectory, JobPool) {
        let directory = EmploymentDirectory::default();
        let job_pool = pool(9, 1, open_count, 50, 0);
        assert!(directory.publish_pools(RegionId(9), vec![job_pool]));
        let stamped = directory.broker.lock().unwrap().pools_by_workplace[&job_pool.workplace];
        (directory, stamped)
    }

    #[test]
    fn submit_claims_rejects_a_stale_generation() {
        // P3 review check: "submit_claims checks pool.generation against the
        // requested generation."
        let (directory, job_pool) = directory_with_pool(3);

        let woken = directory.submit_claims(vec![(
            citizen(1, 50),
            job_pool.workplace,
            job_pool.generation + 1, // stale: the snapshot moved on
        )]);

        assert!(woken.is_empty(), "a stale claim wakes nobody");
        let state = directory.broker.lock().unwrap();
        assert!(state.claims_by_id.is_empty());
        assert!(state.pending_by_citizen.is_empty());
    }

    #[test]
    fn submit_claims_never_exceeds_open_count() {
        // P3 behavior forbidden: "no workplace pool accepts more than open_count."
        let (directory, job_pool) = directory_with_pool(2);

        let requests = (0..5)
            .map(|i| (citizen(1, 50 + i), job_pool.workplace, job_pool.generation))
            .collect::<Vec<_>>();
        let woken = directory.submit_claims(requests);

        assert_eq!(woken, vec![RegionId(9)], "the employer is woken once");
        let state = directory.broker.lock().unwrap();
        assert_eq!(
            state.pending_by_workplace[&job_pool.workplace].len(),
            2,
            "only open_count seats may be reserved, no matter how many citizens ask"
        );
        assert_eq!(state.claims_by_id.len(), 2);
    }

    #[test]
    fn submit_claims_refuses_a_citizen_who_already_has_a_pending_or_accepted_job() {
        // P3 review check: "submit_claims checks accepted_by_citizen and
        // pending_by_citizen." P3 behavior forbidden: "no citizen can hold two
        // pending or accepted cross-region jobs."
        let (directory, job_pool) = directory_with_pool(3);
        let pending_citizen = citizen(1, 50);
        let accepted_citizen = citizen(1, 51);

        directory.submit_claims(vec![(
            pending_citizen,
            job_pool.workplace,
            job_pool.generation,
        )]);
        directory
            .broker
            .lock()
            .unwrap()
            .accepted_by_citizen
            .insert(accepted_citizen, assignment_for(job_pool));

        // Both citizens try again. Neither may take a second seat.
        directory.submit_claims(vec![
            (pending_citizen, job_pool.workplace, job_pool.generation),
            (accepted_citizen, job_pool.workplace, job_pool.generation),
        ]);

        let state = directory.broker.lock().unwrap();
        assert_eq!(
            state.claims_by_id.len(),
            1,
            "the pending citizen keeps exactly one claim; the accepted one gets none"
        );
        assert!(!state.pending_by_citizen.contains_key(&accepted_citizen));
    }

    #[test]
    fn apply_claim_decisions_clears_every_pending_index_and_wakes_the_home() {
        // P3 review checks: "apply_claim_decisions removes claims from every
        // pending index" and "... returns home regions to wake for accepted and
        // rejected claims."
        let (directory, job_pool) = directory_with_pool(2);
        let accepted = citizen(1, 50);
        let rejected = citizen(2, 60);

        directory.submit_claims(vec![
            (accepted, job_pool.workplace, job_pool.generation),
            (rejected, job_pool.workplace, job_pool.generation),
        ]);
        let claims = directory.take_pending_claims_for_employer(RegionId(9));
        assert_eq!(claims.len(), 2);

        let decisions = claims
            .iter()
            .map(|claim| {
                if claim.citizen == accepted {
                    JobClaimDecision::Accepted {
                        claim_id: claim.claim_id,
                        assignment: assignment_for(job_pool),
                    }
                } else {
                    JobClaimDecision::Rejected {
                        claim_id: claim.claim_id,
                    }
                }
            })
            .collect::<Vec<_>>();

        let homes = directory.apply_claim_decisions(RegionId(9), decisions);
        assert_eq!(
            homes,
            vec![RegionId(1), RegionId(2)],
            "both the accepted and the rejected home must be woken"
        );

        let state = directory.broker.lock().unwrap();
        assert!(state.claims_by_id.is_empty(), "claims_by_id cleared");
        assert!(
            state.pending_by_workplace.is_empty(),
            "pending_by_workplace cleared"
        );
        assert!(
            state.pending_by_citizen.is_empty(),
            "pending_by_citizen cleared -- the rejected citizen may retry"
        );
        assert!(
            state.pending_by_employer[&RegionId(9)].is_empty(),
            "pending_by_employer cleared"
        );

        assert!(state.accepted_by_citizen.contains_key(&accepted));
        assert!(!state.accepted_by_citizen.contains_key(&rejected));
        assert_eq!(
            state.pools_by_workplace[&job_pool.workplace].open_count, 1,
            "an accepted claim decrements the cached open_count"
        );
    }

    #[test]
    fn apply_claim_decisions_from_one_employer_cannot_decide_another_employers_claim() {
        // Same ownership rule publish_pools enforces. Employer B must not be
        // able to accept a claim against employer A's workplace.
        let (directory, job_pool) = directory_with_pool(2);
        let claimant = citizen(1, 50);
        directory.submit_claims(vec![(claimant, job_pool.workplace, job_pool.generation)]);

        let claim = directory.take_pending_claims_for_employer(RegionId(9))[0];
        let homes = directory.apply_claim_decisions(
            RegionId(4), // not the workplace's owner
            vec![JobClaimDecision::Accepted {
                claim_id: claim.claim_id,
                assignment: assignment_for(job_pool),
            }],
        );

        assert!(homes.is_empty(), "a foreign employer decides nothing");
        let state = directory.broker.lock().unwrap();
        assert!(
            state.claims_by_id.contains_key(&claim.claim_id),
            "the claim must survive a foreign employer's decision"
        );
        assert!(state.accepted_by_citizen.is_empty());
        assert_eq!(
            state.pools_by_workplace[&job_pool.workplace].open_count, 2,
            "open_count must not be decremented by a foreign employer"
        );
    }

    #[test]
    fn take_pending_claims_for_employer_does_not_drain_the_claims() {
        // The claims are removed by apply_claim_decisions, not by the read. A
        // second wake landing mid-validation must still see them.
        let (directory, job_pool) = directory_with_pool(1);
        directory.submit_claims(vec![(
            citizen(1, 50),
            job_pool.workplace,
            job_pool.generation,
        )]);

        assert_eq!(
            directory
                .take_pending_claims_for_employer(RegionId(9))
                .len(),
            1
        );
        assert_eq!(
            directory
                .take_pending_claims_for_employer(RegionId(9))
                .len(),
            1,
            "reading the batch twice must not lose the claim"
        );
    }

    #[test]
    fn normalize_claim_requests_is_deterministic_and_dedups_exact_duplicates() {
        let workplace_a = Entity::new(RegionId(9), 1);
        let workplace_b = Entity::new(RegionId(9), 2);
        let one = citizen(1, 50);
        let two = citizen(1, 51);

        let normalized = normalize_claim_requests(vec![
            (two, workplace_b, 7),
            (one, workplace_a, 7),
            (two, workplace_b, 7), // exact duplicate
            (two, workplace_a, 7),
        ]);

        assert_eq!(
            normalized,
            vec![
                (one, workplace_a, 7),
                (two, workplace_a, 7),
                (two, workplace_b, 7)
            ],
            "sorted by (workplace, citizen, generation); exact duplicates removed"
        );
    }

    #[test]
    fn normalize_claim_decisions_sorts_by_claim_id_and_dedups() {
        let decisions = normalize_claim_decisions(vec![
            JobClaimDecision::Rejected {
                claim_id: JobClaimId(3),
            },
            JobClaimDecision::Rejected {
                claim_id: JobClaimId(1),
            },
            JobClaimDecision::Rejected {
                claim_id: JobClaimId(3),
            },
        ]);

        let ids = decisions.iter().map(claim_id_of).collect::<Vec<_>>();
        assert_eq!(ids, vec![JobClaimId(1), JobClaimId(3)]);
    }

    #[test]
    fn choose_best_pool_only_offers_pools_reachable_from_a_home_network() {
        use crate::core::regions::directory::CrossRegionDiscovery;

        let directory = EmploymentDirectory::default();
        let reachable = pool(9, 1, 1, 50, 0);
        let unreachable = pool(4, 1, 1, 90, 0); // richer, but in another component
        assert!(directory.publish_pools(RegionId(9), vec![reachable]));
        assert!(directory.publish_pools(RegionId(4), vec![unreachable]));
        let snapshot = directory.snapshot();

        let home_network = RegionRoadNetworkId {
            region: RegionId(1),
            road_network: 0,
        };
        let discovery = CrossRegionDiscovery {
            // home shares a component with employer 9 only.
            components: vec![
                vec![home_network, reachable.network],
                vec![unreachable.network],
            ],
            ..Default::default()
        };

        let chosen = choose_best_pool(&snapshot, &discovery, RegionId(1), &[home_network])
            .expect("a reachable pool exists");
        assert_eq!(
            chosen.workplace, reachable.workplace,
            "an unreachable pool must never be chosen, however good its salary"
        );

        // A home with no component at all reaches nothing.
        let isolated = RegionRoadNetworkId {
            region: RegionId(7),
            road_network: 0,
        };
        assert!(choose_best_pool(&snapshot, &discovery, RegionId(7), &[isolated]).is_none());
    }

    #[test]
    fn republishing_a_pool_with_changed_facts_invalidates_its_pending_claims() {
        // Found in review. A claim is chosen against a pool's facts at
        // generation G1. If the employer republishes those facts (G2) before
        // validating, the claim is stale: it may have been picked for a salary,
        // capacity, or *network* that no longer holds. The plan states the rule
        // from the other side -- "pending claims against untouched pools stay
        // valid" -- so a touched pool must drop them.
        let (directory, job_pool) = directory_with_pool(3);
        let claimant = citizen(1, 50);
        directory.submit_claims(vec![(claimant, job_pool.workplace, job_pool.generation)]);
        assert_eq!(directory.broker.lock().unwrap().claims_by_id.len(), 1);

        // Same workplace, different facts -> changed, not removed.
        let changed = JobPool {
            salary: job_pool.salary + 25,
            generation: 0,
            ..job_pool
        };
        assert!(directory.publish_pools(RegionId(9), vec![changed]));

        let state = directory.broker.lock().unwrap();
        assert!(
            state.pools_by_workplace.contains_key(&job_pool.workplace),
            "the pool itself survives -- it changed, it was not removed"
        );
        assert!(
            state.claims_by_id.is_empty(),
            "the stale claim must be dropped, not handed to the employer"
        );
        assert!(state.pending_by_workplace.is_empty());
        assert!(
            !state.pending_by_citizen.contains_key(&claimant),
            "the citizen is un-pended and free to retry against the fresh facts"
        );
        assert!(
            !state.pending_by_employer.contains_key(&RegionId(9)),
            "the employer's now-empty pending set is removed outright"
        );
    }

    #[test]
    fn an_unchanged_pool_keeps_its_pending_claims_across_a_republish() {
        // The other half of the same rule, and the reason publish_pools must
        // not simply drop every claim on every republish.
        let (directory, job_pool) = directory_with_pool(3);
        let other = pool(9, 2, 1, 70, 0);
        assert!(directory.publish_pools(
            RegionId(9),
            vec![
                JobPool {
                    generation: 0,
                    ..job_pool
                },
                other
            ]
        ));

        let stamped = directory.broker.lock().unwrap().pools_by_workplace[&job_pool.workplace];
        directory.submit_claims(vec![(
            citizen(1, 50),
            stamped.workplace,
            stamped.generation,
        )]);

        // Republish where only `other` changes.
        assert!(directory.publish_pools(
            RegionId(9),
            vec![
                JobPool {
                    generation: 0,
                    ..stamped
                },
                JobPool {
                    salary: 999,
                    generation: 0,
                    ..other
                },
            ]
        ));

        let state = directory.broker.lock().unwrap();
        assert_eq!(
            state.claims_by_id.len(),
            1,
            "a claim against the untouched pool must survive an unrelated pool's change"
        );
    }
}
