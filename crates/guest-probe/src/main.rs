//! A Linux VM guest's `/init` for the mount-route tests: see the library (`src/lib.rs`) for what it is for, and
//! `src/guest.rs` for what it does.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

#[cfg(target_os = "linux")]
mod guest;

#[cfg(target_os = "linux")]
fn main() {
  // Run by a guest's `Escalate` as a planted setuid program (A-94): it says the effective uid it runs with, and is no
  // `/init`.
  if std::env::args().nth(1).as_deref() == Some(slates_guest_probe::protocol::REPORT_EUID) {
    use std::io::Write;
    let said = writeln!(std::io::stdout(), "{}", rustix::process::geteuid().as_raw());
    // A refused write exits non-zero; the guest that ran it reports the empty answer as a refusal.
    std::process::exit(i32::from(said.is_err()));
  }
  guest::run()
}

/// Off Linux there is no guest to be: the binary says so and exits.
#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("slates-guest-probe runs only as a Linux VM guest's /init");
}
