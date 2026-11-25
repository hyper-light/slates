//! The daemon's spawner (A-924): a process the daemon starts while it holds few descriptors, which starts every child
//! the daemon asks for, so a spawn costs the asking shard a message, never a copy of the daemon's descriptor table.
//!
//! **Why.** One `posix_spawn` call costs its calling thread about 0.1 ms per 1,000 of the parent's open descriptors with
//! `POSIX_SPAWN_CLOEXEC_DEFAULT`, which [`crate::confined_spawn`] sets on macOS (GAPS 8a-hook-exit: 0.2 ms at 100
//! descriptors, 8 to 13 ms at 100,000). A daemon holding its clients' sockets paid that on the shard that spawned.
//!
//! **The process** ([`start`], [`serve_from_env`], [`serve`]). The current executable in its spawner role, its
//! environment empty but for [`ENDS_VARIABLE`], its standard streams the null device, handed one end of a stream pair a
//! shard at descriptor [`crate::confined_spawn::FIRST_HANDED`] on. It is single-threaded: one `poll` over its ends and a
//! self-pipe its `SIGCHLD` handler writes, installed before its first spawn, so no child's exit can be missed.
//!
//! **A conversation on one end** ([`FrameKind`]). Frames of a fixed header and a body. The daemon's side asks
//! [`FrameKind::Spawn`] (the child's standard streams riding the header by `SCM_RIGHTS`) and sends its next spawn only
//! once the last one was answered, so the descriptors the spawner reads belong to one request; it says
//! [`FrameKind::Release`] when it lets a child go. The spawner answers [`FrameKind::Spawned`] or [`FrameKind::Refused`],
//! and later [`FrameKind::Exited`] with the child's status, read with `WNOWAIT`: it reaps a child only once released,
//! so the pid names it until its owner lets it go.
//!
//! **Bound to the daemon.** An end whose peer has gone has its running children's groups killed; once every end has
//! gone, the spawner kills what is left and exits, so a dead daemon leaves no hook running.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};

use rustix::io::Errno;

use crate::confined_spawn::{self, ChildStream, Environment, FIRST_HANDED, Spawn, SpawnedChild};

/// Format: the variable that tells the spawner how many ends it was handed, from [`FIRST_HANDED`] on.
pub const ENDS_VARIABLE: &str = "SLATES_SPAWNER_ENDS";

/// Format: a frame's header: its kind (one byte), three bytes unused, its body's length (`u32`, little-endian), the id
/// of the spawn it is about (`u64`, little-endian).
pub const HEADER_BYTES: usize = 16;
/// Format: where a header's body length begins.
const LENGTH_AT: usize = 4;
/// Format: where a header's id begins.
const ID_AT: usize = 8;
/// Format: the standard streams a spawn names, each the null device or a descriptor riding its request.
const STREAMS: usize = 3;
/// Format: a stream named by a descriptor that rides the request.
const STREAM_HANDED: u8 = 1;
/// Format: an exit by a code ([`FrameKind::Exited`]'s first body byte).
const EXITED_BY_CODE: u8 = 0;
/// Format: an exit by a signal.
const EXITED_BY_SIGNAL: u8 = 1;
/// Format: the byte the `SIGCHLD` handler writes to the spawner's self-pipe.
const WAKE_BYTE: u8 = 1;
/// Format: a wait status's code sits in its second byte (`WEXITSTATUS`), as `waitpid` reports it.
const CODE_SHIFT: u32 = 8;
/// Format: the bits of an exit code.
const CODE_MASK: i32 = 0xff;
/// Shape: the bytes one read of an end takes: a page, many frames' headers.
const READ_BYTES: usize = 4096;

/// What a frame is (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
  /// The daemon asks a child started.
  Spawn = 1,
  /// The daemon lets a child go: reap it once it has exited.
  Release = 2,
  /// The spawner started it: its pid.
  Spawned = 11,
  /// The spawner could not start it: the error's number.
  Refused = 12,
  /// It exited: its status.
  Exited = 13,
}

impl FrameKind {
  /// The kind `byte` names, if any.
  fn of(byte: u8) -> Option<FrameKind> {
    [
      FrameKind::Spawn,
      FrameKind::Release,
      FrameKind::Spawned,
      FrameKind::Refused,
      FrameKind::Exited,
    ]
    .into_iter()
    .find(|kind| *kind as u8 == byte)
  }
}

