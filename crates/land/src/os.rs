//! The write seam over the operating system (§4.15 step 6, "Exchange fallback"; D-26): the
//! only code in the workspace that creates, writes, links, exchanges, renames or removes a host
//! path. It runs inside a granted landing only; the engine calls nothing here before the grant
//! and the lease.
//!
//! Shape: every verb is relative to a descriptor the base host holds (`OsHost`, which resolves
//! the one root path with `O_NOFOLLOW` and hands out handles); no verb takes a path string.
//! A batch's barrier is a job over its own descriptors of the batch's files (`dup`), run on a thread of the landing's
//! and never on the shard that owns it (A-132): it waits on the device and on every other writer of the filesystem.
//! Linux: `O_TMPFILE` temporaries linked by `linkat` through `/proc/self/fd`, each file's writeback
//! started as it is written out (`sync_file_range`) and a batch's barrier as its files' `fdatasync`s
//! on ⌈√n⌉ streams (A-130: journal group commit merges those in flight into few commits and flushes),
//! `renameat2(RENAME_EXCHANGE)`, `fsync` on directories. macOS: hidden-name
//! temporaries (`O_CREAT|O_EXCL`), a batch's barrier as each file's `fsync` and then one `fcntl(F_BARRIERFSYNC)` per
//! device, `renameatx_np(RENAME_SWAP)`, `fcntl(F_FULLFSYNC)` on the target as the
//! media barrier when the grant asked for it. Other unixes: a batch's barrier as each file's `fdatasync`. Linux's one
//! call that covers a batch, `syncfs`, flushes every other writer's
//! dirty data too: beside one busy writer a barrier took up to 3.7 s and a 16-file batch cost 4.0–7.8 ms
//! a file against `fdatasync`'s 0.65–0.79 ms (2026-10-10, `BENCHMARKS.md`), so it is not used. Both:
//! `fchmod` (on files and on each made directory, so the umask never edits a recorded mode),
//! `futimens`, `unlinkat`, `mkdirat`, `symlinkat`, `renameat`. The four `unsafe`
//! sites are the libc calls rustix does not wrap (macOS `F_BARRIERFSYNC`, `F_FULLFSYNC`,
//! `renameatx_np`; Linux `sync_file_range`), each on a descriptor this host owns.
//!
//! Containment (§4.15 step 4, §4.13): [`OsLand::open_target`] opens the target path one
//! component at a time with `O_DIRECTORY|O_NOFOLLOW`; an unfollowed symlink is classified
//! as `EscapesTarget` even when the OS reports `ENOTDIR`. `..` is refused; the target must be owned by the effective
//! user (`TargetNotOwned`). `TargetIsVolume` waits on the mount table of Phase 3 (GAPS §8c).

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path};

use rustix::fs::{AtFlags, Mode, OFlags};
use slates_base::OsHost;
use slates_vfs::host::{
  Barrier, BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, LandCapabilities,
  LandFs, OsCode, WatchState,
};
use slates_vfs::inode::Fingerprint;

use crate::engine::LandingTarget;

/// Format: the mode a temporary is created with before `set_mode` applies the overlay's: owner
/// read and write only, so a half-written file is never readable by others.
const TEMP_MODE: u32 = 0o600;
/// Format: nanoseconds per second, for `futimens`.
const NS_PER_S: i64 = 1_000_000_000;
/// Format: the permission and setuid/setgid/sticky bits of a POSIX mode; the type bits above
/// them never reach `fchmod` or `mkdirat`.
const PERMISSION_MASK: u32 = 0o7777;

/// A mode's permission bits in the platform's `mode_t` width (16 bits on macOS).
fn mode_bits(mode: u32) -> Mode {
  Mode::from_bits_truncate(rustix::fs::RawMode::try_from(mode & PERMISSION_MASK).unwrap_or(0))
}

/// Why a target path was refused before the lease (§4.15 step 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetRefusal {
  /// The path is not absolute.
  NotAbsolute,
  /// A component is a symlink or `..`: the path would escape the directory the human named.
  EscapesTarget,
  /// The target is owned by another user.
  TargetNotOwned,
  /// A component lies on a mount this daemon serves (§4.6: no daemon thread opens a path on a volume the daemon
  /// serves; A-227): refused before it was entered, so the landing wrote nothing and waits on nothing of its own.
  OwnMount,
  /// The host refused.
  Unavailable(HostError),
}

/// A temporary's bookkeeping: where it was created and under which name, if any.
#[derive(Clone, Debug)]
struct Temp {
  dir: HostDir,
  created_as: Option<Box<str>>,
}

/// The operating-system writer.
pub struct OsLand {
  host: OsHost,
  temps: BTreeMap<u64, Temp>,
}

/// A writer's own state while it has lent its host away ([`OsLand::lend`]).
#[derive(Debug)]
pub struct Away {
  temps: BTreeMap<u64, Temp>,
}

