//! Serving a volume as a host mount (§4.6): the volume at a path every process of the host sees, through the
//! platform's own mechanism and no privilege (R10).
//!
//! - **macOS**: the loopback NFS bridge, `mount(2)` with the export's root handle fetched over this process's own
//!   loopback socket, so the mount capability never appears in `ps`, `mount` or `nfsstat -m` (§4.6 A-34). No kernel
//!   extension, no FUSE library and no Apple entitlement: slates serves its own NFS. `unmount(2)` removes it, no
//!   program run (A-507).
//! - **Linux**: FUSE; the daemon mounts through the OS's `fusermount3` and serves the mount on the volume's owner shard
//!   (AUD-29-64), and `fusermount3 -u` removes it.
//! - **Windows**: a WinFsp drive the daemon mounts and serves itself; detaching its attachment removes it.
//! - **The other BSDs**: `mount_nfs` on the `PATH`, with the same options as macOS's `mount(2)`.
//!
//! Every mount rides the mount's own attachment (§4.6, §4.13; AUD-01): it outlives the process that made it and a
//! daemon restart, and ends with the kernel's unmount, a `detach`, or the volume's destroy. A write mount takes the
//! volume's write lease (D-16); a read-only mount takes none and is served read-only.
//!
//! Moved here from `crates/cli/src/mount.rs` on 2026-11-18 (A-278), so the CLI and an embedding host mount through
//! one implementation. The shape is adapted from sylk's cgofuse mount (`core/purevfs`): a pure, injectable capability
//! detection (`classify`, the other BSDs' and tested on any host with no live mount), a probe that refuses with a message
//! naming what is missing, and the mount/unmount lifecycle.

#[cfg(all(unix, not(target_os = "macos")))]
use std::process::Command;

#[cfg(not(windows))]
use slates_bridge_oci::mount_table::{MountEntry, MountIdentity, MountTableError};

use slates_ipc::protocol::{Intent, VolumeId};

use crate::error::{Error, Missing, MountFault, MountMissing};
use crate::instance::Instance;

/// How a volume is mounted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountOptions {
  /// A read-only mount: no write lease, every change refused (the kernel refuses writes at the mount too).
  pub read_only: bool,
  /// Present only this directory of the volume, held to it by the daemon on every request (AUD-29-76); the Unix
  /// mounts' only.
  pub subtree: Option<String>,
  /// Let other local users' processes (a container runtime's) reach the mount: the Linux FUSE mount's `allow_other`.
  pub shared: bool,
}

/// A mount made: its attachment (what `detach` takes) and the path the kernel records it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mounted {
  /// The mount's attachment.
  pub attachment: u64,
  /// The mount point, as the kernel records it (the real path; a drive letter on Windows).
  pub path: String,
}

/// A mount removed ([`Instance::unmount`]). On Windows the drive is found among the volumes' reported mounts, and a
/// volume whose status the daemon refused (one recovery could not rebuild) cannot report its mounts: when another
/// volume reported the drive and it was released, each such volume is named here, so the caller can say whose mounts
/// are unknown. Empty on macOS and Linux, whose unmount is the OS's own and asks no volume.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Unmounted {
  /// Each volume the scan could not ask, and its status's refusal.
  pub unreported: Vec<crate::error::Unreported>,
}

/// Format: a mount capability as `attach` returns it — the attachment id and its 16-byte secret token (§4.13; AUD-01),
/// the bearer authority the daemon validates every request's file handle against.
pub type MountCapability = (u64, [u8; 16]);

/// The export path a capability mount presents: `/<name>@<attachment_hex>.<token_hex>` (§4.13; AUD-01) — the
/// loopback mount's source, and the network export's PersistentVolume path (`slates export`).
pub fn export_path(name: &str, capability: MountCapability) -> String {
  let (attachment, token) = capability;
  let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
  format!("/{name}@{attachment:x}.{token_hex}")
}

impl Instance {
  /// Mounts `volume` at `path` (§4.6): an existing directory the caller owns on macOS and Linux, a drive letter (`S:`)
  /// on Windows. Returns the mount's attachment and the path the kernel records. Refused typed: the daemon's refusal
  /// (`LeaseHeld` for a write mount while another principal holds the volume's write lease, D-16), or a
  /// [`MountFault`] naming what the host lacks or what the kernel refused.
  pub fn mount(
    &mut self,
    volume: VolumeId,
    path: &str,
    options: &MountOptions,
  ) -> Result<Mounted, Error> {
    let intent = if options.read_only {
      Intent::Read
    } else {
      Intent::Write
    };
    self.mount_platform(volume, path, intent, options)
  }

  /// Removes the mount at `path`: on macOS and Linux the OS's own unmount ([`unmount`]), which needs no daemon; on
  /// Windows the drive is the daemon's own, so its attachment is found among the volumes' reported mounts and
  /// detached. What was removed names each volume the Windows scan could not ask ([`Unmounted::unreported`]).
  pub fn unmount(&mut self, path: &str) -> Result<Unmounted, Error> {
    self.unmount_platform(path)
  }

