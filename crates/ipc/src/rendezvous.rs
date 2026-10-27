//! The rendezvous per OS (§4.7 "Rendezvous"; `research/low-latency-ipc-and-runtime.md` §2.2,
//! §4): how a client finds the daemon and receives its region, creating no filesystem entry
//! at any point, with the peer authenticated (§4.13).
//!
//! - Linux: the daemon listens on an abstract-namespace `AF_UNIX` socket named from the uid
//!   and the instance; on accept it reads `SO_PEERCRED`, refuses another uid, creates the
//!   client's region, and sends the region's descriptor and the completion eventfd with
//!   `SCM_RIGHTS` in one message that also carries the client id and the region's length;
//!   the socket stays open as the control channel (its close is how the daemon learns of a
//!   dead client).
//! - macOS: the daemon creates a bootstrap object with `shm_open` (per-user name, mode 0600,
//!   the kernel's uid check is the authentication) holding a table of claim slots; a client
//!   claims a free slot by compare-and-swap, writes its pid, and waits on the slot's state
//!   word; the daemon's control shard finds the claim, creates the region, writes its name
//!   and length into the slot and releases the state to ready; the client opens the region
//!   and marks the slot done so the daemon can reclaim it. Liveness of a client is its
//!   heartbeat slot (§4.7's "slot heartbeat lapse").
//! - Windows: the same bootstrap shape over a `Local\` section (the DACL is the
//!   authentication), with a named Event per claim slot. A section's name lives while any
//!   process maps it and cannot be unlinked, and every connected client's doorbell maps the
//!   object, so its owner (the anchor, or a daemon run alone) holds a token only it opens and
//!   takes over an object a dead owner's clients still map (A-176).
//!
//! A daemon adopts the rendezvous its anchor holds when it is handed an [`Adoption`] (across a
//! spawn in [`ENV_RENDEZVOUS`], or from an anchor in its own process), else makes its own. A
//! claim refused in an accept round ends that claim alone (A-176).
//!
//! Discovery: `SLATES_ENDPOINT` names the instance; else the well-known name `default`.

use crate::error::IpcError;
use crate::region::ClientRegion;

/// Format: the environment variable naming the daemon instance to connect to.
pub const ENV_ENDPOINT: &str = "SLATES_ENDPOINT";
/// Format: the well-known instance name.
pub const DEFAULT_INSTANCE: &str = "default";

/// The instance to connect to: the environment's, else the well-known one.
pub fn instance_from_env() -> String {
  std::env::var(ENV_ENDPOINT).unwrap_or_else(|_| DEFAULT_INSTANCE.to_owned())
}

/// What the daemon prepares for a client: its region on its shard and, on Linux, the shard's
/// kick descriptor the client writes to wake a parked shard (macOS and Windows ring the
/// daemon-wide doorbell word of the bootstrap object instead).
pub struct Prepared {
  /// The region.
  pub region: ClientRegion,
  /// The shard's kick descriptor (Linux: an eventfd), duplicated for the client.
  pub kick_fd: Option<i32>,
}

impl std::fmt::Debug for Prepared {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Prepared")
      .field("region", &self.region)
      .finish()
  }
}

/// A client the daemon accepted: its id, its uid, and the region the daemon keeps.
pub struct Accepted {
  /// The client id the daemon assigned.
  pub client_id: u32,
  /// The peer's uid.
  pub uid: u32,
  /// The peer's process id as the daemon's pid namespace sees it (the liveness probe's input where no socket closes).
  /// 0 on Linux when the peer's process is not visible from the daemon's namespace (`crate::peer`): its control socket
  /// still tells its liveness.
  pub pid: u32,
  /// The daemon's mapping of the region.
  pub region: ClientRegion,
  /// The control channel and completion signal, where the platform has one.
  pub control: Option<platform::Control>,
}

impl std::fmt::Debug for Accepted {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Accepted")
      .field("client_id", &self.client_id)
      .field("uid", &self.uid)
      .finish()
  }
}

#[cfg(unix)]
impl Accepted {
  /// A dup of the completion fd the platform handed the daemon for this client (Linux's eventfd today;
  /// macOS/Windows owed) — the original stays in the client slot for its liveness socket. The dup
  /// shares the eventfd object, so the daemon's `DaemonEnd` nudging it wakes the client's poll (D-19).
  pub fn completion_dup(&self) -> Option<std::os::fd::OwnedFd> {
    self
      .control
      .as_ref()
      .and_then(platform::Control::completion_dup)
  }
}

/// What one accept round served (A-176): every client it accepted, each answered with its region, and the failure of
/// the rendezvous itself that ended it early, if one did (the next round tries again with what is still pending).
pub struct Round {
  /// The clients accepted.
  pub accepted: Vec<Accepted>,
  /// The failure that ended the round before every pending claim was served.
  pub ended: Option<IpcError>,
}

/// What serving one pending claim came to.
pub enum Served {
  /// Accepted and answered with its region (boxed: on Linux the accepted client's control socket and
  /// completion signal make it several times the size of a refusal).
  Accepted(Box<Accepted>),
  /// Refused and told so where the claimant can still be reached; the claim is over, and the round goes on.
  Refused(IpcError),
}

/// What the daemon opens once: the listener (Linux) or the bootstrap object (macOS, Windows).
pub struct Listener {
  inner: platform::Listener,
  next_client: u32,
  /// Cross-uid connects refused (the audit counter of §4.13).
  refused: u64,
  /// Connects refused at the daemon's client bound, each answered typed (AC-2.6).
  capacity_refused: u64,
}

impl std::fmt::Debug for Listener {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Listener")
      .field("next_client", &self.next_client)
      .field("refused", &self.refused)
      .finish()
  }
}

/// Format: the environment variable a supervising anchor sets when it holds `instance`'s rendezvous (A-114): on Linux
/// the inherited listening socket's descriptor number; on macOS and Windows `held` (the bootstrap object is found by
/// the instance's name, which the anchor created and keeps).
pub const ENV_RENDEZVOUS: &str = "SLATES_ANCHOR_RENDEZVOUS";

/// The rendezvous a supervising anchor holds across daemon restarts (A-114), as it holds the NFS listener: a client
/// that connects while a daemon is restarting waits for the next daemon (Linux: in the listening socket's backlog;
/// macOS and Windows: its published claim in the bootstrap object, which the next daemon answers), where before the
/// restart unlinked or closed the endpoint and the connect waited out a claim wait to fail "no daemon".
pub struct HeldRendezvous {
  inner: platform::Held,
}

impl std::fmt::Debug for HeldRendezvous {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("HeldRendezvous")
  }
}

impl HeldRendezvous {
  /// Creates `instance`'s rendezvous for the anchor to hold for its life: an inheritable listening socket (Linux), or
  /// the bootstrap object with every slot free and nothing published (macOS, Windows), so a client finds no daemon
  /// until the first one stamps it.
  pub fn hold(instance: &str) -> Result<HeldRendezvous, IpcError> {
    Ok(HeldRendezvous {
      inner: platform::Held::hold(instance)?,
    })
  }

  /// How a daemon adopts this rendezvous: handed to a daemon the anchor spawns in [`ENV_RENDEZVOUS`], or directly to
  /// one an anchor in this process starts.
  pub fn adoption(&self) -> Adoption {
    Adoption {
      value: self.inner.env_value(),
    }
  }
}

/// How a daemon adopts the rendezvous its anchor holds (A-114): on Linux the held listening socket's descriptor number
/// (duplicated by the daemon, never owned), on macOS and Windows `held` (the bootstrap object is found by the instance's
/// name). It crosses a spawn in [`ENV_RENDEZVOUS`] and is handed over directly by an anchor in this process (tests,
/// embeddings), as a segment's handoff is, so nothing below the daemon's start reads the environment: an anchor in a
/// test process could hand one daemon its rendezvous only by setting a variable every daemon of that process reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adoption {
  value: String,
}

impl Adoption {
  /// The adoption a supervising anchor handed across the spawn, if [`ENV_RENDEZVOUS`] is set (none: no anchor holds
  /// this daemon's rendezvous). A value that is not text is refused rather than taken for no anchor.
  pub fn from_env() -> Result<Option<Adoption>, IpcError> {
    match std::env::var(ENV_RENDEZVOUS) {
      Ok(value) => Ok(Some(Adoption { value })),
      Err(std::env::VarError::NotPresent) => Ok(None),
      Err(std::env::VarError::NotUnicode(_)) => Err(IpcError::Layout {
        reason: "the anchor's rendezvous variable is not text",
      }),
    }
  }

  /// The value a spawning anchor sets in [`ENV_RENDEZVOUS`].
  pub fn env_value(&self) -> &str {
    &self.value
  }
}

impl Listener {
  /// Opens the daemon's end of the rendezvous for `instance`: the one its anchor holds when `adoption` names it
  /// (adopted, its pending claims answered), else a fresh one this daemon owns.
  pub fn open(instance: &str, adoption: Option<&Adoption>) -> Result<Listener, IpcError> {
    Ok(Listener {
      inner: platform::Listener::open(instance, adoption.map(Adoption::env_value))?,
      next_client: 1,
      refused: 0,
      capacity_refused: 0,
    })
  }

  /// Restore the daemon's durable allocation floor before accepting any connections (§4.9).
  /// Zero is the exhausted sentinel, never a fresh identity; an old session can still resume.
  pub fn resume_after(&mut self, highest: u32) {
    self.next_client = highest.checked_add(1).unwrap_or(0);
  }

  /// Serves every pending connection without blocking: for each, `make_region` builds the
  /// client's region (the daemon's derivation of its geometry and shard) for the id the
  /// daemon assigns (the peer's wanted id when `in_use` says it is free, else a fresh one),
  /// and the handoff is completed. A peer of another uid and a connect past the client bound
  /// are counted and answered; any other claim refused is answered to its claimant and handed
  /// to `refused` (a `HandoffLost` names the id to give back), and the round goes on: one
  /// claim's refusal never costs the round a client it accepted (A-176).
  pub fn accept_pending(
    &mut self,
    in_use: &dyn Fn(u32) -> bool,
    make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    refused: &mut dyn FnMut(IpcError),
  ) -> Round {
    let mut round = Round {
      accepted: Vec::new(),
      ended: None,
    };
    loop {
      let mut assign = |wanted: u32| -> u32 {
        let assigned = if wanted != 0 && !in_use(wanted) {
          wanted
        } else {
          self.next_client
        };
        // Once selected, a fresh identity is consumed even if the handoff later fails.
        if self.next_client != 0 && assigned >= self.next_client {
          self.next_client = assigned.checked_add(1).unwrap_or(0);
        }
        assigned
      };
      match self.inner.accept_one(&mut assign, make_region) {
        Ok(Some(Served::Accepted(accepted))) => round.accepted.push(*accepted),
        Ok(None) => break,
        Ok(Some(Served::Refused(IpcError::PeerRefused { .. }))) => {
          self.refused = self.refused.saturating_add(1)
        }
        // Answered typed to the client by the platform's `accept_one` (the bound in the refusal),
        // counted here, and the next pending connection is served: a full daemon keeps answering.
        Ok(Some(Served::Refused(IpcError::TooManyClients { .. }))) => {
          self.capacity_refused = self.capacity_refused.saturating_add(1)
        }
        Ok(Some(Served::Refused(refusal))) => refused(refusal),
        // The rendezvous itself failed: the round ends with what it accepted, and the next one tries again.
        Err(failure) => {
          round.ended = Some(failure);
          break;
        }
      }
    }
    round
  }

  /// Cross-uid connects refused so far.
  pub fn refused(&self) -> u64 {
    self.refused
  }

  /// Connects refused at the client bound so far, each answered typed.
  pub fn capacity_refused(&self) -> u64 {
    self.capacity_refused
  }

  /// The daemon-wide doorbell word clients ring when a shard is parked (macOS, Windows: the
  /// bootstrap object's word the doorbell thread waits on); none on Linux, where a client
  /// writes the shard's kick descriptor.
  pub fn doorbell(&self) -> Option<&std::sync::atomic::AtomicU32> {
    self.inner.doorbell()
  }

  /// A waiter on the daemon-wide doorbell for the doorbell thread (macOS, Windows): each call opens
  /// its own mapping of the bootstrap object (and its own handle on the doorbell Event on Windows).
  /// None on Linux.
  pub fn doorbell_waiter(&self) -> Result<Option<DoorbellWaiter>, IpcError> {
    Ok(
      self
        .inner
        .doorbell_waiter()?
        .map(|inner| DoorbellWaiter { inner }),
    )
  }

  /// The listening socket's descriptor (Linux), for the owning control shard's readiness wait.
  pub fn raw_fd(&self) -> Option<i32> {
    self.inner.raw_fd()
  }
}

/// What a client rings to wake a parked shard.
pub enum Doorbell {
  /// Write eight bytes to the shard's kick eventfd (Linux).
  #[cfg(target_os = "linux")]
  Eventfd(std::os::fd::OwnedFd),
  /// Bump the daemon-wide word in the bootstrap object and wake the daemon's doorbell thread
  /// (macOS, Windows).
  #[cfg(any(target_os = "macos", windows))]
  Word(platform::Bell),
}

impl std::fmt::Debug for Doorbell {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Doorbell")
  }
}

impl Doorbell {
  /// Rings.
  pub fn ring(&self) -> Result<(), IpcError> {
    match self {
      #[cfg(target_os = "linux")]
      Doorbell::Eventfd(fd) => rustix::io::write(fd, &1u64.to_ne_bytes())
        .map(|_| ())
        .map_err(|e| IpcError::OsRefused {
          call: "eventfd write",
          code: Some(e.raw_os_error()),
        }),
      #[cfg(any(target_os = "macos", windows))]
      Doorbell::Word(bell) => bell.ring(),
      #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
      _ => Err(IpcError::Unsupported {
        feature: "the doorbell",
      }),
    }
  }
}

/// What the daemon's doorbell thread waits on: on macOS and Windows its own mapping of the bootstrap
/// object's doorbell word and, on Windows, the named doorbell Event clients signal, because the word's
/// wake is process-local there (D-10). Linux has none: a client writes its shard's kick descriptor.
/// A second waiter from [`Listener::doorbell_waiter`] is the handle the daemon rings to stop the thread.
pub struct DoorbellWaiter {
  inner: platform::DoorbellWaiter,
}

impl std::fmt::Debug for DoorbellWaiter {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("DoorbellWaiter")
  }
}

impl DoorbellWaiter {
  /// The doorbell word's value now: what the thread compares each later value against, so a ring
  /// that lands between two waits shows as a change and is never lost.
  pub fn current(&self) -> Result<u32, IpcError> {
    self.inner.current()
  }

  /// Waits until the doorbell word differs from `seen`, or up to `timeout_ns`; the word's value then.
  pub fn wait(&self, seen: u32, timeout_ns: u64) -> Result<u32, IpcError> {
    self.inner.wait(seen, timeout_ns)
  }

  /// Rings the doorbell as a client does: bumps the word and wakes the waiting thread (the daemon
  /// rings it to stop its own thread).
  pub fn ring(&self) -> Result<(), IpcError> {
    self.inner.ring()
  }
}

/// Shape: how long a client waits for the daemon to answer a claim before reporting it unavailable
/// (nanoseconds): the control shard's loop is microseconds, so a second is a dead daemon.
pub const CLAIM_WAIT_NS: u64 = 1_000_000_000;

