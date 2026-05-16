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

    fn index(&self, x: usize, y: usize) -> Option<usize> {
        self.contains(x, y).then_some(y * self.width + x)
    }
}
