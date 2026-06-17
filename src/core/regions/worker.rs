//! Single-threaded worker that schedules multiple region runtimes fairly.
//!
//! The worker routes owned runtime messages between region inboxes. It never
//! reads or mutates ECS state directly; all simulation work stays inside each
//! `RegionRuntime`.

use crate::core::regional_types::{
    RegionCommandResponse, RegionSnapshotResponse, RegionTickResponse, UiRequestId,
};
pub use crate::core::regions::directory::CrossRegionDiscovery;
use crate::core::regions::directory::RegionDirectory;
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::runtime::{
    ExportAllocationRelease, ExportAllocationRequest, JobExportRequest, OutboundMessage,
    PowerExportRequest, RegionEvent, RegionRuntime, RegionRuntimeError,
};
use crate::core::regions::{
    JobExportGrant, NetworkBorderLink, PowerExportGrant, RegionId, RegionNeighborLink,
    RegionRoadNetworkId, RegionState, RegionalAvailabilityHint,
};
use std::collections::HashMap;
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

    fn register_region(
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

    forwarded.sort_by_key(|event| event.order_key);
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

#[derive(Debug)]
/// Owns and schedules multiple regional runtimes on one thread.
pub struct RegionWorker {
    id: WorkerId,
    regions: Vec<RegionRuntime>,
    directory: Arc<RegionDirectory>,
    owners: Arc<RegionOwnerDirectory>,
}

impl RegionWorker {
    pub fn new(id: WorkerId) -> Self {
        Self::with_directory(id, Arc::new(RegionDirectory::default()))
    }

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
        }
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
        Ok(())
    }

    /// Removes one owned runtime so a caller can move it at a safe point.
    pub fn remove_region(&mut self, region_id: RegionId) -> Option<RegionRuntime> {
        let position = self
            .regions
            .iter()
            .position(|runtime| runtime.region_id() == region_id)?;

        let runtime = self.regions.remove(position);
        self.publish_region_summary(region_id, Vec::new(), Vec::new());
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

    fn process_region_events_for_barrier(
        &mut self,
        max_events_per_region: usize,
    ) -> WorkerRunSummary {
        self.process_region_events_with_mode(max_events_per_region, RegionRoutingMode::Barrier)
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
            let source_region = runtime.region_id();
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
            changed_summaries.push((
                source_region,
                runtime.state().network_border_links(),
                runtime.state().availability_hints(),
            ));
        }

        for (region_id, links, hints) in changed_summaries {
            self.publish_region_summary(region_id, links, hints);
        }

        let mut routing_errors = Vec::new();
        let mut forwarded_events = Vec::new();
        let mut command_replies = Vec::new();
        let mut tick_replies = Vec::new();
        let mut snapshot_replies = Vec::new();

        // Allocation releases are causal cleanup for the next export cycle.
        // Route them before new requests so runtime traversal order cannot make
        // a producer deny a fresh caller with capacity allocated by an older
        // caller generation in the same worker pass. Both power and job releases
        // share this ordering rule.
        let (release_outbound, other_outbound): (Vec<_>, Vec<_>) =
            outbound.into_iter().partition(|(_, message)| {
                matches!(
                    message,
                    OutboundMessage::PowerExportAllocationsReleased(_)
                        | OutboundMessage::JobExportAllocationsReleased(_)
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
            OutboundMessage::RuntimeError(error) => Err(WorkerRoutingError::RuntimeError {
                source_region,
                error,
            }),
        }
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
            return self.route_region_event(
                R::caller_region(&request.request),
                R::caller_region(&request.request),
                R::apply_grant_event(grant),
                R::request_order_key(&request.request, R::caller_region(&request.request), 2),
                routing_mode,
            );
        }

        request.candidate_index += 1;
        if request.candidate_index >= request.candidates.len() {
            return self.route_region_event(
                R::caller_region(&request.request),
                R::caller_region(&request.request),
                R::apply_grant_event(grant),
                R::request_order_key(&request.request, R::caller_region(&request.request), 2),
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
        self.route_region_event(
            R::caller_region(request),
            R::caller_region(request),
            R::apply_grant_event(R::deny_grant(request)),
            R::request_order_key(request, R::caller_region(request), 2),
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
///        │  has_spare(hint)        → has_spare_power | has_spare_jobs      │
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
///   +-- JobExport   -> has_spare_jobs  -> ProcessJobExportRequest
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
    fn apply_grant_event(grant: Self::Grant) -> RegionEvent;
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
    fn apply_grant_event(grant: Self::Grant) -> RegionEvent {
        RegionEvent::ApplyPowerExportGrant(grant)
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
        hint.has_spare_jobs
    }
    fn granted(grant: &Self::Grant) -> bool {
        grant.granted
    }
    fn deny_grant(request: &Self::Request) -> Self::Grant {
        JobExportGrant {
            token: request.token,
            granted: false,
            source_region: None,
            position: None,
            slot_id: None,
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
    fn apply_grant_event(grant: Self::Grant) -> RegionEvent {
        RegionEvent::ApplyJobExportGrant(grant)
    }
    fn release_event(release: ExportAllocationRelease) -> RegionEvent {
        RegionEvent::ReleaseJobExportAllocations(release)
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
        };
        let second = PowerExportRequest {
            request_id: UiRequestId(2),
            caller_region: RegionId(1),
            caller_network: network(1, 0),
            token: 11,
            demand: 1,
        };

        worker
            .route_export_request::<PowerExport>(first, RegionRoutingMode::Immediate)
            .unwrap();
        worker
            .route_export_request::<PowerExport>(second, RegionRoutingMode::Immediate)
            .unwrap();

        assert_eq!(directory.rebuild_count(), 0);
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

    fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }
}
