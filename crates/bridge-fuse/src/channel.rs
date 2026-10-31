//! The `/dev/fuse` transport (§4.6 "Linux (own /dev/fuse driver)"; Phase 3 task 1b). The kernel
//! and the daemon exchange messages over the character device `/dev/fuse`: a `read` returns one
//! request (or a batch the driver splits), a `write` sends one reply. This module owns the
//! device descriptor and turns it into the request/reply stream the [`crate::dispatch`] serves.
//!
//! Linux only: `/dev/fuse` and the FUSE ABI are the Linux kernel's. The serve loop and the
//! device I/O run in the CI Linux lane against a real mount; this file compiles and cross-lints
//! everywhere. The blocking loop here is the fallback the design names; the io_uring command
//! path is the driver's next piece (owed); a mount keeps one channel, on its volume's owner shard (A-62: per-shard
//! `FUSE_DEV_IOC_CLONE` channels measured and rejected), and
//! the mount establishment (the new mount API, or `fusermount3`) is [`crate::mount`].
//!
//! Kernel coherence (§4.6 "Cache posture"; AUD-02): the loop runs one delivery round
//! ([`crate::coherence`]) at **every wake** — a kernel request, or a change another mutation source
//! signalled through a [`ChangeSignal`] — asking the seam for every invalidation owed since the
//! cursor (a change through the SDK or another attachment, an outsider's change beneath a base
//! directory) and writing each to the device as an unsolicited notification, so the kernel never
//! answers a lookup or a stat from a cache newer than the daemon's view, and a change while the
//! kernel is answering from its cache is told without waiting for a request. The loop's own request
//! needs no invalidation in its own kernel, so the cursor is taken again after it — only when the
//! round before it was delivered whole; a refused gather keeps the cursor so the round is retried
//! rather than lost. What the kernel negotiated at `INIT` decides whether an entry is expired
//! (`FUSE_EXPIRE_ONLY`, the live-source case) or dropped. The wait is a `poll` over the device and
//! the signal; the owner that interleaves other work (a shard serving verbs) drives [`wait`] and
//! [`serve_step`] itself, and [`serve_blocking`] is the loop over them.
//!
//! No `unsafe`: the device is opened, read and written through rustix's I/O-safe wrappers over
//! an owned descriptor.

#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, OwnedFd};

use rustix::event::{EventfdFlags, PollFd, PollFlags};
use rustix::fs::{Mode, OFlags};

use crate::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use crate::allocate::{Allocation, Begun};
use crate::bridge::{Bridge, KernelChange, reply_error};
use crate::coherence::{
  KernelHolds, MOST_ADDED_BY_AN_UPDATE, OwnRecords, PageStates, RANGE_BYTES, WritableOpens,
};
use crate::dispatch;
use crate::error::FuseError;
use crate::init::negotiate;
use crate::notify::{EXPIRE_ONLY, inc_epoch, inval_entry, inval_inode};
use crate::reply::ReplyHeader;
use crate::request::Request;
use crate::session::Session;
use slates_bridge_core::delivery::{Coherence, Delivered};
use slates_bridge_core::{AttachmentId, Attachments, CachedBytes, Invalidation};

/// Format: the device the kernel's FUSE client and the daemon exchange messages over.
const FUSE_DEVICE: &str = "/dev/fuse";

/// Format: the journal sequence before the volume's first record (`slates_vfs::journal::OpLog` numbers records from 1):
/// a kernel told through it has been told of nothing ([`ServeState::told_through`]).
const TOLD_OF_NOTHING: u64 = 0;

/// Shape: the request buffer size: the negotiated maximum write (256 KiB) plus a page for the
/// headers, the size the kernel expects a reader to offer so a large write arrives in one read.
const BUFFER_BYTES: usize = 256 * 1024 + 4096;

/// Format: the longest notification: the header, the 24-byte inode or delete body, and a name up
/// to the volume's name cap with its NUL.
const NOTIFY_BYTES: usize = OUT_HEADER_LEN + 24 + 255 + 1;

/// Format: `fuse_notify_inval_inode_out.off` of -1 — invalidate the attributes only, no data.
const ATTRIBUTES_ONLY: i64 = -1;
/// Format: `fuse_notify_inval_inode_out.len` of -1 — the data from `off` to the end of the file (the kernel reads any
/// `len` of zero or less so, `fs/fuse/inode.c` `fuse_reverse_inval_inode`); with a zero `off`, the whole file.
const TO_THE_END: i64 = -1;

/// A refusal from the transport.
#[derive(Debug)]
pub enum ChannelError {
  /// The device could not be opened (the FUSE module is absent, or the caller lacks access).
  Open {
    /// The OS error code.
    code: Option<i32>,
  },
  /// A read or write on the device refused.
  Device {
    /// The call.
    call: &'static str,
    /// The OS error code.
    code: Option<i32>,
  },
  /// The kernel closed the connection (the mount is gone).
  Disconnected,
}

impl std::fmt::Display for ChannelError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Open { code } => write!(f, "cannot open {FUSE_DEVICE} (code {code:?})"),
      Self::Device { call, code } => write!(f, "{call} on {FUSE_DEVICE} refused (code {code:?})"),
      Self::Disconnected => f.write_str("the FUSE connection is gone"),
    }
  }
}

impl std::error::Error for ChannelError {}

/// What became of a reply written to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
  /// The kernel took it (or the request needed none).
  Delivered,
  /// The kernel answered `ENOENT`: no request with that id waits any more (its caller was interrupted), so
  /// whatever the reply granted must be reclaimed by the server ([`crate::bridge::reclaim_unreported`]).
  Unmatched,
}

/// The daemon's end of one FUSE connection: the device descriptor and a reusable buffer.
pub struct FuseChannel {
  device: OwnedFd,
  buffer: Vec<u8>,
}

impl std::fmt::Debug for FuseChannel {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FuseChannel").finish()
  }
}

impl FuseChannel {
  /// Opens `/dev/fuse`. The mount ([`crate::mount`]) then attaches this descriptor to a mount
  /// point; a mount keeps this one channel, read by its volume's owner shard (A-62: clones over one
  /// connection, `FUSE_DEV_IOC_CLONE`, would hand most requests to a shard that cannot serve them).
  pub fn open() -> Result<FuseChannel, ChannelError> {
    // /dev/fuse is a character device the kernel exposes, opened read-write to exchange FUSE
    // messages; it is not a disk file and creates nothing (R1, §4.6).
    // structural: allow — device open, not a disk-file write.
    let device = rustix::fs::open(FUSE_DEVICE, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
      .map_err(|e| ChannelError::Open {
        code: Some(e.raw_os_error()),
      })?;
    Ok(FuseChannel::from_device(device))
  }

  /// A channel over a device descriptor the mount already opened (the anchor hands it back on a
  /// restart, §2.6 step 4).
  pub fn from_device(device: OwnedFd) -> FuseChannel {
    FuseChannel {
      device,
      buffer: vec![0u8; BUFFER_BYTES],
    }
  }

  /// The device descriptor (for the mount and for cloning).
  pub fn device(&self) -> impl AsFd + '_ {
    self.device.as_fd()
  }

  /// A channel whose reads never block (`O_NONBLOCK`): for an owner that awaits the device's readiness
  /// through its own runtime and reads with [`FuseChannel::try_read_request`] (the daemon's shard, R6).
  pub fn nonblocking(device: OwnedFd) -> Result<FuseChannel, ChannelError> {
    rustix::io::ioctl_fionbio(&device, true).map_err(|e| ChannelError::Device {
      call: "fionbio",
      code: Some(e.raw_os_error()),
    })?;
    Ok(FuseChannel::from_device(device))
  }

  /// The device's descriptor number, for registering its readiness with a runtime; the channel keeps
  /// owning it.
  pub fn raw_device(&self) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    self.device.as_raw_fd()
  }

  /// Reads the next request if one is waiting, without blocking: its length, `None` when none is
  /// (`EAGAIN`) or a signal interrupted the read; `Disconnected` once the kernel has unmounted.
  pub fn try_read_request(&mut self) -> Result<Option<usize>, ChannelError> {
    match rustix::io::read(&self.device, self.buffer.as_mut_slice()) {
      Ok(n) => Ok(Some(n)),
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Ok(None),
      Err(rustix::io::Errno::NODEV) => Err(ChannelError::Disconnected),
      Err(e) => Err(ChannelError::Device {
        call: "read",
        code: Some(e.raw_os_error()),
      }),
    }
  }

  /// Reads the next request into the internal buffer; returns the bytes read. `ENODEV` means
  /// the kernel unmounted (the caller stops); `EINTR`/`EAGAIN` are retried by the caller.
  pub fn read_request(&mut self) -> Result<&[u8], ChannelError> {
    match rustix::io::read(&self.device, self.buffer.as_mut_slice()) {
      Ok(n) => Ok(self.buffer.get(..n).unwrap_or_default()),
      Err(rustix::io::Errno::NODEV) => Err(ChannelError::Disconnected),
      Err(e) => Err(ChannelError::Device {
        call: "read",
        code: Some(e.raw_os_error()),
      }),
    }
  }

  /// Writes one reply to the device.
  pub fn write_reply(&self, reply: &[u8]) -> Result<Sent, ChannelError> {
    // A zero-length reply is a request that needs none (FORGET); write nothing.
    if reply.is_empty() {
      return Ok(Sent::Delivered);
    }
    match rustix::io::write(&self.device, reply) {
      Ok(_) => Ok(Sent::Delivered),
      // The kernel no longer holds the request (its caller was interrupted): not a fault of the mount, but
      // the reply's grants never reached anyone (AUD-29-85).
      Err(rustix::io::Errno::NOENT) => Ok(Sent::Unmatched),
      Err(e) => Err(ChannelError::Device {
        call: "write",
        code: Some(e.raw_os_error()),
      }),
    }
  }

  /// A second descriptor over the same device, for the thread that writes a round of invalidations
  /// ([`Notifier`]). The notification write blocks in the kernel, so it must never run on the thread that
  /// answers requests; it needs a descriptor that outlives the borrow and crosses threads.
  fn device_clone(&self) -> Result<OwnedFd, ChannelError> {
    self.device.try_clone().map_err(|e| ChannelError::Device {
      call: "dup",
      code: e.raw_os_error(),
    })
  }
}

/// Format: the eventfd counter's width — one native-endian `u64` per read or write (`eventfd(2)`).
const EVENTFD_WORD: usize = size_of::<u64>();

/// Encodes one kernel invalidation as an unsolicited notification (§4.6 `notify`). `expire_only` says the kernel
/// negotiated `FUSE_HAS_EXPIRE_ONLY`, so a live-source entry is expired rather than dropped. `None` for a name past
/// the wire cap: the kernel cannot be holding it either, so there is nothing to drop. The flag says whether this is
/// an **entry** invalidation, because the kernel's `ENOENT` means something different for one (see [`Notifier`]).
fn encode_invalidation(invalidation: &Invalidation, expire_only: bool) -> Option<(Vec<u8>, bool)> {
  let mut out = [0u8; NOTIFY_BYTES];
  let entry = matches!(invalidation, Invalidation::Entry { .. });
  let encoded = match invalidation {
    Invalidation::Entry {
      parent,
      name,
      expire,
    } => {
      let flags = if *expire && expire_only {
        EXPIRE_ONLY
      } else {
        0
      };
      inval_entry(*parent, name, flags, &mut out)
    }
    Invalidation::Inode { ino, data } => {
      let (off, len) = notified_range(*data);
      inval_inode(*ino, off, len, &mut out)
    }
    // Never one message: the round expands it into what the kernel holds ([`everything_told`]).
    Invalidation::Everything => return None,
  };
  let n = encoded.ok()?;
  Some((out.get(..n)?.to_vec(), entry))
}

