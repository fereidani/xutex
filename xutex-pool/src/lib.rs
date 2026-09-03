//! Wait-queue structure and global queue pool shared by all versions of
//! [`xutex`](https://crates.io/crates/xutex).
//!
//! Every `xutex` version depends on `xutex-pool = "1"`, so cargo resolves a
//! single copy of this crate — and therefore a single pool — per binary, even
//! when dependants use semver-incompatible `xutex` versions. If the pool
//! lived in `xutex` itself, each version would hold its own.
//!
//! Node contents differ between `xutex` versions, so the queue stores nodes
//! type-erased (`*mut ()`) and only its methods are generic over the node
//! type ([`Node`]), touching nothing but the intrusive `next` link. The
//! structure itself is one concrete two-pointer type, and queues are only
//! pooled while empty — which is what makes recycling them across versions
//! sound.
//!
//! # Semver contract
//!
//! **Never release a semver-breaking version of this crate**: a `2.0.0` would
//! split the pool again. The [`QueueStructure`] layout and the queue
//! algorithm are frozen; any change must be additive.
//!
//! # Provenance
//!
//! The pool stores raw pointers and never frees a queue: once allocated, a
//! queue's allocation (and with it the provenance of every pointer to it)
//! lives for the rest of the process. That is what keeps the tag-CAS of the
//! queue lock sound: a locker that loaded a queue pointer before the queue
//! was returned to the pool can still see its CAS succeed after the queue
//! has been recycled at the same address (a benign ABA), and it then touches
//! a live allocation through a pointer that still carries that allocation's
//! provenance. Freeing a queue and reallocating at the same address would
//! break exactly that guarantee, so the pool is unbounded and its footprint
//! is the peak number of simultaneously contended primitives, two pointer
//! words each.

#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::boxed::Box;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicPtr, Ordering};
use crossbeam_queue::SegQueue;

/// An intrusive wait-queue node: linked through a `next` pointer stored in
/// the node itself, so enqueueing never allocates. Everything beyond the link
/// is opaque to this crate.
///
/// # Safety
///
/// `get_next`/`set_next` must faithfully read/write a dedicated
/// `Option<NonNull<Self>>` link field and touch nothing else. While a node is
/// enqueued, the link belongs to the queue.
pub unsafe trait Node: Sized {
    /// Reads the node's `next` link.
    ///
    /// # Safety
    ///
    /// `node` must be live and the caller must have exclusive access to its
    /// link (hold the queue lock).
    unsafe fn get_next(node: NonNull<Self>) -> Option<NonNull<Self>>;

    /// Writes the node's `next` link.
    ///
    /// # Safety
    ///
    /// Same requirements as [`get_next`](Node::get_next).
    unsafe fn set_next(node: NonNull<Self>, next: Option<NonNull<Self>>);
}

#[inline(always)]
fn erase<N>(ptr: NonNull<N>) -> *mut () {
    ptr.as_ptr().cast()
}

#[inline(always)]
fn typed<N>(ptr: *mut ()) -> Option<NonNull<N>> {
    NonNull::new(ptr.cast())
}

/// An intrusive FIFO queue of waiter nodes, stored type-erased.
///
/// `#[repr(C)]`, two pointer words, frozen — pooled queues are recycled
/// across `xutex` versions. A queue instance must be used with a single node
/// type `N` across all operations; this is part of [`push`](Self::push)'s
/// safety contract.
#[repr(C)]
pub struct SignalQueue {
    first: *mut (),
    last: *mut (),
}

unsafe impl Send for SignalQueue {}