/// The client's side: connects to `instance` and returns its region, the doorbell for a parked
/// shard, and the platform's control channel where one exists.
/// A daemon no anchor holds the rendezvous for (an in-process daemon in a test) has no restart to wait out, so this
/// connect waits one claim wait; a client of an anchored instance passes its restart bound to [`connect_as`].
pub fn connect(instance: &str) -> Result<Connected, IpcError> {
  connect_as(instance, 0, CLAIM_WAIT_NS)
}

/// Connects asking for a client id it held before (a reconnect after the daemon restarted, so
/// its retries under the old request ids meet their completion records, §4.9); the daemon
/// honours the id when no live client holds it, else assigns a fresh one. The blocking facade over
/// [`begin_connect_as`]: the claim, then a wait for its answer of at most the claim wait (twice, when
/// the daemon took the claim to answer it). `restart_wait_ns` is how long a claim may wait while the daemon restarts
/// under an anchor that holds the rendezvous ([`begin_connect_as`]).
pub fn connect_as(
  instance: &str,
  wanted: u32,
  restart_wait_ns: u64,
) -> Result<Connected, IpcError> {
  begin_connect_as(instance, wanted, restart_wait_ns)?.wait()
}

/// Starts a connect without waiting (AUD-29-19, §4.7 "Rendezvous"): the claim is made and announced to
/// the daemon, and [`Claim::poll`] reads its answer whenever the caller's event loop looks. Nothing here
/// waits on the daemon, so an event loop that drives the claim is never held by a slow, stopped or dead
/// one.
///
/// A claim waits one claim wait for its answer, unless the rendezvous is held by a live anchor (A-114): then the daemon
/// may be restarting, and the claim waits up to `restart_wait_ns` (the caller's bound on a restart — a client passes
/// its reconnect deadline, the recovery budget plus a reply deadline), so a restart slower than a claim wait (a large
/// volume to recover, a busy machine) is waited out instead of reported as no daemon. On Linux a listening socket
/// exists only while its owner lives (a daemon answers at once; the anchor holds it across a restart), so the handoff
/// may always wait that long; on macOS and Windows the bootstrap header names the holding anchor, whose liveness is
/// checked.
pub fn begin_connect_as(
  instance: &str,
  wanted: u32,
  restart_wait_ns: u64,
) -> Result<Claim, IpcError> {
  Ok(Claim {
    inner: platform::begin(instance, wanted, restart_wait_ns.max(CLAIM_WAIT_NS))?,
  })
}

/// A connect in flight: the claim made, its answer not yet read. Dropped unanswered, the claim is given
/// back (Linux: the socket closes, which the daemon reads as a client gone).
pub struct Claim {
  inner: platform::Claim,
}

impl std::fmt::Debug for Claim {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Claim")
  }
}

impl Claim {
  /// Reads the daemon's answer without waiting: `Some` once connected, `None` while it is still due,
  /// a typed refusal when the daemon refused the claim or did not answer it within the claim wait.
  /// Once it has returned `Some` or an error, the claim is spent and a further poll is refused.
  pub fn poll(&mut self) -> Result<Option<Connected>, IpcError> {
    self.inner.poll()
  }

  /// Waits for the answer (the synchronous facade): the platform's own wait between polls, bounded by
  /// the claim wait.
  pub fn wait(self) -> Result<Connected, IpcError> {
    self.inner.wait()
  }
}

/// Format: the bound a refusal carries when the region was refused for a reason other than the client
/// bound — no bound applies; the client reports the daemon unavailable with the reason.
const NO_BOUND: usize = 0;

/// The error a refusal with `limit` decodes to: the client bound when one was given, else a daemon that
/// refused the claim for a reason of its own (counted and logged on its side).
fn refusal_of(limit: usize, instance: &str) -> IpcError {
  if limit == NO_BOUND {
    IpcError::DaemonUnavailable {
      endpoint: instance.to_owned(),
      why: "the daemon refused the claim (its region could not be created; see its log)",
    }
  } else {
    IpcError::TooManyClients { limit }
  }
}

/// What a client holds after the rendezvous.
pub struct Connected {
  /// The region.
  pub region: ClientRegion,
  /// The doorbell.
  pub doorbell: Doorbell,
  /// How the client tells a dead daemon from a slow one.
  pub liveness: Liveness,
  /// The control channel, where the platform has one.
  pub control: Option<platform::ClientControl>,
}

/// How a client tells a dead daemon from a slow one (§4.7 "Failure matrix": a stalled reply
/// and "the control channel reset" mean the daemon died and the client reconnects; a stalled
/// reply alone means it is slow). Linux: the control socket, whose peer end the kernel closes
/// when the daemon dies. macOS and Windows: the bootstrap object's start stamp, which a
/// restarted daemon rewrites and a dead one leaves unreachable.
pub struct Liveness {
  inner: platform::Liveness,
}

impl std::fmt::Debug for Liveness {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("Liveness")
  }
}

/// A direct map the daemon delivered to a same-kernel client (§4.6, A-171): the descriptor of its copy of one file,
/// sealed against every writer but the daemon, and where the file sits within it (offset zero). The client `mmap`s
/// `block` read-only, all `block_len` bytes; past the file's end the copy reads zero.
#[cfg(unix)]
pub struct MappingDelivery {
  /// The arena block's backing object, to map read-only.
  pub block: std::os::fd::OwnedFd,
  /// The request this delivery answers (the command-ring map request's id).
  pub request_id: u64,
  /// The daemon-minted mapping id, named by the later release message.
  pub mapping_id: u64,
  /// The block's byte offset within `block`.
  pub region_off: u64,
  /// The file's authorized length; the bytes from here to the page-span end read zero.
  pub file_len: u64,
  /// The block's length (its buddy size); the map covers only the file's page span within it.
  pub block_len: u64,
}

/// The other end of a Shards fs stream the daemon delivered to the client that attached it (§4.6, A-161): the
/// attachment's capability, which the client hands to the VMM. The daemon serves that one attachment on its own end.
#[cfg(unix)]
pub struct StreamDelivery {
  /// The stream's other end.
  pub stream: std::os::fd::OwnedFd,
  /// The request this delivery answers (the attach's id).
  pub request_id: u64,
  /// The attachment the daemon serves on the stream.
  pub attachment: u64,
}

/// A descriptor the daemon delivered over the control socket, with what it is for: a content map (A-135) or a
/// stream end (A-161). Each names the request it answers, so a client takes only its own.
#[cfg(unix)]
pub enum ControlDelivery {
  /// A read-only content map.
  Mapping(MappingDelivery),
  /// A Shards fs stream's other end.
  Stream(StreamDelivery),
}

#[cfg(unix)]
impl ControlDelivery {
  /// The request the delivery answers.
  pub fn request_id(&self) -> u64 {
    match self {
      Self::Mapping(mapping) => mapping.request_id,
      Self::Stream(stream) => stream.request_id,
    }
  }
}

impl Liveness {
  /// Whether the daemon the client connected to is gone (dead, or restarted since). A cold
  /// path: asked only after a reply has stalled past the client's deadline.
  pub fn daemon_gone(&self) -> bool {
    self.inner.daemon_gone()
  }

  /// Receives a descriptor the daemon sent over the control socket (§4.6: a content map, A-135; a stream end,
  /// A-161), or `None` when none is waiting. Non-blocking. A platform without a control socket (macOS, Windows)
  /// never delivers one: a same-kernel client there reads through the copy path, and a stream end comes by the
  /// platform's own route.
  #[cfg(unix)]
  pub fn recv_delivery(&self) -> Result<Option<ControlDelivery>, IpcError> {
    self.inner.recv_delivery()
  }
}

impl std::fmt::Debug for Connected {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Connected")
      .field("region", &self.region)
      .finish()
  }
}

#[cfg(unix)]
impl Connected {
  /// Takes the completion fd the platform handed the client (Linux's eventfd today; macOS/Windows
  /// owed), consuming the control channel. The client's `ClientEnd` polls it for an async SDK (D-19).
  pub fn take_completion(&mut self) -> Option<std::os::fd::OwnedFd> {
    self
      .control
      .take()
      .and_then(platform::ClientControl::into_completion)
  }
}

/// The rendezvous name for an instance, per user.
fn rendezvous_name(instance: &str) -> String {
  let clean: String = instance
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
    .collect();
  format!("slates-rv-{clean}")
}

#[cfg(target_os = "linux")]
pub mod platform {
  //! Linux: the abstract-namespace socket, `SO_PEERCRED`, `SCM_RIGHTS`.

  use std::io::{IoSlice, IoSliceMut};
  use std::os::fd::{AsFd, OwnedFd};

