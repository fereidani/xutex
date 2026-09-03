//! A cell that is initialized exactly once, split into a synchronous
//! [`OnceCell`] and an asynchronous [`AsyncOnceCell`] with identical memory
//! layout.
//!
//! Semantics follow `tokio::sync::OnceCell`: concurrent
//! `get_or_init` callers race to become *the initializer*; everyone else
//! parks on the allocation-free wait queue. If the initializer fails or is
//! cancelled, one queued waiter is woken to take over with its own
//! initialization closure.

use core::fmt;
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};

#[cfg(all(feature = "std", not(loom)))]
use alloc::sync::Arc;

use branches::{likely, unlikely};

use crate::backoff::Backoff;
use crate::shim::cell::UnsafeCell;
use crate::shim::const_fn;
use crate::shim::sync::atomic::{AtomicUsize, Ordering};
use crate::wait_queue::{PoppedChain, WaitQueue, signal_node};
use crate::{
    SIGNAL_INIT_WAITING, SIGNAL_RETRY, SIGNAL_RETURNED, SIGNAL_SIGNALED, SIGNAL_UNINIT, Signal,
};

/// No value, no initializer running.
const EMPTY: usize = 0;
/// An initializer is currently running.
const RUNNING: usize = 1;
/// The value is initialized; terminal state.
const READY: usize = 2;

struct OnceCore {
    state: AtomicUsize,
    queue: WaitQueue,
}

enum WaitOutcome {
    /// The state changed before parking; the caller must re-inspect it.
    State,
    /// The entry was pushed to the wait queue.
    Enqueued,
}

impl OnceCore {
    const_fn! {
        const fn new(state: usize) -> Self {
            Self {
                state: AtomicUsize::new(state),
                queue: WaitQueue::new(),
            }
        }
    }

    /// Attempts to become the initializer.
    fn try_begin_init(&self) -> Result<(), usize> {
        match self
            .state
            .compare_exchange(EMPTY, RUNNING, Ordering::Acquire, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(actual) => Err(actual),
        }
    }

    /// Publishes the value (initializer duty) and wakes all waiters.
    fn complete(&self) {
        // An RMW (not a plain store) so that a waiter's authoritative
        // freshness RMW on `state` is totally ordered against it: seeing
        // RUNNING there then proves this swap — and the queue drain after
        // it — has not yet happened and will observe the waiter.
        self.state.swap(READY, Ordering::AcqRel);
        let Some(mut locked) = self.queue.lock(false) else {
            return;
        };
        let mut chain = PoppedChain::new();
        locked.with_queue(|q| {
            // SAFETY: queued nodes are alive under the tag-lock; FIFO pop
            // order.
            while let Some(node) = unsafe { q.pop() } {
                unsafe { chain.append(node) };
            }
        });
        chain.seal();
        locked.unlock(|| {});
        chain.signal_all(SIGNAL_SIGNALED);
    }

    /// Reverts to `EMPTY` after a failed/cancelled initialization and hands
    /// the initializer role to one queued waiter.
    fn abort(&self) {
        // RMW for the same reason as in `complete`.
        self.state.swap(EMPTY, Ordering::AcqRel);
        self.wake_one_retry();
    }

    /// Wakes a single waiter with `SIGNAL_RETRY` so it can attempt to become
    /// the initializer. Safe to call spuriously: woken waiters re-inspect
    /// the state and re-enqueue if someone else already took over.
    fn wake_one_retry(&self) {
        let Some(mut locked) = self.queue.lock(false) else {
            return;
        };
        // SAFETY: queued nodes are alive under the tag-lock.
        let popped = locked.with_queue(|q| unsafe { q.pop() });
        locked.unlock(|| {});
        if let Some(node) = popped {
            // SAFETY: popped under the tag-lock, exclusively ours.
            unsafe { signal_node(node, SIGNAL_RETRY) };
        }
    }

