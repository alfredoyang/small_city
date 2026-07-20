//! Single-threaded worker that schedules multiple region runtimes fairly.
//!
//! The worker routes owned runtime messages between region inboxes. It never
//! reads or mutates ECS state directly; all simulation work stays inside each
//! `RegionRuntime`.

#[cfg(test)]
use crate::core::components::TravelerHandoff;
use crate::core::regional_types::{
    RegionCommandResponse, RegionSnapshotResponse, RegionTickResponse, UiRequestId,
};
#[cfg(test)]
use crate::core::regions::RegionRoadNetworkId;
use crate::core::regions::RegionRoadReport;
use crate::core::regions::coordinator::RoutedRegionEvent;
pub use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::directory::{RegionDirectory, power_capacity_recheck_targets};
use crate::core::regions::employment_directory::EmploymentDirectory;
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::{
    OutboundMessage, RegionEvent, RegionRuntime, RegionRuntimeError, RuntimeReply,
};
use crate::core::regions::{
    BorderEdge, BorderLinkId, NetworkBorderLink, RegionId, RegionNeighborLink, RegionState,
    RegionalAvailabilityHint,
};
use crate::core::world::CrossRegionGoodsRoutes;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable identity for one single-threaded worker scheduler.
pub struct WorkerId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic routing error produced by a worker pass.
pub enum WorkerRoutingError {
    /// A worker cannot own two runtimes with the same routing key.
    DuplicateRegion { region_id: RegionId },
    /// A routed event targeted a region this worker does not own.
    MissingTargetRegion { target_region: RegionId },
    /// A region runtime returned its own deterministic processing error.
    RuntimeError {
        source_region: RegionId,
        error: RegionRuntimeError,
    },
}

#[derive(Debug)]
/// Failed region attachment that returns the still-owned runtime to the caller.
pub struct RegionAddError {
    error: WorkerRoutingError,
    runtime: Box<RegionRuntime>,
}

impl RegionAddError {
    pub fn routing_error(&self) -> WorkerRoutingError {
        self.error
    }

    pub fn into_runtime(self) -> RegionRuntime {
        *self.runtime
    }
}

#[derive(Debug, Default)]
/// Summary returned after one worker scheduling pass.
pub struct WorkerRunSummary {
    pub processed_regions: usize,
    pub routing_errors: Vec<WorkerRoutingError>,
    pub command_replies: Vec<RegionCommandResponse>,
    pub tick_replies: Vec<RegionTickResponse>,
    pub snapshot_replies: Vec<RegionSnapshotResponse>,
    pub runtime_replies: Vec<RuntimeReply>,
    pub coordinator_events: Vec<RoutedRegionEvent>,
}

/// One coordinator-driven scheduler round.
///
/// It preserves per-region input installation, road refresh, and the full
/// hint-publish sweep.
#[derive(Debug, Default)]
pub(crate) struct AutonomousWorkerRound {
    #[allow(dead_code)] // P2 tests assert bounded slice progress.
    pub processed_regions: usize,
    pub routing_errors: Vec<WorkerRoutingError>,
    pub command_replies: Vec<RegionCommandResponse>,
    pub tick_replies: Vec<RegionTickResponse>,
    pub snapshot_replies: Vec<RegionSnapshotResponse>,
    pub runtime_replies: Vec<RuntimeReply>,
    pub coordinator_events: Vec<RoutedRegionEvent>,
}

#[derive(Debug)]
/// Coordinator-owned routing table from region IDs to worker IDs.
///
/// M3 keeps this table separate from `RegionDirectory`: the directory answers
/// "which regional road networks are connected?", while this table answers
/// "which worker owns the target region inbox?".
pub struct RegionOwnerDirectory {
    owners: Mutex<HashMap<RegionId, WorkerId>>,
}

impl RegionOwnerDirectory {
    pub fn new() -> Self {
        Self {
            owners: Mutex::new(HashMap::new()),
        }
    }

    pub fn owner_of(&self, region_id: RegionId) -> Option<WorkerId> {
        self.owners
            .lock()
            .expect("region owner directory lock poisoned")
            .get(&region_id)
            .copied()
    }

    /// Returns every registered region in stable routing order.
    ///
    /// The coordinator uses this only for explicit broadcast recipients. The
    /// owner table itself remains a hash map because point lookups dominate.
    #[allow(dead_code)] // P1 broadcast support is activated when P2 emits routes.
    pub(crate) fn region_ids(&self) -> Vec<RegionId> {
        let mut region_ids: Vec<_> = self
            .owners
            .lock()
            .expect("region owner directory lock poisoned")
            .keys()
            .copied()
            .collect();
        region_ids.sort_unstable();
        region_ids
    }

    pub(crate) fn register_region(
        &self,
        region_id: RegionId,
        worker_id: WorkerId,
    ) -> Result<(), WorkerRoutingError> {
        let mut owners = self
            .owners
            .lock()
            .expect("region owner directory lock poisoned");
        if owners
            .get(&region_id)
            .is_some_and(|existing| *existing != worker_id)
        {
            return Err(WorkerRoutingError::DuplicateRegion { region_id });
        }
        owners.insert(region_id, worker_id);
        Ok(())
    }

