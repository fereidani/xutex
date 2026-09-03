//! Barrier synchronization primitive, split into a synchronous [`Barrier`]
//! and an asynchronous [`AsyncBarrier`] with identical memory layout.
//!
//! A barrier of size `n` blocks the first `n - 1` arrivals until the `n`-th
//! arrives; then all are released together and exactly one of them observes
//! [`BarrierWaitResult::is_leader`] `== true`. The barrier is reusable: it
//! resets for the next round (generation) immediately.
//!
//! The state is a single atomic word `generation << HALF | arrived` plus the
//! same allocation-free intrusive wait queue used by every primitive in this
//! crate. Unlike `tokio::sync::Barrier`, dropping a pending wait future
//! *withdraws* the arrival, so cancellation cannot brick the barrier.

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
use crate::{SIGNAL_ALL, SIGNAL_INIT_WAITING, SIGNAL_RETURNED, SIGNAL_UNINIT, Signal};

/// Half the word: low half counts arrivals, high half is the generation.
const HALF: u32 = usize::BITS / 2;
const ARRIVED_MASK: usize = (1 << HALF) - 1;

#[inline(always)]
fn generation(state: usize) -> usize {
    state >> HALF
}

pub(crate) struct BarrierCore {
    state: AtomicUsize,
    queue: WaitQueue,
    n: usize,
}

enum ArriveOutcome {
    /// This arrival completed the round; the caller is the leader and must
    /// release the waiters of `generation`.
    Leader { generation: usize },
    /// The caller must wait for the round of `generation` to complete.
    Wait { generation: usize },
}

enum WaitOutcome {
    /// The round completed while enqueueing; no parking needed.
    Completed,
    /// The entry was pushed to the wait queue.
    Enqueued,
}

impl BarrierCore {
    const_fn! {
        pub(crate) const fn new(n: usize) -> Self {
            // A zero-sized barrier behaves like a one-sized one (std/tokio
            // semantics: every wait completes immediately as leader).
            let n = if n == 0 { 1 } else { n };
            assert!(
                n <= ARRIVED_MASK,
                "barrier size exceeds half the word size"
            );
            Self {
                state: AtomicUsize::new(0),
                queue: WaitQueue::new(),
                n,
            }
        }
    }

