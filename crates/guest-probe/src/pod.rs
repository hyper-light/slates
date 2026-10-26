//! A pod's map probe for the KIND lane's export leg (`cargo xtask kind export`, step 6; G-mount step 5;
//! `docs/wip/virtiofs.md` §8): a process in a Kubernetes pod maps a file of a slates volume read-only through kubelet's
//! NFS mount and waits until another path's change shows in its map.
//!
//! Its arguments: the file, its length, the byte the file is filled with first, the byte another path fills it with,
//! and the wait and the poll in milliseconds. It maps the file `PROT_READ`, `MAP_SHARED`, checks the map shows the
//! first fill and prints `mapper: mapped`, which the lane waits for before it changes the file. Then it stats the file
//! until a stat shows the change (a new modify or change time) and says whether the map already shows it. From then on,
//! each poll it reads one byte through its descriptor and reads its map, until the map shows the new fill (exit 0) or
//! the wait passes (exit 1).
//!
//! The read is the step Linux needs. Its NFS client takes a changed file's pages out of every map only when it
//! revalidates the file's mapping (`nfs_revalidate_mapping` in `fs/nfs/inode.c`, called on a `read`, a `splice`, an
//! `mmap` or a lock, which calls `invalidate_inode_pages2`). A `stat` fetches the new change attribute and marks the
//! pages invalid, but drops none (`nfs_getattr`). The probe prints what the stat alone did, so each run records it.
//!
//! The map is read with volatile loads, since the kernel replaces a mapped page under the probe. Linux only; it is test
//! infrastructure, built into a lane-only image (`deploy/kind/pod-probe.Dockerfile`) and never shipped.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

#[cfg(target_os = "linux")]
fn main() {
  std::process::exit(linux::run(std::env::args().skip(1).collect()));
}

/// Off Linux there is no pod to be in: the binary says so and exits.
#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("slates-pod-probe runs only in a Linux pod");
}

#[cfg(target_os = "linux")]
mod linux {
  use std::os::fd::OwnedFd;
  use std::time::{Duration, Instant};

  use rustix::fs::{Mode, OFlags};
  use rustix::mm::{MapFlags, ProtFlags};

  /// Format: the exit code when the map followed the change.
  const FOLLOWED: i32 = 0;
  /// Format: the exit code when the map kept the old bytes for the whole wait.
  const KEPT_THE_OLD_BYTES: i32 = 1;
  /// Format: the exit code when the probe could not run (bad arguments, an open, a map or a first fill refused).
  const COULD_NOT_RUN: i32 = 2;

  /// What the lane asks of the probe (the module doc).
  struct Request {
    path: String,
    len: usize,
    before: u8,
    after: u8,
    wait: Duration,
    poll: Duration,
  }

  /// Runs the probe on `args`: its exit code.
  pub(super) fn run(args: Vec<String>) -> i32 {
    let request = match parse(&args) {
      Ok(request) => request,
      Err(e) => {
        println!("mapper: {e}");
        return COULD_NOT_RUN;
      }
    };
    match follow(&request) {
      Ok(code) => code,
      Err(e) => {
        println!("mapper: {e}");
        COULD_NOT_RUN
      }
    }
  }

  /// The request from `PATH LEN BEFORE AFTER WAIT_MS POLL_MS`.
  fn parse(args: &[String]) -> Result<Request, String> {
    let [path, len, before, after, wait, poll] = args else {
      return Err(format!(
        "usage: slates-pod-probe PATH LEN BEFORE AFTER WAIT_MS POLL_MS (given {} arguments)",
        args.len()
      ));
    };
    let number = |what: &str, text: &str| {
      text
        .parse::<u64>()
        .map_err(|e| format!("{what} {text:?}: {e}"))
    };
    let byte = |what: &str, text: &str| {
      text
        .parse::<u8>()
        .map_err(|e| format!("{what} {text:?}: {e}"))
    };
    Ok(Request {
      path: path.clone(),
      len: usize::try_from(number("LEN", len)?).map_err(|e| format!("LEN {len}: {e}"))?,
      before: byte("BEFORE", before)?,
      after: byte("AFTER", after)?,
      wait: Duration::from_millis(number("WAIT_MS", wait)?),
      poll: Duration::from_millis(number("POLL_MS", poll)?),
    })
  }

