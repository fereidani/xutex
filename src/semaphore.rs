//! Counting semaphore with the same allocation-free wait-queue algorithm as
//! the mutex, split into a synchronous [`Semaphore`] and an asynchronous
//! [`AsyncSemaphore`] with identical memory layout.

use core::fmt;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};

#[cfg(all(feature = "std", not(loom)))]
use alloc::sync::Arc;

use branches::{likely, unlikely};

use crate::backoff::Backoff;
use crate::shim::const_fn;
use crate::shim::sync::atomic::{AtomicUsize, Ordering};
use crate::wait_queue::{PoppedChain, WaitQueue, rearm};
use crate::{
    SIGNAL_CLOSED, SIGNAL_INIT_WAITING, SIGNAL_RETURNED, SIGNAL_SIGNALED, SIGNAL_UNINIT, Signal,
};

/// Bit 0 of the pool word: the semaphore has been closed.
const CLOSED: usize = 0b01;
/// Bit 1 of the pool word: a wait queue exists (waiters may be parked).
/// While this bit is set the fast path never grabs permits, which keeps the
/// semaphore strictly FIFO.
const HAS_QUEUE: usize = 0b10;
/// Number of low bits used for flags; the permit count lives above them.
const PERMIT_SHIFT: u32 = 2;

/// The maximum number of permits a semaphore can hold (`usize::MAX >> 3`).
pub(crate) const MAX_PERMITS: usize = usize::MAX >> 3;

/// Result of an enqueue attempt on the core.
pub(crate) enum EnqueueResult {
    /// The permits were acquired directly while holding the queue lock.
    Acquired,
    /// The semaphore is closed; nothing was acquired or queued.
    Closed,
    /// The entry was pushed to the wait queue. The flag is the "was first in
    /// queue" hint used by the synchronous path to spin before parking.
    Enqueued(#[cfg_attr(not(any(feature = "std", loom)), allow(dead_code))] bool),
}

/// The permit-counting core shared by [`Semaphore`], [`AsyncSemaphore`],
/// [`crate::RwLock`] and [`crate::AsyncRwLock`].
///
/// State is a single atomic word: `permits << 2 | HAS_QUEUE | CLOSED`, plus
/// the [`WaitQueue`] pointer. All waiters are stack-allocated [`Signal`]
/// nodes; `Signal::aux` holds the number of permits the waiter still needs.
/// Releases move permits into the pool word and then hand them to queued
/// waiters in FIFO order under the queue tag-lock.
pub(crate) struct SemCore {
    pool: AtomicUsize,
    queue: WaitQueue,
}

impl SemCore {
    const_fn! {
        pub(crate) const fn new(permits: usize) -> Self {
            assert!(
                permits <= MAX_PERMITS,
                "permit count exceeds MAX_PERMITS"
            );
            Self {
                pool: AtomicUsize::new(permits << PERMIT_SHIFT),
                queue: WaitQueue::new(),
            }
        }
    }

    #[inline(always)]
    pub(crate) fn available_permits(&self) -> usize {
        self.pool.load(Ordering::Acquire) >> PERMIT_SHIFT
    }