/// A frame read whole: its kind, its id and its body.
#[derive(Debug)]
pub struct Frame {
  /// Its kind.
  pub kind: FrameKind,
  /// The spawn it is about.
  pub id: u64,
  /// Its body.
  pub body: Vec<u8>,
}

/// The bytes of a frame of `kind` about spawn `id` with `body`.
pub fn frame(kind: FrameKind, id: u64, body: &[u8]) -> io::Result<Vec<u8>> {
  let length =
    u32::try_from(body.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
  let mut bytes = vec![0u8; HEADER_BYTES];
  if let Some(first) = bytes.first_mut() {
    *first = kind as u8;
  }
  put_at(&mut bytes, LENGTH_AT, &length.to_le_bytes());
  put_at(&mut bytes, ID_AT, &id.to_le_bytes());
  bytes.extend_from_slice(body);
  Ok(bytes)
}

/// Writes `value` into `bytes` at `at`.
fn put_at(bytes: &mut [u8], at: usize, value: &[u8]) {
  if let Some(place) = at
    .checked_add(value.len())
    .and_then(|end| bytes.get_mut(at..end))
  {
    place.copy_from_slice(value);
  }
}

/// Appends `bytes` with its length before it.
fn put_bytes(into: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
  let length =
    u32::try_from(bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
  into.extend_from_slice(&length.to_le_bytes());
  into.extend_from_slice(bytes);
  Ok(())
}

/// The body of a [`FrameKind::Spawn`] for `spawn` (the module doc): its program, directory, arguments, environment
/// resolved here, group, and which streams ride the request.
pub fn spawn_body(spawn: &Spawn<'_>) -> io::Result<Vec<u8>> {
  let mut body = Vec::new();
  put_bytes(&mut body, spawn.program.as_os_str().as_bytes())?;
  put_bytes(&mut body, spawn.directory.as_os_str().as_bytes())?;
  put_count(&mut body, spawn.args.len())?;
  for arg in spawn.args {
    put_bytes(&mut body, arg.as_bytes())?;
  }
  let environment = confined_spawn::environment_entries(spawn);
  put_count(&mut body, environment.len())?;
  for entry in &environment {
    put_bytes(&mut body, entry)?;
  }
  body.push(u8::from(spawn.own_group));
  for stream in [spawn.stdin, spawn.stdout, spawn.stderr] {
    body.push(u8::from(matches!(stream, ChildStream::Fd(_))));
  }
  Ok(body)
}

/// Appends a count.
fn put_count(into: &mut Vec<u8>, count: usize) -> io::Result<()> {
  let count = u32::try_from(count).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
  into.extend_from_slice(&count.to_le_bytes());
  Ok(())
}

/// The descriptors a [`FrameKind::Spawn`] for `spawn` carries: its streams named by a descriptor, in order.
pub fn spawn_descriptors<'a>(spawn: &Spawn<'a>) -> Vec<BorrowedFd<'a>> {
  [spawn.stdin, spawn.stdout, spawn.stderr]
    .into_iter()
    .filter_map(|stream| match stream {
      ChildStream::Fd(fd) => Some(fd),
      ChildStream::Null => None,
    })
    .collect()
}

/// A body read in order.
struct Cursor<'b> {
  bytes: &'b [u8],
  at: usize,
}

impl<'b> Cursor<'b> {
  /// The next `count` bytes.
  fn take(&mut self, count: usize) -> io::Result<&'b [u8]> {
    let end = self.at.checked_add(count).ok_or_else(malformed)?;
    let taken = self.bytes.get(self.at..end).ok_or_else(malformed)?;
    self.at = end;
    Ok(taken)
  }

  /// The next `u32`.
  fn u32(&mut self) -> io::Result<u32> {
    let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| malformed())?;
    Ok(u32::from_le_bytes(bytes))
  }

  /// The next length-prefixed bytes.
  fn bytes(&mut self) -> io::Result<&'b [u8]> {
    let length = usize::try_from(self.u32()?).map_err(|_| malformed())?;
    self.take(length)
  }

  /// The next byte.
  fn byte(&mut self) -> io::Result<u8> {
    self.take(1)?.first().copied().ok_or_else(malformed)
  }
}

/// A body that is not one of ours.
fn malformed() -> io::Error {
  io::Error::from(io::ErrorKind::InvalidData)
}

