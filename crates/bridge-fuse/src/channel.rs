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
use crate::bridge::Bridge;
use crate::coherence::{Coherence, Delivered, FetchPoints, OwnRecords, WritableOpens};
use crate::dispatch;
use crate::error::FuseError;
use crate::init::negotiate;
use crate::notify::{EXPIRE_ONLY, inval_entry, inval_inode};
use crate::reply::ReplyHeader;
use crate::request::Request;
use crate::session::Session;
use slates_bridge_core::{AttachmentId, Attachments, Invalidation};

/// Format: the device the kernel's FUSE client and the daemon exchange messages over.
const FUSE_DEVICE: &str = "/dev/fuse";

/// Shape: the request buffer size: the negotiated maximum write (256 KiB) plus a page for the
/// headers, the size the kernel expects a reader to offer so a large write arrives in one read.
const BUFFER_BYTES: usize = 256 * 1024 + 4096;

/// Format: the longest notification: the header, the 24-byte inode or delete body, and a name up
/// to the volume's name cap with its NUL.
const NOTIFY_BYTES: usize = OUT_HEADER_LEN + 24 + 255 + 1;

/// Format: `fuse_notify_inval_inode_out.off` of -1 — invalidate the attributes only, no data.
const ATTRIBUTES_ONLY: i64 = -1;
/// Format: `fuse_notify_inval_inode_out.len` of -1 with a zero offset — the whole data range.
const WHOLE_FILE: i64 = -1;

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
  /// ([`NotifyThread`]). The notification write blocks in the kernel, so it must never run on the thread that
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
/// an **entry** invalidation, because the kernel's `ENOENT` means something different for one (see [`NotifyThread`]).
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
      if *data {
        inval_inode(*ino, 0, WHOLE_FILE, &mut out)
      } else {
        inval_inode(*ino, ATTRIBUTES_ONLY, 0, &mut out)
      }
    }
  };
  let n = encoded.ok()?;
  Some((out.get(..n)?.to_vec(), entry))
}

/// A round of kernel invalidations written **away from the serving thread** (§4.6 "Cache posture"), owned by the
/// [`ServeState`] that started it: the thread writes each notification, sends the round's outcome over a channel of
/// one and wakes the loop. Dropped before the outcome came (an unmount while a round is out), it joins the thread
/// first, so no thread outlives its owner.
///
/// It exists because the write blocks. `fuse_reverse_inval_entry()` takes the parent directory's inode lock, which a
/// concurrent lookup holds for the whole request — and the only thread that would answer that lookup is the serving
/// thread. Writing a notification there deadlocked the mount outright
/// (`docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md`): the serving thread must stay free to answer,
/// because answering is what releases the lock the write is waiting on. `FUSE_EXPIRE_ONLY` does not avoid it (the
/// kernel locks the parent before it reads the flags), and nor does any ordering inside our own loop (the kernel
/// holds the lock from the moment it issues the request, whenever we read it). This mirrors the landing barrier's
/// thread (A-132, `slates-server` `landing.rs`), which exists for the same reason.
struct NotifyThread {
  /// The round's outcome: how many entry invalidations the kernel refused, or the device's own refusal.
  outcome: std::sync::mpsc::Receiver<Result<u32, ChannelError>>,
  thread: Option<std::thread::JoinHandle<()>>,
  /// Where the cursor stood before the round, to put it back if the kernel refuses the write.
  before: Option<slates_bridge_core::InvalidationCursor>,
}