/// The `off` and `len` of the `FUSE_NOTIFY_INVAL_INODE` that drops `data` with the attributes (A-146). The kernel drops
/// every cached page holding a byte of `[off, off + len)`, or from `off` to the end for [`TO_THE_END`]. The ABI's
/// offsets are signed (`loff_t`): a range they cannot express lies past any file's largest size, and is sent as the
/// rest of the file from where it can still be named, which drops more than the change and so is never stale. An empty
/// range drops no bytes.
fn notified_range(data: CachedBytes) -> (i64, i64) {
  match data {
    CachedBytes::Kept | CachedBytes::Range { len: 0, .. } => (ATTRIBUTES_ONLY, 0),
    CachedBytes::Range { at, len } => match (i64::try_from(at), i64::try_from(len)) {
      (Ok(off), Ok(len)) if off.checked_add(len).is_some() => (off, len),
      (Ok(off), _) => (off, TO_THE_END),
      (Err(_), _) => (0, TO_THE_END),
    },
    CachedBytes::From { at } => (i64::try_from(at).unwrap_or(0), TO_THE_END),
  }
}

/// The mount's notify thread (§4.6 "Cache posture"; A-142). It writes each round of kernel invalidations **away from the
/// serving thread**, one round at a time, for the life of the mount. Rounds reach it over a channel of one, and each
/// outcome comes back over a channel of one and wakes the loop. Owned by the [`ServeState`]: dropped, it closes the
/// round channel and joins the thread, which first finishes or fails the round in flight (at an unmount the device
/// write fails), so no thread outlives its owner.
///
/// It exists because the write blocks. `fuse_reverse_inval_entry()` takes the parent directory's inode lock, which a
/// concurrent lookup holds for the whole request — and the only thread that would answer that lookup is the serving
/// thread. Writing a notification there deadlocked the mount outright
/// (`docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md`): the serving thread must stay free to answer,
/// because answering is what releases the lock the write is waiting on. `FUSE_EXPIRE_ONLY` does not avoid it (the
/// kernel locks the parent before it reads the flags), and nor does any ordering inside our own loop (the kernel
/// holds the lock from the moment it issues the request, whenever we read it). This mirrors the landing barrier's
/// thread (A-132, `slates-server` `landing.rs`), which exists for the same reason.
///
/// One thread per mount, started with its first round and kept. It was one thread per round until 2026-10-22, which
/// cost the owner's shard a thread start on every change another path made (`docs/wip/BENCHMARKS.md`, A-142).
struct Notifier {
  /// Rounds to write: a channel of one, since the loop starts a round only once the last one is back. `None` once
  /// the owner has closed it.
  rounds: Option<std::sync::mpsc::SyncSender<Round>>,
  /// Each round's outcome.
  outcomes: std::sync::mpsc::Receiver<Outcome>,
  thread: Option<std::thread::JoinHandle<()>>,
  /// The round out, if one is: where the cursor stood before it, to put it back if the kernel refuses the write.
  out: Option<Option<slates_bridge_core::InvalidationCursor>>,
}

impl Notifier {
  /// Starts the mount's notify thread on its own duplicate of the device. `wake` is signalled once each outcome is
  /// sent, so the loop sees it without waiting for the kernel's next request. Refused with the system's errno when no
  /// thread can start.
  fn start(device: OwnedFd, wake: Option<ChangeNotifier>) -> Result<Notifier, ChannelError> {
    let (rounds, inbox) = std::sync::mpsc::sync_channel::<Round>(1);
    let (send, outcomes) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
      .name("slates-fuse-notify".to_owned())
      .spawn(move || {
        // Until the owner closes the round channel: each round written whole, its outcome sent and the loop woken.
        while let Ok(messages) = inbox.recv() {
          // The receiver outlives the thread (its owner joins before it drops), so the send cannot fail; were it to,
          // nobody is left to tell.
          if send.send(write_round(&device, &messages)).is_err() {
            return;
          }
          if let Some(wake) = wake.as_ref() {
            let _ = wake.notify();
          }
        }
      })
      .map_err(|e| ChannelError::Device {
        call: "notify-thread",
        code: e.raw_os_error(),
      })?;
    Ok(Notifier {
      rounds: Some(rounds),
      outcomes,
      thread: Some(thread),
      out: None,
    })
  }

  /// Hands the thread a round, noting where the cursor stood `before` it. A thread that is gone reads as the device
  /// refusing.
  fn send(
    &mut self,
    messages: Round,
    before: Option<slates_bridge_core::InvalidationCursor>,
  ) -> Result<(), ChannelError> {
    match self.rounds.as_ref().map(|rounds| rounds.try_send(messages)) {
      Some(Ok(())) => {
        self.out = Some(before);
        Ok(())
      }
      _ => Err(ChannelError::Device {
        call: "notify",
        code: rustix::io::Errno::IO.raw_os_error().into(),
      }),
    }
  }

  /// Whether a round is out.
  fn busy(&self) -> bool {
    self.out.is_some()
  }

  /// The round's outcome once it is back, with where the cursor stood before it; `None` while it is still being
  /// written, or when no round is out. A thread gone without an outcome (it cannot be: shipped code does not panic, and
  /// a panic aborts) reads as the device refusing.
  fn returned(&mut self) -> Option<(Outcome, Option<slates_bridge_core::InvalidationCursor>)> {
    let before = self.out?;
    let round = match self.outcomes.try_recv() {
      Ok(round) => round,
      Err(std::sync::mpsc::TryRecvError::Empty) => return None,
      Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(ChannelError::Device {
        call: "notify",
        code: rustix::io::Errno::IO.raw_os_error().into(),
      }),
    };
    self.out = None;
    Some((round, before))
  }
}

impl Drop for Notifier {
  fn drop(&mut self) {
    // Closing the round channel ends the thread's loop once any round in flight is written; then it is joined.
    self.rounds = None;
    if let Some(thread) = self.thread.take()
      && thread.join().is_err()
    {
      eprintln!("slates-bridge-fuse: a notify thread panicked");
    }
  }
}

/// One round of notifications: each message's bytes, and whether its `ENOENT` is counted as a refusal ([`write_round`]):
/// set for an entry invalidation of a name the kernel was handed, and for nothing else.
type Round = Vec<(Vec<u8>, bool)>;
/// A round's outcome: how many entry invalidations the kernel refused, or the device's own refusal.
type Outcome = Result<u32, ChannelError>;

/// Writes one round's notifications to the device, in order, stopping at the first the device refuses: how many entry
/// invalidations the kernel refused, or that refusal.
fn write_round(device: &OwnedFd, messages: &[(Vec<u8>, bool)]) -> Outcome {
  let mut refused = 0u32;
  for (message, counted) in messages {
    // `ENOTEMPTY` is the kernel declining to drop a directory that is in use, and `ENOENT` on an *inode* invalidation
    // is the kernel not holding that inode — neither is a fault of the mount. Nor is `ENOENT` for a name the kernel was
    // never handed, which cannot be in its cache. `ENOENT` for an entry the kernel **was** handed is counted, not
    // discarded: on Linux 6.19 the kernel refused it and went on answering from the cached negative entry, so the name a
    // neighbour created was never seen (`docs/bugs/2026-10-19-linux-6-19-refuses-the-entry-invalidation-slates-swallows.md`).
    // It is counted rather than raised because the refusal is not retryable — the next round would be refused the same
    // way, and a round kept for retry is an unbounded loop.
    match rustix::io::write(device, message) {
      Ok(_) => {}
      Err(rustix::io::Errno::NOENT) if *counted => refused = refused.saturating_add(1),
      Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => {}
      Err(e) => {
        return Err(ChannelError::Device {
          call: "notify",
          code: Some(e.raw_os_error()),
        });
      }
    }
  }
  Ok(refused)
}

/// A change signal (§4.6 "Cache posture"; AUD-02): the descriptor the serve loop waits on beside the
/// device, so a change another mutation source made — the SDK or another attachment on the volume,
/// an outsider's change beneath a base directory the watcher reported — wakes the loop to deliver
/// the owed invalidations without waiting for the kernel's next request. An `eventfd`: a counter the
/// notifiers add to and the wait drains, never blocking either side.
pub struct ChangeSignal {
  fd: OwnedFd,
}

impl std::fmt::Debug for ChangeSignal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChangeSignal").finish()
  }
}

impl ChangeSignal {
  /// A fresh signal, unsignalled.
  pub fn new() -> Result<ChangeSignal, ChannelError> {
    let fd =
      rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map_err(|e| {
        ChannelError::Device {
          call: "eventfd",
          code: Some(e.raw_os_error()),
        }
      })?;
    Ok(ChangeSignal { fd })
  }

  /// A notifier of this signal for another party (another thread's mutation source): its own
  /// descriptor over the same counter, so it may outlive the borrow and cross threads.
  pub fn notifier(&self) -> Result<ChangeNotifier, ChannelError> {
    let fd = self.fd.try_clone().map_err(|e| ChannelError::Device {
      call: "dup",
      code: e.raw_os_error(),
    })?;
    Ok(ChangeNotifier { fd })
  }

  /// The signal's descriptor number, for registering its readiness with a runtime beside the device's
  /// ([`FuseChannel::raw_device`]); the signal keeps owning it. An owner that interleaves other work awaits both
  /// itself, rather than through the blocking [`wait`].
  pub fn raw_signal(&self) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    self.fd.as_raw_fd()
  }

  /// Takes the pending signals (the counter, in one read); a counter already at zero is nothing. An owner awaiting
  /// [`Self::raw_signal`] itself drains once it is readable, or the descriptor stays readable and the wait spins.
  pub fn drain(&self) {
    let mut word = [0u8; EVENTFD_WORD];
    let _ = rustix::io::read(&self.fd, &mut word);
  }
}

/// The signalling end of a [`ChangeSignal`]: held by a mutation source, which calls
/// [`ChangeNotifier::notify`] after changing something the transport's kernel may hold cached.
pub struct ChangeNotifier {
  fd: OwnedFd,
}

impl std::fmt::Debug for ChangeNotifier {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChangeNotifier").finish()
  }
}

impl ChangeNotifier {
  /// Another descriptor over the same counter, for a thread that must wake the loop when it finishes (the round of
  /// invalidations in [`Notifier`]).
  fn try_clone(&self) -> Result<ChangeNotifier, ChannelError> {
    let fd = self.fd.try_clone().map_err(|e| ChannelError::Device {
      call: "dup",
      code: e.raw_os_error(),
    })?;
    Ok(ChangeNotifier { fd })
  }

  /// Signals a change. A counter already at its ceiling (`EAGAIN`) is already signalled, so that
  /// is not a failure; any other refusal is the descriptor's.
  pub fn notify(&self) -> Result<(), ChannelError> {
    match rustix::io::write(&self.fd, &1u64.to_ne_bytes()) {
      Ok(_) | Err(rustix::io::Errno::AGAIN) => Ok(()),
      Err(e) => Err(ChannelError::Device {
        call: "notify",
        code: Some(e.raw_os_error()),
      }),
    }
  }
}

/// What a wait observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
  /// The device has a request to read (or the connection ended: the read tells which).
  Request,
  /// The change signal fired: the owed invalidations are to be delivered, with no request.
  Change,
}

/// Blocks until the device has a request or the change `signal` fires, whichever first (both:
/// the request, since serving it delivers the owed round first anyway). An interrupted wait is
/// resumed; the signal's counter is drained before `Change` is reported, so a level-triggered
/// descriptor does not report the same change again.
pub fn wait(channel: &FuseChannel, signal: Option<&ChangeSignal>) -> Result<Wake, ChannelError> {
  loop {
    let device = channel.device.as_fd();
    let interest = PollFlags::IN;
    let mut fds = [
      PollFd::new(&device, interest),
      match signal {
        Some(signal) => PollFd::new(&signal.fd, interest),
        None => PollFd::new(&device, interest),
      },
    ];
    let polled = if signal.is_some() { 2 } else { 1 };
    match rustix::event::poll(fds.get_mut(..polled).unwrap_or_default(), None) {
      Ok(_) => {}
      Err(rustix::io::Errno::INTR) => continue,
      Err(e) => {
        return Err(ChannelError::Device {
          call: "poll",
          code: Some(e.raw_os_error()),
        });
      }
    }
    // A hung-up or erroring device is a request to read: the read reports the disconnect.
    if fds[0]
      .revents()
      .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
    {
      return Ok(Wake::Request);
    }
    if let Some(signal) = signal
      && fds[1].revents().intersects(PollFlags::IN)
    {
      signal.drain();
      return Ok(Wake::Change);
    }
  }
}

