//! The KIND lane's direct-map probe (`cargo xtask kind direct`; the DAX matrix's Kubernetes socket-route cells,
//! `docs/wip/virtiofs.md` §8): processes in containers of the daemon's pod write a file of a slates volume and map it by
//! the direct map (§4.6, A-135, A-171), and say whether the map is zero-copy and holds every byte.
//!
//! It connects to the daemon the environment names (`SLATES_ENDPOINT`, else the well-known instance), as `slates`
//! itself does. That reaches the daemon from any container of its pod: the rendezvous is an abstract socket, which
//! Linux scopes to its network namespace (unix(7)), and every container of a pod shares one. The daemon hands a direct
//! map only to its own user (A-171), which the pod's security context makes every container's.
//!
//! Two modes, each one client process:
//! - `write VOLUME PATH LEN FILL` attaches the volume for writing, writes `LEN` bytes of `FILL` at `PATH`, and detaches.
//!   It prints `direct-write: wrote N`. One client does all three because a client's attachments end with it (§4.7's
//!   failure matrix: the reaper removes a dead client's attachments at the liveness cadence), so a write split across
//!   `slates attach`, `slates write` and `slates detach` races that sweep (met by the lane on a loaded machine,
//!   2026-10-27: the detach answered `NotFound`).
//! - `map VOLUME PATH LEN FILL` maps the file by the direct map and prints `direct-map: zero_copy=BOOL whole=BOOL len=N`.
//!
//! `VOLUME` is the volume's 32 hexadecimal digits, `PATH` the file's path from the volume's root, `LEN` its length and
//! `FILL` the one byte value it holds. The exit code is 0 when the file was written, or mapped zero-copy and whole; 1
//! when the map is a copy or not whole; 2 when the probe could not run. Test infrastructure, built into the lane's probe
//! image (`deploy/kind/pod-probe.Dockerfile`) and never shipped.
//!
//! Run: `slates-pod-direct-probe (write | map) VOLUME PATH LEN FILL` in a container of the daemon's pod.

use std::process::ExitCode;

use slates_client::{Client, Deadlines, Intent, VolumeId};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_server::daemon::LIVENESS_BUDGET_NS;

/// Format: the tag of the map mode's one line, which the lane looks for.
const MAP_TAG: &str = "direct-map:";
/// Format: the tag of the write mode's one line, which the lane looks for.
const WRITE_TAG: &str = "direct-write:";
/// Format: the exit code when the file was written, or mapped zero-copy and whole.
const DONE: u8 = 0;
/// Format: the exit code when the map is a copy, or does not hold every byte.
const NOT_ZERO_COPY_OR_NOT_WHOLE: u8 = 1;
/// Format: the exit code when the probe could not run (bad arguments, no daemon, a refused verb).
const COULD_NOT_RUN: u8 = 2;
/// Format: the hexadecimal digits of a volume's id (16 bytes).
const VOLUME_HEX_DIGITS: usize = 32;
/// Format: the mode the written file is created with: the owner reads and writes it, the rest read it.
const FILE_MODE: u32 = 0o644;

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  let (tag, outcome) = match args.split_first() {
    Some((mode, rest)) if mode == "write" => (WRITE_TAG, write(rest).map(|()| true)),
    Some((mode, rest)) if mode == "map" => (MAP_TAG, map(rest)),
    _ => (
      MAP_TAG,
      Err(format!(
        "usage: slates-pod-direct-probe (write | map) VOLUME PATH LEN FILL (given {args:?})"
      )),
    ),
  };
  match outcome {
    Ok(true) => ExitCode::from(DONE),
    Ok(false) => ExitCode::from(NOT_ZERO_COPY_OR_NOT_WHOLE),
    Err(why) => {
      println!("{tag} could not run: {why}");
      ExitCode::from(COULD_NOT_RUN)
    }
  }
}

/// What both modes are given: the volume, the file's path from its root, its length and its one byte value.
struct File {
  volume: VolumeId,
  path: String,
  len: usize,
  fill: u8,
}

impl File {
  /// The file `VOLUME PATH LEN FILL` names.
  fn of(args: &[String]) -> Result<File, String> {
    let [volume, path, len, fill] = args else {
      return Err(format!(
        "VOLUME PATH LEN FILL expected (given {} arguments)",
        args.len()
      ));
    };
    Ok(File {
      volume: volume_id(volume)?,
      path: path.clone(),
      len: len.parse().map_err(|e| format!("LEN {len:?}: {e}"))?,
      fill: fill.parse().map_err(|e| format!("FILL {fill:?}: {e}"))?,
    })
  }
}

/// A client of the daemon the environment names.
fn connect() -> Result<Client, String> {
  let instance = slates_ipc::rendezvous::instance_from_env();
  let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
  Client::connect(&instance, deadlines).map_err(|e| format!("connect to {instance:?}: {e}"))
}

/// The write mode: attaches for writing, writes the file and detaches, as one client, and prints the tagged line.
fn write(args: &[String]) -> Result<(), String> {
  let file = File::of(args)?;
  let mut client = connect()?;
  let attachment = client
    .attach(file.volume, None, Intent::Write)
    .map_err(|e| format!("attach for writing: {e}"))?
    .attachment;
  let size = client
    .fs_write(
      (file.volume, attachment),
      file.path.trim_start_matches('/'),
      &vec![file.fill; file.len],
      FILE_MODE,
    )
    .map_err(|e| format!("write {}: {e}", file.path))?;
  client
    .detach(attachment)
    .map_err(|e| format!("detach: {e}"))?;
  println!("{WRITE_TAG} wrote {size}");
  Ok(())
}

/// The map mode: maps the file by the direct map, prints the tagged line, and says whether the map is zero-copy and
/// holds every byte.
fn map(args: &[String]) -> Result<bool, String> {
  let file = File::of(args)?;
  let mut client = connect()?;
  let mapped = client
    .map_read(file.volume, &file.path)
    .map_err(|e| format!("map_read {}: {e}", file.path))?;
  let bytes = mapped.to_vec().map_err(|e| format!("read the map: {e}"))?;
  let whole = bytes.len() == file.len && bytes.iter().all(|byte| *byte == file.fill);
  let zero_copy = mapped.is_zero_copy();
  println!(
    "{MAP_TAG} zero_copy={zero_copy} whole={whole} len={}",
    bytes.len()
  );
  Ok(zero_copy && whole)
}

/// The volume id `hex` spells: 32 hexadecimal digits, two to a byte.
fn volume_id(hex: &str) -> Result<VolumeId, String> {
  if hex.len() != VOLUME_HEX_DIGITS {
    return Err(format!(
      "VOLUME {hex:?}: {VOLUME_HEX_DIGITS} hexadecimal digits expected"
    ));
  }
  let mut bytes = [0_u8; VOLUME_HEX_DIGITS / 2];
  let (pairs, _) = hex.as_bytes().as_chunks::<2>();
  for (byte, pair) in bytes.iter_mut().zip(pairs) {
    let digits =
      std::str::from_utf8(pair.as_slice()).map_err(|e| format!("VOLUME {hex:?}: {e}"))?;
    *byte = u8::from_str_radix(digits, 16).map_err(|e| format!("VOLUME {hex:?}: {e}"))?;
  }
  Ok(VolumeId { bytes })
}
