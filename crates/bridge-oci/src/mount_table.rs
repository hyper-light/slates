//! The kernel's mount table, read as a query that never touches a mount (§4.6 A-9; the crate doc
//! states why a `statfs` of the path would deadlock a single-shard daemon). One seam,
//! [`mount_table`], with a paired implementation per platform: macOS asks `getfsstat(MNT_NOWAIT)`
//! for the kernel's cached table; Linux reads `/proc/self/mountinfo`, the kernel's own table, through
//! a read-only descriptor; every other platform is refused typed. The parser of the Linux text is pure
//! and tested on every host.

use std::fmt;

/// One mounted filesystem as the kernel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
  /// Where it is mounted (the real path the kernel resolved at mount time).
  pub mount_point: String,
  /// Its filesystem type (`nfs`, `apfs`, `fuse.slates`, `ext4`, ...).
  pub fstype: String,
  /// Its source (`localhost:/<name>`, `/dev/disk3s1`, `slates`, ...).
  pub source: String,
}

/// Why the table could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountTableError {
  /// No table query is built for this platform.
  Unsupported {
    /// The platform.
    platform: &'static str,
  },
  /// The kernel refused the query.
  Query {
    /// The errno.
    errno: i32,
  },
  /// The kernel's text did not have the documented shape at this line (1-based).
  Malformed {
    /// The line.
    line: usize,
  },
  /// The table exceeded its derived bound.
  TooLarge {
    /// The bound in bytes.
    cap: usize,
  },
}

impl fmt::Display for MountTableError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Unsupported { platform } => write!(f, "no mount-table query on {platform}"),
      Self::Query { errno } => write!(f, "the kernel refused the mount-table query: errno {errno}"),
      Self::Malformed { line } => write!(f, "the mount table is malformed at line {line}"),
      Self::TooLarge { cap } => write!(f, "the mount table exceeds its bound of {cap} bytes"),
    }
  }
}

impl std::error::Error for MountTableError {}

impl MountTableError {
  /// The errno behind the error, 0 when there is none.
  pub const fn errno(&self) -> i32 {
    match self {
      Self::Query { errno } => *errno,
      Self::Unsupported { .. } | Self::Malformed { .. } | Self::TooLarge { .. } => 0,
    }
  }
}

/// Every mounted filesystem, in mount order (a later mount at the same path shadows an earlier one).
#[cfg(target_os = "macos")]
pub fn mount_table() -> Result<Vec<MountEntry>, MountTableError> {
  macos::query()
}

/// Every mounted filesystem, in mount order (a later mount at the same path shadows an earlier one).
#[cfg(target_os = "linux")]
pub fn mount_table() -> Result<Vec<MountEntry>, MountTableError> {
  parse_mountinfo(&linux::read_mountinfo()?)
}

/// Every mounted filesystem: no query is built here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn mount_table() -> Result<Vec<MountEntry>, MountTableError> {
  Err(MountTableError::Unsupported {
    platform: std::env::consts::OS,
  })
}

#[cfg(target_os = "macos")]
mod macos {
  use super::{MountEntry, MountTableError};

  /// A NUL-terminated C string field of a `statfs` record as text, stopping at the first NUL. Each
  /// `c_char` is one byte, reinterpreted bit for bit (a path byte above 127 is a UTF-8 continuation
  /// byte, not a negative number).
  fn field(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
      .iter()
      .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
      .take_while(|b| *b != 0)
      .collect();
    String::from_utf8_lossy(&bytes).into_owned()
  }

  fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
  }

  /// The table through `getfsstat(MNT_NOWAIT)`: the count first, then the records into a buffer
  /// sized for exactly that count.
  pub(super) fn query() -> Result<Vec<MountEntry>, MountTableError> {
    let record_len = size_of::<libc::statfs>();
    // SAFETY: `getfsstat(NULL, 0, MNT_NOWAIT)` writes nothing and returns the number of mounted
    // filesystems (getfsstat(2)); the second call is given a buffer this function owns with capacity
    // for `count` records and a byte size that is exactly `count * sizeof(statfs)`, so the kernel
    // writes at most that many records into memory that is ours; `set_len` is then called with the
    // number of records the kernel reports it wrote, never more than the capacity, and every record
    // is a plain C struct the kernel filled in full. `MNT_NOWAIT` asks for the cached table without
    // contacting any filesystem, so no mount served by this process is touched.
    let records: Vec<libc::statfs> = unsafe {
      let count = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
      let count = usize::try_from(count).map_err(|_| MountTableError::Query {
        errno: last_errno(),
      })?;
      let bytes = count
        .checked_mul(record_len)
        .and_then(|b| libc::c_int::try_from(b).ok())
        .ok_or(MountTableError::TooLarge {
          cap: usize::try_from(libc::c_int::MAX).unwrap_or(usize::MAX),
        })?;
      let mut records: Vec<libc::statfs> = Vec::with_capacity(count);
      let written = libc::getfsstat(records.as_mut_ptr(), bytes, libc::MNT_NOWAIT);
      let written = usize::try_from(written).map_err(|_| MountTableError::Query {
        errno: last_errno(),
      })?;
      records.set_len(written.min(count));
      records
    };
    Ok(
      records
        .iter()
        .map(|record| MountEntry {
          mount_point: field(&record.f_mntonname),
          fstype: field(&record.f_fstypename),
          source: field(&record.f_mntfromname),
        })
        .collect(),
    )
  }
}