/// A spawn request read back: what [`spawn_body`] wrote.
struct Requested<'b> {
  program: &'b [u8],
  directory: &'b [u8],
  args: Vec<&'b [u8]>,
  environment: Vec<&'b [u8]>,
  own_group: bool,
  handed: [bool; STREAMS],
}

/// Reads a [`FrameKind::Spawn`]'s body.
fn requested(body: &[u8]) -> io::Result<Requested<'_>> {
  let mut cursor = Cursor { bytes: body, at: 0 };
  let program = cursor.bytes()?;
  let directory = cursor.bytes()?;
  let args = listed(&mut cursor)?;
  let environment = listed(&mut cursor)?;
  let own_group = cursor.byte()? != 0;
  let mut handed = [false; STREAMS];
  for stream in &mut handed {
    *stream = cursor.byte()? == STREAM_HANDED;
  }
  Ok(Requested {
    program,
    directory,
    args,
    environment,
    own_group,
    handed,
  })
}

/// A counted list of length-prefixed bytes.
fn listed<'b>(cursor: &mut Cursor<'b>) -> io::Result<Vec<&'b [u8]>> {
  let count = cursor.u32()?;
  (0..count).map(|_| cursor.bytes()).collect()
}

/// What the spawner answered about one spawn.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
  /// Started, with this pid.
  Spawned(u32),
  /// Not started: the operating system's error.
  Refused(i32),
  /// Exited with this status.
  Exited(ExitStatus),
}

/// Reads an answer frame; none for a frame that is not an answer or not well formed.
pub fn answer_of(frame: &Frame) -> Option<Answer> {
  let mut cursor = Cursor {
    bytes: &frame.body,
    at: 0,
  };
  match frame.kind {
    FrameKind::Spawned => cursor.u32().ok().map(Answer::Spawned),
    FrameKind::Refused => cursor
      .u32()
      .ok()
      .map(|number| Answer::Refused(i32::from_le_bytes(number.to_le_bytes()))),
    FrameKind::Exited => {
      let how = cursor.byte().ok()?;
      let value = i32::from_le_bytes(cursor.u32().ok()?.to_le_bytes());
      Some(Answer::Exited(status_of(how, value)))
    }
    FrameKind::Spawn | FrameKind::Release => None,
  }
}

/// The wait status an exit by `how` with `value` reads as (`waitpid`'s encoding).
fn status_of(how: u8, value: i32) -> ExitStatus {
  if how == EXITED_BY_SIGNAL {
    ExitStatus::from_raw(value)
  } else {
    ExitStatus::from_raw((value & CODE_MASK).checked_shl(CODE_SHIFT).unwrap_or(0))
  }
}

/// What has been read of an end and not yet taken as frames, and the descriptors that rode it.
#[derive(Debug, Default)]
pub struct Inbound {
  bytes: Vec<u8>,
  descriptors: Vec<OwnedFd>,
}

/// What a read of an end found.
#[derive(Debug, PartialEq, Eq)]
pub enum Read {
  /// Bytes arrived.
  Bytes,
  /// Nothing waits now.
  Waiting,
  /// The peer has gone.
  Closed,
  /// The peer sent what no conversation here sends: more descriptors waiting than one request's streams, or a frame
  /// longer than [`most_body_bytes`]. The end is ended as if gone.
  Malformed,
}

/// Derived: the longest body a frame may carry: a spawn's arguments and environment are held by the system to
/// `ARG_MAX` bytes counting each string's NUL, which its four-byte length here stands for, so they encode in at most
/// five times `ARG_MAX`; and its program and directory to `PATH_MAX` each. Read from the system once a read needs it.
pub fn most_body_bytes() -> usize {
  /// Format: the length prefix a string carries here, against the one NUL byte `ARG_MAX` counts it by.
  const PREFIX_PER_COUNTED_BYTE: usize = 4;
  // SAFETY: `sysconf` takes a name by value and reads the system's configuration; it touches no memory of this process's.
  let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
  let arg_max = usize::try_from(arg_max).unwrap_or(usize::MAX);
  let path_max = usize::try_from(libc::PATH_MAX).unwrap_or(usize::MAX);
  arg_max
    .saturating_mul(PREFIX_PER_COUNTED_BYTE.saturating_add(1))
    .saturating_add(path_max.saturating_mul(2))
}