  /// See [`Instance::mount`]: the loopback NFS bridge (macOS and the BSDs).
  #[cfg(all(unix, not(target_os = "linux")))]
  fn mount_platform(
    &mut self,
    volume: VolumeId,
    path: &str,
    intent: Intent,
    options: &MountOptions,
  ) -> Result<Mounted, Error> {
    // A container runtime reaches a macOS host mount through Docker Desktop's file sharing, as the mounting user (the
    // runtime profile's identity rule), so nothing need be shared with other local users.
    if options.shared {
      return Err(Error::Mount(MountFault::Unsupported {
        missing: MountMissing::SharedMount,
      }));
    }
    available().map_err(Error::Mount)?;
    // The mount point as the kernel records it (a read, never a write): the daemon finds the mount it binds and the
    // mounts it keeps across a restart in the kernel's table by exactly this path (A-227).
    let real = real_path(path)?;
    // The raw status: a mount needs only the volume's name and the daemon's port, so a base the status could not read
    // now does not stop it.
    let report = self
      .client
      .status(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    let port = report
      .nfs_port
      .ok_or(Error::Mount(MountFault::NoNfsListener))?;
    let attachment = match &options.subtree {
      Some(subtree) => self.client.attach_scoped_mount(volume, intent, subtree),
      None => self.client.attach_mount(volume, intent),
    }
    .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    let token = attachment
      .token
      .ok_or(Error::Mount(MountFault::NoCapability))?;
    mount_volume(
      port,
      &report.name,
      (attachment.attachment, token),
      &real,
      options.read_only,
    )?;
    // The established mount point is reported back (§4.4 `Binding → Bound`), so the record names it and `status`
    // shows it; a bind the daemon refuses takes the mount down again rather than leave a mount whose record says it
    // is bound nowhere.
    if let Err(error) = self.client.bind_mount(attachment.attachment, &real) {
      let bind = self.fail(
        error,
        Some(Missing::Attachment {
          attachment: attachment.attachment,
        }),
      );
      return Err(match unmount(&real) {
        Ok(()) => bind,
        Err(fault) => Error::Mount(MountFault::Unbound {
          bind: Box::new(bind),
          unmount: Box::new(fault),
        }),
      });
    }
    Ok(Mounted {
      attachment: attachment.attachment,
      path: real,
    })
  }

  /// See [`Instance::mount`]: Linux FUSE (§4.6 "Linux"; AUD-29-64). The daemon mounts through the OS's `fusermount3` at
  /// the path — resolved here, in the client, to the real path the kernel records (a read, never a write) — and serves
  /// it; the reply comes once the mount is established.
  #[cfg(target_os = "linux")]
  fn mount_platform(
    &mut self,
    volume: VolumeId,
    path: &str,
    intent: Intent,
    options: &MountOptions,
  ) -> Result<Mounted, Error> {
    let real = real_path(path)?;
    let attachment = match (&options.subtree, options.shared) {
      (subtree, true) => self
        .client
        .attach_shared_fuse(volume, intent, &real, subtree.as_deref()),
      (Some(subtree), false) => self
        .client
        .attach_scoped_fuse(volume, intent, &real, subtree),
      (None, false) => self.client.attach_fuse(volume, intent, &real),
    }
    .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    Ok(Mounted {
      attachment: attachment.attachment,
      path: attachment.path.unwrap_or(real),
    })
  }

  /// See [`Instance::mount`]: a WinFsp drive the daemon mounts and serves itself (§4.6 "Windows"). It presents the
  /// whole volume, so a subtree or a shared mount is refused.
  #[cfg(windows)]
  fn mount_platform(
    &mut self,
    volume: VolumeId,
    path: &str,
    intent: Intent,
    options: &MountOptions,
  ) -> Result<Mounted, Error> {
    if options.subtree.is_some() || options.shared {
      return Err(Error::Mount(MountFault::Unsupported {
        missing: MountMissing::WindowsSubtreeOrShared,
      }));
    }
    let attachment = self
      .client
      .attach_winfsp(volume, intent, path)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    Ok(Mounted {
      attachment: attachment.attachment,
      path: attachment.path.unwrap_or_else(|| path.to_owned()),
    })
  }

  /// See [`Instance::unmount`]: the OS's own unmount.
  #[cfg(not(windows))]
  fn unmount_platform(&mut self, path: &str) -> Result<Unmounted, Error> {
    unmount(path).map_err(Error::Mount)?;
    Ok(Unmounted::default())
  }

  /// See [`Instance::unmount`]: the WinFsp drive at `path` is the daemon's own, so it is unmounted by detaching its
  /// attachment, found among the volumes' reported mounts; the daemon then removes the drive. Every volume is asked:
  /// one whose status is refused (a volume recovery could not rebuild answers `NotRecovered`) cannot report its mounts,
  /// so it is passed over and named, and the drive is still released when another volume reports it: named in the
  /// answer then, in the refusal when no volume reported the drive. Until 2026-11-18 the answer dropped the names
  /// (the integrator's Windows lane, run 44257181904). A channel or daemon failure ends the scan, since no later volume
  /// can be asked either.
  #[cfg(windows)]
  fn unmount_platform(&mut self, path: &str) -> Result<Unmounted, Error> {
    let volumes = self.list()?;
    let mut found = None;
    let mut unreported = Vec::new();
    for summary in volumes {
      match self.client.status(summary.id) {
        Ok(report) => {
          if found.is_none() {
            found = report
              .mounts
              .iter()
              .find(|mount| mount.path.eq_ignore_ascii_case(path))
              .map(|mount| mount.attachment);
          }
        }
        Err(slates_client::ClientError::Refused(refusal)) => {
          unreported.push(crate::error::Unreported {
            volume: summary.id,
            name: summary.name,
            refusal: Box::new(Error::of_refusal(
              refusal,
              Some(Missing::Volume(summary.id)),
              self.client.retry_pause_ns(0),
            )),
          })
        }
        Err(other) => return Err(self.error(other)),
      }
    }
    match found {
      Some(attachment) => {
        self.detach(attachment)?;
        Ok(Unmounted { unreported })
      }
      None => Err(Error::Mount(MountFault::NotMounted {
        path: path.to_owned(),
        unreported,
      })),
    }
  }
}

/// The mount point as the kernel records it (macOS resolves `/var` to `/private/var`, and a mount is found in the
/// kernel's table by exactly this path): a read of the path, never a write.
#[cfg(unix)]
fn real_path(path: &str) -> Result<String, Error> {
  std::fs::canonicalize(path)
    .map(|real| real.to_string_lossy().into_owned())
    .map_err(|error| {
      Error::Mount(MountFault::MountPoint {
        path: path.to_owned(),
        errno: error.raw_os_error(),
      })
    })
}

/// Derived: the attribute-cache timeout (`actimeo`, whole seconds — `man mount_nfs`: `actimeo=⟨seconds⟩`) the mount
/// requests. §4.6 owed this value "from the measured loopback RTT"; the RTT resolves it, and floors it. slates's
/// loopback GETATTR RTT is sub-millisecond — roughly 10,000× finer than the whole-second `actimeo` knob — so an
/// RTT-derived timeout rounds to the knob's minimum. The two competing goals both land on that minimum: `actimeo=0`
/// (`noac`) sends every `getattr` to the server and defeats `rdirplus`'s attribute batching, while the macOS default
/// (5–60 s, scaled by file age) is far too stale for an overlay that changes under merges and outside edits. So the
/// mount asks for the finest nonzero cache the knob expresses, one second — the same freshness/amortization trade-off
/// sylk documents for its 100 ms FUSE attribute timeout (`core/purevfs`).
#[cfg(any(all(unix, not(target_os = "linux")), test))]
const ATTR_CACHE_SECONDS: u32 = 1;

/// The READ and WRITE size the macOS mount asks for (`rsize`/`wsize`; the kernel takes the smaller of this and its own
/// limit, and the server's `MAX_TRANSFER` caps it). The largest power of two at which the macOS client's small-file
/// walk does not slow. macOS's default was 32 KiB. Swept 2026-10-05 in random order, four rounds each (Apple M5 Max, a
/// fresh daemon and mount per round):
///
/// | transfer | 688-file tree, cold read | 64 MiB write + fsync | 64 MiB cold read |
/// |---|---|---|---|
/// | 32 KiB | 110–122 ms | 79–83 ms | 59–61 ms |
/// | 64 KiB | 108–123 ms | 50–53 ms | 37–41 ms |
/// | 128 KiB | 109–123 ms | 35–39 ms | 25–29 ms |
/// | 256 KiB | 127–143 ms | 30–31 ms | 19–21 ms |
///
/// At 256 KiB the client's own per-request cost slowed the small-file walk by about 17%, for a further 15–25% on large
/// transfers. 128 KiB keeps the walk and more than halves large transfers.
/// Measured: 128 KiB, the knee of the sweep above.
#[cfg(target_os = "macos")]
const MACOS_TRANSFER_BYTES: u32 = 128 * 1024;

/// The loopback mount mechanism a host offers (§4.6), the analogue of sylk's FUSE-backend selection. slates serves its
/// own NFS, so its mechanism is `mount_nfs` — built into macOS and the BSDs, needing no FUSE library, kernel
/// extension, or Apple entitlement.
#[cfg(any(all(unix, not(target_os = "macos"), not(target_os = "linux")), test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MountBackend {
  /// The loopback NFS mount, via `mount_nfs`.
  NfsLoopback,
  /// No supported loopback mount on this host (`mount_nfs` is not on the `PATH`).
  Unsupported,
}

