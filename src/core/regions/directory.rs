//! Coordinator-owned cross-region discovery directory.
//!
//! `RegionDirectory` stores only owned summaries: topology, border-road links,
//! and stale-tolerant availability hints. It never owns or reads a region's ECS
//! `World`; workers publish summaries after processing their local runtimes and
//! route export requests from the built discovery snapshot.
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

#[derive(Debug, Default)]
/// Shared directory for cross-region discovery and availability hints.
pub struct RegionDirectory {
    topology: Vec<RegionNeighborLink>,
    discovery: CrossRegionDiscovery,
    #[cfg(test)]
    rebuild_count: usize,
}

impl RegionDirectory {
    pub fn new(topology: Vec<RegionNeighborLink>) -> Self {
        Self {
            topology,
            discovery: CrossRegionDiscovery::default(),
            #[cfg(test)]
            rebuild_count: 0,
        }
    }

    pub fn set_topology(&mut self, topology: Vec<RegionNeighborLink>) {
        self.topology = topology;
        self.discovery = CrossRegionDiscovery::default();
    }

    pub fn refresh(
        &mut self,
        links: Vec<NetworkBorderLink>,
        mut availability_hints: Vec<RegionalAvailabilityHint>,
    ) {
        availability_hints.sort_by_key(|hint| hint.network);
        self.discovery = CrossRegionDiscovery {
            components: build_component_graph(&links, &availability_hints, &self.topology),
            availability_hints,
        };
        #[cfg(test)]
        {
            self.rebuild_count += 1;
        }
    }

    pub fn discovery(&self) -> &CrossRegionDiscovery {
        &self.discovery
    }

    #[cfg(test)]
    pub(crate) fn rebuild_count(&self) -> usize {
        self.rebuild_count
    }

    pub(crate) fn from_summaries(
        topology: Vec<RegionNeighborLink>,
        links: Vec<NetworkBorderLink>,
        availability_hints: Vec<RegionalAvailabilityHint>,
    ) -> Self {
        let mut directory = Self::new(topology);
        directory.refresh(links, availability_hints);
        directory
    }
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
            directory.discovery().component_of(network(1, 0)),
            Some([network(1, 0), network(2, 0)].as_slice())
        );
    }

    fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }
}