    /// Parks `entry` while an initializer is running.
    ///
    /// # Safety
    ///
    /// `entry` must point to a live node with a `None` link that stays alive
    /// and in place until it is signaled or removed.
    unsafe fn enqueue_or_state(&self, entry: NonNull<Signal>) -> WaitOutcome {
        let mut locked = self.queue.lock(true).unwrap();
        // An RMW returns the latest state: either the initializer's
        // completion/abort is visible here, or our enqueue is published
        // before its queue drain (which then pops us). No lost wakeups.
        let fresh = self.state.fetch_add(0, Ordering::AcqRel);
        if fresh != RUNNING {
            locked.unlock(|| {});
            return WaitOutcome::State;
        }
        locked.with_queue(|q| {
            // SAFETY: forwarded caller guarantee.
            unsafe { q.push(entry) }
        });
        locked.unlock(|| {});
        WaitOutcome::Enqueued
    }

    /// Removes `entry` from the queue on cancellation; if it was already
    /// popped, waits for the in-flight signal and re-delivers a retry wakeup
    /// when necessary.
    ///
    /// # Safety
    ///
    /// `entry` must be the same entry passed to `enqueue_or_state`.
    #[cold]
    unsafe fn cancel_wait(&self, entry: NonNull<Signal>) {
        if let Some(mut locked) = self.queue.lock(false) {
            let found = locked.with_queue(|q| {
                // SAFETY: queued nodes are alive under the tag-lock; our own
                // node is compared by address only.
                unsafe { q.remove(entry) }
            });
            locked.unlock(|| {});
            if found {
                return;
            }
        }
        // SAFETY: forwarded caller guarantee (the node is alive); raw place
        // accesses so no reference to the whole node is held while the
        // signaler touches it.
        let node = entry.as_ptr();
        let backoff = Backoff::new();
        let mut value = unsafe { (*node).value.load(Ordering::Acquire) };
        while value < SIGNAL_SIGNALED {
            backoff.snooze();
            value = unsafe { (*node).value.load(Ordering::Acquire) };
        }
        if value == SIGNAL_RETRY {
            // We were chosen to take over initialization but are being
            // dropped: pass the role to the next waiter.
            self.wake_one_retry();
        }
    }
}

