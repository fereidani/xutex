use std::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

use branches::likely;

/// Backoff implements an exponential back‑off strategy used by
/// synchronization primitives to reduce contention.
///
/// The back‑off starts with a spin loop that yields increasingly
/// longer pauses before eventually yielding the thread. The
/// implementation adapts to the number of CPU cores: on a
/// multi‑core system it performs a spin‑loop for a few iterations
/// before yielding, while on a single‑core system it yields
/// immediately.
///
/// # Fields
///
/// * `spin` – A `Cell<u32>` tracking the current back‑off step.
/// * `snooze_fn` – Function pointer to the appropriate snooze implementation
///   for the current hardware configuration.
///
/// # Methods
///
/// * `new()` – Creates a new `Backoff` instance, selecting the appropriate
///   snooze function based on the system's parallelism.
/// * `snooze()` – Executes the selected snooze function, advancing the back‑off
///   state.
/// * `snooze_multi_core()` – Internal implementation used when more than one
///   CPU core is available. It spins for a number of iterations that grows
///   exponentially (up to a limit) and then yields the thread once the spin
///   count exceeds a threshold.
/// * `snooze_single_core()` – Internal implementation used on a single‑core
///   system; it simply yields the thread on each call as spinning in
///   single-core environment only wastes CPU cycles.
/// * `is_completed()` – Returns `true` when the back‑off has spun beyond a
///   predefined limit, indicating that the caller should give up or take an
///   alternative action.
pub(crate) struct Backoff {
    spin: Cell<u32>,
    snooze_fn: fn(&Self),
}

impl Backoff {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            spin: Cell::new(0),
            snooze_fn: if get_parallelism() > 1 {
                Self::snooze_multi_core
            } else {
                Self::snooze_single_core
            },
        }
    }

    #[inline(always)]
    pub fn snooze(&self) {
        (self.snooze_fn)(self);
    }

    #[inline(always)]
    fn snooze_multi_core(&self) {
        let spin: u32 = self.spin.get();
        if spin <= 6 {
            for _ in 0..(1 << spin.min(5)) {
                core::hint::spin_loop();
            }
        } else {
            std::thread::yield_now();
        }
        self.spin.set(spin + 1);
    }

    #[inline(always)]
    fn snooze_single_core(&self) {
        std::thread::yield_now();
        self.spin.set(self.spin.get() + 1);
    }

    #[inline(always)]
    pub fn is_completed(&self) -> bool {
        self.spin.get() > 32
    }
}

/// Returns the number of logical CPU cores.
///
/// The value is cached in a static `AtomicUsize` so the expensive
/// `std::thread::available_parallelism()` call is performed only once.
/// The first thread that observes an uninitialized value (`0`) computes the
/// parallelism and stores it with a release store; subsequent calls take the
/// fast‑path load.
///
/// This function is deliberately `#[inline(always)]` because it is used in
/// hot paths (e.g. spin‑backoff) where the overhead of a function call would
/// be noticeable.
#[inline(always)]
pub fn get_parallelism() -> usize {
    static PARALLELISM: AtomicUsize = AtomicUsize::new(0);

    let cached = PARALLELISM.load(Ordering::Relaxed);
    if likely(cached != 0) {
        return cached;
    }

    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    PARALLELISM.store(parallelism, Ordering::Release);
    parallelism
}
