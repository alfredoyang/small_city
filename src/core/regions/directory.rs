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

use std::collections::{BTreeSet, HashMap};
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
    /// Event-driven plan, P-2: monotonic snapshot generation, bumped once per
    /// `rebuild_discovery` call. Stored on the snapshot itself so a reader
    /// gets it atomically with the discovery data it's paired to — no
    /// separate read can observe a generation newer than the data it names.
    /// Regions read this per-slice (`RegionRuntime::set_discovery_generation`)
    /// and compare against their own `seen_*_generation` to decide whether a
    /// cross-region change happened since their last reconcile.
    pub generation: u64,
    /// Directory employment ledger plan, P7-b: a stable hash of the component
    /// graph below, bumped only when *connectivity* changes (a road, border
    /// link, or topology edge) — never when a hint *value* changes.
    ///
    /// `generation` moves on every publish, including hint-value churn
    /// (goods/power/spare-capacity numbers), so it cannot gate employer route
    /// reconciliation without firing on exactly the "unrelated resource noise"
    /// P7 forbids. The component graph is a function of links + topology + the
    /// hint node *set* only — hint values never touch it — so a hash of it is a
    /// connectivity-only signal. P7-c/d compare it against a per-region
    /// `seen_connectivity_fingerprint` to decide whether to re-check contract
    /// reachability. Nothing reads it yet (P7-b just populates it).
    pub connectivity_fingerprint: u64,
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