/// The serve loop's state across steps: the reply buffer, what the kernel negotiated, and the
/// kernel's coherence (the cursor and what is owed).
pub struct ServeState {
  reply: Vec<u8>,
  /// Whether the kernel honours FUSE_EXPIRE_ONLY, learned from its INIT (the negotiation is pure,
  /// so re-running it here agrees with the reply the dispatch sends).
  expire_only: bool,
  /// What the connection negotiated at `INIT`, once it has: what a restarted daemon needs to serve this
  /// device (§4.6 "restore from the anchor's held fd"), handed to the anchor beside it.
  session: Option<Session>,
  /// For a device taken over from a daemon that died (A-61), the reply that daemon published with a barrier's
  /// effect and never delivered — the request's unique id (without the resend bit) and the reply's bytes —
  /// answered when the kernel resends that request, so its effect is never applied twice.
  replay: Option<(u64, Vec<u8>)>,
  /// Where the kernel's cache stands and what it is owed (AUD-02).
  pub coherence: Coherence,
  /// A `FALLOCATE` still running (A-108), with where the kernel's cache stood before it began: stepped one slice
  /// per turn, and no other request is read until it is answered.
  allocating: Option<(Allocation, Option<slates_bridge_core::InvalidationCursor>)>,
  /// The mount's notify thread, which writes each round of invalidations away from this thread ([`Notifier`]); started
  /// with the first round. At most one round is out at a time: the gather consumes what it reports
  /// ([`slates_bridge_core::Bridge::invalidations`] takes the base plane's stale entries), so a second round is never
  /// started while one is in flight — which is also its bound.
  notifier: Option<Notifier>,
  /// Wakes this loop when a round finishes, so its outcome is seen and the next round started without waiting for
  /// the kernel's next request. Absent in a harness that drives the loop itself.
  notify_wake: Option<ChangeNotifier>,
  /// Entry invalidations the kernel refused with `ENOENT` since this was last read, counted only for names the kernel
  /// was handed ([`KernelHolds`]). Each is a name this mount believes the kernel holds, which the kernel says it does
  /// not: it dropped the name on its own (a dentry evicted, which [`KernelHolds`] cannot see), or it refused to drop an
  /// entry it still serves. Counted so a mount losing coherence is visible instead of silent (CLAUDE.md #9); read and
  /// cleared by the owner that reports it.
  entry_invalidations_refused: u64,
  /// The journal records this mount's own requests made: the bytes they wrote are fresh in its kernel, so a later cache
  /// write-back is not held stale there (the dirty-page write-back rule, [`dispatch_request`]).
  own: OwnRecords,
  /// The handles this mount's kernel holds open for writing: only their files can have dirty pages.
  writable: WritableOpens,
  /// What the kernel holds of each file open for writing — per byte, the point it is as of and whether another path
  /// changed it since — which is what a cache write-back is judged by. Fed as this mount consumes the journal.
  states: PageStates,
  /// The journal's head when this mount served its first request: the point a file with no record here is judged
  /// from — one a restored mount's kernel opened before this daemon served it (A-61).
  started_at: Option<u64>,
  /// Whether this mount was taken over from a daemon that died (A-61), so its kernel holds handles opened before this
  /// daemon served it, which it never saw opened. A request naming one adopts its file into the byte states
  /// ([`adopt_unseen_handle`]; A-147).
  adopts_unseen_handles: bool,
  /// Whether this restored mount has yet to track the files the volume restored opens of for it, which it does once, at
  /// its first turn ([`adopt_restored`]).
  adopt_restored: bool,
  /// Where this mount's takeover round stands (A-158): the epoch bump and the invalidations of everything its kernel may
  /// hold older than the volume ([`takeover_told`]), sent in slices from a restored mount's first turn.
  takeover: Takeover,
  /// Whether the round out carries a slice of the takeover round: until it is back, the kernel has been told of nothing
  /// this mount's writers could rely on ([`ServeState::told_through`]).
  takeover_out: bool,
  /// The time one slice of the takeover round may take on the serving thread, set by the owner before each turn from its
  /// shard's live step quantum ([`ServeState::slices_within`]). Zero sends one inode a slice.
  slice_ns: u64,
  /// Takeover rounds that bumped the kernel's dentry epoch, since this was last read: the non-vacuity count of the
  /// names half of A-158. Read and cleared by the owner that reports it.
  epochs_bumped: u64,
  /// Takeover rounds sent to a kernel that cannot bump its epoch (FUSE before 7.44), since this was last read: names its
  /// predecessor changed and never told it of may outlive the takeover there. Read and cleared by the owner.
  epochs_unsupported: u64,
  /// Inode invalidations sent in takeover rounds, since this was last read: the non-vacuity count of the bytes and
  /// attributes half of A-158. Read and cleared by the owner that reports it.
  takeover_invalidated: u64,
  /// Bytes of cache write-backs kept back because another path changed them after the kernel's copy was fetched, since
  /// this was last read: the rule's non-vacuity count. Read and cleared by the owner that reports it.
  writeback_bytes_kept_back: u64,
  /// Cache write-backs refused `EIO` because which of their bytes were stale could no longer be told (the file's states
  /// were lost, or the journal no longer held the records needed), since this was last read. Read and cleared by the
  /// owner that reports it.
  writebacks_unknown: u64,
  /// What this mount's kernel holds of the volume — every node it references and every name it was handed — fed by
  /// each reply as it reaches the kernel: what an overrun invalidates (A-141).
  holds: KernelHolds,
  /// Rounds that found this mount behind the journal's retention and invalidated everything its kernel holds, since
  /// this was last read. Read and cleared by the owner that reports it.
  journal_overruns: u64,
  /// The bytes of the shard's metadata ledger admitted for this mount's byte states ([`make_room`]). Returned to the
  /// ledger by the mount's owner when the mount ends ([`ServeState::states_charged`]).
  states_charged: u64,
}

impl std::fmt::Debug for ServeState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ServeState")
      .field("expire_only", &self.expire_only)
      .field("coherence", &self.coherence)
      .finish()
  }
}

impl ServeState {
  /// The state at mount time: nothing negotiated, nothing in the kernel's cache.
  pub fn new() -> ServeState {
    ServeState {
      reply: vec![0u8; BUFFER_BYTES],
      expire_only: false,
      session: None,
      replay: None,
      coherence: Coherence::new(),
      allocating: None,
      notifier: None,
      notify_wake: None,
      entry_invalidations_refused: 0,
      own: OwnRecords::default(),
      writable: WritableOpens::default(),
      states: PageStates::default(),
      started_at: None,
      adopts_unseen_handles: false,
      adopt_restored: false,
      takeover: Takeover::Told,
      takeover_out: false,
      slice_ns: 0,
      epochs_bumped: 0,
      epochs_unsupported: 0,
      takeover_invalidated: 0,
      writeback_bytes_kept_back: 0,
      writebacks_unknown: 0,
      holds: KernelHolds::default(),
      journal_overruns: 0,
      states_charged: 0,
    }
  }

  /// The bytes of the shard's metadata ledger this mount's byte states were admitted ([`make_room`]): what its owner
  /// returns to the ledger when the mount ends, so the mount's states are charged while it lives and not after.
  pub fn states_charged(&self) -> u64 {
    self.states_charged
  }

  /// The files whose byte states were lost for want of room since this was last asked, and zero afterwards: the ledger
  /// refused to grow the room ([`PageStates::take_lost_for_room`]). Zero is the healthy reading.
  pub fn take_states_lost_for_room(&mut self) -> u64 {
    self.states.take_lost_for_room()
  }

  /// The bytes of cache write-backs kept back since this was last asked, and zero afterwards
  /// ([`dispatch_request`]).
  pub fn take_writeback_bytes_kept_back(&mut self) -> u64 {
    std::mem::take(&mut self.writeback_bytes_kept_back)
  }

  /// The cache write-backs refused `EIO` as unknown since this was last asked, and zero afterwards
  /// ([`dispatch_request`]).
  pub fn take_writebacks_unknown(&mut self) -> u64 {
    std::mem::take(&mut self.writebacks_unknown)
  }

  /// The rounds that found this mount behind the journal's retention since this was last asked, and zero afterwards:
  /// each invalidated everything the kernel held (A-141). Zero is the healthy reading.
  pub fn take_journal_overruns(&mut self) -> u64 {
    std::mem::take(&mut self.journal_overruns)
  }

  /// Whether this mount, taken over from a daemon that died, has a slice of its takeover round to send now (A-158): some
  /// is owed and no round is out. Its owner turns it at once while this holds, rather than waiting for a request or a
  /// change: a kernel serving from its cache sends neither, and the round is what tells it its cache is stale. While a
  /// round is out this is false, and the round's return wakes the owner, so a takeover never spins its owner's shard.
  pub fn owes_takeover_round(&self) -> bool {
    self.takeover != Takeover::Told && !self.notifier.as_ref().is_some_and(Notifier::busy)
  }

  /// How far this mount's kernel has been told of its volume's journal, for a writer outside FUSE whose reply waits on
  /// every mount of the volume (AC-3.3): the newest journal sequence such that the kernel holds nothing older that a
  /// record up to it could have made stale. `None` while the kernel holds nothing at all — no round has run, and the
  /// first starts at the head ([`Coherence::deliver`]), so no record before it can be in the kernel.
  ///
  /// It is the cursor, except while a round is out: that round's records are told only once its write returns, so the
  /// kernel stands where the cursor stood before it (a first round out started at the head with an empty kernel, so the
  /// cursor stands). The cursor also moves past this mount's own requests' records, which its kernel made and needs no
  /// notice of. A mount taken over from a daemon that died has been told of nothing until its takeover round is sent and
  /// back (A-158): the records before its adoption reach its kernel only through that round.
  pub fn told_through(&self) -> Option<u64> {
    if self.takeover != Takeover::Told || self.takeover_out {
      return Some(TOLD_OF_NOTHING);
    }
    match self.notifier.as_ref().and_then(|notifier| notifier.out) {
      Some(Some(before)) => Some(before.journal_seq),
      Some(None) | None => self.coherence.cursor().map(|cursor| cursor.journal_seq),
    }
  }

  /// Bounds each slice of the takeover round to `ns` of the serving thread (A-158): the owner's share of its shard's live
  /// step quantum, set before each turn so the slices follow the quantum the shard keeps (§4.3).
  pub fn slices_within(&mut self, ns: u64) {
    self.slice_ns = ns;
  }

  /// The takeover rounds sent since this was last asked, and zero afterwards, as `(epoch bumped, epoch unsupported,
  /// inodes invalidated)` (A-158; [`takeover_told`]).
  pub fn take_takeover_counts(&mut self) -> (u64, u64, u64) {
    (
      std::mem::take(&mut self.epochs_bumped),
      std::mem::take(&mut self.epochs_unsupported),
      std::mem::take(&mut self.takeover_invalidated),
    )
  }

  /// The entry invalidations the kernel has refused since this was last asked, and zero afterwards: names it was handed
  /// that it says it does not hold. A non-zero answer means the kernel dropped such names on its own, or is still
  /// serving names this mount has tried to invalidate.
  pub fn take_entry_invalidations_refused(&mut self) -> u64 {
    std::mem::take(&mut self.entry_invalidations_refused)
  }

