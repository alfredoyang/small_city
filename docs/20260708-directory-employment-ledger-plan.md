# Directory employment ledger — stable cross-region jobs without daily wipes

Status: **proposal, P1-P5 done; P7 in progress (P7-a done)** (data model,
employer pool publishing, the claim flow, home apply, release/loss, and the
contract-seat reservation foundation — see the "P<n>, implemented" sections
at the end of this doc. P3-P5 are *staged*: built and tested, but nothing
calls the ledger from the daily tick until P7-d's cutover). **P7 is split
into P7-a..P7-d** (it is too large for one reviewable diff and flips the
live allocator): P7-a retained reservations (done); P7-b connectivity
fingerprint; P7-c discovery install + route invalidation; P7-d the cutover.
**Remaining order is P7 → P6 → P8** — P6 (save/load durability) is deferred
until after P7 activates the flow and removes the daily wipe; see the note
at the P6 section head. This is an
alternative to pushing
[20260706-per-producer-job-staleness.md](20260706-per-producer-job-staleness.md)
further. It targets more precise citizen behavior: jobs should be
stable assignments, not daily batch allocations that can briefly fire and
re-hire a worker.

## Goal

```text
 no daily wipe for stable workers
 no overbooked workplace pools
 no unpaid transition unless a job is actually lost
 regions still own their ECS
 the directory coordinates cross-region employment
```

The current cross-region job path is a distributed request/grant protocol:

```text
 home region owns citizen
 employer region owns workplace pools/contracts
 directory owns stale availability hints
 worker routes messages
```

That split is why the code needs generations, stale-granted cleanup, broad
dirty gates, and full daily wipes. This plan changes only jobs:
local job assignment can stay region-local.

## The model

The directory becomes a cross-region employment broker. It is not allowed
to expose `World` to UI and it does not directly mutate a region's ECS.
It owns claim coordination and read snapshots, not the final employment
truth.

```rust
pub struct JobPool {
    pub region: RegionId,
    pub workplace: Entity,
    pub open_count: u16,
    pub network: RegionRoadNetworkId,
    pub salary: i32,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CitizenRef {
    pub region: RegionId,
    pub citizen: Entity,
}

pub struct JobClaim {
    pub claim_id: JobClaimId,
    pub citizen: CitizenRef,
    pub workplace: Entity,
    pub generation: u64,
}

pub enum JobClaimDecision {
    Accepted {
        claim_id: JobClaimId,
        assignment: WorkplaceAssignment,
    },
    Rejected {
        claim_id: JobClaimId,
    },
}

pub struct EmploymentLeaseRef {
    pub citizen: CitizenRef,
    pub workplace: Entity,
}

pub struct JobLoss {
    pub lease: EmploymentLeaseRef,
    pub reason: JobLossReason,
}

pub enum JobLossReason {
    PoolInvalid,
    EmployerMissing,
}
```

Exact type names can change. The important part is the authority split:
In the structs above, `region` means the workplace-owning region.

Reuse existing types where they already match the meaning:

```text
 WorkplaceAssignment:
   existing home-side applied job summary
   use it instead of adding a new Employment struct

 Entity:
   existing city-wide id for citizens and workplaces
   use workplace Entity directly as the pool identity

 JobExportRequest / JobExportGrant:
   existing one-shot request/grant transport
   may be adapted during migration, but they are not durable ledger state
```

```text
 Home region owns:
   citizen body, money, morale, home, local schedule
   the citizen's applied workplace assignment

 Employer region owns:
   real workplace pool validity and final accept/reject
   the accepted contracts for each workplace

 Employment directory coordinates:
   published job pool snapshot
   pending job claims
   committed employment read cache (optional, not source of truth)
```

## Stable Job Pool Identity

This is the first hard requirement. Today job capacity is aggregate:
`remaining_workplaces` contains the same workplace `Entity` repeated once
per open job, and is recomputed fresh. That is not enough for a lease
model, but the directory also does not need to know the employer's internal
seat numbering.

Use the workplace `Entity` itself as the stable pool identity. Do not add a
new wrapper type unless a second identity field is added later; a wrapper
that contains only `workplace` is just `Entity` with extra ceremony.

The employer publishes a `JobPool` with its current `open_count`. The
directory treats that count as claimable capacity for the workplace pool.
Pending claims are subtracted while building snapshots; accepted claims
decrement the cached `open_count` until the employer publishes a fresh
count. The employer remains the source of truth for the real employment
contracts.

Employer-side state needs a contract map grouped by workplace. In this
plan, "employer" means the workplace-owning region state:

```rust
pub struct EmployerState {
    pub contracts_by_workplace: BTreeMap<Entity, BTreeMap<CitizenRef, EmploymentContract>>,
    pub pool_generations: BTreeMap<Entity, u64>,
}

pub struct EmploymentContract {
    pub salary: i32,
    pub accepted_generation: u64,
}
```

`pool_generations[workplace]` changes only when the employer republishes
changed pool facts: the workplace appears or disappears, the employer's
published open count changes, reachable network changes, or job terms
change. Directory-side coordination changes, such as pending claims or an
accepted claim decrementing cached `open_count`, do not bump the pool
generation. Otherwise two valid claims against the same multi-worker
workplace could invalidate each other. When capacity shrinks below existing
contracts, the employer chooses which contracts are lost using deterministic
local policy, then reports those losses explicitly.

## The protocol

```text
 1. Employer publishes job pools
    B -> directory:
      workplace pool, open count, salary, network, generation

 2. Home region asks for work
    A has an unemployed citizen
    A reads the directory snapshot
    A submits a claim against one workplace pool

 3. Directory reserves claim
    target pool's claimable count decreases, so homes cannot overclaim it

 4. Employer validates
    B processes the pending claim:
      pool still exists and has employer-owned capacity? accept
      otherwise reject

 5. Employer accepts and creates contract
    B records the employment contract in its own region state
    directory records an accepted claim / read-cache lease

 6. Home region applies
    A reads accepted employment for its citizens
    citizen gets a stable workplace assignment
```

```text
        publish pools              claim pool
Employer B ─────────► Directory ◄──────── Home A
   ▲                     │   │                │
   │ validate claim      │   │ committed job  │
   └─────────────────────┘   └───────────────► citizen paid/commutes
```

## Truth And Cache

The directory does not replace regional authority. A job is durable only
after both owning regions have observed the accepted claim and updated
their own state:

```text
 Employer truth:
   workplace X has an employment contract for citizen A7

 Home truth:
   citizen A7 is assigned to workplace X

 Directory broker/cache:
   claim K was accepted
   optional employment read model for fast lookup
```

If the directory read cache is lost and rebuilt, the regions can republish
their current assignment/contract summaries. If a pending claim is lost,
the citizen simply retries later; an already-applied job should not be
lost just because a broker cache was rebuilt.

## Directory storage

Use the directory as the short-lock owner of broker state. Store published
pool snapshots, pending claims, and optional read-cache leases in
deterministic maps. The employer region remains the source of truth for
whether a worker is really reserved; the home region remains the source of
truth for whether a citizen has applied the assignment.

```rust
pub struct EmploymentDirectory {
    broker: Mutex<EmploymentBrokerState>,
    active_snapshot: RwLock<Arc<EmploymentSnapshot>>,
}

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
    // Read cache of accepted claims. This mirrors region truth so home regions
    // can discover accepted employment cheaply; it is not contract authority.
    accepted_by_citizen: BTreeMap<CitizenRef, WorkplaceAssignment>,
    pool_generation_by_workplace: BTreeMap<Entity, u64>,
    global_generation: u64,
}

pub struct EmploymentSnapshot {
    pub generation: u64,
    pub open_pools_by_network: BTreeMap<RegionRoadNetworkId, Vec<JobPool>>,
    pub accepted_by_home_region: BTreeMap<RegionId, Vec<(CitizenRef, WorkplaceAssignment)>>,
    pub pending_claims_by_employer: BTreeMap<RegionId, Vec<JobClaim>>,
    pub active_citizens_by_home_region: BTreeMap<RegionId, BTreeSet<Entity>>,
}
```

The broker state owns only directory coordination state. The snapshot is a
read-optimized copy of that broker state. Rebuild it after short mutations,
then swap the `Arc`:

```rust
impl EmploymentDirectory {
    pub fn snapshot(&self) -> Arc<EmploymentSnapshot> {
        // Lock only long enough to clone the Arc, not while a region scans it.
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
            let mut claimable_pool = pool.clone();
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
}
```

`pending_by_workplace` prevents homes from claiming more workers than a
workplace pool currently advertises.
`pending_by_citizen` prevents one citizen from holding two pending claims.
`active_citizens_by_home_region` lets home regions filter their local
unemployed list without a per-citizen directory lock. The directory still
checks the same rule inside `submit_claims` because the snapshot may be
stale.

`accepted_by_citizen` is the only accepted read cache. Capacity decisions use
the employer's contracts and the directory's cached `JobPool::open_count`;
they never use accepted-cache membership. A workplace grouping can be derived
from `accepted_by_citizen` if a real read consumer appears later.

```rust
fn group_active_citizens_by_home(
    state: &EmploymentBrokerState,
) -> BTreeMap<RegionId, BTreeSet<Entity>> {
    let mut active = BTreeMap::new();

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
```

## Publishing Pools

Employer regions publish job pools after derived state is current. The
directory updates only that employer's pools, then rebuilds/swaps the
snapshot.

```rust
impl EmploymentDirectory {
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
            mark_pool_missing_for_validation(&mut state, removed.workplace);
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
        }

        state.global_generation = next_generation;

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        true
    }
}
```

Publishing must be per-pool, not "stamp everything from this employer."
If employer B changes one workplace pool, only that pool gets a new
generation. Pending claims against untouched pools stay valid.
`diff_pools_for_employer` must split the employer's full republished list
into `added`, `changed`, `removed`, and `unchanged`; unchanged pools are
left in `pools_by_workplace` with their existing
`pool_generation_by_workplace` value.

`generation` is directory-owned metadata, not an employer fact. Do not
compare the whole `JobPool` when deciding whether a pool changed. Compare
only stable employer facts:

```rust
fn same_pool_facts(a: &JobPool, b: &JobPool) -> bool {
    a.region == b.region
        && a.workplace == b.workplace
        && a.open_count == b.open_count
        && a.network == b.network
        && a.salary == b.salary
}
```

`diff_pools_for_employer` uses `same_pool_facts`:

```rust
match state.pools_by_workplace.get(&incoming.workplace) {
    None => delta.added.push(incoming),
    Some(existing) if !same_pool_facts(existing, &incoming) => {
        delta.changed.push(incoming);
    }
    Some(_existing) => {
        // Facts are unchanged. Keep the existing directory-owned generation.
    }
}
```

## Submitting Claims

Home regions do not hold a directory lock while picking candidates. They
clone the snapshot `Arc`, choose deterministically, then submit a compact
claim batch.

```rust
fn home_region_daily_jobs(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    let snapshot = directory.snapshot(); // cheap Arc clone
    let home = runtime.region_id();
    let active_citizens = snapshot
        .active_citizens_by_home_region
        .get(&home)
        .cloned()
        .unwrap_or_default();

    let claims = runtime
        .state()
        .unemployed_citizens()
        .into_iter()
        .filter(|citizen| !active_citizens.contains(citizen))
        .filter_map(|citizen| {
            choose_best_pool(&snapshot, citizen).map(|pool| {
                (
                    CitizenRef {
                        region: home,
                        citizen,
                    },
                    pool.workplace,
                    pool.generation,
                )
            })
        })
        .collect::<Vec<_>>();

    // One short lock to reserve pending claims. The returned regions are
    // wake targets only; claims stay in the directory.
    let regions_to_wake = directory.submit_claims(claims);
    runtime.emit_employment_directory_ready(regions_to_wake);
}
```

Directory submission must reserve both pool capacity and citizens
immediately, so homes cannot overclaim a workplace pool and one citizen
cannot hold two
pending cross-region claims from stale snapshots:

```rust
impl EmploymentDirectory {
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
            state
                .pending_by_citizen
                .insert(citizen, claim_id);
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
}
```

`submit_claims` returns employer region ids, not claim payloads. The worker
routes a lightweight wake event to each target region's worker:

```rust
enum RegionEvent {
    EmploymentDirectoryReady,
}

fn route_employment_directory_wakes(worker: &mut RegionWorker, regions: Vec<RegionId>) {
    for region in regions {
        worker.push_region_event(region, RegionEvent::EmploymentDirectoryReady);
    }
}
```

The wake event carries no claims, contracts, or losses. It only tells the
region to pull whatever directory work is relevant to its role:
employer-side pending claims, employer-side release requests, home-side
accepted assignments, and home-side losses. This keeps the directory as
the coordination source and avoids polling every region each tick.

```rust
fn handle_employment_directory_ready(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) {
    employer_validate_claims(runtime, directory);
    employer_apply_releases(runtime, directory);
    home_apply_accepted_employment(runtime, directory);
    home_apply_losses(runtime, directory);
}
```

## Employer Validation

Employer regions copy their pending claims out of the snapshot or take a
short locked batch from the directory, validate against their owned ECS,
then return compact decisions.

```rust
fn employer_validate_claims(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    let claims = directory.take_pending_claims_for_employer(runtime.region_id());
    if claims.is_empty() {
        return;
    }

    let decisions = claims
        .into_iter()
        .map(|claim| {
            let accepted = runtime.state().job_pool_still_has_open_capacity(
                claim.workplace,
                claim.generation,
                claim.citizen.region,
            );
            if accepted {
                JobClaimDecision::Accepted {
                    claim_id: claim.claim_id,
                    assignment: runtime.state().accept_claim_and_create_assignment(&claim),
                }
            } else {
                JobClaimDecision::Rejected {
                    claim_id: claim.claim_id,
                }
            }
        })
        .collect::<Vec<_>>();

    let regions_to_wake = directory.apply_claim_decisions(runtime.region_id(), decisions);
    runtime.emit_employment_directory_ready(regions_to_wake);
}
```

`RegionRuntime` calls `employer_validate_claims` when it handles
`EmploymentDirectoryReady`. If multiple wakes arrive before the runtime runs,
the first validation drains the pending directory claims and later wakes are
cheap no-ops. Accepted and rejected decisions both wake the home region:
accepted claims are ready to apply, and rejected claims release the
citizen-side pending guard so the home can retry later.

```rust
impl EmploymentDirectory {
    pub fn take_pending_claims_for_employer(&self, employer: RegionId) -> Vec<JobClaim> {
        let state = self.broker.lock().unwrap();
        let claim_ids = state
            .pending_by_employer
            .get(&employer)
            .cloned()
            .unwrap_or_default();
        claim_ids
            .into_iter()
            .filter_map(|claim_id| state.claims_by_id.get(&claim_id).cloned())
            .collect()
    }

    pub fn apply_claim_decisions(
        &self,
        employer: RegionId,
        decisions: Vec<JobClaimDecision>,
    ) -> Vec<RegionId> {
        let mut state = self.broker.lock().unwrap();
        let mut homes_to_wake = BTreeSet::new();

        for decision in normalize_claim_decisions(decisions) {
            let claim_id = match &decision {
                JobClaimDecision::Accepted { claim_id, .. }
                | JobClaimDecision::Rejected { claim_id } => *claim_id,
            };
            let Some(claim) = state.claims_by_id.remove(&claim_id) else {
                continue;
            };

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

            if let JobClaimDecision::Accepted {
                assignment,
                ..
            } = decision
            {
                if let Some(pool) = state.pools_by_workplace.get_mut(&claim.workplace) {
                    pool.open_count = pool.open_count.saturating_sub(1);
                }
                state.accepted_by_citizen.insert(claim.citizen, assignment);
            }
        }

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        homes_to_wake.into_iter().collect()
    }
}
```

## Applying Accepted Employment

After an employer accepts a claim, the directory read cache exposes that
accepted employment to the home region. The home region applies it to its
own citizen state and then acknowledges application. The acknowledgement is
important after restart/rebuild because the directory cache is not the
source of truth.

```rust
fn home_apply_accepted_employment(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    let snapshot = directory.snapshot();
    let accepted = snapshot
        .accepted_by_home_region
        .get(&runtime.region_id())
        .cloned()
        .unwrap_or_default();

    let mut applied = Vec::new();
    for (citizen, assignment) in accepted {
        if runtime.state().apply_workplace_assignment(citizen.citizen, assignment) {
            applied.push(citizen);
        }
    }

    directory.acknowledge_home_applied(applied);
}
```

`apply_workplace_assignment` writes the home region's durable
`Citizen.workplace_assignment`.
The economy reads that regional assignment on normal daily ticks.

```rust
impl EmploymentDirectory {
    pub fn acknowledge_home_applied(&self, _citizens: Vec<CitizenRef>) {
        let mut state = self.broker.lock().unwrap();

        // The durable copy now lives in the home region's existing
        // Citizen.workplace_assignment. The directory accepted cache is only
        // a read model, so there is no terminal JobClaim to retain or GC.

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
    }
}
```

## Fast Snapshot Exchange

The lock rule is simple:

```text
 lock directory only to:
   clone Arc<EmploymentSnapshot>
   submit compact claim requests
   apply compact employer decisions
   publish compact pool lists

 do NOT hold directory lock while:
   scanning citizens
   running pathfinding
   reading World
   validating employer-owned contracts
   rendering UI
```

That gives regions a cheap, stable view:

```rust
let snapshot = directory.snapshot(); // Arc clone under short RwLock
// No directory lock is held here.
let candidates = snapshot.open_pools_by_network.get(&network);
```

Snapshots can be one pass stale. That is fine because claims reference the
pool generation they were chosen from. A stale claim either reserves
capacity that is still valid, or is rejected quickly without changing the citizen's
current assignment.

## Loss And Invalidation

Loss is explicit. Do not infer loss by omitting a pool from a snapshot and
do not clear the home assignment before the employer confirms.

Cases that create invalidation work:

```text
 employer workplace removed
 employer capacity shrinks
 employer pool no longer reachable from the home region
 citizen/home disappeared
 home region explicitly releases the job
```

Employer-side invalidation:

```rust
fn employer_publish_pools(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    let pools = runtime.state().published_job_pools();
    let lost_contracts = runtime
        .state()
        .release_contracts_no_longer_valid(&pools);

    directory.publish_pools(runtime.region_id(), pools);

    let mut regions_to_wake = Vec::new();
    for (workplace, citizen, _contract) in lost_contracts {
        regions_to_wake.extend(directory.report_lost_employment(JobLoss {
            lease: EmploymentLeaseRef { citizen, workplace },
            reason: JobLossReason::PoolInvalid,
        }));
    }
    runtime.emit_employment_directory_ready(regions_to_wake);
}
```

Home-side loss application:

```rust
fn home_apply_losses(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    let losses = directory.take_losses_for_home(runtime.region_id());
    for loss in losses {
        runtime.state().clear_employment_if_matches(
            loss.lease.citizen.citizen,
            loss.lease.workplace.region(),
            loss.lease.workplace,
        );
    }
}
```

Home-side release:

```rust
fn home_release_job(runtime: &mut RegionRuntime, directory: &EmploymentDirectory, citizen: Entity) {
    let Some(assignment) = runtime.state().clear_employment(citizen) else {
        return;
    };
    let regions_to_wake = directory.request_release(EmploymentLeaseRef {
        citizen: CitizenRef {
            region: runtime.region_id(),
            citizen,
        },
        workplace: assignment.workplace,
    });
    runtime.emit_employment_directory_ready(regions_to_wake);
}
```

Employer applies releases against its own contract map:

```rust
fn employer_apply_releases(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    let releases = directory.take_releases_for_employer(runtime.region_id());
    for release in releases {
        if runtime.state().release_contract_if_matches(
            release.workplace,
            release.citizen.region,
            release.citizen.citizen,
        ) {
            directory.confirm_release(runtime.region_id(), release);
        }
    }
}
```

Directory-side release and loss handling:

```rust
impl EmploymentDirectory {
    pub fn request_release(&self, release: EmploymentLeaseRef) -> Vec<RegionId> {
        let mut state = self.broker.lock().unwrap();
        let employer = release.workplace.region();

        // Keep accepted_by_citizen populated until the employer confirms, so
        // this citizen cannot claim a second job. Capacity stays unavailable
        // because open_count is unchanged until confirm_release.
        state
            .releases_by_employer
            .entry(employer)
            .or_default()
            .push(release);

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        vec![employer]
    }

    pub fn take_releases_for_employer(&self, employer: RegionId) -> Vec<EmploymentLeaseRef> {
        let mut state = self.broker.lock().unwrap();
        state.releases_by_employer.remove(&employer).unwrap_or_default()
    }

    pub fn confirm_release(&self, employer: RegionId, release: EmploymentLeaseRef) {
        let mut state = self.broker.lock().unwrap();
        if release.workplace.region() != employer {
            return;
        }

        clear_accepted_cache_if_matches(&mut state, &release);
        if let Some(pool) = state.pools_by_workplace.get_mut(&release.workplace) {
            pool.open_count = pool.open_count.saturating_add(1);
        }

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
    }

    pub fn report_lost_employment(&self, loss: JobLoss) -> Vec<RegionId> {
        let mut state = self.broker.lock().unwrap();
        let home = loss.lease.citizen.region;

        clear_accepted_cache_if_matches(
            &mut state,
            &loss.lease,
        );
        state
            .losses_by_home
            .entry(home)
            .or_default()
            .push(loss);

        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
        vec![home]
    }

    pub fn take_losses_for_home(&self, home: RegionId) -> Vec<JobLoss> {
        let mut state = self.broker.lock().unwrap();
        state.losses_by_home.remove(&home).unwrap_or_default()
    }
}

fn mark_pool_missing_for_validation(state: &mut EmploymentBrokerState, workplace: Entity) {
    // Missing pools can reject pending claims immediately because no home has
    // applied them yet. Accepted employment stays active until the employer
    // confirms loss with report_lost_employment.
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

fn clear_accepted_cache_if_matches(
    state: &mut EmploymentBrokerState,
    release: &EmploymentLeaseRef,
) {
    let Some(assignment) = state.accepted_by_citizen.get(&release.citizen) else {
        return;
    };
    if assignment.workplace != release.workplace {
        return;
    }

    state.accepted_by_citizen.remove(&release.citizen);
}
```

This is the part that enforces the user-facing rule:

```text
 worker keeps being paid until:
   employer confirms the pool contract is invalid, or
   home explicitly releases the assignment
```

## Live Cutover And Route Reconciliation

Per-slice snapshot install:

```rust
struct RegionRuntime {
    discovery_snapshot: Arc<CrossRegionDiscovery>,
    // ...
}

impl RegionRuntime {
    fn set_discovery_snapshot(&mut self, snapshot: Arc<CrossRegionDiscovery>) {
        self.discovery_generation = snapshot.generation;
        self.discovery_snapshot = snapshot;
    }

    fn discovery_snapshot(&self) -> Arc<CrossRegionDiscovery> {
        Arc::clone(&self.discovery_snapshot)
    }
}

fn process_worker_slice(worker: &mut RegionWorker) {
    let discovery = worker.directory.discovery_snapshot();
    for runtime in &mut worker.regions {
        runtime.set_discovery_snapshot(Arc::clone(&discovery));
        runtime.set_employment_directory(Arc::clone(&worker.employment_directory));
        runtime.process_some_events(MAX_EVENTS_PER_REGION);
    }
}
```

Contract-aware registry resolution:

```rust
fn resolve_job_assignments(
    world: &World,
    requests: &[JobRequest],
    workplace_slots: &[Entity],
    reserved_by_workplace: &BTreeMap<Entity, u16>,
) -> (Vec<JobAssignment>, Vec<Entity>) {
    let mut remaining_workplaces = workplace_slots.to_vec();

    // One repeated Entity is one physical seat. Remove contracted seats before
    // nearest_slot_index can offer them to local citizens.
    for (&workplace, &reserved) in reserved_by_workplace {
        for _ in 0..reserved {
            let Some(index) = remaining_workplaces
                .iter()
                .position(|slot| *slot == workplace)
            else {
                break;
            };
            remaining_workplaces.remove(index);
        }
    }

    let mut assignments = Vec::new();
    for request in requests {
        let index = nearest_slot_index(world, request.home, &remaining_workplaces);
        let workplace = index.map(|index| remaining_workplaces.remove(index));
        assignments.push(JobAssignment {
            citizen: request.citizen,
            workplace,
        });
    }

    (assignments, remaining_workplaces)
}
```

```text
 physical workplace seats:  [W, W, W]
 employer contracts:        [W, W]
                              └──┴── remove inside JobsRegistry
 local matching sees:       [W]
 published open pool sees:  [W]

 one reservation-aware JobResolution feeds both decisions
```

Daily employment:

```rust
fn daily_employment_phase(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let discovery = runtime.discovery_snapshot();
    runtime.ensure_derived_state();

    // Employer truth first: a disconnected lease no longer reserves a seat.
    let mut lost = runtime
        .state_mut()
        .release_contracts_with_unreachable_homes(&discovery);

    // Rebuild the JobsRegistry with contracts removed from local availability.
    let reserved = runtime.state().contracted_seats_by_workplace();
    runtime
        .state_mut()
        .resolve_and_cache_jobs_with_contract_reservations(&reserved);
    runtime.state_mut().apply_cached_local_job_assignments();

    // Local changes may have removed or reduced a workplace.
    lost.extend(
        runtime
            .state_mut()
            .release_contracts_over_current_capacity(),
    );

    // Keep every jobs consumer on the final contract-aware resolution.
    let reserved = runtime.state().contracted_seats_by_workplace();
    runtime
        .state_mut()
        .resolve_and_cache_jobs_with_contract_reservations(&reserved);
    runtime.state_mut().apply_cached_local_job_assignments();

    // Report each dropped contract, then publish open_count recomputed from the
    // employer's final contracts.
    let mut wake = BTreeSet::new();
    for (workplace, citizen, _contract) in lost {
        wake.extend(directory.report_lost_employment(JobLoss {
            lease: EmploymentLeaseRef { citizen, workplace },
            reason: JobLossReason::PoolInvalid,
        }));
    }

    directory.publish_pools(
        runtime.region_id(),
        runtime.state().published_job_pools(),
    );

    let mut outbound = runtime.emit_employment_directory_ready(
        wake.into_iter().collect(),
    );
    outbound.extend(home_region_daily_jobs(runtime, directory, &discovery));
    outbound
}
```

`release_contracts_over_current_capacity` is P5's existing
`release_contracts_no_longer_valid`, renamed for clarity — not a new function.
Once local matching can no longer take a contracted seat (the reservation
above), its trigger narrows to genuine capacity loss (bulldoze / downgrade /
power-off), which also changes what its existing tests exercise.

When the daily employment phase runs (the trigger gate):

```text
 run on a daily boundary when ANY of:
   world.is_jobs_exports_dirty()   local change: building create/replace/
                                    upgrade/bulldoze (player OR sim, e.g.
                                    business auto-upgrade), road connectivity,
                                    power on/off, or citizen born/removed
   connectivity_dirty               a NEIGHBOUR's roads/topology moved
     (component-graph fingerprint    (NOT the raw discovery generation --
      changed since last check)       see below)
   state.has_unassigned_citizen()   a jobless citizen still wants work
```

Three deliberate points about this gate:

- **Power belongs with road connectivity, not as a separate case.** Every
  power flip already funnels into `jobs_exports_dirty`: an imported grant or
  denial through `apply_power_export_grant`'s `invalidate_jobs_registry()`, and
  a local flip only ever through the building/road change that caused it. Do not
  strip that invalidation out of the power-grant path — it is what makes a
  workplace losing power invalidate its own pool (its `salary` becomes `None`,
  a published fact, not just a seat count).

- **Route reconciliation keys off *connectivity*, not the raw discovery
  generation.** P7 chose employer-side validation, so when home A bulldozes the
  only road to employer B, B has no local change at all and must learn through
  the shared directory. But `RegionDirectory::publish_region` bumps a single
  `generation` whenever links **or hints** change
  (`directory.rs:200`, `:255`), and hints carry spare-power / spare-goods /
  spare-job-slot *numbers*. So the raw generation moving means "some
  availability number, anywhere in the component, changed" — exactly the
  unrelated goods/power noise P7 forbids from firing workers. The component
  graph itself (`build_component_graph`) is a function of links + topology only;
  hint values never change it. So P7 must gate employer route reconciliation on
  a **connectivity signal that ignores hint values** — a separate
  connectivity-only generation on the directory, or a fingerprint of the
  component graph (e.g. a hash of the sorted components) compared against the
  region's last-seen value. `has_unassigned_citizen()` below still lets a
  jobless citizen retry when a *reconnection* opens a new pool, so this
  connectivity gate only needs to catch *disconnections* that strand an
  existing contract.

- **`has_unassigned_citizen()` is load-bearing, not belt-and-braces.** A loss
  clears the citizen's assignment through `refresh_jobs_cache_after_grant_applied`,
  which deliberately does **not** set `jobs_exports_dirty` (P-c's choice, so the
  old wipe never ate its own output). So a laid-off citizen leaves the gate
  clean. Gating on `is_jobs_exports_dirty() || discovery_dirty` alone would
  strand it on an otherwise-quiet day. Gating the claim-submission half on
  "this region still has a jobless citizen" is what makes the allowed behaviour
  "an unemployed citizen may retry on later daily employment phases" true.

Route validation:

```rust
fn contract_route_is_reachable(
    state: &RegionState,
    discovery: &CrossRegionDiscovery,
    workplace: Entity,
    home: RegionId,
) -> bool {
    state.workplace_networks(workplace).iter().any(|network| {
        discovery
            .component_of(*network)
            .is_some_and(|component| {
                component.iter().any(|member| member.region == home)
            })
    })
}
```

Use current workplace networks, not `JobPool` rows:

```text
 healthy + fully contracted                 bridge workplace

 spare=2, contracts=2                       network B1 ── workplace ── network B2
 open=0 => no JobPool row                         │                         │
 contract still route-valid                       X                         │
                                                                            │
 home A ────────────────────────────────────────────────────────────────────┘
                                             reachable through B2 => KEEP
```

Bridge asymmetry — validity and claimability use different rules, on purpose.
`contract_route_is_reachable` keeps a contract valid while **any** of the
workplace's networks reaches the home (`.any()` above). But
`published_job_pools` advertises a bridge workplace under only its **lowest-id**
network (P2's dedup, so two components can't each treat the same seats as
claimable), and `choose_best_pool` filters on that one network's component. So
a home reachable **only** via the second network keeps an existing contract but
can never win a **new** claim there:

```text
 B1 (id 0) ── workplace ── B2 (id 1)
     X                         │            existing contract via B2: KEPT
                               │            new claim via B2:         NOT OFFERED
 home A ────────────────────────┘           (pool is published under B1 only)
```

This is conservative, not unsound — it never keeps an unreachable job, and it
never double-offers a bridged seat. Making a bridge claimable from every
network is a job-quality refinement, an explicit non-goal.

Connection lifecycle:

```text
 CONNECTED
 contract + assignment
       │
       │ road disconnect
       ▼
 employer removes contract
       │ report_lost_employment
       ▼
 directory clears accepted cache ──wake──> home clears matching assignment
       │
       │ road reconnect
       ▼
 unemployed citizen sees reachable pool
       │ submit_claims
       ▼
 NEW contract + NEW assignment

 old contract is never resurrected
```

Seat handoff:

```text
 BEFORE LOSS              DROP + REPORT               PUBLISH RECOMPUTED

 employer contract=yes    employer contract=no        employer contract=no
 directory accepted=yes   directory accepted=no       directory accepted=no
 published open=0         published open=0             published open=1

 booked once              old open_count unchanged    claimable once
```

P7/P8 boundary:

```text
 P7: behavioral cutover                 P8: deletion

 daily tick ──> directory ledger        remove JobExport* types/events
             X old allocator            remove old routing/bookkeeping

 old code still compiles                behavior stays identical to P7
 old code gets no production traffic
```

Legacy removal:

```text
 RegionRuntime                 RegionWorker                 RegionState
 ─────────────                 ────────────                 ───────────
 JobExport events       ─┐     JobExport routing     ─┐     old allocation ledger ─┐
 release_and_request_job ├──>  DELETE                 ├──>  DELETE                  ├──> gone
 stale-grant handling   ─┘                             │     old tax input          ─┘
                                                       │
 directory claim/contract path ───────────────────────┴──> unchanged
```

## Rebuild And Save/Load

Stable employment is no longer derived-only state. Today's
`Citizen::workplace_assignment` is skipped by serde because daily job
assignment rebuilds it. This plan cannot rely on that. Accepted
assignments and employer contracts must become durable region-owned
state, or load must run a reconciliation protocol before the first economy
settlement.

Preferred save model:

```text
 Home region save:
   citizen -> applied WorkplaceAssignment

 Employer region save:
   workplace -> citizen contracts

 Directory save:
   pending claims only, optional
```

On load or directory rebuild, do not clear the live broker and then publish
pieces into it. Build a scratch state from regional truth, reconcile it,
then atomically replace the broker and snapshot:

```rust
fn rebuild_employment_directory(regions: &mut [RegionRuntime], directory: &EmploymentDirectory) {
    let mut rebuild = EmploymentDirectoryRebuild::new();

    for runtime in regions.iter_mut() {
        runtime.ensure_derived_state();
        rebuild.publish_pools(
            runtime.region_id(),
            runtime.state().published_job_pools(),
        );
        rebuild.publish_employer_contracts(
            runtime.region_id(),
            runtime.state().employer_contracts(),
        );
        rebuild.publish_home_assignments(
            runtime.region_id(),
            runtime.state().home_assignments(),
        );
    }

    let rebuilt_state = rebuild.reconcile_republished_truth();
    directory.replace_broker_state(rebuilt_state);
}
```

The home-side republish path is mandatory. It redeclares already-applied
citizen assignments so a rebuilt directory does not temporarily expose
their employer pools as open:

```rust
struct EmploymentDirectoryRebuild {
    pools_by_workplace: BTreeMap<Entity, JobPool>,
    employer_contracts: BTreeMap<Entity, BTreeMap<CitizenRef, EmploymentContract>>,
    home_assignments: BTreeMap<CitizenRef, WorkplaceAssignment>,
}

impl EmploymentDirectoryRebuild {
    pub fn publish_home_assignments(
        &mut self,
        home: RegionId,
        assignments: Vec<(Entity, WorkplaceAssignment)>,
    ) {
        for (citizen, assignment) in normalize_assignments(assignments) {
            self.home_assignments
                .insert(CitizenRef { region: home, citizen }, assignment);
        }
    }

    pub fn publish_employer_contracts(
        &mut self,
        employer: RegionId,
        contracts: Vec<(Entity, CitizenRef, EmploymentContract)>,
    ) {
        for (workplace, citizen, contract) in normalize_contracts(contracts) {
            if workplace_belongs_to_employer(workplace, employer) {
                self.employer_contracts
                    .entry(workplace)
                    .or_default()
                    .insert(citizen, contract);
            }
        }
    }
}
```

`replace_broker_state` is one short directory lock:

```rust
impl EmploymentDirectory {
    pub fn replace_broker_state(&self, rebuilt_state: EmploymentBrokerState) {
        let mut state = self.broker.lock().unwrap();
        *state = rebuilt_state;
        let snapshot = Arc::new(Self::rebuild_snapshot_locked(&state));
        *self.active_snapshot.write().unwrap() = snapshot;
    }
}
```

Reconciliation rules:

```text
 employer contract + matching home assignment -> accepted read cache
 employer contract without home assignment    -> release employer contract
 home assignment without employer contract    -> mark citizen unemployed
 pending claim                                   -> drop and retry later
```

Old saves need migration:

```text
 old save has no durable employment:
   clear skipped workplace assignments as today
   first daily job phase creates new claims
```

## Claim Retention

`claims_by_id` is live pending coordination state, not an audit log.
Accepted and rejected decisions remove the pending claim immediately after
all secondary indexes are cleared. Accepted employment is represented by
the existing `WorkplaceAssignment` in the home region and by the employer's
`EmploymentContract`; the directory's `accepted_by_citizen` map is only a
read cache.

This avoids unbounded claim growth without adding a separate terminal-claim
GC path.

## The key rule

Never clear an existing assignment just to check whether it is
still valid.

```text
 current cross-region job stays active
 revalidation or replacement claim happens separately
 only switch/release after accepted replacement or explicit invalidation
```

This is the main behavior change from the current daily wipe/re-request
model. Stable citizens keep working. The simulation only mutates cross-region
employment when a job-relevant event actually happens.

## Daily tick shape

Most days are read-only:

```text
 normal day:
   read applied WorkplaceAssignment
   pay salary
   commute to workplace
   no re-request
```

Job-relevant events create writes:

```text
 citizen born or becomes unemployed
 citizen moves away / home removed
 workplace built, removed, or changes capacity
 route/reachability invalidated
 explicit balancing policy decides to rematch
```

Then the ledger updates affected claims or leases. It should not wipe every
worker in a region as a proxy for checking them.

## Why this fixes the current class of bugs

```text
 stale grant:
   no bare grant; claim has explicit state

 double-booking:
   directory reserves pool claimable capacity before another home can use it
   directory marks a citizen pending or committed before that citizen can
   claim another cross-region job

 unnecessary fired/unpaid day:
   old assignment remains until replacement/loss is confirmed

 broad dirty gates:
   stable workers are not wiped on unrelated goods/power noise
```

## Bounded Nondeterminism

Cross-region employment winner identity is allowed to be nondeterministic
when several valid citizens race for the same external job. The simulation
does not care which valid citizen fills the job, only that the resulting
employment state is valid.

The directory must still preserve correctness invariants:

```text
 no pool accepts more claims than its current claimable count
 no citizen is accepted into two jobs
 stable workers are not cleared before explicit release or loss
 accepted workers are paid from their applied assignment
 no HashMap iteration order in allocation decisions
```

The directory may read published snapshots, but it should not read private
`World` storage directly. Employer validation remains an event handled by
the employer region. Tests for contested cross-region jobs should assert
the invariants above rather than a specific winning citizen.

## Patch split

### P1: Data model only

Scope:

```text
 add JobPool, JobClaim, JobClaimId, JobClaimDecision
 add CitizenRef and EmploymentLeaseRef
 add EmployerState::contracts_by_workplace
 add EmploymentContract
 add EmploymentDirectory broker/snapshot storage shape
```

Behavior allowed:

```text
 compile-only integration of new types
 constructors/helpers for normalizing deterministic map/set inputs
 no runtime use by the existing job path yet
```

Behavior forbidden:

```text
 no daily job behavior change
 no save-format behavior change yet
 no UI exposure of directory or World internals
 no new claim submission or acceptance path
```

Review checks:

```text
 workplace Entity is the pool identity; no one-field JobPoolId wrapper
 generation is directory-owned metadata
 same_pool_facts exists or equivalent comparison ignores generation
 pending indexes have clear ownership:
   claims_by_id
   pending_by_workplace
   pending_by_citizen
   pending_by_employer
```

### P2: Employer publish

Scope:

```text
 employer regions compute published JobPool rows from current derived jobs
 EmploymentDirectory::publish_pools stores pools by workplace
 publish_pools diffs added/changed/removed/unchanged pools for one employer
 snapshot exposes open_pools_by_network
 old request/grant path remains active
```

Behavior allowed:

```text
 directory snapshot reflects employer pool availability
 changed pool facts bump only that pool's generation
 removed pools reject only pending claims for that missing pool
```

Behavior forbidden:

```text
 do not stamp every pool from an employer on unrelated changes
 do not compare JobPool::generation as an employer fact
 do not clear accepted employment when a pool disappears
 do not replace the old job allocation path yet
```

Review checks:

```text
 unchanged pools keep their existing generation
 publish_pools uses same_pool_facts or equivalent field comparison
 mark_pool_missing_for_validation clears all pending indexes, including pending_by_employer
 snapshot rebuild does not read private World storage
```

### P3: Claim flow

Scope:

```text
 home regions read EmploymentSnapshot for candidate pools
 home regions submit compact claim batches
 submit_claims reserves pending pool capacity and pending citizen identity
 EmploymentDirectoryReady wakes employer regions
 employer regions pull pending claims and accept/reject against owned ECS
 apply_claim_decisions updates directory read cache and wakes home regions
```

Behavior allowed:

```text
 contested cross-region winner identity may be nondeterministic
 tests may assert "one valid worker got the job" instead of a specific citizen
 accepted claims decrement cached open_count until next employer publish
 rejected claims release the pending citizen guard so the home can retry
```

Behavior forbidden:

```text
 no workplace pool accepts more than open_count
 no citizen can hold two pending or accepted cross-region jobs
 no employer validation outside the employer-owned region state
 no direct UI access to directory internals
```

Review checks:

```text
 submit_claims checks accepted_by_citizen and pending_by_citizen
 submit_claims checks pool.generation against the requested generation
 apply_claim_decisions removes claims from every pending index
 apply_claim_decisions returns home regions to wake for accepted and rejected claims
 EmploymentDirectoryReady carries no claim payload; regions pull work from the directory
```

### P4: Home apply

Scope:

```text
 home regions handle EmploymentDirectoryReady by reading accepted assignments
 home_apply_accepted_employment writes existing Citizen.workplace_assignment
 economy reads applied WorkplaceAssignment for salary/payment
 home acknowledges applied assignments to the directory read cache
```

Behavior allowed:

```text
 newly accepted workers become paid after home application
 stable applied assignments remain across normal daily ticks
 accepted read cache can be used for cheap lookup but is not truth
```

Behavior forbidden:

```text
 do not pay from pending claims
 do not clear an old assignment while merely checking for replacement work
 do not make directory cache the durable source of home employment truth
```

Review checks:

```text
 payment path uses home-region Citizen.workplace_assignment
 accepted worker is paid on the next daily economy phase after apply
 rejected claims do not create assignments
 repeated EmploymentDirectoryReady events are idempotent
```

### P5: Release and invalidation

Scope:

```text
 home release requests enqueue EmploymentLeaseRef for the employer
 employer release handling removes matching EmploymentContract
 employer loss reporting sends JobLoss to the home region
 home loss handling clears assignment only if it still matches the lost workplace
 EmploymentDirectoryReady wakes both employer release work and home loss work
```

Behavior allowed:

```text
 explicit home release clears home assignment first, then employer confirms capacity
 employer-confirmed loss clears home assignment
 capacity returns to available only after employer confirmation
```

Behavior forbidden:

```text
 do not infer accepted job loss from missing snapshot rows
 do not advertise released capacity before employer confirms release
 do not clear a home assignment if the citizen already moved to a different workplace
```

Review checks:

```text
 request_release leaves open_count unchanged and keeps accepted_by_citizen active
 confirm_release clears accepted cache only for the matching employer/workplace/citizen
 report_lost_employment clears accepted cache and wakes the home region
 take_losses_for_home drains loss queue deterministically
 stable workers keep being paid until explicit release or employer-confirmed loss
```

### P6: Save/load and directory rebuild

**Ordering: P6 runs AFTER P7, despite the number.** (Decided 2026-07-11;
sections not renumbered to avoid churning the many "P7 must…"/"P8…"
cross-references.) Persisting employment durability before P7 would be both
inert and fragile: nothing tick-drives the directory until P7, so there is no
live employment to persist, and the daily wipe (which P7 removes) still clears
`Citizen.workplace_assignment` every day — it would erase a persisted *remote*
assignment on the first tick after load. Once P7 makes the ledger the live
allocator and deletes the wipe, durable `workplace_assignment` becomes both
meaningful and safe, and the rebuild reconciliation has real regional truth to
reconcile. So: P1–P5 (done) → P7 → P6 → P8.

Scope:

```text
 persist or reconstruct home applied WorkplaceAssignment
 persist or reconstruct employer EmploymentContract maps
 rebuild directory from regional truth before first economy settlement
 publish pools, employer contracts, and home assignments into a scratch rebuild state
 atomically replace broker state and snapshot after reconciliation
 remove the redundant accepted_by_workplace reverse index
```

Behavior allowed:

```text
 pending claims may be dropped on rebuild and retried later
 accepted jobs survive save/load if both regional truths agree
 old saves without durable employment migrate by starting with no applied jobs
```

Behavior forbidden:

```text
 do not expose a partially rebuilt directory snapshot
 do not make directory-only accepted cache the save truth
 do not double-book a pool during rebuild when home and employer truth already agree
```

Review checks:

```text
 home-side republish path exists
 employer-side contract republish path exists
 accepted cache is rebuilt into accepted_by_citizen only
 reconciliation handles:
   matching employer contract + home assignment
   employer contract without home assignment
   home assignment without employer contract
   pending claim
```

### P7: Activate the employment ledger

Scope:

```text
 make the directory claim/contract path the live cross-region job allocator
 RegionWorker installs Arc<CrossRegionDiscovery> into RegionRuntime each slice
 employment reconciliation reads the installed discovery snapshot
 run the daily employment phase when jobs_exports_dirty OR connectivity changed
   OR the region still has an unassigned citizen
 add a connectivity-only signal (a component-graph fingerprint, or a separate
   directory generation bumped only on link/topology change) -- the raw
   discovery generation also bumps on hint-value changes (goods/power/capacity),
   which must NOT fire employer route reconciliation
 an employer runs route invalidation whenever connectivity changed, so a
   neighbour's road change (which never dirties the employer locally) still lands
 invalidate employer contracts whose home region is no longer reachable
 let unemployed homes discover newly reachable pools and submit fresh claims
 remove cross-region job dependence on jobs_exports_dirty assignment wipes
 keep local-only job assignment path working
 rename release_contracts_no_longer_valid -> release_contracts_over_current_capacity
   (same function; its trigger narrows to real capacity loss once seats are reserved)
 ResourceRegistryCache gains a RETAINED reservation input: reservations_by_workplace,
   set from EmploymentContract state, NOT recomputed from World on cache rebuild
 contract accept/release/loss updates that input and invalidates the jobs cache
 resource_registry.rs subtracts the retained reservation before local matching
 JobResolution carries reserved_seats_by_workplace, its consumers read it from there
 reserved_seats(workplace) = min(contract_count, physical_seat_count)
   (reservation happens BEFORE locals, so it cannot depend on "seats left after locals")
 published_job_pools stops subtracting contracts: open_count is remaining, as-is
 job_pool_still_has_open_capacity becomes a remaining > 0 check
 release_contracts_over_current_capacity evicts contract_count - reserved_seats(workplace)
   = max(0, contract_count - physical_seat_count)
 availability_hints inherit the reservation, so importable_remote_jobs stops
   counting seats already contracted to another region's citizen
 source employer workplace-tax accounting from EmploymentContract state
 stop calling or emitting the old cross-region request/grant path
 leave the inactive legacy types/functions/events in place for P8 to remove
```

Exactly one layer subtracts contracts. Today three call sites each subtract
them from a `remaining_workplaces` that does **not** reserve them. Once the
registry does the reserving, every one of those subtractions must go, or it
double-counts:

```text
 today                              after reservation, if left unchanged
 ─────                              ───────────────────────────────────
 open = remaining - contracts       remaining - 2*contracts
                                      -> healthy pools vanish, and
                                         publish_pools treats them as removed
 remaining > contracts              0 > 1  -> employer rejects every claim
 holders <= remaining               2 <= 0 -> evicts EVERY contract, silently,
                                              on every reconciliation
```

`reserved_seats` exists because capacity cannot be reconstructed as
`remaining + contracts`: a bulldozed workplace has zero slots, so `remaining`
is 0 and that sum reports `contracts` — evicting nobody, exactly when
everybody should be evicted. The reservation is capped at the seats that
really exist and is computed *before* locals, so it depends only on physical
seat count, never on how many locals are seeking:

```text
 reserved = min(contract_count, physical_seat_count)
 remaining (offered to locals) = physical_seat_count - reserved
 evict    = contract_count - reserved = max(0, contract_count - physical_seat_count)

 seats=2 contracts=1   reserved=1  remaining=1  open=1   evict=0
 seats=2 contracts=2   reserved=2  remaining=0  open=0   evict=0
                       (a local seeker gets 0 remaining: contracts win)
 seats=1 contracts=2   reserved=1  remaining=0  open=0   evict=1  (downgrade)
 seats=0 contracts=2   reserved=0  remaining=0  no row   evict=2  (bulldozed)
```

The reservation is a **retained** registry input, not a per-tick injection.
`ResourceRegistryCache::ensure_jobs` rebuilds `JobResolution` from `World`
alone on any `invalidate_jobs_registry`, so a one-off
`resolve_with_reservations` call would be silently undone by the very next
build — re-exposing the reserved seats. The cache must hold
`reservations_by_workplace` as an input alongside the `World`-derived data;
contract accept / release / loss updates it and marks the jobs cache dirty,
exactly as building/road/citizen mutations already do. `JobResolution` then
carries `reserved_seats_by_workplace`, and
`release_contracts_over_current_capacity` reads eviction counts from there
rather than recomputing them.

Behavior allowed:

```text
 local daily assignment can still fill local jobs
 cross-region workers keep stable assignments across unrelated goods/power changes
 road disconnection may end a contract through explicit employer-confirmed JobLoss
 road reconnection lets an unemployed citizen compete for a new claim
 an unemployed citizen may retry on later daily employment phases
 explicit rematch policy may release/reclaim jobs later, but not in this patch
```

Behavior forbidden:

```text
 do not run the old allocator and the directory allocator together
 do not wipe all cross-region workplace assignments as a validation proxy
 do not leave old stale-grant cleanup clearing stable workers
 do not automatically resurrect a contract that was already lost
 do not let local assignment consume employer-contracted capacity
 do not publish newly freed capacity before dropping the employer contract
 do not compute local jobs from a registry that omitted contract reservations
 do not subtract employer contracts twice: exactly one layer reserves them
 do not reconstruct contract capacity as remaining + contracts (a bulldozed
   workplace has remaining 0 and would then evict nobody)
 do not let unrelated resource noise fire workers
```

Review checks:

```text
 a daily tick emits no JobExportRequested, JobExportAllocationsReleased, or JobExportRequestCompleted
 RegionRuntime reads the same discovery Arc the slice installed for reachability
 route reconciliation keys off a connectivity fingerprint, not the raw generation
 a neighbour's goods/power/capacity-hint change bumps the discovery generation
   but does NOT trigger employer route reconciliation (connectivity fingerprint unchanged)
 stable worker remains paid across normal ticks
 disconnecting the only route reports loss and clears only the matching home assignment
 a neighbour-side road bulldoze (no local change on the employer) still ends the contract
 a fired cross-region worker re-enters the claim pool on a quiet day (no local dirty flag)
 reconnecting a route lets an unemployed citizen claim without resurrecting the old contract
 a bridge workplace stays valid while either workplace network reaches the home,
   but a new claim is only offered on its lowest-id network's component
 remaining_workplaces excludes one repeated workplace Entity per contract
 reservations survive a jobs-cache invalidation: after invalidate_jobs_registry
   the rebuilt JobResolution still excludes contracted seats (retained input, not
   a per-tick injection)
 local assignment, published pools, and employer capacity checks agree on reserved seats
 a 2-seat workplace with 1 contract still publishes open_count 1 (no double subtraction)
 a 2-seat workplace with 2 contracts leaves a local job seeker unmatched, evicting nobody
 a bulldozed workplace evicts every contract, not zero of them
 importable_remote_jobs never counts a seat already held by an EmploymentContract
 newly freed capacity is advertised exactly once after the contract is removed
 employer workplace tax reads current EmploymentContract state, not the old export ledger
 local job behavior remains covered by existing tests
```

### P8: Remove the inactive legacy job-export path

Scope:

```text
 remove JobExportRequest, JobExportGrant, and their allocation aliases
 remove ProcessJobExportRequest, ApplyJobExportGrant, and ReleaseJobExportAllocations
 remove the corresponding OutboundMessage variants and worker routing implementation
 remove release_and_request_job and old producer-allocation bookkeeping
 remove current_job_request_id and job_export_producers when no longer referenced
 remove assign_local_jobs_for_daily_tick and stale daily-rebuild comments
 remove legacy-only tests
 rename or simplify jobs_exports_dirty only if its remaining local meaning requires it
```

Behavior allowed:

```text
 compile-time cleanup of code made unreachable by P7
 mechanical simplification of imports, enums, dispatch, and tests
```

Behavior forbidden:

```text
 do not change claim, contract, release, loss, payment, or route behavior
 do not change local job allocation policy established by P7
 do not introduce another allocator or compatibility state machine
```

Review checks:

```text
 no legacy JobExport request/grant/release traffic types remain
 no old producer job-allocation ledger contributes capacity or tax
 all P7 connection, disconnection, stable-payment, and local-job tests pass unchanged
 source search finds no stale comments describing daily remote-job reconstruction
```

Each patch should stay green. Do not mix this with per-producer staleness;
that plan is the smaller improvement inside the existing batch-allocation
model, while this plan replaces the cross-region job authority model.

## Risks

- This is larger than per-producer staleness. It changes employment
  from "batch request/grant" to "claim/lease."
- The directory becomes authoritative for pending claim coordination. That
  is intentional, but it should not become authoritative for employer
  contract truth or citizen assignment truth.
- P7's route policy may revalidate more leases than strictly necessary when a
  discovery snapshot changes, but it must not silently keep unreachable jobs
  forever or react to unrelated goods/power availability noise. Discovery is
  one pass stale by design, so a road bulldozed this tick keeps its contract
  one extra day and a road built this tick is claimable next day — consistent
  with "may revalidate more than strictly necessary", not a bug.
- Save/load is a first-class patch, not a footnote. Accepted assignments
  and employer contracts must become durable, or load must reconcile
  before the first economy settlement.
- Terminal claim GC must be part of the implementation, otherwise rejected
  and released claims become an unbounded broker-side collection.

## Non-goals

- Do not centralize all local jobs in the first patch series.
- Do not let UI read `World` or ledger internals directly; expose view
  models through the existing facade boundary.
- Do not solve job quality/preference matching yet. First make cross-region
  employment stable and correct; ranking can improve later.

## P1, implemented (2026-07-09)

Landed the data model named in "P1: Data model only" above, verbatim to
the pseudocode in this doc — no invented fields, no invented methods, no
type renamed. Nothing outside the new module is touched; the existing job
path is unaware these types exist.

### Status

**Implemented, compile-only.** Matches P1's own bar exactly: types exist
and hold the shapes/derives their *later* pseudocode (P2-P7) requires to
compile against them, but nothing constructs or reads them from the
existing job path yet.

### Scope

One new file, one one-line registration edit. Nothing else.

```text
 src/core/regions/employment_directory.rs   (new)
 src/core/regions/mod.rs                    (+ `pub mod employment_directory;`)
```

### What changed

| type | matches |
|------|---------|
| `JobPool` | "## The model" — same 6 fields; `Debug, Clone, Copy`, deliberately no `PartialEq` (see below) |
| `CitizenRef` | exact derive line as given in the doc |
| `JobClaimId` | newtype, same shape as `RegionId`/`UiRequestId` elsewhere in this codebase (not shown as a standalone snippet in the doc, but named in P1's scope and required by every `JobClaim`/pending-index usage) |
| `JobClaim` | "## The model" — same 4 fields |
| `JobClaimDecision` | "## The model" — `Accepted`/`Rejected` variants, same payload |
| `EmploymentLeaseRef` | "## The model" — same 2 fields |
| `JobLoss` / `JobLossReason` | "## The model" — required by `EmploymentBrokerState.losses_by_home`, which P1's scope names directly ("EmploymentDirectory broker/snapshot storage shape") |
| `EmploymentContract` | "Stable Job Pool Identity" section — same 2 fields |
| `EmployerState` | "Stable Job Pool Identity" section — same 2 fields, not yet embedded in `RegionState` |
| `EmploymentBrokerState` | "## Directory storage" — same 12 fields, private (no `pub`, matching the doc) |
| `EmploymentSnapshot` | "## Directory storage" — same 5 fields |
| `EmploymentDirectory` | "## Directory storage" — `Mutex<EmploymentBrokerState>` + `RwLock<Arc<EmploymentSnapshot>>`, matching the doc's locking split |
| `same_pool_facts` | "## Publishing Pools" — copied verbatim; this is the one piece of *logic* P1 carries, because P1's own review checks name it explicitly ("`same_pool_facts` exists or equivalent comparison ignores generation") |

**Not included** (explicitly out of scope per P1's "Behavior forbidden"
and confirmed by re-reading which section each later snippet lives under):
`publish_pools`, `submit_claims`, `apply_claim_decisions`,
`take_pending_claims_for_employer`, `snapshot()`, `rebuild_snapshot_locked`,
the three `group_*` snapshot helpers, `diff_pools_for_employer`, every
`normalize_*` helper (their bodies are never given in the doc — writing
them now would mean inventing behavior the doc doesn't specify), the
release/loss/rebuild methods, `RegionEvent::EmploymentDirectoryReady`, and
every free `fn employer_*`/`fn home_*` function. All of that is P2-P7.
`EmployerState` is defined but not wired onto `RegionState`.

### Deviations from the doc, and why

None in shape. Two additions beyond the literal snippets, both structural
rather than behavioral:

- **`#[allow(dead_code)]` on `EmploymentBrokerState` and
  `EmploymentDirectory`**, each with a `// P1: ...` comment naming the
  patch that starts reading the field. Required because P1 is deliberately
  unwired — under plain `cargo clippy -- -D warnings` (no `--all-targets`),
  private fields never read outside `#[cfg(test)]` are flagged. Matches
  existing precedent in this codebase for the same situation
  (`src/core/systems/schedule.rs:79`, `road_network_analysis.rs:420`, both
  `#[allow(dead_code)] // P1-of-X; patch Y wires this.`).
- **Deliberately no `PartialEq`/`Eq` derive on `JobPool`.** The doc warns
  in prose ("`generation` is directory-owned metadata... do not compare
  the whole `JobPool`") but the struct shown in "## The model" doesn't
  show any derive line at all, so there's no derive attribute to omit
  literally — the doc simply never shows one. Reading the intent rather
  than a literal absence: giving `JobPool` a derived `PartialEq` would
  make `existing_pool == incoming_pool` compile and silently fold
  `generation` into the comparison, which is exactly the bug the doc's
  own prose warns against. Not deriving it turns that warning into a
  compile error if anyone tries it later, rather than relying on every
  future author remembering the comment. `same_pool_facts` is the
  sanctioned comparison.

### Tests added

Five, all in `src/core/regions/employment_directory.rs::tests`, all
compile-only/type-shape checks — P1 has no behavior to exercise yet:

- `same_pool_facts_ignores_generation` — direct check of the P1 review
  requirement.
- `same_pool_facts_catches_a_real_change` — same requirement, negative
  case (`open_count`, `salary`).
- `employment_directory_default_is_empty` — `EmploymentDirectory::default()`
  produces an empty broker and an empty, generation-0 snapshot.
- `citizen_ref_ordering_is_deterministic` — `CitizenRef` sorts by
  `(region, citizen)` in a `BTreeSet`, confirming the doc's "no HashMap
  iteration order" requirement holds for this key type.
- `job_claim_decision_and_lease_types_compile_and_hold_expected_fields` —
  constructs one of each remaining type (`JobClaimDecision`,
  `EmploymentLeaseRef`, `JobLoss`, `EmploymentContract`, `EmployerState`)
  and checks field plumbing, since P1's job is exactly "these types exist
  and hold together" and nothing else exercises them yet.

### Existing tests modified

None.

### Risks remaining

- None functional — no behavior shipped. The only carried-forward risk is
  documentation drift: if P2+ changes a field shape, this file and its
  tests need updating in lockstep, same as any staged data-model patch.
- The `#[allow(dead_code)]` annotations are a marker, not a guarantee —
  nothing stops a future patch from wiring in only *some* of the fields
  and leaving others permanently dead. Each later patch's own review
  checks (already written into this doc) are what catches that, not this
  patch.

### Assumptions

- `WorkplaceAssignment`, `Entity`, `RegionId`, `RegionRoadNetworkId` are
  reused as-is per "Reuse existing types where they already match the
  meaning" — none were modified.
- `EmployerState` is defined as its own standalone struct, matching the
  doc's literal `pub struct EmployerState { ... }` snippet, even though
  the doc's prose says "in this plan, 'employer' means the workplace-owning
  region state" (i.e. it will likely be embedded into `RegionState` once
  P2 wires it in). P1 does not decide that embedding — it only makes the
  type exist, per "no runtime use by the existing job path yet."

### Commands run

```text
 cargo build --lib                              → clean (only pre-existing
                                                    unrelated travel.rs warnings)
 cargo fmt
 cargo clippy -- -D warnings                     → 22 pre-existing errors,
                                                    confirmed via git stash
                                                    to predate this patch;
                                                    none reference the new file
 cargo test --lib employment_directory -q        → 5 passed
 cargo test -q                                   → 330 passed, 0 failed
                                                    (325 lib tests before
                                                    this patch, +5 new)
```

### Diagram — what P1 actually adds

```text
 BEFORE P1                          AFTER P1

 src/core/regions/                  src/core/regions/
   directory.rs   (RegionDirectory,   directory.rs        (unchanged)
   mod.rs          jobs/power/goods   employment_directory.rs  <- NEW,
                   export/grant,                               inert
                   unchanged)
                                      mod.rs   (+ pub mod employment_directory;)

 existing job path: unaware new types exist, before AND after
   home_region_daily_jobs / release_and_request_job / etc.
     — none of it references employment_directory.rs
     — same behavior, same tests, same daily wipe/gate as before this patch
```

## P2, implemented (2026-07-10)

Employer pool publishing. The directory can now accept an employer's
republished `JobPool` rows, diff them per-pool, and expose the claimable
result through `EmploymentSnapshot`. Still not wired into any tick: nothing
calls `published_job_pools` yet, and the old request/grant job path is
completely untouched.

### Status

**Implemented, still unwired.** All P2 machinery exists and is tested, but
`RegionState::published_job_pools` carries `#[allow(dead_code)]` because
P3 is the patch that starts calling it. No daily-tick behavior changed, no
save-format change, no UI change.

### Scope

| file | why |
|------|-----|
| `src/core/regions/employment_directory.rs` | `publish_pools`, `diff_pools_for_employer`, `normalize_pools`, `mark_pool_missing_for_validation`, `snapshot`, `rebuild_snapshot_locked`, the three `group_*` helpers, `PoolDelta` |
| `src/core/regions/mod.rs` | `RegionState::published_job_pools` (derives `JobPool` rows from the region's own effective workplace slots) |

Diff: 2 files, +723 / -6.

### What changed

| item | comes from |
|------|-----------|
| `publish_pools` | "Publishing Pools" — transcribed, including the removed/added/changed ordering and the `global_generation + 1` stamp |
| `diff_pools_for_employer` | "Publishing Pools" — the doc gives only the `match` arm; the `added`/`changed`/`removed`/`unchanged` split and ownership filter are derived from its prose |
| `same_pool_facts` | already landed in P1; now actually used |
| `normalize_pools` | never bodied in the doc; modeled on the established `normalize_links`/`normalize_hints` pattern in `directory.rs` (sort + dedup by identity) |
| `mark_pool_missing_for_validation` | "Loss And Invalidation" — transcribed verbatim, including the `pending_by_employer` cleanup |
| `snapshot` / `rebuild_snapshot_locked` | "Directory storage" — transcribed |
| `group_active_citizens_by_home` | "Directory storage" — transcribed |
| `group_accepted_by_home` / `group_pending_claims_by_employer` | never bodied in the doc; mechanical grouping, required for `rebuild_snapshot_locked` to compile as written |
| `RegionState::published_job_pools` | P2 scope line "employer regions compute published JobPool rows from current derived jobs"; reads the same `spare_job_slots_on_network` the old export path already uses |

**Not included** (P3+): `submit_claims`, `apply_claim_decisions`,
`take_pending_claims_for_employer`, `acknowledge_home_applied`, the
release/loss/rebuild methods, `RegionEvent::EmploymentDirectoryReady`, and
every `employer_*` / `home_*` free function. `EmployerState` remains
defined-but-unembedded.

### Deviations from the doc, and why

- **`*pool` instead of `pool.clone()`** in `rebuild_snapshot_locked`.
  `JobPool` is `Copy` (P1), so clippy's `clone_on_copy` is a hard error
  under this project's required `cargo clippy -- -D warnings` gate. The
  doc's pseudocode predates that constraint.
- **Explicit `BTreeMap<RegionId, BTreeSet<Entity>>` annotation** in
  `group_active_citizens_by_home`. The doc writes `BTreeMap::new()`, which
  does not type-check: `.or_default()` needs the value type pinned first.
- **Ownership filter in `diff_pools_for_employer`** (see "Bug found in
  review" below). The doc's pseudocode passes `employer` into the function
  but its `match` snippet never uses it. Filtering incoming rows to that
  employer is what makes the doc's own rule — *"the directory updates only
  that employer's pools"* — actually hold.
- **`published_job_pools` publishes a bridge workplace once**, under its
  lowest-id network. A workplace adjacent to two disconnected road networks
  appears in `spare_job_slots_on_network` for both, but `JobPool` names
  exactly one `network`; listing it twice would let two components each see
  the same seats as independently claimable. The doc never addresses this
  case. (`network_capacities` preserves `discover_road_networks`' ascending
  id order, so "lowest-id" is deterministic.)

### Bug found in review

Codex review (session `small_city`) caught a real correctness bug across
two rounds, both in `diff_pools_for_employer`:

1. **Round 1** — incoming pools were classified as added/changed with no
   check that the employer owns them. `publish_pools` is a public API, so
   employer A could add or overwrite a row owned by employer B just by
   naming B's workplace entity. (The *removal* pass was already scoped
   correctly, which is what made the asymmetry visible: `employer` was used
   in one loop and ignored in the other.)
2. **Round 2** — the round-1 fix filtered on `pool.region == employer`, but
   `pool.region` is a **self-declared field**. The real ownership authority
   is `workplace.region()` — the birth region packed into the `Entity` id,
   and the same authority `mark_pool_missing_for_validation` already uses to
   locate an employer. A could still spoof `region: A` while naming B's
   workplace and overwrite `pools_by_workplace[B_workplace]`.

Final guard requires **both** `pool.region == employer` **and**
`pool.workplace.region() == employer`, and the removal pass now scopes on
`existing.workplace.region()` too. Both failure modes have a regression
test, and both were confirmed to fail against the pre-fix code.

### Tests added

`src/core/regions/employment_directory.rs` (+7, 12 total in module):

- `publish_pools_bumps_generation_only_for_changed_pools_leaves_unchanged_pools_alone`
  — P2 review check *"unchanged pools keep their existing generation"*.
- `publish_pools_returns_false_when_republish_is_identical` — the
  idempotence fast-path actually fires.
- `publish_pools_removed_pool_drops_from_snapshot_and_clears_its_pending_indexes`
  — P2 review check *"mark_pool_missing_for_validation clears all pending
  indexes, including pending_by_employer"*.
- `rebuild_snapshot_subtracts_pending_capacity_and_hides_fully_pending_pools`
  — snapshot subtracts pending seats; a fully-pending pool disappears.
- `publish_pools_from_one_employer_cannot_touch_another_employers_pools`
  — the review bug above: A cannot change/add/remove B's row, nor overwrite
  it via a spoofed `pool.region`.
- `publish_pools_removing_a_pool_does_not_clear_accepted_employment`
  — P2 behavior-forbidden *"do not clear accepted employment when a pool
  disappears"*.
- `employment_directory_never_reads_private_world_storage` — P2 review
  check *"snapshot rebuild does not read private World storage"*. Source-scan
  contract test over the production half of the file; needles built with
  `.concat()` so the file never contains the literals it forbids.

`src/core/regions/mod.rs` (+2):

- `published_job_pools_reports_open_count_and_salary_for_one_effective_workplace`
- `published_job_pools_lists_a_bridge_workplace_once_not_once_per_network`

Four of these were mutation-tested (reverted the fix, confirmed the test
fails, restored).

### Existing tests modified

None.

### Risks remaining

- `pending_count as u16` in `rebuild_snapshot_locked` truncates above 65535
  pending claims on one workplace. Matches the doc's pseudocode; unreachable
  in practice since `open_count` is itself `u16` and P3's `submit_claims`
  caps pending at `open_count`. Flagged rather than "fixed" so as not to
  deviate.
- `published_job_pools` derives `open_count` by counting repeated entries in
  `spare_job_slots_on_network`, i.e. *remaining* (unassigned) local slots —
  not total capacity. That is the correct meaning for "claimable by a remote
  citizen," but it means a pool's `open_count` moves whenever local job
  assignment moves, and every such move is a `changed` pool that bumps a
  generation. Whether that churn matters only becomes visible in P3, when
  pending claims start being invalidated by generation mismatch.
- `EmployerState` is still not embedded in `RegionState`; P2 does not decide
  where it lives.

### Assumptions

- `pool.region` and `pool.workplace.region()` agree for every legitimately
  produced row. `published_job_pools` guarantees this (it sets
  `region: self.id` and only ever names workplaces built in its own grid).
  The directory no longer *trusts* it — it requires both to equal `employer`.
- Buildings do not relocate across regions, so a workplace `Entity`'s birth
  region is its owning region for the lifetime of the pool. (Citizens do
  relocate; workplaces do not.)

### Commands run

```text
 cargo fmt                                → clean
 cargo clippy -- -D warnings              → error set byte-identical to the
                                             pre-patch branch (verified by
                                             `git stash` + `diff` of sorted
                                             errors); zero new findings
 cargo test --lib employment_directory -q → 12 passed
 cargo test -q                            → 339 lib tests passed, 0 failed
                                             (333 before this patch, +6 net)
 codex exec resume small_city             → 3 rounds; 1 real bug across
                                             rounds 1-2, "No findings" on
                                             round 3
```

### Diagram — where P2 sits

```text
 EMPLOYER REGION (owns its World)            EMPLOYMENT DIRECTORY (owns no ECS)
 ────────────────────────────────            ─────────────────────────────────
 RegionState
   spare_job_slots_on_network(N)
        │  (already used by the OLD
        │   export path — unchanged)
        ▼
   published_job_pools()          ── Vec<JobPool> ──►  publish_pools(employer, pools)
     one row per workplace                                   │
     open_count = spare seats                                │ filter: pool.region == employer
     generation = 0 (directory stamps)                       │      && workplace.region() == employer
     bridge workplace → lowest-id network only               ▼
                                                       diff_pools_for_employer
                                                         added / changed  → stamp new generation
                                                         unchanged        → keep old generation
                                                         removed          → drop row +
                                                                            mark_pool_missing_for_validation
                                                                            (pending claims only —
                                                                             accepted employment survives)
                                                              │
                                                              ▼
                                                       rebuild_snapshot_locked
                                                         open_count -= pending_count
                                                         zero-claimable pools omitted
                                                              │
                                                              ▼
                                                       Arc<EmploymentSnapshot> swap
                                                         open_pools_by_network
```

### Diagram — the bug review caught

```text
 BEFORE (round 1)                      publish_pools(employer = A, [row])
   row { region: B, workplace: B_w } ──────► classified as added/changed
                                              └► overwrote pools_by_workplace[B_w]   ✘

 AFTER round-1 fix                     filter: row.region == A
   row { region: B, workplace: B_w } ──────► filtered out                            ✔
   row { region: A, workplace: B_w } ──────► PASSES the filter (region is
                                              caller-supplied, i.e. spoofable)
                                              └► still overwrote B's row            ✘

 AFTER round-2 fix                     filter: row.region == A
                                            && row.workplace.region() == A
   row { region: A, workplace: B_w } ──────► B_w's Entity encodes birth region B
                                              └► filtered out                        ✔

   ownership authority = the region packed into the workplace Entity id,
   never the caller-declared `region` field.
```

## P3, implemented (2026-07-10)

The claim flow: a home region submits claims against published pools, the
directory reserves pool capacity and citizen identity, and the employer
validates each claim against its own ECS and records a contract.

### Status

**Implemented, staged — deliberately not called from the daily tick.**

The old cross-region request/grant path is still the live allocator until
P7, and P4 is what teaches a home region to *apply* an accepted assignment.
Wiring `home_region_daily_jobs` into the daily job phase now would put two
allocators on the same spare workplace slots, and accepted claims would pile
up in the directory with nobody applying them. Everything is reachable and
tested; `home_region_daily_jobs` carries `#[allow(dead_code)]` until P4/P7
call it.

### Scope

| file | why |
|------|-----|
| `src/core/regions/employment_directory.rs` | `submit_claims`, `take_pending_claims_for_employer`, `apply_claim_decisions`, `choose_best_pool`, `normalize_claim_requests`, `normalize_claim_decisions`, `claim_id_of`; `mark_pool_missing_for_validation` generalized to `invalidate_pending_claims_for_pool` |
| `src/core/regions/mod.rs` | `RegionState` gains `employer_state`; `unemployed_citizens`, `job_pool_still_has_open_capacity`, `accept_claim_and_create_assignment` (+ two private helpers); `published_job_pools` now subtracts contracted seats |
| `src/core/regions/runtime/mod.rs` | `RegionEvent::EmploymentDirectoryReady`, `OutboundMessage::EmploymentDirectoryReady`, `set_employment_directory`, `state_mut`, the event handler, and the free functions `home_region_daily_jobs` / `employer_validate_claims` |
| `src/core/regions/worker.rs` | `RegionWorker` owns `Arc<EmploymentDirectory>`, installs it per slice, routes the wake through the deterministic barrier |
| `src/core/regional_game_runner.rs` | one `EmploymentDirectory` shared by every worker |

Diff: 5 files, +1497 / -16.

### What changed

| item | comes from |
|------|-----------|
| `submit_claims` | "Submitting Claims" — transcribed |
| `take_pending_claims_for_employer`, `apply_claim_decisions` | "Employer Validation" — transcribed |
| `home_region_daily_jobs`, `employer_validate_claims` | "Submitting Claims" / "Employer Validation" — transcribed, minus the P4/P5 calls |
| `RegionEvent::EmploymentDirectoryReady` + worker routing | "Submitting Claims" — the doc sketches `push_region_event`; the `ForwardedEventOrderKey` is new (see Deviations) |
| `normalize_claim_requests`, `normalize_claim_decisions` | never bodied in the doc; modeled on `normalize_pools`/`normalize_links` (sort by identity, then dedup) |
| `choose_best_pool` | never bodied; reachability rule derived (see Deviations) |
| `unemployed_citizens` | never bodied; mirrors the old path's `pending_job_demands` (sorted keys, `workplace_assignment.is_none()`) |
| `job_pool_still_has_open_capacity`, `accept_claim_and_create_assignment` | never bodied; semantics taken from the protocol's step 4/5 |
| `EmployerState` embedded in `RegionState` | "Stable Job Pool Identity" — P1 defined the type, P3 gives it a home |

**Not included** (later patches): `home_apply_accepted_employment`,
`acknowledge_home_applied` (P4); `request_release`, `confirm_release`,
`report_lost_employment`, `take_losses_for_home`, `employer_apply_releases`,
`home_apply_losses` (P5); `EmploymentDirectoryRebuild`,
`replace_broker_state` (P6). The plan's `handle_employment_directory_ready`
calls four functions; P3 wires only `employer_validate_claims`.

### Deviations from the doc, and why

- **`choose_best_pool(&snapshot, citizen)` → `(snapshot, discovery, home, home_networks)`.**
  The snapshot cannot answer reachability: `open_pools_by_network` carries no
  component graph. Jobs are network-scoped across regions exactly like power,
  so a pool is a candidate only when its network shares a
  `CrossRegionDiscovery` component with one of the home's border networks —
  the same rule the old job-export path uses. Ranking is lowest
  `(region, workplace)`; job-quality matching is an explicit plan non-goal.
  The function does not depend on the citizen at all, so it is called **once**
  per batch, not once per citizen.
- **`job_pool_still_has_open_capacity(workplace, generation, home_region)` →
  `(workplace)`.** The protocol's step 4 defines the whole check as *"pool
  still exists and has employer-owned capacity"*. `generation` is validated by
  the directory (see the review fix below) and recorded on the contract as
  `accepted_generation`; reachability was already decided by
  `choose_best_pool`, and an employer owns no topology to re-check it with.
  Capacity is `spare_job_slots_for_workplace - contracted_seats_at`.
- **`apply_claim_decisions` gained an ownership guard**
  (`claim.workplace.region() != employer → skip`), mirroring P2's fix. Without
  it, employer A could accept a claim against employer B's workplace and
  corrupt B's `open_count` and accepted cache.
- **`mark_pool_missing_for_validation` renamed to
  `invalidate_pending_claims_for_pool`** and now also runs for *changed*
  pools, not only removed ones. See the review fix below.
- **`ForwardedEventOrderKey` for the wake** — the doc never assigns one.
  `resource_rank: 4` (after power 0 / jobs 1 / goods 2 / travel 3), its own
  rank rather than reusing jobs(1) because the old job path is live until P7
  and the two must not interleave ambiguously. `request_id`/`token` are `0`:
  the event is payload-free, so two wakes for the same `(target, source)` in
  one pass are genuinely identical and idempotent.
- **`RegionWorker` owns the `Arc<EmploymentDirectory>` and installs it into
  each runtime per slice**, mirroring `set_discovery_generation`. The doc
  never says who owns it. `regional_game_runner` shares exactly **one** across
  all workers — per-worker brokers would each hand out the same seat.

### Bugs found in review

**1. Stale-generation claims reached the employer (codex, High).**
`publish_pools` invalidated pending claims when a pool was *removed*, but not
when its facts *changed*. A claim chosen at generation `G1` therefore survived
into `G2` and — because employer validation is capacity-only — was accepted
against facts that no longer held. The worst case is a citizen hired into a
pool whose `network` had moved out of the home's reachable component.

Fixed directory-side, where the generation authority lives: a *changed* pool
now drops its pending claims exactly as a removed one does. This is the direct
contrapositive of the plan's own sentence, *"Pending claims against untouched
pools stay valid"* — so a **touched** pool must not keep them. With the
directory guaranteeing every claim's generation is current, the capacity-only
employer check becomes sound.

**2. The employer resurrected contracted seats on republish (self-review).**
`published_job_pools` (P2) derived `open_count` from spare slots, which are net
of *local* assignment but know nothing about the `EmploymentContract`s P3
introduced. The plan says the directory's cached decrement lasts only *"until
next employer publish"* — so the republished count is the authoritative
replacement, and it was re-advertising seats already contracted out.

Fixed: `open_count = spare − contracted`, and a fully contracted workplace
publishes no row at all. This makes the two counts converge:

```text
 after an accept:
   directory cached open_count = published − 1     (apply_claim_decisions)
   employer's next published    = spare − contracted
                                = same number
   ⇒ the republish is UNCHANGED: no generation bump,
     and therefore no churn of other pools' valid pending claims.
```

Both bugs have mutation-tested regression tests (reverted the fix, confirmed
the test fails, restored).

### Tests added

`employment_directory.rs` (+11):

- `submit_claims_rejects_a_stale_generation` — review check *"submit_claims checks pool.generation"*.
- `submit_claims_never_exceeds_open_count` — behavior forbidden *"no workplace pool accepts more than open_count"*.
- `submit_claims_refuses_a_citizen_who_already_has_a_pending_or_accepted_job` — review check *"checks accepted_by_citizen and pending_by_citizen"*; behavior forbidden *"no citizen can hold two pending or accepted cross-region jobs"*.
- `apply_claim_decisions_clears_every_pending_index_and_wakes_the_home` — review checks *"removes claims from every pending index"* and *"returns home regions to wake for accepted and rejected claims"*.
- `apply_claim_decisions_from_one_employer_cannot_decide_another_employers_claim` — the ownership guard.
- `take_pending_claims_for_employer_does_not_drain_the_claims` — a second wake mid-validation must not lose a claim.
- `normalize_claim_requests_is_deterministic_and_dedups_exact_duplicates`, `normalize_claim_decisions_sorts_by_claim_id_and_dedups` — determinism.
- `choose_best_pool_only_offers_pools_reachable_from_a_home_network` — an unreachable pool is never chosen, however good its salary.
- `republishing_a_pool_with_changed_facts_invalidates_its_pending_claims` — review bug 1.
- `an_unchanged_pool_keeps_its_pending_claims_across_a_republish` — the other half: an unrelated pool's change must not drop this pool's claims.

`regions/mod.rs` (+4):

- `unemployed_citizens_lists_only_jobless_citizens_in_entity_order`
- `job_pool_still_has_open_capacity_counts_down_as_contracts_are_created`
- `accept_claim_records_the_claims_generation_on_the_contract`
- `published_job_pools_subtracts_seats_already_contracted_to_remote_citizens` — review bug 2.

`runtime/mod.rs`, new `employment_claim_flow_tests` module (+9):

- `a_claim_round_trip_contracts_the_seat_and_wakes_the_home`
- `the_wake_event_carries_no_claim_payload_and_pulls_work_from_the_directory` — review check *"EmploymentDirectoryReady carries no claim payload; regions pull work from the directory"*.
- `a_second_wake_is_a_cheap_no_op_once_the_claims_are_decided`
- `a_wake_without_an_installed_directory_is_a_no_op`
- `an_employer_never_contracts_more_seats_than_it_has`
- `choose_best_pool_ignores_a_pool_in_another_component`
- `an_employer_never_validates_a_claim_chosen_from_stale_pool_facts` — review bug 1, end-to-end.
- `republishing_after_an_accept_is_a_no_op_so_surviving_claims_are_not_churned` — the convergence property above.
- `a_fully_contracted_workplace_publishes_no_pool_but_keeps_its_accepted_workers`

### Existing tests modified

None.

### Risks remaining

- **Orphan contract on a dropped decision.** `employer_validate_claims` creates
  the contract *before* `apply_claim_decisions` confirms the claim still
  exists; if the claim had vanished, the decision is skipped but the contract
  remains. Unreachable today (a region's publish and validation both run on its
  own worker thread, so they cannot interleave), but it is an **unstated
  invariant** doing load-bearing work. If P4+ ever moves validation off that
  thread, this becomes a real orphan.
- **Local-churn starvation.** A pending claim is dropped whenever its pool's
  facts change. If an employer's *local* job assignment churns every pass, its
  `open_count` moves every pass, and remote claims could be invalidated
  repeatedly. Stable facts have no such path (confirmed in review); the
  post-accept republish is provably a no-op. Only local churn can trigger it.
- **All unemployed citizens claim the same pool.** `choose_best_pool` returns
  one pool per home region, so a batch of N citizens all target it and
  `submit_claims` admits only `open_count` of them. The rest retry next pass,
  even if a *second* reachable pool had free seats. This is the doc's own
  structure (`choose_best_pool(&snapshot, citizen)` ignores what other citizens
  picked); improving it is job-quality matching, an explicit non-goal.
- **Contracts are not serialized.** `RegionState` is not `Serialize`, so a
  loaded region starts with none. P6 makes them durable.
- **`pending_count as u16` / `contracted as u16`** truncate above 65535.
  Matches the doc; unreachable in practice.

### Assumptions

- Buildings do not relocate across regions, so a workplace `Entity`'s birth
  region is its owning region for the pool's lifetime. (Citizens relocate;
  workplaces do not.)
- `network_capacities` preserves `discover_road_networks`' ascending id order,
  so "a bridge workplace publishes under its lowest-id network" is
  deterministic.
- Every worker in a city shares one `EmploymentDirectory`. Enforced in
  `regional_game_runner`; the `#[cfg(test)]` `RegionWorker` constructors
  default a private one, which is fine for single-worker tests but would be
  wrong for a multi-worker fixture that exercised employment.

### Commands run

```text
 cargo fmt --check                        → clean
 cargo clippy -- -D warnings              → error set byte-identical to the
                                             pre-patch branch (git stash + diff
                                             of sorted errors); zero new findings
 cargo test --lib employment -q           → 34 passed
 cargo test -q                            → 363 lib tests passed, 0 failed
                                             (357 before this patch)
 codex exec resume small_city             → 3 rounds. Round 1: one High
                                             (stale generation). Round 2 and 3:
                                             "No findings."
```

### Diagram — the P3 round trip

```text
 HOME REGION A                DIRECTORY (owns no ECS)            EMPLOYER REGION B
 ─────────────                ──────────────────────             ─────────────────
 unemployed_citizens()
 network_border_links()
        │
        │ snapshot()  ── Arc clone, no lock held ──►
        │                open_pools_by_network
        │                active_citizens_by_home_region
        ▼
 choose_best_pool(discovery)
   reachable = same component as a home border network
   pick lowest (region, workplace)
        │
        │ submit_claims([(citizen, workplace, generation)])
        └──────────────────────►  reserve pool seat  (pending_by_workplace)
                                  reserve citizen    (pending_by_citizen)
                                  reject if: stale generation
                                             pending_count >= open_count
                                             citizen already pending/accepted
                                          │
                                          │ Vec<RegionId> = employers to wake
                                          ▼
                                  OutboundMessage::EmploymentDirectoryReady
                                          │ (barrier, resource_rank 4,
                                          │  payload-free)
                                          ▼
                                                        RegionEvent::
                                                        EmploymentDirectoryReady
                                                                │
                              ◄── take_pending_claims_for_employer ──┘
                                  (reads; does NOT drain)
                                                                │
                                                job_pool_still_has_open_capacity
                                                  spare − contracted > 0 ?
                                                                │
                                                  accept_claim_and_create_assignment
                                                    contracts_by_workplace[W][A7]
                                                    ⇒ EMPLOYER TRUTH
                                                                │
                              ◄── apply_claim_decisions(decisions) ──┘
                                  guard: workplace.region() == employer
                                  clear all 4 pending indexes
                                  accepted → open_count -= 1
                                             accepted_by_citizen (read cache)
                                          │
                                          │ Vec<RegionId> = homes to wake
                                          ▼
                                  EmploymentDirectoryReady → home
                                  (accepted AND rejected: a rejection is what
                                   releases the citizen's pending guard)

 P4 is what makes the home's wake do something: apply the assignment.
 In P3 the home's wake is a no-op, and the tick never starts any of this.
```

### Diagram — review bug 1: the stale-generation window

```text
 BEFORE                                    AFTER

 publish W @ G1                            publish W @ G1
   home reads W @ G1                         home reads W @ G1
   submit claim(W, G1)   ─┐                  submit claim(W, G1)   ─┐
                          │ pending           pending               │
 employer republishes W    │                employer republishes W  │
   facts changed → G2      │                  facts changed → G2    │
   pool row updated        │                  pool row updated      │
   ✗ claim still pending  ─┘                  ✓ invalidate_pending_claims_for_pool
                          │                     claim dropped, citizen un-pended
 employer validates       │                                        │
   capacity-only check ✓  │                employer validates      │
   ⇒ ACCEPTS a claim      │                  batch is empty        │
     chosen from G1 facts │                  ⇒ contracts nobody    │
     (e.g. a network that │                                        │
      no longer reaches A)│                home retries next pass against G2
```

## P4, implemented (2026-07-10)

Home apply. An accepted claim now becomes a durable
`Citizen.workplace_assignment` in the home region, and the existing economy
pays from it on the next daily settlement.

### Status

**Implemented, still staged.** P4 completes the round trip
(claim → accept → contract → *apply* → paid), but nothing drives it from the
daily tick: `home_region_daily_jobs` is still only called by tests. The old
request/grant path remains the live allocator until P7. So P4 changes no tick
behaviour on its own.

### Scope

| file | why |
|------|-----|
| `src/core/regions/mod.rs` | `RegionState::apply_workplace_assignment` |
| `src/core/regions/employment_directory.rs` | `EmploymentDirectory::acknowledge_home_applied` |
| `src/core/regions/runtime/mod.rs` | `home_apply_accepted_employment`, wired into `handle_employment_directory_ready` |

Diff: 3 files, +448 / -12.

**No economy change was needed.** `economy::run` already pays a remote
assignment from the salary captured at accept time
(`None => (assignment.salary, 0)` — a remote workplace pays the citizen but
its tax accrues to the exporting region). P4's scope line *"economy reads
applied WorkplaceAssignment for salary/payment"* was already satisfied; it
only needed proving.

### What changed

| item | comes from |
|------|-----------|
| `home_apply_accepted_employment` | "Applying Accepted Employment" — transcribed |
| `acknowledge_home_applied` | "Applying Accepted Employment" — see Deviations |
| `apply_workplace_assignment` | never bodied in the doc; mirrors the old path's `apply_job_export_grant` |
| handler now runs employer-then-home | the plan's `handle_employment_directory_ready` order, minus its two P5 calls |

**Not included** (later patches): `employer_apply_releases`,
`home_apply_losses`, `request_release`, `confirm_release`,
`report_lost_employment` (P5); `EmploymentDirectoryRebuild`,
`replace_broker_state` (P6); tick wiring and retiring the daily wipe (P7).

### Deviations from the doc, and why

- **`acknowledge_home_applied` is a true no-op.** The plan's body takes the
  broker lock and rebuilds/swaps the snapshot — but it mutates nothing, so
  that rebuild would produce a byte-identical snapshot at
  `O(pools + claims + accepted)` cost on *every* home wake. (It also would not
  compile clean: `let mut state` with no mutation trips clippy's `unused_mut`
  under this project's `-D warnings` gate.) The method is kept as the seam P6's
  restart/rebuild reconciliation needs. Behaviour is unchanged.
- **`apply_workplace_assignment` returns `bool` and never overwrites.** The doc
  never bodies it, only calls it and pushes the citizen onto `applied` when it
  returns true. One guard — *refuse to overwrite any existing assignment* —
  delivers two of P4's requirements at once:
  - *"repeated EmploymentDirectoryReady events are idempotent"*: the accepted
    read cache keeps re-offering an already-applied citizen; the second call
    answers `false` and changes nothing.
  - *"do not clear an old assignment while merely checking for replacement
    work"*: a citizen who picked up a local job between claim and apply keeps it.
- **`runtime.state()` → `runtime.state_mut()`** in the transcribed body; the doc
  writes an immutable borrow for a call that must mutate.
- **The snapshot's accepted list is read by reference**, not `.cloned()`. Same
  behaviour, one fewer allocation per wake.

### Why `refresh_jobs_cache_after_grant_applied`

`apply_workplace_assignment` calls `World::refresh_jobs_cache_after_grant_applied`,
**not** `invalidate_jobs_registry`. Re-flagging `jobs_exports_dirty` would make
the next daily job phase's wipe (`assign_local_jobs_for_daily_tick`) destroy the
very assignment just applied — the bug that method exists to close, documented
in retire-tickstate P-c. Exactly the same call, for exactly the same reason, as
the old path's `apply_job_export_grant`.

### Tests added

`regions/mod.rs` (+5):

- `apply_workplace_assignment_writes_the_citizen_and_is_idempotent` — review check *"repeated EmploymentDirectoryReady events are idempotent"*.
- `apply_workplace_assignment_never_clears_an_existing_assignment` — behavior forbidden *"do not clear an old assignment"*.
- `apply_workplace_assignment_ignores_a_citizen_that_no_longer_exists`
- `an_applied_remote_assignment_is_paid_by_the_next_daily_economy_phase` — review checks *"payment path uses home-region Citizen.workplace_assignment"* and *"accepted worker is paid on the next daily economy phase after apply"*. Drives `economy::run` directly and asserts the citizen's private money rises by exactly the captured salary. (The citizen starts solvent on purpose: a broke citizen skips rent, which would otherwise pollute the delta.)
- `applying_an_assignment_does_not_re_dirty_the_daily_wipe_gate` — the `refresh_jobs_cache_after_grant_applied` reasoning above.

`runtime/mod.rs` (+7):

- `home_apply_writes_the_accepted_assignment_onto_the_citizen`
- `the_home_wake_applies_accepted_employment_through_the_real_event_path` — through `RegionEvent::EmploymentDirectoryReady`, not the free function.
- `repeated_home_wakes_are_idempotent`
- `a_pending_claim_is_never_applied_or_paid` — behavior forbidden *"do not pay from pending claims"*.
- `a_rejected_claim_never_becomes_an_assignment` — review check *"rejected claims do not create assignments"*.
- `home_apply_does_not_overwrite_a_job_the_citizen_took_meanwhile`
- `the_directory_cache_is_not_the_durable_source_of_home_employment_truth` — behavior forbidden. Drops the entire broker and proves the citizen keeps its job.

### Existing tests modified

None.

### Housekeeping: three stale `#[allow(dead_code)]` removed

`apply_workplace_assignment`, `job_pool_still_has_open_capacity`, and
`accept_claim_and_create_assignment` are now genuinely reachable from non-test
code, via `handle_employment_directory_ready`. Each annotation was verified
stale by removing it and confirming no `dead_code` warning appears; the three
survivors (`published_job_pools`, `unemployed_citizens`'s caller
`home_region_daily_jobs`, and the broker-state fields) were verified *needed*
the same way. Two comments that credited P4 with tick wiring were corrected to
say P7.

### Risks remaining

- **The daily wipe still destroys an applied assignment on a *dirty* day.**
  `assign_local_jobs_for_daily_tick` clears every `workplace_assignment` when
  the jobs gate is dirty. `apply_workplace_assignment` deliberately does not
  re-open that gate, so a *quiet* day leaves the assignment alone — which is
  P4's "stable applied assignments remain across normal daily ticks". But a day
  made dirty by anything else (a build, a bulldoze, a moved discovery
  generation) still wipes it, and no wake would fire to re-apply. This is not a
  live regression because nothing drives the flow from the tick yet; **P7 is
  what must remove the wipe.**
- **A citizen who takes a local job between claim and apply strands its
  contract.** `apply_workplace_assignment` correctly refuses to overwrite, so
  the accepted employment is never applied — but the employer still holds the
  contract and the seat. P5's explicit release is what reclaims it. The
  directory's `accepted_by_citizen` also keeps the citizen out of
  `unemployed_citizens`' claim path via `active_citizens_by_home_region`.
- **`accepted_by_citizen` is never evicted by `acknowledge_home_applied`**, by
  design — it is a read cache, cleared only by release or employer-confirmed
  loss (P5). So `home_apply_accepted_employment` re-scans every accepted
  assignment for the region on every wake. Bounded by that region's employed
  cross-region citizens; idempotent.
- **Contracts and assignments are still not serialized.** P6.

### Assumptions

- The economy's remote-salary path (`assignment.salary`, no local workplace tax)
  is the intended payment route for a directory-accepted job, identical to the
  old export grant's. Confirmed by reading `economy::run`; unchanged by P4.
- A region may be both an employer and a home. `handle_employment_directory_ready`
  therefore runs both halves, employer first, so a claim accepted in one pass is
  visible to its home's own wake.

### Commands run

```text
 cargo fmt --check                        → clean
 cargo clippy -- -D warnings              → error set byte-identical to the
                                             pre-patch branch (git stash + diff
                                             of sorted errors); zero new findings
 cargo test --lib employment_claim_flow -q → 16 passed
 cargo test -q                            → 375 lib tests passed, 0 failed
                                             (363 before this patch, +12)
 codex exec resume small_city             → 1 round, "No findings"
```

### Diagram — what P4 closes

```text
 BEFORE P4 (end of P3)                     AFTER P4

 employer accepts a claim                  employer accepts a claim
   contracts_by_workplace[W][A7]             contracts_by_workplace[W][A7]
   ⇒ EMPLOYER TRUTH                          ⇒ EMPLOYER TRUTH
        │                                         │
        ▼                                         ▼
 directory accepted_by_citizen[A7]         directory accepted_by_citizen[A7]
   (read cache)                              (read cache)
        │                                         │
        │ home is woken...                        │ home is woken...
        ▼                                         ▼
   ...and does nothing.                      home_apply_accepted_employment
   A7 stays jobless.                           apply_workplace_assignment(A7)
   Nobody is paid.                               refuses if already employed
                                                 refresh_jobs_cache_after_
                                                   grant_applied  (NOT
                                                   invalidate → no wipe)
                                                    │
                                                    ▼
                                            Citizen.workplace_assignment
                                              ⇒ HOME TRUTH (durable)
                                                    │
                                                    ▼
                                            economy::run pays
                                              salary captured at accept time
                                              (no local workplace tax --
                                               that accrued to the employer)

 acknowledge_home_applied(applied) — a no-op today; the seam P6 uses to learn
 which assignments a home has really applied after a rebuild.
```

### Diagram — the three truths after P4

```text
        HOME REGION A                 DIRECTORY                EMPLOYER REGION B
        ─────────────                 ─────────                ─────────────────
 durable:                        read cache only:          durable:
   Citizen.workplace_assignment    accepted_by_citizen       contracts_by_workplace
   ⇒ who A pays                    accepted_by_workplace*    ⇒ who really holds a seat
                                   *redundant reverse index;
                                    P6 removes it

   drop the directory ─────────────────► both regions keep their truth
   (tested: the_directory_cache_is_not_the_durable_source_of_home_employment_truth)
```

## P5, implemented (2026-07-10)

Release and invalidation — the only two ways an accepted job ends. Loss is
never inferred; an employer decides, drops its own contract, and *tells* the
home region.

### Status

**Implemented, still staged.** The `EmploymentDirectoryReady` handler is now
the plan's full four-call shape. `employer_publish_pools` and `home_release_job`
are reachable and tested but not driven by the tick (P7).

### Scope

| file | why |
|------|-----|
| `src/core/regions/employment_directory.rs` | `request_release`, `take_releases_for_employer`, `confirm_release`, `report_lost_employment`, `take_losses_for_home`, `clear_accepted_cache_if_matches` |
| `src/core/regions/mod.rs` | `clear_employment`, `clear_employment_if_matches`, `release_contract_if_matches`, `release_contracts_no_longer_valid` |
| `src/core/regions/runtime/mod.rs` | `home_release_job`, `employer_apply_releases`, `home_apply_losses`, `employer_publish_pools`; handler completed to four calls |

Diff: 3 files, +1016 / -14.

### What changed

Every directory-side function is transcribed from "Loss And Invalidation".
The four `RegionState` methods it calls are never bodied in the doc.

```text
 handle_employment_directory_ready, now complete:
   employer_validate_claims        (P3)
   employer_apply_releases         (P5)  ← new
   home_apply_accepted_employment  (P4)
   home_apply_losses               (P5)  ← new
```

Employer-side work settles this pass's accepts and releases into the directory
*before* the home-side work reads the accepted cache and the loss queue.

**Not included**: `EmploymentDirectoryRebuild`, `replace_broker_state` (P6);
tick wiring and retiring the daily wipe (P7).

### Deviations from the doc, and why

- **`release_contracts_no_longer_valid` takes no `pools` argument.** The plan
  passes the freshly published pools, but they cannot answer the question:
  P3's `published_job_pools` omits a workplace whose `open_count` is zero, and a
  *fully contracted but perfectly healthy* workplace is exactly that. Absence
  from `pools` therefore cannot mean "invalid". It reads the employer's own ECS
  instead: a contract is valid iff `contracted <= spare_job_slots_for_workplace`.
  The plan's call *order* (pools computed before the release) is preserved and
  is harmless — after dropping the excess, `contracted == spare`, so the
  affected workplace's `open_count` is zero either way.
- **Eviction policy: seniority.** The plan delegates this outright ("the
  employer chooses which contracts are lost using deterministic local policy").
  Ours sorts by `(accepted_generation, citizen)` and evicts from the end, so the
  most recently hired lose first. Total order, hence deterministic.
- **`clear_employment_if_matches` / `release_contract_if_matches` drop the
  plan's redundant region parameters.** An `Entity` already packs its owning
  region, and `CitizenRef` already carries one; comparing the workplace alone is
  strictly stronger.
- **`employer_publish_pools` dedups its wake targets** through a `BTreeSet`.

### Bug found in review

**`confirm_release` handed back a seat it never freed (codex, High).** The
plan's pseudocode clears the accepted cache *conditionally* but increments
`open_count` *unconditionally*. A confirmation naming the right employer and
workplace but the **wrong citizen** therefore left the lease intact and still
advertised a phantom seat — violating P5's own review check, *"confirm_release
clears accepted cache only for the matching employer/workplace/citizen"*.

Fixed: `clear_accepted_cache_if_matches` now returns whether it matched, and
`confirm_release` gates the hand-back on that answer. A miss is not an error —
after a P6 rebuild the cache can be empty while real contracts exist, and the
employer's next authoritative publish recomputes `open_count = spare −
contracted` regardless. `report_lost_employment` deliberately ignores the
answer: it queues the loss either way, and the home re-checks the exact
workplace before clearing its citizen.

Mutation-tested: with the unconditional increment restored, the extended
`confirm_release_only_matches_...` test fails with `left: 2, right: 1`.

### A process bug this patch exposed

Codex also caught a test-only `unused_mut`. **`cargo clippy -- -D warnings` —
the command `CLAUDE.md` mandates — does not compile test targets**, so warnings
that live only in `#[cfg(test)]` code pass the required gate silently. Verified:
the plain form reports 0 hits for that warning, `--all-targets` reports 1. The
`claude-city-dev` skill now runs both forms and requires `cargo test` to be
warning-free for touched code. (Pre-existing lint debt on this branch: ~23 for
the lib, ~34 with `--all-targets`.)

### Known gap: route invalidation (deferred to P7, by decision)

The plan lists *"employer pool no longer reachable from the home region"* among
its invalidation cases, but P5 does **not** implement it. A contract survives a
workplace becoming unreachable from its worker's home region.

This is deferred deliberately, not overlooked:

- The plan itself files it under **Risks**, not as settled design: *"Route
  invalidation needs a clear policy: conservative route changes may revalidate
  more leases than strictly necessary, but must not silently keep unreachable
  jobs forever."*
- It appears in no P5 `Review check` and no `Behavior forbidden` line.
- `RegionState` owns no topology. An employer cannot compute reachability
  without new plumbing — `CrossRegionDiscovery` lives in the *other* directory.
- Nothing is tick-wired, so there is no live bug today.

**P7 must close this as part of its behavioral cutover.** The chosen policy is
employer-side validation. The worker already holds the discovery snapshot at
the point where publishing is driven, so it passes that snapshot into employer
reconciliation. A contract is lost when none of the workplace's current road
networks shares a discovery component with a network in the citizen's home
region. The employer removes the contract and reports `JobLoss`; the home does
not infer loss from directory cache state. A later reconnection creates a new
claim opportunity for unemployed citizens and never resurrects the old
contract. P8 only removes the legacy code after this behavior is active.

### Tests added

`employment_directory.rs` (+7):

- `request_release_keeps_the_seat_booked_until_the_employer_confirms` — review check, and forbidden *"do not advertise released capacity before employer confirms"*.
- `confirm_release_frees_the_seat_and_clears_the_accepted_cache`
- `confirm_release_only_matches_the_right_employer_workplace_and_citizen` — review check **and** the phantom-seat bug above.
- `report_lost_employment_clears_the_accepted_cache_and_wakes_the_home` — review check.
- `a_lost_lease_for_a_citizen_who_moved_on_leaves_the_new_job_alone`
- `take_losses_and_releases_drain_deterministically` — review check.
- `a_pool_vanishing_from_a_republish_never_ends_accepted_employment` — forbidden *"do not infer accepted job loss from missing snapshot rows"*.

`regions/mod.rs` (+6):

- `release_contracts_no_longer_valid_keeps_a_healthy_fully_contracted_workplace` — the exact reason the `pools` argument cannot drive the check.
- `release_contracts_no_longer_valid_drops_every_contract_when_the_workplace_dies`
- `release_contracts_evicts_the_most_recently_hired_first` — the seniority policy, driven by a local citizen taking a seat.
- `release_contract_if_matches_only_drops_the_exact_contract`
- `clear_employment_returns_the_assignment_it_gave_up`
- `clear_employment_if_matches_leaves_a_citizen_who_moved_on_alone` — forbidden.

`runtime/mod.rs` (+8):

- `an_explicit_release_frees_the_seat_only_after_the_employer_confirms`
- `republishing_after_a_confirmed_release_is_a_no_op` — the convergence invariant (below).
- `a_released_seat_can_be_claimed_again` — end to end.
- `a_release_racing_an_employer_loss_strands_nothing` — the drained-release hazard.
- `a_bulldozed_workplace_reports_an_explicit_loss_that_the_home_applies`
- `a_stable_worker_keeps_the_job_across_unrelated_republishes` — review check *"stable workers keep being paid until explicit release or employer-confirmed loss"*. (Asserts the assignment survives; P4 already proves the economy pays from it.)
- `a_stale_loss_never_clears_a_job_the_citizen_moved_to` — forbidden.
- `the_wake_handler_runs_release_and_loss_work_through_the_real_event_path` — scope line *"EmploymentDirectoryReady wakes both employer release work and home loss work"*.

### The convergence invariant, now for release

The same property P3 and P4 rely on, and the reason a release does not churn
every other pool's pending claims:

```text
 after confirm_release:
   directory cached open_count = published + 1        (confirm_release)
   employer's next published    = spare − contracted   (one fewer contract)
                               = the same number
   ⇒ the republish is UNCHANGED: no generation bump.
```

### Existing tests modified

None.

### Risks remaining

- **Route invalidation is not implemented.** See the named gap above. Mandatory
  in P7 before the directory path becomes live.
- **Local citizens preempt remote workers.** `assign_local_jobs` consumes
  `remaining_workplaces`, which knows nothing about contracts, so a local
  citizen can take a seat a remote worker holds. `release_contracts_no_longer_valid`
  then evicts the most recently hired remote worker to reconcile. That is a real
  gameplay/balance decision — locals win — and it is now the *only* thing making
  the two allocators consistent. P7 closes this by reserving contracted seats
  before local matching; P8 later removes the inactive old allocator.
- **`home_release_job` will happily release a *local* job.** It clears any
  assignment and enqueues a release keyed by `workplace.region()`, which for a
  local job is the region itself; `employer_apply_releases` then finds no
  contract and drops it. Harmless, but the local assignment is gone. The caller
  is responsible for only releasing cross-region jobs.
- **`employer_apply_releases` drops a release it cannot match.** Traced and
  tested: the only way the contract is already gone is that the employer lost it
  and reported that loss, which already cleared the accepted cache. Nothing is
  stranded. (`confirm_release`'s employer-mismatch guard is unreachable in
  practice, since `request_release` keys the queue by `workplace.region()`.)
- **Contracts and assignments are still not serialized.** P6.

### Assumptions

- `spare_job_slots_for_workplace` is current when `release_contracts_no_longer_valid`
  runs. Callers must `ensure_derived_state()` after a build/bulldoze; the tests
  do, and the tick will.
- The employer is free to choose *any* deterministic eviction policy. Seniority
  is a choice, not a requirement of the plan.

### Commands run

```text
 cargo fmt --check                          → clean
 cargo clippy -- -D warnings                → error set byte-identical to the
                                               pre-patch branch (23 pre-existing)
 cargo clippy --all-targets -- -D warnings  → byte-identical too (34 pre-existing)
 cargo test -q                              → 396 lib tests passed, 0 failed
                                               (375 before this patch, +21)
 codex exec resume small_city               → 2 rounds. Round 1: 1 High (phantom
                                               seat), 1 Medium (route invalidation,
                                               deferred by decision), 1 Low (the
                                               test-only unused_mut). Round 2:
                                               "No code findings."
```

### Diagram — the two ways an accepted job ends

```text
 (A) HOME RELEASES                          (B) EMPLOYER LOSES IT

 home_release_job(citizen)                  employer_publish_pools
   clear_employment                           release_contracts_no_longer_valid
   ⇒ HOME TRUTH cleared FIRST                   contracted > spare ?
        │                                         evict newest-hired
        │                                       ⇒ EMPLOYER TRUTH cleared FIRST
        ▼                                            │
   request_release(lease)                            ▼
     queue for employer                        report_lost_employment(loss)
     accepted cache UNTOUCHED ──┐                clear accepted cache
     seat still booked          │                queue loss for home
     citizen still "active"     │                     │
     (cannot claim a 2nd job)   │                     ▼
        │                       │              home_apply_losses
        ▼                       │                clear_employment_if_matches
   employer_apply_releases      │                  ONLY if it still names
     release_contract_if_matches│                  that workplace
        │ true                  │
        ▼                       │
   confirm_release ─────────────┘
     lease matched ?  no → return (no phantom seat)   ← the review bug
     yes → clear accepted cache
           open_count += 1     ⇒ seat advertised ONLY now
```

### Diagram — why `pools` cannot drive the validity check

```text
 published_job_pools omits any workplace whose open_count is 0.

   healthy, fully contracted        dead / shrunk
   ─────────────────────────        ─────────────
   spare 2, contracted 2            spare 0, contracted 2
   open = 0  → NO ROW               open = 0  → NO ROW
        ▲                                ▲
        └──────── indistinguishable ─────┘

 So release_contracts_no_longer_valid reads the employer's own ECS:

   valid  iff  contracted <= spare_job_slots_for_workplace(W)
```

## P7-a, implemented (2026-07-11)

The contract-seat reservation foundation. Employer-contracted seats are now
held out of local job matching by a **retained** registry input, and the three
call sites that used to subtract contracts themselves are dedup'd to the single
registry layer. No allocator cutover, no tick wiring, no route invalidation —
those are P7-b/c/d.

### Status

**Implemented, still staged.** P7-a changes *how* seats are reserved, but the
ledger is still not tick-driven — the reservation input is populated only by the
staged `accept_claim` / `release` paths (i.e. by tests) until P7-d flips the
allocator. Live local job behaviour is unchanged (a contract-free region has an
empty reservation set).

### Why P7 is split

P7 as written touches ~7 files, rewires the daily tick, and flips the live
cross-region job allocator — far past this repo's "propose a split beyond ~5
files / ~400 lines" rule, and exactly the kind of behavioural cutover
`retire-tickstate` split into P-a..P-e. Split, dependency-ordered, each green:

```text
 P7-a  retained contract reservations + dedup the three subtractions  (DONE)
 P7-b  connectivity-only fingerprint on the directory
 P7-c  discovery Arc install on RegionRuntime + route invalidation
 P7-d  the cutover: daily_employment_phase into the tick, drop the wipe,
         stop the old path, employer tax from contracts
```

### Scope

| file | why |
|------|-----|
| `src/core/world.rs` | `job_reservations` retained input + `set_job_reservations` |
| `src/core/resource_registry.rs` | `reserve_contracted_seats`, `JobResolution.reserved_seats_by_workplace`, reservation-aware `unemployment` |
| `src/core/regions/mod.rs` | `sync_job_reservations`, dedup in `published_job_pools` / `job_pool_still_has_open_capacity`, rename+rework the eviction fn, delete dead `contracted_seats_at` |
| `src/core/regions/runtime/mod.rs` | update the one caller of the renamed eviction fn (staged) |

### Deviation: the input lives on `World`, not `ResourceRegistryCache`

The plan says "ResourceRegistryCache gains a retained reservation input". It's
on `World` instead. The parity guard
(`cached_registry_matches_forced_recompute_script`) compares a cached
`JobResolution` against a fresh `for_jobs(world)` recompute; both read `World`,
so a `World` field is seen by both and parity holds. A cache-only field would
make the fresh recompute diverge. It still satisfies the plan's *intent* —
retained, survives `invalidate_jobs`, feeds resolve — verified by
`reservations_survive_a_jobs_cache_invalidation`. Codex confirmed the placement
sound.

### The two traps, and how P7-a avoids each

- **Retained, not injected (trap 1).** `ensure_jobs` rebuilds `JobResolution`
  from `World` on any `invalidate_jobs_registry`. Because `job_reservations` is
  a `World` field (not a per-call argument), the rebuild re-reads it — the
  reserved seat is never re-exposed. `set_job_reservations` has a no-op fast
  path so a contract-free region doesn't churn its registry.
- **No `remaining + contracts` reconstruction (trap 2).**
  `reserve_contracted_seats` caps at physical seats
  (`min(contract_count, physical)`) by removing at most `contract_count` copies
  of a workplace and stopping when none remain. A bulldozed workplace (0
  physical) reserves 0, so `evict = contracts - 0 = contracts` — all of them,
  which is correct. Eviction reads `reserved_seats_by_workplace` from the
  resolution, never `spare_job_slots_for_workplace` (which is *post*-reservation
  and would be circular).

### The single subtraction layer

Three call sites used to each subtract contracts from a `remaining_workplaces`
that did *not* reserve them. P7-a moves the subtraction into the registry and
removes all three:

| call site | before | after |
|-----------|--------|-------|
| `published_job_pools` | `open = spare - contracted` | `open = spare` (registry already net) |
| `job_pool_still_has_open_capacity` | `spare > contracted` | `spare > 0` |
| eviction (`release_contracts_over_current_capacity`) | `holders <= spare` (circular) | `holders <= reserved`, evict `holders - reserved` |

`availability_hints` → `importable_remote_jobs` reads the same
`remaining_workplaces`, so it inherits the reservation and stops over-counting
seats already contracted to another region — the free win the plan noted, and
confirmed by codex to introduce no fourth double-subtraction.

### Bugs found in review

- **Unemployment under-reported (codex, Medium).** `unemployment =
  job_seekers - total_jobs`, but `total_jobs` counts physical seats including
  those reserved for remote contracts — so a 2-seat / 2-contract workplace with
  one unmatched local reported **zero** unemployment (and, downstream, spurious
  happiness). Fixed: `unemployment = max(0, job_seekers - (total_jobs -
  reserved_total))`. With no contracts `reserved_total` is 0, byte-identical to
  the old formula, so the existing contract-free `unemployment` assertions are
  untouched. Chose "subtract reserved from total" over "count unmatched
  assignments" to leave the pre-existing unreachable-seat approximation alone —
  codex agreed that's the right conservative call for P7-a.
- **Stale comments/name (codex, Low).** `spare_job_slots_on_network` /
  `spare_job_slots_for_workplace` described only local assignment; updated to
  note they now also exclude reservations. Renamed
  `published_job_pools_subtracts_...` → `..._excludes_...`.

### Tests added

`resource_registry.rs` (+4):

- `reserve_contracted_seats_caps_at_physical_and_reports_the_reservation` — trap 2 (the cap).
- `reserved_seats_are_held_out_of_remaining_workplaces` — the core behaviour.
- `reservations_survive_a_jobs_cache_invalidation` — trap 1 (the retained-input property).
- `set_job_reservations_is_a_no_op_when_unchanged` — the fast path doesn't churn the cache.

`regions/mod.rs` (+1, and 1 rewrite):

- `contracted_seats_are_reserved_before_local_matching` — contracts win over a local seeker; nobody evicted; **unemployment == 1** (the Medium regression).
- `release_contracts_evicts_the_most_recently_hired_first` — rewritten: eviction now fires on physical capacity loss (3 contracts on 2 seats → evict newest), not on a local citizen taking a seat (which P7-a forbids).

### Existing tests modified

Two P5 eviction tests renamed for the function rename; one
(`release_contracts_evicts_the_most_recently_hired_first`) had its premise
rewritten because P7-a inverts it (a local citizen can no longer displace a
contract). No assertion weakened.

### Risks remaining

- **Reservation sync is per-mutation, not batched.** Evicting N contracts calls
  `release_contract_if_matches` N times, each re-syncing (N cache
  invalidations). Correct, mildly wasteful; a daily-frequency op. Left as-is to
  keep `release_contract_if_matches` self-consistent for its P5 callers.
- **Not yet load-bearing.** Nothing tick-drives the reservation until P7-d.
  The staged accept/release paths keep it correct; P7-d is where it goes live.

### Commands run

```text
 cargo fmt --check                          → clean
 cargo clippy -- -D warnings                → no new findings vs pre-patch
 cargo clippy --all-targets -- -D warnings  → no new findings vs pre-patch
 cargo test -q                              → 401 lib tests pass (was ~397), +7 net
 codex exec resume small_city               → 2 rounds: 1 Medium + 1 Low, then
                                               "No findings"
```

### Diagram — one subtraction layer

```text
 BEFORE P7-a                            AFTER P7-a

 workplace_slots (physical, repeated)   workplace_slots (physical, repeated)
        │                                      │  reserve_contracted_seats:
        │                                      │  remove min(contracts, physical)
        ▼                                      ▼
 remaining_workplaces                   remaining_workplaces (net of RESERVATION)
   = physical - locals                    = physical - reserved - locals
        │                                      │
   read by:                               read by (all already net):
     published_job_pools  - contracts       published_job_pools     (no subtract)
     job_pool_has_cap     - contracts       job_pool_has_cap        (> 0)
     eviction             vs spare          eviction  vs reserved_seats_by_workplace
     availability_hints   (over-counts!)    availability_hints      (correct)
        ▲                                      ▲
   three separate subtractions,           ONE subtraction, in the registry,
   availability_hints forgotten           fed by a retained World input
```
