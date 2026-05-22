//! Deterministic map partition metadata for future region-owned simulation actors.
//!
//! This module maps grid cells to stable region IDs and exposes region bounds/neighbors. It does
//! not own ECS state or change simulation behavior.

use crate::core::region_actor::RegionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPos {
    pub x: usize,
    pub y: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionBounds {
    pub id: RegionId,
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
}

impl RegionBounds {
    pub fn contains(self, position: GridPos) -> bool {
        position.x >= self.min_x
            && position.x < self.max_x
            && position.y >= self.min_y
            && position.y < self.max_y
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionPartition {
    map_width: usize,
    map_height: usize,
    region_width: usize,
    region_height: usize,
    columns: usize,
    rows: usize,
}

impl RegionPartition {
    pub fn new(
        map_width: usize,
        map_height: usize,
        region_width: usize,
        region_height: usize,
    ) -> Self {
        let region_width = region_width.max(1);
        let region_height = region_height.max(1);
        Self {
            map_width,
            map_height,
            region_width,
            region_height,
            columns: map_width.div_ceil(region_width),
            rows: map_height.div_ceil(region_height),
        }
    }

    pub fn region_count(&self) -> usize {
        self.columns * self.rows
    }

    pub fn region_ids(&self) -> impl Iterator<Item = RegionId> {
        (0..self.region_count()).map(|id| RegionId(id as u32))
    }

    pub fn region_for_cell(&self, position: GridPos) -> Option<RegionId> {
        if position.x >= self.map_width || position.y >= self.map_height {
            return None;
        }
        let column = position.x / self.region_width;
        let row = position.y / self.region_height;
        Some(self.region_id(column, row))
    }

    pub fn bounds(&self, region: RegionId) -> Option<RegionBounds> {
        let index = region.0 as usize;
        if index >= self.region_count() || self.columns == 0 {
            return None;
        }
        let column = index % self.columns;
        let row = index / self.columns;
        let min_x = column * self.region_width;
        let min_y = row * self.region_height;
        Some(RegionBounds {
            id: region,
            min_x,
            min_y,
            max_x: (min_x + self.region_width).min(self.map_width),
            max_y: (min_y + self.region_height).min(self.map_height),
        })
    }

    pub fn neighbors(&self, region: RegionId) -> Vec<RegionId> {
        let index = region.0 as usize;
        if index >= self.region_count() || self.columns == 0 {
            return Vec::new();
        }
        let column = index % self.columns;
        let row = index / self.columns;
        let mut neighbors = Vec::new();

        if column > 0 {
            neighbors.push(self.region_id(column - 1, row));
        }
        if column + 1 < self.columns {
            neighbors.push(self.region_id(column + 1, row));
        }
        if row > 0 {
            neighbors.push(self.region_id(column, row - 1));
        }
        if row + 1 < self.rows {
            neighbors.push(self.region_id(column, row + 1));
        }

        neighbors.sort();
        neighbors
    }

    fn region_id(&self, column: usize, row: usize) -> RegionId {
        RegionId((row * self.columns + column) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::{GridPos, RegionPartition};
    use crate::core::region_actor::RegionId;

    #[test]
    fn every_map_cell_maps_to_exactly_one_region() {
        let partition = RegionPartition::new(5, 4, 2, 2);

        for y in 0..4 {
            for x in 0..5 {
                let position = GridPos { x, y };
                let region = partition
                    .region_for_cell(position)
                    .expect("cell should have region");
                let bounds = partition.bounds(region).expect("region bounds");
                assert!(bounds.contains(position));
            }
        }
        assert_eq!(partition.region_for_cell(GridPos { x: 5, y: 0 }), None);
        assert_eq!(partition.region_for_cell(GridPos { x: 0, y: 4 }), None);
    }

    #[test]
    fn region_mapping_is_deterministic() {
        let first = RegionPartition::new(7, 5, 3, 2);
        let second = RegionPartition::new(7, 5, 3, 2);

        let first_mapping = cell_mapping(&first, 7, 5);
        let second_mapping = cell_mapping(&second, 7, 5);

        assert_eq!(first_mapping, second_mapping);
        assert_eq!(
            first.region_ids().collect::<Vec<_>>(),
            vec![
                RegionId(0),
                RegionId(1),
                RegionId(2),
                RegionId(3),
                RegionId(4),
                RegionId(5),
                RegionId(6),
                RegionId(7),
                RegionId(8)
            ]
        );
    }

    #[test]
    fn border_regions_identify_expected_neighbors() {
        let partition = RegionPartition::new(4, 4, 2, 2);

        assert_eq!(
            partition.neighbors(RegionId(0)),
            vec![RegionId(1), RegionId(2)]
        );
        assert_eq!(
            partition.neighbors(RegionId(1)),
            vec![RegionId(0), RegionId(3)]
        );
        assert_eq!(
            partition.neighbors(RegionId(2)),
            vec![RegionId(0), RegionId(3)]
        );
        assert_eq!(
            partition.neighbors(RegionId(3)),
            vec![RegionId(1), RegionId(2)]
        );
    }

    #[test]
    fn small_maps_still_produce_valid_regions() {
        let partition = RegionPartition::new(1, 1, 4, 4);

        assert_eq!(partition.region_count(), 1);
        assert_eq!(
            partition.region_for_cell(GridPos { x: 0, y: 0 }),
            Some(RegionId(0))
        );
        assert_eq!(partition.neighbors(RegionId(0)), Vec::<RegionId>::new());
        assert_eq!(
            partition.bounds(RegionId(0)),
            Some(super::RegionBounds {
                id: RegionId(0),
                min_x: 0,
                min_y: 0,
                max_x: 1,
                max_y: 1
            })
        );
    }

    fn cell_mapping(
        partition: &RegionPartition,
        width: usize,
        height: usize,
    ) -> Vec<Option<RegionId>> {
        let mut mapping = Vec::new();
        for y in 0..height {
            for x in 0..width {
                mapping.push(partition.region_for_cell(GridPos { x, y }));
            }
        }
        mapping
    }
}
