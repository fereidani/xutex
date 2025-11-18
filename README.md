# Xutex — High‑Performance Hybrid Mutex

[![Crates.io](https://img.shields.io/crates/v/xutex.svg)](https://crates.io/crates/xutex)
[![Documentation](https://docs.rs/xutex/badge.svg)](https://docs.rs/xutex)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Xutex** is a high-performance mutex that seamlessly bridges synchronous and asynchronous Rust code with a single type and unified internal representation. Designed for **extremely low-latency lock acquisition** under minimal contention, it achieves near-zero overhead on the fast path while remaining runtime-agnostic.

## Key Features

- **⚡ Blazing-fast async performance**: Up to 50× faster than standard sync mutexes in single-threaded async runtimes, and 3–5× faster in multi-threaded async runtime.
- **🔄 Hybrid API**: Use the same lock in both sync and async contexts
- **⚡ 8-byte lock state**: Single `AtomicPtr` on 64-bit platforms (guarded data stored separately)
- **🚀 Zero-allocation fast path**: Lock acquisition requires no heap allocation when uncontended
- **♻️ Smart allocation reuse**: Object pooling minimizes allocations under contention
- **🎯 Runtime-agnostic**: Works with Tokio, async-std, monoio, or any executor using `std::task::Waker`
- **🔒 Lock-free fast path**: Single CAS operation for uncontended acquisition
- **📦 Minimal footprint**: Compact state representation with lazy queue allocation

## Installation

```toml
[dependencies]
xutex = "0.1"
```

Or via cargo:

```sh
cargo add xutex
```

## Quick Start

### Synchronous Usage

```rust
use xutex::Mutex;

let mutex = Mutex::new(0);
{
    let mut guard = mutex.lock();
    *guard += 1;
} // automatically unlocked on drop
assert_eq!(*mutex.lock(), 1);
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
use xutex::{Mutex, AsyncMutex};

// Sync → Async
let sync_mutex = Mutex::new(42);
let async_ref: &AsyncMutex<_> = sync_mutex.as_async();
let guard = async_ref.lock().await;

// Async → Sync
let async_mutex = AsyncMutex::new(5);
let sync_ref: &Mutex<_> = async_mutex.as_sync();
let guard = sync_ref.lock();

// Block on async mutex from sync context
let guard = async_mutex.lock_sync();
```

## Performance Characteristics

### Why It's Fast

1. **Atomic state machine**: Three states encoded in a single pointer:

   - `UNLOCKED` (null): Lock is free
   - `LOCKED` (sentinel): Lock held, no waiters
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
- **High contention**: 2-5× faster than `tokio::sync::Mutex` in async contexts
- **Sync contexts**: Comparable to `std::sync::Mutex`, slight overhead due to queue pointer check overhead.

## Design Deep Dive

### Architecture

```
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

| Method         | Description                                   |
| -------------- | --------------------------------------------- |
| `new(data: T)` | Create a new asynchronous mutex               |
| `lock()`       | Acquire the lock (returns `Future`)           |
| `try_lock()`   | Attempt non-blocking acquisition              |
| `lock_sync()`  | Acquire synchronously (blocks current thread) |
| `as_sync()`    | View as `&Mutex<T>`                           |

### `MutexGuard<'a, T>`

Implements `Deref<Target = T>` and `DerefMut` for transparent access to the protected data. Automatically releases the lock on drop.

## Use Cases

### ✅ Ideal For

- High-frequency, low-contention async locks
- Hybrid applications mixing sync and async code
- Performance-critical sections with short critical regions
- Runtime-agnostic async libraries
- Situations requiring zero-allocation fast paths

### ⚠️ Not Ideal For

- **Predominantly synchronous workloads**: In pure sync environments without async interaction, `std::sync::Mutex` may offer slightly better performance due to lower abstraction overhead
- **Read-heavy workloads**: If your use case involves frequent reads with infrequent writes, consider using `RwLock` implementations (e.g., `std::sync::RwLock` or `tokio::sync::RwLock`) that allow multiple concurrent readers
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

# Benchmarks
cargo bench
```

## TODO

- [ ] Implement `RwLock` variant with shared/exclusive locking

## Contributing

Contributions are welcome! Please:

1. Run `cargo +nightly fmt` and `cargo clippy` before submitting
2. Add tests for new functionality
3. Update documentation as needed
4. Verify `cargo miri test` passes

## License

Licensed under the [MIT License](LICENSE).

---

**Author**: Khashayar Fereidani  
**Repository**: [github.com/fereidani/xutex](https://github.com/fereidani/xutex)
