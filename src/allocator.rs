use crate::{QueueStructure, backoff::get_parallelism};
use alloc::boxed::Box;
use crossbeam_queue::ArrayQueue;

#[cfg(feature = "std")]
use std::sync::OnceLock;

#[cfg(not(feature = "std"))]
use crate::oncelock::OnceLock;

static QUEUE_ALLOCATOR: OnceLock<ArrayQueue<Box<QueueStructure>>> = OnceLock::new();

fn get_queue_allocator() -> &'static ArrayQueue<Box<QueueStructure>> {
    QUEUE_ALLOCATOR.get_or_init(|| {
        let pool_cap = (get_parallelism() * 16).min(128);
        let queue = ArrayQueue::new(pool_cap);
        for _ in 0..pool_cap {
            let _ = queue.push(Box::new(QueueStructure::new()));
        }
        queue
    })
}

#[inline(always)]
pub(crate) fn allocate_queue() -> Box<QueueStructure> {
    get_queue_allocator()
        .pop()
        .unwrap_or_else(|| Box::new(QueueStructure::new()))
}

#[inline(always)]
pub(crate) fn deallocate_queue(element: Box<QueueStructure>) {
    _ = get_queue_allocator().push(element)
}