  /// Wakes this state's serve loop whenever a round of invalidations finishes being written (§4.6): the round runs
  /// off the serving thread, so its outcome arrives out of band.
  pub fn wakes_with(&mut self, notifier: ChangeNotifier) {
    self.notify_wake = Some(notifier);
  }

  /// The state of a device taken over from a previous daemon: what its connection negotiated, the reply that
  /// daemon published and never delivered (if any), and nothing in the kernel's cache that this daemon knows of. The
  /// files its kernel holds open from before are adopted as requests name them (A-147), and its first round tells the
  /// kernel what it may hold stale (A-158).
  pub fn restored(session: Session, replay: Option<(u64, Vec<u8>)>) -> ServeState {
    ServeState {
      expire_only: session.expire_only,
      session: Some(session),
      replay,
      adopts_unseen_handles: true,
      adopt_restored: true,
      takeover: Takeover::Owed,
      ..ServeState::new()
    }
  }

  /// The recorded reply for the request with `unique`, once, when the kernel resent the very request it answers
  /// ([`crate::abi::UNIQUE_RESEND`] set, the rest equal): its bytes with the reply's unique set to the resent
  /// request's, as the kernel matches the whole word.
  fn replay_for(&mut self, unique: u64) -> Option<Vec<u8>> {
    let resent = unique & crate::abi::UNIQUE_RESEND != 0;
    let matches = self
      .replay
      .as_ref()
      .is_some_and(|(recorded, _)| resent && *recorded == unique & !crate::abi::UNIQUE_RESEND);
    if !matches {
      return None;
    }
    let (_, mut reply) = self.replay.take()?;
    /// Format: where `fuse_out_header`'s unique sits, after its length and error words.
    const AT_UNIQUE: usize = 2 * size_of::<u32>();
    reply
      .get_mut(AT_UNIQUE..AT_UNIQUE.saturating_add(size_of::<u64>()))?
      .copy_from_slice(&unique.to_le_bytes());
    Some(reply)
  }

  /// What the connection negotiated, once its `INIT` has been answered.
  pub fn session(&self) -> Option<Session> {
    self.session
  }

  /// Takes what an `INIT` request negotiates.
  fn negotiated(&mut self, body: &[u8]) {
    if let Ok(negotiated) = negotiate(body, slates_bridge_core::CacheCoherence::Invalidated) {
      let session = Session::of(&negotiated);
      self.expire_only = session.expire_only;
      self.session = Some(session);
    }
  }
}

impl Default for ServeState {
  fn default() -> ServeState {
    ServeState::new()
  }
}

/// What one step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
  /// A kernel request was served — its opcode, or `None` for one slates does not serve — after the
  /// owed round was delivered.
  Request {
    /// The request's opcode.
    opcode: Option<Opcode>,
    /// The round delivered before it.
    delivered: Delivered,
    /// The reply's errno (the kernel's convention: zero for success, else the positive errno the
    /// reply carried negated), or zero for a request that takes no reply.
    error: i32,
  },
  /// A change was signalled and the owed round delivered; no request.
  Change(Delivered),
  /// A read shorter than a header — a message the kernel never sends — dropped.
  Dropped,
  /// The mount ended: the kernel disconnected, or the attachment admits no more requests.
  Ended,
}

/// One step of the serve loop after `wake`: a signalled change delivers the owed round; a request
/// is read, the owed round delivered, the request dispatched to `bridge` and its reply written.
/// Each step is admitted with [`Attachments::begin`] and ended with [`Attachments::end`] around its
/// whole service, so a barrier over the volume (§4.6 "Writeback and snapshot barrier") sees exactly
/// the requests the seam may still be applying — one at a time here — and a loop that dies
/// mid-request leaves that request counted, which a barrier reports as incomplete until the owner's
/// failed-consumer cleanup. A revoked or epoch-fenced attachment ends the mount rather than serving
/// under stale authority (§4.8; per-request revalidation). A device error other than a disconnect is
/// returned to the caller.
pub fn serve_step(
  channel: &mut FuseChannel,
  bridge: &mut dyn Bridge,
  attachments: &mut Attachments,
  attachment: AttachmentId,
  state: &mut ServeState,
  wake: Wake,
) -> Result<Step, ChannelError> {
  if wake == Wake::Change {
    let Ok(cx) = attachments.begin(attachment) else {
      return Ok(Step::Ended);
    };
    let delivered = deliver_owed(channel, bridge, &cx, state);
    attachments.end(attachment);
    return delivered.map(Step::Change);
  }
  let request = match channel.read_request() {
    Ok(r) => r,
    Err(ChannelError::Disconnected) => return Ok(Step::Ended),
    Err(e) => return Err(e),
  };
  if request.len() < IN_HEADER_LEN {
    // A truncated read: the kernel never sends one, so drop it rather than reply to a message
    // with no header.
    return Ok(Step::Dropped);
  }
  let Ok(cx) = attachments.begin(attachment) else {
    return Ok(Step::Ended);
  };
  // `dispatch` needs a mutable reply buffer separate from the request buffer the channel owns,
  // so the request is copied out (one memcpy of a small message; the io_uring path avoids it
  // with registered buffers, owed).
  let request = channel.take_request();
  let opcode = Request::parse(&request).ok().and_then(|parsed| {
    if parsed.opcode == Some(Opcode::Init) {
      state.negotiated(parsed.body);
    }
    parsed.opcode
  });
  let served = serve_request(channel, bridge, &cx, &request, state);
  // The request is ended whatever happened to it: a transport error ends the loop, and the mount's
  // teardown (the owner's sweep) is the cleanup, not a phantom in-flight count.
  attachments.end(attachment);
  served.map(|(delivered, error)| Step::Request {
    opcode,
    delivered,
    error,
  })
}

/// Delivers the round owed to the kernel: every invalidation since the cursor, written as an
/// unsolicited notification **on the mount's notify thread** ([`Notifier`]) — never here, because the write blocks in
/// the kernel on a lock that only an answered request releases, and this is the thread that answers
/// (`docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md`).
///
/// One round is out at a time. While it is being written this delivers nothing and reports the round as owed again,
/// so the cursor does not move past changes the kernel has not been told of yet. A refused write puts the cursor
/// back to where it stood before the round, so the next wake gathers it again (an invalidation delivered twice is
/// harmless — the kernel drops nothing it does not hold).
fn deliver_owed(
  channel: &FuseChannel,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) -> Result<Delivered, ChannelError> {
  adopt_restored(bridge, cx, state);
  if let Some(notifier) = state.notifier.as_mut()
    && notifier.busy()
  {
    let Some((outcome, before)) = notifier.returned() else {
      // Still being written: nothing new may be gathered (the gather consumes), and the cursor must not advance.
      return Ok(Delivered {
        written: 0,
        gather_refused: true,
      });
    };
    // The round is back, whatever its outcome: a takeover slice it carried is no longer out.
    state.takeover_out = false;
    match outcome {
      Err(refusal) => {
        state.coherence.rewind(before);
        return Err(refusal);
      }
      Ok(refused) => {
        state.entry_invalidations_refused = state
          .entry_invalidations_refused
          .saturating_add(u64::from(refused));
      }
    }
  }
  let expire_only = state.expire_only;
  // The kernel names the volume's root node id 1 (`FUSE_ROOT_ID`), not by the volume's root inode: an
  // invalidation naming the root inode named nothing the kernel knows and was dropped, so a change to a name in the
  // root through another attachment never reached a mount that had cached it (found 2026-10-06 by a negative entry
  // that outlived the name's creation).
  let root = bridge.root(cx).ok();
  let before = state.coherence.cursor();
  let mut messages: Round = Vec::new();
  // A restored mount's rounds open with a slice of its takeover round until it is sent whole (A-158). A slice counts as
  // sent only once its round reaches the notify thread, so a turn refused before then sends it again.
  let takeover = (state.takeover != Takeover::Told)
    .then(|| takeover_told(bridge, cx, state, root, &mut messages));
  let mut overran = false;
  let holds = &mut state.holds;
  let delivered = state
    .coherence
    .deliver(bridge, cx, &mut |invalidation: &Invalidation| {
      if matches!(invalidation, Invalidation::Everything) {
        overran = true;
        everything_told(holds, expire_only, &mut messages);
        return Ok::<(), ChannelError>(());
      }
      let addressed = addressed_to_the_kernel(invalidation, root);
      if let Some((message, entry)) = encode_invalidation(&addressed, expire_only) {
        // The kernel drops or expires the name as this round reaches it; a later use is a new lookup that names it
        // again ([`KernelHolds::unnamed`]). Taken now, not when the round returns, so a lookup answered meanwhile is
        // never lost. Its `ENOENT` counts as a refusal only for a name the kernel was handed: one it never got is not in
        // its cache, and since A-137 no lookup miss is cached either, so its `ENOENT` is the expected answer.
        let held = match &addressed {
          Invalidation::Entry { parent, name, .. } => {
            let held = holds.holds_name(*parent, name);
            holds.unnamed(*parent, name);
            held
          }
          Invalidation::Inode { .. } | Invalidation::Everything => false,
        };
        messages.push((message, entry && held));
      }
      Ok::<(), ChannelError>(())
    })?;
  if overran {
    state.journal_overruns = state.journal_overruns.saturating_add(1);
  }
  // The records this round gathered, consumed by the byte states from the same point (the dirty-page write-back rule).
  // Before the first round nothing is tracked, so there is nothing to feed.
  if let (false, Some(since)) = (delivered.gather_refused, before) {
    feed_states(bridge, cx, state, since.journal_seq);
  }
  if !messages.is_empty() {
    let notifier = match state.notifier.take() {
      Some(notifier) => notifier,
      None => {
        let wake = match state.notify_wake.as_ref() {
          Some(notifier) => Some(notifier.try_clone()?),
          None => None,
        };
        Notifier::start(channel.device_clone()?, wake)?
      }
    };
    let notifier = state.notifier.insert(notifier);
    notifier.send(messages, before)?;
    state.takeover_out = takeover.is_some();
  }
  if let Some(slice) = takeover {
    if state.takeover == Takeover::Owed {
      if slice.bumped {
        state.epochs_bumped = state.epochs_bumped.saturating_add(1);
      } else {
        state.epochs_unsupported = state.epochs_unsupported.saturating_add(1);
      }
    }
    state.takeover = slice.next;
    state.takeover_invalidated = state.takeover_invalidated.saturating_add(slice.invalidated);
  }
  Ok(delivered)
}

/// Where a mount's takeover round stands (A-158; [`takeover_told`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Takeover {
  /// Nothing is owed: a mount this daemon started, or a round sent whole.
  Told,
  /// Owed, and none of it sent: its first slice carries the epoch bump and the root.
  Owed,
  /// Sent through inode `after` (the volume's numbering, in which the takeover set is walked); the rest is owed.
  SentThrough {
    /// The last inode a slice sent.
    after: u64,
  },
}

/// One slice of a takeover round, pushed onto a round's messages ([`takeover_told`]).
struct TakeoverSlice {
  /// Whether it bumps the kernel's epoch (only a first slice can).
  bumped: bool,
  /// The inode invalidations it carries.
  invalidated: u64,
  /// Where the round stands once the slice is sent.
  next: Takeover,
}