impl std::fmt::Debug for OsLand {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OsLand")
      .field("host", &self.host)
      .field("temps", &self.temps.len())
      .finish()
  }
}

/// The seam's refusal for an errno (A-539): the typed refusals, then the kinds the landing tells apart, each named by
/// this platform's own numbers (`EEXIST`; `ENOSPC` and `EDQUOT`, 122 on Linux and 69 on macOS; `EOPNOTSUPP` and
/// `ENOTSUP`, one number on Linux and two on macOS), so the engine compares no number; anything else as the errno it is.
fn refusal(e: rustix::io::Errno) -> HostError {
  use rustix::io::Errno;
  let code = OsCode::Errno(e.raw_os_error());
  if e == Errno::NOENT {
    HostError::NotFound
  } else if e == Errno::NOTDIR {
    HostError::NotDirectory
  } else if e == Errno::LOOP || e == Errno::ISDIR {
    HostError::NotFile
  } else if e == Errno::EXIST {
    HostError::Exists(code)
  } else if e == Errno::NOSPC || e == Errno::DQUOT {
    HostError::NoSpace(code)
  } else if e == Errno::OPNOTSUPP || e == Errno::NOTSUP {
    HostError::Unsupported(code)
  } else {
    HostError::Unavailable(code)
  }
}

/// An exchange's refusal (A-539): a filesystem without one answers Linux's `renameat2(RENAME_EXCHANGE)` `EINVAL` and
/// macOS's `renameatx_np(RENAME_SWAP)` `ENOTSUP`, the verb it lacks; anything else as [`refusal`] reads it.
fn exchange_refusal(e: rustix::io::Errno) -> HostError {
  if e == rustix::io::Errno::INVAL {
    HostError::Unsupported(OsCode::Errno(e.raw_os_error()))
  } else {
    refusal(e)
  }
}

/// An errno the writer names itself, as the seam's refusal of anything it could not serve.
fn unavailable(e: rustix::io::Errno) -> HostError {
  HostError::Unavailable(OsCode::Errno(e.raw_os_error()))
}

/// The directory flags every containment step uses.
fn dir_flags() -> OFlags {
  OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Linux may report ENOTDIR for O_DIRECTORY|O_NOFOLLOW on a symlink. Inspect only the
/// refused component, without following it; every successful open still uses NOFOLLOW.
fn target_component_refusal(
  dir: BorrowedFd<'_>,
  name: &str,
  error: rustix::io::Errno,
) -> TargetRefusal {
  if error == rustix::io::Errno::LOOP {
    return TargetRefusal::EscapesTarget;
  }
  if error == rustix::io::Errno::NOTDIR {
    let stat = match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
      Ok(stat) => stat,
      Err(error) => return TargetRefusal::Unavailable(refusal(error)),
    };
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Symlink {
      return TargetRefusal::EscapesTarget;
    }
  }
  TargetRefusal::Unavailable(refusal(error))
}

/// Refuses a target component on one of this daemon's own mounts before the walk enters it (A-227): on Linux the
/// component is opened `O_PATH` and its mount compared; on macOS the kernel's path of `dir` with the component is
/// compared with the kernel's mount point of each mount this daemon serves (`slates_base::own_mounts`).
fn check_crossing(dir: BorrowedFd<'_>, name: &str) -> Result<(), TargetRefusal> {
  slates_base::own_mounts::check_child(dir, std::ffi::OsStr::new(name)).map_err(|error| match error
  {
    HostError::OwnMount(_) => TargetRefusal::OwnMount,
    other => TargetRefusal::Unavailable(other),
  })
}

/// Walks an absolute target path from `/` one component at a time (no `..`, no symlink: containment), checks
/// the target is owned by this process's user, and returns its descriptor and the key it lands under.
fn walk_target(path: &Path) -> Result<(OwnedFd, String), TargetRefusal> {
  let mut components = path.components();
  if components.next() != Some(Component::RootDir) {
    return Err(TargetRefusal::NotAbsolute);
  }
  let mut current = rustix::fs::open("/", dir_flags(), Mode::empty())
    .map_err(|e| TargetRefusal::Unavailable(refusal(e)))?;
  let mut key = String::new();
  for component in components {
    let name = match component {
      Component::Normal(n) => n.to_str().ok_or(TargetRefusal::EscapesTarget)?,
      Component::CurDir => continue,
      _ => return Err(TargetRefusal::EscapesTarget),
    };
    check_crossing(current.as_fd(), name)?;
    let next = rustix::fs::openat(&current, name, dir_flags(), Mode::empty())
      .map_err(|error| target_component_refusal(current.as_fd(), name, error))?;
    key.push('/');
    key.push_str(name);
    current = next;
  }
  let st = rustix::fs::fstat(&current).map_err(|e| TargetRefusal::Unavailable(refusal(e)))?;
  if st.st_uid != rustix::process::geteuid().as_raw() {
    return Err(TargetRefusal::TargetNotOwned);
  }
  Ok((current, key))
}