  use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
  };
  use slates_mem::Handoff;

  use super::{Accepted, Connected, Doorbell, Prepared, Served, rendezvous_name};
  use crate::error::IpcError;
  use crate::region::ClientRegion;

  /// Refuses a descriptor that would authorize a peer more than the hand-off does (§4.6; A-135, A-171).
  ///
  /// What may cross is one file's copy, all of it authorized, sealed against writing and resizing: the
  /// object the direct map makes for each file it hands (`slates_mem::PeerObject`). Two questions, both
  /// measured on the descriptor itself rather than taken from the caller:
  ///
  /// 1. **Does it reach past what is authorized?** `fstat` gives the object's whole length, which is what
  ///    the holder of the descriptor may map. The authorized bytes start at `region_off` and run
  ///    `authorized` bytes, so the descriptor passes only when they are the whole object: offset zero and
  ///    equal lengths. One file's copy is that by construction; the shared content arena holding every
  ///    volume's blocks never is, and it is what the 2026-10-18 escape handed over.
  /// 2. **Could the peer still write or resize it?** `F_GET_SEALS`. Writing must be sealed
  ///    (`F_SEAL_WRITE`, or `F_SEAL_FUTURE_WRITE` where the owner keeps its own earlier map) and both
  ///    `F_SEAL_SHRINK` and `F_SEAL_GROW` must be set, or a peer shrinks the object under the daemon's
  ///    feet. A descriptor duplicated read-only does **not** pass and must not: measured, a same-uid
  ///    peer re-opens such a duplicate `O_RDWR` through `/proc/self/fd` and maps it writable.
  ///
  /// An object with no seal support at all answers `F_GET_SEALS` with `F_SEAL_SEAL` absent and no write
  /// seal, so it is refused by the same arm; a file that is not a memfd refuses the call, and is
  /// refused here too.
  fn confined_for_a_peer(
    descriptor: std::os::fd::BorrowedFd<'_>,
    region_off: u64,
    authorized: u64,
  ) -> Result<(), IpcError> {
    let object_bytes = match rustix::fs::fstat(descriptor) {
      Ok(stat) => u64::try_from(stat.st_size).unwrap_or(u64::MAX),
      Err(e) => return Err(refused("fstat mapping", e)),
    };
    let seals = rustix::fs::fcntl_get_seals(descriptor).unwrap_or(rustix::fs::SealFlags::empty());
    let write_sealed = seals.contains(rustix::fs::SealFlags::WRITE)
      || seals.contains(rustix::fs::SealFlags::FUTURE_WRITE);
    let size_sealed =
      seals.contains(rustix::fs::SealFlags::SHRINK) && seals.contains(rustix::fs::SealFlags::GROW);
    let writable = !(write_sealed && size_sealed);
    if region_off != 0 || object_bytes != authorized || writable {
      return Err(IpcError::UnconfinedDescriptor {
        object_bytes,
        authorized_bytes: authorized,
        writable,
      });
    }
    Ok(())
  }

  /// No doorbell waiter on this platform: an uninhabited type, so no value of it exists.
  pub enum DoorbellWaiter {}

  impl DoorbellWaiter {
    pub(super) fn current(&self) -> Result<u32, IpcError> {
      match *self {}
    }

    pub(super) fn wait(&self, _seen: u32, _timeout_ns: u64) -> Result<u32, IpcError> {
      match *self {}
    }

    pub(super) fn ring(&self) -> Result<(), IpcError> {
      match *self {}
    }
  }

  /// Format: the handoff message: client id (4), region length (8).
  const HANDOFF_BYTES: usize = 12;
  /// Format: the client's hello: the id it wants (4), zero for a fresh one.
  const HELLO_BYTES: usize = 4;
  /// Format: the client id's offset in the handoff message.
  const HANDOFF_AT_CLIENT: usize = 0;
  /// Format: the region length's offset in the handoff message.
  const HANDOFF_AT_LEN: usize = 4;
  /// Format: descriptors in the handoff: the region, the completion eventfd, the shard's kick.
  const HANDOFF_FDS: usize = 3;
  /// Format: the client id a refusal handoff names — never assigned (a hello of zero asks for a fresh
  /// id), so a handoff naming it carries no region: its length word is the client bound the connect
  /// was refused at (`IpcError::TooManyClients`), or [`super::NO_BOUND`] for a region refused for another
  /// reason.
  const REFUSED_CLIENT: u32 = 0;

  /// Format: a delivery's kind word, its first eight bytes, so deliveries of every kind share the socket and none is
  /// taken for another's: a content map (§4.6, A-135) or a stream end (A-161).
  const KIND_BYTES: usize = 8;
  /// Format: the kind of a content-map delivery.
  const KIND_MAPPING: u64 = 1;
  /// Format: the kind of a stream-end delivery.
  const KIND_STREAM: u64 = 2;
  /// Format: a mapping delivery's body after its kind (§4.6, A-135): request id (8), mapping id (8), the block's
  /// offset in its backing object (8), the file's authorized length (8), the block length (8).
  const MAPPING_MSG_BYTES: usize = 40;
  /// Format: the request id's offset in a delivery's body, the same for every kind.
  const AT_REQUEST: usize = 0;
  /// Format: the mapping id's offset.
  const MAPPING_AT_ID: usize = 8;
  /// Format: the block offset's offset.
  const MAPPING_AT_OFF: usize = 16;
  /// Format: the file length's offset.
  const MAPPING_AT_FILE_LEN: usize = 24;
  /// Format: the block length's offset.
  const MAPPING_AT_BLOCK_LEN: usize = 32;
  /// Format: a stream delivery's body after its kind (A-161): request id (8), attachment id (8).
  const STREAM_MSG_BYTES: usize = 16;
  /// Format: the attachment id's offset in a stream delivery.
  const STREAM_AT_ATTACHMENT: usize = 8;
  /// Derived: the longest delivery, a kind word and a mapping's body; every receive reads into this much.
  const DELIVERY_MAX_BYTES: usize = KIND_BYTES + MAPPING_MSG_BYTES;
  /// Format: the one descriptor every delivery carries: an arena block's backing object, or a stream end.
  const DELIVERY_FDS: usize = 1;

  /// Sends the typed refusal handoff to `peer`: client [`REFUSED_CLIENT`], `limit` in the length word,
  /// no descriptors.
  fn send_refusal(peer: &OwnedFd, limit: usize) -> Result<(), IpcError> {
    let mut body = [0u8; HANDOFF_BYTES];
    body[HANDOFF_AT_CLIENT..HANDOFF_AT_LEN].copy_from_slice(&REFUSED_CLIENT.to_le_bytes());
    body[HANDOFF_AT_LEN..].copy_from_slice(&u64::try_from(limit).unwrap_or(u64::MAX).to_le_bytes());
    rustix::net::send(peer, &body, SendFlags::empty()).map_err(|e| refused("send", e))?;
    Ok(())
  }
  /// Shape: the listen backlog (connections pending accept); the control shard drains them
  /// every loop, so the backlog only covers one loop of arrivals.
  const BACKLOG: i32 = 64;

  fn refused(call: &'static str, e: rustix::io::Errno) -> IpcError {
    IpcError::OsRefused {
      call,
      code: Some(e.raw_os_error()),
    }
  }

  /// The control channel to one client: the socket (its close means the client died) and the
  /// completion eventfd the daemon signals for a parked SDK event loop.
  pub struct Control {
    /// The socket.
    pub socket: OwnedFd,
    /// The completion eventfd.
    pub completion: OwnedFd,
  }

  /// The client's control channel: the completion eventfd an SDK event loop polls.
  pub struct ClientControl {
    /// The completion eventfd.
    pub completion: OwnedFd,
  }

  impl Control {
    /// A dup of the completion eventfd, for the daemon's end to nudge — the original stays here for the
    /// liveness socket's owner. Dup'ing shares the eventfd object, so a nudge on the dup increments the
    /// counter the client's own dup reads.
    pub fn completion_dup(&self) -> Option<OwnedFd> {
      rustix::io::dup(&self.completion).ok()
    }

    /// Sends a direct-map delivery to the client over the control socket (§4.6, A-171): the descriptor of
    /// the daemon's copy of one file by `SCM_RIGHTS` with the mapping metadata. The client `mmap`s `block`
    /// whole, from `region_off` (zero). One `sendmsg`, so the descriptor arrives with the body; the command
    /// ring cannot carry a descriptor, this socket can.
    ///
    /// **The descriptor is checked against what it authorizes before it is sent**, and that check is
    /// here — at the one place a descriptor leaves this process for a peer — so no caller can skip it.
    /// A descriptor carries its whole object, read and write; `region_off`, `file_len` and `block_len`
    /// scope the *request* and bind the peer to nothing. The peer is not trusted merely for sharing
    /// this uid: a §4.13 consumer is an untrusted process of exactly this uid holding exactly this
    /// channel. So the object must be all authorized, one file's copy and nothing more
    /// ([`IpcError::UnconfinedDescriptor`] otherwise), and it must be sealed against writing and resizing.
    ///
    /// The seals are the measured mechanism rather than a chosen one (2026-10-18, kernel 7.2.9): a
    /// read-only duplicate of a memfd is **no** defence — the peer re-opens it `O_RDWR` through
    /// `/proc/self/fd` and maps it writable — whereas `F_SEAL_FUTURE_WRITE` with `F_SEAL_SHRINK` and
    /// `F_SEAL_GROW`, added after the owner has taken its own writable map, refuses the peer every
    /// writable map, every `pwrite` and every `ftruncate` while the owner keeps writing through the map
    /// it already holds.
    pub fn send_mapping(
      &self,
      block: std::os::fd::BorrowedFd<'_>,
      request_id: u64,
      mapping_id: u64,
      region_off: u64,
      file_len: u64,
      block_len: u64,
    ) -> Result<(), IpcError> {
      confined_for_a_peer(block, region_off, block_len)?;
      let mut body = [0u8; KIND_BYTES + MAPPING_MSG_BYTES];
      put_words(
        &mut body,
        &[
          (0, KIND_MAPPING),
          (KIND_BYTES + AT_REQUEST, request_id),
          (KIND_BYTES + MAPPING_AT_ID, mapping_id),
          (KIND_BYTES + MAPPING_AT_OFF, region_off),
          (KIND_BYTES + MAPPING_AT_FILE_LEN, file_len),
          (KIND_BYTES + MAPPING_AT_BLOCK_LEN, block_len),
        ],
      );
      self.send_delivery(&body, block, "sendmsg mapping")
    }

    /// Sends the client that attached a Shards fs stream the stream's other end (§4.6, A-161), by `SCM_RIGHTS` with
    /// the request it answers and the attachment it serves. Sent before the attach's reply on the ring, so the
    /// client finds it waiting when it reads the reply.
    ///
    /// **No confinement check, by construction.** A memory object's descriptor carries the whole object
    /// ([`Self::send_mapping`]); a stream end carries only the conversation the daemon answers on the other end, and
    /// the daemon answers one attachment there, with the rights and the directory its record holds. Whoever holds
    /// the end reaches that attachment and nothing else, which is what the attach granted.
    pub fn send_stream(
      &self,
      stream: std::os::fd::BorrowedFd<'_>,
      request_id: u64,
      attachment: u64,
    ) -> Result<(), IpcError> {
      let mut body = [0u8; KIND_BYTES + STREAM_MSG_BYTES];
      put_words(
        &mut body,
        &[
          (0, KIND_STREAM),
          (KIND_BYTES + AT_REQUEST, request_id),
          (KIND_BYTES + STREAM_AT_ATTACHMENT, attachment),
        ],
      );
      self.send_delivery(&body, stream, "sendmsg stream")
    }

    /// One delivery, `body` with `descriptor` by `SCM_RIGHTS`, in one `sendmsg`, so the descriptor arrives with its
    /// body and a receive never runs past it into the next delivery.
    fn send_delivery(
      &self,
      body: &[u8],
      descriptor: std::os::fd::BorrowedFd<'_>,
      call: &'static str,
    ) -> Result<(), IpcError> {
      let fds = [descriptor];
      let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(DELIVERY_FDS))];
      let mut control = SendAncillaryBuffer::new(&mut space);
      if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(IpcError::Layout {
          reason: "a delivery's descriptor did not fit the ancillary buffer",
        });
      }
      rustix::net::sendmsg(
        &self.socket,
        &[IoSlice::new(body)],
        &mut control,
        SendFlags::empty(),
      )
      .map_err(|e| refused(call, e))?;
      Ok(())
    }
  }

  /// Writes each `(offset, word)` little-endian into `body`.
  fn put_words(body: &mut [u8], words: &[(usize, u64)]) {
    for &(at, word) in words {
      let bytes = word.to_le_bytes();
      if let Some(field) = body.get_mut(at..at.saturating_add(bytes.len())) {
        field.copy_from_slice(&bytes);
      }
    }
  }

  /// The little-endian word at `at` in `body`; zero past its end (a length checked by the caller first).
  fn word_at(body: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; size_of::<u64>()];
    if let Some(field) = body.get(at..at.saturating_add(size_of::<u64>())) {
      raw.copy_from_slice(field);
    }
    u64::from_le_bytes(raw)
  }

  impl ClientControl {
    /// The completion eventfd an async SDK event loop polls for reply-readiness.
    pub fn into_completion(self) -> Option<OwnedFd> {
      Some(self.completion)
    }
  }

  /// The client's liveness check: the control socket; its peer end closes with the daemon.
  pub struct Liveness {
    socket: OwnedFd,
  }

  impl Liveness {
    pub(super) fn daemon_gone(&self) -> bool {
      let mut probe = [0u8; 1];
      match rustix::net::recv(
        &self.socket,
        &mut probe,
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
      ) {
        // End of stream: the peer closed.
        Ok((0, _)) => true,
        // Bytes waiting, or nothing yet: the peer is there.
        Ok(_) | Err(rustix::io::Errno::AGAIN) => false,
        // Reset, or an unusable descriptor: the channel is gone either way.
        Err(_) => true,
      }
    }

    /// Receives a delivery from the daemon over the control socket (§4.6: a content map, A-135; a stream end,
    /// A-161), or `None` when none is waiting. Non-blocking; the descriptor arrives by `SCM_RIGHTS`
    /// (close-on-exec), what it is for in the body after its kind. A body of the wrong length for its kind, an
    /// unknown kind, or a descriptor count other than one is refused `Layout`, and every descriptor that came with
    /// it is closed: nothing a refused delivery carried stays open in the client.
    pub(super) fn recv_delivery(&self) -> Result<Option<super::ControlDelivery>, IpcError> {
      let mut body = [0u8; DELIVERY_MAX_BYTES];
      let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(DELIVERY_FDS))];
      let mut control = RecvAncillaryBuffer::new(&mut space);
      let received = match rustix::net::recvmsg(
        &self.socket,
        &mut [IoSliceMut::new(&mut body)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
      ) {
        Ok(received) => received.bytes,
        // Nothing waiting yet.
        Err(rustix::io::Errno::AGAIN) => return Ok(None),
        Err(e) => return Err(refused("recvmsg delivery", e)),
      };
      let mut descriptors: Vec<OwnedFd> = Vec::new();
      for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(iter) = message {
          descriptors.extend(iter);
        }
      }
      // The peer closed: no delivery (the daemon is gone; `daemon_gone` reports it).
      if received == 0 {
        return Ok(None);
      }
      let kind = word_at(&body, 0);
      let expected = match kind {
        KIND_MAPPING => KIND_BYTES + MAPPING_MSG_BYTES,
        KIND_STREAM => KIND_BYTES + STREAM_MSG_BYTES,
        _ => {
          return Err(IpcError::Layout {
            reason: "a delivery of an unknown kind",
          });
        }
      };
      if received != expected {
        return Err(IpcError::Layout {
          reason: "a delivery was the wrong length for its kind",
        });
      }
      if descriptors.len() != DELIVERY_FDS {
        return Err(IpcError::Layout {
          reason: "a delivery carried the wrong number of descriptors",
        });
      }
      let descriptor = descriptors.pop().ok_or(IpcError::Layout {
        reason: "a delivery carried no descriptor",
      })?;
      let field = |at: usize| word_at(&body, KIND_BYTES.saturating_add(at));
      Ok(Some(if kind == KIND_STREAM {
        super::ControlDelivery::Stream(super::StreamDelivery {
          stream: descriptor,
          request_id: field(AT_REQUEST),
          attachment: field(STREAM_AT_ATTACHMENT),
        })
      } else {
        super::ControlDelivery::Mapping(super::MappingDelivery {
          block: descriptor,
          request_id: field(AT_REQUEST),
          mapping_id: field(MAPPING_AT_ID),
          region_off: field(MAPPING_AT_OFF),
          file_len: field(MAPPING_AT_FILE_LEN),
          block_len: field(MAPPING_AT_BLOCK_LEN),
        })
      }))
    }
  }

  pub(super) struct Listener {
    socket: OwnedFd,
    uid: u32,
  }

  fn address(instance: &str) -> Result<SocketAddrUnix, IpcError> {
    let uid = rustix::process::getuid().as_raw();
    let name = format!("{}/{uid}", rendezvous_name(instance));
    SocketAddrUnix::new_abstract_name(name.as_bytes()).map_err(|e| refused("abstract name", e))
  }

  /// A listening socket bound at `instance`'s address, with `flags` (close-on-exec for the daemon's own; none for the
  /// anchor's, which every daemon it spawns inherits).
  fn listening(instance: &str, flags: SocketFlags) -> Result<OwnedFd, IpcError> {
    let socket = rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      flags | SocketFlags::NONBLOCK,
      None,
    )
    .map_err(|e| refused("socket", e))?;
    rustix::net::bind(&socket, &address(instance)?).map_err(|e| refused("bind", e))?;
    rustix::net::listen(&socket, BACKLOG).map_err(|e| refused("listen", e))?;
    Ok(socket)
  }

  /// The anchor's listening socket (A-114): inheritable, so the kernel keeps the address and queues every connect while
  /// no daemon runs.
  pub(super) struct Held {
    socket: OwnedFd,
  }

  impl Held {
    pub(super) fn hold(instance: &str) -> Result<Held, IpcError> {
      Ok(Held {
        socket: listening(instance, SocketFlags::empty())?,
      })
    }

    pub(super) fn env_value(&self) -> String {
      use std::os::fd::AsRawFd;
      self.socket.as_raw_fd().to_string()
    }
  }

  /// Adopts the listening socket an anchor handed over at descriptor `value`: duplicated close-on-exec (the number stays
  /// the anchor's, inherited across the spawn or held by an anchor in this process, as a segment's handoff does), made
  /// non-blocking, and refused unless it listens at `instance`'s own address — a number that names anything else is not
  /// this instance's rendezvous.
  fn adopt(instance: &str, value: &str) -> Result<OwnedFd, IpcError> {
    use std::os::fd::BorrowedFd;
    let raw = value
      .parse::<std::os::fd::RawFd>()
      .ok()
      .filter(|raw| *raw >= 0)
      .ok_or(IpcError::Layout {
        reason: "the anchor's rendezvous descriptor is not a number",
      })?;
    // SAFETY: the anchor made this listening socket and handed over its number, across the spawn (inherited, open for
    // the process's life) or within this process (held by the anchor for its own life, which spans every daemon it
    // starts); nothing here closes the number, and the borrow lasts only for the duplication.
    let inherited = unsafe { BorrowedFd::borrow_raw(raw) };
    let socket = rustix::io::fcntl_dupfd_cloexec(inherited, 0).map_err(|e| refused("dup", e))?;
    rustix::io::ioctl_fionbio(&socket, true).map_err(|e| refused("ioctl(FIONBIO)", e))?;
    let bound = rustix::net::getsockname(&socket).map_err(|e| refused("getsockname", e))?;
    let expected = rustix::net::SocketAddrAny::from(address(instance)?);
    if bound != expected {
      return Err(IpcError::Layout {
        reason: "the anchor's rendezvous descriptor is not bound at this instance's address",
      });
    }
    Ok(socket)
  }

  impl Listener {
    pub(super) fn open(instance: &str, adoption: Option<&str>) -> Result<Listener, IpcError> {
      let socket = match adoption {
        Some(value) => adopt(instance, value)?,
        None => listening(instance, SocketFlags::CLOEXEC)?,
      };
      Ok(Listener {
        socket,
        uid: rustix::process::getuid().as_raw(),
      })
    }

    pub(super) fn doorbell(&self) -> Option<&std::sync::atomic::AtomicU32> {
      None
    }

    pub(super) fn doorbell_waiter(&self) -> Result<Option<DoorbellWaiter>, IpcError> {
      Ok(None)
    }

    pub(super) fn raw_fd(&self) -> Option<i32> {
      use std::os::fd::AsRawFd;
      Some(self.socket.as_raw_fd())
    }

    /// Accepts the next pending connection and serves it. A failure of the accept itself is the
    /// rendezvous's (the round ends); anything after it is that one connection's refusal (A-176).
    pub(super) fn accept_one(
      &mut self,
      assign: &mut dyn FnMut(u32) -> u32,
      make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Option<Served>, IpcError> {
      let peer = match rustix::net::accept_with(&self.socket, SocketFlags::CLOEXEC) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::AGAIN) => return Ok(None),
        Err(e) => return Err(refused("accept", e)),
      };
      Ok(Some(match self.serve(peer, assign, make_region) {
        Ok(accepted) => Served::Accepted(Box::new(accepted)),
        Err(refusal) => Served::Refused(refusal),
      }))
    }

    /// Serves one accepted connection: its credentials, its hello, its region and the handoff. Every
    /// failure here is this connection's alone, and the peer is told what it can be told.
    fn serve(
      &mut self,
      peer: OwnedFd,
      assign: &mut dyn FnMut(u32) -> u32,
      make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Accepted, IpcError> {
      let cred = crate::peer::peer_credentials(peer.as_fd())?;
      let uid = cred.uid;
      if uid != self.uid {
        return Err(IpcError::PeerRefused { uid });
      }
      let mut hello = [0u8; HELLO_BYTES];
      let got =
        rustix::net::recv(&peer, &mut hello, RecvFlags::empty()).map_err(|e| refused("recv", e))?;
      let wanted = if got.0 == HELLO_BYTES {
        u32::from_le_bytes(hello)
      } else {
        0
      };
      let client_id = assign(wanted);
      let Prepared { region, kick_fd } = match make_region(client_id) {
        Ok(prepared) => prepared,
        // The bound is reached: the client is told so, typed — a handoff naming client `REFUSED_CLIENT`
        // with the bound in the length word and no descriptors — where before the socket was dropped
        // and the client read a short handoff it could not interpret (2026-09-14).
        Err(IpcError::TooManyClients { limit }) => {
          send_refusal(&peer, limit)?;
          return Err(IpcError::TooManyClients { limit });
        }
        // Any other refusal of the region (one that could not be created): the client is told there is
        // no region for it (a refusal with no bound), never left to read a closed socket.
        Err(e) => {
          send_refusal(&peer, super::NO_BOUND)?;
          return Err(e);
        }
      };
      // From here the client holds an id the daemon reserved: a failure is reported with that id so
      // the daemon gives it back (`IpcError::HandoffLost`), never a reservation held for a client that
      // was never seated.
      let lost = |cause: IpcError| IpcError::HandoffLost {
        client_id,
        cause: Box::new(cause),
      };
      let (handoff, len) = region.handoff().map_err(lost)?;
      let Handoff::Descriptor(raw) = handoff else {
        return Err(lost(IpcError::Layout {
          reason: "a Linux region hands off a descriptor",
        }));
      };
      let kick = match kick_fd {
        Some(fd) => rustix::io::dup(
          // SAFETY: the caller's control shard is completing this handoff. The runtime
          // keeps every shard's kick descriptor until all its shard threads have joined,
          // so the eventfd stays open throughout this duplicate.
          unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
        )
        .map_err(|e| lost(refused("dup", e)))?,
        None => rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)
          .map_err(|e| lost(refused("eventfd", e)))?,
      };
      // SAFETY: the number is the duplicate `handoff` created for a child to inherit; this
      // process owns it and closes it after the send.
      let region_fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
      let completion = rustix::event::eventfd(
        0,
        rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
      )
      .map_err(|e| lost(refused("eventfd", e)))?;
      let mut body = [0u8; HANDOFF_BYTES];
      body[HANDOFF_AT_CLIENT..HANDOFF_AT_LEN].copy_from_slice(&client_id.to_le_bytes());
      body[HANDOFF_AT_LEN..].copy_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
      let fds = [region_fd.as_fd(), completion.as_fd(), kick.as_fd()];
      let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HANDOFF_FDS))];
      let mut control = SendAncillaryBuffer::new(&mut space);
      control.push(SendAncillaryMessage::ScmRights(&fds));
      rustix::net::sendmsg(
        &peer,
        &[IoSlice::new(&body)],
        &mut control,
        SendFlags::empty(),
      )
      .map_err(|e| lost(refused("sendmsg", e)))?;
      Ok(Accepted {
        client_id,
        uid,
        pid: cred.pid,
        region,
        control: Some(Control {
          socket: peer,
          completion,
        }),
      })
    }
  }

  /// A claim in flight (AUD-29-19): the connected, non-blocking socket with the hello sent, the
  /// handoff not yet received.
  pub struct Claim {
    instance: String,
    /// The socket; taken once the claim is answered or refused.
    socket: Option<OwnedFd>,
    started: std::time::Instant,
    /// How long the handoff may take: the caller's restart bound, since a listening socket means a live daemon or the
    /// anchor holding the rendezvous across a restart (`super::begin_connect_as`).
    wait_ns: u64,
  }

  /// Connects a non-blocking socket to the daemon's rendezvous and sends the hello; nothing waits (a
  /// stream connect over `AF_UNIX` completes at once or is refused when the daemon's backlog is full).
  pub(super) fn begin(instance: &str, wanted: u32, wait_ns: u64) -> Result<Claim, IpcError> {
    let socket = rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
      None,
    )
    .map_err(|e| refused("socket", e))?;
    rustix::net::connect(&socket, &address(instance)?).map_err(|errno| {
      IpcError::DaemonUnavailable {
        endpoint: instance.to_owned(),
        why: if errno == rustix::io::Errno::AGAIN {
          "the rendezvous backlog is full (the daemon is not accepting)"
        } else {
          "the rendezvous socket refused the connection (no daemon listening)"
        },
      }
    })?;
    // A fresh socket's send buffer takes the four-byte hello whole. A daemon that died between the connect
    // and the hello is unavailable (retried), as at the handoff's receive.
    rustix::net::send(&socket, &wanted.to_le_bytes(), SendFlags::empty()).map_err(|errno| {
      match errno {
        rustix::io::Errno::CONNRESET
        | rustix::io::Errno::CONNREFUSED
        | rustix::io::Errno::PIPE
        | rustix::io::Errno::NOTCONN => IpcError::DaemonUnavailable {
          endpoint: instance.to_owned(),
          why: "the daemon closed the rendezvous before the claim was sent",
        },
        other => refused("send", other),
      }
    })?;
    Ok(Claim {
      instance: instance.to_owned(),
      socket: Some(socket),
      started: std::time::Instant::now(),
      wait_ns,
    })
  }

  impl Claim {
    /// The handoff, without waiting: received and decoded when it has come; `None` while it is due
    /// within the claim wait; unavailable past it, or when the daemon closed the socket unanswered.
    pub(super) fn poll(&mut self) -> Result<Option<Connected>, IpcError> {
      let Some(socket) = self.socket.as_ref() else {
        return Err(IpcError::DaemonUnavailable {
          endpoint: self.instance.clone(),
          why: "the claim was already answered or refused",
        });
      };
      let mut body = [0u8; HANDOFF_BYTES];
      let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HANDOFF_FDS))];
      let mut control = RecvAncillaryBuffer::new(&mut space);
      match rustix::net::recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut body)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
      ) {
        Err(rustix::io::Errno::AGAIN) => {
          let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
          if elapsed < self.wait_ns {
            return Ok(None);
          }
          self.socket = None;
          Err(IpcError::DaemonUnavailable {
            endpoint: self.instance.clone(),
            why: "the daemon did not answer the claim within the claim wait",
          })
        }
        Err(rustix::io::Errno::INTR) => Ok(None),
        // The daemon went away with the claim still queued (killed before it accepted, or closing as it
        // did): it never answered, which a reconnect retries — not a refusal that ends the caller's calls
        // (found on CI 2026-10-01: `OsRefused { call: "recvmsg", code: Some(104) }`).
        Err(
          rustix::io::Errno::CONNRESET
          | rustix::io::Errno::CONNREFUSED
          | rustix::io::Errno::PIPE
          | rustix::io::Errno::NOTCONN,
        ) => {
          self.socket = None;
          Err(IpcError::DaemonUnavailable {
            endpoint: self.instance.clone(),
            why: "the daemon closed the rendezvous before answering the claim",
          })
        }
        Err(e) => {
          self.socket = None;
          Err(refused("recvmsg", e))
        }
        Ok(received) => {
          let socket = self.socket.take().ok_or(IpcError::Layout {
            reason: "the claim's socket was already taken",
          })?;
          if received.bytes == 0 {
            return Err(IpcError::DaemonUnavailable {
              endpoint: self.instance.clone(),
              why: "the daemon closed the rendezvous before answering the claim",
            });
          }
          decode_handoff(&self.instance, socket, &body, received.bytes, &mut control).map(Some)
        }
      }
    }

    /// Waits for the handoff between polls on the socket's readability, bounded by the claim wait.
    pub(super) fn wait(mut self) -> Result<Connected, IpcError> {
      use rustix::event::{PollFd, PollFlags, Timespec};
      loop {
        if let Some(connected) = self.poll()? {
          return Ok(connected);
        }
        let Some(socket) = self.socket.as_ref() else {
          continue;
        };
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let remaining = self.wait_ns.saturating_sub(elapsed);
        let timeout = Timespec {
          tv_sec: i64::try_from(remaining / NS_PER_SECOND).unwrap_or(i64::MAX),
          tv_nsec: i64::try_from(remaining % NS_PER_SECOND).unwrap_or(0),
        };
        let mut fds = [PollFd::new(socket, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
          Ok(_) | Err(rustix::io::Errno::INTR) => {}
          Err(e) => return Err(refused("poll", e)),
        }
      }
    }
  }

  /// Format: nanoseconds in a second, the unit split of a `poll` timeout.
  const NS_PER_SECOND: u64 = 1_000_000_000;

  /// Decodes a received handoff of `received` bytes: the daemon's typed refusal, or the region, the
  /// completion eventfd and the shard's kick, with `socket` kept as the control channel and the
  /// liveness signal.
  fn decode_handoff(
    instance: &str,
    socket: OwnedFd,
    body: &[u8; HANDOFF_BYTES],
    received: usize,
    control: &mut RecvAncillaryBuffer<'_>,
  ) -> Result<Connected, IpcError> {
    if received != HANDOFF_BYTES {
      return Err(IpcError::Layout {
        reason: "short handoff message",
      });
    }
    let mut client_word = [0u8; size_of::<u32>()];
    client_word.copy_from_slice(&body[HANDOFF_AT_CLIENT..HANDOFF_AT_LEN]);
    if u32::from_le_bytes(client_word) == REFUSED_CLIENT {
      // The daemon's typed refusal: no region, the bound in the length word (any descriptor that
      // came with it is closed with `control`).
      let mut limit_word = [0u8; size_of::<u64>()];
      limit_word.copy_from_slice(&body[HANDOFF_AT_LEN..HANDOFF_BYTES]);
      return Err(super::refusal_of(
        usize::try_from(u64::from_le_bytes(limit_word)).unwrap_or(usize::MAX),
        instance,
      ));
    }
    let mut fds: Vec<OwnedFd> = Vec::new();
    for message in control.drain() {
      if let RecvAncillaryMessage::ScmRights(iter) = message {
        fds.extend(iter);
      }
    }
    if fds.len() != HANDOFF_FDS {
      return Err(IpcError::Layout {
        reason: "the handoff carried the wrong number of descriptors",
      });
    }
    let kick = fds.pop().ok_or(IpcError::Layout {
      reason: "no kick fd",
    })?;
    let completion = fds.pop().ok_or(IpcError::Layout {
      reason: "no completion fd",
    })?;
    let region_fd = fds.pop().ok_or(IpcError::Layout {
      reason: "no region fd",
    })?;
    let mut len_word = [0u8; size_of::<u64>()];
    len_word.copy_from_slice(&body[HANDOFF_AT_LEN..HANDOFF_BYTES]);
    let len = usize::try_from(u64::from_le_bytes(len_word)).unwrap_or(0);
    let raw = std::os::fd::IntoRawFd::into_raw_fd(region_fd);
    let region = ClientRegion::open(&Handoff::Descriptor(raw), len)?;
    Ok(Connected {
      region,
      doorbell: Doorbell::Eventfd(kick),
      liveness: super::Liveness {
        inner: Liveness { socket },
      },
      control: Some(ClientControl { completion }),
    })
  }

  #[cfg(test)]
  mod tests {
    use std::os::fd::AsFd;

    use rustix::fs::{MemfdFlags, memfd_create};
    use rustix::net::{AddressFamily, SocketFlags, SocketType};

    use super::{Control, Liveness};
    use crate::error::IpcError;

    /// A-135: the daemon sends a read-only content-map descriptor with its metadata over the
    /// control socket, and the client receives that exact descriptor — the file's bytes read back
    /// through it. Drives the real `send_mapping`/`recv_delivery` over a real `AF_UNIX`/`SOCK_STREAM`
    /// pair (the kind the rendezvous retains), with a real `memfd` standing in for an arena block
    /// whose file is shorter than the block (so the metadata's file length is exercised). A failing
    /// check returns a typed error rather than panicking (the lint wall forbids a panic in a
    /// `Result` function).
    #[test]
    fn a_mapping_delivery_hands_the_client_the_block_descriptor_and_its_bytes()
    -> Result<(), IpcError> {
      let (daemon_socket, client_socket) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
      )
      .map_err(|e| super::refused("socketpair", e))?;
      let completion = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)
        .map_err(|e| super::refused("eventfd", e))?;
      let control = Control {
        socket: daemon_socket,
        completion,
      };
      let liveness = Liveness {
        socket: client_socket,
      };

      // An arena-block stand-in: a memfd holding a full block of known bytes, with the file's
      // authorized length shorter than the block (so the metadata's file length is exercised).
      // Writing the whole block sizes the memfd to it, so no `ftruncate` is needed.
      let block: Vec<u8> = (0..512)
        .map(|byte| u8::try_from(byte % 256).unwrap_or(0))
        .collect();
      let block_len = u64::try_from(block.len()).unwrap_or(0);
      let file_len: u64 = 300;
      let file_bytes = usize::try_from(file_len).unwrap_or(0);
      // Sealable, and sealed once written: `send_mapping` refuses a descriptor a peer could still
      // write or resize, so a hand-off that is meant to go through must be sealed — which is also how
      // a confined backing would be built.
      let mem = memfd_create(
        "slates-a135-test",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
      )
      .map_err(|e| super::refused("memfd_create", e))?;
      let wrote = rustix::io::pwrite(&mem, &block, 0).map_err(|e| super::refused("pwrite", e))?;
      if wrote != block.len() {
        return Err(IpcError::Layout {
          reason: "the block stand-in was not fully written",
        });
      }
      rustix::fs::fcntl_add_seals(
        &mem,
        rustix::fs::SealFlags::WRITE | rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
      )
      .map_err(|e| super::refused("add seals", e))?;

      // Nothing waiting yet: a non-blocking recv returns None.
      if liveness.recv_delivery()?.is_some() {
        return Err(IpcError::Layout {
          reason: "a delivery arrived before one was sent",
        });
      }

      control.send_mapping(mem.as_fd(), 7, 42, 0, file_len, block_len)?;

      let delivery = mapping_of(liveness.recv_delivery()?)?;
      let metadata_ok = delivery.request_id == 7
        && delivery.mapping_id == 42
        && delivery.region_off == 0
        && delivery.file_len == file_len
        && delivery.block_len == block_len;
      if !metadata_ok {
        return Err(IpcError::Layout {
          reason: "the mapping metadata did not survive the wire",
        });
      }

      // The delivered descriptor names the same object: the file's bytes read back through it,
      // equal to the daemon's bytes over exactly the authorized length.
      let mut got = vec![0u8; file_bytes];
      let read = rustix::io::pread(&delivery.block, &mut got, delivery.region_off)
        .map_err(|e| super::refused("pread", e))?;
      let want = block.get(..file_bytes).ok_or(IpcError::Layout {
        reason: "the block stand-in was shorter than the file length",
      })?;
      if read != file_bytes || got.as_slice() != want {
        return Err(IpcError::Layout {
          reason: "the bytes the client read through the delivered descriptor did not match",
        });
      }
      Ok(())
    }

    /// §4.6, A-135: a descriptor that would authorize more than the hand-off never reaches the peer.
    /// Do: offer `send_mapping` the three shapes a real arena produces — an object larger than the
    /// window, a window that starts partway into the object, and an object the peer could still write
    /// — and look on the client's side of the socket afterwards. Expect: each is refused
    /// `UnconfinedDescriptor` naming what it would have authorized, and nothing was sent, so a peer
    /// never holds a descriptor the check refused.
    ///
    /// This is the guard for the measured escape of 2026-10-18, where a 20 KiB read handed a peer a
    /// writable descriptor for a 176.8 GiB content object holding every volume on the host.
    #[test]
    fn a_descriptor_that_would_authorize_more_than_the_hand_off_is_never_sent()
    -> Result<(), IpcError> {
      let (daemon_socket, client_socket) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
      )
      .map_err(|e| super::refused("socketpair", e))?;
      let completion = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)
        .map_err(|e| super::refused("eventfd", e))?;
      let control = Control {
        socket: daemon_socket,
        completion,
      };
      let liveness = Liveness {
        socket: client_socket,
      };

      // A sealed object of 8 KiB, standing in for an arena holding more than one volume's blocks.
      let object_bytes: u64 = 8192;
      let window: u64 = 4096;
      let sealed = memfd_create(
        "slates-a135-arena",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
      )
      .map_err(|e| super::refused("memfd_create", e))?;
      #[allow(clippy::disallowed_methods)] // sizing a memory object: RAM, not a host path (R1).
      rustix::fs::ftruncate(&sealed, object_bytes).map_err(|e| super::refused("ftruncate", e))?;
      rustix::fs::fcntl_add_seals(
        &sealed,
        rustix::fs::SealFlags::WRITE | rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
      )
      .map_err(|e| super::refused("add seals", e))?;

      // The same bytes in an unsealed object exactly the size of the window: confined in extent, and
      // still refused, because the peer could write and resize it.
      let unsealed = memfd_create("slates-a135-open", MemfdFlags::CLOEXEC)
        .map_err(|e| super::refused("memfd_create", e))?;
      #[allow(clippy::disallowed_methods)] // sizing a memory object: RAM, not a host path (R1).
      rustix::fs::ftruncate(&unsealed, window).map_err(|e| super::refused("ftruncate", e))?;

      let offers: [(std::os::fd::BorrowedFd<'_>, u64, u64, &str); 3] = [
        (
          sealed.as_fd(),
          0,
          window,
          "an object larger than the window",
        ),
        (
          sealed.as_fd(),
          window,
          window,
          "a window partway into the object",
        ),
        (unsealed.as_fd(), 0, window, "a window the peer could write"),
      ];
      // Nothing crossed: the peer has no delivery waiting. Proved non-vacuously — a `recv_delivery` that could
      // receive nothing at all would satisfy that however `send_mapping` behaved — by sending one hand-off
      // that *does* meet the rule and taking it off the socket first. So the peer's side is known to work, and
      // the emptiness after the three refusals is a fact about the refusals.
      control.send_mapping(sealed.as_fd(), 9, 11, 0, object_bytes, object_bytes)?;
      let good = mapping_of(liveness.recv_delivery()?)?;
      if good.mapping_id != 11 {
        return Err(IpcError::Layout {
          reason: "the confined hand-off arrived with the wrong metadata",
        });
      }
      for (descriptor, region_off, authorized, what) in offers {
        match control.send_mapping(descriptor, 7, 42, region_off, authorized, authorized) {
          Err(IpcError::UnconfinedDescriptor { .. }) => {}
          Ok(()) => {
            let _ = what;
            return Err(IpcError::Layout {
              reason: "a descriptor that authorizes more than the hand-off was sent",
            });
          }
          Err(other) => return Err(other),
        }
      }
      if liveness.recv_delivery()?.is_some() {
        return Err(IpcError::Layout {
          reason: "a refused hand-off still reached the peer",
        });
      }
      Ok(())
    }

    /// The mapping a delivery holds, or a typed error for none or another kind.
    fn mapping_of(
      delivery: Option<super::super::ControlDelivery>,
    ) -> Result<super::super::MappingDelivery, IpcError> {
      match delivery {
        Some(super::super::ControlDelivery::Mapping(mapping)) => Ok(mapping),
        Some(super::super::ControlDelivery::Stream(_)) => Err(IpcError::Layout {
          reason: "a stream end arrived where a mapping was sent",
        }),
        None => Err(IpcError::Layout {
          reason: "a delivery was waiting but recv returned None",
        }),
      }
    }

    /// A control socket's two ends: the daemon's `Control` and the client's `Liveness`.
    fn channel() -> Result<(Control, Liveness), IpcError> {
      let (daemon_socket, client_socket) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
      )
      .map_err(|e| super::refused("socketpair", e))?;
      let completion = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC)
        .map_err(|e| super::refused("eventfd", e))?;
      Ok((
        Control {
          socket: daemon_socket,
          completion,
        },
        Liveness {
          socket: client_socket,
        },
      ))
    }

    /// A-161: the daemon sends a stream end with the attach it answers, and the client receives that very end. Do:
    /// send a stream end, then a content map, then a second stream end, back to back, before the client reads any.
    /// Expect: three deliveries in order, each its own kind with its own request and attachment, and the stream end
    /// carrying bytes between the two sides (it is the pair's other end, not a copy).
    #[test]
    fn a_stream_delivery_hands_the_client_its_end_and_kinds_never_mix() -> Result<(), IpcError> {
      let (control, liveness) = channel()?;
      let (daemon_end, handed) = crate::stream::pair()?;
      let (_other_daemon_end, other_handed) = crate::stream::pair()?;
      let block = memfd_create(
        "slates-a161-block",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
      )
      .map_err(|e| super::refused("memfd_create", e))?;
      #[allow(clippy::disallowed_methods)] // sizing a memory object: RAM, not a host path (R1).
      rustix::fs::ftruncate(&block, 4096).map_err(|e| super::refused("ftruncate", e))?;
      rustix::fs::fcntl_add_seals(
        &block,
        rustix::fs::SealFlags::WRITE | rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
      )
      .map_err(|e| super::refused("add seals", e))?;

      control.send_stream(handed.as_fd(), 5, 501)?;
      control.send_mapping(block.as_fd(), 6, 12, 0, 4096, 4096)?;
      control.send_stream(other_handed.as_fd(), 7, 502)?;
      drop(handed);

      let first = match liveness.recv_delivery()? {
        Some(super::super::ControlDelivery::Stream(stream)) => stream,
        _ => {
          return Err(IpcError::Layout {
            reason: "the first delivery was not the stream end sent first",
          });
        }
      };
      let mapping = mapping_of(liveness.recv_delivery()?)?;
      let third = match liveness.recv_delivery()? {
        Some(super::super::ControlDelivery::Stream(stream)) => stream,
        _ => {
          return Err(IpcError::Layout {
            reason: "the third delivery was not the second stream end",
          });
        }
      };
      if (first.request_id, first.attachment) != (5, 501)
        || (mapping.request_id, mapping.mapping_id) != (6, 12)
        || (third.request_id, third.attachment) != (7, 502)
      {
        return Err(IpcError::Layout {
          reason: "a delivery's request or attachment did not survive the wire",
        });
      }
      rustix::io::write(&first.stream, b"ping").map_err(|e| super::refused("write", e))?;
      let mut got = [0u8; 4];
      let read =
        crate::stream::recv(daemon_end.as_fd(), &mut got).map_err(|e| super::refused("recv", e))?;
      if got.get(..read) != Some(&b"ping"[..]) {
        return Err(IpcError::Layout {
          reason: "the delivered end is not the daemon's pair",
        });
      }
      if liveness.recv_delivery()?.is_some() {
        return Err(IpcError::Layout {
          reason: "a fourth delivery appeared from nowhere",
        });
      }
      Ok(())
    }

    /// Hostile input on the client's side: a delivery of an unknown kind, and one of a known kind but the wrong
    /// length, are refused `Layout`, and the descriptor each carried is closed in the client (its peer sees the end).
    #[test]
    fn an_unknown_or_misshapen_delivery_is_refused_and_its_descriptor_closed()
    -> Result<(), IpcError> {
      use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
      let (control, liveness) = channel()?;
      for (kind, length) in [(99u64, 24usize), (super::KIND_STREAM, 20)] {
        let (daemon_end, handed) = crate::stream::pair()?;
        let mut body = vec![0u8; length];
        super::put_words(&mut body, &[(0, kind)]);
        let fds = [handed.as_fd()];
        let mut space = [std::mem::MaybeUninit::<u8>::uninit();
          rustix::cmsg_space!(ScmRights(super::DELIVERY_FDS))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
          return Err(IpcError::Layout {
            reason: "the hostile delivery's descriptor did not fit",
          });
        }
        rustix::net::sendmsg(
          &control.socket,
          &[std::io::IoSlice::new(&body)],
          &mut ancillary,
          SendFlags::empty(),
        )
        .map_err(|e| super::refused("sendmsg", e))?;
        drop(handed);
        if !matches!(liveness.recv_delivery(), Err(IpcError::Layout { .. })) {
          return Err(IpcError::Layout {
            reason: "a hostile delivery was taken",
          });
        }
        let mut probe = [0u8; 1];
        if crate::stream::recv(daemon_end.as_fd(), &mut probe) != Ok(0) {
          return Err(IpcError::Layout {
            reason: "a refused delivery's descriptor stayed open in the client",
          });
        }
      }
      Ok(())
    }
  }
}

