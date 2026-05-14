# Codex Instructions

## Project

This is a minimal SimCity-like simulation game in Rust.

The core uses a custom minimal ECS.

## Hard Rules

- Do not expose ECS World to UI.
- UI must use Game API only.
- UI must render from GameView only.
- Keep systems deterministic.
- Prefer simple readable Rust over clever abstractions.
- Avoid external dependencies unless clearly necessary.
- Run cargo fmt and cargo test after changes.

## Architecture

Core:
- Entity
- Components
- World
- Grid
- Resources
- Systems
- Game API

Interface:
- View models
- Input enums
- Events
- View adapter

UI:
- ASCII terminal UI

## Testing

Every core rule should have a test.
Every UI boundary rule should have a test.