impl OsLand {
  /// Opens an absolute target path with containment and the ownership check, and returns the
  /// writer with the target. Ancestor descriptors used to resolve it are closed on the way.
  pub fn open_target(path: &Path) -> Result<(Self, LandingTarget), TargetRefusal> {
    let (current, key) = walk_target(path)?;
    let (host, _) = OsHost::open_root(Path::new("/")).map_err(TargetRefusal::Unavailable)?;
    Self::adopt_target(host, current, key).map_err(|handed| handed.1)
  }

  /// [`OsLand::open_target`] inside an existing host — the base host an overlay volume's slot holds — so the
  /// target and every handle the volume's base names live in one host (a handle is an index into its own
  /// host's table; another host's same number names another directory). A refusal hands the host back (boxed, with the
  /// refusal), so the volume keeps it.
  pub fn open_target_in(
    host: OsHost,
    path: &Path,
  ) -> Result<(Self, LandingTarget), Box<(OsHost, TargetRefusal)>> {
    match walk_target(path) {
      Ok((current, key)) => Self::adopt_target(host, current, key),
      Err(refusal) => Err(Box::new((host, refusal))),
    }
  }

  /// The writer over `host`, with the walked target `current` adopted into it under `key`.
  fn adopt_target(
    mut host: OsHost,
    current: OwnedFd,
    key: String,
  ) -> Result<(Self, LandingTarget), Box<(OsHost, TargetRefusal)>> {
    let dir = match host.adopt_dir(current) {
      Ok(dir) => dir,
      Err(error) => return Err(Box::new((host, TargetRefusal::Unavailable(error)))),
    };
    let target = LandingTarget {
      dir,
      key: if key.is_empty() {
        "/".into()
      } else {
        key.into()
      },
    };
    Ok((
      Self {
        host,
        temps: BTreeMap::new(),
      },
      target,
    ))
  }

  /// The base host beneath, for the volume's read verbs.
  pub fn host(&mut self) -> &mut OsHost {
    &mut self.host
  }

  /// The base host beneath, kept after the landing: a scratch volume the landing made an overlay over its
  /// target (`Overlay::land_advance`) names the target by this host's handles, so the volume must be served
  /// through this host and no other. The writer's temporaries are names only; any a crash left are its
  /// hidden siblings, which the next landing's sweep removes.
  pub fn into_host(self) -> OsHost {
    self.host
  }

  /// Lends the host back between a sliced landing's slices (AUD-29-25): an overlay volume serves its base
  /// through it while the landing waits for its next slice. The writer's own state — its temporaries, none
  /// open between units — is kept in the returned [`Away`], which [`OsLand::resume`] takes back with the host.
  pub fn lend(self) -> (OsHost, Away) {
    (self.host, Away { temps: self.temps })
  }

  /// The writer again, over the host it lent and its own state.
  pub fn resume(host: OsHost, away: Away) -> Self {
    Self {
      host,
      temps: away.temps,
    }
  }

  fn dir(&self, dir: HostDir) -> Result<BorrowedFd<'_>, HostError> {
    self.host.dir_fd(dir)
  }

  fn file(&self, file: HostFile) -> Result<BorrowedFd<'_>, HostError> {
    self.host.file_fd(file)
  }

  /// A named temporary at `name` in `dir` (`O_CREAT|O_EXCL`).
  fn create_named(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let fd = rustix::fs::openat(
      self.dir(dir)?,
      name,
      OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
      mode_bits(TEMP_MODE),
    )
    .map_err(refusal)?;
    let file = self.host.adopt_file(fd)?;
    self.temps.insert(
      file.0,
      Temp {
        dir,
        created_as: Some(name.into()),
      },
    );
    Ok(file)
  }

  /// An unnamed temporary (`O_TMPFILE`), where the filesystem offers it.
  #[cfg(target_os = "linux")]
  fn create_unnamed(&mut self, dir: HostDir) -> Result<Option<HostFile>, HostError> {
    let opened = rustix::fs::openat(
      self.dir(dir)?,
      ".",
      OFlags::WRONLY | OFlags::TMPFILE | OFlags::CLOEXEC,
      mode_bits(TEMP_MODE),
    );
    match opened {
      Ok(fd) => {
        let file = self.host.adopt_file(fd)?;
        self.temps.insert(
          file.0,
          Temp {
            dir,
            created_as: None,
          },
        );
        Ok(Some(file))
      }
      Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::ISDIR | rustix::io::Errno::INVAL) => {
        Ok(None)
      }
      Err(e) => Err(refusal(e)),
    }
  }

  #[cfg(not(target_os = "linux"))]
  fn create_unnamed(&mut self, _dir: HostDir) -> Result<Option<HostFile>, HostError> {
    Ok(None)
  }

  /// Links an unnamed temporary at `name`.
  #[cfg(target_os = "linux")]
  #[allow(clippy::disallowed_methods)] // the write seam's own placement (R1, D-26; AUD-29-31).
  fn link_unnamed(&self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    let proc_path = format!("/proc/self/fd/{}", self.file(file)?.as_raw_fd());
    rustix::fs::linkat(
      rustix::fs::CWD,
      proc_path.as_str(),
      self.dir(dir)?,
      name,
      AtFlags::SYMLINK_FOLLOW,
    )
    .map_err(refusal)
  }

  #[cfg(not(target_os = "linux"))]
  fn link_unnamed(&self, _file: HostFile, _dir: HostDir, _name: &str) -> Result<(), HostError> {
    Err(refusal(rustix::io::Errno::OPNOTSUPP))
  }
}