    /// Registers an arrival; the completing arrival becomes the leader and
    /// resets the barrier for the next generation.
    fn arrive(&self) -> ArriveOutcome {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            let generation = generation(cur);
            let next = if (cur & ARRIVED_MASK) + 1 >= self.n {
                // Completing arrival: reset the count, bump the generation
                // (wrapping naturally off the top of the word).
                generation.wrapping_add(1) << HALF
            } else {
                cur + 1
            };
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    return if next & ARRIVED_MASK == 0 {
                        ArriveOutcome::Leader { generation }
                    } else {
                        ArriveOutcome::Wait { generation }
                    };
                }
                Err(actual) => cur = actual,
            }
        }
    }

    /// Wakes every waiter of `generation` (leader duty).
    fn release_waiters(&self, generation: usize) {
        let Some(mut locked) = self.queue.lock(false) else {
            // Nobody parked yet: pending arrivals detect the generation
            // change through the freshness RMW in `enqueue_or_complete`.
            return;
        };
        let mut chain = PoppedChain::new();
        locked.with_queue(|q| {
            while let Some(front) = q.first::<Signal>() {
                // SAFETY: queued nodes are valid under the tag-lock; `aux` is
                // read through a raw place so no reference to the whole node
                // is formed.
                if unsafe { (*front.as_ptr()).aux.load(Ordering::Relaxed) } != generation {
                    // Waiters of the next generation; FIFO order guarantees
                    // our generation forms a prefix.
                    break;
                }
                // SAFETY: see above; FIFO pop order.
                unsafe {
                    let node = q.pop().unwrap();
                    chain.append(node);
                }
            }
        });
        chain.seal();
        locked.unlock(|| {});
        chain.signal_all(SIGNAL_ALL);
    }

    /// Parks `entry` for the completion of round `generation`.
    ///
    /// # Safety
    ///
    /// `entry` must point to a live node with a `None` link that stays alive
    /// and in place until it is signaled or removed.
    unsafe fn enqueue_or_complete(&self, entry: NonNull<Signal>, generation: usize) -> WaitOutcome {
        let mut locked = self.queue.lock(true).unwrap();
        // An RMW returns the latest state: either the leader's reset is
        // visible here (round complete, don't park), or our enqueue is
        // published before the leader's drain (which then pops us). This
        // makes lost wakeups impossible.
        let fresh = self.state.fetch_add(0, Ordering::AcqRel);
        if crate::barrier::generation(fresh) != generation {
            locked.unlock(|| {});
            return WaitOutcome::Completed;
        }
        // SAFETY: forwarded caller guarantee; `aux` is written through a raw
        // place so no reference to the whole node is formed.
        unsafe { (*entry.as_ptr()).aux.store(generation, Ordering::Relaxed) };
        locked.with_queue(|q| {
            // SAFETY: forwarded caller guarantee.
            unsafe { q.push(entry) }
        });
        locked.unlock(|| {});
        WaitOutcome::Enqueued
    }

    /// Blocking barrier wait.
    #[cfg(any(feature = "std", loom))]
    pub(crate) fn wait(&self) -> BarrierWaitResult {
        match self.arrive() {
            ArriveOutcome::Leader { generation } => {
                self.release_waiters(generation);
                BarrierWaitResult { is_leader: true }
            }
            ArriveOutcome::Wait { generation } => {
                let mut entry = Signal::new_sync();
                // SAFETY: the entry outlives its queue membership: we do not
                // return before it is signaled. The pointer is derived without
                // a `&mut` to the node so the reads of `entry.value` below
                // alias the queued node soundly.
                let node = unsafe { NonNull::new_unchecked(&raw mut entry) };
                match unsafe { self.enqueue_or_complete(node, generation) } {
                    WaitOutcome::Completed => BarrierWaitResult { is_leader: false },
                    WaitOutcome::Enqueued => {
                        if entry.value.swap(SIGNAL_INIT_WAITING, Ordering::AcqRel) >= SIGNAL_ALL {
                            return BarrierWaitResult { is_leader: false };
                        }
                        loop {
                            crate::shim::thread::park();
                            if entry.value.load(Ordering::Acquire) >= SIGNAL_ALL {
                                return BarrierWaitResult { is_leader: false };
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A synchronous barrier enabling multiple threads to synchronize the
/// beginning of some computation.
///
/// The synchronous counterpart of [`AsyncBarrier`] (identical layout,
/// `#[repr(C)]`, freely convertible). Reusable across rounds, with
/// stack-allocated waiter nodes — no allocation per wait.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use std::thread;
/// use xutex::Barrier;
///
/// let barrier = Arc::new(Barrier::new(4));
/// let handles: Vec<_> = (0..4)
///     .map(|_| {
///         let barrier = Arc::clone(&barrier);
///         thread::spawn(move || barrier.wait().is_leader())
///     })
///     .collect();
/// let leaders = handles
///     .into_iter()
///     .map(|h| h.join().unwrap())
///     .filter(|leader| *leader)
///     .count();
/// assert_eq!(leaders, 1);
/// ```
#[cfg(feature = "std")]
#[repr(C)]
pub struct Barrier {
    core: BarrierCore,
}

/// An asynchronous barrier enabling multiple tasks to synchronize the
/// beginning of some computation.
///
/// The async counterpart of [`Barrier`]. Unlike `tokio::sync::Barrier`,
/// dropping the [`AsyncBarrierWaitRequest`] future before completion
/// *withdraws* the arrival, so cancelled waits cannot deadlock the barrier.
///
/// # Examples
///
/// ```
/// use xutex::AsyncBarrier;
/// use swait::*;
///
/// async fn example() {
///     let barrier = AsyncBarrier::new(1);
///     // A one-task barrier completes immediately as leader.
///     assert!(barrier.wait().await.is_leader());
/// }
/// example().swait();
/// ```
#[repr(C)]
pub struct AsyncBarrier {
    core: BarrierCore,
}

#[cfg(feature = "std")]
impl Barrier {
    /// Creates a new barrier that releases once `n` threads have arrived.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new(n: usize) -> Self {
        Self {
            core: BarrierCore::new(n),
        }
    }

    /// Creates a new barrier (loom model-checking build).
    #[cfg(loom)]
    pub fn new(n: usize) -> Self {
        Self {
            core: BarrierCore::new(n),
        }
    }

    /// Blocks until all `n` threads have arrived at the barrier.
    ///
    /// Exactly one of the released threads receives a result with
    /// [`is_leader`](BarrierWaitResult::is_leader) `== true`.
    #[inline(always)]
    pub fn wait(&self) -> BarrierWaitResult {
        self.core.wait()
    }

    /// Waits at the barrier asynchronously; see [`AsyncBarrier::wait`].
    #[inline(always)]
    pub fn wait_async(&self) -> AsyncBarrierWaitRequest<'_> {
        self.as_async().wait()
    }

    /// Views this barrier as an [`AsyncBarrier`].
    #[inline(always)]
    pub fn as_async(&self) -> &AsyncBarrier {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const Barrier as *const AsyncBarrier) }
    }

    /// Converts this barrier into an [`AsyncBarrier`] without allocating.
    #[inline(always)]
    pub fn to_async(self) -> AsyncBarrier {
        let Barrier { core } = self;
        AsyncBarrier { core }
    }

    /// Converts an `Arc<Barrier>` into an `Arc<AsyncBarrier>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn to_async_arc(self: Arc<Self>) -> Arc<AsyncBarrier> {
        let raw = Arc::into_raw(self) as *const AsyncBarrier;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<Barrier>` as an `Arc<AsyncBarrier>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn clone_async(self: &Arc<Self>) -> Arc<AsyncBarrier> {
        Arc::clone(self).to_async_arc()
    }
}

impl AsyncBarrier {
    /// Creates a new asynchronous barrier that releases once `n` tasks have
    /// arrived.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new(n: usize) -> Self {
        Self {
            core: BarrierCore::new(n),
        }
    }

    /// Creates a new asynchronous barrier (loom model-checking build).
    #[cfg(loom)]
    pub fn new(n: usize) -> Self {
        Self {
            core: BarrierCore::new(n),
        }
    }

    /// Waits until all `n` tasks have arrived at the barrier.
    ///
    /// The arrival is registered on first poll. Dropping the future before
    /// completion withdraws the arrival.
    #[inline(always)]
    pub fn wait(&self) -> AsyncBarrierWaitRequest<'_> {
        AsyncBarrierWaitRequest {
            core: &self.core,
            entry: Signal::new_none(),
            generation: 0,
            _pinned: PhantomPinned,
        }
    }

    /// Blocks the current thread at the barrier; see [`Barrier::wait`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn wait_sync(&self) -> BarrierWaitResult {
        self.core.wait()
    }

    /// Views this barrier as a synchronous [`Barrier`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn as_sync(&self) -> &Barrier {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const AsyncBarrier as *const Barrier) }
    }

    /// Converts this barrier into a synchronous [`Barrier`] without
    /// allocating.
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn to_sync(self) -> Barrier {
        let AsyncBarrier { core } = self;
        Barrier { core }
    }

    /// Converts an `Arc<AsyncBarrier>` into an `Arc<Barrier>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn to_sync_arc(self: Arc<Self>) -> Arc<Barrier> {
        let raw = Arc::into_raw(self) as *const Barrier;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<AsyncBarrier>` as an `Arc<Barrier>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn clone_sync(self: &Arc<Self>) -> Arc<Barrier> {
        Arc::clone(self).to_sync_arc()
    }
}