    fn unregister_region(&self, region_id: RegionId, worker_id: WorkerId) {
        let mut owners = self
            .owners
            .lock()
            .expect("region owner directory lock poisoned");
        if owners.get(&region_id) == Some(&worker_id) {
            owners.remove(&region_id);
        }
    }
}

impl Default for RegionOwnerDirectory {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionRoutingMode {
    /// Normal single-worker behavior: owned target inboxes receive events now.
    Immediate,
    /// Production coordinator behavior: every inter-region delivery is emitted
    /// as a target-bearing coordinator route without worker-side merging.
    Coordinator,
}

#[derive(Debug)]
/// Owns and schedules multiple regional runtimes on one thread.
pub struct RegionWorker {
    id: WorkerId,
    regions: Vec<RegionRuntime>,
    directory: Arc<RegionDirectory>,
    owners: Arc<RegionOwnerDirectory>,
    /// Directory employment ledger plan, P3: shared across every worker, just
    /// like `directory`. Installed into each runtime at the start of its slice
    /// so an `EmploymentDirectoryReady` event can pull the region's work.
    employment_directory: Arc<EmploymentDirectory>,
    // Retire-tickstate, P-b: mints ids for the eager nudge, which isn't
    // triggered by any UI request so has no `UiRequestId` to borrow. See
    // `next_worker_request_id` for the disjoint-bit-range scheme.
    recheck_counter: u32,
}

impl RegionWorker {
    #[cfg(test)]
    pub fn new(id: WorkerId) -> Self {
        Self::with_directory(id, Arc::new(RegionDirectory::default()))
    }

    #[cfg(test)]
    pub fn with_directory(id: WorkerId, directory: Arc<RegionDirectory>) -> Self {
        Self::with_directory_and_owners(id, directory, Arc::new(RegionOwnerDirectory::default()))
    }

    pub fn with_directory_and_owners(
        id: WorkerId,
        directory: Arc<RegionDirectory>,
        owners: Arc<RegionOwnerDirectory>,
    ) -> Self {
        Self::with_directories_and_owners(
            id,
            directory,
            Arc::new(EmploymentDirectory::default()),
            owners,
        )
    }

    /// P3: the multi-worker constructor. Every worker in a city must share one
    /// `EmploymentDirectory` (exactly as they already share one
    /// `RegionDirectory`), or two workers would broker claims against separate
    /// broker states and both could hand out the same seat.
    pub fn with_directories_and_owners(
        id: WorkerId,
        directory: Arc<RegionDirectory>,
        employment_directory: Arc<EmploymentDirectory>,
        owners: Arc<RegionOwnerDirectory>,
    ) -> Self {
        Self {
            id,
            regions: Vec::new(),
            directory,
            owners,
            employment_directory,
            recheck_counter: 0,
        }
    }

    /// Retire-tickstate, P-b: a fresh id for the eager nudge. This fan-out
    /// isn't triggered by any UI request, so there's no `UiRequestId` to
    /// borrow — but the resulting requests still need a fresh generation
    /// (both the producer's `release_stale_for_caller` and the caller's own
    /// `current_power_request_id` staleness check depend on batch ids
    /// actually changing between batches).
    ///
    /// Bit 63 marks "worker-minted" (`RegionalGame`'s UI counter starts at 1
    /// and increments per player action — it never reaches bit 63). Bits
    /// 32..62 encode which worker minted it, so two workers' independent
    /// counters can't collide either: a multi-worker game has several
    /// `RegionWorker`s, and two workers each running their own bare counter
    /// *can* mint the same id and nudge the same target with it, defeating
    /// both staleness checks — encoding `WorkerId` makes that structurally
    /// impossible instead of merely unlikely.
    fn next_worker_request_id(&mut self) -> UiRequestId {
        // WorkerId starts at 1 and stays tiny (INITIAL_WORKER_ID = 1, +index);
        // 31 bits is astronomically more than any real deployment. A real
        // assert (not debug_assert!) keeps this checked in release builds
        // too -- an oversized WorkerId silently colliding with another
        // worker's id range would be exactly the bug this scheme exists to
        // prevent structurally.
        assert!(self.id.0 < (1 << 31), "WorkerId must fit in 31 bits");
        self.recheck_counter = self
            .recheck_counter
            .checked_add(1)
            .expect("recheck_counter overflowed u32");
        UiRequestId((1u64 << 63) | (u64::from(self.id.0) << 32) | u64::from(self.recheck_counter))
    }

    pub fn id(&self) -> WorkerId {
        self.id
    }

