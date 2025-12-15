#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

extern crate alloc;
use alloc::boxed::Box;
#[cfg(feature = "std")]
use alloc::sync::Arc;
use core::ptr::NonNull;
use core::{
    cell::UnsafeCell,
    marker::PhantomPinned,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
    task::Poll,
};

mod allocator;
mod backoff;
#[cfg(not(feature = "std"))]
mod oncelock;
mod signal_queue;
mod waker;
pub(crate) use signal_queue::SignalQueue;

use crate::{
    allocator::{allocate_queue, deallocate_queue},
    waker::{DynamicWaker, WakerSlot},
};
use backoff::Backoff;
use branches::{likely, unlikely};

#[repr(C)]
pub(crate) struct Signal {
    next: Option<NonNull<Signal>>,
    value: AtomicUsize,
    waker: DynamicWaker,
    _pinned: PhantomPinned,
}

impl Signal {
    pub fn new_none() -> Self {
        Self {
            next: None,
            value: AtomicUsize::new(SIGNAL_UNINIT),
            waker: DynamicWaker::new(),
            _pinned: PhantomPinned,
        }
    }
    #[cfg(feature = "std")]
    pub fn new_sync() -> Self {
        Self {
            next: None,
            value: AtomicUsize::new(SIGNAL_UNINIT),
            waker: DynamicWaker::new_sync(),
            _pinned: PhantomPinned,
        }
    }
}

struct QueueStructure {
    pub inner: UnsafeCell<SignalQueue>,
}

impl QueueStructure {
    const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(SignalQueue::new()),
        }
    }
}

const LOCKED: *mut QueueStructure = usize::MAX as *mut _;
const UNLOCKED: *mut QueueStructure = core::ptr::null_mut();
const UPDATING: *mut QueueStructure = LOCKED.wrapping_sub(1);

#[inline(always)]
fn tag_pointer<T>(ptr: *mut T) -> *mut T {
    debug_assert!(
        (ptr.addr() & 1) == 0,
        "Tagging pointer failed due to target memory alignment"
    );
    ptr.map_addr(|addr| addr | 1).cast()
}

#[inline(always)]
fn untag_pointer<T>(ptr: *mut T) -> (*mut T, bool) {
    let is_tagged = (ptr.addr() & 1) == 1;
    let untagged = ptr.map_addr(|addr| addr & !1).cast::<T>();
    (untagged, is_tagged)
}

#[repr(C)]
pub(crate) struct MutexInternal<T> {
    queue: AtomicPtr<QueueStructure>,
    inner: UnsafeCell<T>,
}

impl<T> MutexInternal<T> {
    pub(crate) const fn new(data: T) -> Self {
        Self {
            inner: UnsafeCell::new(data),
            queue: AtomicPtr::new(UNLOCKED),
        }
    }

    #[inline(always)]
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if likely(
            self.queue
                .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
        ) {
            Some(MutexGuard { mutex: self })
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) unsafe fn create_guard(&self) -> MutexGuard<'_, T> {
        MutexGuard { mutex: self }
    }
}

/// A future that represents a pending asynchronous lock acquisition request.
///
/// This struct is returned by [`AsyncMutex::lock()`] and
/// [`Mutex::lock_async()`] and implements [`Future`] to provide async/await
/// support for mutex locking. When polled, it will either immediately return a
/// [`MutexGuard`] if the lock is available, or register itself in the mutex's
/// wait queue and return [`Poll::Pending`] until the lock becomes available.
///
/// ## Pinning Requirements
///
/// This type contains [`PhantomPinned`] and is `!Unpin`, meaning it cannot be
/// moved once pinned. This is necessary because the struct contains a
/// [`Signal`] that may be linked into the mutex's wait queue, and moving it
/// would invalidate the queue pointers.
///
/// ## Memory Safety
///
/// The struct maintains a reference to the mutex and embeds a [`Signal`] entry
/// that may be inserted into the mutex's internal wait queue. If the future is
/// dropped while waiting, it will automatically remove itself from the queue
/// or wait synchronously for the lock to avoid use-after-free issues.
///
/// ## Cancellation Safety
///
/// This future handles cancellation (dropping while pending) safely by either:
/// 1. Successfully removing itself from the wait queue, or
/// 2. Waiting synchronously for the lock signal and immediately dropping the
///    guard
///
/// This ensures that no pointers to the dropped future remain in the queue.
///
/// ## Performance Notes
///
/// - **Fast Path**: If the mutex is unlocked, returns immediately without
///   allocation
/// - **Contended Path**: Allocates a wait queue if needed and registers the
///   request
/// - **Cancellation**: Minimal overhead when the future completes normally
///
/// ## Examples
///
/// Basic usage:
///
/// ```
/// use xutex::AsyncMutex;
/// use swait::*;
///
/// async fn example() {
///     let mutex = AsyncMutex::new(42);
///     
///     // lock() returns an AsyncLockRequest
///     let guard = mutex.lock().await;
///     assert_eq!(*guard, 42);
/// }
///
/// example().swait();
/// ```
#[must_use = "futures do nothing unless polled"]
pub struct AsyncLockRequest<'a, T> {
    mutex: &'a MutexInternal<T>,
    entry: Signal,
    _pinned: PhantomPinned,
}

