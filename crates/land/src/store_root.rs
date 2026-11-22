//! The version-control store's directory under its standing grant (A-767 items 4 and 9; main's ruling of 2026-11-20:
//! "the engine's disk writes sit behind the land crate's write seam, under the repository's standing grant").
//!
//! A repository's history lives on disk in one directory a human granted once, a standing grant ([`StoreGrant`]),
//! issued at the repository's creation and recorded (R10). This module is the only way slates opens a file there: it
//! walks the granted path as every landing target is walked (absolute, no symlink and no `..` on the way, owned by
//! this process's user, on none of this daemon's own mounts), checks the directory is the one the grant names (its
//! device and inode, so a directory replaced at the same path is not the grant's), and then creates or opens files
//! only as single names relative to that directory's handle (`openat` with `O_NOFOLLOW`), so no file outside it can
//! be named. A name it creates is made durable with the directory (`fsync` of the directory), so a crash never loses
//! a file whose bytes were flushed.
//!
//! The store's crates (`slates-store`, and the vendored engine and block layer under it) never open a path: they are
//! handed the files this module opens, and write into them in place. R1 still holds: disk is written only under a
//! grant, here a standing one, and the hermeticity tracer counts writes under a granted store root as granted.
//!
//! **Windows (the store's Windows lane).** The same contract, through the land crate's Windows seam
//! (`crate::os`, `os_windows.rs`): the granted path is walked from its drive's root with every component opened
//! `FILE_OPEN_REPARSE_POINT`, so a symbolic link or a junction on the way is refused (`slates_base::walk_dir`), and the
//! directory must be owned by this process's user, compared by security identifier. Its identity is its volume's
//! serial number and its file id (`GetFileInformationByHandleEx`, `FileIdInfo`). Every file is created or opened by
//! `NtCreateFile` with the directory's handle as `OBJECT_ATTRIBUTES.RootDirectory` and one plain name, opened
//! `FILE_OPEN_REPARSE_POINT` (a link under the name is opened as itself, then refused, never followed); a create is
//! `FILE_CREATE`, refused when the name exists. Free bytes are the volume's `FileFsFullSizeInformation` for this caller.
//!
//! **What makes a created name durable on NTFS** (Microsoft's documentation, tier B): `FlushFileBuffers` "writes all the
//! buffered information for a specified file to the device", through a handle that "must have the GENERIC_WRITE access
//! right" ([FlushFileBuffers]); given a directory's handle it flushes that directory's own buffered data, its entries,
//! which is what a create added ([Directory Handles]: a handle to a directory is opened with `FILE_FLAG_BACKUP_SEMANTICS`
//! and takes the same calls a file's does). So a create flushes the granted directory through a handle opened again on
//! it with `FILE_ADD_FILE` (the directory's write right), as the landing's directory sync does (`os_windows.rs`
//! `flush_dir`). What it does not do: it makes nothing durable in another directory, and it is not a barrier for the
//! file's own bytes, which the store flushes through the file's handle (`FlushFileBuffers`, the block layer's full
//! flush). NTFS journals its metadata (the $LogFile), so a name whose directory was flushed survives a crash with the
//! log's guarantees; a name that was not may be gone after one, its file's flushed bytes with it.
//!
//! [FlushFileBuffers]: https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers
//! [Directory Handles]: https://learn.microsoft.com/windows/win32/fileio/obtaining-a-handle-to-a-directory

use std::fs::File;
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use rustix::fs::{Mode, OFlags};

use crate::grant::TargetIdentity;
use crate::os::TargetRefusal;

/// A standing grant for a repository's store: the human's approval, recorded once, that slates may write the
/// repository's history into one directory for as long as the repository lives (A-767 item 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreGrant {
  /// The repository the grant is for.
  pub repository: u64,
  /// The directory the grant names, as it was when the grant was issued.
  pub target: TargetIdentity,
  /// Issued at, monotonic ns.
  pub issued_ns: u64,
}

/// Why a store's directory or one of its files was refused.
#[derive(Debug)]
pub enum StoreRootRefusal {
  /// The path could not be walked as a granted target is (`TargetRefusal`'s reasons).
  Target(TargetRefusal),
  /// The directory at the path is not the one the grant names (another device or inode).
  NotTheGrantsDirectory,
  /// A file name that is not one plain name (empty, `.`, `..`, a separator or a NUL in it).
  BadName,
  /// The OS refused a call on the directory or a file.
  Io {
    /// The call.
    op: &'static str,
    /// The OS's error code.
    code: i32,
  },
  /// This platform has no store directory.
  Unsupported,
}

