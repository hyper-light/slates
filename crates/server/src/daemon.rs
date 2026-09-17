//! The daemon (§2.6 "Boot order" steps 2, 3 and 5): attach or create the segment, start the
//! runtime's shards, recover each shard's partition and install its state, publish the
//! profile, start the doorbell thread, and run the control shard's rendezvous; stop in
//! reverse, joining everything the daemon started.

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use slates_anchor::ENV_NFS_LISTENER;
use slates_anchor::{AnchorSegment, RegionKind};
use slates_db::catalog::Principal;
use slates_ipc::{ClientRegion, Listener, Prepared};
use slates_machine::facts::Identity;
use slates_machine::{MachineProfile, derived};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_mem::{Handoff, Slab};
#[cfg(unix)]
use slates_rt::RtError;
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
#[cfg(unix)]
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_rt::{Runtime, ShardId, futures, registry};
use slates_vfs::clock::HostClock;
use slates_vfs::volume::{Store, StoreConfig};
use slates_wire::observe::{Chokepoint, ChokepointRegistry};

use crate::config::{DaemonConfig, DurabilityBound};
use crate::doorbell::{DoorbellThread, Waits};
use crate::error::ServerError;
use crate::observe::{Admitted, Observation, ObserveError, ObserveStage};
use crate::state::{self, ClientSlot, ShardState, StateAccess};
use crate::verbs;

/// Shape: the heartbeat cadence the control shard beats the anchor's word at (§4.14
/// `daemon.alive`): a tenth of the anchor's liveness budget, so nine beats fit inside it.
pub const HEARTBEAT_NS: u64 = 100_000_000;
/// Shape: the anchor's liveness budget for `daemon.alive` (a second; the supervisor's
/// input until the CLI takes the operator's value).
pub const LIVENESS_BUDGET_NS: u64 = 1_000_000_000;
/// How long an out-of-band observation ([`Daemon::observation`]) has, from its submission through its
/// admission to its answer, before it ends with a deadline naming the stage it reached
/// ([`crate::observe::ObserveError::Deadline`]). A live shard answers within a few coordinator periods
/// (~[`HEARTBEAT_NS`] each), but under heavy CPU load a period stretches toward a liveness budget; ample
/// headroom for a merely-slow shard to answer means an observation is not lost to a false timeout and
/// misread as a real change (a peer "left", a leader "lost"), while a genuinely wedged shard still ends
/// it — typed, so a poll tells the starvation from a fact.
/// Derived: ten liveness budgets ([`LIVENESS_BUDGET_NS`]).
pub const OBSERVE_BUDGET_NS: u64 = 10 * LIVENESS_BUDGET_NS;

/// Where the segment comes from.
#[derive(Clone, Debug)]
pub enum SegmentSource {
  /// Create a fresh segment (tests, the first start without an anchor).
  Create {
    /// The object's name.
    name: String,
  },
  /// Attach the segment the anchor handed over in the environment.
  FromEnv,
  /// Attach a segment by its handoff (an anchor in this process: tests, embeddings).
  Handoff {
    /// The handoff.
    handoff: Handoff,
    /// The mapped length.
    len: usize,
    /// The content object's handoff and length, if the anchor made one, so its shard content
    /// survives the restart (§4.8). `None` recreates content empty on rebuild as before (BUG-11).
    content: Option<(Handoff, usize)>,
  },
}

/// The daemon.
pub struct Daemon {
  runtime: Option<Runtime>,
  segment: AnchorSegment,
  doorbell: Option<DoorbellThread>,
  config: DaemonConfig,
  shards: Vec<ShardId>,
  /// The loopback port the NFS transport (§4.6) listens on, when it is serving; `None` if the
  /// listener could not be bound. A client mounts `nfs://localhost:PORT`.
  nfs_port: Option<u16>,
}

impl std::fmt::Debug for Daemon {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Daemon")
      .field("shards", &self.shards)
      .field("instance", &self.config.instance)
      .finish()
  }
}

/// The wire refusal a bootstrap reports for an observation it could not make on `shard`: a shard merely
/// starved past the observe budget is **overloaded** (the caller may retry); a shard the daemon can never
/// reach — gone, terminated, its state absent — leaves the group **not initialized**.
fn bootstrap_refusal(
  shard: Option<ShardId>,
  refusal: &ObserveError,
) -> slates_ipc::protocol::Refusal {
  match shard {
    Some(shard) if !refusal.is_terminal() => {
      slates_ipc::protocol::Refusal::Overloaded { shard: shard.0 }
    }
    _ => slates_ipc::protocol::Refusal::ConsensusNotInitialized,
  }
}

/// A control shard's task arena filled by [`Daemon::fill_task_arena`]: the fillers park until this is
/// dropped, which releases every one (each ends at its next step).
#[derive(Debug)]
pub struct ArenaFill {
  releases: Vec<std::sync::mpsc::Sender<()>>,
  admitted: usize,
  reached_the_bound: bool,
}

impl ArenaFill {
  /// How many fillers the arena admitted.
  pub fn admitted(&self) -> usize {
    self.admitted
  }

  /// Whether the fill met the arena's bound (a filler refused `TooManyTasks`) — the state a full-arena
  /// test relies on, asserted rather than assumed.
  pub fn reached_the_bound(&self) -> bool {
    self.reached_the_bound
  }

  /// Releases the fillers now, keeping the fill's counts.
  pub fn release(&mut self) {
    self.releases.clear();
  }
}

/// One shard's forward-progress pulse (§4.14), read by [`Daemon::shard_pulses`] straight off the runtime's
/// registry. A stall diagnosis reads it beside the coordinator's period count ([`Daemon::fleet_progress`]):
/// a coordinator not advancing on a shard whose `steps` still climb is a coordinator awaiting something (a
/// peer's reply, another shard's answer); one on a shard whose `steps` are frozen while `parked` is set is
/// a shard waiting in its driver for a kick that has not come; frozen and not parked is a shard held inside
/// one poll (a spin, a blocking call) or not being scheduled at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardPulse {
  /// The shard's runtime id.
  pub shard: u16,
  /// Loop iterations the shard has run.
  pub steps: u64,
  /// Driver waits (parks) the shard has entered.
  pub waits: u64,
  /// Tasks the shard has admitted to its arena.
  pub spawns: u64,
  /// Tasks whose future returned on the shard.
  pub completed: u64,
  /// Admissions the shard refused because its task arena was full — a new operation's task could not be
  /// spawned (the signal that an observation flood or a leak has saturated the shard, §4.3 "Task arena full").
  pub admission_refused: u64,
  /// The shard's longest single poll, nanoseconds (a step longer than a peer's wake starves the shard, §4.3).
  pub longest_step_ns: u64,
  /// Whether the shard announced itself parked at the moment of the read (a snapshot).
  pub parked: bool,
  /// Kicks senders skipped because the shard was not parked (§4.7 "Wake strategy": the saving, counted).
  pub kicks_skipped: u64,
  /// Times a foreign sender found the shard's wake ring full and spun (a tripwire).
  pub ring_full_events: u64,
  /// The shard's measured scheduler overrun, nanoseconds — how late its steps have run after the waits
  /// before them (`slates_rt::shard::ShardContext::scheduler_overrun_ns`, §4.8 "scheduler quantum"):
  /// a shard the operating system is not scheduling shows it climbing; one held inside its own work
  /// does not (`longest_step_ns` climbs instead).
  pub scheduler_overrun_ns: u64,
}

/// One doorbell flag per control shard, indexed by the shard's registry id: set by the daemon's doorbell
/// thread each time it kicks, swapped off by that daemon's control task poller. A table, not one flag,
/// because a process that runs several daemons (the test suites; a bench) must give each its own — with
/// one process-wide flag, one daemon's poller consumed the ring meant for another, whose control loop
/// then never ran its accept round and left a client's claim unanswered for the whole claim wait
/// (found by the typed-refusal test under the parallel daemon suite, 2026-09-14; one daemon per process
/// never saw it). Shape: one entry per registry slot (`MAX_SHARDS`), each on its own cache line — the
/// doorbell thread writes and the control shard swaps its entry, so two daemons' entries must never
/// share a line.
static DOORBELL_RANG: [DoorbellFlag; slates_rt::registry::MAX_SHARDS] =
  [const { DoorbellFlag(AtomicBool::new(false)) }; slates_rt::registry::MAX_SHARDS];

/// A daemon's doorbell flag on its own cache line (see [`DOORBELL_RANG`]).
#[repr(align(128))]
struct DoorbellFlag(AtomicBool);

/// The doorbell flag of the daemon whose control shard is `control` (see [`DOORBELL_RANG`]).
fn doorbell_flag(control: u16) -> &'static AtomicBool {
  &DOORBELL_RANG
    .get(usize::from(control))
    .unwrap_or(&DOORBELL_RANG[0])
    .0
}
/// Shape: pending NFS connections the kernel queues before the accept loop takes them. A mount opens a
/// small, bounded number of connections; the OS clamps the backlog to the system maximum anyway. Unix
/// only, with the NFS transport.
#[cfg(unix)]
const NFS_BACKLOG: i32 = 16;

