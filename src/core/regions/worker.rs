//! Single-threaded worker that schedules multiple region runtimes fairly.
//!
//! The worker routes owned runtime messages between region inboxes. It never
//! reads or mutates ECS state directly; all simulation work stays inside each
//! `RegionRuntime`.

use crate::core::components::TravelerHandoff;
use crate::core::regional_types::{
    RegionCommandResponse, RegionSnapshotResponse, RegionTickResponse, UiRequestId,
};
use crate::core::regions::RegionRoadReport;
pub use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::directory::RegionDirectory;
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::{
    ExportAllocationRelease, ExportAllocationRequest, GoodsExportRequest, JobExportRequest,
    OutboundMessage, PowerExportRequest, RegionEvent, RegionRuntime, RegionRuntimeError,
};
use crate::core::regions::{
    BorderEdge, BorderLinkId, GoodsExportGrant, JobExportGrant, NetworkBorderLink,
    PowerExportGrant, RegionId, RegionNeighborLink, RegionRoadNetworkId, RegionState,
    RegionalAvailabilityHint,
};
use crate::core::world::CrossRegionGoodsRoutes;
use std::collections::{BTreeSet, HashMap, HashSet};
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
    pub forwarded_events: Vec<ForwardedRegionEvent>,
    pub command_replies: Vec<RegionCommandResponse>,
    pub tick_replies: Vec<RegionTickResponse>,
    pub snapshot_replies: Vec<RegionSnapshotResponse>,
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

