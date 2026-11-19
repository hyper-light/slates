//! The read-only host seam of the base plane (§4.5, §4.15, D-25): everything the volume core
//! asks of the disk beneath an overlay volume, as one trait with opaque handles, so the core
//! links no host call at all. `slates-base` implements it over the operating system with the
//! bulk listing and `O_NOFOLLOW` opens the design names and with the watchers behind the same
//! seam; [`sim::SimHost`] implements it in memory with outsider edits, clock control and
//! watcher overflow on demand, which is what the oracle tests drive.
//!
//! Nothing behind this trait can write: it has no verb for it, and the implementing crate's
//! lint wall refuses write-capable syscalls (R1).

pub mod sim;
/// A volume symlink as a Windows symbolic link (A-275): the reparse buffer the WinFsp drive shows and a Windows landing
/// writes (A-438).
pub mod windows_link;

use crate::inode::Fingerprint;

/// An open directory on the host, owned by the host implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostDir(pub u64);

/// An open file on the host, owned by the host implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostFile(pub u64);

/// What a listed entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostKind {
  /// A regular file.
  File,
  /// A directory.
  Dir,
  /// A symlink.
  Symlink,
  /// Anything else (a device, a socket): listed, never served.
  Other,
}

/// One entry of a listing, with the fingerprint the bulk call returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseEntry {
  /// The name as the host holds it.
  pub name: Box<str>,
  /// The kind.
  pub kind: HostKind,
  /// The stat fingerprint at listing time.
  pub fingerprint: Fingerprint,
}

/// Facts about the filesystem beneath a directory that the drift rules need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
  /// The filesystem's timestamp granularity in nanoseconds, from the cited table by type
  /// (`research/disk-source-of-truth.md` §4), plus the measured clock resolution.
  pub timestamp_granularity_ns: u64,
}

/// A watcher's word, never the truth (§4.5): a directory or one of its entries changed, or events were lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
  /// Something under this directory changed, the watcher not saying what (the directory itself moved, or the event
  /// named no entry).
  Changed(HostDir),
  /// The named entry of this directory changed (inotify names the child of every event it reports; a watched file's
  /// own registration knows its name). Everything a [`Hint::Changed`] of the directory does, and the recall it asks
  /// (A-122) is of that entry's delegations alone: a directory delegation is never granted, so a change to one entry
  /// changes no other file a client holds a delegation of.
  Entry(HostDir, Box<str>),
  /// The watcher overflowed: every listing is suspect and every witness is re-checked.
  Overflow,
}

/// The watcher's state, as `status` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchState {
  /// Delivering hints.
  Live,
  /// Lost events; a full re-check is scheduled or done.
  Overflowed,
  /// No watcher on this host or directory; fingerprints alone.
  Unavailable,
}

/// An operating system's number for a refusal, typed by the numbering it is in (A-527, A-539): an errno (Linux, macOS)
/// or a Win32 error code (Windows). The two number their codes on their own, so a number is read only with its kind:
/// 17 is `EEXIST` as an errno and `ERROR_NOT_SAME_DEVICE` as a Win32 code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsCode {
  /// An errno.
  Errno(i32),
  /// A Win32 error code.
  Win32(u32),
}

impl OsCode {
  /// A number in this platform's own numbering, typed as its kind: an errno here.
  #[cfg(not(windows))]
  pub fn native(number: i32) -> Self {
    Self::Errno(number)
  }

  /// A number in this platform's own numbering, typed as its kind: a Win32 error code here (a negative number names
  /// none, read as zero).
  #[cfg(windows)]
  pub fn native(number: i32) -> Self {
    Self::Win32(u32::try_from(number).unwrap_or(0))
  }

  /// An I/O error's OS number, in this platform's numbering (zero where it named none).
  pub fn of_io(error: &std::io::Error) -> Self {
    Self::native(error.raw_os_error().unwrap_or(0))
  }
}

impl std::fmt::Display for OsCode {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Errno(errno) => write!(f, "errno {errno}"),
      Self::Win32(code) => write!(f, "Win32 error {code}"),
    }
  }
}

