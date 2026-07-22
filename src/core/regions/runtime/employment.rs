use std::collections::BTreeSet;

use super::{OutboundMessage, RegionEvent, RegionRuntime};
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::core::regions::coordinator::{RegionRecipients, RoutedRegionEvent};
use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::employment_directory::{
    CitizenRef, EmploymentDirectory, EmploymentLeaseRef, JobClaimDecision, JobLoss, JobLossReason,
    choose_best_pool,
};

impl RegionRuntime {
    /// P3/P4/P5: pull whatever employment work the directory holds for this
    /// region. This is now the plan's full four-call handler.
    ///
    /// One region can be both an employer and a home, so all four run. The order
    /// is the plan's, and it matters: employer-side work settles this pass's
    /// accepts and releases into the directory *before* the home-side work reads
    /// the accepted cache and the loss queue.
    ///
    /// ```text
    ///   employer_validate_claims        accept/reject pending claims
    ///   employer_apply_releases         free seats the home gave back
    ///   home_apply_accepted_employment  write Citizen.workplace_assignment
    ///   home_apply_losses               clear assignments the employer lost
    /// ```
    ///
    /// A runtime with no directory installed (a bare `RegionRuntime::new`, or
    /// a worker that never set one) treats the wake as a no-op rather than
    /// panicking.
    pub(super) fn handle_employment_directory_ready(&mut self) -> Vec<OutboundMessage> {
        let Some(directory) = self.employment_directory.clone() else {
            return Vec::new();
        };
        let mut outbound = employer_validate_claims(self, &directory);
        employer_apply_releases(self, &directory);
        outbound.extend(home_apply_accepted_employment(self, &directory));
        home_apply_losses(self, &directory);
        outbound
    }

    /// P3: the wake fan-out. One payload-free message per target region; the
    /// coordinator routes them through the same event path as every other
    /// cross-region event.
    pub(super) fn emit_employment_directory_ready(
        &self,
        regions: Vec<RegionId>,
    ) -> Vec<OutboundMessage> {
        regions
            .into_iter()
            .map(|target_region| {
                OutboundMessage::CoordinatorRoute(RoutedRegionEvent {
                    recipients: RegionRecipients::One(target_region),
                    event: RegionEvent::EmploymentDirectoryReady,
                })
            })
            .collect()
    }

    /// P7-d: the ledger's cross-region employment phase, run only on a dirty
    /// daily tick. Local assignment already happened (no wipe); this owns the
    /// ledger side.
    ///
    /// Order (deviates from the plan pseudocode, which predates P7-a's retained
    /// reservations and its own double resolve/apply is now automatic):
    ///   1. route invalidation — drop contracts whose home no longer reaches the
    ///      workplace (frees their reserved seats via the P7-a sync).
    ///   2. capacity invalidation — drop contracts a shrunk/bulldozed workplace
    ///      can no longer physically hold.
    ///   3. report every dropped contract as an explicit `JobLoss` (wakes homes).
    ///   4. publish this region's pools (open_count already net of contracts).
    ///   5. submit fresh claims for this region's still-jobless citizens.
    ///
    /// The employer-validate / home-apply / release halves run when the wakes
    /// this emits are processed (`handle_employment_directory_ready`).
    ///
    /// A runtime with no directory installed does only local work (steps 1-5 are
    /// skipped): a single-region game has no cross-region employment.
    pub(super) fn daily_employment_phase(&mut self) -> Vec<OutboundMessage> {
        let Some(directory) = self.employment_directory.clone() else {
            return Vec::new();
        };
        let discovery = self.discovery.clone();

        let mut lost = Vec::new();
        if let Some(discovery) = discovery.as_deref() {
            lost.extend(
                self.state
                    .release_contracts_with_unreachable_homes(discovery),
            );
        }
        lost.extend(self.state.release_contracts_over_current_capacity());

        let mut wake = BTreeSet::new();
        for (workplace, citizen, _contract) in lost {
            wake.extend(directory.report_lost_employment(JobLoss {
                lease: EmploymentLeaseRef { citizen, workplace },
                reason: JobLossReason::PoolInvalid,
            }));
        }

        directory.publish_pools(self.region_id(), self.state.published_job_pools());

        let mut outbound = self.emit_employment_directory_ready(wake.into_iter().collect());
        if let Some(discovery) = discovery.as_deref() {
            outbound.extend(home_region_daily_jobs(self, &directory, discovery));
        }
        outbound
    }
}