impl Inbound {
  /// Reads what `end` holds now into this.
  ///
  /// # Errors
  /// The operating system's refusal of the read, but for its having nothing now.
  pub fn read(&mut self, end: BorrowedFd<'_>) -> Result<Read, Errno> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(STREAMS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut buffer = [0u8; READ_BYTES];
    let received = match rustix::net::recvmsg(
      end,
      &mut [io::IoSliceMut::new(&mut buffer)],
      &mut control,
      RecvFlags::empty(),
    ) {
      Ok(received) => received,
      Err(Errno::AGAIN | Errno::INTR) => return Ok(Read::Waiting),
      Err(error) => return Err(error),
    };
    for message in control.drain() {
      if let RecvAncillaryMessage::ScmRights(carried) = message {
        self.descriptors.extend(carried);
      }
    }
    if received.bytes == 0 {
      return Ok(Read::Closed);
    }
    self
      .bytes
      .extend_from_slice(buffer.get(..received.bytes).unwrap_or_default());
    let longest = HEADER_BYTES.saturating_add(most_body_bytes());
    if self.descriptors.len() > STREAMS || self.next_length().is_some_and(|length| length > longest)
    {
      return Ok(Read::Malformed);
    }
    Ok(Read::Bytes)
  }

  /// The whole length of the frame the bytes begin with, its header and body, once its header has arrived.
  fn next_length(&self) -> Option<usize> {
    let header = self.bytes.get(..HEADER_BYTES)?;
    let length = u32::from_le_bytes(header.get(LENGTH_AT..ID_AT)?.try_into().ok()?);
    HEADER_BYTES.checked_add(usize::try_from(length).ok()?)
  }

  /// The next whole frame read, if one is; none while one is still arriving (or bytes that name no frame kind, which
  /// are dropped with the frame they head).
  pub fn next_frame(&mut self) -> Option<Frame> {
    loop {
      let header = self.bytes.get(..HEADER_BYTES)?;
      let length = usize::try_from(u32::from_le_bytes(
        header.get(LENGTH_AT..ID_AT)?.try_into().ok()?,
      ))
      .ok()?;
      let id = u64::from_le_bytes(header.get(ID_AT..HEADER_BYTES)?.try_into().ok()?);
      let kind = header.first().copied().and_then(FrameKind::of);
      let end = HEADER_BYTES.checked_add(length)?;
      let body = self.bytes.get(HEADER_BYTES..end)?.to_vec();
      self.bytes.drain(..end);
      if let Some(kind) = kind {
        return Some(Frame { kind, id, body });
      }
    }
  }

  /// The descriptors that have ridden the frames read so far, oldest first, up to `count`.
  pub fn take_descriptors(&mut self, count: usize) -> Vec<OwnedFd> {
    let count = count.min(self.descriptors.len());
    self.descriptors.drain(..count).collect()
  }
}

/// What is owed to be written on an end: frames in order, each with the descriptors that ride its first byte, sent
/// with that byte and never with another frame's.
#[derive(Debug, Default)]
pub struct Outbound {
  frames: std::collections::VecDeque<(Vec<u8>, Vec<OwnedFd>)>,
}

impl Outbound {
  /// Owes `bytes` after what is owed already, `descriptors` riding their first byte.
  pub fn owe(&mut self, bytes: &[u8], descriptors: Vec<OwnedFd>) {
    if !bytes.is_empty() {
      self.frames.push_back((bytes.to_vec(), descriptors));
    }
  }

  /// Whether anything is owed.
  pub fn is_empty(&self) -> bool {
    self.frames.is_empty()
  }

  /// Writes what `end` takes now.
  ///
  /// # Errors
  /// The operating system's refusal of the write, but for its being full now.
  pub fn flush(&mut self, end: BorrowedFd<'_>) -> Result<(), Errno> {
    while let Some((bytes, descriptors)) = self.frames.front_mut() {
      match send(end, bytes, descriptors) {
        Ok(0) | Err(Errno::AGAIN | Errno::INTR) => return Ok(()),
        Ok(taken) => {
          descriptors.clear();
          bytes.drain(..taken.min(bytes.len()));
          if bytes.is_empty() {
            self.frames.pop_front();
          }
        }
        Err(error) => return Err(error),
      }
    }
    Ok(())
  }
}

