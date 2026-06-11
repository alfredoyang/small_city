//! Single-threaded worker that schedules multiple region runtimes fairly.
//!
//! The worker routes owned runtime messages between region inboxes. It never
//! reads or mutates ECS state directly; all simulation work stays inside each
//! `RegionRuntime`.

use crate::core::regional_types::{
    RegionCommandResponse, RegionSnapshotResponse, RegionTickResponse,
};
use crate::core::regions::handle::RegionHandle;
use crate::core::regions::load_manager::WorkerLoad;
use crate::core::regions::runtime::{
    ExportAllocationRelease, ExportAllocationRequest, JobExportRequest, OutboundMessage,
    PowerExportRequest, RegionEvent, RegionRuntime, RegionRuntimeError,
};
use crate::core::regions::{
    BorderEdge, JobExportGrant, NetworkBorderLink, PowerExportGrant, RegionId, RegionNeighborLink,
    RegionRoadNetworkId, RegionalAvailabilityHint,
};
use std::collections::HashMap;

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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
/// Summary returned after one worker scheduling pass.
pub struct WorkerRunSummary {
    pub processed_regions: usize,
    pub routing_errors: Vec<WorkerRoutingError>,
    pub command_replies: Vec<RegionCommandResponse>,
    pub tick_replies: Vec<RegionTickResponse>,
    pub snapshot_replies: Vec<RegionSnapshotResponse>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
/// Owned discovery snapshot used before authoritative cross-region requests.
///
/// Components are keyed by `(region, road-network)`, not just by region.
pub struct CrossRegionDiscovery {
    pub components: Vec<Vec<RegionRoadNetworkId>>,
    pub availability_hints: Vec<RegionalAvailabilityHint>,
}

impl CrossRegionDiscovery {
    pub fn component_of(&self, network: RegionRoadNetworkId) -> Option<&[RegionRoadNetworkId]> {
        self.components
            .iter()
            .find(|component| component.contains(&network))
            .map(Vec::as_slice)
    }
}

#[derive(Debug)]
/// Owns and schedules multiple regional runtimes on one thread.
pub struct RegionWorker {
    id: WorkerId,
    regions: Vec<RegionRuntime>,
    topology: Vec<RegionNeighborLink>,
}

impl RegionWorker {
    pub fn new(id: WorkerId) -> Self {
        Self {
            id,
            regions: Vec::new(),
            topology: Vec::new(),
        }
    }

    pub fn id(&self) -> WorkerId {
        self.id
    }

    pub fn add_region(&mut self, runtime: RegionRuntime) -> Result<(), RegionAddError> {
        let region_id = runtime.region_id();
        if self.region(region_id).is_some() {
            return Err(RegionAddError {
                error: WorkerRoutingError::DuplicateRegion { region_id },
                runtime: Box::new(runtime),
            });
        }

        self.regions.push(runtime);
        Ok(())
    }

    /// Removes one owned runtime so a caller can move it at a safe point.
    pub fn remove_region(&mut self, region_id: RegionId) -> Option<RegionRuntime> {
        let position = self
            .regions
            .iter()
            .position(|runtime| runtime.region_id() == region_id)?;

        Some(self.regions.remove(position))
    }

    pub fn region(&self, region_id: RegionId) -> Option<&RegionRuntime> {
        self.regions
            .iter()
            .find(|runtime| runtime.region_id() == region_id)
    }

    pub fn handle_for(&self, region_id: RegionId) -> Option<RegionHandle> {
        self.region(region_id).map(RegionRuntime::handle)
    }

    pub fn load(&self) -> WorkerLoad {
        let region_ids = self
            .regions
            .iter()
            .map(RegionRuntime::region_id)
            .collect::<Vec<_>>();
        let queued_events = self
            .regions
            .iter()
            .map(RegionRuntime::pending_event_count)
            .sum();

        WorkerLoad::new(self.id, region_ids, queued_events)
    }

    pub fn set_region_topology(&mut self, topology: Vec<RegionNeighborLink>) {
        self.topology = topology;
    }

