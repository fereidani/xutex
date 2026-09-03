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

/// Outcome of [`DynamicWaker::register`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Registered {
    /// The slot already held a waker or thread handle: the registered waker
    /// is the one a signaler will find from now on.
    Armed,
    /// The slot was empty. For a queued entry that had a waker registered
    /// before, this means a signaler already took that waker and is about to
    /// publish (or has published) the signal, which will never reach the
    /// waker stored by this call.
    WasEmpty,
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
    ///
    /// Reports whether the slot was empty beforehand, see [`Registered`].
    #[inline(always)]
    pub(crate) fn register(&self, waker: &Waker) -> Registered {
        // Spin until we acquire the lock.
        let backoff = Backoff::new();
        while self.updating.swap(true, Ordering::Acquire) {
            backoff.snooze();
        }

        // SAFETY: We have exclusive access because `updating` is set to true.
        let outcome = unsafe { self.value.with_mut(|slot| store_async(&mut *slot, waker)) };

        // Release the lock.
        self.updating.store(false, Ordering::Release);
        outcome
    }

    /// [`register`](Self::register) for an entry that no other thread can
    /// reach: skips the slot lock (an atomic swap plus a store on every
    /// contended acquisition) because there is nothing to synchronize with.
    ///
    /// This is the initial registration done right before an entry is
    /// pushed to a wait queue. Until the push publishes the entry, only the
    /// owning future/thread can see it, so plain access is data-race free.
    ///
    /// # Safety
    ///
    /// The entry must not be linked into a wait queue and no signaler may
    /// hold a pointer to it, i.e. no concurrent [`register`](Self::register)
    /// or [`take`](Self::take) can be in progress.
    #[inline(always)]
    pub(crate) unsafe fn register_unsync(&self, waker: &Waker) {
        debug_assert!(
            !self.updating.load(Ordering::Relaxed),
            "register_unsync on a slot whose lock is held"
        );
        // SAFETY: the caller guarantees exclusive access to the entry.
        unsafe { self.value.with_mut(|slot| store_async(&mut *slot, waker)) };
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

/// Stores `waker` into `slot`, reusing the existing clone when it would wake
/// the same task. Reports whether the slot was empty beforehand.
#[cfg(not(loom))]
#[inline(always)]
fn store_async(slot: &mut WakerSlot, waker: &Waker) -> Registered {
    let was_empty = matches!(slot, WakerSlot::None);
    if let WakerSlot::Async(val) = slot {
        // Replace only if the new waker would cause a different wake.
        if !val.will_wake(waker) {
            *val = waker.clone();
        }
    } else {
        *slot = WakerSlot::Async(waker.clone());
    }
    if was_empty {
        Registered::WasEmpty
    } else {
        Registered::Armed
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

    pub(crate) fn register(&self, waker: &Waker) -> Registered {
        let mut slot = self.value.lock().unwrap();
        let was_empty = matches!(&*slot, WakerSlot::None);
        if let WakerSlot::Async(val) = &mut *slot {
            if !val.will_wake(waker) {
                *val = waker.clone();
            }
        } else {
            *slot = WakerSlot::Async(waker.clone());
        }
        if was_empty {
            Registered::WasEmpty
        } else {
            Registered::Armed
        }
    }

    /// Under loom the slot lock is a real mutex, so the unsynchronized
    /// registration simply takes it; the protocol being verified is the same.
    pub(crate) unsafe fn register_unsync(&self, waker: &Waker) {
        self.register(waker);
    }

    pub(crate) fn take(&self) -> WakerSlot {
        core::mem::replace(&mut *self.value.lock().unwrap(), WakerSlot::None)
    }
}

unsafe impl Send for DynamicWaker {}
unsafe impl Sync for DynamicWaker {}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn register_reports_whether_the_slot_was_empty() {
        let dw = DynamicWaker::new();
        assert_eq!(dw.register(Waker::noop()), Registered::WasEmpty);
        assert_eq!(dw.register(Waker::noop()), Registered::Armed);
        assert!(matches!(dw.take(), WakerSlot::Async(_)));
        assert!(matches!(dw.take(), WakerSlot::None));
        // A signaler consumed the waker: a re-poll must learn that the waker
        // it stores now will not be woken by that signal.
        assert_eq!(dw.register(Waker::noop()), Registered::WasEmpty);

        #[cfg(feature = "std")]
        {
            let dw = DynamicWaker::new_sync();
            assert_eq!(dw.register(Waker::noop()), Registered::Armed);
        }
    }

    #[test]
    fn register_unsync_arms_an_unpublished_slot() {
        let dw = DynamicWaker::new();
        // SAFETY: the slot is local to this test and not shared.
        unsafe { dw.register_unsync(Waker::noop()) };
        assert!(matches!(dw.take(), WakerSlot::Async(_)));
        // A stale async waker is replaced (or kept when it would wake the
        // same task) exactly like the locked registration does.
        unsafe { dw.register_unsync(Waker::noop()) };
        assert_eq!(dw.register(Waker::noop()), Registered::Armed);
        assert!(matches!(dw.take(), WakerSlot::Async(_)));
        assert!(matches!(dw.take(), WakerSlot::None));
    }
}
