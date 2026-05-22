//! Deterministic promise primitives for the region actor runtime prototype.
//!
//! Promise callbacks collect responses and produce resolved records. The actor runtime remains
//! responsible for turning those records into ordered local events that commit state.

use std::collections::BTreeMap;

use crate::core::region_actor::{RegionId, SimPhase, SimTick};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PromiseId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromiseResponse {
    pub promise_id: PromiseId,
    pub tick: SimTick,
    pub phase: SimPhase,
    pub dependency: RegionId,
    pub value: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseResolved {
    pub promise_id: PromiseId,
    pub ordered_dependencies: Vec<RegionId>,
    pub total: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseGroup {
    id: PromiseId,
    tick: SimTick,
    phase: SimPhase,
    required_dependencies: Vec<RegionId>,
    responses: BTreeMap<RegionId, i32>,
    resolved: bool,
}

impl PromiseGroup {
    pub fn new(
        id: PromiseId,
        tick: SimTick,
        phase: SimPhase,
        required_dependencies: impl IntoIterator<Item = RegionId>,
    ) -> Self {
        let mut required_dependencies: Vec<_> = required_dependencies.into_iter().collect();
        required_dependencies.sort();
        required_dependencies.dedup();
        Self {
            id,
            tick,
            phase,
            required_dependencies,
            responses: BTreeMap::new(),
            resolved: false,
        }
    }

    pub fn id(&self) -> PromiseId {
        self.id
    }

    pub fn record_response(&mut self, response: PromiseResponse) -> Option<PromiseResolved> {
        if self.resolved
            || response.promise_id != self.id
            || response.tick != self.tick
            || response.phase != self.phase
            || !self.required_dependencies.contains(&response.dependency)
        {
            return None;
        }
        self.responses
            .entry(response.dependency)
            .or_insert(response.value);

        if self.responses.len() != self.required_dependencies.len() {
            return None;
        }

        self.resolved = true;
        Some(PromiseResolved {
            promise_id: self.id,
            ordered_dependencies: self.required_dependencies.clone(),
            total: self
                .required_dependencies
                .iter()
                .filter_map(|dependency| self.responses.get(dependency))
                .sum(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseChain {
    id: PromiseId,
    tick: SimTick,
    phase: SimPhase,
    ordered_steps: Vec<RegionId>,
    pending_responses: BTreeMap<RegionId, i32>,
    resolved_dependencies: Vec<RegionId>,
    total: i32,
    resolved: bool,
}

impl PromiseChain {
    pub fn new(
        id: PromiseId,
        tick: SimTick,
        phase: SimPhase,
        ordered_steps: impl IntoIterator<Item = RegionId>,
    ) -> Self {
        Self {
            id,
            tick,
            phase,
            ordered_steps: ordered_steps.into_iter().collect(),
            pending_responses: BTreeMap::new(),
            resolved_dependencies: Vec::new(),
            total: 0,
            resolved: false,
        }
    }

    pub fn id(&self) -> PromiseId {
        self.id
    }

    pub fn record_response(&mut self, response: PromiseResponse) -> Option<PromiseResolved> {
        if self.resolved
            || response.promise_id != self.id
            || response.tick != self.tick
            || response.phase != self.phase
            || !self.ordered_steps.contains(&response.dependency)
        {
            return None;
        }
        self.pending_responses
            .entry(response.dependency)
            .or_insert(response.value);

        while let Some(next_dependency) = self.ordered_steps.get(self.resolved_dependencies.len()) {
            let Some(value) = self.pending_responses.remove(next_dependency) else {
                break;
            };
            self.total += value;
            self.resolved_dependencies.push(*next_dependency);
        }

        if self.resolved_dependencies.len() != self.ordered_steps.len() {
            return None;
        }

        self.resolved = true;
        Some(PromiseResolved {
            promise_id: self.id,
            ordered_dependencies: self.resolved_dependencies.clone(),
            total: self.total,
        })
    }
}
