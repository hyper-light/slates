//! Handing a test's held sockets to the one slates process it spawns, as the anchor hands its daemon what it holds
//! (A-931, `hand_on_in_child` in `crates/anchor/src/supervise.rs`): each descriptor stays close-on-exec in the test
//! process and is cleared in that child alone, between its fork and its exec.
//!
//! Until 2026-11-27 these suites made an inheritable duplicate of each socket in the test process before the spawn.
//! Every other child any test of the binary started meanwhile kept that duplicate, and the port with it, for its whole
//! life (the same defect A-931 removed from the anchor:
//! `docs/bugs/2026-11-27-an-anchors-held-sockets-reached-every-child-of-their-holders-process.md`). A close-on-exec
//! descriptor is still copied into a child for the moment between its fork and its exec; that window is all that
//! remains.

use std::os::fd::RawFd;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

/// A spawn that hands on held descriptors to its child alone.
pub(crate) trait HandOn {
  /// Clears close-on-exec on each of `numbers` in the child this command spawns, and in no other process: each is a
  /// descriptor this process holds open, close-on-exec, until the spawn returns.
  fn handing_on(&mut self, numbers: &[RawFd]) -> &mut Self;
}

impl HandOn for Command {
  fn handing_on(&mut self, numbers: &[RawFd]) -> &mut Self {
    let numbers = numbers.to_vec();
    // SAFETY: the closure runs in the child between its fork and its exec and makes one `fcntl(F_SETFD)` per number in
    // `numbers`, which was copied in before the fork: no allocation, no lock and no other state, as `pre_exec` asks.
    // Each number is open in the child, whose descriptor table the fork copied from this process, which holds each open
    // until the spawn returns; the flag cleared is the child's own, so this process's stay close-on-exec.
    unsafe {
      self.pre_exec(move || {
        for &number in &numbers {
          rustix::io::fcntl_setfd(
            std::os::fd::BorrowedFd::borrow_raw(number),
            rustix::io::FdFlags::empty(),
          )?;
        }
        Ok(())
      });
    }
    self
  }
}
