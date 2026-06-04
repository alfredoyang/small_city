//! Regional state ownership plus resource cache rules for future cross-region simulation.
//!
//! This module keeps each region's ECS `World` private inside `RegionState`.
//! Runtime and worker code can use owned resource summaries and UI-safe views
//! without reading another region's ECS storage.
//!
//! ```text
//! Local tick path:
//!
//!   RegionState::tick_local()
//!                 |
//!                 v
//!   tick_world(&mut World)
//!                 |
//!                 v
//!   shared deterministic simulation helpers
//!     power -> stats -> local effects
//!     -> citizens/population/economy/business
//!                 |
//!                 v
//!   CommandResult events
//!
//! Imported resource processing:
//!
//!   RegionState::process_imported_resource(...)
//!                 |
//!                 v
//!   imported_resources.accept(resource)
//!                 |
//!       +---------+-------------------+
//!       |         |                   |
//!       v         v                   v
//!   Accepted  ReplacedOlderGeneration RejectedDuplicate/RejectedStale
//!       |         |                   |
//!       +----+----+                   v
//!            |                 forwarded_resources = []
//!            v
//!   Build forwarded resources for target neighbors:
//!     - skip source neighbor
//!     - subtract local_used_capacity
//!     - add border_crossing_cost
//!     - increment hop_count
//!     - stop at max_hops or zero capacity
//!            |
//!            v
//!   ImportedResourceResult
//!     decision
//!     forwarded_resources
//!
//! Neighbor reply recording:
//!
//!   RegionState::apply_neighbor_import_result(result)
//!                 |
//!                 v
//!   neighbor_import_results.push(result)
//!
//!   No other region's World is touched.
//! ```

use crate::core::simulation::{refresh_derived_state_for_world, tick_world};
use crate::core::systems::{build, bulldoze, replace, upgrade};
use crate::core::world::World;
use crate::interface::adapter::{inspect_world, view_world, view_world_with_overlay};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameView, InspectView};
use serde::{Deserialize, Serialize};

pub mod handle;
pub mod load_manager;
pub mod runtime;
pub mod threaded;
pub mod worker;
pub use runtime::continuation;

