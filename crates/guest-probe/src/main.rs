//! A Linux VM guest's `/init` for the mount-route tests: see the library (`src/lib.rs`) for what it is for, and
//! `src/guest.rs` for what it does.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

#[cfg(target_os = "linux")]
mod guest;

#[cfg(target_os = "linux")]
fn main() {
  guest::run()
}

/// Off Linux there is no guest to be: the binary says so and exits.
#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("slates-guest-probe runs only as a Linux VM guest's /init");
}
