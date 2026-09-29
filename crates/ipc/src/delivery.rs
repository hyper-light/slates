//! The harness delivery channel of a consumer's capability (§4.13 "Principals": "a consumer channel
//! is bound at rendezvous using a capability delivered and retained outside other agents' reach, for
//! example an inherited endpoint from the trusted harness"; "the harness owns process isolation and
//! capability delivery"; GAP-A9-9 "the harness delivery channel"; `docs/wip/enrollment.md`).
//!
//! Decided 2026-09-14: the capability travels as an **inherited descriptor**, the one channel other
//! same-uid processes provably cannot read. An environment variable shows in `ps -E` and in
//! `/proc/<pid>/environ` to every same-uid reader; an anchor slot is readable by every same-uid
//! client; a pipe end that one child inherits is visible to that child alone.
//!
//! The harness side ([`Delivery::prepare`]) writes one fixed-layout record — the consumer id and its
//! capability under a magic and a CRC32C — into a fresh pipe whose two ends are close-on-exec like
//! every other descriptor of the process, closes the write end, and spawns the workload
//! ([`Delivery::spawn`]) so that this child, and only it, inherits the read end: on Unix the
//! close-on-exec flag is cleared on that one descriptor *in the forked child*, between `fork` and
//! `exec` (`pre_exec`), never in the parent, so a child another thread spawns meanwhile inherits
//! nothing; on Windows the child is created with a `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming the
//! read handle and the three standard handles, so nothing else inheritable in the process reaches
//! it. The child is told *which* descriptor by [`ENV_CONSUMER_FD`]: its number and the identity of the
//! channel behind it, which leak nothing.
//!
//! The number alone does not identify the channel. Numbers are indexes into each process's own table,
//! and the variable is inherited by every process the consumer starts while the descriptor is not —
//! it is closed at the take, and many spawners pass only the standard three. Such a process holds
//! pipes and sockets of its own at the same small numbers, so a take that trusted the number adopted
//! one of them, read its bytes, changed its flags for every holder and closed it under its owner
//! (`docs/bugs/2026-09-28-a-stale-delivery-name-took-a-process-s-own-pipe.md`). The take therefore
//! confirms the identity with calls that touch nothing before it adopts: on Unix, `fstat`'s device,
//! inode and modification time of the pipe, taken after the record is written — the device and inode
//! alone are not enough, because macOS hands a dead pipe's pair to the next pipe (1,999 times in 2,000,
//! measured 2026-09-28), while the nanosecond modification time of a later pipe differs; on Windows,
//! the pipe's unique name, read only after two inherited handles are confirmed to be the one object
//! (`CompareObjectHandles` reads the handle table and never waits, while a name query on a foreign
//! synchronous handle waits behind any read pending on it).
//!
//! The workload side ([`delivered`]) takes the record exactly once per process: the descriptor is
//! marked close-on-exec before anything else (so no exec from another thread carries the capability
//! along), its kind is checked (a pipe or a socket end; a directory, a terminal or a file is
//! refused), the record is read without blocking (the harness wrote it before the spawn, so a short
//! read is a broken harness, never a wait), its length, magic and checksum are verified before a
//! field is decoded, the descriptor is closed and the scratch zeroed. Every failure is a typed
//! [`DeliveryFault`]; only the *absence* of the variable means "not spawned as a consumer", and a
//! present but unusable delivery refuses rather than binding the channel to the account's ambient
//! authority (§4.13: "unsupported secure enrollment refuses, instead of issuing an ambient admin
//! channel").
//!
//! What the workload then does with the record is the client's ([`attest_proof`] keyed over its
//! client id, presented in an `Attest`): the capability itself never crosses the ring.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

use crate::error::IpcError;

/// Format: the environment variable naming the inherited descriptor the capability is delivered on and
/// the identity of its channel, colon-separated decimal fields — on Unix `NUMBER:DEVICE:INODE:MTIME:
/// MTIME_NSEC` (the pipe's `fstat` after the record is written); on Windows `HANDLE:TWIN:PIPE_NAME` (two
/// inherited handles of the one pipe, and the pipe's name). None of it leaks anything: the capability
/// travels only inside the descriptor.
pub const ENV_CONSUMER_FD: &str = "SLATES_CONSUMER_FD";

/// Format: a capability is a BLAKE3 key, 32 bytes — the width of the wire's `Enrolled { secret }` and
/// `Attest { proof }`, and of the anchor's issuer secret (`ISSUER_SECRET_BYTES`): one width for every
/// keyed proof of §4.13.
pub const CAPABILITY_BYTES: usize = 32;

/// A consumer's secret capability (§4.13), as `Enroll` returns it once to the human.
pub type Capability = [u8; CAPABILITY_BYTES];

/// Format: the record's magic, `SLCD` in little-endian ASCII ("slates consumer delivery").
const MAGIC: u32 = 0x4443_4C53;
/// Format: the record: magic (4), consumer id (8, little-endian), capability (32), CRC32C of the 44
/// bytes before it (4, little-endian).
const RECORD_BYTES: usize = 48;
/// Format: the consumer id's offset in the record.
const AT_CONSUMER: usize = 4;
/// Format: the capability's offset in the record.
const AT_CAPABILITY: usize = 12;
/// Format: the checksum's offset in the record (what it covers ends here).
const AT_CHECKSUM: usize = 44;

/// What the harness delivered: the consumer this process runs as, and the capability that proves it.
#[derive(Clone, PartialEq, Eq)]
pub struct Delivered {
  /// The consumer id `Enroll` minted.
  pub consumer: u64,
  /// The capability shown once to the human, delivered here and nowhere else.
  pub capability: Capability,
}

impl std::fmt::Debug for Delivered {
  /// The consumer only: the capability is a secret and never rendered.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Delivered")
      .field("consumer", &self.consumer)
      .finish_non_exhaustive()
  }
}

/// The proof a workload presents to bind its channel to an enrolled consumer (§4.13 "a consumer
/// channel is bound at rendezvous using a capability delivered and retained outside other agents'
/// reach"): the keyed hash, under the consumer's capability, of the client id the daemon assigned
/// this channel — so a proof captured from one session cannot bind another (the id differs), and the
/// capability itself never crosses the ring. The daemon recomputes it (`verify_attestation`), so
/// this is the one definition of the formula for both sides.
pub fn attest_proof(capability: &Capability, client_id: u32) -> [u8; CAPABILITY_BYTES] {
  let mut hasher = blake3::Hasher::new_keyed(capability);
  hasher.update(&client_id.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// Why a delivered capability could not be taken (the closed taxonomy of the delivery channel; each
/// is carried as `IpcError::CapabilityNotDelivered`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFault {
  /// No [`ENV_CONSUMER_FD`] in the environment: this process was not spawned as a consumer.
  Absent,
  /// The variable's value is not a delivery name: not a descriptor number followed by its channel's
  /// identity in the platform's format ([`ENV_CONSUMER_FD`]).
  Malformed,
  /// The delivery is not here: no descriptor is open at the name's number, or the one open there is
  /// not the named channel — the spawner left it close-on-exec (or out of the child's handle list), it
  /// was closed at an earlier take, or the variable is a stale copy a consumer's own process inherited,
  /// whose number now holds a descriptor of that process. Decided with calls that touch nothing: a
  /// descriptor that is not the channel is never adopted, read, changed or closed.
  NotInherited,
  /// The named channel is not a pipe or a socket end (a directory, a terminal, a file): not a harness
  /// channel, and never read.
  WrongKind,
  /// The channel did not hold exactly one record: a short read (the harness has not written it
  /// whole) or bytes past it; `got` is what was there.
  WrongLength {
    /// The bytes found.
    got: usize,
  },
  /// The record's magic or checksum does not match: not a record this crate wrote.
  Corrupt,
  /// The channel was at end of stream before any byte: the record was read already — by another
  /// process that inherited the same end — or the harness wrote nothing before closing it.
  AlreadyConsumed,
}

impl std::fmt::Display for DeliveryFault {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Absent => write!(f, "no {ENV_CONSUMER_FD} in the environment"),
      Self::Malformed => write!(
        f,
        "{ENV_CONSUMER_FD} is not a descriptor number and its channel's identity"
      ),
      Self::NotInherited => write!(
        f,
        "{ENV_CONSUMER_FD} names no channel this process inherited"
      ),
      Self::WrongKind => f.write_str("the delivery descriptor is not a pipe or a socket"),
      Self::WrongLength { got } => write!(
        f,
        "the delivery holds {got} bytes, not one record of {RECORD_BYTES}"
      ),
      Self::Corrupt => f.write_str("the delivery record's magic or checksum does not match"),
      Self::AlreadyConsumed => f.write_str("the delivery was consumed already (end of stream)"),
    }
  }
}

/// The record as the harness writes it: the fields in order, then the CRC32C of everything before it.
fn encode(consumer: u64, capability: &Capability) -> [u8; RECORD_BYTES] {
  let mut record = [0u8; RECORD_BYTES];
  record[..AT_CONSUMER].copy_from_slice(&MAGIC.to_le_bytes());
  record[AT_CONSUMER..AT_CAPABILITY].copy_from_slice(&consumer.to_le_bytes());
  record[AT_CAPABILITY..AT_CHECKSUM].copy_from_slice(capability);
  let checksum = slates_wire::crc32c::crc32c(&record[..AT_CHECKSUM]);
  record[AT_CHECKSUM..].copy_from_slice(&checksum.to_le_bytes());
  record
}

