//! Shared tagged-pointer wait-queue used by the semaphore, rwlock, notify,
//! barrier and once-cell primitives.
//!
//! This is the same allocation-free algorithm as the mutex in `lib.rs`:
//!
//! * The queue of waiters is an intrusive linked list of stack/future-owned
//!   [`Signal`] nodes, so enqueueing a waiter never allocates.
//! * The list head lives behind a single `AtomicPtr` with four states:
//!   - `null`     – no queue exists (no waiters),
//!   - `UPDATING` – a thread is currently allocating the queue,
//!   - `ptr`      – a published queue,
//!   - `ptr | 1`  – the queue is tag-locked; the tag owner has exclusive access
//!     to the queue contents.
//! * The queue structure itself is the only heap object and comes from the
//!   global pool in `allocator.rs`; it is returned to the pool as soon as the
//!   last waiter leaves, so an uncontended primitive owns no heap memory.
//!
//! Unlike the mutex, the pointer does not double as the lock state: primitives
//! built on `WaitQueue` keep their own state word (permit counter, generation
//! counter, …) and use the queue only to park waiters.

use core::ptr::NonNull;

use branches::unlikely;

#[cfg(any(feature = "std", loom))]
use crate::SIGNAL_INIT_WAITING;
#[cfg(not(loom))]
use crate::allocator::{allocate_queue, deallocate_queue};
#[cfg(not(loom))]
use crate::backoff::Backoff;
#[cfg(not(loom))]
use crate::shim::const_fn;
use crate::shim::spin::SpinAtomicPtr;
use crate::shim::sync::atomic::Ordering;
use crate::waker::WakerSlot;
use crate::{QueueStructure, Signal, SignalQueue};
#[cfg(not(loom))]
use crate::{UPDATING, tag_pointer, untag_pointer};

#[cfg(not(loom))]
const NULL: *mut QueueStructure = core::ptr::null_mut();

/// Re-reads a spin variable while waiting for a transient state to pass.
///
/// The `SpinAtomicPtr` wrapper makes this read `SeqCst` under loom so the
/// model checker always observes the latest value and the spin loop is
/// bounded; on real hardware it is a plain acquire load.
#[inline(always)]
pub(crate) fn spin_reload(ptr: &SpinAtomicPtr<QueueStructure>) -> *mut QueueStructure {
    ptr.load(Ordering::Acquire)
}

#[cfg(not(loom))]
pub(crate) struct WaitQueue {
    ptr: SpinAtomicPtr<QueueStructure>,
}

#[cfg(not(loom))]
impl WaitQueue {
    const_fn! {
        pub(crate) const fn new() -> Self {
            Self {
                ptr: SpinAtomicPtr::new(NULL),
            }
        }
    }

    /// Acquires exclusive (tag-locked) access to the wait queue.
    ///
    /// * If no queue exists and `alloc` is `false`, returns `None`.
    /// * If no queue exists and `alloc` is `true`, allocates one (from the
    ///   pool) and returns it in a locked state; it is only published to other
    ///   threads by [`LockedQueue::unlock`].
    /// * Otherwise spins until the tag-lock on the existing queue is acquired.
    #[inline]
    pub(crate) fn lock(&self, alloc: bool) -> Option<LockedQueue<'_>> {
        let backoff = Backoff::new();
        let mut ptr = self.ptr.load(Ordering::Acquire);
        loop {
            if ptr == NULL {
                if !alloc {
                    // A plain load may be stale under weak memory: an
                    // enqueuer locks the queue pointer *before* setting the
                    // caller-visible flag that led us here, so trusting a
                    // stale NULL would skip a waiter that is already
                    // published. An RMW reads the latest value in the
                    // modification order, making NULL authoritative:
                    // enqueuers touch this variable before setting their
                    // flag, so a true NULL here proves either that no waiter
                    // exists or that a late enqueuer's flag RMW will observe
                    // the caller's state update instead.
                    match self
                        .ptr
                        .compare_exchange(NULL, NULL, Ordering::AcqRel, Ordering::Acquire)
                    {
                        Ok(_) => return None,
                        Err(actual) => {
                            ptr = actual;
                            continue;
                        }
                    }
                }
                // Try to become the allocator of the queue.
                match self.ptr.compare_exchange(
                    NULL,
                    UPDATING,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        // Other threads spin while they observe UPDATING;
                        // the queue is unpublished so we have exclusive
                        // access.
                        return Some(LockedQueue {
                            owner: self,
                            queue: allocate_queue(),
                        });
                    }
                    Err(actual) => {
                        ptr = actual;
                        continue;
                    }
                }
            }