/// The exchange: Linux `renameat2(RENAME_EXCHANGE)`.
#[cfg(target_os = "linux")]
fn exchange_names(dir: BorrowedFd<'_>, a: &str, b: &str) -> Result<(), HostError> {
  rustix::fs::renameat_with(dir, a, dir, b, rustix::fs::RenameFlags::EXCHANGE)
    .map_err(exchange_refusal)
}

/// The exchange: macOS `renameatx_np(RENAME_SWAP)`.
#[cfg(target_os = "macos")]
fn exchange_names(dir: BorrowedFd<'_>, a: &str, b: &str) -> Result<(), HostError> {
  renameatx(dir, a, dir, b, libc::RENAME_SWAP).map_err(exchange_refusal)
}

/// A rename that never replaces (AUD-29-04): Linux `renameat2(RENAME_NOREPLACE)`, refused `EEXIST` when
/// `to` exists.
#[cfg(target_os = "linux")]
fn rename_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  rustix::fs::renameat_with(dir, from, to_dir, to, rustix::fs::RenameFlags::NOREPLACE)
    .map_err(refusal)
}

/// A rename that never replaces: macOS `renameatx_np(RENAME_EXCL)`, refused `EEXIST` when `to` exists.
#[cfg(target_os = "macos")]
fn rename_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  renameatx(dir, from, to_dir, to, libc::RENAME_EXCL).map_err(refusal)
}

/// A named temporary moved from `from` to `to`, refused `EEXIST` when `to` exists, holding the one name after: the
/// rename that never replaces, one metadata operation where a link and an unlink were two. APFS measured 46–57 µs a
/// file against 198–259 µs for the pair (2026-10-10, `BENCHMARKS.md`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn move_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  rename_without_replacing(dir, from, to_dir, to)
}

/// A named temporary moved where no rename refuses an existing name: a link (which refuses one) and the old name's
/// removal, the temporary at both names between the two; the sweep removes a hidden one a crash leaves.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(clippy::disallowed_methods)] // the write seam's own placement (R1, D-26; AUD-29-31).
fn move_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  rustix::fs::linkat(dir, from, to_dir, to, AtFlags::empty()).map_err(refusal)?;
  rustix::fs::unlinkat(dir, from, AtFlags::empty()).map_err(refusal)
}

/// Where neither exists, the rename that never replaces is unavailable: a removal refuses rather than race.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_without_replacing(
  _dir: BorrowedFd<'_>,
  _from: &str,
  _to_dir: BorrowedFd<'_>,
  _to: &str,
) -> Result<(), HostError> {
  Err(refusal(rustix::io::Errno::OPNOTSUPP))
}

/// macOS `renameatx_np` from `a` under `dir` to `b` under `to_dir`, with `flag` (`RENAME_SWAP` or
/// `RENAME_EXCL`): the errno it refused with, which the caller reads (an exchange its own way, A-539).
#[cfg(target_os = "macos")]
fn renameatx(
  dir: BorrowedFd<'_>,
  a: &str,
  to_dir: BorrowedFd<'_>,
  b: &str,
  flag: libc::c_uint,
) -> Result<(), rustix::io::Errno> {
  let a = std::ffi::CString::new(a).map_err(|_| rustix::io::Errno::INVAL)?;
  let b = std::ffi::CString::new(b).map_err(|_| rustix::io::Errno::INVAL)?;
  // SAFETY: `dir` and `to_dir` are directory descriptors this host owns for the call's duration; `a` and
  // `b` are NUL-terminated C strings that outlive the call; `renameatx_np` reads them and
  // writes nothing to memory the caller holds.
  let rc = unsafe {
    libc::renameatx_np(
      dir.as_raw_fd(),
      a.as_ptr(),
      to_dir.as_raw_fd(),
      b.as_ptr(),
      flag,
    )
  };
  if rc == 0 {
    Ok(())
  } else {
    Err(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    ))
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange_names(_dir: BorrowedFd<'_>, _a: &str, _b: &str) -> Result<(), HostError> {
  Err(refusal(rustix::io::Errno::OPNOTSUPP))
}

/// A file's write-out on Linux: its writeback started (`sync_file_range(SYNC_FILE_RANGE_WRITE)`, which neither waits
/// nor writes metadata), so its data is in flight and its blocks allocated before the batch's barrier syncs every file
/// at once. The barrier, not this, makes the file durable. Measured (2026-10-10, `BENCHMARKS.md`, ten rounds, the arms'
/// order alternated): with the concurrent barrier, 64–129 µs a file against 112–197 for the barrier alone when the disk
/// is quiet, and 115–3,688 against 211–6,120 beside a busy writer. Its errors are the file's: `ENOSPC` where starting
/// writeback allocates, `EIO` where the device refuses.
#[cfg(target_os = "linux")]
fn write_out_file(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  // SAFETY: `sync_file_range` takes no pointer argument; `fd` is a descriptor this host owns for the call's duration.
  let rc = unsafe { libc::sync_file_range(fd.as_raw_fd(), 0, 0, libc::SYNC_FILE_RANGE_WRITE) };
  if rc == 0 {
    Ok(())
  } else {
    Err(refusal(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )))
  }
}

