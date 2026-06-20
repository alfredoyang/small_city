//! Fixed-size map grid that stores only cell-occupying building entity IDs.

use serde::{Deserialize, Serialize};

use crate::core::entity::Entity;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Option<Entity>>,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![None; width.saturating_mul(height)],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn contains(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.height
    }

    pub fn get(&self, x: usize, y: usize) -> Option<Entity> {
        self.index(x, y).and_then(|index| self.cells[index])
    }

    pub fn set(&mut self, x: usize, y: usize, entity: Entity) -> bool {
        if let Some(index) = self.index(x, y) {
            self.cells[index] = Some(entity);
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self, x: usize, y: usize) -> Option<Entity> {
        self.index(x, y).and_then(|index| self.cells[index].take())
    }

    /// Clears every cell of a `width` x `height` rectangle anchored at `(x, y)`. Used to remove a
    /// multi-cell building from all the cells it occupies. Out-of-bounds cells are skipped.
    pub fn clear_footprint(&mut self, x: usize, y: usize, width: usize, height: usize) {
        for cy in y..y.saturating_add(height) {
            for cx in x..x.saturating_add(width) {
                if let Some(index) = self.index(cx, cy) {
                    self.cells[index] = None;
                }
            }
        }
    }

    /// Writes `entity` into every cell of a `width` x `height` rectangle anchored at `(x, y)`. Used
    /// when a building grows so all the cells it now occupies map back to it. Out-of-bounds cells
    /// are skipped.
    pub fn set_footprint(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        entity: Entity,
    ) {
        for cy in y..y.saturating_add(height) {
            for cx in x..x.saturating_add(width) {
                if let Some(index) = self.index(cx, cy) {
                    self.cells[index] = Some(entity);
                }
            }
        }
    }

    fn index(&self, x: usize, y: usize) -> Option<usize> {
        self.contains(x, y).then_some(y * self.width + x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_footprint_clears_the_whole_rectangle_and_nothing_else() {
        let mut grid = Grid::new(4, 4);
        let building = Entity(1);
        for &(x, y) in &[(1, 1), (2, 1), (1, 2), (2, 2)] {
            grid.set(x, y, building);
        }
        grid.set(0, 0, Entity(2)); // outside the footprint

        grid.clear_footprint(1, 1, 2, 2);

        for &(x, y) in &[(1, 1), (2, 1), (1, 2), (2, 2)] {
            assert_eq!(grid.get(x, y), None);
        }
        assert_eq!(grid.get(0, 0), Some(Entity(2)), "neighbours are untouched");
    }
}