impl SignalQueue {
    /// Creates a new empty queue.
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            first: null_mut(),
            last: null_mut(),
        }
    }

    /// Pushes a node onto the back of the queue.
    /// Returns true if the queue was previously empty as a hint for spinning.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the node lives long enough in the queue or
    /// is removed from the queue on drop, must guarantee the node's `next`
    /// link is `None`, and must use the same `N` for every operation on this
    /// queue.
    #[inline(always)]
    pub unsafe fn push<N: Node>(&mut self, entry: NonNull<N>) -> bool {
        let old = typed::<N>(self.last);
        self.last = erase(entry);
        match old {
            Some(old) => {
                // SAFETY: self.last was guaranteed to be valid
                unsafe {
                    N::set_next(old, Some(entry));
                }
                false
            }
            None => {
                self.first = erase(entry);
                true
            }
        }
    }

    /// Returns the node at the front of the queue without removing it.
    #[inline(always)]
    pub fn first<N: Node>(&self) -> Option<NonNull<N>> {
        typed(self.first)
    }

    /// Returns `true` when no node is queued.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.first.is_null()
    }

    /// Pops a node from the front of the queue.
    ///
    /// # Safety
    ///
    /// Every node in the queue must have been pushed as an `N` (see
    /// [`push`](Self::push)) and must still be alive.
    #[inline(always)]
    pub unsafe fn pop<N: Node>(&mut self) -> Option<NonNull<N>> {
        // Take the first element; return None if the queue is empty.
        let first = typed::<N>(self.first)?;
        // SAFETY: `first` is a valid node because it came from `push`.
        let next = unsafe { N::get_next(first) };
        match next {
            Some(next) => {
                // There is a next element; update the head of the queue.
                self.first = erase(next);
            }
            None => {
                // Queue becomes empty; clear both pointers.
                self.first = null_mut();
                self.last = null_mut();
            }
        }
        Some(first)
    }

    /// Removes a specific node from the queue.
    /// Returns true if the node was found and removed, false otherwise.
    ///
    /// # Safety
    ///
    /// Same requirements as [`pop`](Self::pop); `entry` is only compared by
    /// address and need not be queued.
    #[inline(always)]
    pub unsafe fn remove<N: Node>(&mut self, entry: NonNull<N>) -> bool {
        let mut cur = typed::<N>(self.first);
        let mut prev: Option<NonNull<N>> = None;
        while let Some(cur_ptr) = cur {
            if cur_ptr == entry {
                // SAFETY: nodes reachable from the queue are valid and the
                // caller holds the queue lock.
                unsafe {
                    let next = N::get_next(cur_ptr);
                    match prev {
                        Some(prev) => N::set_next(prev, next),
                        None => self.first = next.map_or(null_mut(), erase),
                    }
                }
                if self.last == erase(entry) {
                    self.last = prev.map_or(null_mut(), erase);
                }
                return true;
            }
            prev = Some(cur_ptr);
            // SAFETY: current is not null and guaranteed to be valid
            cur = unsafe { N::get_next(cur_ptr) };
        }
        false
    }
}

impl Default for SignalQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The pooled heap object: a [`SignalQueue`] behind an `UnsafeCell` so the
/// owner of the consumer-side tag-lock can access it through a shared
/// pointer.
///
/// `#[repr(C)]`; layout is the queue's two pointer words, frozen forever.
#[repr(C)]
pub struct QueueStructure {
    inner: core::cell::UnsafeCell<SignalQueue>,
}

impl QueueStructure {
    /// Creates a new structure holding an empty queue.
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            inner: core::cell::UnsafeCell::new(SignalQueue::new()),
        }
    }

    /// Runs `f` with an exclusive raw pointer to the queue contents.
    ///
    /// # Safety
    ///
    /// The caller must have exclusive access to the queue contents for the
    /// duration of the call (hold the tag-lock), exactly like dereferencing
    /// `core::cell::UnsafeCell::get`.
    #[inline(always)]
    pub unsafe fn with_mut<R>(&self, f: impl FnOnce(*mut SignalQueue) -> R) -> R {
        f(self.inner.get())
    }
}

impl Default for QueueStructure {
    fn default() -> Self {
        Self::new()
    }
}

/// `Send` wrapper so raw queue pointers can live in the global pool.
struct QueuePtr(*mut QueueStructure);
unsafe impl Send for QueuePtr {}

static POOL: AtomicPtr<SegQueue<QueuePtr>> = AtomicPtr::new(null_mut());

#[inline]
fn pool() -> &'static SegQueue<QueuePtr> {
    let ptr = POOL.load(Ordering::Acquire);
    if !ptr.is_null() {
        // SAFETY: a published pool is never unpublished or freed.
        return unsafe { &*ptr };
    }
    init_pool()
}

