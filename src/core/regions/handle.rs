//! Stable thread-safe region mailbox handles.
//!
//! A `RegionHandle` is the sender endpoint that neighboring regions can keep.
//! The matching receiver is owned by the target `RegionRuntime` and moves with
//! that runtime when worker ownership changes in later patches.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::core::regions::RegionId;
use crate::core::regions::runtime::RegionEvent;

type EventQueue = Arc<Mutex<VecDeque<RegionEvent>>>;

#[derive(Debug, Clone)]
/// Cloneable sender endpoint for one region mailbox.
pub struct RegionHandle {
    region_id: RegionId,
    sender: RegionEventSender,
}

impl RegionHandle {
    pub fn region_id(&self) -> RegionId {
        self.region_id
    }

    pub fn send(&self, event: RegionEvent) {
        self.sender.send(event);
    }
}

/// In-process event sender shared by region handles.
#[derive(Debug, Clone)]
struct RegionEventSender {
    queue: EventQueue,
}

impl RegionEventSender {
    fn send(&self, event: RegionEvent) {
        self.queue
            .lock()
            .expect("region mailbox poisoned")
            .push_back(event);
    }
}

#[derive(Debug)]
/// Receiver side owned by exactly one `RegionRuntime`.
pub(crate) struct RegionEventReceiver {
    queue: EventQueue,
}

impl RegionEventReceiver {
    pub(crate) fn push_event(&mut self, event: RegionEvent) {
        self.queue
            .lock()
            .expect("region mailbox poisoned")
            .push_back(event);
    }

    pub(crate) fn pop_event(&mut self) -> Option<RegionEvent> {
        self.queue
            .lock()
            .expect("region mailbox poisoned")
            .pop_front()
    }

    pub(crate) fn pop_event_matching(
        &mut self,
        predicate: impl FnMut(&RegionEvent) -> bool,
    ) -> Option<RegionEvent> {
        let mut queue = self.queue.lock().expect("region mailbox poisoned");
        let position = queue.iter().position(predicate)?;
        queue.remove(position)
    }

    pub(crate) fn pending_event_count(&self) -> usize {
        self.queue.lock().expect("region mailbox poisoned").len()
    }
}

/// Creates a matched handle and receiver for one region runtime.
pub(crate) fn mailbox(region_id: RegionId) -> (RegionHandle, RegionEventReceiver) {
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let handle = RegionHandle {
        region_id,
        sender: RegionEventSender {
            queue: Arc::clone(&queue),
        },
    };
    let receiver = RegionEventReceiver { queue };

    (handle, receiver)
}