/// The record as the workload reads it: the checksum is verified, then the magic, before a field is
/// decoded (CLAUDE.md §3: a parser of external bytes verifies the checksum before decoding).
fn decode(record: &[u8; RECORD_BYTES]) -> Result<Delivered, DeliveryFault> {
  let mut checksum = [0u8; size_of::<u32>()];
  checksum.copy_from_slice(&record[AT_CHECKSUM..]);
  if u32::from_le_bytes(checksum) != slates_wire::crc32c::crc32c(&record[..AT_CHECKSUM]) {
    return Err(DeliveryFault::Corrupt);
  }
  let mut magic = [0u8; size_of::<u32>()];
  magic.copy_from_slice(&record[..AT_CONSUMER]);
  if u32::from_le_bytes(magic) != MAGIC {
    return Err(DeliveryFault::Corrupt);
  }
  let mut consumer = [0u8; size_of::<u64>()];
  consumer.copy_from_slice(&record[AT_CONSUMER..AT_CAPABILITY]);
  let mut capability = [0u8; CAPABILITY_BYTES];
  capability.copy_from_slice(&record[AT_CAPABILITY..AT_CHECKSUM]);
  Ok(Delivered {
    consumer: u64::from_le_bytes(consumer),
    capability,
  })
}

/// Zeroes a scratch buffer that held the record, in a way the compiler keeps (the buffer is observed
/// after the fill), so the capability does not linger on the stack past the take.
fn zero(bytes: &mut [u8]) {
  bytes.fill(0);
  std::hint::black_box(bytes);
}

/// The harness's end of a delivery: a pipe holding one record, whose read end one child inherits.
/// Created by [`Delivery::prepare`], spent by [`Delivery::spawn`]; dropped unspent, it closes and the
/// record is gone.
pub struct Delivery {
  carrier: platform::Carrier,
}

impl std::fmt::Debug for Delivery {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Delivery")
  }
}

impl Delivery {
  /// Writes the record for `consumer` into a fresh channel both of whose ends are close-on-exec, and
  /// holds its read end for one child. The capability is copied into the record and the scratch is
  /// zeroed; the caller's copy is its own to keep or drop.
  pub fn prepare(consumer: u64, capability: &Capability) -> Result<Delivery, IpcError> {
    let mut record = encode(consumer, capability);
    let carrier = platform::Carrier::create(&record);
    zero(&mut record);
    Ok(Delivery { carrier: carrier? })
  }

  /// The value a child reads from [`ENV_CONSUMER_FD`]: the read end's number and its pipe's identity
  /// (Unix), or the values of the read end and its twin and the pipe's name (Windows). For a harness
  /// that spawns by its own means (Python's `pass_fds`, a Windows `handle_list`) rather than
  /// [`Delivery::spawn`]: such a spawner must make these descriptors, and only them, inheritable for
  /// the one child at the same numbers, and set the variable to this value.
  pub fn descriptor_name(&self) -> String {
    self.carrier.name()
  }

  /// Spawns `program args` as the consumer: the child inherits the read end and nothing else of this
  /// process's descriptors beyond its standard three; its standard streams (the output as `output`
  /// says), working directory and environment are this process's, with `environment` added (or
  /// replaced) and [`ENV_CONSUMER_FD`] set. Consumes the delivery: the record is readable once, by
  /// this child.
  pub fn spawn(
    self,
    program: &OsStr,
    args: &[OsString],
    environment: &[(&OsStr, &OsStr)],
    output: Output,
  ) -> Result<ConsumerChild, IpcError> {
    self
      .carrier
      .spawn(program, args, environment, output)
      .map(|inner| ConsumerChild { inner })
  }
}

/// Where a spawned workload's standard output goes: this process's (a workload run in the open), or a
/// channel the harness reads back with [`ConsumerChild::wait_with_output`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
  /// The child writes to this process's standard output.
  Inherit,
  /// The child's standard output is captured for the harness.
  Captured,
}

/// Shape: the chunk one read of a captured output takes — a page, the pipe's own granularity; a
/// larger chunk only sits unused for a workload that prints lines.
const OUTPUT_CHUNK: usize = 4096;

/// Appends `chunk` to `kept` while the total stays within `capacity`, counting every byte offered so
/// an overflow is reported with the size the workload produced.
fn keep_within(kept: &mut Vec<u8>, chunk: &[u8], capacity: usize, offered: &mut usize) {
  *offered = offered.saturating_add(chunk.len());
  let room = capacity.saturating_sub(kept.len());
  kept.extend_from_slice(&chunk[..chunk.len().min(room)]);
}

/// A workload [`Delivery::spawn`] started: waited for, or killed, by the harness that owns it.
pub struct ConsumerChild {
  inner: platform::ConsumerChild,
}

impl std::fmt::Debug for ConsumerChild {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ConsumerChild")
      .field("id", &self.inner.id())
      .finish()
  }
}

impl ConsumerChild {
  /// The child's process id.
  pub fn id(&self) -> u32 {
    self.inner.id()
  }

  /// Waits for the child to end; its exit code, or `None` when a signal ended it (Unix).
  pub fn wait(&mut self) -> Result<Option<i32>, IpcError> {
    self.inner.wait()
  }

  /// Reads the captured output to its end and then waits for the child: its exit code and the bytes
  /// (empty under [`Output::Inherit`]). The harness bounds what it holds: past `capacity` bytes the
  /// rest is drained and dropped — so the child never blocks on a full pipe — and, once the child has
  /// ended, the result is refused `PayloadTooLarge` with the size the workload produced.
  pub fn wait_with_output(&mut self, capacity: usize) -> Result<(Option<i32>, Vec<u8>), IpcError> {
    self.inner.wait_with_output(capacity)
  }

  /// Ends the child now.
  pub fn kill(&mut self) -> Result<(), IpcError> {
    self.inner.kill()
  }
}

/// The delivery this process was spawned with, taken once: the first call reads and closes the
/// descriptor; every later call answers the same, so each client the process opens binds to the same
/// consumer and no second read ever happens. Refused typed when the delivery is absent or unusable.
pub fn delivered() -> Result<&'static Delivered, IpcError> {
  static TAKEN: OnceLock<Result<Delivered, DeliveryFault>> = OnceLock::new();
  TAKEN
    .get_or_init(take_from_environment)
    .as_ref()
    .map_err(|fault| IpcError::CapabilityNotDelivered { fault: *fault })
}

/// Takes the record from the descriptor [`ENV_CONSUMER_FD`] names, once.
fn take_from_environment() -> Result<Delivered, DeliveryFault> {
  let Some(value) = std::env::var_os(ENV_CONSUMER_FD) else {
    return Err(DeliveryFault::Absent);
  };
  take_named(&value.to_string_lossy())
}

/// Takes the record from the descriptor `name` names (the value of [`ENV_CONSUMER_FD`]): checks its
/// kind, reads the one record without blocking, verifies it, closes the descriptor and zeroes the
/// scratch. The one-shot form; a client uses [`delivered`], which takes it once per process. After
/// this returns, whatever it returned, the descriptor is no longer open.
pub fn take_named(name: &str) -> Result<Delivered, DeliveryFault> {
  platform::take(name)
}

#[cfg(unix)]
mod platform {
  //! Unix: a pipe, `pre_exec` clearing close-on-exec on the one inherited end, `fstat` for the kind,
  //! non-blocking reads.

  use std::ffi::{OsStr, OsString};
  use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
  use std::os::unix::process::CommandExt;
  use std::process::{Command, Stdio};

  use rustix::fs::{FileType, OFlags};
  use rustix::io::{Errno, FdFlags};

  use super::{
    Delivered, DeliveryFault, ENV_CONSUMER_FD, OUTPUT_CHUNK, Output, RECORD_BYTES, decode,
    keep_within, zero,
  };
  use crate::error::IpcError;

  fn refused(call: &'static str, e: Errno) -> IpcError {
    IpcError::OsRefused {
      call,
      code: Some(e.raw_os_error()),
    }
  }

  fn refused_io(call: &'static str, e: &std::io::Error) -> IpcError {
    IpcError::OsRefused {
      call,
      code: e.raw_os_error(),
    }
  }