/// Builds and publishes the pool; on losing the publication race, frees the
/// local build and returns the winner's pool.
#[cold]
fn init_pool() -> &'static SegQueue<QueuePtr> {
    #[cfg(feature = "std")]
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    #[cfg(not(feature = "std"))]
    let parallelism = 1;

    // Pre-fill so the first contended primitives find a queue ready; the pool
    // grows on demand beyond this.
    let prefill = (parallelism * 16).min(128);
    let queue = SegQueue::new();
    for _ in 0..prefill {
        queue.push(QueuePtr(Box::leak(Box::new(QueueStructure::new()))));
    }
    let fresh = Box::into_raw(Box::new(queue));
    match POOL.compare_exchange(null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: just published, never freed.
        Ok(_) => unsafe { &*fresh },
        Err(existing) => {
            // Lost the race: free the local build, queues included.
            // SAFETY: `fresh` was never published, so it is exclusively ours.
            let lost = unsafe { Box::from_raw(fresh) };
            while let Some(queue) = lost.pop() {
                // SAFETY: leaked above and never published.
                drop(unsafe { Box::from_raw(queue.0) });
            }
            // SAFETY: a published pool is never unpublished or freed.
            unsafe { &*existing }
        }
    }
}

/// Takes an empty queue from the global pool, or allocates a fresh one.
///
/// The queue is valid, empty, and exclusively owned by the caller until
/// passed to [`deallocate_queue`]. Its allocation may be recycled from a
/// different `xutex` version; queues are pooled only while empty, so no
/// version-specific state crosses over.
#[inline(always)]
pub fn allocate_queue() -> *mut QueueStructure {
    match pool().pop() {
        Some(ptr) => ptr.0,
        None => Box::leak(Box::new(QueueStructure::new())),
    }
}

/// Returns a queue to the global pool.
///
/// Pooled queues are never freed (see the crate docs on provenance); the
/// pool simply grows to the peak number of queues ever in use at once.
///
/// # Safety
///
/// `queue` must come from [`allocate_queue`], be exclusively owned, be empty
/// (leftover node pointers would dangle into a foreign version's waiters),
/// and not be accessed through this pointer afterwards.
#[inline(always)]
pub unsafe fn deallocate_queue(queue: *mut QueueStructure) {
    pool().push(QueuePtr(queue));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestNode {
        next: Option<NonNull<TestNode>>,
        value: u32,
    }

    unsafe impl Node for TestNode {
        unsafe fn get_next(node: NonNull<Self>) -> Option<NonNull<Self>> {
            unsafe { node.as_ref().next }
        }
        unsafe fn set_next(mut node: NonNull<Self>, next: Option<NonNull<Self>>) {
            unsafe { node.as_mut().next = next }
        }
    }

    fn node(value: u32) -> TestNode {
        TestNode { next: None, value }
    }

    fn value_of(node: Option<NonNull<TestNode>>) -> u32 {
        unsafe { node.unwrap().as_ref().value }
    }

    #[test]
    fn queue_fifo_and_remove() {
        let mut a = node(1);
        let mut b = node(2);
        let mut c = node(3);
        // Reuse each pointer for push and remove: a second `&mut` would
        // invalidate the tag held by the queue links.
        let (a, b, c) = (
            NonNull::from(&mut a),
            NonNull::from(&mut b),
            NonNull::from(&mut c),
        );
        let mut q = SignalQueue::new();
        assert!(q.is_empty());
        unsafe {
            assert!(q.push(a));
            assert!(!q.push(b));
            assert!(!q.push(c));
        }
        assert_eq!(value_of(q.first()), 1);
        unsafe {
            assert!(q.remove(b));
            assert!(!q.remove(b));
            assert_eq!(value_of(q.pop()), 1);
            assert_eq!(value_of(q.pop()), 3);
            assert!(q.pop::<TestNode>().is_none());
        }
        assert!(q.is_empty());
    }

    #[test]
    fn allocate_recycle_roundtrip() {
        let q = allocate_queue();
        let mut a = node(7);
        unsafe {
            // The queue must arrive empty and be usable immediately.
            (*q).with_mut(|queue| {
                assert!((*queue).is_empty());
                (*queue).push(NonNull::from(&mut a));
                assert_eq!(value_of((*queue).pop()), 7);
            });
            deallocate_queue(q);
        }
    }

    #[test]
    fn pool_grows_past_the_prefill() {
        // Drain past the pre-filled queues to hit the fresh-allocation path,
        // return everything, and check the grown pool hands them out again.
        let queues: alloc::vec::Vec<*mut QueueStructure> =
            (0..256).map(|_| allocate_queue()).collect();
        for queue in queues {
            unsafe { deallocate_queue(queue) };
        }
        let again: alloc::vec::Vec<*mut QueueStructure> =
            (0..256).map(|_| allocate_queue()).collect();
        for queue in again {
            unsafe { deallocate_queue(queue) };
        }
    }
}
