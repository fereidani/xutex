//! Loom model-checking tests: exhaustively explore thread interleavings
//! (including weak-memory effects) of every primitive.
//!
//! Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test --release --test loom
//! ```
#![cfg(loom)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use loom::model::Builder;
use loom::thread;
use xutex::{
    AsyncBarrier, AsyncMutex, AsyncNotify, AsyncOnceCell, AsyncRwLock, AsyncSemaphore, Barrier,
    Mutex, Notify, OnceCell, RwLock, Semaphore, TryAcquireError,
};

/// Runs a loom model with a preemption bound that keeps the state space
/// tractable while still exploring every weak-memory behavior loom models.
fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = Builder::new();
    builder.preemption_bound = Some(3);
    builder.max_branches = 50_000;
    builder.location = true;
    builder.check(f);
}

#[test]
fn loom_mutex_handoff() {
    model(|| {
        let mutex = Arc::new(Mutex::new(0usize));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                thread::spawn(move || {
                    *mutex.lock() += 1;
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*mutex.lock(), 2);
    });
}

#[test]
fn loom_mutex_try_lock() {
    model(|| {
        let mutex = Arc::new(Mutex::new(0usize));
        let t = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || {
                if let Some(mut g) = mutex.try_lock() {
                    *g += 1;
                }
            })
        };
        if let Some(mut g) = mutex.try_lock() {
            *g += 1;
        }
        t.join().unwrap();
    });
}

#[test]
fn loom_async_mutex() {
    model(|| {
        let mutex = Arc::new(AsyncMutex::new(0usize));
        let t = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || {
                loom::future::block_on(async {
                    *mutex.lock().await += 1;
                });
            })
        };
        loom::future::block_on(async {
            *mutex.lock().await += 1;
        });
        t.join().unwrap();
        assert_eq!(*mutex.try_lock().unwrap(), 2);
    });
}

#[test]
fn loom_semaphore_release_acquire() {
    model(|| {
        // One permit, two contenders: the permit must be handed over without
        // ever being lost.
        let sem = Arc::new(Semaphore::new(1));
        let t = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || {
                let permit = sem.acquire().unwrap();
                drop(permit);
            })
        };
        let permit = sem.acquire().unwrap();
        drop(permit);
        t.join().unwrap();
        assert_eq!(sem.available_permits(), 1);
    });
}

#[test]
fn loom_semaphore_acquire_many_partial() {
    model(|| {
        // The waiter needs 2 permits released one at a time by two threads:
        // exercises partial assignment under the queue lock.
        let sem = Arc::new(Semaphore::new(0));
        let releasers: Vec<_> = (0..2)
            .map(|_| {
                let sem = Arc::clone(&sem);
                thread::spawn(move || sem.add_permits(1))
            })
            .collect();
        let permit = sem.acquire_many(2).unwrap();
        drop(permit);
        for h in releasers {
            h.join().unwrap();
        }
        assert_eq!(sem.available_permits(), 2);
    });
}

#[test]
fn loom_semaphore_close_race() {
    model(|| {
        let sem = Arc::new(Semaphore::new(0));
        let closer = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || sem.close())
        };
        // Either we fail fast (already closed) or we park and are woken with
        // the closed error; we must never hang.
        assert!(sem.acquire().is_err());
        closer.join().unwrap();
    });
}

#[test]
fn loom_semaphore_try_acquire_vs_release() {
    model(|| {
        let sem = Arc::new(Semaphore::new(0));
        let t = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || sem.add_permits(1))
        };
        // try_acquire may fail (release not yet visible) but must never lose
        // a permit.
        match sem.try_acquire() {
            Ok(p) => drop(p),
            Err(TryAcquireError::NoPermits) => {}
            Err(TryAcquireError::Closed) => panic!("never closed"),
        }
        t.join().unwrap();
        assert_eq!(sem.available_permits(), 1);
    });
}

#[test]
fn loom_async_semaphore_cancel_race() {
    model(|| {
        // The poll-once-then-drop future races with a release: the drop must
        // either remove the waiter from the queue or absorb the in-flight
        // signal and return the permit. Afterwards exactly one permit must
        // remain.
        let sem = Arc::new(AsyncSemaphore::new(0));
        let t = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || sem.add_permits(1))
        };
        {
            let mut fut = Box::pin(sem.acquire());
            let waker = futures_task_noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            let _ = fut.as_mut().poll(&mut cx);
            // fut dropped here, possibly while queued, possibly signaled.
        }
        t.join().unwrap();
        assert_eq!(sem.available_permits(), 1);
    });
}

