use core::task::Waker;

#[cfg(not(loom))]
use crate::shim::cell::UnsafeCell;
#[cfg(not(loom))]
use crate::shim::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(feature = "std", loom))]
use crate::shim::thread;

#[cfg(not(loom))]
use crate::backoff::Backoff;

pub(crate) enum WakerSlot {
    /// No waker is stored.
    None,
    /// Holds a synchronous thread handle for contexts that wake a thread
    /// directly.
    #[cfg(any(feature = "std", loom))]
    Sync(thread::Thread),
    /// Holds an asynchronous `Waker` for futures/tasks.
    Async(Waker),
}

/// A slot holding the current waker (none, a thread handle, or an async
/// waker), protected by a tiny spin lock.
///
/// The critical sections are a handful of instructions (replace/take the
/// slot), so a spin lock beats any blocking primitive here. Under loom the
/// spin lock is modeled as a `loom::sync::Mutex` because loom's scheduler
/// cannot prove progress through a raw spin loop (this changes nothing about
/// the protocol being verified: the lock only guards slot access).
#[cfg(not(loom))]
pub(crate) struct DynamicWaker {
    /// Spin‑lock flag indicating whether a thread is currently modifying the
    /// slot. `true` means the lock is held; `false` means it is free.
    updating: AtomicBool,

    /// The mutable slot; manually synchronized via `updating`.
    value: UnsafeCell<WakerSlot>,
}

#[cfg(not(loom))]
impl DynamicWaker {
    /// Create a new, empty `DynamicWaker`.
    pub(crate) fn new() -> Self {
        Self {
            updating: AtomicBool::new(false),
            value: UnsafeCell::new(WakerSlot::None),
        }
    }

    /// Create a new `DynamicWaker` that initially holds the current thread.
    #[cfg(feature = "std")]
    pub(crate) fn new_sync() -> Self {
        Self {
            updating: AtomicBool::new(false),
            value: UnsafeCell::new(WakerSlot::Sync(thread::current())),
        }
    }

    /// Register (or replace) an async `Waker` in the slot.
    ///
    /// * If the slot already contains an `Async` waker, it is only replaced
    ///   when the new waker would actually cause a different wake‑up
    ///   (`!val.will_wake(waker)`).
    /// * If the slot holds any other variant (`None` or `Sync`), it is
    ///   overwritten with the new async waker.
    #[inline(always)]
    pub(crate) fn register(&self, waker: &Waker) {
        // Spin until we acquire the lock.
        let backoff = Backoff::new();
        while self.updating.swap(true, Ordering::Acquire) {
            backoff.snooze();
        }

        // SAFETY: We have exclusive access because `updating` is set to true.
        unsafe {
            self.value.with_mut(|slot| {
                if let WakerSlot::Async(val) = &mut *slot {
                    // Replace only if the new waker would cause a different
                    // wake.
                    if !val.will_wake(waker) {
                        *val = waker.clone();
                    }
                } else {
                    *slot = WakerSlot::Async(waker.clone());
                }
            });
        }

        // Release the lock.
        self.updating.store(false, Ordering::Release);
    }

    /// Take the current `WakerSlot` out of the container, leaving `None`
    /// behind.
    #[inline(always)]
    pub(crate) fn take(&self) -> WakerSlot {
        // Acquire the lock.
        let backoff = Backoff::new();
        while self.updating.swap(true, Ordering::Acquire) {
            backoff.snooze();
        }

        // SAFETY: We hold the lock, so it's safe to replace the inner value.
        let value = unsafe {
            self.value
                .with_mut(|slot| core::mem::replace(&mut *slot, WakerSlot::None))
        };

        // Release the lock.
        self.updating.store(false, Ordering::Release);
        value
    }
}

/// Loom model of the waker slot: same semantics, loom-aware lock.
#[cfg(loom)]
pub(crate) struct DynamicWaker {
    value: loom::sync::Mutex<WakerSlot>,
}

#[cfg(loom)]
impl DynamicWaker {
    pub(crate) fn new() -> Self {
        Self {
            value: loom::sync::Mutex::new(WakerSlot::None),
        }
    }

    pub(crate) fn new_sync() -> Self {
        Self {
            value: loom::sync::Mutex::new(WakerSlot::Sync(thread::current())),
        }
    }

    pub(crate) fn register(&self, waker: &Waker) {
        let mut slot = self.value.lock().unwrap();
        if let WakerSlot::Async(val) = &mut *slot {
            if !val.will_wake(waker) {
                *val = waker.clone();
            }
        } else {
            *slot = WakerSlot::Async(waker.clone());
        }
    }

    pub(crate) fn take(&self) -> WakerSlot {
        core::mem::replace(&mut *self.value.lock().unwrap(), WakerSlot::None)
    }
}

unsafe impl Send for DynamicWaker {}
unsafe impl Sync for DynamicWaker {}
