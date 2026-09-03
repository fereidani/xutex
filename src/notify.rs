//! Task/thread notification primitive, split into a synchronous [`Notify`]
//! and an asynchronous [`AsyncNotify`] with identical memory layout.
//!
//! Semantics follow `tokio::sync::Notify`:
//!
//! * [`notify_one`](AsyncNotify::notify_one) wakes one waiter, or stores a
//!   single permit that the next wait consumes immediately.
//! * [`notify_waiters`](AsyncNotify::notify_waiters) wakes all current waiters
//!   (and completes every [`AsyncNotified`] future created before the call)
//!   without storing a permit.
//! * A notified-but-dropped [`AsyncNotified`] future passes its wakeup on to
//!   the next waiter.
//!
//! The state is a single atomic word `generation << 3 | PERMIT | HAS_QUEUE`
//! plus the same allocation-free intrusive wait queue used by every other
//! primitive in this crate.

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
use crate::wait_queue::{PoppedChain, WaitQueue, signal_node};
use crate::{
    SIGNAL_ALL, SIGNAL_INIT_WAITING, SIGNAL_RETURNED, SIGNAL_SIGNALED, SIGNAL_UNINIT, Signal,
};

/// Bit 1: a wait queue exists (bit 0 is reserved/unused so the flag layout
/// matches the other primitives).
const HAS_QUEUE: usize = 0b010;
/// Bit 2: a stored notification permit from `notify_one`.
const PERMIT: usize = 0b100;
/// The `notify_waiters` generation counter lives above the flag bits.
const GEN_SHIFT: u32 = 3;
const GEN_ONE: usize = 1 << GEN_SHIFT;

#[inline(always)]
fn generation(state: usize) -> usize {
    state >> GEN_SHIFT
}

pub(crate) struct NotifyCore {
    state: AtomicUsize,
    queue: WaitQueue,
}

enum WaitOutcome {
    /// The wait completed without parking (permit consumed or generation
    /// advanced).
    Completed,
    /// The entry was pushed to the wait queue.
    Enqueued,
}

impl NotifyCore {
    const_fn! {
        pub(crate) const fn new() -> Self {
            Self {
                state: AtomicUsize::new(0),
                queue: WaitQueue::new(),
            }
        }
    }

    /// Fast path of a wait: consume a stored permit or detect a generation
    /// advance. Returns `true` when the wait is already complete.
    fn try_complete(&self, captured_gen: usize) -> bool {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            if generation(cur) != captured_gen {
                // A notify_waiters call happened after this wait began.
                return true;
            }
            if cur & PERMIT == 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                cur,
                cur & !PERMIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Slow path: enqueue `entry` under the queue tag-lock, unless the wait
    /// completes while doing so.
    ///
    /// The caller must have prepared `entry` (value/waker) for parking.
    ///
    /// # Safety
    ///
    /// `entry` must stay alive until it is signaled or removed.
    unsafe fn enqueue_or_complete(&self, entry: &mut Signal, captured_gen: usize) -> WaitOutcome {
        let mut locked = self.queue.lock(true).unwrap();
        // Setting the flag is an RMW on the state word, so it returns the
        // *latest* generation/permit: either this happens before a
        // notifier's RMW (which then sees the flag and drains the queue) or
        // after it (and we observe its effect here). This makes lost
        // wakeups impossible.
        let mut cur = self.state.fetch_or(HAS_QUEUE, Ordering::AcqRel) | HAS_QUEUE;
        loop {
            if generation(cur) != captured_gen {
                locked.unlock(|| {
                    self.state.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                });
                return WaitOutcome::Completed;
            }
            if cur & PERMIT != 0 {
                match self.state.compare_exchange_weak(
                    cur,
                    cur & !PERMIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        locked.unlock(|| {
                            self.state.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                        });
                        return WaitOutcome::Completed;
                    }
                    Err(actual) => {
                        cur = actual;
                        continue;
                    }
                }
            }
            entry.aux.store(captured_gen, Ordering::Relaxed);
            locked.with_queue(|q| {
                // SAFETY: forwarded caller guarantee.
                unsafe { q.push(NonNull::new_unchecked(entry)) }
            });
            locked.unlock(|| {
                self.state.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
            });
            return WaitOutcome::Enqueued;
        }
    }