    #[inline(always)]
    pub(crate) fn is_closed(&self) -> bool {
        self.pool.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Fast-path acquisition of `n` permits.
    ///
    /// Fails with `NoPermits` when the pool is short *or* when waiters are
    /// queued (`HAS_QUEUE`), preserving FIFO fairness.
    #[inline(always)]
    pub(crate) fn try_acquire(&self, n: usize) -> Result<(), TryAcquireError> {
        let mut cur = self.pool.load(Ordering::Relaxed);
        loop {
            if unlikely(cur & CLOSED != 0) {
                return Err(TryAcquireError::Closed);
            }
            if unlikely(cur & HAS_QUEUE != 0) || (cur >> PERMIT_SHIFT) < n {
                return Err(TryAcquireError::NoPermits);
            }
            match self.pool.compare_exchange_weak(
                cur,
                cur - (n << PERMIT_SHIFT),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Adds `n` permits to the pool and hands them to queued waiters.
    ///
    /// The `fetch_add` and the waiter flag live in the same word, so the
    /// release either observes `HAS_QUEUE` (and drains the queue) or the
    /// concurrent enqueuer's flag-setting RMW observes these permits — a
    /// lost wakeup is impossible.
    pub(crate) fn release(&self, n: usize) {
        if n == 0 {
            return;
        }
        let prev = self.pool.fetch_add(n << PERMIT_SHIFT, Ordering::Release);
        assert!(
            (prev >> PERMIT_SHIFT) + n <= MAX_PERMITS,
            "released permits exceed MAX_PERMITS"
        );
        if unlikely(prev & HAS_QUEUE != 0) {
            self.drain();
        }
    }

    /// Moves permits from the pool to queued waiters in FIFO order.
    #[cold]
    #[inline(never)]
    fn drain(&self) {
        // No allocation: if the queue pointer is null the waiters are already
        // gone (stale flag) and the permits simply stay in the pool.
        let Some(mut locked) = self.queue.lock(false) else {
            return;
        };

        // Take every permit currently in the pool; leftovers are returned
        // below. The flag bits stay untouched.
        let mut available = 0;
        let mut cur = self.pool.load(Ordering::Relaxed);
        loop {
            if cur >> PERMIT_SHIFT == 0 {
                break;
            }
            match self.pool.compare_exchange_weak(
                cur,
                cur & (CLOSED | HAS_QUEUE),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    available = cur >> PERMIT_SHIFT;
                    break;
                }
                Err(actual) => cur = actual,
            }
        }

        let mut chain = PoppedChain::new();
        locked.with_queue(|queue| {
            while let Some(front) = queue.first::<Signal>() {
                // SAFETY: nodes in the queue are valid; we hold the tag-lock.
                // `aux` is reached through a raw place so no reference to the
                // whole node exists while its owner may poll it.
                let remaining = unsafe { (*front.as_ptr()).aux.load(Ordering::Relaxed) };
                if remaining > available {
                    // The head still needs more than we have: assign what is
                    // there and stop, it blocks everyone behind it (FIFO
                    // fairness, like tokio).
                    if available > 0 {
                        unsafe {
                            (*front.as_ptr())
                                .aux
                                .store(remaining - available, Ordering::Relaxed)
                        };
                        available = 0;
                    }
                    break;
                }
                // Satisfied. This includes heads that need nothing more, e.g.
                // an `acquire_many(0)` that queued behind other waiters.
                available -= remaining;
                unsafe { (*front.as_ptr()).aux.store(0, Ordering::Relaxed) };
                // SAFETY: see above; popped in FIFO order right after the
                // previous appended node.
                unsafe {
                    let node = queue.pop().unwrap();
                    chain.append(node);
                }
            }
        });
        chain.seal();

        // Return unused permits to the pool. `available > 0` implies the
        // queue was fully drained.
        if available > 0 {
            self.pool
                .fetch_add(available << PERMIT_SHIFT, Ordering::Release);
        }

        locked.unlock(|| {
            self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
        });

        // Wake the satisfied waiters after the tag-lock is released so they
        // can immediately cancel/complete without contending on it.
        chain.signal_all(SIGNAL_SIGNALED);
    }

    /// Slow path: retry the acquisition under the queue tag-lock and enqueue
    /// `entry` if the permits are still unavailable.
    ///
    /// The caller must have prepared `entry` (value/waker/aux) for parking.
    ///
    /// # Safety
    ///
    /// `entry` must point to a live node with a `None` link that stays alive
    /// and in place until it is signaled or removed from the queue.
    pub(crate) unsafe fn enqueue_or_acquire(
        &self,
        entry: NonNull<Signal>,
        n: usize,
    ) -> EnqueueResult {
        assert!(
            n <= MAX_PERMITS,
            "requested permits exceed MAX_PERMITS; the acquisition could never succeed"
        );
        let mut locked = self.queue.lock(true).unwrap();
        let queue_empty = locked.with_queue(|q| q.is_empty());
        // Setting the flag and reading the permit count is one RMW on the
        // pool word, so it returns the *latest* value (a plain load could be
        // stale under weak memory): it either happens before a release's
        // `fetch_add` (which then sees the flag and drains the queue) or
        // after it (and we observe its permits below). This is what makes a
        // lost wakeup impossible. If we end up not enqueueing, the unlock
        // clears the flag again.
        let mut cur = self.pool.fetch_or(HAS_QUEUE, Ordering::AcqRel) | HAS_QUEUE;
        loop {
            if unlikely(cur & CLOSED != 0) {
                locked.unlock(|| {
                    self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                });
                return EnqueueResult::Closed;
            }
            // Only the head of the line may take from the pool (FIFO).
            if queue_empty && (cur >> PERMIT_SHIFT) >= n {
                match self.pool.compare_exchange_weak(
                    cur,
                    cur - (n << PERMIT_SHIFT),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        locked.unlock(|| {
                            self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                        });
                        return EnqueueResult::Acquired;
                    }
                    Err(actual) => {
                        // A failed CAS may itself read a stale value, but
                        // that is harmless for liveness: the flag is already
                        // set, so any release will drain the queue and hand
                        // the permits over even if we enqueue spuriously.
                        cur = actual;
                        continue;
                    }
                }
            }
            let first = locked.with_queue(|q| {
                // SAFETY: forwarded caller guarantee: the entry outlives its
                // queue membership.
                unsafe { q.push(entry) }
            });
            locked.unlock(|| {
                self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
            });
            return EnqueueResult::Enqueued(first);
        }
    }

    /// Closes the semaphore: fails all queued waiters and future acquires.
    pub(crate) fn close(&self) {
        let prev = self.pool.fetch_or(CLOSED, Ordering::AcqRel);
        if prev & CLOSED != 0 {
            return;
        }
        if prev & HAS_QUEUE != 0 {
            let Some(mut locked) = self.queue.lock(false) else {
                return;
            };
            let mut chain = PoppedChain::new();
            locked.with_queue(|queue| {
                // SAFETY: queued nodes are alive under the tag-lock; FIFO pop
                // order.
                while let Some(node) = unsafe { queue.pop() } {
                    unsafe { chain.append(node) };
                }
            });
            chain.seal();
            locked.unlock(|| {
                self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
            });
            // Waiters return their partially assigned permits themselves:
            // they know how many they requested, we only know the remainder.
            chain.signal_all(SIGNAL_CLOSED);
        }
    }

    /// Removes up to `n` permits from the pool, returning how many were
    /// actually removed.
    pub(crate) fn forget_permits(&self, n: usize) -> usize {
        let mut cur = self.pool.load(Ordering::Relaxed);
        loop {
            let count = cur >> PERMIT_SHIFT;
            let forget = count.min(n);
            if forget == 0 {
                return 0;
            }
            match self.pool.compare_exchange_weak(
                cur,
                cur - (forget << PERMIT_SHIFT),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return forget,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Blocking acquisition of `n` permits.
    #[cfg(any(feature = "std", loom))]
    pub(crate) fn acquire_sync(&self, n: usize) -> Result<(), AcquireError> {
        if likely(self.try_acquire(n).is_ok()) {
            return Ok(());
        }
        self.acquire_sync_slow(n)
    }

    #[cfg(any(feature = "std", loom))]
    #[cold]
    #[inline(never)]
    fn acquire_sync_slow(&self, n: usize) -> Result<(), AcquireError> {
        match self.try_acquire(n) {
            Ok(()) => return Ok(()),
            Err(TryAcquireError::Closed) => return Err(AcquireError(())),
            Err(TryAcquireError::NoPermits) => {}
        }
        let mut entry = Signal::new_sync();
        entry.aux.store(n, Ordering::Relaxed);
        // SAFETY: the entry outlives its queue membership: this function does
        // not return before the entry is signaled (and thereby dequeued). The
        // pointer is derived without a `&mut` to the node so the reads of
        // `entry.value` below alias the queued node soundly.
        let node = unsafe { NonNull::new_unchecked(&raw mut entry) };
        let first = match unsafe { self.enqueue_or_acquire(node, n) } {
            EnqueueResult::Acquired => return Ok(()),
            EnqueueResult::Closed => return Err(AcquireError(())),
            EnqueueResult::Enqueued(first) => first,
        };

        // If we are first in line we spin briefly before parking: the permits
        // often arrive within a few hundred cycles.
        let mut value;
        if first {
            let backoff = Backoff::new();
            loop {
                value = entry.value.load(Ordering::Acquire);
                if value >= SIGNAL_SIGNALED {
                    return self.finish_sync(&entry, n, value);
                }
                if backoff.is_completed() {
                    break;
                }
                backoff.snooze();
            }
        }
        value = entry.value.swap(SIGNAL_INIT_WAITING, Ordering::AcqRel);
        if likely(value >= SIGNAL_SIGNALED) {
            return self.finish_sync(&entry, n, value);
        }
        loop {
            crate::shim::thread::park();
            value = entry.value.load(Ordering::Acquire);
            if value >= SIGNAL_SIGNALED {
                return self.finish_sync(&entry, n, value);
            }
        }
    }

    #[cfg(any(feature = "std", loom))]
    fn finish_sync(&self, entry: &Signal, n: usize, value: usize) -> Result<(), AcquireError> {
        if likely(value == SIGNAL_SIGNALED) {
            Ok(())
        } else {
            // Closed while waiting: return the permits that were partially
            // assigned to us before the closure.
            let granted = n - entry.aux.load(Ordering::Relaxed);
            self.release(granted);
            Err(AcquireError(()))
        }
    }

    /// Polls an asynchronous acquisition of `n` permits using `entry` as the
    /// wait-queue node.
    ///
    /// # Safety
    ///
    /// `entry` must point to a live node that stays pinned for the whole
    /// acquisition and, if this ever returned `Poll::Pending`,
    /// [`Self::drop_acquire`] must be called before the node is invalidated
    /// (unless the acquisition completed).
    pub(crate) unsafe fn poll_acquire(
        &self,
        entry: NonNull<Signal>,
        n: usize,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), AcquireError>> {
        // SAFETY (all raw accesses below): the caller guarantees the node is
        // alive; fields are reached through raw places so no reference to the
        // whole node is held while a signaler may touch it.
        let node = entry.as_ptr();
        let mut sig_val = unsafe { (*node).value.load(Ordering::Acquire) };
        if sig_val == SIGNAL_INIT_WAITING {
            // Queued: re-arm the waker, or learn that the signal is already
            // in flight and consume it below.
            match unsafe { rearm(entry, cx) } {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(value) => sig_val = value,
            }
        }
        if likely(sig_val >= SIGNAL_SIGNALED) {
            if unlikely(sig_val == SIGNAL_RETURNED) {
                unreachable!("acquire polled after completion");
            }
            unsafe { (*node).value.store(SIGNAL_RETURNED, Ordering::Relaxed) };
            if unlikely(sig_val == SIGNAL_CLOSED) {
                let granted = n - unsafe { (*node).aux.load(Ordering::Relaxed) };
                self.release(granted);
                return Poll::Ready(Err(AcquireError(())));
            }
            return Poll::Ready(Ok(()));
        }
        debug_assert_eq!(sig_val, SIGNAL_UNINIT);
        match self.try_acquire(n) {
            Ok(()) => {
                unsafe { (*node).value.store(SIGNAL_RETURNED, Ordering::Relaxed) };
                return Poll::Ready(Ok(()));
            }
            Err(TryAcquireError::Closed) => {
                unsafe { (*node).value.store(SIGNAL_RETURNED, Ordering::Relaxed) };
                return Poll::Ready(Err(AcquireError(())));
            }
            Err(TryAcquireError::NoPermits) => {}
        }
        unsafe {
            (*node).value.store(SIGNAL_INIT_WAITING, Ordering::Release);
            (*node).waker.register(cx.waker());
            (*node).aux.store(n, Ordering::Relaxed);
        }
        // SAFETY: forwarded caller guarantee (pin + cancel-on-drop).
        match unsafe { self.enqueue_or_acquire(entry, n) } {
            EnqueueResult::Acquired => {
                unsafe { (*node).value.store(SIGNAL_RETURNED, Ordering::Relaxed) };
                Poll::Ready(Ok(()))
            }
            EnqueueResult::Closed => {
                unsafe { (*node).value.store(SIGNAL_RETURNED, Ordering::Relaxed) };
                Poll::Ready(Err(AcquireError(())))
            }
            EnqueueResult::Enqueued(_) => Poll::Pending,
        }
    }

    /// Cleans up after a dropped acquisition future.
    ///
    /// Handles every state: removes a still-queued entry, waits out an
    /// in-flight signal, and returns any permits that were already granted
    /// (fully or partially) to the pool.
    ///
    /// # Safety
    ///
    /// `entry` must be the same entry passed to `poll_acquire` and must not
    /// be used afterwards.
    #[cold]
    #[inline(never)]
    pub(crate) unsafe fn drop_acquire(&self, entry: NonNull<Signal>, n: usize) {
        // SAFETY: forwarded caller guarantee (the node is alive); raw place
        // accesses, see `poll_acquire`.
        let node = entry.as_ptr();
        match unsafe { (*node).value.load(Ordering::Acquire) } {
            SIGNAL_INIT_WAITING => {
                // SAFETY: forwarded caller guarantee.
                unsafe { self.cancel_acquire(entry, n) }
            }
            // Granted but never observed by a poll: give the permits back.
            SIGNAL_SIGNALED => self.release(n),
            SIGNAL_CLOSED => {
                let granted = n - unsafe { (*node).aux.load(Ordering::Relaxed) };
                self.release(granted);
            }
            // SIGNAL_UNINIT (never enqueued) or SIGNAL_RETURNED (completed).
            _ => {}
        }
    }

    /// Cancels a pending asynchronous acquisition (drop of the future).
    ///
    /// Either removes the entry from the wait queue, or — if a signaler
    /// already popped it — waits for the in-flight signal and returns the
    /// granted permits to the pool.
    ///
    /// # Safety
    ///
    /// `entry` must be the same entry passed to `poll_acquire`.
    unsafe fn cancel_acquire(&self, entry: NonNull<Signal>, n: usize) {
        // SAFETY: forwarded caller guarantee (the node is alive); raw place
        // accesses, see `poll_acquire`.
        let node = entry.as_ptr();
        if let Some(mut locked) = self.queue.lock(false) {
            let (found, head_changed) = locked.with_queue(|q| {
                let was_head = q.first::<Signal>() == Some(entry);
                // SAFETY: queued nodes are alive under the tag-lock; our own
                // node may or may not be queued and is compared by address
                // only.
                let found = unsafe { q.remove(entry) };
                (found, found && was_head && !q.is_empty())
            });
            if found {
                let granted = n - unsafe { (*node).aux.load(Ordering::Relaxed) };
                locked.unlock(|| {
                    self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                });
                if granted > 0 {
                    // Permits partially assigned to us must be redistributed;
                    // `release` drains the queue for the waiters behind us.
                    self.release(granted);
                } else if head_changed {
                    // We were at the head of the line with nothing assigned
                    // yet, so the permits we were waiting for may already sit
                    // in the pool: the new head must be given its chance at
                    // them, which only a drain does.
                    self.drain();
                }
                return;
            }
            locked.unlock(|| {
                self.pool.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
            });
        }
        // We were already popped by a signaler; the signal is in flight.
        // Wait for it and give the granted permits back.
        let backoff = Backoff::new();
        let mut value = unsafe { (*node).value.load(Ordering::Acquire) };
        while value < SIGNAL_SIGNALED {
            backoff.snooze();
            value = unsafe { (*node).value.load(Ordering::Acquire) };
        }
        let granted = if value == SIGNAL_CLOSED {
            n - unsafe { (*node).aux.load(Ordering::Relaxed) }
        } else {
            n
        };
        self.release(granted);
    }
}

/// Error returned when acquiring permits from a closed semaphore.
///
/// Returned by [`Semaphore::acquire`], [`AsyncSemaphore::acquire`] and
/// related methods after [`Semaphore::close`]/[`AsyncSemaphore::close`] has
/// been called.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct AcquireError(());

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("semaphore closed")
    }
}

impl core::error::Error for AcquireError {}

/// Error returned by non-blocking permit acquisition.
///
/// Returned by [`Semaphore::try_acquire`], [`AsyncSemaphore::try_acquire`]
/// and related methods.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TryAcquireError {
    /// The semaphore has been closed and no further permits can be acquired.
    Closed,
    /// The semaphore has insufficient available permits (or waiters are
    /// queued ahead, which would make taking permits unfair).
    NoPermits,
}

impl fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryAcquireError::Closed => f.write_str("semaphore closed"),
            TryAcquireError::NoPermits => f.write_str("no permits available"),
        }
    }
}

