//! Coordinator-owned cross-region discovery directory.
//!
//! `RegionDirectory` stores only owned summaries: topology, border-road links,
//! and stale-tolerant availability hints. It never owns or reads a region's ECS
//! `World`; workers publish summaries for regions that changed, and route export
//! requests from the built discovery snapshot.
//!
//! ```text
//! RegionRuntime owns World
//!        |
//!        | publishes owned summaries
//!        v
//! RegionDirectory
//!   topology + component graph + availability hints
//!        |
//!        | read-only candidate lookup
//!        v
//! RegionWorker routes export request
//!        |
//!        v
//! Producer RegionRuntime validates allocation authoritatively
//! ```

use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::regions::{
    BorderEdge, NetworkBorderLink, RegionId, RegionNeighborLink, RegionRoadNetworkId,
    RegionalAvailabilityHint,
};

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
/// Shared directory for cross-region discovery and availability hints.
///
/// M2 splits writes and reads onto two locks so a reader never blocks the
/// writer's heavy rebuild:
///
/// ```text
///   WRITE path (publish)                              READ path (route)
///
///  changed region ──publish──┐
///   summary differs          │
///                            v
///  unchanged region ··skip··> publish_state (Mutex)
///   no rebuild                 per-region links + hints
///                                   │ rebuild + swap
///                                   v
///                             active_snapshot ──clone──> route_export_request
///                              Mutex<Arc<discovery>>          │ pick from snapshot
///                                                             v
///                                                         candidates
///                                                          spare-hint filter
/// ```
///
/// `publish_region` rebuilds only when a region's normalized summary actually
/// changes (idempotent skip). The graph rebuild happens before the
/// `active_snapshot` lock, which is held only long enough to swap in a new
/// `Arc`. `discovery_snapshot` just clones that `Arc` and releases the lock, so
/// routing reads its own snapshot without contending on the rebuild.
pub struct RegionDirectory {
    publish_state: Mutex<DirectoryPublishState>,
    active_snapshot: Mutex<Arc<CrossRegionDiscovery>>,
    #[cfg(test)]
    rebuild_count: AtomicUsize,
}

impl Default for RegionDirectory {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

#[derive(Debug, Default)]
struct DirectoryPublishState {
    topology: Vec<RegionNeighborLink>,
    region_links: HashMap<RegionId, Vec<NetworkBorderLink>>,
    region_hints: HashMap<RegionId, Vec<RegionalAvailabilityHint>>,
}

impl RegionDirectory {
    pub fn new(topology: Vec<RegionNeighborLink>) -> Self {
        Self {
            publish_state: Mutex::new(DirectoryPublishState {
                topology,
                ..DirectoryPublishState::default()
            }),
            active_snapshot: Mutex::new(Arc::new(CrossRegionDiscovery::default())),
            #[cfg(test)]
            rebuild_count: AtomicUsize::new(0),
        }
    }

    pub fn set_topology(&self, topology: Vec<RegionNeighborLink>) {
        let mut state = self
            .publish_state
            .lock()
            .expect("region directory publish state lock poisoned");
        state.topology = topology;
        self.rebuild_discovery(&state);
    }

    pub fn refresh(
        &self,
        links: Vec<NetworkBorderLink>,
        availability_hints: Vec<RegionalAvailabilityHint>,
    ) {
        // Compatibility helper for test shims that still build a complete
        // discovery snapshot in one call. Production M2 publishing goes through
        // `publish_region` so unchanged region summaries do not rebuild.
        let mut state = self
            .publish_state
            .lock()
            .expect("region directory publish state lock poisoned");
        state.region_links = group_links_by_region(links);
        state.region_hints = group_hints_by_region(availability_hints);
        self.rebuild_discovery(&state);
    }

    /// Publishes one region's owned discovery summaries.
    ///
    /// Publishing is idempotent: if the normalized summaries are unchanged, the
    /// directory keeps the current discovery snapshot and avoids a rebuild.
    pub fn publish_region(
        &self,
        region: RegionId,
        links: Vec<NetworkBorderLink>,
        hints: Vec<RegionalAvailabilityHint>,
    ) -> bool {
        let links = normalize_links(links);
        let hints = normalize_hints(hints);
        let mut state = self
            .publish_state
            .lock()
            .expect("region directory publish state lock poisoned");
        let current_links = state.region_links.get(&region).cloned().unwrap_or_default();
        let current_hints = state.region_hints.get(&region).cloned().unwrap_or_default();
        if current_links == links && current_hints == hints {
            return false;
        }

        set_or_remove(&mut state.region_links, region, links);
        set_or_remove(&mut state.region_hints, region, hints);
        self.rebuild_discovery(&state);
        true
    }