/// Returns every other region sharing a changed power-hint component.
///
/// The coordinator receives only these explicit recipients; topology remains
/// owned by this directory snapshot.
pub(crate) fn power_capacity_recheck_targets(
    discovery: &CrossRegionDiscovery,
    source: RegionId,
    hints: &[RegionalAvailabilityHint],
) -> Vec<RegionId> {
    let mut targets = BTreeSet::new();
    for hint in hints {
        let Some(component) = discovery.component_of(hint.network) else {
            continue;
        };
        for network in component {
            if network.region != source {
                targets.insert(network.region);
            }
        }
    }
    targets.into_iter().collect()
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
///                             active_snapshot ──clone──> consumer runtime request routing
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
    /// Event-driven plan, P-2: monotonic counter bumped once per
    /// `rebuild_discovery`, under the same lock as every other field here.
    generation: u64,
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

    /// P5b/P-a: the current region topology (read by the worker to build
    /// direct border-neighbour facts for road-report pricing).
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
        self.rebuild_discovery(&mut state);
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
        self.rebuild_discovery(&mut state);
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
        self.rebuild_discovery(&mut state);
        true
    }

    fn rebuild_discovery(&self, state: &mut DirectoryPublishState) {
        let links = flattened_region_values(&state.region_links);
        let availability_hints = flattened_region_values(&state.region_hints);
        let mut regions: Vec<RegionId> = state.region_road_reports.keys().copied().collect();
        regions.sort();
        let road_reports: Vec<RegionRoadReport> = regions
            .into_iter()
            .map(|region| state.region_road_reports[&region].clone())
            .collect();
        let region_routes = build_region_routes(&road_reports, &self.owners);
        state.generation += 1;
        let components = build_component_graph(&links, &availability_hints, &state.topology);
        let connectivity_fingerprint = connectivity_fingerprint(&components);
        let discovery = CrossRegionDiscovery {
            generation: state.generation,
            connectivity_fingerprint,
            components,
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
///
/// Mental model for one region report. A report is local pricing only: "where
/// can I leave this region, and what does it cost to drive between my border
/// openings?"
///
/// ```text
/// Region A
/// +-------------------+
/// | West link         |        East link
/// | to X              |        to B
/// |     <--- road ----+-------->
/// +-------------------+
///
/// RegionBorderLink = "this border opening crosses to that region"
///   { link: West/0, neighbour: X }
///   { link: East/0, neighbour: B }
///
/// RegionCrossCost  = "inside this region, entry border -> exit border costs N"
///   { entry: West/0, exit: East/0, cost: 7 }
/// ```
///
/// The directory first turns matching reports into a **border-node graph**:
/// each owned border opening `(R, link)` is a node, and edges are either
/// inside-region `(R, entry) -> (R, exit)` with weight = `RegionCrossCost.cost`
/// (clamped to >= 1) or border-crossing `(R, link) -> (N, link.complement())`
/// with weight 1. A line A-B-C with B's interior 7 becomes:
///
/// ```text
/// Published border links:
///   A East/0 faces B West/0
///   B East/0 faces C West/0
///
/// Border-node graph:
///
///   (A, East/0) --1--> (B, West/0) --7--> (B, East/0) --1--> (C, West/0)
/// ```
///
/// Then it runs one destination-rooted Dijkstra over the reversed graph per
/// target region T. For T=C:
///
/// ```text
/// Distance field (dist[node] = shortest path to T):
///
///   (A, East/0)   (B, West/0)  (B, East/0)   (C, West/0)
///        9             8             1             0
///
/// A's valid first hop is (A, East/0) -> (B, West/0) because 8 < 9.
/// B's valid first hop is (B, East/0) -> (C, West/0) because 0 < 1.
/// (B, West/0) -> (A, East/0) is rejected because 9 is not < 8, so
/// loops cannot form.
/// ```
///
/// Output shape:
///
/// ```text
/// RegionRoutes.to[C].from[A] =
///   RouteHop {
///     exits: [ExitLink { link: East/0, to_region: B, cost: 9 }],
///   }
/// ```
///
/// `RegionRoutes::exits_from(A)` flips this into the stepper-friendly answer:
/// "for final destination C, leave A through East/0 toward B."
fn build_region_routes(
    reports: &[RegionRoadReport],
    owners: &RegionOwnerDirectory,
) -> RegionRoutes {
    // Layer-1 graph uses **border-link nodes** so a region's interior is
    // paid for exactly once, no matter how many hops the path takes:
    //
    //   node  = (RegionId, BorderLinkId)            // one per published
    //                                                //   border opening
    //   edge  = 1) inside-region:  (R, entry) -> (R, exit)
    //                                 weight = RegionCrossCost{entry, exit, cost}
    //                                          clamped to >= 1 (strict-decrease)
    //          2) border-crossing: (R, link) -> (N, link.matching_neighbor_link())
    //                                 weight = 1
    //                                 payload = ExitLink { link, to_region: N, cost }
    //
    // For each owned destination T, run a destination-rooted Dijkstra over
    // the reversed graph: dist[(R, link)] = shortest path from (R, link)
    // to any (T, _) border node. Then a source region R's first hop is
    // any local border (R, link) whose crossing edge leads to a
    // (N, matching) node with strictly lower distance. The route cost
    // is the minimum over chosen next-hop regions, and ALL
    // strict-decrease exits to those chosen regions are emitted so a
    // token on a different local network is never stranded.

    let reports_by_region: HashMap<RegionId, &RegionRoadReport> =
        reports.iter().map(|r| (r.region, r)).collect();

    // Border nodes: every owned region's published border opening.
    // Two parallel edge lists per node: inside edges (forward, used to
    // enumerate successors when scanning forward) and crossing edges
    // (forward, carrying the ExitLink payload). For the reversed-graph
    // Dijkstra we also need a reverse index of crossing edges (built
    // below after the forward pass).
    type RouteNode = (RegionId, BorderLinkId);
    type InsideEdges = Vec<(RouteNode, u32)>;
    /// P-e: the per-crossing tuple carries the border link and the
    /// neighbour RegionId (no ExitLink yet — the per-exit `cost` is
    /// filled in the candidate phase from `dist[(N, matching)]`).
    type CrossingEdges = Vec<(RouteNode, BorderLinkId, RegionId, u32)>;
    let mut graph: HashMap<RouteNode, (InsideEdges, CrossingEdges)> = HashMap::new();
    // Reverse index of crossing edges: for each target (N, matching_link),
    // the list of predecessor (source_node, weight) pairs. Built once.
    let mut crossing_rev: HashMap<RouteNode, Vec<(RouteNode, u32)>> = HashMap::new();

    let owned_region_set: std::collections::HashSet<RegionId> = reports
        .iter()
        .map(|r| r.region)
        .filter(|r| owners.owner_of(*r).is_some())
        .collect();

    // 1. Inside-region edges: (R, entry) -> (R, exit).
    for report in reports {
        if !owned_region_set.contains(&report.region) {
            continue;
        }
        for c in &report.crossing_costs {
            let weight = c.cost.max(1);
            graph
                .entry((report.region, c.entry))
                .or_insert_with(|| (Vec::new(), Vec::new()))
                .0
                .push(((report.region, c.exit), weight));
        }
    }

    // 2. Border-crossing edges: (R, link) -> (N, matching_link).
    //    A crossing edge is valid only when the neighbour has a published
    //    border link with the complementary edge + same offset (no
    //    roadless-border edges).
    for report in reports {
        for bl in &report.border_links {
            let n = bl.neighbour;
            // Live-inbox check: only edges to owned regions matter for routing.
            if owners.owner_of(n).is_none() {
                continue;
            }
            let Some(n_report) = reports_by_region.get(&n) else {
                continue;
            };
            let matching = bl.link.matching_neighbor_link();
            // Confirm the neighbour has a published border link back to R with
            // the matching edge/offset. This is what stops a raw adjacency
            // from creating an edge.
            if !n_report
                .border_links
                .iter()
                .any(|b| b.link == matching && b.neighbour == report.region)
            {
                continue;
            }
            let target = (n, matching);
            graph
                .entry((report.region, bl.link))
                .or_insert_with(|| (Vec::new(), Vec::new()))
                .1
                .push((target, bl.link, n, 1));
            crossing_rev
                .entry(target)
                .or_default()
                .push(((report.region, bl.link), 1));
        }
    }

    // 2b. Reverse inside index: for each (R, exit), the predecessors
    //     (R, entry, weight) of original inside edges (entry -> exit).
    //     Used by the reversed Dijkstra to relax predecessors when
    //     popping a node. The forward graph's "inside" lists are
    //     entry -> exit; the reverse index is exit -> [(entry, w)].
    let mut inside_rev: HashMap<RouteNode, Vec<(RouteNode, u32)>> = HashMap::new();
    for (node, (inside, _)) in &graph {
        for (target, weight) in inside {
            inside_rev
                .entry(*target)
                .or_default()
                .push((*node, *weight));
        }
    }

    // 3. Per-destination Dijkstra. dist[(R, link)] = shortest path from
    //    (R, link) to any (T, _) in the original graph. We run it as
    //    forward Dijkstra on the reversed graph (seed T's nodes at 0).
    //    Border nodes for regions that own no report are absent from
    //    `graph` and never relax — they cannot be reached.
    let owned_regions: Vec<RegionId> = {
        let mut v: Vec<RegionId> = owned_region_set.iter().copied().collect();
        v.sort();
        v.dedup();
        v
    };
    let mut to_map: std::collections::HashMap<RegionId, RouteField> = HashMap::new();

    for t in &owned_regions {
        let t_report = match reports_by_region.get(t) {
            Some(r) => r,
            None => continue, // T has no report; cannot be a destination.
        };

        // Seed dist: every (T, link) border node starts at 0. Nodes not
        // present in the graph are unreachable and stay at MAX.
        let mut dist: HashMap<RouteNode, u32> = HashMap::new();
        let mut unvisited: std::collections::BTreeSet<(u32, RouteNode)> =
            std::collections::BTreeSet::new();
        for bl in &t_report.border_links {
            let node = (*t, bl.link);
            dist.insert(node, 0);
            unvisited.insert((0, node));
        }

        // Reversed-graph Dijkstra:
        //   inside edge (R, entry) -> (R, exit) in original means the
        //   reversed-graph successors of (R, exit) are (R, entry) with the
        //   same weight — we use `inside_rev` to look up predecessors when
        //   we pop a node.
        //   crossing edge (R, link) -> (N, matching) in original means the
        //   reversed-graph successors of (N, matching) are (R, link) with
        //   weight 1 — we use `crossing_rev` for that.
        while let Some((cost, node)) = unvisited.iter().next().copied() {
            unvisited.remove(&(cost, node));
            // Reverse inside predecessors: every (R, entry) with original
            // edge entry -> node gets relaxed as a reversed-graph successor
            // of `node`.
            if let Some(preds) = inside_rev.get(&node) {
                for (pre, weight) in preds {
                    let new_cost = cost.saturating_add(*weight);
                    let entry = dist.entry(*pre).or_insert(u32::MAX);
                    if new_cost < *entry {
                        *entry = new_cost;
                        unvisited.insert((new_cost, *pre));
                    }
                }
            }
            // Reverse crossing predecessors: every (P, source_link) whose
            // original crossing edge lands at `node` becomes a
            // reversed-graph successor with weight 1.
            if let Some(preds) = crossing_rev.get(&node) {
                for (pre, weight) in preds {
                    let new_cost = cost.saturating_add(*weight);
                    let entry = dist.entry(*pre).or_insert(u32::MAX);
                    if new_cost < *entry {
                        *entry = new_cost;
                        unvisited.insert((new_cost, *pre));
                    }
                }
            }
        }

        // 4. For each source region R, find every (R, link) whose
        //    crossing edge goes to a (N, matching) node with a strictly
        //    lower dist. Those are valid first hops; keep all
        //    minimum-cost ties.
        let mut from: std::collections::HashMap<RegionId, RouteHop> = HashMap::new();
        // Destination T: no hop.
        from.insert(*t, RouteHop { exits: Vec::new() });
        for r in &owned_regions {
            if r == t {
                continue;
            }
            let Some(r_report) = reports_by_region.get(r) else {
                continue;
            };
            // First pass: enumerate every strict-decrease crossing from
            // each reachable R border. For each next-hop region N,
            // n_dist_for_n = min over N's borders of dist[(N, link)].
            // The optimal route cost is
            //     best_next_total = min over valid N of (1 + n_dist_for_n).
            // Tied N's are kept. We then emit EVERY strict-decrease
            // crossing from any R border to any chosen N, so a token
            // on a disconnected local network always has an exit to
            // its chosen next-hop region (the frozen-token guarantee).
            let mut n_dist_for_n: std::collections::HashMap<RegionId, u32> =
                std::collections::HashMap::new();
            // Every valid (R, link) -> N crossing, by (link, N, exit).
            // P-e: every (R.link, n, target, n_dist) is a valid
            // strict-decrease crossing. The ExitLink is built in the
            // emission step with cost = 1 + n_dist.
            let mut valid_crossings: Vec<(BorderLinkId, RegionId, RouteNode, u32)> = Vec::new();
            for bl in &r_report.border_links {
                let r_node = (*r, bl.link);
                let r_dist = dist.get(&r_node).copied().unwrap_or(u32::MAX);
                if r_dist == u32::MAX {
                    continue;
                }
                let Some((_, crossings)) = graph.get(&r_node) else {
                    continue;
                };
                for (target, _link, n_region, _weight) in crossings {
                    let n_dist = dist.get(target).copied().unwrap_or(u32::MAX);
                    if n_dist == u32::MAX {
                        continue;
                    }
                    // Strict-decrease loop safety.
                    if n_dist >= r_dist {
                        continue;
                    }
                    valid_crossings.push((bl.link, *n_region, *target, n_dist));
                    let entry = n_dist_for_n.entry(*n_region).or_insert(u32::MAX);
                    if n_dist < *entry {
                        *entry = n_dist;
                    }
                }
            }
            if n_dist_for_n.is_empty() {
                continue; // r has no strict-decrease crossings
            }
            let best_next_total = n_dist_for_n
                .values()
                .map(|d| 1u32.saturating_add(*d))
                .min()
                .unwrap_or(u32::MAX);
            if best_next_total == u32::MAX {
                continue;
            }
            // Chosen next-hop regions: any N with 1 + n_dist_for_n == best_next_total.
            let chosen_regions: std::collections::HashSet<RegionId> = n_dist_for_n
                .iter()
                .filter_map(|(n, d)| {
                    if 1u32.saturating_add(*d) == best_next_total {
                        Some(*n)
                    } else {
                        None
                    }
                })
                .collect();
            // Build per-exit ExitLinks for every valid crossing to a
            // chosen next-hop region. cost = 1 (cross) + dist[(N, matching)]
            // = border-node distance from THIS exit onward to T.
            let mut all_exits: Vec<ExitLink> = Vec::new();
            for (link, n, _target, n_dist) in valid_crossings {
                if !chosen_regions.contains(&n) {
                    continue;
                }
                let cost = 1u32.saturating_add(n_dist);
                all_exits.push(ExitLink {
                    link,
                    to_region: n,
                    cost,
                });
            }
            // Stable sort by (cost, to_region, edge, offset) so the
            // emitted list is deterministic and per-exit costs are
            // observable in tests.
            all_exits.sort_by_key(|e| (e.cost, e.to_region.0, e.link.edge, e.link.offset));
            let exits: Vec<ExitLink> = dedup_exits(all_exits);
            from.insert(*r, RouteHop { exits });
        }
        to_map.insert(*t, RouteField { from });
    }

    RegionRoutes { to: to_map }
}

/// Deduplicate `ExitLink`s by (to_region, link.edge, link.offset). Preserves
/// the first occurrence's position.
fn dedup_exits(exits: Vec<ExitLink>) -> Vec<ExitLink> {
    let mut seen: std::collections::HashSet<(RegionId, BorderEdge, usize)> =
        std::collections::HashSet::new();
    let mut out: Vec<ExitLink> = Vec::with_capacity(exits.len());
    for e in exits {
        if seen.insert((e.to_region, e.link.edge, e.link.offset)) {
            out.push(e);
        }
    }
    out
}

/// P7-b: a stable, connectivity-only hash of the component graph.
///
/// `components` is canonically ordered (`UnionFind::components` sorts each
/// component and then the outer list), so hashing it directly is deterministic
/// — no re-sort needed. `DefaultHasher::new()` uses fixed keys, so the same
/// graph always hashes to the same value within a build; the fingerprint is
/// only ever compared against an earlier value from the same process, never
/// persisted, so cross-build hash stability is irrelevant.
///
/// Because the graph depends on links + topology + the hint node *set* (never
/// hint values), this hash does not move when a goods/power/spare-capacity
/// number changes — which is the whole point.
fn connectivity_fingerprint(components: &[Vec<RegionRoadNetworkId>]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    components.hash(&mut hasher);
    hasher.finish()
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
                    // West->East (1) and East->West (1) express B's interior
                    // traversal. The border-node model needs entry != exit
                    // pairs for the inside-region edges.
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
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
                            edge: BorderEdge::West,
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
        // For destination C (3) under the border-node model:
        //   dist[(C, West/0)] = 0
        //   dist[(B, East/0)] = 1   (cross from C: 0+1)
        //   dist[(B, West/0)] = 2   (inside B: 1+1)
        //   dist[(A, East/0)] = 3   (cross from B: 2+1)
        // A's only border is (East/0). Its crossing to (B, West/0) has
        // n_dist=2 < r_dist=3 (strict decrease), so A hops to B with
        // cost = 1 + 2 = 3.
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.exits[0].to_region, RegionId(2));
        // For destination A (1): dist[(A, East/0)] = 0, dist[(B, West/0)] = 1,
        // dist[(B, East/0)] = 2, dist[(C, West/0)] = 3. B's (West/0) crossing
        // to A has n_dist=0 < r_dist=1; B's (East/0) crossing to C has
        // n_dist=3 > r_dist=2 (rejected). B hops to A with cost = 1 + 0 = 1.
        let to_a_from_b = &routes.to[&RegionId(1)].from[&RegionId(2)];
        assert_eq!(to_a_from_b.exits[0].to_region, RegionId(1));
    }

    /// P-c regression: Layer 1 chooses the next-hop region, not a single local
    /// border cell. If A has two disconnected roads that both cross to B, keeping
    /// only the cheapest A→B border can strand a token that entered on the other
    /// road. Preserve all exits toward the chosen next-hop region; Layer 2 filters
    /// by local reachability.
    #[test]
    fn region_routes_preserve_all_exits_to_best_next_region() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2]);
        let a_east_0 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let a_east_1 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 1,
        };
        let b_west_0 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 0,
        };
        let b_west_1 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 1,
        };
        let reports = vec![
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![
                    RegionBorderLink {
                        link: a_east_0,
                        neighbour: RegionId(2),
                    },
                    RegionBorderLink {
                        link: a_east_1,
                        neighbour: RegionId(2),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: a_east_0,
                        exit: a_east_0,
                        cost: 1,
                    },
                    RegionCrossCost {
                        entry: a_east_1,
                        exit: a_east_1,
                        cost: 9,
                    },
                ],
            },
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: b_west_0,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: b_west_1,
                        neighbour: RegionId(1),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: b_west_0,
                        exit: b_west_0,
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: b_west_1,
                        exit: b_west_1,
                        cost: 0,
                    },
                ],
            },
        ];

        let routes = build_region_routes(&reports, &owners);
        let exits = &routes.to[&RegionId(2)].from[&RegionId(1)].exits;
        assert_eq!(
            exits,
            &vec![
                ExitLink {
                    link: a_east_0,
                    to_region: RegionId(2),
                    cost: 1,
                },
                ExitLink {
                    link: a_east_1,
                    to_region: RegionId(2),
                    cost: 1,
                },
            ]
        );
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
                    // B's slow interior: West->East (4) and East->West (4).
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::West,
                            offset: 0,
                        },
                        exit: BorderLinkId {
                            edge: BorderEdge::East,
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
                            edge: BorderEdge::West,
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
                    // C is a leaf — entry != exit pairs would force a real
                    // interior traversal. We only have one border connection
                    // on each side, so we use self-pair costs of 0; the new
                    // model treats those as no-progress from that border node
                    // and C's role as a destination is unaffected.
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
            // D: north → A, south → E. D's south->north and north->south
            // pairs each cost 1 (the "slow D-corridor" makes the
            // comparison meaningful).
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
                            edge: BorderEdge::North,
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
                            edge: BorderEdge::South,
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
                    // E's North->West and West->North pairs each cost 1.
                    RegionCrossCost {
                        entry: BorderLinkId {
                            edge: BorderEdge::North,
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
                            edge: BorderEdge::West,
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
        // Border-node distances from A to C (3):
        //   B-corridor: A->B (cross 1) + B West->East (4) + B->C (cross 1) = 6
        //   D-corridor: A->D (cross 1) + D North->South (1)
        //             + D->E (cross 1) + E North->West (1)
        //             + E->C (cross 1) = 5
        // A should hop to D (the cheaper corridor). A's cost to C is 5.
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
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
            // D: north → A, east → C. D's North->East and East->North pairs
            // each cost 0 (D is a thin passthrough region).
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
                            edge: BorderEdge::North,
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
                            edge: BorderEdge::East,
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

    /// P-d: **border-node cost counts middle region once.** A line A-B-C
    /// with B's interior traversal costing 10. The old region-edge model
    /// would have reported A→B=10 + B→C=10 = 20 (double-counted B).
    /// The new border-node model reports the full A→C distance as one
    /// cross + 10 interior + one cross = 12.
    #[test]
    fn region_routes_cost_counts_middle_region_once() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2, 3]);
        let west = BorderLinkId {
            edge: BorderEdge::West,
            offset: 0,
        };
        let east = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let reports = vec![
            // A is a leaf with one border. (East, East, 0) is a self-pair
            // (no real traversal needed) — keeps the data shape honest.
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![RegionBorderLink {
                    link: east,
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: east,
                    exit: east,
                    cost: 0,
                }],
            },
            // B has the slow traversal. (West, East, 10) and (East, West, 10).
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: west,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: east,
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: west,
                        exit: east,
                        cost: 10,
                    },
                    RegionCrossCost {
                        entry: east,
                        exit: west,
                        cost: 10,
                    },
                ],
            },
            // C is a leaf with one border.
            RegionRoadReport {
                region: RegionId(3),
                border_links: vec![RegionBorderLink {
                    link: west,
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: west,
                    exit: west,
                    cost: 0,
                }],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // A's cost to C = cross A->B (1) + B's interior West->East (10)
        // + cross B->C (1) = 12. NOT 20.
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.exits[0].to_region, RegionId(2));
    }

    /// P-d: **tie-break determinism.** A has two border nodes that both
    /// cross to the same destination T with identical cost. The selector
    /// must emit BOTH ExitLinks (a tied minimum, not a single one) and
    /// sort them deterministically by (to_region, edge, offset).
    #[test]
    fn region_routes_tie_break_is_deterministic() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2]);
        let east_0 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let east_1 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 1,
        };
        let west_0 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 0,
        };
        let west_1 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 1,
        };
        let reports = vec![
            // A: two borders (East/0, East/1) both leading to B.
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![
                    RegionBorderLink {
                        link: east_0,
                        neighbour: RegionId(2),
                    },
                    RegionBorderLink {
                        link: east_1,
                        neighbour: RegionId(2),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: east_0,
                        exit: east_0,
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: east_1,
                        exit: east_1,
                        cost: 0,
                    },
                ],
            },
            // B is the destination T; it has matching West borders.
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: west_0,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: west_1,
                        neighbour: RegionId(1),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: west_0,
                        exit: west_0,
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: west_1,
                        exit: west_1,
                        cost: 0,
                    },
                ],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // A's exits to T=B: both East borders are valid (each crosses to
        // the matching West of B at cost 1). Both are emitted; sorted by
        // (cost, to_region, edge, offset) -> East/0 then East/1 (same cost).
        let to_b_from_a = &routes.to[&RegionId(2)].from[&RegionId(1)];
        assert_eq!(to_b_from_a.exits.len(), 2);
        assert_eq!(
            to_b_from_a.exits[0],
            ExitLink {
                link: east_0,
                to_region: RegionId(2),
                cost: 1,
            }
        );
        assert_eq!(
            to_b_from_a.exits[1],
            ExitLink {
                link: east_1,
                to_region: RegionId(2),
                cost: 1,
            }
        );
    }

    /// P-d: **frozen-token regression for unequal distances to the
    /// same next-hop.** A has two borders that both cross to the
    /// same next-hop B, but B's matching West borders have different
    /// distances to T. Both A borders must still be emitted,
    /// otherwise a token on the "more expensive" A border is
    /// stranded.
    #[test]
    fn region_routes_emit_all_exits_to_chosen_next_hop_regardless_of_border_cost() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2, 3]);
        let a_east_0 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let a_east_1 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 1,
        };
        let b_west_0 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 0,
        };
        let b_west_1 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 1,
        };
        let b_east = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let reports = vec![
            // A: two borders both crossing to B.
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![
                    RegionBorderLink {
                        link: a_east_0,
                        neighbour: RegionId(2),
                    },
                    RegionBorderLink {
                        link: a_east_1,
                        neighbour: RegionId(2),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: a_east_0,
                        exit: a_east_0,
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: a_east_1,
                        exit: a_east_1,
                        cost: 0,
                    },
                ],
            },
            // B: West/0 connects to B's interior (cost 0) which reaches
            // East/0 (the C crossing) at cost 1. West/1 connects to the
            // interior at cost 5 (a longer interior route), then East/0
            // to C at cost 1. So:
            //   B.w0 -> ... -> C  cost 1 (just B's interior to East/0)
            //   B.w1 -> ... -> C  cost 5+1 = 6
            // A.e0 -> B.w0 has n_dist_for_B = 1.
            // A.e1 -> B.w1 has n_dist_for_B = 6.
            // The chosen next-hop region is B (only). Both A borders
            // emit, even though A.e1 is the more expensive first hop.
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: b_west_0,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: b_west_1,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: b_east,
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: b_west_0,
                        exit: b_east,
                        cost: 1,
                    },
                    RegionCrossCost {
                        entry: b_west_1,
                        exit: b_east,
                        cost: 5,
                    },
                ],
            },
            // C: only West connects to B's East/0.
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
                    cost: 0,
                }],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // A's RouteHop to T=C: dist[(B, b_west_0)] = 2 (cross C->B
        // 1 + B w0->east 1). dist[(B, b_west_1)] = 6 (cross C->B 1
        // + B w1->east 5). n_dist_for_B = min(2, 6) = 2.
        // best_next_total = 1 + 2 = 3. Both A.e0 and A.e1 are valid
        // strict-decrease exits to B (the only chosen next-hop region).
        // Both must be emitted. P-e: per-exit costs differ:
        //   A.e0 cost = 1 + dist[(B, b_west_0)] = 1 + 2 = 3
        //   A.e1 cost = 1 + dist[(B, b_west_1)] = 1 + 6 = 7
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.exits.len(), 2);
        // Sorted by (cost, to_region, edge, offset): cheaper East/0 first.
        assert_eq!(to_c_from_a.exits[0].link, a_east_0);
        assert_eq!(to_c_from_a.exits[0].cost, 3);
        assert_eq!(to_c_from_a.exits[1].link, a_east_1);
        assert_eq!(to_c_from_a.exits[1].cost, 7);
    }

    /// P-e: **per-exit cost on emitted ExitLinks.** A has two exits to
    /// the same next-hop region B. B's interior to the final target
    /// differs by which entry border the token uses (B entry 1 reaches
    /// C cheaply, entry 2 reaches C at twice the cost). Both exits
    /// are emitted, and the per-exit `cost` carries the border-node
    /// distance from THAT exit onward to T.
    #[test]
    fn region_routes_exit_links_carry_per_exit_cost() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2, 3]);
        let a_east_0 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let a_east_1 = BorderLinkId {
            edge: BorderEdge::East,
            offset: 1,
        };
        let b_west_0 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 0,
        };
        let b_west_1 = BorderLinkId {
            edge: BorderEdge::West,
            offset: 1,
        };
        let b_east = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let reports = vec![
            // A: two borders both crossing to B.
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![
                    RegionBorderLink {
                        link: a_east_0,
                        neighbour: RegionId(2),
                    },
                    RegionBorderLink {
                        link: a_east_1,
                        neighbour: RegionId(2),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: a_east_0,
                        exit: a_east_0,
                        cost: 0,
                    },
                    RegionCrossCost {
                        entry: a_east_1,
                        exit: a_east_1,
                        cost: 0,
                    },
                ],
            },
            // B: West/0 -> East (cost 8). West/1 -> East (cost 16).
            // C is reachable only via East/0.
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: b_west_0,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: b_west_1,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: b_east,
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![
                    RegionCrossCost {
                        entry: b_west_0,
                        exit: b_east,
                        cost: 8,
                    },
                    RegionCrossCost {
                        entry: b_west_1,
                        exit: b_east,
                        cost: 16,
                    },
                ],
            },
            // C: West connects to B's East/0.
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
                    cost: 0,
                }],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // dist[(B, b_west_0)] = 1 (cross C->B) + 8 = 9.
        // dist[(B, b_west_1)] = 1 (cross C->B) + 16 = 17.
        // dist[(B, b_east)] = 1 (cross C->B).
        // A.e0 -> (B, b_west_0): n_dist 9. cost = 1 + 9 = 10.
        // A.e1 -> (B, b_west_1): n_dist 17. cost = 1 + 17 = 18.
        // The chosen next-hop region is B (only). Both A borders are
        // emitted, with their distinct per-exit costs.
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.exits.len(), 2);
        // Sorted by (cost, to_region, edge, offset).
        assert_eq!(to_c_from_a.exits[0].cost, 10);
        assert_eq!(to_c_from_a.exits[0].link, a_east_0);
        assert_eq!(to_c_from_a.exits[1].cost, 18);
        assert_eq!(to_c_from_a.exits[1].link, a_east_1);
    }

    /// P-d: **asymmetric inside cost.** A-B-C with B's interior traversal
    /// only declared in one direction: (West -> East, 5) but NOT the
    /// reverse. The reversed Dijkstra must follow the directed inside
    /// edge, not assume symmetry. A path A -> C is: cross A->B (1) +
    /// inside B West->East (5) + cross B->C (1) = 7. The reverse data
    /// (East -> West) is missing, so the path A->B is one-way.
    #[test]
    fn region_routes_asymmetric_inside_cost() {
        use crate::core::regions::{
            BorderLinkId, RegionBorderLink, RegionCrossCost, RegionRoadReport,
        };
        let owners = owned(&[1, 2, 3]);
        let west = BorderLinkId {
            edge: BorderEdge::West,
            offset: 0,
        };
        let east = BorderLinkId {
            edge: BorderEdge::East,
            offset: 0,
        };
        let reports = vec![
            RegionRoadReport {
                region: RegionId(1),
                border_links: vec![RegionBorderLink {
                    link: east,
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: east,
                    exit: east,
                    cost: 0,
                }],
            },
            // B has the (West, East, 5) edge only — no reverse.
            RegionRoadReport {
                region: RegionId(2),
                border_links: vec![
                    RegionBorderLink {
                        link: west,
                        neighbour: RegionId(1),
                    },
                    RegionBorderLink {
                        link: east,
                        neighbour: RegionId(3),
                    },
                ],
                crossing_costs: vec![RegionCrossCost {
                    entry: west,
                    exit: east,
                    cost: 5,
                }],
            },
            RegionRoadReport {
                region: RegionId(3),
                border_links: vec![RegionBorderLink {
                    link: west,
                    neighbour: RegionId(2),
                }],
                crossing_costs: vec![RegionCrossCost {
                    entry: west,
                    exit: west,
                    cost: 0,
                }],
            },
        ];
        let routes = build_region_routes(&reports, &owners);
        // A -> C: cross A->B (1) + B's (West, East, 5) + cross B->C (1) = 7.
        let to_c_from_a = &routes.to[&RegionId(3)].from[&RegionId(1)];
        assert_eq!(to_c_from_a.exits[0].to_region, RegionId(2));
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

    // ---- P7-b: connectivity fingerprint ----

    fn hint_on(network: RegionRoadNetworkId, spare_goods: u32) -> RegionalAvailabilityHint {
        RegionalAvailabilityHint {
            network,
            has_spare_power: true,
            spare_job_slot_ids: Vec::new(),
            spare_goods_units: spare_goods,
        }
    }

    #[test]
    fn connectivity_fingerprint_is_stable_across_hint_value_changes() {
        // The whole point of P7-b: a goods/power/capacity number moving bumps the
        // generation (there IS a change) but must NOT move the connectivity
        // fingerprint, so it cannot fire employer route reconciliation.
        //
        // Vary EVERY hint value field while keeping `hint.network` fixed, so a
        // regression that folded any of them into the fingerprint is caught.
        let directory = RegionDirectory::new(Vec::new());
        let net = network(1, 0);
        let low = RegionalAvailabilityHint {
            network: net,
            has_spare_power: true,
            spare_job_slot_ids: Vec::new(),
            spare_goods_units: 0,
        };
        let high = RegionalAvailabilityHint {
            network: net,
            has_spare_power: false,         // value change
            spare_job_slot_ids: vec![1, 2], // value change
            spare_goods_units: 99,          // value change
        };

        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![low]));
        let first = directory.discovery_snapshot();

        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![high]));
        let second = directory.discovery_snapshot();

        assert!(
            second.generation > first.generation,
            "a hint-value change is still a publish: the generation moves"
        );
        assert_eq!(
            second.connectivity_fingerprint, first.connectivity_fingerprint,
            "but the connectivity fingerprint must not move on hint-value noise"
        );
    }

    #[test]
    fn connectivity_fingerprint_moves_when_the_hint_network_set_changes() {
        // Deliberate, and acceptable. `build_component_graph` seeds a node from
        // every hint network, so a hint appearing for a NEW network adds a
        // singleton to the component graph and moves the fingerprint. That is a
        // road-network *set* change (a new local network gained spare capacity),
        // not goods/power value noise -- and it only causes a harmless
        // conservative re-check downstream, never a false "unrelated noise" fire,
        // because a value change keeps the same network id (previous test).
        let directory = RegionDirectory::new(Vec::new());
        assert!(directory.publish_region(RegionId(1), Vec::new(), vec![hint_on(network(1, 0), 0)]));
        let before = directory.discovery_snapshot().connectivity_fingerprint;

        assert!(directory.publish_region(
            RegionId(1),
            Vec::new(),
            vec![hint_on(network(1, 0), 0), hint_on(network(1, 1), 0)],
        ));
        let after = directory.discovery_snapshot().connectivity_fingerprint;

        assert_ne!(
            after, before,
            "a new hint network is a node-set change: the fingerprint moves"
        );
    }

    #[test]
    fn connectivity_fingerprint_moves_when_a_border_link_appears() {
        // Two regions become connected: the component graph merges two singletons
        // into one component. The fingerprint must move.
        let directory = RegionDirectory::new(vec![RegionNeighborLink::new(
            RegionId(1),
            BorderEdge::East,
            RegionId(2),
        )]);
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

        // Both regions present but not yet linked to each other.
        assert!(directory.publish_region(RegionId(1), vec![left], Vec::new()));
        let before = directory.discovery_snapshot().connectivity_fingerprint;

        // Region 2 publishes the matching border link: the two networks join.
        assert!(directory.publish_region(RegionId(2), vec![right], Vec::new()));
        let after = directory.discovery_snapshot();

        assert_ne!(
            after.connectivity_fingerprint, before,
            "a new cross-region connection must move the connectivity fingerprint"
        );
        assert!(
            after
                .components
                .iter()
                .any(|component| component.len() == 2),
            "the two networks now share one component"
        );
    }

    #[test]
    fn connectivity_fingerprint_is_deterministic_for_the_same_graph() {
        let build = || {
            let directory = RegionDirectory::new(Vec::new());
            directory.publish_region(RegionId(1), Vec::new(), vec![hint_on(network(1, 0), 7)]);
            directory.publish_region(RegionId(2), Vec::new(), vec![hint_on(network(2, 0), 3)]);
            directory.discovery_snapshot().connectivity_fingerprint
        };
        assert_eq!(build(), build(), "same graph -> same fingerprint");
    }
}
