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
//!   authentication); the named Event per client arrives with the Windows bridge (Phase 4).
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
  /// The peer's process id (the liveness probe's input where no socket closes).
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

impl Listener {
  /// Opens the daemon's end of the rendezvous for `instance`.
  pub fn open(instance: &str) -> Result<Listener, IpcError> {
    Ok(Listener {
      inner: platform::Listener::open(instance)?,
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
  /// and the handoff is completed. A refused peer is counted and skipped.
  pub fn accept_pending(
    &mut self,
    in_use: &dyn Fn(u32) -> bool,
    make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
  ) -> Result<Vec<Accepted>, IpcError> {
    let mut out = Vec::new();
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
        Ok(Some(accepted)) => {
          out.push(accepted);
        }
        Ok(None) => break,
        Err(IpcError::PeerRefused { .. }) => self.refused += 1,
        // Answered typed to the client by the platform's `accept_one` (the bound in the refusal),
        // counted here, and the next pending connection is served: a full daemon keeps answering.
        Err(IpcError::TooManyClients { .. }) => self.capacity_refused += 1,
        Err(e) => return Err(e),
      }
    }
    Ok(out)
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
pub fn connect(instance: &str) -> Result<Connected, IpcError> {
  connect_as(instance, 0)
}

/// Connects asking for a client id it held before (a reconnect after the daemon restarted, so
/// its retries under the old request ids meet their completion records, §4.9); the daemon
/// honours the id when no live client holds it, else assigns a fresh one. The blocking facade over
/// [`begin_connect_as`]: the claim, then a wait for its answer of at most the claim wait (twice, when
/// the daemon took the claim to answer it).
pub fn connect_as(instance: &str, wanted: u32) -> Result<Connected, IpcError> {
  begin_connect_as(instance, wanted)?.wait()
}

/// Starts a connect without waiting (AUD-29-19, §4.7 "Rendezvous"): the claim is made and announced to
/// the daemon, and [`Claim::poll`] reads its answer whenever the caller's event loop looks. Nothing here
/// waits on the daemon, so an event loop that drives the claim is never held by a slow, stopped or dead
/// one.
pub fn begin_connect_as(instance: &str, wanted: u32) -> Result<Claim, IpcError> {
  Ok(Claim {
    inner: platform::begin(instance, wanted)?,
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

impl Liveness {
  /// Whether the daemon the client connected to is gone (dead, or restarted since). A cold
  /// path: asked only after a reply has stalled past the client's deadline.
  pub fn daemon_gone(&self) -> bool {
    self.inner.daemon_gone()
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
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType, sockopt,
  };
  use slates_mem::Handoff;

  use super::{Accepted, CLAIM_WAIT_NS, Connected, Doorbell, Prepared, rendezvous_name};
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
  /// was refused at (`IpcError::TooManyClients`), or [`NO_BOUND`] for a region refused for another
  /// reason.
  const REFUSED_CLIENT: u32 = 0;

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

  impl Listener {
    pub(super) fn open(instance: &str) -> Result<Listener, IpcError> {
      let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
      )
      .map_err(|e| refused("socket", e))?;
      rustix::net::bind(&socket, &address(instance)?).map_err(|e| refused("bind", e))?;
      rustix::net::listen(&socket, BACKLOG).map_err(|e| refused("listen", e))?;
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

    pub(super) fn accept_one(
      &mut self,
      assign: &mut dyn FnMut(u32) -> u32,
      make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Option<Accepted>, IpcError> {
      let peer = match rustix::net::accept_with(&self.socket, SocketFlags::CLOEXEC) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::AGAIN) => return Ok(None),
        Err(e) => return Err(refused("accept", e)),
      };
      let cred = sockopt::socket_peercred(&peer).map_err(|e| refused("SO_PEERCRED", e))?;
      let uid = cred.uid.as_raw();
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
      Ok(Some(Accepted {
        client_id,
        uid,
        pid: u32::try_from(cred.pid.as_raw_nonzero().get()).unwrap_or(0),
        region,
        control: Some(Control {
          socket: peer,
          completion,
        }),
      }))
    }
  }

  /// A claim in flight (AUD-29-19): the connected, non-blocking socket with the hello sent, the
  /// handoff not yet received.
  pub struct Claim {
    instance: String,
    /// The socket; taken once the claim is answered or refused.
    socket: Option<OwnedFd>,
    started: std::time::Instant,
  }

  /// Connects a non-blocking socket to the daemon's rendezvous and sends the hello; nothing waits (a
  /// stream connect over `AF_UNIX` completes at once or is refused when the daemon's backlog is full).
  pub(super) fn begin(instance: &str, wanted: u32) -> Result<Claim, IpcError> {
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
    // A fresh socket's send buffer takes the four-byte hello whole.
    rustix::net::send(&socket, &wanted.to_le_bytes(), SendFlags::empty())
      .map_err(|e| refused("send", e))?;
    Ok(Claim {
      instance: instance.to_owned(),
      socket: Some(socket),
      started: std::time::Instant::now(),
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
          if elapsed < CLAIM_WAIT_NS {
            return Ok(None);
          }
          self.socket = None;
          Err(IpcError::DaemonUnavailable {
            endpoint: self.instance.clone(),
            why: "the daemon did not answer the claim within the claim wait",
          })
        }
        Err(rustix::io::Errno::INTR) => Ok(None),
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
        let remaining = CLAIM_WAIT_NS.saturating_sub(elapsed);
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
}

#[cfg(any(target_os = "macos", windows))]
pub mod platform {
  //! macOS and Windows: the bootstrap object with claim slots.

  use std::sync::atomic::{AtomicU32, Ordering};

  use slates_mem::{Handoff, SharedObject, Width, WordRun, Words};

  use super::{Accepted, CLAIM_WAIT_NS, Connected, Doorbell, Prepared, rendezvous_name};
  use crate::error::IpcError;
  use crate::region::ClientRegion;
  #[cfg(not(windows))]
  use crate::wake;

  /// Format: the bootstrap object's magic, `SLBT` in little-endian ASCII.
  const MAGIC: u32 = 0x5442_4C53;
  /// Format: the bootstrap header: magic (4), slots (4), the daemon-wide doorbell word (4),
  /// padding (4), the daemon's start stamp (8), the daemon's process id (4), padding to a cache line.
  const HEADER_BYTES: usize = 64;
  /// Format: the slot count's offset in the header (after the magic).
  const AT_SLOT_COUNT: usize = 4;
  /// Format: the doorbell word's offset in the header.
  pub const AT_DOORBELL: usize = 8;
  /// Format: the start stamp's offset in the header: the wall clock in nanoseconds when the
  /// daemon opened the object, so a client that remembers it tells a restarted daemon (a new
  /// stamp) from a slow one (the same stamp).
  const AT_GENERATION: usize = 16;
  /// Format: the daemon's process id's offset in the header, written before the start stamp publishes
  /// the header: the process a client's exit watch is taken on (`crate::exit_watch`, AUD-29-20).
  const AT_DAEMON_PID: usize = 24;
  /// Shape: claim slots in the bootstrap object: clients connecting inside one control-shard
  /// loop; the loop drains them, so the table only covers one loop of arrivals.
  const SLOTS: usize = 64;
  /// Format: a claim slot: state (4), client pid (4), client id (4), padding (4), region
  /// length (8), region name (40), padding to two cache lines.
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
  }

  #[cfg(unix)]
  impl ClientControl {
    /// No completion fd on macOS yet (the control socket is owed); an async SDK falls back to polling.
    pub fn into_completion(self) -> Option<std::os::fd::OwnedFd> {
      None
    }
  }

  /// The client's liveness check: the daemon's process exited (its exit watch, taken when the claim was
  /// answered), or the start stamp it saw differs from the one the bootstrap object holds now (none when
  /// no daemon holds the object). The stamp alone cannot see a killed daemon nothing restarted, since the
  /// object outlives it (AUD-29-20).
  pub struct Liveness {
    instance: String,
    generation: u64,
    watch: crate::exit_watch::ExitWatch,
  }

  impl Liveness {
    pub(super) fn daemon_gone(&self) -> bool {
      self.watch.exited() || generation_of(&self.instance).is_none_or(|now| now != self.generation)
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

  impl Listener {
    pub(super) fn open(instance: &str) -> Result<Listener, IpcError> {
      let mut object =
        SharedObject::create(&rendezvous_name(instance), OBJECT_BYTES, bootstrap_words())?;
      object.write(0, &MAGIC.to_le_bytes())?;
      object.write(
        AT_SLOT_COUNT,
        &u32::try_from(SLOTS).unwrap_or(u32::MAX).to_le_bytes(),
      )?;
      for i in 0..SLOTS {
        state(&object, i)?.store(FREE, Ordering::Release);
      }
      object.write(AT_DAEMON_PID, &current_pid().to_le_bytes())?;
      // The start stamp last, with release: it publishes the header and the free table (never zero).
      object
        .atomic_u64(AT_GENERATION)?
        .store(stamp_now().max(1), Ordering::Release);
      Ok(Listener {
        object,
        seen: vec![None; SLOTS],
        #[cfg(windows)]
        ready_events: (0..SLOTS)
          .map(|index| crate::wake::Event::open(&ready_event_name(instance, index)))
          .collect::<Result<Vec<_>, _>>()?,
        #[cfg(windows)]
        _doorbell_event: crate::wake::Event::open(&doorbell_event_name(instance))?,
        #[cfg(windows)]
        instance: instance.to_owned(),
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
    ) -> Result<Option<Accepted>, IpcError> {
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
              // claimant's refusal, never a slot left `ANSWERING` for the claimant to wait out.
              if let Err(error) = &answered
                && state(&self.object, i)?.load(Ordering::Acquire) == ANSWERING
              {
                self.refuse(i, error)?;
              }
              return answered.map(Some);
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

  /// The bootstrap object of `instance`, its handoff, and its published start stamp.
  fn open_bootstrap(instance: &str) -> Result<(Handoff, SharedObject, u64), IpcError> {
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
    let generation = published(&object)?
      .ok_or_else(|| unavailable("the daemon has not yet published its rendezvous object"))?;
    Ok((handoff, object, generation))
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
    generation: u64,
    /// The daemon-wide doorbell, rung at the claim and kept as the client's doorbell once connected;
    /// taken when the claim completes.
    bell: Option<Bell>,
    /// The Event the daemon signals when it answers this slot (Windows; the word wake is process-local
    /// there). The daemon holds every slot's Event from its start, so a signal raised before a wait is
    /// kept until the wait consumes it.
    #[cfg(windows)]
    ready_event: crate::wake::Event,
    started: std::time::Instant,
    /// The answer's deadline from `started`: one claim wait, extended by one more when the daemon has
    /// taken the claim to answer it.
    wait_ns: u64,
    /// Whether the slot is still this client's to give back (cleared once answered or refused).
    open: bool,
  }

  /// Claims a slot and rings the daemon-wide doorbell so a parked control shard sees the claim; nothing
  /// waits.
  pub(super) fn begin(instance: &str, wanted: u32) -> Result<Claim, IpcError> {
    let (handoff, mut object, generation) = open_bootstrap(instance)?;
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
      generation,
      bell: Some(bell),
      #[cfg(windows)]
      ready_event,
      started: std::time::Instant::now(),
      wait_ns: CLAIM_WAIT_NS,
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
      if took_back || self.wait_ns > CLAIM_WAIT_NS {
        // Taken back, or the daemon's answer is a whole claim wait late: its slot is left to the
        // daemon's stale reclaim.
        self.open = false;
        return Err(IpcError::DaemonUnavailable {
          endpoint: self.instance.clone(),
          why: "the daemon did not answer the claim within the claim wait",
        });
      }
      // The daemon took the claim to answer it: its answer is due; wait one more claim wait for it.
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
      finish(&self.object, self.index, READY, &self.instance)?;
      let region = ClientRegion::open(&Handoff::Name(name), len)?;
      // The daemon answered, so it was alive: watch its process from here.
      let watch = crate::exit_watch::ExitWatch::on(read_u32(&self.object, AT_DAEMON_PID)?)?;
      let bell = self.bell.take().ok_or(IpcError::Layout {
        reason: "the claim's doorbell was already taken",
      })?;
      Ok(Connected {
        region,
        doorbell: Doorbell::Word(bell),
        liveness: super::Liveness {
          inner: Liveness {
            instance: self.instance.clone(),
            generation: self.generation,
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
      listener.accept_one(&mut |wanted| wanted.max(1), &mut |id| {
        Ok(Prepared {
          region: ClientRegion::create(
            &format!("rdvr-{tag}-{id}-{}", std::process::id()),
            id,
            0,
            geometry(),
          )?,
          kick_fd: None,
        })
      })
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
      let mut listener = Listener::open(&name).unwrap();
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
      let mut listener = Listener::open(&name).unwrap();
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
      let _listener = Listener::open(&name).unwrap();
      let client = client_mapping(&name);
      let claim = begin(&name, 0).unwrap();
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
        begin(&name, 0).map(drop),
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

  use super::{Accepted, Connected, Prepared};
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
    pub(super) fn open(_instance: &str) -> Result<Listener, IpcError> {
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
    ) -> Result<Option<Accepted>, IpcError> {
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

  pub(super) fn begin(instance: &str, _wanted: u32) -> Result<Claim, IpcError> {
    Err(IpcError::DaemonUnavailable {
      endpoint: instance.to_owned(),
      why: "no rendezvous exists on this platform",
    })
  }
}