    fn rebuild_discovery(&self, state: &DirectoryPublishState) {
        let links = flattened_region_values(&state.region_links);
        let availability_hints = flattened_region_values(&state.region_hints);
        let discovery = CrossRegionDiscovery {
            components: build_component_graph(&links, &availability_hints, &state.topology),
            availability_hints,
        };

        // Build work happens before this lock. Readers hold the active-snapshot
        // lock only long enough to clone the Arc, so they do not block the heavy
        // publish-state rebuild and old snapshots are dropped when readers stop
        // holding them.
        *self
            .active_snapshot
            .lock()
            .expect("region directory active snapshot lock poisoned") = Arc::new(discovery);
        #[cfg(test)]
        {
            self.rebuild_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn discovery_snapshot(&self) -> Arc<CrossRegionDiscovery> {
        Arc::clone(
            &self
                .active_snapshot
                .lock()
                .expect("region directory active snapshot lock poisoned"),
        )
    }

    #[cfg(test)]
    pub(crate) fn rebuild_count(&self) -> usize {
        self.rebuild_count.load(Ordering::Relaxed)
    }

    pub(crate) fn from_summaries(
        topology: Vec<RegionNeighborLink>,
        links: Vec<NetworkBorderLink>,
        availability_hints: Vec<RegionalAvailabilityHint>,
    ) -> Self {
        let directory = Self::new(topology);
        directory.refresh(links, availability_hints);
        directory
    }
}

fn set_or_remove<T>(map: &mut HashMap<RegionId, Vec<T>>, region: RegionId, values: Vec<T>) {
    if values.is_empty() {
        map.remove(&region);
    } else {
        map.insert(region, values);
    }
}

fn group_links_by_region(
    links: Vec<NetworkBorderLink>,
) -> HashMap<RegionId, Vec<NetworkBorderLink>> {
    let mut grouped: HashMap<RegionId, Vec<NetworkBorderLink>> = HashMap::new();
    for link in links {
        grouped.entry(link.network.region).or_default().push(link);
    }
    for values in grouped.values_mut() {
        *values = normalize_links(std::mem::take(values));
    }
    grouped
}

fn group_hints_by_region(
    hints: Vec<RegionalAvailabilityHint>,
) -> HashMap<RegionId, Vec<RegionalAvailabilityHint>> {
    let mut grouped: HashMap<RegionId, Vec<RegionalAvailabilityHint>> = HashMap::new();
    for hint in hints {
        grouped.entry(hint.network.region).or_default().push(hint);
    }
    for values in grouped.values_mut() {
        *values = normalize_hints(std::mem::take(values));
    }
    grouped
}

fn normalize_links(mut links: Vec<NetworkBorderLink>) -> Vec<NetworkBorderLink> {
    links.sort();
    links.dedup();
    links
}

fn normalize_hints(mut hints: Vec<RegionalAvailabilityHint>) -> Vec<RegionalAvailabilityHint> {
    hints.sort_by_key(|hint| hint.network);
    // `RegionState::availability_hints` publishes one hint per regional road
    // network. If that invariant changes, merge spare flags here instead of
    // dropping duplicates by key.
    hints.dedup_by_key(|hint| hint.network);
    hints
}

fn flattened_region_values<T: Clone>(map: &HashMap<RegionId, Vec<T>>) -> Vec<T> {
    let mut regions = map.keys().copied().collect::<Vec<_>>();
    regions.sort();
    regions
        .into_iter()
        .flat_map(|region| map.get(&region).into_iter().flatten().cloned())
        .collect()
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

    #[test]
    fn directory_builds_component_graph_from_owned_summaries() {
        let left = NetworkBorderLink {
            network: network(1, 0),
            link: crate::core::regions::BorderLinkId {
                edge: BorderEdge::East,
                offset: 0,
            },
        };
        let right = NetworkBorderLink {
            network: network(2, 0),
            link: crate::core::regions::BorderLinkId {
                edge: BorderEdge::West,
                offset: 0,
            },
        };
        let directory = RegionDirectory::from_summaries(
            vec![RegionNeighborLink::new(
                RegionId(1),
                BorderEdge::East,
                RegionId(2),
            )],
            vec![left, right],
            Vec::new(),
        );

        assert_eq!(
            directory.discovery_snapshot().component_of(network(1, 0)),
            Some([network(1, 0), network(2, 0)].as_slice())
        );
    }

    #[test]
    fn publishing_identical_region_summary_is_idempotent() {
        let directory = RegionDirectory::new(Vec::new());
        let hint = RegionalAvailabilityHint {
            network: network(1, 0),
            has_spare_power: true,
            has_spare_jobs: false,
        };

        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![hint]));
        assert_eq!(directory.rebuild_count(), 1);
        assert!(!directory.publish_region(RegionId(1), Vec::new(), vec![hint]));
        assert_eq!(directory.rebuild_count(), 1);
        assert!(directory.publish_region(RegionId(1), Vec::new(), Vec::new()));

        assert_eq!(directory.rebuild_count(), 2);
        assert!(directory.discovery_snapshot().availability_hints.is_empty());
    }

    #[test]
    fn publishing_keeps_old_reader_snapshot_alive_without_retaining_all_snapshots() {
        let directory = RegionDirectory::new(Vec::new());
        let first = RegionalAvailabilityHint {
            network: network(1, 0),
            has_spare_power: true,
            has_spare_jobs: false,
        };
        let second = RegionalAvailabilityHint {
            network: network(1, 0),
            has_spare_power: false,
            has_spare_jobs: true,
        };
        let third = RegionalAvailabilityHint {
            network: network(1, 0),
            has_spare_power: true,
            has_spare_jobs: true,
        };
        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![first]));
        let reader_snapshot = directory.discovery_snapshot();

        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![second]));
        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![third]));

        assert_eq!(directory.rebuild_count(), 3);
        assert_eq!(reader_snapshot.availability_hints, vec![first]);
        assert_eq!(
            directory.discovery_snapshot().availability_hints,
            vec![third]
        );
    }

    fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }
}