    pub fn add_region(&mut self, mut runtime: RegionRuntime) -> Result<(), RegionAddError> {
        let region_id = runtime.region_id();
        if self.region(region_id).is_some() {
            return Err(RegionAddError {
                error: WorkerRoutingError::DuplicateRegion { region_id },
                runtime: Box::new(runtime),
            });
        }
        if let Err(error) = self.owners.register_region(region_id, self.id) {
            return Err(RegionAddError {
                error,
                runtime: Box::new(runtime),
            });
        }

        // DT1: a region built up via commands before being added would carry a
        // dirty derived state; recompute it so the first published summaries are
        // accurate.
        runtime.ensure_derived_state();
        let links = runtime.state().network_border_links();
        let hints = runtime.state().availability_hints();
        self.regions.push(runtime);
        self.publish_region_summary(region_id, links, hints);
        // Adding a region changes the live-owner set. Earlier regions may have
        // skipped border links to this neighbour while it was still unowned, so
        // republish all reports once the new owner is registered.
        // ponytail: O(N^2) over startup adds; narrow to neighbours if region
        // counts grow enough for setup time to matter.
        self.publish_current_road_reports();
        Ok(())
    }

    fn publish_current_road_reports(&self) {
        let topology = self.directory.topology();
        for runtime in &self.regions {
            let links = runtime.state().network_border_links();
            let region_id = runtime.region_id();
            let border_neighbours =
                border_neighbor_map_for_region(&topology, region_id, &links, |neighbor| {
                    self.owners.owner_of(neighbor).is_some()
                });
            let road_report = runtime.state().road_report(&border_neighbours);
            self.directory.publish_region_road_report(road_report);
            runtime.state().clear_road_topology_dirty();
        }
    }

    /// Removes one owned runtime so a caller can move it at a safe point.
    pub fn remove_region(&mut self, region_id: RegionId) -> Option<RegionRuntime> {
        let position = self
            .regions
            .iter()
            .position(|runtime| runtime.region_id() == region_id)?;

        let runtime = self.regions.remove(position);
        self.publish_region_summary(region_id, Vec::new(), Vec::new());
        // P-a: clear the road report for the removed region. publish_region with
        // empty links/hints is idempotent for the availability path; the road
        // report needs an explicit empty publish (the snapshot's
        // `road_reports` Vec is sorted/deduped by region on rebuild).
        self.directory.publish_region_road_report(RegionRoadReport {
            region: region_id,
            border_links: Vec::new(),
            crossing_costs: Vec::new(),
        });
        self.owners.unregister_region(region_id, self.id);
        Some(runtime)
    }

    /// Restarts one owned region through the same save-record boundary used by saves.
    ///
    /// This is a safe-point operation: callers should drain work first because
    /// queued runtime events and transient export allocations are intentionally
    /// not durable save truth.
    pub fn restart_region_from_save_record(
        &mut self,
        region_id: RegionId,
    ) -> Result<(), WorkerRoutingError> {
        let Some(runtime) = self.remove_region(region_id) else {
            return Err(WorkerRoutingError::MissingTargetRegion {
                target_region: region_id,
            });
        };
        let state = RegionState::from_save_record(runtime.into_state().into_save_record());
        self.add_region(RegionRuntime::new(state))
            .map_err(|error| error.routing_error())
    }

    pub fn region(&self, region_id: RegionId) -> Option<&RegionRuntime> {
        self.regions
            .iter()
            .find(|runtime| runtime.region_id() == region_id)
    }

    /// Mutable access to one owned runtime, so a derived-state read (DT1 inspect)
    /// can recompute the derived pass before reading it.
    pub fn region_mut(&mut self, region_id: RegionId) -> Option<&mut RegionRuntime> {
        self.regions
            .iter_mut()
            .find(|runtime| runtime.region_id() == region_id)
    }

    pub fn handle_for(&self, region_id: RegionId) -> Option<RegionHandle> {
        self.region(region_id).map(RegionRuntime::handle)
    }

    pub fn set_region_topology(&mut self, topology: Vec<RegionNeighborLink>) {
        // Compatibility shim for direct worker tests. Production routing receives
        // topology through the shared `RegionDirectory` owned by the runner.
        self.directory.set_topology(topology);
    }

    /// Builds discovery data only; availability hints are not allocations.
    ///
    /// The worker uses this component graph and stale-tolerant hints only to
    /// route export requests. The producer runtime remains authoritative for
    /// granting or denying export allocation.
    pub fn cross_region_discovery(&self, topology: &[RegionNeighborLink]) -> CrossRegionDiscovery {
        // Compatibility shim for the integration test suite. Production routing
        // reads the shared directory snapshot instead of allocating this
        // throwaway directory.
        let directory = RegionDirectory::new(topology.to_vec());
        for runtime in &self.regions {
            let state = runtime.state();
            directory.publish_region(
                runtime.region_id(),
                state.network_border_links(),
                state.availability_hints(),
            );
        }
        (*directory.discovery_snapshot()).clone()
    }

    pub fn push_event(
        &mut self,
        target_region: RegionId,
        event: RegionEvent,
    ) -> Result<(), WorkerRoutingError> {
        let Some(handle) = self.handle_for(target_region) else {
            return Err(WorkerRoutingError::MissingTargetRegion { target_region });
        };

        handle.send(event);
        Ok(())
    }