  /// A pipe whose two ends are close-on-exec from birth (`pipe2`): no child of any thread inherits
  /// either.
  #[cfg(not(target_vendor = "apple"))]
  fn pipe_close_on_exec() -> Result<(OwnedFd, OwnedFd), IpcError> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(|e| refused("pipe2", e))
  }

  /// Apple has no `pipe2`: the ends are marked close-on-exec by the two `fcntl` calls that follow the
  /// `pipe` at once. A child another thread spawns inside that window would inherit both ends — and
  /// the record is written only after both are marked, so it could read it. A harness on Apple must
  /// not spawn from another thread while it prepares a delivery; Linux and Windows have no window.
  #[cfg(target_vendor = "apple")]
  fn pipe_close_on_exec() -> Result<(OwnedFd, OwnedFd), IpcError> {
    let (read_end, write_end) = rustix::pipe::pipe().map_err(|e| refused("pipe", e))?;
    rustix::io::fcntl_setfd(&read_end, FdFlags::CLOEXEC).map_err(|e| refused("fcntl", e))?;
    rustix::io::fcntl_setfd(&write_end, FdFlags::CLOEXEC).map_err(|e| refused("fcntl", e))?;
    Ok((read_end, write_end))
  }

  /// The held read end, and the identity of its pipe ([`identity_of`]) taken once the record is in it.
  pub(super) struct Carrier {
    read_end: OwnedFd,
    identity: String,
  }

  /// A channel's identity as a delivery name carries it: the device, inode and modification time
  /// (seconds, then nanoseconds) of what `fd` refers to, colon-separated decimal. Stable once the
  /// record is written: nothing writes to the pipe again, and a read changes only its access time.
  /// The device and inode alone tell live pipes apart but not a dead pipe from its successor on macOS,
  /// which hands the dead pipe's pair to the next pipe at once (`st_dev` is 0 for every pipe and the
  /// inode is derived from the pipe's kernel address; 1,999 of 2,000 new pipes matched the one just
  /// closed, 2026-09-28); that successor's nanosecond modification time is later (2,459 ns in the
  /// measured case). Linux gives every pipe a fresh inode (0 of 2,000 repeated) while its coarse
  /// modification time can repeat; the four fields together separate them on both.
  pub(super) fn identity_of(fd: impl AsFd) -> Result<String, Errno> {
    rustix::fs::fstat(fd).map(|stat| identity_text(&stat))
  }

  /// The identity fields of one `fstat`, formatted as [`identity_of`] describes. Their integer types
  /// differ across Unixes; the text is the same wherever it is compared.
  fn identity_text(stat: &rustix::fs::Stat) -> String {
    format!(
      "{}:{}:{}:{}",
      stat.st_dev, stat.st_ino, stat.st_mtime, stat.st_mtime_nsec
    )
  }

  /// Format: the fields of an identity ([`identity_of`]).
  const IDENTITY_FIELDS: usize = 4;

  /// Whether `identity` has the shape [`identity_of`] writes: four integer fields.
  fn well_formed(identity: &str) -> bool {
    identity.split(':').count() == IDENTITY_FIELDS
      && identity
        .split(':')
        .all(|field| field.parse::<i128>().is_ok())
  }

  impl Carrier {
    pub(super) fn create(record: &[u8; RECORD_BYTES]) -> Result<Carrier, IpcError> {
      let (read_end, write_end) = pipe_close_on_exec()?;
      // One write: a fresh pipe's buffer holds at least `PIPE_BUF` bytes (512 by POSIX) and a write
      // of at most that many is atomic, so the record lands whole or the OS refuses.
      let written = rustix::io::write(&write_end, record).map_err(|e| refused("pipe write", e))?;
      if written != RECORD_BYTES {
        return Err(IpcError::Layout {
          reason: "the delivery record was not written whole",
        });
      }
      // Closing the write end is what makes the reader see end of stream right after the record.
      drop(write_end);
      let identity = identity_of(&read_end).map_err(|e| refused("fstat", e))?;
      Ok(Carrier { read_end, identity })
    }

    pub(super) fn name(&self) -> String {
      format!("{}:{}", self.read_end.as_raw_fd(), self.identity)
    }

    pub(super) fn spawn(
      self,
      program: &OsStr,
      args: &[OsString],
      environment: &[(&OsStr, &OsStr)],
      output: Output,
    ) -> Result<ConsumerChild, IpcError> {
      // The child's copy: a close-on-exec duplicate this call owns. Its number, with the pipe's identity
      // (a duplicate shares it), is what the child is told, and only the forked child clears the flag on
      // it — the parent's copies keep it.
      let inherited = self
        .read_end
        .try_clone()
        .map_err(|e| refused_io("dup", &e))?;
      let mut command = Command::new(program);
      command.args(args);
      for (name, value) in environment {
        command.env(name, value);
      }
      command.env(
        ENV_CONSUMER_FD,
        format!("{}:{}", inherited.as_raw_fd(), self.identity),
      );
      if output == Output::Captured {
        command.stdout(Stdio::piped());
      }
      // SAFETY: the closure runs in the forked child between `fork` and `exec` and does one thing —
      // `fcntl(F_SETFD, 0)` on the descriptor it owns: a single async-signal-safe syscall, no
      // allocation, no lock, no other shared state — which is what `pre_exec` requires. It clears
      // close-on-exec on this child's copy only; every descriptor of the parent keeps the flag, so
      // a child any other thread spawns inherits nothing of the delivery.
      unsafe {
        command.pre_exec(move || {
          rustix::io::fcntl_setfd(&inherited, FdFlags::empty())
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
        });
      }
      let child = command.spawn().map_err(|e| refused_io("spawn", &e))?;
      Ok(ConsumerChild { child })
    }
  }

  /// The spawned workload.
  pub(super) struct ConsumerChild {
    child: std::process::Child,
  }

  impl ConsumerChild {
    pub(super) fn id(&self) -> u32 {
      self.child.id()
    }

    pub(super) fn wait(&mut self) -> Result<Option<i32>, IpcError> {
      self
        .child
        .wait()
        .map(|status| status.code())
        .map_err(|e| refused_io("waitpid", &e))
    }

    pub(super) fn wait_with_output(
      &mut self,
      capacity: usize,
    ) -> Result<(Option<i32>, Vec<u8>), IpcError> {
      let mut kept = Vec::new();
      let mut offered = 0usize;
      if let Some(mut stdout) = self.child.stdout.take() {
        let mut chunk = [0u8; OUTPUT_CHUNK];
        loop {
          match std::io::Read::read(&mut stdout, &mut chunk) {
            Ok(0) => break,
            Ok(got) => keep_within(&mut kept, &chunk[..got], capacity, &mut offered),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(refused_io("read", &e)),
          }
        }
      }
      let code = self.wait()?;
      if offered > capacity {
        return Err(IpcError::PayloadTooLarge { offered, capacity });
      }
      Ok((code, kept))
    }

    pub(super) fn kill(&mut self) -> Result<(), IpcError> {
      self.child.kill().map_err(|e| refused_io("kill", &e))
    }
  }

  pub(super) fn take(name: &str) -> Result<Delivered, DeliveryFault> {
    let (number, identity) = name.split_once(':').ok_or(DeliveryFault::Malformed)?;
    let number: RawFd = number.parse().map_err(|_| DeliveryFault::Malformed)?;
    if number < 0 || !well_formed(identity) {
      return Err(DeliveryFault::Malformed);
    }
    let owned = adopt(number, identity)?;
    // Close-on-exec before anything else: from here on no exec from another thread carries the
    // capability along.
    rustix::io::fcntl_setfd(&owned, FdFlags::CLOEXEC).map_err(|_| DeliveryFault::NotInherited)?;
    // Non-blocking: the harness wrote the record before the spawn, so a read that would wait is a
    // broken harness — refused, never waited for.
    let flags = rustix::fs::fcntl_getfl(&owned).map_err(|_| DeliveryFault::WrongKind)?;
    rustix::fs::fcntl_setfl(&owned, flags | OFlags::NONBLOCK)
      .map_err(|_| DeliveryFault::WrongKind)?;
    let mut record = [0u8; RECORD_BYTES];
    let read = read_record(&owned, &mut record);
    let decoded = read.and_then(|()| decode(&record));
    zero(&mut record);
    drop(owned);
    decoded
  }

  /// Adopts the descriptor the harness named once it is known to be the named channel and a pipe or a
  /// socket end. The decision is one `fstat`, which touches nothing: a number that is not open, or is
  /// open on another channel (this process's own descriptor at a stale name's number), is
  /// `NotInherited` and left exactly as it was; the named channel of another kind is `WrongKind`.
  fn adopt(number: RawFd, identity: &str) -> Result<OwnedFd, DeliveryFault> {
    // SAFETY: the number is checked here before any other use: `fstat` on a number that is not open
    // fails with `EBADF` and touches nothing, and the borrow lasts for that one call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(number) };
    let stat = rustix::fs::fstat(borrowed).map_err(|_| DeliveryFault::NotInherited)?;
    if identity_text(&stat) != identity {
      return Err(DeliveryFault::NotInherited);
    }
    match FileType::from_raw_mode(stat.st_mode) {
      FileType::Fifo | FileType::Socket => {}
      _ => return Err(DeliveryFault::WrongKind),
    }
    // SAFETY: the descriptor is open (`fstat` succeeded) and is the very channel the harness named
    // (its device, inode and modification time match the name), handed to this process for this one
    // take — so adopting it makes the caller its only owner, which closes it when the take ends.
    Ok(unsafe { OwnedFd::from_raw_fd(number) })
  }

  /// Reads exactly one record and confirms nothing follows it, without ever blocking.
  fn read_record(fd: &OwnedFd, record: &mut [u8; RECORD_BYTES]) -> Result<(), DeliveryFault> {
    let mut got = 0usize;
    while got < RECORD_BYTES {
      match rustix::io::read(fd, &mut record[got..]) {
        Ok(0) if got == 0 => return Err(DeliveryFault::AlreadyConsumed),
        Ok(0) => return Err(DeliveryFault::WrongLength { got }),
        Ok(n) => got += n,
        Err(Errno::INTR) => {}
        Err(Errno::AGAIN) => return Err(DeliveryFault::WrongLength { got }),
        Err(_) => return Err(DeliveryFault::WrongKind),
      }
    }
    let mut extra = [0u8; 1];
    loop {
      match rustix::io::read(fd, &mut extra) {
        Ok(0) | Err(Errno::AGAIN) => return Ok(()),
        Ok(n) => return Err(DeliveryFault::WrongLength { got: got + n }),
        Err(Errno::INTR) => {}
        Err(_) => return Err(DeliveryFault::WrongKind),
      }
    }
  }
}

