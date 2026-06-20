//! Core simulation layer containing the custom ECS data model and deterministic systems.

pub mod building_rules;
pub mod building_stats;
pub mod components;
pub mod entity;
pub mod grid;
pub mod regional_game;
pub mod regional_game_runner;
pub mod regional_types;
pub mod regions;
pub(crate) mod resource_registry;
pub mod resources;
pub(crate) mod simulation;
pub mod systems;

pub(crate) mod world;