const SIGNAL_UNINIT: usize = 0;
const SIGNAL_INIT_WAITING: usize = 1;
const SIGNAL_SIGNALED: usize = 2;
const SIGNAL_RETURNED: usize = !0;

impl<'a, T> AsyncLockRequest<'a, T> {
    #[cold]
    #[inline(never)]
    fn remove_from_queue(&mut self) -> bool {
        // Find and remove ourselves from the queue
        let backoff = Backoff::new();
        loop {
            let ptr = self.mutex.queue.load(Ordering::Acquire);
            if ptr == UNLOCKED || ptr == LOCKED {
                // Queue was deallocated or mutex is just locked, we're not in any queue
                return false;
            }

            let (ptr, is_tagged) = untag_pointer(ptr);

            if is_tagged {
                backoff.snooze();
                continue;
            }

            // Try to tag the queue to get exclusive access
            let tagged_ptr = tag_pointer(ptr);
            let tagging_result = self.mutex.queue.compare_exchange(
                ptr,
                tagged_ptr,
                Ordering::Acquire,
                Ordering::Relaxed,
            );

            if tagging_result.is_err() {
                backoff.snooze();
                continue;
            }

            // Remove ourselves from the queue
            let result = unsafe {
                // SAFETY: ptr is tagged and we have exclusive access
                let queue = (&mut *ptr).inner.get_mut();

                queue.remove(NonNull::new_unchecked(&mut self.entry))
            };
            // Untag the queue we have guard
            self.mutex.queue.store(ptr, Ordering::Release);

            return result;
        }
    }
}

impl<'a, T> Future for AsyncLockRequest<'a, T> {
    type Output = MutexGuard<'a, T>;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let sig_val = this.entry.value.load(Ordering::Acquire);
        if likely(sig_val >= SIGNAL_SIGNALED) {
            if unlikely(sig_val == SIGNAL_RETURNED) {
                unreachable!(
                    "Mutex guard has already been returned, poll after ready state detected"
                );
            }
            // SAFETY: signaled by the mutex drop
            this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
            return Poll::Ready(unsafe { this.mutex.create_guard() });
        }
        if unlikely(sig_val == SIGNAL_INIT_WAITING) {
            this.entry.waker.register(cx.waker());
            Poll::Pending
        } else {
            let backoff = Backoff::new();
            let mut need_initialization = true;
            loop {
                let locking_result = this.mutex.queue.compare_exchange(
                    UNLOCKED,
                    LOCKED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                );

                if likely(locking_result.is_ok()) {
                    this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                    return Poll::Ready(unsafe { this.mutex.create_guard() });
                }

                if need_initialization {
                    this.entry
                        .value
                        .store(SIGNAL_INIT_WAITING, Ordering::Release);
                    this.entry.waker.register(cx.waker());
                    need_initialization = false;
                }

                let mut ptr = locking_result.unwrap_err();

                if unlikely(ptr == UPDATING) {
                    backoff.snooze();
                    continue;
                }

                if ptr == LOCKED {
                    // init queue
                    let updating_token_init_result = this.mutex.queue.compare_exchange(
                        LOCKED,
                        UPDATING,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    );

                    if unlikely(updating_token_init_result.is_err()) {
                        backoff.snooze();
                        continue;
                    }

                    ptr = Box::leak(allocate_queue());
                } else {
                    let is_tagged;
                    (ptr, is_tagged) = untag_pointer(ptr);

                    if unlikely(is_tagged) {
                        backoff.snooze();
                        continue;
                    }

                    // try to tag it
                    let tagged_ptr = tag_pointer(ptr);
                    let tagging_result = this.mutex.queue.compare_exchange(
                        ptr,
                        tagged_ptr,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    );

                    if unlikely(tagging_result.is_err()) {
                        continue;
                    }
                }

                let queue = unsafe {
                    // SAFETY: ptr is tagged and we have exclusive access
                    (&mut *ptr).inner.get_mut()
                };

                unsafe {
                    // SAFETY: We guarantee that the entry lives long enough, or is removed from the
                    // queue on drop
                    // Convert the mutable raw pointer to NonNull as required by `push`
                    queue.push(NonNull::new_unchecked(&mut this.entry));
                }

                this.mutex.queue.store(ptr, Ordering::Release);

                return Poll::Pending;
            }
        }
    }
}

