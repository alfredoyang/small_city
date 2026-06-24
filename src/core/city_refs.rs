//! City-wide reference types that identify a cell across
//! regions without leaking a bare region-local `Entity` into another region's `World`.
//!
//! ```text
//!   CityCellRef    = { region, x, y }             portable crossing/save/display cell
//! ```

use serde::{Deserialize, Serialize};

use crate::core::regions::RegionId;

/// A city-wide reference to a grid cell, tagged with its region. Self-describing, so it
/// can travel on its own (display, roster, save, cross-region message) without the
/// owning entity ref beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CityCellRef {
    pub region: RegionId,
    pub x: usize,
    pub y: usize,
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