macro_rules! barrier_traits {
    ($name:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("n", &self.core.n)
                    .finish_non_exhaustive()
            }
        }
    };
}

#[cfg(feature = "std")]
barrier_traits!(Barrier);
barrier_traits!(AsyncBarrier);

/// Returned by barrier waits; reports whether this waiter was the one that
/// completed the round.
#[derive(Debug, Clone)]
pub struct BarrierWaitResult {
    is_leader: bool,
}

impl BarrierWaitResult {
    /// Returns `true` if this waiter was the last to arrive (the "leader"
    /// of the released round). Exactly one waiter per round is the leader.
    #[inline(always)]
    pub fn is_leader(&self) -> bool {
        self.is_leader
    }
}

/// A future that completes when all parties have arrived at the barrier.
///
/// Created by [`AsyncBarrier::wait`] and [`Barrier::wait_async`]. The
/// arrival is registered at first poll; dropping the future before it
/// completes withdraws the arrival again, making barrier waits fully
/// cancellation-safe.
#[must_use = "futures do nothing unless polled"]
pub struct AsyncBarrierWaitRequest<'a> {
    core: &'a BarrierCore,
    entry: Signal,
    generation: usize,
    _pinned: PhantomPinned,
}

impl Future for AsyncBarrierWaitRequest<'_> {
    type Output = BarrierWaitResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we never move out of `this`; the entry stays pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let mut sig_val = this.entry.value.load(Ordering::Acquire);
        if sig_val == SIGNAL_INIT_WAITING {
            // Queued: re-arm the waker, or learn that the release is already
            // in flight and consume it below.
            // SAFETY: the entry is pinned inside this future and alive.
            match unsafe { rearm(NonNull::new_unchecked(&raw mut this.entry), cx) } {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(value) => sig_val = value,
            }
        }
        if sig_val >= SIGNAL_ALL {
            if unlikely(sig_val == SIGNAL_RETURNED) {
                unreachable!("barrier wait polled after completion");
            }
            this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
            return Poll::Ready(BarrierWaitResult { is_leader: false });
        }
        debug_assert_eq!(sig_val, SIGNAL_UNINIT);
        match this.core.arrive() {
            ArriveOutcome::Leader { generation } => {
                this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                this.core.release_waiters(generation);
                Poll::Ready(BarrierWaitResult { is_leader: true })
            }
            ArriveOutcome::Wait { generation } => {
                this.generation = generation;
                this.entry
                    .value
                    .store(SIGNAL_INIT_WAITING, Ordering::Release);
                this.entry.waker.register(cx.waker());
                // SAFETY: entry is pinned inside this future and the drop
                // implementation removes it from the queue.
                match unsafe {
                    this.core.enqueue_or_complete(
                        NonNull::new_unchecked(&raw mut this.entry),
                        generation,
                    )
                } {
                    WaitOutcome::Completed => {
                        this.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                        Poll::Ready(BarrierWaitResult { is_leader: false })
                    }
                    WaitOutcome::Enqueued => Poll::Pending,
                }
            }
        }
    }
}

