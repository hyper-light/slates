//! `slates-rt` — the runtime: a thread-per-core executor with arena task slots and `Copy` waker
//! encodings, an intrusive run queue, a hierarchical timing wheel, per-shard inbound rings with
//! cross-shard wake kicks, the OS drivers behind one completion seam, and a deterministic
//! simulation driver (design §4.3, D-7, D-9).
//!
//! The doctrine, in one paragraph. A task belongs to one shard for its whole life; nothing is
//! work-stolen and nothing is reference counted. A `Waker` is a `RawWaker` whose data pointer is
//! the packed `shard:16 | slot:24 | generation:24` word of the task's handle, so `clone` and
//! `drop` are no-ops and `wake` from the owning shard pushes the slot onto the local run queue,
//! `wake` from another shard pushes the word onto that pair's single-producer ring and kicks the
//! target's driver, and `wake` from a foreign thread goes through the target's multi-producer ring
//! (embassy-executor's model on std [C: embassy-executor src/raw; B: `RawWakerVTable` docs]). A
//! stale generation is ignored. Timers live in a hierarchical timing wheel [A: Varghese & Lauck,
//! SOSP'87] whose tick is derived from the measured wake cost. Every operation is cancel-safe by
//! construction: resources live in arenas keyed by handle, so dropping a future releases nothing
//! it did not own; a parent's completion cancels and joins its children (hecate's task-lifecycle
//! law). The drivers (io_uring or epoll, kqueue, IOCP) share one seam: block until a kick, a
//! completion or a deadline; the simulation driver replaces time and the kick with seeded,
//! single-threaded stand-ins so a whole cluster runs deterministically in one process (D-20).
//!
//! `LocalWaker` is still a nightly-only API on Rust 1.98 (`local_waker`, #118959), so the
//! executor uses `Waker` with a vtable that is thread-safe by construction; the `Copy` encoding
//! costs nothing to clone, which is what `LocalWaker` would have saved (Phase 0 task 6).
//!
//! Modules: [`error`], [`msg`], [`waker`], [`registry`], [`task`], [`queue`], [`timer`],
//! [`driver`], [`sim`], [`shard`], [`runtime`], [`futures`], and the OS drivers.

pub mod driver;
pub mod error;
pub mod futures;
pub mod msg;
pub mod queue;
pub mod registry;
pub mod runtime;
pub mod shard;
pub mod sim;
pub mod task;
pub mod timer;
pub mod waker;

#[cfg(target_os = "linux")]
pub mod epoll;
#[cfg(target_os = "windows")]
pub mod iocp;
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub mod kqueue;
#[cfg(target_os = "linux")]
pub mod uring;

pub use driver::{Driver, DriverKind};
pub use error::RtError;
pub use runtime::{Runtime, RuntimeConfig};
pub use shard::{ShardId, TaskId};
pub use sim::SimRuntime;
pub use task::Outcome;
