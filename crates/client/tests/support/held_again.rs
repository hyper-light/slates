//! Holding an instance's rendezvous again in a client test, after the test let its earlier hold go (the TSan lane's
//! red of 2026-11-27, diag CI 2, run 45986066740: `a_restarted_daemon_adopts_its_anchors_sealing_root_and_a_fresh_anchor_mints_another`
//! refused `OsRefused { call: "bind", code: Some(98) }` at its fresh anchor's hold).
//!
//! Every child a test of the binary spawns holds a copy of each descriptor open at its fork until its exec closes the
//! close-on-exec ones, and a Linux socket keeps its abstract address until its last copy closes. So a rendezvous a
//! test has just dropped stays bound while another test's child is between the two, and a second hold of it at once is
//! refused as in use. TSan's slower fork-to-exec widens that window. The rendezvous tests met the same race and wait it
//! out the same way (`once_released` in `crates/ipc/tests/rendezvous.rs`;
//! `docs/bugs/2026-10-26-a-dropped-rendezvous-stayed-bound-in-another-tests-spawn.md`). A first hold of an instance
//! needs no wait: nothing held it before. Included by each client test binary that holds an instance again (`#[path]`),
//! so the binaries share one wait.

use std::time::{Duration, Instant};

use slates_ipc::{CLAIM_WAIT_NS, HeldRendezvous, IpcError};

/// Shape: how long a test that has let an instance's rendezvous go waits to hold it again: one claim wait, far past any
/// fork-to-exec (each refusal measured gone at the first retry, 14 to 36 µs later, on adacachy at load 44 to 80; the
/// bug record above), and it still fails a hold that never ends.
const RELEASE_WAIT: Duration = Duration::from_nanos(CLAIM_WAIT_NS);

/// Holds `instance`'s rendezvous, retried while it is refused as held ([`refused_as_held`]) within [`RELEASE_WAIT`],
/// for the reason in the module doc. Prints how many refusals it waited out, so a log shows when another test's spawn
/// held the address.
pub(crate) fn hold_once_released(instance: &str) -> HeldRendezvous {
  let deadline = Instant::now() + RELEASE_WAIT;
  let mut refusals = 0u32;
  loop {
    match HeldRendezvous::hold(instance) {
      Err(refusal) if refused_as_held(&refusal) && Instant::now() < deadline => {
        refusals = refusals.saturating_add(1);
        std::thread::yield_now();
      }
      Ok(held) => {
        if refusals > 0 {
          eprintln!("hold_once_released: {instance} held after {refusals} refusals as held");
        }
        return held;
      }
      Err(refusal) => {
        panic!(
          "{instance}'s rendezvous could not be held again: {refusal:?}, after {refusals} refusals as held"
        )
      }
    }
  }
}

/// Whether `refusal` is what a live owner's rendezvous answers a second owner: the owner's token (Windows, macOS).
#[cfg(any(windows, target_os = "macos"))]
fn refused_as_held(refusal: &IpcError) -> bool {
  matches!(refusal, IpcError::InstanceHeld { .. })
}

/// The paired arm: the abstract address in use (Linux).
#[cfg(target_os = "linux")]
fn refused_as_held(refusal: &IpcError) -> bool {
  matches!(
    refusal,
    IpcError::OsRefused { call: "bind", code: Some(code) }
      if std::io::Error::from_raw_os_error(*code).kind() == std::io::ErrorKind::AddrInUse
  )
}