impl std::fmt::Display for StoreRootRefusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Target(refusal) => write!(f, "the store's directory: {refusal:?}"),
      Self::NotTheGrantsDirectory => {
        f.write_str("the directory is not the one the store's grant names")
      }
      Self::BadName => f.write_str("a store file's name must be one plain name"),
      Self::Io { op, code } => write!(f, "{op}: OS error {code}"),
      Self::Unsupported => {
        f.write_str("a version-control store directory is not supported on this platform")
      }
    }
  }
}

impl std::error::Error for StoreRootRefusal {}

/// The granted directory of a repository's store, open: every file of the store is opened relative to it.
#[derive(Debug)]
pub struct StoreRoot {
  #[cfg(unix)]
  dir: OwnedFd,
  #[cfg(windows)]
  dir: OwnedHandle,
  path: PathBuf,
  target: TargetIdentity,
  /// The standing grant it was opened under, recorded by the store at its open (A-767's `G` row, R10).
  grant: StoreGrant,
}

#[cfg(windows)]
impl StoreRoot {
  /// Opens the directory `path` that `grant` names (the module doc's Windows contract). Refused when the walk is (as a
  /// landing target's), and when the directory found is not the grant's.
  pub fn open(path: &Path, grant: &StoreGrant) -> Result<Self, StoreRootRefusal> {
    let (dir, key) = crate::os::store_dir(path).map_err(StoreRootRefusal::Target)?;
    let target = identity_of(&dir, key)?;
    if target.device != grant.target.device || target.inode != grant.target.inode {
      return Err(StoreRootRefusal::NotTheGrantsDirectory);
    }
    Ok(Self {
      dir,
      path: path.to_path_buf(),
      target,
      grant: grant.clone(),
    })
  }

  /// The identity a standing grant for the directory at `path` binds: the walked directory's key, its volume's serial
  /// number and its file id.
  pub fn identify(path: &Path) -> Result<TargetIdentity, StoreRootRefusal> {
    let (dir, key) = crate::os::store_dir(path).map_err(StoreRootRefusal::Target)?;
    identity_of(&dir, key)
  }

  /// The directory's identity.
  pub fn target(&self) -> &TargetIdentity {
    &self.target
  }

  /// The standing grant the directory was opened under.
  pub fn grant(&self) -> &StoreGrant {
    &self.grant
  }

  /// The path a store file is named under in messages (never opened by it).
  pub fn path_of(&self, name: &str) -> PathBuf {
    self.path.join(name)
  }

  /// Creates the file `name` in the directory, new and empty, for reading and writing, and makes its name durable (the
  /// directory flushed). Refused when it exists.
  pub fn create(&self, name: &str) -> Result<File, StoreRootRefusal> {
    plain(name)?;
    crate::os::store_create(self.dir.as_handle(), name)
      .map(File::from)
      .map_err(|error| io("create a store file", &error))
  }

  /// Opens the existing file `name` in the directory for reading and writing; a link or a junction there is refused.
  pub fn open_file(&self, name: &str) -> Result<File, StoreRootRefusal> {
    plain(name)?;
    crate::os::store_open(self.dir.as_handle(), name)
      .map(File::from)
      .map_err(|error| io("open a store file", &error))
  }

  /// The bytes free to this process on the directory's volume now: the disk a store's grant can grow into.
  pub fn free_bytes(&self) -> Result<u64, StoreRootRefusal> {
    crate::os::store_free_bytes(self.dir.as_handle())
      .map_err(|error| io("read the store's volume's free space", &error))
  }

  /// Whether the name `name` exists in the directory (the name itself, never through a link).
  pub fn exists(&self, name: &str) -> Result<bool, StoreRootRefusal> {
    plain(name)?;
    crate::os::store_exists(self.dir.as_handle(), name)
      .map_err(|error| io("look up a store file", &error))
  }