/// A host refusal, as the seam reports it; the volume maps it to its own taxonomy. The kinds the landing tells apart
/// are named here (A-539): a name that exists, no room, a verb the filesystem lacks; each writer reads its platform's
/// numbers into them, so no reader compares a number across platforms. Every refusal the host gives a number carries it
/// typed by its numbering ([`OsCode`], A-527).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
  /// The entry does not exist (any more).
  NotFound,
  /// Not a directory where one was needed.
  NotDirectory,
  /// Not a file where one was needed (a directory, a device).
  NotFile,
  /// A handle the host no longer holds.
  StaleHandle,
  /// The name exists (`EEXIST`; Windows' `ERROR_FILE_EXISTS`, `ERROR_ALREADY_EXISTS`), by the code the host named it
  /// with (A-539).
  Exists(OsCode),
  /// No room: the filesystem is full or its owner's quota spent (`ENOSPC`, `EDQUOT`; Windows' `ERROR_DISK_FULL`,
  /// `ERROR_HANDLE_DISK_FULL`), by the code the host named it with (A-539).
  NoSpace(OsCode),
  /// The verb does not exist on this filesystem: an exchange or a clone it lacks (`EOPNOTSUPP`, `ENOTSUP`, a rename
  /// flag's `EINVAL`; Windows' own), by the code the host named it with (A-539).
  Unsupported(OsCode),
  /// Anything else the host cannot serve, by the code it named, typed by its numbering (A-527, A-539).
  Unavailable(OsCode),
  /// The path lies on a mount this daemon serves (§4.6: no daemon thread opens a path on a volume the daemon serves;
  /// A-227): nothing was opened, since the daemon would have waited on its own answer. The errno the platform names a
  /// deadlock avoided by.
  OwnMount(i32),
  /// The host is fetching what the call needs and has not got it yet (an S3 base's miss, §4.15 "An S3 base", A-524):
  /// nothing changed, and the same call is answered once the fetch lands. The caller parks the request and runs it
  /// again then; a host that reads a local disk never answers it.
  Pending,
}

impl std::fmt::Display for HostError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotFound => f.write_str("not found"),
      Self::NotDirectory => f.write_str("not a directory"),
      Self::NotFile => f.write_str("not a regular file"),
      Self::StaleHandle => f.write_str("stale host handle"),
      Self::Exists(code) => write!(f, "the name exists ({code})"),
      Self::NoSpace(code) => write!(f, "no room ({code})"),
      Self::Unsupported(code) => write!(f, "not supported here ({code})"),
      Self::Unavailable(code) => write!(f, "host unavailable ({code})"),
      Self::OwnMount(errno) => write!(f, "on a mount this daemon serves (errno {errno})"),
      Self::Pending => f.write_str("the host's fetch is pending"),
    }
  }
}

impl std::error::Error for HostError {}

impl HostError {
  /// The code a refusal names, typed by its numbering: every kind but the typed ones the host names no number for (not
  /// found, not a directory, not a file, a stale handle) and a fetch under way (A-524); a mount of this daemon's names
  /// the errno its platform names a deadlock avoided by.
  pub fn code(&self) -> Option<OsCode> {
    match self {
      Self::Exists(code) | Self::NoSpace(code) | Self::Unsupported(code) | Self::Unavailable(code) => Some(*code),
      Self::OwnMount(errno) => Some(OsCode::Errno(*errno)),
      Self::NotFound | Self::NotDirectory | Self::NotFile | Self::StaleHandle | Self::Pending => None,
    }
  }
}

/// The seam. Every call is relative to a handle the host handed out, never to a path string,
/// so a rename of the base directory by the user changes nothing (§4.4).
pub trait HostFs {
  /// Facts about the filesystem under a directory.
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError>;
  /// The directory's own fingerprint, which changes when its entries do.
  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError>;
  /// The directory's entries with their fingerprints, one bulk call.
  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError>;
  /// Opens a subdirectory (never through a symlink).
  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError>;
  /// Opens a file read-only (never through a symlink); the descriptor keeps the inode's data
  /// alive if the file is renamed over or unlinked.
  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError>;
  /// The fingerprint of an open file, now.
  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError>;
  /// Reads at an offset; returns the bytes read (fewer at the end).
  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError>;
  /// A symlink's target.
  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError>;
  /// Closes a file handle.
  fn close_file(&mut self, file: HostFile);
  /// Closes a directory handle.
  fn close_dir(&mut self, dir: HostDir);
  /// Asks for hints about a directory; `Unavailable` when the host has no watcher.
  fn watch(&mut self, dir: HostDir) -> WatchState;
  /// Drains the hints that arrived since the last call (never blocks).
  fn hints(&mut self) -> Vec<Hint>;
  /// The host's clock, in the domain of its fingerprints' timestamps (the operating system's
  /// wall clock in the units `fstat` reports; the simulated clock in the simulator). The racy
  /// rule asks "is this file's timestamp tick still open?", a question about the filesystem's
  /// clock, never the volume's monotonic one.
  fn now_ns(&mut self) -> i64;
}