/// The NFS loopback listener to serve: the one a supervising anchor holds and hands over in the
/// environment ([`slates_anchor::ENV_NFS_LISTENER`]), so its port survives a daemon restart (§4.6) —
/// adopted here; failing that (a standalone start or tests) a fresh ephemeral bind. Adopting the
/// anchor's descriptor is the only path that keeps the port stable across restarts. Unix only: NFS is
/// the macOS/Linux mount path (Windows mounts through WinFsp), and it rides the Unix-only rt TCP.
#[cfg(unix)]
fn nfs_listener() -> Result<TcpListener, RtError> {
  if let Some(raw) = std::env::var(ENV_NFS_LISTENER)
    .ok()
    .and_then(|value| value.parse::<RawFd>().ok())
  {
    // SAFETY: the anchor bound this listening socket and handed its descriptor to us across the spawn,
    // inherited at this number; ownership is ours now (the anchor keeps its own copy), so wrapping it
    // in an `OwnedFd` gives it a single owner that closes it on drop.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    return TcpListener::from_fd(owned);
  }
  TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), NFS_BACKLOG)
}
/// Shards whose initialization refused (a health signal; the daemon serves the rest).
pub static INIT_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Clients handed to a shard that could not seat them — no state to take them, the seat task refused by
/// the shard's arena and dropped unrun, the shard's client table full — each with its id given back to
/// the control shard's live set (a health signal; before 2026-09-14 a refused seat kept the id, so the
/// client reconnected under it, was told `SessionTaken`, and the node refused every client once the bound
/// was consumed: `docs/bugs/2026-09-14-fleet-tasks-outside-the-task-budget-poison-client-admission.md`).
pub static HANDOFF_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Client ids that could not be given back to the control shard's live set after a lost handoff or a
/// reaped client (the control shard's channel full or gone when the forget task was sent): each is a
/// slot of the client bound held until the daemon restarts, counted here and logged once, never silent.
pub static RELEASE_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Accept rounds of the rendezvous that ended in an error other than a typed refusal (a peer's socket
/// refusing mid-handoff, a region that could not be created): counted here and logged once, where before
/// the round's error was dropped and the peer left with no handoff.
pub static ACCEPTS_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Gives `client_id` back to the control shard's live set (`state::with_handed`), from any shard: directly
/// when the caller runs on the control shard, otherwise as a forget task on it (sharing by move). A forget
/// task the control shard's channel refuses is counted `RELEASE_LOST` and logged once — the id then holds a
/// slot of the client bound until the daemon restarts, a visible fault rather than a silent one. The one
/// release path for a reaped client (`verbs::reap_client`) and a lost admission ([`Admission`]).
pub(crate) fn release_client_id(client_id: u32, control: u16) {
  if registry::current_shard() == Some(control) {
    state::with_handed(|handed| {
      handed.remove(&client_id);
    });
    return;
  }
  let forget = Box::new(SpawnRequest::new(
    Box::pin(async move {
      state::with_handed(|handed| {
        handed.remove(&client_id);
      });
    }),
    None,
  ));
  if let Err(e) = registry::send_control(control, Control::Spawn(forget))
    && RELEASE_LOST.fetch_add(1, Ordering::AcqRel) == 0
  {
    eprintln!(
      "slates-server: client {client_id}'s id could not be given back to the control shard: {e} (first \
       occurrence; later ones are counted)"
    );
  }
}

/// One client's admission, from its id's entry in the control shard's live set to its seat on its shard.
/// Moved into the seat task; dropped **unseated** — the task refused by the shard's arena and dropped
/// unrun, or the shard's client table refusing the slot — it gives the id back and counts the lost
/// handoff, so a refused admission never leaks an id (cancellation safety by construction: the terminal
/// step owns the release).
struct Admission {
  client_id: u32,
  control: u16,
  seated: bool,
}

impl Admission {
  /// The terminal step: the client's slot is in its shard's table. A method, not a field write, so the
  /// seat task captures the **whole** guard: an `async move` block captures a `Copy` field it assigns
  /// (`seated`) by copy and leaves the guard behind in the accept loop, where it dropped unseated at
  /// once and gave back the id of a client that was seated (the 2021 disjoint-capture rule; found by
  /// the typed-refusal test on 2026-09-14).
  fn seat(&mut self) {
    self.seated = true;
  }
}

impl Drop for Admission {
  fn drop(&mut self) {
    if self.seated {
      return;
    }
    release_client_id(self.client_id, self.control);
    if HANDOFF_LOST.fetch_add(1, Ordering::AcqRel) == 0 {
      eprintln!(
        "slates-server: client {}'s seat was refused by its shard; its id is given back (first \
         occurrence; later ones are counted)",
        self.client_id
      );
    }
  }
}
/// Volumes the recovered catalog holds that a shard could not rebuild (a health signal).
pub static RECOVERY_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The status refusal count under which the daemon records a perpetual loop of its own that the control
/// shard's arena refused — the mount listener's serve loop, the anchor heartbeat — counted, never silent
/// (banned item 9); a shard's serve and reap loops are a typed initialization failure instead
/// (`init_shard`), since a shard without them serves nothing.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const LOOP_SPAWN_REFUSED: &str = "daemon.loop_spawn";
/// Volumes skipped from a shard publish because they could not be imaged (an overlay with base-backed
/// inodes, whose base recovery is its own gate); the rest of the shard still publishes (a health
/// signal, §4.8).
pub static PUBLISH_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Shard publishes refused outright — the image did not fit its content-object slot, or the slot
/// could not be written — so nothing changed since the last committed image survives a restart until
/// a publish succeeds (a health signal, §4.8). A data-plane barrier that meets this answers its
/// client `NFS3ERR_IO`; a control verb records a refused completion (AUD-05). The effect can
/// remain unacknowledged in memory; the client is never promised that it survives a restart.
pub static PUBLISH_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Mount-transport stability barriers refused because the image omitted the touched volume
/// (§4.8, D-18; AUD-05). Counted alongside the `NFS3ERR_IO` reply, never a successful acknowledgement.
pub static BARRIER_UNCAPTURED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Connects refused at the daemon's derived client bound (a health signal, AC-2.6).
pub static CLIENTS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Clients found dead and reclaimed (a health signal; T-2.3).
pub static CLIENTS_REAPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The loopback port the NFS transport serves on (§4.6), 0 until the listener is bound. Published as
/// a process-global word (like the health signals) so a verb handler on any shard can report it to a
/// client — `slates mount` reads it to run `mount_nfs localhost:PORT`.
pub static NFS_PORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Configuration changes whose installed configuration **breached** the operator's declared durability bound
/// (§4.8 "the copyset count check at every configuration change"; D-14). A health signal, surfaced not
/// silently over-scattered: a breach is a recovery-vs-durability conflict for the operator to resolve. Zero
/// when no durability policy is declared (the default) or every installed configuration stays within it.
pub static DURABILITY_BREACHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Measures a just-installed `configuration` against the operator's durability `bound` — the design's
/// `within_loss_bound(ε, F)` check at every configuration change (§4.8 "Placement", D-14) — returning the
/// **shortfall** the write path refuses with (`None` within the bound, or with no bound declared) and counting
/// a breach as a health signal ([`DURABILITY_BREACHES`]), surfaced never a silent over-scatter. Called where a
/// configuration change is first seen on this node (the control shard's boot and council install); a shard
/// that receives the same configuration by fan measures it with [`DurabilityBound::shortfall`] alone, so one
/// change moves the signal once.
pub(crate) fn record_durability(
  configuration: &slates_db::register::Configuration,
  bound: Option<DurabilityBound>,
) -> Option<crate::config::DurabilityShortfall> {
  let shortfall = bound.and_then(|bound| bound.shortfall(configuration));
  if shortfall.is_some() {
    DURABILITY_BREACHES.fetch_add(1, Ordering::Relaxed);
  }
  shortfall
}

/// Measures the configuration a shard forms at boot against the operator's durability policy (§4.8, D-14):
/// the shortfall that shard's writes are refused with (`None` with no policy). Every shard measures — each
/// holds its own copy of the configuration and serves its own writes — but only the control shard (`counts`)
/// records the breach, so one boot moves the health signal once, not once per shard.
fn boot_durability(
  config: &DaemonConfig,
  configuration: &slates_db::register::Configuration,
  counts: bool,
) -> Option<crate::config::DurabilityShortfall> {
  let bound = config
    .fleet
    .as_ref()
    .and_then(|membership| membership.durability);
  if counts {
    record_durability(configuration, bound)
  } else {
    bound.and_then(|bound| bound.shortfall(configuration))
  }
}

impl Daemon {
  /// This start's fresh member identity. The instance name and certificate anchor remain stable. An
  /// observation the daemon could not make is its typed refusal ([`Self::observation`]).
  pub fn member_identity(&self) -> Result<slates_db::HostId, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.fleet.host())
  }

  /// The regional voting set after its transition commits — `Ok(None)` while joining or changing, which is
  /// observed state, kept apart from the typed refusal of an observation the daemon could not make.
  pub fn council_voters(&self) -> Result<Option<Vec<slates_db::HostId>>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.council.committed_voters()
    })
  }

  /// The root's committed voter configuration — `Ok(None)` during initialization or a joint change, observed
  /// state kept apart from the typed refusal of an observation the daemon could not make.
  pub fn root_voters(&self) -> Result<Option<Vec<slates_db::register::HostId>>, ObserveError> {
    self.observe(self.shards.first().copied(), |state| {
      state.root.committed_voters()
    })
  }

  /// The stable local rendezvous name, which does not change with a member incarnation.
  pub fn instance(&self) -> &str {
    &self.config.instance
  }

  /// Explicit first-time group creation by the process owning this daemon and its anchor.
  /// Embeddings and fixtures name the current member just as the CLI does. Startup never calls it.
  pub fn bootstrap(&self, root: bool) -> Result<(), slates_ipc::protocol::Refusal> {
    use slates_ipc::protocol::{Refusal, ReplyBody};
    let control = self.shards.first().copied();
    let member = self
      .member_identity()
      .map_err(|refusal| bootstrap_refusal(control, &refusal))?
      .0;
    let (reply, publication) = self
      .observe(control, move |state| {
        let reply = crate::consensus::bootstrap(state, root, member);
        (reply, crate::consensus::Publication::capture(state))
      })
      .map_err(|refusal| bootstrap_refusal(control, &refusal))?;
    match reply {
      ReplyBody::Acknowledged => {}
      ReplyBody::Refused { refusal } => return Err(refusal),
      _ => return Err(Refusal::ConsensusNotInitialized),
    }
    for shard in self.shards.iter().copied().skip(1) {
      let publication = publication.clone();
      self
        .observe(Some(shard), move |state| publication.apply(state))
        .map_err(|refusal| bootstrap_refusal(Some(shard), &refusal))?;
    }
    Ok(())
  }

  /// Starts the daemon from a profile.
  pub fn start(
    profile: &MachineProfile,
    config: DaemonConfig,
    source: SegmentSource,
  ) -> Result<Daemon, ServerError> {
    Self::start_with_fleet(profile, config, source, None)
  }

  /// Like [`start`](Daemon::start) but **joins a fleet** (§4.8, boot step 6): `fleet_transport` — this
  /// node's fleet TLS identity, its advertised accept socket, and its peers — drives the control shard's
  /// membership loop, which probes the peers, serves their probes, and folds the converged view into the
  /// `FleetNode` the verbs read for placement. `None` is the laptop: no loop runs, and the placement path
  /// still runs the same `FleetNode`, degenerate (R8). The membership *policy* (quorum + peer hosts) comes
  /// from [`DaemonConfig::fleet`](crate::config::DaemonConfig); this is the transport material, kept
  /// separate because [`Identity`](slates_transport::handshake::Identity) is not `Clone`.
  pub fn start_with_fleet(
    profile: &MachineProfile,
    config: DaemonConfig,
    source: SegmentSource,
    fleet_transport: Option<crate::fleet::FleetTransport>,
  ) -> Result<Daemon, ServerError> {
    // The observability gate (§2.6, §4.14): the health plane refuses to serve until every chokepoint
    // span has registered its emitter, so a daemon never serves with a silently missing span source.
    // Fail-closed and checked before any resource is acquired — an incomplete roster stops the boot
    // here, naming what is missing, rather than serving blind.
    let chokepoints = registered_chokepoints();
    if !chokepoints.is_ready() {
      return Err(ServerError::ChokepointsUnregistered {
        missing: chokepoints
          .missing()
          .into_iter()
          .map(Chokepoint::name)
          .collect(),
      });
    }
    let identity = profile.facts.identity.clone();
    let mut segment = match source {
      SegmentSource::Create { name } => {
        let content_name = content_name_of(&name);
        AnchorSegment::create(&name, &identity, config.geometry)?
          .with_content(&content_name, content_bytes(&config))?
      }
      SegmentSource::FromEnv => AnchorSegment::attach_from_env(&identity)?,
      SegmentSource::Handoff {
        handoff,
        len,
        content,
      } => {
        let mut segment = AnchorSegment::attach(&handoff, len, &identity)?;
        if let Some((content_handoff, content_len)) = content {
          let object = slates_mem::SharedObject::open(&content_handoff, content_len)
            .map_err(slates_anchor::AnchorError::from)?;
          segment.adopt_content(object);
        }
        segment
      }
    };
    if let Ok(json) = profile.to_json() {
      segment.publish(RegionKind::Profile, json.as_bytes())?;
    }
    // The grant-issuer secret (§4.13 "only an authenticated human confirmation surface holds grant-issuer
    // authority"): minted fresh at every start from the TLS provider's secure random and published into the
    // anchor's supervision block, which only the supervisor, this daemon and a `slates` command running as
    // the anchor's user map — never a ring, MCP or SDK client. A `Grant` request proves possession by a keyed
    // hash over the exact landing it approves (`landing::grant_proof`); a fresh secret per start means a
    // proof minted against a previous daemon's authority does not verify against this one (no replay across
    // restarts). Refused, never started, if the provider cannot mint it: a daemon with no issuer authority
    // would silently make every landing un-grantable.
    let mut issuer_secret = [0u8; slates_anchor::layout::ISSUER_SECRET_BYTES];
    slates_transport::handshake::secure_random(&mut issuer_secret)
      .map_err(|e| ServerError::IssuerSecret(e.to_string()))?;
    segment.publish_issuer_secret(&issuer_secret)?;
    if let Some((was, now)) = limits::raise_descriptor_limit() {
      eprintln!("slates-server: descriptor limit raised from {was} to {now}");
    }
    let retained = crate::retention::load(&segment)?;
    let runtime = Runtime::start(&config.runtime)?;
    let shards: Vec<ShardId> = runtime.shard_ids().to_vec();
    for (index, shard) in shards.iter().enumerate() {
      let env = segment.handoff_env()?;
      let config = config.clone();
      let identity = identity.clone();
      let partition = u16::try_from(index).unwrap_or(u16::MAX);
      let all: Vec<u16> = shards.iter().map(|s| s.0).collect();
      let retained = retained.clone();
      runtime.spawn_on(*shard, async move {
        if let Err(e) = init_shard(&config, &env, &identity, partition, &all, retained) {
          INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
          eprintln!("slates-server: shard {partition} failed to initialize: {e}");
        }
      })?;
    }
    // The rendezvous and the doorbell.
    let listener = Listener::open(&config.instance)?;
    let kicks: Vec<slates_rt::driver::Kick> = shards
      .iter()
      .filter_map(|s| registry::with_entry(s.0, |e| e.kick))
      .collect();
    let waits = match listener.doorbell_waiter()? {
      Some((object, offset)) => Waits::Word { object, offset },
      None => Waits::Socket(listener.raw_fd().unwrap_or(-1)),
    };
    let doorbell = DoorbellThread::start(
      waits,
      kicks,
      doorbell_flag(shards.first().map_or(0, |shard| shard.0)),
    );
    let control_config = config.clone();
    let control_env = segment.handoff_env()?;
    let control_identity = identity.clone();
    let control = shards.first().copied().ok_or(ServerError::NotOnShard)?;
    let shard_ids = shards.clone();
    runtime.spawn_on(control, async move {
      control_loop(
        listener,
        control_config,
        control_env,
        control_identity,
        shard_ids,
      )
      .await;
    })?;
    // In a fleet (§4.8, boot step 6): the control shard runs the membership loop over the fleet transport,
    // probing its peers, serving their probes, and folding the converged view into the `FleetNode` the
    // verbs read for placement. A laptop passes no transport and runs no loop (R8: the same placement path,
    // degenerate). The loop's perpetual tasks are detached and cancelled by `runtime.shutdown()`.
    // The fleet coordinator's forward-progress heartbeat (§4.14) lives on the control shard's registry
    // pulse (`Pulse::progress`), which an observer reads directly under CPU load (no shard round-trip)
    // and which outlives the shard as its entry does — no allocation per boot (before 2026-09-14 an atomic
    // was leaked per daemon start). A laptop spawns no coordinator, so it stays zero.
    if let Some(transport) = fleet_transport {
      runtime.spawn_on(control, async move {
        crate::fleet::run_membership(transport).await;
      })?;
    }
    // The NFS transport (§4.6): one loopback listener served on the control shard. A supervising
    // anchor holds the listener and hands its descriptor over in the environment, so its port survives
    // a daemon restart (Unix); the daemon adopts that when present, or binds a fresh ephemeral one when
    // it runs standalone (tests). Either way the port is known here, before the serve task moves the
    // listener onto the shard, and the cross-shard bridge queue reaches volumes on other shards.
    #[cfg(unix)]
    let nfs_port = match nfs_listener() {
      Ok(nfs_listener) => {
        let port = nfs_listener.local_addr().ok().map(|addr| addr.port());
        if let Some(port) = port {
          // Publish the port for a verb handler to report to a client (`slates mount`).
          NFS_PORT.store(u32::from(port), Ordering::Release);
          runtime.spawn_on(control, async move {
            match futures::spawn(crate::nfs::serve(nfs_listener, port)) {
              Ok(task) => {
                let _ = futures::detach(task);
              }
              Err(_) => {
                crate::fleet::count_refusal(LOOP_SPAWN_REFUSED);
              }
            }
          })?;
        }
        port
      }
      Err(_) => None,
    };
    // No NFS transport on Windows: NFS is the macOS/Linux mount path (Windows mounts through WinFsp),
    // so a Windows daemon serves IPC clients and lands, but publishes no mount port.
    #[cfg(windows)]
    let nfs_port: Option<u16> = None;
    Ok(Daemon {
      runtime: Some(runtime),
      segment,
      doorbell: Some(doorbell),
      config,
      shards,
      nfs_port,
    })
  }

  /// The loopback port the NFS transport (§4.6) is serving on, if the listener bound; a client mounts
  /// `nfs://localhost:PORT` to reach this daemon's volumes (those on its accepting shard, R8).
  pub fn nfs_port(&self) -> Option<u16> {
    self.nfs_port
  }

  /// The configuration.
  pub fn config(&self) -> &DaemonConfig {
    &self.config
  }

  /// Begins an observation of this daemon's shard at `target` (§4.14; [`crate::observe`]): `question`
  /// runs on that shard under a borrow of its state and its answer comes back to the asker, who owns
  /// the pending [`Observation`] and runs it with [`Observation::wait`] (or [`Observation::admit`], to
  /// hold the admitted task and read its answer later). One absolute budget of `budget_ns` spans the
  /// question's submission, admission and execution, and every way it ends short of an answer is a
  /// typed [`ObserveError`] naming the stage — never a `None` an asker could read as a fact. Decided
  /// here, before anything is submitted: no such shard (`NoTarget`), no runtime (`NoRuntime`), the
  /// shard's registry slot no longer held by it (`ShardGone`). The observation is pinned to the shard's
  /// registration, so a slot reused by a later daemon refuses it rather than answering for a stranger.
  /// `question` is `Clone` because each admission attempt builds a fresh task (a refused request's
  /// future is dropped by the runtime).
  pub fn observation<T, Q>(
    &self,
    target: Option<ShardId>,
    budget_ns: u64,
    question: Q,
  ) -> Result<
    Observation<T, impl FnOnce() -> Result<T, StateAccess> + Clone + Send + 'static>,
    ObserveError,
  >
  where
    T: Send + 'static,
    Q: FnOnce(&mut ShardState) -> T + Clone + Send + 'static,
  {
    self.pending(target, budget_ns, move || state::try_with_state(question))
  }

  /// Runs `question` on this daemon's shard at `target` and returns its answer within
  /// [`OBSERVE_BUDGET_NS`] ([`Self::observation`]). That budget is **generous on purpose**: under noisy,
  /// heavy CPU load a live shard can be starved of the scheduler for several seconds before it services
  /// this one-shot question, and a tight budget would end the observation with a deadline where the
  /// shard was merely slow — so a slow shard answers, and only a wedged one ends the observation, with
  /// the stage it reached and the refusal that held it there named. The same holds for **admission**:
  /// the question is a task submitted through the shard's bounded control channel, which under that
  /// same load is routinely *full*, and a submission refused `ControlFull` used to end the observation
  /// at once — a starved shard read as a fact (an injected death that never landed, a leader that "was
  /// not"; docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md). A capacity refusal is
  /// retried, paced, inside the one budget. The calling thread parks while it waits (it does not spin),
  /// so it steals no CPU from the shard it waits on.
  fn observe<T, Q>(&self, target: Option<ShardId>, question: Q) -> Result<T, ObserveError>
  where
    T: Send + 'static,
    Q: FnOnce(&mut ShardState) -> T + Clone + Send + 'static,
  {
    self
      .observation(target, OBSERVE_BUDGET_NS, question)?
      .wait()
  }

  /// Like [`Self::observe`] for a question of the shard's runtime context rather than of its state (a
  /// task count), answered the same way.
  fn observe_shard<T, Q>(&self, target: Option<ShardId>, question: Q) -> Result<T, ObserveError>
  where
    T: Send + 'static,
    Q: FnOnce(&slates_rt::shard::ShardContext) -> T + Clone + Send + 'static,
  {
    self
      .pending(target, OBSERVE_BUDGET_NS, move || {
        registry::with_current(question).ok_or(StateAccess::Absent)
      })?
      .wait()
  }

  /// Runs `question` on this daemon's control shard with the asker's own `budget_ns`
  /// ([`Self::observation`]): the form a test drives directly to prove what an observation reports at
  /// each stage — a full control channel, a full arena, a shard held past the budget, a daemon stopped
  /// while the question is pending.
  pub fn observe_control<T, Q>(&self, budget_ns: u64, question: Q) -> Result<T, ObserveError>
  where
    T: Send + 'static,
    Q: FnOnce(&mut ShardState) -> T + Clone + Send + 'static,
  {
    self
      .observation(self.shards.first().copied(), budget_ns, question)?
      .wait()
  }

  /// The pending form of [`Self::observe_control`]: nothing is submitted until the returned observation
  /// is run, which may happen on another thread, and after this daemon has stopped.
  pub fn observation_on_control<T, Q>(
    &self,
    budget_ns: u64,
    question: Q,
  ) -> Result<
    Observation<T, impl FnOnce() -> Result<T, StateAccess> + Clone + Send + 'static>,
    ObserveError,
  >
  where
    T: Send + 'static,
    Q: FnOnce(&mut ShardState) -> T + Clone + Send + 'static,
  {
    self.observation(self.shards.first().copied(), budget_ns, question)
  }

  /// The observation core: a question in the unified form (it answers, or names why the shard's state
  /// was out of reach), pinned to the registration holding the shard at `target`, with the refusals
  /// decidable before a submission decided here.
  fn pending<T, Q>(
    &self,
    target: Option<ShardId>,
    budget_ns: u64,
    question: Q,
  ) -> Result<Observation<T, Q>, ObserveError>
  where
    T: Send + 'static,
    Q: FnOnce() -> Result<T, StateAccess> + Clone + Send + 'static,
  {
    let shard = target.ok_or(ObserveError::NoTarget)?;
    let runtime = self.runtime.as_ref().ok_or(ObserveError::NoRuntime)?;
    let holder = runtime.holder_of(shard).ok_or(ObserveError::ShardGone {
      shard: shard.0,
      stage: ObserveStage::Submission,
    })?;
    Ok(Observation::new(holder, budget_ns, question))
  }

  /// Test support: submits fire-and-forget no-op tasks to the control shard until its control channel
  /// refuses one as full, and returns how many were queued — the state in which an observation's
  /// submission meets `ControlFull` (§4.3 "admission limit"). Each queued task ends as soon as the shard
  /// drains it, so the flood clears by itself once the shard runs. Zero means the channel was already
  /// full; a refusal other than a full channel is the observation's.
  pub fn flood_control_channel(&self) -> Result<usize, ObserveError> {
    let shard = self.shards.first().copied().ok_or(ObserveError::NoTarget)?;
    let runtime = self.runtime.as_ref().ok_or(ObserveError::NoRuntime)?;
    let mut queued = 0;
    loop {
      match runtime.spawn_on(shard, async {}) {
        Ok(()) => queued += 1,
        Err(slates_rt::RtError::ControlFull { .. }) => return Ok(queued),
        Err(slates_rt::RtError::ShardGone { shard }) => {
          return Err(ObserveError::ShardGone {
            shard,
            stage: ObserveStage::Submission,
          });
        }
        Err(refusal) => {
          return Err(ObserveError::Submission {
            refusal,
            attempts: 1,
            waited_ns: 0,
          });
        }
      }
    }
  }

  /// Test support: fills the control shard's task arena with tasks that park until the returned
  /// [`ArenaFill`] is dropped, so an observation's admission meets a full arena while the control channel
  /// stays receptive (§4.3 "Task arena full"). Each filler is submitted with an admission receipt and the
  /// fill stops at the first refusal; how many were admitted, and whether the arena's bound was in fact
  /// reached, are on the fill. A refusal other than a full arena is the observation's.
  pub fn fill_task_arena(&self) -> Result<ArenaFill, ObserveError> {
    let shard = self.shards.first().copied().ok_or(ObserveError::NoTarget)?;
    let runtime = self.runtime.as_ref().ok_or(ObserveError::NoRuntime)?;
    let mut fill = ArenaFill {
      releases: Vec::new(),
      admitted: 0,
      reached_the_bound: false,
    };
    // Bounded at twice the arena: a fill that is not refused by then is reported as not reaching the
    // bound, never spun on.
    for _ in 0..self.config.runtime.tasks_per_shard.saturating_mul(2) {
      let (release, released) = std::sync::mpsc::channel::<()>();
      let receipt = runtime
        .spawn_on_with_receipt(shard, async move {
          // Parked by yielding, not by a timer: the release is seen at the next step, and no timer
          // slot is held for it.
          while released.try_recv() == Err(std::sync::mpsc::TryRecvError::Empty) {
            slates_rt::futures::yield_now().await;
          }
        })
        .map_err(|refusal| ObserveError::Submission {
          refusal,
          attempts: 1,
          waited_ns: 0,
        })?;
      match receipt.wait(std::time::Duration::from_nanos(OBSERVE_BUDGET_NS)) {
        Some(slates_rt::Admission::Admitted(_)) => {
          fill.releases.push(release);
          fill.admitted += 1;
        }
        Some(slates_rt::Admission::Refused(slates_rt::RtError::TooManyTasks { .. })) => {
          fill.reached_the_bound = true;
          return Ok(fill);
        }
        Some(slates_rt::Admission::Refused(refusal)) => {
          return Err(ObserveError::Admission {
            refusal,
            attempts: 1,
            waited_ns: 0,
          });
        }
        Some(slates_rt::Admission::Terminated) => {
          return Err(ObserveError::Terminated {
            stage: ObserveStage::Admission,
            attempts: 1,
          });
        }
        None => {
          return Err(ObserveError::Deadline {
            stage: ObserveStage::Admission,
            budget_ns: OBSERVE_BUDGET_NS,
            attempts: 1,
            waited_ns: OBSERVE_BUDGET_NS,
            last_refusal: None,
          });
        }
      }
    }
    Ok(fill)
  }

  /// This daemon's fleet coordinator **forward-progress** count — periods the control-shard fleet loop has
  /// executed (§4.14). Read **directly** off the shared atomic, with no shard round-trip, so it is reported
  /// even when that shard is too CPU-starved to answer a query. A test charges its `poll_until` budget against
  /// this (periods, not wall-clock): a correct-but-slow operation — the coordinator still cycling, just fewer
  /// periods per wall-second under load — is never failed, while a genuinely stalled fleet (this count frozen)
  /// still is. Zero for a laptop (no coordinator runs).
  pub fn fleet_progress(&self) -> u64 {
    self
      .shards
      .first()
      .and_then(|control| registry::with_entry(control.0, |entry| entry.pulse.progress()))
      .unwrap_or(0)
  }

  /// Every shard's forward-progress pulse ([`ShardPulse`], §4.14), read **directly** from the runtime's
  /// registry with no shard round-trip — the discipline of [`Self::fleet_progress`] — so it is reported even
  /// when a shard is too starved, or too wedged, to answer a query. In shard order; a shard whose registry
  /// entry is gone is omitted. Zero-cost to the shards beyond one plain store per step: reading moves each
  /// pulse's cache line once, so an observer samples it, never spins on it.
  pub fn shard_pulses(&self) -> Vec<ShardPulse> {
    self
      .shards
      .iter()
      .filter_map(|shard| {
        registry::with_entry(shard.0, |entry| ShardPulse {
          shard: shard.0,
          steps: entry.pulse.steps(),
          waits: entry.pulse.waits(),
          spawns: entry.pulse.spawns(),
          completed: entry.pulse.completed(),
          admission_refused: entry.pulse.admission_refused(),
          longest_step_ns: entry.pulse.longest_step_ns(),
          parked: entry.parking.parked(),
          kicks_skipped: entry.parking.kicks_skipped(),
          ring_full_events: entry.ring_full_events.load(Ordering::Relaxed),
          scheduler_overrun_ns: entry.pulse.scheduler_overrun_ns(),
        })
      })
      .collect()
  }

  /// The hosts this daemon's fleet currently sees alive (§4.8) — this node and the peers its control-shard
  /// membership loop has probed and found live — for a test or an operator to observe the membership the
  /// verbs read for placement. An observation the daemon could not make is its typed refusal
  /// ([`Self::observation`]) — which a caller must treat as *unknown*, never as an empty membership; a
  /// laptop (no fleet) reports itself alone.
  pub fn fleet_members(&self) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.fleet.membership().alive()
    })
  }

  /// The peers this node has **formed a probe session** to (§4.8 formation; the members of
  /// `formed_probe_peers`), for a formation observer to name which sessions are still missing when the
  /// mesh has not formed — beside the seeded members ([`fleet_members`](Daemon::fleet_members)) and the
  /// coordinator's period count ([`fleet_progress`](Daemon::fleet_progress)), the facts that tell a genuine
  /// non-convergence from a wedge. An observation the daemon could not make is its typed refusal
  /// ([`Self::observation`]).
  pub fn fleet_formed_probe_peers(&self) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.formed_probe_peers.iter().copied().collect()
    })
  }

  /// Whether this daemon's fleet has formed its **direct probe mesh** (§4.8): every peer this node keeps
  /// direct contact with — its record neighbourhood and its council and root voters
  /// (`fleet::keeps_direct_contact_with`) — has a live probe session: the handshake completed and the peer
  /// was recorded in `formed_probe_peers`. Unlike [`fleet_members`](Daemon::fleet_members), which reads the
  /// membership's optimistically **seeded** alive set (every configured peer is believed alive from boot,
  /// before any is contacted), this reflects sessions that have actually formed. A formation observer
  /// waits on this so it does not act on a fleet whose mesh is not yet up — for instance retiring a node
  /// that dies before its peers ever probed it, which no survivor could then detect; and it must cover the
  /// consensus voters outside the copyset, or a test proceeds while those probe sessions are still
  /// establishing under load. A laptop (no peers) is trivially meshed. Runs a one-shot question on the
  /// control shard; an observation the daemon could not make is its typed refusal ([`Self::observation`]),
  /// which a caller must treat as *unknown*, never as "not meshed".
  pub fn fleet_meshed(&self) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      // The peers to reach are every configured member this node keeps direct contact with, less this
      // node; the mesh is up when every one has a formed probe session. A laptop has an empty peer set
      // and is meshed at once.
      let host = s.fleet.host();
      s.fleet
        .members()
        .iter()
        .filter(|&&peer| peer != host && crate::fleet::keeps_direct_contact_with(s, peer))
        .all(|peer| s.formed_probe_peers.contains(peer))
    })
  }

  /// Probe progress and timer decisions lengthened by this shard's measured scheduling delay (§4.8).
  /// An unavailable observation is a typed refusal, never a zero count.
  pub fn fleet_probe_windows(&self) -> Result<crate::fleet::ProbeWindows, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.probe_windows)
  }

  /// Whether this daemon's regional configuration council (§4.8, D-14) currently believes itself the
  /// **leader** — the elected configuration master for the region. A one-shot question on the control
  /// shard ([`Self::observation`]); an observation the daemon could not make is its typed refusal, which
  /// a caller must treat as *unknown*, never as "not the leader". Exposed so a fleet test can prove the
  /// council elected a single stable leader over the real transport.
  pub fn council_leads(&self) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.council.is_leader())
  }

  /// This daemon's council **leader-contact** counter (§4.8): the number of leader appends and granted votes
  /// its council has answered. A follower's counter advancing across periods is the proof that the leader's
  /// heartbeats are flowing over the transport — replication is live, not merely an election won (a
  /// non-vacuity counter). A one-shot control-shard question ([`Self::observation`]); an observation the
  /// daemon could not make is its typed refusal — unknown, not zero.
  pub fn council_contact(&self) -> Result<u64, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.council.leader_contact())
  }

  /// The configuration council's **election timing** as this daemon last derived it (§4.8 "Derived
  /// constants": "election timeout ≥ 10 × broadcast RTT p99 with the randomization span from RTT
  /// variance"; `slates_cluster::timing::ElectionTiming`): the base and span in coordinator periods, the
  /// measured tail and spread they were derived from, and the round trips behind them — so an observer
  /// tells a measured floor (samples counted) from a defaulted one. On a loopback fleet it is the floor,
  /// ten periods, by construction; across a WAN it is ten times the measured tail. A one-shot control-shard
  /// question ([`Self::observation`]); an observation the daemon could not make is its typed refusal —
  /// unknown, never the floor.
  pub fn council_timing(&self) -> Result<slates_cluster::timing::ElectionTiming, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.council_timing)
  }

  /// The root group's election timing, derived as [`Self::council_timing`] is over the root voters' paths.
  pub fn root_timing(&self) -> Result<slates_cluster::timing::ElectionTiming, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.root_timing)
  }

  /// A diagnostic dump of this daemon's regional council Raft and applied state
  /// ([`slates_cluster::config_group::RegionalCouncil::debug_state`]) — the committed configuration-change
  /// log with terms, the voter set, the joint-change flag and the commit indexes — for diagnosing a stalled
  /// membership commit. A one-shot control-shard observation.
  pub fn council_debug(&self) -> Result<String, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.council.debug_state())
  }

  /// The **regional membership** this daemon's configuration council has committed and applied so far
  /// (§4.8, D-14) — the members of the `RegionalConfiguration`, the region the council masters. A one-shot
  /// control-shard question ([`Self::observation`]); an observation the daemon could not make is its typed
  /// refusal — unknown, never an empty membership (a predicate "no longer a member" must not be satisfied
  /// by a shard that did not answer). Exposed so a fleet test can observe a membership change (a
  /// retirement or admission) commit across the council over the transport.
  pub fn council_members(&self) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.council.configuration().members.clone()
    })
  }

  /// The failure domains the regional configuration currently declares, by member (§4.8, D-14 — copysets
  /// form across distinct domains); a member absent from the map is unique-per-host. A one-shot
  /// control-shard question ([`Self::observation`]); an observation the daemon could not make is its typed
  /// refusal. Exposed so a fleet test can prove a restarted node's new member id inherited its node's
  /// declared domain through the council's admission (task #22).
  pub fn council_domains(
    &self,
  ) -> Result<
    std::collections::BTreeMap<slates_db::HostId, slates_db::register::DomainId>,
    ObserveError,
  > {
    self.observe(self.shards.first().copied(), |s| {
      s.council.configuration().domains.clone()
    })
  }

  /// The refusals this daemon's control shard has counted, by kind — the same counts `slates status`
  /// reports (§4.14): a peer refused at a serve socket, a serve bind that failed, a membership announcement
  /// whose member id did not derive from its authenticated certificate (task #22), and the
  /// rest. A one-shot control-shard question ([`Self::observation`]); an observation the daemon could not
  /// make is its typed refusal. Exposed so a test proves a refusal was counted rather than silently
  /// absorbed (banned item 9).
  pub fn fleet_refusals(
    &self,
  ) -> Result<std::collections::BTreeMap<&'static str, u64>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.refusals.clone())
  }

  /// The counters of this daemon's fleet demultiplexers — the probe plane's, then the record plane's
  /// (§4.14): sessions opened for a new source, sessions replaced by their peer's re-dial, handshakes
  /// refused for want of a session slot, and datagrams dropped. Empty for a laptop (no planes); an
  /// observation the daemon could not make is its typed refusal.
  pub fn fleet_demux_counters(
    &self,
  ) -> Result<Vec<slates_transport::demux::DemuxCounters>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.demuxes.iter().map(|demux| demux.counters()).collect()
    })
  }

  /// Live tasks in the control shard's arena (§4.14), the observation's own task among them. A test reads
  /// it before and after a burst of work to prove the burst's tasks ended — the accept-side serve tasks a
  /// peer's re-dials spawn, in particular, whose share of the arena is sized by `config::with_fleet`.
  /// An observation the daemon could not make is its typed refusal.
  pub fn live_tasks(&self) -> Result<usize, ObserveError> {
    self.observe_shard(self.shards.first().copied(), |shard| shard.live_tasks())
  }

  /// Whether this daemon leads the **root group** across regions (§4.8, D-14 — the root master). A one-shot
  /// control-shard question ([`Self::observation`]); an observation the daemon could not make is its typed
  /// refusal — unknown, never "not the leader". Exposed so a fleet test can prove the root group elected a
  /// leader over the transport.
  pub fn root_leads(&self) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.root.is_leader())
  }

  /// The **region membership** this daemon's root group has committed and applied so far (§4.8, D-14) — the
  /// regions of the `RootConfiguration`. A one-shot control-shard question ([`Self::observation`]); an
  /// observation the daemon could not make is its typed refusal — unknown, never an empty membership (a
  /// predicate "the lost region is gone" must not be satisfied by a shard that did not answer). Exposed so
  /// a fleet test can observe a region change (a promotion or a lost region's retirement) commit across the
  /// root group over the transport.
  pub fn root_regions(&self) -> Result<Vec<slates_db::register::RegionId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.root.configuration().regions.clone()
    })
  }

  /// Promotes a lost region's mirror through the root group (§4.8 "region loss promotes the mirror through the
  /// root group at operator cadence"): a deliberate operator failover, never automatic — a region that is
  /// merely partitioned must not be failed over while it is still serving (that would promote a second owner,
  /// split-brain). If this node leads the root group and `lost` has a declared mirror, it proposes
  /// `PromoteRegion{lost, mirror}`; the root group commits it over the transport, and every node then routes
  /// `lost`'s volumes to the mirror ([`RootConfiguration::home_of`](slates_db::register::RootConfiguration::home_of)).
  /// Returns whether the promotion was proposed — `Ok(false)` if this node is not the root leader, or `lost`
  /// has no mirror, or is not a current region; the typed refusal when the daemon could not be reached to
  /// ask ([`Self::observation`]: stopping, no shard, or the observe budget spent at a named stage). A
  /// one-shot control-shard action; the operator issues it (a CLI verb over this) after judging the region
  /// truly lost.
  pub fn promote_region(&self, lost: slates_db::register::RegionId) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      match s.region_mirrors.get(&lost) {
        Some(&mirror) => s
          .root
          .propose(slates_cluster::root_group::RootCommand::PromoteRegion { lost, mirror }),
        None => false,
      }
    })
  }

  /// The region a `volume` created in `creator_region` is served from, per this daemon's committed root
  /// configuration (§4.8 — its moved home if any, else its creator region, then any region promotion of that
  /// region followed to a fixed point): `RootConfiguration::home_of`. An observation the daemon could not
  /// make is its typed refusal ([`Self::observation`]) — unknown, never "still the creator region" (a shard
  /// that did not answer must not read as "not yet promoted" or as "still lost"). Exposed so a fleet test
  /// can prove a region-loss promotion re-homes the lost region's volumes to the mirror.
  pub fn region_home(
    &self,
    volume: slates_db::register::ObjectId,
    creator_region: slates_db::register::RegionId,
  ) -> Result<slates_db::register::RegionId, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.root.configuration().home_of(volume, creator_region)
    })
  }

  /// Like [`Self::region_home`] but answered on the shard at `shard_index` (an index into this daemon's shard
  /// list) rather than the control shard. The cross-region lookup guard (`verbs::home_redirect`) runs on
  /// whatever shard a client lands on, so every shard must read the committed root configuration; this lets a
  /// fleet test prove a promotion committed on the control shard reaches the others (`sync_root_to_shards`).
  /// No such shard, or one that could not be observed, is the typed refusal.
  pub fn region_home_on_shard(
    &self,
    shard_index: usize,
    volume: slates_db::register::ObjectId,
    creator_region: slates_db::register::RegionId,
  ) -> Result<slates_db::register::RegionId, ObserveError> {
    self.observe(self.shards.get(shard_index).copied(), move |s| {
      s.root.configuration().home_of(volume, creator_region)
    })
  }

  /// This daemon's **placement** neighbourhood (§4.8, D-14): the neighbourhood of the configuration the node
  /// currently places under — the owner view it **installed from the council** (`FleetNode::configuration`),
  /// the set the verbs draw candidates from. Distinct from [`council_members`](Daemon::council_members) (the
  /// council's committed region) and [`fleet_members`](Daemon::fleet_members) (the SWIM alive set): this is
  /// what the placement path actually reads, so a test can prove a council-committed change reaches
  /// placement. A one-shot control-shard question ([`Self::observation`]); an observation the daemon could
  /// not make is its typed refusal — unknown, never an empty neighbourhood.
  pub fn placement_neighbourhood(&self) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.fleet.configuration().neighbourhood.clone()
    })
  }

  /// Like [`Self::placement_neighbourhood`] but read on the shard at `shard_index` rather than the control
  /// shard. The placement verbs (`place`/`region_placed`/`await_placed`/`host_epoch`) run on a volume's owner
  /// shard, which may not be the control shard, so every shard must read the committed configuration; this
  /// lets a fleet test prove a council-committed change reaches every shard (`sync_config_from_council`'s
  /// fan-out). No such shard, or one that could not be observed, is the typed refusal.
  pub fn placement_neighbourhood_on_shard(
    &self,
    shard_index: usize,
  ) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.get(shard_index).copied(), |s| {
      s.fleet.configuration().neighbourhood.clone()
    })
  }

  /// Test and operator support: folds a `Dead` belief about `peer` at `incarnation` into this node's
  /// membership on the control shard — the same effect its failure detector has when it ages a peer to
  /// death (§4.8). Exposed so **rejoin** can be driven deterministically: a real process kill cannot be
  /// exercised in-process (a stopped daemon's serve socket is leaked to the process's lifetime and cannot be
  /// rebound, as a live deployment's OS would free it), so a test injects the (possibly false) death here
  /// and lets the still-live peer's own probing drive the recovery — the peer learns of the death from this
  /// node's probe echo, refutes past `incarnation` ([`Membership::refute`](slates_cluster::membership)), and
  /// is re-admitted. Synchronous: the injection is spawned onto the control shard — retried while that
  /// shard's control channel is full under load, never silently dropped ([`Daemon::observation`]) — and
  /// this waits for the fold, both bounded by the generous [`OBSERVE_BUDGET_NS`] (a starved shard can take
  /// seconds to service it). `Ok` once the death **landed and folded** within that budget, so a test asserts
  /// the cue it relies on actually reached the daemon instead of reading a starved shard's silence as
  /// delivery; the typed refusal on a laptop, a stopping daemon, or a shard that stayed unresponsive, naming
  /// the stage. Injected at a high `incarnation` so it wins over the current belief.
  pub fn observe_peer_dead(
    &self,
    peer: slates_db::HostId,
    incarnation: u64,
  ) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let dead = slates_cluster::membership::MemberState {
        liveness: slates_cluster::membership::Liveness::Dead,
        incarnation,
      };
      let _ = slates_cluster::fleet::apply_peer_state(&mut s.fleet, peer, Some(dead));
      crate::fleet::wake_link_waiter_of(s, peer);
    })
  }

  /// Injects a peer's **restart** into this node's membership for a test (§4.8 "Recovery"; task #22): the peer
  /// at `old` has come back under a **new** ephemeral member id `new` (a higher daemon generation), so `old`
  /// is folded `Dead` at `death_incarnation` (its process is gone — its objects are taken over) and `new` is
  /// folded `Alive` (a new member joins). This is exactly the fold the live `serve_peer_probes` /
  /// `probe_and_apply` learn-on-contact produces when the returning node's probes announce `new` and its `old`
  /// id stops answering — driven directly here because an in-process daemon cannot rebind its leaked fleet
  /// sockets to actually restart (the same reason the rejoin test injects `observe_peer_dead`). Delivered
  /// like [`Self::observe_peer_dead`]: admission retried while the control channel is full
  /// ([`Self::observation`]), the fold awaited, both within the observe budget; `Ok` once it **landed and
  /// folded**, so a test asserts its cue reached the daemon.
  pub fn observe_peer_restart(
    &self,
    old: slates_db::HostId,
    new: slates_db::HostId,
    death_incarnation: u64,
  ) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      // The old id is dead at a high incarnation (so it outranks its seeded-alive belief); the new id
      // joins alive. `apply_peer_state` is the incarnation-gated fold the detector uses.
      let _ = slates_cluster::fleet::apply_peer_state(
        &mut s.fleet,
        old,
        Some(slates_cluster::membership::MemberState {
          liveness: slates_cluster::membership::Liveness::Dead,
          incarnation: death_incarnation,
        }),
      );
      crate::fleet::wake_link_waiter_of(s, old);
      let _ = slates_cluster::fleet::apply_peer_state(
        &mut s.fleet,
        new,
        Some(slates_cluster::membership::MemberState {
          liveness: slates_cluster::membership::Liveness::Alive,
          incarnation: 0,
        }),
      );
    })
  }

  /// Test support: **starves** this daemon's control shard for `span_ns` — the CPU starvation a shared,
  /// oversubscribed box inflicts on a live daemon (measured here at load average 41: a live root voter was
  /// retired and re-admitted in a loop), injected deterministically so the fleet's tolerance of it is a
  /// test, not a hope. The hold is a task spawned onto the control shard that spins on the shard clock
  /// until the span has passed: the shard is cooperative, so while the hold runs nothing else on that shard
  /// does — its probe serve tasks (so its acknowledgements to every peer's probes stop for the whole span,
  /// then resume), its own probes, its record plane and its consensus loops. Admission is retried while the
  /// control channel is full ([`Self::observation`]); the typed refusal when there is no control shard or
  /// the hold was not admitted within the observe budget, otherwise the **admitted** hold, whose answer
  /// ([`Admitted::answer`]) is its **measured span** (nanoseconds) once it ends — so a test proves the
  /// starvation it relied on actually held. Returns once admitted: the caller watches the rest of the
  /// fleet while the shard is held. The hold's budget is the observe budget past the span it holds for.
  pub fn starve_control_shard(&self, span_ns: u64) -> Result<Admitted<u64>, ObserveError> {
    self
      .pending(
        self.shards.first().copied(),
        OBSERVE_BUDGET_NS.saturating_add(span_ns),
        move || {
          let started = slates_rt::futures::now_ns();
          let end = started.saturating_add(span_ns);
          while slates_rt::futures::now_ns() < end {
            std::hint::spin_loop();
          }
          Ok(slates_rt::futures::now_ns().saturating_sub(started))
        },
      )?
      .admit()
  }

  /// Whether this daemon's fleet has **region-placed** the head of `object` (§4.8): its control-shard
  /// membership loop replicated the head's record to the candidate holders and recorded a quorum of
  /// acknowledgements. A test or an operator reads this to observe cross-node replication: `Ok(false)` if
  /// it is not yet placed (or this is a laptop, where no fleet loop runs); the typed refusal when the owner
  /// shard could not be observed ([`Self::observation`]) — unknown, never "not placed". Runs a one-shot
  /// question on the object's owner shard.
  pub fn fleet_head_placed(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<bool, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      let quorum = s.fleet.configuration().quorum;
      s.placed_heads
        .get(&object)
        .is_some_and(|head| head.placement.placed(quorum))
    })
  }

  /// Whether this node **holds whole**, as a content candidate, the snapshot content with `manifest`
  /// identity (§4.10 "Content replication"): every chunk the manifest references, verified on arrival. A
  /// test or an operator reads this to observe that a holder received an owner's sealed content over the
  /// wire — the bytes a takeover successor materializes from. Non-vacuous: a holder holds nothing until the
  /// owner's content put reaches it and verifies. Runs a one-shot question on the control shard
  /// ([`Self::observation`]); an observation the daemon could not make is its typed refusal — unknown,
  /// never "does not hold".
  pub fn fleet_holder_content(&self, manifest: [u8; 32]) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.held_content.holds_manifest(&manifest)
    })
  }

  /// The **measured put latency** of the content class on the shard that owns `object` (§4.8 "Derived
  /// constants": "hedge delay = measured p95 put latency per class"): how many binding content
  /// acknowledgements that owner shard has timed, and their p95 in nanoseconds — the hedge trigger the next
  /// content round will use. A test reads this as the non-vacuity counter of the measured trigger: a seal
  /// whose content placed must have left readings behind, and the p95 must be what it hedged on. The typed
  /// refusal when the owner shard could not be observed ([`Self::observation`]); `Ok((0, None))` before any
  /// acknowledgement, and on a laptop, where no content round runs.
  pub fn fleet_put_latency(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<(usize, Option<u64>), ObserveError> {
    self.observe(self.shard_of_object(object), |s| {
      (s.put_latency.len(), s.put_latency.p95_ns())
    })
  }

  /// Test support: makes this node **forget** the content it holds for `manifest` as a candidate holder —
  /// the manifest record and the chunks only it referenced — the state a RAM-only holder is in after a
  /// restart (§4.8 "Recovery": it "holds nothing for others until re-replication fills it"), injected
  /// deterministically because an in-process daemon cannot restart (its fleet sockets are leaked to the
  /// process, the same reason `observe_peer_dead` injects a death). The healer (§4.10) must notice and
  /// re-put it. Delivered like [`Self::observe_peer_dead`]: admission retried while the control channel is
  /// full, the drop awaited, both within the observe budget; returns whether the manifest **was held and is
  /// now forgotten** — `Ok(false)` if it was not held, the typed refusal if the daemon could not be reached.
  pub fn drop_held_content(&self, manifest: [u8; 32]) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.held_content.forget_manifest(&manifest)
    })
  }

  /// Whether the merge record of `green`'s `version` has **placed** at `f + 1` candidate holders on
  /// this owner (§4.16 "Commit": "committed at f+1 acknowledgements, … issued only when every identity
  /// the version references is placed"). `Ok(false)` while the record waits — on its inputs' placement,
  /// or on its holders — or on a laptop before the version exists; the typed refusal when the owner shard
  /// could not be observed. Runs a one-shot question on the green's owner shard.
  pub fn merge_record_placed(
    &self,
    green: slates_ipc::protocol::VolumeId,
    version: u64,
  ) -> Result<bool, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    self.observe(self.shard_of_object(object), move |s| {
      crate::merge_service::placed_version(s, object).is_some_and(|placed| placed >= version)
    })
  }

  /// What this node holds as a **candidate holder** of `green`'s merge chain (§4.16 "Apply on
  /// holders"): the replica's head version once the owner's records reached and recomputed here, and
  /// whether this holder refused the green for good after a recomputation mismatch. A test reads it as
  /// the non-vacuity of holder recomputation: a holder holds nothing until a record it recomputed
  /// arrived. Runs a one-shot question on the control shard; the typed refusal when it could not be
  /// observed.
  pub fn merge_holder_state(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<crate::merge_service::HolderMergeState, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    self.observe(self.shards.first().copied(), move |s| {
      crate::merge_service::holder_state(s, object)
    })
  }

  /// Test support: injects a merge-plane fault on this node's control shard (§4.16; never reachable
  /// from the wire): refuse every content put while set, so an owner's merge record must wait for its
  /// inputs to place; or corrupt the next inputs a record is recomputed from, so the recomputation
  /// mismatches. Delivered like [`Self::observe_peer_dead`]; `Ok` once installed, else the typed refusal.
  pub fn inject_merge_fault(
    &self,
    fault: crate::merge_service::MergeFault,
  ) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| s.merge.fault = fault)
  }

  /// Test support: holds every discovery reply this node's record serve side would send (never reachable
  /// from the wire) while `withhold` is set, so a peer's refresh exchange to this node stays pending with
  /// its record endpoint borrowed by that exchange — the interrupted-discovery restart the
  /// replacement-voter regression forces by stopping this node while it holds
  /// (`docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`). Delivered like
  /// [`Self::inject_merge_fault`]; `Ok` once installed, else the typed refusal.
  pub fn inject_discovery_fault(&self, withhold: bool) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.discovery_withhold_replies = withhold;
    })
  }

  /// This node's record links (§4.8, `ShardState::record_sessions`): each peer with an entry, and whether
  /// its session is out on a borrow at the moment of the read — a dispatch's, or the link task's own
  /// discovery exchange. A test reads it to prove an exchange is pending on a link before interrupting it,
  /// and that a link to a fresh member exists after. The typed refusal when the control shard could not be
  /// observed.
  pub fn fleet_record_links(&self) -> Result<Vec<(slates_db::HostId, bool)>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.record_sessions
        .iter()
        .map(|(host, link)| (*host, link.endpoint.is_none()))
        .collect()
    })
  }

  /// How many placed snapshots the shard owning `object` has **repaired** (§4.10 "the healer"): re-put to
  /// a recorded holder that answered the healer's offer with chunks it lacked. The non-vacuity counter a
  /// test reads — a holder shown to hold content again proves the repair only together with this count
  /// having moved, since a holder could also be refilled by a fresh seal. The typed refusal when the owner
  /// shard could not be observed; `Ok(0)` before any repair, and on a laptop.
  pub fn fleet_repairs(&self, object: slates_db::register::ObjectId) -> Result<u64, ObserveError> {
    self.observe(self.shard_of_object(object), |s| s.repairs)
  }

  /// The manifest identity of the head snapshot of the volume `object` names, once the content plane has
  /// archived it (§4.10; recorded durably as `SnapshotIdentified`), or `Ok(None)` before then, for a volume
  /// with no snapshot, or for one this node does not own; the typed refusal when the owner shard could not
  /// be observed ([`Self::observation`]) — kept apart from "no identity yet", which a poll would otherwise
  /// read into a starved shard. Runs a one-shot question on the object's owner shard.
  pub fn fleet_head_manifest(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<[u8; 32]>, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      let id = slates_db::catalog::VolumeId { bytes: object.0 };
      let record = s.db.partition().volume(id)?;
      s.db.partition().snapshot(id, record.head)?.identity
    })
  }

  /// The head this node **durably holds** for `object` as a candidate holder (§4.8 "records are sent to
  /// all candidates"): the object's current owner (as this node's routing view records it) and the value
  /// of the highest record this node has accepted for it, or `None` if this node backs no such object. A
  /// test or an operator reads this to observe that a survivor durably holds an owner's replicated head —
  /// the state phase-one recovery reads on a takeover. Non-vacuous: before the head replicates, or on a
  /// laptop, this node holds nothing and the answer is `Ok(None)`. Runs a one-shot question on the control
  /// shard ([`Self::observation`]) — before 2026-09-17 this one accessor spawned unretried under the
  /// liveness budget alone, so a full control channel under load read as "holds nothing"; the typed
  /// refusal when the daemon is stopping or the shard does not answer within the observe budget.
  pub fn fleet_holder_head(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<(slates_db::HostId, Vec<u8>)>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let owner = s.fleet.object_owner(object)?;
      let (_, positions) = s.holder_records.get(&object)?.persisted();
      let value = positions
        .into_iter()
        .filter(|(held_object, _, _, _)| *held_object == object)
        .max_by_key(|(_, sequence, epoch, _)| (*sequence, epoch.0))
        .map(|(_, _, _, value)| value)?;
      Some((owner, value))
    })
  }

  /// The shards.
  pub fn shards(&self) -> &[ShardId] {
    &self.shards
  }

  /// The shard that owns the volume `object` names — the partition its id encodes (`verbs::owner_of`),
  /// where the volume, its placement and its seal live (D-7: one owning shard per volume). An owner-shard
  /// fact is queried there, never on the control shard.
  fn shard_of_object(&self, object: slates_db::register::ObjectId) -> Option<ShardId> {
    let partition = verbs::owner_of(slates_ipc::protocol::VolumeId { bytes: object.0 });
    self.shards.get(usize::from(partition)).copied()
  }

  /// The segment (the daemon's own mapping).
  pub fn segment(&self) -> &AnchorSegment {
    &self.segment
  }

  /// Stops the daemon: the doorbell thread, then every shard, joined.
  pub fn stop(mut self) {
    if let Some(mut doorbell) = self.doorbell.take() {
      doorbell.stop();
    }
    if let Some(runtime) = self.runtime.take() {
      runtime.shutdown();
    }
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    if let Some(mut doorbell) = self.doorbell.take() {
      doorbell.stop();
    }
    if let Some(runtime) = self.runtime.take() {
      runtime.shutdown();
    }
  }
}