impl core::error::Error for TryAcquireError {}

/// A synchronous counting semaphore that limits concurrent access to a
/// resource.
///
/// This semaphore is fair (strict FIFO) and uses the same allocation-free
/// wait-queue algorithm as [`crate::Mutex`]: waiters are stack-allocated
/// intrusive nodes and the only heap object — the queue itself — is pooled
/// and only present while waiters exist.
///
/// The layout is identical to [`AsyncSemaphore`] (`#[repr(C)]`), enabling
/// free conversion between the two.
///
/// # Examples
///
/// ```
/// use xutex::Semaphore;
///
/// let semaphore = Semaphore::new(3);
/// let permit = semaphore.acquire().unwrap();
/// assert_eq!(semaphore.available_permits(), 2);
/// drop(permit);
/// assert_eq!(semaphore.available_permits(), 3);
/// ```
#[cfg(feature = "std")]
#[repr(C)]
pub struct Semaphore {
    core: SemCore,
}

/// An asynchronous counting semaphore that limits concurrent access to a
/// resource.
///
/// The async counterpart of [`Semaphore`]: acquisition returns a future and
/// parked tasks are woken through their [`core::task::Waker`], making it
/// usable on any async runtime. It is fair (strict FIFO), cancellation-safe
/// (dropping an [`AsyncAcquireRequest`] cleanly removes it from the queue or
/// returns already-assigned permits) and allocation-free on the waiter path.
///
/// # Examples
///
/// ```
/// use xutex::AsyncSemaphore;
/// use swait::*;
///
/// async fn example() {
///     let semaphore = AsyncSemaphore::new(2);
///     let permit = semaphore.acquire().await.unwrap();
///     assert_eq!(semaphore.available_permits(), 1);
///     drop(permit);
/// }
/// example().swait();
/// ```
#[repr(C)]
pub struct AsyncSemaphore {
    core: SemCore,
}

