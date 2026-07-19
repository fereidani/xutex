use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::{Arc, Mutex as StdMutex};
use std::thread;
use xutex::{AsyncMutex, Mutex};

const THREAD_COUNT: usize = 64;

// Import your custom mutex implementation

fn benchmark_std_mutex_uncontended(c: &mut Criterion) {
    c.bench_function("std_mutex_uncontended", |b| {
        let mutex = StdMutex::new(0);
        b.iter(|| {
            let mut guard = mutex.lock().unwrap();
            *guard += 1;
            black_box(*guard);
        });
    });
}

fn benchmark_parkinglot_mutex_uncontended(c: &mut Criterion) {
    c.bench_function("parkinglot_mutex_uncontended", |b| {
        let mutex = parking_lot::Mutex::new(0);
        b.iter(|| {
            let mut guard = mutex.lock();
            *guard += 1;
            black_box(*guard);
        });
    });
}

fn benchmark_xutex_uncontended(c: &mut Criterion) {
    c.bench_function("xutex_uncontended", |b| {
        let mutex = Mutex::new(0);
        b.iter(|| {
            let mut guard = mutex.lock();
            *guard += 1;
            black_box(*guard);
        });
    });
}

fn benchmark_tokionative_uncontended(c: &mut Criterion) {
    c.bench_function("tokionative_uncontended", |b| {
        let mutex = tokio::sync::Mutex::new(0);
        b.iter(|| {
            swait::swait(async {
                let mut guard = mutex.lock().await;
                *guard += 1;
                black_box(*guard);
            });
        });
    });
}

fn benchmark_xutex_uncontended_async(c: &mut Criterion) {
    c.bench_function("xutex_uncontended_async", |b| {
        let mutex = AsyncMutex::new(0);
        b.iter(|| {
            swait::swait(async {
                let mut guard = mutex.lock().await;
                *guard += 1;
                black_box(*guard);
            });
        });
    });
}