/// Classifies the loopback mount mechanism from an injectable "is this command on the `PATH`" probe. Pure — no
/// filesystem or process references — so every branch tests on any host without a live mount, exactly as sylk factors
/// its detection into a predicate-injected `classifyDarwinFUSEBackend`.
#[cfg(any(all(unix, not(target_os = "macos"), not(target_os = "linux")), test))]
pub(crate) fn classify(command_exists: impl Fn(&str) -> bool) -> MountBackend {
  if command_exists("mount_nfs") {
    MountBackend::NfsLoopback
  } else {
    MountBackend::Unsupported
  }
}

/// Whether `name` resolves to an executable on the `PATH` (the analogue of sylk's `commandAvailable` over
/// `exec.LookPath`): a directory of the `PATH` holds an entry `name` the caller may execute.
#[cfg(all(unix, not(target_os = "macos")))]
fn command_on_path(name: &str) -> bool {
  let Some(paths) = std::env::var_os("PATH") else {
    return false;
  };
  std::env::split_paths(&paths)
    .any(|dir| rustix::fs::access(dir.join(name), rustix::fs::Access::EXEC_OK).is_ok())
}

/// The loopback mount mechanism a BSD other than macOS offers, probing the real `PATH` for `mount_nfs`; macOS mounts
/// through `mount(2)` itself (§4.6 A-34), always present ([`available`]).
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
pub(crate) fn backend() -> MountBackend {
  classify(command_on_path)
}

/// The host-mount mechanism a machine offers (§4.6), as [`available`] finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountKind {
  /// The loopback NFS bridge: `mount(2)` on macOS (A-34), `mount_nfs` on the other BSDs.
  NfsLoopback,
  /// FUSE, mounted by the daemon through the OS's `fusermount3` (Linux).
  Fuse,
  /// A WinFsp drive the daemon mounts and serves (Windows).
  WinFsp,
}

/// Format: the FUSE device `fusermount3` opens for the daemon (Linux).
#[cfg(target_os = "linux")]
const FUSE_DEVICE: &str = "/dev/fuse";

/// Whether this machine can serve a volume as a host mount, and through what (§4.6): the fact a host reports before it
/// asks for a mount (kiln's runner facts). Refused [`MountFault::Unsupported`] naming what is missing. It reads the
/// machine's state — this process's `PATH`, `/dev/fuse`, WinFsp's library — and mounts nothing, so it needs no daemon;
/// a daemon started from this process finds the same `PATH`. macOS always offers its loopback mount, `mount(2)` being
/// built in.
///
/// On macOS and the BSDs the loopback mount is offered only where no one but root can capture loopback traffic (A-535):
/// its capability rides every file handle in the clear, since the kernel's NFS client speaks neither RPC-with-TLS nor
/// RPCSEC_GSS ([`MountMissing::PrivateLoopback`] otherwise).
#[cfg(target_os = "macos")]
pub fn available() -> Result<MountKind, MountFault> {
  loopback_available(capture_open(bpf_devices()))
}

/// See the macOS arm: the other BSDs need `mount_nfs` on the `PATH`, and a loopback no one but root can capture.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
pub fn available() -> Result<MountKind, MountFault> {
  nfs_available(backend()).and_then(|_| loopback_available(capture_open(bpf_devices())))
}

/// Format: the BPF capture devices of macOS and the BSDs: `/dev/bpf` (FreeBSD's cloning device) and `/dev/bpf<n>`,
/// which xnu makes in order from 0 (`bsd/net/bpf.c`).
#[cfg(all(unix, not(target_os = "linux")))]
const BPF_DEVICE: &str = "/dev/bpf";

/// Format: the owner-read permission bit (`S_IRUSR`, `<sys/stat.h>`).
#[cfg(any(all(unix, not(target_os = "linux")), test))]
const READ_BY_OWNER: u32 = 0o400;
/// Format: the group-read and other-read permission bits (`S_IRGRP | S_IROTH`, `<sys/stat.h>`).
#[cfg(any(all(unix, not(target_os = "linux")), test))]
const READ_BY_OTHERS: u32 = 0o044;
/// Format: root's user id.
#[cfg(any(all(unix, not(target_os = "linux")), test))]
const ROOT_UID: u32 = 0;

/// The BPF capture devices this host has, as `(st_mode, st_uid)`: `/dev/bpf` if it exists, then `/dev/bpf0`,
/// `/dev/bpf1` and on until one is missing or cannot be read, so the count is the system's own (a read of the device
/// nodes' metadata, never an open).
#[cfg(all(unix, not(target_os = "linux")))]
fn bpf_devices() -> Vec<(u32, u32)> {
  let of = |stat: rustix::fs::Stat| (u32::from(stat.st_mode), stat.st_uid);
  let mut devices: Vec<(u32, u32)> = rustix::fs::stat(BPF_DEVICE).map(of).into_iter().collect();
  for index in 0u32.. {
    match rustix::fs::stat(format!("{BPF_DEVICE}{index}")) {
      Ok(stat) => devices.push(of(stat)),
      Err(_) => break,
    }
  }
  devices
}

/// Whether anyone but root can capture this host's loopback traffic (A-535), judged from its BPF devices' modes and
/// owners (`devices`, as `(st_mode, st_uid)`): one a group or anyone may read, or one owned by a user other than root
/// who may read it. macOS leaves each `crw-------` root's alone; Wireshark's ChmodBPF gives every one to its
/// `access_bpf` group to read. Pure, so every case is tested on any host.
#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(crate) fn capture_open(devices: impl IntoIterator<Item = (u32, u32)>) -> bool {
  devices
    .into_iter()
    .any(|(mode, uid)| mode & READ_BY_OTHERS != 0 || (uid != ROOT_UID && mode & READ_BY_OWNER != 0))
}

/// Whether the loopback mount may be offered on a host where loopback capture is `open` or not (A-535): refused
/// [`MountMissing::PrivateLoopback`] where it is open. Pure, so every case is tested on any host.
#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(crate) fn loopback_available(open: bool) -> Result<MountKind, MountFault> {
  if open {
    return Err(MountFault::Unsupported {
      missing: MountMissing::PrivateLoopback,
    });
  }
  Ok(MountKind::NfsLoopback)
}

/// See the macOS arm: Linux needs `fusermount3` on the `PATH` and `/dev/fuse` this user may read and write.
#[cfg(target_os = "linux")]
pub fn available() -> Result<MountKind, MountFault> {
  let device = rustix::fs::access(
    FUSE_DEVICE,
    rustix::fs::Access::READ_OK | rustix::fs::Access::WRITE_OK,
  );
  fuse_available(command_on_path("fusermount3"), device.is_ok())
}