  /// The directory `name` in the granted directory, a mount point for a view slates presents there: made empty, and its
  /// name made durable, where missing; kept where it is already a directory and no link. The path it lies at.
  ///
  /// # Errors
  /// [`StoreRootRefusal::BadName`] for a name that is not one plain name; [`StoreRootRefusal::Io`] with
  /// `ERROR_DIRECTORY` (267) when the name holds something else, or the OS's code when it refuses the make.
  pub fn mount_point(&self, name: &str) -> Result<PathBuf, StoreRootRefusal> {
    plain(name)?;
    crate::os::store_mount_point(self.dir.as_handle(), name)
      .map_err(|error| io("make a mount point", &error))?;
    Ok(self.path_of(name))
  }
}

/// A Windows store verb's refusal: the call and its Win32 code.
#[cfg(windows)]
fn io(op: &'static str, error: &slates_vfs::host::HostError) -> StoreRootRefusal {
  StoreRootRefusal::Io {
    op,
    code: crate::os::store_code(error),
  }
}

/// The walked directory's identity under `key`: its volume's serial number and its file id.
#[cfg(windows)]
fn identity_of(dir: &OwnedHandle, key: String) -> Result<TargetIdentity, StoreRootRefusal> {
  let (device, inode) = crate::os::store_identity(dir.as_handle())
    .map_err(|error| io("identify the store's directory", &error))?;
  Ok(TargetIdentity {
    key: key.into_boxed_str(),
    device,
    inode,
  })
}

#[cfg(unix)]
impl StoreRoot {
  /// Opens the directory `path` that `grant` names. Refused when the walk is (as a landing target's), and when the
  /// directory found is not the grant's.
  pub fn open(path: &Path, grant: &StoreGrant) -> Result<Self, StoreRootRefusal> {
    let (dir, key) = crate::os::walk_target(path).map_err(StoreRootRefusal::Target)?;
    let target = identity_of(&dir, key)?;
    if target.device != grant.target.device || target.inode != grant.target.inode {
      return Err(StoreRootRefusal::NotTheGrantsDirectory);
    }
    Ok(Self {
      dir,
      path: path.to_path_buf(),
      target,
      grant: grant.clone(),
    })
  }

  /// The standing grant the directory was opened under.
  pub fn grant(&self) -> &StoreGrant {
    &self.grant
  }

  /// The identity a standing grant for the directory at `path` binds (for the grant's issue, which the human
  /// approves): the walked directory's canonical key, device and inode.
  pub fn identify(path: &Path) -> Result<TargetIdentity, StoreRootRefusal> {
    let (dir, key) = crate::os::walk_target(path).map_err(StoreRootRefusal::Target)?;
    identity_of(&dir, key)
  }

  /// The directory's identity.
  pub fn target(&self) -> &TargetIdentity {
    &self.target
  }

  /// The path the store's files are named under in messages (never opened by it).
  pub fn path_of(&self, name: &str) -> PathBuf {
    self.path.join(name)
  }

  /// Creates the file `name` in the directory, new and empty, for reading and writing, and makes its name durable.
  /// Refused when it exists.
  #[allow(clippy::disallowed_methods)] // the store's own create under its standing grant (R1, A-767; AUD-29-31).
  pub fn create(&self, name: &str) -> Result<File, StoreRootRefusal> {
    let file = self.open_at(name, OFlags::CREATE | OFlags::EXCL)?;
    rustix::fs::fsync(&self.dir).map_err(|errno| StoreRootRefusal::Io {
      op: "flush the store's directory after a create",
      code: errno.raw_os_error(),
    })?;
    Ok(file)
  }

  /// Opens the existing file `name` in the directory for reading and writing.
  pub fn open_file(&self, name: &str) -> Result<File, StoreRootRefusal> {
    self.open_at(name, OFlags::empty())
  }

  /// The bytes free to this process on the directory's file system now (`fstatvfs` of its handle: the blocks available
  /// to an unprivileged user times the fragment size): the disk a store's grant can grow into.
  pub fn free_bytes(&self) -> Result<u64, StoreRootRefusal> {
    let stat = rustix::fs::fstatvfs(&self.dir).map_err(|errno| StoreRootRefusal::Io {
      op: "read the store's file system's free space",
      code: errno.raw_os_error(),
    })?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
  }

