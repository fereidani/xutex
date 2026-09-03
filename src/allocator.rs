//! Queue allocation: delegates to the global pool in the [`xutex_pool`]
//! crate.
//!
//! `xutex-pool` is semver-frozen at `1.x`, so every `xutex` version in a
//! dependency graph resolves to the same copy and the binary holds exactly
//! one queue pool, even when dependants use incompatible `xutex` versions.
//! The queue structure lives there too (generic over the version-private
//! [`Signal`](crate::Signal) node), so recycled queues need no conversion.

use crate::QueueStructure;
#[cfg(loom)]
use alloc::boxed::Box;

#[cfg(not(loom))]
#[inline(always)]
pub(crate) fn allocate_queue() -> *mut QueueStructure {
    xutex_pool::allocate_queue()
}

#[cfg(not(loom))]
#[inline(always)]
pub(crate) fn deallocate_queue(element: *mut QueueStructure) {
    // SAFETY: callers own the queue exclusively and only ever deallocate an
    // empty queue, as the pool requires.
    unsafe { xutex_pool::deallocate_queue(element) }
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