/// Internal shared representation of the once-cells.
#[repr(C)]
struct OnceCellInternal<T> {
    core: OnceCore,
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> OnceCellInternal<T> {
    const_fn! {
        const fn new() -> Self {
            Self {
                core: OnceCore::new(EMPTY),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            }
        }
    }

    fn new_with(value: Option<T>) -> Self {
        match value {
            Some(v) => Self {
                core: OnceCore::new(READY),
                value: UnsafeCell::new(MaybeUninit::new(v)),
            },
            None => Self::new(),
        }
    }

    #[inline(always)]
    fn is_ready(&self) -> bool {
        self.core.state.load(Ordering::Acquire) == READY
    }

    /// # Safety
    ///
    /// The state must be `READY`.
    #[inline(always)]
    unsafe fn value_ref(&self) -> &T {
        // SAFETY: READY is terminal, the value is initialized and never
        // mutated again.
        unsafe { self.value.with(|ptr| (*ptr).assume_init_ref()) }
    }

    /// Writes the value and publishes it. Caller must hold the `RUNNING`
    /// role.
    fn write_and_complete(&self, value: T) {
        // SAFETY: the RUNNING role grants exclusive access to the slot.
        unsafe {
            self.value.with_mut(|ptr| {
                (*ptr).write(value);
            })
        };
        self.core.complete();
    }

    fn get(&self) -> Option<&T> {
        if self.is_ready() {
            // SAFETY: checked READY.
            Some(unsafe { self.value_ref() })
        } else {
            None
        }
    }

    fn set(&self, value: T) -> Result<(), SetError<T>> {
        match self.core.try_begin_init() {
            Ok(()) => {
                self.write_and_complete(value);
                Ok(())
            }
            Err(READY) => Err(SetError::AlreadyInitializedError(value)),
            Err(_) => Err(SetError::InitializingError(value)),
        }
    }

    fn take(&mut self) -> Option<T> {
        if self.is_ready() {
            self.core.state.store(EMPTY, Ordering::Relaxed);
            // SAFETY: state was READY (value initialized) and `&mut self`
            // guarantees exclusivity; state is reset to EMPTY so the value
            // is not dropped twice.
            Some(unsafe { self.value.with_mut(|ptr| (*ptr).assume_init_read()) })
        } else {
            None
        }
    }

    /// Blocking `get_or_try_init`.
    #[cfg(any(feature = "std", loom))]
    fn get_or_try_init_sync<E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        let mut f = Some(f);
        loop {
            match self.core.state.load(Ordering::Acquire) {
                READY => {
                    // SAFETY: checked READY.
                    return Ok(unsafe { self.value_ref() });
                }
                EMPTY => {
                    if self.core.try_begin_init().is_err() {
                        continue;
                    }
                    let guard = AbortOnPanic { core: &self.core };
                    let result = (f.take().unwrap())();
                    core::mem::forget(guard);
                    match result {
                        Ok(value) => {
                            self.write_and_complete(value);
                            // SAFETY: just completed.
                            return Ok(unsafe { self.value_ref() });
                        }
                        Err(e) => {
                            self.core.abort();
                            return Err(e);
                        }
                    }
                }
                _ => {
                    let mut entry = Signal::new_sync();
                    // SAFETY: the entry outlives its queue membership: we do
                    // not leave this block before it is signaled. The pointer
                    // is derived without a `&mut` to the node so the reads of
                    // `entry.value` below alias the queued node soundly.
                    let node = unsafe { NonNull::new_unchecked(&raw mut entry) };
                    match unsafe { self.core.enqueue_or_state(node) } {
                        WaitOutcome::State => continue,
                        WaitOutcome::Enqueued => {
                            let mut value = entry.value.swap(SIGNAL_INIT_WAITING, Ordering::AcqRel);
                            while value < SIGNAL_SIGNALED {
                                crate::shim::thread::park();
                                value = entry.value.load(Ordering::Acquire);
                            }
                            // SIGNAL_SIGNALED: the value is ready (checked at
                            // the top of the loop). SIGNAL_RETRY: the
                            // initializer failed, we contend to take over.
                            continue;
                        }
                    }
                }
            }
        }
    }
}

/// Reverts the cell to `EMPTY` if the initializer panics.
#[cfg(any(feature = "std", loom))]
struct AbortOnPanic<'a> {
    core: &'a OnceCore,
}

#[cfg(any(feature = "std", loom))]
impl Drop for AbortOnPanic<'_> {
    fn drop(&mut self) {
        self.core.abort();
    }
}

/// Error returned by [`OnceCell::set`]/[`AsyncOnceCell::set`], carrying the
/// rejected value.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SetError<T> {
    /// The cell was already initialized.
    AlreadyInitializedError(T),
    /// An initializer was running concurrently.
    InitializingError(T),
}

impl<T> SetError<T> {
    /// Consumes the error, returning the value that could not be stored.
    pub fn into_inner(self) -> T {
        match self {
            SetError::AlreadyInitializedError(v) | SetError::InitializingError(v) => v,
        }
    }
}

impl<T> fmt::Display for SetError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetError::AlreadyInitializedError(_) => f.write_str("cell already initialized"),
            SetError::InitializingError(_) => f.write_str("cell is being initialized"),
        }
    }
}

impl<T: fmt::Debug> core::error::Error for SetError<T> {}