/// See the macOS arm: Windows needs WinFsp's library to load (its own `FspLoad` search), and the machine to let WinFsp's
/// driver make the mount's drive (A-369), which the driver decides at every mount from `MountUseMountmgrFromFSD` (A-417):
/// without it a host was told it could mount, and each mount was then refused `MountManagerNotAllowed`.
#[cfg(windows)]
pub fn available() -> Result<MountKind, MountFault> {
  winfsp_available(
    slates_bridge_winfsp::host::installed(),
    slates_bridge_winfsp::host::mount_manager_allowed(),
  )
}

/// WinFsp's availability from the two facts it needs: its library loads, and the machine lets its driver make the
/// mount's drive. Pure, so every case tests on any host.
#[cfg(any(windows, test))]
fn winfsp_available(library_loads: bool, drive_allowed: bool) -> Result<MountKind, MountFault> {
  match (library_loads, drive_allowed) {
    (true, true) => Ok(MountKind::WinFsp),
    (false, _) => Err(MountFault::Unsupported {
      missing: MountMissing::WinFsp,
    }),
    (true, false) => Err(MountFault::Unsupported {
      missing: MountMissing::MountManager,
    }),
  }
}

/// The loopback mount's availability from the backend a BSD offers. Pure, so it tests on any host.
#[cfg(any(all(unix, not(target_os = "macos"), not(target_os = "linux")), test))]
fn nfs_available(backend: MountBackend) -> Result<MountKind, MountFault> {
  match backend {
    MountBackend::NfsLoopback => Ok(MountKind::NfsLoopback),
    MountBackend::Unsupported => Err(MountFault::Unsupported {
      missing: MountMissing::MountNfs,
    }),
  }
}

/// FUSE's availability from the two facts it needs: `fusermount3` found on the `PATH`, and the FUSE device usable by
/// this user. Pure, so every case tests on any host.
#[cfg(any(target_os = "linux", test))]
fn fuse_available(helper_found: bool, device_usable: bool) -> Result<MountKind, MountFault> {
  match (helper_found, device_usable) {
    (true, true) => Ok(MountKind::Fuse),
    (false, _) => Err(MountFault::Unsupported {
      missing: MountMissing::Fusermount3,
    }),
    (true, false) => Err(MountFault::Unsupported {
      missing: MountMissing::FuseDevice,
    }),
  }
}

/// The `mount_nfs` arguments to mount the volume named `name`, served on the daemon's loopback `port`, at `path`, under
/// the mount `capability` (§4.13; AUD-01 — the attachment id and token `attach` returned, the bearer authority the
/// daemon validates every request's file handle against; a mount path with no capability reaches nothing): the
/// design's options (§4.6 — NFSv3 over TCP, the explicit `port`/`mountport` so no portmap query is needed, `hard,intr`
/// so a call waits as long as the daemon asks and a signal still ends the wait, `locallocks`, `nosuid`, `nodev`,
/// `rdirplus`), plus `noresvport` for the unprivileged mount (R10) and the derived attribute-cache timeout, and
/// `rdonly` for a read-only mount (the kernel refuses writes at the mount, as the daemon does under the read-only
/// capability). slates serves NFS and MOUNT on one port, so `port` and `mountport` are the same. The export is
/// `localhost:/<name>@<attachment_hex>.<token_hex>`, the volume's provisioned name under the synthetic host root with
/// its capability.
#[cfg(any(all(unix, not(target_os = "macos"), not(target_os = "linux")), test))]
pub(crate) fn mount_args(
  port: u16,
  name: &str,
  capability: MountCapability,
  path: &str,
  read_only: bool,
) -> Vec<String> {
  let access = if read_only { ",rdonly" } else { "" };
  let options = format!(
    "vers=3,tcp,port={port},mountport={port},noresvport,hard,intr,locallocks,nosuid,nodev,rdirplus,\
     actimeo={ATTR_CACHE_SECONDS}{access}"
  );
  vec![
    "-o".to_owned(),
    options,
    format!("localhost:{}", export_path(name, capability)),
    path.to_owned(),
  ]
}

/// Mounts the volume on macOS without the capability leaving this process (§4.6 A-34): the root handle comes from the
/// daemon's MOUNT service over this process's own loopback socket, and goes to the kernel in `mount(2)`'s XDR
/// arguments (`NFS_MATTR_FH`), with a source name that holds no secret (`slates:/<name>`). Nothing another user can
/// read (`ps`, the mount table) carries the token. The MOUNT call waits while its connection lives, as the hard mount
/// it makes does: a daemon gone ends it at once and a stuck one is ended by its watchdog or its anchor (A-517,
/// `slates_bridge_nfs::client::exchange`); until A-517 it was bounded by one liveness budget.
#[cfg(target_os = "macos")]
fn mount_volume(
  port: u16,
  name: &str,
  capability: MountCapability,
  path: &str,
  read_only: bool,
) -> Result<(), Error> {
  use slates_bridge_nfs::client::{Credentials, FetchError, MountArgs, fetch_root_handle};
  let caller = Credentials {
    uid: rustix::process::getuid().as_raw(),
    gid: rustix::process::getgid().as_raw(),
    gids: rustix::process::getgroups()
      .map(|groups| groups.into_iter().map(|gid| gid.as_raw()).collect())
      .unwrap_or_default(),
  };
  let handle =
    fetch_root_handle(port, &caller, &export_path(name, capability)).map_err(|error| {
      Error::Mount(match error {
        FetchError::Io(io) => MountFault::RootHandle {
          errno: io.raw_os_error(),
          status: None,
        },
        FetchError::Malformed => MountFault::RootHandle {
          errno: None,
          status: None,
        },
        FetchError::Refused(status) => MountFault::RootHandle {
          errno: None,
          status: Some(status),
        },
      })
    })?;
  // `nosuid` and `nodev` whoever mounts (A-94): a volume is shared, so a setuid binary or a device node one writer
  // plants must not run or open with its privilege for another. xnu adds both for an unprivileged caller only.
  let mut mnt_flags = libc::MNT_NOSUID | libc::MNT_NODEV;
  if read_only {
    mnt_flags |= libc::MNT_RDONLY;
  }
  let refused = |errno: Option<i32>| {
    Error::Mount(MountFault::Refused {
      path: path.to_owned(),
      errno,
    })
  };
  let mnt_flags_word = u32::try_from(mnt_flags).map_err(|_| refused(None))?;
  let args = MountArgs {
    port,
    handle,
    attr_cache_seconds: ATTR_CACHE_SECONDS,
    transfer_bytes: slates_bridge_nfs::procedures::MAX_TRANSFER.min(MACOS_TRANSFER_BYTES),
    mnt_flags: mnt_flags_word,
    mnt_from: format!("slates:/{name}"),
    path: vec![name.to_owned()],
  }
  .encode();
  let kind = std::ffi::CString::new("nfs").map_err(|_| refused(None))?;
  let dir = std::ffi::CString::new(path).map_err(|_| refused(None))?;
  let mut data = args;
  // SAFETY: `kind` and `dir` are NUL-terminated strings that outlive the call; `data` is the XDR argument buffer
  // `mount(2)` reads for the "nfs" type (its length is encoded in its second word, which the kernel checks against its
  // own bound before copying it in), alive for the call.
  let result = unsafe {
    libc::mount(
      kind.as_ptr(),
      dir.as_ptr(),
      mnt_flags,
      data.as_mut_ptr().cast::<libc::c_void>(),
    )
  };
  if result != 0 {
    // The refusal names the mount point, never the capability token (a secret).
    return Err(refused(std::io::Error::last_os_error().raw_os_error()));
  }
  Ok(())
}