    /// Wakes one waiter, or stores a permit for the next wait.
    pub(crate) fn notify_one(&self) {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            if cur & HAS_QUEUE != 0 {
                match self.queue.lock(false) {
                    Some(mut locked) => {
                        let popped = locked.with_queue(|q| q.pop());
                        locked.unlock(|| {
                            self.state.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                        });
                        match popped {
                            Some(node) => {
                                // SAFETY: the node was popped under the
                                // tag-lock and is exclusively ours.
                                unsafe { signal_node(node, SIGNAL_SIGNALED) };
                                return;
                            }
                            None => {
                                // Published queues are never empty; only a
                                // stale flag can lead here.
                                cur = self.state.load(Ordering::Relaxed);
                                continue;
                            }
                        }
                    }
                    None => {
                        // Stale flag: the queue vanished. Re-read and retry.
                        cur = self.state.load(Ordering::Relaxed);
                        continue;
                    }
                }
            }
            if cur & PERMIT != 0 {
                // A permit is already stored; notify_one saturates at one.
                return;
            }
            match self.state.compare_exchange_weak(
                cur,
                cur | PERMIT,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Wakes all current waiters without storing a permit.
    pub(crate) fn notify_waiters(&self) {
        let prev = self.state.fetch_add(GEN_ONE, Ordering::AcqRel);
        if likely(prev & HAS_QUEUE == 0) {
            return;
        }
        let Some(mut locked) = self.queue.lock(false) else {
            return;
        };
        // Waiters enqueued for the *current* generation (concurrent bumps
        // included) must stay; everything older is released. FIFO order
        // guarantees old-generation entries form a prefix.
        let live_gen = generation(self.state.load(Ordering::Relaxed));
        let mut chain = PoppedChain::new();
        locked.with_queue(|q| {
            while let Some(front) = q.first::<Signal>() {
                // SAFETY: nodes in the queue are valid under the tag-lock.
                if unsafe { front.as_ref() }.aux.load(Ordering::Relaxed) == live_gen {
                    break;
                }
                let node = q.pop().unwrap();
                // SAFETY: FIFO pop order.
                unsafe { chain.append(node) };
            }
        });
        chain.seal();
        locked.unlock(|| {
            self.state.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
        });
        chain.signal_all(SIGNAL_ALL);
    }

    /// Blocking wait for a notification.
    #[cfg(any(feature = "std", loom))]
    pub(crate) fn wait(&self) {
        let captured_gen = generation(self.state.load(Ordering::Acquire));
        if self.try_complete(captured_gen) {
            return;
        }
        self.wait_slow(captured_gen);
    }

    #[cfg(any(feature = "std", loom))]
    #[cold]
    #[inline(never)]
    fn wait_slow(&self, captured_gen: usize) {
        let mut entry = Signal::new_sync();
        // SAFETY: the entry outlives its queue membership: this function
        // does not return before the entry is signaled.
        match unsafe { self.enqueue_or_complete(&mut entry, captured_gen) } {
            WaitOutcome::Completed => (),
            WaitOutcome::Enqueued => {
                if entry.value.swap(SIGNAL_INIT_WAITING, Ordering::AcqRel) >= SIGNAL_SIGNALED {
                    return;
                }
                loop {
                    crate::shim::thread::park();
                    if entry.value.load(Ordering::Acquire) >= SIGNAL_SIGNALED {
                        return;
                    }
                }
            }
        }
    }
}

/// A synchronous notification primitive: threads [`wait`](Notify::wait) for
/// a wakeup sent by [`notify_one`](Notify::notify_one) or
/// [`notify_waiters`](Notify::notify_waiters).
///
/// The synchronous counterpart of [`AsyncNotify`] (identical layout,
/// `#[repr(C)]`, freely convertible). Waiters park on stack-allocated
/// intrusive nodes: no allocation per wait.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use std::thread;
/// use xutex::Notify;
///
/// let notify = Arc::new(Notify::new());
/// let handle = {
///     let notify = Arc::clone(&notify);
///     thread::spawn(move || notify.wait())
/// };
/// notify.notify_one();
/// handle.join().unwrap();
/// ```
#[cfg(feature = "std")]
#[repr(C)]
pub struct Notify {
    core: NotifyCore,
}

/// An asynchronous notification primitive: tasks await
/// [`notified`](AsyncNotify::notified) for a wakeup sent by
/// [`notify_one`](AsyncNotify::notify_one) or
/// [`notify_waiters`](AsyncNotify::notify_waiters).
///
/// Semantics match `tokio::sync::Notify`, including the guarantee that an
/// [`AsyncNotified`] future created *before* a `notify_waiters` call
/// completes immediately when polled afterwards, and that dropping a
/// `notify_one`-woken future passes the wakeup to the next waiter.
///
/// # Examples
///
/// ```
/// use xutex::AsyncNotify;
/// use swait::*;
///
/// async fn example() {
///     let notify = AsyncNotify::new();
///     notify.notify_one(); // store a permit
///     notify.notified().await; // consumes it without waiting
/// }
/// example().swait();
/// ```
#[repr(C)]
pub struct AsyncNotify {
    core: NotifyCore,
}

macro_rules! notify_common {
    ($name:ident) => {
        impl $name {
            /// Wakes one waiter; if none is waiting, stores a single permit
            /// that the next wait consumes immediately.
            #[inline(always)]
            pub fn notify_one(&self) {
                self.core.notify_one();
            }

            /// Wakes all current waiters without storing a permit.
            ///
            /// Pending wait futures created before this call also complete.
            #[inline(always)]
            pub fn notify_waiters(&self) {
                self.core.notify_waiters();
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    };
}

#[cfg(feature = "std")]
notify_common!(Notify);
notify_common!(AsyncNotify);

#[cfg(feature = "std")]
impl Notify {
    /// Creates a new synchronous notifier.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            core: NotifyCore::new(),
        }
    }

    /// Creates a new synchronous notifier (loom model-checking build).
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            core: NotifyCore::new(),
        }
    }