/// A thread-safe cell that is initialized at most once, with blocking
/// waiters.
///
/// The synchronous counterpart of [`AsyncOnceCell`] (identical layout,
/// `#[repr(C)]`, freely convertible). Waiting threads park on
/// stack-allocated intrusive nodes — no allocation per wait.
///
/// # Examples
///
/// ```
/// use xutex::OnceCell;
///
/// static CELL: OnceCell<u32> = OnceCell::new();
///
/// let value = CELL.get_or_init(|| 92);
/// assert_eq!(*value, 92);
/// assert_eq!(CELL.get(), Some(&92));
/// ```
#[cfg(feature = "std")]
#[repr(C)]
pub struct OnceCell<T> {
    internal: OnceCellInternal<T>,
}

/// A thread-safe cell that is initialized at most once, with asynchronous
/// waiters.
///
/// Follows `tokio::sync::OnceCell` semantics: `get_or_init` callers race,
/// the losers await the winner, and a failed or cancelled initializer hands
/// the role over to a queued waiter (which then runs its *own* closure).
///
/// # Examples
///
/// ```
/// use xutex::AsyncOnceCell;
/// use swait::*;
///
/// async fn example() {
///     let cell = AsyncOnceCell::new();
///     let value = cell.get_or_init(|| async { 5 }).await;
///     assert_eq!(*value, 5);
/// }
/// example().swait();
/// ```
#[repr(C)]
pub struct AsyncOnceCell<T> {
    internal: OnceCellInternal<T>,
}

macro_rules! once_cell_common {
    ($name:ident) => {
        impl<T> $name<T> {
            /// Creates an initialized cell when `value` is `Some`, an empty
            /// cell otherwise.
            pub fn new_with(value: Option<T>) -> Self {
                Self {
                    internal: OnceCellInternal::new_with(value),
                }
            }

            /// Returns a reference to the value if the cell is initialized.
            #[inline(always)]
            pub fn get(&self) -> Option<&T> {
                self.internal.get()
            }

            /// Returns a mutable reference to the value if the cell is
            /// initialized (`&mut self` guarantees exclusivity).
            pub fn get_mut(&mut self) -> Option<&mut T> {
                if self.internal.is_ready() {
                    // SAFETY: READY + exclusive access.
                    Some(unsafe { self.internal.value.with_mut(|ptr| (*ptr).assume_init_mut()) })
                } else {
                    None
                }
            }

            /// Returns `true` if the cell is initialized.
            #[inline(always)]
            pub fn initialized(&self) -> bool {
                self.internal.is_ready()
            }

            /// Stores `value` if the cell is empty and no initializer is
            /// running; on failure returns the value inside the error.
            pub fn set(&self, value: T) -> Result<(), SetError<T>> {
                self.internal.set(value)
            }

            /// Takes the value out of the cell, leaving it empty.
            pub fn take(&mut self) -> Option<T> {
                self.internal.take()
            }

            /// Consumes the cell, returning the value if it was initialized.
            pub fn into_inner(mut self) -> Option<T> {
                self.take()
            }
        }

        impl<T> Default for $name<T> {
            fn default() -> Self {
                Self::new()
            }
        }

        impl<T> From<T> for $name<T> {
            fn from(value: T) -> Self {
                Self::new_with(Some(value))
            }
        }

        impl<T: fmt::Debug> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("value", &self.get())
                    .finish()
            }
        }

        impl<T> Drop for $name<T> {
            fn drop(&mut self) {
                if self.internal.is_ready() {
                    // SAFETY: READY means the value is initialized; the cell
                    // is being destroyed so it is dropped exactly once.
                    unsafe {
                        self.internal
                            .value
                            .with_mut(|ptr| (*ptr).assume_init_drop())
                    };
                }
            }
        }
    };
}

#[cfg(feature = "std")]
once_cell_common!(OnceCell);
once_cell_common!(AsyncOnceCell);