#[derive(Debug)]
/// One event that must cross from one worker to another at the M3 barrier.
pub struct ForwardedRegionEvent {
    pub target_worker: WorkerId,
    pub target_region: RegionId,
    order_key: ForwardedEventOrderKey,
    event: RegionEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// Stable merge key for cross-worker delivery.
///
/// Requests reaching a producer must not depend on thread timing. The barrier
/// sorts by target first (producer inbox), then caller/source and request token:
///
/// ```text
/// Worker A outbound ┐
/// Worker B outbound ├─ collect ─ sort key ─ deliver to target inboxes
/// Worker C outbound ┘
/// ```
struct ForwardedEventOrderKey {
    target_region: RegionId,
    source_region: RegionId,
    request_id: UiRequestId,
    token: u32,
    resource_rank: u8,
    event_rank: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionRoutingMode {
    /// Normal single-worker behavior: owned target inboxes receive events now.
    Immediate,
    /// M3 barrier behavior: all region-to-region events wait for stable ordering.
    Barrier,
}

#[derive(Debug, Default)]
/// Combined result from one deterministic multi-worker barrier step.
pub struct DeterministicBarrierSummary {
    pub worker_summaries: Vec<WorkerRunSummary>,
    pub routing_errors: Vec<WorkerRoutingError>,
}

/// Runs each worker's local pass, then deterministically delivers region events.
///
/// Local runtime processing stays single-threaded per worker. During a barrier
/// pass, every region-to-region export control event is collected, ordered by
/// `ForwardedEventOrderKey`, and only then pushed into target inboxes. That
/// includes same-worker targets; otherwise a local caller could beat a lower-key
/// remote caller to the same producer just because it bypassed the merge point.
pub fn process_workers_with_deterministic_barrier(
    workers: &mut [&mut RegionWorker],
    max_events_per_region: usize,
) -> DeterministicBarrierSummary {
    let mut summaries = Vec::new();
    let mut forwarded = Vec::new();
    let mut routing_errors = Vec::new();

    for worker in workers.iter_mut() {
        let mut summary = worker.process_region_events_for_barrier(max_events_per_region);
        forwarded.append(&mut summary.forwarded_events);
        routing_errors.extend(summary.routing_errors.iter().copied());
        summaries.push(summary);
    }

    sort_forwarded_events(&mut forwarded);
    for forwarded_event in forwarded {
        let Some(target_worker) = workers
            .iter_mut()
            .find(|worker| worker.id() == forwarded_event.target_worker)
        else {
            routing_errors.push(WorkerRoutingError::MissingTargetRegion {
                target_region: forwarded_event.target_region,
            });
            continue;
        };
        if let Err(error) =
            target_worker.push_event(forwarded_event.target_region, forwarded_event.event)
        {
            routing_errors.push(error);
        }
    }

    DeterministicBarrierSummary {
        worker_summaries: summaries,
        routing_errors,
    }
}

/// Sorts forwarded region events by the stable M3 merge key.
pub fn sort_forwarded_events(events: &mut [ForwardedRegionEvent]) {
    events.sort_by_key(|event| event.order_key);
}

#[derive(Debug)]
/// Owns and schedules multiple regional runtimes on one thread.
pub struct RegionWorker {
    id: WorkerId,
    regions: Vec<RegionRuntime>,
    directory: Arc<RegionDirectory>,
    owners: Arc<RegionOwnerDirectory>,
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
        Self {
            id,
            regions: Vec::new(),
            directory,
            owners,
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

    /// Anchor `Position` of the building at `(x, y)` in one owned region, used to
    /// normalize a clicked footprint cell to the workplace anchor before the remote
    /// roster scan. `None` if this worker does not own `region_id` or the cell is
    /// empty.
    pub(crate) fn building_anchor_at(
        &self,
        region_id: RegionId,
        x: usize,
        y: usize,
    ) -> Option<crate::core::components::Position> {
        self.region(region_id)
            .and_then(|runtime| runtime.building_anchor_at(x, y))
    }

    /// Remote staff of the workplace at `(producer_region, pos)`: every owned
    /// region's residents who commute there. The producer region is skipped (its
    /// own workers at that cell are Local, already on the local roster). Within a
    /// region the order is `Entity.0`; the runner sorts the cross-worker merge by
    /// home region for full determinism.
    pub(crate) fn remote_workers_at(
        &mut self,
        producer_region: RegionId,
        pos: crate::core::components::Position,
    ) -> Vec<crate::interface::view::CitizenDetailView> {
        let mut workers = Vec::new();
        for runtime in &mut self.regions {
            if runtime.region_id() == producer_region {
                continue;
            }
            workers.extend(runtime.remote_workers_for(producer_region, pos));
        }
        workers
    }

    pub(crate) fn refresh_importable_remote_jobs(&mut self, region_id: RegionId) {
        let Some(index) = self
            .regions
            .iter()
            .position(|runtime| runtime.region_id() == region_id)
        else {
            return;
        };
        let importable_remote_jobs = {
            let runtime = &self.regions[index];
            importable_remote_jobs_for_region(
                &self.directory.discovery_snapshot(),
                runtime.region_id(),
                &runtime.state().network_border_links(),
            )
        };
        self.regions[index].set_importable_remote_jobs(importable_remote_jobs);
    }

    pub(crate) fn refresh_cross_region_goods_routes(&mut self, region_id: RegionId) {
        let Some(index) = self
            .regions
            .iter()
            .position(|runtime| runtime.region_id() == region_id)
        else {
            return;
        };
        let routes = {
            let runtime = &self.regions[index];
            cross_region_goods_routes_for_region(
                &self.directory.discovery_snapshot(),
                runtime.region_id(),
                &runtime.state().network_border_links(),
            )
        };
        self.regions[index].set_cross_region_goods_routes(routes);
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

    pub fn process_region_events_for_barrier(
        &mut self,
        max_events_per_region: usize,
    ) -> WorkerRunSummary {
        self.process_region_events_with_mode(max_events_per_region, RegionRoutingMode::Barrier)
    }

    pub fn deliver_forwarded_events(
        &mut self,
        events: Vec<ForwardedRegionEvent>,
    ) -> Vec<WorkerRoutingError> {
        let mut errors = Vec::new();
        for forwarded_event in events {
            if let Err(error) =
                self.push_event(forwarded_event.target_region, forwarded_event.event)
            {
                errors.push(error);
            }
        }
        errors
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

        let mut forwarded_events = Vec::new();

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
            let mut notified = HashSet::new();
            for hint in &hints {
                let Some(component) = discovery.component_of(hint.network) else {
                    continue;
                };
                for network in component {
                    if network.region == region_id || !notified.insert(network.region) {
                        continue; // skip self and duplicates
                    }
                    let order_key = ForwardedEventOrderKey {
                        target_region: network.region,
                        source_region: region_id,
                        request_id: recheck_id,
                        token: hint.network.road_network,
                        resource_rank: 0,
                        event_rank: 3, // after release/request/reply
                    };
                    if let Ok(WorkerRoutedMessage::Forwarded(event)) = self.route_region_event(
                        network.region,
                        region_id,
                        RegionEvent::PowerCapacityRecheck {
                            request_id: recheck_id,
                            source_region: region_id,
                        },
                        order_key,
                        routing_mode,
                    ) {
                        forwarded_events.push(event);
                    }
                }
            }
        }

        let mut routing_errors = Vec::new();
        let mut command_replies = Vec::new();
        let mut tick_replies = Vec::new();
        let mut snapshot_replies = Vec::new();

        // Allocation releases are causal cleanup for the next export cycle.
        // Route them before new requests so runtime traversal order cannot make
        // a producer deny a fresh caller with capacity allocated by an older
        // caller generation in the same worker pass. All export resources share
        // this ordering rule.
        let (release_outbound, other_outbound): (Vec<_>, Vec<_>) =
            outbound.into_iter().partition(|(_, message)| {
                matches!(
                    message,
                    OutboundMessage::PowerExportAllocationsReleased(_)
                        | OutboundMessage::JobExportAllocationsReleased(_)
                        | OutboundMessage::GoodsExportAllocationsReleased(_)
                )
            });

        for (source_region, message) in release_outbound.into_iter().chain(other_outbound) {
            match self.route_outbound(source_region, message, routing_mode) {
                Ok(WorkerRoutedMessage::CommandReply(reply)) => command_replies.push(reply),
                Ok(WorkerRoutedMessage::TickReply(reply)) => tick_replies.push(reply),
                Ok(WorkerRoutedMessage::SnapshotReply(reply)) => snapshot_replies.push(reply),
                Ok(WorkerRoutedMessage::Forwarded(event)) => forwarded_events.push(event),
                Ok(WorkerRoutedMessage::ForwardedMany(mut events)) => {
                    forwarded_events.append(&mut events);
                }
                Ok(WorkerRoutedMessage::None) => {}
                Err(error) => routing_errors.push(error),
            }
        }

        WorkerRunSummary {
            processed_regions,
            routing_errors,
            forwarded_events,
            command_replies,
            tick_replies,
            snapshot_replies,
        }
    }

    fn route_outbound(
        &mut self,
        source_region: RegionId,
        message: OutboundMessage,
        routing_mode: RegionRoutingMode,
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
            OutboundMessage::PowerExportRequested(request) => {
                self.route_export_request::<PowerExport>(request, routing_mode)
            }
            OutboundMessage::PowerExportRequestCompleted { request, grant } => {
                self.route_export_request_result::<PowerExport>(request, grant, routing_mode)
            }
            OutboundMessage::PowerExportAllocationsReleased(release) => {
                self.route_export_allocation_release::<PowerExport>(release, routing_mode)
            }
            OutboundMessage::JobExportRequested(request) => {
                self.route_export_request::<JobExport>(request, routing_mode)
            }
            OutboundMessage::JobExportRequestCompleted { request, grant } => {
                self.route_export_request_result::<JobExport>(request, grant, routing_mode)
            }
            OutboundMessage::JobExportAllocationsReleased(release) => {
                self.route_export_allocation_release::<JobExport>(release, routing_mode)
            }
            OutboundMessage::GoodsExportRequested(request) => {
                self.route_export_request::<GoodsExport>(request, routing_mode)
            }
            OutboundMessage::GoodsExportRequestCompleted { request, grant } => {
                self.route_export_request_result::<GoodsExport>(request, grant, routing_mode)
            }
            OutboundMessage::GoodsExportAllocationsReleased(release) => {
                self.route_export_allocation_release::<GoodsExport>(release, routing_mode)
            }
            OutboundMessage::TravelerHandedOff(handoff) => {
                self.route_traveler_handoff(handoff, routing_mode)
            }
            OutboundMessage::RuntimeError(error) => Err(WorkerRoutingError::RuntimeError {
                source_region,
                error,
            }),
        }
    }

