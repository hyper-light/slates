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
//! Windows' counterpart (an `NtCreateFile` relative to the root's handle) is owed with the store's Windows lane.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

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
    /// The OS error.
    errno: rustix::io::Errno,
  },
}

impl std::fmt::Display for StoreRootRefusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Target(refusal) => write!(f, "the store's directory: {refusal:?}"),
      Self::NotTheGrantsDirectory => {
        f.write_str("the directory is not the one the store's grant names")
      }
      Self::BadName => f.write_str("a store file's name must be one plain name"),
      Self::Io { op, errno } => write!(f, "{op}: {errno}"),
    }
  }
}

impl std::error::Error for StoreRootRefusal {}

/// The granted directory of a repository's store, open: every file of the store is opened relative to it.
#[derive(Debug)]
pub struct StoreRoot {
  dir: OwnedFd,
  path: PathBuf,
  target: TargetIdentity,
}

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
    })
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
      errno,
    })?;
    Ok(file)
  }

  /// Opens the existing file `name` in the directory for reading and writing.
  pub fn open_file(&self, name: &str) -> Result<File, StoreRootRefusal> {
    self.open_at(name, OFlags::empty())
  }

  /// Whether the file `name` exists in the directory (without following a symlink).
  pub fn exists(&self, name: &str) -> Result<bool, StoreRootRefusal> {
    plain(name)?;
    match rustix::fs::statat(&self.dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
      Ok(_) => Ok(true),
      Err(rustix::io::Errno::NOENT) => Ok(false),
      Err(errno) => Err(StoreRootRefusal::Io {
        op: "look up a store file",
        errno,
      }),
    }
  }

  fn open_at(&self, name: &str, extra: OFlags) -> Result<File, StoreRootRefusal> {
    plain(name)?;
    let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | extra;
    let fd =
      rustix::fs::openat(&self.dir, name, flags, Mode::RUSR | Mode::WUSR).map_err(|errno| {
        StoreRootRefusal::Io {
          op: "open a store file",
          errno,
        }
      })?;
    Ok(File::from(fd))
  }
}

/// Refuses a name that is not one plain name.
fn plain(name: &str) -> Result<(), StoreRootRefusal> {
  if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
    return Err(StoreRootRefusal::BadName);
  }
  Ok(())
}

/// The walked directory's identity under `key`.
fn identity_of(dir: &OwnedFd, key: String) -> Result<TargetIdentity, StoreRootRefusal> {
  let stat = rustix::fs::fstat(dir.as_fd()).map_err(|errno| StoreRootRefusal::Io {
    op: "stat the store's directory",
    errno,
  })?;
  Ok(TargetIdentity {
    key: key.into_boxed_str(),
    device: u64::try_from(stat.st_dev).unwrap_or(u64::MAX),
    inode: stat.st_ino,
  })
}