#[cfg(windows)]
mod platform {
  //! Windows: a byte pipe with a unique name and one instance, `CreateProcessW` with a
  //! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming the two inherited handles of its read end (and the
  //! standard three), the pipe's identity confirmed before adoption (the module's header says why and
  //! how), `PeekNamedPipe` for a read that never blocks. The standard library's `Command` cannot restrict inheritance —
  //! it passes `bInheritHandles` and the handle-list attribute is unstable on the pinned toolchain
  //! (`windows_process_extensions_raw_attribute`, rust-lang/rust#114854) — so the spawn is made here,
  //! the way `crates/rt`'s AFD reactor and `bridge-winfsp` make their Win32 calls: hand-transcribed
  //! from the headers windows-sys binds, each call with its invariant stated.

  use std::ffi::{OsStr, OsString};
  use std::os::windows::ffi::OsStrExt;
  use std::ptr::{null, null_mut};
  use std::sync::atomic::{AtomicU64, Ordering};

  use windows_sys::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE,
    FILETIME, GENERIC_WRITE, GetHandleInformation, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, MAX_PATH, SetHandleInformation, WAIT_OBJECT_0,
  };
  use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE,
    FILE_TYPE_PIPE, FileNameInfo, GetFileInformationByHandleEx, GetFileType, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
  };
  use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
  };
  use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, CreatePipe, NAMED_PIPE_MODE, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, PeekNamedPipe, SetNamedPipeHandleState,
  };
  use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, INFINITE,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
  };

  use super::{
    Delivered, DeliveryFault, ENV_CONSUMER_FD, OUTPUT_CHUNK, Output, RECORD_BYTES, decode,
    keep_within, zero,
  };
  use crate::error::IpcError;

  /// The calling thread's last Win32 error, as the refusal's code.
  fn last_error() -> Option<i32> {
    // SAFETY: a thread-local read with no preconditions.
    let code = unsafe { GetLastError() };
    Some(i32::try_from(code).unwrap_or(i32::MAX))
  }

  fn refused(call: &'static str) -> IpcError {
    IpcError::OsRefused {
      call,
      code: last_error(),
    }
  }

  /// A handle this module created or duplicated, closed when it goes out of scope.
  struct ClosedOnDrop(HANDLE);

  impl Drop for ClosedOnDrop {
    fn drop(&mut self) {
      if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
        // SAFETY: a live handle this module owns and nothing else closes.
        unsafe { CloseHandle(self.0) };
      }
    }
  }

  /// Whether a duplicate is inheritable (the `bInheritHandle` of `DuplicateHandle`).
  #[derive(Clone, Copy)]
  enum Inheritance {
    /// Only this process holds it.
    Kept,
    /// A child created with it in its handle list inherits it.
    Inheritable,
  }

  /// A duplicate of `handle` in this process — the same object, a new handle — which the caller
  /// closes.
  fn duplicate(handle: HANDLE, inheritance: Inheritance) -> Result<ClosedOnDrop, IpcError> {
    let mut duplicate: HANDLE = null_mut();
    let inherit = match inheritance {
      Inheritance::Kept => 0,
      Inheritance::Inheritable => 1,
    };
    // SAFETY: the current process's pseudo-handle needs no closing; `handle` is live; the
    // out-pointer is a local the call fills.
    let ok = unsafe {
      DuplicateHandle(
        GetCurrentProcess(),
        handle,
        GetCurrentProcess(),
        &mut duplicate,
        0,
        inherit,
        DUPLICATE_SAME_ACCESS,
      )
    };
    if ok == 0 {
      return Err(refused("DuplicateHandle"));
    }
    Ok(ClosedOnDrop(duplicate))
  }

  /// An inheritable duplicate of `handle` in this process, which the caller closes after the spawn.
  fn duplicate_inheritable(handle: HANDLE) -> Result<ClosedOnDrop, IpcError> {
    duplicate(handle, Inheritance::Inheritable)
  }

  /// An inheritable duplicate of one of this process's standard handles, or none when the process
  /// has no such handle (no console, a closed stream).
  fn standard_handle_inheritable(which: STD_HANDLE) -> Result<Option<ClosedOnDrop>, IpcError> {
    // SAFETY: a query with no preconditions; the answer may be null or invalid, checked below.
    let handle = unsafe { GetStdHandle(which) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
      return Ok(None);
    }
    duplicate_inheritable(handle).map(Some)
  }

  /// The `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming exactly the handles the child inherits (Raymond
  /// Chen, "Programmatically controlling which handles are inherited by new processes in Win32" [D]).
  /// The list's storage is pointer-aligned and the handle array outlives the list, as the attribute
  /// keeps a pointer to it until the list is deleted (on drop).
  struct AttributeList {
    storage: Vec<usize>,
    handles: Vec<HANDLE>,
  }

  impl AttributeList {
    fn new(handles: Vec<HANDLE>) -> Result<AttributeList, IpcError> {
      let mut size: usize = 0;
      // SAFETY: the documented size query — a null list with a count of one fills `size` and fails
      // with ERROR_INSUFFICIENT_BUFFER, which is the expected answer here.
      unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
      if size == 0 {
        return Err(refused("InitializeProcThreadAttributeList"));
      }
      let mut list = AttributeList {
        storage: vec![0usize; size.div_ceil(size_of::<usize>())],
        handles,
      };
      // SAFETY: `storage` holds `size` bytes, pointer-aligned, and lives as long as the list.
      let ok = unsafe {
        InitializeProcThreadAttributeList(list.storage.as_mut_ptr().cast(), 1, 0, &mut size)
      };
      if ok == 0 {
        list.storage.clear();
        return Err(refused("InitializeProcThreadAttributeList"));
      }
      let bytes = list.handles.len() * size_of::<HANDLE>();
      // SAFETY: the list is initialized; the handle array is live for the list's whole life (it is
      // owned beside it and dropped after the delete); the attribute id and byte length are the
      // documented ones.
      let ok = unsafe {
        UpdateProcThreadAttribute(
          list.storage.as_mut_ptr().cast(),
          0,
          usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST).unwrap_or(0),
          list.handles.as_ptr().cast(),
          bytes,
          null_mut(),
          null(),
        )
      };
      if ok == 0 {
        return Err(refused("UpdateProcThreadAttribute"));
      }
      Ok(list)
    }

    fn pointer(&mut self) -> *mut core::ffi::c_void {
      self.storage.as_mut_ptr().cast()
    }
  }

  impl Drop for AttributeList {
    fn drop(&mut self) {
      if !self.storage.is_empty() {
        // SAFETY: an initialized list this struct owns; deleted once, here.
        unsafe { DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast()) };
      }
    }
  }

  /// Appends `argument` to a command line under the C runtime's parsing rules (Microsoft, "Parsing
  /// C++ command-line arguments" [B]): an argument with a space, a tab, a quote — or nothing at all —
  /// is quoted; a quote inside is escaped with a backslash; backslashes are doubled only where they
  /// precede a quote or the closing quote.
  fn append_quoted(line: &mut Vec<u16>, argument: &OsStr) {
    let quote = u16::from(b'"');
    let backslash = u16::from(b'\\');
    let units: Vec<u16> = argument.encode_wide().collect();
    let needs_quotes = units.is_empty()
      || units
        .iter()
        .any(|unit| *unit == u16::from(b' ') || *unit == u16::from(b'\t') || *unit == quote);
    if needs_quotes {
      line.push(quote);
    }
    let mut backslashes = 0usize;
    for unit in units {
      if unit == backslash {
        backslashes += 1;
        line.push(unit);
        continue;
      }
      if unit == quote {
        line.extend(std::iter::repeat_n(backslash, backslashes + 1));
      }
      backslashes = 0;
      line.push(unit);
    }
    if needs_quotes {
      line.extend(std::iter::repeat_n(backslash, backslashes));
      line.push(quote);
    }
  }

  /// The command line `CreateProcessW` parses back into the child's `argv`, null-terminated; the
  /// program is its first token, which the call resolves as a shell would (the application's
  /// directory, the working directory, the system directories, then `PATH`).
  fn command_line_of(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut line: Vec<u16> = Vec::new();
    append_quoted(&mut line, program);
    for argument in args {
      line.push(u16::from(b' '));
      append_quoted(&mut line, argument);
    }
    line.push(0);
    line
  }

  /// Whether two variable names are the same to the environment's reader (case-insensitively).
  fn same_name(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy().to_uppercase() == b.to_string_lossy().to_uppercase()
  }

  /// The child's environment block: this process's variables with `added` and the delivery variable
  /// set (replacing any same-named one), sorted by name without regard to case as the block's reader
  /// requires — `NAME=VALUE\0` per entry and a final `\0`.
  fn environment_block(added: &[(&OsStr, &OsStr)], descriptor_name: &str) -> Vec<u16> {
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let mut set = |name: &OsStr, value: OsString| {
      variables.retain(|(existing, _)| !same_name(existing, name));
      variables.push((name.to_os_string(), value));
    };
    for (name, value) in added {
      set(name, (*value).to_os_string());
    }
    set(OsStr::new(ENV_CONSUMER_FD), OsString::from(descriptor_name));
    variables.sort_by_cached_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (name, value) in variables {
      block.extend(name.encode_wide());
      block.push(u16::from(b'='));
      block.extend(value.encode_wide());
      block.push(0);
    }
    block.push(0);
    block
  }

  /// An anonymous pipe, both ends non-inheritable: (read end, write end).
  fn create_pipe() -> Result<(ClosedOnDrop, ClosedOnDrop), IpcError> {
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    // SAFETY: two out-pointers to locals the call fills; no security attributes (null) makes both
    // ends non-inheritable — the default the delivery relies on; a size of zero is the system's
    // default buffer, larger than one record.
    let ok = unsafe { CreatePipe(&mut read, &mut write, null(), 0) };
    if ok == 0 {
      return Err(refused("CreatePipe"));
    }
    Ok((ClosedOnDrop(read), ClosedOnDrop(write)))
  }

  /// Format: the first part of every delivery pipe's name; the rest names the harness process (its
  /// id and creation time) and the delivery's ordinal within it, so no two pipes ever share a name.
  const PIPE_PREFIX: &str = "slates-delivery-";
  /// Format: the local machine's pipe namespace, where a pipe's name is opened.
  const PIPE_NAMESPACE: &str = r"\\.\pipe\";
  /// Shape: the ordinal of the next delivery this process prepares — one per spawned workload, a cold
  /// path, so one relaxed counter.
  static NEXT_DELIVERY: AtomicU64 = AtomicU64::new(0);

  /// This process's creation time (the 100-nanosecond count `GetProcessTimes` reports): with the
  /// process id it names this process among every process the machine has run, since ids are reused.
  fn process_creation() -> Result<u64, IpcError> {
    let empty = FILETIME {
      dwLowDateTime: 0,
      dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (empty, empty, empty, empty);
    // SAFETY: this process's pseudo-handle needs no closing; the four out-pointers are live locals.
    let ok = unsafe {
      GetProcessTimes(
        GetCurrentProcess(),
        &mut creation,
        &mut exit,
        &mut kernel,
        &mut user,
      )
    };
    if ok == 0 {
      return Err(refused("GetProcessTimes"));
    }
    Ok((u64::from(creation.dwHighDateTime) << u32::BITS) | u64::from(creation.dwLowDateTime))
  }

  /// A delivery pipe's name, unique across processes (this process's id and creation time) and within
  /// this one (the ordinal).
  fn fresh_pipe_name() -> Result<String, IpcError> {
    let ordinal = NEXT_DELIVERY.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
      "{PIPE_PREFIX}{}-{}-{ordinal}",
      std::process::id(),
      process_creation()?
    ))
  }

  /// A byte pipe named `pipe_name` with one instance, both ends non-inheritable: its server end, which
  /// reads (the end a child inherits), and its client end, which writes the record. The name is new
  /// (`FILE_FLAG_FIRST_PIPE_INSTANCE` refuses a name another process already holds) and the one
  /// instance is connected to this process's own client before the name is published anywhere, so no
  /// other process can connect to it; remote clients are refused outright. A named pipe instead of
  /// `CreatePipe`'s anonymous one only so the name can identify it: an anonymous pipe is the same
  /// server/client pair with no name to read back.
  fn create_named_pipe(pipe_name: &str) -> Result<(ClosedOnDrop, ClosedOnDrop), IpcError> {
    let path: Vec<u16> = format!("{PIPE_NAMESPACE}{pipe_name}")
      .encode_utf16()
      .chain(std::iter::once(0))
      .collect();
    let mode: NAMED_PIPE_MODE =
      PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    // Shape: one instance (the delivery is one channel); no outbound quota (the server end only
    // reads); the inbound quota holds exactly the one record; the default wait (zero), which only
    // `WaitNamedPipe` consults and nothing here calls.
    let (instances, outbound_quota, default_wait) = (1, 0, 0);
    let inbound_quota = u32::try_from(RECORD_BYTES).unwrap_or(u32::MAX);
    // SAFETY: the NUL-terminated path outlives the call; no security attributes (null) makes the
    // handle non-inheritable and gives the default security of the creator.
    let server = unsafe {
      CreateNamedPipeW(
        path.as_ptr(),
        PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
        mode,
        instances,
        outbound_quota,
        inbound_quota,
        default_wait,
        null(),
      )
    };
    if server == INVALID_HANDLE_VALUE {
      return Err(refused("CreateNamedPipeW"));
    }
    let server = ClosedOnDrop(server);
    // SAFETY: the path outlives the call; write access, no sharing, no security attributes (not
    // inheritable); the pipe exists, just created, and its one instance is listening.
    let client = unsafe {
      CreateFileW(
        path.as_ptr(),
        GENERIC_WRITE,
        FILE_SHARE_NONE,
        null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        null_mut(),
      )
    };
    if client == INVALID_HANDLE_VALUE {
      return Err(refused("CreateFileW"));
    }
    Ok((server, ClosedOnDrop(client)))
  }

  /// Writes the record through the client end in non-blocking mode, so a quota that could not hold it
  /// shows as a short write, refused, instead of a wait for a reader that has not been spawned.
  fn write_record(write_end: &ClosedOnDrop, record: &[u8; RECORD_BYTES]) -> Result<(), IpcError> {
    let mode: NAMED_PIPE_MODE = PIPE_READMODE_BYTE | PIPE_NOWAIT;
    // SAFETY: the client end this module owns, opened for writing; the mode is a live local; the two
    // collection settings stay as they are (null).
    if unsafe { SetNamedPipeHandleState(write_end.0, &mode, null(), null()) } == 0 {
      return Err(refused("SetNamedPipeHandleState"));
    }
    let mut written = 0u32;
    // SAFETY: the buffer is the record, live for the call and of the length passed; the write
    // end is a live handle this function owns; a null OVERLAPPED makes the write synchronous.
    let ok = unsafe {
      WriteFile(
        write_end.0,
        record.as_ptr(),
        u32::try_from(RECORD_BYTES).unwrap_or(u32::MAX),
        &mut written,
        null_mut(),
      )
    };
    if ok == 0 || usize::try_from(written).unwrap_or(0) != RECORD_BYTES {
      return Err(refused("WriteFile"));
    }
    Ok(())
  }

  /// The held read end, a twin handle of it (the same object, so a name can prove the pair), and the
  /// pipe's name.
  pub(super) struct Carrier {
    read_end: ClosedOnDrop,
    twin: ClosedOnDrop,
    pipe_name: String,
  }

  impl Carrier {
    pub(super) fn create(record: &[u8; RECORD_BYTES]) -> Result<Carrier, IpcError> {
      let pipe_name = fresh_pipe_name()?;
      let (read_end, write_end) = create_named_pipe(&pipe_name)?;
      write_record(&write_end, record)?;
      // Closing the write end is what makes the reader see end of stream right after the record.
      drop(write_end);
      let twin = duplicate(read_end.0, Inheritance::Kept)?;
      Ok(Carrier {
        read_end,
        twin,
        pipe_name,
      })
    }

    pub(super) fn name(&self) -> String {
      delivery_name(&self.read_end, &self.twin, &self.pipe_name)
    }

    pub(super) fn spawn(
      self,
      program: &OsStr,
      args: &[OsString],
      environment: &[(&OsStr, &OsStr)],
      output: Output,
    ) -> Result<ConsumerChild, IpcError> {
      // The child's copies: two inheritable duplicates of the read end that exist for the length of
      // this call only; their values and the pipe's name are what the child is told (inheritance
      // keeps a handle's value). Between here and the create, a `CreateProcess` from another thread
      // of this process that inherits handles would carry them along — the window Win32 leaves every
      // handle list (Chen [D]).
      let inherited = duplicate_inheritable(self.read_end.0)?;
      let inherited_twin = duplicate_inheritable(self.read_end.0)?;
      let stdin = standard_handle_inheritable(STD_INPUT_HANDLE)?;
      // A captured output is a pipe whose write end the child inherits (as its standard output) and
      // whose read end this process keeps; the parent's own copies of the write end close after the
      // spawn, so the read end sees end of stream when the child ends.
      let captured = match output {
        Output::Captured => Some(create_pipe()?),
        Output::Inherit => None,
      };
      let stdout = match &captured {
        Some((_, write_end)) => Some(duplicate_inheritable(write_end.0)?),
        None => standard_handle_inheritable(STD_OUTPUT_HANDLE)?,
      };
      let stderr = standard_handle_inheritable(STD_ERROR_HANDLE)?;
      // SAFETY: a plain-data record every zero of which is a valid field value.
      let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
      startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0);
      startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
      let mut handles: Vec<HANDLE> = vec![inherited.0, inherited_twin.0];
      for (slot, duplicate) in [
        (&mut startup.StartupInfo.hStdInput, &stdin),
        (&mut startup.StartupInfo.hStdOutput, &stdout),
        (&mut startup.StartupInfo.hStdError, &stderr),
      ] {
        if let Some(duplicate) = duplicate {
          *slot = duplicate.0;
          handles.push(duplicate.0);
        }
      }
      let mut list = AttributeList::new(handles)?;
      startup.lpAttributeList = list.pointer();
      let mut command_line = command_line_of(program, args);
      let environment_block = environment_block(
        environment,
        &delivery_name(&inherited, &inherited_twin, &self.pipe_name),
      );
      // SAFETY: a plain-data record the call fills.
      let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
      // SAFETY: the command line is a mutable null-terminated buffer (the call may edit it); the
      // environment block is a well-formed double-null-terminated wide block; the startup record
      // is the extended form with its attribute list live, and the flag says so; inheritance is
      // on, narrowed to the list by the attribute.
      let ok = unsafe {
        CreateProcessW(
          null(),
          command_line.as_mut_ptr(),
          null(),
          null(),
          1,
          EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
          environment_block.as_ptr().cast(),
          null(),
          &startup.StartupInfo,
          &mut information,
        )
      };
      let error = if ok == 0 {
        Some(refused("CreateProcessW"))
      } else {
        None
      };
      // The list, the duplicates and this process's own read end are closed now: the child holds
      // its own copies, and nothing of the delivery stays open here.
      drop(list);
      drop(stdin);
      drop(stdout);
      drop(stderr);
      drop(inherited_twin);
      drop(inherited);
      drop(self);
      let captured_read_end = captured.map(|(read_end, write_end)| {
        drop(write_end);
        read_end
      });
      if let Some(error) = error {
        return Err(error);
      }
      drop(ClosedOnDrop(information.hThread));
      Ok(ConsumerChild {
        process: ClosedOnDrop(information.hProcess),
        id: information.dwProcessId,
        stdout: captured_read_end,
      })
    }
  }

  /// The spawned workload.
  pub(super) struct ConsumerChild {
    process: ClosedOnDrop,
    id: u32,
    /// The read end of the captured output, until it is read to its end.
    stdout: Option<ClosedOnDrop>,
  }

  impl ConsumerChild {
    pub(super) fn id(&self) -> u32 {
      self.id
    }

    pub(super) fn wait_with_output(
      &mut self,
      capacity: usize,
    ) -> Result<(Option<i32>, Vec<u8>), IpcError> {
      let mut kept = Vec::new();
      let mut offered = 0usize;
      if let Some(stdout) = self.stdout.take() {
        let mut chunk = [0u8; OUTPUT_CHUNK];
        loop {
          let mut got = 0u32;
          // SAFETY: the buffer is live and of the length passed; the read end is a live handle this
          // struct owns; a synchronous read on a pipe returns when bytes arrive or every writer is
          // gone (ERROR_BROKEN_PIPE, the end of the stream).
          let ok = unsafe {
            ReadFile(
              stdout.0,
              chunk.as_mut_ptr(),
              u32::try_from(OUTPUT_CHUNK).unwrap_or(u32::MAX),
              &mut got,
              null_mut(),
            )
          };
          if ok == 0 {
            // SAFETY: a thread-local read with no preconditions.
            if unsafe { GetLastError() } == ERROR_BROKEN_PIPE {
              break;
            }
            return Err(refused("ReadFile"));
          }
          let got = usize::try_from(got).unwrap_or(0);
          if got == 0 {
            break;
          }
          keep_within(&mut kept, &chunk[..got], capacity, &mut offered);
        }
      }
      let code = self.wait()?;
      if offered > capacity {
        return Err(IpcError::PayloadTooLarge { offered, capacity });
      }
      Ok((code, kept))
    }

    pub(super) fn wait(&mut self) -> Result<Option<i32>, IpcError> {
      // SAFETY: a live process handle this struct owns; the wait returns when the process ends.
      let waited = unsafe { WaitForSingleObject(self.process.0, INFINITE) };
      if waited != WAIT_OBJECT_0 {
        return Err(refused("WaitForSingleObject"));
      }
      let mut code = 0u32;
      // SAFETY: the out-pointer is a live local; the handle is the process's.
      let ok = unsafe { GetExitCodeProcess(self.process.0, &mut code) };
      if ok == 0 {
        return Err(refused("GetExitCodeProcess"));
      }
      Ok(Some(i32::from_ne_bytes(code.to_ne_bytes())))
    }

    pub(super) fn kill(&mut self) -> Result<(), IpcError> {
      // SAFETY: the process handle this struct owns.
      let ok = unsafe { TerminateProcess(self.process.0, 1) };
      if ok == 0 {
        return Err(refused("TerminateProcess"));
      }
      Ok(())
    }
  }

  /// A delivery name as [`super::ENV_CONSUMER_FD`] carries it on Windows: the two handles' values and
  /// the pipe's name.
  fn delivery_name(handle: &ClosedOnDrop, twin: &ClosedOnDrop, pipe_name: &str) -> String {
    format!(
      "{}:{}:{pipe_name}",
      handle.0.expose_provenance(),
      twin.0.expose_provenance()
    )
  }

  /// Format: the two low bits of a kernel handle's value, which the kernel ignores and never sets
  /// (Chen, "Why are kernel HANDLEs always a multiple of four?" [D]); a named value with either set
  /// aliases another value, so it is refused as malformed.
  const HANDLE_TAG_BITS: usize = 0b11;

  /// Format: the fields of a Windows delivery name — the handle, its twin, the pipe's name.
  const NAME_FIELDS: usize = 3;

  /// The two handle values and the pipe name of a delivery name, or `Malformed`: two distinct nonzero
  /// kernel handle values and a pipe name of this module's making.
  fn parse(name: &str) -> Result<(HANDLE, HANDLE, &str), DeliveryFault> {
    let mut fields = name.splitn(NAME_FIELDS, ':');
    let mut value = || -> Result<usize, DeliveryFault> {
      let value: usize = fields
        .next()
        .and_then(|field| field.parse().ok())
        .ok_or(DeliveryFault::Malformed)?;
      if value == 0 || value & HANDLE_TAG_BITS != 0 {
        return Err(DeliveryFault::Malformed);
      }
      Ok(value)
    };
    let (handle, twin) = (value()?, value()?);
    let pipe_name = fields
      .next()
      .filter(|pipe_name| pipe_name.starts_with(PIPE_PREFIX) && !pipe_name.contains('\\'))
      .ok_or(DeliveryFault::Malformed)?;
    if handle == twin {
      return Err(DeliveryFault::Malformed);
    }
    Ok((
      std::ptr::with_exposed_provenance_mut(handle),
      std::ptr::with_exposed_provenance_mut(twin),
      pipe_name,
    ))
  }

  pub(super) fn take(name: &str) -> Result<Delivered, DeliveryFault> {
    let (handle, twin, pipe_name) = parse(name)?;
    let (owned, owned_twin) = adopt(handle, twin, pipe_name)?;
    // Not inheritable before anything else: an inherited handle keeps its inherit flag, and a
    // process this one creates from here on must not carry the capability along.
    for held in [&owned, &owned_twin] {
      // SAFETY: a live pipe handle this function owns.
      if unsafe { SetHandleInformation(held.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(DeliveryFault::NotInherited);
      }
    }
    let mut record = [0u8; RECORD_BYTES];
    let read = read_record(&owned, &mut record);
    let decoded = read.and_then(|()| decode(&record));
    zero(&mut record);
    drop(owned_twin);
    drop(owned);
    decoded
  }

  /// Adopts the two handles the harness named once they are known to be the named pipe, decided in an
  /// order that keeps every call that could wait off a handle that is not the delivery's: both values
  /// open (`GetHandleInformation`) and the one object (`CompareObjectHandles`, which reads the handle
  /// table and never waits — two handles of a process's own at exactly those values are one object
  /// only by a coincidence of two values), then a pipe (`GetFileType`), then the pipe's name, the one
  /// query that waits on a synchronous handle with a read pending on another thread. Every handle
  /// that fails a check is `NotInherited` and left exactly as it was.
  fn adopt(
    handle: HANDLE,
    twin: HANDLE,
    pipe_name: &str,
  ) -> Result<(ClosedOnDrop, ClosedOnDrop), DeliveryFault> {
    if !is_open(handle) || !is_open(twin) {
      return Err(DeliveryFault::NotInherited);
    }
    // SAFETY: two open handle values (checked above); the call compares the objects they refer to
    // and changes neither.
    if unsafe { CompareObjectHandles(handle, twin) } == 0 {
      return Err(DeliveryFault::NotInherited);
    }
    // SAFETY: an open handle (checked above); the call reports its device type and changes nothing.
    if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
      return Err(DeliveryFault::NotInherited);
    }
    if !pipe_is_named(handle, pipe_name) {
      return Err(DeliveryFault::NotInherited);
    }
    Ok((ClosedOnDrop(handle), ClosedOnDrop(twin)))
  }

  /// Whether a handle is open in this process at `handle`'s value.
  fn is_open(handle: HANDLE) -> bool {
    let mut flags = 0u32;
    // SAFETY: `GetHandleInformation` answers for any handle value, failing for one that is not open;
    // the out-pointer is a live local.
    unsafe { GetHandleInformation(handle, &mut flags) != 0 }
  }

  /// Format: the longest pipe name read back — `MAX_PATH` UTF-16 units, the bound the standard
  /// library's own pipe-name query uses (Rust std, `msys_tty_on` [C]); a delivery name is far shorter,
  /// and a name that does not fit is not a delivery's.
  const NAME_UNITS: usize = MAX_PATH as usize;

  /// Whether the pipe `handle` refers to is named `pipe_name`. `FileNameInfo` on a pipe answers its
  /// name under the pipe namespace (the part after the last backslash is compared, as the standard
  /// library's query does); a name longer than the buffer fails the call, which is a mismatch.
  fn pipe_is_named(handle: HANDLE, pipe_name: &str) -> bool {
    // The FILE_NAME_INFO record: its length word, then the name's units, held in u32s for alignment.
    let bytes = size_of::<u32>().saturating_add(NAME_UNITS.saturating_mul(size_of::<u16>()));
    let mut buffer = vec![0u32; bytes.div_ceil(size_of::<u32>())];
    let Ok(size) = u32::try_from(buffer.len().saturating_mul(size_of::<u32>())) else {
      return false;
    };
    // SAFETY: the buffer is live, u32-aligned (FILE_NAME_INFO's alignment) and of the size passed;
    // the handle is an open pipe (checked by the caller); the query changes nothing.
    let ok = unsafe {
      GetFileInformationByHandleEx(handle, FileNameInfo, buffer.as_mut_ptr().cast(), size)
    };
    if ok == 0 {
      return false;
    }
    let Some(length) = buffer
      .first()
      .and_then(|length| usize::try_from(*length).ok())
    else {
      return false;
    };
    let units: Vec<u16> = buffer
      .iter()
      .skip(1)
      .flat_map(|word| {
        let [first, second, third, fourth] = word.to_ne_bytes();
        [
          u16::from_ne_bytes([first, second]),
          u16::from_ne_bytes([third, fourth]),
        ]
      })
      .collect();
    let Some(name) = units.get(..length / size_of::<u16>()) else {
      return false;
    };
    let backslash = u16::from(b'\\');
    let leaf = name
      .rsplit(|unit| *unit == backslash)
      .next()
      .unwrap_or(name);
    leaf.iter().copied().eq(pipe_name.encode_utf16())
  }

  /// Reads exactly one record, after `PeekNamedPipe` has said exactly one is there, so the read never
  /// blocks; a pipe at end of stream with nothing in it is `AlreadyConsumed`.
  fn read_record(
    pipe: &ClosedOnDrop,
    record: &mut [u8; RECORD_BYTES],
  ) -> Result<(), DeliveryFault> {
    let mut available = 0u32;
    // SAFETY: no buffer (null, length zero) and one out-pointer for the byte count; the handle is a
    // live pipe end its owner holds for the call.
    let ok = unsafe {
      PeekNamedPipe(
        pipe.0,
        null_mut(),
        0,
        null_mut(),
        &mut available,
        null_mut(),
      )
    };
    if ok == 0 {
      // SAFETY: a thread-local read with no preconditions.
      let code = unsafe { GetLastError() };
      return Err(if code == ERROR_BROKEN_PIPE {
        DeliveryFault::AlreadyConsumed
      } else {
        DeliveryFault::WrongKind
      });
    }
    let available = usize::try_from(available).unwrap_or(usize::MAX);
    if available != RECORD_BYTES {
      return Err(DeliveryFault::WrongLength { got: available });
    }
    let mut got = 0u32;
    // SAFETY: the buffer is the record, live and of the length passed; the bytes are there, so the
    // synchronous read (null OVERLAPPED) returns at once.
    let ok = unsafe {
      ReadFile(
        pipe.0,
        record.as_mut_ptr(),
        u32::try_from(RECORD_BYTES).unwrap_or(u32::MAX),
        &mut got,
        null_mut(),
      )
    };
    let got = usize::try_from(got).unwrap_or(0);
    if ok == 0 || got != RECORD_BYTES {
      return Err(DeliveryFault::WrongLength { got });
    }
    Ok(())
  }
}