    /// P5b: routes a travel token to its destination region's inbox as a
    /// `ReceiveTraveler` event, by the same `RegionNeighborLink` topology as
    /// exports. Fire-and-forget: no grant, no tick pause.
    fn route_traveler_handoff(
        &mut self,
        handoff: TravelerHandoff,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        let target_region = handoff.to_region;
        if self.owners.owner_of(target_region).is_none() {
            return Err(WorkerRoutingError::MissingTargetRegion { target_region });
        }
        // Travel sorts after every export (rank 3); the token disambiguates by the
        // home citizen's local id for a deterministic cross-worker merge.
        let order_key = ForwardedEventOrderKey {
            target_region,
            source_region: handoff.traveler.citizen.region(),
            request_id: UiRequestId(0),
            token: handoff.traveler.citizen.local(),
            resource_rank: 3,
            event_rank: 0,
        };
        self.route_region_event(
            target_region,
            handoff.traveler.citizen.region(),
            RegionEvent::ReceiveTraveler(handoff),
            order_key,
            routing_mode,
        )
    }

    /// Routes a fresh consumer export request to the first reachable candidate.
    ///
    /// Shared by power and jobs (CR3R): candidates are component members on another
    /// region whose availability hint says spare, in stable order. The resource
    /// trait supplies only the hint selector, event constructors, and deny grant.
    fn route_export_request<R: ExportResource>(
        &mut self,
        request: R::Request,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        let candidates = {
            let discovery = self.directory.discovery_snapshot();
            discovery
                .component_of(R::caller_network(&request))
                .unwrap_or(&[])
                .iter()
                .copied()
                .filter(|network| network.region != R::caller_region(&request))
                .filter(|network| {
                    discovery
                        .availability_hints
                        .iter()
                        .any(|hint| hint.network == *network && R::has_spare(hint))
                })
                .collect::<Vec<_>>()
        };

        if candidates.is_empty() {
            return self.deny_export_request::<R>(&request, routing_mode);
        }

        let target_region = candidates[0].region;
        if self.owners.owner_of(target_region).is_none() {
            return self.deny_export_request::<R>(&request, routing_mode);
        }

        self.route_region_event(
            target_region,
            R::caller_region(&request),
            R::process_request_event(ExportAllocationRequest {
                request: request.clone(),
                candidates,
                candidate_index: 0,
            }),
            R::request_order_key(&request, target_region, 1),
            routing_mode,
        )
    }

