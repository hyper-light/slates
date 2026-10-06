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
//! [A: Baumann SOSP'09]): the client spins on the completion slot for its wake estimate (seeded
//! with the daemon's published `spin_ns`, refined from the parks a reply ended by the daemon's
//! reply stamp), sets the parked flag, re-checks the slot to close the race, and waits on the
//! wake word; the daemon, after writing a reply, bumps the word and wakes only when the flag is
//! set.
//!
//! Modules: [`protocol`] (the bodies and their framing), [`slot`] (the slot and the ring), [`region`] (the client region's layout over a
//! shared object), [`wake`] (the wake word per OS), [`rendezvous`] (per OS), [`endpoint`]
//! (the daemon's and the client's ends), [`delivery`] (the harness delivery channel of a consumer's
//! capability, §4.13: an inherited descriptor), [`error`].

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

/// The client-side completion bridge that gives an async SDK event loop a descriptor it can adopt as
/// a stream (§4.7, D-19). macOS and Windows pass no completion fd (Mach and named sockets are refused,
/// D-10), so a thread makes a client-local descriptor readable when an armed reply lands — a self-pipe
/// on macOS, a loopback socket on Windows. Linux passes the rendezvous eventfd, which Python's asyncio
/// polls directly with no bridge; the Linux arm exists only for an SDK that adopts the fd (Node's
/// `net.Socket` refuses an eventfd), converting the eventfd's readability to a pollable self-pipe.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub mod completion;
pub mod delivery;
pub mod doorbell;
pub mod endpoint;
pub mod error;
pub mod exit_watch;
pub mod park;
pub mod protocol;
pub mod region;
pub mod rendezvous;

pub mod slot;
pub mod status;
pub mod wake;

pub use delivery::{Capability, Delivered, Delivery, DeliveryFault};
pub use endpoint::{ClientEnd, DaemonEnd, Reply, Request};
pub use error::IpcError;
pub use region::{ClientRegion, RegionGeometry};
pub use rendezvous::{
  Accepted, CLAIM_WAIT_NS, Claim, Connected, Doorbell, Listener, Liveness, Prepared,
  begin_connect_as, connect, connect_as, instance_from_env,
};
pub use slot::{PAYLOAD_BYTES, Slot, SlotKind};

/// Serializes the tests that hold a pipe they expect to close against the tests that spawn a child process. On Apple
/// a pipe is made close-on-exec by an `fcntl` after `pipe` (no `pipe2`), and a child spawned by a parallel test inside
/// that window inherits the read end for its whole life: `a_whole_record_takes_once_and_the_descriptor_is_closed`
/// then wrote into a pipe that still had a reader, 2 of 80 runs beside `exit_watch`'s `/bin/sleep 30`, 0 of 80 without
/// it (2026-10-06). The same window binds a harness, as `delivery`'s `pipe_close_on_exec` says.
#[cfg(test)]
#[allow(clippy::disallowed_types)]
pub(crate) fn descriptor_test_gate() -> std::sync::MutexGuard<'static, ()> {
  // structural: allow — D-8 exception 3: a test harness; the gate's owners are the descriptor and spawning tests.
  static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
  GATE
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}