const IMPORTED_RESOURCE_CAPACITY_PER_SOURCE: u32 = 1;
const IMPORTED_RESOURCE_MAX_HOPS: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
/// Stable identity for one independently owned simulation region.
///
/// Future runtimes and workers will use this as a routing key. It is not an ECS
/// entity ID and should never identify another region's local `World` storage.
pub struct RegionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Compact categories of cross-region access that can be imported as cache.
///
/// These variants describe what a region exports through its borders without
/// exposing the building, citizen, or road entities that produced the resource.
pub enum ResourceKind {
    Jobs,
    ParkAccess,
    ServiceAccess,
    ShoppingAccess,
    RoadExitAccess,
    TrafficPressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Stable identity for one exported regional resource generation.
///
/// The origin region and kind identify the source of the resource, while
/// `generation` changes when that source's exported value changes. Forwarding
/// regions must preserve this ID so the same remote supply cannot echo back as
/// new supply under a different origin.
pub struct ResourceId {
    pub origin_region: RegionId,
    pub resource_kind: ResourceKind,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Rebuildable imported resource cache entry received from a neighboring region.
///
/// This is not authoritative remote state. It is a compact summary that a region
/// may use locally and forward to other neighbors until capacity or hop limits
/// are exhausted.
pub struct ImportedResource {
    /// Original exported resource identity. It stays unchanged while forwarded.
    pub id: ResourceId,
    /// Capacity still available after earlier regions have used part of it.
    pub remaining_capacity: u32,
    /// Number of border-to-border forwards already taken from the origin.
    pub hop_count: u32,
    /// Maximum allowed forwards before propagation stops.
    pub max_hops: u32,
    /// Integer distance/cost accumulated along the import path.
    pub travel_cost: u32,
    /// Neighbor that sent this resource to the receiving region.
    pub source_neighbor: RegionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Authoritative export count produced by one region runtime.
pub struct RegionalExport {
    pub region_id: RegionId,
    pub resource_kind: ResourceKind,
    pub count: u32,
    /// Monotonic only while a runtime is alive. Imported caches are rebuilt
    /// empty after load, so save files do not need to preserve this generation.
    pub generation: u64,
}

impl RegionalExport {
    pub fn imported_resource(self) -> ImportedResource {
        ImportedResource {
            id: ResourceId {
                origin_region: self.region_id,
                resource_kind: self.resource_kind,
                generation: self.generation,
            },
            remaining_capacity: self
                .count
                .saturating_mul(IMPORTED_RESOURCE_CAPACITY_PER_SOURCE),
            hop_count: 0,
            max_hops: IMPORTED_RESOURCE_MAX_HOPS,
            travel_cost: 0,
            source_neighbor: self.region_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Runtime-owned export delta for the worker to route to neighboring regions.
pub struct RegionalExportChange {
    pub source_region: RegionId,
    pub current: Vec<RegionalExport>,
    pub removed: Vec<ResourceKind>,
}

impl RegionalExportChange {
    pub fn tombstone(source_region: RegionId, resource_kind: ResourceKind) -> ImportedResource {
        ImportedResource {
            id: ResourceId {
                origin_region: source_region,
                resource_kind,
                generation: u64::MAX,
            },
            remaining_capacity: 0,
            hop_count: 0,
            max_hops: IMPORTED_RESOURCE_MAX_HOPS,
            travel_cost: 0,
            source_neighbor: source_region,
        }
    }
}

impl ImportedResource {
    /// Builds the copy that should be sent from `current_region` to one neighbor.
    ///
    /// This returns `None` when forwarding would immediately echo the resource
    /// back to the sender, exceed the hop limit, or leave no capacity for the
    /// next region.
    pub fn forwarded_to(
        self,
        current_region: RegionId,
        target_neighbor: RegionId,
        local_used_capacity: u32,
        border_crossing_cost: u32,
    ) -> Option<Self> {
        if target_neighbor == self.source_neighbor || self.hop_count >= self.max_hops {
            return None;
        }

        let remaining_capacity = self.remaining_capacity.saturating_sub(local_used_capacity);
        if remaining_capacity == 0 {
            return None;
        }

        Some(Self {
            remaining_capacity,
            hop_count: self.hop_count.saturating_add(1),
            travel_cost: self.travel_cost.saturating_add(border_crossing_cost),
            // From the target region's view, this region becomes the neighbor
            // that supplied the forwarded resource.
            source_neighbor: current_region,
            ..self
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Outcome of attempting to place an imported resource into a region cache.
///
/// Runtime code can use this later for deterministic tracing and for deciding
/// whether there is anything new to forward to neighboring regions.
pub enum ImportDecision {
    /// The cache had no matching origin/kind/generation and stored the resource.
    Accepted,
    /// The exact same `ResourceId` was already known.
    RejectedDuplicate,
    /// A newer generation for the same origin and kind was already known.
    RejectedStale,
    /// The resource was newer than older cached generations for its origin/kind.
    ReplacedOlderGeneration,
    /// A zero-capacity tombstone removed cached resources for its origin/kind.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result returned after one region processes a neighbor's imported resource.
///
/// Later runtime patches can route this owned value back to the caller region
/// without giving either side access to the other's ECS `World`.
pub struct ImportedResourceResult {
    pub decision: ImportDecision,
    pub forwarded_resources: Vec<ImportedResource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
/// Region-local cache of imported resources accepted from neighbors.
///
/// The cache intentionally stores a small vector. Patch 1 favors readable,
/// deterministic behavior over lookup complexity, and expected regional border
/// resource counts are small.
pub struct ImportedResourceCache {
    resources: Vec<ImportedResource>,
}

#[derive(Debug, Serialize, Deserialize)]
/// Serialized authoritative region state.
///
/// Rebuildable imported-resource caches and neighbor reply traces are
/// intentionally excluded from permanent saves.
pub(crate) struct RegionStateSaveRecord {
    id: RegionId,
    world: World,
}

impl ImportedResourceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resources(&self) -> &[ImportedResource] {
        &self.resources
    }

    /// Accepts a resource if it is new enough for this region's local cache.
    ///
    /// The same `ResourceId` is rejected as a duplicate. An older generation is
    /// rejected after a newer generation for the same origin and kind is known.
    /// A newer generation replaces older cached entries for that origin/kind.
    pub fn accept(&mut self, resource: ImportedResource) -> ImportDecision {
        if self.resources.iter().any(|known| known.id == resource.id) {
            return ImportDecision::RejectedDuplicate;
        }

        let same_source_kind = |known: &&ImportedResource| {
            known.id.origin_region == resource.id.origin_region
                && known.id.resource_kind == resource.id.resource_kind
        };

        if self
            .resources
            .iter()
            .filter(same_source_kind)
            .any(|known| known.id.generation > resource.id.generation)
        {
            return ImportDecision::RejectedStale;
        }

        let before_len = self.resources.len();
        self.resources.retain(|known| {
            known.id.origin_region != resource.id.origin_region
                || known.id.resource_kind != resource.id.resource_kind
                || known.id.generation > resource.id.generation
        });

        let decision = if self.resources.len() == before_len {
            ImportDecision::Accepted
        } else {
            ImportDecision::ReplacedOlderGeneration
        };

        self.resources.push(resource);
        decision
    }

    /// Removes cached resources for one authoritative origin/kind pair.
    ///
    /// Multi-region play uses zero-capacity resource messages as tombstones
    /// when the source region no longer exports a resource after bulldoze,
    /// replace, or another mutating command.
    pub fn remove_origin_kind(
        &mut self,
        origin_region: RegionId,
        resource_kind: ResourceKind,
    ) -> bool {
        let before_len = self.resources.len();
        self.resources.retain(|known| {
            known.id.origin_region != origin_region || known.id.resource_kind != resource_kind
        });
        self.resources.len() != before_len
    }

    /// Produces deterministic outbound resource copies for neighboring regions.
    ///
    /// Target neighbors are considered in caller-provided order. Each resource
    /// copy subtracts the same locally used capacity and adds the same border
    /// crossing cost; later gameplay patches can replace those inputs with
    /// per-neighbor route costs without changing the cache identity rule.
    pub fn forwarded_resources(
        &self,
        current_region: RegionId,
        local_used_capacity: u32,
        border_crossing_cost: u32,
        target_neighbors: &[RegionId],
    ) -> Vec<ImportedResource> {
        self.resources
            .iter()
            .flat_map(|resource| {
                target_neighbors.iter().filter_map(move |target_neighbor| {
                    resource.forwarded_to(
                        current_region,
                        *target_neighbor,
                        local_used_capacity,
                        border_crossing_cost,
                    )
                })
            })
            .collect()
    }
}

#[derive(Debug)]
/// Authoritative state for one independently simulated region.
///
/// The ECS `World` stays private inside this core wrapper. Runtime and worker
/// code should interact through these methods and owned regional resource
/// summaries, while UI code continues to use regional facades and UI-safe view models.
pub struct RegionState {
    id: RegionId,
    world: World,
    imported_resources: ImportedResourceCache,
    neighbor_import_results: Vec<ImportedResourceResult>,
}

impl RegionState {
    /// Creates a region with its own private ECS world and empty import cache.
    pub fn new(id: RegionId, width: usize, height: usize) -> Self {
        Self {
            id,
            world: World::new(width, height),
            imported_resources: ImportedResourceCache::new(),
            neighbor_import_results: Vec::new(),
        }
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    /// Advances only this region's local simulation using the shared tick order.
    pub fn tick_local(&mut self) -> CommandResult {
        tick_world(&mut self.world)
    }

    /// Applies one player build command through the core systems.
    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = build::build(&mut self.world, x, y, kind);
        refresh_derived_state_for_world(&mut self.world);
        result
    }

    /// Explains whether a build would succeed without mutating this region.
    pub fn preview_build(&self, x: usize, y: usize, kind: BuildingKind) -> BuildPreviewView {
        build::preview_build(&self.world, x, y, kind)
    }

    /// Removes one occupied cell through the core systems.
    pub fn bulldoze(&mut self, x: usize, y: usize) -> CommandResult {
        let result = bulldoze::bulldoze(&mut self.world, x, y);
        if result.success {
            refresh_derived_state_for_world(&mut self.world);
        }
        result
    }

    /// Replaces one occupied cell through the core systems.
    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = replace::replace(&mut self.world, x, y, kind);
        if result.success {
            refresh_derived_state_for_world(&mut self.world);
        }
        result
    }

    /// Upgrades one supported occupied cell through the core systems.
    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        let result = upgrade::upgrade(&mut self.world, x, y);
        if result.success {
            refresh_derived_state_for_world(&mut self.world);
        }
        result
    }

    /// Accepts one imported resource and builds deterministic forwarded copies.
    pub fn process_imported_resource(
        &mut self,
        resource: ImportedResource,
        local_used_capacity: u32,
        border_crossing_cost: u32,
        target_neighbors: &[RegionId],
    ) -> ImportedResourceResult {
        if resource.remaining_capacity == 0 {
            let removed = self
                .imported_resources
                .remove_origin_kind(resource.id.origin_region, resource.id.resource_kind);
            return ImportedResourceResult {
                decision: if removed {
                    ImportDecision::Removed
                } else {
                    ImportDecision::RejectedStale
                },
                forwarded_resources: Vec::new(),
            };
        }

        let decision = self.imported_resources.accept(resource);
        let forwarded_resources = if matches!(
            decision,
            ImportDecision::Accepted | ImportDecision::ReplacedOlderGeneration
        ) {
            target_neighbors
                .iter()
                .filter_map(|target_neighbor| {
                    resource.forwarded_to(
                        self.id,
                        *target_neighbor,
                        local_used_capacity,
                        border_crossing_cost,
                    )
                })
                .collect()
        } else {
            Vec::new()
        };

        ImportedResourceResult {
            decision,
            forwarded_resources,
        }
    }

    /// Records a completed neighbor import reply in this caller-owned region.
    pub fn apply_neighbor_import_result(&mut self, result: ImportedResourceResult) {
        self.neighbor_import_results.push(result);
    }

    /// Returns a UI-safe snapshot without exposing this region's ECS world.
    pub fn view(&self) -> GameView {
        view_world(&self.world)
    }

    /// Returns a UI-safe snapshot using the requested map overlay.
    pub fn view_with_overlay(&self, overlay: MapOverlayInput) -> GameView {
        view_world_with_overlay(&self.world, overlay)
    }

    /// Returns a UI-safe inspect model without exposing this region's ECS world.
    pub fn inspect(&self, x: usize, y: usize) -> InspectView {
        let mut inspect = inspect_world(&self.world, x, y);
        if !self.imported_resources.resources().is_empty() {
            // This is region-level imported-resource awareness surfaced through
            // the existing cell inspect channel until regional status panels
            // get a dedicated field.
            inspect.explanations.push(format!(
                "Imported regional resources: {}",
                self.imported_resources.resources().len()
            ));
        }
        inspect
    }

    pub fn imported_resources(&self) -> &[ImportedResource] {
        self.imported_resources.resources()
    }

    /// Derives current local exports from authoritative region state.
    pub fn exported_resource_counts(&self) -> Vec<(ResourceKind, u32)> {
        let mut exports: Vec<(ResourceKind, u32)> = Vec::new();
        for building in self.world.buildings.values() {
            let Some(resource_kind) = exported_resource_kind_for_building(building.kind) else {
                continue;
            };
            if let Some((_, count)) = exports
                .iter_mut()
                .find(|(known_kind, _)| *known_kind == resource_kind)
            {
                *count = (*count).saturating_add(1);
            } else {
                exports.push((resource_kind, 1));
            }
        }

        exports.sort_by_key(|(resource_kind, _)| *resource_kind);
        exports
    }

    /// Rebuilds transient imported cache state from authoritative local data.
    ///
    /// Regional export generation does not exist yet, so the current
    /// authoritative rebuild is an empty cache. Later export rules can populate
    /// this method without making imported resources permanent save data.
    pub fn rebuild_imported_resource_cache(&mut self) {
        self.imported_resources = ImportedResourceCache::new();
    }

    pub fn neighbor_import_results(&self) -> &[ImportedResourceResult] {
        &self.neighbor_import_results
    }

    pub(crate) fn into_save_record(self) -> RegionStateSaveRecord {
        RegionStateSaveRecord {
            id: self.id,
            world: self.world,
        }
    }

    pub(crate) fn from_save_record(record: RegionStateSaveRecord) -> Self {
        Self::from_world(record.id, record.world)
    }

    pub(crate) fn from_legacy_world_bytes(
        id: RegionId,
        bytes: &[u8],
    ) -> Result<Self, serde_json::Error> {
        let world = serde_json::from_slice(bytes)?;
        Ok(Self::from_world(id, world))
    }

    pub(crate) fn from_world(id: RegionId, mut world: World) -> Self {
        world.rebuild_entity_records();
        refresh_derived_state_for_world(&mut world);

        let mut state = Self {
            id,
            world,
            imported_resources: ImportedResourceCache::new(),
            neighbor_import_results: Vec::new(),
        };
        state.rebuild_imported_resource_cache();
        state
    }
}

fn exported_resource_kind_for_building(kind: BuildingKind) -> Option<ResourceKind> {
    match kind {
        BuildingKind::Road => None,
        BuildingKind::Residential => Some(ResourceKind::ServiceAccess),
        BuildingKind::Commercial => Some(ResourceKind::ShoppingAccess),
        BuildingKind::Industrial => Some(ResourceKind::Jobs),
        BuildingKind::PowerPlant => Some(ResourceKind::ServiceAccess),
        BuildingKind::Park => Some(ResourceKind::ParkAccess),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_record_restores_authoritative_world_without_import_cache() {
        let mut region = RegionState::new(RegionId(1), 3, 3);
        assert!(region.build(1, 1, BuildingKind::Residential).success);
        let result = region.process_imported_resource(
            ImportedResource {
                id: ResourceId {
                    origin_region: RegionId(9),
                    resource_kind: ResourceKind::Jobs,
                    generation: 1,
                },
                remaining_capacity: 4,
                hop_count: 0,
                max_hops: 2,
                travel_cost: 0,
                source_neighbor: RegionId(9),
            },
            0,
            1,
            &[],
        );
        assert_eq!(result.decision, ImportDecision::Accepted);
        assert_eq!(region.imported_resources().len(), 1);

        let saved_view = region.view();
        let restored = RegionState::from_save_record(region.into_save_record());

        assert_eq!(restored.view(), saved_view);
        assert!(restored.imported_resources().is_empty());
        assert!(restored.neighbor_import_results().is_empty());
    }

    #[test]
    fn exported_resource_counts_use_authoritative_buildings_without_view_adapter() {
        let mut region = RegionState::new(RegionId(1), 4, 4);
        assert!(region.build(0, 0, BuildingKind::Road).success);
        assert!(region.build(1, 0, BuildingKind::Park).success);
        assert!(region.build(2, 0, BuildingKind::Park).success);
        assert!(region.build(3, 0, BuildingKind::Industrial).success);

        assert_eq!(
            region.exported_resource_counts(),
            vec![(ResourceKind::Jobs, 1), (ResourceKind::ParkAccess, 2)]
        );
    }

    #[test]
    fn regional_state_imports_shared_simulation_helpers_not_game_facade() {
        let source = std::fs::read_to_string("src/core/regions/mod.rs").expect("region source");
        let forbidden = ["crate::core::", "game"].concat();

        assert!(!source.contains(&forbidden));
        assert!(source.contains("crate::core::simulation"));
    }
}