#[cfg(not(any(unix, windows)))]
mod platform {
  //! Other platforms: no delivery channel yet; a delivery cannot be prepared and a take is absent.

  use std::ffi::{OsStr, OsString};

  use super::{Delivered, DeliveryFault, Output, RECORD_BYTES};
  use crate::error::IpcError;

  pub(super) struct Carrier;

  impl Carrier {
    pub(super) fn create(_record: &[u8; RECORD_BYTES]) -> Result<Carrier, IpcError> {
      Err(IpcError::Unsupported {
        feature: "capability delivery",
      })
    }

    pub(super) fn name(&self) -> String {
      String::new()
    }

    pub(super) fn spawn(
      self,
      _program: &OsStr,
      _args: &[OsString],
      _environment: &[(&OsStr, &OsStr)],
      _output: Output,
    ) -> Result<ConsumerChild, IpcError> {
      Err(IpcError::Unsupported {
        feature: "capability delivery",
      })
    }
  }

  pub(super) struct ConsumerChild;

  impl ConsumerChild {
    pub(super) fn id(&self) -> u32 {
      0
    }

    pub(super) fn wait(&mut self) -> Result<Option<i32>, IpcError> {
      Err(IpcError::Unsupported {
        feature: "capability delivery",
      })
    }

    pub(super) fn wait_with_output(
      &mut self,
      _capacity: usize,
    ) -> Result<(Option<i32>, Vec<u8>), IpcError> {
      Err(IpcError::Unsupported {
        feature: "capability delivery",
      })
    }

