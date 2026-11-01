//! A file mapped read-only and shared through a FUSE mount, as a program that only reads it holds it, read as memory
//! alone (AC-3.3's reader: `tests/fuse_told.rs`, and the Shards stream's in `tests/shards.rs`), and whether FUSE runs
//! on this host at all.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::fd::AsFd;
use std::process::Command;

/// Whether `name` is a command on the `PATH`.
pub(crate) fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Whether the FUSE tests run here; a skip is printed, never a failure.
pub(crate) fn fuse_available() -> bool {
  let available = command_available("fusermount3") && std::path::Path::new("/dev/fuse").exists();
  if !available {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
  }
  available
}

/// A file mapped read-only through a mount, as a program that only reads it holds it.
pub(crate) struct ReadOnlyMap {
  map: *mut u8,
  length: usize,
  _file: std::fs::File,
}

impl ReadOnlyMap {
  /// Maps `length` bytes of `path`, shared and read-only, and reads it once so its pages are in the mount's cache.
  #[allow(clippy::disallowed_methods)] // the test's own open through the slates mount: RAM, not disk
  pub(crate) fn of(path: &str, length: usize) -> ReadOnlyMap {
    let file = std::fs::File::open(path).unwrap();
    // SAFETY: a fresh shared, read-only map of `length` bytes of a file this value holds open, at an address the kernel
    // chooses; nothing else maps it, and `Drop` unmaps it before the file closes.
    let map = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        length,
        rustix::mm::ProtFlags::READ,
        rustix::mm::MapFlags::SHARED,
        file.as_fd(),
        0,
      )
    }
    .unwrap()
    .cast::<u8>();
    let mapped = ReadOnlyMap {
      map,
      length,
      _file: file,
    };
    let _ = mapped.seen();
    mapped
  }

  /// The map's bytes as they read now: a copy taken a byte at a time with volatile loads, since the kernel replaces the
  /// map's pages when the file changes, and a slice held over memory that changes under it is not sound.
  pub(crate) fn seen(&self) -> Vec<u8> {
    (0..self.length)
      .map(|offset| {
        // SAFETY: `map` is `length` readable bytes, mapped in `of` and unmapped only in `Drop`, and `offset` is below
        // `length`; a volatile load of one byte holds no reference across the kernel's page replacement.
        unsafe { std::ptr::read_volatile(self.map.add(offset)) }
      })
      .collect()
  }

  /// Whether every byte reads `fill` now.
  pub(crate) fn shows(&self, fill: u8) -> bool {
    self.seen().iter().all(|byte| *byte == fill)
  }
}

impl Drop for ReadOnlyMap {
  fn drop(&mut self) {
    // SAFETY: unmaps exactly the map made in `of`, which nothing reads after this.
    let _ = unsafe { rustix::mm::munmap(self.map.cast(), self.length) };
  }
}
