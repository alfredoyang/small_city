//! Stable, city-wide-unique entity identifier (CW5c).
//!
//! An `Entity` packs its **birth region** in the high 32 bits and a region-local
//! counter in the low 32 bits, so ids are unique across the whole city without any
//! shared allocator: each region's `World::spawn` increments its own `next_entity` and
//! tags it with the region. The region is the *birth* region — fixed for the entity's
//! life — so it survives relocation into another region's `World` (Model B), where it
//! can never collide with that region's own ids.
//!
//! ```text
//!   Entity(u64) = (region.0 as u64) << 32 | local
//!   region 0 ⇒ packed value == local, so `Entity(n)` still reads as "region 0, local n"
//! ```
//!
//! Kept a numeric newtype (not a struct) so it still serializes as a JSON map key.
//! `Ord`/`Hash` are over the packed value: within one region (constant high bits) the
//! ordering is just the local-id order, exactly as before.

use serde::{Deserialize, Serialize};

use crate::core::regions::RegionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Entity(pub u64);

impl Entity {
    /// Builds an id from its birth region and the region-local counter value.
    pub fn new(region: RegionId, local: u32) -> Self {
        Entity(((region.0 as u64) << 32) | local as u64)
    }

    /// The birth region packed into this id.
    pub fn region(self) -> RegionId {
        RegionId((self.0 >> 32) as u32)
    }

    /// The region-local counter value packed into this id.
    pub fn local(self) -> u32 {
        self.0 as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trips_region_and_local() {
        let id = Entity::new(RegionId(3), 42);
        assert_eq!(id.region(), RegionId(3));
        assert_eq!(id.local(), 42);
        // region 0 keeps the legacy shape: packed value == local id.
        assert_eq!(Entity::new(RegionId(0), 7), Entity(7));
        assert_eq!(Entity(7).region(), RegionId(0));
        // Different birth regions never collide on the same local counter.
        assert_ne!(Entity::new(RegionId(1), 5), Entity::new(RegionId(2), 5));
        // Within one region, ordering follows the local counter.
        assert!(Entity::new(RegionId(9), 1) < Entity::new(RegionId(9), 2));
    }
}