    pub(super) fn kill(&mut self) -> Result<(), IpcError> {
      Err(IpcError::Unsupported {
        feature: "capability delivery",
      })
    }
  }

  pub(super) fn take(_name: &str) -> Result<Delivered, DeliveryFault> {
    Err(DeliveryFault::Absent)
  }
}

#[cfg(test)]
mod tests {
  use super::{
    AT_CHECKSUM, CAPABILITY_BYTES, Capability, DeliveryFault, RECORD_BYTES, attest_proof, decode,
    encode,
  };

  /// A capability with a recognizable pattern, for the vectors below.
  fn capability() -> Capability {
    let mut out = [0u8; CAPABILITY_BYTES];
    for (index, byte) in out.iter_mut().enumerate() {
      *byte = u8::try_from(index)
        .unwrap_or(0)
        .wrapping_mul(7)
        .wrapping_add(3);
    }
    out
  }

  /// The record's layout is pinned (a golden vector, CLAUDE.md §4): the magic bytes, the consumer id
  /// little-endian, the capability, the CRC32C little-endian; and it decodes back to what was encoded.
  #[test]
  fn the_record_encodes_to_its_pinned_layout_and_decodes_back() {
    let record = encode(0x0102_0304_0506_0708, &capability());
    assert_eq!(&record[..4], b"SLCD");
    assert_eq!(&record[4..12], &[8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(&record[12..44], &capability());
    assert_eq!(&record[44..], &[0x57, 0xd8, 0x7f, 0x45]);
    let decoded = decode(&record).unwrap();
    assert_eq!(decoded.consumer, 0x0102_0304_0506_0708);
    assert_eq!(decoded.capability, capability());
  }

  /// Hostile records: a flipped bit anywhere (in a field or in the checksum), or a foreign magic, is
  /// `Corrupt`; nothing is decoded from it.
  #[test]
  fn a_flipped_bit_or_a_foreign_magic_is_corrupt() {
    let record = encode(7, &capability());
    for position in 0..RECORD_BYTES {
      let mut flipped = record;
      flipped[position] ^= 0x40;
      assert_eq!(
        decode(&flipped),
        Err(DeliveryFault::Corrupt),
        "bit flipped at {position}"
      );
    }
    let mut foreign = record;
    foreign[..4].copy_from_slice(b"SLBT");
    let checksum = slates_wire::crc32c::crc32c(&foreign[..AT_CHECKSUM]);
    foreign[AT_CHECKSUM..].copy_from_slice(&checksum.to_le_bytes());
    assert_eq!(decode(&foreign), Err(DeliveryFault::Corrupt));
  }

  /// The attestation proof is the keyed BLAKE3 of the client id under the capability, pinned to the
  /// same vector the daemon's `landing::tests::the_enrollment_proofs_are_domain_separated_and_pinned`
  /// verifies against, so the workload's proof and the daemon's check cannot drift apart; and it
  /// depends on the client id, so a proof captured from one session does not bind another.
  #[test]
  fn the_attestation_proof_is_keyed_over_the_client_id_and_pinned() {
    let capability = [0x13u8; CAPABILITY_BYTES];
    let proof = attest_proof(&capability, 0x0000_0007);
    let expected = {
      let mut hasher = blake3::Hasher::new_keyed(&capability);
      hasher.update(&7u32.to_le_bytes());
      *hasher.finalize().as_bytes()
    };
    assert_eq!(proof, expected);
    assert_ne!(proof, attest_proof(&capability, 8));
    assert_ne!(proof, attest_proof(&[0x14u8; CAPABILITY_BYTES], 7));
  }

  /// The descriptor-level hostile inputs, on the platform whose descriptors a test can make: what a
  /// consumer finds when its harness is broken or hostile, each a typed fault and never a hang.
  #[cfg(unix)]
  mod descriptors {
    use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};

    use super::super::platform::identity_of;
    use super::super::{DeliveryFault, RECORD_BYTES, encode, take_named};

    /// The delivery name of `fd` as a harness writes it — its number and its channel's identity — with
    /// the descriptor handed over to the take, which owns it from here.
    fn named(fd: OwnedFd) -> String {
      let name = format!("{}:{}", fd.as_raw_fd(), identity_of(&fd).unwrap());
      let _ = fd.into_raw_fd();
      name
    }

    /// A pipe holding `bytes`, its write end closed; the read end's name (owned by the take).
    fn pipe_holding(bytes: &[u8]) -> String {
      let (read_end, write_end) = rustix::pipe::pipe().unwrap();
      if !bytes.is_empty() {
        assert_eq!(rustix::io::write(&write_end, bytes).unwrap(), bytes.len());
      }
      drop(write_end);
      named(read_end)
    }

    /// A pipe holding `bytes` whose write end stays open (the harness has not finished); returns the
    /// read end's name and the write end to keep alive.
    fn pipe_still_open(bytes: &[u8]) -> (String, OwnedFd) {
      let (read_end, write_end) = rustix::pipe::pipe().unwrap();
      if !bytes.is_empty() {
        assert_eq!(rustix::io::write(&write_end, bytes).unwrap(), bytes.len());
      }
      (named(read_end), write_end)
    }

    /// A whole record on a pipe takes, and the take closes the descriptor: the pipe's write end, kept
    /// here, then meets a pipe with no reader (`EPIPE`; the Rust runtime ignores `SIGPIPE`). Proved on
    /// the pipe rather than by a second take: the kernel hands a closed number to the next open, a
    /// parallel test's pipe or socket takes it, and a take that trusted the number adopted and closed
    /// *that* descriptor — whose owner then closed it again, which the runtime aborts on (CI run
    /// 36201084174: "IO Safety violation: owned file descriptor already closed"). The take now confirms
    /// the channel's identity first (`a_name_whose_number_holds_another_channel_leaves_it_untouched`).
    #[test]
    fn a_whole_record_takes_once_and_the_descriptor_is_closed() {
      let (name, write_end) = pipe_still_open(&encode(42, &[9u8; 32]));
      let delivered = take_named(&name).unwrap();
      assert_eq!(delivered.consumer, 42);
      assert_eq!(delivered.capability, [9u8; 32]);
      assert_eq!(
        rustix::io::write(&write_end, &[0]),
        Err(rustix::io::Errno::PIPE),
        "the take closed the pipe's only read end"
      );
    }

    /// A short record — truncated with the write end closed, or not yet whole with it open — is
    /// `WrongLength` with the bytes found, and never a wait.
    #[test]
    fn a_short_record_is_wrong_length_whether_closed_or_still_open() {
      let record = encode(1, &[1u8; 32]);
      let name = pipe_holding(&record[..20]);
      assert_eq!(
        take_named(&name),
        Err(DeliveryFault::WrongLength { got: 20 })
      );
      let (name, _write_end) = pipe_still_open(&record[..30]);
      assert_eq!(
        take_named(&name),
        Err(DeliveryFault::WrongLength { got: 30 })
      );
      let (name, _write_end) = pipe_still_open(&[]);
      assert_eq!(
        take_named(&name),
        Err(DeliveryFault::WrongLength { got: 0 })
      );
    }

    /// Bytes past one record are `WrongLength` too — a channel carrying more than the one record is
    /// not the harness's.
    #[test]
    fn an_oversize_delivery_is_wrong_length() {
      let mut bytes = encode(1, &[1u8; 32]).to_vec();
      bytes.push(0);
      let name = pipe_holding(&bytes);
      assert_eq!(
        take_named(&name),
        Err(DeliveryFault::WrongLength {
          got: RECORD_BYTES + 1
        })
      );
    }

    /// A record whose bytes were altered on the way is `Corrupt`.
    #[test]
    fn an_altered_record_is_corrupt() {
      let mut record = encode(1, &[1u8; 32]);
      record[13] ^= 1;
      let name = pipe_holding(&record);
      assert_eq!(take_named(&name), Err(DeliveryFault::Corrupt));
    }

    /// An empty channel at end of stream — read already by a twin process, or never written — is
    /// `AlreadyConsumed`.
    #[test]
    fn an_empty_closed_channel_is_already_consumed() {
      let name = pipe_holding(&[]);
      assert_eq!(take_named(&name), Err(DeliveryFault::AlreadyConsumed));
    }

    /// A directory, the null device (a character device, as a terminal is) and a regular file are
    /// the wrong kind, refused before a byte is read; a terminal too, where the test has one.
    #[test]
    fn a_directory_a_device_or_a_file_is_the_wrong_kind() {
      use rustix::fs::{Mode, OFlags};
      let directory =
        rustix::fs::open(".", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
      assert_eq!(take_named(&named(directory)), Err(DeliveryFault::WrongKind));
      let null = rustix::fs::open("/dev/null", OFlags::RDONLY, Mode::empty()).unwrap();
      assert_eq!(take_named(&named(null)), Err(DeliveryFault::WrongKind));
      let file = rustix::fs::open(
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        OFlags::RDONLY,
        Mode::empty(),
      )
      .unwrap();
      assert_eq!(take_named(&named(file)), Err(DeliveryFault::WrongKind));
      match rustix::fs::open("/dev/tty", OFlags::RDONLY, Mode::empty()) {
        Ok(tty) => assert_eq!(take_named(&named(tty)), Err(DeliveryFault::WrongKind)),
        Err(_) => eprintln!("skipping the terminal case: this process has no controlling terminal"),
      }
    }

    /// Hostile names are `Malformed` before any descriptor is looked at: not a number, a negative
    /// number, a bare number without its channel's identity (the form a take trusted before
    /// 2026-09-28), an identity of the wrong shape. A well-formed name whose number no descriptor is
    /// open at is `NotInherited`. The unopened number is the largest a descriptor can have, which no
    /// process table reaches (this box: `kern.maxfilesperproc` 245,760) — not a freshly closed number:
    /// the kernel hands the lowest free number to the next open, so a closed number is reused at once by
    /// a parallel test's pipe (2026-09-14: this test and
    /// `a_whole_record_takes_once_and_the_descriptor_is_closed` failed together, 2 of 17, once in an
    /// integration run and never in isolation).
    #[test]
    fn a_malformed_name_and_a_closed_number_are_typed() {
      for hostile in [
        "pipe",
        "-1",
        "-1:0:0:0:0",
        "3",
        "3:",
        "3:0:0:0",
        "3:0:0:0:0:0",
        "3:x:0:0:0",
      ] {
        assert_eq!(
          take_named(hostile),
          Err(DeliveryFault::Malformed),
          "{hostile:?}"
        );
      }
      assert_eq!(
        take_named(&format!("{}:0:0:0:0", i32::MAX)),
        Err(DeliveryFault::NotInherited)
      );
    }

    /// A name whose number holds a channel other than the one it identifies — a stale name in a
    /// process that reused the number for a pipe of its own — is `NotInherited`, and that pipe is left
    /// exactly as it was: the same pipe at the same number, its bytes unread, its status and
    /// descriptor flags unchanged. The pipe stays this test's, so no number is freed or reused.
    #[test]
    fn a_name_whose_number_holds_another_channel_leaves_it_untouched() {
      let (own_read, own_write) = rustix::pipe::pipe().unwrap();
      assert_eq!(rustix::io::write(&own_write, b"probe").unwrap(), 5);
      let (other_read, other_write) = rustix::pipe::pipe().unwrap();
      assert_eq!(
        rustix::io::write(&other_write, &encode(1, &[1u8; 32])).unwrap(),
        RECORD_BYTES
      );
      drop(other_write);
      let other_identity = identity_of(&other_read).unwrap();
      let own_identity = identity_of(&own_read).unwrap();
      let status = rustix::fs::fcntl_getfl(&own_read).unwrap();
      let descriptor_flags = rustix::io::fcntl_getfd(&own_read).unwrap();
      let stale = format!("{}:{other_identity}", own_read.as_raw_fd());
      assert_eq!(take_named(&stale), Err(DeliveryFault::NotInherited));
      assert_eq!(identity_of(&own_read).unwrap(), own_identity);
      assert_eq!(rustix::io::ioctl_fionread(&own_read).unwrap(), 5);
      assert_eq!(rustix::fs::fcntl_getfl(&own_read).unwrap(), status);
      assert_eq!(
        rustix::io::fcntl_getfd(&own_read).unwrap(),
        descriptor_flags
      );
      // The other channel is untouched too: its record is still whole behind its own name.
      assert_eq!(
        take_named(&named(other_read)).map(|delivered| delivered.consumer),
        Ok(1)
      );
    }
  }
}
