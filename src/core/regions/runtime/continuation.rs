//! Opaque caller-owned continuations for cross-region follow-up work.
//!
//! A neighboring region can carry this handle and return it with a result, but
//! only the regional runtime can execute it against the caller's state.

use crate::core::regions::{RegionId, RegionState};

type ContinuationApply<R> = Box<dyn FnOnce(&mut RegionState, R) + Send + 'static>;

/// Caller-owned follow-up work that must run in the caller region runtime.
pub struct CallerContinuation<R> {
    caller_region: RegionId,
    apply: ContinuationApply<R>,
}

impl<R> CallerContinuation<R> {
    /// Creates an opaque continuation for a specific caller region.
    pub fn new(
        caller_region: RegionId,
        apply: impl FnOnce(&mut RegionState, R) + Send + 'static,
    ) -> Self {
        Self {
            caller_region,
            apply: Box::new(apply),
        }
    }

    pub fn caller_region(&self) -> RegionId {
        self.caller_region
    }

    pub(super) fn run(self, region: &mut RegionState, result: R) {
        (self.apply)(region, result);
    }
}

impl<R> std::fmt::Debug for CallerContinuation<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallerContinuation")
            .field("caller_region", &self.caller_region)
            .finish_non_exhaustive()
    }
}

/// Neighbor work request carrying payload plus an opaque caller continuation.
#[derive(Debug)]
pub struct NeighborRequest<P, R> {
    pub payload: P,
    pub continuation: CallerContinuation<R>,
}
