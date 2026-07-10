# Directory employment ledger — stable cross-region jobs without daily wipes

Status: **proposal, P1 + P2 implemented** (data model, and employer pool
publishing — see "P1, implemented" and "P2, implemented" at the end of
this doc; P3-P7 still plan-only). This is an
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
    // Optional read cache of accepted claims. This mirrors region truth so
    // home regions can discover accepted employment cheaply; it is not the
    // authority for whether the employer contract actually exists.
    accepted_by_citizen: BTreeMap<CitizenRef, WorkplaceAssignment>,
    accepted_by_workplace: BTreeMap<Entity, BTreeSet<CitizenRef>>,
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

        // Keep accepted_by_workplace populated until the employer confirms. That
        // prevents this capacity from being advertised as open during release.
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
    let mut remove_pool_entry = false;
    if let Some(workers) = state
        .accepted_by_workplace
        .get_mut(&release.workplace)
    {
        workers.remove(&release.citizen);
        remove_pool_entry = workers.is_empty();
    }
    if remove_pool_entry {
        state.accepted_by_workplace.remove(&release.workplace);
    }
}
```

This is the part that enforces the user-facing rule:

```text
 worker keeps being paid until:
   employer confirms the pool contract is invalid, or
   home explicitly releases the assignment
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
`EmploymentContract`; the directory's `accepted_by_citizen` /
`accepted_by_workplace` maps are only read caches.

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
 request_release keeps accepted_by_workplace populated until confirm_release
 confirm_release clears accepted cache only for the matching employer/workplace/citizen
 report_lost_employment clears accepted cache and wakes the home region
 take_losses_for_home drains loss queue deterministically
 stable workers keep being paid until explicit release or employer-confirmed loss
```

### P6: Save/load and directory rebuild

Scope:

```text
 persist or reconstruct home applied WorkplaceAssignment
 persist or reconstruct employer EmploymentContract maps
 rebuild directory from regional truth before first economy settlement
 publish pools, employer contracts, and home assignments into a scratch rebuild state
 atomically replace broker state and snapshot after reconciliation
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
 reconciliation handles:
   matching employer contract + home assignment
   employer contract without home assignment
   home assignment without employer contract
   pending claim
```

### P7: Remove old cross-region wipe/re-request path

Scope:

```text
 remove cross-region job dependence on jobs_exports_dirty daily wipes
 keep local-only job assignment path working
 remove or retire old cross-region request/grant cleanup that conflicts with leases
 route new cross-region job behavior through applied assignments and explicit releases/losses
```

Behavior allowed:

```text
 local daily assignment can still fill local jobs
 cross-region workers keep stable assignments across unrelated goods/power changes
 explicit rematch policy may release/reclaim jobs later, but not in this patch
```

Behavior forbidden:

```text
 do not wipe all cross-region workplace assignments as a validation proxy
 do not leave old stale-grant cleanup clearing stable workers
 do not let unrelated resource noise fire workers
```

Review checks:

```text
 unrelated goods/power changes do not fire cross-region workers
 stable worker remains paid across normal ticks
 job loss still happens when employer reports invalidation
 local job behavior remains covered by existing tests
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
- Route invalidation needs a clear policy: conservative route changes may
  revalidate more leases than strictly necessary, but must not silently keep
  unreachable jobs forever.
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
