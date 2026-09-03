# xutex-pool

Shared global block pool for [`xutex`](https://crates.io/crates/xutex).

`xutex` primitives allocate a small wait-queue structure the first time a lock
is contended and recycle it through a process-wide pool. This crate hosts that
queue structure and its pool so they can be shared across *every* version of
`xutex` in a dependency graph: all `xutex` versions depend on
`xutex-pool = "1"`, cargo unifies every `1.x` requirement into a single copy
of this crate, and the final binary ends up with exactly one pool — even when
different dependencies pull in different, semver-incompatible versions of
`xutex`. The queue is generic over the waiter node type, so each `xutex`
version keeps its own private node layout while sharing the queue algorithm
and the pooled allocations.

## Semver contract

**This crate must never have a semver-breaking release.** A `2.0.0` would let
two copies of the pool coexist again, defeating its purpose.

- The `QueueStructure` layout (two pointer-sized words, `#[repr(C)]`) and the
  queue algorithm are frozen forever.
- All public API changes must be additive (minor releases).
- Internal dependencies are private and may change freely.

You should not depend on this crate directly; it is an implementation detail
of `xutex`.

## License

MIT