#[cfg(any(target_os = "macos", windows))]
pub mod platform {
  //! macOS and Windows: the bootstrap object with claim slots.

  use std::sync::atomic::{AtomicU32, Ordering};

  use slates_mem::{Handoff, SharedObject, Width, WordRun, Words};

  use super::{Accepted, CLAIM_WAIT_NS, Connected, Doorbell, Prepared, Served, rendezvous_name};
  use crate::error::IpcError;
  use crate::region::ClientRegion;
  #[cfg(not(windows))]
  use crate::wake;

  /// Format: the bootstrap object's magic, `SLBT` in little-endian ASCII.
  const MAGIC: u32 = 0x5442_4C53;
  /// Format: the bootstrap header: magic (4), slots (4), the daemon-wide doorbell word (4),
  /// padding (4), the daemon's start stamp (8), padding (4), the holding anchor's process id (4),
  /// padding to a cache line. The padding after the stamp held the daemon's process id until A-159: a client judges the
  /// daemon that answered its claim by the pid in the answer (A-143), so the header's copy had no reader left.
  const HEADER_BYTES: usize = 64;
  /// Format: the slot count's offset in the header (after the magic).
  const AT_SLOT_COUNT: usize = 4;
  /// Format: the doorbell word's offset in the header.
  pub const AT_DOORBELL: usize = 8;
  /// Format: the start stamp's offset in the header: the wall clock in nanoseconds when the
  /// daemon opened the object, so a client that remembers it tells a restarted daemon (a new
  /// stamp) from a slow one (the same stamp).
  const AT_GENERATION: usize = 16;
  /// Format: the holding anchor's process id's offset in the header (A-114): written when the anchor creates the
  /// object, zero in an object a daemon made for itself. A claim made while this process lives may wait out a restart.
  const AT_ANCHOR_PID: usize = 28;
  /// Shape: claim slots in the bootstrap object: clients connecting inside one control-shard
  /// loop; the loop drains them, so the table only covers one loop of arrivals.
  const SLOTS: usize = 64;
  /// Format: a claim slot: state (4), client pid (4), client id (4), padding (4), region
  /// length (8), region name (40), the answering daemon's start stamp (8) and process id (4),
  /// padding to two cache lines.
  const SLOT_BYTES: usize = 128;
  /// Format: the state word's offset in a slot.
  const AT_STATE: usize = 0;
  /// Format: the pid's offset.
  const AT_PID: usize = 4;
  /// Format: the client id's offset.
  const AT_CLIENT: usize = 8;
  /// Format: the region length's offset.
  const AT_LEN: usize = 16;
  /// Format: the region name's offset and width.
  const AT_NAME: usize = 24;
  /// Format: the region name's width.
  const NAME_BYTES: usize = 40;
  /// Format: the offset in a slot of the answering daemon's start stamp (A-143), written with its answer. A client
  /// judges its daemon by this stamp, not by the header's: the header names whichever daemon published last, and a
  /// claim begun before a restart is answered by the next daemon (A-114).
  const AT_ANSWERED_STAMP: usize = 64;
  /// Format: the offset in a slot of the answering daemon's process id (A-143), written with its answer: the process
  /// a client's exit watch is taken on.
  const AT_ANSWERED_PID: usize = 72;
  /// Format: the slot states.
  const FREE: u32 = 0;
  /// Format: a client claimed the slot.
  const CLAIMED: u32 = 1;
  /// Format: the daemon wrote the region into the slot.
  const READY: u32 = 2;
  /// Format: a client took the slot and is writing its pid and wanted id; the daemon ignores it until
  /// the client publishes `CLAIMED` (AUD-29-09: before, the claim was visible before those fields were
  /// written, and the daemon could read them unwritten).
  const CLAIMING: u32 = 5;
  /// Format: the daemon took a claimed slot to answer it; neither the client's timeout nor another client
  /// can take the slot while the daemon writes its answer.
  const ANSWERING: u32 = 6;
  /// Format: the client opened the region; the daemon reclaims the slot.
  const DONE: u32 = 3;
  /// The daemon refused the claim: the client reads it typed and marks the slot `DONE`, where before a
  /// refused claim was left `CLAIMED` and the client waited out its claim wait (2026-09-14). The slot's
  /// length word carries the client bound (`IpcError::TooManyClients`), or `NO_BOUND` for a region
  /// refused for another reason (the daemon unavailable, with the reason).
  /// Format: the fourth slot state, after `FREE`/`CLAIMED`/`READY`/`DONE`.
  const REFUSED: u32 = 4;
  /// Derived: how long a slot may sit unchanged in a state a live party moves on from (`CLAIMING`,
  /// `READY`, `REFUSED`) before the daemon takes it back: twice the claim wait, past which no live client
  /// is still waiting on it (a client gives up after one claim wait, or two once the daemon took its
  /// claim), so only a dead party's slot is reclaimed.
  const STALE_SLOT_NS: u64 = 2 * CLAIM_WAIT_NS;