impl<'a, T> Drop for AsyncLockRequest<'a, T> {
    fn drop(&mut self) {
        if unlikely(self.entry.value.load(Ordering::Acquire) == SIGNAL_INIT_WAITING)
            && !self.remove_from_queue()
        {
            // we failed to remove ourself so we need to wait synchronously for the lock,
            // this usually wouldn't take much as other one acquired our pointer and soon
            // will be signaled. the spin is likely to never happen.
            let backoff = Backoff::new();
            while self.entry.value.load(Ordering::Acquire) != SIGNAL_SIGNALED {
                backoff.snooze();
            }
            // SAFETY: we have the lock now, load it and drop it.
            drop(unsafe { self.mutex.create_guard() });
        };
    }
}

/// A synchronous mutex that provides exclusive access to shared data.
///
/// This mutex implementation is designed for high performance with minimal
/// overhead in the uncontended case. It uses an optimistic approach where the
/// fast path (when the mutex is unlocked) requires only a single atomic
/// compare-and-swap operation.
///
/// ## Performance Characteristics
///
/// This lock is slightly slower than `std::sync::Mutex` in purely synchronous
/// workloads due to additional overhead required to support async contexts.
/// However, this library excels when used in async/await code, where it can
/// yield to the async runtime instead of blocking threads.
///
/// The primary benefit is the ability to use the same lock type in both
/// synchronous and asynchronous contexts, enabling flexible code that can
/// seamlessly transition between blocking and non-blocking operations without
/// changing data structures.
///
/// ## Memory Layout
///
/// The struct uses `#[repr(C)]` to ensure a stable memory layout that is
/// identical to [`AsyncMutex<T>`]. This allows safe conversion between
/// synchronous and asynchronous mutex types without allocation.
///
/// ## Contention Handling
///
/// When contention occurs, the mutex dynamically allocates a wait queue to
/// manage blocked threads. The queue is automatically deallocated when
/// contention subsides. Waiting threads use an adaptive spinning strategy
/// before parking to minimize latency for short critical sections.
///
/// ## Thread Safety
///
/// This type is `Send + Sync` when `T: Send`. The mutex ensures that only one
/// thread can access the protected data at any time through the [`MutexGuard`].
///
/// ## Conversion to Async
///
/// This synchronous mutex can be seamlessly converted to an [`AsyncMutex<T>`]
/// using [`as_async()`](Self::as_async), [`to_async()`](Self::to_async), or
/// [`lock_async()`](Self::lock_async) without any allocation overhead.
///
/// ## Examples
///
/// Basic usage:
///
/// ```
/// use xutex::Mutex;
/// use std::sync::Arc;
/// use std::thread;
///
/// let mutex = Arc::new(Mutex::new(0));
/// let handles: Vec<_> = (0..10).map(|_| {
///     let mutex = Arc::clone(&mutex);
///     thread::spawn(move || {
///         let mut guard = mutex.lock();
///         *guard += 1;
///     })
/// }).collect();
///
/// for handle in handles {
///     handle.join().unwrap();
/// }
///
/// assert_eq!(*mutex.lock(), 10);
/// ```
///
/// Converting to async:
///
/// ```
/// use xutex::Mutex;
/// use swait::*;
///
/// async fn example() {
///     let sync_mutex = Mutex::new(42);
///     
///     // Use as async without conversion
///     let guard = sync_mutex.lock_async().await;
///     assert_eq!(*guard, 42);
///     drop(guard);
///     
///     // Convert to AsyncMutex
///     let async_mutex = sync_mutex.to_async();
///     let guard = async_mutex.lock().await;
///     assert_eq!(*guard, 42);
/// }
///
/// example().swait();
/// ```
#[cfg(feature = "std")]
#[repr(C)]
pub struct Mutex<T> {
    internal: MutexInternal<T>,
}