    /// Applies a producer grant, or walks to the next candidate on a stale-hint
    /// denial; denies the caller once candidates are exhausted. Shared (CR3R).
    fn route_export_request_result<R: ExportResource>(
        &mut self,
        mut request: ExportAllocationRequest<R::Request>,
        grant: R::Grant,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        if R::granted(&grant) {
            let target_region = R::caller_region(&request.request);
            let order_key = R::request_order_key(&request.request, target_region, 2);
            return self.route_region_event(
                target_region,
                target_region,
                R::apply_grant_event(request.request, grant),
                order_key,
                routing_mode,
            );
        }

        request.candidate_index += 1;
        if request.candidate_index >= request.candidates.len() {
            let target_region = R::caller_region(&request.request);
            let order_key = R::request_order_key(&request.request, target_region, 2);
            return self.route_region_event(
                target_region,
                target_region,
                R::apply_grant_event(request.request, grant),
                order_key,
                routing_mode,
            );
        }

        let target_region = request.candidates[request.candidate_index].region;
        if self.owners.owner_of(target_region).is_none() {
            return self.deny_export_request::<R>(&request.request, routing_mode);
        }

        let order_key = R::request_order_key(&request.request, target_region, 1);
        self.route_region_event(
            target_region,
            R::caller_region(&request.request),
            R::process_request_event(request),
            order_key,
            routing_mode,
        )
    }