  /// No control channel on these platforms yet: the bootstrap-object rendezvous passes no descriptor,
  /// so the completion fd an async SDK polls (D-19) arrives with the control socket owed here (a Unix
  /// socketpair on macOS, a loopback socket on Windows). Until then `into_completion` yields `None` and
  /// the SDK falls back to the sync path.
  pub struct Control;

  /// No client control channel on these platforms yet (the completion fd is owed, as for `Control`).
  pub struct ClientControl;

  #[cfg(unix)]
  impl Control {
    /// No completion fd on macOS yet (the control socket is owed); the daemon nudges nothing.
    pub fn completion_dup(&self) -> Option<std::os::fd::OwnedFd> {
      None
    }

    /// No control socket here yet (§4.6, A-135): the same-kernel direct map is not offered, so the
    /// server falls back to a copy. The owed macOS control socket would carry the block's descriptor.
    pub fn send_mapping(
      &self,
      _block: std::os::fd::BorrowedFd<'_>,
      _request_id: u64,
      _mapping_id: u64,
      _region_off: u64,
      _file_len: u64,
      _block_len: u64,
    ) -> Result<(), IpcError> {
      Err(IpcError::Unsupported {
        feature: "same-kernel direct map (macOS control socket owed)",
      })
    }

    /// No control socket here (A-161): a stream end reaches a macOS client by the socket route instead.
    pub fn send_stream(
      &self,
      _stream: std::os::fd::BorrowedFd<'_>,
      _request_id: u64,
      _attachment: u64,
    ) -> Result<(), IpcError> {
      Err(IpcError::Unsupported {
        feature: "a stream end over the control socket (macOS has the socket route)",
      })
    }
  }