    /// Gives each owned region up to `max_events_per_region` events of work.
    ///
    /// Outbound messages are routed after all regions receive their scheduling
    /// slice. This keeps one region from creating same-pass work for another
    /// region that has not yet had its turn.
    pub fn process_region_events(&mut self, max_events_per_region: usize) -> WorkerRunSummary {
        self.process_region_events_with_mode(max_events_per_region, RegionRoutingMode::Immediate)
    }

    /// Runs one coordinator-driven scheduler round.
    pub(crate) fn process_autonomous_round(
        &mut self,
        max_events_per_region: usize,
    ) -> AutonomousWorkerRound {
        let summary = self
            .process_region_events_with_mode(max_events_per_region, RegionRoutingMode::Coordinator);
        AutonomousWorkerRound {
            processed_regions: summary.processed_regions,
            routing_errors: summary.routing_errors,
            command_replies: summary.command_replies,
            tick_replies: summary.tick_replies,
            snapshot_replies: summary.snapshot_replies,
            runtime_replies: summary.runtime_replies,
            coordinator_events: summary.coordinator_events,
        }
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.regions
            .iter()
            .any(|runtime| runtime.pending_event_count() > 0)
    }

    pub(crate) fn has_dirty_hints(&self) -> bool {
        self.regions
            .iter()
            .any(|runtime| runtime.state().is_hints_dirty())
    }

    fn process_region_events_with_mode(
        &mut self,
        max_events_per_region: usize,
        routing_mode: RegionRoutingMode,
    ) -> WorkerRunSummary {
        if max_events_per_region == 0 {
            return WorkerRunSummary::default();
        }

        let mut processed_regions = 0;
        let mut outbound = Vec::new();
        let mut changed_summaries = Vec::new();

        for runtime in &mut self.regions {
            if runtime.pending_event_count() == 0 {
                continue;
            }

            processed_regions += 1;
            let discovery = self.directory.discovery_snapshot();
            let importable_remote_jobs = importable_remote_jobs_for_region(
                &discovery,
                runtime.region_id(),
                &runtime.state().network_border_links(),
            );
            runtime.set_importable_remote_jobs(importable_remote_jobs);
            // Event-driven plan, P-2: install this pass's directory generation
            // before processing events, so the power reconcile gate compares
            // against the same snapshot this slice's routing already used.
            runtime.set_discovery_generation(discovery.generation);
            // P7-c: install the full snapshot too, so the daily employment phase
            // (P7-d) can re-check contract reachability against the component graph.
            runtime.set_discovery_snapshot(Arc::clone(&discovery));
            // P3: same per-slice install, so an `EmploymentDirectoryReady` event
            // processed below can pull this region's employment work.
            runtime.set_employment_directory(Arc::clone(&self.employment_directory));
            let source_region = runtime.region_id();
            // P-c: install the current Layer-1 route exits before processing events,
            // so `StepTravel` uses the latest published route snapshot. A post-event
            // refresh below picks up any road changes this pass.
            let exits = self.directory.exits_from(source_region).unwrap_or_default();
            runtime.set_region_routes(&exits);
            outbound.extend(
                runtime
                    .process_some_events(max_events_per_region)
                    .into_iter()
                    .map(|message| (source_region, message)),
            );
            // DT1: a processed command (build/bulldoze) only marked the region
            // dirty; recompute the derived pass before reading the summaries it
            // feeds, so published hints reflect the latest config.
            runtime.ensure_derived_state();
            if runtime.state().is_road_topology_dirty() {
                // L1 repricing gate: only recompute the road report when local
                // road topology changed. `publish_region_road_report` remains
                // idempotent as the safety net for false positives.
                let owners = Arc::clone(&self.owners);
                let post_links = runtime.state().network_border_links();
                let post_border_neighbor_map = border_neighbor_map_for_region(
                    &self.directory.topology(),
                    runtime.region_id(),
                    &post_links,
                    |neighbor| owners.owner_of(neighbor).is_some(),
                );
                let road_report = runtime.state().road_report(&post_border_neighbor_map);
                self.directory.publish_region_road_report(road_report);
                // P-c: refresh the multi-hop `remote_exit_cells` from the routes
                // snapshot rebuilt by the publish above.
                let exits = self.directory.exits_from(source_region).unwrap_or_default();
                runtime.set_region_routes(&exits);
                runtime.state().clear_road_topology_dirty();
            }
        }

        // P-1 (event-driven plan): a second sweep over EVERY owned region, not
        // only ones with pending events this pass — closes the stale-hint gap
        // where an event-idle region's directory entry never catches up.
        // Gated on hints_dirty: a clean region costs one flag check.
        for runtime in &mut self.regions {
            if !runtime.state().is_hints_dirty() {
                continue;
            }
            // Hints read derived state; a region reached only through this
            // sweep (zero events this pass) may still have a paused command
            // from an earlier pass, so ensure it's current before reading.
            runtime.ensure_derived_state();
            let region_id = runtime.region_id();
            changed_summaries.push((
                region_id,
                runtime.state().network_border_links(),
                runtime.state().availability_hints(),
            ));
            runtime.state().clear_hints_dirty();
        }

        let mut coordinator_events = Vec::new();

        // Retire-tickstate, P-b: the eager nudge. Gated on `publish_region`'s
        // own idempotence check -- only fans out on a REAL change, not on
        // every pass a hint happens to get re-published unchanged. Coarse on
        // purpose: the whole connected component is nudged, not just actual
        // importers (see the plan's Risks section for why that's a safe,
        // deliberate choice, same as every other dirty flag in this
        // codebase). This only ever makes the common case faster; the
        // discovery-generation gate alone still guarantees the worst case.
        for (region_id, links, hints) in changed_summaries {
            let republished = self
                .directory
                .publish_region(region_id, links, hints.clone());
            if !republished {
                continue; // nothing actually changed -- nothing to nudge
            }
            let recheck_id = self.next_worker_request_id();
            let discovery = self.directory.discovery_snapshot();
            for target_region in power_capacity_recheck_targets(&discovery, region_id, &hints) {
                if let Ok(WorkerRoutedMessage::Coordinator(event)) = self.route_region_event(
                    target_region,
                    RegionEvent::PowerCapacityRecheck {
                        request_id: recheck_id,
                        source_region: region_id,
                    },
                    routing_mode,
                ) {
                    coordinator_events.push(event);
                }
            }
        }

        let mut routing_errors = Vec::new();
        let mut command_replies = Vec::new();
        let mut tick_replies = Vec::new();
        let mut snapshot_replies = Vec::new();
        let mut runtime_replies = Vec::new();

        for (source_region, message) in outbound {
            match self.route_outbound(source_region, message) {
                Ok(WorkerRoutedMessage::CommandReply(reply)) => command_replies.push(reply),
                Ok(WorkerRoutedMessage::TickReply(reply)) => tick_replies.push(reply),
                Ok(WorkerRoutedMessage::SnapshotReply(reply)) => snapshot_replies.push(reply),
                Ok(WorkerRoutedMessage::RuntimeReply(reply)) => runtime_replies.push(reply),
                Ok(WorkerRoutedMessage::Coordinator(event)) => {
                    if matches!(routing_mode, RegionRoutingMode::Immediate) {
                        routing_errors.extend(self.deliver_coordinator_event_locally(event));
                    } else {
                        coordinator_events.push(event);
                    }
                }
                Ok(WorkerRoutedMessage::None) => {}
                Err(error) => routing_errors.push(error),
            }
        }

        WorkerRunSummary {
            processed_regions,
            routing_errors,
            command_replies,
            tick_replies,
            snapshot_replies,
            runtime_replies,
            coordinator_events,
        }
    }

