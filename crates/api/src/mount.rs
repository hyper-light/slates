//! Serving a volume as a host mount (§4.6): the volume at a path every process of the host sees, through the
//! platform's own mechanism and no privilege (R10).
//!
//! - **macOS**: the loopback NFS bridge, `mount(2)` with the export's root handle fetched over this process's own
//!   loopback socket, so the mount capability never appears in `ps`, `mount` or `nfsstat -m` (§4.6 A-34). No kernel
//!   extension, no FUSE library and no Apple entitlement: slates serves its own NFS.
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

#[cfg(not(windows))]
use std::process::Command;

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
  /// detached.
  pub fn unmount(&mut self, path: &str) -> Result<(), Error> {
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
  fn unmount_platform(&mut self, path: &str) -> Result<(), Error> {
    unmount(path).map_err(Error::Mount)
  }

  /// See [`Instance::unmount`]: the WinFsp drive at `path` is the daemon's own, so it is unmounted by detaching its
  /// attachment, found among the volumes' reported mounts; the daemon then removes the drive. Every volume is asked:
  /// one whose status is refused (a volume recovery could not rebuild answers `NotFound`) cannot report its mounts, so
  /// it is passed over and named in the refusal, and the drive is still released when another volume reports it. A
  /// channel or daemon failure ends the scan, since no later volume can be asked either.
  #[cfg(windows)]
  fn unmount_platform(&mut self, path: &str) -> Result<(), Error> {
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
      Some(attachment) => self.detach(attachment),
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
#[cfg(target_os = "macos")]
pub fn available() -> Result<MountKind, MountFault> {
  Ok(MountKind::NfsLoopback)
}

/// See the macOS arm: the other BSDs need `mount_nfs` on the `PATH`.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
pub fn available() -> Result<MountKind, MountFault> {
  nfs_available(backend())
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

/// See the macOS arm: Windows needs WinFsp's library to load (its own `FspLoad` search).
#[cfg(windows)]
pub fn available() -> Result<MountKind, MountFault> {
  if slates_bridge_winfsp::host::installed() {
    Ok(MountKind::WinFsp)
  } else {
    Err(MountFault::Unsupported {
      missing: MountMissing::WinFsp,
    })
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

/// Derived: how long the mount waits for the daemon's MOUNT service on each of connect, send and receive: one liveness
/// budget (`slates_server::daemon::LIVENESS_BUDGET_NS`). A daemon that cannot answer one loopback call within the
/// budget the anchor gives it to beat is not serving. macOS only, as the `mount(2)` path that makes the call is.
#[cfg(target_os = "macos")]
const MOUNT_CALL_DEADLINE: std::time::Duration =
  std::time::Duration::from_nanos(slates_server::daemon::LIVENESS_BUDGET_NS);

/// Mounts the volume on macOS without the capability leaving this process (§4.6 A-34): the root handle comes from the
/// daemon's MOUNT service over this process's own loopback socket, and goes to the kernel in `mount(2)`'s XDR
/// arguments (`NFS_MATTR_FH`), with a source name that holds no secret (`slates:/<name>`). Nothing another user can
/// read (`ps`, the mount table) carries the token.
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
  let handle = fetch_root_handle(
    port,
    &caller,
    &export_path(name, capability),
    MOUNT_CALL_DEADLINE,
  )
  .map_err(|error| {
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

/// Unmounts the slates mount at `path` with the OS's own program, the lifecycle counterpart of a mount (sylk's cgofuse
/// `Close`): `fusermount3 -u` on Linux (the daemon's serve task then sees the kernel's disconnect and ends the
/// attachment, AUD-29-64), the user's own `umount` elsewhere. No privilege: a user unmounts a mount they made. It needs
/// no daemon, so a stale mount is removable after its daemon has gone. A WinFsp drive is the daemon's own and is
/// unmounted by detaching it ([`Instance::unmount`]), so this is the Unix hosts' alone.
#[cfg(not(windows))]
pub fn unmount(path: &str) -> Result<(), MountFault> {
  run_unmount(UNMOUNT_ARGS, path)
}

/// Lets go of the mount at `path` even while processes still use it (Linux: `fusermount3 -u -z`): the kernel takes it
/// out of the namespace now and finishes the unmount once its last user closes — what a host does for an ended agent
/// whose leftover process still holds a file there, where [`unmount`] is refused busy. Like [`unmount`] it needs no
/// daemon and no privilege. Elsewhere refused [`MountFault::Unsupported`]: macOS and the BSDs have no lazy unmount.
#[cfg(target_os = "linux")]
pub fn unmount_lazy(path: &str) -> Result<(), MountFault> {
  run_unmount(LAZY_UNMOUNT_ARGS, path)
}

/// See the Linux arm: no lazy unmount here.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn unmount_lazy(_path: &str) -> Result<(), MountFault> {
  Err(MountFault::Unsupported {
    missing: MountMissing::LazyUnmount,
  })
}

/// Runs [`UNMOUNT_PROGRAM`] with `args` on `path`: its exit status, or the error number of starting it, typed.
#[cfg(not(windows))]
fn run_unmount(args: &[&str], path: &str) -> Result<(), MountFault> {
  let status = Command::new(UNMOUNT_PROGRAM)
    .args(args)
    .arg(path)
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

/// Format: the program that removes a slates mount without a privilege (R10): on Linux every slates mount is FUSE,
/// removed by the OS's `fusermount3 -u`; elsewhere the user's own `umount` of the loopback NFS mount.
#[cfg(target_os = "linux")]
const UNMOUNT_PROGRAM: &str = "fusermount3";
/// Format: see [`UNMOUNT_PROGRAM`].
#[cfg(target_os = "linux")]
const UNMOUNT_ARGS: &[&str] = &["-u"];
/// Format: `fusermount3`'s lazy unmount (`-z`: the kernel's `MNT_DETACH`), for [`unmount_lazy`].
#[cfg(target_os = "linux")]
const LAZY_UNMOUNT_ARGS: &[&str] = &["-u", "-z"];
/// Format: see the Linux form.
#[cfg(all(unix, not(target_os = "linux")))]
const UNMOUNT_PROGRAM: &str = "umount";
/// Format: see the Linux form.
#[cfg(all(unix, not(target_os = "linux")))]
const UNMOUNT_ARGS: &[&str] = &[];

#[cfg(test)]
mod tests {
  use super::{
    MountBackend, MountKind, classify, export_path, fuse_available, mount_args, nfs_available,
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

  /// §4.6 A-34, A-278 — on macOS the loopback mount is always available: `mount(2)` is built in, so a host's fact is
  /// `NfsLoopback` with nothing to install.
  #[cfg(target_os = "macos")]
  #[test]
  fn macos_always_offers_its_loopback_mount() {
    assert_eq!(super::available(), Ok(MountKind::NfsLoopback));
  }

  /// A-278 — a lazy unmount is Linux's: elsewhere it is refused `Unsupported` before any program runs.
  #[cfg(all(unix, not(target_os = "linux")))]
  #[test]
  fn a_lazy_unmount_is_refused_where_the_os_has_none() {
    assert!(matches!(
      super::unmount_lazy(env!("CARGO_MANIFEST_DIR")),
      Err(MountFault::Unsupported { .. })
    ));
  }

  /// A-278 — a lazy unmount runs `fusermount3 -u -z`, which refuses a path where nothing is mounted: the refusal comes
  /// back typed with the program's exit status, and nothing changes (this crate's own directory is never a mount).
  /// Skips loudly where `fusermount3` is not installed.
  #[cfg(target_os = "linux")]
  #[test]
  fn a_lazy_unmount_of_a_path_where_nothing_is_mounted_is_refused_with_the_programs_status() {
    if !super::command_on_path("fusermount3") {
      eprintln!("SKIP: fusermount3 is not on the PATH");
      return;
    }
    let path = env!("CARGO_MANIFEST_DIR");
    assert!(matches!(
      super::unmount_lazy(path),
      Err(MountFault::Unmount { path: refused, status: Some(_), errno: None }) if refused == path
    ));
  }
}
