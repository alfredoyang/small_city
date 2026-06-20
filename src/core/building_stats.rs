//! Single source of building capacity as a function of footprint area.
//!
//! Residential capacity is `max population`; commercial/industrial capacity is the number of job
//! slots. `area <= 1` (a 1x1 building) returns the base value; a larger footprint scales it by
//! `base * area * mult`, with `mult` = 3/2 for Residential & Commercial and 2 for Industrial
//! (integer math; see docs/multi-cell-buildings-plan.md). Non-zoned buildings have no area
//! capacity. Every system that needs a building's capacity goes through here so the formula lives
//! in exactly one place.

use crate::interface::input::BuildingKind;

pub(crate) fn capacity_for(kind: BuildingKind, area: u32) -> i32 {
    let (base, mult_num, mult_den): (i32, i32, i32) = match kind {
        BuildingKind::Residential => (5, 3, 2),
        BuildingKind::Commercial => (2, 3, 2),
        BuildingKind::Industrial => (3, 2, 1),
        BuildingKind::Road | BuildingKind::PowerPlant | BuildingKind::Park => return 0,
    };
    let area = area.max(1) as i32;
    if area <= 1 {
        base
    } else {
        base * area * mult_num / mult_den
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_scales_with_area_per_zone() {
        // area 1 = base; then base * area * mult.
        assert_eq!(capacity_for(BuildingKind::Residential, 1), 5);
        assert_eq!(capacity_for(BuildingKind::Residential, 2), 15);
        assert_eq!(capacity_for(BuildingKind::Residential, 4), 30);

        assert_eq!(capacity_for(BuildingKind::Commercial, 1), 2);
        assert_eq!(capacity_for(BuildingKind::Commercial, 2), 6);
        assert_eq!(capacity_for(BuildingKind::Commercial, 4), 12);

        assert_eq!(capacity_for(BuildingKind::Industrial, 1), 3);
        assert_eq!(capacity_for(BuildingKind::Industrial, 2), 12);
        assert_eq!(capacity_for(BuildingKind::Industrial, 4), 24);
    }

    #[test]
    fn non_zoned_buildings_have_no_area_capacity() {
        assert_eq!(capacity_for(BuildingKind::PowerPlant, 4), 0);
        assert_eq!(capacity_for(BuildingKind::Park, 4), 0);
        assert_eq!(capacity_for(BuildingKind::Road, 4), 0);
    }

    #[test]
    fn zero_area_clamps_to_base() {
        assert_eq!(capacity_for(BuildingKind::Residential, 0), 5);
    }
}