/// The chokepoint span emitters this daemon declares at boot (§4.14 "Span roster", §2.6): the health
/// plane refuses to serve until every one has registered. Registration proves the emitter *exists*,
/// not that it is *live* (a registered emitter may be idle — a laptop runs no consensus or replication
/// yet still declares those chokepoints, R8). Each line names the subsystem that owns the emitter, so
/// a future refactor moves each `register` to that subsystem's own initialization and this function
/// becomes the point that confirms they all reported in. A missing line here shuts the gate.
fn registered_chokepoints() -> ChokepointRegistry {
  let mut registry = ChokepointRegistry::new();
  registry.register(Chokepoint::BridgeRequest); // the NFS/FSKit bridge request (§4.6, `crate::nfs`)
  registry.register(Chokepoint::RingRequest); // a client ring slot (the control loop, below)
  registry.register(Chokepoint::ShardOp); // one verb on its owner shard (`crate::verbs`)
  registry.register(Chokepoint::LogAppend); // an op-log record appended (`slates_db::replay`)
  registry.register(Chokepoint::ShipRecord); // a record shipped to its candidates (§4.8 register)
  registry.register(Chokepoint::ConsensusStep); // a configuration commit (§4.8 config group)
  registry.register(Chokepoint::ArchiveChunk); // a chunk compressed or expanded (§4.10 archive)
  registry.register(Chokepoint::LandEntry); // one landing entry (§4.15, `crate::landing`)
  registry.register(Chokepoint::MergeVerdict); // one increment judged (§4.16 merge)
  registry
}

