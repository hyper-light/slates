//! `slates-mem` — memory: pre-sized, pre-faulted, locked arenas; per-shard slabs with
//! generational handles; buddy allocation over locked regions for chunks; segmented arrays;
//! message-passing frees; the RAM-only policy with honest degradation (design §4.2, D-8, D-12).
//!
//! The doctrine, in one paragraph. Every slab and arena belongs to exactly one shard, so nothing
//! here takes a lock and nothing here is reference counted [A: Roghanchi SOSP'17; A: David
//! SOSP'13]. A `Handle<T>` is an index plus a generation: freeing a slot bumps its generation, so
//! a stale handle is refused, never dereferenced [C: hecate's handle doctrine]. Storage never
//! moves: slots live in segmented arrays that grow by appending a segment [A: Brodnik et al.,
//! "Resizable arrays in optimal time and space", 1999], so an address handed out stays valid
//! until its slot is freed. Chunk bytes come from a buddy allocator over regions that were mapped,
//! pre-faulted and locked at start [A: Knowlton CACM'65; A: Bonwick USENIX'94]; a hot-path
//! allocation never reaches the system allocator (AC-0.4). A free from another shard is a message
//! to the owner, snmalloc's model [A: Liétar et al., ISMM'19], carried on the same SPSC ring the
//! runtime uses for wakes. Locking follows the OS's rules and reports what it got: `mlock` within
//! `RLIMIT_MEMLOCK` or the macOS wire limit, `VirtualLock` within the working set; a refusal
//! degrades to unlocked bytes that are counted, never hidden [B: mlock(2); B: Microsoft Learn].
//!
//! Every size in this crate is derived from the machine profile or from a measured rate; the
//! formulas live next to their values as `Derived` (§4.2, "Derived constants").
//!
//! Modules: [`handle`], [`segmented`], [`slab`], [`buddy`], [`region`], [`lock`], [`prefault`],
//! [`arena`], [`ring`], [`mpsc`], [`budget`], [`error`].

pub mod arena;
pub mod buddy;
pub mod budget;
pub mod error;
pub mod handle;
pub mod lock;
pub mod mpsc;
pub mod prefault;
pub mod region;
pub mod ring;
pub mod segmented;
pub mod shared;
pub mod slab;

pub use arena::{ChunkArena, Extent};
pub use error::MemError;
pub use handle::{Encoded, Handle};
pub use mpsc::MpscRing;
pub use region::Region;
pub use ring::SpscRing;
pub use segmented::Segmented;
pub use shared::{Handoff, SharedObject};
pub use slab::Slab;