/// A file's write-out on macOS and the other unixes: nothing reaches the device here. Their only call that starts a
/// file's writeback also waits for it (`fsync`, `fdatasync`), so it belongs to the batch's barrier, off the shard
/// (A-132): on the shard a macOS `fsync` held every other client up to 206 ms a file on a busy machine (2026-10-10).
#[cfg(not(target_os = "linux"))]
fn write_out_file(_fd: BorrowedFd<'_>) -> Result<(), HostError> {
  Ok(())
}

/// A batch's barrier away from its host (A-132): its own descriptor of each file, so the host may close a file while it
/// runs, and the platform's syncs over them when it runs — each file synced ([`sync_file`]) on ⌈√n⌉ streams for n
/// files, then the syncs ordered ahead of every later write ([`order_synced`]).
struct OsBarrier {
  files: Vec<(OwnedFd, Durability)>,
}

/// What a batch's barrier makes durable of one file (A-472). A temporary's data (`Data`): its name and inode reach the
/// disk with the directory sync its placement is followed by. A file whose metadata alone a landing set in place
/// (`Whole`): its inode, which `fsync` flushes ("as well as flushing the file data, fsync() also flushes the metadata
/// information associated with the file", fsync(2)) and `fdatasync` need not ("does not flush modified metadata unless
/// that metadata is needed in order to allow a subsequent data retrieval", the same page), nor any directory's sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Durability {
  /// A temporary's data.
  Data,
  /// A file's metadata with its data.
  Whole,
}