    fn route_outbound(
        &mut self,
        source_region: RegionId,
        message: OutboundMessage,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        match message {
            OutboundMessage::RegionCommandCompleted(reply) => {
                Ok(WorkerRoutedMessage::CommandReply(reply))
            }
            OutboundMessage::RegionTickCompleted(reply) => {
                Ok(WorkerRoutedMessage::TickReply(reply))
            }
            OutboundMessage::RegionSnapshotReady(reply) => {
                Ok(WorkerRoutedMessage::SnapshotReply(reply))
            }
            OutboundMessage::RegionInspectReady {
                request_id,
                region_id,
                inspect,
            } => Ok(WorkerRoutedMessage::RuntimeReply(RuntimeReply::Inspect {
                request_id,
                region_id,
                inspect: Box::new(inspect),
            })),
            OutboundMessage::RoadTravelerPanelSeedReady {
                request_id,
                region_id,
                seed,
            } => Ok(WorkerRoutedMessage::RuntimeReply(
                RuntimeReply::RoadTravelerPanelSeed {
                    request_id,
                    region_id,
                    seed,
                },
            )),
            OutboundMessage::BuildingAnchorReady {
                request_id,
                region_id,
                anchor,
            } => Ok(WorkerRoutedMessage::RuntimeReply(
                RuntimeReply::BuildingAnchor {
                    request_id,
                    region_id,
                    anchor,
                },
            )),
            OutboundMessage::RemoteWorkersReady {
                request_id,
                region_id,
                workers,
            } => Ok(WorkerRoutedMessage::RuntimeReply(
                RuntimeReply::RemoteWorkers {
                    request_id,
                    region_id,
                    workers,
                },
            )),
            OutboundMessage::PowerImportsSettled {
                request_id,
                region_id,
            } => Ok(WorkerRoutedMessage::RuntimeReply(
                RuntimeReply::PowerImportsSettled {
                    request_id,
                    region_id,
                },
            )),
            OutboundMessage::CoordinatorRoute(event) => Ok(WorkerRoutedMessage::Coordinator(event)),
            OutboundMessage::RuntimeError(error) => Err(WorkerRoutingError::RuntimeError {
                source_region,
                error,
            }),
        }
    }

