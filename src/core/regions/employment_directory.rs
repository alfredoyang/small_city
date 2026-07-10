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
/// Cross-region employment broker (P1: storage shape only).
///
/// Not allowed to expose `World` to UI and does not mutate a region's ECS
/// directly. Owns claim coordination and read snapshots, not the final
/// employment truth: the employer region remains the source of truth for
/// whether a worker is really reserved; the home region remains the source
/// of truth for whether a citizen has applied the assignment.
#[allow(dead_code)] // P1: data model only; P2's publish_pools starts using these fields.
pub struct EmploymentDirectory {
    broker: Mutex<EmploymentBrokerState>,
    active_snapshot: RwLock<Arc<EmploymentSnapshot>>,
}

/// Stable-fact equality for a `JobPool`: everything the employer controls,
/// excluding the directory-owned `generation`. Comparing whole `JobPool`
/// values would make an unrelated republish look like a change to every
/// pool from that employer — see "Publishing Pools" in the plan.
#[allow(dead_code)] // P1: data model only; wired by P2's diff_pools_for_employer.
fn same_pool_facts(a: &JobPool, b: &JobPool) -> bool {
    a.region == b.region
        && a.workplace == b.workplace
        && a.open_count == b.open_count
        && a.network == b.network
        && a.salary == b.salary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::city_refs::CityCellRef;

    fn pool(region: u32, workplace: u64, open_count: u16, salary: i32, generation: u64) -> JobPool {
        JobPool {
            region: RegionId(region),
            workplace: Entity(workplace),
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
}
