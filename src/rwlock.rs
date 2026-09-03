//! Reader-writer lock built on the semaphore core, split into a synchronous
//! [`RwLock`] and an asynchronous [`AsyncRwLock`] with identical layout.
//!
//! Like tokio's `RwLock`, this is a fair (write-preferring, strict FIFO)
//! lock implemented as a counting semaphore: a reader holds one permit, a
//! writer holds all `max_readers` permits. A queued writer therefore blocks
//! later readers from barging, so writers cannot starve.

use core::fmt;
use core::marker::PhantomPinned;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};

#[cfg(all(feature = "std", not(loom)))]
use alloc::sync::Arc;

use branches::unlikely;

use crate::semaphore::SemCore;
use crate::shim::cell::UnsafeCell;
use crate::shim::sync::atomic::Ordering;
use crate::{SIGNAL_RETURNED, SIGNAL_UNINIT, Signal};

/// Maximum (and default) number of concurrent readers, matching tokio.
const MAX_READERS: usize = (u32::MAX >> 3) as usize;

#[repr(C)]
pub(crate) struct RwLockInternal<T> {
    sem: SemCore,
    max_readers: usize,
    data: UnsafeCell<T>,
}

impl<T> RwLockInternal<T> {
    #[cfg(any(feature = "std", loom))]
    #[inline(always)]
    fn acquire_sync_infallible(&self, n: usize) {
        if self.sem.acquire_sync(n).is_err() {
            // The internal semaphore is never closed.
            unreachable!("rwlock semaphore closed");
        }
    }
}

macro_rules! rwlock_shared_impl {
    ($name:ident) => {
        impl<T> $name<T> {
            /// Attempts to acquire shared read access without waiting.
            ///
            /// Returns `None` if a writer holds the lock or writers are
            /// queued (fairness: readers never barge past queued writers).
            #[inline(always)]
            pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
                if self.internal.sem.try_acquire(1).is_ok() {
                    Some(RwLockReadGuard {
                        lock: &self.internal,
                    })
                } else {
                    None
                }
            }

            /// Attempts to acquire exclusive write access without waiting.
            #[inline(always)]
            pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
                if self
                    .internal
                    .sem
                    .try_acquire(self.internal.max_readers)
                    .is_ok()
                {
                    Some(RwLockWriteGuard {
                        lock: &self.internal,
                    })
                } else {
                    None
                }
            }

            /// Returns a mutable reference to the underlying data without
            /// locking (`&mut self` guarantees exclusivity).
            #[inline(always)]
            pub fn get_mut(&mut self) -> &mut T {
                self.internal.data.get_mut()
            }

            /// Consumes the lock, returning the underlying data.
            #[inline(always)]
            pub fn into_inner(self) -> T {
                self.internal.data.into_inner()
            }
        }

        impl<T: Default> Default for $name<T> {
            fn default() -> Self {
                Self::new(T::default())
            }
        }

        impl<T> From<T> for $name<T> {
            fn from(value: T) -> Self {
                Self::new(value)
            }
        }

        impl<T: fmt::Debug> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut d = f.debug_struct(stringify!($name));
                match self.try_read() {
                    Some(guard) => d.field("data", &&*guard),
                    None => d.field("data", &format_args!("<locked>")),
                };
                d.finish()
            }
        }
    };
}

/// A synchronous reader-writer lock allowing many concurrent readers or one
/// exclusive writer.
///
/// Fair (write-preferring FIFO) and built on the same allocation-free
/// wait-queue algorithm as [`crate::Mutex`]: blocked threads park on
/// stack-allocated intrusive nodes, and the only heap allocation — the wait
/// queue itself — is pooled and exists only while there is contention.
///
/// The layout is identical to [`AsyncRwLock`] (`#[repr(C)]`), enabling free
/// conversion between the two, and every lock can also be taken
/// asynchronously via [`read_async`](Self::read_async)/
/// [`write_async`](Self::write_async).
///
/// # Examples
///
/// ```
/// use xutex::RwLock;
///
/// let lock = RwLock::new(5);
///
/// {
///     let r1 = lock.read();
///     let r2 = lock.read();
///     assert_eq!(*r1 + *r2, 10);
/// } // read locks dropped
///
/// {
///     let mut w = lock.write();
///     *w += 1;
///     assert_eq!(*w, 6);
/// }
/// ```
#[cfg(feature = "std")]
#[repr(C)]
pub struct RwLock<T> {
    internal: RwLockInternal<T>,
}