/// P3, home side: submit one claim batch for this region's unemployed citizens.
///
/// **Not called from the tick.** P3 stages the claim flow; the old
/// request/grant path is still the live allocator until P7, and P4 is what
/// teaches the home region to apply an accepted assignment. Wiring this into
/// the daily job phase now would have two allocators drawing on the same spare
/// workplace slots. Tests drive it directly.
///
/// Citizens already spoken for — pending or accepted, per the directory's
/// `active_citizens_by_home_region` — are skipped before any lock is taken.
/// The directory re-checks the same rule inside `submit_claims`, because this
/// snapshot may be one pass stale.
#[allow(dead_code)] // P3: staged; the daily job phase starts calling this in P7.
pub(crate) fn home_region_daily_jobs(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
    discovery: &CrossRegionDiscovery,
) -> Vec<OutboundMessage> {
    let snapshot = directory.snapshot(); // cheap Arc clone; no directory lock held below
    let home = runtime.region_id();
    let active_citizens = snapshot
        .active_citizens_by_home_region
        .get(&home)
        .cloned()
        .unwrap_or_default();

    let home_networks = runtime
        .state()
        .network_border_links()
        .into_iter()
        .map(|link| link.network)
        .collect::<Vec<_>>();

    // Hoisted out of the per-citizen loop: the plan writes
    // `choose_best_pool(&snapshot, citizen)`, but reachability and ranking
    // depend only on the *home region*, not on which of its citizens is
    // asking. Every unemployed citizen would get the same answer, so compute
    // it once. `submit_claims` caps the batch at the pool's `open_count`; the
    // citizens it turns away retry next pass, when the snapshot no longer
    // advertises the seats already reserved.
    let Some(pool) = choose_best_pool(&snapshot, discovery, home, &home_networks) else {
        return Vec::new(); // nothing reachable and open; nobody to wake
    };

    let claims = runtime
        .state()
        .unemployed_citizens()
        .into_iter()
        .filter(|citizen| !active_citizens.contains(citizen))
        .map(|citizen| {
            (
                CitizenRef {
                    region: home,
                    citizen,
                },
                pool.workplace,
                pool.generation,
            )
        })
        .collect::<Vec<_>>();

    // One short lock to reserve pending claims. The returned regions are wake
    // targets only; the claims themselves stay in the directory.
    let regions_to_wake = directory.submit_claims(claims);
    runtime.emit_employment_directory_ready(regions_to_wake)
}

/// P3, employer side: decide every pending claim against this region's own ECS.
///
/// Reads the batch, validates each claim against employer-owned capacity, and
/// hands compact decisions back. Both accepted *and* rejected decisions wake
/// the home region: an acceptance is ready to apply (P4), and a rejection is
/// what releases the home's citizen-side pending guard so it can retry.
///
/// If several wakes land before this runs, the first call decides the pending
/// claims and `apply_claim_decisions` clears them, so later wakes see an empty
/// batch and return immediately.
pub(crate) fn employer_validate_claims(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let claims = directory.take_pending_claims_for_employer(runtime.region_id());
    if claims.is_empty() {
        return Vec::new();
    }

    let decisions = claims
        .into_iter()
        .map(|claim| {
            if runtime
                .state()
                .job_pool_still_has_open_capacity(claim.workplace)
            {
                JobClaimDecision::Accepted {
                    claim_id: claim.claim_id,
                    assignment: runtime
                        .state_mut()
                        .accept_claim_and_create_assignment(&claim),
                }
            } else {
                JobClaimDecision::Rejected {
                    claim_id: claim.claim_id,
                }
            }
        })
        .collect::<Vec<_>>();

    let regions_to_wake = directory.apply_claim_decisions(runtime.region_id(), decisions);
    runtime.emit_employment_directory_ready(regions_to_wake)
}