  #[cfg(unix)]
  impl ClientControl {
    /// No completion fd on macOS yet (the control socket is owed); an async SDK falls back to polling.
    pub fn into_completion(self) -> Option<std::os::fd::OwnedFd> {
      None
    }
  }

  /// The client's liveness check: the daemon's process exited (its exit watch), or that daemon's start stamp
  /// differs from the one the bootstrap object holds now (none when no daemon holds the object). Both the
  /// process and the stamp are the ones the answering daemon wrote into its answer (A-143), never the
  /// header's, which may already name a later daemon. The stamp alone cannot see a killed daemon nothing
  /// restarted, since the object outlives it (AUD-29-20).
  pub struct Liveness {
    instance: String,
    generation: u64,
    watch: crate::exit_watch::ExitWatch,
  }

  impl Liveness {
    pub(super) fn daemon_gone(&self) -> bool {
      self.watch.exited() || generation_of(&self.instance).is_none_or(|now| now != self.generation)
    }

    /// No control socket here yet (§4.6, A-135), so no descriptor is ever delivered over one: a same-kernel client on
    /// this platform reads by copy, and a stream end comes by the platform's own route (A-161).
    #[cfg(unix)]
    pub(super) fn recv_delivery(&self) -> Result<Option<super::ControlDelivery>, IpcError> {
      Ok(None)
    }
  }