fn benchmark_std_mutex_contended(c: &mut Criterion) {
    c.bench_function("std_mutex_contended", |b| {
        let mutex = Arc::new(StdMutex::new(0));
        b.iter(|| {
            let mut handles = vec![];
            for _ in 0..THREAD_COUNT {
                let mutex = Arc::clone(&mutex);
                handles.push(thread::spawn(move || {
                    for _ in 0..100 {
                        let mut guard = mutex.lock().unwrap();
                        *guard += 1;
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
    });
}

fn benchmark_parkinglot_mutex_contended(c: &mut Criterion) {
    c.bench_function("parkinglot_mutex_contended", |b| {
        let mutex = Arc::new(parking_lot::Mutex::new(0));
        b.iter(|| {
            let mut handles = vec![];
            for _ in 0..THREAD_COUNT {
                let mutex = Arc::clone(&mutex);
                handles.push(thread::spawn(move || {
                    for _ in 0..100 {
                        let mut guard = mutex.lock();
                        *guard += 1;
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
    });
}

fn benchmark_xutex_contended(c: &mut Criterion) {
    c.bench_function("xutex_contended", |b| {
        let mutex = Arc::new(Mutex::new(0));
        b.iter(|| {
            let mut handles = vec![];
            for _ in 0..THREAD_COUNT {
                let mutex = Arc::clone(&mutex);
                handles.push(thread::spawn(move || {
                    for _ in 0..100 {
                        let mut guard = mutex.lock();
                        *guard += 1;
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
    });
}

fn core_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn benchmark_xutex_contended_tokio(c: &mut Criterion) {
    use tokio::runtime::Builder;
    c.bench_function("xutex_contended_tokio", |b| {
        let runtime = Builder::new_multi_thread()
            .worker_threads(core_count())
            .enable_all()
            .build()
            .unwrap();
        let mutex = Arc::new(AsyncMutex::new(0));
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let mutex = Arc::clone(&mutex);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            let mut guard = mutex.lock().await;
                            *guard += 1;
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        });
    });
}

fn benchmark_xutex_contended_tokio_current_thread(c: &mut Criterion) {
    use tokio::runtime::Builder;
    c.bench_function("xutex_contended_tokio_current_thread", |b| {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let mutex = Arc::new(AsyncMutex::new(0));
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let mutex = Arc::clone(&mutex);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            let mut guard = mutex.lock().await;
                            *guard += 1;
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        });
    });
}

fn benchmark_xutex_contended_monoio(_c: &mut Criterion) {
    #[cfg(target_os = "linux")]
    _c.bench_function("xutex_contended_monoio", |b| {
        let mut runtime = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
            .build()
            .unwrap();
        let mutex = Arc::new(AsyncMutex::new(0));
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let mutex = Arc::clone(&mutex);
                    handles.push(monoio::spawn(async move {
                        for _ in 0..100 {
                            let mut guard = mutex.lock().await;
                            *guard += 1;
                        }
                    }));
                }
                for handle in handles {
                    handle.await;
                }
            });
        });
    });
}

fn benchmark_xutex_contended_swait(c: &mut Criterion) {
    c.bench_function("xutex_contended_swait", |b| {
        //let mut futures = vec![];
        let mutex = Arc::new(AsyncMutex::new(0));
        b.iter(|| {
            let mut handles = vec![];
            for _ in 0..THREAD_COUNT {
                let mutex = Arc::clone(&mutex);
                handles.push(async move {
                    for _ in 0..100 {
                        let mut guard = mutex.lock().await;
                        *guard += 1;
                    }
                });
            }
            for handle in handles {
                swait::swait(handle);
            }
        });
    });
}

fn benchmark_tokionative_contended_tokio(c: &mut Criterion) {
    use tokio::runtime::Builder;
    c.bench_function("tokionative_contended", |b| {
        let runtime = Builder::new_multi_thread()
            .worker_threads(core_count())
            .enable_all()
            .build()
            .unwrap();
        let mutex = Arc::new(tokio::sync::Mutex::new(0));
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let mutex = Arc::clone(&mutex);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            let mut guard = mutex.lock().await;
                            *guard += 1;
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        });
    });
}

fn benchmark_tokionative_contended_tokio_current_thread(c: &mut Criterion) {
    use tokio::runtime::Builder;
    c.bench_function("tokionative_contended_current_thread", |b| {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let mutex = Arc::new(tokio::sync::Mutex::new(0));
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let mutex = Arc::clone(&mutex);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            let mut guard = mutex.lock().await;
                            *guard += 1;
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        });
    });
}

fn benchmark_std_mutex_try_lock(c: &mut Criterion) {
    c.bench_function("std_mutex_try_lock", |b| {
        let mutex = StdMutex::new(0);
        b.iter(|| {
            if let Ok(mut guard) = mutex.try_lock() {
                *guard += 1;
                black_box(*guard);
            }
        });
    });
}

fn benchmark_xutex_try_lock(c: &mut Criterion) {
    c.bench_function("xutex_try_lock", |b| {
        let mutex = Mutex::new(0);
        b.iter(|| {
            if let Some(mut guard) = mutex.try_lock() {
                *guard += 1;
                black_box(*guard);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// RwLock benchmarks
// ---------------------------------------------------------------------------

fn benchmark_rwlock_read_uncontended(c: &mut Criterion) {
    let mut g = c.benchmark_group("rwlock_read_uncontended");
    let lock = xutex::RwLock::new(0usize);
    g.bench_function("xutex", |b| {
        b.iter(|| {
            black_box(*lock.read());
        })
    });
    let lock = std::sync::RwLock::new(0usize);
    g.bench_function("std", |b| {
        b.iter(|| {
            black_box(*lock.read().unwrap());
        })
    });
    let lock = parking_lot::RwLock::new(0usize);
    g.bench_function("parking_lot", |b| {
        b.iter(|| {
            black_box(*lock.read());
        })
    });
    let lock = tokio::sync::RwLock::new(0usize);
    g.bench_function("tokio", |b| {
        b.iter(|| {
            swait::swait(async {
                black_box(*lock.read().await);
            })
        })
    });
    g.finish();
}

fn benchmark_rwlock_write_uncontended(c: &mut Criterion) {
    let mut g = c.benchmark_group("rwlock_write_uncontended");
    let lock = xutex::RwLock::new(0usize);
    g.bench_function("xutex", |b| {
        b.iter(|| {
            *lock.write() += 1;
        })
    });
    let lock = std::sync::RwLock::new(0usize);
    g.bench_function("std", |b| {
        b.iter(|| {
            *lock.write().unwrap() += 1;
        })
    });
    let lock = parking_lot::RwLock::new(0usize);
    g.bench_function("parking_lot", |b| {
        b.iter(|| {
            *lock.write() += 1;
        })
    });
    let lock = tokio::sync::RwLock::new(0usize);
    g.bench_function("tokio", |b| {
        b.iter(|| {
            swait::swait(async {
                *lock.write().await += 1;
            })
        })
    });
    g.finish();
}

/// Read-heavy contended workload on a multi-threaded tokio runtime:
/// 7 readers to 1 writer per task batch.
fn benchmark_rwlock_contended_tokio(c: &mut Criterion) {
    use tokio::runtime::Builder;
    let mut g = c.benchmark_group("rwlock_contended_tokio");
    g.sample_size(10);

    let runtime = Builder::new_multi_thread()
        .worker_threads(core_count())
        .enable_all()
        .build()
        .unwrap();

    let lock = Arc::new(xutex::AsyncRwLock::new(0usize));
    g.bench_function("xutex", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for i in 0..THREAD_COUNT {
                    let lock = Arc::clone(&lock);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            if i % 8 == 0 {
                                *lock.write().await += 1;
                            } else {
                                black_box(*lock.read().await);
                            }
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        })
    });

    let lock = Arc::new(tokio::sync::RwLock::new(0usize));
    g.bench_function("tokio", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for i in 0..THREAD_COUNT {
                    let lock = Arc::clone(&lock);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            if i % 8 == 0 {
                                *lock.write().await += 1;
                            } else {
                                black_box(*lock.read().await);
                            }
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        })
    });
    g.finish();
}

fn benchmark_rwlock_contended_sync(c: &mut Criterion) {
    let mut g = c.benchmark_group("rwlock_contended_sync");
    g.sample_size(10);

    let lock = Arc::new(xutex::RwLock::new(0usize));
    g.bench_function("xutex", |b| {
        b.iter(|| {
            let mut handles = vec![];
            for i in 0..THREAD_COUNT {
                let lock = Arc::clone(&lock);
                handles.push(thread::spawn(move || {
                    for _ in 0..100 {
                        if i % 8 == 0 {
                            *lock.write() += 1;
                        } else {
                            black_box(*lock.read());
                        }
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        })
    });

    let lock = Arc::new(std::sync::RwLock::new(0usize));
    g.bench_function("std", |b| {
        b.iter(|| {
            let mut handles = vec![];
            for i in 0..THREAD_COUNT {
                let lock = Arc::clone(&lock);
                handles.push(thread::spawn(move || {
                    for _ in 0..100 {
                        if i % 8 == 0 {
                            *lock.write().unwrap() += 1;
                        } else {
                            black_box(*lock.read().unwrap());
                        }
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        })
    });

    let lock = Arc::new(parking_lot::RwLock::new(0usize));
    g.bench_function("parking_lot", |b| {
        b.iter(|| {
            let mut handles = vec![];
            for i in 0..THREAD_COUNT {
                let lock = Arc::clone(&lock);
                handles.push(thread::spawn(move || {
                    for _ in 0..100 {
                        if i % 8 == 0 {
                            *lock.write() += 1;
                        } else {
                            black_box(*lock.read());
                        }
                    }
                }));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        })
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// Semaphore benchmarks
// ---------------------------------------------------------------------------

fn benchmark_semaphore_uncontended(c: &mut Criterion) {
    let mut g = c.benchmark_group("semaphore_uncontended");
    let sem = xutex::Semaphore::new(8);
    g.bench_function("xutex", |b| {
        b.iter(|| {
            let permit = sem.acquire().unwrap();
            black_box(&permit);
        })
    });
    let sem = tokio::sync::Semaphore::new(8);
    g.bench_function("tokio", |b| {
        b.iter(|| {
            swait::swait(async {
                let permit = sem.acquire().await.unwrap();
                black_box(&permit);
            })
        })
    });
    g.finish();
}

fn benchmark_semaphore_contended_tokio(c: &mut Criterion) {
    use tokio::runtime::Builder;
    let mut g = c.benchmark_group("semaphore_contended_tokio");
    g.sample_size(10);

    let runtime = Builder::new_multi_thread()
        .worker_threads(core_count())
        .enable_all()
        .build()
        .unwrap();

    let sem = Arc::new(xutex::AsyncSemaphore::new(4));
    g.bench_function("xutex", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let sem = Arc::clone(&sem);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            let _permit = sem.acquire().await.unwrap();
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        })
    });

    let sem = Arc::new(tokio::sync::Semaphore::new(4));
    g.bench_function("tokio", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut handles = vec![];
                for _ in 0..THREAD_COUNT {
                    let sem = Arc::clone(&sem);
                    handles.push(tokio::spawn(async move {
                        for _ in 0..100 {
                            let _permit = sem.acquire().await.unwrap();
                        }
                    }));
                }
                for handle in handles {
                    handle.await.unwrap();
                }
            });
        })
    });
    g.finish();
}

// ---------------------------------------------------------------------------
// Notify benchmark
// ---------------------------------------------------------------------------

fn benchmark_notify_permit_roundtrip(c: &mut Criterion) {
    let mut g = c.benchmark_group("notify_permit_roundtrip");
    let notify = xutex::AsyncNotify::new();
    g.bench_function("xutex", |b| {
        b.iter(|| {
            notify.notify_one();
            swait::swait(notify.notified());
        })
    });
    let notify = tokio::sync::Notify::new();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            notify.notify_one();
            swait::swait(notify.notified());
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    benchmark_std_mutex_uncontended,
    benchmark_parkinglot_mutex_uncontended,
    benchmark_xutex_uncontended,
    benchmark_tokionative_uncontended,
    benchmark_xutex_uncontended_async,
    benchmark_std_mutex_contended,
    benchmark_parkinglot_mutex_contended,
    benchmark_xutex_contended,
    benchmark_tokionative_contended_tokio,
    benchmark_tokionative_contended_tokio_current_thread,
    benchmark_xutex_contended_tokio,
    benchmark_xutex_contended_tokio_current_thread,
    benchmark_xutex_contended_monoio,
    benchmark_xutex_contended_swait,
    benchmark_std_mutex_try_lock,
    benchmark_xutex_try_lock,
    benchmark_rwlock_read_uncontended,
    benchmark_rwlock_write_uncontended,
    benchmark_rwlock_contended_tokio,
    benchmark_rwlock_contended_sync,
    benchmark_semaphore_uncontended,
    benchmark_semaphore_contended_tokio,
    benchmark_notify_permit_roundtrip
);
criterion_main!(benches);
