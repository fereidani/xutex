//! Pooled allocator for wait-queue structures.
//!
//! The pool stores *raw pointers* and never round-trips a recycled queue
//! through `Box::from_raw`/`Box::leak`: doing so would re-tag the
//! allocation and invalidate pointers that concurrent lockers loaded before
//! the queue was returned to the pool (the tag-CAS in the queue lock can
//! legitimately succeed on a recycled queue at the same address — an
//! ABA that is benign value-wise but must not invalidate provenance).
//! A queue's allocation therefore keeps its original borrow tag for as long
//! as it stays in the pool cycle; `Box::from_raw` is only used to actually
//! free a queue when the pool is full.

use crate::QueueStructure;
use alloc::boxed::Box;
#[cfg(not(loom))]
use crossbeam_queue::ArrayQueue;

#[cfg(all(feature = "std", not(loom)))]
use std::sync::OnceLock;

#[cfg(all(not(feature = "std"), not(loom)))]
use crate::oncelock::OnceLock;

#[cfg(not(loom))]
use crate::backoff::get_parallelism;

/// `Send` wrapper so raw queue pointers can live in the global pool.
#[cfg(not(loom))]
struct QueuePtr(*mut QueueStructure);
#[cfg(not(loom))]
unsafe impl Send for QueuePtr {}

#[cfg(not(loom))]
static QUEUE_ALLOCATOR: OnceLock<ArrayQueue<QueuePtr>> = OnceLock::new();

#[cfg(not(loom))]
fn get_queue_allocator() -> &'static ArrayQueue<QueuePtr> {
    QUEUE_ALLOCATOR.get_or_init(|| {
        let pool_cap = (get_parallelism() * 16).min(128);
        let queue = ArrayQueue::new(pool_cap);
        for _ in 0..pool_cap {
            let _ = queue.push(QueuePtr(Box::leak(Box::new(QueueStructure::new()))));
        }
        queue
    })
}

#[cfg(not(loom))]
#[inline(always)]
pub(crate) fn allocate_queue() -> *mut QueueStructure {
    match get_queue_allocator().pop() {
        Some(ptr) => ptr.0,
        None => Box::leak(Box::new(QueueStructure::new())),
    }
}

#[cfg(not(loom))]
#[inline(always)]
pub(crate) fn deallocate_queue(element: *mut QueueStructure) {
    if let Err(element) = get_queue_allocator().push(QueuePtr(element)) {
        // Pool full: actually free the queue.
        // SAFETY: the caller owns the queue exclusively and it was allocated
        // by `Box::new` above.
        unsafe { drop(Box::from_raw(element.0)) }
    }
}

// Under loom the global pool would leak loom-tracked objects across model
// iterations, so queues are allocated and freed directly.
#[cfg(loom)]
pub(crate) fn allocate_queue() -> *mut QueueStructure {
    Box::leak(Box::new(QueueStructure::new()))
}

#[cfg(loom)]
pub(crate) fn deallocate_queue(element: *mut QueueStructure) {
    // SAFETY: the caller owns the queue exclusively and it was allocated by
    // `Box::new` above.
    unsafe { drop(Box::from_raw(element)) }
}