impl Barrier for OsBarrier {
  fn run(self: Box<Self>) -> Result<(), HostError> {
    let files: Vec<(BorrowedFd<'_>, Durability)> = self
      .files
      .iter()
      .map(|(fd, durability)| (fd.as_fd(), *durability))
      .collect();
    let fds: Vec<BorrowedFd<'_>> = files.iter().map(|(fd, _)| *fd).collect();
    let streams = files
      .len()
      .isqrt()
      .saturating_add(usize::from(!is_square(files.len())));
    sync_on_streams(&files, streams).and_then(|()| order_synced(&fds))
  }
}

/// Whether `n` is a perfect square (so ⌈√n⌉ is `n.isqrt()` itself).
fn is_square(n: usize) -> bool {
  let root = n.isqrt();
  root.checked_mul(root) == Some(n)
}

/// A batch's syncs on `streams` streams: the caller takes stream zero and a scoped thread each further stream, every
/// thread joined before the return; the first refusal. A thread the system refuses to start leaves its share to the
/// caller.
///
/// Derived: k streams cost about a·k for their threads' start and join and b·n/k for each stream's serial chain of
/// syncs, least at k = √(b·n/a). The measurement (2026-10-10, `BENCHMARKS.md`) puts a and b about equal on Linux: for
/// 16-file batches, four streams beat two and sixteen at 1, 2, 4 and 18 CPUs, quiet and beside a busy writer, while
/// sixteen threads at once stalled a 1-CPU container 31–43 ms a barrier and spent 27–37 s of a bench throttled by its
/// CPU quota. An online ramp over the count was tried the same day and rejected: a barrier's time varies about 6× at
/// one count on a shared disk, so two samples mismeasured the noise and the ramp settled on one stream in 9,620 of
/// 9,690 barriers.
fn sync_on_streams(fds: &[(BorrowedFd<'_>, Durability)], streams: usize) -> Result<(), HostError> {
  let streams = streams.clamp(1, fds.len().max(1));
  std::thread::scope(|scope| {
    let mut started = Vec::with_capacity(streams.saturating_sub(1));
    let mut refused = Vec::new();
    for stream in 1..streams {
      match std::thread::Builder::new()
        .spawn_scoped(scope, move || sync_stream(fds, stream, streams))
      {
        Ok(thread) => started.push(thread),
        Err(_) => refused.push(stream),
      }
    }
    let mut synced = sync_stream(fds, 0, streams);
    for stream in refused {
      synced = synced.and(sync_stream(fds, stream, streams));
    }
    for thread in started {
      let joined = thread
        .join()
        .unwrap_or(Err(unavailable(rustix::io::Errno::IO)));
      synced = synced.and(joined);
    }
    synced
  })
}

/// One stream's share of a batch: files `stream`, `stream + streams`, and so on, each synced in turn; the first refusal
/// (the rest are still synced).
fn sync_stream(
  fds: &[(BorrowedFd<'_>, Durability)],
  stream: usize,
  streams: usize,
) -> Result<(), HostError> {
  let mut synced = Ok(());
  for (fd, durability) in fds.iter().skip(stream).step_by(streams.max(1)) {
    synced = synced.and(sync_file(*fd, *durability));
  }
  synced
}

/// One file of a batch made durable on Linux and the other unixes: a temporary's data by `fdatasync` (on Linux its
/// writeback was started at the write-out, and the journal's group commit merges the syncs in flight together: ext4's
/// jbd2 joins every sync waiting on the running transaction to its commit); a file whose metadata alone a landing set,
/// whole by `fsync`, since `fdatasync` need not write a mode (fsync(2), A-472).
#[cfg(not(target_os = "macos"))]
#[allow(clippy::disallowed_methods)] // the write seam's own barrier (R1, D-26; AUD-29-31).
fn sync_file(fd: BorrowedFd<'_>, durability: Durability) -> Result<(), HostError> {
  match durability {
    Durability::Data => rustix::fs::fdatasync(fd).map_err(refusal),
    Durability::Whole => rustix::fs::fsync(fd).map_err(refusal),
  }
}

/// One file of a batch handed to the device on macOS: `fsync`, which "causes all modified data and attributes of fildes
/// to be moved to a permanent storage device" (Apple's fsync(2)), a temporary's and a set mode's alike (not a media
/// flush); the batch's barrier after every one orders them ahead of every later write ([`order_synced`]).
#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_methods)] // the write seam's own barrier (R1, D-26; AUD-29-31).
fn sync_file(fd: BorrowedFd<'_>, _durability: Durability) -> Result<(), HostError> {
  rustix::fs::fsync(fd).map_err(refusal)
}

/// The batch's syncs ordered on macOS: one `F_BARRIERFSYNC` per device among the files, on that device's last file.
/// The barrier applies to everything `fsync`ed on its device before it, so it orders every file synced there; files on
/// another device (a mount inside the target) need that device's own.
#[cfg(target_os = "macos")]
fn order_synced(fds: &[BorrowedFd<'_>]) -> Result<(), HostError> {
  let mut last_on_device = BTreeMap::new();
  for fd in fds {
    let device = rustix::fs::fstat(*fd).map_err(refusal)?.st_dev;
    last_on_device.insert(device, *fd);
  }
  for fd in last_on_device.into_values() {
    data_barrier(fd)?;
  }
  Ok(())
}

/// The batch's syncs ordered on Linux and the other unixes: each `fdatasync` returned with its file durable, so nothing
/// remains.
#[cfg(not(target_os = "macos"))]
fn order_synced(_fds: &[BorrowedFd<'_>]) -> Result<(), HostError> {
  Ok(())
}

/// The data barrier: macOS `fcntl(F_BARRIERFSYNC)`: `fsync` of the file, then a barrier that "applies to I/O that have
/// been flushed with fsync(2) on the same device before" — ordered against later writes, not a media flush (the media
/// barrier is `sync_media`). fcntl(2) names the batch's use: "execute operations of phase one, then fsync(2) each FD and
/// issue a single barrier. Finally execute operations of phase two" (implemented on HFS and APFS) [B: macOS fcntl(2)].
#[cfg(target_os = "macos")]
fn data_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  // SAFETY: `fcntl` with `F_BARRIERFSYNC` takes no pointer argument; `fd` is a descriptor
  // this host owns for the call's duration.
  let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_BARRIERFSYNC) };
  if rc == 0 {
    Ok(())
  } else {
    Err(refusal(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )))
  }
}

/// The media barrier: Linux `fsync` reaches the media already.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::disallowed_methods)] // the write seam's own barrier (R1, D-26; AUD-29-31).
fn media_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  rustix::fs::fsync(fd).map_err(refusal)
}

/// The media barrier: macOS `fcntl(F_FULLFSYNC)`.
#[cfg(target_os = "macos")]
fn media_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  // SAFETY: `fcntl` with `F_FULLFSYNC` takes no pointer argument; `fd` is a descriptor this
  // host owns for the call's duration.
  let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_FULLFSYNC) };
  if rc == 0 {
    Ok(())
  } else {
    Err(refusal(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )))
  }
}

impl HostFs for OsLand {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.host.facts(dir)
  }

  fn now_ns(&mut self) -> i64 {
    self.host.now_ns()
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    self.host.fingerprint_dir(dir)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    self.host.list(dir)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    self.host.open_dir(parent, name)
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.host.open_file(dir, name)
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self.host.fstat(file)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.host.read_at(file, off, buf)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    self.host.read_link(dir, name)
  }

  fn close_file(&mut self, file: HostFile) {
    self.temps.remove(&file.0);
    self.host.close_file(file);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.host.close_dir(dir);
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    self.host.watch(dir)
  }

  fn hints(&mut self) -> Vec<Hint> {
    self.host.hints()
  }
}