    /// Builds discovery data only; availability hints are not allocations.
    ///
    /// The worker uses this component graph and stale-tolerant hints only to
    /// route export requests. The producer runtime remains authoritative for
    /// granting or denying export allocation.
    pub fn cross_region_discovery(&self, topology: &[RegionNeighborLink]) -> CrossRegionDiscovery {
        let links = self
            .regions
            .iter()
            .flat_map(|runtime| runtime.state().network_border_links())
            .collect::<Vec<_>>();
        let mut availability_hints = self
            .regions
            .iter()
            .flat_map(|runtime| runtime.state().availability_hints())
            .collect::<Vec<_>>();
        availability_hints.sort_by_key(|hint| hint.network);

        CrossRegionDiscovery {
            components: build_component_graph(&links, &availability_hints, topology),
            availability_hints,
        }
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
        if max_events_per_region == 0 {
            return WorkerRunSummary::default();
        }

        let mut processed_regions = 0;
        let mut outbound = Vec::new();

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
        }

        let mut routing_errors = Vec::new();
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
            match self.route_outbound(source_region, message) {
                Ok(WorkerRoutedMessage::CommandReply(reply)) => command_replies.push(reply),
                Ok(WorkerRoutedMessage::TickReply(reply)) => tick_replies.push(reply),
                Ok(WorkerRoutedMessage::SnapshotReply(reply)) => snapshot_replies.push(reply),
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
            OutboundMessage::PowerExportRequested(request) => {
                self.route_export_request::<PowerExport>(request)?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::PowerExportRequestCompleted { request, grant } => {
                self.route_export_request_result::<PowerExport>(request, grant)?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::PowerExportAllocationsReleased(release) => {
                self.route_export_allocation_release::<PowerExport>(release)?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::JobExportRequested(request) => {
                self.route_export_request::<JobExport>(request)?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::JobExportRequestCompleted { request, grant } => {
                self.route_export_request_result::<JobExport>(request, grant)?;
                Ok(WorkerRoutedMessage::None)
            }
            OutboundMessage::JobExportAllocationsReleased(release) => {
                self.route_export_allocation_release::<JobExport>(release)?;
                Ok(WorkerRoutedMessage::None)
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
    ) -> Result<(), WorkerRoutingError> {
        // TODO(CR2 perf): cache cross-region discovery for one scheduling pass
        // instead of rebuilding the component graph for every export request.
        let discovery = self.cross_region_discovery(&self.topology);
        let candidates = discovery
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
            .collect::<Vec<_>>();

        if candidates.is_empty() {
            return self.deny_export_request::<R>(&request);
        }

        let target_region = candidates[0].region;
        if self.region(target_region).is_none() {
            return self.deny_export_request::<R>(&request);
        }

        self.push_event(
            target_region,
            R::process_request_event(ExportAllocationRequest {
                request,
                candidates,
                candidate_index: 0,
            }),
        )
    }

    /// Applies a producer grant, or walks to the next candidate on a stale-hint
    /// denial; denies the caller once candidates are exhausted. Shared (CR3R).
    fn route_export_request_result<R: ExportResource>(
        &mut self,
        mut request: ExportAllocationRequest<R::Request>,
        grant: R::Grant,
    ) -> Result<(), WorkerRoutingError> {
        if R::granted(&grant) {
            return self.push_event(
                R::caller_region(&request.request),
                R::apply_grant_event(grant),
            );
        }

        request.candidate_index += 1;
        if request.candidate_index >= request.candidates.len() {
            return self.push_event(
                R::caller_region(&request.request),
                R::apply_grant_event(grant),
            );
        }

        let target_region = request.candidates[request.candidate_index].region;
        if self.region(target_region).is_none() {
            return self.deny_export_request::<R>(&request.request);
        }

        self.push_event(target_region, R::process_request_event(request))
    }

    /// Broadcasts a caller's new-generation release to every other owned region so
    /// producers drop the caller's prior reservations. Shared (CR3R).
    fn route_export_allocation_release<R: ExportResource>(
        &mut self,
        release: ExportAllocationRelease,
    ) -> Result<(), WorkerRoutingError> {
        // TODO(CR2 scale): this broadcasts to every owned region today. Track
        // producer regions recorded from successful caller grants (for example
        // imported power sources and remote workplace regions) so release
        // messages can be narrowed to producers that may hold old allocations.
        let target_regions = self
            .regions
            .iter()
            .map(RegionRuntime::region_id)
            .filter(|region_id| *region_id != release.caller_region)
            .collect::<Vec<_>>();

        for target_region in target_regions {
            self.push_event(target_region, R::release_event(release))?;
        }

        Ok(())
    }

    fn deny_export_request<R: ExportResource>(
        &mut self,
        request: &R::Request,
    ) -> Result<(), WorkerRoutingError> {
        self.push_event(
            R::caller_region(request),
            R::apply_grant_event(R::deny_grant(request)),
        )
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
    fn process_request_event(request: ExportAllocationRequest<Self::Request>) -> RegionEvent;
    fn apply_grant_event(grant: Self::Grant) -> RegionEvent;
    fn release_event(release: ExportAllocationRelease) -> RegionEvent;
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
            slot_id: None,
            salary: 0,
        }
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
    CommandReply(RegionCommandResponse),
    TickReply(RegionTickResponse),
    SnapshotReply(RegionSnapshotResponse),
}

fn build_component_graph(
    links: &[NetworkBorderLink],
    hints: &[RegionalAvailabilityHint],
    topology: &[RegionNeighborLink],
) -> Vec<Vec<RegionRoadNetworkId>> {
    let mut networks = links
        .iter()
        .map(|link| link.network)
        .chain(hints.iter().map(|hint| hint.network))
        .collect::<Vec<_>>();
    networks.sort();
    networks.dedup();

    let link_index = BorderLinkIndex::new(links);
    let mut union_find = UnionFind::new(&networks);
    for left in links {
        for neighbor in topology
            .iter()
            .filter(|neighbor| neighbor.allows_source(left.network.region, left.link.edge))
        {
            for right in link_index.matching_links(*left, *neighbor) {
                union_find.union(left.network, right.network);
            }
        }
    }

    union_find.components()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BorderLinkKey {
    region: RegionId,
    edge: BorderEdge,
    offset: usize,
}

#[derive(Debug)]
struct BorderLinkIndex {
    links: HashMap<BorderLinkKey, Vec<NetworkBorderLink>>,
}

impl BorderLinkIndex {
    fn new(links: &[NetworkBorderLink]) -> Self {
        let mut index: HashMap<BorderLinkKey, Vec<NetworkBorderLink>> = HashMap::new();
        for link in links {
            index
                .entry(BorderLinkKey::from(*link))
                .or_default()
                .push(*link);
        }
        Self { links: index }
    }

    fn matching_links(
        &self,
        left: NetworkBorderLink,
        topology: RegionNeighborLink,
    ) -> &[NetworkBorderLink] {
        self.links
            .get(&BorderLinkKey {
                region: topology.neighbor,
                edge: left.link.edge.complementary_neighbor_edge(),
                offset: left.link.offset,
            })
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

impl From<NetworkBorderLink> for BorderLinkKey {
    fn from(link: NetworkBorderLink) -> Self {
        Self {
            region: link.network.region,
            edge: link.link.edge,
            offset: link.link.offset,
        }
    }
}

#[derive(Debug)]
struct UnionFind {
    parent: HashMap<RegionRoadNetworkId, RegionRoadNetworkId>,
}

impl UnionFind {
    fn new(networks: &[RegionRoadNetworkId]) -> Self {
        Self {
            parent: networks
                .iter()
                .copied()
                .map(|network| (network, network))
                .collect(),
        }
    }

    fn union(&mut self, left: RegionRoadNetworkId, right: RegionRoadNetworkId) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return;
        }

        let (parent, child) = if left_root <= right_root {
            (left_root, right_root)
        } else {
            (right_root, left_root)
        };
        self.parent.insert(child, parent);
    }

    fn find(&mut self, network: RegionRoadNetworkId) -> RegionRoadNetworkId {
        let parent = *self.parent.get(&network).expect("known network");
        if parent == network {
            return network;
        }

        let root = self.find(parent);
        self.parent.insert(network, root);
        root
    }

    fn components(mut self) -> Vec<Vec<RegionRoadNetworkId>> {
        let mut networks = self.parent.keys().copied().collect::<Vec<_>>();
        networks.sort();

        let mut grouped: HashMap<RegionRoadNetworkId, Vec<RegionRoadNetworkId>> = HashMap::new();
        for network in networks {
            let root = self.find(network);
            grouped.entry(root).or_default().push(network);
        }

        let mut components = grouped.into_values().collect::<Vec<_>>();
        for component in &mut components {
            component.sort();
        }
        components.sort_by_key(|component| component[0]);
        components
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::regional_types::UiRequestId;
    use crate::core::regions::RegionState;

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