/// The node's host id: the first eight bytes of the machine identity's hash. Public so a fleet operator or
/// a multi-node test computes a node's host id from its machine identity the same way the daemon does — the
/// id a peer names in its fleet configuration (§4.8).
pub fn host_id_of(identity: &Identity) -> u64 {
  let hash = identity.hash();
  u64::from_le_bytes([
    hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
  ])
}

/// This daemon's public boot nonce (§4.8, AUD-07), derived with a separate keyed-hash domain
/// from the fresh secret minted once before its shards start. A reset supervision counter cannot
/// repeat the old member identity. The secret itself never crosses the membership wire.
fn boot_incarnation(secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES]) -> u64 {
  let digest = blake3::keyed_hash(secret, b"slates/member-incarnation/v1");
  let mut word = [0; size_of::<u64>()];
  word.copy_from_slice(&digest.as_bytes()[..size_of::<u64>()]);
  u64::from_le_bytes(word)
}

/// The content object's name for a segment named `seg_name`: the segment name with its `seg`
/// marker replaced by `con`, so it stays the same length (within the platform's shared-object name
/// limit the segment already meets) and is distinct from the segment's own name.
fn content_name_of(seg_name: &str) -> String {
  match seg_name.strip_prefix("slates-seg-") {
    Some(rest) => format!("slates-con-{rest}"),
    None => format!("{seg_name}-c"),
  }
}