macro_rules! semaphore_common {
    ($name:ident) => {
        impl $name {
            /// The maximum number of permits the semaphore can hold.
            pub const MAX_PERMITS: usize = MAX_PERMITS;

            /// Returns the number of permits currently available.
            #[inline(always)]
            pub fn available_permits(&self) -> usize {
                self.core.available_permits()
            }

            /// Adds `n` new permits to the semaphore, waking waiters that can
            /// now be satisfied.
            ///
            /// # Panics
            ///
            /// Panics if the permit count would exceed
            /// [`MAX_PERMITS`](Self::MAX_PERMITS).
            #[inline(always)]
            pub fn add_permits(&self, n: usize) {
                self.core.release(n);
            }

            /// Removes up to `n` permits from the pool without blocking and
            /// returns the number actually removed.
            #[inline(always)]
            pub fn forget_permits(&self, n: usize) -> usize {
                self.core.forget_permits(n)
            }

            /// Closes the semaphore.
            ///
            /// All queued waiters fail with [`AcquireError`] and every
            /// subsequent acquisition attempt fails immediately. Outstanding
            /// permits can still be released back.
            #[inline(always)]
            pub fn close(&self) {
                self.core.close();
            }

            /// Returns `true` if the semaphore has been closed.
            #[inline(always)]
            pub fn is_closed(&self) -> bool {
                self.core.is_closed()
            }

            /// Attempts to acquire one permit without waiting.
            #[inline(always)]
            pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
                self.try_acquire_many(1)
            }

            /// Attempts to acquire `n` permits without waiting.
            #[inline(always)]
            pub fn try_acquire_many(
                &self,
                n: usize,
            ) -> Result<SemaphorePermit<'_>, TryAcquireError> {
                self.core.try_acquire(n)?;
                Ok(SemaphorePermit {
                    core: &self.core,
                    permits: n,
                })
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("available_permits", &self.available_permits())
                    .field("closed", &self.is_closed())
                    .finish()
            }
        }
    };
}