/// The next slice of the takeover round of a mount taken over from a daemon that died (A-158), pushed onto `messages`.
///
/// The dead daemon may have made changes it never told this kernel of: a change reaches a mount's kernel about 82 µs
/// after the writer's reply (A-142), and this daemon's journal starts empty, so nothing names them. The round tells the
/// kernel everything such a change can have left stale:
/// - **names:** `FUSE_NOTIFY_INC_EPOCH`, after which the kernel looks every cached name up again at its next use, where
///   the kernel offered FUSE 7.44 ([`Session::kernel_bumps_epoch`]). A name is the one thing no inode set reaches: a
///   name renamed or removed away belongs to no inode this daemon can list.
/// - **attributes, bytes and listings:** `FUSE_NOTIFY_INVAL_INODE` with all data for the root (the kernel's node 1) and
///   every inode the bridge says the kernel may hold older than the volume ([`Bridge::takeover_inodes`]). A directory's
///   invalidation drops its cached listing too, which kernels before Linux 7.2 keep past an epoch bump.
///
/// **Slices.** The set is as large as what the kernel had looked up, tens of thousands of inodes for a tree a build or
/// `git status` walked, and each message is built on the serving thread. So a slice takes at most the owner's budget
/// ([`ServeState::slices_within`]) and at least one inode, walked in inode order from where the last slice ended. The
/// first slice carries the epoch bump and the root, so names are right from the first round. An inode looked up after
/// the takeover below where the walk stands is fresh in the kernel, and one the kernel forgot needs nothing, so a set
/// that changes between slices loses nothing.
///
/// Neither message takes a lock the serving thread's answer releases, but dropping a file's pages waits for any page
/// a pending `READ` holds, so the round goes through the notify thread like every other ([`Notifier`]).
fn takeover_told(
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &ServeState,
  root: Option<u64>,
  messages: &mut Round,
) -> TakeoverSlice {
  let began = std::time::Instant::now();
  let budget = std::time::Duration::from_nanos(state.slice_ns);
  let first = state.takeover == Takeover::Owed;
  let bumped = first
    && state
      .session
      .is_some_and(|session| session.kernel_bumps_epoch);
  let mut invalidated: u64 = 0;
  let mut invalidate = |ino: u64, messages: &mut Round| {
    let invalidation = Invalidation::Inode {
      ino,
      data: CachedBytes::ALL,
    };
    if let Some(message) = encode_invalidation(&invalidation, state.expire_only) {
      messages.push(message);
      invalidated = invalidated.saturating_add(1);
    }
  };
  if bumped {
    let mut out = [0u8; OUT_HEADER_LEN];
    if let Some(message) = inc_epoch(&mut out).ok().and_then(|len| out.get(..len)) {
      messages.push((message.to_vec(), false));
    }
  }
  // The kernel names the root node 1 whatever the volume's root inode is (see [`addressed_to_the_kernel`]), and holds
  // the root whether or not any reference names it; the walk skips the root inode for that reason.
  if first {
    invalidate(crate::bridge::FUSE_ROOT_ID, messages);
  }
  let after = match state.takeover {
    Takeover::SentThrough { after } => Some(after),
    Takeover::Owed | Takeover::Told => None,
  };
  let mut last = after;
  let whole = bridge.takeover_inodes(cx, after, &mut |ino| {
    last = Some(ino);
    if Some(ino) != root {
      invalidate(ino, messages);
    }
    if began.elapsed() >= budget {
      std::ops::ControlFlow::Break(())
    } else {
      std::ops::ControlFlow::Continue(())
    }
  });
  let next = match (whole, last) {
    (false, Some(after)) => Takeover::SentThrough { after },
    _ => Takeover::Told,
  };
  TakeoverSlice {
    bumped,
    invalidated,
    next,
  }
}

/// The messages an `Everything` becomes (A-141): an attribute-and-data invalidation of every node the kernel references
/// and of the root, and an entry invalidation of every name it holds — expire-only where the kernel took that, so a
/// busy directory is revalidated rather than torn down
/// (`docs/bugs/2026-10-22-a-mount-that-falls-behind-the-journal-never-hears-of-the-changes-it-missed.md`). The kernel
/// may have dropped some of these already, so its `ENOENT` for a name here is expected and is not counted as a refusal.
fn everything_told(holds: &mut KernelHolds, expire_only: bool, messages: &mut Round) {
  let (nodes, names) = holds.invalidate_everything();
  for ino in nodes
    .into_iter()
    .chain(std::iter::once(crate::bridge::FUSE_ROOT_ID))
  {
    if let Some(message) = encode_invalidation(
      &Invalidation::Inode {
        ino,
        data: CachedBytes::ALL,
      },
      expire_only,
    ) {
      messages.push(message);
    }
  }
  for (parent, name) in names {
    let entry = Invalidation::Entry {
      parent,
      name,
      expire: true,
    };
    if let Some((message, _counted)) = encode_invalidation(&entry, expire_only) {
      messages.push((message, false));
    }
  }
}

/// `invalidation` with the volume's root inode (`root`) named as the kernel names it, node id 1.
fn addressed_to_the_kernel(invalidation: &Invalidation, root: Option<u64>) -> Invalidation {
  let kernel = |ino: u64| {
    if Some(ino) == root {
      crate::bridge::FUSE_ROOT_ID
    } else {
      ino
    }
  };
  match invalidation {
    Invalidation::Entry {
      parent,
      name,
      expire,
    } => Invalidation::Entry {
      parent: kernel(*parent),
      name: name.clone(),
      expire: *expire,
    },
    Invalidation::Inode { ino, data } => Invalidation::Inode {
      ino: kernel(*ino),
      data: *data,
    },
    Invalidation::Everything => Invalidation::Everything,
  }
}

/// Dispatches one admitted request and notes the journal records it made as this mount's own. A **cache write-back**
/// (a `WRITE` carrying [`crate::abi::WRITE_CACHE`]) goes by the dirty-page write-back rule ([`serve_writeback`]); any
/// other request by the bridge's dispatch. Returns the reply's length and, for a write-back, the byte ranges it was
/// kept back from, which its owner's log must leave out ([`Dispatched::written`]).
///
/// **The rule** (§4.6 "Cache posture";
/// `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`). A page a process
/// dirtied through a shared map stays in the kernel until the kernel writes it back, periodically, at `msync`, or while
/// it launders the page inside one of this mount's own invalidations. That page is the kernel's copy from before every
/// change it has not confirmed, so where another path changed the file since, its bytes are stale. Applied whole, it
/// undid the later write (measured 2026-10-21: 4,095 of a page's 4,096 bytes reverted). Applied outside those ranges,
/// the volume holds what one shared page cache would: the process's store, and the other path's newer bytes.
fn dispatch_request(
  request: &[u8],
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) -> (usize, Vec<std::ops::Range<u64>>) {
  adopt_restored(bridge, cx, state);
  let before = bridge.seen(cx).journal_seq;
  let started_at = *state.started_at.get_or_insert(before);
  // Before the write is judged or applied: a handle from before a takeover brings its file into the states (A-147).
  if state.adopts_unseen_handles
    && let Some((fh, ino)) = written_handle(request)
  {
    adopt_unseen_handle(fh, ino, started_at, bridge, cx, state);
  }
  let served = match crate::bridge::cache_writeback(request) {
    Some(nodeid) => serve_writeback(request, nodeid, started_at, bridge, cx, state),
    None => (dispatch(request, bridge, cx, &mut state.reply), Vec::new()),
  };
  note_opens_and_fetches(request, served.0, before, bridge, cx, state);
  note_own_records(bridge, cx, state, before);
  served
}

/// What a served request tells the dirty-page write-back rule ([`dispatch_request`]):
/// - a granted writable `OPEN` or `CREATE` is noted, and the file's first on this mount has its reply cleared of
///   `FOPEN_KEEP_CACHE`, so the kernel drops the file's cached pages as it opens it and every page cached after was
///   fetched after (or written whole by this mount);
/// - a `READ` of a file open for writing makes the bytes it returned fresh as of the journal's head, since a read
///   changes nothing ([`PageStates::fetched`]) — unless it was made through a handle that may bypass the page cache
///   (`O_DIRECT`), which fills nothing the mount can count on: its stale bytes become unknown
///   ([`PageStates::read_ambiguously`], A-144);
/// - a `RELEASE` forgets its handle (by value and file, since a restored mount's kernel can hold one value for two
///   files, A-147), and with the file's last writable handle its byte states (no write-back of it can follow, since a
///   write-back holds its file open until it is answered).
fn note_opens_and_fetches(
  request: &[u8],
  len: usize,
  before: u64,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) {
  let Ok(parsed) = Request::parse(request) else {
    return;
  };
  let ino = parsed.header.nodeid;
  match parsed.opcode {
    Some(Opcode::Open | Opcode::Create) => {
      if crate::bridge::opens_for_writing(parsed.opcode, parsed.body) != Some(true) {
        return;
      }
      let reply = state.reply.get(..len).unwrap_or(&[]);
      let Some(fh) = crate::bridge::granted_handle(parsed.opcode, reply) else {
        return;
      };
      let file = crate::bridge::created_node(parsed.opcode, reply).unwrap_or(ino);
      if state.writable.opened(fh, file) {
        if let Some(reply) = state.reply.get_mut(..len) {
          crate::bridge::clear_keep_cache(parsed.opcode, reply);
        }
        make_room(bridge, cx, state, 1);
        state.states.opened(file, bridge.seen(cx).journal_seq);
      }
    }
    Some(Opcode::Read) => {
      let answered = state
        .reply
        .get(..len)
        .is_some_and(|reply| reply_error(reply) == 0);
      if answered
        && let Ok(read) = crate::request::ReadIn::parse(Opcode::Read.to_wire(), parsed.body)
      {
        note_read(&read, ino, before, bridge, cx, state);
      }
    }
    Some(Opcode::Release) => {
      let handle = parsed
        .body
        .get(..size_of::<u64>())
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes);
      match handle.map(|fh| state.writable.released(fh, ino)) {
        Some(Some(file)) => state.states.dropped(file),
        // A handle this mount never saw opened: one from before a takeover, whose release the bridge has just
        // answered by giving back a restored open (A-152).
        Some(None) if state.adopts_unseen_handles => {
          let restored = bridge.restored_open_files(cx);
          drop_released_restored(state, ino, &restored);
        }
        Some(None) | None => {}
      }
    }
    _ => {}
  }
}

/// An answered `READ` of `ino` through `read`'s handle, made at head `before` ([`note_opens_and_fetches`]). On a restored
/// mount a handle from before the takeover first brings its file into the states ([`adopt_unseen_handle`]). For a file
/// open for writing, the bytes returned become fresh as of `before`, since a read changes nothing
/// ([`PageStates::fetched`]), unless the handle may bypass the page cache (`O_DIRECT`), whose stale bytes become unknown
/// instead ([`PageStates::read_ambiguously`], A-144).
fn note_read(
  read: &crate::request::ReadIn,
  ino: u64,
  before: u64,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) {
  let started_at = state.started_at.unwrap_or(before);
  adopt_unseen_handle(read.fh, ino, started_at, bridge, cx, state);
  if !state.writable.holds(ino) {
    return;
  }
  let end = read.offset.saturating_add(u64::from(read.size));
  if !may_bypass_the_cache(read.flags) {
    make_room(bridge, cx, state, MOST_ADDED_BY_AN_UPDATE);
    state.states.fetched(ino, read.offset..end, before);
    return;
  }
  match pending_changes(ino, started_at, bridge, cx, state) {
    Some(pending) => {
      let most = state
        .states
        .most_added_by_an_ambiguous_read(ino, pending.len());
      make_room(bridge, cx, state, most);
      state
        .states
        .read_ambiguously(ino, read.offset..end, before, &pending);
    }
    // As a round that finds the journal no longer reaching back loses every tracked file.
    None => state.states.lose_all(),
  }
}

/// The handle and file a `WRITE` names, when `request` is one: what [`adopt_unseen_handle`] takes before it is served.
fn written_handle(request: &[u8]) -> Option<(u64, u64)> {
  let parsed = Request::parse(request).ok()?;
  if parsed.opcode != Some(Opcode::Write) {
    return None;
  }
  let write = crate::request::WriteIn::parse(Opcode::Write.to_wire(), parsed.body).ok()?;
  Some((write.fh, parsed.header.nodeid))
}