            if unlikely(ptr == UPDATING) {
                backoff.snooze();
                ptr = spin_reload(&self.ptr);
                continue;
            }

            let (untagged, is_tagged) = untag_pointer(ptr);
            if unlikely(is_tagged) {
                backoff.snooze();
                ptr = spin_reload(&self.ptr);
                continue;
            }

            match self.ptr.compare_exchange(
                untagged,
                tag_pointer(untagged),
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // A published queue pointer is always valid and the tag
                    // grants exclusive access to its contents. The pointer
                    // is kept raw: even if the queue was recycled through
                    // the pool between our load and the CAS (benign ABA),
                    // pool recycling preserves the allocation and its
                    // provenance.
                    return Some(LockedQueue {
                        owner: self,
                        queue: untagged,
                    });
                }
                Err(actual) => {
                    backoff.snooze();
                    ptr = actual;
                }
            }
        }
    }
}

/// Exclusive access to a tag-locked wait queue.
///
/// Must be released with [`unlock`](Self::unlock); there is deliberately no
/// `Drop` implementation because unlock decisions (deallocation, flag
/// clearing) are caller-specific and no panic can occur while it is held.
#[cfg(not(loom))]
#[must_use]
pub(crate) struct LockedQueue<'a> {
    owner: &'a WaitQueue,
    queue: *mut QueueStructure,
}

#[cfg(not(loom))]
impl<'a> LockedQueue<'a> {
    /// Runs `f` with exclusive access to the queue contents.
    #[inline(always)]
    pub(crate) fn with_queue<R>(&mut self, f: impl FnOnce(&mut SignalQueue) -> R) -> R {
        // SAFETY: holding the tag-lock (or the unpublished fresh queue) grants
        // exclusive access to the queue contents.
        unsafe { (*self.queue).inner.with_mut(|q| f(&mut *q)) }
    }

    /// Publishes the queue again (or releases it back to the pool when it is
    /// empty, maintaining the invariant that a published queue is never
    /// empty). `on_empty` runs right before the queue is unpublished so the
    /// caller can clear its "has queue" flag.
    #[inline]
    pub(crate) fn unlock(mut self, on_empty: impl FnOnce()) {
        let is_empty = self.with_queue(|q| q.is_empty());
        if unlikely(is_empty) {
            on_empty();
            self.owner.ptr.store(NULL, Ordering::Release);
            // The queue is empty, unpublished, and we are its sole owner:
            // nobody can hold a reference to it anymore.
            deallocate_queue(self.queue);
        } else {
            self.owner.ptr.store(self.queue, Ordering::Release);
        }
    }
}

/// Loom model of the wait queue.
///
/// Under loom the tag-lock *mechanism* (pointer tagging, UPDATING
/// allocation hand-off, pool recycling) is replaced by a loom-aware mutex:
/// loom cannot prove progress through spin loops when two threads spin
/// simultaneously, and the mechanism itself is covered by miri and the
/// native stress tests. What loom then exhaustively verifies is everything
/// built *on top*: the state-word protocols (permit pool flags, generation
/// counters, once-cell states), the queue-vs-state ordering that prevents
/// lost wakeups, signal handoff, and cancellation.
#[cfg(loom)]
pub(crate) struct WaitQueue {
    inner: loom::sync::Mutex<SignalQueue>,
}

#[cfg(loom)]
impl WaitQueue {
    pub(crate) fn new() -> Self {
        Self {
            inner: loom::sync::Mutex::new(SignalQueue::new()),
        }
    }