/// Mounts the volume with `mount_nfs` (the BSDs other than macOS, where the XDR arguments are not Apple's).
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
fn mount_volume(
  port: u16,
  name: &str,
  capability: MountCapability,
  path: &str,
  read_only: bool,
) -> Result<(), Error> {
  let args = mount_args(port, name, capability, path, read_only);
  let status = Command::new("mount_nfs")
    .args(&args)
    .status()
    .map_err(|error| {
      Error::Mount(MountFault::Refused {
        path: path.to_owned(),
        errno: error.raw_os_error(),
      })
    })?;
  if !status.success() {
    // The refusal names the mount point, never the capability token (a secret).
    return Err(Error::Mount(MountFault::Refused {
      path: path.to_owned(),
      errno: None,
    }));
  }
  Ok(())
}

/// Unmounts the slates mount at `path`, the lifecycle counterpart of a mount (sylk's cgofuse `Close`), with no daemon
/// and no privilege: a user unmounts a mount they made, so a stale mount is removable after its daemon has gone. macOS
/// and the BSDs call `unmount(2)` itself (A-507: until 2026-11-18 this ran `umount` found on the `PATH`, so a host
/// whose `PATH` lacked `/sbin` could not unmount at all, g264's kiln runner). Linux runs `fusermount3 -u`, the setuid
/// helper its FUSE mount was made through, since `umount2(2)` needs `CAP_SYS_ADMIN` (the daemon's serve task then sees
/// the kernel's disconnect and ends the attachment, AUD-29-64). Either answer is judged by the kernel's mount table:
/// nothing mounted at `path` is `NotMounted` before any call, and an unmount is done only once the table no longer
/// lists the mount. A WinFsp drive is the daemon's own and is unmounted by detaching it ([`Instance::unmount`]), so
/// this is the Unix hosts' alone.
#[cfg(not(windows))]
pub fn unmount(path: &str) -> Result<(), MountFault> {
  unmount_as(path, Unmounting::Plain)
}

/// How [`unmount_busy`] let go of a mount processes still used: the OS's way, by which a host knows what its leftover
/// processes meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LetGo {
  /// Taken out of the namespace now; its users read and write on until they close, and the unmount finishes then
  /// (Linux: `fusermount3 -u -z`, the kernel's `MNT_DETACH`).
  Detached,
  /// Unmounted now; its users' next calls on it fail (macOS and the BSDs: `unmount(2)` with `MNT_FORCE`). The loopback
  /// NFS mount is `hard`, so this is the only way off it for a holder whose daemon has gone.
  Forced,
}

/// Lets go of the mount at `path` even while processes still use it: what a host does for an ended agent whose
/// leftover process still holds a file there, where [`unmount`] is refused `Busy`. Linux detaches it (`fusermount3 -u
/// -z`); macOS and the BSDs have no detaching unmount and force it (`unmount(2)` with `MNT_FORCE`); the answer says
/// which ([`LetGo`]). Like [`unmount`] it needs no daemon and no privilege, and is judged by the kernel's mount table.
/// Until 2026-11-18 this was Linux's `unmount_lazy`, refused `Unsupported` elsewhere, so a macOS host had no library
/// route off a busy mount (found by g076a, whose conformance harness forces it).
#[cfg(not(windows))]
pub fn unmount_busy(path: &str) -> Result<LetGo, MountFault> {
  unmount_as(path, Unmounting::LetGo).map(|()| BUSY_UNMOUNT)
}

/// Which unmount is asked.
#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unmounting {
  /// Refused while a process holds a file on the mount.
  Plain,
  /// Lets go of the mount even then ([`LetGo`]).
  LetGo,
}

/// An unmount as the OS is asked it: the mount point as the call takes it, and whether it lets go of a busy mount.
#[cfg(not(windows))]
#[derive(Debug, PartialEq, Eq)]
struct UnmountRequest {
  /// The mount point, NUL-terminated.
  point: std::ffi::CString,
  /// Whether it lets go of a busy mount: `MNT_FORCE` on macOS and the BSDs; `-z`, the kernel's `MNT_DETACH`, through
  /// `fusermount3` on Linux.
  let_go: bool,
}

/// The request for `path`, or `MountPoint` when no OS call can name it (a path holding a NUL byte).
#[cfg(not(windows))]
fn unmount_request(path: &str, unmounting: Unmounting) -> Result<UnmountRequest, MountFault> {
  let point = std::ffi::CString::new(path).map_err(|_| MountFault::MountPoint {
    path: path.to_owned(),
    errno: None,
  })?;
  Ok(UnmountRequest {
    point,
    let_go: unmounting == Unmounting::LetGo,
  })
}

/// `unmount(2)`'s flags for a request: `MNT_FORCE` ("forcibly unmounted even if files are still active") to let go of
/// a busy mount, none otherwise.
#[cfg(all(unix, any(not(target_os = "linux"), test)))]
fn unmount_flags(request: &UnmountRequest) -> libc::c_int {
  if request.let_go { libc::MNT_FORCE } else { 0 }
}

/// Unmounts `path` as `unmounting` asks, judged by the kernel's mount table (A-507): the mount the table lists at
/// `path` before the call must be gone from it after. With nothing mounted there the answer is `NotMounted` and no call
/// is made; where the platform has no table to ask (the BSDs other than macOS), the call's own answer stands.
#[cfg(not(windows))]
fn unmount_as(path: &str, unmounting: Unmounting) -> Result<(), MountFault> {
  let request = unmount_request(path, unmounting)?;
  let point = recorded_point(path)?;
  let before = mount_table_for(path)?;
  let mounted = match &before {
    Some(table) => match mount_at(table, &point) {
      Some(entry) => Some(entry.identity),
      None => {
        return Err(MountFault::NotMounted {
          path: path.to_owned(),
          unreported: Vec::new(),
        });
      }
    },
    None => None,
  };
  let answer = ask_unmount(path, &request);
  let after = mount_table_for(path)?;
  judged(path, mounted, after.as_deref(), answer)
}

/// The mount point as the kernel's table records it, worked out without touching the mount (a lookup on a `hard` NFS
/// mount whose daemon has gone waits forever): the real path of its parent directory with its own name, so macOS's
/// `/var` reads as the table's `/private/var`. `MountPoint` when `path` names no entry or its parent cannot be
/// resolved.
#[cfg(not(windows))]
fn recorded_point(path: &str) -> Result<String, MountFault> {
  let refused = |errno: Option<i32>| MountFault::MountPoint {
    path: path.to_owned(),
    errno,
  };
  let given = std::path::Path::new(path);
  let name = given.file_name().ok_or_else(|| refused(None))?;
  let parent = given
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .unwrap_or_else(|| std::path::Path::new("."));
  let parent = std::fs::canonicalize(parent).map_err(|error| refused(error.raw_os_error()))?;
  Ok(parent.join(name).to_string_lossy().into_owned())
}