/// On a mount taken over from a daemon that died (A-61), at its first turn: tracks every file the volume restored opens
/// of for this mount's owner (A-152) as fresh as of the mount's start, and starts the rounds there, so they feed each
/// file every change after it and no write-back of it reads the journal back to the start. A request naming one of its
/// handles then finds it tracked ([`adopt_unseen_handle`]). Until 2026-10-23 every such file was adopted only when a
/// request named it, from the journal's records since the start, and lost once the journal no longer reached back. A
/// handle opened after the last publication is in no count, so its file is still adopted when a request names it.
fn adopt_restored(
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) {
  if !state.adopt_restored {
    return;
  }
  state.adopt_restored = false;
  let seen = bridge.seen(cx);
  state.coherence.begin_at(seen);
  let start = *state.started_at.get_or_insert(seen.journal_seq);
  let files = bridge.restored_open_files(cx);
  make_room(bridge, cx, state, files.len());
  for ino in files {
    state.states.adopted(ino, start, Some(&[]));
  }
}

/// The release of a handle of `ino` this restored mount never saw opened has been answered: the file's byte states go
/// once nothing of it is open here any more — no handle this mount noted and no open left in `restored`, the files the
/// volume still holds restored opens of for it. Kept otherwise, a file's states would outlive its handles, and a later
/// open would inherit them (a lost file's refusals among them) though that open dropped the kernel's cached pages.
fn drop_released_restored(state: &mut ServeState, ino: u64, restored: &[u64]) {
  if state.states.tracks(ino) && !state.writable.holds(ino) && !restored.contains(&ino) {
    state.states.dropped(ino);
  }
}

/// On a mount taken over from a daemon that died (A-61), a request naming handle `fh` of `ino` that this mount never saw
/// opened adopts it (A-147). The kernel opened the handle before this daemon served it, maybe for writing, and a store
/// through a map of it reaches this mount as a cache write-back. Its file is tracked from here on
/// ([`PageStates::adopted`]): fresh as of the mount's start `started_at`, with the journal's changes to it since then.
/// So a re-fetch is recorded, and a write-back is judged as on a mount that saw the open.
///
/// Before A-147 such a file stayed untracked, and each write-back of it was judged against every change since the
/// mount's start. A store made after a re-fetch of bytes another path had changed was kept back, and lost
/// (`docs/bugs/2026-10-22-a-mount-taken-over-by-a-restarted-daemon-dropped-a-store-made-after-a-refetch.md`). A handle
/// opened for reading only is adopted too: its file's states cost a little until its release, and no write-back comes
/// through it.
fn adopt_unseen_handle(
  fh: u64,
  ino: u64,
  started_at: u64,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) {
  if !state.adopts_unseen_handles || state.writable.knows(fh, ino) {
    return;
  }
  state.writable.opened(fh, ino);
  if state.states.tracks(ino) {
    return;
  }
  let changes = pending_changes(ino, started_at, bridge, cx, state);
  let most = changes
    .as_ref()
    .map_or(0, Vec::len)
    .saturating_mul(MOST_ADDED_BY_AN_UPDATE)
    .saturating_add(1);
  make_room(bridge, cx, state, most);
  state.states.adopted(ino, started_at, changes.as_deref());
}

/// Feeds what `request` made the kernel hold now that its reply of `len` bytes reached it — or at once, for a request
/// that takes no reply (a `FORGET`): the entries it handed out, the lookups given back, the names taken away or moved
/// ([`KernelHolds`], A-141). A reply the kernel never received is not fed, so nothing it granted is counted as held.
fn note_delivered(request: &[u8], len: usize, state: &mut ServeState) {
  let reply = state.reply.get(..len).unwrap_or(&[]);
  let holds = &mut state.holds;
  crate::bridge::kernel_changes(request, reply, &mut |change| match change {
    KernelChange::HandedOut { parent, name, node } => holds.handed_out(parent, name, node),
    KernelChange::Forgot { node, count } => holds.forgot(node, count),
    KernelChange::Unnamed { parent, name } => holds.unnamed(parent, name),
    KernelChange::Renamed { from, to, exchange } => holds.renamed(from, to, exchange),
  });
}

/// Forgets the writable handle a lost `OPEN` or `CREATE` reply granted on node `nodeid`, when the server gives it back
/// itself ([`crate::bridge::reclaim_unreported`]), and with the file's last such handle its byte states. The file is
/// found as the open noted it: a `CREATE`'s new node, else the request's.
fn forget_unreported_open(state: &mut ServeState, opcode: Option<Opcode>, nodeid: u64, len: usize) {
  let reply = state.reply.get(..len).unwrap_or(&[]);
  let file = crate::bridge::created_node(opcode, reply).unwrap_or(nodeid);
  if let Some(file) =
    crate::bridge::granted_handle(opcode, reply).and_then(|fh| state.writable.released(fh, file))
  {
    state.states.dropped(file);
  }
}

/// Notes the journal records made since `before` as this mount's own ([`OwnRecords`], at most as many spans as the
/// journal retains). Called around every request this mount serves, each served whole on the shard.
fn note_own_records(
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
  before: u64,
) {
  let after = bridge.seen(cx).journal_seq;
  let capacity = bridge.journal_capacity(cx);
  state.own.record(before, after, capacity);
  if after <= before {
    return;
  }
  // The request's records refresh what they wrote in this kernel, applied now: the cursor moves past them without a
  // gather when the request's round was whole (`Coherence::served_own_request`).
  let mut changes = Vec::new();
  match bridge.data_changes(cx, before, &mut changes) {
    Ok(true) => {
      make_room(
        bridge,
        cx,
        state,
        changes.len().saturating_mul(MOST_ADDED_BY_AN_UPDATE),
      );
      for change in changes {
        state
          .states
          .changed(change.ino, change.range, change.seq, true);
      }
    }
    Ok(false) | Err(_) => state.states.lose_all(),
  }
}

/// Grows the byte states' room to hold `more` ranges beyond those they hold (§4.2, the metadata dimension): asked of the
/// shard's metadata ledger through the bridge, a page of ranges at a time ([`ranges_per_page`]), so a mount asks the
/// ledger once a page of states and its unused room stays under a page. The bytes admitted are charged to the mount
/// until it ends ([`ServeState::states_charged`]). A refusal leaves the room as it is, and an update that would pass it
/// loses its file, counted ([`ServeState::take_states_lost_for_room`]).
fn make_room(
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
  more: usize,
) {
  let short = state
    .states
    .held()
    .saturating_add(more)
    .saturating_sub(state.states.room());
  if short == 0 {
    return;
  }
  let per_page = ranges_per_page();
  let ranges = short.div_ceil(per_page).saturating_mul(per_page);
  let bytes = u64::try_from(ranges.saturating_mul(RANGE_BYTES)).unwrap_or(u64::MAX);
  if bridge.admit_states(cx, bytes) {
    state.states.grow_room(ranges);
    state.states_charged = state.states_charged.saturating_add(bytes);
  }
}

/// Derived: the byte-state ranges one page of the host's memory holds — the page size over [`RANGE_BYTES`], at least
/// one — the granule the states' room grows by.
fn ranges_per_page() -> usize {
  rustix::param::page_size()
    .checked_div(RANGE_BYTES)
    .unwrap_or(1)
    .max(1)
}

/// Feeds the journal's data changes after `since` to the byte states, each the mount's own or another path's by its
/// sequence ([`OwnRecords`]): what the round just gathered for invalidation, consumed from the same point in the same
/// turn. Every record through the head is then consumed, so the points at or below it are raised to it
/// ([`PageStates::consumed`]). A journal that no longer reaches back to `since` loses every tracked file, whose
/// write-backs are then refused rather than guessed.
fn feed_states(
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
  since: u64,
) {
  let mut changes = Vec::new();
  let complete = bridge
    .data_changes(cx, since, &mut changes)
    .unwrap_or(false);
  if !complete {
    state.states.lose_all();
    return;
  }
  make_room(
    bridge,
    cx,
    state,
    changes.len().saturating_mul(MOST_ADDED_BY_AN_UPDATE),
  );
  for change in changes {
    let own = state.own.took(change.seq);
    state
      .states
      .changed(change.ino, change.range, change.seq, own);
  }
  // The same synchronous turn as the walk: no record can have come between it and this reading of the head.
  state.states.consumed(bridge.seen(cx).journal_seq);
}

/// Serves a cache write-back of node `nodeid` by the dirty-page write-back rule ([`dispatch_request`]): applied whole
/// when none of its bytes is stale, outside the stale ranges when some are, and refused `EIO` when which are stale can
/// no longer be told — then neither applying (which may revert an acknowledged write) nor dropping (which would discard
/// data the process synced) is safe, and the process is told at its next `fsync` or `msync`.
fn serve_writeback(
  request: &[u8],
  nodeid: u64,
  started_at: u64,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) -> (usize, Vec<std::ops::Range<u64>>) {
  let Some(stale) = stale_in_writeback(request, nodeid, started_at, bridge, cx, state) else {
    state.writebacks_unknown = state.writebacks_unknown.saturating_add(1);
    return (
      crate::bridge::refuse_with_eio(request, &mut state.reply),
      Vec::new(),
    );
  };
  if stale.is_empty() {
    return (dispatch(request, bridge, cx, &mut state.reply), Vec::new());
  }
  let (len, kept_back) =
    crate::bridge::serve_cache_write(request, bridge, cx, &stale, &mut state.reply);
  state.writeback_bytes_kept_back = state.writeback_bytes_kept_back.saturating_add(kept_back);
  (len, stale)
}

/// The bytes of a cache write-back of node `nodeid` that are stale ([`PageStates::stale_after`]): the file's stale bytes
/// within the write, with the records not yet consumed — those after the coherence cursor, a few — applied in order to a
/// scratch copy. A file with no states (a mount restored across a daemon restart, whose opens predate this daemon) is
/// judged as fresh at `started_at` against every record since. Ascending and disjoint. `None` when it cannot be told: the
/// file's states were lost, the journal no longer holds the records needed, or the write-back cannot be read.
fn stale_in_writeback(
  request: &[u8],
  nodeid: u64,
  started_at: u64,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &ServeState,
) -> Option<Vec<std::ops::Range<u64>>> {
  let parsed = Request::parse(request).ok()?;
  let write = crate::request::WriteIn::parse(Opcode::Write.to_wire(), parsed.body).ok()?;
  let end = write
    .offset
    .saturating_add(u64::try_from(write.data.len()).unwrap_or(u64::MAX));
  let pending = pending_changes(nodeid, started_at, bridge, cx, state)?;
  state
    .states
    .stale_after(nodeid, write.offset..end, started_at, &pending)
}

/// The journal's changes to node `nodeid` that its byte states do not hold yet, each with whether it is the mount's own:
/// those after the coherence cursor, or, for a file not tracked, after `started_at`. A cache write-back and an ambiguous
/// read are judged with the same records. `None` when the journal no longer reaches back that far.
fn pending_changes(
  nodeid: u64,
  started_at: u64,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &ServeState,
) -> Option<Vec<(std::ops::Range<u64>, u64, bool)>> {
  let since = if state.states.tracks(nodeid) {
    state
      .coherence
      .cursor()
      .map_or(started_at, |cursor| cursor.journal_seq)
  } else {
    started_at
  };
  let mut changes = Vec::new();
  if !bridge.data_changes(cx, since, &mut changes).ok()? {
    return None;
  }
  Some(
    changes
      .into_iter()
      .filter(|change| change.ino == nodeid)
      .map(|change| {
        let own = state.own.took(change.seq);
        (change.range, change.seq, own)
      })
      .collect(),
  )
}

/// Whether a `READ` made through a handle with these open flags may have bypassed the kernel's page cache (A-144): a
/// handle opened `O_DIRECT` reads directly, or fills the cache through a map or a buffered fallback, and nothing on the
/// wire says which; a body from before protocol 7.9 carries no flags, so it cannot say either. `O_DIRECT` is the
/// running architecture's value, which differs between them.
fn may_bypass_the_cache(flags: Option<u32>) -> bool {
  flags.is_none_or(|flags| flags & OFlags::DIRECT.bits() != 0)
}