/// Sends what of `bytes` `end` takes now, `descriptors` riding the first byte by `SCM_RIGHTS`: the bytes taken.
///
/// # Errors
/// The operating system's refusal: `EAGAIN` when the end is full now and nothing went.
pub fn send<Fd: AsFd>(
  end: BorrowedFd<'_>,
  bytes: &[u8],
  descriptors: &[Fd],
) -> Result<usize, Errno> {
  use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
  #[cfg(target_os = "linux")]
  let flags = SendFlags::NOSIGNAL;
  #[cfg(not(target_os = "linux"))]
  let flags = SendFlags::empty();
  if descriptors.is_empty() {
    return rustix::net::send(end, bytes, flags);
  }
  let fds: Vec<BorrowedFd<'_>> = descriptors.iter().map(AsFd::as_fd).collect();
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(STREAMS))];
  let mut control = SendAncillaryBuffer::new(&mut space);
  if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
    return Err(Errno::NOBUFS);
  }
  rustix::net::sendmsg(end, &[io::IoSlice::new(bytes)], &mut control, flags)
}

/// A started spawner: the child, and the daemon's end a shard.
#[derive(Debug)]
pub struct Started {
  /// The spawner's process.
  pub child: SpawnedChild,
  /// The daemon's ends, one a shard, non-blocking.
  pub ends: Vec<OwnedFd>,
}

/// Starts the spawner as `program` with `args` (the current executable in its spawner role), with `ends` ends: one a
/// shard of the daemon's.
///
/// # Errors
/// The pairs' making or the spawn's refusal.
pub fn start(program: &Path, args: &[&OsStr], ends: usize) -> io::Result<Started> {
  let mut ours = Vec::with_capacity(ends);
  let mut theirs = Vec::with_capacity(ends);
  for _ in 0..ends {
    let (daemon, handed) =
      crate::stream::pair().map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    ours.push(daemon);
    theirs.push(handed);
  }
  let handed: Vec<BorrowedFd<'_>> = theirs.iter().map(AsFd::as_fd).collect();
  let count = ends.to_string();
  let child = confined_spawn::spawn(&Spawn {
    program,
    args,
    environment: Environment::Clear,
    overrides: &[(OsStr::new(ENDS_VARIABLE), OsStr::new(&count))],
    directory: Path::new("/"),
    own_group: true,
    stdin: ChildStream::Null,
    stdout: ChildStream::Null,
    stderr: ChildStream::Null,
    handed: &handed,
  })?;
  Ok(Started { child, ends: ours })
}

/// Format: the test a test binary runs as the spawner ([`spawner_role!`] defines it at the binary's root), named so
/// libtest runs it alone (`--exact`); the `slates` binary serves before it reads any argument, so the same arguments
/// start either.
pub const ROLE_TEST: &str = "spawner_role";

/// The arguments the spawner is started with ([`start_here`]): [`ROLE_TEST`] alone, its output not captured.
pub fn role_args() -> [&'static OsStr; 3] {
  [
    OsStr::new(ROLE_TEST),
    OsStr::new("--exact"),
    OsStr::new("--nocapture"),
  ]
}

/// Starts the spawner as this process's own executable ([`role_args`]), with `ends` ends.
///
/// # Errors
/// The executable's path unreadable, or [`start`]'s refusal.
pub fn start_here(ends: usize) -> io::Result<Started> {
  let program = std::env::current_exe()?;
  start(&program, &role_args(), ends)
}

/// Defines a test binary's [`ROLE_TEST`] at its root: it serves as the spawner when the binary was started as one, and
/// does nothing otherwise. A test binary whose daemon or hooks spawn children names it once; one that does not has its
/// spawns refused typed (the spawner it starts runs no test and ends at once).
#[macro_export]
macro_rules! spawner_role {
  () => {
    /// The spawner's role in this test binary (A-924; `slates_ipc::spawner::ROLE_TEST`).
    #[test]
    fn spawner_role() {
      if let Some(served) = $crate::spawner::serve_from_env() {
        std::process::exit(i32::from(served.is_err()));
      }
    }
  };
}

/// Serves as the spawner when this process was started as one ([`ENDS_VARIABLE`] set): what serving ended with; none
/// when it was not.
pub fn serve_from_env() -> Option<io::Result<()>> {
  let count = std::env::var(ENDS_VARIABLE).ok()?.parse::<i32>().ok()?;
  let last = FIRST_HANDED.checked_add(count)?;
  let ends: Vec<OwnedFd> = (FIRST_HANDED..last)
    // SAFETY: the starter placed one end at each of these numbers ([`start`]); nothing else of this process owns them,
    // since this runs before the process opens anything of its own.
    .map(|number| unsafe { OwnedFd::from_raw_fd(number) })
    .collect();
  // Handed with close-on-exec cleared (`dup2`'s copy), so marked again: no child of the spawner's holds an end, where
  // its spawn has no close-by-default (Linux).
  for end in &ends {
    if let Err(error) = rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC) {
      return Some(Err(io::Error::from(error)));
    }
  }
  Some(serve(ends))
}

