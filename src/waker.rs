use std::{cell::UnsafeCell, sync::atomic::AtomicBool, task::Waker, thread};

use crate::backoff::Backoff;

pub(crate) enum WakerSlot {
    /// No waker is stored.
    None,
    /// Holds a synchronous thread handle for contexts that wake a thread
    /// directly.
    Sync(thread::Thread),
    /// Holds an asynchronous `Waker` for futures/tasks.
    Async(Waker),
}

pub(crate) struct DynamicWaker {
    /// Spin‑lock flag indicating whether a thread is currently modifying the
    /// slot. `true` means the lock is held; `false` means it is free.
    updating: AtomicBool,

    /// The mutable slot that holds the current waker (none, a thread handle, or
    /// an async waker). Wrapped in `UnsafeCell` because we manually enforce
    /// exclusive access via `updating`.
    value: UnsafeCell<WakerSlot>,
}

impl DynamicWaker {
    /// Create a new, empty `DynamicWaker`.
    ///
    /// The internal state starts with no stored waker (`WakerSlot::None`) and
    /// the `updating` flag cleared, meaning no thread is currently modifying
    /// the slot.
    pub(crate) fn new() -> Self {
        Self {
            updating: AtomicBool::new(false),
            value: UnsafeCell::new(WakerSlot::None),
        }
    }

    /// Create a new `DynamicWaker` that initially holds the current thread.
    ///
    /// This variant is used for synchronous contexts where the waker should be
    /// a `thread::Thread` rather than an async `Waker`. The `updating` flag is
    /// also cleared initially.
    pub(crate) fn new_sync() -> Self {
        Self {
            updating: AtomicBool::new(false),
            value: UnsafeCell::new(WakerSlot::Sync(thread::current())),
        }
    }

    /// Register (or replace) an async `Waker` in the slot.
    ///
    /// The method uses a spin‑lock based on `AtomicBool::swap` to obtain
    /// exclusive access to the `value`. `Backoff` provides an exponential
    /// pause while waiting, reducing contention on the lock.
    ///
    /// * If the slot already contains an `Async` waker, we only replace it when
    ///   the new waker would actually cause a different wake‑up
    ///   (`!val.will_wake(waker)`).
    /// * If the slot holds any other variant (`None` or `Sync`), we overwrite
    ///   it with the new async waker.
    #[inline(always)]
    pub(crate) fn register(&self, waker: &Waker) {
        // Spin until we acquire the lock.
        let backoff = Backoff::new();
        while self
            .updating
            .swap(true, std::sync::atomic::Ordering::Acquire)
        {
            backoff.snooze();
        }

        // SAFETY: We have exclusive access because `updating` is set to true.
        unsafe {
            if let WakerSlot::Async(val) = &mut *self.value.get() {
                // Replace only if the new waker would cause a different wake.
                if !val.will_wake(waker) {
                    // The stored waker would not wake the task; replace it.
                    *val = waker.clone();
                }
            } else {
                // Slot is empty or holds a sync thread; store the async waker.
                *self.value.get() = WakerSlot::Async(waker.clone());
            }
        }

        // Release the lock.
        self.updating
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Take the current `WakerSlot` out of the container, leaving `None`
    /// behind.
    ///
    /// This also uses the same spin‑lock pattern as `register` to guarantee
    /// exclusive access while swapping the value.
    #[inline(always)]
    pub(crate) fn take(&self) -> WakerSlot {
        // Acquire the lock.
        let backoff = Backoff::new();
        while self
            .updating
            .swap(true, std::sync::atomic::Ordering::Acquire)
        {
            backoff.snooze();
        }

        // SAFETY: We hold the lock, so it's safe to replace the inner value.
        let value = unsafe { std::mem::replace(&mut *self.value.get(), WakerSlot::None) };

        // Release the lock.
        self.updating
            .store(false, std::sync::atomic::Ordering::Release);
        value
    }
}

unsafe impl Send for DynamicWaker {}
unsafe impl Sync for DynamicWaker {}
