//! City-wide reference types that identify a person, an entity, or a cell across
//! regions without leaking a bare region-local `Entity` into another region's `World`.
//!
//! Every region owns a private `World` whose `Entity` ids only mean something inside
//! that region. To talk about "who" or "where" across the region boundary we tag the
//! id with its owning `RegionId`. A region may only resolve such a ref back to a local
//! `Entity` when the ref's `region == self.id`; a foreign ref is carried/echoed, never
//! dereferenced.
//!
//! ```text
//!   CitizenId      = stable city-wide person id   (home_region + local entity)
//!   CityEntityRef  = { region, entity }           internal city-wide ECS reference
//!   CityCellRef    = { region, x, y }             portable crossing/save/display cell
//!
//!   ref.region == self.id  ─►  as_local(self) = Some(entity)   (safe to use locally)
//!   ref.region != self.id  ─►  as_local(self) = None           (carry, never deref)
//! ```

use serde::{Deserialize, Serialize};

use crate::core::entity::Entity;
use crate::core::regions::RegionId;

// `Ord`/`Hash` give deterministic ordering for sorts and map-key use; `Serialize`/
// `Deserialize` let these refs live in save records. All three are plain `Copy` data.

/// Stable city-wide identity for one person, independent of any region's local ids.
///
/// Derived from the citizen's `home_region` plus its home-region-local `Entity`, so two
/// regions never collide and a returning traveler can be matched to its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CitizenId {
    pub home_region: RegionId,
    pub local: Entity,
}

/// A city-wide reference to an ECS entity (a building/workplace), tagged with the
/// region that owns it. Resolve with [`CityEntityRef::as_local`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CityEntityRef {
    pub region: RegionId,
    pub entity: Entity,
}

/// A city-wide reference to a grid cell, tagged with its region. Self-describing, so it
/// can travel on its own (display, roster, save, cross-region message) without the
/// owning entity ref beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CityCellRef {
    pub region: RegionId,
    pub x: usize,
    pub y: usize,
}

impl CityEntityRef {
    /// Builds a reference to `entity` owned by `region`.
    pub fn local(region: RegionId, entity: Entity) -> Self {
        Self { region, entity }
    }

    /// The local `Entity` iff this ref belongs to `region`; `None` for a foreign ref.
    ///
    /// This is the single guard that keeps another region's ids from being used as
    /// local entities: callers only ever get an `Entity` back for refs they own.
    pub fn as_local(self, region: RegionId) -> Option<Entity> {
        (self.region == region).then_some(self.entity)
    }
}

impl CityCellRef {
    /// Builds a reference to cell `(x, y)` in `region`.
    pub fn local(region: RegionId, x: usize, y: usize) -> Self {
        Self { region, x, y }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_local_returns_entity_for_matching_region() {
        let r = RegionId(1);
        let reference = CityEntityRef::local(r, Entity(7));
        assert_eq!(reference.as_local(r), Some(Entity(7)));
    }

    #[test]
    fn as_local_returns_none_for_a_different_region() {
        let reference = CityEntityRef::local(RegionId(1), Entity(7));
        assert_eq!(reference.as_local(RegionId(2)), None);
    }

    #[test]
    fn citizen_id_ordering_is_deterministic() {
        // Ordered by (home_region, local): region is the major key, entity the minor.
        let a = CitizenId {
            home_region: RegionId(1),
            local: Entity(2),
        };
        let b = CitizenId {
            home_region: RegionId(1),
            local: Entity(5),
        };
        let c = CitizenId {
            home_region: RegionId(2),
            local: Entity(0),
        };

        let mut ids = vec![c, b, a];
        ids.sort();
        assert_eq!(ids, vec![a, b, c]);

        // Equality is structural.
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn city_cell_ref_carries_its_own_region() {
        let cell = CityCellRef::local(RegionId(3), 4, 5);
        assert_eq!(
            cell,
            CityCellRef {
                region: RegionId(3),
                x: 4,
                y: 5
            }
        );
    }
}