#[cfg(feature = "std")]
semaphore_common!(Semaphore);
semaphore_common!(AsyncSemaphore);

#[cfg(feature = "std")]
impl Semaphore {
    /// Creates a new semaphore with the given number of permits.
    ///
    /// # Panics
    ///
    /// Panics if `permits` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new(permits: usize) -> Self {
        Self {
            core: SemCore::new(permits),
        }
    }

    /// Creates a new semaphore (loom model-checking build).
    #[cfg(loom)]
    pub fn new(permits: usize) -> Self {
        Self {
            core: SemCore::new(permits),
        }
    }

    /// Acquires one permit, blocking the current thread until it is
    /// available.
    ///
    /// Returns an error if the semaphore is (or becomes) closed.
    #[inline(always)]
    pub fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.acquire_many(1)
    }

    /// Acquires `n` permits, blocking the current thread until all of them
    /// are available (they are handed over atomically, FIFO).
    ///
    /// Returns an error if the semaphore is (or becomes) closed.
    #[inline(always)]
    pub fn acquire_many(&self, n: usize) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.core.acquire_sync(n)?;
        Ok(SemaphorePermit {
            core: &self.core,
            permits: n,
        })
    }

    /// Acquires one permit asynchronously; see [`AsyncSemaphore::acquire`].
    #[inline(always)]
    pub fn acquire_async(&self) -> AsyncAcquireRequest<'_> {
        self.as_async().acquire()
    }

    /// Views this semaphore as an [`AsyncSemaphore`].
    #[inline(always)]
    pub fn as_async(&self) -> &AsyncSemaphore {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const Semaphore as *const AsyncSemaphore) }
    }

    /// Converts this semaphore into an [`AsyncSemaphore`] without allocating.
    #[inline(always)]
    pub fn to_async(self) -> AsyncSemaphore {
        let Semaphore { core } = self;
        AsyncSemaphore { core }
    }

    /// Converts an `Arc<Semaphore>` into an `Arc<AsyncSemaphore>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn to_async_arc(self: Arc<Self>) -> Arc<AsyncSemaphore> {
        let raw = Arc::into_raw(self) as *const AsyncSemaphore;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<Semaphore>` as an `Arc<AsyncSemaphore>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn clone_async(self: &Arc<Self>) -> Arc<AsyncSemaphore> {
        Arc::clone(self).to_async_arc()
    }
}

