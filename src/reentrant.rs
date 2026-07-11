use std::cell::Cell;
use std::marker::{PhantomData, PhantomPinned};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{AsyncLockRequest, Mutex, MutexGuard};

// Thanks to https://github.com/Amanieu/parking_lot/blob/f989a09dbb391bd4b0920c618234e5d3c151ea76/src/remutex.rs#L18
#[inline]
fn thread_id() -> usize {
    thread_local!(static KEY: u8 = const { 0 });
    KEY.with(|x| NonZeroUsize::new(x as *const _ as usize).unwrap())
        .get()
}

/// A mutex that can be locked multiple times by the same thread.
pub struct ReentrantMutex<T> {
    mutex: Mutex<T>,
    owner: AtomicUsize,
    count: Cell<usize>,
}

// SAFETY: count is only accessed by the owning thread
unsafe impl<T: Send> Send for ReentrantMutex<T> {}
unsafe impl<T: Send> Sync for ReentrantMutex<T> {}

impl<T> ReentrantMutex<T> {
    /// Creates a new reentrant mutex.
    pub const fn new(v: T) -> Self {
        Self {
            mutex: Mutex::new(v),
            owner: AtomicUsize::new(0),
            count: Cell::new(0),
        }
    }

    #[inline]
    fn owner(&self) -> usize {
        self.owner.load(Ordering::Relaxed)
    }

    #[inline]
    fn has_waiters(&self) -> bool {
        self.mutex.has_waiters()
    }

    #[inline]
    fn reentrant_guard(&self, tid: usize) -> Option<ReentrantMutexGuard<'_, T>> {
        (self.owner() == tid).then(|| {
            self.count.set(self.count.get() + 1);
            ReentrantMutexGuard {
                mutex: self,
                _marker: PhantomData,
            }
        })
    }

    #[inline]
    fn acquired<'a>(&'a self, tid: usize, guard: MutexGuard<'a, T>) -> ReentrantMutexGuard<'a, T> {
        std::mem::forget(guard); // Prevent unlocking the underlying mutex
        self.owner.store(tid, Ordering::Relaxed);
        self.count.set(1);
        ReentrantMutexGuard {
            mutex: self,
            _marker: PhantomData,
        }
    }

    /// Acquires the mutex, blocking if held by another thread.
    /// Re-entering from the same thread increments the lock count.
    pub fn lock(&self) -> ReentrantMutexGuard<'_, T> {
        let tid = thread_id();
        if let Some(g) = self.reentrant_guard(tid) {
            return g;
        }
        self.acquired(tid, self.mutex.lock())
    }

    /// Async version of `lock`. Yields to the runtime while waiting.
    /// Re-entering from the same thread increments the lock count.
    #[inline]
    pub fn lock_async(&self) -> ReentrantLockRequest<'_, T> {
        ReentrantLockRequest {
            mutex: self,
            inner: None,
            _pinned: PhantomPinned,
        }
    }

    /// Attempts to acquire without blocking. Returns `None` if held
    /// by another thread.
    pub fn try_lock(&self) -> Option<ReentrantMutexGuard<'_, T>> {
        let tid = thread_id();
        if let Some(g) = self.reentrant_guard(tid) {
            return Some(g);
        }
        Some(self.acquired(tid, self.mutex.try_lock()?))
    }
}

/// RAII guard for `ReentrantMutex`. Unlocks on drop when count reaches zero.
///
/// This guard is `!Send` and `!Sync`.
pub struct ReentrantMutexGuard<'a, T> {
    mutex: &'a ReentrantMutex<T>,
    _marker: PhantomData<*const ()>,
}

impl<T> ReentrantMutexGuard<'_, T> {
    /// Temporarily releases the lock if there are waiters and lock count is 1.
    /// Immediately re-acquires after yielding.
    pub fn bump(&mut self) {
        if self.mutex.count.get() == 1 && self.mutex.has_waiters() {
            let tid = self.mutex.owner();

            self.mutex.owner.store(0, Ordering::Relaxed);
            self.mutex.count.set(0);

            unsafe {
                self.mutex.mutex.force_unlock();
            }

            std::mem::forget(self.mutex.mutex.lock());

            self.mutex.owner.store(tid, Ordering::Relaxed);
            self.mutex.count.set(1);
        }
    }
}

impl<T> Deref for ReentrantMutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.mutex.internal.inner.get() }
    }
}

impl<T> Drop for ReentrantMutexGuard<'_, T> {
    fn drop(&mut self) {
        let count = self.mutex.count.get() - 1;
        self.mutex.count.set(count);
        if count == 0 {
            self.mutex.owner.store(0, Ordering::Relaxed);
            unsafe {
                self.mutex.mutex.force_unlock();
            }
        }
    }
}

pub struct ReentrantLockRequest<'a, T> {
    mutex: &'a ReentrantMutex<T>,
    inner: Option<AsyncLockRequest<'a, T>>,
    _pinned: PhantomPinned,
}

impl<'a, T> Future for ReentrantLockRequest<'a, T> {
    type Output = ReentrantMutexGuard<'a, T>;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let rm = this.mutex;

        if this.inner.is_none() {
            if let Some(g) = rm.try_lock() {
                return core::task::Poll::Ready(g);
            }
            this.inner = Some(rm.mutex.lock_async());
        }

        let inner = this.inner.as_mut().unwrap();
        let pinned = unsafe { core::pin::Pin::new_unchecked(inner) };
        match pinned.poll(cx) {
            core::task::Poll::Ready(guard) => {
                this.inner = None;
                core::task::Poll::Ready(rm.acquired(thread_id(), guard))
            }
            core::task::Poll::Pending => core::task::Poll::Pending,
        }
    }
}