    fn deliver_coordinator_event_locally(
        &mut self,
        event: RoutedRegionEvent,
    ) -> Vec<WorkerRoutingError> {
        use crate::core::regions::coordinator::RegionRecipients;

        let recipients = match event.recipients {
            RegionRecipients::One(region) => vec![region],
            RegionRecipients::Many(regions) => regions,
            RegionRecipients::All => self.owners.region_ids(),
        };
        recipients
            .into_iter()
            .filter_map(|region| self.push_event(region, event.event.clone()).err())
            .collect()
    }

    fn route_region_event(
        &mut self,
        target_region: RegionId,
        event: RegionEvent,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        if routing_mode == RegionRoutingMode::Immediate && self.region(target_region).is_some() {
            self.push_event(target_region, event)?;
            return Ok(WorkerRoutedMessage::None);
        }

        if routing_mode == RegionRoutingMode::Coordinator {
            return Ok(WorkerRoutedMessage::Coordinator(RoutedRegionEvent {
                recipients: crate::core::regions::coordinator::RegionRecipients::One(target_region),
                event,
            }));
        }

        Err(WorkerRoutingError::MissingTargetRegion { target_region })
    }

    fn publish_region_summary(
        &self,
        region_id: RegionId,
        links: Vec<NetworkBorderLink>,
        hints: Vec<RegionalAvailabilityHint>,
    ) {
        self.directory.publish_region(region_id, links, hints);
    }
}

/// P-a: builds the `BorderLinkId → neighbor RegionId` facts for one region's
/// road report — "this border link faces this neighbor." Travel routing reads the
/// Layer-1 `RegionRoutes`; this helper only prices/publishes the local road graph.
fn border_neighbor_map_for_region(
    topology: &[RegionNeighborLink],
    region_id: RegionId,
    border_links: &[NetworkBorderLink],
    is_owned: impl Fn(RegionId) -> bool,
) -> HashMap<BorderLinkId, RegionId> {
    let mut neighbor_by_edge: HashMap<BorderEdge, RegionId> = HashMap::new();
    for link in topology {
        if link.region == region_id {
            neighbor_by_edge.insert(link.edge, link.neighbor);
        }
    }
    let mut map = HashMap::new();
    for border in border_links {
        if border.network.region != region_id {
            continue;
        }
        if let Some(&neighbor) = neighbor_by_edge.get(&border.link.edge) {
            // Skip a neighbor with no live owner (stale topology): a mover must not
            // pick an exit toward a region the worker can't route to, or it would
            // cross, mark Away, and strand when the handoff fails to deliver.
            if is_owned(neighbor) {
                map.insert(border.link, neighbor);
            }
        }
    }
    map
}

pub(crate) fn importable_remote_jobs_for_region(
    discovery: &CrossRegionDiscovery,
    region_id: RegionId,
    border_links: &[NetworkBorderLink],
) -> i32 {
    let mut remote_networks = BTreeSet::new();
    for link in border_links {
        if link.network.region != region_id {
            continue;
        }
        for network in discovery.component_of(link.network).unwrap_or(&[]) {
            if network.region != region_id {
                remote_networks.insert(*network);
            }
        }
    }

    let mut remote_slots = BTreeSet::new();
    for network in remote_networks {
        let Some(hint) = discovery
            .availability_hints
            .iter()
            .find(|hint| hint.network == network)
        else {
            continue;
        };
        for slot_id in &hint.spare_job_slot_ids {
            remote_slots.insert((network.region, *slot_id));
        }
    }

    remote_slots.len() as i32
}

pub(crate) fn cross_region_goods_routes_for_region(
    discovery: &CrossRegionDiscovery,
    region_id: RegionId,
    border_links: &[NetworkBorderLink],
) -> CrossRegionGoodsRoutes {
    let mut supplier_networks = BTreeSet::new();
    for link in border_links {
        if link.network.region != region_id {
            continue;
        }
        let has_remote_supplier = discovery
            .component_of(link.network)
            .unwrap_or(&[])
            .iter()
            .filter(|network| network.region != region_id)
            .any(|network| {
                discovery
                    .availability_hints
                    .iter()
                    .any(|hint| hint.network == *network && hint.spare_goods_units > 0)
            });
        if has_remote_supplier {
            supplier_networks.insert(link.network.road_network);
        }
    }

    CrossRegionGoodsRoutes {
        supplier_networks: supplier_networks.into_iter().collect(),
    }
}

enum WorkerRoutedMessage {
    None,
    Coordinator(RoutedRegionEvent),
    CommandReply(RegionCommandResponse),
    TickReply(RegionTickResponse),
    SnapshotReply(RegionSnapshotResponse),
    RuntimeReply(RuntimeReply),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::regional_types::UiRequestId;
    use crate::core::regions::RegionState;