    /// Routes a caller's new-generation release to producers that granted before.
    fn route_export_allocation_release<R: ExportResource>(
        &mut self,
        release: ExportAllocationRelease,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        let mut forwarded = Vec::new();
        let target_regions = release.producer_regions.clone();

        for target_region in target_regions {
            match self.route_region_event(
                target_region,
                release.caller_region,
                R::release_event(release.clone()),
                R::release_order_key(&release, target_region),
                routing_mode,
            )? {
                WorkerRoutedMessage::Forwarded(event) => forwarded.push(event),
                WorkerRoutedMessage::None => {}
                other => return Ok(other),
            }
        }

        Ok(WorkerRoutedMessage::ForwardedMany(forwarded))
    }

    fn deny_export_request<R: ExportResource>(
        &mut self,
        request: &R::Request,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        let target_region = R::caller_region(request);
        let order_key = R::request_order_key(request, target_region, 2);
        let grant = R::deny_grant(request);
        self.route_region_event(
            target_region,
            target_region,
            R::apply_grant_event(request.clone(), grant),
            order_key,
            routing_mode,
        )
    }

    fn route_region_event(
        &mut self,
        target_region: RegionId,
        source_region: RegionId,
        event: RegionEvent,
        mut order_key: ForwardedEventOrderKey,
        routing_mode: RegionRoutingMode,
    ) -> Result<WorkerRoutedMessage, WorkerRoutingError> {
        if routing_mode == RegionRoutingMode::Immediate && self.region(target_region).is_some() {
            self.push_event(target_region, event)?;
            return Ok(WorkerRoutedMessage::None);
        }

        let Some(target_worker) = self.owners.owner_of(target_region) else {
            return Err(WorkerRoutingError::MissingTargetRegion { target_region });
        };
        order_key.target_region = target_region;
        order_key.source_region = source_region;
        Ok(WorkerRoutedMessage::Forwarded(ForwardedRegionEvent {
            target_worker,
            target_region,
            order_key,
            event,
        }))
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

fn importable_remote_jobs_for_region(
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

fn cross_region_goods_routes_for_region(
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

/// The variable bits of one cross-region export resource for the shared routing.
///
/// Everything in `route_export_request*` / `route_export_allocation_release` is
/// identical between power and jobs; this trait supplies only what differs: which
/// availability hint to read, how to build the concrete `RegionEvent`s, and the
/// deny grant. Available-capacity computation and grant application stay on the
/// producer runtime / `RegionState`, where the two resources genuinely diverge.
///
///   route_outbound
///     ├─ route_export_request::<PowerExport>(req)   ┐  same candidate-walk,
///     └─ route_export_request::<JobExport>(req)     ┘  missing-target deny,
///                  │                                   release ordering …
///                  ▼  calls back into R = PowerExport | JobExport for:
///        ┌──────────────────────────────────────────────────────────────┐
///        │  has_spare(hint)        → has_spare_power | slot ids non-empty  │
///        │  process_request_event  → ProcessPowerExportRequest | …Job…    │
///        │  apply_grant_event      → ApplyPowerExportGrant | …Job…        │
///        │  deny_grant(request)    → PowerExportGrant{..} | JobExportGrant │
///        └──────────────────────────────────────────────────────────────┘
///
/// `RegionEvent` / `OutboundMessage` variants stay concrete (PowerXxx / JobXxx);
/// the trait only chooses which one to build, so the wire format is unchanged.
///
/// ```text
/// Caller demand
///   |
///   v
/// Worker route_export_request<R>
///   |
///   +-- PowerExport -> has_spare_power -> ProcessPowerExportRequest
///   |
///   +-- JobExport   -> slot ids non-empty -> ProcessJobExportRequest
///                                       |
///                                       v
///                          Producer ExportAllocations<U>
///                            U = i32 power demand
///                            U = Entity workplace slot
///                                       |
///                                       v
///                          grant/deny -> caller applies grant
/// ```
trait ExportResource {
    type Request: Clone;
    type Grant;

    fn caller_region(request: &Self::Request) -> RegionId;
    fn caller_network(request: &Self::Request) -> RegionRoadNetworkId;
    fn has_spare(hint: &RegionalAvailabilityHint) -> bool;
    fn granted(grant: &Self::Grant) -> bool;
    fn deny_grant(request: &Self::Request) -> Self::Grant;
    fn request_id(request: &Self::Request) -> UiRequestId;
    fn token(request: &Self::Request) -> u32;
    fn resource_rank() -> u8;
    fn process_request_event(request: ExportAllocationRequest<Self::Request>) -> RegionEvent;
    /// Retire-tickstate, P-a: takes the original request too, not just the
    /// grant — power's event now carries both so the caller needs no
    /// continuation to remember what the reply answers. Job/goods ignore the
    /// request for now (P-c/P-d); their event shape is unchanged.
    fn apply_grant_event(request: Self::Request, grant: Self::Grant) -> RegionEvent;
    fn release_event(release: ExportAllocationRelease) -> RegionEvent;

    fn request_order_key(
        request: &Self::Request,
        target_region: RegionId,
        event_rank: u8,
    ) -> ForwardedEventOrderKey {
        ForwardedEventOrderKey {
            target_region,
            source_region: Self::caller_region(request),
            request_id: Self::request_id(request),
            token: Self::token(request),
            resource_rank: Self::resource_rank(),
            event_rank,
        }
    }

    fn release_order_key(
        release: &ExportAllocationRelease,
        target_region: RegionId,
    ) -> ForwardedEventOrderKey {
        ForwardedEventOrderKey {
            target_region,
            source_region: release.caller_region,
            request_id: release.request_id,
            token: 0,
            resource_rank: Self::resource_rank(),
            event_rank: 0,
        }
    }
}

/// Power: capacity hint, demand-carrying request, region-only grant.
struct PowerExport;

impl ExportResource for PowerExport {
    type Request = PowerExportRequest;
    type Grant = PowerExportGrant;

    fn caller_region(request: &Self::Request) -> RegionId {
        request.caller_region
    }
    fn caller_network(request: &Self::Request) -> RegionRoadNetworkId {
        request.caller_network
    }
    fn has_spare(hint: &RegionalAvailabilityHint) -> bool {
        hint.has_spare_power
    }
    fn granted(grant: &Self::Grant) -> bool {
        grant.granted
    }
    fn deny_grant(request: &Self::Request) -> Self::Grant {
        PowerExportGrant {
            token: request.token,
            granted: false,
            source_region: None,
        }
    }
    fn request_id(request: &Self::Request) -> UiRequestId {
        request.request_id
    }
    fn token(request: &Self::Request) -> u32 {
        request.token
    }
    fn resource_rank() -> u8 {
        0
    }
    fn process_request_event(request: ExportAllocationRequest<Self::Request>) -> RegionEvent {
        RegionEvent::ProcessPowerExportRequest(request)
    }
    fn apply_grant_event(request: Self::Request, grant: Self::Grant) -> RegionEvent {
        RegionEvent::ApplyPowerExportGrant { request, grant }
    }
    fn release_event(release: ExportAllocationRelease) -> RegionEvent {
        RegionEvent::ReleasePowerExportAllocations(release)
    }
}

/// Jobs: spare-slots hint, identity-free request, slot+salary grant.
struct JobExport;

impl ExportResource for JobExport {
    type Request = JobExportRequest;
    type Grant = JobExportGrant;

    fn caller_region(request: &Self::Request) -> RegionId {
        request.caller_region
    }
    fn caller_network(request: &Self::Request) -> RegionRoadNetworkId {
        request.caller_network
    }
    fn has_spare(hint: &RegionalAvailabilityHint) -> bool {
        !hint.spare_job_slot_ids.is_empty()
    }
    fn granted(grant: &Self::Grant) -> bool {
        grant.granted
    }
    fn deny_grant(request: &Self::Request) -> Self::Grant {
        JobExportGrant {
            token: request.token,
            granted: false,
            workplace: None,
            location: None,
            salary: 0,
        }
    }
    fn request_id(request: &Self::Request) -> UiRequestId {
        request.request_id
    }
    fn token(request: &Self::Request) -> u32 {
        request.token
    }
    fn resource_rank() -> u8 {
        1
    }
    fn process_request_event(request: ExportAllocationRequest<Self::Request>) -> RegionEvent {
        RegionEvent::ProcessJobExportRequest(request)
    }
    fn apply_grant_event(request: Self::Request, grant: Self::Grant) -> RegionEvent {
        RegionEvent::ApplyJobExportGrant { request, grant }
    }
    fn release_event(release: ExportAllocationRelease) -> RegionEvent {
        RegionEvent::ReleaseJobExportAllocations(release)
    }
}

/// Goods: fungible-unit hint, batch request, region+unit grant.
struct GoodsExport;

impl ExportResource for GoodsExport {
    type Request = GoodsExportRequest;
    type Grant = GoodsExportGrant;

    fn caller_region(request: &Self::Request) -> RegionId {
        request.caller_region
    }
    fn caller_network(request: &Self::Request) -> RegionRoadNetworkId {
        request.caller_network
    }
    fn has_spare(hint: &RegionalAvailabilityHint) -> bool {
        hint.spare_goods_units > 0
    }
    fn granted(grant: &Self::Grant) -> bool {
        grant.granted
    }
    fn deny_grant(request: &Self::Request) -> Self::Grant {
        GoodsExportGrant {
            token: request.token,
            granted: false,
            source_region: None,
            units: 0,
        }
    }
    fn request_id(request: &Self::Request) -> UiRequestId {
        request.request_id
    }
    fn token(request: &Self::Request) -> u32 {
        request.token
    }
    fn resource_rank() -> u8 {
        2
    }
    fn process_request_event(request: ExportAllocationRequest<Self::Request>) -> RegionEvent {
        RegionEvent::ProcessGoodsExportRequest(request)
    }
    fn apply_grant_event(_request: Self::Request, grant: Self::Grant) -> RegionEvent {
        RegionEvent::ApplyGoodsExportGrant(grant)
    }
    fn release_event(release: ExportAllocationRelease) -> RegionEvent {
        RegionEvent::ReleaseGoodsExportAllocations(release)
    }
}

enum WorkerRoutedMessage {
    None,
    Forwarded(ForwardedRegionEvent),
    ForwardedMany(Vec<ForwardedRegionEvent>),
    CommandReply(RegionCommandResponse),
    TickReply(RegionTickResponse),
    SnapshotReply(RegionSnapshotResponse),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::regional_types::UiRequestId;
    use crate::core::regions::RegionState;

    #[test]
    fn export_routing_reads_published_directory_without_rebuilding() {
        let directory = Arc::new(RegionDirectory::new(Vec::new()));
        let mut worker = RegionWorker::with_directory(WorkerId(100), Arc::clone(&directory));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 2)))
            .unwrap();
        let first = PowerExportRequest {
            request_id: UiRequestId(1),
            caller_region: RegionId(1),
            caller_network: network(1, 0),
            token: 10,
            demand: 1,
            consumer: crate::core::entity::Entity::new(RegionId(1), 0),
        };
        let second = PowerExportRequest {
            request_id: UiRequestId(2),
            caller_region: RegionId(1),
            caller_network: network(1, 0),
            token: 11,
            demand: 1,
            consumer: crate::core::entity::Entity::new(RegionId(1), 1),
        };

        worker
            .route_export_request::<PowerExport>(first, RegionRoutingMode::Immediate)
            .unwrap();
        worker
            .route_export_request::<PowerExport>(second, RegionRoutingMode::Immediate)
            .unwrap();

        // P-a: add_region now publishes a road report too, which triggers one
        // rebuild. After that, the two route_export_request calls are
        // idempotent (no rebuilt summaries), so the count stays at 1.
        assert_eq!(directory.rebuild_count(), 1);
    }

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

    #[test]
    fn missing_power_request_candidate_denies_caller_instead_of_routing_error() {
        let mut worker = RegionWorker::new(WorkerId(99));
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(1), 2, 2)))
            .unwrap();
        let request = ExportAllocationRequest {
            request: PowerExportRequest {
                request_id: UiRequestId(1),
                caller_region: RegionId(1),
                caller_network: network(1, 0),
                token: 7,
                demand: 1,
                consumer: crate::core::entity::Entity::new(RegionId(1), 0),
            },
            candidates: vec![network(2, 0), network(3, 0)],
            candidate_index: 0,
        };