  /// Whether the file `name` exists in the directory (without following a symlink).
  pub fn exists(&self, name: &str) -> Result<bool, StoreRootRefusal> {
    plain(name)?;
    match rustix::fs::statat(&self.dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
      Ok(_) => Ok(true),
      Err(rustix::io::Errno::NOENT) => Ok(false),
      Err(errno) => Err(StoreRootRefusal::Io {
        op: "look up a store file",
        code: errno.raw_os_error(),
      }),
    }
  }

  /// The directory `name` in the granted directory, a mount point for a view slates presents there (A-645 (a): a
  /// Subversion repository's FSFS view, mounted read-only for its hooks' `REPOS-PATH`): made empty, for this process's
  /// user alone, and its name made durable, where missing; kept where it is already a directory, not a symlink. The
  /// path it lies at.
  ///
  /// # Errors
  /// [`StoreRootRefusal::BadName`] for a name that is not one plain name; [`StoreRootRefusal::Io`] when the name holds
  /// something else (a file, a symlink: `ENOTDIR`) or the OS refuses the make.
  #[allow(clippy::disallowed_methods)] // a mount point under the store's standing grant (R1, A-767; A-645 (a)).
  pub fn mount_point(&self, name: &str) -> Result<PathBuf, StoreRootRefusal> {
    plain(name)?;
    match rustix::fs::mkdirat(&self.dir, name, Mode::RWXU) {
      Ok(()) => rustix::fs::fsync(&self.dir).map_err(|errno| StoreRootRefusal::Io {
        op: "flush the store's directory after a mount point's make",
        code: errno.raw_os_error(),
      })?,
      Err(rustix::io::Errno::EXIST) => {
        let stat = rustix::fs::statat(&self.dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
          .map_err(|errno| StoreRootRefusal::Io {
            op: "look up a mount point",
            code: errno.raw_os_error(),
          })?;
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
          return Err(StoreRootRefusal::Io {
            op: "use a mount point that is not a directory",
            code: rustix::io::Errno::NOTDIR.raw_os_error(),
          });
        }
      }
      Err(errno) => {
        return Err(StoreRootRefusal::Io {
          op: "make a mount point",
          code: errno.raw_os_error(),
        });
      }
    }
    Ok(self.path_of(name))
  }

  fn open_at(&self, name: &str, extra: OFlags) -> Result<File, StoreRootRefusal> {
    plain(name)?;
    let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | extra;
    let fd =
      rustix::fs::openat(&self.dir, name, flags, Mode::RUSR | Mode::WUSR).map_err(|errno| {
        StoreRootRefusal::Io {
          op: "open a store file",
          code: errno.raw_os_error(),
        }
      })?;
    Ok(File::from(fd))
  }
}

/// Refuses a name that is not one plain name: empty, `.`, `..`, a separator or a NUL in it, and on Windows a `\\`
/// separator or a `:` (a stream or a drive).
fn plain(name: &str) -> Result<(), StoreRootRefusal> {
  let windows_only = cfg!(windows) && name.contains(['\\', ':']);
  if name.is_empty()
    || name == "."
    || name == ".."
    || name.contains('/')
    || name.contains('\0')
    || windows_only
  {
    return Err(StoreRootRefusal::BadName);
  }
  Ok(())
}

/// The walked directory's identity under `key`.
#[cfg(unix)]
fn identity_of(dir: &OwnedFd, key: String) -> Result<TargetIdentity, StoreRootRefusal> {
  let stat = rustix::fs::fstat(dir.as_fd()).map_err(|errno| StoreRootRefusal::Io {
    op: "stat the store's directory",
    code: errno.raw_os_error(),
  })?;
  Ok(TargetIdentity {
    key: key.into_boxed_str(),
    device: device_of(&stat),
    inode: stat.st_ino,
  })
}

/// The directory's device as the identity's `u64`, on Linux: glibc's and musl's `st_dev` already is one.
#[cfg(target_os = "linux")]
fn device_of(stat: &rustix::fs::Stat) -> u64 {
  stat.st_dev
}

/// See the Linux arm: elsewhere `st_dev` is narrower and signed (macOS's `dev_t` is an `i32`), so it is widened checked,
/// a negative one, which names no device, kept apart as `u64::MAX`.
#[cfg(all(unix, not(target_os = "linux")))]
fn device_of(stat: &rustix::fs::Stat) -> u64 {
  u64::try_from(stat.st_dev).unwrap_or(u64::MAX)
}
