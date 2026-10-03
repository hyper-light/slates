//! The `/dev/fuse` transport (§4.6 "Linux (own /dev/fuse driver)"; Phase 3 task 1b). The kernel
//! and the daemon exchange messages over the character device `/dev/fuse`: a `read` returns one
//! request (or a batch the driver splits), a `write` sends one reply. This module owns the
//! device descriptor and turns it into the request/reply stream the [`crate::dispatch`] serves.
//!
//! Linux only: `/dev/fuse` and the FUSE ABI are the Linux kernel's. The serve loop and the
//! device I/O run in the CI Linux lane against a real mount; this file compiles and cross-lints
//! everywhere. The blocking loop here is the fallback the design names; the io_uring command
//! path and the per-shard `FUSE_DEV_IOC_CLONE` channels are the driver's next pieces (owed), and
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
use crate::bridge::Bridge;
use crate::coherence::{Coherence, Delivered};
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
  /// point; several channels over one connection are made by cloning the descriptor
  /// (`FUSE_DEV_IOC_CLONE`, owed).
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
      Ok(n) => Ok(&self.buffer[..n]),
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

  /// Writes one kernel invalidation to the device as an unsolicited notification (§4.6
  /// `notify`). `expire_only` says the kernel negotiated `FUSE_HAS_EXPIRE_ONLY`, so a live-source
  /// entry is expired rather than dropped. The kernel answers `ENOENT` when it holds no such
  /// entry and `ENOTEMPTY` when a directory it would drop is in use — neither is a fault of the
  /// mount (the kernel has nothing stale, or will revalidate on its next use), so both are
  /// absorbed; any other refusal is the transport's.
  pub fn write_invalidation(
    &self,
    invalidation: &Invalidation,
    expire_only: bool,
  ) -> Result<(), ChannelError> {
    let mut out = [0u8; NOTIFY_BYTES];
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
    let Ok(n) = encoded else {
      // A name past the wire cap cannot be cached by the kernel either: nothing to drop.
      return Ok(());
    };
    match rustix::io::write(&self.device, &out[..n]) {
      Ok(_) | Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTEMPTY) => Ok(()),
      Err(e) => Err(ChannelError::Device {
        call: "notify",
        code: Some(e.raw_os_error()),
      }),
    }
  }
}

/// Format: the eventfd counter's width — one native-endian `u64` per read or write (`eventfd(2)`).
const EVENTFD_WORD: usize = size_of::<u64>();

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

  /// Takes the pending signals (the counter, in one read); a counter already at zero is nothing.
  fn drain(&self) {
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
    match rustix::event::poll(&mut fds[..polled], None) {
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
    }
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
      .get_mut(AT_UNIQUE..AT_UNIQUE + size_of::<u64>())?
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
/// unsolicited notification (a seam refusal keeps the cursor for the next wake; a device refusal is
/// the transport's).
fn deliver_owed(
  channel: &FuseChannel,
  bridge: &mut dyn Bridge,
  cx: &slates_bridge_core::OpContext,
  state: &mut ServeState,
) -> Result<Delivered, ChannelError> {
  let expire_only = state.expire_only;
  state
    .coherence
    .deliver(bridge, cx, &mut |invalidation: &Invalidation| {
      channel.write_invalidation(invalidation, expire_only)
    })
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
  let n = dispatch(request, bridge, cx, &mut state.reply);
  let reply = state.reply.get(..n).unwrap_or(&[]);
  let error = reply_error(reply);
  if n > 0 && channel.write_reply(reply)? == Sent::Unmatched {
    let parsed = Request::parse(request).ok();
    crate::bridge::reclaim_unreported(
      parsed.as_ref().and_then(|parsed| parsed.opcode),
      parsed.as_ref().map_or(0, |parsed| parsed.header.nodeid),
      reply,
      bridge,
      cx,
    );
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
    .get(ERROR_AT..ERROR_AT + size_of::<i32>())
    .and_then(|bytes| bytes.try_into().ok())
    .map_or(0, |bytes| i32::from_le_bytes(bytes).saturating_neg())
}

/// A request served by [`dispatch_ready`] whose reply waits for its owner: the opcode, the round delivered
/// before it, the reply's errno, and where the reply sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

impl Dispatched {
  /// The node the request named (the kernel's node id; the root is 1).
  pub fn nodeid(&self) -> u64 {
    self.nodeid
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
) -> Result<Turn, ChannelError> {
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
  let dispatched = delivered.map(|delivered| {
    let len = dispatch(&request, bridge, &cx, &mut state.reply);
    let error = state.reply.get(..len).map_or(0, reply_error);
    state.coherence.served_own_request(bridge, &cx, delivered);
    Dispatched {
      opcode,
      delivered,
      error,
      unique,
      nodeid,
      len,
    }
  });
  attachments.end(attachment);
  dispatched.map(Turn::Dispatched)
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

/// Gives back what a dispatched request's success reply granted — its lookup references and open handle —
/// when the reply will not reach the caller (a refused barrier replaces it, or the kernel no longer waits for
/// it), under `attachment` as the dispatch was. Call it while the reply is still in `state`: before
/// [`send_reply`] refuses, or after it answers [`Sent::Unmatched`] (AUD-29-85).
pub fn reclaim_dispatched(
  state: &ServeState,
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
    self.buffer[..len].to_vec()
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
}
