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
    benchmark_xutex_try_lock
);
criterion_main!(benches);
