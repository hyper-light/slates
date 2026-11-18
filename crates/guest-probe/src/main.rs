//! A Linux VM guest's `/init` for the mount-route tests: see the library (`src/lib.rs`) for what it is for, and
//! `src/guest.rs` for what it does.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

#[cfg(target_os = "linux")]
mod guest;

/// The guest's `/init` only as the kernel's first process. Run any other way, as the copy a guest's test runs from a
/// mount to learn whether the mount honoured its set-id bits (A-94), it says its ids and ends: it never takes a
/// console, mounts anything or ends a machine it was not started to be the first process of.
#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
  if rustix::process::getpid().is_init() {
    guest::run()
  } else {
    guest::say_identity()
  }
}

/// Off Linux there is no guest to be: the binary says so and exits.
#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("slates-guest-probe runs only as a Linux VM guest's /init");
}