/// An asynchronous reader-writer lock allowing many concurrent readers or
/// one exclusive writer.
///
/// The async counterpart of [`RwLock`]: acquisition returns futures
/// ([`AsyncReadRequest`]/[`AsyncWriteRequest`]) that are cancellation-safe
/// and runtime-agnostic. Fair (write-preferring FIFO), with the same
/// allocation-free waiter algorithm as [`crate::AsyncMutex`].
///
/// # Examples
///
/// ```
/// use xutex::AsyncRwLock;
/// use swait::*;
///
/// async fn example() {
///     let lock = AsyncRwLock::new(1);
///     let mut w = lock.write().await;
///     *w += 1;
///     drop(w);
///     assert_eq!(*lock.read().await, 2);
/// }
/// example().swait();
/// ```
#[repr(C)]
pub struct AsyncRwLock<T> {
    internal: RwLockInternal<T>,
}

#[cfg(feature = "std")]
rwlock_shared_impl!(RwLock);
rwlock_shared_impl!(AsyncRwLock);

#[cfg(feature = "std")]
impl<T> RwLock<T> {
    /// Creates a new reader-writer lock protecting `data`.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new(data: T) -> Self {
        Self {
            internal: RwLockInternal {
                sem: SemCore::new(MAX_READERS),
                max_readers: MAX_READERS,
                data: UnsafeCell::new(data),
            },
        }
    }

    /// Creates a new reader-writer lock (loom model-checking build).
    #[cfg(loom)]
    pub fn new(data: T) -> Self {
        Self::with_max_readers(data, MAX_READERS)
    }

    /// Creates a new lock that allows at most `max_readers` concurrent
    /// readers.
    ///
    /// # Panics
    ///
    /// Panics if `max_readers` is zero or greater than `u32::MAX >> 3`.
    pub fn with_max_readers(data: T, max_readers: usize) -> Self {
        assert!(
            max_readers > 0 && max_readers <= MAX_READERS,
            "max_readers must be in 1..=u32::MAX >> 3"
        );
        Self {
            internal: RwLockInternal {
                sem: SemCore::new(max_readers),
                max_readers,
                data: UnsafeCell::new(data),
            },
        }
    }

    /// Acquires shared read access, blocking the current thread until no
    /// writer holds or awaits the lock.
    ///
    /// Multiple readers can hold the lock simultaneously.
    #[inline(always)]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.internal.acquire_sync_infallible(1);
        RwLockReadGuard {
            lock: &self.internal,
        }
    }

    /// Acquires exclusive write access, blocking the current thread until
    /// all readers and writers have released the lock.
    #[inline(always)]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.internal
            .acquire_sync_infallible(self.internal.max_readers);
        RwLockWriteGuard {
            lock: &self.internal,
        }
    }

    /// Acquires shared read access asynchronously; see
    /// [`AsyncRwLock::read`].
    #[inline(always)]
    pub fn read_async(&self) -> AsyncReadRequest<'_, T> {
        self.as_async().read()
    }

    /// Acquires exclusive write access asynchronously; see
    /// [`AsyncRwLock::write`].
    #[inline(always)]
    pub fn write_async(&self) -> AsyncWriteRequest<'_, T> {
        self.as_async().write()
    }

    /// Views this lock as an [`AsyncRwLock`].
    #[inline(always)]
    pub fn as_async(&self) -> &AsyncRwLock<T> {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const RwLock<T> as *const AsyncRwLock<T>) }
    }

    /// Converts this lock into an [`AsyncRwLock`] without allocating.
    #[inline(always)]
    pub fn to_async(self) -> AsyncRwLock<T> {
        let RwLock { internal } = self;
        AsyncRwLock { internal }
    }

    /// Converts an `Arc<RwLock<T>>` into an `Arc<AsyncRwLock<T>>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn to_async_arc(self: Arc<Self>) -> Arc<AsyncRwLock<T>> {
        let raw = Arc::into_raw(self) as *const AsyncRwLock<T>;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<RwLock<T>>` as an `Arc<AsyncRwLock<T>>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn clone_async(self: &Arc<Self>) -> Arc<AsyncRwLock<T>> {
        Arc::clone(self).to_async_arc()
    }
}