/// What a target filesystem offers the landing, probed inside the granted target (§4.15
/// "Exchange fallback"; never at boot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandCapabilities {
  /// An atomic exchange of two names exists (`RENAME_EXCHANGE`, `RENAME_SWAP`).
  pub exchange: bool,
  /// Reflinks exist (`FICLONE`, `clonefile`, ReFS block cloning: A-445): [`LandFs::clone_temp`] shares an
  /// earlier temporary's bytes instead of copying them.
  pub reflink: bool,
  /// Unnamed temporaries exist (`O_TMPFILE`); otherwise a hidden sibling name is used.
  pub unnamed_temporaries: bool,
}

/// A batch's durability barrier taken from its host to run where waiting costs nothing (A-132): a thread of its own,
/// never the shard that owns the landing. The wait is the device's and every other writer's on the filesystem, not the
/// landing's own: on a busy machine a barrier measured 1–160 ms a batch on Linux and an `fsync` up to 206 ms a file on
/// macOS (2026-10-10, `BENCHMARKS.md`), and while one ran on the shard every other client of that shard waited for it.
/// It holds what it syncs by its own descriptors, so its host may close the files meanwhile.
pub trait Barrier: Send {
  /// Blocks until every file the barrier names is durable ahead of any write that follows: the first refusal (every file
  /// is still synced).
  fn run(self: Box<Self>) -> Result<(), HostError>;
}