#[cfg(target_os = "linux")]
mod linux {
  use rustix::fs::{Mode, OFlags};

  use super::MountTableError;

  /// Format: the kernel's table of this process's mount namespace (proc(5)).
  const MOUNTINFO: &str = "/proc/self/mountinfo";
  /// Format: the kernel's bound on mounts per namespace (`fs.mount-max`, proc(5); Linux 4.9+, under
  /// the 5.10 floor).
  const MOUNT_MAX: &str = "/proc/sys/fs/mount-max";
  /// Format: `PATH_MAX` on Linux (limits.h): the bound on each of a line's three path fields.
  const PATH_MAX: usize = 4096;
  /// Derived: a `mountinfo` line holds three paths (the root, the mount point, the source) and the
  /// fixed-width ids, options and type fields; four `PATH_MAX` bounds all of them.
  const LINE_CAP: usize = 4 * PATH_MAX;

  fn errno_of(e: rustix::io::Errno) -> MountTableError {
    MountTableError::Query {
      errno: e.raw_os_error(),
    }
  }

  /// Reads a pseudo-file whole through a read-only descriptor, refusing past `cap` bytes.
  fn read_pseudo_file(path: &str, cap: usize) -> Result<String, MountTableError> {
    let fd =
      rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).map_err(errno_of)?;
    let page = rustix::param::page_size();
    let mut text: Vec<u8> = Vec::new();
    loop {
      let start = text.len();
      text.resize(start + page, 0);
      let read = rustix::io::read(&fd, &mut text[start..]).map_err(errno_of)?;
      text.truncate(start + read);
      if read == 0 {
        break;
      }
      if text.len() > cap {
        return Err(MountTableError::TooLarge { cap });
      }
    }
    String::from_utf8(text).map_err(|_| MountTableError::Malformed { line: 0 })
  }

  /// The kernel's mount bound, as the sysctl states it.
  fn mount_max() -> Result<usize, MountTableError> {
    read_pseudo_file(MOUNT_MAX, LINE_CAP)?
      .trim()
      .parse()
      .map_err(|_| MountTableError::Malformed { line: 0 })
  }

  /// The text of `/proc/self/mountinfo`, bounded by the kernel's mount bound times the line bound.
  pub(super) fn read_mountinfo() -> Result<String, MountTableError> {
    let cap = mount_max()?.saturating_mul(LINE_CAP);
    read_pseudo_file(MOUNTINFO, cap)
  }
}

/// Format: the field separator `mountinfo` puts between the optional fields and the filesystem type
/// (proc(5)).
const MOUNTINFO_SEPARATOR: &str = "-";
/// Format: the zero-based index of the mount point in a `mountinfo` line (proc(5): mount ID, parent
/// ID, `major:minor`, root, mount point, ...).
const MOUNT_POINT_FIELD: usize = 4;

/// Format: a `mountinfo` escape is a backslash followed by exactly three octal digits (proc(5):
/// `\040` space, `\011` tab, `\012` newline, `\134` backslash).
const ESCAPE_DIGITS: usize = 3;
/// Format: the escape's radix, octal.
const ESCAPE_RADIX: u32 = 8;

/// Undoes `mountinfo`'s octal escapes; a backslash not followed by three octal digits is kept as is.
fn unescape(field: &str) -> String {
  let mut out = String::with_capacity(field.len());
  let mut chars = field.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '\\' {
      out.push(c);
      continue;
    }
    let digits: String = chars.clone().take(ESCAPE_DIGITS).collect();
    match u8::from_str_radix(&digits, ESCAPE_RADIX) {
      Ok(byte) if digits.len() == ESCAPE_DIGITS => {
        out.push(char::from(byte));
        for _ in 0..ESCAPE_DIGITS {
          chars.next();
        }
      }
      _ => out.push(c),
    }
  }
  out
}

/// Parses the text of `/proc/self/mountinfo` (proc(5)): per line, the mount point is the fifth field,
/// and the filesystem type and source follow the `-` separator. Pure, so it is tested on every host.
pub fn parse_mountinfo(text: &str) -> Result<Vec<MountEntry>, MountTableError> {
  let mut entries = Vec::new();
  for (index, line) in text.lines().enumerate() {
    if line.trim().is_empty() {
      continue;
    }
    let fields: Vec<&str> = line.split(' ').collect();
    let malformed = MountTableError::Malformed { line: index + 1 };
    let mount_point = fields.get(MOUNT_POINT_FIELD).ok_or(malformed.clone())?;
    let separator = fields
      .iter()
      .skip(MOUNT_POINT_FIELD + 1)
      .position(|f| *f == MOUNTINFO_SEPARATOR)
      .map(|p| p + MOUNT_POINT_FIELD + 1)
      .ok_or(malformed.clone())?;
    let fstype = fields.get(separator + 1).ok_or(malformed.clone())?;
    let source = fields.get(separator + 2).ok_or(malformed)?;
    entries.push(MountEntry {
      mount_point: unescape(mount_point),
      fstype: unescape(fstype),
      source: unescape(source),
    });
  }
  Ok(entries)
}