/// P4, home side: write every accepted assignment for this region's citizens
/// into their durable `Citizen.workplace_assignment`, then tell the directory
/// which ones actually landed.
///
/// The directory's `accepted_by_home_region` is a **read cache**, not truth: it
/// keeps re-offering an already-applied citizen on every wake, and
/// `apply_workplace_assignment` answers `false` for those. So a repeated
/// `EmploymentDirectoryReady` is idempotent, and only *newly* applied citizens
/// are acknowledged.
///
/// Nothing is paid from a *pending* claim: a claim only reaches
/// `accepted_by_home_region` once its employer has accepted it and recorded a
/// contract. The economy then pays from the applied assignment on the next
/// daily settlement, using the salary captured at accept time — the same path
/// the old export grant already used.
pub(crate) fn home_apply_accepted_employment(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let snapshot = directory.snapshot(); // cheap Arc clone; no directory lock held below
    let Some(accepted) = snapshot.accepted_by_home_region.get(&runtime.region_id()) else {
        return Vec::new();
    };

    let mut applied = Vec::new();
    let mut declined = Vec::new();
    for (citizen, assignment) in accepted {
        if runtime
            .state_mut()
            .apply_workplace_assignment(citizen.citizen, *assignment)
        {
            applied.push(*citizen);
        } else if !runtime
            .state()
            .citizen_holds_workplace(citizen.citizen, assignment.workplace)
        {
            // The employer accepted this claim and is reserving+taxing the seat,
            // but this home can never apply it: the citizen took a local job or
            // left between claim and apply. Decline so the employer frees the
            // seat, otherwise the contract is a phantom that reserves and taxes a
            // seat nobody works. (An already-applied lease answers `false` too,
            // but `citizen_holds_workplace` keeps it out of this branch.)
            declined.push(EmploymentLeaseRef {
                citizen: *citizen,
                workplace: assignment.workplace,
            });
        }
    }

    directory.acknowledge_home_applied(applied);

    let mut wake = BTreeSet::new();
    for lease in declined {
        wake.extend(directory.request_release(lease));
    }
    runtime.emit_employment_directory_ready(wake.into_iter().collect())
}

/// P5, home side: this citizen gives its job up.
///
/// The home clears its own truth first, then asks the employer to free the
/// seat. Between the two, the directory still lists the citizen as accepted,
/// which is what stops it claiming a second job before the first is confirmed
/// released — and what stops the seat being advertised twice.
#[allow(dead_code)] // P5: staged; no gameplay action releases a job yet.
pub(crate) fn home_release_job(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
    citizen: Entity,
) -> Vec<OutboundMessage> {
    let Some(assignment) = runtime.state_mut().clear_employment(citizen) else {
        return Vec::new(); // nothing to release
    };
    let regions_to_wake = directory.request_release(EmploymentLeaseRef {
        citizen: CitizenRef {
            region: runtime.region_id(),
            citizen,
        },
        workplace: assignment.workplace,
    });
    runtime.emit_employment_directory_ready(regions_to_wake)
}

/// P5, employer side: honour the release requests queued for this region.
///
/// Only a release the employer can actually match is confirmed. One it cannot
/// (the contract is already gone — typically because the employer lost it first
/// and reported that loss) is dropped: the accepted cache was already cleared
/// by that loss, so there is nothing left to free.
pub(crate) fn employer_apply_releases(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) {
    for release in directory.take_releases_for_employer(runtime.region_id()) {
        if runtime
            .state_mut()
            .release_contract_if_matches(release.workplace, release.citizen)
        {
            directory.confirm_release(runtime.region_id(), release);
        }
    }
}

/// P5, home side: apply the losses an employer has confirmed.
///
/// Clears the citizen's assignment only if it still names the lost workplace —
/// a loss report can be one pass stale, and the citizen may already have moved
/// to a different job.
pub(crate) fn home_apply_losses(runtime: &mut RegionRuntime, directory: &EmploymentDirectory) {
    for loss in directory.take_losses_for_home(runtime.region_id()) {
        runtime
            .state_mut()
            .clear_employment_if_matches(loss.lease.citizen.citizen, loss.lease.workplace);
    }
}

/// P5, employer side: republish this region's pools, and explicitly report
/// every contract it can no longer honour.
///
/// Loss is never inferred from a pool vanishing out of a snapshot. The employer
/// decides, drops the contract in its own state, and *tells* the home region.
///
/// Deviation: the plan passes the freshly computed `pools` into
/// `release_contracts_over_current_capacity`; they cannot answer the question
/// (see that method, which reads the registry's reservation instead). The
/// pool/eviction *ordering* is revisited by P7-d's cutover; this staged
/// version keeps the plan's "pools before release" order.
#[allow(dead_code)] // P7-a: staged; the daily tick starts publishing in P7-d.
pub(crate) fn employer_publish_pools(
    runtime: &mut RegionRuntime,
    directory: &EmploymentDirectory,
) -> Vec<OutboundMessage> {
    let pools = runtime.state().published_job_pools();
    let lost_contracts = runtime
        .state_mut()
        .release_contracts_over_current_capacity();

    directory.publish_pools(runtime.region_id(), pools);

    let mut regions_to_wake = BTreeSet::new();
    for (workplace, citizen, _contract) in lost_contracts {
        regions_to_wake.extend(directory.report_lost_employment(JobLoss {
            lease: EmploymentLeaseRef { citizen, workplace },
            reason: JobLossReason::PoolInvalid,
        }));
    }
    runtime.emit_employment_directory_ready(regions_to_wake.into_iter().collect())
}