/// The kernel's mount table (`slates_bridge_oci::mount_table`: `getfsstat(MNT_NOWAIT)` on macOS, `/proc/self/mountinfo`
/// on Linux, neither touching a mount), `None` where the platform has no query, or `MountTable` when the kernel refused
/// it.
#[cfg(not(windows))]
fn mount_table_for(path: &str) -> Result<Option<Vec<MountEntry>>, MountFault> {
  match slates_bridge_oci::mount_table::mount_table() {
    Ok(table) => Ok(Some(table)),
    Err(MountTableError::Unsupported { .. }) => Ok(None),
    Err(MountTableError::Query { errno }) => Err(MountFault::MountTable {
      path: path.to_owned(),
      errno: Some(errno),
    }),
    Err(_) => Err(MountFault::MountTable {
      path: path.to_owned(),
      errno: None,
    }),
  }
}

/// The mount the table lists at `point` that an unmount takes: the last listed there, since a later mount at one path
/// shadows an earlier one.
#[cfg(not(windows))]
fn mount_at<'table>(table: &'table [MountEntry], point: &str) -> Option<&'table MountEntry> {
  table.iter().rev().find(|entry| entry.mount_point == point)
}

/// An unmount's answer judged by the table read after it (A-507; kiln judges its own so): the mount gone from the
/// table is done, whatever the call said; still listed, it is the call's refusal, or `StillMounted` when the call said
/// done. With no table on the platform (`mounted` or `after` is `None`) the call's own answer stands.
#[cfg(not(windows))]
fn judged(
  path: &str,
  mounted: Option<MountIdentity>,
  after: Option<&[MountEntry]>,
  answer: Result<(), MountFault>,
) -> Result<(), MountFault> {
  match (mounted, after) {
    (Some(identity), Some(table)) => {
      if table.iter().any(|entry| entry.identity == identity) {
        answer.and(Err(MountFault::StillMounted {
          path: path.to_owned(),
        }))
      } else {
        Ok(())
      }
    }
    _ => answer,
  }
}

/// The kernel's refusal of an unmount typed by its error number (`unmount(2)`): `EBUSY` ("a process is holding a
/// reference to a file located on the filesystem") is `Busy`, `EINVAL` ("the requested directory is not in the mount
/// table") is `NotMounted`, `ENOENT` and `ENOTDIR` (no such directory) are `MountPoint`, and every other is `Unmount`
/// with its number.
#[cfg(all(unix, any(not(target_os = "linux"), test)))]
fn refusal_of_errno(path: &str, errno: Option<i32>) -> MountFault {
  let path = path.to_owned();
  match errno {
    Some(libc::EBUSY) => MountFault::Busy { path },
    Some(libc::EINVAL) => MountFault::NotMounted {
      path,
      unreported: Vec::new(),
    },
    Some(libc::ENOENT | libc::ENOTDIR) => MountFault::MountPoint { path, errno },
    errno => MountFault::Unmount {
      path,
      status: None,
      errno,
    },
  }
}

/// Asks the kernel to unmount (macOS and the BSDs, `unmount(2)`): no program and no `PATH` (A-507), and no privilege,
/// since only "the caller is not the super-user, and the mount() was not done by the user" is refused `EPERM`.
#[cfg(all(unix, not(target_os = "linux")))]
fn ask_unmount(path: &str, request: &UnmountRequest) -> Result<(), MountFault> {
  // SAFETY: `request.point` is a NUL-terminated string that outlives the call, the only pointer `unmount(2)` reads,
  // and the flags are `unmount(2)`'s own.
  let result = unsafe { libc::unmount(request.point.as_ptr(), unmount_flags(request)) };
  if result == 0 {
    Ok(())
  } else {
    Err(refusal_of_errno(
      path,
      std::io::Error::last_os_error().raw_os_error(),
    ))
  }
}

/// Asks `fusermount3 -u` (with `-z`, the kernel's `MNT_DETACH`, to let go of a busy mount), the setuid helper the FUSE
/// mount was made through: `umount2(2)` "requires appropriate privilege (Linux: the `CAP_SYS_ADMIN` capability)", so an
/// unprivileged caller's own call is refused `EPERM`. Its answer is its exit, or the error number of starting it.
#[cfg(target_os = "linux")]
fn ask_unmount(path: &str, request: &UnmountRequest) -> Result<(), MountFault> {
  use std::os::unix::ffi::OsStrExt;
  let args = if request.let_go {
    BUSY_UNMOUNT_ARGS
  } else {
    UNMOUNT_ARGS
  };
  let status = Command::new(UNMOUNT_PROGRAM)
    .args(args)
    .arg(std::ffi::OsStr::from_bytes(request.point.as_bytes()))
    .status()
    .map_err(|error| MountFault::Unmount {
      path: path.to_owned(),
      status: None,
      errno: error.raw_os_error(),
    })?;
  if status.success() {
    Ok(())
  } else {
    Err(MountFault::Unmount {
      path: path.to_owned(),
      status: status.code(),
      errno: None,
    })
  }
}

/// Format: the helper that removes a slates mount on Linux without a privilege (R10): every slates mount there is FUSE,
/// made and removed by the OS's `fusermount3`.
#[cfg(target_os = "linux")]
const UNMOUNT_PROGRAM: &str = "fusermount3";
/// Format: `fusermount3`'s unmount (`-u`).
#[cfg(target_os = "linux")]
const UNMOUNT_ARGS: &[&str] = &["-u"];
/// Format: `fusermount3`'s lazy unmount (`-z`: the kernel's `MNT_DETACH`), for [`unmount_busy`].
#[cfg(target_os = "linux")]
const BUSY_UNMOUNT_ARGS: &[&str] = &["-u", "-z"];
/// Format: how [`unmount_busy`] lets go on Linux.
#[cfg(target_os = "linux")]
const BUSY_UNMOUNT: LetGo = LetGo::Detached;
/// Format: how [`unmount_busy`] lets go on macOS and the BSDs (`unmount(2)` with `MNT_FORCE`).
#[cfg(all(unix, not(target_os = "linux")))]
const BUSY_UNMOUNT: LetGo = LetGo::Forced;

#[cfg(test)]
mod tests {
  use super::{
    MountBackend, MountKind, ROOT_UID, capture_open, classify, export_path, fuse_available,
    loopback_available, mount_args, nfs_available, winfsp_available,
  };
  use crate::error::{MountFault, MountMissing};

  /// §4.6 A-34 — a read-only mount asks the kernel for `rdonly` (with the daemon refusing writes under the read-only
  /// capability too); a writable mount does not.
  #[test]
  fn a_read_only_mount_asks_the_kernel_for_rdonly() {
    let writable = mount_args(
      54321,
      "myproject",
      (0x1f, [0xab; 16]),
      "/Users/me/mnt",
      false,
    );
    assert!(
      !writable[1].contains("rdonly"),
      "a writable mount is not read-only: {}",
      writable[1]
    );
    let read_only = mount_args(
      54321,
      "myproject",
      (0x1f, [0xab; 16]),
      "/Users/me/mnt",
      true,
    );
    assert!(
      read_only[1].ends_with(",rdonly"),
      "a read-only mount asks the kernel for rdonly: {}",
      read_only[1]
    );
  }

