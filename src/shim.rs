//! Abstraction layer that selects between the real `core`/`std`
//! synchronization primitives and [`loom`](https://docs.rs/loom) mock
//! primitives when the crate is compiled with `--cfg loom`.
//!
//! All internal code must go through this module instead of using
//! `core::sync::atomic`, `core::cell::UnsafeCell` or `std::thread`
//! directly so the whole crate can be exhaustively model-checked by loom.
//!
//! In a normal (non-loom) build everything in here compiles down to the
//! plain `core`/`std` types with zero overhead.

#[cfg(not(loom))]
pub(crate) mod sync {
    pub(crate) mod atomic {
        pub(crate) use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
    }
}

#[cfg(loom)]
pub(crate) mod sync {
    pub(crate) mod atomic {
        pub(crate) use loom::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
    }
}

pub(crate) mod cell {
    /// `UnsafeCell` facade with the loom closure-based access API.
    ///
    /// The non-loom variant is a `repr(transparent)` wrapper around
    /// `core::cell::UnsafeCell` whose accessors compile to nothing.
    #[cfg(not(loom))]
    #[repr(transparent)]
    pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

    #[cfg(not(loom))]
    impl<T> UnsafeCell<T> {
        #[inline(always)]
        pub(crate) const fn new(data: T) -> Self {
            Self(core::cell::UnsafeCell::new(data))
        }

        /// Runs `f` with a shared raw pointer to the contents.
        ///
        /// # Safety
        ///
        /// The caller must guarantee that no `&mut` access races with this
        /// access, exactly like dereferencing `core::cell::UnsafeCell::get`.
        #[inline(always)]
        pub(crate) unsafe fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }

        /// Runs `f` with an exclusive raw pointer to the contents.
        ///
        /// # Safety
        ///
        /// The caller must guarantee exclusive access for the duration of the
        /// call, exactly like dereferencing `core::cell::UnsafeCell::get`.
        #[inline(always)]
        pub(crate) unsafe fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }

        /// Returns a mutable reference to the contents through `&mut self`.
        #[inline(always)]
        pub(crate) fn get_mut(&mut self) -> &mut T {
            self.0.get_mut()
        }

        /// Consumes the cell and returns the contained value.
        #[inline(always)]
        pub(crate) fn into_inner(self) -> T {
            self.0.into_inner()
        }
    }

    #[cfg(loom)]
    pub(crate) struct UnsafeCell<T>(loom::cell::UnsafeCell<T>);

    #[cfg(loom)]
    impl<T> UnsafeCell<T> {
        pub(crate) fn new(data: T) -> Self {
            Self(loom::cell::UnsafeCell::new(data))
        }

        /// See the non-loom variant. Under loom this registers an immutable
        /// access with the model checker.
        pub(crate) unsafe fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            self.0.with(f)
        }

        /// See the non-loom variant. Under loom this registers a mutable
        /// access with the model checker.
        pub(crate) unsafe fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            self.0.with_mut(f)
        }

        pub(crate) fn get_mut(&mut self) -> &mut T {
            // loom's `UnsafeCell` has no `&mut self` accessor; `with_mut`
            // through `&self` is sound here because `&mut self` proves
            // exclusivity.
            unsafe { self.0.with_mut(|ptr| &mut *ptr) }
        }

        pub(crate) fn into_inner(self) -> T {
            self.0.into_inner()
        }
    }
}

#[cfg(all(feature = "std", not(loom)))]
pub(crate) mod thread {
    pub(crate) use std::thread::{Thread, current, park};
}

#[cfg(loom)]
pub(crate) mod thread {
    pub(crate) use loom::thread::{Thread, current, park};
}

/// Atomics for spin variables: identical to the plain atomics in normal
/// builds, but upgraded to `SeqCst` under loom.
///
/// Spin loops wait for *another thread's* store (an unlock, a signal): loom
/// explores executions where a relaxed/acquire load — and even the value
/// returned by a failed compare-exchange — is served stale indefinitely, so
/// such loops never converge in the model. `SeqCst` accesses participate in
/// loom's single total order and always observe the latest value, bounding
/// the spins. This only strengthens the verified model of the spin
/// *variables*; the state words carrying the interesting ordering proofs
/// (permit pools, generation counters) keep their real orderings.
pub(crate) mod spin {
    use super::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    #[inline(always)]
    fn up(order: Ordering) -> Ordering {
        #[cfg(loom)]
        {
            let _ = order;
            Ordering::SeqCst
        }
        #[cfg(not(loom))]
        order
    }

    pub(crate) struct SpinAtomicPtr<T>(AtomicPtr<T>);

    impl<T> SpinAtomicPtr<T> {
        super::const_fn! {
            pub(crate) const fn new(ptr: *mut T) -> Self {
                Self(AtomicPtr::new(ptr))
            }
        }

        #[inline(always)]
        pub(crate) fn load(&self, order: Ordering) -> *mut T {
            self.0.load(up(order))
        }

        #[inline(always)]
        pub(crate) fn store(&self, ptr: *mut T, order: Ordering) {
            self.0.store(ptr, up(order))
        }

        #[inline(always)]
        pub(crate) fn compare_exchange(
            &self,
            current: *mut T,
            new: *mut T,
            success: Ordering,
            failure: Ordering,
        ) -> Result<*mut T, *mut T> {
            self.0
                .compare_exchange(current, new, up(success), up(failure))
        }
    }

    pub(crate) struct SpinAtomicUsize(AtomicUsize);

    impl SpinAtomicUsize {
        pub(crate) fn new(value: usize) -> Self {
            Self(AtomicUsize::new(value))
        }

        #[inline(always)]
        pub(crate) fn load(&self, order: Ordering) -> usize {
            self.0.load(up(order))
        }

        #[inline(always)]
        pub(crate) fn store(&self, value: usize, order: Ordering) {
            self.0.store(value, up(order))
        }

        #[cfg(any(feature = "std", loom))]
        #[inline(always)]
        pub(crate) fn swap(&self, value: usize, order: Ordering) -> usize {
            self.0.swap(value, up(order))
        }
    }
}

/// Declares a `const fn` in normal builds and a plain `fn` under loom
/// (loom's atomics cannot be constructed in const context).
macro_rules! const_fn {
    ($(#[$meta:meta])* $vis:vis const fn $name:ident $($rest:tt)*) => {
        #[cfg(not(loom))]
        $(#[$meta])*
        $vis const fn $name $($rest)*

        #[cfg(loom)]
        $(#[$meta])*
        $vis fn $name $($rest)*
    };
}
pub(crate) use const_fn;