    /// Blocks the current thread until notified.
    ///
    /// Consumes a stored permit immediately if one is present.
    #[inline(always)]
    pub fn wait(&self) {
        self.core.wait();
    }

    /// Waits for a notification asynchronously; see
    /// [`AsyncNotify::notified`].
    #[inline(always)]
    pub fn notified(&self) -> AsyncNotified<'_> {
        self.as_async().notified()
    }

    /// Views this notifier as an [`AsyncNotify`].
    #[inline(always)]
    pub fn as_async(&self) -> &AsyncNotify {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const Notify as *const AsyncNotify) }
    }

    /// Converts this notifier into an [`AsyncNotify`] without allocating.
    #[inline(always)]
    pub fn to_async(self) -> AsyncNotify {
        let Notify { core } = self;
        AsyncNotify { core }
    }

    /// Converts an `Arc<Notify>` into an `Arc<AsyncNotify>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn to_async_arc(self: Arc<Self>) -> Arc<AsyncNotify> {
        let raw = Arc::into_raw(self) as *const AsyncNotify;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<Notify>` as an `Arc<AsyncNotify>` without allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn clone_async(self: &Arc<Self>) -> Arc<AsyncNotify> {
        Arc::clone(self).to_async_arc()
    }
}

impl AsyncNotify {
    /// Creates a new asynchronous notifier.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            core: NotifyCore::new(),
        }
    }

    /// Creates a new asynchronous notifier (loom model-checking build).
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            core: NotifyCore::new(),
        }
    }

    /// Returns a future that completes when this notifier is notified.
    ///
    /// The future is "armed" at creation: a
    /// [`notify_waiters`](Self::notify_waiters) call between creating the
    /// future and awaiting it completes it immediately.
    #[inline(always)]
    pub fn notified(&self) -> AsyncNotified<'_> {
        AsyncNotified {
            core: &self.core,
            captured_gen: generation(self.core.state.load(Ordering::Acquire)),
            entry: Signal::new_none(),
            _pinned: PhantomPinned,
        }
    }

    /// Blocks the current thread until notified; see [`Notify::wait`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn wait_sync(&self) {
        self.core.wait();
    }

    /// Views this notifier as a synchronous [`Notify`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn as_sync(&self) -> &Notify {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const AsyncNotify as *const Notify) }
    }

    /// Converts this notifier into a synchronous [`Notify`] without
    /// allocating.
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn to_sync(self) -> Notify {
        let AsyncNotify { core } = self;
        Notify { core }
    }

    /// Converts an `Arc<AsyncNotify>` into an `Arc<Notify>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn to_sync_arc(self: Arc<Self>) -> Arc<Notify> {
        let raw = Arc::into_raw(self) as *const Notify;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<AsyncNotify>` as an `Arc<Notify>` without allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn clone_sync(self: &Arc<Self>) -> Arc<Notify> {
        Arc::clone(self).to_sync_arc()
    }
}

/// A future that completes when its [`AsyncNotify`]/[`Notify`] is notified.
///
/// Created by [`AsyncNotify::notified`] and [`Notify::notified`]. `!Unpin`
/// (it embeds the intrusive wait-queue node) and cancellation-safe: if the
/// future is dropped after consuming a `notify_one` wakeup without
/// completing, the wakeup is passed on to the next waiter.
#[must_use = "futures do nothing unless polled"]
pub struct AsyncNotified<'a> {
    core: &'a NotifyCore,
    captured_gen: usize,
    entry: Signal,
    _pinned: PhantomPinned,
}

impl Future for AsyncNotified<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we never move out of `this`; the entry stays pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let sig_val = this.entry.value.load(Ordering::Acquire);
        if likely(sig_val >= SIGNAL_SIGNALED) {
            if unlikely(sig_val == SIGNAL_RETURNED) {
                unreachable!("notified future polled after completion");
            }
            this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
            return Poll::Ready(());
        }
        if sig_val == SIGNAL_INIT_WAITING {
            this.entry.waker.register(cx.waker());
            return Poll::Pending;
        }
        debug_assert_eq!(sig_val, SIGNAL_UNINIT);
        if this.core.try_complete(this.captured_gen) {
            this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
            return Poll::Ready(());
        }
        this.entry
            .value
            .store(SIGNAL_INIT_WAITING, Ordering::Release);
        this.entry.waker.register(cx.waker());
        // SAFETY: entry is pinned inside this future and the drop
        // implementation removes it from the queue.
        match unsafe {
            this.core
                .enqueue_or_complete(&mut this.entry, this.captured_gen)
        } {
            WaitOutcome::Completed => {
                this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                Poll::Ready(())
            }
            WaitOutcome::Enqueued => Poll::Pending,
        }
    }
}

impl Drop for AsyncNotified<'_> {
    fn drop(&mut self) {
        let value = self.entry.value.load(Ordering::Acquire);
        if likely(value == SIGNAL_UNINIT || value == SIGNAL_RETURNED) {
            return;
        }
        self.drop_slow(value);
    }
}

impl AsyncNotified<'_> {
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self, mut value: usize) {
        if value == SIGNAL_INIT_WAITING {
            // Try to leave the wait queue.
            if let Some(mut locked) = self.core.queue.lock(false) {
                let found = locked.with_queue(|q| {
                    // SAFETY: node address comparison only.
                    q.remove(unsafe { NonNull::new_unchecked(&mut self.entry) })
                });
                locked.unlock(|| {
                    self.core.state.fetch_and(!HAS_QUEUE, Ordering::AcqRel);
                });
                if found {
                    return;
                }
            }
            // Already popped: the signal is in flight, wait for it.
            let backoff = Backoff::new();
            value = self.entry.value.load(Ordering::Acquire);
            while value < SIGNAL_SIGNALED {
                backoff.snooze();
                value = self.entry.value.load(Ordering::Acquire);
            }
        }
        if value == SIGNAL_SIGNALED {
            // We consumed a notify_one wakeup without completing; pass it on
            // (tokio semantics).
            self.core.notify_one();
        }
    }
}

unsafe impl Send for AsyncNotified<'_> {}

#[cfg(feature = "std")]
unsafe impl Send for Notify {}
#[cfg(feature = "std")]
unsafe impl Sync for Notify {}
unsafe impl Send for AsyncNotify {}
unsafe impl Sync for AsyncNotify {}