/// Serves one admitted request: the owed round first, so the reply never coexists with a stale
/// cached name or attribute (§4.6), then the dispatch and its reply; then the cursor moves past the
/// request's own records — only when the round was delivered whole ([`Coherence::served_own_request`]).
fn serve_request(
  channel: &mut FuseChannel,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  request: &[u8],
  state: &mut ServeState,
) -> Result<(Delivered, i32), ChannelError> {
  let delivered = deliver_owed(channel, bridge, cx, state)?;
  let (n, _) = dispatch_request(request, bridge, cx, state);
  let reply = state.reply.get(..n).unwrap_or(&[]);
  let error = reply_error(reply);
  if n > 0 && channel.write_reply(reply)? == Sent::Unmatched {
    let parsed = Request::parse(request).ok();
    let opcode = parsed.as_ref().and_then(|parsed| parsed.opcode);
    crate::bridge::reclaim_unreported(
      opcode,
      parsed.as_ref().map_or(0, |parsed| parsed.header.nodeid),
      reply,
      bridge,
      cx,
    );
    forget_unreported_open(
      state,
      opcode,
      parsed.as_ref().map_or(0, |parsed| parsed.header.nodeid),
      n,
    );
  } else {
    note_delivered(request, n, state);
  }
  state.coherence.served_own_request(bridge, cx, delivered);
  Ok((delivered, error))
}

/// A request served by [`dispatch_ready`] whose reply waits for its owner: the opcode, the round delivered
/// before it, the reply's errno, and where the reply sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatched {
  /// The request's opcode, or `None` for one slates does not serve.
  pub opcode: Option<Opcode>,
  /// The round delivered before it.
  pub delivered: Delivered,
  /// The reply's errno (zero for success, or for a request that takes no reply).
  pub error: i32,
  unique: u64,
  nodeid: u64,
  len: usize,
  /// The request as the kernel sent it, kept for what its owner records with it (a write's bytes, A-63).
  request: Vec<u8>,
  /// Where the kernel's cache stood just before the request was served: a refused reply rewinds to it, so the
  /// request's own applied change is delivered as an invalidation ([`redeliver_after_refusal`]).
  cursor_before: Option<slates_bridge_core::InvalidationCursor>,
  /// For a cache write-back, the byte ranges it was not applied over (another path changed them after the kernel's copy
  /// was fetched); empty for every other request. What it applied, and so what its owner logs, excludes them.
  kept_back: Vec<std::ops::Range<u64>>,
}

impl Dispatched {
  /// The request's opcode, when slates serves it.
  pub fn opcode(&self) -> Option<Opcode> {
    self.opcode
  }

  /// The node the request named (the kernel's node id; the root is 1).
  pub fn nodeid(&self) -> u64 {
    self.nodeid
  }

  /// The parts of a `WRITE` that were applied, each as its offset and bytes, for the owner to log before the reply
  /// (A-63); empty for any other request. A plain write is one part; a cache write-back is the parts outside what it
  /// was kept back from ([`crate::bridge::unmasked_segments`]), so a recovery never replays bytes the volume refused.
  pub fn written(&self) -> Vec<(u64, &[u8])> {
    let Some(parsed) = Request::parse(&self.request).ok() else {
      return Vec::new();
    };
    if parsed.opcode != Some(Opcode::Write) {
      return Vec::new();
    }
    let Ok(write) = crate::request::WriteIn::parse(Opcode::Write.to_wire(), parsed.body) else {
      return Vec::new();
    };
    crate::bridge::unmasked_segments(write.offset, write.data.len(), &self.kept_back)
      .into_iter()
      .filter_map(|(at, range)| write.data.get(range).map(|bytes| (at, bytes)))
      .collect()
  }

  /// The request's unique id, without the kernel's resend bit: what a recorded reply is matched by (A-61).
  pub fn unique(&self) -> u64 {
    self.unique & !crate::abi::UNIQUE_RESEND
  }

  /// The reply this request's dispatch left in `state`, before [`send_reply`] writes it: what a barrier publishes
  /// with the effect (A-61). Empty for a request that takes no reply.
  pub fn reply<'s>(&self, state: &'s ServeState) -> &'s [u8] {
    state.reply.get(..self.len).unwrap_or(&[])
  }

  /// Whether the kernel resent this request after a `FUSE_NOTIFY_RESEND` ([`crate::abi::UNIQUE_RESEND`]): a
  /// request a previous daemon read and never answered (A-61).
  pub fn resent(&self) -> bool {
    self.unique & crate::abi::UNIQUE_RESEND != 0
  }

  /// Whether the request's reply waits for its owner's barrier ([`crate::bridge::needs_barrier`]).
  pub fn needs_barrier(&self) -> bool {
    crate::bridge::needs_barrier(self.opcode, self.error)
  }
}

/// What one non-blocking turn of [`dispatch_ready`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
  /// No request was waiting: the owner awaits the device's readiness again.
  Idle,
  /// A read shorter than a header — a message the kernel never sends — dropped.
  Dropped,
  /// The mount ended: the kernel disconnected, or the attachment admits no more requests.
  Ended,
  /// A request was dispatched; its reply waits in the state for [`send_reply`].
  Dispatched(Dispatched),
  /// A resent request was answered from the reply its dead daemon published and never delivered (A-61): applied
  /// once, by that daemon, never again.
  Replayed,
  /// A slice of a running allocation landed and more remains (A-108): the owner yields, then turns again.
  Continued,
}

/// One non-blocking turn for an owner that makes a request's effect durable before answering (the daemon,
/// §4.8): reads a waiting request ([`FuseChannel::try_read_request`]), delivers the owed round, dispatches
/// it to `bridge` under `attachment` — admitted and ended around the service as in [`serve_step`] — and
/// leaves the reply in `state` for [`send_reply`]. A request that takes no reply is answered by nothing.
pub fn dispatch_ready(
  channel: &mut FuseChannel,
  bridge: &mut dyn Bridge,
  attachments: &mut Attachments,
  attachment: AttachmentId,
  state: &mut ServeState,
  slice_bytes: u64,
) -> Result<Turn, ChannelError> {
  if let Some((allocation, cursor_before)) = state.allocating.take() {
    return continue_allocation(
      channel,
      bridge,
      attachments,
      attachment,
      state,
      (allocation, cursor_before),
      slice_bytes,
    );
  }
  let read = match channel.try_read_request() {
    Ok(Some(read)) => read,
    Ok(None) => return Ok(Turn::Idle),
    Err(ChannelError::Disconnected) => return Ok(Turn::Ended),
    Err(e) => return Err(e),
  };
  if read < IN_HEADER_LEN {
    return Ok(Turn::Dropped);
  }
  let Ok(cx) = attachments.begin(attachment) else {
    return Ok(Turn::Ended);
  };
  let request = channel.take_request();
  let parsed = Request::parse(&request).ok();
  let unique = parsed.as_ref().map_or(0, |parsed| parsed.header.unique);
  let nodeid = parsed.as_ref().map_or(0, |parsed| parsed.header.nodeid);
  if let Some(answer) = state.replay_for(unique) {
    attachments.end(attachment);
    channel.write_reply(&answer)?;
    return Ok(Turn::Replayed);
  }
  let opcode = parsed.and_then(|parsed| {
    if parsed.opcode == Some(Opcode::Init) {
      state.negotiated(parsed.body);
    }
    parsed.opcode
  });
  let delivered = deliver_owed(channel, bridge, &cx, state);
  if let (Some(Opcode::Fallocate), Ok(delivered)) = (opcode, &delivered) {
    let delivered = *delivered;
    let cursor_before = state.coherence.cursor();
    let began_at = bridge.seen(&cx).journal_seq;
    let begun = Allocation::begin(&request, bridge, &cx, &mut state.reply);
    note_own_records(bridge, &cx, state, began_at);
    state.coherence.served_own_request(bridge, &cx, delivered);
    attachments.end(attachment);
    return match begun {
      Begun::Running(allocation) => continue_allocation(
        channel,
        bridge,
        attachments,
        attachment,
        state,
        (allocation, cursor_before),
        slice_bytes,
      ),
      Begun::Answered(len) => {
        let error = state.reply.get(..len).map_or(0, reply_error);
        Ok(Turn::Dispatched(Dispatched {
          opcode,
          delivered,
          error,
          unique,
          nodeid,
          len,
          request,
          cursor_before,
          kept_back: Vec::new(),
        }))
      }
    };
  }
  let dispatched = delivered.map(|delivered| {
    let cursor_before = state.coherence.cursor();
    let (len, kept_back) = dispatch_request(&request, bridge, &cx, state);
    let error = state.reply.get(..len).map_or(0, reply_error);
    state.coherence.served_own_request(bridge, &cx, delivered);
    Dispatched {
      opcode,
      delivered,
      error,
      unique,
      nodeid,
      len,
      request,
      cursor_before,
      kept_back,
    }
  });
  attachments.end(attachment);
  dispatched.map(Turn::Dispatched)
}

/// One slice of a running allocation (A-108), admitted and ended around its service as a request is: the owed round
/// first, then the slice. While more remains the allocation is kept and the turn is [`Turn::Continued`]; once it is
/// done, or a slice is refused, its reply waits in the state as a dispatched request's does.
fn continue_allocation(
  channel: &mut FuseChannel,
  bridge: &mut dyn Bridge,
  attachments: &mut Attachments,
  attachment: AttachmentId,
  state: &mut ServeState,
  (mut allocation, cursor_before): (Allocation, Option<slates_bridge_core::InvalidationCursor>),
  slice_bytes: u64,
) -> Result<Turn, ChannelError> {
  let Ok(cx) = attachments.begin(attachment) else {
    return Ok(Turn::Ended);
  };
  let delivered = match deliver_owed(channel, bridge, &cx, state) {
    Ok(delivered) => delivered,
    Err(e) => {
      attachments.end(attachment);
      return Err(e);
    }
  };
  let stepped_at = bridge.seen(&cx).journal_seq;
  let answered = allocation.step(bridge, &cx, slice_bytes, &mut state.reply);
  note_own_records(bridge, &cx, state, stepped_at);
  state.coherence.served_own_request(bridge, &cx, delivered);
  attachments.end(attachment);
  let Some(len) = answered else {
    state.allocating = Some((allocation, cursor_before));
    return Ok(Turn::Continued);
  };
  let parsed = Request::parse(allocation.request()).ok();
  let unique = parsed.as_ref().map_or(0, |parsed| parsed.header.unique);
  let nodeid = parsed.as_ref().map_or(0, |parsed| parsed.header.nodeid);
  Ok(Turn::Dispatched(Dispatched {
    opcode: Some(Opcode::Fallocate),
    delivered,
    error: state.reply.get(..len).map_or(0, reply_error),
    unique,
    nodeid,
    len,
    request: allocation.into_request(),
    cursor_before,
    kept_back: Vec::new(),
  }))
}

/// Writes a dispatched request's reply — or, when its owner could not make the effect durable, an error
/// reply with `refuse_with` (an errno; `EIO` for a refused barrier) in its place, so the caller is told
/// rather than promised survival. A request that takes no reply writes nothing either way. A reply that is
/// replaced, or that the kernel answers [`Sent::Unmatched`], never told its caller what it granted, so the
/// owner reclaims it with [`reclaim_dispatched`] — before this call when it refuses, since the refusal
/// overwrites the reply (AUD-29-85).
pub fn send_reply(
  channel: &FuseChannel,
  state: &mut ServeState,
  dispatched: &Dispatched,
  refuse_with: Option<i32>,
) -> Result<Sent, ChannelError> {
  if dispatched.len == 0 {
    note_delivered(&dispatched.request, 0, state);
    return Ok(Sent::Delivered);
  }
  match refuse_with {
    None => {
      let sent = channel.write_reply(state.reply.get(..dispatched.len).unwrap_or(&[]))?;
      if sent == Sent::Delivered {
        note_delivered(&dispatched.request, dispatched.len, state);
      }
      Ok(sent)
    }
    Some(errno) => {
      let n = ReplyHeader::write_error(dispatched.unique, errno, &mut state.reply)
        .map_err(ChannelError::from)?;
      channel.write_reply(state.reply.get(..n).unwrap_or(&[]))
    }
  }
}