impl NotifyThread {
  /// Starts the round on its own thread. `wake` is signalled once the outcome is sent, so the loop sees it without
  /// waiting for the kernel's next request. Refused with the system's errno when no thread can start.
  fn start(
    device: OwnedFd,
    messages: Vec<(Vec<u8>, bool)>,
    before: Option<slates_bridge_core::InvalidationCursor>,
    wake: Option<ChangeNotifier>,
  ) -> Result<NotifyThread, ChannelError> {
    let (send, outcome) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
      .name("slates-fuse-notify".to_owned())
      .spawn(move || {
        let mut round = Ok(0u32);
        for (message, entry) in &messages {
          // `ENOTEMPTY` is the kernel declining to drop a directory that is in use, and `ENOENT` on an *inode*
          // invalidation is the kernel not holding that inode — neither is a fault of the mount. `ENOENT` on an
          // **entry** invalidation is not benign and is counted, not discarded: on Linux 6.19 the kernel refuses it
          // and goes on answering from the cached negative entry, so the name a neighbour created is never seen
          // (`docs/bugs/2026-10-19-linux-6-19-refuses-the-entry-invalidation-slates-swallows.md`). It is counted
          // rather than raised because the refusal is not retryable — the next round would be refused the same way,
          // and a round kept for retry is an unbounded loop.
          match rustix::io::write(&device, message) {
            Ok(_) => {}
            Err(rustix::io::Errno::NOENT) if *entry => {
              if let Ok(refused) = round.as_mut() {
                *refused = refused.saturating_add(1);
              }
            }
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => {}
            Err(e) => {
              round = Err(ChannelError::Device {
                call: "notify",
                code: Some(e.raw_os_error()),
              });
              break;
            }
          }
        }
        // The receiver outlives the thread (its owner joins before it drops), so the send cannot fail; were it to,
        // nobody is left to tell.
        if send.send(round).is_ok()
          && let Some(wake) = wake
        {
          let _ = wake.notify();
        }
      })
      .map_err(|e| ChannelError::Device {
        call: "notify-thread",
        code: e.raw_os_error(),
      })?;
    Ok(NotifyThread {
      outcome,
      thread: Some(thread),
      before,
    })
  }

  /// The round's outcome once it is back (its thread joined: it has only to exit), `None` while it is still being
  /// written. A thread gone without an outcome (it cannot be: shipped code does not panic, and a panic aborts) reads
  /// as the device refusing.
  fn returned(&mut self) -> Option<Result<u32, ChannelError>> {
    let round = match self.outcome.try_recv() {
      Ok(round) => round,
      Err(std::sync::mpsc::TryRecvError::Empty) => return None,
      Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(ChannelError::Device {
        call: "notify",
        code: rustix::io::Errno::IO.raw_os_error().into(),
      }),
    };
    self.join();
    Some(round)
  }

  fn join(&mut self) {
    if let Some(thread) = self.thread.take()
      && thread.join().is_err()
    {
      eprintln!("slates-bridge-fuse: a notify thread panicked");
    }
  }
}

