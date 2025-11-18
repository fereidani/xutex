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
    let mutex = Arc::new(Mutex::new(0));

    rt.block_on(async {
        let mut handles = vec![];
        for _ in 0..THREAD_COUNT {
            let mutex_clone = Arc::clone(&mutex);
            let handle = tokio::spawn(async move {
                for _ in 0..INCREMENTS_PER_THREAD {
                    let mut guard = mutex_clone.lock();
                    *guard += 1;
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }
    });

    assert_eq!(*mutex.lock(), THREAD_COUNT * INCREMENTS_PER_THREAD);
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
