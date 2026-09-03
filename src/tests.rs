use super::*;
use std::sync::Arc;
use std::thread;

#[test]
fn test_mutex_new() {
    let mutex = Mutex::new(42);
    assert_eq!(*mutex.lock(), 42);
}

#[test]
fn test_mutex_lock_unlock() {
    let mutex = Mutex::new(0);
    {
        let mut guard = mutex.lock();
        *guard = 10;
    }
    assert_eq!(*mutex.lock(), 10);
}

#[test]
fn test_mutex_try_lock_success() {
    let mutex = Mutex::new(5);
    let guard = mutex.try_lock();
    assert!(guard.is_some());
    assert_eq!(*guard.unwrap(), 5);
}

#[test]
fn test_mutex_try_lock_fail() {
    let mutex = Mutex::new(5);
    let _guard = mutex.lock();
    assert!(mutex.try_lock().is_none());
}

#[cfg(miri)]
const THREAD_COUNT: usize = 16;
#[cfg(not(miri))]
const THREAD_COUNT: usize = 128;
#[cfg(miri)]
const INCREMENTS_PER_THREAD: usize = 32;
#[cfg(not(miri))]
const INCREMENTS_PER_THREAD: usize = 1024 * 10;

#[test]
fn test_mutex_multithreaded() {
    let mutex = Arc::new(Mutex::new(0));
    let mut handles = vec![];

    for _ in 0..THREAD_COUNT {
        let mutex_clone = Arc::clone(&mutex);
        let handle = thread::spawn(move || {
            for _ in 0..INCREMENTS_PER_THREAD {
                let mut guard = mutex_clone.lock();
                *guard += 1;
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(*mutex.lock(), THREAD_COUNT * INCREMENTS_PER_THREAD);
}

#[test]
fn test_tokio_multicoroutine() {
    use tokio::runtime::Builder;

    let rt = Builder::new_multi_thread()
        .worker_threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        )
        .enable_all()
        .build()
        .unwrap();
    let mutex = Arc::new(AsyncMutex::new(0));

    rt.block_on(async {
        let mut handles = vec![];
        for _ in 0..THREAD_COUNT {
            let mutex_clone = Arc::clone(&mutex);
            let handle = tokio::spawn(async move {
                for _ in 0..INCREMENTS_PER_THREAD {
                    let mut guard = mutex_clone.lock().await;
                    *guard += 1;
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }
    });

    rt.block_on(async {
        assert_eq!(*mutex.lock().await, THREAD_COUNT * INCREMENTS_PER_THREAD);
    });
}

#[test]
fn test_mutex_multiple_locks() {
    let mutex = Mutex::new(0);
    for i in 1..=10 {
        let mut guard = mutex.lock();
        *guard += i;
    }
    assert_eq!(*mutex.lock(), 55);
}

#[test]
fn test_mutex_as_async_conversion() {
    let mutex = Mutex::new(100);
    let async_ref = mutex.as_async();
    assert_eq!(*async_ref.try_lock().unwrap(), 100);
}

#[test]
fn test_async_mutex_as_sync_conversion() {
    let async_mutex = AsyncMutex::new(200);
    let sync_ref = async_mutex.as_sync();
    assert_eq!(*sync_ref.try_lock().unwrap(), 200);
}

// ---------------------------------------------------------------------------
// Helpers for manual future polling
// ---------------------------------------------------------------------------

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Polls a pinned future once with a noop waker.
fn poll_once<F: Future>(fut: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    fut.poll(&mut cx)
}

macro_rules! pin_mut {
    ($x:ident) => {
        let mut $x = $x;
        // SAFETY: the future is shadowed and never moved again.
        #[allow(unused_mut)]
        let mut $x = unsafe { Pin::new_unchecked(&mut $x) };
    };
}

// ---------------------------------------------------------------------------
// Semaphore
// ---------------------------------------------------------------------------

#[test]
fn test_semaphore_basic() {
    let sem = Semaphore::new(3);
    assert_eq!(sem.available_permits(), 3);
    let p1 = sem.acquire().unwrap();
    let p2 = sem.acquire_many(2).unwrap();
    assert_eq!(sem.available_permits(), 0);
    assert!(matches!(sem.try_acquire(), Err(TryAcquireError::NoPermits)));
    drop(p1);
    assert_eq!(sem.available_permits(), 1);
    drop(p2);
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn test_semaphore_forget_and_add() {
    let sem = Semaphore::new(4);
    let p = sem.acquire_many(2).unwrap();
    p.forget();
    assert_eq!(sem.available_permits(), 2);
    sem.add_permits(3);
    assert_eq!(sem.available_permits(), 5);
    assert_eq!(sem.forget_permits(10), 5);
    assert_eq!(sem.available_permits(), 0);
}

#[test]
fn test_semaphore_permit_split_merge() {
    let sem = Semaphore::new(10);
    let mut p = sem.acquire_many(5).unwrap();
    let q = p.split(2).unwrap();
    assert_eq!(p.num_permits(), 3);
    assert_eq!(q.num_permits(), 2);
    assert!(p.split(4).is_none());
    p.merge(q);
    assert_eq!(p.num_permits(), 5);
    drop(p);
    assert_eq!(sem.available_permits(), 10);
}

#[test]
fn test_semaphore_close() {
    let sem = Arc::new(Semaphore::new(0));
    let waiter = {
        let sem = Arc::clone(&sem);
        thread::spawn(move || sem.acquire().is_err())
    };
    // Give the waiter time to enqueue, then close.
    thread::sleep(std::time::Duration::from_millis(50));
    sem.close();
    assert!(waiter.join().unwrap());
    assert!(sem.is_closed());
    assert!(matches!(sem.try_acquire(), Err(TryAcquireError::Closed)));
    assert!(sem.acquire().is_err());
}

#[test]
fn test_semaphore_concurrency_limit() {
    const LIMIT: usize = 4;
    let sem = Arc::new(Semaphore::new(LIMIT));
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..THREAD_COUNT)
        .map(|_| {
            let sem = Arc::clone(&sem);
            let active = Arc::clone(&active);
            let max_seen = Arc::clone(&max_seen);
            thread::spawn(move || {
                for _ in 0..INCREMENTS_PER_THREAD / 16 {
                    let _permit = sem.acquire().unwrap();
                    let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                    max_seen.fetch_max(now, Ordering::AcqRel);
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(max_seen.load(Ordering::Acquire) <= LIMIT);
    assert_eq!(sem.available_permits(), LIMIT);
}

#[test]
fn test_semaphore_acquire_many_blocks_until_enough() {
    let sem = Arc::new(Semaphore::new(0));
    let handle = {
        let sem = Arc::clone(&sem);
        thread::spawn(move || {
            let p = sem.acquire_many(3).unwrap();
            assert_eq!(p.num_permits(), 3);
        })
    };
    // Feed permits one by one; the waiter must only wake when all 3 exist.
    for _ in 0..3 {
        thread::sleep(std::time::Duration::from_millis(20));
        sem.add_permits(1);
    }
    handle.join().unwrap();
    // The waiter's permit is dropped when its thread finishes.
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn test_async_semaphore_tokio() {
    use tokio::runtime::Builder;
    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let sem = Arc::new(AsyncSemaphore::new(3));
    let active = Arc::new(AtomicUsize::new(0));

    rt.block_on(async {
        let mut handles = vec![];
        for _ in 0..64 {
            let sem = Arc::clone(&sem);
            let active = Arc::clone(&active);
            handles.push(tokio::spawn(async move {
                for _ in 0..64 {
                    let _permit = sem.acquire().await.unwrap();
                    let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                    assert!(now <= 3);
                    tokio::task::yield_now().await;
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    });
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn test_async_semaphore_cancellation() {
    let sem = AsyncSemaphore::new(1);
    let _held = sem.try_acquire().unwrap();

    // Enqueue a waiter and drop it while pending.
    {
        let fut = sem.acquire();
        pin_mut!(fut);
        assert!(poll_once(fut.as_mut()).is_pending());
    } // dropped here: must remove itself from the queue

    drop(_held);
    // The semaphore must be fully functional afterwards.
    let p = sem.try_acquire().unwrap();
    drop(p);
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn test_async_semaphore_fifo_handoff() {
    let sem = AsyncSemaphore::new(1);
    let held = sem.try_acquire().unwrap();

    let fut1 = sem.acquire();
    pin_mut!(fut1);
    assert!(poll_once(fut1.as_mut()).is_pending());

    let fut2 = sem.acquire();
    pin_mut!(fut2);
    assert!(poll_once(fut2.as_mut()).is_pending());

    // Releasing hands the permit to fut1 (FIFO), not fut2.
    drop(held);
    assert!(matches!(sem.try_acquire(), Err(TryAcquireError::NoPermits)));
    match poll_once(fut2.as_mut()) {
        Poll::Pending => {}
        _ => panic!("fut2 must still be pending"),
    }
    let p1 = match poll_once(fut1.as_mut()) {
        Poll::Ready(Ok(p)) => p,
        _ => panic!("fut1 must be ready"),
    };
    drop(p1);
    match poll_once(fut2.as_mut()) {
        Poll::Ready(Ok(_)) => {}
        _ => panic!("fut2 must be ready after p1 drop"),
    }
}

#[test]
fn test_semaphore_conversions() {
    let sem = Semaphore::new(2);
    assert_eq!(sem.as_async().available_permits(), 2);
    let async_sem = sem.to_async();
    assert_eq!(async_sem.as_sync().available_permits(), 2);
    let arc = Arc::new(async_sem.to_sync());
    let async_arc = arc.clone_async();
    assert_eq!(async_arc.available_permits(), 2);
}

// ---------------------------------------------------------------------------
// RwLock
// ---------------------------------------------------------------------------

#[test]
fn test_rwlock_basic() {
    let lock = RwLock::new(5);
    {
        let r1 = lock.read();
        let r2 = lock.read();
        assert_eq!(*r1 + *r2, 10);
        assert!(lock.try_write().is_none());
    }
    {
        let mut w = lock.write();
        *w += 1;
        assert!(lock.try_read().is_none());
        assert!(lock.try_write().is_none());
    }
    assert_eq!(*lock.read(), 6);
}

#[test]
fn test_rwlock_get_mut_into_inner() {
    let mut lock = RwLock::new(7);
    *lock.get_mut() += 1;
    assert_eq!(lock.into_inner(), 8);
}

#[test]
fn test_rwlock_downgrade() {
    let lock = RwLock::new(1);
    let mut w = lock.write();
    *w = 2;
    let r = w.downgrade();
    // Another reader can join while we still hold the downgraded read lock.
    let r2 = lock.try_read().expect("read must be possible");
    assert_eq!(*r + *r2, 4);
    drop((r, r2));
    assert!(lock.try_write().is_some());
}

#[test]
fn test_rwlock_multithreaded() {
    let lock = Arc::new(RwLock::new(0usize));
    let readers_active = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];
    for i in 0..THREAD_COUNT {
        let lock = Arc::clone(&lock);
        let readers_active = Arc::clone(&readers_active);
        handles.push(thread::spawn(move || {
            for _ in 0..INCREMENTS_PER_THREAD / 16 {
                if i % 4 == 0 {
                    // Writer: must be exclusive.
                    let mut guard = lock.write();
                    assert_eq!(readers_active.load(Ordering::Acquire), 0);
                    *guard += 1;
                } else {
                    // Reader: many at once, but never with a writer.
                    let guard = lock.read();
                    readers_active.fetch_add(1, Ordering::AcqRel);
                    let _v = *guard;
                    readers_active.fetch_sub(1, Ordering::AcqRel);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let writers = THREAD_COUNT.div_ceil(4);
    assert_eq!(*lock.read(), writers * (INCREMENTS_PER_THREAD / 16));
}

#[test]
fn test_async_rwlock_tokio() {
    use tokio::runtime::Builder;
    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let lock = Arc::new(AsyncRwLock::new(0usize));

    rt.block_on(async {
        let mut handles = vec![];
        for i in 0..32 {
            let lock = Arc::clone(&lock);
            handles.push(tokio::spawn(async move {
                for _ in 0..128 {
                    if i % 4 == 0 {
                        *lock.write().await += 1;
                    } else {
                        let _ = *lock.read().await;
                    }
                    tokio::task::yield_now().await;
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*lock.read().await, 8 * 128);
    });
}

#[test]
fn test_async_rwlock_write_waits_for_readers() {
    let lock = AsyncRwLock::new(0);
    let r1 = lock.try_read().unwrap();
    let r2 = lock.try_read().unwrap();

    let w = lock.write();
    pin_mut!(w);
    assert!(poll_once(w.as_mut()).is_pending());

    // A queued writer blocks new readers (fairness).
    assert!(lock.try_read().is_none());

    drop(r1);
    assert!(poll_once(w.as_mut()).is_pending());
    drop(r2);
    match poll_once(w.as_mut()) {
        Poll::Ready(mut guard) => *guard += 1,
        Poll::Pending => panic!("writer must acquire after all readers left"),
    }
}

#[test]
fn test_async_rwlock_cancellation() {
    let lock = AsyncRwLock::new(0);
    let r = lock.try_read().unwrap();
    {
        let w = lock.write();
        pin_mut!(w);
        assert!(poll_once(w.as_mut()).is_pending());
    } // cancelled writer must return its partial permits
    drop(r);
    // All permits must be back: a writer can acquire immediately.
    assert!(lock.try_write().is_some());
}

#[test]
fn test_rwlock_conversions() {
    let lock = RwLock::new(3);
    assert_eq!(*lock.as_async().try_read().unwrap(), 3);
    let async_lock = lock.to_async();
    assert_eq!(*async_lock.as_sync().read(), 3);
    let arc = Arc::new(async_lock);
    let sync_arc = arc.clone_sync();
    assert_eq!(*sync_arc.read(), 3);
}

// ---------------------------------------------------------------------------
// Notify
// ---------------------------------------------------------------------------

#[test]
fn test_notify_permit_stored() {
    let notify = Notify::new();
    notify.notify_one();
    // Must not block: a permit is stored.
    notify.wait();
}

#[test]
fn test_notify_cross_thread() {
    let notify = Arc::new(Notify::new());
    let handle = {
        let notify = Arc::clone(&notify);
        thread::spawn(move || notify.wait())
    };
    thread::sleep(std::time::Duration::from_millis(50));
    notify.notify_one();
    handle.join().unwrap();
}

#[test]
fn test_notify_waiters_wakes_all() {
    let notify = Arc::new(Notify::new());
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let mut handles = vec![];
    for _ in 0..8 {
        let notify = Arc::clone(&notify);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            notify.wait();
        }));
    }
    barrier.wait();
    // Waiters may still be between barrier and wait; keep broadcasting.
    loop {
        notify.notify_waiters();
        if handles.iter().all(|h| h.is_finished()) {
            break;
        }
        thread::yield_now();
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn test_async_notified_created_before_notify_waiters() {
    let notify = AsyncNotify::new();
    let fut = notify.notified();
    notify.notify_waiters();
    pin_mut!(fut);
    // Must complete: the future was created before notify_waiters.
    assert!(poll_once(fut.as_mut()).is_ready());

    // A future created after must not complete.
    let fut2 = notify.notified();
    pin_mut!(fut2);
    assert!(poll_once(fut2.as_mut()).is_pending());
}

#[test]
fn test_async_notify_drop_redelivers() {
    let notify = AsyncNotify::new();

    // Box::pin so that dropping the handle drops the future itself.
    let mut fut1 = Box::pin(notify.notified());
    assert!(poll_once(fut1.as_mut()).is_pending());

    let mut fut2 = Box::pin(notify.notified());
    assert!(poll_once(fut2.as_mut()).is_pending());

    notify.notify_one();
    // fut1 was signaled; dropping it must pass the wakeup to fut2.
    drop(fut1);
    assert!(poll_once(fut2.as_mut()).is_ready());
}

#[test]
fn test_async_notify_tokio() {
    use tokio::runtime::Builder;
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let notify = Arc::new(AsyncNotify::new());
    rt.block_on(async {
        let handle = {
            let notify = Arc::clone(&notify);
            tokio::spawn(async move { notify.notified().await })
        };
        tokio::task::yield_now().await;
        notify.notify_one();
        handle.await.unwrap();
    });
}

// ---------------------------------------------------------------------------
// Barrier
// ---------------------------------------------------------------------------

#[test]
fn test_barrier_sync_rounds() {
    let barrier = Arc::new(Barrier::new(8));
    for _round in 0..4 {
        let mut handles = vec![];
        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || barrier.wait().is_leader()));
        }
        let leaders = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|l| *l)
            .count();
        assert_eq!(leaders, 1);
    }
}

#[test]
fn test_async_barrier_tokio() {
    use tokio::runtime::Builder;
    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let barrier = Arc::new(AsyncBarrier::new(16));
    rt.block_on(async {
        let mut handles = vec![];
        for _ in 0..16 {
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(
                async move { barrier.wait().await.is_leader() },
            ));
        }
        let mut leaders = 0;
        for h in handles {
            if h.await.unwrap() {
                leaders += 1;
            }
        }
        assert_eq!(leaders, 1);
    });
}

#[test]
fn test_async_barrier_cancellation_withdraws_arrival() {
    let barrier = AsyncBarrier::new(2);
    {
        let w = barrier.wait();
        pin_mut!(w);
        assert!(poll_once(w.as_mut()).is_pending());
    } // dropped: arrival withdrawn

    // A fresh pair must complete on its own (the cancelled arrival must not
    // count towards the round).
    let w1 = barrier.wait();
    pin_mut!(w1);
    assert!(poll_once(w1.as_mut()).is_pending());
    let w2 = barrier.wait();
    pin_mut!(w2);
    match poll_once(w2.as_mut()) {
        Poll::Ready(result) => assert!(result.is_leader()),
        Poll::Pending => panic!("second arrival must complete the round"),
    }
    assert!(poll_once(w1.as_mut()).is_ready());
}

// ---------------------------------------------------------------------------
// OnceCell
// ---------------------------------------------------------------------------

#[test]
fn test_once_cell_single_init() {
    let cell = Arc::new(OnceCell::new());
    let inits = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];
    for i in 0..16 {
        let cell = Arc::clone(&cell);
        let inits = Arc::clone(&inits);
        handles.push(thread::spawn(move || {
            *cell.get_or_init(|| {
                inits.fetch_add(1, Ordering::AcqRel);
                i
            })
        }));
    }
    let first = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(inits.load(Ordering::Acquire), 1);
    assert!(first.iter().all(|v| v == &first[0]));
    assert_eq!(cell.get(), Some(&first[0]));
}

#[test]
fn test_once_cell_set_get_take() {
    let mut cell = OnceCell::new();
    assert_eq!(cell.get(), None);
    assert!(cell.set(5).is_ok());
    assert!(matches!(
        cell.set(6),
        Err(SetError::AlreadyInitializedError(6))
    ));
    assert_eq!(cell.get(), Some(&5));
    *cell.get_mut().unwrap() = 7;
    assert_eq!(cell.take(), Some(7));
    assert_eq!(cell.get(), None);
    assert!(cell.set(8).is_ok());
    assert_eq!(cell.into_inner(), Some(8));
}

#[test]
fn test_once_cell_try_init_failure_then_success() {
    let cell = OnceCell::new();
    let r: Result<&i32, &str> = cell.get_or_try_init(|| Err("boom"));
    assert_eq!(r.unwrap_err(), "boom");
    assert_eq!(cell.get(), None);
    let v = cell.get_or_init(|| 42);
    assert_eq!(*v, 42);
}

#[test]
fn test_once_cell_panic_recovery() {
    let cell = Arc::new(OnceCell::new());
    let cell2 = Arc::clone(&cell);
    let result = thread::spawn(move || {
        cell2.get_or_init(|| -> i32 { panic!("init failed") });
    })
    .join();
    assert!(result.is_err());
    // The cell must be usable after a panicking initializer.
    assert_eq!(*cell.get_or_init(|| 9), 9);
}

#[test]
fn test_async_once_cell_takeover_on_cancel() {
    let cell = AsyncOnceCell::new();

    // First initializer starts but its future never completes. Box::pin so
    // that dropping the handle drops (cancels) the future itself.
    let mut pending_init = Box::pin(cell.get_or_init(std::future::pending::<i32>));
    assert!(poll_once(pending_init.as_mut()).is_pending());

    // Second caller parks behind the initializer.
    let second = cell.get_or_init(|| std::future::ready(7));
    pin_mut!(second);
    assert!(poll_once(second.as_mut()).is_pending());

    // Cancelling the initializer must hand the role to the second caller.
    drop(pending_init);
    match poll_once(second.as_mut()) {
        Poll::Ready(v) => assert_eq!(*v, 7),
        Poll::Pending => panic!("second caller must take over initialization"),
    }
    assert_eq!(cell.get(), Some(&7));
}

#[test]
fn test_async_once_cell_tokio() {
    use tokio::runtime::Builder;
    let rt = Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let cell = Arc::new(AsyncOnceCell::new());
    let inits = Arc::new(AtomicUsize::new(0));
    rt.block_on(async {
        let mut handles = vec![];
        for i in 0..32 {
            let cell = Arc::clone(&cell);
            let inits = Arc::clone(&inits);
            handles.push(tokio::spawn(async move {
                *cell
                    .get_or_init(|| async move {
                        inits.fetch_add(1, Ordering::AcqRel);
                        tokio::task::yield_now().await;
                        i
                    })
                    .await
            }));
        }
        let mut values = vec![];
        for h in handles {
            values.push(h.await.unwrap());
        }
        assert_eq!(inits.load(Ordering::Acquire), 1);
        assert!(values.iter().all(|v| v == &values[0]));
    });
}

// ---------------------------------------------------------------------------
// Regressions: cancelled head-of-line waiters must not strand the queue
// ---------------------------------------------------------------------------

#[test]
fn test_async_semaphore_cancelled_head_redrains_queue() {
    let sem = AsyncSemaphore::new(2);
    let small = sem.acquire();
    pin_mut!(small);
    {
        // Head of the line: needs more than the pool holds, so it parks while
        // the two permits stay in the pool.
        let big = sem.acquire_many(3);
        pin_mut!(big);
        assert!(poll_once(big.as_mut()).is_pending());
        // FIFO: queued behind it although its single permit is available.
        assert!(poll_once(small.as_mut()).is_pending());
        assert_eq!(sem.available_permits(), 2);
    } // `big` cancelled: the new head must be matched against the pool again
    match poll_once(small.as_mut()) {
        Poll::Ready(Ok(p)) => {
            assert_eq!(p.num_permits(), 1);
            assert_eq!(sem.available_permits(), 1);
        }
        _ => panic!("waiter stranded behind a cancelled acquire_many"),
    }
    assert_eq!(sem.available_permits(), 2);
}

#[test]
fn test_async_rwlock_cancelled_writer_unblocks_queued_reader() {
    let lock = AsyncRwLock::new(0);
    let r1 = lock.try_read().unwrap();
    let r2 = lock.read();
    pin_mut!(r2);
    {
        let w = lock.write();
        pin_mut!(w);
        assert!(poll_once(w.as_mut()).is_pending()); // waits for r1
        assert!(poll_once(r2.as_mut()).is_pending()); // fair: behind the writer
    } // writer cancelled (e.g. by a timeout)
    assert!(
        poll_once(r2.as_mut()).is_ready(),
        "reader stranded behind a cancelled writer"
    );
    drop(r1);
    assert!(lock.try_write().is_some());
}

#[test]
fn test_async_semaphore_zero_permit_request_behind_queue() {
    let sem = AsyncSemaphore::new(1);
    let held = sem.try_acquire().unwrap();
    let one = sem.acquire();
    pin_mut!(one);
    assert!(poll_once(one.as_mut()).is_pending());
    let zero = sem.acquire_many(0);
    pin_mut!(zero);
    // FIFO: even a request for nothing queues behind an existing waiter.
    assert!(poll_once(zero.as_mut()).is_pending());
    // Handing the permit to `one` leaves nothing in the pool, but `zero` needs
    // nothing and must be released in the same drain.
    drop(held);
    let p1 = match poll_once(one.as_mut()) {
        Poll::Ready(Ok(p)) => p,
        _ => panic!("first waiter must get the permit"),
    };
    match poll_once(zero.as_mut()) {
        Poll::Ready(Ok(p)) => assert_eq!(p.num_permits(), 0),
        _ => panic!("zero-permit waiter stranded"),
    }
    drop(p1);
    assert_eq!(sem.available_permits(), 1);
}
