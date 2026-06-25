//! ECS component data used by buildings, citizens, power, pollution, and happiness systems.

use serde::{Deserialize, Serialize};

use crate::core::city_refs::CityCellRef;
use crate::core::entity::Entity;
use crate::core::regions::RegionId;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Grid location for cell-occupying entities.
///
/// Only buildings currently receive `Position`; citizens are intentionally off-grid and store
/// their residential building in `Citizen::home`. Systems use `Position` to inspect neighbors,
/// compute local effects, connect roads/power, and render map cells.
pub struct Position {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// A building's rectangular footprint, anchored at its `Position` (top-left corner).
///
/// Buildings start 1x1 and (from the multi-cell upgrade work) grow to a larger rectangle on
/// upgrade. The footprint cells are `anchor.x .. anchor.x + width` by `anchor.y .. anchor.y +
/// height`; every one of those grid cells maps back to the same building entity.
pub struct Footprint {
    pub width: u8,
    pub height: u8,
}

impl Footprint {
    /// The default single-cell footprint.
    pub const fn single() -> Self {
        Self {
            width: 1,
            height: 1,
        }
    }

    /// Number of grid cells this footprint occupies.
    pub fn area(&self) -> u32 {
        u32::from(self.width) * u32::from(self.height)
    }
}

impl Default for Footprint {
    fn default() -> Self {
        Self::single()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Building identity and upgrade level for entities placed on the grid.
///
/// `kind` determines which companion components are attached by placement: residential buildings
/// get `Population` and `PowerConsumer`, power plants get `PowerProvider`, industrial buildings
/// get `PollutionSource`, and parks get `HappinessEffect`. The upgrade system mutates `level` and
/// updates any dependent component values such as capacity or population limit.
pub struct Building {
    pub kind: BuildingKind,
    /// Player-facing building level. New buildings start at level 1; missing save data is treated as level 1.
    #[serde(default = "default_building_level")]
    pub level: u8,
    /// Kind-specific building state. Most buildings have no extra data; commercial buildings
    /// keep durable local goods inventory here so it stays attached to the building itself.
    #[serde(default)]
    pub data: BuildingData,
    /// Rectangular footprint anchored at this building's `Position`. Defaults to 1x1 so saves
    /// written before multi-cell buildings load unchanged.
    #[serde(default)]
    pub footprint: Footprint,
}

fn default_building_level() -> u8 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Kind-specific state stored directly on the owning building.
///
/// This avoids a separate component map for state that only makes sense for one building kind,
/// while leaving room for later variants such as warehouse, office, or civic building data.
pub enum BuildingData {
    None,
    /// Commercial inventory for goods made by local industrial buildings.
    ///
    /// Imported goods are not stored here yet. They are an on-demand fallback used by the economy
    /// system when a customer shops and this local inventory is empty. If imports later become
    /// delayed, limited, or trucked, this variant can grow an `imported_goods_stored` field.
    Commercial {
        local_goods_stored: i32,
        #[serde(default)]
        business: BusinessFinance,
    },
    /// Industrial business state for tracking private profit and reinvestment.
    Industrial {
        #[serde(default)]
        business: BusinessFinance,
    },
}

impl Default for BuildingData {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
/// Private business finance attached to revenue-generating buildings.
///
/// City money still tracks public taxes and fees. This finance record tracks the building owner's
/// retained profit so commercial and industrial buildings can later reinvest in automatic upgrades.
pub struct BusinessFinance {
    #[serde(default)]
    pub business_cash: i32,
    #[serde(default)]
    pub lifetime_profit: i32,
    #[serde(default)]
    pub days_profitable: i32,
    #[serde(default)]
    pub last_period_profit: i32,
    #[serde(default)]
    pub last_period_goods_from_city: i32,
    #[serde(default)]
    pub last_period_goods_from_outside: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Residential capacity cache for a housing building.
///
/// `max` is the building's housing capacity. `current` is kept in sync from citizen `home`
/// assignments so existing building views can still show population without exposing citizen
/// storage. Population growth creates citizen entities, then the citizen system refreshes this
/// cache from the number of citizens whose home points at the residential entity.
pub struct Population {
    pub current: i32,
    pub max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Individual resident state for an off-grid citizen entity.
///
/// Citizens do not occupy map cells. Stable citizen-only state is grouped here to keep the custom
/// ECS simple: `home` is a city-wide `Entity` to a residential building
/// (always in this region — a citizen is owned by its home region), `workplace_assignment`
/// stores the current derived local-or-remote job, and `morale` keeps the DT2
/// derived/time happiness boundary in one nested value.
/// Future movement/pathfinding state should remain in separate reusable components instead of
/// growing this record.
pub struct Citizen {
    /// Stable city-wide identity: this citizen's own entity (the map key), which packs
    /// its birth region. Never serialized — `RegionState` rebuilds it at the load boundary
    /// from the map key and region id.
    ///
    /// ponytail: no consumer reads `id` yet. It is stored ahead of the relocation
    /// mission (Model B), which needs a relocation-stable identity; drop it if that
    /// mission never lands.
    #[serde(skip, default)]
    pub id: Entity,
    #[serde(default)]
    pub age: u32,
    /// Home residential building. Stored as a city-wide `Entity`; a home is always local
    /// to the owning region. On disk it is just the bare entity (the packed u64 already
    /// carries its region), so no custom serde is needed.
    pub home: Entity,
    /// Derived local-or-remote workplace assignment used by simulation and views.
    ///
    /// This is rebuilt by the daily job phase. It is skipped on save for the same
    /// reason as imported power/job state: assignments are derived from local
    /// buildings, citizens, road components, and producer export allocations.
    #[serde(default, skip)]
    pub workplace_assignment: Option<WorkplaceAssignment>,
    #[serde(default)]
    pub morale: Morale,
    #[serde(default)]
    pub money: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Citizen morale, organized by the DT2 derived/time boundary.
///
/// ```text
/// conditions (home, work, power, amenities, pollution) --derived--> target
/// target - decay - rent_stress                         --time-----> actual
/// ```
///
/// `target` intentionally stores the raw, unclamped condition score so actual
/// happiness can preserve the old single-clamp formula. Views clamp target for
/// display.
pub struct Morale {
    pub actual: i32,
    pub target: i32,
    pub decay: i32,
    pub rent_stress: i32,
}

impl Default for Morale {
    fn default() -> Self {
        Self {
            actual: 50,
            target: 50,
            decay: 0,
            rent_stress: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Owned job assignment describing local or producer-exported work via city-wide refs.
///
/// `workplace` is the building's city-wide `Entity`; `location` is its
/// self-describing cell (kept because a remote workplace's entity cannot be
/// dereferenced locally — it is the only way to show where the job is). There is no
/// separate local/remote tag: a job is **local iff `workplace.region() == self_region`**
/// (`workplace.as_local(self_region).is_some()`), otherwise it is a remote/exported job
/// whose `workplace.region()` is the producer. `location.region == workplace.region()`.
///
/// UI adapters convert this to a view-safe shape and never expose the raw entity.
pub struct WorkplaceAssignment {
    pub workplace: Entity,
    pub location: CityCellRef,
    pub salary: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Power capacity supplied by a power plant entity.
///
/// The power system adds this capacity to any road network orthogonally adjacent to the provider.
/// Consumers connected to that powered road network draw from the shared capacity in deterministic
/// map order.
pub struct PowerProvider {
    #[serde(default = "default_power_capacity", alias = "radius")]
    pub capacity: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Source that granted power to a consumer during the latest power resolution.
///
/// This is derived state recomputed by the power system. R1 records only local
/// providers; regional export grants add owned cross-region source IDs without
/// changing the local request/grant flow.
pub enum PowerSource {
    Local(Entity),
    Imported { source_region: RegionId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Power demand and current powered state for a consuming building.
///
/// Residential, commercial, and industrial buildings use this component. `demand` is the capacity
/// they require from their road network, and `powered` is reset/recomputed by the power system each
/// refresh before downstream systems read it for growth, jobs, income, and inspect output. `source`
/// records the derived grant source for registry-based local and future cross-region accounting.
pub struct PowerConsumer {
    #[serde(default)]
    pub powered: bool,
    #[serde(default = "default_power_demand")]
    pub demand: i32,
    #[serde(default, skip_serializing)]
    pub source: Option<PowerSource>,
}

fn default_power_capacity() -> i32 {
    10
}

fn default_power_demand() -> i32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Pollution contribution from an industrial building.
///
/// The city pollution system sums these sources for global pollution, and the local effects system
/// uses industrial buildings to create nearby pollution pressure and land-value penalties.
pub struct PollutionSource {
    pub amount: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// City-wide happiness bonus from a park building.
///
/// Parks use this component for the global happiness effect. Local effects are still derived from
/// the park building's position and kind, so this component intentionally stores only the broad
/// happiness amount.
pub struct HappinessEffect {
    pub amount: i32,
}

/// P3 movement: where a citizen is in its daily commute. Lives only in the
/// `#[serde(skip)]` `World::travel` map (transient display/derived state — it is
/// rebuilt from the schedule each tick and never saved), so it carries no serde.
///
/// `AtHome`/`AtWork` are the idle endpoints (citizen inside a building, off the
/// road graph); `Traveling` means the citizen is on a road cell stepping toward
/// its destination. P5 will add an `Away` variant for a token out in a neighbor
/// region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TravelStatus {
    AtHome,
    AtWork,
    Traveling,
}

/// P3 movement: one citizen's per-tick trip state.
///
/// ```text
///   idle:      current_cell = None,       building = Some(b)   (inside building b)
///   en route:  current_cell = Some(road), building = None      (on that road cell)
///   destination = Some(b) while travelling toward b; None when idle.
/// ```
///
/// `building` records the building the citizen actually occupies while idle, so
/// the departure origin is read from movement state — **not** re-inferred from
/// the (mutable) workplace assignment. Re-inferring would teleport a citizen
/// whose assignment changed while idle, or strand one whose assignment cleared.
///
/// No stored path: the citizen re-reads the region route cache (`came_from`)
/// each tick and steps one cell, so this stays tiny and `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TravelState {
    pub status: TravelStatus,
    pub current_cell: Option<Entity>,
    pub destination: Option<Entity>,
    /// The building occupied while idle (`None` while travelling).
    pub building: Option<Entity>,
}

impl Default for TravelState {
    fn default() -> Self {
        // A citizen with no recorded trip is assumed home and idle; the movement
        // system fills `building` with the citizen's home on the first tick.
        Self {
            status: TravelStatus::AtHome,
            current_cell: None,
            destination: None,
            building: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn building_without_footprint_field_defaults_to_single_cell() {
        // Saves written before multi-cell buildings have no footprint; they must load as 1x1.
        let building: Building = serde_json::from_str(r#"{"kind":"Residential"}"#)
            .expect("legacy building json deserializes");
        assert_eq!(building.footprint, Footprint::single());
        assert_eq!(building.level, 1);
    }

    #[test]
    fn citizen_home_serializes_as_bare_entity() {
        // The on-disk form of `home` is just the entity (the packed u64 already carries
        // its region), so saves are unchanged and no placeholder is needed.
        let citizen = Citizen {
            id: Entity(3),
            age: 1,
            home: Entity::new(RegionId(9), 3),
            workplace_assignment: None,
            morale: Morale::default(),
            money: 5,
        };

        let json = serde_json::to_value(citizen).expect("serialize citizen");
        assert_eq!(
            json["home"],
            serde_json::json!(Entity::new(RegionId(9), 3).0),
            "home is the bare entity id (packed u64)"
        );

        // Loading it back (same shape) preserves the entity.
        let loaded: Citizen = serde_json::from_value(json).expect("deserialize citizen");
        assert_eq!(loaded.home, Entity::new(RegionId(9), 3));
    }
}
