use core::cell::Cell;
#[cfg(all(feature = "std", not(loom)))]
use core::sync::atomic::{AtomicUsize, Ordering};

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
/// Constructing a `Backoff` is free (it only zeroes a counter), so it can
/// be created eagerly at the top of a function that usually never spins:
/// the parallelism lookup that picks the spinning strategy is deferred to
/// the first [`snooze`](Self::snooze), which is kept out of line so hot
/// callers do not carry the spin/yield machinery inline.
///
/// # Fields
///
/// * `spin` – A `Cell<u32>` tracking the current back‑off step.
///
/// # Methods
///
/// * `new()` – Creates a new `Backoff` instance.
/// * `snooze()` – Executes one back‑off step, advancing the state. On a
///   multi‑core system it spins for a number of iterations that grows
///   exponentially (up to a limit) and then yields the thread once the spin
///   count exceeds a threshold; on a single‑core system it yields the thread on
///   each call as spinning there only wastes CPU cycles.
/// * `is_completed()` – Returns `true` when the back‑off has spun beyond a
///   predefined limit, indicating that the caller should give up or take an
///   alternative action.
pub(crate) struct Backoff {
    spin: Cell<u32>,
}

impl Backoff {
    #[inline(always)]
    pub fn new() -> Self {
        Self { spin: Cell::new(0) }
    }
}

#[cfg(all(not(feature = "std"), not(loom)))]
impl Backoff {
    /// In no-std environments the thread cannot be yielded, so every step is
    /// a bounded spin loop.
    #[inline(never)]
    pub fn snooze(&self) {
        let spin: u32 = self.spin.get();
        for _ in 0..(1u32 << spin.min(5)) {
            core::hint::spin_loop();
        }
        self.spin.set(spin.saturating_add(1));
    }
}

#[cfg(loom)]
impl Backoff {
    /// Under loom every snooze is a plain scheduler yield so the model
    /// checker can explore interleavings without a state-space explosion.
    /// The iteration guard turns a runaway spin (livelock) into a panic with
    /// a usable backtrace instead of loom's opaque branch-limit error.
    #[inline(always)]
    pub fn snooze(&self) {
        let spin = self.spin.get();
        assert!(spin < 2_000, "loom: unbounded spin loop detected");
        self.spin.set(spin + 1);
        loom::thread::yield_now();
    }

    /// Under loom the pre-park spin phase is skipped entirely to keep the
    /// explored state space small.
    #[inline(always)]
    pub fn is_completed(&self) -> bool {
        true
    }
}

#[cfg(all(feature = "std", not(loom)))]
impl Backoff {
    /// One back-off step: a short `spin_loop` burst that doubles each call
    /// for the first few calls on a multi-core machine, a thread yield
    /// otherwise.
    ///
    /// Deliberately not inlined: it only ever runs on contended paths, and
    /// keeping it out of line keeps the parallelism lookup and the yield
    /// syscall out of every generic instantiation that merely *might* spin.
    #[inline(never)]
    pub fn snooze(&self) {
        let spin: u32 = self.spin.get();
        self.spin.set(spin.saturating_add(1));
        if spin <= 6 && get_parallelism() > 1 {
            for _ in 0..(1u32 << spin.min(5)) {
                core::hint::spin_loop();
            }
        } else {
            std::thread::yield_now();
        }
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
/// parallelism and stores it; subsequent calls take the fast‑path load.
#[inline(always)]
#[cfg(all(feature = "std", not(loom)))]
pub fn get_parallelism() -> usize {
    static PARALLELISM: AtomicUsize = AtomicUsize::new(0);

    let cached = PARALLELISM.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    init_parallelism(&PARALLELISM)
}

/// Cold half of [`get_parallelism`]: the `available_parallelism` call and
/// the drop of its `io::Error` stay out of line so they are not duplicated
/// into every caller.
#[cold]
#[inline(never)]
#[cfg(all(feature = "std", not(loom)))]
fn init_parallelism(slot: &AtomicUsize) -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // Racing initializers store the same value; Relaxed is enough because
    // the value is only ever used as a heuristic.
    slot.store(parallelism, Ordering::Relaxed);
    parallelism
}

/// Fixed parallelism under loom: the pool allocator is bypassed and the
/// backoff never consults this, but keep the symbol available.
#[cfg(loom)]
#[allow(dead_code)]
pub fn get_parallelism() -> usize {
    2
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Constructing a `Backoff` must stay free: it is created eagerly at the
    /// top of fast paths that usually never spin, so it must not carry a
    /// function pointer or any lookup that has to run before the fast path.
    #[test]
    fn backoff_is_a_bare_counter() {
        assert_eq!(core::mem::size_of::<Backoff>(), core::mem::size_of::<u32>());
        let backoff = Backoff::new();
        for _ in 0..40 {
            backoff.snooze();
        }
        #[cfg(feature = "std")]
        assert!(backoff.is_completed());
    }
}