  /// §4.6 — the mount arguments carry the daemon's loopback port for both NFS and MOUNT, the unprivileged
  /// `noresvport`, NFSv3, the derived attribute-cache timeout, and the export named by the volume's provisioned name
  /// at the chosen path — the command a BSD mount runs.
  #[test]
  fn the_mount_arguments_target_the_daemon_port_and_the_named_export() {
    let args = mount_args(
      54321,
      "myproject",
      (0x1f, [0xab; 16]),
      "/Users/me/mnt",
      false,
    );
    assert_eq!(args[0], "-o");
    let options = &args[1];
    assert!(options.contains("port=54321"), "the NFS port: {options}");
    assert!(
      options.contains("mountport=54321"),
      "the MOUNT port, same server: {options}"
    );
    assert!(
      options.contains("noresvport"),
      "the unprivileged mount, R10: {options}"
    );
    assert!(options.contains("vers=3"), "NFSv3: {options}");
    assert!(
      options.contains("actimeo=1"),
      "the derived attribute-cache timeout: {options}"
    );
    assert_eq!(
      args[2],
      format!("localhost:/myproject@1f.{}", "ab".repeat(16)),
      "the export is the volume's provisioned name with its mount capability (AUD-01)"
    );
    assert_eq!(args[3], "/Users/me/mnt", "the chosen mount point");
  }

  /// §4.13 AUD-01 — the export path a capability mount presents is the volume's name, then the attachment in hex and
  /// its token in hex: exactly what the daemon's MOUNT service parses.
  #[test]
  fn an_export_path_names_the_volume_its_attachment_and_its_token() {
    assert_eq!(
      export_path("work", (0xabc, [0x01; 16])),
      format!("/work@abc.{}", "01".repeat(16))
    );
  }

  /// §4.6 — the capability detection is pure: `mount_nfs` present is the NFS loopback backend, its absence is
  /// unsupported — tested with an injected predicate, so it runs on any host with no live mount (the property sylk's
  /// predicate-injected FUSE-backend detection has).
  #[test]
  fn classify_finds_the_nfs_backend_only_when_mount_nfs_is_present() {
    assert_eq!(
      classify(|cmd| cmd == "mount_nfs"),
      MountBackend::NfsLoopback,
      "mount_nfs on the PATH is the NFS loopback backend"
    );
    assert_eq!(
      classify(|_| false),
      MountBackend::Unsupported,
      "no mount_nfs is unsupported"
    );
    assert_eq!(
      classify(|cmd| cmd == "mount"),
      MountBackend::Unsupported,
      "another mount tool is not mount_nfs"
    );
  }

  /// §4.6, A-278 — what a host reports before it mounts: FUSE is available only with both `fusermount3` on the `PATH`
  /// and a usable `/dev/fuse`, and each missing piece is refused by its own reason; a BSD's loopback mount follows its
  /// `mount_nfs`. Pure, so every case runs on any host.
  #[test]
  fn a_mount_is_available_only_when_every_piece_it_needs_is_there() {
    assert_eq!(fuse_available(true, true), Ok(MountKind::Fuse));
    assert_eq!(
      fuse_available(false, true),
      Err(MountFault::Unsupported {
        missing: MountMissing::Fusermount3
      })
    );
    assert_eq!(
      fuse_available(false, false),
      Err(MountFault::Unsupported {
        missing: MountMissing::Fusermount3
      }),
      "the helper is named first: without it the device is never opened"
    );
    assert_eq!(
      fuse_available(true, false),
      Err(MountFault::Unsupported {
        missing: MountMissing::FuseDevice
      })
    );
    assert_eq!(
      nfs_available(MountBackend::NfsLoopback),
      Ok(MountKind::NfsLoopback)
    );
    assert_eq!(
      nfs_available(MountBackend::Unsupported),
      Err(MountFault::Unsupported {
        missing: MountMissing::MountNfs
      })
    );
  }

  /// §4.6, A-369, A-417 — a Windows host is told it can mount only when the mount would be made: WinFsp's library loads
  /// and the machine lets WinFsp's driver make the drive (`MountUseMountmgrFromFSD`). Do: each pair of the two facts.
  /// Expect `WinFsp` with both, the library named first without it (no drive is asked for then), and the Mount Manager
  /// setting named when only it is missing. Before A-417 the second fact was not asked, so a machine without the
  /// setting was told it could mount and each mount was refused `MountManagerNotAllowed` after its volume was made.
  /// Pure, so every case runs on any host.
  #[test]
  fn a_windows_mount_is_available_only_where_winfsps_driver_may_make_its_drive() {
    assert_eq!(winfsp_available(true, true), Ok(MountKind::WinFsp));
    assert_eq!(
      winfsp_available(true, false),
      Err(MountFault::Unsupported {
        missing: MountMissing::MountManager
      })
    );
    assert_eq!(
      winfsp_available(false, true),
      Err(MountFault::Unsupported {
        missing: MountMissing::WinFsp
      })
    );
    assert_eq!(
      winfsp_available(false, false),
      Err(MountFault::Unsupported {
        missing: MountMissing::WinFsp
      }),
      "the library is named first: without it no drive is ever asked for"
    );
    let words = MountMissing::MountManager.to_string();
    assert!(
      words.contains("MountUseMountmgrFromFSD") && words.contains("REG_DWORD /d 1"),
      "the refusal names the one command that allows it: {words}"
    );
  }

  /// §4.13, A-535 — a macOS or BSD host is told it can mount only where no one but root can capture its loopback: a
  /// mount's capability rides every file handle in the clear, which the kernel's NFS client cannot encrypt. Do: judge BPF
  /// devices as macOS leaves them (root's, read by root alone), as Wireshark's ChmodBPF leaves one (read by its group),
  /// read by anyone, owned and read by a user, and none at all; then ask the loopback's availability under each answer.
  /// Expect capture open exactly where a group, anyone or a user other than root may read a device; the mount offered
  /// where capture is closed and refused `PrivateLoopback` where it is open, its words naming the devices and the fix.
  /// Before A-535 every macOS host was told it could mount, and a group granted capture read each mount's capability
  /// off loopback. Pure, so every case runs on any host.
  #[test]
  fn a_loopback_mount_is_offered_only_where_no_one_but_root_can_capture_loopback() {
    /// Format: a character device's file-type bits (`S_IFCHR`, `<sys/stat.h>`).
    const CHAR_DEVICE: u32 = 0o020_000;
    /// Shape: a user other than root (macOS's first account's id).
    const USER: u32 = 501;
    let root_only = (CHAR_DEVICE | 0o600, ROOT_UID);
    assert!(
      !capture_open([root_only, root_only]),
      "macOS's default: each device root's alone"
    );
    assert!(
      capture_open([root_only, (CHAR_DEVICE | 0o660, ROOT_UID)]),
      "one device its group may read, as ChmodBPF leaves them"
    );
    assert!(
      capture_open([(CHAR_DEVICE | 0o604, ROOT_UID)]),
      "a device anyone may read"
    );
    assert!(
      capture_open([(CHAR_DEVICE | 0o600, USER)]),
      "a device a user owns and may read"
    );
    assert!(!capture_open([]), "no device: nothing captures");
    assert_eq!(loopback_available(false), Ok(MountKind::NfsLoopback));
    assert_eq!(
      loopback_available(true),
      Err(MountFault::Unsupported {
        missing: MountMissing::PrivateLoopback
      })
    );
    let words = MountMissing::PrivateLoopback.to_string();
    assert!(
      words.contains("/dev/bpf") && words.contains("chmod 600"),
      "the refusal names the devices and the fix: {words}"
    );
  }