impl Drop for NotifyThread {
  fn drop(&mut self) {
    self.join();
  }
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
  /// invalidations in [`NotifyThread`]).
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
  /// A round of invalidations being written away from this thread ([`NotifyThread`]). At most one round is out at a
  /// time: the gather consumes what it reports ([`slates_bridge_core::Bridge::invalidations`] takes the base plane's
  /// stale entries), so a second round is never started while one is in flight — which is also its bound.
  notifying: Option<NotifyThread>,
  /// Wakes this loop when a round finishes, so its outcome is seen and the next round started without waiting for
  /// the kernel's next request. Absent in a harness that drives the loop itself.
  notify_wake: Option<ChangeNotifier>,
  /// Entry invalidations the kernel refused with `ENOENT` since this was last read. Every one of them is a name
  /// this mount believes it has told the kernel about and has not: the kernel kept the cached entry. Counted so a
  /// mount losing coherence is visible instead of silent (CLAUDE.md #9); read and cleared by the owner that
  /// reports it.
  entry_invalidations_refused: u64,
  /// The journal records this mount's own requests made: the bytes they wrote are fresh in its kernel, so a later cache
  /// write-back is not held stale there (the dirty-page write-back rule, [`dispatch_request`]).
  own: OwnRecords,
  /// The handles this mount's kernel holds open for writing: only their files can have dirty pages.
  writable: WritableOpens,
  /// When each cached page of a file open for writing was fetched, which is what a cache write-back is judged by.
  fetched: FetchPoints,
  /// The journal's head when this mount served its first request: the point a file with no record here is judged
  /// from — one a restored mount's kernel opened before this daemon served it (A-61).
  started_at: Option<u64>,
  /// Bytes of cache write-backs kept back because another path changed them after the kernel's confirmed point, since
  /// this was last read: the rule's non-vacuity count. Read and cleared by the owner that reports it.
  writeback_bytes_kept_back: u64,
  /// Cache write-backs refused `EIO` because the journal no longer held every change since the confirmed point, since
  /// this was last read. Read and cleared by the owner that reports it.
  writebacks_unknown: u64,
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
      notifying: None,
      notify_wake: None,
      entry_invalidations_refused: 0,
      own: OwnRecords::default(),
      writable: WritableOpens::default(),
      fetched: FetchPoints::default(),
      started_at: None,
      writeback_bytes_kept_back: 0,
      writebacks_unknown: 0,
    }
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

  /// The entry invalidations the kernel has refused since this was last asked, and zero afterwards. A non-zero
  /// answer means the kernel is still serving names this mount has tried to invalidate.
  pub fn take_entry_invalidations_refused(&mut self) -> u64 {
    std::mem::take(&mut self.entry_invalidations_refused)
  }

  /// Wakes this state's serve loop whenever a round of invalidations finishes being written (§4.6): the round runs
  /// off the serving thread, so its outcome arrives out of band.
  pub fn wakes_with(&mut self, notifier: ChangeNotifier) {
    self.notify_wake = Some(notifier);
  }

  /// The state of a device taken over from a previous daemon: what its connection negotiated, the reply that
  /// daemon published and never delivered (if any), and nothing in the kernel's cache that this daemon knows of.
  pub fn restored(session: Session, replay: Option<(u64, Vec<u8>)>) -> ServeState {
    ServeState {
      expire_only: session.expire_only,
      session: Some(session),
      replay,
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
/// unsolicited notification **on a thread of its own** ([`NotifyThread`]) — never here, because the write blocks in
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
  if let Some(round) = state.notifying.as_mut() {
    let Some(outcome) = round.returned() else {
      // Still being written: nothing new may be gathered (the gather consumes), and the cursor must not advance.
      return Ok(Delivered {
        written: 0,
        gather_refused: true,
      });
    };
    let before = round.before;
    state.notifying = None;
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
  let mut messages: Vec<(Vec<u8>, bool)> = Vec::new();
  let delivered = state
    .coherence
    .deliver(bridge, cx, &mut |invalidation: &Invalidation| {
      if let Some(message) =
        encode_invalidation(&addressed_to_the_kernel(invalidation, root), expire_only)
      {
        messages.push(message);
      }
      Ok::<(), ChannelError>(())
    })?;
  if !messages.is_empty() {
    let wake = match state.notify_wake.as_ref() {
      Some(notifier) => Some(notifier.try_clone()?),
      None => None,
    };
    state.notifying = Some(NotifyThread::start(
      channel.device_clone()?,
      messages,
      before,
      wake,
    )?);
  }
  Ok(delivered)
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
  let before = bridge.seen(cx).journal_seq;
  let started_at = *state.started_at.get_or_insert(before);
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
/// - a `READ` of a file open for writing records the journal's head as its pages' fetch point, since a read changes
///   nothing;
/// - a `RELEASE` forgets its handle, and with the file's last writable handle its fetch points (no write-back of it can
///   follow, since a write-back holds its file open until it is answered).
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
        state.fetched.opened(file, bridge.seen(cx).journal_seq);
      }
    }
    Some(Opcode::Read) => {
      let answered = state
        .reply
        .get(..len)
        .is_some_and(|reply| reply_error(reply) == 0);
      let read = crate::request::ReadIn::parse(Opcode::Read.to_wire(), parsed.body);
      if let (true, Ok(read)) = (answered && state.writable.holds(ino), read) {
        let end = read.offset.saturating_add(u64::from(read.size));
        let bound = bridge.journal_capacity(cx);
        state.fetched.fetched(ino, read.offset..end, before, bound);
      }
    }
    Some(Opcode::Release) => {
      let handle = parsed
        .body
        .get(..size_of::<u64>())
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes);
      if let Some(file) = handle.and_then(|fh| state.writable.released(fh)) {
        state.fetched.dropped(file);
      }
    }
    _ => {}
  }
}