#[cfg(feature = "std")]
impl<T> Mutex<T> {
    /// Creates a new synchronous mutex.
    ///
    /// # Examples
    ///
    /// ```
    /// use xutex::Mutex;
    ///
    /// let mutex = Mutex::new(42);
    /// ```
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        Self {
            internal: MutexInternal::new(data),
        }
    }

    #[cold]
    #[inline(never)]
    fn lock_slow(&self) {
        let mut entry = Signal::new_sync();
        let mutex = &self.internal;
        let backoff = Backoff::new();
        loop {
            let locking_result = mutex.queue.compare_exchange(
                UNLOCKED,
                LOCKED,
                Ordering::Acquire,
                Ordering::Acquire,
            );

            if likely(locking_result.is_ok()) {
                return;
            }

            let mut ptr = locking_result.unwrap_err();

            if unlikely(ptr == UPDATING) {
                backoff.snooze();
                continue;
            }

            if ptr == LOCKED {
                // init queue
                let updating_token_init_result = mutex.queue.compare_exchange(
                    LOCKED,
                    UPDATING,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                );

                if unlikely(updating_token_init_result.is_err()) {
                    backoff.snooze();
                    continue;
                }

                ptr = Box::leak(allocate_queue());
            } else {
                let is_tagged;
                (ptr, is_tagged) = untag_pointer(ptr);

                if unlikely(is_tagged) {
                    backoff.snooze();
                    continue;
                }

                // try to tag it
                let tagged_ptr = tag_pointer(ptr);
                let tagging_result = mutex.queue.compare_exchange(
                    ptr,
                    tagged_ptr,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                );

                if unlikely(tagging_result.is_err()) {
                    backoff.snooze();
                    continue;
                }
            }

            let queue = unsafe {
                // SAFETY: ptr is tagged and we have exclusive access
                (&mut *ptr).inner.get_mut()
            };

            let do_spin = unsafe {
                // SAFETY: We guarantee that the entry lives long enough by blocking here
                // Convert the mutable raw pointer to `NonNull<Signal>` as required by `push`.
                queue.push(NonNull::new_unchecked(&mut entry))
            };

            mutex.queue.store(ptr, Ordering::Release);

            // if we are first in the queue, we can try to spin before parking
            if do_spin {
                let backoff = Backoff::new();
                while !backoff.is_completed() {
                    if entry.value.load(Ordering::Acquire) == SIGNAL_SIGNALED {
                        return;
                    }
                    backoff.snooze();
                }
            }
            if likely(entry.value.swap(SIGNAL_INIT_WAITING, Ordering::AcqRel) == SIGNAL_SIGNALED) {
                return;
            }
            loop {
                std::thread::park();
                if entry.value.load(Ordering::Acquire) == SIGNAL_SIGNALED {
                    return;
                }
            }
        }
    }

    /// Acquires the mutex, blocking the current thread until it is able to do
    /// so.
    ///
    /// This function will block if the lock is unavailable. When this function
    /// returns, the mutex is locked and the caller is free to modify the
    /// contained data.
    ///
    /// # Examples
    ///
    /// ```
    /// use xutex::Mutex;
    ///
    /// let mutex = Mutex::new(0);
    /// {
    ///     let mut guard = mutex.lock();
    ///     *guard += 1;
    /// }
    /// assert_eq!(*mutex.lock(), 1);
    /// ```
    #[inline(always)]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        if let Some(guard) = self.internal.try_lock() {
            return guard;
        }
        if unlikely(
            self.internal
                .queue
                .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_err(),
        ) {
            self.lock_slow();
        }
        unsafe { self.internal.create_guard() }
    }

    /// Attempts to acquire the mutex without blocking.
    ///
    /// Returns `Some(guard)` if the lock was acquired, or `None` if it was
    /// already locked.
    ///
    /// # Examples
    ///
    /// ```
    /// use xutex::Mutex;
    ///
    /// let mutex = Mutex::new(0);
    /// if let Some(mut guard) = mutex.try_lock() {
    ///     *guard += 1;
    /// }
    /// ```
    #[inline(always)]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.internal.try_lock()
    }

    /// Converts this synchronous mutex reference to an async mutex reference.
    ///
    /// # Safety
    ///
    /// This is safe because both types have the same memory layout
    /// (`#[repr(C)]`).
    ///
    /// # Examples
    ///
    /// ```
    /// use xutex::Mutex;
    ///
    /// let mutex = Mutex::new(42);
    /// let async_mutex = mutex.as_async();
    /// ```
    #[inline(always)]
    pub fn as_async(&self) -> &AsyncMutex<T> {
        // SAFETY: same memory layout and structure
        unsafe { &*(self as *const Mutex<T> as *const AsyncMutex<T>) }
    }
    /// Converts this `Mutex<T>` into an `AsyncMutex<T>` without allocating.
    ///
    /// Both structs are `#[repr(C)]` and have identical layout, so the
    /// transmute is sound.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use xutex::{Mutex, AsyncMutex};
    /// use swait::*;
    /// async fn example() {
    ///     // Create a synchronous mutex.
    ///     let sync_mutex = Mutex::new(10);
    ///
    ///     // Convert it into an asynchronous mutex without any allocation.
    ///     let async_mutex: AsyncMutex<i32> = sync_mutex.to_async();
    ///
    ///     // The data is still accessible via the async mutex.
    ///     let guard = async_mutex.lock().await;
    ///     assert_eq!(*guard, 10);
    /// }
    /// example().swait();
    /// ```
    #[inline(always)]
    pub fn to_async(self: Mutex<T>) -> AsyncMutex<T> {
        // Decompose the `Mutex<T>` and reassemble an `AsyncMutex<T>`.
        // Both structs contain the same `internal` field, so this conversion
        // is safe and does not require `transmute`.
        let Mutex { internal } = self;
        AsyncMutex { internal }
    }

    /// Converts an `Arc<Mutex<T>>` into an `Arc<AsyncMutex<T>>` without
    /// allocating.
    ///
    /// The underlying pointer is re-interpreted because the two types share the
    /// same layout.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use xutex::{Mutex, AsyncMutex};
    /// use swait::*;
    ///
    /// async fn example() {
    ///     // Wrap a synchronous mutex in an `Arc`.
    ///     let sync_arc: Arc<Mutex<u32>> = Arc::new(Mutex::new(10));
    ///
    ///     // Convert the `Arc<Mutex<_>>` into an `Arc<AsyncMutex<_>>` without allocation.
    ///     let async_arc: Arc<AsyncMutex<u32>> = sync_arc.clone().to_async_arc();
    ///
    ///     // The data is still accessible via the async mutex.
    ///     let guard = async_arc.lock().await;
    ///     assert_eq!(*guard, 10);
    /// }
    /// example().swait();
    /// ```
    #[inline(always)]
    pub fn to_async_arc(self: Arc<Self>) -> Arc<AsyncMutex<T>> {
        // Turn the Arc into a raw pointer, cast it, then rebuild an Arc.
        let raw = Arc::into_raw(self) as *const AsyncMutex<T>;
        // SAFETY: the raw pointer points to a valid `AsyncMutex<T>` because the
        // layout of `Mutex<T>` and `AsyncMutex<T>` is identical.
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<Mutex<T>>` and returns it as an `Arc<AsyncMutex<T>>`
    /// without allocating.
    ///
    /// This is a convenience method that combines `Arc::clone` with
    /// `to_async_arc`. It's useful when you want to share the mutex with
    /// async code while keeping the original `Arc<Mutex<T>>` for
    /// synchronous code.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use xutex::{Mutex, AsyncMutex};
    /// use swait::*;
    ///
    /// async fn example() {
    ///     let sync_arc: Arc<Mutex<u32>> = Arc::new(Mutex::new(10));
    ///     
    ///     // Clone and convert to async without consuming the original
    ///     let async_arc: Arc<AsyncMutex<u32>> = sync_arc.clone_async();
    ///     
    ///     // Both can be used independently
    ///     *sync_arc.lock() = 20;
    ///     let guard = async_arc.lock().await;
    ///     assert_eq!(*guard, 20);
    /// }
    /// example().swait();
    /// ```
    #[inline(always)]
    pub fn clone_async(self: &Arc<Self>) -> Arc<AsyncMutex<T>> {
        Arc::clone(self).to_async_arc()
    }

    /// Acquires the mutex asynchronously, returning a future that resolves to a
    /// guard.
    ///
    /// This method allows the synchronous `Mutex` to be used in async contexts
    /// by converting it to an `AsyncMutex` internally.
    ///
    /// # Examples
    ///
    /// ```
    /// use xutex::Mutex;
    /// use swait::*;
    ///
    /// async fn example() {
    ///     let mutex = Mutex::new(0);
    ///     let mut guard = mutex.lock_async().await;
    ///     *guard += 1;
    /// }
    ///
    /// example().swait();
    /// ```
    #[inline(always)]
    pub fn lock_async(&self) -> AsyncLockRequest<'_, T> {
        self.as_async().lock()
    }
}