// The write seam (R1, D-26): the one place in slates that writes host paths, under a granted landing. The
// resolved-path lint that refuses these calls everywhere else (`clippy.toml`, AUD-29-31) is allowed here only.
#[allow(clippy::disallowed_methods)]
impl LandFs for OsLand {
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError> {
    self.dir(dir)?;
    Ok(LandCapabilities {
      exchange: cfg!(any(target_os = "linux", target_os = "macos")),
      reflink: false,
      unnamed_temporaries: cfg!(target_os = "linux"),
    })
  }

  /// No reflink here yet (`capabilities` says so, so the engine never asks): Linux's `FICLONE` and macOS's
  /// `clonefile` are owed (A-445); refused `ENOTSUP`, nothing made.
  fn clone_temp(
    &mut self,
    dir: HostDir,
    _name: &str,
    _from: HostFile,
    _len: u64,
  ) -> Result<HostFile, HostError> {
    self.dir(dir)?;
    Err(refusal(rustix::io::Errno::NOTSUP))
  }

  fn create_temp(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    if let Some(file) = self.create_unnamed(dir)? {
      return Ok(file);
    }
    self.create_named(dir, name)
  }

  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError> {
    let fd = self.file(file)?;
    let mut written = 0usize;
    while written < bytes.len() {
      let at = off.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
      let n =
        rustix::io::pwrite(fd, bytes.get(written..).unwrap_or_default(), at).map_err(refusal)?;
      if n == 0 {
        return Err(unavailable(rustix::io::Errno::IO));
      }
      written = written.saturating_add(n);
    }
    Ok(())
  }

  fn write_out(&mut self, file: HostFile) -> Result<(), HostError> {
    write_out_file(self.file(file)?)
  }

  /// The batch's barrier over its own descriptor of each file: a temporary synced for its data, any other file (one whose
  /// metadata alone a landing set in place) synced whole (A-472).
  fn begin_barrier(&mut self, files: &[HostFile]) -> Result<Box<dyn Barrier>, HostError> {
    let files = files
      .iter()
      .map(|file| {
        let durability = if self.temps.contains_key(&file.0) {
          Durability::Data
        } else {
          Durability::Whole
        };
        self
          .file(*file)?
          .try_clone_to_owned()
          .map(|fd| (fd, durability))
          .map_err(|error| HostError::Unavailable(OsCode::of_io(&error)))
      })
      .collect::<Result<Vec<_>, _>>()?;
    Ok(Box::new(OsBarrier { files }))
  }

  fn end_barrier(
    &mut self,
    _files: &[HostFile],
    ran: Result<(), HostError>,
  ) -> Result<(), HostError> {
    ran
  }

  /// The file opened to read, never through a link (`O_NOFOLLOW`; a regular file alone): `fchmod` asks ownership, not
  /// write access (A-458). A file its owner may not read is refused `EACCES`, typed, and the entry fails.
  fn open_to_chmod(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.host.open_file(dir, name)
  }

  /// The directory opened as the host opens one (`O_DIRECTORY|O_NOFOLLOW`, its name checked and never on one of this
  /// daemon's own mounts, A-227) and held as a file handle (A-501): `fchmod`, `fstat` and the barrier's `fsync` take any
  /// descriptor, and `fsync` of a directory's descriptor writes its inode, its mode among it.
  fn open_dir_to_chmod(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let opened = self.host.open_dir(dir, name)?;
    let held = self.host.dir_fd(opened).and_then(|fd| {
      fd.try_clone_to_owned()
        .map_err(|error| HostError::Unavailable(OsCode::of_io(&error)))
    });
    self.host.close_dir(opened);
    self.host.adopt_file(held?)
  }

  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError> {
    rustix::fs::fchmod(self.file(file)?, mode_bits(mode)).map_err(refusal)
  }

  /// Linux and macOS hold no Windows attribute bits (A-262: their landings carry them in the volume and apply none), so
  /// the file is kept as it is (A-451).
  fn set_windows(&mut self, file: HostFile, _windows: u16) -> Result<(), HostError> {
    self.file(file).map(drop)
  }

  fn set_mtime(&mut self, file: HostFile, mtime_ns: i64) -> Result<(), HostError> {
    let stamp = rustix::fs::Timespec {
      tv_sec: mtime_ns.div_euclid(NS_PER_S),
      tv_nsec: mtime_ns.rem_euclid(NS_PER_S),
    };
    let omit = rustix::fs::Timespec {
      tv_sec: 0,
      tv_nsec: rustix::fs::UTIME_OMIT,
    };
    rustix::fs::futimens(
      self.file(file)?,
      &rustix::fs::Timestamps {
        last_access: omit,
        last_modification: stamp,
      },
    )
    .map_err(refusal)
  }