  /// §4.6 A-34, A-278, A-535 — on a Mac whose loopback no one but root can capture (macOS's default) the loopback mount
  /// is available: `mount(2)` is built in, so a host's fact is `NfsLoopback` with nothing to install.
  #[cfg(target_os = "macos")]
  #[test]
  fn macos_always_offers_its_loopback_mount() {
    assert_eq!(super::available(), Ok(MountKind::NfsLoopback));
  }

  /// A-507 (g264's kiln runner, whose `PATH` lacked `/sbin`): an unmount is judged by the kernel's mount table, and a
  /// path the table lists no mount at is refused before any call, on every Unix host, with no program run and no `PATH`
  /// read. Do: unmount, and let go of, this crate's own directory, never a mount. Expect both refused `NotMounted`
  /// naming the path. Until A-507 macOS ran `umount` found on the `PATH` and answered its exit status, or the error of
  /// starting it where the `PATH` lacked `/sbin`.
  #[cfg(unix)]
  #[test]
  fn an_unmount_of_a_path_where_nothing_is_mounted_is_refused_not_mounted() {
    let path = env!("CARGO_MANIFEST_DIR");
    let not_mounted = MountFault::NotMounted {
      path: path.to_owned(),
      unreported: Vec::new(),
    };
    assert_eq!(super::unmount(path), Err(not_mounted.clone()));
    assert_eq!(super::unmount_busy(path), Err(not_mounted));
  }

  /// A-507: do build the request a plain unmount and a busy one make; expect the mount point passed whole as the call
  /// takes it, the plain unmount asking no force and the busy one `unmount(2)`'s `MNT_FORCE` (`-z` through
  /// `fusermount3` on Linux), and a path no call can name (a NUL byte in it) refused `MountPoint`, never a panic.
  #[cfg(unix)]
  #[test]
  fn an_unmount_is_asked_as_the_os_takes_it() {
    let plain = super::unmount_request("/Volumes/work", super::Unmounting::Plain).unwrap();
    assert_eq!(plain.point.as_bytes(), b"/Volumes/work");
    assert!(!plain.let_go);
    assert_eq!(super::unmount_flags(&plain) & libc::MNT_FORCE, 0);
    let busy = super::unmount_request("/Volumes/work", super::Unmounting::LetGo).unwrap();
    assert!(busy.let_go);
    assert_eq!(super::unmount_flags(&busy), libc::MNT_FORCE);
    assert_eq!(
      super::unmount_request("/Volumes/a\0b", super::Unmounting::Plain),
      Err(MountFault::MountPoint {
        path: "/Volumes/a\0b".to_owned(),
        errno: None
      })
    );
  }

  /// A-507: do map each error number `unmount(2)` documents; expect `EBUSY` refused `Busy` (a process holds a file on
  /// the mount, `unmount_busy` lets go anyway), `EINVAL` `NotMounted` (not in the mount table), `ENOENT` and `ENOTDIR`
  /// `MountPoint` with the number, and every other number, or none, `Unmount` with it.
  #[cfg(unix)]
  #[test]
  fn the_kernels_unmount_refusals_are_typed() {
    let path = "/Volumes/work";
    assert_eq!(
      super::refusal_of_errno(path, Some(libc::EBUSY)),
      MountFault::Busy {
        path: path.to_owned()
      }
    );
    assert!(
      super::refusal_of_errno(path, Some(libc::EBUSY))
        .to_string()
        .contains("unmount_busy"),
      "a busy refusal names the way to let go"
    );
    assert_eq!(
      super::refusal_of_errno(path, Some(libc::EINVAL)),
      MountFault::NotMounted {
        path: path.to_owned(),
        unreported: Vec::new()
      }
    );
    for errno in [libc::ENOENT, libc::ENOTDIR] {
      assert_eq!(
        super::refusal_of_errno(path, Some(errno)),
        MountFault::MountPoint {
          path: path.to_owned(),
          errno: Some(errno)
        }
      );
    }
    for errno in [Some(libc::EPERM), Some(libc::EIO), None] {
      assert_eq!(
        super::refusal_of_errno(path, errno),
        MountFault::Unmount {
          path: path.to_owned(),
          status: None,
          errno
        }
      );
    }
  }

  /// A-507 (kiln's own rule since g264's 68eda5b14): do judge unmount answers against the table read after them;
  /// expect the mount gone from the table done whatever the call said, the mount still listed the call's refusal or
  /// `StillMounted` when the call said done, and with no table on the platform the call's own answer. The mount an
  /// unmount takes is the last the table lists at its point, a later mount shadowing an earlier one.
  #[cfg(unix)]
  #[test]
  fn an_unmount_is_believed_only_when_the_table_agrees() {
    use slates_bridge_oci::mount_table::{MountEntry, MountIdentity};
    let entry = |mount: u64, point: &str| MountEntry {
      identity: MountIdentity { mount, device: 7 },
      mount_point: point.to_owned(),
      fstype: "nfs".to_owned(),
      source: "slates:/work".to_owned(),
    };
    let path = "/Volumes/work";
    let before = [entry(1, "/"), entry(2, path), entry(3, path)];
    let taken = super::mount_at(&before, path).map(|mounted| mounted.identity);
    assert_eq!(
      taken,
      Some(MountIdentity {
        mount: 3,
        device: 7
      }),
      "the shadowing mount is taken"
    );
    assert_eq!(super::mount_at(&before, "/Volumes/other"), None);
    let busy = || {
      Err(MountFault::Busy {
        path: path.to_owned(),
      })
    };
    let gone = [entry(1, "/"), entry(2, path)];
    assert_eq!(super::judged(path, taken, Some(&gone), Ok(())), Ok(()));
    assert_eq!(
      super::judged(path, taken, Some(&gone), busy()),
      Ok(()),
      "the table, not the call"
    );
    assert_eq!(
      super::judged(path, taken, Some(&before), Ok(())),
      Err(MountFault::StillMounted {
        path: path.to_owned()
      })
    );
    assert_eq!(super::judged(path, taken, Some(&before), busy()), busy());
    assert_eq!(
      super::judged(path, None, None, busy()),
      busy(),
      "no table: the call's answer"
    );
    assert_eq!(super::judged(path, None, None, Ok(())), Ok(()));
  }
}
