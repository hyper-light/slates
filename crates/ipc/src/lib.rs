//! `slates-ipc` — the local IPC of the provisioning fast path (design §4.7, D-10; Phase 2
//! task 3): one shared-memory region per client holding a command ring and a completion ring
//! of fixed-layout 64-byte slots, a wake word, a parked flag and the daemon's published spin
//! window; the rendezvous per OS that hands the region over without creating a filesystem
//! entry; peer authentication at rendezvous; the completion fd for SDK event loops.
//!
//! The ring is a single-producer, single-consumer ring of slots each carrying its own
//! sequence word (the LMAX/Vyukov shape [C: LMAX Disruptor; C: Vyukov bounded MPMC]): the
//! producer writes the payload and then releases the slot's sequence, the consumer acquires
//! the sequence, reads, and releases the slot back with the sequence advanced by the ring's
//! length, so a partially written slot is never seen and no shared head or tail word sits on
//! the hot path. The wake strategy is spin-then-park with the spin equal to the measured wake
//! cost (Karlin's 2-competitive rule [A: Karlin et al. 1990]; Barrelfish's P = C
//! [A: Baumann SOSP'09]): the client spins on the completion slot for `spin_ns`, sets the
//! parked flag, re-checks the slot to close the race, and waits on the wake word; the daemon,
//! after writing a reply, bumps the word and wakes only when the flag is set.
//!
//! Modules: [`protocol`] (the bodies and their framing), [`slot`] (the slot and the ring), [`region`] (the client region's layout over a
//! shared object), [`wake`] (the wake word per OS), [`rendezvous`] (per OS), [`endpoint`]
//! (the daemon's and the client's ends), [`error`].

/// The client-side completion bridge for platforms whose rendezvous passes no completion fd (macOS
/// and Windows): a thread that makes a client-local descriptor readable when an armed reply lands,
/// for an async SDK event loop (§4.7, D-19) — a self-pipe on macOS, a loopback socket on Windows.
/// Linux uses the rendezvous eventfd instead.
#[cfg(any(target_os = "macos", windows))]
pub mod completion;
pub mod endpoint;
pub mod error;
pub mod protocol;
pub mod region;
pub mod rendezvous;

pub mod slot;
pub mod wake;

pub use endpoint::{ClientEnd, DaemonEnd, Reply, Request};
pub use error::IpcError;
pub use region::{ClientRegion, RegionGeometry};
pub use rendezvous::{
  Accepted, Connected, Doorbell, Listener, Liveness, Prepared, connect, connect_as,
  instance_from_env,
};
pub use slot::{PAYLOAD_BYTES, Slot, SlotKind};