/// The spawner's self-pipe's write end, for its `SIGCHLD` handler; negative before it is made.
static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// `SIGCHLD`'s handler: one byte to the self-pipe (async-signal-safe: one `write`, `errno` kept).
extern "C" fn on_child(_signal: libc::c_int) {
  let fd = WAKE_WRITE.load(Ordering::Relaxed);
  if fd < 0 {
    return;
  }
  let byte = WAKE_BYTE;
  let errno = errno_location();
  // SAFETY: `errno` is this thread's own `errno`, a valid int for the thread's life; it is read and written back around
  // the write, which is async-signal-safe, of one byte on this frame to a descriptor the spawner keeps open for its life.
  unsafe {
    let saved = *errno;
    let _ = libc::write(fd, (&raw const byte).cast(), 1);
    *errno = saved;
  }
}

/// This thread's `errno` (macOS).
#[cfg(target_vendor = "apple")]
fn errno_location() -> *mut libc::c_int {
  // SAFETY: `__error` takes nothing and answers this thread's `errno` location; it is async-signal-safe.
  unsafe { libc::__error() }
}

/// This thread's `errno` (Linux).
#[cfg(target_os = "linux")]
fn errno_location() -> *mut libc::c_int {
  // SAFETY: `__errno_location` takes nothing and answers this thread's `errno` location; it is async-signal-safe.
  unsafe { libc::__errno_location() }
}

/// Makes the self-pipe and installs `SIGCHLD`'s handler: the pipe's read end, non-blocking.
fn child_wakes() -> io::Result<OwnedFd> {
  let (read, write) = rustix::pipe::pipe().map_err(io::Error::from)?;
  for end in [&read, &write] {
    rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC).map_err(io::Error::from)?;
    let flags = rustix::fs::fcntl_getfl(end).map_err(io::Error::from)?;
    rustix::fs::fcntl_setfl(end, flags | rustix::fs::OFlags::NONBLOCK).map_err(io::Error::from)?;
  }
  // The write end lives as long as the process: the handler writes to it by number, so it is never closed.
  WAKE_WRITE.store(write.into_raw_fd(), Ordering::Relaxed);
  // SAFETY: a zeroed `sigaction` is a valid record; the handler is an `extern "C"` function of this signature, set with
  // `SA_RESTART` (so the loop's calls resume) and `SA_NOCLDSTOP` (an exit, not a stop, wakes it).
  let installed = unsafe {
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = on_child as extern "C" fn(libc::c_int) as usize;
    action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
    libc::sigemptyset(&raw mut action.sa_mask);
    libc::sigaction(libc::SIGCHLD, &raw const action, std::ptr::null_mut())
  };
  if installed != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(read)
}

/// One end of the spawner's: what it read and owes.
struct End {
  fd: OwnedFd,
  inbound: Inbound,
  outbound: Outbound,
  open: bool,
}

/// A child the spawner started.
struct Child {
  end: usize,
  pid: u32,
  exited: bool,
  released: bool,
}

/// The spawner's state.
struct Serving {
  ends: Vec<End>,
  children: BTreeMap<(usize, u64), Child>,
}

/// Serves `ends` as the spawner (the module doc) until every end has gone: then every group it still holds is killed.
///
/// # Errors
/// The self-pipe's making, or a `poll` the operating system refuses.
pub fn serve(ends: Vec<OwnedFd>) -> io::Result<()> {
  let wakes = child_wakes()?;
  let mut serving = Serving {
    ends: ends
      .into_iter()
      .map(|fd| {
        let _ = set_non_blocking(&fd);
        End {
          fd,
          inbound: Inbound::default(),
          outbound: Outbound::default(),
          open: true,
        }
      })
      .collect(),
    children: BTreeMap::new(),
  };
  while serving.ends.iter().any(|end| end.open) {
    serving.poll_once(&wakes)?;
  }
  serving.kill_all();
  Ok(())
}

