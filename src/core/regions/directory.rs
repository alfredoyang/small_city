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

use crate::core::regions::worker::RegionOwnerDirectory;
use crate::core::regions::{
    BorderEdge, BorderLinkId, ExitLink, NetworkBorderLink, RegionId, RegionNeighborLink,
    RegionRoadNetworkId, RegionRoadReport, RegionRoutes, RegionalAvailabilityHint, RouteField,
    RouteHop,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
/// Owned discovery snapshot used before authoritative cross-region requests.
///
/// Components are keyed by `(region, road-network)`, not just by region.
pub struct CrossRegionDiscovery {
    pub components: Vec<Vec<RegionRoadNetworkId>>,
    pub availability_hints: Vec<RegionalAvailabilityHint>,
    /// P-a: per-region INPUT for the Layer-1 Dijkstra. Each region prices its
    /// own crossings (one Layer-2 Dijkstra per border-link pair) and publishes
    /// the report alongside the existing availability hint. The directory
    /// assembles all reports and runs the small Layer-1 Dijkstra on the
    /// region road graph (P-b).
    pub road_reports: Vec<RegionRoadReport>,
    /// P-b: the Layer-1 Dijkstra output — every source's answer for every
    /// destination. Destination-keyed (one Dijkstra seeded at T fills a whole
    /// field); read by the stepper to find the next-hop exit toward T.
    pub region_routes: RegionRoutes,
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
    /// P-b: owner filter for the Layer-1 Dijkstra (only owned regions are
    /// reachable nodes; only edges leading to owned regions are added).
    /// Set once at construction; the worker passes its `owners` in.
    owners: Arc<RegionOwnerDirectory>,
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
    /// P-a: per-region road reports (INPUT for the Layer-1 Dijkstra in P-b).
    region_road_reports: HashMap<RegionId, RegionRoadReport>,
}

impl RegionDirectory {
    /// P-b: builds the directory with an owner filter. The worker passes its
    /// `owners` here so the Layer-1 Dijkstra only considers owned regions
    /// (reachability is intrinsic to ownership — a P-b edge to an unowned
    /// region is useless because no worker can route to it).
    pub fn with_owners(
        topology: Vec<RegionNeighborLink>,
        owners: Arc<RegionOwnerDirectory>,
    ) -> Self {
        Self {
            publish_state: Mutex::new(DirectoryPublishState {
                topology,
                ..DirectoryPublishState::default()
            }),
            active_snapshot: Mutex::new(Arc::new(CrossRegionDiscovery::default())),
            owners,
            #[cfg(test)]
            rebuild_count: AtomicUsize::new(0),
        }
    }

    pub fn new(topology: Vec<RegionNeighborLink>) -> Self {
        Self {
            publish_state: Mutex::new(DirectoryPublishState {
                topology,
                ..DirectoryPublishState::default()
            }),
            active_snapshot: Mutex::new(Arc::new(CrossRegionDiscovery::default())),
            owners: Arc::new(RegionOwnerDirectory::default()),
            #[cfg(test)]
            rebuild_count: AtomicUsize::new(0),
        }
    }

    /// P5b: the current region topology (read by the worker to build the
    /// per-region border-neighbor hint for travel routing).
    pub fn topology(&self) -> Vec<RegionNeighborLink> {
        self.publish_state
            .lock()
            .expect("region directory publish state lock poisoned")
            .topology
            .clone()
    }

    /// P-c: the current snapshot's `region_routes.exits_from(region)` —
    /// for every reachable target T, the first-hop exits `region` should
    /// use to head toward T. None if the snapshot hasn't been rebuilt
    /// yet (no reports published).
    pub fn exits_from(&self, region: RegionId) -> Option<HashMap<RegionId, Vec<ExitLink>>> {
        let snapshot = self
            .active_snapshot
            .lock()
            .expect("region directory active-snapshot lock poisoned")
            .clone();
        let map = snapshot.region_routes.exits_from(region);
        if map.is_empty() { None } else { Some(map) }
    }

    pub fn set_topology(&self, topology: Vec<RegionNeighborLink>) {
        let mut state = self
            .publish_state
            .lock()
            .expect("region directory publish state lock poisoned");
        state.topology = topology;
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

    /// P-a: publishes one region's road report (INPUT for the Layer-1 Dijkstra
    /// assembled in P-b). The report is computed by the region (it owns the
    /// road graph) and pushed here. Publishing is idempotent.
    pub fn publish_region_road_report(&self, report: RegionRoadReport) -> bool {
        let mut state = self
            .publish_state
            .lock()
            .expect("region directory publish state lock poisoned");
        let current = state.region_road_reports.get(&report.region);
        if current == Some(&report) {
            return false;
        }
        set_or_remove_report(
            &mut state.region_road_reports,
            report.region,
            report.clone(),
        );
        self.rebuild_discovery(&state);
        true
    }

    fn rebuild_discovery(&self, state: &DirectoryPublishState) {
        let links = flattened_region_values(&state.region_links);
        let availability_hints = flattened_region_values(&state.region_hints);
        let mut regions: Vec<RegionId> = state.region_road_reports.keys().copied().collect();
        regions.sort();
        let road_reports: Vec<RegionRoadReport> = regions
            .into_iter()
            .map(|region| state.region_road_reports[&region].clone())
            .collect();
        let region_routes = build_region_routes(&road_reports, &self.owners);
        let discovery = CrossRegionDiscovery {
            components: build_component_graph(&links, &availability_hints, &state.topology),
            availability_hints,
            road_reports,
            region_routes,
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
}

fn set_or_remove<T>(map: &mut HashMap<RegionId, Vec<T>>, region: RegionId, values: Vec<T>) {
    if values.is_empty() {
        map.remove(&region);
    } else {
        map.insert(region, values);
    }
}

fn set_or_remove_report(
    map: &mut HashMap<RegionId, RegionRoadReport>,
    region: RegionId,
    report: RegionRoadReport,
) {
    if report.border_links.is_empty() && report.crossing_costs.is_empty() {
        map.remove(&region);
    } else {
        map.insert(region, report);
    }
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

/// P-b: assemble the region road graph from the per-region reports and run
/// Layer-1 Dijkstra (one Dijkstra seeded at each destination T). The
/// output `region_routes.to[T].from[R]` is r's answer for "how do I get
/// toward T?".
///
/// Loop-safety is the load-bearing invariant: a node is an entry to the
/// field from r only if `cost_to_T[r]` is strictly lower than `cost_to_T[n]`
/// for n = the neighbour's region on the chosen edge. A Dijkstra distance
/// is monotonic, so this is automatically true for a strictly-decreasing
/// step — **no A→B→A loop is possible**. Graph correctness is intrinsic:
/// edges are published border links (a road actually crosses), never raw
/// `RegionNeighborLink` adjacency, so a roadless border is never an edge
/// (**no dead-end**).
///
/// Determinism: every `Vec<ExitLink>` is sorted + deduped by
/// `(link.edge, link.offset, to_region)`; region-routes lookups are by
/// `HashMap`. One Dijkstra per destination T.
fn build_region_routes(
    reports: &[RegionRoadReport],
    owners: &RegionOwnerDirectory,
) -> RegionRoutes {
    // Region road graph (Layer-1):
    //   nodes  = regions that own a report,
    //   edges  = (r, n) pairs joined by a published border link (a road
    //            actually crosses — adjacency alone is not enough),
    //   weight = r_cost_to_exit + n_cost_from_entry, forced to be strictly
    //            positive (max 1) so the strict-decrease next-hop rule
    //            always fires.
    //
    // ponytail: the edge weight is the SUM of r's "interior→border" cost
    // and n's "border→interior" cost, both measured by the producer's
    // crossing_costs (which is Layer-2 road distance, symmetric). This
    // model double-counts the interior of a region that lies BETWEEN
    // two hops in a multi-hop path: a line A-B-C with B's traversal
    // cost 10 produces edge weights A→B=10, B→C=10, so cost_to_C(A) = 20
    // even though the true A→C interior cost is 10. The next-hop
    // direction (which neighbour to pick first) is still correct because
    // the inflation is symmetric per-edge; only the absolute RouteHop.cost
    // is approximate. P-c consumes `from[r].exits`, not `.cost`. The
    // exact fix is a border-node / line-graph Dijkstra keyed on
    // (region, border_link); deferred until the cost is used for budget.

    // Build a per-(r, n) → Vec<(weight, BorderLinkId)> map. Two border
    // links at different offsets may both reach n; we keep all of them
    // and let the next-hop selector pick the cheapest exit.
    let reports_by_region: HashMap<RegionId, &RegionRoadReport> =
        reports.iter().map(|r| (r.region, r)).collect();
    let mut r_to_n: HashMap<(RegionId, RegionId), Vec<(u32, BorderLinkId)>> = HashMap::new();
    for (r_id, r_report) in &reports_by_region {
        for bl in &r_report.border_links {
            let n_id = bl.neighbour;
            let Some(n_report) = reports_by_region.get(&n_id) else {
                continue;
            };
            // Find the matching n border link: same offset, complementary
            // edge (r's East ↔ n's West, etc.).
            for n_bl in &n_report.border_links {
                if n_bl.link.offset != bl.link.offset
                    || n_bl.link.edge != bl.link.edge.complementary_neighbor_edge()
                {
                    continue;
                }
                // r's cost to exit at bl.link: the minimum crossing_cost in
                // r's report where exit is bl.link, defaulting to 0 if no
                // self-crossing is published. A region adjacent to its own
                // border is already at the border, so the cost to "reach" the
                // border is 0; production reports may omit the (entry, exit)
                // self-pair when the region has only that one border.
                let r_cost_to_exit = r_report
                    .crossing_costs
                    .iter()
                    .filter(|c| c.exit == bl.link)
                    .map(|c| c.cost)
                    .min()
                    .unwrap_or(0);
                // n's cost from its border entry (n_bl.link) to n: the
                // minimum crossing_cost in n's report where entry is n_bl.link,
                // defaulting to 0 for the same reason.
                let n_cost_from_entry = n_report
                    .crossing_costs
                    .iter()
                    .filter(|c| c.entry == n_bl.link)
                    .map(|c| c.cost)
                    .min()
                    .unwrap_or(0);
                // Owned-region check: only r→n edges to owned regions
                // are useful (you can't cross into an unowned region).
                if owners.owner_of(n_id).is_some() {
                    // Edge weights must be strictly positive so the
                    // strict-decrease next-hop rule fires even when both
                    // regions have no published self-crossing cost (their
                    // distance to a shared border is 0 by default).
                    let raw = r_cost_to_exit.saturating_add(n_cost_from_entry);
                    let total = raw.max(1);
                    r_to_n
                        .entry((*r_id, n_id))
                        .or_default()
                        .push((total, bl.link));
                }
            }
        }
    }

    // 2. Dijkstra at T: for each owned destination T, compute cost_to_T
    //    over the region road graph (nodes = owned regions with reports,
    //    edges = r_to_n).
    let mut owned_regions: Vec<RegionId> = reports
        .iter()
        .map(|r| r.region)
        .filter(|r| owners.owner_of(*r).is_some())
        .collect();
    owned_regions.sort();
    owned_regions.dedup();

    let mut to_map: std::collections::HashMap<RegionId, RouteField> =
        std::collections::HashMap::new();

    for t in &owned_regions {
        // Cost to T from T is 0.
        let mut cost_to_t: std::collections::HashMap<RegionId, u32> =
            std::collections::HashMap::new();
        for r in &owned_regions {
            cost_to_t.insert(*r, u32::MAX);
        }
        cost_to_t.insert(*t, 0);

        // Destination-seeded Dijkstra: `cost_to_t[r]` = shortest path from r
        // to T in the original directed graph. We compute it by running
        // Dijkstra at T over the INCOMING edges of each node (equivalently:
        // Dijkstra in the reversed graph). For an original edge p→r with
        // weight w(p,r) (= p's cost to its border exit + r's cost from its
        // border entry), the cost to T from p is at most cost_to_t[r] + w(p,r).
        let mut unvisited: std::collections::BTreeSet<(u32, RegionId)> = cost_to_t
            .iter()
            .filter_map(|(r, c)| if *c == u32::MAX { None } else { Some((*c, *r)) })
            .collect();
        let mut visited: std::collections::HashSet<RegionId> = std::collections::HashSet::new();
        while let Some((cost, r)) = unvisited.iter().next().copied() {
            unvisited.remove(&(cost, r));
            if !visited.insert(r) {
                continue;
            }
            // Relax all p→r edges (incoming to r in the original graph).
            // For each (p, r) in r_to_n, take the minimum weight over all
            // exit links for that (p, r) pair, then update cost_to_t[p].
            let mut incoming: HashMap<RegionId, u32> = HashMap::new();
            for (rn, exits) in &r_to_n {
                if rn.1 != r {
                    continue;
                }
                let p = rn.0;
                if visited.contains(&p) {
                    continue;
                }
                let min_w = exits.iter().map(|(w, _)| *w).min().unwrap_or(u32::MAX);
                let entry = incoming.entry(p).or_insert(u32::MAX);
                if min_w < *entry {
                    *entry = min_w;
                }
            }
            for (p, w) in &incoming {
                let new_cost = cost.saturating_add(*w);
                let entry = cost_to_t.entry(*p).or_insert(u32::MAX);
                if new_cost < *entry {
                    *entry = new_cost;
                    unvisited.insert((new_cost, *p));
                }
            }
        }

        // 3. For each source region r, find the best next hop: the n with
        //    cost_to_t[n] < cost_to_t[r] and the minimum (cost_to_t[n] +
        //    r_to_n cost). Strict decrease ⇒ it's a real next hop.
        let mut from: std::collections::HashMap<RegionId, RouteHop> =
            std::collections::HashMap::new();
        for r in &owned_regions {
            if *r == *t {
                // Destination T: no next hop (you're there). r_graph_cost to
                // T's first border is 0 (you don't need to cross).
                from.insert(
                    *r,
                    RouteHop {
                        exits: Vec::new(),
                        cost: 0,
                    },
                );
                continue;
            }
            let r_cost = cost_to_t.get(r).copied().unwrap_or(u32::MAX);
            if r_cost == u32::MAX {
                continue; // unreachable
            }
            // Collect every candidate next hop with the minimum total cost,
            // then sort by (total, to_region, edge, offset) for determinism.
            // Ties are emitted as multiple ExitLinks so a region with several
            // equally-good first hops keeps all of them.
            let mut candidates: Vec<(u32, RegionId, BorderLinkId)> = Vec::new();
            let mut best_total: u32 = u32::MAX;
            for (rn, exits) in &r_to_n {
                if rn.0 != *r {
                    continue;
                }
                let n = rn.1;
                let n_cost = cost_to_t.get(&n).copied().unwrap_or(u32::MAX);
                if n_cost == u32::MAX {
                    continue;
                }
                // Strict decrease is the loop-safety invariant: a Dijkstra
                // distance strictly decreases along a shortest path.
                if n_cost >= r_cost {
                    continue;
                }
                // Expand every (weight, exit) candidate for this (r, n) edge.
                for (w, edge) in exits {
                    let total = w.saturating_add(n_cost);
                    if total < best_total {
                        best_total = total;
                        candidates.clear();
                        candidates.push((total, n, *edge));
                    } else if total == best_total {
                        candidates.push((total, n, *edge));
                    }
                }
            }
            if !candidates.is_empty() {
                candidates.sort_by_key(|(t, n, e)| (*t, n.0, e.edge, e.offset));
                let total_cost = candidates[0].0;
                let entry = from.entry(*r).or_insert(RouteHop {
                    exits: Vec::new(),
                    cost: u32::MAX,
                });
                entry.cost = total_cost;
                for (_, next_region, edge) in &candidates {
                    entry.exits.push(ExitLink {
                        link: *edge,
                        to_region: *next_region,
                    });
                }
            }
        }
        // Sort + dedupe exits deterministically.
        for hop in from.values_mut() {
            hop.exits
                .sort_by_key(|e| (e.link.edge, e.link.offset, e.to_region.0));
            hop.exits.dedup();
        }
        to_map.insert(*t, RouteField { from });
    }

    RegionRoutes { to: to_map }
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
        let directory = RegionDirectory::new(vec![RegionNeighborLink::new(
            RegionId(1),
            BorderEdge::East,
            RegionId(2),
        )]);
        directory.publish_region(RegionId(1), vec![left], Vec::new());
        directory.publish_region(RegionId(2), vec![right], Vec::new());

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
            spare_job_slot_ids: Vec::new(),
            spare_goods_units: 0,
        };

        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![hint.clone()]));
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
            spare_job_slot_ids: Vec::new(),
            spare_goods_units: 0,
        };
        let second = RegionalAvailabilityHint {
            network: network(1, 0),
            has_spare_power: false,
            spare_job_slot_ids: vec![1],
            spare_goods_units: 1,
        };
        let third = RegionalAvailabilityHint {
            network: network(1, 0),
            has_spare_power: true,
            spare_job_slot_ids: vec![1],
            spare_goods_units: 1,
        };
        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![first.clone()]));
        let reader_snapshot = directory.discovery_snapshot();

        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![second]));
        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![third.clone()]));

        assert_eq!(directory.rebuild_count(), 3);
        assert_eq!(reader_snapshot.availability_hints, vec![first]);
        assert_eq!(
            directory.discovery_snapshot().availability_hints,
            vec![third]
        );
    }

    /// P-b: A–B–C road graph with a real cost gradient (each crossing
    /// costs 1). `to[C].from[A]` → A/B link (next hop B, cost 1);
    /// `to[A].from[B]` → B/A link (next hop A, cost 1).
    #[test]
    fn region_routes_map_multihop_destination_to_first_hop() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2, 3]);
        let reports = vec![
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![RegionBorderLink {
                    link: BorderLinkId {
                        edge: BorderEdge::East,
                        offset: 0,
                    },
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: BorderLinkId {
                        edge: BorderEdge::East,
                        offset: 0,
                    },
                    exit: BorderLinkId {
                        edge: BorderEdge::East,
                        offset: 0,
                    },
                    cost: 1,
                }],
            },
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        cost: 1,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        cost: 1,
                    },
                ],
            },
            RegionRoadReport {
                region: RegionId(3),
                border_links: vec![RegionBorderLink {
                    link: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    exit: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    cost: 1,
                }],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // For destination C (3): cost_to_3 = {1: 4, 2: 2, 3: 0}. A→B
        // (cost 2) then B→C (cost 2). A's hop is to B (cost 4).
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.cost, 4);
        assert_eq!(to_c_from_a.exits[0].to_region, RegionId(2));
        // For destination A (1): cost_to_1 = {1: 0, 2: 2, 3: 4}. B's hop to
        // A: w=2, cost_to_1(A)=0 < cost_to_1(B)=2.
        let to_a_from_b = &routes.to[&RegionId(1)].from[&RegionId(2)];
        assert_eq!(to_a_from_b.cost, 2);
        assert_eq!(to_a_from_b.exits[0].to_region, RegionId(1));
    }

    /// P-b: **strict-decrease.** Inter-region edges are forced strictly
    /// positive (`max(1, r_cost + n_cost)`), so the Dijkstra distance to T
    /// strictly decreases along every path. In a simple A↔B pair, A is the
    /// destination T, so cost_to_T(A)=0 and cost_to_T(B)=1. B's only neighbour
    /// is A: total = w(B→A) + cost_to_T(A) = 1 + 0 = 1 (strict decrease
    /// 0 < 1 fires). B hops to A — the route is real, not a zero-weight loop.
    #[test]
    fn region_routes_pick_only_cost_decreasing_neighbour() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2]);
        let reports = vec![
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![RegionBorderLink {
                    link: BorderLinkId {
                        edge: BorderEdge::East,
                        offset: 0,
                    },
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: BorderLinkId {
                        edge: BorderEdge::East,
                        offset: 0,
                    },
                    exit: BorderLinkId {
                        edge: BorderEdge::East,
                        offset: 0,
                    },
                    cost: 0,
                }],
            },
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![RegionBorderLink {
                    link: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    neighbour: RegionId(1),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    exit: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    cost: 0,
                }],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // For destination 1: cost_to_1 = {1: 0, 2: 1}. Node 1 is the
        // destination (no hop). Node 2's only neighbour is A with w=1 and
        // cost_to_1(A)=0; strict-decrease 0 < 1 fires, so B hops to A.
        let to_a_from_b = &routes.to[&RegionId(1)].from[&RegionId(2)];
        assert_eq!(to_a_from_b.cost, 1);
        assert_eq!(to_a_from_b.exits[0].to_region, RegionId(1));
    }

    /// P-b: **weighting.** Two corridors A→B→C (2 hops, B crossing cost 1)
    /// and A→D→E→C (3 hops, each crossing cost 1). The Layer-1 picks the
    /// 3-hop corridor (cost 1) over the 2-hop corridor (cost 1+1=2). Actually
    /// these are equal: 2 hops × 1 = 2 vs 3 hops × 1 = 3 — pick the cheaper.
    /// Let's make the 2-hop B crossing cost 4 (a slow 2-hop) so 2+4=6 vs 3.
    #[test]
    fn region_routes_prefer_lower_cost_corridor() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let reports = vec![
            // A: east → B, south → D
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        neighbour: RegionId(2),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::South,
                            offset: 0,
                        },
                        neighbour: RegionId(4),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::South,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::South,
                            offset: 0,
                        },
                        cost: 0,
                    },
                ],
            },
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        cost: 4,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        cost: 4,
                    },
                ],
            },
            // C: west → B, east → E
            RegionRoadReport {
                region: RegionId(3),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        neighbour: RegionId(2),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        neighbour: RegionId(5),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        cost: 0,
                    },
                ],
            },
            // D: north → A, south → E. D's south exit costs 1, north entry
            // costs 1 (the "slow D-corridor" makes the comparison meaningful).
            RegionRoadReport {
                region: RegionId(4),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::South,
                            offset: 0,
                        },
                        neighbour: RegionId(5),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::South,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::South,
                            offset: 0,
                        },
                        cost: 1,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        cost: 1,
                    },
                ],
            },
            RegionRoadReport {
                region: RegionId(5),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        neighbour: RegionId(4),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        cost: 1,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        cost: 1,
                    },
                ],
            },
        ];
        let owners = owned(&[1, 2, 3, 4, 5]);
        let routes = build_region_routes(&reports, &owners);
        // For destination C (3): A→B→C = 0(A East) + 4(B West) + 4(B East) + 0(C West) = 8
        //                       A→D→E→C = 0(A South) + 1(D North) + 1(D South) + 1(E North) + 1(E West) + 0(C East) = 4
        // A should hop to D (the cheaper corridor), cost = 1(D North) + 1(D South) + 1(E) + 1(E) + 0(C) = 4.
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.cost, 4);
        assert_eq!(to_c_from_a.exits[0].to_region, RegionId(4));
    }

    /// P-b: **graph correctness.** A and B share a map border with NO road
    /// crossing it (roadless), but a road path A–D–C exists. `to[C].from[A]`
    /// routes via D, never the roadless edge. Edges come from published border
    /// links (a road actually crosses), never raw adjacency.
    #[test]
    fn region_routes_skip_roadless_border() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let reports = vec![
            // A: south → D (a road crosses). A's exit costs 1 so the route
            // is not degenerate (all-zero edges would put A at the same
            // Dijkstra distance as D, defeating the strict-decrease rule).
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![RegionBorderLink {
                    link: BorderLinkId {
                        edge: BorderEdge::South,
                        offset: 0,
                    },
                    neighbour: RegionId(4),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: BorderLinkId {
                        edge: BorderEdge::South,
                        offset: 0,
                    },
                    exit: BorderLinkId {
                        edge: BorderEdge::South,
                        offset: 0,
                    },
                    cost: 1,
                }],
            },
            // B: NO border link — A and B share a map border but no road crosses.
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![],
                crossing_costs: vec![],
            },
            // D: north → A, east → C
            RegionRoadReport {
                region: RegionId(4),
                border_links: vec![
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
                            offset: 0,
                        },
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::North,
                            offset: 0,
                        },
                        cost: 0,
                    },
                ],
            },
            // C: west → D
            RegionRoadReport {
                region: RegionId(3),
                border_links: vec![RegionBorderLink {
                    link: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    neighbour: RegionId(4),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    exit: BorderLinkId {
                        edge: BorderEdge::West,
                        offset: 0,
                    },
                    cost: 0,
                }],
            },
        ];
        let owners = owned(&[1, 2, 3, 4]);
        let routes = build_region_routes(&reports, &owners);
        // For destination C: A's next hop is D (the only edge A has).
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.exits[0].to_region, RegionId(4));
    }

    /// P-b test helper: build a `RegionOwnerDirectory` that owns the given
    /// region ids. The directory's `build_region_routes` filters by
    /// `owner_of(r)`, so the tests must register the regions they use.
    fn owned(region_ids: &[u32]) -> Arc<RegionOwnerDirectory> {
        use crate::core::regions::worker::WorkerId;
        let owners = RegionOwnerDirectory::default();
        for &r in region_ids {
            owners.register_region(RegionId(r), WorkerId(0)).unwrap();
        }
        Arc::new(owners)
    }

    fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
        RegionRoadNetworkId {
            region: RegionId(region),
            road_network,
        }
    }
}