/// After [`send_reply`] replaced a request's reply with an error, delivers the request's own applied change to the
/// kernel as an invalidation (§4.6 "Cache posture"): the cursor goes back to where it stood before the request and one
/// round runs. The caller was told the operation failed while it took effect in the volume (a refused barrier: the
/// change is applied, not promised to survive), so without this the kernel kept what the failure implied — a
/// `mkdir` answered `EIO` left a cached negative entry for a directory that exists. Called after the error reply,
/// never before: an entry invalidation takes the directory's lock, which the waiting caller holds until its reply.
pub fn redeliver_after_refusal(
  channel: &FuseChannel,
  state: &mut ServeState,
  dispatched: &Dispatched,
  bridge: &mut dyn Bridge,
  attachments: &mut Attachments,
  attachment: AttachmentId,
) -> Result<Delivered, ChannelError> {
  let Ok(cx) = attachments.begin(attachment) else {
    return Ok(Delivered::default());
  };
  state.coherence.rewind(dispatched.cursor_before);
  let delivered = deliver_owed(channel, bridge, &cx, state);
  attachments.end(attachment);
  delivered
}

/// Gives back what a dispatched request's success reply granted — its lookup references and open handle —
/// when the reply will not reach the caller (a refused barrier replaces it, or the kernel no longer waits for
/// it), under `attachment` as the dispatch was. Call it while the reply is still in `state`: before
/// [`send_reply`] refuses, or after it answers [`Sent::Unmatched`] (AUD-29-85).
pub fn reclaim_dispatched(
  state: &mut ServeState,
  dispatched: &Dispatched,
  bridge: &mut dyn Bridge,
  attachments: &mut Attachments,
  attachment: AttachmentId,
) -> crate::bridge::Reclaimed {
  let Ok(cx) = attachments.begin(attachment) else {
    // A revoked attachment is swept whole by its teardown; nothing is owed one reply at a time.
    return crate::bridge::Reclaimed::default();
  };
  let reclaimed = crate::bridge::reclaim_unreported(
    dispatched.opcode,
    dispatched.nodeid,
    state.reply.get(..dispatched.len).unwrap_or(&[]),
    bridge,
    &cx,
  );
  attachments.end(attachment);
  forget_unreported_open(state, dispatched.opcode, dispatched.nodeid, dispatched.len);
  reclaimed
}

/// The blocking serve loop (the fallback path, §4.6): [`wait`] for a request or a signalled change,
/// then [`serve_step`], until the kernel unmounts or the attachment admits no more. The io_uring
/// command path replaces this on 6.14+; both drive the same [`dispatch`]. An owner that interleaves
/// other work with the mount (a shard serving verbs) drives `wait` and `serve_step` itself, applying
/// its own changes between them and signalling `signal` so they are delivered.
pub fn serve_blocking(
  channel: &mut FuseChannel,
  bridge: &mut dyn Bridge,
  attachments: &mut Attachments,
  attachment: AttachmentId,
  signal: Option<&ChangeSignal>,
) -> Result<(), ChannelError> {
  let mut state = ServeState::new();
  loop {
    let wake = wait(channel, signal)?;
    if serve_step(channel, bridge, attachments, attachment, &mut state, wake)? == Step::Ended {
      return Ok(());
    }
  }
}

impl FuseChannel {
  /// The last request read, copied out so the reply may be written into the channel's own
  /// buffer without aliasing (the copy is one memcpy of a small message; the io_uring path
  /// avoids it with registered buffers, owed).
  fn take_request(&self) -> Vec<u8> {
    // The blocking loop read into `buffer`; the request is its head up to the header's length.
    let len = self
      .buffer
      .get(..size_of::<u32>())
      .map(|b| u32::from_le_bytes(b.try_into().unwrap_or_default()))
      .map(|l| usize::try_from(l).unwrap_or(0).min(self.buffer.len()))
      .unwrap_or(0);
    self.buffer.get(..len).unwrap_or_default().to_vec()
  }
}

/// A stable reference for `FuseError` so the transport's error can carry a codec refusal where
/// one is surfaced to callers (kept for the driver's structured errors).
impl From<FuseError> for ChannelError {
  fn from(_: FuseError) -> ChannelError {
    ChannelError::Device {
      call: "codec",
      code: None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: a recorded reply: `fuse_out_header` (len 16, error 0, unique 41) and no body.
  const RECORDED: [u8; 16] = [16, 0, 0, 0, 0, 0, 0, 0, 41, 0, 0, 0, 0, 0, 0, 0];

  fn restored() -> ServeState {
    let session = Session {
      expire_only: true,
      kernel_resends: true,
      kernel_bumps_epoch: true,
    };
    ServeState::restored(session, Some((41, RECORDED.to_vec())))
  }

  /// Step 3 of "Journal budget derived" (a restored mount tracks its pre-takeover files from its start): the release of a
  /// handle the mount never saw opened drops its file's byte states once nothing of the file is open here. Do: track
  /// files 7 and 8 from the mount's start; answer a release of 7 while the volume still holds a restored open of it,
  /// then one with none left; then a release of 8 while a handle the mount noted still holds it. Expect 7 kept, then
  /// dropped, and 8 kept.
  #[test]
  fn a_released_pre_takeover_files_states_go_once_nothing_of_it_is_open() {
    let mut state = restored();
    state.states.grow_room(2);
    state.states.adopted(7, 1, Some(&[]));
    state.states.adopted(8, 1, Some(&[]));
    drop_released_restored(&mut state, 7, &[7]);
    assert!(state.states.tracks(7), "a restored open of it is left");
    drop_released_restored(&mut state, 7, &[]);
    assert!(!state.states.tracks(7), "none left: its states go");
    state.writable.opened(3, 8);
    drop_released_restored(&mut state, 8, &[]);
    assert!(
      state.states.tracks(8),
      "a handle the mount noted still holds it"
    );
  }

  /// A-61 (the exact replay). Do: offer a restored state the resent request its dead daemon answered, then the same
  /// unique again. Expect: the recorded reply, its unique set to the resent request's (resend bit included, as
  /// the kernel matches the whole word); the second time nothing — answered once.
  #[test]
  fn a_resent_request_is_answered_from_its_record_once() {
    let mut state = restored();
    let resent = 41 | crate::abi::UNIQUE_RESEND;
    let answer = state.replay_for(resent).unwrap();
    assert_eq!(
      answer.get(..8),
      RECORDED.get(..8),
      "length and error unchanged"
    );
    assert_eq!(
      answer.get(8..16),
      Some(&resent.to_le_bytes()[..]),
      "the resent unique, whole"
    );
    assert_eq!(state.replay_for(resent), None, "answered once");
  }

  /// A-61. Do: offer the recorded unique without the resend bit (a request the kernel never resent: uniques are
  /// not reused, so this would be another request), and a different resent unique. Expect: neither is answered from
  /// the record, which stays for its own request.
  #[test]
  fn only_the_resent_request_the_record_answers_is_replayed() {
    let mut state = restored();
    assert_eq!(state.replay_for(41), None, "not resent");
    assert_eq!(
      state.replay_for(42 | crate::abi::UNIQUE_RESEND),
      None,
      "another request"
    );
    assert!(
      state.replay_for(41 | crate::abi::UNIQUE_RESEND).is_some(),
      "the record stayed for its request"
    );
  }

  /// Shape: how long the drop test waits before closing the pipe's reader, so the notifier is dropped while its write
  /// is still blocked.
  const READER_CLOSES_AFTER: std::time::Duration = std::time::Duration::from_millis(50);

  /// Fills the pipe behind `writer` so a write to it blocks until its reader drains it. The non-blocking flag belongs
  /// to the open pipe, which the notifier's descriptor shares, so it is set only while the notifier waits for a round.
  fn fill(writer: &OwnedFd) {
    rustix::fs::fcntl_setfl(writer, rustix::fs::OFlags::NONBLOCK).expect("non-blocking");
    let block = [0u8; 4096];
    while let Ok(n) = rustix::io::write(writer, &block) {
      if n == 0 {
        break;
      }
    }
    rustix::fs::fcntl_setfl(writer, rustix::fs::OFlags::empty()).expect("blocking again");
  }

  /// Drains `reader` until `notifier`'s round is back, and returns its outcome.
  /// Shape: how long a drained round may take to come back — far above a thread's wake-up.
  const DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

  /// Drains `reader` until `notifier`'s round is back, within [`DRAIN_WAIT`], and returns its outcome. The reader is
  /// non-blocking (its own open pipe end, so the notifier's write still blocks): a read of an empty pipe must not wait
  /// for a write that has already landed, which a blocking read here once did, hanging the test.
  fn drained(notifier: &mut Notifier, reader: &OwnedFd) -> Option<Outcome> {
    rustix::fs::fcntl_setfl(reader, rustix::fs::OFlags::NONBLOCK).expect("a non-blocking reader");
    let mut sink = vec![0u8; 1 << 16];
    let deadline = std::time::Instant::now() + DRAIN_WAIT;
    while std::time::Instant::now() < deadline {
      let _ = rustix::io::read(reader, &mut sink);
      if let Some((outcome, _before)) = notifier.returned() {
        return Some(outcome);
      }
      std::thread::yield_now();
    }
    None
  }

  /// §4.6 Cache posture (`docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md`; A-142). Do: twice, fill
  /// the device's pipe, hand the mount's notifier a round, check it as the serve loop does, then drain. Expect: the
  /// owner is **not** blocked while either write waits, and each round completes once its write can land, refusing
  /// nothing — the second through the same notifier, whose thread would be gone had it been joined after the first.
  #[test]
  fn a_round_whose_write_blocks_does_not_hold_its_owner_and_the_next_reuses_its_thread() {
    let (reader, writer) = rustix::pipe::pipe().expect("a pipe");
    let refill = rustix::io::dup(&writer).expect("a second descriptor on the pipe");
    let mut notifier = Notifier::start(writer, None).expect("the notifier starts");
    for round in 1..=2 {
      fill(&refill);
      notifier
        .send(vec![(vec![7u8; 64], false)], None)
        .expect("the round is handed to the mount's thread");
      assert!(
        notifier.returned().is_none(),
        "round {round}: the owner is free while the round's write waits"
      );
      assert!(
        matches!(drained(&mut notifier, &reader), Some(Ok(0))),
        "round {round} completed once its write could land, refusing nothing"
      );
    }
  }

  /// A-142: a notifier dropped with a round out ends its thread. Do: fill the device's pipe, hand the notifier a round,
  /// then drop the notifier while the write waits, the pipe's reader closing a moment later. Expect the drop to return:
  /// the write fails once the reader is gone, the thread finds the round channel closed and ends, and is joined.
  #[test]
  fn a_notifier_dropped_with_a_round_out_joins_its_thread() {
    let (reader, writer) = rustix::pipe::pipe().expect("a pipe");
    let refill = rustix::io::dup(&writer).expect("a second descriptor on the pipe");
    let mut notifier = Notifier::start(writer, None).expect("the notifier starts");
    fill(&refill);
    drop(refill);
    notifier
      .send(vec![(vec![7u8; 64], false)], None)
      .expect("the round is handed over");
    std::thread::scope(|scope| {
      scope.spawn(move || {
        // The test paces itself; shipped code parks on its driver (D-9).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(READER_CLOSES_AFTER);
        drop(reader);
      });
      drop(notifier);
    });
  }
}