    #[test]
    fn worker_minted_ids_are_disjoint_from_ui_ids_and_other_workers() {
        // Retire-tickstate, P-b: the nudge mints its own ids since it isn't
        // triggered by any UI request. Bit 63 must never overlap a
        // UI-minted id (RegionalGame's counter starts at 1, incrementing per
        // player action), and bits 32..62 must keep two workers' counters
        // from ever colliding even if both mint their Nth id at the same
        // moment.
        let mut worker_a = RegionWorker::new(WorkerId(1));
        let mut worker_b = RegionWorker::new(WorkerId(2));

        let a_first = worker_a.next_worker_request_id();
        let a_second = worker_a.next_worker_request_id();
        let b_first = worker_b.next_worker_request_id();

        for id in [a_first, a_second, b_first] {
            assert_eq!(
                id.0 & (1u64 << 63),
                1u64 << 63,
                "worker-minted ids must always have bit 63 set"
            );
        }
        assert_ne!(
            a_first, a_second,
            "the same worker's own counter must not repeat"
        );
        assert_ne!(
            a_first, b_first,
            "two workers minting their first id must not collide"
        );
        assert_ne!(
            a_second, b_first,
            "different workers' counters must not collide even at the same count"
        );
    }

    fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }

    /// P5b: the topology edge A-East→B maps A's East border link to neighbor B.
    #[test]
    fn border_neighbor_map_maps_edge_to_neighbor() {
        let topology = vec![RegionNeighborLink::new(
            RegionId(1),
            BorderEdge::East,
            RegionId(2),
        )];
        let link = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let border_links = vec![NetworkBorderLink {
            network: network(1, 0),
            link,
        }];
        let map = border_neighbor_map_for_region(&topology, RegionId(1), &border_links, |_| true);
        assert_eq!(map.get(&link), Some(&RegionId(2)));

        // A neighbor with no live owner is excluded, so a mover never picks an exit
        // toward an unroutable region (and so can't strand Away).
        let empty =
            border_neighbor_map_for_region(&topology, RegionId(1), &border_links, |_| false);
        assert!(empty.is_empty(), "unowned neighbor excluded from the hint");
    }

