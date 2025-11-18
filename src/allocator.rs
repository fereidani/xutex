use crate::{QueueStructure, SignalQueue};
use crossbeam_queue::ArrayQueue;

const QUEUE_POOL_CAPACITY: usize = 128;

static QUEUE_ALLOCATOR: std::sync::OnceLock<ArrayQueue<Box<QueueStructure>>> =
    std::sync::OnceLock::new();

fn get_queue_allocator() -> &'static ArrayQueue<Box<QueueStructure>> {
    QUEUE_ALLOCATOR.get_or_init(|| {
        let queue = ArrayQueue::new(QUEUE_POOL_CAPACITY);
        for _ in 0..QUEUE_POOL_CAPACITY {
            let _ = queue.push(Box::new(std::sync::Mutex::new(SignalQueue::new())));
        }
        queue
    })
}

#[inline(always)]
pub(crate) fn allocate_queue() -> Box<QueueStructure> {
    get_queue_allocator()
        .pop()
        .unwrap_or_else(|| Box::new(std::sync::Mutex::new(SignalQueue::new())))
}

#[inline(always)]
pub(crate) fn deallocate_queue(element: Box<QueueStructure>) {
    _ = get_queue_allocator().push(element)
}