impl Drop for AsyncBarrierWaitRequest<'_> {
    fn drop(&mut self) {
        if likely(self.entry.value.load(Ordering::Acquire) != SIGNAL_INIT_WAITING) {
            return;
        }
        self.drop_slow();
    }
}

impl AsyncBarrierWaitRequest<'_> {
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self) {
        // Try to withdraw the arrival: only possible while our generation is
        // still forming. If the round completed (generation advanced), the
        // leader has (or will have) signaled us; wait for that instead.
        let mut cur = self.core.state.load(Ordering::Relaxed);
        while generation(cur) == self.generation {
            debug_assert!(cur & ARRIVED_MASK > 0);
            match self.core.state.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Arrival withdrawn; now leave the queue. The leader
                    // cannot have popped us (our generation did not
                    // complete), so removal succeeds unless a racing round
                    // completion sneaked in between — handled below.
                    if let Some(mut locked) = self.core.queue.lock(false) {
                        let found = locked.with_queue(|q| {
                            // SAFETY: queued nodes are alive under the
                            // tag-lock; our own node is compared by address
                            // only.
                            unsafe { q.remove(NonNull::new_unchecked(&raw mut self.entry)) }
                        });
                        locked.unlock(|| {});
                        if found {
                            return;
                        }
                    }
                    break;
                }
                Err(actual) => cur = actual,
            }
        }
        // The round completed concurrently: the signal is (or will be) in
        // flight. Wait for it so no dangling queue reference remains.
        let backoff = Backoff::new();
        while self.entry.value.load(Ordering::Acquire) < SIGNAL_ALL {
            backoff.snooze();
        }
    }
}

unsafe impl Send for AsyncBarrierWaitRequest<'_> {}

#[cfg(feature = "std")]
unsafe impl Send for Barrier {}
#[cfg(feature = "std")]
unsafe impl Sync for Barrier {}
unsafe impl Send for AsyncBarrier {}
unsafe impl Sync for AsyncBarrier {}