impl<T> AsyncRwLock<T> {
    /// Creates a new asynchronous reader-writer lock protecting `data`.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new(data: T) -> Self {
        Self {
            internal: RwLockInternal {
                sem: SemCore::new(MAX_READERS),
                max_readers: MAX_READERS,
                data: UnsafeCell::new(data),
            },
        }
    }

    /// Creates a new asynchronous reader-writer lock (loom build).
    #[cfg(loom)]
    pub fn new(data: T) -> Self {
        Self::with_max_readers(data, MAX_READERS)
    }

    /// Creates a new lock that allows at most `max_readers` concurrent
    /// readers.
    ///
    /// # Panics
    ///
    /// Panics if `max_readers` is zero or greater than `u32::MAX >> 3`.
    pub fn with_max_readers(data: T, max_readers: usize) -> Self {
        assert!(
            max_readers > 0 && max_readers <= MAX_READERS,
            "max_readers must be in 1..=u32::MAX >> 3"
        );
        Self {
            internal: RwLockInternal {
                sem: SemCore::new(max_readers),
                max_readers,
                data: UnsafeCell::new(data),
            },
        }
    }

    /// Acquires shared read access asynchronously.
    ///
    /// The returned future is cancellation-safe: dropping it while pending
    /// removes it from the wait queue.
    #[inline(always)]
    pub fn read(&self) -> AsyncReadRequest<'_, T> {
        AsyncReadRequest {
            lock: &self.internal,
            entry: Signal::new_none(),
            _pinned: PhantomPinned,
        }
    }

    /// Acquires exclusive write access asynchronously.
    ///
    /// The returned future is cancellation-safe: dropping it while pending
    /// removes it from the wait queue and re-distributes any permits that
    /// were already assigned to it.
    #[inline(always)]
    pub fn write(&self) -> AsyncWriteRequest<'_, T> {
        AsyncWriteRequest {
            lock: &self.internal,
            entry: Signal::new_none(),
            _pinned: PhantomPinned,
        }
    }

    /// Acquires shared read access, blocking the current thread; see
    /// [`RwLock::read`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn read_sync(&self) -> RwLockReadGuard<'_, T> {
        self.as_sync().read()
    }

    /// Acquires exclusive write access, blocking the current thread; see
    /// [`RwLock::write`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn write_sync(&self) -> RwLockWriteGuard<'_, T> {
        self.as_sync().write()
    }

    /// Views this lock as a synchronous [`RwLock`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn as_sync(&self) -> &RwLock<T> {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const AsyncRwLock<T> as *const RwLock<T>) }
    }

    /// Converts this lock into a synchronous [`RwLock`] without allocating.
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn to_sync(self) -> RwLock<T> {
        let AsyncRwLock { internal } = self;
        RwLock { internal }
    }

    /// Converts an `Arc<AsyncRwLock<T>>` into an `Arc<RwLock<T>>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn to_sync_arc(self: Arc<Self>) -> Arc<RwLock<T>> {
        let raw = Arc::into_raw(self) as *const RwLock<T>;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<AsyncRwLock<T>>` as an `Arc<RwLock<T>>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn clone_sync(self: &Arc<Self>) -> Arc<RwLock<T>> {
        Arc::clone(self).to_sync_arc()
    }
}

/// Shared read access to the data of a [`RwLock`]/[`AsyncRwLock`].
///
/// The read lock is released when this guard is dropped.
#[must_use = "the lock is released when the guard is dropped"]
pub struct RwLockReadGuard<'a, T> {
    lock: &'a RwLockInternal<T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: holding a read permit guarantees no writer exists.
        unsafe { self.lock.data.with(|ptr| &*ptr) }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    #[inline(always)]
    fn drop(&mut self) {
        self.lock.sem.release(1);
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// Exclusive write access to the data of a [`RwLock`]/[`AsyncRwLock`].
///
/// The write lock is released when this guard is dropped, or downgraded to a
/// read lock with [`downgrade`](Self::downgrade).
#[must_use = "the lock is released when the guard is dropped"]
pub struct RwLockWriteGuard<'a, T> {
    lock: &'a RwLockInternal<T>,
}

