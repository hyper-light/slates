//! The live crosvm virtio-fs DAX A/B harness (§4.6, A-9, A-135): slates' vhost-user-fs backend listening on a
//! filesystem unix socket so a REAL crosvm front end can connect and boot a guest against it — the end-to-end
//! proof that the message-based DAX binding (FUSE_SETUPMAPPING -> SHMEM_MAP over the vhost-user back-end
//! channel) maps a slates file into a guest's DAX window. The in-tree hermetic proof of the same park/ack path,
//! with a stand-in front end, is `a_message_based_setupmapping_...` in `crates/bridge-virtiofs/tests/device.rs`.
//!
//! This is a manually-run harness, not a CI test: it needs a Linux host with KVM and a `crosvm` binary (see the
//! sibling `README.md` for the recorded commands, hardware and date). On a non-Linux host it prints a skip and
//! exits; `cargo clippy --all-targets` compiles it everywhere.
//!
//! Ada authorized the on-disk socket for THIS harness only (2026-10-16): crosvm's `--vhost-user` takes only
//! `socket=PATH` (no fd-passing, no abstract socket), so the backend must listen on a named socket. This is the
//! one R1 deviation, confined to this example binary — the hermetic daemon never binds an on-disk socket;
//! production keeps control-channel fd-passing. `examples/` is exempt from the `cargo xtask structural` wall, and
//! the file-level allow below covers the clippy methods the authorized socket cleanup and the harness `expect`s
//! would otherwise trip.
#![allow(
  clippy::disallowed_methods,
  clippy::disallowed_types,
  clippy::unwrap_used,
  clippy::expect_used
)]

#[cfg(target_os = "linux")]
fn main() {
  linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("skip: the crosvm virtio-fs DAX A/B harness runs on Linux (KVM + crosvm) only");
}

#[cfg(target_os = "linux")]
mod linux {
  use std::os::fd::OwnedFd;
  use std::os::unix::net::UnixListener;
  use std::sync::mpsc::channel;

  use slates_client::{Client, CreateSpec, Deadlines, NamePolicy, SizeClass};
  use slates_machine::{MachineProfile, ProfileOptions};
  use slates_server::virtiofs::{FsTag, GuestHarness, GuestView};
  use slates_server::{Daemon, DaemonConfig, SegmentSource};

  /// A generous boot budget for the vhost-user negotiation: a real guest kernel boots (seconds) before its
  /// driver configures the rings; the serve loop then waits on the doorbell. 120 s covers a cold boot on a busy
  /// box. (A harness constant: `examples/` is exempt from the literal check; stated here so the wait is legible.)
  const BOOT_NS: u64 = 120_000_000_000;

  /// The DAX cache window offered to the front end (8 MiB); crosvm sizes the DAX BAR to this. Deliberately small
  /// for the learned-value measurement: the guest's working set must EXCEED its cache for the guest kernel to
  /// reclaim mapping ranges, which is the only way a revoke→remap interval is ever produced. A 256 MiB cache with a
  /// file that fits in a 512 MB VM never reclaims, so it would record no remap at all.
  const DAX_REGION: u64 = 8 << 20;

  fn deadlines() -> Deadlines {
    Deadlines::derive(
      slates_server::daemon::LIVENESS_BUDGET_NS,
      slates_db::replay::RECOVERY_BUDGET_NS,
    )
    .get()
  }

  pub(crate) fn run() {
    let socket_path = std::env::args()
      .nth(1)
      .expect("usage: crosvm_dax_ab <socket_path> [dax]");
    let dax = std::env::args().nth(2).as_deref() == Some("dax");

    let profile = MachineProfile::measure(ProfileOptions {
      budget_per_probe: ProfileOptions::default().budget_per_probe,
      codecs: false,
      core_matrix: false,
    })
    .expect("the machine profile measures");
    let instance = format!("crosvm-ab-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance, Some(1));
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: format!("slates-crosvm-ab-{}", std::process::id()),
      },
    )
    .expect("the daemon starts");
    daemon
      .bootstrap(true)
      .expect("the local consensus group is created");

    let mut client = Client::connect(&instance, deadlines()).expect("the client connects");
    let id = client
      .create(&CreateSpec {
        name: "fs".to_owned(),
        size: SizeClass::Bounded { limit: 96 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .expect("the volume is provisioned");

    // The authorized on-disk socket: remove any stale node, bind, listen; crosvm connects here.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("the harness binds its socket");
    println!("HARNESS-LISTENING {socket_path}");
    let (conn, _) = listener.accept().expect("crosvm connects");
    println!("HARNESS-ACCEPTED a front-end connection");
    let fd: OwnedFd = conn.into();

    let (end_tx, end_rx) = channel();
    let harness = GuestHarness {
      on_admitted: Box::new(|_| println!("HARNESS-ADMITTED the guest device")),
      on_end: Box::new(move |outcome| {
        let _ = end_tx.send(format!("{outcome:?}"));
      }),
    };
    let tag = FsTag::new("slates").unwrap();
    let attached = if dax {
      println!("HARNESS-DAX on (cache region {DAX_REGION} bytes)");
      daemon.attach_vhost_user_device_dax(
        id,
        tag,
        GuestView::default(),
        (fd, BOOT_NS),
        DAX_REGION,
        harness,
      )
    } else {
      println!("HARNESS-DAX off (non-DAX baseline)");
      daemon.attach_vhost_user_device(id, tag, GuestView::default(), (fd, BOOT_NS), harness)
    };
    attached.expect("the vhost-user device attaches");
    println!("HARNESS-ATTACHED; serving until the guest device ends");

    match end_rx.recv() {
      Ok(outcome) => println!("HARNESS-ENDED {outcome}"),
      Err(e) => println!("HARNESS-END-CHANNEL-CLOSED {e}"),
    }
    let _ = std::fs::remove_file(&socket_path);
  }
}