/// Minimal noop waker without external dependencies.
fn futures_task_noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable, Waker};
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: all vtable functions are no-ops.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

use std::future::Future;

#[test]
fn loom_rwlock_read_write_exclusion() {
    model(|| {
        let lock = Arc::new(RwLock::with_max_readers(0usize, 2));
        let writers_active = Arc::new(AtomicUsize::new(0));
        let t = {
            let lock = Arc::clone(&lock);
            let writers_active = Arc::clone(&writers_active);
            thread::spawn(move || {
                let mut g = lock.write();
                writers_active.store(1, Ordering::Relaxed);
                *g += 1;
                writers_active.store(0, Ordering::Relaxed);
            })
        };
        {
            let g = lock.read();
            // A writer can never be active while we hold a read guard.
            assert_eq!(writers_active.load(Ordering::Relaxed), 0);
            let _ = *g;
        }
        t.join().unwrap();
        assert_eq!(*lock.read(), 1);
    });
}

#[test]
fn loom_rwlock_two_readers() {
    model(|| {
        let lock = Arc::new(RwLock::with_max_readers(7usize, 2));
        let t = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || *lock.read())
        };
        assert_eq!(*lock.read(), 7);
        assert_eq!(t.join().unwrap(), 7);
        assert!(lock.try_write().is_some());
    });
}

#[test]
fn loom_notify_no_lost_wakeup() {
    model(|| {
        // The fundamental notify race: wait() and notify_one() concurrently.
        // The notification must never be lost (wait must always return).
        let notify = Arc::new(Notify::new());
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || notify.wait())
        };
        notify.notify_one();
        waiter.join().unwrap();
    });
}

#[test]
fn loom_notify_waiters_generation() {
    model(|| {
        let notify = Arc::new(AsyncNotify::new());
        let t = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || {
                // Future created before the broadcast may or may not observe
                // it depending on the interleaving of creation vs. bump; it
                // must complete either via generation or via explicit wake,
                // never hang, and never corrupt state.
                let fut = notify.notified();
                notify.notify_one();
                loom::future::block_on(fut);
            })
        };
        notify.notify_waiters();
        t.join().unwrap();
    });
}

#[test]
fn loom_barrier_two_parties() {
    model(|| {
        let barrier = Arc::new(Barrier::new(2));
        let t = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || barrier.wait().is_leader())
        };
        let l1 = barrier.wait().is_leader();
        let l2 = t.join().unwrap();
        // Exactly one leader.
        assert!(l1 ^ l2);
    });
}

#[test]
fn loom_barrier_reuse() {
    model(|| {
        let barrier = Arc::new(AsyncBarrier::new(2));
        let t = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait_sync();
                barrier.wait_sync();
            })
        };
        barrier.wait_sync();
        barrier.wait_sync();
        t.join().unwrap();
    });
}

#[test]
fn loom_once_cell_racing_init() {
    model(|| {
        let cell = Arc::new(OnceCell::new());
        let t = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || *cell.get_or_init(|| 1usize))
        };
        let a = *cell.get_or_init(|| 2usize);
        let b = t.join().unwrap();
        // Exactly one initializer wins and everyone agrees.
        assert_eq!(a, b);
        assert_eq!(cell.get(), Some(&a));
    });
}

#[test]
fn loom_once_cell_set_race() {
    model(|| {
        let cell = Arc::new(AsyncOnceCell::new());
        let t = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || cell.set(1usize).is_ok())
        };
        let mine = cell.set(2usize).is_ok();
        let theirs = t.join().unwrap();
        // Exactly one set succeeds.
        assert!(mine ^ theirs);
        let v = *cell.get().unwrap();
        assert!(v == 1 || v == 2);
    });
}

#[test]
fn loom_async_rwlock_write_handoff() {
    model(|| {
        let lock = Arc::new(AsyncRwLock::with_max_readers(0usize, 2));
        let t = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                loom::future::block_on(async {
                    *lock.write().await += 1;
                });
            })
        };
        loom::future::block_on(async {
            let _ = *lock.read().await;
        });
        t.join().unwrap();
        assert_eq!(*lock.try_read().unwrap(), 1);
    });
}