impl AsyncSemaphore {
    /// Creates a new asynchronous semaphore with the given number of permits.
    ///
    /// # Panics
    ///
    /// Panics if `permits` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new(permits: usize) -> Self {
        Self {
            core: SemCore::new(permits),
        }
    }

    /// Creates a new asynchronous semaphore (loom model-checking build).
    #[cfg(loom)]
    pub fn new(permits: usize) -> Self {
        Self {
            core: SemCore::new(permits),
        }
    }

    /// Acquires one permit asynchronously.
    ///
    /// The returned future resolves to a [`SemaphorePermit`] or an
    /// [`AcquireError`] if the semaphore is closed. Dropping the future
    /// before completion is safe: it removes itself from the wait queue (or
    /// returns any permits that were already handed to it).
    #[inline(always)]
    pub fn acquire(&self) -> AsyncAcquireRequest<'_> {
        self.acquire_many(1)
    }

    /// Acquires `n` permits asynchronously (handed over atomically, FIFO).
    #[inline(always)]
    pub fn acquire_many(&self, n: usize) -> AsyncAcquireRequest<'_> {
        AsyncAcquireRequest {
            core: &self.core,
            entry: Signal::new_none(),
            permits: n,
            _pinned: PhantomPinned,
        }
    }

    /// Acquires one permit, blocking the current thread; see
    /// [`Semaphore::acquire`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn acquire_sync(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.as_sync().acquire()
    }

    /// Views this semaphore as a synchronous [`Semaphore`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn as_sync(&self) -> &Semaphore {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const AsyncSemaphore as *const Semaphore) }
    }

    /// Converts this semaphore into a synchronous [`Semaphore`] without
    /// allocating.
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn to_sync(self) -> Semaphore {
        let AsyncSemaphore { core } = self;
        Semaphore { core }
    }

    /// Converts an `Arc<AsyncSemaphore>` into an `Arc<Semaphore>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn to_sync_arc(self: Arc<Self>) -> Arc<Semaphore> {
        let raw = Arc::into_raw(self) as *const Semaphore;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<AsyncSemaphore>` as an `Arc<Semaphore>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn clone_sync(self: &Arc<Self>) -> Arc<Semaphore> {
        Arc::clone(self).to_sync_arc()
    }
}