/// Slots per shard in the content object: the recovery image is published as a double buffer (§4.8),
/// so each shard's slice holds two reserve-sized slots — the last committed image and the one being
/// published. An interrupted publish lands in the non-committed slot, so the committed one always
/// survives. Two is the minimum for that guarantee (a single slot cannot survive a torn write of
/// itself); more slots would only add unused space.
const PUBLISH_SLOTS: usize = 2;

/// The content object's total size: two reserve-sized slots per shard (a double buffer) times the
/// partitions, so each shard owns space for its committed recovery image and the one it is writing
/// (§4.8). The object is lazily backed, so the unused tail costs address space, not RAM, until an
/// image is published into it.
fn content_bytes(config: &DaemonConfig) -> usize {
  let per_shard = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX);
  per_shard
    .saturating_mul(PUBLISH_SLOTS)
    .saturating_mul(usize::from(config.geometry.partitions.max(1)))
}

fn handoff_of(env: &[(String, String)]) -> Result<(Handoff, usize), ServerError> {
  let handoff = env
    .iter()
    .find(|(k, _)| k == slates_anchor::segment::ENV_HANDOFF)
    .map(|(_, v)| v.clone())
    .ok_or(ServerError::NotOnShard)?;
  let len: usize = env
    .iter()
    .find(|(k, _)| k == slates_anchor::segment::ENV_LEN)
    .and_then(|(_, v)| v.parse().ok())
    .ok_or(ServerError::NotOnShard)?;
  let handoff = match handoff.parse::<i32>() {
    Ok(fd) if cfg!(target_os = "linux") => Handoff::Descriptor(fd),
    _ => Handoff::Name(handoff),
  };
  Ok((handoff, len))
}

/// Builds an uninitialized root participant and the manifest's region map (§4.8, AUD-07).
/// A missing region declaration means region zero, including on a laptop. The manifest cannot
/// initialize voters: that requires explicit bootstrap or a common-prefix join.
fn build_root_group(
  config: &DaemonConfig,
  host: slates_db::HostId,
) -> (
  slates_cluster::root_group::RootGroup,
  std::collections::BTreeMap<slates_db::HostId, slates_db::register::RegionId>,
  std::collections::BTreeMap<slates_db::register::RegionId, slates_db::register::RegionId>,
) {
  let mut regions: std::collections::BTreeMap<_, _> = config
    .fleet
    .as_ref()
    .map(|fleet| fleet.regions.clone())
    .unwrap_or_default();
  if let Some(fleet) = &config.fleet {
    let region = regions
      .get(&fleet.host)
      .copied()
      .unwrap_or(slates_db::register::RegionId(0));
    regions.insert(host, region);
  }
  let mirrors = config
    .fleet
    .as_ref()
    .map(|fleet| fleet.region_mirrors.clone())
    .unwrap_or_default();
  (
    slates_cluster::root_group::RootGroup::learner(host),
    regions,
    mirrors,
  )
}

