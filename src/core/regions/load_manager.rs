//! Deterministic load movement policy for regional workers.
//!
//! The load manager only reads worker-level summaries such as region count,
//! queued event count, and optional frame time. It does not inspect region ECS
//! state and is not connected to normal message routing.

use std::cmp::Ordering;

use crate::core::regions::RegionId;
use crate::core::regions::worker::WorkerId;

/// Frame time is bucketed to milliseconds so raw microseconds do not overwhelm
/// region count and queued-event signals.
const FRAME_TIME_BUCKET_MICROS: u64 = 1_000;
/// Do not propose moving the last region away from a worker.
const MIN_SOURCE_REGIONS_FOR_MOVE: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Routing-safe summary of one worker's scheduling load.
pub struct WorkerLoad {
    pub worker_id: WorkerId,
    pub region_count: usize,
    pub queued_events: usize,
    pub frame_time_micros: Option<u64>,
    pub region_ids: Vec<RegionId>,
}

impl WorkerLoad {
    pub fn new(worker_id: WorkerId, mut region_ids: Vec<RegionId>, queued_events: usize) -> Self {
        region_ids.sort();
        Self {
            worker_id,
            region_count: region_ids.len(),
            queued_events,
            frame_time_micros: None,
            region_ids,
        }
    }

    pub fn with_frame_time_micros(mut self, frame_time_micros: u64) -> Self {
        self.frame_time_micros = Some(frame_time_micros);
        self
    }

    fn score(&self) -> u64 {
        self.region_count as u64 + self.queued_events as u64 + self.frame_time_bucket()
    }

    fn frame_time_bucket(&self) -> u64 {
        self.frame_time_micros.unwrap_or_default() / FRAME_TIME_BUCKET_MICROS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One safe-point reassignment proposed by the load manager.
pub struct RegionMove {
    pub region_id: RegionId,
    pub from_worker: WorkerId,
    pub to_worker: WorkerId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic policy for deciding whether to move one region.
pub struct LoadManager {
    min_load_gap: u64,
}

impl LoadManager {
    pub fn new(min_load_gap: u64) -> Self {
        Self { min_load_gap }
    }

    pub fn choose_move(&self, loads: &[WorkerLoad]) -> Option<RegionMove> {
        let source = loads
            .iter()
            .filter(|load| load.region_count >= MIN_SOURCE_REGIONS_FOR_MOVE)
            .min_by(|left, right| compare_source(left, right))?;

        let target = loads
            .iter()
            .filter(|load| load.worker_id != source.worker_id)
            .min_by(|left, right| compare_target(left, right))?;

        if source.score().saturating_sub(target.score()) < self.min_load_gap {
            return None;
        }

        // TODO(multi-worker): When this is wired to a scheduler, add a post-move
        // balance check so repeated calls cannot oscillate a region between workers.
        // Tracked in docs/regional-multi-worker-plan.md (M6).
        Some(RegionMove {
            region_id: *source.region_ids.first()?,
            from_worker: source.worker_id,
            to_worker: target.worker_id,
        })
    }
}

fn compare_source(left: &WorkerLoad, right: &WorkerLoad) -> Ordering {
    right
        .score()
        .cmp(&left.score())
        .then_with(|| right.region_count.cmp(&left.region_count))
        .then_with(|| left.worker_id.cmp(&right.worker_id))
}

fn compare_target(left: &WorkerLoad, right: &WorkerLoad) -> Ordering {
    left.score()
        .cmp(&right.score())
        .then_with(|| left.region_count.cmp(&right.region_count))
        .then_with(|| left.worker_id.cmp(&right.worker_id))
}