/// Makes `fd` non-blocking.
fn set_non_blocking(fd: &OwnedFd) -> rustix::io::Result<()> {
  let flags = rustix::fs::fcntl_getfl(fd)?;
  rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
}

impl Serving {
  /// One `poll` over the self-pipe and the open ends, and what it found done.
  fn poll_once(&mut self, wakes: &OwnedFd) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags};
    let mut fds = vec![PollFd::new(wakes, PollFlags::IN)];
    let open: Vec<usize> = (0..self.ends.len())
      .filter(|at| self.ends.get(*at).is_some_and(|end| end.open))
      .collect();
    for at in &open {
      if let Some(end) = self.ends.get(*at) {
        let mut flags = PollFlags::IN;
        if !end.outbound.is_empty() {
          flags |= PollFlags::OUT;
        }
        fds.push(PollFd::new(&end.fd, flags));
      }
    }
    match rustix::event::poll(&mut fds, None) {
      Ok(_) | Err(Errno::INTR) => {}
      Err(error) => return Err(io::Error::from(error)),
    }
    drop(fds);
    drain(wakes);
    self.look_at_children();
    for at in open {
      self.serve_end(at);
    }
    Ok(())
  }

  /// Reads, answers and writes what end `at` has now.
  fn serve_end(&mut self, at: usize) {
    let read = match self.ends.get_mut(at) {
      Some(end) => end.inbound.read(end.fd.as_fd()),
      None => return,
    };
    if matches!(read, Ok(Read::Closed | Read::Malformed) | Err(_)) {
      self.end_gone(at);
      return;
    }
    while let Some(frame) = self
      .ends
      .get_mut(at)
      .and_then(|end| end.inbound.next_frame())
    {
      self.serve_frame(at, &frame);
    }
    if let Some(end) = self.ends.get_mut(at)
      && end.outbound.flush(end.fd.as_fd()).is_err()
    {
      self.end_gone(at);
    }
  }

  /// Does what `frame`, read on end `at`, asks.
  fn serve_frame(&mut self, at: usize, frame: &Frame) {
    match frame.kind {
      FrameKind::Spawn => self.spawn(at, frame),
      FrameKind::Release => self.release(at, frame.id),
      FrameKind::Spawned | FrameKind::Refused | FrameKind::Exited => {}
    }
  }

  /// Starts the child `frame` asks for and answers it.
  fn spawn(&mut self, at: usize, frame: &Frame) {
    let answer = match self.started(at, &frame.body) {
      Ok(child) => {
        let pid = child.id();
        self.children.insert(
          (at, frame.id),
          Child {
            end: at,
            pid,
            exited: false,
            released: false,
          },
        );
        self::frame(FrameKind::Spawned, frame.id, &pid.to_le_bytes())
      }
      Err(error) => {
        let number = error.raw_os_error().unwrap_or(libc::EINVAL);
        self::frame(FrameKind::Refused, frame.id, &number.to_le_bytes())
      }
    };
    self.owe(at, answer);
  }

  /// The child the request `body` on end `at` names, started as `confined_spawn` starts one: the descriptors that rode
  /// it taken whatever the outcome, and closed after.
  fn started(&mut self, at: usize, body: &[u8]) -> io::Result<SpawnedChild> {
    let parsed = requested(body);
    let handed_count = parsed.as_ref().map_or(0, |request| {
      request.handed.iter().filter(|handed| **handed).count()
    });
    let descriptors = self
      .ends
      .get_mut(at)
      .map(|end| end.inbound.take_descriptors(handed_count))
      .unwrap_or_default();
    let request = parsed?;
    if descriptors.len() != handed_count {
      return Err(malformed());
    }
    let mut riding = descriptors.iter();
    let streams: Vec<ChildStream<'_>> = request
      .handed
      .iter()
      .map(|handed| match handed {
        true => riding
          .next()
          .map_or(ChildStream::Null, |fd| ChildStream::Fd(fd.as_fd())),
        false => ChildStream::Null,
      })
      .collect();
    let args: Vec<&OsStr> = request
      .args
      .iter()
      .map(|arg| OsStr::from_bytes(arg))
      .collect();
    let overrides: Vec<(&OsStr, &OsStr)> = request
      .environment
      .iter()
      .filter_map(|entry| {
        let at = entry.iter().position(|byte| *byte == b'=')?;
        Some((
          OsStr::from_bytes(entry.get(..at)?),
          OsStr::from_bytes(entry.get(at.checked_add(1)?..)?),
        ))
      })
      .collect();
    confined_spawn::spawn(&Spawn {
      program: Path::new(OsStr::from_bytes(request.program)),
      args: &args,
      environment: Environment::Clear,
      overrides: &overrides,
      directory: Path::new(OsStr::from_bytes(request.directory)),
      own_group: request.own_group,
      stdin: streams.first().copied().unwrap_or(ChildStream::Null),
      stdout: streams.get(1).copied().unwrap_or(ChildStream::Null),
      stderr: streams.get(2).copied().unwrap_or(ChildStream::Null),
      handed: &[],
    })
  }

  /// Lets child `id` of end `at` go: reaped now if it has exited, else once it does.
  fn release(&mut self, at: usize, id: u64) {
    let exited = match self.children.get_mut(&(at, id)) {
      Some(child) => {
        child.released = true;
        child.exited
      }
      None => return,
    };
    if exited && let Some(child) = self.children.remove(&(at, id)) {
      reap(child.pid);
    }
  }

  /// Reads which children have exited (without reaping them): an exit answered to its end, a released child reaped.
  fn look_at_children(&mut self) {
    let mut gone = Vec::new();
    let mut answers = Vec::new();
    for (key, child) in &mut self.children {
      if child.exited {
        continue;
      }
      let Some((how, value)) = exited_unreaped(child.pid) else {
        continue;
      };
      child.exited = true;
      if child.released {
        gone.push(*key);
      } else {
        let mut body = vec![how];
        body.extend_from_slice(&value.to_le_bytes());
        answers.push((child.end, frame(FrameKind::Exited, key.1, &body)));
      }
    }
    for key in gone {
      if let Some(child) = self.children.remove(&key) {
        reap(child.pid);
      }
    }
    for (end, answer) in answers {
      self.owe(end, answer);
    }
  }

  /// Owes `answer` on end `at`, written as far as it takes now.
  fn owe(&mut self, at: usize, answer: io::Result<Vec<u8>>) {
    let Ok(bytes) = answer else {
      return;
    };
    if let Some(end) = self.ends.get_mut(at)
      && end.open
    {
      end.outbound.owe(&bytes, Vec::new());
      let _ = end.outbound.flush(end.fd.as_fd());
    }
  }

  /// End `at`'s peer has gone: its running children's groups killed, each reaped once it exits.
  fn end_gone(&mut self, at: usize) {
    if let Some(end) = self.ends.get_mut(at) {
      end.open = false;
    }
    for child in self.children.values_mut().filter(|child| child.end == at) {
      child.released = true;
      if !child.exited {
        kill_group(child.pid);
      }
    }
  }

  /// Every end has gone: every running child's group killed.
  fn kill_all(&mut self) {
    for child in self.children.values().filter(|child| !child.exited) {
      kill_group(child.pid);
    }
  }
}