/// Runs on the shard: attaches the segment, recovers the partition, builds the store and
/// installs the state, then spawns the server loop as a poller.
fn init_shard(
  config: &DaemonConfig,
  env: &[(String, String)],
  identity: &Identity,
  partition: u16,
  config_shards: &[u16],
  retained: Option<crate::retention::Retained>,
) -> Result<(), ServerError> {
  let (handoff, len) = handoff_of(env)?;
  let mut segment = AnchorSegment::attach(&handoff, len, identity)?;
  let mut clock = HostClock::new();
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
  let (db, recovered) = slates_db::replay::recover(&mut segment, partition, config.caps, now)?;
  let mut arena = ChunkArena::new(config.page);
  let region_len = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX);
  arena.add_region(Region::map(
    region_len.max(config.page),
    config.page,
    config.huge_pages,
  )?)?;
  // The operation headroom (§4.2): the bounded temporary coexistence of in-flight operations, kept
  // free of every admission (reservation and dynamic growth alike). A write into a sealed chunk
  // copies it into a new open extent — copy-on-write at chunk granularity — so the source chunk and
  // its destination coexist (two chunk windows) until the seal. A copy-up exists only while a write
  // is IN FLIGHT, and a shard admits at most `requests_in_flight_per_shard` operations at once (its
  // Little's-law admission limit, carried as `config.caps.attachments`), so the concurrent copy-ups
  // are bounded by min(seatable clients, in-flight operations) — never one per seatable client. The
  // earlier `× clients_per_shard` reserved a copy-up for every client the RAM could seat (hundreds),
  // which on a large page (macOS arm64's 16 KiB) drove the headroom past the arena's own capacity and
  // refused every volume with `BudgetExceeded { available: 0 }`
  // (docs/bugs/2026-09-16-operation-headroom-exceeds-the-arena-capacity.md).
  let chunk_window =
    u64::try_from(slates_vfs::content::chunk_bytes(config.page).get()).unwrap_or(u64::MAX);
  let in_flight = u64::try_from(config.caps.attachments).unwrap_or(1).max(1);
  let concurrent_writers = u64::try_from(config.clients_per_shard)
    .unwrap_or(1)
    .max(1)
    .min(in_flight);
  // D-12 "degrade and keep serving": the headroom is a reserve *within* the arena's usable capacity,
  // so it can never consume the whole arena — a shard whose operation headroom ≥ its capacity refuses
  // every volume. Cap it to leave at least one chunk window admittable, whatever the machine.
  let capacity = u64::try_from(arena.capacity()).unwrap_or(u64::MAX);
  let headroom = derived!(
    chunk_window
      .saturating_mul(2)
      .saturating_mul(concurrent_writers)
      .min(capacity.saturating_sub(chunk_window)),
    "2 × chunk_bytes × min(clients_per_shard, requests_in_flight_per_shard), capped to leave one chunk window admittable",
    [
      "vfs.chunk_bytes",
      "clients_per_shard",
      "requests_in_flight_per_shard"
    ]
  );
  // The store owns the shard budget (§4.2): it is over what the arena can actually hand out (its
  // buddy-allocatable capacity), not the region's mapping length, so admission never promises quota
  // the arena cannot back (BUG-2), and it keeps the derived operation headroom free of every
  // admission. Living with the store, the write path reaches it without a lock.
  let mut store = Store::new(
    &StoreConfig {
      page: config.page,
      cache_line: config.cache_line,
      max_dirs: config.store.max_dirs,
      max_inodes: config.store.max_inodes,
      max_chunks: config.store.max_chunks,
      max_dir_blocks: config.store.max_dir_blocks,
      dir_cutover: config.store.dir_cutover,
    },
    arena,
    headroom.get(),
  );
  // The metadata class (§4.2 metadata dimension): the slabs' maximum footprint comes off the top and
  // the remainder is the ledger every volume's records are reserved from; a layout whose slabs alone
  // exceed the class is refused here, before serving, rather than admitting records it cannot back.
  store
    .set_metadata_class(config.store.metadata_class_bytes)
    .map_err(ServerError::Memory)?;
  let shard = registry::current_shard().unwrap_or(partition);
  // The stable anchor keys TLS authentication and completion origins (§4.8). A random
  // per-start nonce derives the member id used for voting and ownership. The anchor's
  // supervision counter cannot supply freshness after whole-pod RAM loss (AUD-07).
  let origin_anchor = config.fleet.as_ref().map_or_else(
    || slates_db::HostId(host_id_of(identity)),
    |m| m.origin_anchor,
  );
  let issuer_secret = segment.issuer_secret()?;
  let incarnation = retained
    .as_ref()
    .map_or_else(|| boot_incarnation(&issuer_secret), |record| record.nonce);
  let host = crate::deploy::member_id(origin_anchor, incarnation);
  // The owner runtime this node takes part in a region as (§4.8, boot step 6): membership + the
  // configuration group + the owner's acceptor, composed by `slates-cluster`. Built from the configured
  // fleet membership (its quorum and peers) when the operator deploys a fleet, or the laptop `f = 0`
  // degenerate — `solo`, one member, itself the owner — when there is none; `solo` is `new(host, f = 0,
  // no peers)`, so it is the same code path a fleet runs (R8), not a branch in behaviour. The placement
  // authority the verbs read is `fleet.configuration()`; the live probe/gossip loop that folds
  // membership into it (and the cross-node commit) are the next fleet pieces.
  // The regional council starts uninitialized on every daemon boot (§4.8, AUD-07). Neither
  // a deployment roster nor retained volume bytes restore a term, vote or log. An explicit
  // first-time bootstrap or a validated join initializes it; writes wait for admission.
  let quorum = config
    .fleet
    .as_ref()
    .map_or(slates_db::register::Quorum { f: 0 }, |fleet| fleet.quorum);
  let council = slates_cluster::config_group::RegionalCouncil::learner(
    host,
    quorum,
    retained
      .as_ref()
      .map_or_else(|| config.derived_scatter(quorum), |record| record.scatter()),
    false,
  );
  // The discovery view holds manifest placeholders until authenticated contact. Placement
  // becomes usable only after this fresh member has joined its committed regional group.
  let mut fleet = match &config.fleet {
    Some(membership) => {
      slates_cluster::fleet::FleetNode::new(host, membership.quorum, &membership.peers)
    }
    None => slates_cluster::fleet::FleetNode::solo(host),
  };
  let council_members = council.configuration().members.clone();
  let mut durability_shortfall = None;
  if let Some(configuration) = council.configuration().configuration_for(host) {
    let counts = config_shards.first() == Some(&shard);
    durability_shortfall = boot_durability(config, &configuration, counts);
    let _ = fleet.install_configuration(configuration, &council_members);
  }
  // The root group across regions (§4.8, D-14): the regions the fleet spans and the representative host of
  // each (the root voters), driven over the transport by the control shard's config plane
  // (`crate::fleet::drive_root_group`), exactly as the regional council is.
  let (root, node_regions, region_mirrors) = build_root_group(config, host);
  // The anchor-owned content object that survives a restart (§4.8), if the anchor provides one.
  // Shards share the one object, partitioned by index: this shard owns the slice `[start, end)`.
  let content = match AnchorSegment::open_content(env) {
    Some(result) => Some(result?),
    None => None,
  };
  let content_range = match &content {
    Some(object) => {
      let partitions = config_shards.len().max(1);
      let per_shard = object.len() / partitions;
      let start = usize::from(partition).saturating_mul(per_shard);
      (start, start.saturating_add(per_shard))
    }
    None => (0, 0),
  };
  // The telemetry sink keeps the most recent spans up to one client ring's depth (§4.14): a shard
  // processes at most a ring of in-flight requests, so a ring's depth of recent spans covers the
  // current activity window; older spans are shed (and counted), telemetry being the shed-first class.
  let telemetry_capacity = usize::try_from(config.region.slots).unwrap_or(1).max(1);
  // The grant-issuer secret this daemon minted at start (§4.13), read from the anchor's supervision block
  // before the segment moves into the state; every shard reads the same value, so a `Grant` verifies on
  // whichever shard serves the client.
  let issuer_secret = segment.issuer_secret()?;
  let mut state = ShardState {
    shard,
    partition,
    config: config.clone(),
    segment,
    issuer_secret,
    content,
    content_range,
    write_verifier: now.to_be_bytes(),
    db,
    fleet,
    durability_shortfall,
    origin_anchor,
    landing: crate::landing::LandingState::default(),
    store,
    volumes: Slab::new(config.caps.segment_slots, config.caps.volumes),
    by_id: std::collections::BTreeMap::new(),
    clients: Slab::new(config.caps.segment_slots, config.clients_per_shard),
    next_prefix: partition
      .saturating_mul(u16::try_from(config.caps.segment_slots).unwrap_or(u16::MAX))
      .max(1),
    next_attachment: 1,
    clock,
    served: 0,
    refusals: std::collections::BTreeMap::new(),
    deferred: Vec::new(),
    server_task: None,
    scatters: std::collections::BTreeMap::new(),
    grant_scatters: std::collections::BTreeMap::new(),
    shards: config_shards.to_vec(),
    recovered,
    booted_ns: now,
    status_scatters: std::collections::BTreeMap::new(),
    last_work_ns: now,
    pending_forwards: std::collections::VecDeque::new(),
    greens: std::collections::BTreeMap::new(),
    works: std::collections::BTreeMap::new(),
    merge: crate::merge_service::MergeShardState::default(),
    ack_scatters: std::collections::BTreeMap::new(),
    telemetry: slates_wire::observe::SpanSink::with_capacity(telemetry_capacity),
    // The shard's span opener folds in this node's member id and the partition, so every trace and
    // span id it mints is distinct across the daemon and the fleet without coordination (§4.14).
    tracer: slates_wire::observe::Tracer::new(host.0, partition),
    current_span: None,
    forwarded_rings: std::collections::BTreeMap::new(),
    telemetry_quota: config.telemetry_spans_per_reply,
    last_drain_ns: now,
    placed_heads: std::collections::BTreeMap::new(),
    formed_probe_peers: std::collections::BTreeSet::new(),
    member_boot_nonce: incarnation,
    learned_members: std::collections::BTreeMap::new(),
    authenticated_members: std::collections::BTreeSet::new(),
    council_death_watch: std::collections::BTreeMap::new(),
    discovery: None,
    enrolled: Vec::new(),
    demuxes: Vec::new(),
    holder_records: std::collections::BTreeMap::new(),
    pending_takeovers: std::collections::BTreeSet::new(),
    config_refresh_wanted: false,
    record_sessions: std::collections::BTreeMap::new(),
    link_waiters: std::collections::BTreeMap::new(),
    discovery_withhold_replies: false,
    peer_paths: std::collections::BTreeMap::new(),
    probe_windows: crate::fleet::ProbeWindows::default(),
    council_timing: slates_cluster::timing::ElectionTiming::floor(),
    root_timing: slates_cluster::timing::ElectionTiming::floor(),
    council,
    consensus_ready: false,
    consensus_generation: 0,
    recovery: crate::consensus_recovery::RecoveryState::default(),
    consensus_failure: None,
    bootstrap_authorized: None,
    council_group: None,
    root_group: None,
    root,
    node_regions,
    region_mirrors,
    held_content: slates_cluster::content::ContentHold::new(),
    seals: std::collections::BTreeMap::new(),
    put_latency: crate::fleet::PutLatency::default(),
    put_outcomes: crate::fleet::PutOutcomes::default(),
    healer: crate::fleet::HealerCursor::default(),
    repairs: 0,
    pending_materializations: std::collections::BTreeMap::new(),
  };
  if let Some(retained) = retained {
    retained.restore(&mut state)?;
  }
  crate::retention::retain(&mut state)?;
  let rebuilt = verbs::rebuild_recovered(&mut state);
  if rebuilt.skipped > 0 {
    RECOVERY_SKIPPED.fetch_add(
      u64::try_from(rebuilt.skipped).unwrap_or(u64::MAX),
      Ordering::AcqRel,
    );
  }
  if rebuilt != verbs::Rebuilt::default() {
    // Say what recovery did, not more: volumes rebuilt from their recovery images in anchor-owned
    // RAM (content, tree and snapshots restored, §4.8), the ones that could not be (refused, never
    // presented empty), and what the images did not carry and was reconciled out of the catalog.
    eprintln!(
      "slates-server: shard {shard}: recovered {} volumes from their images ({} refused), {} merge volumes (green chains replayed, works reset), reconciled out {} unrecovered local snapshots and {} attachments, trimmed {} unacknowledged snapshots the images carried, completed {} destroys in flight, corrected {} clone pins",
      rebuilt.volumes,
      rebuilt.skipped,
      rebuilt.merge_volumes,
      rebuilt.snapshots_dropped,
      rebuilt.attachments_dropped,
      rebuilt.snapshots_trimmed,
      rebuilt.destroys_completed,
      rebuilt.pins_reconciled
    );
  }
  state::install(state);
  // Detached: the loop lives as long as the shard; nothing joins it (a joinable task stays in
  // the arena after it ends, which would hold the shard's shutdown).
  // A shard whose serve or reap loop the arena refuses serves nothing: a typed initialization failure
  // (surfaced by the boot as `INIT_FAILURES`), never a silently loop-less shard (banned item 9).
  let serve = futures::spawn(serve_loop()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(serve);
  let reap = futures::spawn(reap_loop()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(reap);
  Ok(())
}

/// The shard's sweep for dead clients and expired leases, at the liveness cadence: the same
/// budget the anchor allows the daemon's own heartbeat, so a peer silent that long is asked
/// about (§4.7 "Failure matrix"). Derived: the cadence is the budget; a client is asked about
/// once silent for the budget, so a death is seen within two budgets at most.
async fn reap_loop() {
  let cadence = derived!(
    LIVENESS_BUDGET_NS,
    "the liveness budget (the anchor's question of the daemon, asked of the daemon's clients)",
    ["LIVENESS_BUDGET_NS"]
  )
  .get();
  loop {
    futures::sleep(cadence).await;
    let reaped = state::with_state(|s| {
      let _ = verbs::expire_leases(s);
      verbs::reap_dead_clients(s, cadence)
    })
    .unwrap_or_default();
    if reaped.clients > 0 {
      CLIENTS_REAPED.fetch_add(
        u64::try_from(reaped.clients).unwrap_or(u64::MAX),
        Ordering::AcqRel,
      );
    }
  }
}

/// The shard's server loop: a poller of its clients' rings; serves while there is work, keeps
/// polling for the idle window after its last work (§4.7 "Wake strategy": a shard polls while
/// any client has activity within the window), and idles past it.
async fn serve_loop() {
  if let Some(task) = futures::current_task() {
    let _ =
      registry::with_current(|ctx| ctx.register_poller(task, Box::new(state::any_ring_ready)));
    state::with_state(|s| s.server_task = Some(task));
  }
  let idle_window_ns = state::with_state(|s| {
    derived!(
      u64::from(s.config.region.spin_ns).saturating_mul(crate::config::IDLE_WINDOW_RATIO),
      "spin_ns × IDLE_WINDOW_RATIO",
      ["wake.p99_ns", "IDLE_WINDOW_RATIO"]
    )
    .get()
  })
  .unwrap_or(0);
  loop {
    let (did, within_window) = state::with_state(|s| {
      let did = verbs::serve_round(s);
      let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
      if did {
        s.last_work_ns = now;
      }
      (did, now.saturating_sub(s.last_work_ns) < idle_window_ns)
    })
    .unwrap_or((false, false));
    if did || within_window {
      futures::yield_now().await;
    } else {
      state::with_state(|s| verbs::mark_parked(s, true));
      futures::idle().await;
      state::with_state(|s| verbs::mark_parked(s, false));
    }
  }
}

/// The control shard's loop: the rendezvous, the heartbeat, and a client handed to its shard
/// as a spawned task (sharing by move).
async fn control_loop(
  mut listener: Listener,
  config: DaemonConfig,
  env: Vec<(String, String)>,
  identity: Identity,
  shards: Vec<ShardId>,
) {
  let Ok((handoff, len)) = handoff_of(&env) else {
    return;
  };
  let Ok(segment) = AnchorSegment::attach(&handoff, len, &identity) else {
    return;
  };
  // This loop runs on the control shard, the first of `shards`; its doorbell flag is that shard's.
  let control = shards.first().map_or(0, |shard| shard.0);
  if let Some(task) = futures::current_task() {
    let _ = registry::with_current(|ctx| {
      ctx.register_poller(
        task,
        Box::new(move || doorbell_flag(control).swap(false, Ordering::AcqRel)),
      )
    });
  }
  match futures::spawn(heartbeat_loop(segment)) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      crate::fleet::count_refusal(LOOP_SPAWN_REFUSED);
    }
  }
  // Clients this daemon handed out, so a wanted id that is live is not given twice; bounded
  // by the daemon's client capacity, refused typed beyond it (AC-2.6). A client's shard is
  // its id's residue, so a client reconnecting under its old id after a restart lands on the
  // shard that holds its completion records (§4.9), with fresh ids still round-robin.
  let bound = derived!(
    config.clients_per_shard.saturating_mul(shards.len()),
    "clients_per_shard × shards",
    ["clients_per_shard", "shards"]
  )
  .get();
  loop {
    let accepted = listener.accept_pending(
      &|id| state::with_handed(|h| h.contains(&id)),
      &mut |client_id| {
        if state::with_handed(|h| h.len()) >= bound {
          CLIENTS_REFUSED.fetch_add(1, Ordering::AcqRel);
          return Err(slates_ipc::IpcError::TooManyClients { limit: bound });
        }
        // The id is reserved in the live set **here**, at admission, not once the accept round returns:
        // an accept round serves every pending connection before it returns, so a burst of connects
        // inside one round was admitted against a live set that did not yet count the earlier ones —
        // the bound of one admitted two (2026-09-14). A region that cannot be created gives the
        // reservation back at once.
        state::with_handed(|h| h.insert(client_id));
        let shard = shards[usize::try_from(client_id).unwrap_or(0) % shards.len().max(1)];
        let region = match ClientRegion::create(
          &format!("slates-cr-{}-{client_id}", config.instance),
          client_id,
          shard.0,
          config.region,
        ) {
          Ok(region) => region,
          Err(e) => {
            state::with_handed(|h| h.remove(&client_id));
            return Err(e);
          }
        };
        Ok(Prepared {
          region,
          kick_fd: kick_fd_of(shard),
        })
      },
    );
    let accepted = match accepted {
      Ok(accepted) => accepted,
      // The client was admitted (its id reserved) and its handoff then failed: the id goes back, the
      // loss is counted and logged once. Any other failure of the round is counted and logged once;
      // the round admitted nothing.
      Err(slates_ipc::IpcError::HandoffLost { client_id, cause }) => {
        state::with_handed(|h| h.remove(&client_id));
        if HANDOFF_LOST.fetch_add(1, Ordering::AcqRel) == 0 {
          eprintln!(
            "slates-server: client {client_id}'s handoff failed after admission: {cause} (first \
             occurrence; later ones are counted)"
          );
        }
        Vec::new()
      }
      Err(e) => {
        if ACCEPTS_FAILED.fetch_add(1, Ordering::AcqRel) == 0 {
          eprintln!(
            "slates-server: a rendezvous accept round failed: {e} (first occurrence; later ones are \
             counted)"
          );
        }
        Vec::new()
      }
    };
    {
      for a in accepted {
        let shard = ShardId(a.region.shard());
        let principal = Principal::Uid { uid: a.uid };
        let client_id = a.client_id;
        let mut admission = Admission {
          client_id,
          control,
          seated: false,
        };
        // Dup the completion eventfd (Linux) for the daemon's end to nudge on a reply to a parked
        // client, so an async SDK event loop wakes (D-19); the original stays in the slot's control for
        // liveness. macOS/Windows have no completion fd yet.
        #[cfg(unix)]
        let completion = a.completion_dup();
        let control = a.control;
        let pid = a.pid;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut end = slates_ipc::DaemonEnd::new(a.region);
        #[cfg(unix)]
        end.set_completion(completion);
        let request = Box::new(SpawnRequest::new(
          Box::pin(async move {
            // The server task may be idle with its parked flags set on the clients it knew;
            // a wake makes it mark the new client too before it idles again.
            let server = state::with_state(|s| {
              let last_seen_ns = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
              match s.clients.insert(ClientSlot {
                end,
                principal,
                client_id,
                pid,
                last_seen_ns,
                control,
                revoked: false,
                owner_route: None,
              }) {
                Ok(_) => admission.seat(),
                Err(e) => {
                  eprintln!("slates-server: client {client_id} refused by the shard's table: {e}");
                }
              }
              s.server_task
            });
            match server {
              Some(Some(task)) => {
                registry::wake(task.0);
              }
              Some(None) => {}
              None => {
                HANDOFF_LOST.fetch_add(1, Ordering::AcqRel);
                eprintln!("slates-server: client {client_id} handed to a shard without state");
              }
            }
          }),
          None,
        ));
        if let Err(e) = registry::send_control(shard.0, Control::Spawn(request)) {
          HANDOFF_LOST.fetch_add(1, Ordering::AcqRel);
          eprintln!(
            "slates-server: client {client_id} could not be handed to shard {}: {e}",
            shard.0
          );
        }
        let _ = registry::send_control(shard.0, Control::Active(true));
      }
    }
    futures::idle().await;
  }
}