/// Forgets the writable handle a lost `OPEN` or `CREATE` reply granted, when the server gives it back itself
/// ([`crate::bridge::reclaim_unreported`]), and with the file's last such handle its fetch points.
fn forget_unreported_open(state: &mut ServeState, opcode: Option<Opcode>, len: usize) {
  let reply = state.reply.get(..len).unwrap_or(&[]);
  if let Some(file) =
    crate::bridge::granted_handle(opcode, reply).and_then(|fh| state.writable.released(fh))
  {
    state.fetched.dropped(file);
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
}

/// Serves a cache write-back of node `nodeid` by the dirty-page write-back rule ([`dispatch_request`]): applied whole
/// when nothing changed since the kernel's confirmed point, outside the changed ranges when some did, and refused `EIO`
/// when the journal no longer holds every change since — then neither applying (which may revert an acknowledged
/// write) nor dropping (which would discard data the process synced) is safe, and the process is told at its next
/// `fsync` or `msync`.
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

/// The bytes of a cache write-back of node `nodeid` that are stale: for each part of its range with the point that part
/// was fetched at ([`FetchPoints::parts`]), what another path changed after that point, this mount's own writes since
/// excepted ([`slates_bridge_core::Bridge::changed_since`]). Ascending and disjoint, since the parts are. `None` when it
/// cannot be told: the file's fetch points were lost, the journal no longer holds every change since a part's point, or
/// the write-back cannot be read.
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
  let mut stale = Vec::new();
  for (part, point) in state.fetched.parts(nodeid, write.offset..end, started_at)? {
    let slates_bridge_core::Changed::Known(changed) = bridge
      .changed_since(cx, point, nodeid, state.own.spans())
      .ok()?
    else {
      return None;
    };
    stale.extend(changed.into_iter().filter_map(|range| {
      let (start, end) = (range.start.max(part.start), range.end.min(part.end));
      (start < end).then_some(start..end)
    }));
  }
  Some(stale)
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
    forget_unreported_open(state, opcode, n);
  }
  state.coherence.served_own_request(bridge, cx, delivered);
  Ok((delivered, error))
}

/// The errno a reply carries (`fuse_out_header.error`, negated on the wire), as a positive number;
/// zero for success or for no reply.
fn reply_error(reply: &[u8]) -> i32 {
  /// Format: `fuse_out_header`: `len` (u32) then `error` (i32) — the errno field's offset.
  const ERROR_AT: usize = size_of::<u32>();
  reply
    .get(ERROR_AT..ERROR_AT.saturating_add(size_of::<i32>()))
    .and_then(|bytes| bytes.try_into().ok())
    .map_or(0, |bytes| i32::from_le_bytes(bytes).saturating_neg())
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
  /// For a cache write-back, the byte ranges it was not applied over (another path changed them after the kernel's
  /// confirmed point); empty for every other request. What it applied, and so what its owner logs, excludes them.
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
    return Ok(Sent::Delivered);
  }
  match refuse_with {
    None => channel.write_reply(state.reply.get(..dispatched.len).unwrap_or(&[])),
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
  forget_unreported_open(state, dispatched.opcode, dispatched.len);
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
    };
    ServeState::restored(session, Some((41, RECORDED.to_vec())))
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

  /// §4.6 Cache posture (`docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md`). Do: start a round of
  /// invalidations whose write cannot complete (its destination is full), then carry on as the serve loop does.
  /// Expect: the owner is **not** blocked — it keeps running and sees the round still out — and once the
  /// destination drains the round completes and its thread is joined. This is the invariant the deadlock broke: a
  /// notification write must never hold the thread that answers requests, because answering is what unblocks it.
  #[test]
  fn a_round_whose_write_blocks_does_not_hold_its_owner() {
    let (reader, writer) = rustix::pipe::pipe().expect("a pipe");
    // Fill the pipe so the round's write has nowhere to go: non-blocking while filling, blocking for the round.
    rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::NONBLOCK).expect("non-blocking");
    let block = [0u8; 4096];
    let mut filled = 0usize;
    while let Ok(n) = rustix::io::write(&writer, &block) {
      filled = filled.saturating_add(n);
      if n == 0 || filled > (1 << 22) {
        break;
      }
    }
    rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::empty()).expect("blocking again");

    let mut round = NotifyThread::start(writer, vec![(vec![7u8; 64], false)], None, None)
      .expect("the round starts");
    // The owner carries on: the round is out, and asking does not block.
    assert!(
      round.returned().is_none(),
      "the owner is free while the round's write waits"
    );

    // Drain, so the blocked write can land; the round then comes back and its thread is joined.
    let mut sink = vec![0u8; 1 << 16];
    let mut outcome = None;
    for _ in 0..256 {
      let _ = rustix::io::read(&reader, &mut sink);
      if let Some(back) = round.returned() {
        outcome = Some(back);
        break;
      }
    }
    assert!(
      matches!(outcome, Some(Ok(0))),
      "the round completed once its write could land, refusing nothing: {outcome:?}"
    );
    assert!(round.thread.is_none(), "its thread was joined");
  }
}