    pub(crate) fn lock(&self, _alloc: bool) -> Option<LockedQueue<'_>> {
        Some(LockedQueue {
            guard: self.inner.lock().unwrap(),
        })
    }
}

#[cfg(loom)]
#[must_use]
pub(crate) struct LockedQueue<'a> {
    guard: loom::sync::MutexGuard<'a, SignalQueue>,
}

#[cfg(loom)]
impl LockedQueue<'_> {
    pub(crate) fn with_queue<R>(&mut self, f: impl FnOnce(&mut SignalQueue) -> R) -> R {
        f(&mut self.guard)
    }

    /// Mirrors the real unlock contract: `on_empty` runs when no waiter is
    /// queued so the caller clears its "has queue" flag.
    pub(crate) fn unlock(mut self, on_empty: impl FnOnce()) {
        if self.with_queue(|q| q.is_empty()) {
            on_empty();
        }
    }
}

/// A FIFO chain of signals popped from a queue, to be woken after the
/// tag-lock is released.
///
/// Consecutively popped nodes are already linked through their `next`
/// pointers, so collecting them is free; [`seal`](Self::seal) cuts the link
/// from the last popped node to any node still in the queue.
pub(crate) struct PoppedChain {
    head: Option<NonNull<Signal>>,
    last: Option<NonNull<Signal>>,
}

impl PoppedChain {
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self {
            head: None,
            last: None,
        }
    }

    /// Appends a node that was just popped from the queue.
    ///
    /// # Safety
    ///
    /// `node` must have been popped from the same queue immediately after the
    /// previously appended node (FIFO pop order keeps the intrusive links
    /// intact).
    #[inline(always)]
    pub(crate) unsafe fn append(&mut self, node: NonNull<Signal>) {
        if self.head.is_none() {
            self.head = Some(node);
        }
        self.last = Some(node);
    }

    /// Terminates the chain. Must be called after the last `append` and
    /// while the queue tag-lock is still held.
    #[inline(always)]
    pub(crate) fn seal(&mut self) {
        if let Some(mut last) = self.last {
            // SAFETY: the node was popped under the tag-lock and has not been
            // signaled yet, so we still own it exclusively.
            unsafe { last.as_mut().next = None };
        }
    }

    /// Signals every node in the chain with `value`, front to back.
    ///
    /// Reads each node's `next` pointer *before* signaling it: signaling
    /// transfers ownership to the waiter, which may free the node right away.
    #[inline]
    pub(crate) fn signal_all(self, value: usize) {
        let mut cur = self.head;
        while let Some(node) = cur {
            // SAFETY: nodes in the chain are owned by us until signaled.
            cur = unsafe { node.as_ref().next };
            // SAFETY: see above; this is the last access to the node.
            unsafe { signal_node(node, value) };
        }
    }
}

/// Signals a single waiter node with `value` and wakes it.
///
/// The waker is taken *first*: after the waiter observes `value` it may
/// return/free its `Signal` at any moment, so nothing may touch the node
/// after the value store + wake.
///
/// # Safety
///
/// The caller must own the node (popped from a queue under the tag-lock) and
/// must not access it again afterwards.
pub(crate) unsafe fn signal_node(node: NonNull<Signal>, value: usize) {
    // SAFETY: the caller guarantees the node is alive and exclusively ours.
    let entry_ref = unsafe { node.as_ref() };
    let waker = entry_ref.waker.take();
    match waker {
        WakerSlot::None => {
            // The waiter is mid-cancellation and spinning on `value` (it
            // never registered a waker, or the waker was already consumed);
            // publishing the value alone releases it.
            entry_ref.value.store(value, Ordering::Release);
        }
        #[cfg(any(feature = "std", loom))]
        WakerSlot::Sync(thread) => {
            if entry_ref.value.swap(value, Ordering::AcqRel) == SIGNAL_INIT_WAITING {
                thread.unpark();
            }
        }
        WakerSlot::Async(waker) => {
            entry_ref.value.store(value, Ordering::Release);
            waker.wake();
        }
    }
}