  fn place(&mut self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    let temp = self
      .temps
      .get(&file.0)
      .cloned()
      .ok_or(HostError::StaleHandle)?;
    match temp.created_as {
      None => self.link_unnamed(file, dir, name),
      Some(created) if temp.dir == dir && created.as_ref() == name => Ok(()),
      Some(created) => {
        move_without_replacing(self.dir(temp.dir)?, created.as_ref(), self.dir(dir)?, name)?;
        self.temps.insert(
          file.0,
          Temp {
            dir,
            created_as: Some(name.into()),
          },
        );
        Ok(())
      }
    }
  }

  fn exchange(&mut self, dir: HostDir, a: &str, b: &str) -> Result<(), HostError> {
    exchange_names(self.dir(dir)?, a, b)
  }

  fn rename_noreplace(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError> {
    rename_without_replacing(self.dir(dir)?, from, self.dir(to_dir)?, to)
  }

  fn entry_fingerprint(&mut self, dir: HostDir, name: &str) -> Result<Fingerprint, HostError> {
    self.host.entry_fingerprint(dir, name)
  }

  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    rustix::fs::unlinkat(self.dir(dir)?, name, AtFlags::empty()).map_err(refusal)
  }

  /// `mkdirat` applies the process umask, so the directory is then opened (`O_DIRECTORY|O_NOFOLLOW`,
  /// never a symlink swapped in under the name) and given the recorded mode exactly with `fchmod`,
  /// as a file's temporary is by `set_mode`. Before 2026-10-01 the umask cleared bits the volume
  /// held (0775 landed 0755; docs/bugs/2026-10-01-landed-directory-mode-umask.md).
  /// A directory with its mode; a Unix directory holds no Windows bits (A-451).
  fn mkdir(&mut self, dir: HostDir, name: &str, mode: u32, _windows: u16) -> Result<(), HostError> {
    let parent = self.dir(dir)?;
    rustix::fs::mkdirat(parent, name, mode_bits(mode)).map_err(refusal)?;
    let made = rustix::fs::openat(parent, name, dir_flags(), Mode::empty()).map_err(refusal)?;
    rustix::fs::fchmod(&made, mode_bits(mode)).map_err(refusal)
  }

  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    rustix::fs::unlinkat(self.dir(dir)?, name, AtFlags::REMOVEDIR).map_err(refusal)
  }

  /// `symlinkat` makes the link whole at `name` in one step, so `staged` is not used (the seam's contract); a Unix link
  /// has no kind and holds no Windows bits (A-451).
  fn symlink(
    &mut self,
    dir: HostDir,
    name: &str,
    target: &str,
    _staged: &str,
    _windows: u16,
  ) -> Result<(), HostError> {
    rustix::fs::symlinkat(target, self.dir(dir)?, name).map_err(refusal)
  }

  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError> {
    rustix::fs::fsync(self.dir(dir)?).map_err(refusal)
  }

  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError> {
    media_barrier(self.dir(dir)?)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use rustix::io::Errno;

  /// An exchange the filesystem lacks named `Unsupported`, and every other errno kept `Unavailable`, each by its own
  /// errno.
  fn assert_exchanges_lacking_and_others_kept() {
    let code = |e: Errno| OsCode::Errno(e.raw_os_error());
    for lacking in [Errno::INVAL, Errno::NOTSUP] {
      assert_eq!(
        exchange_refusal(lacking),
        HostError::Unsupported(code(lacking)),
        "{lacking:?}"
      );
    }
    for other in [Errno::INVAL, Errno::IO, Errno::ACCESS] {
      assert_eq!(
        refusal(other),
        HostError::Unavailable(code(other)),
        "{other:?}"
      );
    }
  }

  /// A-539 (the seam names the kinds the engine tells apart). Do: read through the Unix writer's rule the errnos of a
  /// name that exists, a full disk and a spent quota, a verb the filesystem lacks, and an exchange the filesystem lacks
  /// (Linux's `EINVAL`, macOS's `ENOTSUP`). Expect each named as its kind by its own errno: the engine compares no number,
  /// so none is read across platforms (Linux numbers `EDQUOT` 122 and `ENOTSUP` 95, macOS 69 and 45).
  #[test]
  fn the_writer_names_each_kind_the_engine_tells_apart() {
    let code = |e: Errno| OsCode::Errno(e.raw_os_error());
    assert_eq!(refusal(Errno::EXIST), HostError::Exists(code(Errno::EXIST)));
    for full in [Errno::NOSPC, Errno::DQUOT] {
      assert_eq!(refusal(full), HostError::NoSpace(code(full)), "{full:?}");
    }
    for lacking in [Errno::OPNOTSUPP, Errno::NOTSUP] {
      assert_eq!(
        refusal(lacking),
        HostError::Unsupported(code(lacking)),
        "{lacking:?}"
      );
    }
    assert_exchanges_lacking_and_others_kept();
    assert_eq!(refusal(Errno::NOENT), HostError::NotFound);
  }
}