        let result = worker.route_export_request_result::<PowerExport>(
            request,
            PowerExportGrant {
                token: 7,
                granted: false,
                source_region: None,
            },
            RegionRoutingMode::Immediate,
        );

        assert!(result.is_ok());
        assert_eq!(
            worker
                .region(RegionId(1))
                .expect("caller region")
                .pending_event_count(),
            1
        );
    }

    #[test]
    fn goods_export_request_routes_to_producer_and_back_to_caller() {
        let mut worker = RegionWorker::new(WorkerId(101));
        let mut caller = RegionState::new(RegionId(1), 2, 1);
        assert!(
            caller
                .build(1, 0, crate::interface::input::BuildingKind::Road)
                .success
        );
        let mut producer = RegionState::new(RegionId(2), 2, 2);
        assert!(
            producer
                .build(0, 0, crate::interface::input::BuildingKind::Road)
                .success
        );
        assert!(
            producer
                .build(1, 0, crate::interface::input::BuildingKind::Road)
                .success
        );
        assert!(
            producer
                .build(0, 1, crate::interface::input::BuildingKind::PowerPlant)
                .success
        );
        assert!(
            producer
                .build(1, 1, crate::interface::input::BuildingKind::Industrial)
                .success
        );
        worker.add_region(RegionRuntime::new(caller)).unwrap();
        worker.add_region(RegionRuntime::new(producer)).unwrap();
        worker.set_region_topology(vec![RegionNeighborLink::new(
            RegionId(1),
            crate::core::regions::BorderEdge::East,
            RegionId(2),
        )]);

        worker
            .route_export_request::<GoodsExport>(
                GoodsExportRequest {
                    request_id: UiRequestId(1),
                    caller_region: RegionId(1),
                    caller_network: network(1, 0),
                    token: 0,
                    units: 1,
                },
                RegionRoutingMode::Immediate,
            )
            .unwrap();
        let summary = worker.process_region_events(1);

        assert!(summary.routing_errors.is_empty());
        assert_eq!(worker.region(RegionId(1)).unwrap().pending_event_count(), 1);
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
        worker
            .route_outbound(
                RegionId(1),
                OutboundMessage::TravelerHandedOff(handoff),
                RegionRoutingMode::Immediate,
            )
            .unwrap();
        assert_eq!(
            worker.region(RegionId(2)).unwrap().pending_event_count(),
            1,
            "destination received a ReceiveTraveler event"
        );
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
            .push_event(RegionId(1), RegionEvent::StepTravel)
            .expect("event routed");
        let summary = worker.process_region_events_for_barrier(usize::MAX);

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
            .push_event(RegionId(1), RegionEvent::StepTravel)
            .expect("step routed");
        assert!(
            worker
                .process_region_events_for_barrier(usize::MAX)
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