#[cfg(feature = "std")]
impl<T> OnceCell<T> {
    /// Creates an empty cell.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            internal: OnceCellInternal::new(),
        }
    }

    /// Creates an empty cell (loom model-checking build).
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            internal: OnceCellInternal::new(),
        }
    }

    /// Returns the value, initializing it with `f` if the cell was empty,
    /// blocking while another thread is initializing.
    ///
    /// If the running initializer panics or fails, one waiting caller takes
    /// over with its own closure.
    pub fn get_or_init(&self, f: impl FnOnce() -> T) -> &T {
        match self.internal.get_or_try_init_sync(|| Ok::<T, Never>(f())) {
            Ok(v) => v,
            Err(never) => match never {},
        }
    }

    /// Returns the value, initializing it with the fallible `f` if the cell
    /// was empty, blocking while another thread is initializing.
    pub fn get_or_try_init<E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        self.internal.get_or_try_init_sync(f)
    }

    /// Views this cell as an [`AsyncOnceCell`].
    #[inline(always)]
    pub fn as_async(&self) -> &AsyncOnceCell<T> {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const OnceCell<T> as *const AsyncOnceCell<T>) }
    }

    /// Converts an `Arc<OnceCell<T>>` into an `Arc<AsyncOnceCell<T>>`
    /// without allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn to_async_arc(self: Arc<Self>) -> Arc<AsyncOnceCell<T>> {
        let raw = Arc::into_raw(self) as *const AsyncOnceCell<T>;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<OnceCell<T>>` as an `Arc<AsyncOnceCell<T>>` without
    /// allocating.
    #[inline(always)]
    #[cfg(not(loom))]
    pub fn clone_async(self: &Arc<Self>) -> Arc<AsyncOnceCell<T>> {
        Arc::clone(self).to_async_arc()
    }
}

#[cfg(feature = "std")]
enum Never {}

impl<T> AsyncOnceCell<T> {
    /// Creates an empty cell.
    #[inline(always)]
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            internal: OnceCellInternal::new(),
        }
    }

    /// Creates an empty cell (loom model-checking build).
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            internal: OnceCellInternal::new(),
        }
    }

    /// Returns the value, initializing it with the future produced by `f`
    /// if the cell was empty.
    ///
    /// `f` is only invoked if this caller wins the race to initialize.
    /// Dropping the returned future while it holds the initializer role
    /// hands the role to a queued waiter (which runs its own closure) —
    /// exactly like `tokio::sync::OnceCell`.
    #[inline(always)]
    pub fn get_or_init<F, Fut>(&self, f: F) -> AsyncGetOrInit<'_, T, F, Fut>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        AsyncGetOrInit {
            request: AsyncGetOrTryInit {
                internal: &self.internal,
                f: Some(f),
                fut: None,
                entry: Signal::new_none(),
                initializing: false,
                _pinned: PhantomPinned,
            },
        }
    }

    /// Returns the value, initializing it with the fallible future produced
    /// by `f` if the cell was empty.
    #[inline(always)]
    pub fn get_or_try_init<F, Fut, E>(&self, f: F) -> AsyncGetOrTryInit<'_, T, F, Fut>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        AsyncGetOrTryInit {
            internal: &self.internal,
            f: Some(f),
            fut: None,
            entry: Signal::new_none(),
            initializing: false,
            _pinned: PhantomPinned,
        }
    }

    /// Returns the value, initializing it with `f` if the cell was empty,
    /// blocking the current thread; see [`OnceCell::get_or_init`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn get_or_init_sync(&self, f: impl FnOnce() -> T) -> &T {
        self.as_sync().get_or_init(f)
    }

    /// Views this cell as a synchronous [`OnceCell`].
    #[inline(always)]
    #[cfg(feature = "std")]
    pub fn as_sync(&self) -> &OnceCell<T> {
        // SAFETY: same memory layout and structure (#[repr(C)]).
        unsafe { &*(self as *const AsyncOnceCell<T> as *const OnceCell<T>) }
    }

    /// Converts an `Arc<AsyncOnceCell<T>>` into an `Arc<OnceCell<T>>`
    /// without allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn to_sync_arc(self: Arc<Self>) -> Arc<OnceCell<T>> {
        let raw = Arc::into_raw(self) as *const OnceCell<T>;
        // SAFETY: identical layout (#[repr(C)]).
        unsafe { Arc::from_raw(raw) }
    }

    /// Clones an `Arc<AsyncOnceCell<T>>` as an `Arc<OnceCell<T>>` without
    /// allocating.
    #[inline(always)]
    #[cfg(all(feature = "std", not(loom)))]
    pub fn clone_sync(self: &Arc<Self>) -> Arc<OnceCell<T>> {
        Arc::clone(self).to_sync_arc()
    }
}

/// Result of one step of the shared wait/take-over state machine.
enum Step {
    /// The value is initialized and can be returned.
    ValueReady,
    /// This future just became the initializer (`fut` is now set).
    Initializing,
}

/// A future that resolves to `&T`, initializing the [`AsyncOnceCell`] if
/// necessary. Created by [`AsyncOnceCell::get_or_init`].
#[must_use = "futures do nothing unless polled"]
pub struct AsyncGetOrInit<'a, T, F, Fut> {
    request: AsyncGetOrTryInit<'a, T, F, Fut>,
}

impl<'a, T, F, Fut> Future for AsyncGetOrInit<'a, T, F, Fut>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    type Output = &'a T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pin projection of the only field; we never move
        // `fut` or `entry` after they are set.
        let this = unsafe { &mut self.get_unchecked_mut().request };
        loop {
            if this.initializing {
                // We hold the initializer role: drive the user future.
                let fut = this.fut.as_mut().expect("initializer future missing");
                // SAFETY: `fut` is structurally pinned inside `this`.
                match unsafe { Pin::new_unchecked(fut) }.poll(cx) {
                    Poll::Ready(value) => {
                        this.initializing = false;
                        this.fut = None;
                        this.internal.write_and_complete(value);
                        // SAFETY: just completed.
                        return Poll::Ready(unsafe { this.internal.value_ref() });
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            match this.poll_wait(cx) {
                // SAFETY: ValueReady guarantees the READY state.
                Poll::Ready(Step::ValueReady) => {
                    return Poll::Ready(unsafe { this.internal.value_ref() });
                }
                Poll::Ready(Step::Initializing) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// A future that resolves to `Result<&T, E>`, initializing the
/// [`AsyncOnceCell`] if necessary. Created by
/// [`AsyncOnceCell::get_or_try_init`].
#[must_use = "futures do nothing unless polled"]
pub struct AsyncGetOrTryInit<'a, T, F, Fut> {
    internal: &'a OnceCellInternal<T>,
    f: Option<F>,
    fut: Option<Fut>,
    entry: Signal,
    initializing: bool,
    _pinned: PhantomPinned,
}

impl<'a, T, F, Fut> AsyncGetOrTryInit<'a, T, F, Fut>
where
    F: FnOnce() -> Fut,
{
    /// Shared wait/take-over state machine: everything except driving the
    /// initializer future itself.
    ///
    /// The caller must not be in the `initializing` state.
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<Step> {
        loop {
            let sig_val = self.entry.value.load(Ordering::Acquire);
            match sig_val {
                SIGNAL_SIGNALED => {
                    self.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                    debug_assert!(self.internal.is_ready());
                    // SIGNAL_SIGNALED is only sent by `complete`.
                    return Poll::Ready(Step::ValueReady);
                }
                SIGNAL_RETRY => {
                    // The initializer failed; contend to take over below.
                    self.entry.reset();
                }
                SIGNAL_INIT_WAITING => {
                    self.entry.waker.register(cx.waker());
                    return Poll::Pending;
                }
                SIGNAL_RETURNED => unreachable!("get_or_init polled after completion"),
                _ => debug_assert_eq!(sig_val, SIGNAL_UNINIT),
            }

            match self.internal.core.state.load(Ordering::Acquire) {
                READY => {
                    self.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                    return Poll::Ready(Step::ValueReady);
                }
                EMPTY => {
                    if self.internal.core.try_begin_init().is_ok() {
                        self.initializing = true;
                        self.entry.value.store(SIGNAL_RETURNED, Ordering::Relaxed);
                        let f = self.f.take().expect("init closure already consumed");
                        self.fut = Some(f());
                        return Poll::Ready(Step::Initializing);
                    }
                    continue;
                }
                _ => {
                    self.entry
                        .value
                        .store(SIGNAL_INIT_WAITING, Ordering::Release);
                    self.entry.waker.register(cx.waker());
                    // SAFETY: the entry is pinned inside this future (the
                    // caller polls through Pin) and the drop implementation
                    // removes it from the queue.
                    match unsafe {
                        self.internal
                            .core
                            .enqueue_or_state(NonNull::new_unchecked(&raw mut self.entry))
                    } {
                        WaitOutcome::Enqueued => return Poll::Pending,
                        WaitOutcome::State => {
                            self.entry.reset();
                            continue;
                        }
                    }
                }
            }
        }
    }
}

impl<'a, T, F, Fut, E> Future for AsyncGetOrTryInit<'a, T, F, Fut>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    type Output = Result<&'a T, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: we never move `fut` or `entry` after they are set; the
        // projections below respect structural pinning.
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            if this.initializing {
                // We hold the initializer role: drive the user future.
                let fut = this.fut.as_mut().expect("initializer future missing");
                // SAFETY: `fut` is structurally pinned inside `this`.
                match unsafe { Pin::new_unchecked(fut) }.poll(cx) {
                    Poll::Ready(Ok(value)) => {
                        this.initializing = false;
                        this.fut = None;
                        this.internal.write_and_complete(value);
                        // SAFETY: just completed.
                        return Poll::Ready(Ok(unsafe { this.internal.value_ref() }));
                    }
                    Poll::Ready(Err(e)) => {
                        this.initializing = false;
                        this.fut = None;
                        this.internal.core.abort();
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            match this.poll_wait(cx) {
                // SAFETY: ValueReady guarantees the READY state.
                Poll::Ready(Step::ValueReady) => {
                    return Poll::Ready(Ok(unsafe { this.internal.value_ref() }));
                }
                Poll::Ready(Step::Initializing) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<T, F, Fut> Drop for AsyncGetOrTryInit<'_, T, F, Fut> {
    fn drop(&mut self) {
        if unlikely(self.initializing) {
            // We were the initializer: revert and hand the role over.
            self.internal.core.abort();
            return;
        }
        let value = self.entry.value.load(Ordering::Acquire);
        if likely(value == SIGNAL_UNINIT || value == SIGNAL_RETURNED) {
            return;
        }
        if value == SIGNAL_RETRY {
            // Chosen to take over but dropped: pass the role on.
            self.internal.core.wake_one_retry();
            return;
        }
        // SIGNAL_INIT_WAITING: leave the queue (or wait out an in-flight
        // signal and re-deliver a retry if we swallowed one).
        // SAFETY: same entry as passed to enqueue_or_state.
        unsafe {
            self.internal
                .core
                .cancel_wait(NonNull::new_unchecked(&raw mut self.entry))
        };
    }
}

unsafe impl<T: Send + Sync, F: Send, Fut: Send> Send for AsyncGetOrTryInit<'_, T, F, Fut> {}
unsafe impl<T: Send + Sync, F: Send, Fut: Send> Send for AsyncGetOrInit<'_, T, F, Fut> {}

#[cfg(feature = "std")]
unsafe impl<T: Send> Send for OnceCell<T> {}
#[cfg(feature = "std")]
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}
unsafe impl<T: Send> Send for AsyncOnceCell<T> {}
unsafe impl<T: Send + Sync> Sync for AsyncOnceCell<T> {}