/// The heartbeat: the anchor's `daemon.alive` input, beaten at a cadence inside its budget.
async fn heartbeat_loop(segment: AnchorSegment) {
  let mut clock = HostClock::new();
  loop {
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
    if let Ok(sup) = segment.supervision() {
      sup.beat(now);
    }
    futures::sleep(HEARTBEAT_NS).await;
  }
}

#[cfg(target_os = "linux")]
fn kick_fd_of(shard: ShardId) -> Option<i32> {
  match registry::with_entry(shard.0, |e| e.kick) {
    Some(slates_rt::driver::Kick::Eventfd(fd)) => fd.raw(),
    _ => None,
  }
}

#[cfg(not(target_os = "linux"))]
fn kick_fd_of(_shard: ShardId) -> Option<i32> {
  None
}

mod limits {
  //! The process's descriptor limit: every client costs descriptors (its region, its control
  //! channel, its completion signal), and the derived client bound (AC-2.6) is far above the
  //! soft limit a shell hands out; the daemon raises its own soft limit to the hard one, which
  //! needs no privilege. Paired `#[cfg]` functions, one signature.

  /// Raises the soft limit to the hard one; `(before, after)` when it changed.
  #[cfg(unix)]
  pub(super) fn raise_descriptor_limit() -> Option<(u64, u64)> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    let current = limit.current?;
    let maximum = limit.maximum.unwrap_or(u64::MAX);
    let wanted = platform_cap(maximum);
    if wanted <= current {
      return None;
    }
    setrlimit(
      Resource::Nofile,
      Rlimit {
        current: Some(wanted),
        maximum: limit.maximum,
      },
    )
    .ok()?;
    Some((current, wanted))
  }

  /// Format: macOS `OPEN_MAX` (`<sys/syslimits.h>`): `setrlimit` refuses a soft descriptor
  /// limit above it even when the hard limit is unlimited.
  #[cfg(target_os = "macos")]
  const MACOS_OPEN_MAX: u64 = 10_240;

  /// macOS caps the soft limit at `OPEN_MAX`.
  #[cfg(target_os = "macos")]
  fn platform_cap(maximum: u64) -> u64 {
    maximum.min(MACOS_OPEN_MAX)
  }

  #[cfg(all(unix, not(target_os = "macos")))]
  fn platform_cap(maximum: u64) -> u64 {
    maximum
  }

  /// Windows has no per-process handle limit to raise.
  #[cfg(not(unix))]
  pub(super) fn raise_descriptor_limit() -> Option<(u64, u64)> {
    None
  }
}

#[cfg(test)]
pub(crate) use tests::{audit_on_shard, audit_on_shard_configured};

#[cfg(test)]
mod tests {
  use super::registered_chokepoints;
  use slates_wire::observe::Chokepoint;