  /// The start stamp of the daemon holding `instance`'s bootstrap object, if one does. The stamp is
  /// the object's publication word: zero until the daemon has written the header, so a reader that maps
  /// the object mid-creation sees "not yet", never a torn header.
  fn generation_of(instance: &str) -> Option<u64> {
    let handoff = SharedObject::handoff_for_name(&rendezvous_name(instance))?;
    let object = SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words()).ok()?;
    published(&object).ok().flatten()
  }

  /// The object's start stamp once its header is published (`None` before), with the magic checked.
  fn published(object: &SharedObject) -> Result<Option<u64>, IpcError> {
    let generation = object.atomic_u64(AT_GENERATION)?.load(Ordering::Acquire);
    if generation == 0 {
      return Ok(None);
    }
    if read_u32(object, 0)? != MAGIC {
      return Err(IpcError::Layout {
        reason: "bootstrap object has the wrong magic",
      });
    }
    Ok(Some(generation))
  }

  /// The wall clock in nanoseconds, the start stamp of a daemon opening the object now.
  fn stamp_now() -> u64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
      .unwrap_or(0)
  }

  pub(super) struct Listener {
    object: SharedObject,
    /// The start stamp this daemon published in the header, written into each of its answers (A-143).
    stamp: u64,
    /// Per claim slot, the state it was last seen in and since when: a slot left in a state only a live
    /// party moves on from is reclaimed past [`STALE_SLOT_NS`]. Bounded by the table.
    seen: Vec<Option<(u32, std::time::Instant)>>,
    /// One named Event per claim slot, which the slot's claimant waits on for READY or REFUSED and
    /// which is signaled here once the claim is answered. `WaitOnAddress` on the slot word is
    /// process-local on Windows (D-10), so it cannot wake a client in another process; the Event
    /// does. One Event per slot, not one per instance: an auto-reset Event wakes one waiter per
    /// signal, so with one Event shared by concurrent claimants a signal could be consumed by a
    /// claimant whose slot was not answered, and the answered one waited out its claim wait.
    /// macOS's `__ulock` wake reaches across processes, so the Events are Windows-only.
    #[cfg(windows)]
    ready_events: Vec<crate::wake::Event>,
    /// The daemon-wide doorbell Event, held for the daemon's life so its name exists from the
    /// moment the bootstrap object does (Windows; [`Bell`]).
    #[cfg(windows)]
    _doorbell_event: crate::wake::Event,
    /// The instance, for the doorbell waiters' own handles.
    #[cfg(windows)]
    instance: String,
    /// This daemon's ownership of the object when it made its own (A-176); none when it adopted its anchor's.
    _owner: Option<Owner>,
  }

  impl Drop for Listener {
    /// A daemon that stops takes back the start stamp it published, so its clients judge it gone at their next liveness
    /// question, as Linux's do when its control socket closes (A-176). With an anchor holding the object, a stopped
    /// daemon's stamp otherwise stays published, and a client in the daemon's own process, whose exit watch sees no
    /// exit, judged it alive for good; on Windows a client's own doorbell kept even an unheld object, and its stamp,
    /// alive. By exchange, so a successor's stamp is never taken back.
    fn drop(&mut self) {
      if let Ok(word) = self.object.atomic_u64(AT_GENERATION) {
        // Refused only when a successor has stamped the object since, whose stamp is to be left as it is.
        let _ = word.compare_exchange(self.stamp, 0, Ordering::AcqRel, Ordering::Acquire);
      }
    }
  }

  /// The name of claim slot `index`'s READY Event (Windows), derived from the bootstrap object's
  /// name so the daemon and the claimant name the one Event with no handle passing.
  #[cfg(windows)]
  fn ready_event_name(instance: &str, index: usize) -> String {
    format!("{}-rvz{index}", rendezvous_name(instance))
  }

  /// The name of the daemon-wide doorbell Event (Windows).
  #[cfg(windows)]
  fn doorbell_event_name(instance: &str) -> String {
    format!("{}-bell", rendezvous_name(instance))
  }

  /// The daemon-wide doorbell (§4.7): the bootstrap object's word, which a ring bumps so the
  /// doorbell thread sees a change against the value it last acted on (a ring between two of its
  /// waits is never lost), and the wake that reaches the thread where it waits: the word itself
  /// on macOS, whose wake crosses processes, or the named doorbell Event on Windows, where the
  /// word's wake is process-local (D-10). A client rings it; the daemon's thread waits on it.
  pub struct Bell {
    object: SharedObject,
    #[cfg(windows)]
    event: crate::wake::Event,
  }

  impl Bell {
    /// The doorbell of `instance` over a mapping of its bootstrap object.
    fn new(object: SharedObject, instance: &str) -> Result<Bell, IpcError> {
      #[cfg(not(windows))]
      let _ = instance;
      Ok(Bell {
        object,
        #[cfg(windows)]
        event: crate::wake::Event::open(&doorbell_event_name(instance))?,
      })
    }

    fn word(&self) -> Result<&AtomicU32, IpcError> {
      Ok(self.object.atomic_u32(AT_DOORBELL)?)
    }

    /// Rings: bumps the word, then wakes the waiting thread.
    pub(super) fn ring(&self) -> Result<(), IpcError> {
      let word = self.word()?;
      word.fetch_add(1, Ordering::AcqRel);
      #[cfg(not(windows))]
      wake::wake_one(word)?;
      #[cfg(windows)]
      self.event.signal()?;
      Ok(())
    }

    /// The word's value now.
    pub(super) fn current(&self) -> Result<u32, IpcError> {
      Ok(self.word()?.load(Ordering::Acquire))
    }

    /// Waits until the word differs from `seen` or `timeout_ns` passes; the word's value then.
    pub(super) fn wait(&self, seen: u32, timeout_ns: u64) -> Result<u32, IpcError> {
      let word = self.word()?;
      #[cfg(not(windows))]
      wake::wait(word, seen, Some(timeout_ns))?;
      // A ring bumps the word before it signals, so a word already past `seen` needs no wait; a
      // signal left over from a ring already seen ends one wait early, and the caller compares.
      #[cfg(windows)]
      if word.load(Ordering::Acquire) == seen {
        self.event.wait(Some(timeout_ns))?;
      }
      Ok(word.load(Ordering::Acquire))
    }
  }

  /// The daemon's doorbell waiter is the bell itself.
  pub type DoorbellWaiter = Bell;

  /// Format: the bootstrap object's length: the header and the claim slots.
  const OBJECT_BYTES: usize = HEADER_BYTES + SLOTS * SLOT_BYTES;

  /// The bootstrap object's declared atomic words (AUD-29-09): the doorbell, the start stamp (the
  /// header's publication word) and every claim slot's state word; the rest is plain, owned by whichever
  /// side the slot's state names.
  fn bootstrap_words() -> Words {
    Words::new()
      .with(WordRun::one(AT_DOORBELL, Width::U32))
      .with(WordRun::one(AT_GENERATION, Width::U64))
      .with(WordRun::strided(
        HEADER_BYTES + AT_STATE,
        SLOT_BYTES,
        SLOTS,
        Width::U32,
      ))
  }

  fn slot_at(index: usize) -> usize {
    index
      .saturating_mul(SLOT_BYTES)
      .saturating_add(HEADER_BYTES)
  }

  fn state(object: &SharedObject, index: usize) -> Result<&AtomicU32, IpcError> {
    Ok(object.atomic_u32(slot_at(index).saturating_add(AT_STATE))?)
  }

  fn read_u32(object: &SharedObject, at: usize) -> Result<u32, IpcError> {
    let mut word = [0u8; size_of::<u32>()];
    object.read(at, &mut word)?;
    Ok(u32::from_le_bytes(word))
  }

  fn read_u64(object: &SharedObject, at: usize) -> Result<u64, IpcError> {
    let mut word = [0u8; size_of::<u64>()];
    object.read(at, &mut word)?;
    Ok(u64::from_le_bytes(word))
  }

  /// The region name a `READY` slot names, and the region's length.
  fn read_answer(object: &SharedObject, at: usize) -> Result<(String, usize), IpcError> {
    let mut raw = [0u8; NAME_BYTES];
    object.read(at.saturating_add(AT_NAME), &mut raw)?;
    let end = raw.iter().position(|b| *b == 0).unwrap_or(NAME_BYTES);
    let name = String::from_utf8_lossy(raw.get(..end).unwrap_or_default()).into_owned();
    let len = read_u64(object, at.saturating_add(AT_LEN))?;
    Ok((name, usize::try_from(len).unwrap_or(0)))
  }

  /// A slot the daemon answers with a region: the assigned id, the length and the name, written while it
  /// holds the slot `ANSWERING`.
  fn write_answer(
    object: &mut SharedObject,
    at: usize,
    client_id: u32,
    len: usize,
    name: &str,
  ) -> Result<(), IpcError> {
    let mut raw = [0u8; NAME_BYTES];
    if let Some(field) = raw.get_mut(..name.len()) {
      field.copy_from_slice(name.as_bytes());
    }
    object.write(at.saturating_add(AT_CLIENT), &client_id.to_le_bytes())?;
    object.write(
      at.saturating_add(AT_LEN),
      &u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes(),
    )?;
    object.write(at.saturating_add(AT_NAME), &raw)?;
    Ok(())
  }

  /// The answering daemon's identity, written into the slot at `at` with its answer while it holds the slot
  /// `ANSWERING` (A-143): the start stamp it published and its process id.
  fn write_answerer(
    object: &mut SharedObject,
    at: usize,
    stamp: u64,
    pid: u32,
  ) -> Result<(), IpcError> {
    object.write(at.saturating_add(AT_ANSWERED_STAMP), &stamp.to_le_bytes())?;
    object.write(at.saturating_add(AT_ANSWERED_PID), &pid.to_le_bytes())?;
    Ok(())
  }

  /// The identity of the daemon that answered the slot at `at` (A-143): its start stamp and process id. A stamp of
  /// zero is an answer that names no daemon, written by a daemon of another version (`docs/publish.md`: one
  /// version), and is refused.
  fn read_answerer(object: &SharedObject, at: usize) -> Result<(u64, u32), IpcError> {
    let stamp = read_u64(object, at.saturating_add(AT_ANSWERED_STAMP))?;
    if stamp == 0 {
      return Err(IpcError::Layout {
        reason: "an answered claim does not name its daemon (a daemon of another version)",
      });
    }
    Ok((stamp, read_u32(object, at.saturating_add(AT_ANSWERED_PID))?))
  }

  /// What makes a process the owner of an instance's bootstrap object (A-176): the anchor that holds it, or a daemon
  /// run alone that made its own. A token held for the owner's life, made only if no live process holds one by its
  /// name, and gone with its holder, so the name lives exactly as long as the owner. The bootstrap object's own name
  /// cannot tell that. On Windows it lives while any process maps the section, and every connected client's doorbell
  /// maps it, so a dead owner's object outlives it in its clients; there the token is a named Event opened by no one
  /// else. On macOS the object's name lives until it is unlinked, and an owner used to unlink whatever object held
  /// it, a live owner's too; there the token is a Mach bootstrap name (A-178, `crates/ipc/src/owner_name.rs`).
  pub(super) struct Owner {
    #[cfg(windows)]
    _token: crate::wake::Event,
    #[cfg(target_os = "macos")]
    _token: crate::owner_name::OwnerName,
  }

  /// The name of the owner's token (Windows), beside the bootstrap object's.
  #[cfg(windows)]
  fn owner_token_name(instance: &str) -> String {
    format!("{}-owner", rendezvous_name(instance))
  }

  /// The name of the owner's token (macOS): the user's id and a key of the bootstrap object's name, so two users'
  /// instances never meet and the name stays within a bootstrap name's bytes however long the instance's name is.
  #[cfg(target_os = "macos")]
  fn owner_token_name(instance: &str) -> String {
    let digest = blake3::hash(rendezvous_name(instance).as_bytes());
    let key = digest
      .as_bytes()
      .first_chunk::<{ size_of::<u64>() }>()
      .map_or(0, |first| u64::from_le_bytes(*first));
    format!(
      "com.slates.owner.{}.{key:016x}",
      rustix::process::getuid().as_raw()
    )
  }

  /// Takes ownership of `instance`'s rendezvous, refused while a live process holds it: a daemon of the instance, or
  /// its anchor.
  #[cfg(windows)]
  fn take_ownership(instance: &str) -> Result<Owner, IpcError> {
    let token = crate::wake::Event::create_new(&owner_token_name(instance))?.ok_or_else(|| {
      IpcError::InstanceHeld {
        instance: instance.to_owned(),
      }
    })?;
    Ok(Owner { _token: token })
  }

  /// The paired arm (macOS): the owner's bootstrap name, refused while a live process holds it.
  #[cfg(target_os = "macos")]
  fn take_ownership(instance: &str) -> Result<Owner, IpcError> {
    let token = crate::owner_name::claim(&owner_token_name(instance))?.ok_or_else(|| {
      IpcError::InstanceHeld {
        instance: instance.to_owned(),
      }
    })?;
    Ok(Owner { _token: token })
  }

  /// A fresh bootstrap object for `instance`, owned by this process: the header's magic and slot count, every slot
  /// free, nothing published (the start stamp zero until a daemon writes it). On Windows a dead owner's object that
  /// its clients still map is taken over instead ([`owned_object`]).
  fn fresh(instance: &str) -> Result<(SharedObject, Owner), IpcError> {
    let owner = take_ownership(instance)?;
    Ok((owned_object(instance)?, owner))
  }

  /// macOS: the object made new. `SharedObject::create` unlinks the name first, and the object it unlinks is a dead
  /// owner's: this process holds the owner's token, so no live owner holds the object.
  #[cfg(not(windows))]
  fn owned_object(instance: &str) -> Result<SharedObject, IpcError> {
    initialized(SharedObject::create(
      &rendezvous_name(instance),
      OBJECT_BYTES,
      bootstrap_words(),
    )?)
  }

  /// Windows: the object a dead owner left while its clients still map it, taken over; else one made new. This
  /// process holds the owner's token, so no live owner holds the object, and no other owner makes one by its name
  /// meanwhile.
  #[cfg(windows)]
  fn owned_object(instance: &str) -> Result<SharedObject, IpcError> {
    let handoff =
      SharedObject::handoff_for_name(&rendezvous_name(instance)).ok_or(IpcError::Unsupported {
        feature: "rendezvous by name",
      })?;
    match SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words()) {
      Ok(left) => take_over(left),
      // None to open: one is made. Whatever kept the open from finding one, a creation that fails too says why.
      Err(_) => initialized(SharedObject::create(
        &rendezvous_name(instance),
        OBJECT_BYTES,
        bootstrap_words(),
      )?),
    }
  }

  /// A new object's header and table: the magic, the slot count, every slot free.
  fn initialized(mut object: SharedObject) -> Result<SharedObject, IpcError> {
    object.write(0, &MAGIC.to_le_bytes())?;
    object.write(
      AT_SLOT_COUNT,
      &u32::try_from(SLOTS).unwrap_or(u32::MAX).to_le_bytes(),
    )?;
    for i in 0..SLOTS {
      state(&object, i)?.store(FREE, Ordering::Release);
    }
    Ok(object)
  }

  /// A dead owner's object taken over (Windows, A-176): refused unless it is this layout's, its claims readied as a
  /// restarted daemon readies them ([`readied`]), and nothing published and no anchor named until the new owner says
  /// so. Its dead daemon's clients then judge it gone, and their claims are answered by the next daemon.
  #[cfg(windows)]
  fn take_over(left: SharedObject) -> Result<SharedObject, IpcError> {
    let mut object = readied(
      left,
      "the rendezvous's name holds an object of another layout",
    )?;
    object
      .atomic_u64(AT_GENERATION)?
      .store(0, Ordering::Release);
    object.write(AT_ANCHOR_PID, &0u32.to_le_bytes())?;
    Ok(object)
  }

  /// `object`, refused with `reason` unless it is this layout's (its magic and slot count), with its slots readied
  /// for the daemon that answers them now: a claim the last daemon took (`ANSWERING`) and never answered goes back to
  /// `CLAIMED`, to be answered now; a slot its claimant finished (`DONE`) is freed. A claim still being written, one
  /// published, and an answer not yet read are left as they are, for the accept and the stale reclaim.
  fn readied(object: SharedObject, reason: &'static str) -> Result<SharedObject, IpcError> {
    if read_u32(&object, 0)? != MAGIC
      || usize::try_from(read_u32(&object, AT_SLOT_COUNT)?).ok() != Some(SLOTS)
    {
      return Err(IpcError::Layout { reason });
    }
    for i in 0..SLOTS {
      let word = state(&object, i)?;
      // Each exchange is refused for a slot in any other state, which is then left as it is.
      let _ = word.compare_exchange(ANSWERING, CLAIMED, Ordering::AcqRel, Ordering::Acquire);
      let _ = word.compare_exchange(DONE, FREE, Ordering::AcqRel, Ordering::Acquire);
    }
    Ok(object)
  }

  /// The bootstrap object the anchor holds (A-114): created once, kept for the anchor's life, so a client's claim made
  /// while a daemon restarts stays where the next daemon reads it; and with it the anchor's ownership (A-176).
  pub(super) struct Held {
    _object: SharedObject,
    _owner: Owner,
  }

  impl Held {
    pub(super) fn hold(instance: &str) -> Result<Held, IpcError> {
      let (mut object, owner) = fresh(instance)?;
      object.write(AT_ANCHOR_PID, &current_pid().to_le_bytes())?;
      Ok(Held {
        _object: object,
        _owner: owner,
      })
    }

    pub(super) fn env_value(&self) -> String {
      "held".to_owned()
    }
  }

  /// Opens the bootstrap object an anchor holds for `instance`, refused unless it is one (its magic and slot count),
  /// and readies its slots for this daemon ([`readied`]).
  fn adopt(instance: &str) -> Result<SharedObject, IpcError> {
    let handoff =
      SharedObject::handoff_for_name(&rendezvous_name(instance)).ok_or(IpcError::Unsupported {
        feature: "rendezvous by name",
      })?;
    readied(
      SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words())?,
      "the anchor's rendezvous object is not this layout's",
    )
  }

  impl Listener {
    pub(super) fn open(instance: &str, adoption: Option<&str>) -> Result<Listener, IpcError> {
      // The anchor owns the object it holds; a daemon run alone owns the one it makes.
      let (object, owner) = if adoption.is_some() {
        (adopt(instance)?, None)
      } else {
        let (object, owner) = fresh(instance)?;
        (object, Some(owner))
      };
      // The start stamp last, with release: it publishes the header and the free table (never zero).
      let stamp = stamp_now().max(1);
      object
        .atomic_u64(AT_GENERATION)?
        .store(stamp, Ordering::Release);
      Ok(Listener {
        object,
        stamp,
        seen: vec![None; SLOTS],
        #[cfg(windows)]
        ready_events: (0..SLOTS)
          .map(|index| crate::wake::Event::open(&ready_event_name(instance, index)))
          .collect::<Result<Vec<_>, _>>()?,
        #[cfg(windows)]
        _doorbell_event: crate::wake::Event::open(&doorbell_event_name(instance))?,
        #[cfg(windows)]
        instance: instance.to_owned(),
        _owner: owner,
      })
    }

    /// Signals claim slot `index`'s claimant that its slot was answered (Windows).
    #[cfg(windows)]
    fn signal_ready(&self, index: usize) -> Result<(), IpcError> {
      self
        .ready_events
        .get(index)
        .ok_or(IpcError::Layout {
          reason: "a claim slot past the bootstrap table",
        })?
        .signal()
    }

    pub(super) fn doorbell(&self) -> Option<&AtomicU32> {
      self.object.atomic_u32(AT_DOORBELL).ok()
    }

    pub(super) fn doorbell_waiter(&self) -> Result<Option<DoorbellWaiter>, IpcError> {
      let handoff = self.object.handoff()?;
      let object = SharedObject::open(&handoff, self.object.len(), bootstrap_words())?;
      #[cfg(windows)]
      let instance = self.instance.as_str();
      #[cfg(not(windows))]
      let instance = "";
      Ok(Some(Bell::new(object, instance)?))
    }

    pub(super) fn raw_fd(&self) -> Option<i32> {
      None
    }

    pub(super) fn accept_one(
      &mut self,
      assign: &mut dyn FnMut(u32) -> u32,
      make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Option<Served>, IpcError> {
      for i in 0..SLOTS {
        let word = state(&self.object, i)?;
        let now = word.load(Ordering::Acquire);
        match now {
          DONE => word.store(FREE, Ordering::Release),
          CLAIMED => {
            // Take the claim to answer it; a client whose claim wait just ran out took it back first.
            if word
              .compare_exchange(CLAIMED, ANSWERING, Ordering::AcqRel, Ordering::Acquire)
              .is_ok()
            {
              let answered = self.answer(i, assign, make_region);
              // A claim this daemon took is always answered: a failure after it took the slot is the
              // claimant's refusal, never a slot left `ANSWERING` for the claimant to wait out. That
              // failure is this claim's alone (A-176); only one to answer it is the rendezvous's.
              if let Err(error) = &answered
                && state(&self.object, i)?.load(Ordering::Acquire) == ANSWERING
              {
                self.refuse(i, error)?;
              }
              return Ok(Some(match answered {
                Ok(accepted) => Served::Accepted(Box::new(accepted)),
                Err(refusal) => Served::Refused(refusal),
              }));
            }
          }
          CLAIMING | READY | REFUSED => {
            self.reclaim_if_stale(i, now)?;
            continue;
          }
          _ => {}
        }
        self.forget(i);
      }
      Ok(None)
    }

    /// Forgets when slot `index` was first seen in its current state.
    fn forget(&mut self, index: usize) {
      if let Some(seen) = self.seen.get_mut(index) {
        *seen = None;
      }
    }

    /// Takes slot `index` back to `FREE` when it has sat in `now` — a state only a live party moves on
    /// from — past [`STALE_SLOT_NS`]: its client died claiming, or before reading its answer. By CAS, so a
    /// party that moves it on meanwhile keeps it.
    fn reclaim_if_stale(&mut self, index: usize, now: u32) -> Result<(), IpcError> {
      let Some(seen) = self.seen.get_mut(index) else {
        return Ok(());
      };
      let since = match seen {
        Some((state_seen, since)) if *state_seen == now => *since,
        _ => {
          *seen = Some((now, std::time::Instant::now()));
          return Ok(());
        }
      };
      if u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX) >= STALE_SLOT_NS {
        let _ = state(&self.object, index)?.compare_exchange(
          now,
          FREE,
          Ordering::AcqRel,
          Ordering::Acquire,
        );
        *seen = None;
      }
      Ok(())
    }

    /// Answers the claim in slot `index`, which this daemon holds `ANSWERING`: a region, or the typed
    /// refusal.
    fn answer(
      &mut self,
      index: usize,
      assign: &mut dyn FnMut(u32) -> u32,
      make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Accepted, IpcError> {
      let at = slot_at(index);
      let wanted = read_u32(&self.object, at.saturating_add(AT_CLIENT))?;
      let pid = read_u32(&self.object, at.saturating_add(AT_PID))?;
      let client_id = assign(wanted);
      let Prepared { region, .. } = match make_region(client_id) {
        Ok(prepared) => prepared,
        Err(error) => {
          self.refuse(index, &error)?;
          return Err(error);
        }
      };
      // From here the client holds an id the daemon reserved: a failure is reported with that id so the
      // daemon gives it back (`IpcError::HandoffLost`).
      let lost = |cause: IpcError| IpcError::HandoffLost {
        client_id,
        cause: Box::new(cause),
      };
      let (handoff, len) = region.handoff().map_err(lost)?;
      let Handoff::Name(name) = handoff else {
        return Err(lost(IpcError::Layout {
          reason: "a region here hands off a name",
        }));
      };
      if name.len() > NAME_BYTES {
        return Err(lost(IpcError::Layout {
          reason: "region name longer than the slot holds",
        }));
      }
      write_answer(&mut self.object, at, client_id, len, &name).map_err(lost)?;
      write_answerer(&mut self.object, at, self.stamp, current_pid()).map_err(lost)?;
      self.publish(index, READY).map_err(lost)?;
      Ok(Accepted {
        client_id,
        // The object's mode and per-user name are the authentication: whoever opened
        // it is the daemon's user (the kernel refused everyone else).
        uid: current_uid(),
        pid,
        region,
        control: Some(Control),
      })
    }

    /// Answers slot `index` with `error`: the client bound when that is the reason, else no bound — the
    /// client reports the daemon refused it.
    fn refuse(&mut self, index: usize, error: &IpcError) -> Result<(), IpcError> {
      let limit = match error {
        IpcError::TooManyClients { limit } => *limit,
        _ => super::NO_BOUND,
      };
      let at = slot_at(index).saturating_add(AT_LEN);
      self
        .object
        .write(at, &u64::try_from(limit).unwrap_or(u64::MAX).to_le_bytes())?;
      self.publish(index, REFUSED)
    }

    /// Publishes slot `index`'s answer (`READY` or `REFUSED`, with release after its fields) and wakes the
    /// claimant where it waits: on the word here (macOS); on Windows the named ready Event, since the word
    /// wake is process-local there (D-10) — the Event holds a signal raised before the client waits.
    fn publish(&self, index: usize, answer: u32) -> Result<(), IpcError> {
      let word = state(&self.object, index)?;
      word.store(answer, Ordering::Release);
      #[cfg(not(windows))]
      wake::wake_one(word)?;
      #[cfg(windows)]
      self.signal_ready(index)?;
      Ok(())
    }
  }

  #[cfg(unix)]
  fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
  }

  #[cfg(not(unix))]
  fn current_uid() -> u32 {
    0
  }

  #[cfg(unix)]
  fn current_pid() -> u32 {
    rustix::process::getpid()
      .as_raw_nonzero()
      .get()
      .unsigned_abs()
  }

  #[cfg(not(unix))]
  fn current_pid() -> u32 {
    std::process::id()
  }

  /// The bootstrap object of `instance` and its handoff, refused unless a daemon has published it. Which daemon that
  /// is does not bind the claim: the daemon that answers names itself in its answer (A-143).
  fn open_bootstrap(instance: &str) -> Result<(Handoff, SharedObject), IpcError> {
    let unavailable = |why: &'static str| IpcError::DaemonUnavailable {
      endpoint: instance.to_owned(),
      why,
    };
    let handoff =
      SharedObject::handoff_for_name(&rendezvous_name(instance)).ok_or(IpcError::Unsupported {
        feature: "rendezvous by name",
      })?;
    let object = SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words())
      .map_err(|_| unavailable("the rendezvous object is not there (no daemon has created it)"))?;
    published(&object)?
      .ok_or_else(|| unavailable("the daemon has not yet published its rendezvous object"))?;
    Ok((handoff, object))
  }

  /// Claims a free slot: `FREE` → `CLAIMING`, the pid and the wanted id written, then `CLAIMED` with
  /// release, so the daemon never reads the fields before they are written. A slot the daemon reclaimed
  /// meanwhile (this claimant was stalled past the stale bound) is given up and another claimed.
  fn claim(object: &mut SharedObject, wanted: u32) -> Result<usize, IpcError> {
    for index in 0..SLOTS {
      if state(object, index)?
        .compare_exchange(FREE, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
      {
        continue;
      }
      let at = slot_at(index);
      object.write(at.saturating_add(AT_PID), &current_pid().to_le_bytes())?;
      // The id the client wants back (zero: a fresh one); the daemon answers with the id it assigns.
      object.write(at.saturating_add(AT_CLIENT), &wanted.to_le_bytes())?;
      if state(object, index)?
        .compare_exchange(CLAIMING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
      {
        return Ok(index);
      }
    }
    Err(IpcError::RingFull)
  }

  /// Takes the answered slot `index` to `DONE`: the daemon's fields, copied before, are this client's
  /// only if the slot was still its answer (the daemon reclaims an answer left past the stale bound).
  fn finish(
    object: &SharedObject,
    index: usize,
    answer: u32,
    instance: &str,
  ) -> Result<(), IpcError> {
    state(object, index)?
      .compare_exchange(answer, DONE, Ordering::AcqRel, Ordering::Acquire)
      .map(drop)
      .map_err(|_| IpcError::DaemonUnavailable {
        endpoint: instance.to_owned(),
        why: "the daemon reclaimed the claim before the client read its answer",
      })
  }

  /// Gives slot `index` back when its claim cannot be announced; only while it is still merely claimed,
  /// so a claim the daemon already took is left to its answer.
  fn give_back(object: &SharedObject, index: usize) {
    if let Ok(word) = state(object, index) {
      let _ = word.compare_exchange(CLAIMED, FREE, Ordering::AcqRel, Ordering::Acquire);
    }
  }

  /// A claim in flight (AUD-29-19): the slot this client took and announced, its answer not yet read.
  pub struct Claim {
    instance: String,
    object: SharedObject,
    index: usize,
    /// The daemon-wide doorbell, rung at the claim and kept as the client's doorbell once connected;
    /// taken when the claim completes.
    bell: Option<Bell>,
    /// The Event the daemon signals when it answers this slot (Windows; the word wake is process-local
    /// there). The daemon holds every slot's Event from its start, so a signal raised before a wait is
    /// kept until the wait consumes it.
    #[cfg(windows)]
    ready_event: crate::wake::Event,
    started: std::time::Instant,
    /// Whether the deadline was already extended once for a claim the daemon took to answer.
    extended: bool,
    /// The answer's deadline from `started`: one claim wait, extended by one more when the daemon has
    /// taken the claim to answer it.
    wait_ns: u64,
    /// Whether the slot is still this client's to give back (cleared once answered or refused).
    open: bool,
  }

  /// Claims a slot and rings the daemon-wide doorbell so a parked control shard sees the claim; nothing
  /// waits.
  pub(super) fn begin(
    instance: &str,
    wanted: u32,
    restart_wait_ns: u64,
  ) -> Result<Claim, IpcError> {
    let (handoff, mut object) = open_bootstrap(instance)?;
    // A live holding anchor means the daemon may be restarting: the claim waits out the restart (A-114).
    let anchor = read_u32(&object, AT_ANCHOR_PID)?;
    let wait_ns = if anchor != 0
      && crate::exit_watch::ExitWatch::on(anchor).is_ok_and(|watch| !watch.exited())
    {
      restart_wait_ns
    } else {
      CLAIM_WAIT_NS
    };
    let index = claim(&mut object, wanted)?;
    #[cfg(windows)]
    let ready_event = match crate::wake::Event::open(&ready_event_name(instance, index)) {
      Ok(event) => event,
      Err(error) => {
        give_back(&object, index);
        return Err(error);
      }
    };
    // The client's own doorbell for later rings is this bell, over its own mapping of the bootstrap
    // object.
    let bell = SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words())
      .map_err(IpcError::from)
      .and_then(|mapping| Bell::new(mapping, instance));
    let bell = match bell.and_then(|bell| bell.ring().map(|()| bell)) {
      Ok(bell) => bell,
      Err(error) => {
        // The claim cannot be announced: give the slot back rather than leave it claimed.
        give_back(&object, index);
        return Err(error);
      }
    };
    Ok(Claim {
      instance: instance.to_owned(),
      object,
      index,
      bell: Some(bell),
      #[cfg(windows)]
      ready_event,
      started: std::time::Instant::now(),
      extended: false,
      wait_ns,
      open: true,
    })
  }

  impl Claim {
    /// The answer, without waiting: the slot `READY` or `REFUSED` is read and finished; past the claim
    /// wait the claim is taken back by CAS — only if the daemon has not taken it to answer, whose answer
    /// is then awaited one more claim wait.
    pub(super) fn poll(&mut self) -> Result<Option<Connected>, IpcError> {
      if !self.open {
        return Err(IpcError::DaemonUnavailable {
          endpoint: self.instance.clone(),
          why: "the claim was already answered or refused",
        });
      }
      let word = state(&self.object, self.index)?;
      let now = word.load(Ordering::Acquire);
      if now == READY || now == REFUSED {
        self.open = false;
        return self.complete(now).map(Some);
      }
      let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
      if elapsed < self.wait_ns {
        return Ok(None);
      }
      let took_back = word
        .compare_exchange(CLAIMED, FREE, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
      if took_back || self.extended {
        // Taken back, or the daemon's answer is a whole claim wait late: its slot is left to the
        // daemon's stale reclaim.
        self.open = false;
        return Err(IpcError::DaemonUnavailable {
          endpoint: self.instance.clone(),
          why: "the daemon did not answer the claim within the claim wait",
        });
      }
      // The daemon took the claim to answer it: its answer is due; wait one more claim wait for it.
      self.extended = true;
      self.wait_ns = self.wait_ns.saturating_add(CLAIM_WAIT_NS);
      Ok(None)
    }

    /// Waits for the answer between polls on the slot's state word (Windows: the slot's Event).
    pub(super) fn wait(mut self) -> Result<Connected, IpcError> {
      loop {
        if let Some(connected) = self.poll()? {
          return Ok(connected);
        }
        let word = state(&self.object, self.index)?;
        let now = word.load(Ordering::Acquire);
        if now == READY || now == REFUSED {
          continue;
        }
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        #[cfg(not(windows))]
        let _ = wake::wait(word, now, Some(self.wait_ns.saturating_sub(elapsed)))?;
        #[cfg(windows)]
        let _ = self
          .ready_event
          .wait(Some(self.wait_ns.saturating_sub(elapsed)))?;
      }
    }

    /// Reads an answered slot: the daemon's typed refusal, or the region it made, the slot then `DONE`.
    fn complete(&mut self, answer: u32) -> Result<Connected, IpcError> {
      let at = slot_at(self.index);
      if answer == REFUSED {
        // The daemon's typed refusal: the bound when that was the reason, else none.
        let limit = read_u64(&self.object, at.saturating_add(AT_LEN))?;
        finish(&self.object, self.index, REFUSED, &self.instance)?;
        return Err(super::refusal_of(
          usize::try_from(limit).unwrap_or(usize::MAX),
          &self.instance,
        ));
      }
      let (name, len) = read_answer(&self.object, at)?;
      // Read before the slot is finished, refused after: an answer that names no daemon still frees its slot.
      let answerer = read_answerer(&self.object, at);
      finish(&self.object, self.index, READY, &self.instance)?;
      let (stamp, daemon_pid) = answerer?;
      let region = ClientRegion::open(&Handoff::Name(name), len)?;
      // The daemon that answered was alive: watch its process from here, and judge it by its own stamp (A-143). The
      // header names whichever daemon published last; a claim begun before a restart is answered by the next daemon
      // (A-114), and one answered just before a restart is read after it.
      let watch = crate::exit_watch::ExitWatch::on(daemon_pid)?;
      let bell = self.bell.take().ok_or(IpcError::Layout {
        reason: "the claim's doorbell was already taken",
      })?;
      Ok(Connected {
        region,
        doorbell: Doorbell::Word(bell),
        liveness: super::Liveness {
          inner: Liveness {
            instance: self.instance.clone(),
            generation: stamp,
            watch,
          },
        },
        control: Some(ClientControl),
      })
    }
  }

  impl Drop for Claim {
    /// A claim dropped unanswered (a cancelled connect) gives its slot back, only while it is still
    /// merely claimed, so one the daemon already took is left to its answer and the stale reclaim.
    fn drop(&mut self) {
      if self.open {
        give_back(&self.object, self.index);
      }
    }
  }

  #[cfg(test)]
  mod tests {
    use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};

    use super::*;
    use crate::region::RegionGeometry;

    fn instance(tag: &str) -> String {
      format!("rdv-{tag}-{}", std::process::id())
    }

    /// Shape: a small region, enough for a claim to be answered with one.
    fn geometry() -> RegionGeometry {
      RegionGeometry {
        slots: 4,
        spin_ns: 1_000,
        spin_shift: 3,
        bulk_bytes: 4096,
        page: 4096,
      }
    }

    /// The daemon's side of one accept: the wanted id granted, a real region made for it.
    fn accept(listener: &mut Listener, tag: &str) -> Result<Option<Accepted>, IpcError> {
      let tag = tag.to_owned();
      let served = listener.accept_one(&mut |wanted| wanted.max(1), &mut |id| {
        Ok(Prepared {
          region: ClientRegion::create(
            &format!("rdvr-{tag}-{id}-{}", std::process::id()),
            id,
            0,
            geometry(),
          )?,
          kick_fd: None,
        })
      })?;
      match served {
        Some(Served::Accepted(accepted)) => Ok(Some(*accepted)),
        Some(Served::Refused(refusal)) => Err(refusal),
        None => Ok(None),
      }
    }

    /// A client's own mapping of `name`'s bootstrap object.
    fn client_mapping(name: &str) -> SharedObject {
      let handoff = SharedObject::handoff_for_name(&rendezvous_name(name)).unwrap();
      SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words()).unwrap()
    }

    /// AUD-29-09 (the claim protocol). Do: a client takes slot 0 `CLAIMING` and has not written its
    /// fields; the daemon accepts; the client writes its wanted id and publishes `CLAIMED`; the daemon
    /// accepts again. Expect: nothing answered while the fields were unwritten (before 2026-09-30 a claim
    /// was visible before its fields, and the daemon could read them unwritten); the published claim
    /// answered `READY` with the id it wanted.
    #[test]
    fn a_claim_is_answered_only_once_its_fields_are_published() {
      let name = instance("claiming");
      let mut listener = Listener::open(&name, None).unwrap();
      let mut client = client_mapping(&name);
      assert!(
        state(&client, 0)
          .unwrap()
          .compare_exchange(FREE, CLAIMING, AcqRel, Acquire)
          .is_ok()
      );
      assert!(
        accept(&mut listener, "a").unwrap().is_none(),
        "a claim still being written is not answered"
      );
      client
        .write(slot_at(0) + AT_CLIENT, &7u32.to_le_bytes())
        .unwrap();
      client
        .write(slot_at(0) + AT_PID, &current_pid().to_le_bytes())
        .unwrap();
      state(&client, 0).unwrap().store(CLAIMED, Release);
      let accepted = accept(&mut listener, "b")
        .unwrap()
        .expect("the published claim is answered");
      assert_eq!(accepted.client_id, 7);
      assert_eq!(state(&client, 0).unwrap().load(Acquire), READY);
    }

    /// AUD-29-09 (a dead claimant). Do: leave slot 0 `CLAIMING`, as a client killed mid-claim would;
    /// accept once, again past the stale bound. Expect: the slot kept on the first pass (a live claimant
    /// could still publish), taken back `FREE` on the second, so a dead claimant never strands it.
    #[test]
    fn a_claim_abandoned_mid_write_is_reclaimed_past_the_stale_bound() {
      let name = instance("stale");
      let mut listener = Listener::open(&name, None).unwrap();
      let client = client_mapping(&name);
      state(&client, 0).unwrap().store(CLAIMING, Release);
      assert!(accept(&mut listener, "c").unwrap().is_none());
      assert_eq!(state(&client, 0).unwrap().load(Acquire), CLAIMING);
      #[allow(clippy::disallowed_methods)] // a test waiting out the stale bound it checks
      std::thread::sleep(std::time::Duration::from_nanos(
        STALE_SLOT_NS + CLAIM_WAIT_NS / 10,
      ));
      assert!(accept(&mut listener, "d").unwrap().is_none());
      assert_eq!(state(&client, 0).unwrap().load(Acquire), FREE);
    }

    /// AUD-29-19 (a cancelled connect). Do: begin a claim on a daemon end that has not accepted it, then
    /// drop the claim unanswered. Expect: the slot `CLAIMED` while the claim lives and `FREE` once it is
    /// dropped, so a cancelled connect never strands a claim slot until the stale reclaim.
    #[test]
    fn a_claim_dropped_unanswered_gives_its_slot_back() {
      let name = instance("dropped");
      let _listener = Listener::open(&name, None).unwrap();
      let client = client_mapping(&name);
      let claim = begin(&name, 0, CLAIM_WAIT_NS).unwrap();
      let index = claim.index;
      assert_eq!(state(&client, index).unwrap().load(Acquire), CLAIMED);
      drop(claim);
      assert_eq!(state(&client, index).unwrap().load(Acquire), FREE);
    }

    /// AUD-29-09 (header publication). Do: create the bootstrap object with its header written but the
    /// start stamp not yet published, and connect. Expect: `DaemonUnavailable` naming the unpublished
    /// object, never a torn header read as a daemon.
    #[test]
    fn a_client_never_reads_an_unpublished_bootstrap_header() {
      let name = instance("unpublished");
      let mut object =
        SharedObject::create(&rendezvous_name(&name), OBJECT_BYTES, bootstrap_words()).unwrap();
      object.write(0, &MAGIC.to_le_bytes()).unwrap();
      assert!(matches!(
        begin(&name, 0, CLAIM_WAIT_NS).map(drop),
        Err(IpcError::DaemonUnavailable {
          why: "the daemon has not yet published its rendezvous object",
          ..
        })
      ));
    }
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub mod platform {
  //! Other platforms: no rendezvous yet.

  use super::{Accepted, Connected, Prepared, Served};
  use crate::error::IpcError;
  use crate::region::ClientRegion;

  /// No doorbell waiter on this platform: an uninhabited type, so no value of it exists.
  pub enum DoorbellWaiter {}

  impl DoorbellWaiter {
    pub(super) fn current(&self) -> Result<u32, IpcError> {
      match *self {}
    }

    pub(super) fn wait(&self, _seen: u32, _timeout_ns: u64) -> Result<u32, IpcError> {
      match *self {}
    }

    pub(super) fn ring(&self) -> Result<(), IpcError> {
      match *self {}
    }
  }

  /// No control channel.
  pub struct Control;
  /// No client control channel.
  pub struct ClientControl;

  #[cfg(unix)]
  impl Control {
    /// No completion fd on this platform.
    pub fn completion_dup(&self) -> Option<std::os::fd::OwnedFd> {
      None
    }
  }

  #[cfg(unix)]
  impl ClientControl {
    /// No completion fd on this platform.
    pub fn into_completion(self) -> Option<std::os::fd::OwnedFd> {
      None
    }
  }

  /// No liveness check: no daemon to connect to.
  pub struct Liveness;

  impl Liveness {
    pub(super) fn daemon_gone(&self) -> bool {
      true
    }
  }

  pub(super) struct Listener;

  impl Listener {
    pub(super) fn open(_instance: &str, _adoption: Option<&str>) -> Result<Listener, IpcError> {
      Err(IpcError::Unsupported {
        feature: "rendezvous",
      })
    }

    pub(super) fn doorbell(&self) -> Option<&std::sync::atomic::AtomicU32> {
      None
    }

    pub(super) fn doorbell_waiter(&self) -> Result<Option<DoorbellWaiter>, IpcError> {
      Ok(None)
    }

    pub(super) fn raw_fd(&self) -> Option<i32> {
      None
    }

    pub(super) fn accept_one(
      &mut self,
      _assign: &mut dyn FnMut(u32) -> u32,
      _make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Option<Served>, IpcError> {
      let _ = ClientRegion::open;
      Ok(None)
    }
  }

  /// No claim on this platform: an uninhabited type, so no value of it exists.
  pub enum Claim {}

  impl Claim {
    pub(super) fn poll(&mut self) -> Result<Option<Connected>, IpcError> {
      match *self {}
    }

    pub(super) fn wait(self) -> Result<Connected, IpcError> {
      match self {}
    }
  }

  pub(super) fn begin(instance: &str, _wanted: u32, _wait_ns: u64) -> Result<Claim, IpcError> {
    Err(IpcError::DaemonUnavailable {
      endpoint: instance.to_owned(),
      why: "no rendezvous exists on this platform",
    })
  }
}