#[cfg(test)]
#[cfg(feature = "std")]
mod tests;

/// An asynchronous mutex that provides exclusive access to shared data.
///
/// This mutex is optimized for async/await contexts and can yield to the async
/// runtime instead of blocking threads when contention occurs. It maintains the
/// same high-performance characteristics as [`Mutex<T>`] while providing
/// seamless integration with async code.
///
/// ## Performance Characteristics
///
/// `AsyncMutex` is designed for optimal performance in async environments:
///
/// - **Fast Path**: When uncontended, acquiring the lock requires only a single
///   atomic compare-and-swap operation
/// - **Async-Aware**: Uses async wakers instead of thread parking, allowing the
///   runtime to schedule other tasks while waiting
/// - **Zero-Cost Conversion**: Can be converted to/from [`Mutex<T>`] without
///   allocation due to identical memory layout
/// - **Extreme Efficiency During Contention**: Dynamically allocates a wait
///   queue only with no additional heap overhead after contention occurs
///
/// ## Memory Layout
///
/// The struct uses `#[repr(C)]` to ensure a stable memory layout that is
/// identical to [`Mutex<T>`]. This enables safe zero-cost conversions between
/// synchronous and asynchronous mutex types.
///
/// ## Contention Handling
///
/// When multiple tasks attempt to acquire the lock simultaneously:
///
/// 1. The first task to encounter contention allocates a wait queue
/// 2. Subsequent tasks register themselves in the queue with their async wakers
/// 3. When the lock is released, the next waiting task is awakened via its
///    waker
/// 4. The queue is automatically deallocated when contention subsides
///
/// ## Thread Safety
///
/// This type is `Send + Sync` when `T: Send`. The mutex ensures that only one
/// task can access the protected data at any time through the [`MutexGuard`].
///
/// ## Cancellation Safety
///
/// Lock acquisition futures returned by [`lock()`](Self::lock) are
/// cancellation-safe. If a future is dropped while waiting, it will either:
///
/// 1. Successfully remove itself from the wait queue, or
/// 2. Wait synchronously for the lock signal and immediately drop the guard
///
/// This ensures no dangling pointers remain in the queue after cancellation.
///
/// ## Conversion to Sync
///
/// This async mutex can be seamlessly converted to a [`Mutex<T>`] using
/// [`as_sync()`](Self::as_sync), [`to_sync()`](Self::to_sync), or
/// [`lock_sync()`](Self::lock_sync) without any allocation overhead.
///
/// ## Examples
///
/// Basic usage:
///
/// ```
/// use xutex::AsyncMutex;
/// use swait::*;
///
/// async fn example() {
///     let mutex = AsyncMutex::new(0);
///     
///     {
///         let mut guard = mutex.lock().await;
///         *guard += 1;
///     }
///     
///     assert_eq!(*mutex.lock().await, 1);
/// }
///
/// example().swait();
/// ```
///
/// Sharing between tasks:
///
/// ```
/// use xutex::AsyncMutex;
/// use std::sync::Arc;
/// use swait::*;
///
/// async fn example() {
///     let mutex = Arc::new(AsyncMutex::new(0));
///     let mut handles = Vec::new();
///
///     for _ in 0..10 {
///         let mutex = Arc::clone(&mutex);
///         handles.push(async move {
///             let mut guard = mutex.lock().await;
///             *guard += 1;
///         });
///     }
///
///     for handle in handles {
///         handle.await;
///     }
///
///     assert_eq!(*mutex.lock().await, 10);
/// }
///
/// example().swait();
/// ```
///
/// Converting to synchronous mutex:
///
/// ```
/// use xutex::AsyncMutex;
/// use swait::*;
/// #[cfg(feature = "std")]
/// async fn example() {
///     let async_mutex = AsyncMutex::new(42);
///     
///     // Use sync method on async mutex
///     let guard = async_mutex.lock_sync();
///     assert_eq!(*guard, 42);
///     drop(guard);
///     
///     // Convert to sync mutex
///     let sync_mutex = async_mutex.to_sync();
///     let guard = sync_mutex.lock();
///     assert_eq!(*guard, 42);
/// }
/// #[cfg(feature = "std")]
/// example().swait();
/// ```
///
/// ## Performance Notes
///
/// - **Async Contexts**: Significantly faster than blocking mutexes in async
///   code since it doesn't block threads
/// - **Sync Contexts**: Slight overhead compared to `std::sync::Mutex` due to
///   async infrastructure, but the difference is minimal
/// - **Hybrid Usage**: Ideal for libraries that need to work in both sync and
///   async contexts without maintaining separate data structures
#[repr(C)]
pub struct AsyncMutex<T> {
    internal: MutexInternal<T>,
}