  /// Runs an audit history on the daemon's real owning shard (§4.8, §4.16). The daemon owns and
  /// joins every task; only the result crosses back to the test thread.
  pub(crate) fn audit_on_shard<T: Send + 'static>(
    history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
  ) -> T {
    audit_on_shard_configured(history, |_| {})
  }

  /// A bounded audit fixture may reduce a geometry before mapping it, so a capacity test
  /// fills a few pages rather than the machine's measured production-sized reserve.
  pub(crate) fn audit_on_shard_configured<T: Send + 'static>(
    history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
    configure: impl FnOnce(&mut crate::DaemonConfig),
  ) -> T {
    let profile = slates_machine::MachineProfile::measure(slates_machine::ProfileOptions {
      budget_per_probe: std::time::Duration::from_millis(5),
      codecs: false,
      core_matrix: false,
    });
    // One instance name per fixture call, not per process: the name is the daemon's segment and
    // rendezvous object, and libtest runs these tests in parallel — with only the pid in the name,
    // two concurrent audits created the same segment and the second `Daemon::start` failed
    // (twelve tests of a `--lib` run, all at the `expect` below, all passing serially, 2026-09-15).
    static NEXT_AUDIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let instance = format!(
      "audit-{}-{}",
      std::process::id(),
      NEXT_AUDIT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut config = crate::DaemonConfig::derive(&profile, &instance).with_shards(1);
    configure(&mut config);
    let daemon = super::Daemon::start(
      &profile,
      config,
      super::SegmentSource::Create { name: instance },
    )
    .expect("the audit daemon starts");
    daemon
      .bootstrap(true)
      .expect("the audit explicitly creates its initial group");
    let result = daemon.observe(daemon.shards.first().copied(), history);
    daemon.stop();
    result.expect("the audit history completed")
  }

  /// AC-8.1, §4.8 restart-as-join; AUD-07: vote for B, lose the whole anchor, then receive C's
  /// vote request in the same term. The two boots must not supply two votes under one voter id.
  #[test]
  fn a_ram_losing_boot_cannot_cast_its_predecessors_second_vote() {
    use slates_cluster::config_group::RegionalCouncil;
    use slates_cluster::raft::RequestVote;
    use slates_cluster::raft_wire::RaftMessage;
    use slates_db::register::{HostId, Quorum};
    let vote = |candidate: HostId, voters: Option<Vec<HostId>>| {
      audit_on_shard(move |state| {
        let local = state.fleet.host();
        let voters = voters.unwrap_or_else(|| vec![local, HostId(1), HostId(2)]);
        let mut council = RegionalCouncil::new(
          local,
          voters.clone(),
          voters.clone(),
          Quorum { f: 1 },
          Default::default(),
          voters.len() as u64,
          false,
        );
        let reply = council
          .answer(RaftMessage::RequestVote(RequestVote {
            term: 7,
            candidate,
            last_log_index: 0,
            last_log_term: 0,
          }))
          .expect("a vote request has a reply");
        let RaftMessage::VoteReply(reply) = reply else {
          panic!("{reply:?}");
        };
        (voters, reply)
      })
    };
    let (voters, before_loss) = vote(HostId(1), None);
    assert!(
      before_loss.granted,
      "the predecessor supplied the first vote"
    );
    let (_, after_loss) = vote(HostId(2), Some(voters));
    assert!(
      !after_loss.granted || after_loss.voter != before_loss.voter,
      "two candidates received the same voter's grant in term {}: {:?} then {:?}",
      before_loss.term,
      before_loss,
      after_loss,
    );
  }

  /// AC-8.1 / T-2.14, §4.8: acknowledge a vote in both groups, restart over retained anchor
  /// RAM, then ask for a different candidate in the same term. Both groups must refuse.
  #[test]
  fn a_warm_restart_preserves_both_groups_votes_before_their_replies_escape() {
    use slates_anchor::AnchorSegment;
    use slates_cluster::config_group::RegionalCouncil;
    use slates_cluster::raft::RequestVote;
    use slates_cluster::raft_wire::{
      RaftMessage, encode_regional_configuration, encode_root_configuration,
    };
    use slates_cluster::root_group::RootGroup;
    use slates_db::register::{HostId, Quorum, RegionId};
    let profile = slates_machine::MachineProfile::measure(slates_machine::ProfileOptions {
      budget_per_probe: std::time::Duration::from_millis(5),
      codecs: false,
      core_matrix: false,
    });
    let instance = format!("warm-votes-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance).with_shards(1);
    let segment =
      AnchorSegment::create(&instance, &profile.facts.identity, config.geometry).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: None,
      }
    };
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    let request = |candidate| {
      RaftMessage::RequestVote(RequestVote {
        term: 7,
        candidate,
        last_log_index: 0,
        last_log_term: 0,
      })
    };
    let before = first
      .observe(first.shards.first().copied(), move |state| {
        let local = state.fleet.host();
        let voters = vec![local, HostId(1), HostId(2)];
        state.council = RegionalCouncil::new(
          local,
          voters.clone(),
          voters.clone(),
          Quorum { f: 1 },
          Default::default(),
          3,
          false,
        );
        state.root = RootGroup::new(local, vec![RegionId(0)], voters);
        let (raft, base) = state.council.join_state().unwrap();
        state.council_group = Some(crate::consensus::genesis(
          false,
          &raft,
          &encode_regional_configuration(&base),
        ));
        let (raft, base) = state.root.join_state().unwrap();
        state.root_group = Some(crate::consensus::genesis(
          true,
          &raft,
          &encode_root_configuration(&base),
        ));
        [
          state.council.answer(request(HostId(1))).unwrap(),
          state.root.answer(request(HostId(1))).unwrap(),
        ]
      })
      .unwrap();
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    let after = second
      .observe(second.shards.first().copied(), move |state| {
        [
          state.council.answer(request(HostId(2))).unwrap(),
          state.root.answer(request(HostId(2))).unwrap(),
        ]
      })
      .unwrap();
    second.stop();
    for (before, after) in before.into_iter().zip(after) {
      let (RaftMessage::VoteReply(before), RaftMessage::VoteReply(after)) = (before, after) else {
        panic!("both messages must be vote replies");
      };
      assert!(
        before.granted,
        "the first candidate received this voter's grant"
      );
      assert_eq!(
        before.voter, after.voter,
        "the retained identity was reused"
      );
      assert_eq!(before.term, after.term);
      assert!(
        !after.granted,
        "restarting cannot grant a second vote in the same term"
      );
    }
  }

  /// AC-8.1, §4.8, AUD-07: a separately bootstrapped group cannot alter this group's
  /// term or prefix, even when its packet names the authenticated sender correctly.
  #[test]
  fn an_independent_group_cannot_supply_consensus_messages_or_join_state() {
    use slates_cluster::raft::RequestVote;
    use slates_cluster::raft_wire::{RaftMessage, RaftWireError};
    use slates_wire::Wire;
    let (peer, messages, fetches) = audit_on_shard(|state| {
      let peer = state.fleet.host();
      let request = RaftMessage::RequestVote(RequestVote {
        term: u64::MAX,
        candidate: peer,
        last_log_index: 0,
        last_log_term: 0,
      });
      let fetch = crate::consensus::Fetch {
        group: None,
        version: 0,
      }
      .to_bytes();
      let messages =
        [false, true].map(|root| crate::consensus::encode_message(state, root, &request).unwrap());
      let fetches =
        [false, true].map(|root| crate::consensus::serve_fetch(state, root, &fetch).unwrap());
      (peer, messages, fetches)
    });
    audit_on_shard(move |state| {
      for (root, (message, fetch)) in [false, true]
        .into_iter()
        .zip(messages.into_iter().zip(fetches))
      {
        assert_eq!(
          crate::consensus::decode_message(state, root, peer, &message),
          Err(RaftWireError::ForeignGroup)
        );
        assert!(!crate::consensus::adopt_fetch(state, root, peer, &fetch));
      }
      assert!(state.council.is_leader());
      assert!(state.root.is_leader());
    });
  }

  /// AC-2.5, §4.8 authority; AUD-10: accept the owner's first record, then submit another position
  /// from a different authenticated peer claiming that owner. Expect refusal without fencing the
  /// legitimate owner's next record or changing the object's routing owner.
  #[test]
  fn a_holder_binds_every_record_to_the_authenticated_peer() {
    use slates_db::register::{Ack, HostEpoch, HostId, ObjectId, Record};
    let (spoofed, legitimate, owner, routed) = audit_on_shard(|state| {
      let owner = state.fleet.host();
      let record = Record {
        owner,
        object: ObjectId([17; 16]),
        sequence: 0,
        epoch: HostEpoch(1),
        generation: state.fleet.configuration().version,
        value: b"first".to_vec(),
      };
      let first = crate::fleet::accept_held_record(state, owner, owner, &record);
      assert!(Ack::decode(&first).is_ok());
      let next = Record {
        sequence: 1,
        value: b"next".to_vec(),
        ..record
      };
      let spoof = Record {
        epoch: HostEpoch(2),
        ..next.clone()
      };
      let spoofed = crate::fleet::accept_held_record(state, owner, HostId(owner.0 ^ 1), &spoof);
      let legitimate = crate::fleet::accept_held_record(state, owner, owner, &next);
      (
        spoofed,
        legitimate,
        owner,
        state.fleet.object_owner(next.object),
      )
    });
    assert!(
      spoofed.is_empty(),
      "a different TLS peer cannot speak as the owner"
    );
    assert!(
      Ack::decode(&legitimate).is_ok(),
      "the forgery did not raise the fence"
    );
    assert_eq!(routed, Some(owner));
  }

  /// AC-6.3, §4.16 holder recomputation; AUD-12: submit an otherwise valid origin record through
  /// an unauthorized peer. Expect no acknowledgement and no readable replica created by that record.
  #[test]
  fn an_unauthorized_merge_record_never_publishes_a_replica() {
    use slates_db::register::{HostEpoch, HostId, ObjectId, Record};
    let (reply, held) = audit_on_shard(|state| {
      let owner = state.fleet.host();
      let value = crate::merge_service::MergeRecordValue {
        version: 0,
        increment: [0; 32],
        base: 0,
        inputs: None,
        identity: slates_merge::engine::Green::new().head_identity(),
        evidence: Vec::new(),
      };
      let record = Record {
        owner,
        object: ObjectId([18; 16]),
        sequence: 0,
        epoch: HostEpoch(1),
        generation: state.fleet.configuration().version,
        value: value.to_record_bytes(),
      };
      let reply =
        crate::merge_service::accept_merge_record(state, owner, HostId(owner.0 ^ 1), &record);
      (
        reply,
        crate::merge_service::holder_state(state, record.object),
      )
    });
    assert!(reply.is_empty());
    assert_eq!(
      held.version, None,
      "a refused record must not create a readable replica"
    );
    assert!(
      !held.refused,
      "unauthorized traffic must not poison the green"
    );
  }

  /// AC-2.5, §4.8 takeover; AUD-10 sibling: a forged prepare must not raise the promise. The
  /// configuration-authorized successor can still prepare and commit after the refused forgery.
  #[test]
  fn a_takeover_prepare_binds_its_owner_to_the_authenticated_peer() {
    use slates_db::register::{
      Acceptor, Ack, Authority, HostEpoch, HostId, ObjectId, Prepare, Promise, Record,
    };
    let (spoofed, promised, committed) = audit_on_shard(|state| {
      // The sole surviving configured member takes over a departed peer's record. Its authority
      // and its placement both name a real member; the forged transport principal is the old owner.
      let successor = state.fleet.host();
      let owner = HostId(successor.0 ^ 1);
      let object = ObjectId([19; 16]);
      let generation = state.fleet.configuration().version;
      let mut acceptor = Acceptor::new(successor, Authority { generation, owner });
      acceptor
        .install_authority(Authority {
          generation,
          owner: successor,
        })
        .unwrap();
      state.holder_records.insert(object, acceptor);
      let prepare = Prepare {
        owner: successor,
        object,
        epoch: HostEpoch(2),
        generation,
      };
      let forgery = Prepare {
        epoch: HostEpoch(3),
        ..prepare
      };
      let spoofed = crate::fleet::serve_held_promotion(state, owner, &forgery);
      let promised = crate::fleet::serve_held_promotion(state, successor, &prepare);
      let record = Record {
        owner: successor,
        object,
        sequence: 0,
        epoch: prepare.epoch,
        generation,
        value: Vec::new(),
      };
      let committed = crate::fleet::accept_held_record(state, successor, successor, &record);
      (spoofed, promised, committed)
    });
    assert!(spoofed.is_empty());
    assert!(Promise::decode(&promised).is_ok());
    assert!(Ack::decode(&committed).is_ok());
  }

  /// AC-6.3, §4.16; AUD-12: a stale epoch, foreign generation or conflicting accepted position
  /// must refuse before holder recomputation. A later legitimate origin must still seed and serve.
  #[test]
  fn refused_merge_authority_leaves_the_replica_unchanged() {
    use slates_db::register::{Acceptor, Ack, Authority, HostEpoch, ObjectId, Record};
    let (after_refusals, committed, after_commit) = audit_on_shard(|state| {
      let owner = state.fleet.host();
      let object = ObjectId([20; 16]);
      let generation = state.fleet.configuration().version;
      let value = crate::merge_service::MergeRecordValue {
        version: 0,
        increment: [0; 32],
        base: 0,
        inputs: None,
        identity: slates_merge::engine::Green::new().head_identity(),
        evidence: Vec::new(),
      };
      let record = Record {
        owner,
        object,
        sequence: 0,
        epoch: HostEpoch(2),
        generation,
        value: value.to_record_bytes(),
      };
      let mut acceptor = Acceptor::new(owner, Authority { generation, owner });
      acceptor.raise_fence(HostEpoch(2));
      state.holder_records.insert(object, acceptor);
      for refused in [
        Record {
          epoch: HostEpoch(1),
          ..record.clone()
        },
        Record {
          generation: generation + 1,
          ..record.clone()
        },
      ] {
        let reply = crate::merge_service::accept_merge_record(state, owner, owner, &refused);
        assert!(Ack::decode(&reply).is_err());
      }
      let after_refusals = crate::merge_service::holder_state(state, object);
      let committed = crate::merge_service::accept_merge_record(state, owner, owner, &record);
      let after_commit = crate::merge_service::holder_state(state, object);
      (after_refusals, committed, after_commit)
    });
    assert_eq!(after_refusals.version, None);
    assert!(!after_refusals.refused);
    assert!(Ack::decode(&committed).is_ok());
    assert_eq!(after_commit.version, Some(0));
  }

  /// Shape: a runtime for the admission guard's test — one shard, a few task slots.
  fn guard_runtime() -> slates_rt::runtime::LocalRuntime {
    slates_rt::runtime::LocalRuntime::new(&slates_rt::runtime::RuntimeConfig {
      shards: 1,
      tasks_per_shard: 8,
      timers_per_shard: 8,
      ring_entries: 8,
      step_budget_ns: 1_000_000,
      timer_tick_ns: 100_000,
      batch: 8,
      pin: false,
      cores: Vec::new(),
      page_bytes: 4096,
      spin_ns: 0,
    })
    .expect("a local runtime")
  }

  /// AC-2.6 (a refused admission never leaks an id): the seat task moved onto a shard and dropped
  /// **unrun** — what a full task arena does to it — gives the client's id back to the live set; a task
  /// that seats its client keeps the id. Do: on a shard, reserve two ids; build two guards; drop one
  /// unseated and the other after `seat`. Expect: the unseated one's id is gone from the live set, the
  /// seated one's stays, and the lost-handoff count moved by exactly one. On the guard's own shard the
  /// release is direct; from another shard it is a forget task, exercised by the daemon tests' multi-
  /// shard admissions.
  #[test]
  fn an_admission_dropped_unseated_gives_its_id_back() {
    let runtime = guard_runtime();
    let shard = runtime.shard_id().0;
    let (tx, rx) = std::sync::mpsc::channel();
    runtime
      .spawn(async move {
        crate::state::with_handed(|handed| {
          handed.insert(41);
          handed.insert(42);
        });
        let lost_before = super::HANDOFF_LOST.load(std::sync::atomic::Ordering::Acquire);
        let unseated = super::Admission {
          client_id: 41,
          control: shard,
          seated: false,
        };
        let mut seated = super::Admission {
          client_id: 42,
          control: shard,
          seated: false,
        };
        seated.seat();
        drop(unseated);
        drop(seated);
        let live = crate::state::with_handed(|handed| handed.clone());
        let lost = super::HANDOFF_LOST.load(std::sync::atomic::Ordering::Acquire) - lost_before;
        let _ = tx.send((live, lost));
      })
      .expect("the task is admitted");
    runtime.run_until_idle();
    let (live, lost) = rx.try_recv().expect("the task reported");
    assert!(
      !live.contains(&41),
      "the unseated admission's id was given back: {live:?}"
    );
    assert!(
      live.contains(&42),
      "the seated admission's id stays live: {live:?}"
    );
    assert_eq!(lost, 1, "one lost handoff counted");
  }

  /// The daemon declares every chokepoint span, so the observability gate opens and it serves (§2.6,
  /// §4.14). This is the daemon side of the roster doc-truth: if a `register` line were dropped from
  /// [`registered_chokepoints`], the gate would name the missing chokepoint and `Daemon::start` would
  /// refuse — this test catches that omission without spawning a daemon. The live daemon tests
  /// (`crates/server/tests`) are the by-use proof that an open gate actually serves.
  #[test]
  fn the_daemon_declares_every_chokepoint_so_the_gate_opens() {
    let registry = registered_chokepoints();
    assert!(
      registry.is_ready(),
      "the daemon must register every chokepoint to serve; missing: {:?}",
      registry
        .missing()
        .into_iter()
        .map(Chokepoint::name)
        .collect::<Vec<_>>()
    );
  }

  /// A configuration change that breaches the operator's durability bound moves the breach health signal
  /// (§4.8 "the copyset count check at every configuration change") — the non-vacuity counter proving the
  /// check runs at install; a change with no policy, or one within the bound, does not move it.
  #[test]
  fn a_breaching_configuration_moves_the_durability_signal() {
    use super::{DURABILITY_BREACHES, record_durability};
    use crate::config::DurabilityBound;
    use slates_db::register::{HostId, Quorum, RegionalConfiguration};
    use std::sync::atomic::Ordering;

    let configuration = RegionalConfiguration::formed(
      vec![HostId(1), HostId(2), HostId(3)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    )
    .configuration_for(HostId(1))
    .expect("the owner has a placement view");

    // No policy, and a policy that accepts any loss, both leave the signal unmoved and measure no shortfall.
    let quiet = DURABILITY_BREACHES.load(Ordering::Relaxed);
    assert_eq!(record_durability(&configuration, None), None);
    assert_eq!(
      record_durability(
        &configuration,
        Some(DurabilityBound {
          accepted_loss: 1.0,
          coincident_failures: 2,
        }),
      ),
      None
    );
    assert_eq!(
      DURABILITY_BREACHES.load(Ordering::Relaxed),
      quiet,
      "no breach without a policy or within the accepted loss"
    );

    // A zero-loss policy is breached by the redundant configuration, moving the signal — and the measured
    // shortfall comes back, the fact the write path refuses with.
    let shortfall = record_durability(
      &configuration,
      Some(DurabilityBound {
        accepted_loss: 0.0,
        coincident_failures: 2,
      }),
    );
    assert_eq!(
      DURABILITY_BREACHES.load(Ordering::Relaxed),
      quiet + 1,
      "a breach at a configuration change moves the health signal"
    );
    let shortfall = shortfall.expect("a breach measures its shortfall");
    assert!(
      shortfall.coincident_loss > 0.0
        && shortfall.accepted_loss == 0.0
        && shortfall.coincident_failures == 2,
      "the shortfall carries the measured loss, the accepted ε and the failure count: {shortfall:?}"
    );
  }
}