/// The write side of the seam, for the landing engine only (D-26): every verb is relative to
/// a directory handle inside the granted target, and the only crate that implements it over
/// the operating system is `slates-land`. The simulated host implements it too, with crash and
/// outsider-edit injection, so the landing oracle runs without a disk.
pub trait LandFs: HostFs {
  /// What the filesystem under `dir` supports.
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError>;
  /// A new, empty, writable file under `dir`: unnamed where the filesystem allows, else at the
  /// hidden sibling `name`; it becomes visible only through [`LandFs::place`].
  fn create_temp(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError>;
  /// A new temporary at the hidden sibling `name` under `dir` (named, as `create_temp` names one where the filesystem
  /// has no unnamed ones) holding the first `len` bytes of `from`, an earlier temporary of the same landing whose bytes
  /// are all written, shared with it rather than copied (§4.15 step 6, A-445): ReFS block cloning on Windows (Linux
  /// `FICLONE` and macOS `clonefile` are owed). Asked only where [`LandCapabilities::reflink`] says the filesystem has
  /// it; refused `EOPNOTSUPP` (`ENOTSUP` on macOS) where this pair cannot be shared (another volume, the volume's
  /// limit on a cluster's sharers, a file attribute the two do not share), with nothing left behind, so the engine
  /// writes the bytes as it would have.
  fn clone_temp(
    &mut self,
    dir: HostDir,
    name: &str,
    from: HostFile,
    len: u64,
  ) -> Result<HostFile, HostError>;
  /// Writes at an offset (the whole buffer, or a refusal).
  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError>;
  /// The file is final — its bytes, mode and mtime written (§4.15 step 8, A-130) — and joins its batch: its writeback
  /// started where that never waits on the device (Linux `sync_file_range`), nothing more elsewhere. Never durability:
  /// the batch's barrier makes the file durable, off the shard (A-132). A change to the file after it needs another.
  fn write_out(&mut self, file: HostFile) -> Result<(), HostError>;
  /// The durability barrier of a batch of temporaries, each written out ([`LandFs::write_out`]) since its last change, and
  /// of files whose metadata a landing's metadata entry set in place ([`LandFs::open_to_chmod`], A-472), each synced
  /// whole (Linux `fsync`, since `fdatasync` need not write a mode; macOS `fsync`, which moves "all modified data and
  /// attributes"; Windows `FlushFileBuffers`), taken to run elsewhere ([`Barrier::run`]): once it has run, every one is
  /// durable ahead of any write that follows, so a placement after it never shows a name whose bytes a power loss could
  /// take, and a metadata entry is reported only once its file is synced (Linux the temporaries' `fdatasync`s on ⌈√n⌉
  /// streams, merged by journal group commit; macOS each file's `fsync` and then one `F_BARRIERFSYNC` per device among
  /// them; elsewhere every temporary's `fdatasync`). The outcome comes back through
  /// [`LandFs::end_barrier`]; nothing of the batch may be placed before.
  fn begin_barrier(&mut self, files: &[HostFile]) -> Result<Box<dyn Barrier>, HostError>;
  /// The outcome of the barrier [`LandFs::begin_barrier`] took for `files`, after it ran: the files are durable when it
  /// is `Ok`.
  fn end_barrier(
    &mut self,
    files: &[HostFile],
    ran: Result<(), HostError>,
  ) -> Result<(), HostError>;
  /// The existing file `name` under `dir`, opened so its mode and Windows bits can be set and nothing else (a landing's
  /// metadata entry, A-458): never through a link, and refused `NotFile` for anything but a regular file. Unix opens it
  /// to read, since `fchmod` asks for ownership, not write access; Windows asks its attributes and `FILE_WRITE_DATA`,
  /// which its flush needs (A-472), and writes none of its data. It stays open for its batch's barrier
  /// ([`LandFs::begin_barrier`]), then is closed with [`HostFs::close_file`].
  fn open_to_chmod(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError>;
  /// The existing directory `name` under `dir`, opened as [`LandFs::open_to_chmod`] opens a file, to have its mode and
  /// Windows bits set and its batch's barrier sync it whole (a base directory's metadata entry, A-501): never through a
  /// link, and refused `NotDirectory` for anything but a directory. Unix opens it to read (`O_DIRECTORY|O_NOFOLLOW`), as a
  /// listing does; Windows asks its attributes and `FILE_ADD_FILE`, a directory's `FILE_WRITE_DATA`, which its flush
  /// needs. A file handle, closed with [`HostFs::close_file`]: `set_mode`, `set_windows`, `fstat` and the barrier take it
  /// as they take a file's.
  fn open_dir_to_chmod(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError>;
  /// Sets the file's mode.
  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError>;
  /// Gives the file the Windows attribute bits the volume keeps for it (A-262, at Windows' values: hidden, system,
  /// archive, offline, not indexed), where the filesystem holds them (A-451: Windows); its read-only state stays its
  /// mode's ([`LandFs::set_mode`]). Linux and macOS hold none and keep the file as it is.
  fn set_windows(&mut self, file: HostFile, windows: u16) -> Result<(), HostError>;
  /// Sets the file's modification time in nanoseconds.
  fn set_mtime(&mut self, file: HostFile, mtime_ns: i64) -> Result<(), HostError>;
  /// Gives the temporary the name `name` under `dir`, refused `EEXIST` when the name exists, so that it holds that
  /// one name after: an unnamed temporary is linked there; a named one is moved there from the name it holds (Linux
  /// `renameat2(RENAME_NOREPLACE)`, macOS `renameatx_np(RENAME_EXCL)`; elsewhere a link and the old name's removal);
  /// nothing for one already at `name`.
  fn place(&mut self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError>;
  /// Atomically exchanges the entries `a` and `b` under `dir` (both must exist).
  fn exchange(&mut self, dir: HostDir, a: &str, b: &str) -> Result<(), HostError>;
  /// Renames `from` under `dir` to `to` under `to_dir` only when `to` does not exist (Linux
  /// `RENAME_NOREPLACE`, macOS `RENAME_EXCL`), refused `EEXIST` otherwise: the step that moves an entry
  /// aside or puts one back without ever replacing a name an outsider took (AUD-29-04). It is the seam's only
  /// rename: a rename that replaced its destination removed whatever an outsider had put there unchecked, so
  /// the seam no longer offers one (2026-09-29).
  fn rename_noreplace(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError>;
  /// The fingerprint of the entry `name` under `dir` itself — a symlink's own, not its target's (`fstatat`
  /// without following): how a moved-aside entry of any kind is checked against its witness.
  fn entry_fingerprint(&mut self, dir: HostDir, name: &str) -> Result<Fingerprint, HostError>;
  /// Removes a file or symlink.
  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError>;
  /// Creates a directory, with the Windows attribute bits the volume keeps for it where the filesystem holds them
  /// (A-451; a directory's read-only among them, which Windows gives no permission).
  fn mkdir(&mut self, dir: HostDir, name: &str, mode: u32, windows: u16) -> Result<(), HostError>;
  /// Removes an empty directory.
  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError>;
  /// Creates the symlink `name` under `dir` to `target`, refused `EEXIST` when the name exists, so that `name` is never
  /// seen holding anything but the whole link. A writer that makes a link in one step (`symlinkat`) makes it at `name`
  /// and ignores `staged`; one that needs two (Windows: the empty object, then its reparse buffer, A-438) makes it at
  /// `staged`, a hidden name the landing owns, and moves it to `name` without replacing, so a crash between the steps
  /// leaves only a hidden sibling the landing's sweep removes. `windows` is the bits the volume keeps for the link
  /// (A-451): on Windows its directory kind (`FILE_ATTRIBUTE_DIRECTORY`, A-275) makes it a directory link, and the other
  /// bits are its attributes; Linux and macOS links have no kind and hold no bits.
  fn symlink(
    &mut self,
    dir: HostDir,
    name: &str,
    target: &str,
    staged: &str,
    windows: u16,
  ) -> Result<(), HostError>;
  /// Makes the directory's entries durable (`fsync` on its descriptor).
  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError>;
  /// One media barrier on the target after the entries' data syncs (§4.15 step 8: macOS
  /// `F_FULLFSYNC`, elsewhere the directory sync already reaches the media), when the grant
  /// asked for media durability.
  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError>;
}