/// A permit acquired from a [`Semaphore`] or [`AsyncSemaphore`].
///
/// The permits are returned to the semaphore when this is dropped, unless
/// [`forget`](Self::forget) is called.
#[must_use = "permits are released back to the semaphore when dropped"]
pub struct SemaphorePermit<'a> {
    core: &'a SemCore,
    permits: usize,
}

impl<'a> SemaphorePermit<'a> {
    /// Returns the number of permits held by this permit object.
    #[inline(always)]
    pub fn num_permits(&self) -> usize {
        self.permits
    }

    /// Forgets the permit: the permits are permanently removed from the
    /// semaphore instead of being returned on drop.
    #[inline(always)]
    pub fn forget(mut self) {
        self.permits = 0;
    }

    /// Splits `n` permits off into a new permit object, or returns `None`
    /// if fewer than `n` permits are held.
    pub fn split(&mut self, n: usize) -> Option<SemaphorePermit<'a>> {
        if n > self.permits {
            return None;
        }
        self.permits -= n;
        Some(SemaphorePermit {
            core: self.core,
            permits: n,
        })
    }

    /// Merges another permit from the same semaphore into this one.
    ///
    /// # Panics
    ///
    /// Panics if the permits come from different semaphores.
    pub fn merge(&mut self, mut other: SemaphorePermit<'a>) {
        assert!(
            core::ptr::eq(self.core, other.core),
            "merging permits from different semaphores"
        );
        self.permits += other.permits;
        other.permits = 0;
    }
}

