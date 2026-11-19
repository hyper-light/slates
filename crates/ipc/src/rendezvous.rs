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
//!   word; the daemon's control shard finds the claim, creates the region, unlinks its name,
//!   keeps it with the instance owner's Mach token for that slot and that pid (`crate::mach_claim`),
//!   writes its length into the slot and releases the state to ready; the client fetches the
//!   region from the token as a fileport and marks the slot done so the daemon can reclaim it.
//!   No region is ever named where a same-uid stranger could open it (GAP-A9-9). Liveness of a
//!   client is its heartbeat slot (§4.7's "slot heartbeat lapse").
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
  /// When the peer's process started, the mark that with its pid names one process (macOS, Windows; A-522): the claim
  /// carries it beside the pid, and the daemon's watch on the client is taken on that process alone
  /// (`crate::exit_watch::ExitWatch::on_started`), so a pid reused before the watch is not taken for the client. 0 on
  /// Linux, whose watch is the control socket's close.
  pub started: u64,
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
  /// Accepted and its sealed exchange begun (Linux, A-187): the daemon sent its public key and keeps the connection
  /// among its pending handshakes until the client's opening arrives, so no client's pace holds the round.
  Pending,
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
        // A handshake begun: it completes in a later round, when its client's opening arrives.
        Ok(Some(Served::Pending)) => {}
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

  /// The handshakes begun and not yet complete (Linux, A-187): their sockets' descriptors, which the daemon waits on
  /// beside the listener's, and the nanoseconds until the earliest of them is due to be given up. None elsewhere.
  pub fn pending(&self) -> (Vec<i32>, Option<u64>) {
    self.inner.pending()
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
  /// Rings for the client of `shard`, the shard its region names (A-381): only that shard is woken. On Linux the
  /// descriptor is that shard's own kick, so the name is already in it; on macOS and Windows the ring marks the shard
  /// in the bootstrap object before it wakes the daemon's doorbell thread, which kicks the shards marked and no other.
  pub fn ring(&self, shard: u16) -> Result<(), IpcError> {
    match self {
      #[cfg(target_os = "linux")]
      Doorbell::Eventfd(fd) => {
        let _ = shard;
        rustix::io::write(fd, &1u64.to_ne_bytes())
          .map(|_| ())
          .map_err(|e| IpcError::OsRefused {
            call: "eventfd write",
            code: Some(e.raw_os_error()),
          })
      }
      #[cfg(any(target_os = "macos", windows))]
      Doorbell::Word(bell) => bell.ring_for(shard),
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

  /// Rings the doorbell with nothing marked: bumps the word and wakes the waiting thread, which kicks no shard (the
  /// daemon rings it to stop its own thread).
  pub fn ring(&self) -> Result<(), IpcError> {
    self.inner.ring()
  }

  /// What the rings since the last call asked for, taken (A-381): whether a claim rang, and each shard a client rang
  /// for, each once however often it rang. The thread calls it once the word has moved.
  pub fn take_pending(&self) -> Result<Pending, IpcError> {
    self.inner.take_pending()
  }
}

/// What the doorbell's rings asked of the daemon since its thread last looked (A-381).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pending {
  /// A client's claim rang: the control shard, which answers claims, is to be kicked.
  pub claim: bool,
  /// The shards a client rang for, each once, in ascending order: the shard each client's region names.
  pub shards: Vec<u16>,
}

/// How long the rendezvous waits for a silent party, in nanoseconds. Its users: the daemon's wait for a client's opening
/// (Linux); the daemon's reclaim of a claim slot a party left (macOS, Windows: twice it); and the pacing of an async
/// connect's and reconnect's polls (a tenth of it, `slates_client::Connecting::next_poll_ns` and the client's driver).
/// Each is a fixed second R3 forbids, owed in GAPS (the `LIVENESS_BUDGET_NS` entry's class C). A claim waits instead
/// while its rendezvous's holder lives ([`begin_connect_as`], A-443, A-447, A-468).
/// Derived: one liveness window W, the window an anchor judges its daemon silent by (`slates_server::daemon::
/// LIVENESS_BUDGET_NS`, itself `slates_db::replay::RECOVERY_BUDGET_NS`, a second): a party silent that long is gone. This
/// crate sits below both, so the value is restated here.
pub const CLAIM_WAIT_NS: u64 = 1_000_000_000;

/// The client's side: connects to `instance` and returns its region, the doorbell for a parked
/// shard, and the platform's control channel where one exists; it waits while the rendezvous's holder lives
/// ([`begin_connect_as`]).
pub fn connect(instance: &str) -> Result<Connected, IpcError> {
  connect_as(instance, 0)
}

/// Connects asking for a client id it held before (a reconnect after the daemon restarted, so
/// its retries under the old request ids meet their completion records, §4.9); the daemon
/// honours the id when no live client holds it, else assigns a fresh one. The blocking facade over
/// [`begin_connect_as`]: the claim, then a wait for its answer while the rendezvous's holder lives.
pub fn connect_as(instance: &str, wanted: u32) -> Result<Connected, IpcError> {
  begin_connect_as(instance, wanted)?.wait()
}

/// Starts a connect without waiting (AUD-29-19, §4.7 "Rendezvous"): the claim is made and announced to
/// the daemon, and [`Claim::poll`] reads its answer whenever the caller's event loop looks. Nothing here
/// waits on the daemon, so an event loop that drives the claim is never held by a slow, stopped or dead
/// one.
///
/// A claim waits while its rendezvous's holder lives, never by a clock (A-443, A-447, A-468; R3's class C). The holder
/// is the anchor that holds the rendezvous across its daemon's restarts (A-114), or the daemon that holds its own. A
/// daemon answers or goes, and a slow one (a busy machine, a long start or restart) is waited for; the holder's going
/// ends the claim at once. The holder's life is the platform's own. On Linux it is the listening socket, which an anchor
/// keeps across a restart and whose queued connections the kernel resets when its last holder ends. On macOS it is the
/// owner's bootstrap name, which the anchor holds when it holds the rendezvous and launchd takes back at its holder's
/// death (`crate::owner_name`), looked up at twice the time waited each time, so the looks cost the logarithm of the
/// wait. On Windows it is the anchor's process when a live one holds the rendezvous, judged by the pid and creation
/// time it wrote into the header (A-468), else the daemon's (A-447) and the start stamp it published, which it takes
/// back when it stops inside its process; either is waited on beside the claim's Event. Until A-468 a claim under a live
/// anchor waited at most its caller's restart bound (a client's reconnect budget, the recovery budget plus a liveness
/// window), so a restart slower than that failed its clients while the anchor still restarted the daemon.
pub fn begin_connect_as(instance: &str, wanted: u32) -> Result<Claim, IpcError> {
  Ok(Claim {
    inner: platform::begin(instance, wanted)?,
  })
}

/// Whether `instance`'s rendezvous has a live holder now (A-468): an anchor that holds it, or a daemon holding its own.
/// A client whose daemon is gone claims again while one lives, and gives up at once when none does: on Linux, whether
/// a connect to the listening socket is taken or queued (the probe's connection is closed at once, which a listening
/// daemon reads as a client gone); on macOS, whether the owner's bootstrap name is held; on Windows, whether the
/// anchor the header names lives, or else the daemon it names lives and its start stamp is published.
pub fn holder_lives(instance: &str) -> Result<bool, IpcError> {
  platform::holder_lives(instance)
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
  /// a typed refusal when the daemon refused the claim, died before answering it, or did not answer it within its bound
  /// (the restart bound under an anchor; the claim wait on Windows; [`begin_connect_as`]).
  /// Once it has returned `Some` or an error, the claim is spent and a further poll is refused.
  pub fn poll(&mut self) -> Result<Option<Connected>, IpcError> {
    self.inner.poll()
  }

  /// Waits for the answer (the synchronous facade): the platform's own wait between polls, until the answer or the
  /// rendezvous's holder's going.
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

/// What a direct map's delivery says beside its descriptor (§4.6): the request it answers, the mapping, where the file
/// sits in the object and its lengths, and what the peer may do with the object (A-171 read alone, A-186 read and
/// write), which the hand-off judges the descriptor against before it is sent.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub struct Mapping {
  /// The request the delivery answers (the command-ring map request's id).
  pub request_id: u64,
  /// The daemon-minted mapping id, named by the later release message.
  pub mapping_id: u64,
  /// The file's byte offset within the object: zero, since the object is the file's copy.
  pub region_off: u64,
  /// The file's length when it was handed.
  pub file_len: u64,
  /// The object's whole length: the file's page span when it was mapped.
  pub block_len: u64,
  /// What the peer may do with the object.
  pub access: slates_mem::PeerAccess,
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
  /// Whether the stream's resync end follows this delivery, as the next one for the same request (A-316).
  pub resync_follows: bool,
}

/// The device's end of a Shards stream's resync channel (A-316), delivered right after the stream's own end: the
/// channel a successor daemon hands the device a fresh stream on (`slates_bridge_fuse::shards_resync`).
#[cfg(unix)]
pub struct ResyncDelivery {
  /// The channel's end.
  pub end: std::os::fd::OwnedFd,
  /// The request this delivery answers (the attach's id).
  pub request_id: u64,
  /// The attachment the channel resyncs.
  pub attachment: u64,
}

/// A descriptor the daemon delivered over the control socket, with what it is for: a content map (A-135), a stream
/// end (A-161) or a stream's resync end (A-316). Each names the request it answers, so a client takes only its own.
#[cfg(unix)]
pub enum ControlDelivery {
  /// A read-only content map.
  Mapping(MappingDelivery),
  /// A Shards fs stream's other end.
  Stream(StreamDelivery),
  /// A Shards fs stream's resync end.
  Resync(ResyncDelivery),
}

#[cfg(unix)]
impl ControlDelivery {
  /// The request the delivery answers.
  pub fn request_id(&self) -> u64 {
    match self {
      Self::Mapping(mapping) => mapping.request_id,
      Self::Stream(stream) => stream.request_id,
      Self::Resync(resync) => resync.request_id,
    }
  }
}

impl Liveness {
  /// Whether the daemon the client connected to is gone (dead, or restarted since). A cold
  /// path: asked only after a reply has stalled past the client's deadline.
  pub fn daemon_gone(&self) -> bool {
    self.inner.daemon_gone()
  }

  /// The daemon's process handle, to wait on beside the reply's Event (Windows, A-456), as a claim waits beside its
  /// answer's (A-447); `None` when the process was already gone when the watch was taken.
  #[cfg(windows)]
  #[must_use]
  pub fn exit_handle(&self) -> Option<std::os::windows::io::BorrowedHandle<'_>> {
    self.inner.exit_handle()
  }

  /// A liveness check over `socket`, the client's end of a control socket a test made (Linux), its deliveries opened
  /// under the stand-in seal.
  #[cfg(all(test, target_os = "linux"))]
  pub(crate) fn of_socket(socket: std::os::fd::OwnedFd) -> Result<Liveness, IpcError> {
    Ok(Liveness {
      inner: platform::Liveness::of_socket(socket)?,
    })
  }

  /// A liveness check on the daemon of `instance` whose process is `pid` (macOS, Windows): the start stamp the instance's
  /// bootstrap object holds now, and an exit watch on `pid`, as a claim's answer would give a client. Refused when no
  /// daemon has published the object.
  #[cfg(all(test, any(target_os = "macos", windows)))]
  pub(crate) fn of_daemon(instance: &str, pid: u32) -> Result<Liveness, IpcError> {
    Ok(Liveness {
      inner: platform::Liveness::of_daemon(instance, pid)?,
    })
  }

  /// A descriptor that reports the daemon's departure when polled (Linux: the control socket, whose peer end closes
  /// with the daemon's process, so `POLLRDHUP` and `POLLHUP` arrive), for a process that must notice a takeover without
  /// a call in flight (`slates pmem`, A-197). The daemon's deliveries make it readable too, which `POLLRDHUP` alone does
  /// not ask about.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn watch(&self) -> std::os::fd::BorrowedFd<'_> {
    self.inner.watch()
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
  //! Linux: the abstract-namespace socket, `SO_PEERCRED`, `SCM_RIGHTS`, every message after the exchange of public keys
  //! sealed (A-187, `crate::seal`).
  //!
  //! **The address.** An instance named by a plain name is reached at the abstract name `slates-rv-<name>/<uid>`, which
  //! the kernel scopes to a network namespace (unix(7)), so it needs no file. An instance named by an absolute path,
  //! `<directory>/<name>`, is reached at the socket file `slates-rv-<name>.sock` in that directory instead (A-187): an
  //! operator names one so a workload in a network namespace of its own (a pod, a container on a bridge network) can
  //! reach the daemon through a mount of the directory, which it mounts read-only so it can connect but not replace
  //! the node. The directory must be the daemon's user's alone, checked before the bind; the node is made owner-only; a
  //! node a dead owner left is found unanswered and replaced; and the node goes with whoever bound it, the anchor that
  //! holds it across restarts (A-114) or a daemon that runs alone. The protocol over either address is one, sealed.
  //!
  //! **The exchange, daemon first.** The daemon accepts a connection, checks its peer's user, sends its public key and
  //! keeps the connection among its pending handshakes; it waits on nothing. The client, once that key has come, sends
  //! its own public key and its hello sealed, in one message, and waits for the handoff. When that opening arrives the
  //! daemon agrees, opens the hello and answers with the handoff sealed, the client's descriptors beside it. So no
  //! client's pace holds the daemon's round (a connection that never sends is given up at the claim wait, its slot
  //! freed), and no daemon's pace holds the client's poll (AUD-29-19).

  use std::cell::RefCell;
  use std::io::{IoSlice, IoSliceMut};
  use std::os::fd::{AsFd, OwnedFd};
  use std::path::{Path, PathBuf};

  use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
  };
  use slates_mem::Handoff;

  use super::{Accepted, Connected, Doorbell, Prepared, Served, rendezvous_name};
  use crate::error::{IpcError, RouteRefusal};
  use crate::region::ClientRegion;
  use crate::seal::{
    self, Class, PUBLIC_KEY_BYTES, RendezvousSeal, Role, SealedChannel, TAG_BYTES,
  };

  /// Refuses a descriptor that would authorize a peer more than the hand-off does (§4.6; A-135, A-171, A-186).
  ///
  /// What may cross is one file's copy, all of it authorized, sealed against resizing, and against writing too unless
  /// the hand-off is a writable one: the object the direct map makes for each file it hands
  /// (`slates_mem::PeerObject`). Two questions, both measured on the descriptor itself rather than taken from the
  /// caller:
  ///
  /// 1. **Does it reach past what is authorized?** `fstat` gives the object's whole length, which is what
  ///    the holder of the descriptor may map. The authorized bytes start at `region_off` and run
  ///    `authorized` bytes, so the descriptor passes only when they are the whole object: offset zero and
  ///    equal lengths. One file's copy is that by construction; the shared content arena holding every
  ///    volume's blocks never is, and it is what the 2026-10-18 escape handed over.
  /// 2. **Could the peer do more to it than the hand-off allows?** `F_GET_SEALS`. Both `F_SEAL_SHRINK` and
  ///    `F_SEAL_GROW` must be set, or a peer shrinks the object under the daemon's feet. A read-only hand-off needs
  ///    writing sealed too (`F_SEAL_WRITE`, or `F_SEAL_FUTURE_WRITE` where the owner keeps its own earlier map). A
  ///    descriptor duplicated read-only does **not** pass and must not: measured, a same-uid peer re-opens such a
  ///    duplicate `O_RDWR` through `/proc/self/fd` and maps it writable. A writable hand-off needs `F_SEAL_SEAL` as
  ///    well, so its peer can add no seal of its own: what the peer may change is the copy's bytes alone.
  ///
  /// An object with no seal support at all answers `F_GET_SEALS` with `F_SEAL_SEAL` absent and no write
  /// seal, so it is refused by the same arm; a file that is not a memfd refuses the call, and is
  /// refused here too.
  fn confined_for_a_peer(
    descriptor: std::os::fd::BorrowedFd<'_>,
    region_off: u64,
    authorized: u64,
    access: slates_mem::PeerAccess,
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
    let writable = match access {
      slates_mem::PeerAccess::Read => !(write_sealed && size_sealed),
      slates_mem::PeerAccess::ReadWrite => {
        !(size_sealed && seals.contains(rustix::fs::SealFlags::SEAL))
      }
    };
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

    pub(super) fn take_pending(&self) -> Result<super::Pending, IpcError> {
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
  /// Format: the kind of a stream's resync-end delivery (A-316).
  const KIND_RESYNC: u64 = 3;
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
  /// Format: a stream delivery's body after its kind (A-161): request id (8), attachment id (8), and whether the
  /// stream's resync end follows (8, one or zero; A-316). A resync delivery's body is the same, its third word zero.
  const STREAM_MSG_BYTES: usize = 24;
  /// Format: the attachment id's offset in a stream delivery.
  const STREAM_AT_ATTACHMENT: usize = 8;
  /// Format: the resync-follows word's offset in a stream delivery.
  const STREAM_AT_RESYNC_FOLLOWS: usize = 16;
  /// Derived: the longest delivery, a kind word and a mapping's body; every receive reads into this much.
  const DELIVERY_MAX_BYTES: usize = KIND_BYTES + MAPPING_MSG_BYTES;
  /// Format: the one descriptor every delivery carries: an arena block's backing object, or a stream end.
  const DELIVERY_FDS: usize = 1;
  /// Format: the client's hello, sealed.
  const SEALED_HELLO_BYTES: usize = HELLO_BYTES + TAG_BYTES;
  /// Format: the client's opening: its public key, then its hello sealed.
  const OPENING_BYTES: usize = PUBLIC_KEY_BYTES + SEALED_HELLO_BYTES;
  /// Format: the daemon's handoff (or its refusal), sealed.
  const SEALED_HANDOFF_BYTES: usize = HANDOFF_BYTES + TAG_BYTES;
  /// Format: a delivery as it crosses: every kind's body padded to the longest's, then sealed, so every delivery is
  /// one length and a receive reads exactly one.
  const SEALED_DELIVERY_BYTES: usize = DELIVERY_MAX_BYTES + TAG_BYTES;
  /// Derived: the most handshakes a listener keeps pending: one for each connection its backlog can hold, so a
  /// round's arrivals all fit, and a further one is turned away (its socket closed, which its client retries).
  fn pending_most() -> usize {
    usize::try_from(BACKLOG).unwrap_or(1).max(1)
  }

  /// Sends the typed refusal handoff to `peer`, sealed under `channel`: client [`REFUSED_CLIENT`], `limit` in the
  /// length word, no descriptors.
  fn send_refusal(
    peer: &OwnedFd,
    channel: &mut dyn SealedChannel,
    limit: usize,
  ) -> Result<(), IpcError> {
    let mut body = [0u8; HANDOFF_BYTES];
    body[HANDOFF_AT_CLIENT..HANDOFF_AT_LEN].copy_from_slice(&REFUSED_CLIENT.to_le_bytes());
    body[HANDOFF_AT_LEN..].copy_from_slice(&u64::try_from(limit).unwrap_or(u64::MAX).to_le_bytes());
    let sealed = channel.seal(Class::Handoff, &body)?;
    rustix::net::send(peer, &sealed, SendFlags::NOSIGNAL).map_err(|e| refused("send", e))?;
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

  /// The control channel to one client: the socket (its close means the client died), the
  /// completion eventfd the daemon signals for a parked SDK event loop, and the seal its rendezvous agreed, which every
  /// delivery crosses under (A-187).
  pub struct Control {
    /// The socket.
    pub socket: OwnedFd,
    /// The completion eventfd.
    pub completion: OwnedFd,
    /// The channel this client's deliveries are sealed on. Borrowed for one seal at a time on the shard that serves the
    /// client, so the borrow never meets another.
    seal: RefCell<Box<dyn SealedChannel>>,
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
    /// ([`IpcError::UnconfinedDescriptor`] otherwise), and it must be sealed against resizing, and against writing
    /// unless the hand-off is a writable one (A-186), whose object must refuse any new seal instead.
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
      mapping: super::Mapping,
    ) -> Result<(), IpcError> {
      confined_for_a_peer(block, mapping.region_off, mapping.block_len, mapping.access)?;
      let mut body = [0u8; KIND_BYTES + MAPPING_MSG_BYTES];
      put_words(
        &mut body,
        &[
          (0, KIND_MAPPING),
          (KIND_BYTES + AT_REQUEST, mapping.request_id),
          (KIND_BYTES + MAPPING_AT_ID, mapping.mapping_id),
          (KIND_BYTES + MAPPING_AT_OFF, mapping.region_off),
          (KIND_BYTES + MAPPING_AT_FILE_LEN, mapping.file_len),
          (KIND_BYTES + MAPPING_AT_BLOCK_LEN, mapping.block_len),
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
      resync_follows: bool,
    ) -> Result<(), IpcError> {
      let mut body = [0u8; KIND_BYTES + STREAM_MSG_BYTES];
      put_words(
        &mut body,
        &[
          (0, KIND_STREAM),
          (KIND_BYTES + AT_REQUEST, request_id),
          (KIND_BYTES + STREAM_AT_ATTACHMENT, attachment),
          (
            KIND_BYTES + STREAM_AT_RESYNC_FOLLOWS,
            u64::from(resync_follows),
          ),
        ],
      );
      self.send_delivery(&body, stream, "sendmsg stream")
    }

    /// Sends the client that attached a Shards fs stream the device's end of its resync channel (A-316), right after
    /// the stream's own end ([`Self::send_stream`] with `resync_follows`), with the request it answers and the
    /// attachment it resyncs. No confinement check, as for the stream: the channel carries only what the daemon sends
    /// that one attachment's device.
    pub fn send_resync(
      &self,
      end: std::os::fd::BorrowedFd<'_>,
      request_id: u64,
      attachment: u64,
    ) -> Result<(), IpcError> {
      let mut body = [0u8; KIND_BYTES + STREAM_MSG_BYTES];
      put_words(
        &mut body,
        &[
          (0, KIND_RESYNC),
          (KIND_BYTES + AT_REQUEST, request_id),
          (KIND_BYTES + STREAM_AT_ATTACHMENT, attachment),
        ],
      );
      self.send_delivery(&body, end, "sendmsg resync")
    }

    /// One delivery, `body` padded to the longest kind's and sealed, with `descriptor` by `SCM_RIGHTS`, in one
    /// `sendmsg`, so the descriptor arrives with its body and a receive, which reads exactly one sealed delivery, never
    /// runs past it into the next.
    fn send_delivery(
      &self,
      body: &[u8],
      descriptor: std::os::fd::BorrowedFd<'_>,
      call: &'static str,
    ) -> Result<(), IpcError> {
      let mut padded = [0u8; DELIVERY_MAX_BYTES];
      padded
        .get_mut(..body.len())
        .ok_or(IpcError::Layout {
          reason: "a delivery's body is longer than the longest kind's",
        })?
        .copy_from_slice(body);
      let sealed = self
        .seal
        .try_borrow_mut()
        .map_err(|_| IpcError::Layout {
          reason: "a delivery's seal was already in use",
        })?
        .seal(Class::Delivery, &padded)?;
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
        &[IoSlice::new(&sealed)],
        &mut control,
        SendFlags::NOSIGNAL,
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

  /// The client's liveness check: the control socket; its peer end closes with the daemon. It also receives the
  /// daemon's deliveries, which it opens under the seal its rendezvous agreed (A-187).
  pub struct Liveness {
    socket: OwnedFd,
    /// The channel the daemon's deliveries are opened on, borrowed for one open at a time.
    seal: RefCell<Box<dyn SealedChannel>>,
  }

  impl Liveness {
    /// The control socket, polled for the daemon's departure ([`super::Liveness::watch`]).
    pub(super) fn watch(&self) -> std::os::fd::BorrowedFd<'_> {
      std::os::fd::AsFd::as_fd(&self.socket)
    }

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

    /// A liveness check over a test's `socket`, its deliveries opened under the stand-in seal (A-187's test channel).
    #[cfg(test)]
    pub(super) fn of_socket(socket: OwnedFd) -> Result<Liveness, IpcError> {
      let (_daemon_seal, client_seal) = crate::toy_seal::channel_pair()?;
      Ok(Liveness {
        socket,
        seal: RefCell::new(client_seal),
      })
    }

    /// Receives a delivery from the daemon over the control socket (§4.6: a content map, A-135; a stream end,
    /// A-161), or `None` when none is waiting. Non-blocking; the descriptor arrives by `SCM_RIGHTS`
    /// (close-on-exec), what it is for in the body after its kind. A body of the wrong length for its kind, an
    /// unknown kind, or a descriptor count other than one is refused `Layout`, and every descriptor that came with
    /// it is closed: nothing a refused delivery carried stays open in the client.
    pub(super) fn recv_delivery(&self) -> Result<Option<super::ControlDelivery>, IpcError> {
      let mut sealed = [0u8; SEALED_DELIVERY_BYTES];
      let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(DELIVERY_FDS))];
      let mut control = RecvAncillaryBuffer::new(&mut space);
      let received = match rustix::net::recvmsg(
        &self.socket,
        &mut [IoSliceMut::new(&mut sealed)],
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
      if received != SEALED_DELIVERY_BYTES {
        return Err(IpcError::Layout {
          reason: "a delivery was not one sealed delivery long",
        });
      }
      let body = self
        .seal
        .try_borrow_mut()
        .map_err(|_| IpcError::Layout {
          reason: "a delivery's seal was already in use",
        })?
        .open(Class::Delivery, &sealed)?;
      let kind = word_at(&body, 0);
      if ![KIND_MAPPING, KIND_STREAM, KIND_RESYNC].contains(&kind) {
        return Err(IpcError::Layout {
          reason: "a delivery of an unknown kind",
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
          resync_follows: field(STREAM_AT_RESYNC_FOLLOWS) != 0,
        })
      } else if kind == KIND_RESYNC {
        super::ControlDelivery::Resync(super::ResyncDelivery {
          end: descriptor,
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
    /// The socket file this daemon bound itself, removed when it stops (A-187); none for one its anchor holds, which
    /// the anchor removes.
    node: Option<PathBuf>,
    uid: u32,
    /// The handshakes begun whose openings have not all come, oldest first. With [`Self::ready`], at most
    /// [`pending_most`].
    pending: Vec<Handshake>,
    /// Whether this round has read the pending handshakes' openings: once a round, so a round costs one read for each
    /// pending handshake however many connections it serves.
    swept: bool,
    /// The handshakes this round's sweep found complete, oldest first, served before any new connection is accepted.
    ready: std::collections::VecDeque<Handshake>,
    /// The handshakes this round's sweep gave up at the claim wait, each reported as one refusal.
    given_up: usize,
  }

  impl Drop for Listener {
    fn drop(&mut self) {
      let_node_go(self.node.as_deref());
    }
  }

  /// A connection whose sealed exchange is under way (A-187): the daemon sent its public key and waits for the
  /// client's opening, its public key and its hello sealed.
  struct Handshake {
    peer: OwnedFd,
    uid: u32,
    pid: u32,
    seal: Box<dyn RendezvousSeal>,
    /// The opening's bytes received so far.
    opening: Vec<u8>,
    started: std::time::Instant,
  }

  /// Where a pending handshake's opening stands.
  enum Opening {
    /// Not all of it has come.
    Due,
    /// All of it has come.
    Complete,
    /// The client closed its end, or its socket failed: the handshake is dropped.
    Gone,
  }

  /// Reads what has come of `handshake`'s opening, without waiting.
  fn read_opening(handshake: &mut Handshake) -> Opening {
    let want = OPENING_BYTES.saturating_sub(handshake.opening.len());
    if want == 0 {
      return Opening::Complete;
    }
    let mut buffer = [0u8; OPENING_BYTES];
    let Some(room) = buffer.get_mut(..want) else {
      return Opening::Gone;
    };
    match rustix::net::recv(&handshake.peer, room, RecvFlags::DONTWAIT) {
      Ok((0, _)) => Opening::Gone,
      Ok((got, _)) => {
        handshake
          .opening
          .extend_from_slice(buffer.get(..got).unwrap_or_default());
        if handshake.opening.len() >= OPENING_BYTES {
          Opening::Complete
        } else {
          Opening::Due
        }
      }
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Opening::Due,
      Err(_) => Opening::Gone,
    }
  }

  /// Nanoseconds since `started`.
  fn elapsed_ns(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
  }

  /// Format: what follows the rendezvous name in a socket file's name (A-187).
  const NODE_SUFFIX: &str = ".sock";
  /// Format: the bytes a Unix socket address holds for a path, its terminating NUL among them (`sun_path`, unix(7)).
  const SUN_PATH_BYTES: usize = 108;

  /// The socket file `instance`'s rendezvous is reached at when the instance is named by an absolute path (the module
  /// doc): `slates-rv-<name>.sock` in the path's directory. `None` for an instance named otherwise, which is reached at
  /// its abstract name. Refused `PathTooLong` when the node would not fit a socket address.
  fn node(instance: &str) -> Result<Option<PathBuf>, IpcError> {
    let path = Path::new(instance);
    if !path.is_absolute() {
      return Ok(None);
    }
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
      return Err(IpcError::Layout {
        reason: "a rendezvous path names no directory and instance",
      });
    };
    let node = directory.join(format!(
      "{}{NODE_SUFFIX}",
      rendezvous_name(&name.to_string_lossy())
    ));
    if node.as_os_str().len() >= SUN_PATH_BYTES {
      return Err(IpcError::RouteRefused {
        refusal: RouteRefusal::PathTooLong,
      });
    }
    Ok(Some(node))
  }

  fn address(instance: &str) -> Result<SocketAddrUnix, IpcError> {
    if let Some(node) = node(instance)? {
      return SocketAddrUnix::new(&node).map_err(|e| refused("socket address", e));
    }
    let uid = rustix::process::getuid().as_raw();
    let name = format!("{}/{uid}", rendezvous_name(instance));
    SocketAddrUnix::new_abstract_name(name.as_bytes()).map_err(|e| refused("abstract name", e))
  }

  /// Refuses `SharedDirectory` unless `directory` is a directory the effective user owns that no one else may use, the
  /// one a rendezvous's socket file may be bound in (A-187), as the macOS route refuses its own (A-161).
  fn owner_only(directory: &Path) -> Result<(), IpcError> {
    use rustix::fs::{FileType, Mode};
    let stat = rustix::fs::stat(directory).map_err(|e| refused("stat rendezvous directory", e))?;
    let mine = stat.st_uid == rustix::process::geteuid().as_raw();
    let private = !Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO);
    let is_directory = FileType::from_raw_mode(stat.st_mode) == FileType::Directory;
    if mine && private && is_directory {
      Ok(())
    } else {
      Err(IpcError::RouteRefused {
        refusal: RouteRefusal::SharedDirectory,
      })
    }
  }

  /// Whether anything listens at `address`: a daemon, or the anchor holding the rendezvous across a restart.
  fn answers(address: &SocketAddrUnix) -> bool {
    rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .is_ok_and(|probe| rustix::net::connect(&probe, address).is_ok())
  }

  /// Removes a rendezvous's socket file at `node`.
  fn remove_node(node: &Path) -> Result<(), IpcError> {
    // A rendezvous's own socket node, removed by whoever bound it or found left by a dead owner (A-187; socket files
    // authorized by Ada 2026-10-22 when what crosses them is encrypted). structural: allow
    rustix::fs::unlink(node).map_err(|e| refused("unlink rendezvous", e))
  }

  /// Binds `socket` at the socket file `node` (the module doc): its directory checked, a node a dead owner left
  /// replaced, a live owner's refused as the abstract name's is (`EADDRINUSE`), and the node made owner-only.
  fn bind_node(socket: &OwnedFd, address: &SocketAddrUnix, node: &Path) -> Result<(), IpcError> {
    if let Some(directory) = node.parent() {
      owner_only(directory)?;
    }
    match rustix::net::bind(socket, address) {
      Ok(()) => {}
      Err(rustix::io::Errno::ADDRINUSE) => {
        if answers(address) {
          return Err(refused("bind", rustix::io::Errno::ADDRINUSE));
        }
        // A node a dead owner left: nothing answers it, so it is removed and bound again.
        remove_node(node)?;
        rustix::net::bind(socket, address).map_err(|e| refused("bind", e))?;
      }
      Err(e) => return Err(refused("bind", e)),
    }
    // The rendezvous's own socket node, made the user's alone (A-187; socket files authorized by Ada 2026-10-22 when
    // what crosses them is encrypted). structural: allow
    rustix::fs::chmod(node, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR)
      .map_err(|e| refused("chmod rendezvous", e))
  }

  /// A listening socket bound at `instance`'s address, with `flags` (close-on-exec for the daemon's own; none for the
  /// anchor's, which every daemon it spawns inherits), and the socket file it was bound at, if any, which its owner
  /// removes when it lets the socket go.
  fn listening(instance: &str, flags: SocketFlags) -> Result<(OwnedFd, Option<PathBuf>), IpcError> {
    let socket = rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      flags | SocketFlags::NONBLOCK,
      None,
    )
    .map_err(|e| refused("socket", e))?;
    let address = address(instance)?;
    let node = node(instance)?;
    match &node {
      Some(node) => bind_node(&socket, &address, node)?,
      None => rustix::net::bind(&socket, &address).map_err(|e| refused("bind", e))?,
    }
    rustix::net::listen(&socket, BACKLOG).map_err(|e| refused("listen", e))?;
    Ok((socket, node))
  }

  /// Removes the socket file a listener was bound at, when it was bound at one: a removal refused leaves a node the next
  /// owner finds unanswered and replaces.
  fn let_node_go(node: Option<&Path>) {
    if let Some(node) = node {
      let _ = remove_node(node);
    }
  }

  /// The anchor's listening socket (A-114): inheritable, so the kernel keeps the address and queues every connect while
  /// no daemon runs.
  pub(super) struct Held {
    socket: OwnedFd,
    /// The socket file the anchor bound, removed when it stops (A-187).
    node: Option<PathBuf>,
  }

  impl Drop for Held {
    fn drop(&mut self) {
      let_node_go(self.node.as_deref());
    }
  }

  impl Held {
    pub(super) fn hold(instance: &str) -> Result<Held, IpcError> {
      let (socket, node) = listening(instance, SocketFlags::empty())?;
      Ok(Held { socket, node })
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
      let (socket, node) = match adoption {
        Some(value) => (adopt(instance, value)?, None),
        None => listening(instance, SocketFlags::CLOEXEC)?,
      };
      Ok(Listener {
        socket,
        node,
        uid: rustix::process::getuid().as_raw(),
        pending: Vec::new(),
        swept: false,
        ready: std::collections::VecDeque::new(),
        given_up: 0,
      })
    }

    /// The pending handshakes' descriptors, and the nanoseconds until the earliest is due to be given up.
    pub(super) fn pending(&self) -> (Vec<i32>, Option<u64>) {
      use std::os::fd::AsRawFd;
      let descriptors = self
        .pending
        .iter()
        .map(|handshake| handshake.peer.as_raw_fd())
        .collect();
      let due = self
        .pending
        .iter()
        .map(|handshake| super::CLAIM_WAIT_NS.saturating_sub(elapsed_ns(handshake.started)))
        .min();
      (descriptors, due)
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

    /// Serves the listener a step: a pending handshake whose opening has come is completed (its handoff or its
    /// refusal), one given up at the claim wait is reported, else a new connection is accepted and its exchange begun.
    /// A failure of the accept itself is the rendezvous's (the round ends); anything after it is that one connection's
    /// refusal (A-176). `None` once none of these has anything left, which ends the round.
    pub(super) fn accept_one(
      &mut self,
      assign: &mut dyn FnMut(u32) -> u32,
      make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
    ) -> Result<Option<Served>, IpcError> {
      if !self.swept {
        self.sweep();
        self.swept = true;
      }
      if let Some(handshake) = self.ready.pop_front() {
        return Ok(Some(match serve(handshake, assign, make_region) {
          Ok(accepted) => Served::Accepted(Box::new(accepted)),
          Err(refusal) => Served::Refused(refusal),
        }));
      }
      if self.given_up > 0 {
        self.given_up = self.given_up.saturating_sub(1);
        return Ok(Some(Served::Refused(IpcError::DeadlineExceeded)));
      }
      let peer = match rustix::net::accept_with(
        &self.socket,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
      ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::AGAIN) => {
          self.swept = false;
          return Ok(None);
        }
        Err(e) => {
          self.swept = false;
          return Err(refused("accept", e));
        }
      };
      Ok(Some(match self.begin_handshake(peer) {
        Ok(()) => Served::Pending,
        Err(refusal) => Served::Refused(refusal),
      }))
    }

    /// Begins `peer`'s exchange: its user checked, the daemon's public key sent, and the connection kept among the
    /// pending handshakes. Refused for a peer of another user, past [`pending_most`] (its socket closes, which its client
    /// retries), or where the key cannot be made or sent.
    fn begin_handshake(&mut self, peer: OwnedFd) -> Result<(), IpcError> {
      let cred = crate::peer::peer_credentials(peer.as_fd())?;
      if cred.uid != self.uid {
        return Err(IpcError::PeerRefused { uid: cred.uid });
      }
      if self.pending.len().saturating_add(self.ready.len()) >= pending_most() {
        return Err(IpcError::HandshakesFull {
          limit: pending_most(),
        });
      }
      let seal = seal::fresh()?;
      let key = seal.public_key();
      // A fresh socket's send buffer takes the key whole.
      let sent =
        rustix::net::send(&peer, &key, SendFlags::NOSIGNAL).map_err(|e| refused("send", e))?;
      if sent != key.len() {
        return Err(IpcError::Layout {
          reason: "the daemon's public key was sent short",
        });
      }
      self.pending.push(Handshake {
        peer,
        uid: cred.uid,
        pid: cred.pid,
        seal,
        opening: Vec::with_capacity(OPENING_BYTES),
        started: std::time::Instant::now(),
      });
      Ok(())
    }

    /// Reads every pending handshake's opening once: those complete move to [`Self::ready`], those past the claim wait
    /// are given up (their sockets closed, which their clients read as a daemon that did not answer), and those whose
    /// clients have gone are dropped.
    fn sweep(&mut self) {
      let mut kept = Vec::with_capacity(self.pending.len());
      for mut handshake in std::mem::take(&mut self.pending) {
        match read_opening(&mut handshake) {
          Opening::Complete => self.ready.push_back(handshake),
          Opening::Due if elapsed_ns(handshake.started) < super::CLAIM_WAIT_NS => {
            kept.push(handshake);
          }
          Opening::Due => self.given_up = self.given_up.saturating_add(1),
          Opening::Gone => {}
        }
      }
      self.pending = kept;
    }
  }

  /// Serves one completed handshake: its key agreed, its hello opened, its region made and the handoff sent sealed.
  /// Every failure here is this connection's alone, and the peer is told what it can be told.
  fn serve(
    handshake: Handshake,
    assign: &mut dyn FnMut(u32) -> u32,
    make_region: &mut dyn FnMut(u32) -> Result<Prepared, IpcError>,
  ) -> Result<Accepted, IpcError> {
    let Handshake {
      peer,
      uid,
      pid,
      seal,
      opening,
      ..
    } = handshake;
    let (key, sealed_hello) =
      opening
        .split_at_checked(PUBLIC_KEY_BYTES)
        .ok_or(IpcError::Layout {
          reason: "a short opening",
        })?;
    let key = <[u8; PUBLIC_KEY_BYTES]>::try_from(key).map_err(|_| IpcError::Layout {
      reason: "a short public key",
    })?;
    let mut channel = seal.agree(&key, Role::Daemon)?;
    let hello = channel.open(Class::Hello, sealed_hello)?;
    let wanted = <[u8; HELLO_BYTES]>::try_from(hello.as_slice())
      .map(u32::from_le_bytes)
      .unwrap_or(0);
    // The control socket blocks on its sends for its life, as it did before the exchange made the accept non-blocking.
    rustix::io::ioctl_fionbio(&peer, false).map_err(|e| refused("ioctl(FIONBIO)", e))?;
    let client_id = assign(wanted);
    let Prepared { region, kick_fd } = match make_region(client_id) {
      Ok(prepared) => prepared,
      // The bound is reached: the client is told so, typed — a handoff naming client `REFUSED_CLIENT`
      // with the bound in the length word and no descriptors — where before the socket was dropped
      // and the client read a short handoff it could not interpret (2026-09-14).
      Err(IpcError::TooManyClients { limit }) => {
        send_refusal(&peer, channel.as_mut(), limit)?;
        return Err(IpcError::TooManyClients { limit });
      }
      // Any other refusal of the region (one that could not be created): the client is told there is
      // no region for it (a refusal with no bound), never left to read a closed socket.
      Err(e) => {
        send_refusal(&peer, channel.as_mut(), super::NO_BOUND)?;
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
    let sealed = channel.seal(Class::Handoff, &body).map_err(lost)?;
    rustix::net::sendmsg(
      &peer,
      &[IoSlice::new(&sealed)],
      &mut control,
      SendFlags::NOSIGNAL,
    )
    .map_err(|e| lost(refused("sendmsg", e)))?;
    Ok(Accepted {
      client_id,
      uid,
      pid,
      started: 0,
      region,
      control: Some(Control {
        socket: peer,
        completion,
        seal: RefCell::new(channel),
      }),
    })
  }

  /// A claim in flight (AUD-29-19): the connected, non-blocking socket and where its sealed exchange stands (A-187).
  pub struct Claim {
    instance: String,
    /// The socket; taken once the claim is answered or refused.
    socket: Option<OwnedFd>,
    /// Where the exchange stands. The claim waits while the listener's holder lives, its going resetting the connection
    /// (`super::begin_connect_as`, A-443, A-468).
    stage: Stage,
  }

  /// Where a claim's exchange stands, daemon first (the module doc).
  enum Stage {
    /// Waiting for the daemon's public key: this side's half of the exchange, the id it wants, and the key's bytes
    /// received so far.
    AwaitingKey {
      seal: Box<dyn RendezvousSeal>,
      wanted: u32,
      key: Vec<u8>,
    },
    /// The opening sent: waiting for the handoff, which opens under the channel the exchange agreed.
    AwaitingHandoff { channel: Box<dyn SealedChannel> },
    /// Answered or refused: nothing is left to read.
    Spent,
  }

  /// Whether `instance`'s listening socket has a holder (`super::holder_lives`, A-468): a connect taken or queued says
  /// one listens, the anchor across a restart or the daemon itself, and so does a full backlog (`EAGAIN`); a refusal says
  /// none. The probe's connection is closed at once, which a listening daemon reads as a client gone.
  pub(super) fn holder_lives(instance: &str) -> Result<bool, IpcError> {
    let probe = rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
      None,
    )
    .map_err(|e| refused("socket", e))?;
    match rustix::net::connect(&probe, &address(instance)?) {
      Ok(()) | Err(rustix::io::Errno::AGAIN) => Ok(true),
      Err(rustix::io::Errno::CONNREFUSED | rustix::io::Errno::NOENT) => Ok(false),
      Err(e) => Err(refused("connect", e)),
    }
  }

  /// Connects a non-blocking socket to the daemon's rendezvous; nothing waits (a stream connect over `AF_UNIX`
  /// completes at once or is refused when the daemon's backlog is full), and nothing is sent until the daemon's key has
  /// come. This side's half of the exchange is made first, so a process with no seal installed claims nothing.
  pub(super) fn begin(instance: &str, wanted: u32) -> Result<Claim, IpcError> {
    let seal = seal::fresh()?;
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
        why: match errno {
          rustix::io::Errno::AGAIN => {
            "the rendezvous backlog is full (the daemon is not accepting)"
          }
          rustix::io::Errno::ACCESS => {
            "the rendezvous socket file refused this user (its directory or node is another user's)"
          }
          _ => "the rendezvous socket refused the connection (no daemon listening)",
        },
      }
    })?;
    // The daemon is checked as it checks its clients: the listener's user must be this one, or the claim goes no further.
    // An abstract name is anyone's to bind, so without this a process of another user could answer as the daemon.
    let daemon = crate::peer::peer_credentials(socket.as_fd())?;
    if daemon.uid != rustix::process::getuid().as_raw() {
      return Err(IpcError::PeerRefused { uid: daemon.uid });
    }
    Ok(Claim {
      instance: instance.to_owned(),
      socket: Some(socket),
      stage: Stage::AwaitingKey {
        seal,
        wanted,
        key: Vec::with_capacity(PUBLIC_KEY_BYTES),
      },
    })
  }

  /// A handoff opened: its body, and the descriptors that came with it.
  struct Opened {
    body: Vec<u8>,
    fds: Vec<OwnedFd>,
  }

  /// Whether `errno` says the daemon's end of the rendezvous is gone (killed with the claim still queued, or closing as
  /// it did): a claim it never answered, which a reconnect retries, never a refusal that ends the caller's calls (found
  /// on CI 2026-10-01: `OsRefused { call: "recvmsg", code: Some(104) }`).
  fn daemon_left(errno: rustix::io::Errno) -> bool {
    matches!(
      errno,
      rustix::io::Errno::CONNRESET
        | rustix::io::Errno::CONNREFUSED
        | rustix::io::Errno::PIPE
        | rustix::io::Errno::NOTCONN
    )
  }

  impl Claim {
    /// The handoff, without waiting: the exchange advanced as far as what has come allows, and the handoff received and
    /// decoded once it has come; `None` while it is due, while the listener's holder lives; unavailable when the
    /// connection closed or reset unanswered.
    pub(super) fn poll(&mut self) -> Result<Option<Connected>, IpcError> {
      let answer = self.advance();
      if !matches!(answer, Ok(None)) {
        self.socket = None;
        self.stage = Stage::Spent;
      }
      answer
    }

    /// One step of the exchange: the opening sent once the daemon's key has come, then the handoff read.
    fn advance(&mut self) -> Result<Option<Connected>, IpcError> {
      if let Stage::AwaitingKey { .. } = self.stage {
        if !self.read_key()? {
          return Ok(None);
        }
        self.send_opening()?;
      }
      match std::mem::replace(&mut self.stage, Stage::Spent) {
        Stage::AwaitingHandoff { mut channel } => match self.read_handoff(channel.as_mut())? {
          Some(Opened { body, fds }) => {
            let socket = self.socket.take().ok_or(IpcError::Layout {
              reason: "the claim's socket was already taken",
            })?;
            decode_handoff(&self.instance, socket, &body, fds, channel).map(Some)
          }
          None => {
            self.stage = Stage::AwaitingHandoff { channel };
            Ok(None)
          }
        },
        Stage::AwaitingKey { .. } | Stage::Spent => Err(IpcError::DaemonUnavailable {
          endpoint: self.instance.clone(),
          why: "the claim was already answered or refused",
        }),
      }
    }

    /// The socket, while the claim lasts.
    fn socket(&self) -> Result<&OwnedFd, IpcError> {
      self.socket.as_ref().ok_or(IpcError::DaemonUnavailable {
        endpoint: self.instance.clone(),
        why: "the claim was already answered or refused",
      })
    }

    /// The daemon closed the rendezvous before it answered.
    fn left(&self) -> IpcError {
      IpcError::DaemonUnavailable {
        endpoint: self.instance.clone(),
        why: "the daemon closed the rendezvous before answering the claim",
      }
    }

    /// Reads what has come of the daemon's public key: whether all of it has.
    fn read_key(&mut self) -> Result<bool, IpcError> {
      let socket = self.socket()?;
      let Stage::AwaitingKey { key, .. } = &self.stage else {
        return Ok(true);
      };
      let want = PUBLIC_KEY_BYTES.saturating_sub(key.len());
      let mut buffer = [0u8; PUBLIC_KEY_BYTES];
      let room = buffer.get_mut(..want).ok_or(IpcError::Layout {
        reason: "the daemon's key overran its length",
      })?;
      let got = match rustix::net::recv(socket, room, RecvFlags::DONTWAIT) {
        Ok((0, _)) => return Err(self.left()),
        Ok((got, _)) => got,
        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(false),
        Err(errno) if daemon_left(errno) => return Err(self.left()),
        Err(errno) => return Err(refused("recv", errno)),
      };
      let Stage::AwaitingKey { key, .. } = &mut self.stage else {
        return Ok(true);
      };
      key.extend_from_slice(buffer.get(..got).unwrap_or_default());
      Ok(key.len() >= PUBLIC_KEY_BYTES)
    }

    /// Agrees with the daemon's key and sends the opening, this side's public key and its hello sealed, in one send;
    /// the claim then waits for the handoff.
    fn send_opening(&mut self) -> Result<(), IpcError> {
      let Stage::AwaitingKey { seal, wanted, key } =
        std::mem::replace(&mut self.stage, Stage::Spent)
      else {
        return Err(IpcError::Layout {
          reason: "the opening was sent twice",
        });
      };
      let daemon_key =
        <[u8; PUBLIC_KEY_BYTES]>::try_from(key.as_slice()).map_err(|_| IpcError::Layout {
          reason: "the daemon's key is not one key long",
        })?;
      let public = seal.public_key();
      let mut channel = seal.agree(&daemon_key, Role::Client)?;
      let mut opening = Vec::with_capacity(OPENING_BYTES);
      opening.extend_from_slice(&public);
      opening.extend_from_slice(&channel.seal(Class::Hello, &wanted.to_le_bytes())?);
      // A fresh socket's send buffer takes the opening whole.
      let sent = match rustix::net::send(self.socket()?, &opening, SendFlags::NOSIGNAL) {
        Ok(sent) => sent,
        Err(errno) if daemon_left(errno) => return Err(self.left()),
        Err(errno) => return Err(refused("send", errno)),
      };
      if sent != opening.len() {
        return Err(IpcError::Layout {
          reason: "the client's opening was sent short",
        });
      }
      self.stage = Stage::AwaitingHandoff { channel };
      Ok(())
    }

    /// Reads the handoff, sealed, with its descriptors, and opens it under `channel`: its body and the descriptors that
    /// came with it, or `None` while it has not come.
    fn read_handoff(&self, channel: &mut dyn SealedChannel) -> Result<Option<Opened>, IpcError> {
      let mut sealed = [0u8; SEALED_HANDOFF_BYTES];
      let mut space =
        [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(HANDOFF_FDS))];
      let mut control = RecvAncillaryBuffer::new(&mut space);
      let received = match rustix::net::recvmsg(
        self.socket()?,
        &mut [IoSliceMut::new(&mut sealed)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
      ) {
        Ok(received) => received.bytes,
        Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Ok(None),
        Err(errno) if daemon_left(errno) => return Err(self.left()),
        Err(errno) => return Err(refused("recvmsg", errno)),
      };
      // Taken before any check, so every descriptor a refused handoff carried is closed with it.
      let mut fds: Vec<OwnedFd> = Vec::new();
      for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(iter) = message {
          fds.extend(iter);
        }
      }
      if received == 0 {
        return Err(self.left());
      }
      if received != SEALED_HANDOFF_BYTES {
        return Err(IpcError::Layout {
          reason: "short handoff message",
        });
      }
      let body = channel.open(Class::Handoff, &sealed)?;
      Ok(Some(Opened { body, fds }))
    }

    /// Waits for the handoff between polls on the socket's readability: its answer, or its reset at the listener's
    /// holder's going.
    pub(super) fn wait(mut self) -> Result<Connected, IpcError> {
      use rustix::event::{PollFd, PollFlags};
      loop {
        if let Some(connected) = self.poll()? {
          return Ok(connected);
        }
        let Some(socket) = self.socket.as_ref() else {
          continue;
        };
        let mut fds = [PollFd::new(socket, PollFlags::IN)];
        match rustix::event::poll(&mut fds, None) {
          Ok(_) | Err(rustix::io::Errno::INTR) => {}
          Err(e) => return Err(refused("poll", e)),
        }
      }
    }
  }

  /// Decodes an opened handoff: the daemon's typed refusal, or the region, the completion eventfd and the shard's kick,
  /// with `socket` kept as the control channel and the liveness signal, and `channel` as the seal its deliveries open
  /// under (A-187).
  fn decode_handoff(
    instance: &str,
    socket: OwnedFd,
    body: &[u8],
    mut fds: Vec<OwnedFd>,
    channel: Box<dyn SealedChannel>,
  ) -> Result<Connected, IpcError> {
    if body.len() != HANDOFF_BYTES {
      return Err(IpcError::Layout {
        reason: "short handoff message",
      });
    }
    let client_word = body
      .get(HANDOFF_AT_CLIENT..HANDOFF_AT_LEN)
      .and_then(|word| <[u8; size_of::<u32>()]>::try_from(word).ok())
      .map_or(REFUSED_CLIENT, u32::from_le_bytes);
    let len_word = word_at(body, HANDOFF_AT_LEN);
    if client_word == REFUSED_CLIENT {
      // The daemon's typed refusal: no region, the bound in the length word (any descriptor that came with it is closed
      // with `fds`).
      return Err(super::refusal_of(
        usize::try_from(len_word).unwrap_or(usize::MAX),
        instance,
      ));
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
    let len = usize::try_from(len_word).unwrap_or(0);
    let raw = std::os::fd::IntoRawFd::into_raw_fd(region_fd);
    let region = ClientRegion::open(&Handoff::Descriptor(raw), len)?;
    Ok(Connected {
      region,
      doorbell: Doorbell::Eventfd(kick),
      liveness: super::Liveness {
        inner: Liveness {
          socket,
          seal: RefCell::new(channel),
        },
      },
      control: Some(ClientControl { completion }),
    })
  }

  #[cfg(test)]
  mod tests {
    use std::os::fd::{AsFd, OwnedFd};

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
      let (control, liveness) = channel()?;

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

      control.send_mapping(mem.as_fd(), read_only(7, 42, (0, file_len, block_len)))?;

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
      let (control, liveness) = channel()?;

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
      control.send_mapping(
        sealed.as_fd(),
        read_only(9, 11, (0, object_bytes, object_bytes)),
      )?;
      let good = mapping_of(liveness.recv_delivery()?)?;
      if good.mapping_id != 11 {
        return Err(IpcError::Layout {
          reason: "the confined hand-off arrived with the wrong metadata",
        });
      }
      for (descriptor, region_off, authorized, what) in offers {
        match control.send_mapping(
          descriptor,
          read_only(7, 42, (region_off, authorized, authorized)),
        ) {
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

    /// §4.6, A-186: a writable hand-off is judged for what a writable copy may be, and a read-only one is never handed
    /// a writable copy. Do: offer a memfd sealed against resizing and new seals but not writes as a writable hand-off
    /// and as a read-only one; offer one sealed against resizing alone (no `F_SEAL_SEAL`) and an unsealed one as
    /// writable hand-offs; take whatever reached the client. Expect: the first passes as writable and arrives; every
    /// other offer is refused `UnconfinedDescriptor` naming it writable, and nothing more reached the client.
    #[test]
    fn a_writable_hand_off_is_judged_for_what_a_writable_copy_may_be() -> Result<(), IpcError> {
      let (control, liveness) = channel()?;
      let bytes: u64 = 4096;
      let sealed = |name: &str, seals: rustix::fs::SealFlags| -> Result<OwnedFd, IpcError> {
        let fd = memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
          .map_err(|e| super::refused("memfd_create", e))?;
        #[allow(clippy::disallowed_methods)] // sizing a memory object: RAM, not a host path (R1).
        rustix::fs::ftruncate(&fd, bytes).map_err(|e| super::refused("ftruncate", e))?;
        if !seals.is_empty() {
          rustix::fs::fcntl_add_seals(&fd, seals).map_err(|e| super::refused("add seals", e))?;
        }
        Ok(fd)
      };
      let fixed =
        rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW | rustix::fs::SealFlags::SEAL;
      let writable = sealed("slates-a186-writable", fixed)?;
      let resealable = sealed(
        "slates-a186-resealable",
        rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
      )?;
      let open = sealed("slates-a186-open", rustix::fs::SealFlags::empty())?;
      let as_writable = |request_id| super::super::Mapping {
        access: slates_mem::PeerAccess::ReadWrite,
        ..read_only(request_id, request_id, (0, bytes, bytes))
      };
      control.send_mapping(writable.as_fd(), as_writable(1))?;
      if mapping_of(liveness.recv_delivery()?)?.mapping_id != 1 {
        return Err(IpcError::Layout {
          reason: "the writable hand-off arrived with the wrong metadata",
        });
      }
      let offers = [
        (writable.as_fd(), read_only(2, 2, (0, bytes, bytes))),
        (resealable.as_fd(), as_writable(3)),
        (open.as_fd(), as_writable(4)),
      ];
      for (descriptor, mapping) in offers {
        match control.send_mapping(descriptor, mapping) {
          Err(IpcError::UnconfinedDescriptor { writable: true, .. }) => {}
          Ok(()) => {
            return Err(IpcError::Layout {
              reason: "a descriptor that authorizes more than its hand-off was sent",
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

    /// A read-only hand-off's metadata: request `request_id`, mapping `mapping_id`, and `(region_off, file_len,
    /// block_len)`.
    fn read_only(
      request_id: u64,
      mapping_id: u64,
      (region_off, file_len, block_len): (u64, u64, u64),
    ) -> super::super::Mapping {
      super::super::Mapping {
        request_id,
        mapping_id,
        region_off,
        file_len,
        block_len,
        access: slates_mem::PeerAccess::Read,
      }
    }

    /// The mapping a delivery holds, or a typed error for none or another kind.
    fn mapping_of(
      delivery: Option<super::super::ControlDelivery>,
    ) -> Result<super::super::MappingDelivery, IpcError> {
      match delivery {
        Some(super::super::ControlDelivery::Mapping(mapping)) => Ok(mapping),
        Some(
          super::super::ControlDelivery::Stream(_) | super::super::ControlDelivery::Resync(_),
        ) => Err(IpcError::Layout {
          reason: "a stream's end arrived where a mapping was sent",
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
      // Agreed under the stand-in seal, as a rendezvous agrees them under the real one (A-187).
      let (daemon_seal, client_seal) = crate::toy_seal::channel_pair()?;
      Ok((
        Control {
          socket: daemon_socket,
          completion,
          seal: std::cell::RefCell::new(daemon_seal),
        },
        Liveness {
          socket: client_socket,
          seal: std::cell::RefCell::new(client_seal),
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

      control.send_stream(handed.as_fd(), 5, 501, false)?;
      control.send_mapping(block.as_fd(), read_only(6, 12, (0, 4096, 4096)))?;
      control.send_stream(other_handed.as_fd(), 7, 502, false)?;
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

    /// The three deliveries [`a_streams_resync_end_follows_its_end_on_the_control_socket`] sends, read in order off
    /// `liveness`: a stream end, a resync end and another stream end, each refused `Layout` if it is another kind.
    fn stream_resync_and_plain(
      liveness: &Liveness,
    ) -> Result<
      (
        super::super::StreamDelivery,
        super::super::ResyncDelivery,
        super::super::StreamDelivery,
      ),
      IpcError,
    > {
      let Some(super::super::ControlDelivery::Stream(stream)) = liveness.recv_delivery()? else {
        return Err(IpcError::Layout {
          reason: "the first delivery was not the stream end",
        });
      };
      let Some(super::super::ControlDelivery::Resync(resync)) = liveness.recv_delivery()? else {
        return Err(IpcError::Layout {
          reason: "the second delivery was not the resync end",
        });
      };
      let Some(super::super::ControlDelivery::Stream(plain)) = liveness.recv_delivery()? else {
        return Err(IpcError::Layout {
          reason: "the third delivery was not the plain stream end",
        });
      };
      Ok((stream, resync, plain))
    }

    /// A-316: a stream whose resync channel the daemon made is delivered with it. Do: send a stream end saying its
    /// resync end follows, then that resync end, then a plain stream end; read all three, and send a message on the
    /// daemon's end of the resync pair. Expect the first to say a resync end follows and the third not; the second to be
    /// the resync end for the same request and attachment; and the message read whole on the delivered end, which is
    /// the channel's other end, not a copy.
    #[test]
    fn a_streams_resync_end_follows_its_end_on_the_control_socket() -> Result<(), IpcError> {
      let (control, liveness) = channel()?;
      let (_daemon_end, handed) = crate::stream::pair()?;
      let (daemon_resync, handed_resync) = crate::stream::message_pair()?;
      let (_plain_daemon_end, plain_handed) = crate::stream::pair()?;
      control.send_stream(handed.as_fd(), 9, 901, true)?;
      control.send_resync(handed_resync.as_fd(), 9, 901)?;
      control.send_stream(plain_handed.as_fd(), 10, 902, false)?;
      drop(handed_resync);
      let (stream, resync, plain) = stream_resync_and_plain(&liveness)?;
      let told = (stream.resync_follows, plain.resync_follows);
      let named = (resync.request_id, resync.attachment);
      if told != (true, false) || named != (9, 901) {
        return Err(IpcError::Layout {
          reason: "a resync end was not told, or named another request",
        });
      }
      crate::stream::send(daemon_resync.as_fd(), b"attached")
        .map_err(|e| super::refused("send", e))?;
      let mut got = [0u8; 16];
      let (read, descriptors) = crate::stream::recv_message(resync.end.as_fd(), &mut got)
        .map_err(|e| super::refused("recv_message", e))?;
      if got.get(..read) != Some(&b"attached"[..]) || !descriptors.is_empty() {
        return Err(IpcError::Layout {
          reason: "the delivered resync end is not the channel's other end",
        });
      }
      Ok(())
    }

    /// One way a hostile writer on the control socket might hand the client a delivery.
    #[derive(Clone, Copy, Debug)]
    enum Hostile {
      /// The bytes of a stream delivery, unsealed: the wrong length to be one.
      Unsealed,
      /// A sealed delivery's length of bytes that were never sealed.
      Forged,
      /// Sealed by the daemon's own channel, of a kind no delivery has.
      UnknownKind,
      /// Sealed and sent second, after the first was taken off the socket unopened.
      AfterADropped,
    }

    /// Hostile input on the client's side (A-135, A-161, A-187): a delivery that is not one sealed delivery long, one
    /// that does not open, one that opens to an unknown kind, and one that arrives after another was dropped are each
    /// refused, and the descriptor each carried is closed in the client (its peer sees the end). Do: each on a fresh
    /// control socket. Expect: `Layout` for the unsealed one and the unknown kind, `SealRefused` for the forged one
    /// and the one after a drop, whose count is not the next.
    #[test]
    fn an_unsealed_forged_unknown_or_dropped_delivery_is_refused_and_its_descriptor_closed()
    -> Result<(), IpcError> {
      // The zero read waits on the handed end's last close, which a child another test spawns would hold until its
      // exec ([`crate::descriptor_test_gate`]).
      let _gate = crate::descriptor_test_gate();
      for case in [
        Hostile::Unsealed,
        Hostile::Forged,
        Hostile::UnknownKind,
        Hostile::AfterADropped,
      ] {
        let (control, liveness) = channel()?;
        let (daemon_end, handed) = crate::stream::pair()?;
        match case {
          Hostile::Unsealed => {
            let mut body = [0u8; super::KIND_BYTES + super::STREAM_MSG_BYTES];
            super::put_words(&mut body, &[(0, super::KIND_STREAM)]);
            send_unsealed(&control, &body, handed.as_fd())?;
          }
          Hostile::Forged => {
            send_unsealed(
              &control,
              &[0xa5; super::SEALED_DELIVERY_BYTES],
              handed.as_fd(),
            )?;
          }
          Hostile::UnknownKind => {
            let mut body = [0u8; super::KIND_BYTES + super::STREAM_MSG_BYTES];
            super::put_words(&mut body, &[(0, 99)]);
            control.send_delivery(&body, handed.as_fd(), "sendmsg hostile")?;
          }
          Hostile::AfterADropped => {
            let (_first_end, first) = crate::stream::pair()?;
            control.send_stream(first.as_fd(), 1, 1, false)?;
            control.send_stream(handed.as_fd(), 2, 2, false)?;
            drop_one(&liveness)?;
          }
        }
        drop(handed);
        let refusal = liveness.recv_delivery();
        let as_expected = match case {
          Hostile::Unsealed | Hostile::UnknownKind => {
            matches!(refusal, Err(IpcError::Layout { .. }))
          }
          Hostile::Forged | Hostile::AfterADropped => {
            matches!(refusal, Err(IpcError::SealRefused { .. }))
          }
        };
        if !as_expected {
          return Err(IpcError::Layout {
            reason: "a hostile delivery was taken, or refused for another reason than its own",
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

    /// Sends `body` as it is, unsealed, with `descriptor`, as a hostile writer on the control socket would.
    fn send_unsealed(
      control: &Control,
      body: &[u8],
      descriptor: std::os::fd::BorrowedFd<'_>,
    ) -> Result<(), IpcError> {
      use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
      let fds = [descriptor];
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
        &[std::io::IoSlice::new(body)],
        &mut ancillary,
        SendFlags::empty(),
      )
      .map_err(|e| super::refused("sendmsg", e))?;
      Ok(())
    }

    /// Takes one delivery off the client's socket without opening it, and drops it with its descriptor.
    fn drop_one(liveness: &Liveness) -> Result<(), IpcError> {
      use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
      let mut sealed = [0u8; super::SEALED_DELIVERY_BYTES];
      let mut space = [std::mem::MaybeUninit::<u8>::uninit();
        rustix::cmsg_space!(ScmRights(super::DELIVERY_FDS))];
      let mut ancillary = RecvAncillaryBuffer::new(&mut space);
      rustix::net::recvmsg(
        &liveness.socket,
        &mut [std::io::IoSliceMut::new(&mut sealed)],
        &mut ancillary,
        RecvFlags::CMSG_CLOEXEC,
      )
      .map_err(|e| super::refused("recvmsg", e))?;
      for message in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(descriptors) = message {
          descriptors.for_each(drop);
        }
      }
      Ok(())
    }

    /// A region for client `client_id` of a test's daemon end, named for this process and a count.
    fn test_region(client_id: u32) -> Result<super::Prepared, IpcError> {
      use std::sync::atomic::{AtomicU64, Ordering};
      static MADE: AtomicU64 = AtomicU64::new(0);
      let made = MADE.fetch_add(1, Ordering::Relaxed);
      let geometry = crate::region::RegionGeometry {
        slots: 16,
        spin_ns: 100_000,
        spin_shift: 4,
        bulk_bytes: 4096,
        page: u64::try_from(rustix::param::page_size()).map_err(|_| IpcError::Layout {
          reason: "a page size wider than 64 bits",
        })?,
      };
      Ok(super::Prepared {
        region: crate::region::ClientRegion::create(
          &format!("slates-a187-{}-{made}", std::process::id()),
          client_id,
          0,
          geometry,
        )?,
        kick_fd: None,
      })
    }

    /// A connected socket to `instance`'s rendezvous that speaks only when the test makes it.
    fn raw_claimant(instance: &str) -> Result<OwnedFd, IpcError> {
      let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
      )
      .map_err(|e| super::refused("socket", e))?;
      rustix::net::connect(&socket, &super::address(instance)?)
        .map_err(|e| super::refused("connect", e))?;
      Ok(socket)
    }

    /// Reads `bytes.len()` bytes from `socket`, waiting: how many came before its end.
    fn read_whole(socket: &OwnedFd, bytes: &mut [u8]) -> Result<usize, IpcError> {
      let mut got = 0;
      while let Some(room) = bytes.get_mut(got..).filter(|room| !room.is_empty()) {
        match rustix::net::recv(socket, room, rustix::net::RecvFlags::empty()) {
          Ok((0, _)) => break,
          Ok((read, _)) => got += read,
          Err(rustix::io::Errno::INTR) => {}
          Err(e) => return Err(super::refused("recv", e)),
        }
      }
      Ok(got)
    }

    /// A-187 (§4.7 "Rendezvous"): a claimant that never sends its opening holds no round. Do: connect a socket that
    /// neither reads nor sends, then begin an honest claim, and serve rounds while polling the claim. Expect: the
    /// honest claim connects while the silent one is still pending (the listener names its descriptor), the silent one
    /// is given up once, no sooner than the claim wait, and its socket reads the daemon's public key and then its end.
    /// Before A-187 the daemon read each claimant's hello with a blocking receive, and a silent claimant held the round,
    /// and every claimant behind it, for as long as it stayed silent.
    #[test]
    fn a_claimant_that_never_sends_its_opening_holds_no_round() -> Result<(), IpcError> {
      use std::time::{Duration, Instant};
      crate::toy_seal::install();
      let instance = format!("test-silent-{}", std::process::id());
      let mut listener = super::super::Listener::open(&instance, None)?;
      let silent = raw_claimant(&instance)?;
      let mut claim = super::super::begin_connect_as(&instance, 0)?;
      let started = Instant::now();
      let give_up_by = Duration::from_nanos(super::super::CLAIM_WAIT_NS.saturating_mul(2));
      let mut refusals = Vec::new();
      let mut accepted = Vec::new();
      let mut connected_at = None;
      let mut pending_while_connected = 0;
      while started.elapsed() < give_up_by && (connected_at.is_none() || refusals.is_empty()) {
        let round = listener.accept_pending(&|_| false, &mut test_region, &mut |refusal| {
          refusals.push((refusal, started.elapsed()));
        });
        accepted.extend(round.accepted);
        if connected_at.is_none() && claim.poll()?.is_some() {
          connected_at = Some(started.elapsed());
          pending_while_connected = listener.pending().0.len();
        }
      }
      let Some(connected_at) = connected_at else {
        return Err(IpcError::Layout {
          reason: "the honest claim was never served beside the silent one",
        });
      };
      let [(IpcError::DeadlineExceeded, given_up_at)] = refusals.as_slice() else {
        return Err(IpcError::Layout {
          reason: "the silent claimant was not given up once at the claim wait",
        });
      };
      let mut key = [0u8; crate::seal::PUBLIC_KEY_BYTES + 1];
      let read = read_whole(&silent, &mut key)?;
      let shaped = accepted.len() == 1
        && pending_while_connected == 1
        && connected_at < *given_up_at
        && *given_up_at >= Duration::from_nanos(super::super::CLAIM_WAIT_NS)
        && listener.pending().0.is_empty()
        && read == crate::seal::PUBLIC_KEY_BYTES;
      if !shaped {
        return Err(IpcError::Layout {
          reason: "the silent claimant held the round, or was not given up at the claim wait",
        });
      }
      Ok(())
    }

    /// Shape: the reconnect budget a client gave its claim as a restart bound before A-468: the recovery budget and one
    /// liveness window, a second each (`slates_client::Deadlines::derive`'s `T + W`).
    const OLD_RECONNECT_BUDGET_NS: u64 = 2_000_000_000;
    /// Shape: how long the anchored test's restart takes: a quarter again the old reconnect budget.
    const SLOW_RESTART_NS: u64 = OLD_RECONNECT_BUDGET_NS + OLD_RECONNECT_BUDGET_NS / 4;
    /// Shape: the anchored test's pause between its polls: a hundredth of the claim wait.
    const RESTART_POLL_PAUSE_NS: u64 = super::super::CLAIM_WAIT_NS / 100;

    /// A-468 (R3's class C), Linux. Do: hold the instance's listening socket as an anchor does, open a daemon end over it
    /// that never accepts, and begin a claim; stop that daemon end, and only after a restart slower than the reconnect
    /// budget a client gave its claim before A-468 open another daemon end over the held socket and serve rounds while
    /// polling the claim. Expect: the claim due all the while (its connection queued in the held socket's backlog), then
    /// connected by the second daemon end. Before A-468 a client's claim was refused at that budget, the anchor still
    /// restarting its daemon.
    #[test]
    // The test thread stands for a client polling at its own pace: it pauses between its polls.
    #[allow(clippy::disallowed_methods)]
    fn a_claim_under_a_live_anchor_waits_out_a_restart_slower_than_the_old_reconnect_budget()
    -> Result<(), IpcError> {
      use std::time::{Duration, Instant};
      crate::toy_seal::install();
      let instance = format!("test-anchored-restart-{}", std::process::id());
      let anchor = super::super::HeldRendezvous::hold(&instance)?;
      let first = super::super::Listener::open(&instance, Some(&anchor.adoption()))?;
      let mut claim = super::super::begin_connect_as(&instance, 0)?;
      drop(first);
      let restart = Duration::from_nanos(SLOW_RESTART_NS);
      let started = Instant::now();
      while started.elapsed() < restart {
        if claim.poll()?.is_some() {
          return Err(IpcError::Layout {
            reason: "the claim was answered with no daemon serving",
          });
        }
        std::thread::sleep(Duration::from_nanos(RESTART_POLL_PAUSE_NS));
      }
      let mut second = super::super::Listener::open(&instance, Some(&anchor.adoption()))?;
      let served_by = Instant::now();
      while served_by.elapsed() < restart {
        let _round = second.accept_pending(&|_| false, &mut test_region, &mut |_| {});
        if claim.poll()?.is_some() {
          return Ok(());
        }
      }
      Err(IpcError::Layout {
        reason: "the claim queued under the anchor was never answered by the restarted daemon",
      })
    }

    /// A-468, Linux. Do: hold the instance's listening socket as an anchor does, begin a claim no daemon end accepts,
    /// then let the anchor go, and poll the claim. Expect: refused at once (its queued connection reset with the socket),
    /// and `holder_lives` true while the anchor held the socket and false after.
    #[test]
    fn a_claim_under_an_anchor_ends_at_the_anchors_going() -> Result<(), IpcError> {
      use std::time::{Duration, Instant};
      crate::toy_seal::install();
      let _gate = crate::descriptor_test_gate();
      let instance = format!("test-anchor-gone-{}", std::process::id());
      let anchor = super::super::HeldRendezvous::hold(&instance)?;
      let held = super::super::holder_lives(&instance)?;
      let mut claim = super::super::begin_connect_as(&instance, 0)?;
      let gone = Instant::now();
      drop(anchor);
      let ended = loop {
        match claim.poll() {
          Ok(None) => {}
          Ok(Some(_)) => {
            return Err(IpcError::Layout {
              reason: "a claim with no daemon was answered",
            });
          }
          Err(refusal) => break refusal,
        }
        if gone.elapsed() > Duration::from_nanos(super::super::CLAIM_WAIT_NS) {
          return Err(IpcError::Layout {
            reason: "the claim outlived its anchor by a claim wait",
          });
        }
      };
      let after = super::super::holder_lives(&instance)?;
      if !held || after || !matches!(ended, IpcError::DaemonUnavailable { .. }) {
        return Err(IpcError::Layout {
          reason: "the anchor's holding or its going was misread",
        });
      }
      Ok(())
    }

    /// A-187: an opening that does not open is refused and asks for no region. Do: connect a raw claimant, let a round
    /// send it the daemon's key, and answer with a public key and a hello sealed under a channel agreed with another
    /// key. Expect: the next round refuses it `SealRefused` without asking for a region, and closes its socket.
    #[test]
    fn an_opening_that_does_not_open_is_refused_and_asks_for_no_region() -> Result<(), IpcError> {
      crate::toy_seal::install();
      let instance = format!("test-forged-{}", std::process::id());
      let mut listener = super::super::Listener::open(&instance, None)?;
      let forger = raw_claimant(&instance)?;
      let mut regions_asked = 0u32;
      let mut refusals = Vec::new();
      let mut serve = |listener: &mut super::super::Listener, refusals: &mut Vec<IpcError>| {
        listener.accept_pending(
          &|_| false,
          &mut |client_id| {
            regions_asked += 1;
            test_region(client_id)
          },
          &mut |refusal| refusals.push(refusal),
        )
      };
      let begun = serve(&mut listener, &mut refusals);
      let mut key = [0u8; crate::seal::PUBLIC_KEY_BYTES];
      let read = read_whole(&forger, &mut key)?;
      let (_, mut elsewhere) = crate::toy_seal::channel_pair()?;
      let mut opening = crate::toy_seal::make()?.public_key().to_vec();
      opening.extend_from_slice(&elsewhere.seal(crate::seal::Class::Hello, &0u32.to_le_bytes())?);
      rustix::net::send(&forger, &opening, rustix::net::SendFlags::NOSIGNAL)
        .map_err(|e| super::refused("send", e))?;
      let refused = serve(&mut listener, &mut refusals);
      let mut rest = [0u8; 1];
      let after = read_whole(&forger, &mut rest)?;
      let shaped = begun.accepted.is_empty()
        && refused.accepted.is_empty()
        && read == crate::seal::PUBLIC_KEY_BYTES
        && matches!(refusals.as_slice(), [IpcError::SealRefused { .. }])
        && regions_asked == 0
        && after == 0;
      if !shaped {
        return Err(IpcError::Layout {
          reason: "an opening that does not open was served, or not refused as one",
        });
      }
      Ok(())
    }
  }
}

#[cfg(any(target_os = "macos", windows))]
pub mod platform {
  //! macOS and Windows: the bootstrap object with claim slots.

  use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

  use slates_mem::{Handoff, SharedObject, Width, WordRun, Words};

  use super::{Accepted, CLAIM_WAIT_NS, Connected, Doorbell, Prepared, Served, rendezvous_name};
  use crate::error::IpcError;
  use crate::region::ClientRegion;
  #[cfg(not(windows))]
  use crate::wake;

  /// Format: the bootstrap object's magic, `SLB2` in little-endian ASCII: the layout with the rings' marks (A-381). The
  /// layout before it, `SLBT`, had no marks, and a client of it rings without naming its shard, so an object or a client
  /// of either layout is refused by the other as another layout, never served with its rings unheard.
  ///
  /// `SLB3` (A-447) is that layout with the daemon's process mark in its header ([`ProcessMark`]): a Windows claimant of
  /// it waits on the daemon's exit, and one of `SLB2` would read no mark, so each refuses the other as another layout.
  /// `SLB4` (A-468) adds the holding anchor's creation time beside its process id: a Windows claimant under a live
  /// anchor waits on the anchor's exit, judged by both, and one of `SLB3` would read none, so each refuses the other.
  /// `SLB5` (A-522) adds the claimant's start to each claim slot (`AT_STARTED`): a daemon of it watches the client's
  /// process judged by its pid and its start, and a claimant of `SLB4` writes no start, so each refuses the other.
  /// Format: `SLB5`'s four ASCII bytes read as a little-endian `u32`.
  const MAGIC: u32 = 0x3542_4C53;
  /// Format: the bootstrap header: magic (4), slots (4), the daemon-wide doorbell word (4), the claim's mark (4,
  /// A-381), the daemon's start stamp (8), the daemon's process id (4, A-447), the holding anchor's process id (4), the
  /// daemon's creation time (8, A-447), the holding anchor's creation time (8, A-468), padding to a cache line. The
  /// daemon's process id held the slot after the stamp until A-159, when a client judged its daemon by the pid in the
  /// answer alone (A-143); a Windows claim reads it again with the creation time, to wait on the daemon's exit before any
  /// answer (A-447).
  /// Format: 64 bytes, the fields above and their padding.
  const HEADER_BYTES: usize = 64;
  /// Format: the daemon's process id's offset in the header (A-447), written by a Windows daemon before it publishes
  /// the header; zero where none is written (a macOS daemon, whose claimants judge the owner's bootstrap name, A-443).
  #[cfg(any(windows, test))]
  const AT_DAEMON_PID: usize = 24;
  /// Format: the daemon's creation time's offset in the header (A-447): `GetProcessTimes`' creation time, 100 ns
  /// intervals since 1601, which tells the daemon's process from another given its pid after it.
  #[cfg(any(windows, test))]
  const AT_DAEMON_CREATED: usize = 32;
  /// Format: the slot count's offset in the header (after the magic).
  const AT_SLOT_COUNT: usize = 4;
  /// Format: the doorbell word's offset in the header.
  pub const AT_DOORBELL: usize = 8;
  /// Format: the claim's mark in the header (A-381): a client's claim sets it before it rings, and the doorbell thread
  /// kicks the control shard, which answers claims, only when it finds it set.
  const AT_CLAIM_RUNG: usize = 12;
  /// Format: the start stamp's offset in the header: the wall clock in nanoseconds when the
  /// daemon opened the object, so a client that remembers it tells a restarted daemon (a new
  /// stamp) from a slow one (the same stamp).
  const AT_GENERATION: usize = 16;
  /// Format: the holding anchor's process id's offset in the header (A-114): written when the anchor creates the
  /// object, zero in an object a daemon made for itself. A Windows claim made while this process lives waits while it
  /// lives, judged with its creation time (A-468); a macOS claim judges the owner's name the anchor holds.
  const AT_ANCHOR_PID: usize = 28;
  /// Format: the holding anchor's creation time's offset in the header (A-468), as the daemon's is at
  /// `AT_DAEMON_CREATED`: written when a Windows anchor creates the object, zero elsewhere.
  #[cfg(any(windows, test))]
  const AT_ANCHOR_CREATED: usize = 40;
  /// Shape: claim slots in the bootstrap object: clients connecting inside one control-shard
  /// loop; the loop drains them, so the table only covers one loop of arrivals.
  const SLOTS: usize = 64;
  /// Format: a claim slot: state (4), client pid (4), client id (4), padding (4), region
  /// length (8), region name (40), the answering daemon's start stamp (8) and process id (4),
  /// padding (4), the claimant's start (8, A-522), padding to two cache lines.
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
  /// Format: the offset in a slot of the claimant's start (A-522): when its process started, written by the claimant
  /// beside its pid (`ExitWatch::own_start`: macOS the start time the kernel keeps for the process, in microseconds
  /// since 1970; Windows its creation time, in 100 ns intervals since 1601), so the daemon's watch on the client is
  /// taken on that process alone and a pid reused before the watch is never taken for it.
  const AT_STARTED: usize = 80;
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
      _mapping: super::Mapping,
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
      _resync_follows: bool,
    ) -> Result<(), IpcError> {
      Err(IpcError::Unsupported {
        feature: "a stream end over the control socket (macOS has the socket route)",
      })
    }

    /// No control socket here (A-316): a macOS stream is handed no resync end yet.
    pub fn send_resync(
      &self,
      _end: std::os::fd::BorrowedFd<'_>,
      _request_id: u64,
      _attachment: u64,
    ) -> Result<(), IpcError> {
      Err(IpcError::Unsupported {
        feature: "a resync end over the control socket (macOS hands none yet)",
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

    /// The daemon's process handle (Windows; [`super::Liveness::exit_handle`]).
    #[cfg(windows)]
    pub(super) fn exit_handle(&self) -> Option<std::os::windows::io::BorrowedHandle<'_>> {
      self.watch.handle()
    }

    /// A liveness check on `instance`'s daemon whose process is `pid` ([`super::Liveness::of_daemon`]).
    #[cfg(test)]
    pub(super) fn of_daemon(instance: &str, pid: u32) -> Result<Liveness, IpcError> {
      let generation = generation_of(instance).ok_or_else(|| IpcError::DaemonUnavailable {
        endpoint: instance.to_owned(),
        why: "the daemon has not yet published its rendezvous object",
      })?;
      Ok(Liveness {
        instance: instance.to_owned(),
        generation,
        watch: crate::exit_watch::ExitWatch::on(pid)?,
      })
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
    /// The instance: for the doorbell waiters' own handles (Windows), and the token its regions are kept with (macOS).
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

    /// Rings for the client of `shard` (A-381): marks the shard pending, its pending word in the summary, then rings.
    /// The marks come before the word moves, so the thread that sees the word move finds them.
    pub(super) fn ring_for(&self, shard: u16) -> Result<(), IpcError> {
      let (word, bit) = word_and_bit(usize::from(shard));
      mark(&self.object, AT_PENDING, word)?.fetch_or(bit, Ordering::Release);
      let (summary, summary_bit) = word_and_bit(word);
      mark(&self.object, AT_SUMMARY, summary)?.fetch_or(summary_bit, Ordering::Release);
      self.ring()
    }

    /// Rings for a claim (A-381): marks the claim, then rings.
    pub(super) fn ring_claim(&self) -> Result<(), IpcError> {
      self
        .object
        .atomic_u32(AT_CLAIM_RUNG)?
        .store(1, Ordering::Release);
      self.ring()
    }

    /// What the rings since the last call marked, taken (A-381): the claim's mark, and each shard marked, found
    /// through the summary words, so only a pending word a ring touched is read.
    pub(super) fn take_pending(&self) -> Result<super::Pending, IpcError> {
      let claim = self
        .object
        .atomic_u32(AT_CLAIM_RUNG)?
        .swap(0, Ordering::AcqRel)
        != 0;
      let mut shards = Vec::new();
      for summary in 0..SUMMARY_WORDS {
        let touched = mark(&self.object, AT_SUMMARY, summary)?.swap(0, Ordering::AcqRel);
        for word in set_bits(summary, touched) {
          let bits = mark(&self.object, AT_PENDING, word)?.swap(0, Ordering::AcqRel);
          shards.extend(set_bits(word, bits).filter_map(|shard| u16::try_from(shard).ok()));
        }
      }
      Ok(super::Pending { claim, shards })
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

  /// Format: the bits one mark word, a `u64`, holds.
  const MARK_BITS: usize = 64;
  /// Format: a mark word's bytes.
  const MARK_BYTES: usize = size_of::<u64>();
  /// Format: the shard ids a ring can mark (A-381): a region names its shard as a `u16` (`crate::region`), so every id
  /// has a bit and no shard a ring names is ever left unmarked.
  const SHARD_IDS: usize = 1 << u16::BITS;
  /// Format: the pending words, a bit a shard id (A-381).
  const PENDING_WORDS: usize = SHARD_IDS / MARK_BITS;
  /// Format: the summary words, a bit a pending word, so the doorbell thread takes only the pending words a ring
  /// touched and not all of them at every ring.
  const SUMMARY_WORDS: usize = PENDING_WORDS / MARK_BITS;
  /// Format: the summary's offset, after the claim slots.
  const AT_SUMMARY: usize = HEADER_BYTES + SLOTS * SLOT_BYTES;
  /// Format: the pending words' offset, after the summary.
  const AT_PENDING: usize = AT_SUMMARY + SUMMARY_WORDS * MARK_BYTES;
  /// Format: the bootstrap object's length: the header, the claim slots, the summary and the pending words.
  const OBJECT_BYTES: usize = AT_PENDING + PENDING_WORDS * MARK_BYTES;

  /// The bootstrap object's declared atomic words (AUD-29-09): the doorbell, the claim's mark, the start stamp (the
  /// header's publication word), every claim slot's state word, and the rings' marks (A-381); the rest is plain, owned
  /// by whichever side the slot's state names.
  fn bootstrap_words() -> Words {
    Words::new()
      .with(WordRun::one(AT_DOORBELL, Width::U32))
      .with(WordRun::one(AT_CLAIM_RUNG, Width::U32))
      .with(WordRun::one(AT_GENERATION, Width::U64))
      .with(WordRun::strided(
        HEADER_BYTES + AT_STATE,
        SLOT_BYTES,
        SLOTS,
        Width::U32,
      ))
      .with(WordRun::strided(
        AT_SUMMARY,
        MARK_BYTES,
        SUMMARY_WORDS,
        Width::U64,
      ))
      .with(WordRun::strided(
        AT_PENDING,
        MARK_BYTES,
        PENDING_WORDS,
        Width::U64,
      ))
  }

  /// The mark word at `index` of the run at `first`.
  fn mark(object: &SharedObject, first: usize, index: usize) -> Result<&AtomicU64, IpcError> {
    let offset = index
      .checked_mul(MARK_BYTES)
      .and_then(|bytes| bytes.checked_add(first))
      .ok_or(IpcError::Layout {
        reason: "a ring's mark past the bootstrap object",
      })?;
    Ok(object.atomic_u64(offset)?)
  }

  /// The word and the bit within it that stand for `index` among words of [`MARK_BITS`] bits.
  fn word_and_bit(index: usize) -> (usize, u64) {
    let word = index.checked_div(MARK_BITS).unwrap_or(0);
    let bit = index.checked_rem(MARK_BITS).unwrap_or(0);
    (
      word,
      1u64
        .checked_shl(u32::try_from(bit).unwrap_or(0))
        .unwrap_or(0),
    )
  }

  /// Each index whose bit is set in `bits`, the word's `word`th, ascending.
  fn set_bits(word: usize, mut bits: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
      if bits == 0 {
        return None;
      }
      let low = usize::try_from(bits.trailing_zeros()).unwrap_or(0);
      bits &= bits.wrapping_sub(1);
      word.checked_mul(MARK_BITS)?.checked_add(low)
    })
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
    /// The token, served on a thread of its own, through which every client of the instance fetches its region
    /// (GAP-A9-9, `crate::mach_claim`).
    #[cfg(target_os = "macos")]
    _service: crate::mach_claim::OwnerService,
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
    Ok(Owner {
      _service: crate::mach_claim::OwnerService::start(token, SLOTS, STALE_SLOT_NS)?,
    })
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
      // Windows: the anchor's creation time beside its pid, so a claimant judges the anchor's process by both (A-468).
      #[cfg(windows)]
      object.write(
        AT_ANCHOR_CREATED,
        &crate::exit_watch::ExitWatch::own_creation()?.to_le_bytes(),
      )?;
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
      #[cfg_attr(not(windows), allow(unused_mut))]
      let (mut object, owner) = if adoption.is_some() {
        (adopt(instance)?, None)
      } else {
        let (object, owner) = fresh(instance)?;
        (object, Some(owner))
      };
      // Windows: this daemon's process mark, which its claimants wait on the exit of (A-447), before the stamp publishes it.
      #[cfg(windows)]
      {
        let (pid, created) = own_mark()?.bytes();
        object.write(AT_DAEMON_PID, &pid)?;
        object.write(AT_DAEMON_CREATED, &created)?;
      }
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

    /// No handshakes wait here: a claim is answered in the bootstrap object, whole, in the round that finds it.
    pub(super) fn pending(&self) -> (Vec<i32>, Option<u64>) {
      (Vec::new(), None)
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
      let started = read_u64(&self.object, at.saturating_add(AT_STARTED))?;
      let client_id = assign(wanted);
      let Prepared { mut region, .. } = match make_region(client_id) {
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
      let (name, len) = self.hand_over(index, pid, &mut region).map_err(lost)?;
      write_answer(&mut self.object, at, client_id, len, &name).map_err(lost)?;
      write_answerer(&mut self.object, at, self.stamp, current_pid()).map_err(lost)?;
      self.publish(index, READY).map_err(lost)?;
      Ok(Accepted {
        client_id,
        // The object's mode and per-user name are the authentication: whoever opened
        // it is the daemon's user (the kernel refused everyone else).
        uid: current_uid(),
        pid,
        started,
        region,
        control: Some(Control),
      })
    }

    /// What the claim in slot `index`, made by process `pid`, is answered with, and the region's length (macOS,
    /// GAP-A9-9): no name. The region's name is unlinked, so no process can open it by name, and the region is kept with
    /// the instance's token for that claim and that process alone (`crate::mach_claim`), before the answer is published.
    #[cfg(target_os = "macos")]
    fn hand_over(
      &self,
      index: usize,
      pid: u32,
      region: &mut ClientRegion,
    ) -> Result<(String, usize), IpcError> {
      region.unlink_name()?;
      let len = region.object().len();
      crate::mach_claim::keep(
        &owner_token_name(&self.instance),
        index,
        pid,
        region.object().as_fd(),
      )?;
      Ok((String::new(), len))
    }

    /// What the claim in slot `index` is answered with (Windows): the region's name, which the claimant opens.
    #[cfg(windows)]
    fn hand_over(
      &self,
      _index: usize,
      _pid: u32,
      region: &mut ClientRegion,
    ) -> Result<(String, usize), IpcError> {
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
      Ok((name, len))
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

  /// Claims a free slot: `FREE` → `CLAIMING`, the pid, its start and the wanted id written, then `CLAIMED` with
  /// release, so the daemon never reads the fields before they are written. A slot the daemon reclaimed
  /// meanwhile (this claimant was stalled past the stale bound) is given up and another claimed.
  fn claim(object: &mut SharedObject, wanted: u32) -> Result<usize, IpcError> {
    // Read before a slot is taken, so a refusal leaves no slot claimed.
    let started = crate::exit_watch::ExitWatch::own_start()?;
    for index in 0..SLOTS {
      if state(object, index)?
        .compare_exchange(FREE, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
      {
        continue;
      }
      let at = slot_at(index);
      object.write(at.saturating_add(AT_PID), &current_pid().to_le_bytes())?;
      // With its pid, its start: the two name this process alone, so the daemon's watch is not taken on a reuser (A-522).
      object.write(at.saturating_add(AT_STARTED), &started.to_le_bytes())?;
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
    /// How the claim ends if nothing answers it ([`Ends`]).
    ends: Ends,
    /// The time waited at which the owner's life is next looked at ([`Ends::WithOwner`]): at once, then twice the time
    /// waited at each look, so the looks cost the logarithm of the wait and a death is seen within the time waited.
    #[cfg(target_os = "macos")]
    next_look_ns: u64,
    /// The start stamp published when the claim began (Windows): a daemon that stops takes it back (`Listener`'s drop,
    /// A-176), which is how a claim on a daemon stopped inside its own process, whose exit nobody sees, learns it went.
    #[cfg(windows)]
    stamp: u64,
    /// Whether the slot is still this client's to give back (cleared once answered or refused).
    open: bool,
  }

  /// A process's mark in the bootstrap header: its process id and its creation time, which a claimant opens the process
  /// by and judges it by, so a pid reused after the process's exit is never taken for it. The daemon's (A-447) and the
  /// holding anchor's (A-468).
  #[cfg(any(windows, test))]
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  struct ProcessMark {
    /// The process id.
    pid: u32,
    /// Its creation time (`GetProcessTimes`, 100 ns intervals since 1601).
    created: u64,
  }

  #[cfg(any(windows, test))]
  impl ProcessMark {
    /// Its bytes as the header holds them, little-endian: the process id's and the creation time's.
    fn bytes(self) -> ([u8; size_of::<u32>()], [u8; size_of::<u64>()]) {
      (self.pid.to_le_bytes(), self.created.to_le_bytes())
    }

    /// The mark a header's image holds at `pid_at` and `created_at`: `None` for none written (a zero process id or
    /// creation time, which no live process has) or an image too short to hold one.
    fn at(header: &[u8], pid_at: usize, created_at: usize) -> Option<ProcessMark> {
      let pid = header
        .get(pid_at..pid_at.checked_add(size_of::<u32>())?)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)?;
      let created = header
        .get(created_at..created_at.checked_add(size_of::<u64>())?)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)?;
      (pid != 0 && created != 0).then_some(ProcessMark { pid, created })
    }

    /// The daemon's mark (A-447).
    fn daemon_of(header: &[u8]) -> Option<ProcessMark> {
      Self::at(header, AT_DAEMON_PID, AT_DAEMON_CREATED)
    }

    /// The holding anchor's mark (A-468).
    fn anchor_of(header: &[u8]) -> Option<ProcessMark> {
      Self::at(header, AT_ANCHOR_PID, AT_ANCHOR_CREATED)
    }
  }

  /// This process's mark (Windows), written into the header before the daemon publishes it.
  #[cfg(windows)]
  fn own_mark() -> Result<ProcessMark, IpcError> {
    Ok(ProcessMark {
      pid: std::process::id(),
      created: crate::exit_watch::ExitWatch::own_creation()?,
    })
  }

  /// How a claim nothing answers ends: with the rendezvous's holder (A-443, A-447, A-468).
  #[derive(Debug)]
  enum Ends {
    /// When the rendezvous's owner is found gone (macOS): the anchor that holds it, or a daemon that holds its own; its
    /// bootstrap name is looked up on A-443's doubling.
    #[cfg(target_os = "macos")]
    WithOwner,
    /// When the daemon goes (Windows, A-447): its process exits, opened by the header's mark and judged by its creation
    /// time, whose handle a wait takes beside the claim's Event; or it stops inside its process and takes back its start
    /// stamp. No live anchor holds the rendezvous.
    #[cfg(windows)]
    WithDaemon(crate::exit_watch::ExitWatch),
    /// When the anchor that holds the rendezvous exits (Windows, A-468), opened by its mark as the daemon's is: its
    /// daemon may restart any number of times meanwhile, and the next one answers the claim (A-114).
    #[cfg(windows)]
    WithAnchor(crate::exit_watch::ExitWatch),
  }

  /// How a claim on macOS ends unanswered: with the rendezvous's owner, whose bootstrap name launchd takes back at its
  /// death ([`owner_lives`]). An anchor that holds the rendezvous holds the name, so a restart under it is waited out.
  #[cfg(target_os = "macos")]
  fn unanswered_end(_object: &SharedObject) -> Result<Ends, IpcError> {
    Ok(Ends::WithOwner)
  }

  /// The paired arm (Windows): with the anchor the header names while its process lives (A-468), else with the daemon
  /// the header names (A-447); each process the claim opens for `SYNCHRONIZE` and a limited query alone and judges by
  /// its creation time. Opening the owner's named Event instead would keep its name and refuse a successor its ownership
  /// while the probe held it (A-176); a process handle keeps no object the process named. A header with no daemon mark is
  /// refused typed: every daemon of this layout writes one before it publishes.
  #[cfg(windows)]
  fn unanswered_end(object: &SharedObject) -> Result<Ends, IpcError> {
    let mut header = [0u8; HEADER_BYTES];
    object.read(0, &mut header)?;
    if let Some(anchor) = ProcessMark::anchor_of(&header) {
      let watch = crate::exit_watch::ExitWatch::on_created(anchor.pid, anchor.created)?;
      if !watch.exited() {
        return Ok(Ends::WithAnchor(watch));
      }
    }
    let mark = ProcessMark::daemon_of(&header).ok_or(IpcError::Layout {
      reason: "the bootstrap header names no daemon process",
    })?;
    Ok(Ends::WithDaemon(crate::exit_watch::ExitWatch::on_created(
      mark.pid,
      mark.created,
    )?))
  }

  /// Whether `instance`'s rendezvous has a live holder (macOS; `super::holder_lives`): its owner's bootstrap name is
  /// held, by the anchor that holds the rendezvous or by a daemon holding its own.
  #[cfg(target_os = "macos")]
  pub(super) fn holder_lives(instance: &str) -> Result<bool, IpcError> {
    owner_lives(instance)
  }

  /// Whether `instance`'s rendezvous has a live holder (Windows; `super::holder_lives`): the anchor its header names
  /// lives, or else the daemon it names lives and its start stamp is published (a daemon stopped inside its process
  /// took it back). No object by the name reads none. A header not yet written reads held, since its maker lives while
  /// it writes it (and an object whose maker died writing it goes with the last handle, so the next look reads none).
  /// A header of another layout is refused as a claim refuses it.
  #[cfg(windows)]
  pub(super) fn holder_lives(instance: &str) -> Result<bool, IpcError> {
    use slates_mem::MemError;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    let handoff =
      SharedObject::handoff_for_name(&rendezvous_name(instance)).ok_or(IpcError::Unsupported {
        feature: "rendezvous by name",
      })?;
    let object = match SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words()) {
      Ok(object) => object,
      Err(MemError::OsRefused { code, .. }) if code == i32::try_from(ERROR_FILE_NOT_FOUND).ok() => {
        return Ok(false);
      }
      Err(e) => return Err(e.into()),
    };
    match read_u32(&object, 0)? {
      0 => return Ok(true),
      MAGIC => {}
      _ => {
        return Err(IpcError::Layout {
          reason: "bootstrap object has the wrong magic",
        });
      }
    }
    let mut header = [0u8; HEADER_BYTES];
    object.read(0, &mut header)?;
    if mark_lives(ProcessMark::anchor_of(&header))? {
      return Ok(true);
    }
    let published = object.atomic_u64(AT_GENERATION)?.load(Ordering::Acquire) != 0;
    Ok(published && mark_lives(ProcessMark::daemon_of(&header))?)
  }

  /// Whether the process `mark` names lives (Windows): none named reads not.
  #[cfg(windows)]
  fn mark_lives(mark: Option<ProcessMark>) -> Result<bool, IpcError> {
    Ok(match mark {
      Some(mark) => !crate::exit_watch::ExitWatch::on_created(mark.pid, mark.created)?.exited(),
      None => false,
    })
  }

  /// Whether `instance`'s rendezvous has a live owner (macOS): its token's bootstrap name is held, which launchd takes
  /// back the moment its holder's receive right dies (`crate::owner_name`, measured there). The look holds nothing of
  /// the name: a send right to a port keeps neither the port's receive right nor the name alive.
  #[cfg(target_os = "macos")]
  fn owner_lives(instance: &str) -> Result<bool, IpcError> {
    crate::owner_name::look_up(&owner_token_name(instance)).map(|found| found.is_some())
  }

  /// Claims a slot and rings the daemon-wide doorbell so a parked control shard sees the claim; nothing
  /// waits.
  pub(super) fn begin(instance: &str, wanted: u32) -> Result<Claim, IpcError> {
    let (handoff, mut object) = open_bootstrap(instance)?;
    // The claim waits while the rendezvous's holder lives: the anchor that holds it, whose daemon may be restarting
    // (A-114), or the daemon that holds its own.
    let ends = unanswered_end(&object)?;
    #[cfg(windows)]
    let stamp = object.atomic_u64(AT_GENERATION)?.load(Ordering::Acquire);
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
    let bell = match bell.and_then(|bell| bell.ring_claim().map(|()| bell)) {
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
      ends,
      #[cfg(target_os = "macos")]
      next_look_ns: 0,
      #[cfg(windows)]
      stamp,
      open: true,
    })
  }

  impl Claim {
    /// The answer, without waiting: the slot `READY` or `REFUSED` is read and finished; otherwise the claim is due
    /// until it ends as [`Ends`] says, with the rendezvous's holder.
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
      match &self.ends {
        #[cfg(target_os = "macos")]
        Ends::WithOwner => self.look(),
        #[cfg(windows)]
        Ends::WithDaemon(daemon) => {
          if daemon.exited() {
            self.daemon_went("the daemon exited before answering the claim")
          } else if self
            .object
            .atomic_u64(AT_GENERATION)?
            .load(Ordering::Acquire)
            != self.stamp
          {
            self.daemon_went(
              "the daemon stopped before answering the claim: it took back its start stamp",
            )
          } else {
            Ok(None)
          }
        }
        #[cfg(windows)]
        Ends::WithAnchor(anchor) => {
          if anchor.exited() {
            self.daemon_went(
              "the anchor that holds the rendezvous exited before the claim was answered",
            )
          } else {
            Ok(None)
          }
        }
      }
    }

    /// The claim's holder went with the claim unanswered (Windows, A-447, A-468): the daemon's process exited, or it
    /// stopped inside the process and took back its start stamp, or the anchor that holds the rendezvous exited. The
    /// claim is taken back if the daemon never took it (one that took it went mid-answer, its slot left to a successor's
    /// stale reclaim) and reported unavailable, saying `why`.
    #[cfg(windows)]
    fn daemon_went(&mut self, why: &'static str) -> Result<Option<Connected>, IpcError> {
      let _taken_back = state(&self.object, self.index)?.compare_exchange(
        CLAIMED,
        FREE,
        Ordering::AcqRel,
        Ordering::Acquire,
      );
      self.open = false;
      Err(IpcError::DaemonUnavailable {
        endpoint: self.instance.clone(),
        why,
      })
    }

    /// A claim that waits while its rendezvous's owner lives (A-443): due while it does, looked at when the look is due;
    /// once the owner is found gone, the claim is taken back if the daemon never took it (one that took it died
    /// mid-answer, its slot left to a successor's stale reclaim) and reported unavailable.
    #[cfg(target_os = "macos")]
    fn look(&mut self) -> Result<Option<Connected>, IpcError> {
      let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
      if elapsed < self.next_look_ns {
        return Ok(None);
      }
      if owner_lives(&self.instance)? {
        self.next_look_ns = elapsed.saturating_mul(2).max(elapsed.saturating_add(1));
        return Ok(None);
      }
      let _taken_back = state(&self.object, self.index)?.compare_exchange(
        CLAIMED,
        FREE,
        Ordering::AcqRel,
        Ordering::Acquire,
      );
      self.open = false;
      Err(IpcError::DaemonUnavailable {
        endpoint: self.instance.clone(),
        why: "the daemon's rendezvous has no live owner: its daemon died before answering the claim",
      })
    }

    /// Waits for the answer between polls: on the slot's state word until the owner's next look (macOS), or on the
    /// slot's Event beside the holder's process (Windows).
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
        #[cfg(target_os = "macos")]
        let _ = wake::wait(word, now, Some(self.next_look_ns.saturating_sub(elapsed)))?;
        #[cfg(windows)]
        self.wait_on_holder(elapsed)?;
      }
    }

    /// One wait for the answer's Event beside the holder's process (Windows). Beside the daemon's, or as long again as
    /// the claim has waited, when the poll reads the start stamp, which a daemon stopped inside its own process takes
    /// back (A-443's doubling, so the looks cost the logarithm of the wait). Beside the anchor's with no timeout: its
    /// daemon's restarts change nothing the claim waits on. A process already gone is read at the poll.
    #[cfg(windows)]
    fn wait_on_holder(&self, elapsed: u64) -> Result<(), IpcError> {
      let (process, timeout_ns) = match &self.ends {
        Ends::WithDaemon(daemon) => (daemon.handle(), Some(elapsed.max(1))),
        Ends::WithAnchor(anchor) => (anchor.handle(), None),
      };
      if let Some(process) = process {
        self.ready_event.wait_or_exit(process, timeout_ns)?;
      }
      Ok(())
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
      // The region is fetched while the slot is still this claim's, so no later claim of the slot is kept in its place.
      let region = self.region(name, len);
      finish(&self.object, self.index, READY, &self.instance)?;
      let (stamp, daemon_pid) = answerer?;
      let region = region?;
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

  impl Claim {
    /// The answered claim's region (macOS, GAP-A9-9): fetched from the instance's token, which keeps it for this claim
    /// and this process alone; the answer names nothing.
    #[cfg(target_os = "macos")]
    fn region(&self, _name: String, len: usize) -> Result<ClientRegion, IpcError> {
      // Waited for while the owner lives (A-452).
      let fd = crate::mach_claim::fetch(
        &self.instance,
        &owner_token_name(&self.instance),
        self.index,
      )?;
      ClientRegion::from_descriptor(fd, len)
    }

    /// The answered claim's region (Windows): opened by the name the answer carries.
    #[cfg(windows)]
    fn region(&self, name: String, len: usize) -> Result<ClientRegion, IpcError> {
      ClientRegion::open(&Handoff::Name(name), len)
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
        Some(Served::Pending) => Err(IpcError::Layout {
          reason: "the bootstrap object begins no handshake: a claim is answered whole",
        }),
        None => Ok(None),
      }
    }

    /// A client's own mapping of `name`'s bootstrap object.
    fn client_mapping(name: &str) -> SharedObject {
      let handoff = SharedObject::handoff_for_name(&rendezvous_name(name)).unwrap();
      SharedObject::open(&handoff, OBJECT_BYTES, bootstrap_words()).unwrap()
    }

    /// A-381 (§4.7, D-10). Do: clients ring for shards 3, 64, 3 again and 65,535, the last id a region can name, then a
    /// claim rings; the daemon's thread takes what the rings marked, then takes again. Expect: the word moved once a
    /// ring, so the thread is woken for each; the first take holds the claim and exactly shards 3, 64 and 65,535, each
    /// once, in order; the second holds nothing. Before A-381 a ring marked nothing, and the thread kicked every shard.
    #[test]
    fn a_ring_marks_only_the_shard_it_names_and_a_claim_marks_the_claim() {
      let name = instance("marks");
      let listener = Listener::open(&name, None).unwrap();
      let waiter = listener.doorbell_waiter().unwrap().unwrap();
      let before = waiter.current().unwrap();
      let bell = Bell::new(client_mapping(&name), &name).unwrap();
      for shard in [3u16, 64, 3, u16::MAX] {
        bell.ring_for(shard).unwrap();
      }
      bell.ring_claim().unwrap();
      assert_eq!(waiter.current().unwrap(), before.wrapping_add(5));
      assert_eq!(
        waiter.take_pending().unwrap(),
        crate::rendezvous::Pending {
          claim: true,
          shards: vec![3, 64, u16::MAX],
        }
      );
      assert_eq!(
        waiter.take_pending().unwrap(),
        crate::rendezvous::Pending::default()
      );
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

    /// A-522 (golden vector). Do: claim a slot as a client does, and accept it as the daemon does. Expect: the
    /// claimant's start (`ExitWatch::own_start`) eight bytes little-endian at offset 80 of its slot beside its pid at
    /// 4, and the accepted client carrying both, the pair its watch is taken on.
    #[test]
    fn a_claim_names_its_process_by_its_pid_and_its_start() {
      let name = instance("started");
      let mut listener = Listener::open(&name, None).unwrap();
      let mut client = client_mapping(&name);
      let index = claim(&mut client, 0).unwrap();
      let started = crate::exit_watch::ExitWatch::own_start().unwrap();
      let mut word = [0u8; size_of::<u64>()];
      client.read(slot_at(index) + 80, &mut word).unwrap();
      assert_eq!(u64::from_le_bytes(word), started);
      let accepted = accept(&mut listener, "started")
        .unwrap()
        .expect("the claim is answered");
      assert_eq!(accepted.pid, current_pid());
      assert_eq!(accepted.started, started);
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

    /// Shape: how long the late-answer test leaves its claim unanswered: half again the claim wait, past the second a
    /// claim gave up at before A-443.
    #[cfg(target_os = "macos")]
    const LATE_NS: u64 = CLAIM_WAIT_NS + CLAIM_WAIT_NS / 2;
    /// Shape: the late-answer test's pause between its polls: a hundredth of the claim wait, so it polls about a hundred
    /// and fifty times through the wait without spinning a CPU.
    #[cfg(target_os = "macos")]
    const POLL_PAUSE_NS: u64 = CLAIM_WAIT_NS / 100;

    /// A-443 (R3's class C). Do: begin a claim with no restart bound on a daemon end that answers it only after half
    /// again the claim wait, polling it meanwhile; then answer it. Expect: the claim due all the while, then connected
    /// to the region the daemon end made for it. Before, the claim was refused `DaemonUnavailable` at the claim wait, a
    /// second, with its daemon alive.
    #[cfg(target_os = "macos")]
    #[test]
    // The test thread stands for a client polling at its own pace: it pauses between its polls.
    #[allow(clippy::disallowed_methods)]
    fn a_claim_on_a_daemon_that_answers_past_the_claim_wait_is_served() {
      let name = instance("late");
      let mut listener = Listener::open(&name, None).unwrap();
      let mut claim = begin(&name, 0).unwrap();
      let started = std::time::Instant::now();
      while started.elapsed() < std::time::Duration::from_nanos(LATE_NS) {
        assert!(
          claim.poll().unwrap().is_none(),
          "the claim is due while its daemon lives"
        );
        std::thread::sleep(std::time::Duration::from_nanos(POLL_PAUSE_NS));
      }
      let accepted = accept(&mut listener, "late")
        .unwrap()
        .expect("the claim is answered");
      let connected = loop {
        if let Some(connected) = claim.poll().unwrap() {
          break connected;
        }
      };
      assert_eq!(connected.region.client_id(), accepted.client_id);
    }

    /// A-443 (R3's class C). Do: begin a claim with no restart bound on a daemon end, then let the daemon end go with
    /// the claim unanswered, its owner's token with it, and poll the claim. Expect: refused `DaemonUnavailable` for a
    /// rendezvous with no live owner, within the claim wait of the daemon's going: by the owner's life, not by a clock.
    /// Before, the claim waited out the claim wait from its start and was refused for not answering within it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_claim_on_a_daemon_that_died_ends_by_its_owners_life_not_a_clock() {
      let name = instance("gone");
      let listener = Listener::open(&name, None).unwrap();
      let mut claim = begin(&name, 0).unwrap();
      assert!(claim.poll().unwrap().is_none(), "the claim is due");
      let gone = std::time::Instant::now();
      drop(listener);
      let refusal = loop {
        match claim.poll() {
          Ok(None) => {}
          Ok(Some(_)) => panic!("a claim with no daemon was answered"),
          Err(refusal) => break refusal,
        }
      };
      let waited = gone.elapsed();
      println!("a claim on a daemon that went: {refusal:?} after {waited:?}");
      assert!(
        matches!(&refusal, IpcError::DaemonUnavailable { why, .. } if why.contains("no live owner")),
        "{refusal:?}"
      );
      assert!(
        waited < std::time::Duration::from_nanos(CLAIM_WAIT_NS),
        "the claim ended {waited:?} after its daemon went, not by its owner's life"
      );
    }

    /// Shape: the reconnect budget a client gave its claim as a restart bound before A-468: the recovery budget and one
    /// liveness window, a second each (`slates_client::Deadlines::derive`'s `T + W`).
    const OLD_RECONNECT_BUDGET_NS: u64 = 2_000_000_000;
    /// Shape: how long the anchored test's restart takes: a quarter again the old reconnect budget.
    const SLOW_RESTART_NS: u64 = OLD_RECONNECT_BUDGET_NS + OLD_RECONNECT_BUDGET_NS / 4;
    /// Shape: the anchored test's pause between its polls: a hundredth of the claim wait.
    const RESTART_POLL_PAUSE_NS: u64 = CLAIM_WAIT_NS / 100;

    /// A-468 (R3's class C). Do: hold the instance's rendezvous as an anchor does, open a daemon end over it and begin a
    /// claim; stop the daemon end before it answers, and only after a restart slower than the reconnect budget a client
    /// gave its claim before A-468 open another over the held rendezvous and let it answer, polling the claim all the
    /// while. Expect: the claim due all the while, then connected to the region the second daemon end made for it.
    /// Before A-468 a client's claim under a live anchor was refused at that budget, the anchor still restarting its
    /// daemon.
    #[test]
    // The test thread stands for a client polling at its own pace: it pauses between its polls.
    #[allow(clippy::disallowed_methods)]
    fn a_claim_under_a_live_anchor_waits_out_a_restart_slower_than_the_old_reconnect_budget() {
      let name = instance("anchored-restart");
      let _anchor = Held::hold(&name).unwrap();
      let first = Listener::open(&name, Some("held")).unwrap();
      let mut claim = begin(&name, 0).unwrap();
      drop(first);
      let started = std::time::Instant::now();
      while started.elapsed() < std::time::Duration::from_nanos(SLOW_RESTART_NS) {
        assert!(
          claim.poll().unwrap().is_none(),
          "the claim is due while its anchor lives, {:?} into the restart",
          started.elapsed()
        );
        std::thread::sleep(std::time::Duration::from_nanos(RESTART_POLL_PAUSE_NS));
      }
      let mut second = Listener::open(&name, Some("held")).unwrap();
      let accepted = accept(&mut second, "restarted")
        .unwrap()
        .expect("the claim is answered by the restarted daemon end");
      let connected = loop {
        if let Some(connected) = claim.poll().unwrap() {
          break connected;
        }
      };
      assert_eq!(connected.region.client_id(), accepted.client_id);
    }

    /// A-468. Do: hold the instance's rendezvous as an anchor does, open a daemon end over it and begin a claim; stop the
    /// daemon end, then let the anchor go too, its owner's name with it, and poll the claim. Expect: refused
    /// `DaemonUnavailable` for a rendezvous with no live owner, within the claim wait of the anchor's going. Before A-468
    /// the claim waited out its client's restart bound, two seconds, after its anchor was gone.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_claim_under_an_anchor_ends_at_the_anchors_going() {
      let name = instance("anchor-gone");
      let anchor = Held::hold(&name).unwrap();
      let first = Listener::open(&name, Some("held")).unwrap();
      let mut claim = begin(&name, 0).unwrap();
      drop(first);
      assert!(
        claim.poll().unwrap().is_none(),
        "the claim is due while its anchor lives"
      );
      let gone = std::time::Instant::now();
      drop(anchor);
      let refusal = loop {
        match claim.poll() {
          Ok(None) => {}
          Ok(Some(_)) => panic!("a claim with no daemon was answered"),
          Err(refusal) => break refusal,
        }
      };
      let waited = gone.elapsed();
      println!("a claim whose anchor went: {refusal:?} after {waited:?}");
      assert!(
        matches!(&refusal, IpcError::DaemonUnavailable { why, .. } if why.contains("no live owner")),
        "{refusal:?}"
      );
      assert!(
        waited < std::time::Duration::from_nanos(CLAIM_WAIT_NS),
        "the claim ended {waited:?} after its anchor went, not at its going"
      );
    }

    /// Whether `name`'s rendezvous comes to have no live holder within the claim wait (the owner's name is let go as its
    /// holder's port dies, which a look can just miss).
    fn holder_goes(name: &str) -> bool {
      let asked = std::time::Instant::now();
      while asked.elapsed() < std::time::Duration::from_nanos(CLAIM_WAIT_NS) {
        if !holder_lives(name).unwrap() {
          return true;
        }
        std::thread::yield_now();
      }
      false
    }

    /// A-468 (`holder_lives`). Do: ask after an instance's rendezvous with none made, held by an anchor, with a daemon end
    /// over it, once that daemon end stops, and once the anchor goes; then after another instance's, held by its own
    /// daemon end, before and after that daemon end goes. Expect: no holder, a holder (the anchor), a holder, still a
    /// holder (the anchor holds it across a restart), none; then a holder, and none.
    #[test]
    fn a_rendezvous_has_a_holder_while_its_anchor_or_its_own_daemon_lives() {
      let name = instance("holder");
      assert!(!holder_lives(&name).unwrap(), "none made");
      let anchor = Held::hold(&name).unwrap();
      assert!(holder_lives(&name).unwrap(), "an anchor holds it");
      let daemon = Listener::open(&name, Some("held")).unwrap();
      assert!(holder_lives(&name).unwrap(), "a daemon over the anchor's");
      drop(daemon);
      assert!(
        holder_lives(&name).unwrap(),
        "the anchor holds it across a restart"
      );
      drop(anchor);
      assert!(holder_goes(&name), "the anchor went");
      let own = instance("holder-own");
      let daemon = Listener::open(&own, None).unwrap();
      assert!(holder_lives(&own).unwrap(), "a daemon holding its own");
      drop(daemon);
      assert!(holder_goes(&own), "that daemon went");
    }

    /// A header's image with `daemon`'s mark and `anchor`'s put in as each process writes its own (A-447, A-468).
    fn header_with(daemon: ProcessMark, anchor: ProcessMark) -> [u8; HEADER_BYTES] {
      let mut header = [0u8; HEADER_BYTES];
      for (mark, pid_at, created_at) in [
        (daemon, AT_DAEMON_PID, AT_DAEMON_CREATED),
        (anchor, AT_ANCHOR_PID, AT_ANCHOR_CREATED),
      ] {
        let (pid, created) = mark.bytes();
        header[pid_at..pid_at + pid.len()].copy_from_slice(&pid);
        header[created_at..created_at + created.len()].copy_from_slice(&created);
      }
      header
    }

    /// A-447, A-468 (golden vectors). Do: put a daemon's mark and an anchor's into a header's image as they write them,
    /// and read each back. Expect: the daemon's process id's four bytes little-endian at 24 and its creation time's
    /// eight at 32, the anchor's at 28 and 40, each mark read back whole and neither for the other, and the layout's
    /// magic `SLB5` (A-522's claimant's start in each slot).
    #[test]
    fn the_process_marks_are_little_endian_at_their_offsets() {
      let daemon = ProcessMark {
        pid: 0x0403_0201,
        created: 0x1817_1615_1413_1211,
      };
      let anchor = ProcessMark {
        pid: 0x0807_0605,
        created: 0x2827_2625_2423_2221,
      };
      let header = header_with(daemon, anchor);
      assert_eq!(&header[24..28], &[0x01, 0x02, 0x03, 0x04]);
      assert_eq!(&header[28..32], &[0x05, 0x06, 0x07, 0x08]);
      assert_eq!(
        &header[32..40],
        &[0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]
      );
      assert_eq!(
        &header[40..48],
        &[0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28]
      );
      assert_eq!(ProcessMark::daemon_of(&header), Some(daemon));
      assert_eq!(ProcessMark::anchor_of(&header), Some(anchor));
      assert_eq!(MAGIC.to_le_bytes(), *b"SLB5");
    }

    /// A-447, A-468 (hostile images). Do: read the marks of an image with none written, of one whose daemon's process
    /// id is zero, of one whose anchor's creation time is zero, of one too short to hold the anchor's, and of one of every
    /// bit set. Expect: none for the unwritten, zeroed and short marks, the other mark read whole beside a zeroed one, and
    /// the all-set image read as written, for the system to refuse its pid.
    #[test]
    fn no_mark_is_read_where_none_was_written_and_a_hostile_one_is_read_as_written() {
      let anchor = ProcessMark {
        pid: 0x0807_0605,
        created: 0x2827_2625_2423_2221,
      };
      let header = header_with(
        ProcessMark {
          pid: 0x0403_0201,
          created: 0x1817_1615_1413_1211,
        },
        anchor,
      );
      assert_eq!(
        ProcessMark::daemon_of(&[0u8; HEADER_BYTES]),
        None,
        "no mark written"
      );
      assert_eq!(
        ProcessMark::anchor_of(&[0u8; HEADER_BYTES]),
        None,
        "no anchor's mark written"
      );
      let mut no_pid = header;
      no_pid[24..28].fill(0);
      assert_eq!(ProcessMark::daemon_of(&no_pid), None, "a zero process id");
      assert_eq!(
        ProcessMark::anchor_of(&no_pid),
        Some(anchor),
        "the anchor's mark is its own"
      );
      let mut no_time = header;
      no_time[40..48].fill(0);
      assert_eq!(
        ProcessMark::anchor_of(&no_time),
        None,
        "a zero creation time"
      );
      assert_eq!(
        ProcessMark::anchor_of(&header[..47]),
        None,
        "an image too short"
      );
      assert_eq!(
        ProcessMark::daemon_of(&[0xFF; HEADER_BYTES]),
        Some(ProcessMark {
          pid: u32::MAX,
          created: u64::MAX
        }),
        "a hostile mark is read as written; the system refuses its pid"
      );
    }

    /// AUD-29-19 (a cancelled connect). Do: begin a claim on a daemon end that has not accepted it, then
    /// drop the claim unanswered. Expect: the slot `CLAIMED` while the claim lives and `FREE` once it is
    /// dropped, so a cancelled connect never strands a claim slot until the stale reclaim.
    #[test]
    fn a_claim_dropped_unanswered_gives_its_slot_back() {
      let name = instance("dropped");
      let _listener = Listener::open(&name, None).unwrap();
      let client = client_mapping(&name);
      let claim = begin(&name, 0).unwrap();
      let index = claim.index;
      assert_eq!(state(&client, index).unwrap().load(Acquire), CLAIMED);
      drop(claim);
      assert_eq!(state(&client, index).unwrap().load(Acquire), FREE);
    }

    /// GAP-A9-9 (a same-uid stranger opened any client's ring by name, `docs/bugs/2026-10-18-macos-shares-by-name-so-a-
    /// stranger-opens-any-clients-ring.md`). Do: a client claims and the daemon answers it with a real region, and the
    /// client connects; then, as a stranger that never claimed, read every claim slot of the bootstrap object for the
    /// region's name, and open the region by the name the daemon made it under. Expect: the client holds its region, no
    /// slot holds the name, and the open is refused: the region is reachable only through what the daemon handed its
    /// claimant.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_stranger_finds_no_clients_region_by_name() {
      let name = instance("stranger");
      let mut listener = Listener::open(&name, None).unwrap();
      let mut claim = begin(&name, 0).unwrap();
      let accepted = accept(&mut listener, "victim")
        .unwrap()
        .expect("the claim is answered");
      let region_name = format!("rdvr-victim-{}-{}", accepted.client_id, std::process::id());
      let connected = loop {
        if let Some(connected) = claim.poll().unwrap() {
          break connected;
        }
        accept(&mut listener, "late").unwrap();
      };
      assert_eq!(
        connected.region.client_id(),
        accepted.client_id,
        "the client holds its own region"
      );
      let object_name = match SharedObject::handoff_for_name(&region_name) {
        Some(Handoff::Name(object_name)) => object_name,
        other => panic!("a macOS object is named: {other:?}"),
      };
      let stranger = client_mapping(&name);
      for index in 0..SLOTS {
        let mut slot = [0u8; NAME_BYTES];
        stranger.read(slot_at(index) + AT_NAME, &mut slot).unwrap();
        let named = slot
          .windows(object_name.len())
          .any(|window| window == object_name.as_bytes());
        assert!(
          !named,
          "claim slot {index} names the client's region {object_name}"
        );
      }
      let opened = ClientRegion::open(
        &Handoff::Name(object_name.clone()),
        connected.region.object().len(),
      );
      assert!(
        opened.is_err(),
        "a stranger opened the client's region {object_name} by name"
      );
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

    pub(super) fn take_pending(&self) -> Result<super::Pending, IpcError> {
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

    pub(super) fn pending(&self) -> (Vec<i32>, Option<u64>) {
      (Vec::new(), None)
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
