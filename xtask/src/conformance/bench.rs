//! The NFS transports measured side by side (§4.6 A-35, A-38; `docs/wip/BENCHMARKS.md` "NFS
//! transports"): one release-built daemon mounted by the Linux kernel's own NFSv3 and NFSv4.2 clients
//! with the same options, driving the same file calls, round after round, the transports alternating
//! within each round so a drift in the host's load falls on both.
//!
//! A round, on a fresh directory of a fresh session per transport:
//! - `create`: [`SMALL_FILES`] files created and written with [`SMALL_BYTES`] each (open, write,
//!   close) — a namespace change and a small write per file;
//! - `stat`: each stated, after the client's caches are dropped, so every attribute comes from the
//!   server (LOOKUP and GETATTR);
//! - `read`: each read whole, the caches dropped first (READ);
//! - `unlink`: each removed;
//! - `seq-write`: one file of [`SEQ_CHUNKS`] × [`SEQ_CHUNK`] written in order and `fsync`ed (WRITE and
//!   COMMIT, the barrier included);
//! - `seq-read`: that file read in order, the caches dropped first.
//!
//! Every round of every phase is printed with the best and the median, so the table in BENCHMARKS.md
//! shows all N (CLAUDE.md §5). Linux only (the kernel's NFSv4.2 client and `drop_caches`), as root.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::slates::{Session, SlatesBinary};
use super::{Failure, HostOs, Options, Readiness, Run, Scratch, Suite, Transport};

/// Shape: small files per round — enough that the per-file cost dominates the phase's fixed cost.
const SMALL_FILES: usize = 256;
/// Shape: a small file's size: one page.
const SMALL_BYTES: usize = 4096;
/// Shape: the sequential file's write size: one MiB, a typical transfer.
const SEQ_CHUNK: usize = 1 << 20;
/// Shape: the sequential file's writes: 32 MiB in all, well inside a bench volume.
const SEQ_CHUNKS: usize = 32;
/// Shape: the bench volume's size: the sequential file with room for the small files' metadata.
const VOLUME_SIZE: &str = "256MiB";
/// Shape: the rounds a run takes unless `--rounds` says otherwise (best-of-five).
pub(crate) const DEFAULT_ROUNDS: u64 = 5;

/// The phases, in the order a round runs them.
const PHASES: [&str; 6] = ["create", "stat", "read", "unlink", "seq-write", "seq-read"];

/// `conformance bench`: every round of every phase over each Linux transport, printed.
pub(crate) fn run_bench(root: &Path, options: &Options, os: HostOs) -> Result<(), Failure> {
  if os != HostOs::Linux {
    return Err(Failure(
      "conformance bench: Linux only (the kernel's NFSv4.2 client, drop_caches)".to_owned(),
    ));
  }
  let binary = SlatesBinary::build_release(root)?;
  let transports = super::native_transports(os);
  let mut timings: Vec<Vec<Vec<Duration>>> = vec![vec![Vec::new(); PHASES.len()]; transports.len()];
  for round in 0..options.rounds {
    for (index, &transport) in transports.iter().enumerate() {
      let measured = one_round(root, options, os, transport, &binary)?;
      for (phase, duration) in measured.into_iter().enumerate() {
        println!(
          "nfs-bench: round {round} {} {}: {:.3} ms",
          transport.slug(),
          PHASES[phase],
          duration.as_secs_f64() * 1e3
        );
        timings[index][phase].push(duration);
      }
    }
  }
  for (index, &transport) in transports.iter().enumerate() {
    for (phase, durations) in timings[index].iter().enumerate() {
      summarize(transport, PHASES[phase], durations);
    }
  }
  Ok(())
}