impl<T> AsyncMutex<T> {
    /// Creates a new asynchronous mutex wrapping the supplied data.
    ///
    /// This mirrors `Mutex::new` but returns an `AsyncMutex`. The underlying
    /// `MutexInternal` is allocated immediately; no heap allocation is
    /// performed until contention forces a queue to be created.
    #[inline(always)]
    pub const fn new(data: T) -> Self {
        Self {
            internal: MutexInternal::new(data),
        }
    }

    /// Returns a future that resolves to a `MutexGuard` when the lock becomes
    /// available.
    ///
    /// The future is represented by `AsyncLockRequest`. It holds a reference to
    /// the internal mutex, a fresh `Signal` (initially without a waker),
    /// and a `PhantomPinned` to ensure the request is !Unpin.
    #[inline(always)]
    pub fn lock(&self) -> AsyncLockRequest<'_, T> {
        AsyncLockRequest {
            mutex: &self.internal,
            entry: Signal::new_none(),
            _pinned: PhantomPinned,
        }
    }

    /// Attempts to acquire the lock without blocking.
    ///
    /// Returns `Some(MutexGuard)` if the fast‑path succeeds, otherwise `None`.
    /// This simply forwards to the internal `MutexInternal::try_lock`.
    #[inline(always)]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.internal.try_lock()
    }

    /// Casts a reference to `AsyncMutex<T>` to a reference to the synchronous
    /// `Mutex<T>`.
    ///
    /// Both structs are `#[repr(C)]` and have identical layout, so this
    /// `unsafe` conversion is sound. It enables the async mutex to reuse
    /// the synchronous lock implementation when needed (e.g., `lock_sync`).
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn as_sync(&self) -> &Mutex<T> {
        // SAFETY: same memory layout and structure
        unsafe { &*(self as *const AsyncMutex<T> as *const Mutex<T>) }
    }

    /// Converts this `AsyncMutex<T>` into a `Mutex<T>` without allocating.W
    ///
    /// Both structs are `#[repr(C)]` and have identical layout, so the
    /// conversion is sound.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use xutex::{Mutex, AsyncMutex};
    ///
    /// fn example() {
    ///     // Create an asynchronous mutex.
    ///     let async_mutex = AsyncMutex::new(10);
    ///
    ///     // Convert it into a synchronous mutex without any allocation.
    ///     let sync_mutex: Mutex<i32> = async_mutex.to_sync();
    ///
    ///     // The data is still accessible via the sync mutex.
    ///     let guard = sync_mutex.lock();
    ///     assert_eq!(*guard, 10);
    /// }
    /// example();
    /// ```
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn to_sync(self: AsyncMutex<T>) -> Mutex<T> {
        // Decompose the `AsyncMutex<T>` and reassemble a `Mutex<T>`.
        // Both structs contain the same `internal` field, so this conversion
        // is safe and does not require `transmute`.
        let AsyncMutex { internal } = self;
        Mutex { internal }
    }

    /// Converts an `Arc<AsyncMutex<T>>` into an `Arc<Mutex<T>>` without
    /// allocating.
    ///
    /// The underlying pointer is re‑interpreted because the two types share the
    /// same layout.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use xutex::{Mutex, AsyncMutex};
    ///
    /// fn example() {
    ///     // Wrap an asynchronous mutex in an `Arc`.
    ///     let async_arc: Arc<AsyncMutex<u32>> = Arc::new(AsyncMutex::new(10));
    ///
    ///     // Convert the `Arc<AsyncMutex<_>>` into an `Arc<Mutex<_>>` without allocation.
    ///     let sync_arc: Arc<Mutex<u32>> = async_arc.clone().to_sync_arc();
    ///
    ///     // The data is still accessible via the sync mutex.
    ///     let guard = sync_arc.lock();
    ///     assert_eq!(*guard, 10);
    /// }
    /// example();
    /// ```
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn to_sync_arc(self: Arc<Self>) -> Arc<Mutex<T>> {
        // Turn the Arc into a raw pointer, cast it, then rebuild an Arc.
        let raw = Arc::into_raw(self) as *const Mutex<T>;
        // SAFETY: the raw pointer points to a valid `Mutex<T>` because the
        // layout of `AsyncMutex<T>` and `Mutex<T>` is identical.
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<AsyncMutex<T>>` and returns it as an `Arc<Mutex<T>>`
    /// without allocating.
    ///
    /// This is a convenience method that combines `Arc::clone` with
    /// `to_sync_arc`. It's useful when you want to share the mutex with
    /// sync code while keeping the original `Arc<AsyncMutex<T>>` for
    /// asynchronous code.
    ///
    /// # Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use xutex::{Mutex, AsyncMutex};
    /// use swait::*;
    ///
    /// async fn example() {
    ///     let async_arc: Arc<AsyncMutex<u32>> = Arc::new(AsyncMutex::new(10));
    ///     
    ///     // Clone and convert to sync without consuming the original
    ///     let sync_arc: Arc<Mutex<u32>> = async_arc.clone_sync();
    ///     
    ///     // Both can be used independently
    ///     *sync_arc.lock() = 20;
    ///     let guard = async_arc.lock().await;
    ///     assert_eq!(*guard, 20);
    /// }
    /// example().swait();
    /// ```
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn clone_sync(self: &Arc<Self>) -> Arc<Mutex<T>> {
        Arc::clone(self).to_sync_arc()
    }

    /// Acquires the lock synchronously, blocking the current thread.
    ///
    /// Internally it converts `self` to a `&Mutex<T>` via `as_sync` and then
    /// calls the regular `Mutex::lock` method.
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn lock_sync(&self) -> MutexGuard<'_, T> {
        self.as_sync().lock()
    }
}

