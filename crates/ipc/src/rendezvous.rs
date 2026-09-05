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

/// A client the daemon accepted: its id, its uid, and the region the daemon keeps.
pub struct Accepted {
  /// The client id the daemon assigned.
  pub client_id: u32,
  /// The peer's uid.
  pub uid: u32,
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

/// What the daemon opens once: the listener (Linux) or the bootstrap object (macOS, Windows).
pub struct Listener {
  inner: platform::Listener,
  next_client: u32,
  /// Cross-uid connects refused (the audit counter of §4.13).
  refused: u64,
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
    })
  }

  /// Serves every pending connection without blocking: for each, `make_region` builds the
  /// client's region (the daemon's derivation of its geometry and shard), and the handoff is
  /// completed. A refused peer is counted and skipped.
  pub fn accept_pending(
    &mut self,
    make_region: &mut dyn FnMut(u32) -> Result<ClientRegion, IpcError>,
  ) -> Result<Vec<Accepted>, IpcError> {
    let mut out = Vec::new();
    loop {
      let client_id = self.next_client;
      match self.inner.accept_one(client_id, make_region) {
        Ok(Some(accepted)) => {
          self.next_client = self.next_client.wrapping_add(1);
          out.push(accepted);
        }
        Ok(None) => break,
        Err(IpcError::PeerRefused { .. }) => self.refused += 1,
        Err(e) => return Err(e),
      }
    }
    Ok(out)
  }

  /// Cross-uid connects refused so far.
  pub fn refused(&self) -> u64 {
    self.refused
  }
}

