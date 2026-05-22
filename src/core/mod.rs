//! Core simulation layer containing the custom ECS data model and deterministic systems.

pub(crate) mod actor_executor;
pub mod components;
pub mod entity;
pub mod game;
pub mod grid;
pub mod region;
pub mod region_actor;
pub mod region_promise;
pub mod resources;
pub mod systems;

pub(crate) mod world;
