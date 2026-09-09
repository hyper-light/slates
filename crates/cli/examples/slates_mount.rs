//! Watch `slates mount` for real: a live kernel NFS mount of a RAM-only slates volume, with a file
//! written through it and read back — no privilege, no kernel extension, no Apple entitlement (§4.6,
//! R10). It starts an in-process daemon (the same `Daemon` the `slates` binary supervises), provisions
//! a volume through the real client, and then does exactly what `slates mount ID DIR` does: runs
//! `mount_nfs` against the daemon's loopback NFS port with the options `crates/cli/src/mount.rs`
//! builds. Where `crates/bridge-nfs/examples/nfs_loopback.rs` prints a port for a human to mount by
//! hand, this drives the whole lifecycle to a real kernel mount and back, automatically.
//!
//! It cannot call the `slates mount` code directly — that lives in the `slates` binary crate, reachable
//! only as the built command — so it mirrors the command's mechanism here and names the source. The
//! integration test `crates/cli/tests/cli.rs` drives the actual binary end to end.
//!
//! Run (macOS/BSD): `cargo run -p slates-cli --example slates_mount`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, Deadlines};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the machine-probe budget (milliseconds) of the quick profile — an input to derivations for
/// this demo daemon, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: how long to wait for the just-started daemon's rendezvous to answer (seconds) before giving
/// up — `Daemon::start` returns as the shards come up, a beat before the rendezvous is connectable.
const CONNECT_WAIT_SECS: u64 = 5;

/// A live kernel mount, torn down (`umount` then `rmdir`) when dropped, so even a panic mid-demo leaves
/// no mount or temp directory behind. Both steps are best-effort — a leftover mount is worse than a
/// swallowed teardown error.
struct Mounted {
  path: String,
}

impl Drop for Mounted {
  fn drop(&mut self) {
    let _ = Command::new("umount").arg(&self.path).output();
    let _ = Command::new("rmdir").arg(&self.path).output();
  }
}

/// Whether `mount_nfs` — the mechanism `slates mount` drives — is on this host (macOS and the BSDs).
fn mount_nfs_present() -> bool {
  Command::new("sh")
    .args(["-c", "command -v mount_nfs"])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// The quick machine profile the demo daemon derives its constants from.
fn quick_profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

/// A single-shard in-process daemon (R8, the laptop-degenerate case) named for this process.
fn start_daemon(instance: &str) -> Daemon {
  let profile = quick_profile();
  let config = DaemonConfig::derive(&profile, instance).with_shards(1);
  Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-example-{}", std::process::id()),
    },
  )
  .unwrap()
}

/// Connects a client to the daemon's rendezvous, retrying until it answers within the wait budget.
fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
    match Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(e) => {
        assert!(
          started.elapsed() < Duration::from_secs(CONNECT_WAIT_SECS),
          "connect to the demo daemon: {e}"
        );
        std::hint::spin_loop();
      }
    }
  }
}

/// The exact `mount_nfs` options `slates mount` builds (`crates/cli/src/mount.rs::mount_args`): NFSv3
/// over TCP, the daemon's port for both NFS and MOUNT (one server), `noresvport` for the unprivileged
/// mount (R10), `soft,intr` so a wedged mount is escapable, `locallocks,nosuid,rdirplus`, and
/// `actimeo=1` (the derived attribute-cache timeout).
fn mount_options(port: u16) -> String {
  format!(
    "vers=3,tcp,port={port},mountport={port},noresvport,soft,intr,locallocks,nosuid,rdirplus,\
     actimeo=1"
  )
}

/// A fresh, user-owned mount-point directory (`mktemp -d`, not `std::fs::create_dir` — R1).
fn fresh_mount_point() -> String {
  let out = Command::new("mktemp").arg("-d").output().unwrap();
  assert!(out.status.success(), "mktemp -d");
  String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Writes a file through the mount and reads it back (shell, not `std::fs` — R1), showing the bytes
/// round-trip host → NFS → the slates volume → NFS → host.
fn roundtrip_through(path: &str) {
  let message = "hello from a live slates kernel mount";
  let file = format!("{path}/hello.txt");
  let wrote = Command::new("sh")
    .arg("-c")
    .arg(format!("printf '%s' '{message}' > '{file}'"))
    .output()
    .unwrap();
  assert!(
    wrote.status.success(),
    "write through the mount: {}",
    String::from_utf8_lossy(&wrote.stderr)
  );
  let readback = Command::new("cat").arg(&file).output().unwrap();
  let got = String::from_utf8_lossy(&readback.stdout).into_owned();
  println!("  wrote {message:?} through the mount, read back {got:?}");
  assert_eq!(got, message, "the bytes read back byte for byte");
}

fn main() {
  if !mount_nfs_present() {
    println!("mount_nfs is not on this host; this live-mount example is macOS/BSD only.");
    return;
  }

  let instance = format!("slates-example-{}", std::process::id());
  let daemon = start_daemon(&instance);
  let mut client = connect(&instance);

  let name = "demo";
  client
    .create(&CreateSpec {
      name: name.to_owned(),
      // A small bounded RAM volume for the demo.
      size: SizeClass::Bounded { limit: 8 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let port = daemon
    .nfs_port()
    .expect("the daemon is serving NFS on loopback");
  println!("provisioned volume {name:?}; the daemon serves NFS on loopback port {port}");
  println!("the command this mirrors:  slates mount <volume-id> <dir>");

  let path = fresh_mount_point();
  let options = mount_options(port);
  let status = Command::new("mount_nfs")
    .args(["-o", &options, &format!("localhost:/{name}"), &path])
    .status()
    .unwrap();
  assert!(status.success(), "mount_nfs failed ({status})");
  let mounted = Mounted { path: path.clone() };
  println!("mounted a real kernel NFS mount at {path}");
  println!("  (unprivileged: noresvport — no root, no kernel extension, no Apple entitlement)");

  roundtrip_through(&path);

  let listing = Command::new("ls").args(["-la", &path]).output().unwrap();
  print!(
    "  ls of the mount:\n{}",
    String::from_utf8_lossy(&listing.stdout)
  );

  drop(mounted); // umount + rmdir
  println!("unmounted and cleaned up — the mount table is clean again.");
  drop(client);
  drop(daemon);
}