/// The client's side: connects to `instance` and returns its region.
pub fn connect(
  instance: &str,
) -> Result<(ClientRegion, Option<platform::ClientControl>), IpcError> {
  platform::connect(instance)
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

  use super::{Accepted, rendezvous_name};
  use crate::error::IpcError;
  use crate::region::ClientRegion;

  /// Format: the handoff message: client id (4), region length (8).
  const HANDOFF_BYTES: usize = 12;
  /// Format: the client id's offset in the handoff message.
  const HANDOFF_AT_CLIENT: usize = 0;
  /// Format: the region length's offset in the handoff message.
  const HANDOFF_AT_LEN: usize = 4;
  /// Format: descriptors in the handoff: the region and the completion eventfd.
  const HANDOFF_FDS: usize = 2;
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

  /// The client's control channel: the socket and the completion eventfd to poll.
  pub struct ClientControl {
    /// The socket.
    pub socket: OwnedFd,
    /// The completion eventfd.
    pub completion: OwnedFd,
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

    pub(super) fn accept_one(
      &mut self,
      client_id: u32,
      make_region: &mut dyn FnMut(u32) -> Result<ClientRegion, IpcError>,
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
      let region = make_region(client_id)?;
      let (handoff, len) = region.handoff()?;
      let Handoff::Descriptor(raw) = handoff else {
        return Err(IpcError::Layout {
          reason: "a Linux region hands off a descriptor",
        });
      };
      // SAFETY: the number is the duplicate `handoff` created for a child to inherit; this
      // process owns it and closes it after the send.
      let region_fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
      let completion = rustix::event::eventfd(
        0,
        rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
      )
      .map_err(|e| refused("eventfd", e))?;
      let mut body = [0u8; HANDOFF_BYTES];
      body[HANDOFF_AT_CLIENT..HANDOFF_AT_LEN].copy_from_slice(&client_id.to_le_bytes());
      body[HANDOFF_AT_LEN..].copy_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
      let fds = [region_fd.as_fd(), completion.as_fd()];
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
      .map_err(|e| refused("sendmsg", e))?;
      Ok(Some(Accepted {
        client_id,
        uid,
        region,
        control: Some(Control {
          socket: peer,
          completion,
        }),
      }))
    }
  }

  pub(super) fn connect(instance: &str) -> Result<(ClientRegion, Option<ClientControl>), IpcError> {
    let socket = rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .map_err(|e| refused("socket", e))?;
    rustix::net::connect(&socket, &address(instance)?).map_err(|_| {
      IpcError::DaemonUnavailable {
        endpoint: instance.to_owned(),
      }
    })?;
    let mut body = [0u8; HANDOFF_BYTES];
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HANDOFF_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = rustix::net::recvmsg(
      &socket,
      &mut [IoSliceMut::new(&mut body)],
      &mut control,
      RecvFlags::CMSG_CLOEXEC,
    )
    .map_err(|e| refused("recvmsg", e))?;
    if received.bytes != HANDOFF_BYTES {
      return Err(IpcError::Layout {
        reason: "short handoff message",
      });
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
    Ok((region, Some(ClientControl { socket, completion })))
  }
}

#[cfg(any(target_os = "macos", windows))]
pub mod platform {
  //! macOS and Windows: the bootstrap object with claim slots.

  use std::sync::atomic::{AtomicU32, Ordering};

  use slates_mem::{Handoff, SharedObject};

  use super::{Accepted, rendezvous_name};
  use crate::error::IpcError;
  use crate::region::ClientRegion;
  use crate::wake;

  /// Format: the bootstrap object's magic, `SLBT` in little-endian ASCII.
  const MAGIC: u32 = 0x5442_4C53;
  /// Format: the bootstrap header: magic (4), slots (4), padding to a cache line.
  const HEADER_BYTES: usize = 64;
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
  /// Format: the client opened the region; the daemon reclaims the slot.
  const DONE: u32 = 3;
  /// Shape: how long a client waits for the daemon to answer a claim before reporting it
  /// unavailable (nanoseconds): the control shard's loop is microseconds, so a second is a
  /// dead daemon.
  const CLAIM_WAIT_NS: u64 = 1_000_000_000;

  /// No control channel on these platforms in Phase 2 (the completion fd for SDK event loops
  /// arrives with Phase 5's optional control socket).
  pub struct Control;

  /// No client control channel on these platforms in Phase 2.
  pub struct ClientControl;

  pub(super) struct Listener {
    object: SharedObject,
  }

  fn slot_at(index: usize) -> usize {
    HEADER_BYTES + index * SLOT_BYTES
  }

  fn state(object: &SharedObject, index: usize) -> Result<&AtomicU32, IpcError> {
    Ok(object.atomic_u32(slot_at(index) + AT_STATE)?)
  }

  impl Listener {
    pub(super) fn open(instance: &str) -> Result<Listener, IpcError> {
      let mut object = SharedObject::create(
        &rendezvous_name(instance),
        HEADER_BYTES + SLOTS * SLOT_BYTES,
      )?;
      {
        let bytes = object.bytes_mut();
        bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&u32::try_from(SLOTS).unwrap_or(u32::MAX).to_le_bytes());
      }
      for i in 0..SLOTS {
        state(&object, i)?.store(FREE, Ordering::Release);
      }
      Ok(Listener { object })
    }

    pub(super) fn accept_one(
      &mut self,
      client_id: u32,
      make_region: &mut dyn FnMut(u32) -> Result<ClientRegion, IpcError>,
    ) -> Result<Option<Accepted>, IpcError> {
      for i in 0..SLOTS {
        let word = state(&self.object, i)?;
        match word.load(Ordering::Acquire) {
          DONE => word.store(FREE, Ordering::Release),
          CLAIMED => {
            let region = make_region(client_id)?;
            let (handoff, len) = region.handoff()?;
            let Handoff::Name(name) = handoff else {
              return Err(IpcError::Layout {
                reason: "a region here hands off a name",
              });
            };
            if name.len() > NAME_BYTES {
              return Err(IpcError::Layout {
                reason: "region name longer than the slot holds",
              });
            }
            let at = slot_at(i);
            let uid = {
              let bytes = self.object.bytes();
              u32::from_le_bytes([
                bytes[at + AT_PID],
                bytes[at + AT_PID + 1],
                bytes[at + AT_PID + 2],
                bytes[at + AT_PID + 3],
              ])
            };
            {
              let bytes = self.object.bytes_mut();
              bytes[at + AT_CLIENT..at + AT_CLIENT + 4].copy_from_slice(&client_id.to_le_bytes());
              bytes[at + AT_LEN..at + AT_LEN + 8]
                .copy_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
              bytes[at + AT_NAME..at + AT_NAME + NAME_BYTES].fill(0);
              bytes[at + AT_NAME..at + AT_NAME + name.len()].copy_from_slice(name.as_bytes());
            }
            let word = state(&self.object, i)?;
            word.store(READY, Ordering::Release);
            wake::wake_one(word)?;
            let _ = uid;
            return Ok(Some(Accepted {
              client_id,
              // The object's mode and per-user name are the authentication: whoever opened
              // it is the daemon's user (the kernel refused everyone else).
              uid: current_uid(),
              region,
              control: Some(Control),
            }));
          }
          _ => {}
        }
      }
      Ok(None)
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

  pub(super) fn connect(instance: &str) -> Result<(ClientRegion, Option<ClientControl>), IpcError> {
    let handoff =
      SharedObject::handoff_for_name(&rendezvous_name(instance)).ok_or(IpcError::Unsupported {
        feature: "rendezvous by name",
      })?;
    let object = SharedObject::open(&handoff, HEADER_BYTES + SLOTS * SLOT_BYTES).map_err(|_| {
      IpcError::DaemonUnavailable {
        endpoint: instance.to_owned(),
      }
    })?;
    if u32::from_le_bytes([
      object.bytes()[0],
      object.bytes()[1],
      object.bytes()[2],
      object.bytes()[3],
    ]) != MAGIC
    {
      return Err(IpcError::Layout {
        reason: "bootstrap object has the wrong magic",
      });
    }
    // Claim a free slot.
    let mut claimed = None;
    for i in 0..SLOTS {
      let word = state(&object, i)?;
      if word
        .compare_exchange(FREE, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
      {
        claimed = Some(i);
        break;
      }
    }
    let Some(index) = claimed else {
      return Err(IpcError::RingFull);
    };
    let at = slot_at(index);
    // The pid is written after the claim; a daemon reading the claim reads it too. The
    // object is ours alone to write at this slot now.
    {
      let mut object = object;
      object.bytes_mut()[at + AT_PID..at + AT_PID + 4]
        .copy_from_slice(&current_pid().to_le_bytes());
      let word = state(&object, index)?;
      // Wait for READY (spin then wait on the word).
      let started = std::time::Instant::now();
      loop {
        let now = word.load(Ordering::Acquire);
        if now == READY {
          break;
        }
        let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if elapsed >= CLAIM_WAIT_NS {
          word.store(FREE, Ordering::Release);
          return Err(IpcError::DaemonUnavailable {
            endpoint: instance.to_owned(),
          });
        }
        let _ = wake::wait(word, now, Some(CLAIM_WAIT_NS - elapsed))?;
      }
      let (name, len) = {
        let bytes = object.bytes();
        let raw = &bytes[at + AT_NAME..at + AT_NAME + NAME_BYTES];
        let end = raw.iter().position(|b| *b == 0).unwrap_or(NAME_BYTES);
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();
        let len = u64::from_le_bytes(
          bytes[at + AT_LEN..at + AT_LEN + 8]
            .try_into()
            .unwrap_or([0; 8]),
        );
        (name, usize::try_from(len).unwrap_or(0))
      };
      let region = ClientRegion::open(&Handoff::Name(name), len)?;
      state(&object, index)?.store(DONE, Ordering::Release);
      Ok((region, Some(ClientControl)))
    }
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub mod platform {
  //! Other platforms: no rendezvous yet.

  use super::Accepted;
  use crate::error::IpcError;
  use crate::region::ClientRegion;

  /// No control channel.
  pub struct Control;
  /// No client control channel.
  pub struct ClientControl;

  pub(super) struct Listener;

  impl Listener {
    pub(super) fn open(_instance: &str) -> Result<Listener, IpcError> {
      Err(IpcError::Unsupported {
        feature: "rendezvous",
      })
    }

    pub(super) fn accept_one(
      &mut self,
      _client_id: u32,
      _make_region: &mut dyn FnMut(u32) -> Result<ClientRegion, IpcError>,
    ) -> Result<Option<Accepted>, IpcError> {
      Ok(None)
    }
  }

  pub(super) fn connect(instance: &str) -> Result<(ClientRegion, Option<ClientControl>), IpcError> {
    Err(IpcError::DaemonUnavailable {
      endpoint: instance.to_owned(),
    })
  }
}