/// One round over `transport`: a fresh session, each phase timed.
#[allow(clippy::disallowed_methods)] // the file calls under measurement, inside the mounted volume
fn one_round(
  root: &Path,
  options: &Options,
  os: HostOs,
  transport: Transport,
  binary: &SlatesBinary,
) -> Result<Vec<Duration>, Failure> {
  let root_available = match super::readiness(os, transport, Suite::Workloads) {
    Readiness::Ready { root } => root,
    Readiness::Skip(reason) => {
      return Err(Failure(format!(
        "conformance bench: {} is not runnable here: {}",
        transport.slug(),
        reason.text()
      )));
    }
  };
  let run = Run {
    root,
    options,
    os,
    transport,
    root_available,
    scratch: Scratch::open(options, transport)?,
  };
  let session = Session::open_with(&run, binary.clone(), ("bench", VOLUME_SIZE, false), None)?;
  let dir = session.workdir("round")?;
  let names: Vec<_> = (0..SMALL_FILES)
    .map(|index| dir.join(format!("small-{index}")))
    .collect();
  let payload = vec![0xA5u8; SMALL_BYTES];
  let mut measured = Vec::with_capacity(PHASES.len());
  measured.push(timed((transport, "create"), || {
    for path in &names {
      std::fs::File::create(path)?.write_all(&payload)?;
    }
    Ok(())
  })?);
  drop_caches()?;
  measured.push(timed((transport, "stat"), || {
    for path in &names {
      std::fs::metadata(path)?;
    }
    Ok(())
  })?);
  drop_caches()?;
  measured.push(timed((transport, "read"), || {
    let mut buffer = Vec::with_capacity(SMALL_BYTES);
    for path in &names {
      buffer.clear();
      std::fs::File::open(path)?.read_to_end(&mut buffer)?;
    }
    Ok(())
  })?);
  measured.push(timed((transport, "unlink"), || {
    for path in &names {
      std::fs::remove_file(path)?;
    }
    Ok(())
  })?);
  let sequential = dir.join("sequential");
  let chunk = vec![0x5Au8; SEQ_CHUNK];
  measured.push(timed((transport, "seq-write"), || {
    let mut file = std::fs::File::create(&sequential)?;
    for _ in 0..SEQ_CHUNKS {
      file.write_all(&chunk)?;
    }
    file.sync_all()
  })?);
  drop_caches()?;
  measured.push(timed((transport, "seq-read"), || {
    let mut buffer = vec![0u8; SEQ_CHUNK];
    let mut file = std::fs::File::open(&sequential)?;
    while file.read(&mut buffer)? != 0 {}
    Ok(())
  })?);
  std::fs::remove_file(&sequential)?;
  Ok(measured)
}

/// How long `phase` took, or its I/O failure as the bench's.
fn timed(
  (transport, name): (Transport, &str),
  phase: impl FnOnce() -> std::io::Result<()>,
) -> Result<Duration, Failure> {
  let started = Instant::now();
  phase().map_err(|e| Failure(format!("bench {} {name}: {e}", transport.slug())))?;
  Ok(started.elapsed())
}

/// Writes back and drops the kernel's page, dentry and inode caches, so the next phase's reads and
/// attributes come from the server.
fn drop_caches() -> Result<(), Failure> {
  let status = Command::new("sh")
    .args(["-c", "sync && echo 3 > /proc/sys/vm/drop_caches"])
    .status()?;
  if status.success() {
    Ok(())
  } else {
    Err(Failure(
      "conformance bench: dropping the caches failed (run as root)".to_owned(),
    ))
  }
}

/// Prints a phase's rounds: every one, the best and the median.
fn summarize(transport: Transport, phase: &str, durations: &[Duration]) {
  let mut sorted: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1e3).collect();
  sorted.sort_by(f64::total_cmp);
  let all: Vec<String> = durations
    .iter()
    .map(|d| format!("{:.2}", d.as_secs_f64() * 1e3))
    .collect();
  let best = sorted.first().copied().unwrap_or(0.0);
  let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
  println!(
    "nfs-bench: {} {phase}: best {best:.2} ms, median {median:.2} ms, rounds [{}]",
    transport.slug(),
    all.join(", ")
  );
}