    /// P5b: an outbound handoff is delivered as a `ReceiveTraveler` event on the
    /// destination region's inbox.
    #[test]
    fn traveler_handoff_routes_to_destination_inbox() {
        use crate::core::components::{HandoffKind, TravelState, TravelToken, TravelerId};
        use crate::core::entity::Entity;

        let mut worker = RegionWorker::new(WorkerId(7));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 1)))
            .unwrap();
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(2), 2, 1)))
            .unwrap();

        let handoff = TravelerHandoff {
            token: TravelToken {
                state: TravelState::default(),
                home: crate::core::components::PlaceRef {
                    region: RegionId(1),
                    building: Entity::new(RegionId(1), 0),
                },
                work: None,
                trip_gen: 1,
            },
            traveler: TravelerId {
                citizen: Entity::new(RegionId(1), 5),
                generation: 1,
            },
            to_region: RegionId(2),
            entry_link: Some(BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            }),
            kind: HandoffKind::Move,
        };
        let route = RoutedRegionEvent {
            recipients: crate::core::regions::coordinator::RegionRecipients::One(RegionId(2)),
            event: RegionEvent::ReceiveTraveler {
                eligible_step: crate::core::regions::runtime::TravelStepId(2),
                handoff,
            },
        };
        assert!(matches!(
            worker.route_outbound(RegionId(1), OutboundMessage::CoordinatorRoute(route),),
            Ok(WorkerRoutedMessage::Coordinator(_))
        ));
    }

    #[test]
    fn step_travel_does_not_republish_road_report() {
        let directory = Arc::new(RegionDirectory::new(Vec::new()));
        let mut worker = RegionWorker::with_directory(WorkerId(8), Arc::clone(&directory));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 2)))
            .unwrap();
        let before = directory.rebuild_count();

        worker
            .push_event(
                RegionId(1),
                RegionEvent::StepTravel {
                    step: crate::core::regions::runtime::TravelStepId(1),
                },
            )
            .expect("event routed");
        let summary = worker.process_region_events(usize::MAX);

        assert!(summary.routing_errors.is_empty());
        assert_eq!(
            directory.rebuild_count(),
            before,
            "movement-only events should not republish unchanged road reports"
        );
    }

    #[test]
    fn build_road_republishes_road_report() {
        let directory = Arc::new(RegionDirectory::new(Vec::new()));
        let mut worker = RegionWorker::with_directory(WorkerId(9), Arc::clone(&directory));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 2)))
            .unwrap();
        let before = directory.rebuild_count();

        worker
            .push_event(
                RegionId(1),
                RegionEvent::RunCommand {
                    request_id: UiRequestId(1),
                    command: crate::core::regional_types::RegionCommand::Build {
                        x: 1,
                        y: 0,
                        kind: crate::interface::input::BuildingKind::Road,
                    },
                },
            )
            .expect("event routed");
        let summary = worker.process_region_events(usize::MAX);

        assert!(summary.routing_errors.is_empty());
        assert!(
            directory.rebuild_count() > before,
            "road build changes the road report and should rebuild routes"
        );
    }

    #[test]
    fn clean_region_refreshes_routes_after_neighbour_road_change() {
        let topology = vec![
            RegionNeighborLink::new(RegionId(1), BorderEdge::East, RegionId(2)),
            RegionNeighborLink::new(RegionId(2), BorderEdge::West, RegionId(1)),
        ];
        let owners = Arc::new(RegionOwnerDirectory::new());
        let directory = Arc::new(RegionDirectory::with_owners(topology, Arc::clone(&owners)));
        let mut worker =
            RegionWorker::with_directory_and_owners(WorkerId(13), Arc::clone(&directory), owners);
        let mut source = RegionState::new(RegionId(1), 2, 1);
        assert!(
            source
                .build(1, 0, crate::interface::input::BuildingKind::Road)
                .success
        );
        worker
            .add_region(RegionRuntime::new(source))
            .expect("source added");
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(2), 2, 1)))
            .expect("target added");
        assert!(
            worker
                .region(RegionId(1))
                .unwrap()
                .state()
                .world
                .remote_exit_cells
                .is_empty(),
            "no matching road in region 2 yet"
        );

        worker
            .push_event(
                RegionId(2),
                RegionEvent::RunCommand {
                    request_id: UiRequestId(1),
                    command: crate::core::regional_types::RegionCommand::Build {
                        x: 0,
                        y: 0,
                        kind: crate::interface::input::BuildingKind::Road,
                    },
                },
            )
            .expect("build routed");
        assert!(
            worker
                .process_region_events(usize::MAX)
                .routing_errors
                .is_empty()
        );
        assert!(
            directory
                .exits_from(RegionId(1))
                .unwrap_or_default()
                .contains_key(&RegionId(2)),
            "directory routes should learn the new neighbour crossing"
        );

        worker
            .push_event(
                RegionId(1),
                RegionEvent::StepTravel {
                    step: crate::core::regions::runtime::TravelStepId(1),
                },
            )
            .expect("step routed");
        assert!(
            worker
                .process_region_events(usize::MAX)
                .routing_errors
                .is_empty()
        );
        assert!(
            worker
                .region(RegionId(1))
                .unwrap()
                .state()
                .world
                .remote_exit_cells
                .contains_key(&RegionId(2)),
            "clean source region refreshes route exits before StepTravel"
        );
    }

    #[test]
    fn build_house_does_not_republish_road_report() {
        let directory = Arc::new(RegionDirectory::new(Vec::new()));
        let mut worker = RegionWorker::with_directory(WorkerId(10), Arc::clone(&directory));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 2)))
            .unwrap();
        let before = directory.rebuild_count();

        worker
            .push_event(
                RegionId(1),
                RegionEvent::RunCommand {
                    request_id: UiRequestId(1),
                    command: crate::core::regional_types::RegionCommand::Build {
                        x: 0,
                        y: 0,
                        kind: crate::interface::input::BuildingKind::Residential,
                    },
                },
            )
            .expect("event routed");
        let summary = worker.process_region_events(usize::MAX);

        assert!(summary.routing_errors.is_empty());
        assert_eq!(
            directory.rebuild_count(),
            before,
            "non-road build should not recompute road reports"
        );
    }

    #[test]
    fn preview_build_does_not_republish_road_report() {
        let directory = Arc::new(RegionDirectory::new(Vec::new()));
        let mut worker = RegionWorker::with_directory(WorkerId(12), Arc::clone(&directory));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 2)))
            .unwrap();
        let before = directory.rebuild_count();

        worker
            .push_event(
                RegionId(1),
                RegionEvent::RunCommand {
                    request_id: UiRequestId(1),
                    command: crate::core::regional_types::RegionCommand::PreviewBuild {
                        x: 0,
                        y: 0,
                        kind: crate::interface::input::BuildingKind::Road,
                    },
                },
            )
            .expect("event routed");
        let summary = worker.process_region_events(usize::MAX);

        assert!(summary.routing_errors.is_empty());
        assert_eq!(
            directory.rebuild_count(),
            before,
            "preview should not mutate road topology or republish road reports"
        );
    }

    #[test]
    fn bulldoze_road_republishes_road_report() {
        let directory = Arc::new(RegionDirectory::new(Vec::new()));
        let mut worker = RegionWorker::with_directory(WorkerId(11), Arc::clone(&directory));
        let mut region = RegionState::new(RegionId(1), 2, 2);
        assert!(
            region
                .build(1, 0, crate::interface::input::BuildingKind::Road)
                .success
        );
        worker
            .add_region(RegionRuntime::new(region))
            .expect("region added");
        let before = directory.rebuild_count();

        worker
            .push_event(
                RegionId(1),
                RegionEvent::RunCommand {
                    request_id: UiRequestId(1),
                    command: crate::core::regional_types::RegionCommand::Bulldoze { x: 1, y: 0 },
                },
            )
            .expect("event routed");
        let summary = worker.process_region_events(usize::MAX);

        assert!(summary.routing_errors.is_empty());
        assert!(
            directory.rebuild_count() > before,
            "road bulldoze changes the road report and should rebuild routes"
        );
    }
}
