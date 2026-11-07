//! The KIND lane's live-shipping probe (`cargo xtask kind live`; §4.10, A-200; Ada's goal of 2026-11-04, writes
//! replicated under injected latency, jitter and loss): a process in a container of a daemon's pod streams writes to a
//! live-shipped volume and times each, or reads them back.
//!
//! It connects to the daemon the environment names, as `slates` itself does; the rendezvous is an abstract socket, which
//! Linux scopes to its network namespace (unix(7)), and every container of a pod shares one. Two modes, each one client
//! process (a client's attachments end with it, so one process attaches, writes and detaches; `pod_direct_probe.rs`):
//!
//! - `write VOLUME COUNT BYTES` attaches the volume for writing and writes `/live-N` for N in `0..COUNT`, each `BYTES`
//!   long ([`body`]), each once the one before is answered. A write to a live-shipped volume is answered only once a
//!   candidate holds it (A-200's barrier), so each time is a replicated write as its client sees it. It prints
//!   `live-write: count=N acknowledged=N refused=N p50_us=.. p99_us=.. p999_us=.. max_us=.. first_refusal=..`.
//! - `verify VOLUME COUNT BYTES` reads every `/live-N` back and prints `live-verify: whole=N missing=N torn=N`: the
//!   lane runs it on the owner, and on the successor after the owner's pod is deleted (NoLoss).
//!
//! `VOLUME` is the volume's 32 hexadecimal digits. The exit code is 0 when it ran, 2 when it could not; the lane judges
//! the line. Test infrastructure, built into the lane's probe image (`deploy/kind/pod-probe.Dockerfile`), never shipped.
//!
//! Run: `slates-pod-live-probe (write | verify) VOLUME COUNT BYTES` in a container sharing the daemon's network
//! namespace, as the daemon's user.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use slates_client::{Client, Deadlines, Intent, ReadAt, VolumeId};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_server::daemon::LIVENESS_BUDGET_NS;

/// Format: the tag of the write mode's line.
const WRITE_TAG: &str = "live-write:";
/// Format: the tag of the verify mode's line.
const VERIFY_TAG: &str = "live-verify:";
/// Shape: the exit code of a run that ran (the line is the verdict).
const RAN: u8 = 0;
/// Shape: the exit code of a run that could not.
const COULD_NOT_RUN: u8 = 2;
/// Format: a volume id's hexadecimal digits.
const VOLUME_HEX_DIGITS: usize = 32;
/// Format: a written file's mode.
const FILE_MODE: u32 = 0o644;

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let (tag, outcome) = match args.split_first() {
    Some((mode, rest)) if mode == "write" => (WRITE_TAG, Plan::of(rest).and_then(write)),
    Some((mode, rest)) if mode == "verify" => (VERIFY_TAG, Plan::of(rest).and_then(verify)),
    _ => (
      WRITE_TAG,
      Err(format!(
        "usage: slates-pod-live-probe (write | verify) VOLUME COUNT BYTES (given {args:?})"
      )),
    ),
  };
  match outcome {
    Ok(line) => {
      println!("{tag} {line}");
      ExitCode::from(RAN)
    }
    Err(why) => {
      println!("{tag} could not run: {why}");
      ExitCode::from(COULD_NOT_RUN)
    }
  }
}

/// What to write or read: the volume, how many files, and each one's length.
struct Plan {
  volume: VolumeId,
  count: usize,
  bytes: usize,
}

impl Plan {
  fn of(args: &[String]) -> Result<Plan, String> {
    let [volume, count, bytes] = args else {
      return Err(format!(
        "VOLUME COUNT BYTES expected (given {} arguments)",
        args.len()
      ));
    };
    Ok(Plan {
      volume: volume_id(volume)?,
      count: count.parse().map_err(|e| format!("COUNT {count:?}: {e}"))?,
      bytes: bytes.parse().map_err(|e| format!("BYTES {bytes:?}: {e}"))?,
    })
  }
}

fn volume_id(hex: &str) -> Result<VolumeId, String> {
  if hex.len() != VOLUME_HEX_DIGITS || !hex.is_ascii() {
    return Err(format!(
      "VOLUME {hex:?} is not {VOLUME_HEX_DIGITS} hex digits"
    ));
  }
  let mut bytes = [0u8; 16];
  for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
    let pair = std::str::from_utf8(pair).map_err(|e| format!("VOLUME {hex:?}: {e}"))?;
    *byte = u8::from_str_radix(pair, 16).map_err(|e| format!("VOLUME {hex:?}: {e}"))?;
  }
  Ok(VolumeId { bytes })
}

fn connect() -> Result<Client, String> {
  let instance = slates_ipc::rendezvous::instance_from_env();
  let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
  Client::connect(&instance, deadlines).map_err(|e| format!("connect to {instance:?}: {e}"))
}

/// `/live-N`'s bytes: a header naming the file, then a pattern of it, `bytes` in all, so a torn or misplaced write
/// cannot pass for another file's.
fn body(file: usize, bytes: usize) -> Vec<u8> {
  let mut body = format!("live write {file}\n").into_bytes();
  let seed = file.to_le_bytes().first().copied().unwrap_or(0);
  let mut at = 0u8;
  while body.len() < bytes {
    body.push(seed.wrapping_add(at));
    at = at.wrapping_add(1);
  }
  body.truncate(bytes);
  body
}

fn path(file: usize) -> String {
  format!("live-{file}")
}

/// Nearest-rank percentile `per_mille` of sorted `samples`, in microseconds.
fn percentile_us(sorted: &[Duration], per_mille: usize) -> u128 {
  let rank = sorted
    .len()
    .saturating_mul(per_mille)
    .div_ceil(1000)
    .saturating_sub(1);
  sorted.get(rank).map_or(0, Duration::as_micros)
}

fn write(plan: Plan) -> Result<String, String> {
  let mut client = connect()?;
  let attachment = client
    .attach(plan.volume, None, Intent::Write)
    .map_err(|e| format!("attach for writing: {e}"))?
    .attachment;
  let mut times = Vec::with_capacity(plan.count);
  let mut refused = 0usize;
  let mut first_refusal = None;
  for file in 0..plan.count {
    let bytes = body(file, plan.bytes);
    let began = Instant::now();
    match client.fs_write((plan.volume, attachment), &path(file), &bytes, FILE_MODE) {
      Ok(_) => times.push(began.elapsed()),
      Err(e) => {
        refused = refused.saturating_add(1);
        first_refusal.get_or_insert_with(|| format!("{}: {e}", path(file)));
      }
    }
  }
  client
    .detach(attachment)
    .map_err(|e| format!("detach: {e}"))?;
  times.sort();
  Ok(format!(
    "count={} acknowledged={} refused={refused} p50_us={} p99_us={} p999_us={} max_us={} first_refusal={:?}",
    plan.count,
    times.len(),
    percentile_us(&times, 500),
    percentile_us(&times, 990),
    percentile_us(&times, 999),
    times.last().map_or(0, Duration::as_micros),
    first_refusal.unwrap_or_default()
  ))
}

fn verify(plan: Plan) -> Result<String, String> {
  let mut client = connect()?;
  let (mut whole, mut missing, mut torn) = (0usize, 0usize, 0usize);
  for file in 0..plan.count {
    match client.read(plan.volume, &format!("/{}", path(file)), ReadAt::Head) {
      Ok(bytes) if bytes == body(file, plan.bytes) => whole = whole.saturating_add(1),
      Ok(_) => torn = torn.saturating_add(1),
      Err(_) => missing = missing.saturating_add(1),
    }
  }
  Ok(format!("whole={whole} missing={missing} torn={torn}"))
}