/// A guard that provides exclusive access to the data protected by a mutex.
///
/// This type is returned by [`Mutex::lock()`], [`Mutex::try_lock()`],
/// [`AsyncMutex::lock()`], [`AsyncMutex::try_lock()`], and
/// [`AsyncMutex::lock_sync()`]. It implements [`Deref`] and [`DerefMut`] to
/// provide access to the underlying data.
///
/// The guard automatically releases the mutex when it is dropped, ensuring that
/// the lock cannot be forgotten or left in an inconsistent state.
///
/// ## Thread Safety
///
/// `MutexGuard` is `Send` when `T: Send`, allowing it to be transferred between
/// threads. It is `Sync` when `T: Sync`, allowing shared references to be used
/// across threads (though this is rarely useful since the guard provides
/// exclusive access).
///
/// ## Drop Behavior
///
/// When the guard is dropped, it:
/// 1. Attempts to unlock the mutex using a fast path (single atomic operation)
/// 2. If contention exists, it signals the next waiting thread or task
/// 3. Potentially deallocates the wait queue if no more waiters exist
///
/// The drop implementation is carefully optimized to minimize overhead in the
/// common case where no other threads are waiting.
#[repr(C)]
pub struct MutexGuard<'a, T> {
    mutex: &'a MutexInternal<T>,
}

