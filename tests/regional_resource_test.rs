//! Integration tests for data-only regional imported resource propagation rules.

use small_city::core::regions::{
    ImportDecision, ImportedResource, ImportedResourceCache, RegionId, ResourceId, ResourceKind,
};

#[test]
fn cache_accepts_a_new_imported_resource() {
    let mut cache = ImportedResourceCache::new();
    let resource = resource(1, ResourceKind::ParkAccess, 7, 10, 0, 3, 2, 2);

    assert_eq!(cache.accept(resource), ImportDecision::Accepted);
    assert_eq!(cache.resources(), &[resource]);
}

#[test]
fn cache_rejects_the_same_resource_id() {
    let mut cache = ImportedResourceCache::new();
    let resource = resource(1, ResourceKind::Jobs, 4, 12, 1, 4, 3, 2);

    assert_eq!(cache.accept(resource), ImportDecision::Accepted);
    assert_eq!(cache.accept(resource), ImportDecision::RejectedDuplicate);
    assert_eq!(cache.resources(), &[resource]);
}

#[test]
fn cache_replaces_older_generation_for_same_origin_and_kind() {
    let mut cache = ImportedResourceCache::new();
    let old = resource(1, ResourceKind::ShoppingAccess, 2, 8, 0, 4, 1, 2);
    let new = resource(1, ResourceKind::ShoppingAccess, 3, 14, 0, 4, 1, 2);

    assert_eq!(cache.accept(old), ImportDecision::Accepted);
    assert_eq!(cache.accept(new), ImportDecision::ReplacedOlderGeneration);

    assert_eq!(cache.resources(), &[new]);
}

#[test]
fn cache_rejects_older_generation_after_newer_generation_is_known() {
    let mut cache = ImportedResourceCache::new();
    let old = resource(1, ResourceKind::ServiceAccess, 2, 8, 0, 4, 1, 2);
    let new = resource(1, ResourceKind::ServiceAccess, 3, 14, 0, 4, 1, 2);

    assert_eq!(cache.accept(new), ImportDecision::Accepted);
    assert_eq!(cache.accept(old), ImportDecision::RejectedStale);

    assert_eq!(cache.resources(), &[new]);
}

#[test]
fn cache_removes_all_resources_for_origin_and_kind() {
    let mut cache = ImportedResourceCache::new();
    let park = resource(1, ResourceKind::ParkAccess, 2, 8, 0, 4, 1, 2);
    let jobs = resource(1, ResourceKind::Jobs, 3, 14, 0, 4, 1, 2);
    let remote_park = resource(2, ResourceKind::ParkAccess, 1, 6, 0, 4, 1, 2);

    assert_eq!(cache.accept(park), ImportDecision::Accepted);
    assert_eq!(cache.accept(jobs), ImportDecision::Accepted);
    assert_eq!(cache.accept(remote_park), ImportDecision::Accepted);

    assert!(cache.remove_origin_kind(RegionId(1), ResourceKind::ParkAccess));
    assert_eq!(cache.resources(), &[jobs, remote_park]);
    assert!(!cache.remove_origin_kind(RegionId(1), ResourceKind::ParkAccess));
}

#[test]
fn forwarding_uses_remaining_capacity_and_adds_cost_and_hop() {
    let mut cache = ImportedResourceCache::new();
    let original = resource(1, ResourceKind::Jobs, 1, 10, 1, 4, 5, 2);
    assert_eq!(cache.accept(original), ImportDecision::Accepted);

    let forwarded = cache.forwarded_resources(RegionId(4), 3, 2, &[RegionId(3)]);

    assert_eq!(
        forwarded,
        vec![ImportedResource {
            remaining_capacity: 7,
            hop_count: 2,
            travel_cost: 7,
            source_neighbor: RegionId(4),
            ..original
        }]
    );
}

#[test]
fn forwarding_does_not_send_back_to_source_neighbor() {
    let mut cache = ImportedResourceCache::new();
    let original = resource(1, ResourceKind::ParkAccess, 1, 10, 0, 4, 0, 2);
    assert_eq!(cache.accept(original), ImportDecision::Accepted);

    let forwarded = cache.forwarded_resources(RegionId(4), 0, 1, &[RegionId(2), RegionId(3)]);

    assert_eq!(
        forwarded,
        vec![ImportedResource {
            source_neighbor: RegionId(4),
            hop_count: 1,
            travel_cost: 1,
            ..original
        }]
    );
}

#[test]
fn forwarding_stops_at_max_hops_or_zero_capacity() {
    let mut cache = ImportedResourceCache::new();
    let at_max_hops = resource(1, ResourceKind::RoadExitAccess, 1, 10, 2, 2, 0, 2);
    let no_remaining_capacity = resource(2, ResourceKind::TrafficPressure, 1, 4, 0, 2, 0, 3);

    assert_eq!(cache.accept(at_max_hops), ImportDecision::Accepted);
    assert_eq!(
        cache.accept(no_remaining_capacity),
        ImportDecision::Accepted
    );

    assert!(
        cache
            .forwarded_resources(RegionId(5), 4, 1, &[RegionId(4)])
            .is_empty()
    );
}

fn resource(
    origin_region: u32,
    resource_kind: ResourceKind,
    generation: u64,
    remaining_capacity: u32,
    hop_count: u32,
    max_hops: u32,
    travel_cost: u32,
    source_neighbor: u32,
) -> ImportedResource {
    ImportedResource {
        id: ResourceId {
            origin_region: RegionId(origin_region),
            resource_kind,
            generation,
        },
        remaining_capacity,
        hop_count,
        max_hops,
        travel_cost,
        source_neighbor: RegionId(source_neighbor),
    }
}
