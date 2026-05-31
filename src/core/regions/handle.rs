//! Stable in-process region mailbox handles.
//!
//! A `RegionHandle` is the sender endpoint that neighboring regions can keep.
//! The matching receiver is owned by the target `RegionRuntime` and moves with
//! that runtime when worker ownership changes in later patches.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::core::regions::RegionId;
use crate::core::regions::runtime::RegionEvent;

type EventQueue = Rc<RefCell<VecDeque<RegionEvent>>>;

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

#[derive(Debug, Clone)]
/// In-process event sender shared by region handles.
pub struct RegionEventSender {
    queue: EventQueue,
}

impl RegionEventSender {
    fn send(&self, event: RegionEvent) {
        self.queue.borrow_mut().push_back(event);
    }
}

#[derive(Debug)]
/// Receiver side owned by exactly one `RegionRuntime`.
pub struct RegionEventReceiver {
    region_id: RegionId,
    queue: EventQueue,
}

impl RegionEventReceiver {
    pub fn region_id(&self) -> RegionId {
        self.region_id
    }

    pub fn push_event(&mut self, event: RegionEvent) {
        self.queue.borrow_mut().push_back(event);
    }

    pub fn pop_event(&mut self) -> Option<RegionEvent> {
        self.queue.borrow_mut().pop_front()
    }

    pub fn pending_event_count(&self) -> usize {
        self.queue.borrow().len()
    }
}

/// Creates a matched handle and receiver for one region runtime.
pub fn mailbox(region_id: RegionId) -> (RegionHandle, RegionEventReceiver) {
    let queue = Rc::new(RefCell::new(VecDeque::new()));
    let handle = RegionHandle {
        region_id,
        sender: RegionEventSender {
            queue: Rc::clone(&queue),
        },
    };
    let receiver = RegionEventReceiver { region_id, queue };

    (handle, receiver)
}