impl<'a, T> MutexGuard<'a, T> {
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self) {
        let backoff = Backoff::new();
        let unlock_result = self.mutex.queue.compare_exchange(
            LOCKED,
            UNLOCKED,
            Ordering::Release,
            Ordering::Acquire,
        );
        if likely(unlock_result.is_ok()) {
            return;
        }
        let mut ptr = unlock_result.unwrap_err();
        while ptr == UPDATING {
            backoff.snooze();
            ptr = self.mutex.queue.load(Ordering::Acquire);
        }

        (ptr, _) = untag_pointer(ptr);

        // tag pointer
        let tagged_ptr = tag_pointer(ptr);

        loop {
            let tagging_result = self.mutex.queue.compare_exchange(
                ptr,
                tagged_ptr,
                Ordering::Acquire,
                Ordering::Relaxed,
            );
            if likely(tagging_result.is_ok()) {
                break;
            }
            backoff.snooze();
        }

        let queue = unsafe {
            // SAFETY: ptr is tagged and we have exclusive access
            &mut *ptr
        }
        .inner
        .get_mut();

        if let Some(entry) = queue.pop() {
            // untag pointer
            self.mutex.queue.store(ptr, Ordering::Release);
            unsafe {
                // SAFETY: entry is valid as it was pushed by a waiting task
                let entry_ref = entry.as_ref();
                // signal the waker to wake up, first acquire waker so if after signal waiter
                // finished their function, it will be no race condition.
                let waker = entry_ref.waker.take();

                match waker {
                    WakerSlot::None => {}
                    #[cfg(feature = "std")]
                    WakerSlot::Sync(thread) => {
                        if entry_ref.value.swap(SIGNAL_SIGNALED, Ordering::AcqRel)
                            == SIGNAL_INIT_WAITING
                        {
                            thread.unpark();
                        }
                    }
                    WakerSlot::Async(waker) => {
                        entry_ref.value.store(SIGNAL_SIGNALED, Ordering::Release);
                        waker.wake();
                    }
                }
            }
        } else {
            // queue is untouched it is possible to release the queue and unlock
            self.mutex.queue.store(UNLOCKED, Ordering::Release);
            // SAFETY: this section is sole owner of the queue and queue is_empty
            unsafe {
                // no clean up needed as when is_empty is met, both first and last are None
                deallocate_queue(Box::from_raw(ptr));
            }
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.inner.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    #[inline(always)]
    fn drop(&mut self) {
        let unlock_result = self.mutex.queue.compare_exchange(
            LOCKED,
            UNLOCKED,
            Ordering::Release,
            Ordering::Relaxed,
        );
        if likely(unlock_result.is_ok()) {
            return;
        }
        self.drop_slow();
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.inner.get() }
    }
}

unsafe impl<T: Send> Send for MutexInternal<T> {}
unsafe impl<T: Send> Sync for MutexInternal<T> {}
#[cfg(feature = "std")]
unsafe impl<T: Send> Send for Mutex<T> {}
#[cfg(feature = "std")]
unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for AsyncMutex<T> {}
unsafe impl<T: Send> Sync for AsyncMutex<T> {}
unsafe impl<T: Send> Send for AsyncLockRequest<'_, T> {}

unsafe impl<T: Send> Send for MutexGuard<'_, T> {}
unsafe impl<T: Sync> Sync for MutexGuard<'_, T> {}