impl<'a, T> RwLockWriteGuard<'a, T> {
    /// Atomically downgrades the write lock to a read lock without letting
    /// queued writers in between.
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        let lock = self.lock;
        core::mem::forget(self);
        // Keep one permit as our read permit, return the rest.
        lock.sem.release(lock.max_readers - 1);
        RwLockReadGuard { lock }
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: holding all permits guarantees exclusive access.
        unsafe { self.lock.data.with(|ptr| &*ptr) }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding all permits guarantees exclusive access.
        unsafe { self.lock.data.with_mut(|ptr| &mut *ptr) }
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    #[inline(always)]
    fn drop(&mut self) {
        self.lock.sem.release(self.lock.max_readers);
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

macro_rules! rwlock_future {
    ($name:ident, $guard:ident, $permits:expr, $doc:literal) => {
        #[doc = $doc]
        ///
        /// Cancellation-safe: dropping the future while pending removes it
        /// from the wait queue (returning any partially assigned permits).
        #[must_use = "futures do nothing unless polled"]
        pub struct $name<'a, T> {
            lock: &'a RwLockInternal<T>,
            entry: Signal,
            _pinned: PhantomPinned,
        }

        impl<'a, T> Future for $name<'a, T> {
            type Output = $guard<'a, T>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                // SAFETY: we never move out of `this`; the entry stays pinned.
                let this = unsafe { self.get_unchecked_mut() };
                let lock = this.lock;
                let permits = $permits(lock);
                // SAFETY: entry is pinned inside this future and
                // drop_acquire runs on drop.
                match unsafe {
                    lock.sem
                        .poll_acquire(NonNull::new_unchecked(&raw mut this.entry), permits, cx)
                } {
                    Poll::Ready(Ok(())) => Poll::Ready($guard { lock }),
                    // The internal semaphore is never closed.
                    Poll::Ready(Err(_)) => unreachable!("rwlock semaphore closed"),
                    Poll::Pending => Poll::Pending,
                }
            }
        }

        impl<T> Drop for $name<'_, T> {
            fn drop(&mut self) {
                let value = self.entry.value.load(Ordering::Acquire);
                if unlikely(value != SIGNAL_UNINIT && value != SIGNAL_RETURNED) {
                    let permits = $permits(self.lock);
                    // SAFETY: same entry as passed to poll_acquire.
                    unsafe {
                        self.lock
                            .sem
                            .drop_acquire(NonNull::new_unchecked(&raw mut self.entry), permits)
                    };
                }
            }
        }

        unsafe impl<T: Send + Sync> Send for $name<'_, T> {}
    };
}

rwlock_future!(
    AsyncReadRequest,
    RwLockReadGuard,
    |_lock| 1,
    "A future that resolves to a [`RwLockReadGuard`] once shared read access is granted."
);
rwlock_future!(
    AsyncWriteRequest,
    RwLockWriteGuard,
    |lock: &RwLockInternal<T>| lock.max_readers,
    "A future that resolves to a [`RwLockWriteGuard`] once exclusive write access is granted."
);

unsafe impl<T: Send> Send for RwLockInternal<T> {}
unsafe impl<T: Send + Sync> Sync for RwLockInternal<T> {}
#[cfg(feature = "std")]
unsafe impl<T: Send> Send for RwLock<T> {}
#[cfg(feature = "std")]
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
unsafe impl<T: Send> Send for AsyncRwLock<T> {}
unsafe impl<T: Send + Sync> Sync for AsyncRwLock<T> {}

unsafe impl<T: Sync> Send for RwLockReadGuard<'_, T> {}
unsafe impl<T: Sync> Sync for RwLockReadGuard<'_, T> {}
unsafe impl<T: Send + Sync> Send for RwLockWriteGuard<'_, T> {}
unsafe impl<T: Send + Sync> Sync for RwLockWriteGuard<'_, T> {}
