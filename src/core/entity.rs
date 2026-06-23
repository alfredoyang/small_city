//! Stable entity identifier type used as keys into ECS component storage.

use serde::{Deserialize, Serialize};

// `PartialOrd, Ord` make `Entity` sortable so the city-wide reference types in
// `city_refs` (and their maps/sorts) can derive `Ord` deterministically; it is a
// plain `u32` newtype, so the ordering is just the numeric id order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Entity(pub u32);
