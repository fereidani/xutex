#[doc = include_str!("../README.md")]
use std::ptr::NonNull;
#[warn(missing_docs)]
use std::{
    cell::UnsafeCell,
    marker::PhantomPinned,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
    task::Poll,
};

mod allocator;
mod backoff;
mod signal_queue;
mod waker;
pub(crate) use signal_queue::SignalQueue;

use crate::{
    allocator::{allocate_queue, deallocate_queue},
    waker::{DynamicWaker, WakerSlot},
};
use backoff::Backoff;
use branches::{likely, unlikely};
use std::sync::Arc;

#[repr(C)]
pub struct Signal {
    next: Option<NonNull<Signal>>,
    value: AtomicUsize,
    waker: DynamicWaker,
    _pinned: PhantomPinned,
}

#[inline]
/// # Safety
///
/// The caller must guarantee that `m` points to a valid `std::sync::Mutex<T>`
/// that lives for at least the lifetime `'a` of the returned guard.
///
/// This function converts the raw pointer into a reference and then obtains a
/// `MutexGuard` without performing poisoning checks.
pub unsafe fn lock_unpoisoned<'a, T>(
    m: *const std::sync::Mutex<T>,
) -> std::sync::MutexGuard<'a, T> {
    // SAFETY: the caller ensures `m` is valid for `'a`.
    unsafe {
        match (&*m).lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
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
    pub fn new_sync() -> Self {
        Self {
            next: None,
            value: AtomicUsize::new(SIGNAL_UNINIT),
            waker: DynamicWaker::new_sync(),
            _pinned: PhantomPinned,
        }
    }
}

type QueueStructure = std::sync::Mutex<SignalQueue>;

const LOCKED: *mut QueueStructure = usize::MAX as *mut _;
const UNLOCKED: *mut QueueStructure = std::ptr::null_mut();

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
    pub(crate) fn new(data: T) -> Self {
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
                Ordering::AcqRel,
                Ordering::Relaxed,
            );

            if tagging_result.is_err() {
                backoff.snooze();
                continue;
            }

            // Remove ourselves from the queue
            let result = unsafe {
                let mut queue = lock_unpoisoned(ptr);
                queue.remove(NonNull::new_unchecked(&mut self.entry))
            };

            // Untag the queue
            self.mutex.queue.store(ptr, Ordering::Release);
            return result;
        }
    }
}

impl<'a, T> Future for AsyncLockRequest<'a, T> {
    type Output = MutexGuard<'a, T>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
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
        if sig_val == SIGNAL_INIT_WAITING {
            this.entry.waker.register(cx.waker());
            Poll::Pending
        } else {
            let backoff = Backoff::new();
            loop {
                let locking_result = this.mutex.queue.compare_exchange(
                    UNLOCKED,
                    LOCKED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                if likely(locking_result.is_ok()) {
                    return Poll::Ready(unsafe { this.mutex.create_guard() });
                }

                let mut ptr = locking_result.unwrap_err();

                if ptr == LOCKED {
                    // init queue
                    ptr = Box::leak(allocate_queue());
                    let tagged_ptr = tag_pointer(ptr);
                    let q_init_result = this.mutex.queue.compare_exchange(
                        LOCKED,
                        tagged_ptr,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                    // deallocate if failed
                    if unlikely(q_init_result.is_err()) {
                        // SAFETY: ptr is valid as it was just allocated
                        deallocate_queue(unsafe { Box::from_raw(ptr) });
                        backoff.snooze();
                        continue;
                    }
                } else {
                    let is_tagged;
                    (ptr, is_tagged) = untag_pointer(ptr);

                    if is_tagged {
                        backoff.snooze();
                        continue;
                    }

                    // try to tag it
                    let tagged_ptr = tag_pointer(ptr);
                    let tagging_result = this.mutex.queue.compare_exchange(
                        ptr,
                        tagged_ptr,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );

                    if tagging_result.is_err() {
                        continue;
                    }
                }

                this.entry
                    .value
                    .store(SIGNAL_INIT_WAITING, Ordering::Release);

                this.entry.waker.register(cx.waker());

                let mut queue = unsafe { lock_unpoisoned(ptr) };
                // untag
                this.mutex.queue.store(ptr, Ordering::Release);
                unsafe {
                    // SAFETY: We guarantee that the entry lives long enough, or is removed from the
                    // queue on drop
                    // Convert the mutable raw pointer to NonNull as required by `push`
                    queue.push(NonNull::new_unchecked(&mut this.entry));
                }

                return Poll::Pending;
            }
        }
    }
}

impl<'a, T> Drop for AsyncLockRequest<'a, T> {
    fn drop(&mut self) {
        if unlikely(self.entry.value.load(Ordering::Acquire) == SIGNAL_INIT_WAITING) {
            if !self.remove_from_queue() {
                // we failed to remove ourself so we need to wait synchronously for the lock,
                // this usually wouldn't take much as other one acquired our pointer and soon
                // will be signaled. the spin is likely to never happen.
                let backoff = Backoff::new();
                while self.entry.value.load(Ordering::Acquire) != SIGNAL_SIGNALED {
                    backoff.snooze();
                }
                // SAFETY: we have the lock now, load it and drop it.
                drop(unsafe { self.mutex.create_guard() });
            }
        };
    }
}

#[repr(C)]
pub struct Mutex<T> {
    internal: MutexInternal<T>,
}

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
    pub fn new(data: T) -> Self {
        Self {
            internal: MutexInternal::new(data),
        }
    }