impl fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("permits", &self.permits)
            .finish()
    }
}

impl Drop for SemaphorePermit<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.core.release(self.permits);
    }
}

/// A future that resolves once the requested number of semaphore permits has
/// been acquired.
///
/// Created by [`AsyncSemaphore::acquire`], [`AsyncSemaphore::acquire_many`]
/// and [`Semaphore::acquire_async`].
///
/// Like [`crate::AsyncLockRequest`] it is `!Unpin` (it embeds the intrusive
/// wait-queue node) and cancellation-safe: dropping it while pending removes
/// it from the queue, or — if permits were already handed over — returns
/// them to the semaphore.
#[must_use = "futures do nothing unless polled"]
pub struct AsyncAcquireRequest<'a> {
    core: &'a SemCore,
    entry: Signal,
    permits: usize,
    _pinned: PhantomPinned,
}

impl<'a> Future for AsyncAcquireRequest<'a> {
    type Output = Result<SemaphorePermit<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we never move out of `this`; the entry stays pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let core = this.core;
        let permits = this.permits;
        // SAFETY: the entry is pinned inside this future and
        // `drop_acquire` runs on drop.
        match unsafe { core.poll_acquire(NonNull::new_unchecked(&raw mut this.entry), permits, cx) }
        {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(SemaphorePermit { core, permits })),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for AsyncAcquireRequest<'_> {
    fn drop(&mut self) {
        let value = self.entry.value.load(Ordering::Acquire);
        if unlikely(value != SIGNAL_UNINIT && value != SIGNAL_RETURNED) {
            // SAFETY: same entry as passed to poll_acquire.
            unsafe {
                self.core
                    .drop_acquire(NonNull::new_unchecked(&raw mut self.entry), self.permits)
            };
        }
    }
}

unsafe impl Send for AsyncAcquireRequest<'_> {}

// The semaphore carries no data, only counters; it is freely shareable.
#[cfg(feature = "std")]
unsafe impl Send for Semaphore {}
#[cfg(feature = "std")]
unsafe impl Sync for Semaphore {}
unsafe impl Send for AsyncSemaphore {}
unsafe impl Sync for AsyncSemaphore {}
unsafe impl Send for SemaphorePermit<'_> {}
unsafe impl Sync for SemaphorePermit<'_> {}
