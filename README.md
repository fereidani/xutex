# Xutex — High‑Performance Hybrid Synchronization Primitives

[![Crates.io](https://img.shields.io/crates/v/xutex.svg)](https://crates.io/crates/xutex)
[![Documentation](https://docs.rs/xutex/badge.svg)](https://docs.rs/xutex)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Xutex** is a family of high-performance synchronization primitives — `Mutex`, `RwLock`, `Semaphore`, `Notify`, `Barrier` and `OnceCell` — that seamlessly bridge synchronous and asynchronous Rust code with paired types sharing a unified internal representation. Designed for **extremely low-latency acquisition** under minimal contention, they achieve near-zero overhead on the fast path while remaining runtime-agnostic.

## Key Features

- **⚡ Blazing-fast async performance**: Up to 50× faster than standard sync mutexes in single-threaded async runtimes, and 2–6× faster than `tokio::sync` primitives under contention on multi-threaded runtimes.
- **🧰 Complete primitive family**: `Mutex`, `RwLock`, `Semaphore`, `Notify`, `Barrier` and `OnceCell` — every one available in a sync and an async flavor sharing the same algorithm
- **🔄 Hybrid API**: Every primitive comes as a layout-identical `X`/`AsyncX` pair with zero-cost conversions (`as_async()`, `to_sync()`, `Arc` casts), so the same object serves blocking threads and async tasks
- **⚡ 8-byte lock state**: Single `AtomicPtr` mutex state on 64-bit platforms (guarded data stored separately)
- **🚀 Zero-allocation fast path**: Acquisition requires no heap allocation when uncontended; waiters are stack-allocated intrusive nodes, so even contended waits allocate nothing per waiter
- **♻️ Smart allocation reuse**: The only heap object — the wait queue itself — is pooled and exists only while waiters are parked
- **🎯 Runtime-agnostic**: Works with Tokio, async-std, monoio, or any executor using `std::task::Waker`
- **🔒 Lock-free fast path**: Single CAS operation for uncontended acquisition
- **⚖️ Fair by construction**: Semaphore and RwLock are strict-FIFO (write-preferring RwLock, like tokio); no starvation
- **🛑 Cancellation-safe futures**: Dropping any pending acquisition future cleanly unregisters it (and returns already-granted permits); barrier waits even withdraw their arrival
- **🧪 Model-checked & UB-checked**: Exhaustively tested with [loom](https://github.com/tokio-rs/loom) and [miri](https://github.com/rust-lang/miri)
- **📦 Minimal footprint**: Compact state representation with lazy queue allocation
- **🛡️ No-std compatible**: The async primitives are fully `no_std` (only `core` + `alloc`)

## Installation

```toml
[dependencies]
xutex = "0.2"
```

Or via cargo:

```sh
# with std
cargo add xutex
# for no-std environments
cargo add xutex --no-default-features
```

## Quick Start

### Synchronous Usage

```rust
#[cfg(feature = "std")]
fn example() {
  use xutex::Mutex;

  let mutex = Mutex::new(0);
  {
    let mut guard = mutex.lock();
    *guard += 1;
  } // automatically unlocked on drop
  assert_eq!(*mutex.lock(), 1);
}
```

### Asynchronous Usage

```rust
use xutex::AsyncMutex;

async fn increment(mutex: &AsyncMutex<i32>) {
    let mut guard = mutex.lock().await;
    *guard += 1;
}
```

### Hybrid Usage

Convert seamlessly between sync and async:

```rust
#[cfg(feature = "std")]
use xutex::Mutex;
#[cfg(feature = "std")]
async fn example(mutex: &Mutex<i32>) {
  let async_ref = mutex.as_async();
  let guard = async_ref.lock().await;
}
```

```rust
#[cfg(feature = "std")]
fn example(){
  use xutex::{Mutex, AsyncMutex};
  // Async → Sync
  let async_mutex = AsyncMutex::new(5);
  let sync_ref: &Mutex<_> = async_mutex.as_sync();
  let guard = sync_ref.lock();
  drop(guard);
  // Block on async mutex from sync context
  let guard = async_mutex.lock_sync();
}
```

### The Whole Family

Every primitive follows the same split: a synchronous type (`RwLock`, `Semaphore`, `Notify`, `Barrier`, `OnceCell`) and a layout-identical asynchronous twin (`AsyncRwLock`, `AsyncSemaphore`, `AsyncNotify`, `AsyncBarrier`, `AsyncOnceCell`), freely convertible in both directions:

```rust
#[cfg(feature = "std")]
fn example() {
  use xutex::{RwLock, Semaphore, Notify, Barrier, OnceCell};

  // Reader-writer lock: many readers or one writer, write-preferring FIFO.
  let lock = RwLock::new(0);
  {
      let r1 = lock.read();
      let r2 = lock.read();
      assert_eq!(*r1 + *r2, 0);
  }
  *lock.write() += 1;

  // Counting semaphore with atomic multi-permit acquisition.
  let sem = Semaphore::new(4);
  let permit = sem.acquire_many(2).unwrap();
  assert_eq!(sem.available_permits(), 2);
  drop(permit);

  // Notification: store-one-permit or wake-all.
  let notify = Notify::new();
  notify.notify_one();
  notify.wait(); // consumes the stored permit without blocking

  // Reusable barrier.
  let barrier = Barrier::new(1);
  assert!(barrier.wait().is_leader());

  // Initialize-once cell with waiter takeover on failure.
  let cell = OnceCell::new();
  assert_eq!(*cell.get_or_init(|| 42), 42);
}
```

```rust
use xutex::{AsyncRwLock, AsyncSemaphore};

async fn example(lock: &AsyncRwLock<i32>, sem: &AsyncSemaphore) {
    let value = *lock.read().await;
    let _permit = sem.acquire().await.unwrap();
    let mut w = lock.write().await;
    *w += value;
    // Dropping any pending future (e.g. inside tokio::select!) is safe:
    // it unregisters from the wait queue and returns any granted permits.
}
```

## Performance Characteristics

### Why It's Fast

1. **Atomic state machine**: Three states encoded in a single pointer:

   - `UNLOCKED` (null): Lock is free
   - `LOCKED` (sentinel): Lock held, no waiters
   - `UPDATING`: Queue initialization in progress
   - `QUEUE_PTR`: Lock held with waiting tasks/threads

2. **Lock-free fast path**: Uncontended acquisition uses a single `compare_exchange`

3. **Lazy queue allocation**: Wait queue created only when contention occurs

4. **Pointer tagging**: LSB tagging prevents race conditions during queue modifications

5. **Stack-allocated waiters**: `Signal` nodes live on the stack, forming an intrusive linked list

6. **Optimized memory ordering**: Careful use of `Acquire`/`Release` semantics

7. **Adaptive backoff**: Exponential backoff reduces cache thrashing under contention

8. **Minimal heap allocation**: At most one allocation per contended lock via pooled queue reuse, additional waiters require zero allocations

### Benchmarks

Run benchmarks on your machine:

```sh
cargo bench
```

**Expected Performance** (varies by hardware):

- **Uncontended**: ~1-3ns per lock/unlock cycle (single CAS operation)
- **High contention**: 2-3× faster than `tokio::sync::Mutex` in async contexts
- **Sync contexts**: Performance comparable to `std::sync::Mutex` with minimal overhead from queue pointer checks under high contention; matches `parking_lot` performance in low-contention scenarios

Sample results for the newer primitives (Linux x86-64, 16 logical cores; `cargo bench`):

| Benchmark                                            |   xutex |     tokio |    std | parking_lot |
| ---------------------------------------------------- | ------: | --------: | -----: | ----------: |
| `RwLock` read, uncontended                           |  6.4 ns |   28.9 ns | 5.7 ns |      6.3 ns |
| `RwLock` write, uncontended                          |  6.6 ns |         — | 5.7 ns |      5.6 ns |
| `RwLock` contended, tokio rt (64 tasks, 7:1 r/w)     |  449 µs |    872 µs |      — |           — |
| `Semaphore` acquire/release, uncontended             |  6.3 ns |   28.3 ns |      — |           — |
| `Semaphore` contended, tokio rt (64 tasks, 4 permits) |  277 µs |   1.67 ms |      — |           — |
| `Notify` permit round-trip                           |  9.3 ns |   12.9 ns |      — |           — |

## Design Deep Dive

### Architecture

```text
┌─────────────────────────────────────────────┐
│  Mutex<T> / AsyncMutex<T>                   │
│  ┌───────────────────────────────────────┐  │
│  │ MutexInternal<T>                      │  │
│  │  • queue: AtomicPtr<QueueStructure>   │  │
│  │  • inner: UnsafeCell<T>               │  │
│  └───────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
         │
         ├─ UNLOCKED (null) ──────────────► Lock available
         │
         ├─ LOCKED (sentinel) ─────────────► Lock held, no waiters
         │
         └─ Queue pointer ─────────────────► Lock held, waiters queued
                   │
                   ▼
            ┌─────────────────┐
            │  SignalQueue    │
            │  (linked list)  │
            └─────────────────┘
                   │
                   ▼
            ┌─────────────────┐     ┌─────────────────┐
            │    Signal       │────►│    Signal       │────► ...
            │  • waker        │     │  • waker        │
            │  • value        │     │  • value        │
            └─────────────────┘     └─────────────────┘
```

### Signal States

Each waiter tracks its state through atomic transitions:

1. `SIGNAL_UNINIT (0)`: Initial state
2. `SIGNAL_INIT_WAITING (1)`: Enqueued and waiting
3. `SIGNAL_SIGNALED (2)`: Lock granted
4. `SIGNAL_RETURNED (!0)`: Guard has been returned

### Thread Safety

- **Public API**: 100% safe Rust
- **Internal implementation**: Carefully controlled `unsafe` blocks for:
  - Queue manipulation (pointer tagging prevents use-after-free)
  - Guard creation (guaranteed by state machine)
  - Memory ordering (documented and audited)

## API Reference

### `Mutex<T>`

| Method           | Description                                     |
| ---------------- | ----------------------------------------------- |
| `new(data: T)`   | Create a new synchronous mutex                  |
| `lock()`         | Acquire the lock (blocks current thread)        |
| `try_lock()`     | Attempt non-blocking acquisition                |
| `lock_async()`   | Acquire asynchronously (returns `Future`)       |
| `as_async()`     | View as `&AsyncMutex<T>`                        |
| `to_async()`     | Convert to `AsyncMutex<T>`                      |
| `to_async_arc()` | Convert `Arc<Mutex<T>>` to `Arc<AsyncMutex<T>>` |

### `AsyncMutex<T>`

| Method          | Description                                     |
| --------------- | ----------------------------------------------- |
| `new(data: T)`  | Create a new asynchronous mutex                 |
| `lock()`        | Acquire the lock (returns `Future`)             |
| `try_lock()`    | Attempt non-blocking acquisition                |
| `lock_sync()`   | Acquire synchronously (blocks current thread)   |
| `as_sync()`     | View as `&Mutex<T>`                             |
| `to_sync()`     | Convert to `Mutex<T>`                           |
| `to_sync_arc()` | Convert `Arc<AsyncMutex<T>>` to `Arc<Mutex<T>>` |

### `MutexGuard<'a, T>`

Implements `Deref<Target = T>` and `DerefMut` for transparent access to the protected data. Automatically releases the lock on drop.

### `RwLock<T>` / `AsyncRwLock<T>`

| Method                             | Description                                          |
| ---------------------------------- | ---------------------------------------------------- |
| `new(data)` / `with_max_readers`   | Create (default max readers: `u32::MAX >> 3`)        |
| `read()` / `write()`               | Acquire shared/exclusive access (blocking or future) |
| `try_read()` / `try_write()`       | Non-blocking attempts                                |
| `read_async()` … / `read_sync()` … | Cross-mode acquisition                               |
| `RwLockWriteGuard::downgrade()`    | Atomically demote a write guard to a read guard      |
| `get_mut()` / `into_inner()`       | Direct access through exclusive ownership            |

Fair and write-preferring (strict FIFO, tokio-style): queued writers block later readers, so writers cannot starve.

### `Semaphore` / `AsyncSemaphore`

| Method                                | Description                                    |
| ------------------------------------- | ---------------------------------------------- |
| `new(permits)`                        | Create with an initial permit count            |
| `acquire()` / `acquire_many(n)`       | Acquire permits (blocking or future), FIFO     |
| `try_acquire()` / `try_acquire_many`  | Non-blocking attempts                          |
| `add_permits(n)` / `forget_permits`   | Grow / shrink the pool                         |
| `close()` / `is_closed()`             | Fail all current and future acquisitions       |
| `SemaphorePermit::{forget,split,merge}` | Permit manipulation                          |

`acquire_many` hands over all `n` permits atomically; a partially served waiter blocks later waiters (fairness), and cancellation returns partially assigned permits.

### `Notify` / `AsyncNotify`

`notify_one()` (wakes one waiter or stores a single permit), `notify_waiters()` (wakes all current waiters; also completes every `notified()` future created before the call), `wait()`/`notified()`. A `notify_one` wakeup consumed by a dropped future is passed on to the next waiter, matching tokio semantics.

### `Barrier` / `AsyncBarrier`

`new(n)`, `wait()` → `BarrierWaitResult::is_leader()`. Reusable across rounds. Unlike `tokio::sync::Barrier`, dropping a pending wait future *withdraws* the arrival, so cancellation cannot deadlock the barrier.

### `OnceCell<T>` / `AsyncOnceCell<T>`

`get`, `set`, `get_or_init`, `get_or_try_init`, `initialized`, `get_mut`, `take`, `into_inner`. Concurrent initializers race; losers park allocation-free. A failed, panicking, or cancelled initializer hands the role to a queued waiter (which runs its own closure), matching tokio semantics.

## Use Cases

### ✅ Ideal For

- High-frequency, low-contention async locks
- Hybrid applications mixing sync and async code
- Performance-critical sections with short critical regions
- Runtime-agnostic async libraries
- Situations requiring zero-allocation fast paths

### ⚠️ Not Ideal For

- **Predominantly synchronous workloads**: In pure sync environments without async interaction, `std::sync` primitives may offer slightly better performance due to lower abstraction overhead
- **Mutex poison state**: Cases where `std::sync::Mutex` poisoning semantics are required

## Caveats

- **8-byte claim**: Refers to lock metadata only on 64-bit platforms; guarded data `T` stored separately
- **No poisoning**: Unlike `std::sync::Mutex`, panics don't poison the lock
- **Sync overhead**: Slight performance cost vs `std::sync::Mutex` in pure-sync scenarios (~1-5%)

## Testing

Run the test suite:

```sh
# Standard tests
cargo test

# With Miri (undefined behavior detection)
cargo +nightly miri test

# With loom (exhaustive concurrency model checking)
RUSTFLAGS="--cfg loom" cargo test --release --test loom

# Benchmarks
cargo bench
```

The loom suite explores every thread interleaving (including weak-memory
reorderings) of small scenarios for each primitive: lock handoff, permit
handoff and partial `acquire_many` assignment, close/cancel races, lost-wakeup
races in `Notify`, barrier rounds and once-cell initializer races. Under
loom, the internal wait-queue spinlock and waker slot are modeled with
loom-aware locks (loom cannot prove progress through raw spin loops); the
spinlock mechanics themselves are covered by miri and the native stress
tests.

## TODO

- [x] Implement `RwLock` variant with shared/exclusive locking
- [x] `Semaphore`, `Notify`, `Barrier` and `OnceCell` primitives
- [x] Loom-based exhaustive concurrency testing
- [ ] Owned (`Arc`-based) guard variants (`OwnedSemaphorePermit`, …)
- [ ] Explore lock-free linked list implementation for improved wait queue performance

## Contributing

Contributions are welcome! Please:

1. Run `cargo +nightly fmt` and `cargo clippy` before submitting
2. Add tests for new functionality
3. Update documentation as needed
4. Verify `cargo miri test` passes
5. Note: This library is `no-std` compatible; use `core` and `alloc` instead of `std`. Ensure `cargo test` and `cargo test --no-default-features` run without warnings.

## License

Licensed under the [MIT License](LICENSE).

---

**Author**: Khashayar Fereidani  
**Repository**: [github.com/fereidani/xutex](https://github.com/fereidani/xutex)