  /// Maps the file, waits for the change (the module doc) and returns the exit code.
  fn follow(request: &Request) -> Result<i32, String> {
    let path = request.path.as_str();
    // structural: allow — a file of the slates volume kubelet mounts into this pod; this binary never runs on a host.
    let file = rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
      .map_err(|e| format!("open {path}: {e}"))?;
    let map = ReadMap::new(file, request.len).map_err(|e| format!("mmap {path}: {e}"))?;
    if !map.holds(request.before) {
      return Err(format!(
        "the first map does not show the first fill ({} bytes)",
        request.len
      ));
    }
    let initial = times(path)?;
    println!("mapper: mapped {} bytes read-only and shared", request.len);
    let started = Instant::now();
    let mut stat_saw: Option<Duration> = None;
    while started.elapsed() < request.wait {
      if stat_saw.is_none() && times(path)? != initial {
        let saw = started.elapsed();
        stat_saw = Some(saw);
        let alone = if map.holds(request.after) {
          "the new bytes"
        } else {
          "the old bytes"
        };
        println!(
          "mapper: a stat saw the change after {:.2} s; the map after the stat alone shows {alone}",
          saw.as_secs_f64()
        );
      }
      if stat_saw.is_some() {
        let mut one = [0_u8; 1];
        rustix::io::pread(&map.file, &mut one, 0).map_err(|e| format!("pread {path}: {e}"))?;
        if map.holds(request.after) {
          println!(
            "mapper: the map followed the change after {:.2} s",
            started.elapsed().as_secs_f64()
          );
          return Ok(FOLLOWED);
        }
      }
      // A probe's own poll between stats: this process is a test's, alone in its pod, with no runtime to wait on.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(request.poll);
    }
    let saw = stat_saw.map_or_else(
      || "no stat saw the change".to_owned(),
      |saw| format!("a stat saw the change after {:.2} s", saw.as_secs_f64()),
    );
    println!(
      "mapper: the map kept the old bytes for {:.0} s ({saw})",
      request.wait.as_secs_f64()
    );
    Ok(KEPT_THE_OLD_BYTES)
  }

  /// The file's modify and change times, seconds and nanoseconds, as a stat through the mount answers them (widened
  /// to `i128`, since the nanosecond fields' types differ between architectures).
  fn times(path: &str) -> Result<[i128; 4], String> {
    let stat = rustix::fs::stat(path).map_err(|e| format!("stat {path}: {e}"))?;
    Ok([
      i128::from(stat.st_mtime),
      i128::from(stat.st_mtime_nsec),
      i128::from(stat.st_ctime),
      i128::from(stat.st_ctime_nsec),
    ])
  }

  /// A file mapped shared and read-only; unmapped on drop, before the file closes.
  struct ReadMap {
    base: *const u8,
    /// The bytes mapped: the file's length, nonzero.
    len: usize,
    /// Held so the map's file stays open, and read through to revalidate it.
    file: OwnedFd,
  }

  impl ReadMap {
    /// Maps the first `len` bytes of `file`, `PROT_READ` and `MAP_SHARED`.
    fn new(file: OwnedFd, len: usize) -> Result<ReadMap, String> {
      if len == 0 {
        return Err("an empty file maps nothing".to_owned());
      }
      // SAFETY: a fresh read-only shared map of `len` bytes of the open `file`, at an address the kernel chooses;
      // nothing else maps it here, and `Drop` unmaps it before `file` closes.
      let base = unsafe {
        rustix::mm::mmap(
          std::ptr::null_mut(),
          len,
          ProtFlags::READ,
          MapFlags::SHARED,
          &file,
          0,
        )
      }
      .map_err(|e| e.to_string())?;
      Ok(ReadMap {
        base: base.cast::<u8>().cast_const(),
        len,
        file,
      })
    }

    /// Whether every byte of the map holds `byte`, each read as a volatile load.
    fn holds(&self, byte: u8) -> bool {
      (0..self.len).all(|offset| {
        // SAFETY: `offset` is below `self.len`, so the byte lies inside the live map made in `new`; a volatile read, so
        // it reads the page the kernel maps now.
        unsafe { self.base.add(offset).read_volatile() == byte }
      })
    }
  }

  impl Drop for ReadMap {
    fn drop(&mut self) {
      // SAFETY: unmaps exactly the map made in `new`, once; nothing reads it afterwards, and the file closes after.
      if let Err(e) = unsafe { rustix::mm::munmap(self.base.cast_mut().cast(), self.len) } {
        eprintln!("slates-pod-probe: munmap: {e}");
      }
    }
  }
}