    #[cold]
    #[inline(never)]
    fn lock_slow(&self) -> MutexGuard<'_, T> {
        let mut entry = Signal::new_sync();
        let mutex = &self.internal;
        let backoff = Backoff::new();
        loop {
            let locking_result =
                mutex
                    .queue
                    .compare_exchange(UNLOCKED, LOCKED, Ordering::AcqRel, Ordering::Acquire);

            if likely(locking_result.is_ok()) {
                return unsafe { mutex.create_guard() };
            }

            let mut ptr = locking_result.unwrap_err();

            if ptr == LOCKED {
                // init queue
                let q = allocate_queue();
                ptr = Box::leak(q) as *mut QueueStructure;
                let tagged_ptr = tag_pointer(ptr);
                let q_init_result = mutex.queue.compare_exchange(
                    LOCKED,
                    tagged_ptr,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                // deallocate if failed
                if unlikely(q_init_result.is_err()) {
                    // SAFETY: ptr is valid as it was just allocated
                    deallocate_queue(unsafe { Box::from_raw(ptr) });
                    continue;
                }
            } else {
                let is_tagged;
                (ptr, is_tagged) = untag_pointer(ptr);

                if is_tagged {
                    backoff.snooze();
                    continue;
                }

                // try to tag it
                let tagged_ptr = tag_pointer(ptr);
                let tagging_result = mutex.queue.compare_exchange(
                    ptr,
                    tagged_ptr,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );

                if tagging_result.is_err() {
                    backoff.snooze();
                    continue;
                }
            }

            let mut queue = unsafe { lock_unpoisoned(ptr) };
            // untag
            mutex.queue.store(ptr, Ordering::Release);
            let do_spin = unsafe {
                // SAFETY: We guarantee that the entry lives long enough by blocking here
                // Convert the mutable raw pointer to `NonNull<Signal>` as required by `push`.
                queue.push(NonNull::new_unchecked(&mut entry))
            };
            drop(queue);

            // if we are first in the queue, we can try to spin before parking
            if do_spin {
                let backoff = Backoff::new();
                while !backoff.is_completed() {
                    if entry.value.load(Ordering::Acquire) == SIGNAL_SIGNALED {
                        return unsafe { mutex.create_guard() };
                    }
                    backoff.snooze();
                }
            }
            if entry.value.swap(SIGNAL_INIT_WAITING, Ordering::AcqRel) == SIGNAL_SIGNALED {
                return unsafe { mutex.create_guard() };
            }
            loop {
                std::thread::park();
                if entry.value.load(Ordering::Acquire) == SIGNAL_SIGNALED {
                    return unsafe { mutex.create_guard() };
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
        self.lock_slow()
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
    /// Both structs are `#[repr(C)]` and have identical layout, so the transmute
    /// is sound.
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

    /// Converts an `Arc<Mutex<T>>` into an `Arc<AsyncMutex<T>>` without allocating.
    ///
    /// The underlying pointer is re‑interpreted because the two types share the
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
mod tests;

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
    pub fn new(data: T) -> Self {
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
    pub fn as_sync(&self) -> &Mutex<T> {
        // SAFETY: same memory layout and structure
        unsafe { &*(self as *const AsyncMutex<T> as *const Mutex<T>) }
    }

    /// Acquires the lock synchronously, blocking the current thread.
    ///
    /// Internally it converts `self` to a `&Mutex<T>` via `as_sync` and then
    /// calls the regular `Mutex::lock` method.
    #[inline(always)]
    pub fn lock_sync(&self) -> MutexGuard<'_, T> {
        self.as_sync().lock()
    }
}

#[repr(C)]
pub struct MutexGuard<'a, T> {
    mutex: &'a MutexInternal<T>,
}

impl<'a, T> MutexGuard<'a, T> {
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self) {
        let backoff = Backoff::new();
        loop {
            let unlock_result = self.mutex.queue.compare_exchange(
                LOCKED,
                UNLOCKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            if likely(unlock_result.is_ok()) {
                return;
            }
            let ptr = unlock_result.unwrap_err();
            let (ptr, is_tagged) = untag_pointer(ptr);
            let mut queue = unsafe { lock_unpoisoned(&*ptr) };
            if let Some(entry) = queue.pop() {
                drop(queue);
                unsafe {
                    // SAFETY: entry is valid as it was pushed by a waiting task
                    let entry_ref = entry.as_ref();
                    // signal the waker to wake up, first acquire waker so if after signal waiter
                    // finished their function, it will be no race condition.
                    let waker = entry_ref.waker.take();

                    match waker {
                        WakerSlot::None => {}
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
                return;
            } else {
                if is_tagged {
                    // some thread is writing into the queue
                    backoff.snooze();
                    continue;
                }
                let tagged_ptr = tag_pointer(ptr);
                // reserve the queue so no one else writes into it.
                let tagging_result = self.mutex.queue.compare_exchange(
                    ptr,
                    tagged_ptr,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
                // wait for waker to be available in the queue
                if unlikely(tagging_result.is_err()) {
                    backoff.snooze();
                    continue;
                }
                if queue.is_empty() {
                    // queue is untouched it is possible to release the queue and unlock
                    self.mutex.queue.store(UNLOCKED, Ordering::Release);
                    // SAFETY: this section is sole owner of the queue and queue is_empty
                    unsafe {
                        // no clean up needed as when is_empty is met, both first and last are None
                        deallocate_queue(Box::from_raw(ptr));
                    }
                    return;
                } else {
                    // someone written into the queue meanwhile, release it back for others
                    self.mutex.queue.store(ptr, Ordering::Release);
                    continue;
                }
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
            Ordering::AcqRel,
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
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for AsyncMutex<T> {}
unsafe impl<T: Send> Sync for AsyncMutex<T> {}
unsafe impl<T: Send> Send for AsyncLockRequest<'_, T> {}

unsafe impl<T: Send> Send for MutexGuard<'_, T> {}
unsafe impl<T: Sync> Sync for MutexGuard<'_, T> {}