/// Takes every byte the self-pipe holds.
fn drain(wakes: &OwnedFd) {
  let mut bytes = [0u8; READ_BYTES];
  while matches!(rustix::io::read(wakes, &mut bytes), Ok(read) if read > 0) {}
}

/// How `pid` exited, if it has, read without reaping it: its way and its code or signal.
fn exited_unreaped(pid: u32) -> Option<(u8, i32)> {
  let pid = rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)?;
  let options = rustix::process::WaitIdOptions::EXITED
    | rustix::process::WaitIdOptions::NOWAIT
    | rustix::process::WaitIdOptions::NOHANG;
  let status = rustix::process::waitid(rustix::process::WaitId::Pid(pid), options).ok()??;
  Some(match (status.exit_status(), status.terminating_signal()) {
    (_, Some(signal)) => (EXITED_BY_SIGNAL, signal),
    (Some(code), None) => (EXITED_BY_CODE, code),
    (None, None) => (EXITED_BY_SIGNAL, status.raw_signo()),
  })
}

/// Reaps `pid`, which has exited.
fn reap(pid: u32) {
  if let Some(pid) = i32::try_from(pid)
    .ok()
    .and_then(rustix::process::Pid::from_raw)
  {
    let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG);
  }
}

/// Sends `SIGKILL` to the group `pid` leads.
fn kill_group(pid: u32) {
  if let Some(pid) = i32::try_from(pid)
    .ok()
    .and_then(rustix::process::Pid::from_raw)
  {
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
  }
}

#[cfg(test)]
mod tests;
