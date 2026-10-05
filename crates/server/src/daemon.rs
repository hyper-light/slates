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
use crate::doorbell::DoorbellThread;
use crate::error::ServerError;
use crate::observe::{Admitted, Observation, ObserveError, ObserveStage};
use crate::state::{self, ClientSlot, ShardState, StateAccess};
use crate::verbs;

/// A perpetual loop's pace on its shard: waits `ns`. The sleep is refused only off a shard, where no server
/// loop runs; if it ever is, the loop stops here — counted (`runtime.sleep_refused`) and said once in the
/// log — rather than spinning or reading the refusal as elapsed time (AUD-29-39). The task stays owned and
/// ends with its shard.
pub(crate) async fn pace(ns: u64) {
  if let Err(refusal) = slates_rt::futures::sleep(ns).await {
    if crate::fleet::count_refusal(SLEEP_REFUSED) == 1 {
      eprintln!("slates-server: a periodic loop's sleep was refused ({refusal}); the loop stops");
    }
    std::future::pending::<()>().await;
  }
}

/// A perpetual loop's sleep refused ([`pace`]): a tripwire, never an expected count.
const SLEEP_REFUSED: &str = "runtime.sleep_refused";

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

/// Shape: how long [`Daemon::hold_record_session`] waits for a record session that is not in its link — one
/// liveness budget. A live peer's session is out only for a dispatch, a discovery page or a re-dial, each far
/// shorter; one still absent after a liveness budget names a real absence, which the hold reports rather than
/// waits out. Derived: [`LIVENESS_BUDGET_NS`], a tenth of [`OBSERVE_BUDGET_NS`], so the report arrives
/// inside the observation's budget.
pub const SESSION_WAIT_NS: u64 = LIVENESS_BUDGET_NS;

/// What [`Daemon::hold_record_session`] found: the session taken (and how long it waited for it), or, after
/// [`SESSION_WAIT_NS`], why there was none — a link for the peer whose session never came back to it, or no
/// link for the peer at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHold {
  /// The session was taken after `waited_ns`, held for the span and put back.
  Took {
    /// How long the hold waited for the session to be in its link.
    waited_ns: u64,
  },
  /// The link for the peer existed, but its session was out for the whole wait.
  NeverInItsLink {
    /// How long the hold waited.
    waited_ns: u64,
  },
  /// This node kept no link to the peer for the whole wait.
  NoLink {
    /// How long the hold waited.
    waited_ns: u64,
  },
}

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

/// What a leadership drain ([`Daemon::drain_leadership`]) did for one consensus group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadershipHandoff {
  /// This daemon did not lead the group; nothing to hand off.
  NotLeading,
  /// Leadership moved: this daemon no longer leads, having invited `to`.
  HandedOff {
    /// The voter invited to take over.
    to: slates_db::HostId,
  },
  /// This daemon still leads at the drain's deadline (the transfer was aborted or never completed).
  StillLeading,
  /// This daemon stepped down but saw no successor take office by the deadline (the group is mid-election).
  SteppedDown,
  /// This daemon leads a group with no other voter (the laptop's degenerate): there is nobody to hand to.
  NoTarget,
  /// The core refused the transfer (typed).
  Refused(slates_cluster::raft::TransferRefusal),
}

/// What [`Daemon::drain_leadership`] did: each group's handoff and how long the drain waited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainReport {
  /// The regional council.
  pub council: LeadershipHandoff,
  /// The root group.
  pub root: LeadershipHandoff,
  /// From the drain's start to the moment it returned.
  pub took: std::time::Duration,
}

/// One shard's publication counters (`Daemon::db_publication_counters`; AUD-06): how its database has
/// fared making transactions durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationCounters {
  /// The shard.
  pub shard: u16,
  /// Transactions rolled back because their record could not be made durable — the effects and the
  /// completion record gone together, the verb refused `Unpublished`.
  pub rollbacks: u64,
  /// Maintenance snapshots that failed after a durable append and were deferred to a later commit.
  pub maintenance_failures: u64,
  /// Snapshots the database has published since recovery.
  pub snapshots_taken: u64,
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
  /// The shard's online wake estimate, nanoseconds — what a wake costs it now, refined from the kicked
  /// parks it paid (§4.3), which its step quantum and idle windows follow; 0 until its first measured
  /// wake.
  pub wake_cost_ns: u64,
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
pub(crate) const NFS_BACKLOG: i32 = 16;

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
/// release path for a reaped client (`reap::finish`) and a lost admission ([`Admission`]).
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
/// Format: the status refusal under which a shard counts a volume it could not image for a publish (§4.8): the
/// rest of the shard still publishes, the volume gets no stable acknowledgement, and recovery refuses it. Per
/// shard, so each daemon's status and `refusals_on_every_shard` show it (until 2026-10-01 a process-wide
/// static counted it, read by nothing and logged once per process without naming the volume).
pub const PUBLISH_VOLUME_SKIPPED: &str = "publish.volume_skipped";
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
/// What a node holds for other owners and what it is charged for it ([`Daemon::fleet_replica_account`];
/// AUD-29-43).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaAccount {
  /// The shard byte budget's charge for replicated content.
  pub replicated: u64,
  /// The bytes the hold says it is charged; equal to `replicated`.
  pub charged: u64,
  /// The bytes the hold's index is charged on the metadata ledger.
  pub index: u64,
  /// The manifests held.
  pub manifests: usize,
  /// The puts refused because the shard could not admit them.
  pub refused_capacity: u64,
  /// The transfers in progress (stages, AUD-29-55).
  pub stages: usize,
  /// The bytes the shard's budget may still admit.
  pub admittable: u64,
  /// The capacity the memory-pressure hold withholds from admission now (§4.2).
  pub hold: u64,
}

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
          .with_content(&content_name, config.content_bytes())?
      }
      SegmentSource::FromEnv => AnchorSegment::attach_from_env(&identity)?,
      SegmentSource::Handoff {
        handoff,
        len,
        content,
      } => {
        let mut segment = AnchorSegment::attach(&handoff, len, &identity)?;
        if let Some((content_handoff, content_len)) = content {
          let object =
            slates_mem::SparseObject::open(&content_handoff, content_len, slates_mem::Words::new())
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
    // The node's sealing root (A-92): adopted from the previous daemon under this anchor, or made and published into
    // the anchor's locked page; sealing reports itself unavailable, never unlocked, where the OS refuses the lock.
    let sealing = crate::seal_keys::init(&mut segment, config.seal_key_slots);
    if let Some((was, now)) = limits::raise_descriptor_limit() {
      eprintln!("slates-server: descriptor limit raised from {was} to {now}");
    }
    let retained = crate::retention::load(&segment)?;
    let runtime = Runtime::start(&config.runtime)?;
    let shards: Vec<ShardId> = runtime.shard_ids().to_vec();
    // The FUSE devices the anchor held across the restart (A-61), each moved to the shard that owns its mount.
    #[cfg(target_os = "linux")]
    let mut inherited_fuse = crate::fuse_hold::adopt(shards.len()).into_iter();
    for (index, shard) in shards.iter().enumerate() {
      let env = segment.handoff_env()?;
      let config = config.clone();
      let identity = identity.clone();
      let partition = u16::try_from(index).unwrap_or(u16::MAX);
      let all: Vec<u16> = shards.iter().map(|s| s.0).collect();
      let retained = retained.clone();
      #[cfg(target_os = "linux")]
      let held = inherited_fuse.next().unwrap_or_default();
      runtime.spawn_on(*shard, async move {
        if let Err(e) = init_shard(
          &config,
          &env,
          &identity,
          (partition, sealing),
          &all,
          retained,
          #[cfg(target_os = "linux")]
          held,
        ) {
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
    let doorbell = match (listener.doorbell_waiter()?, listener.doorbell_waiter()?) {
      (Some(waiter), Some(waker)) => Some(DoorbellThread::start(
        waiter,
        waker,
        kicks,
        doorbell_flag(shards.first().map_or(0, |shard| shard.0)),
      )?),
      _ => None,
    };
    let control_config = config.clone();
    let control_env = segment.handoff_env()?;
    let control_identity = identity.clone();
    let control = shards.first().copied().ok_or(ServerError::NotOnShard)?;
    let shard_ids = shards.clone();
    runtime.spawn_on(control, async move {
      let task = futures::spawn(control_loop(
        listener,
        control_config,
        control_env,
        control_identity,
        shard_ids,
      ));
      // Construct the control future on its owning shard: recovery owns thread-local
      // cross-shard reply registrations, which must never migrate between threads.
      if task.and_then(futures::detach).is_err() {
        INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
        eprintln!("slates-server: the control loop could not be seated");
      }
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
      doorbell,
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

  /// Test support: makes this node, as a holder, refuse every register record from `owners` — no
  /// acknowledgement, counted `fleet.record.refused_by_fault` — as a holder that never received them would,
  /// while the owners' council and report traffic on the same sessions still flows; so a test keeps a head off
  /// one candidate without touching consensus. Replaces any earlier set; an empty `owners` restores full
  /// service. `Ok` once installed, else the typed refusal.
  pub fn inject_record_refusal(&self, owners: &[slates_db::HostId]) -> Result<(), ObserveError> {
    let owners: std::collections::BTreeSet<slates_db::HostId> = owners.iter().copied().collect();
    self.observe(self.shards.first().copied(), move |s| {
      s.record_refused_from = owners;
    })
  }

  /// Test support: the version at which the council's configuration fixed `member`'s settled neighbourhood
  /// and the version its current neighbourhood was fixed at (§4.8 "Neighbourhood changes"): equal once the
  /// member has reported its change placed and the council settled it; `None` for a non-member. A one-shot
  /// control-shard question; the typed refusal when the shard could not answer.
  pub fn council_settlement(
    &self,
    member: slates_db::HostId,
  ) -> Result<Option<(u64, u64)>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let regional = s.council.configuration();
      let settled = regional.settled.get(&member)?.generation;
      let current = regional.neighbourhoods.get(&member)?.generation;
      Some((settled, current))
    })
  }

  /// Test support: the regional configuration version this daemon's placement has installed and the one its
  /// council holds (§4.8): equal once the coordinator has installed what the council committed. A test reads
  /// them to show which configurations its daemons stood at when an answer depended on it. A one-shot
  /// control-shard question; the typed refusal when the shard could not answer.
  pub fn configuration_versions(&self) -> Result<(u64, u64), ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      (
        s.fleet.configuration().version,
        s.council.configuration().version,
      )
    })
  }

  /// Test support: holds this daemon's record session to `peer` out of its link for `span_ns` and then puts
  /// it back, exactly as a coordinator dispatch or a discovery page holds it (`fleet::take_sessions`,
  /// `fleet::return_sessions`), so a test drives a forward into a session that is out (§4.8 "Lookup";
  /// docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md). The hold is a task on the
  /// control shard, where the record sessions live, and it sleeps on the shard's timer, so the shard keeps
  /// serving meanwhile. A session that is not in its link when the hold starts (out on a dispatch or a
  /// discovery page, or being established by its link task after a membership change) is waited for, paced
  /// at the fleet's poll interval, for at most [`SESSION_WAIT_NS`], as a forward waits for it
  /// (`fleet::forward_over_leader_session`); taking only what is there at the first look made the hold a race
  /// against those (CI run 36275755772, docs/bugs/2026-09-26-a-session-hold-raced-its-own-link.md). Returns
  /// once the hold has run its take, with what it found ([`SessionHold`]); the typed refusal when the control
  /// shard could not take the hold within the observe budget.
  pub fn hold_record_session(
    &self,
    peer: slates_db::HostId,
    span_ns: u64,
  ) -> Result<SessionHold, ObserveError> {
    let shard = self.shards.first().copied().ok_or(ObserveError::NoTarget)?;
    let runtime = self.runtime.as_ref().ok_or(ObserveError::NoRuntime)?;
    let (taken, took) = std::sync::mpsc::sync_channel::<SessionHold>(1);
    let receipt = runtime
      .spawn_on_with_receipt(shard, async move {
        let began = slates_rt::futures::now_ns();
        let (sessions, outcome) = loop {
          let waited_ns = slates_rt::futures::now_ns().saturating_sub(began);
          let sessions = crate::fleet::take_sessions(|host| host == peer);
          if !sessions.is_empty() {
            break (sessions, SessionHold::Took { waited_ns });
          }
          if waited_ns >= SESSION_WAIT_NS {
            let linked =
              crate::state::with_state(|s| s.record_sessions.contains_key(&peer)).unwrap_or(false);
            let outcome = if linked {
              SessionHold::NeverInItsLink { waited_ns }
            } else {
              SessionHold::NoLink { waited_ns }
            };
            break (sessions, outcome);
          }
          pace(HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD).await;
        };
        let _ = taken.send(outcome);
        if !sessions.is_empty() {
          pace(span_ns).await;
          crate::fleet::return_sessions(sessions);
        }
      })
      .map_err(|refusal| ObserveError::Submission {
        refusal,
        attempts: 1,
        waited_ns: 0,
      })?;
    let budget = std::time::Duration::from_nanos(OBSERVE_BUDGET_NS);
    match receipt.wait(budget) {
      Some(slates_rt::Admission::Admitted(_)) => {}
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
    took
      .recv_timeout(budget)
      .map_err(|_| ObserveError::Deadline {
        stage: ObserveStage::Execution,
        budget_ns: OBSERVE_BUDGET_NS,
        attempts: 1,
        waited_ns: OBSERVE_BUDGET_NS,
        last_refusal: None,
      })
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
          wake_cost_ns: entry.pulse.wake_cost_ns(),
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

  /// The membership plane's counts (A-67 H-2): what its sealed plane refused, and its indirect stage's relay requests,
  /// relayed probes and credited indirect answers. `Default` before the plane started.
  pub fn fleet_plane_counts(
    &self,
  ) -> Result<slates_cluster::member_plane::PlaneCounts, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.plane
        .plane
        .as_ref()
        .map(slates_cluster::member_plane::MemberPlane::counts)
        .unwrap_or_default()
    })
  }

  /// The membership plane's detector's belief about `host` (A-67 H-2): `Suspect` where its probes went unanswered,
  /// before (or, in a two-member view, instead of) a condemnation.
  pub fn fleet_plane_belief(
    &self,
    host: slates_db::HostId,
  ) -> Result<Option<slates_cluster::membership::MemberState>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.plane.plane.as_ref().and_then(|plane| plane.belief(host))
    })
  }

  /// What this node's fleet membership holds about `host` (its liveness and the incarnation it was asserted under), or
  /// `None` for a member it has never seen: what a rejoin test reads to tell a refutation (a higher incarnation, which
  /// only the member itself asserts) from a stale re-admission.
  pub fn fleet_member_state(
    &self,
    host: slates_db::HostId,
  ) -> Result<Option<slates_cluster::membership::MemberState>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.fleet.membership().state(host)
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

  /// The configured members this node keeps direct contact with but has no formed probe session to — what
  /// keeps [`Self::fleet_meshed`] false — with the members it holds, for a failure's diagnosis.
  pub fn fleet_unmeshed(
    &self,
  ) -> Result<(Vec<slates_db::HostId>, Vec<slates_db::HostId>), ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      let host = s.fleet.host();
      let members: Vec<slates_db::HostId> = s.fleet.members().to_vec();
      let missing = members
        .iter()
        .copied()
        .filter(|&peer| peer != host && crate::fleet::keeps_direct_contact_with(s, peer))
        .filter(|peer| !s.formed_probe_peers.contains(peer))
        .collect();
      (missing, members)
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

  /// Hands this daemon's council leadership to the voter `target` (thesis §3.10, leadership transfer;
  /// `docs/wip/research/consensus-enhancements.md` §3.2): the council leader stops accepting proposals, brings
  /// `target` up to date, and invites it to campaign at once, so leadership moves without the election timeout a
  /// leader loss costs. The outer result is the observation (a control shard that could not be asked is its
  /// typed refusal); the inner is the core's typed refusal — this daemon does not lead, `target` is itself or
  /// not a voter, or a transfer is already in flight.
  pub fn transfer_council_leadership(
    &self,
    target: slates_db::HostId,
  ) -> Result<Result<(), slates_cluster::raft::TransferRefusal>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.council.transfer_leadership(target)
    })
  }

  /// Whether this daemon's anchor has asked it to stop gracefully (the supervision block's stop request,
  /// `slates_anchor::layout::SUP_STOP`). The daemon's loop reads it each tick, drains
  /// ([`Self::drain_leadership`]) and stops.
  pub fn stop_requested(&self) -> bool {
    self
      .segment
      .supervision()
      .ok()
      .and_then(|supervision| supervision.stop_requested_at())
      .is_some()
  }

  /// Hands off every consensus leadership this daemon holds before it stops (thesis §3.10;
  /// `docs/wip/research/consensus-enhancements.md` §3.2), so a planned stop costs the group a handoff rather
  /// than the election timeout a leader loss waits out. For each group it leads it invites the most
  /// caught-up other voter, then waits — polling each heartbeat — until each successor is in office, or until
  /// the bound: the core aborts a transfer at its second CheckQuorum tick, at most two CheckQuorum intervals
  /// (`base_periods` each) of the slower group, so the drain waits no longer than that and one period more. The bound is declared to the anchor
  /// first (`SUP_STOP_BY`), which kills a daemon that outruns it. A daemon that leads nothing returns at
  /// once.
  pub fn drain_leadership(&self) -> Result<DrainReport, ObserveError> {
    let control = self.shards.first().copied();
    let started = std::time::Instant::now();
    let (council_timing, root_timing) =
      self.observe(control, |s| (s.council_timing, s.root_timing))?;
    // Derived: the core aborts a transfer at its second CheckQuorum tick, and a leader ticks CheckQuorum
    // every `base_periods` (`ElectionTimer::leader_period`), so two base intervals of the slower group bound
    // the handoff, and one period more covers the successor's first append reaching this daemon. (Counting
    // the jitter span too would have declared about 32 s on the 350 ms-one-way KIND profile — past an
    // orchestrator's default 30 s grace — for a bound of about 16 s.)
    let base_periods = u64::from(council_timing.base_periods.max(root_timing.base_periods));
    let bound_ns = HEARTBEAT_NS.saturating_mul(base_periods.saturating_mul(2).saturating_add(1));
    if let Ok(supervision) = self.segment.supervision() {
      let now = slates_vfs::clock::Clock::monotonic_ns(&mut HostClock::new());
      supervision.declare_stop_by(now.saturating_add(bound_ns));
    }
    let (council, root) = self.observe(control, |s| {
      (
        start_handoff(
          s.council.is_leader(),
          s.council.most_caught_up_voter(),
          |target| s.council.transfer_leadership(target),
        ),
        start_handoff(
          s.root.is_leader(),
          s.root.most_caught_up_voter(),
          |target| s.root.transfer_leadership(target),
        ),
      )
    })?;
    let bound = std::time::Duration::from_nanos(bound_ns);
    // Done when each started handoff's successor is in office: this daemon no longer leads **and** knows
    // another leader (it learns one from the successor's first append). Stepping down alone is not enough —
    // it happens on the target's vote request, before the target has won.
    loop {
      let (council_office, root_office) = self.observe(control, |s| {
        (
          Office::of(s.council.is_leader(), s.council.leader()),
          Office::of(s.root.is_leader(), s.root.leader()),
        )
      })?;
      let council_done = !matches!(council, LeadershipHandoff::HandedOff { .. })
        || council_office == Office::Succeeded;
      let root_done =
        !matches!(root, LeadershipHandoff::HandedOff { .. }) || root_office == Office::Succeeded;
      if (council_done && root_done) || started.elapsed() >= bound {
        return Ok(DrainReport {
          council: settle_handoff(council, council_office),
          root: settle_handoff(root, root_office),
          took: started.elapsed(),
        });
      }
      std::thread::park_timeout(std::time::Duration::from_nanos(HEARTBEAT_NS));
    }
  }

  /// Hands this daemon's **root group** leadership to the root voter `target` (thesis §3.10) — the
  /// cross-region counterpart of [`Self::transfer_council_leadership`], with the same two results.
  pub fn transfer_root_leadership(
    &self,
    target: slates_db::HostId,
  ) -> Result<Result<(), slates_cluster::raft::TransferRefusal>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.root.transfer_leadership(target)
    })
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

  /// How long this daemon took to serve each NFS call, from the call read to its reply built, summed over every
  /// shard (§4.14): calls served on the shard that read them, then calls forwarded to their volume's owner shard.
  /// What a caller measured beyond these is outside the serve: the kernel client, the socket and the wakes.
  #[cfg(unix)]
  pub fn nfs_service_times(&self) -> Result<crate::nfs::ServiceQuantiles, ObserveError> {
    let mut summed = crate::nfs::ServiceTimes::default();
    for shard in self.shards.iter().copied() {
      let shard_times = self.observe(Some(shard), |s| s.nfs_service.clone())?;
      summed.local.absorb(&shard_times.local);
      summed.forwarded.absorb(&shard_times.forwarded);
      summed.local_off_cpu.absorb(&shard_times.local_off_cpu);
    }
    Ok(crate::nfs::ServiceQuantiles {
      local: summed.local.quantiles(),
      forwarded: summed.forwarded.quantiles(),
      local_off_cpu: summed.local_off_cpu.quantiles(),
    })
  }

  /// The verbs parked on every shard until their owner lease confirms (`crate::lease_wait`; §4.8 "Leases and
  /// reads"): a test's view of a wait in progress.
  pub fn lease_waiters_parked(&self) -> Result<usize, ObserveError> {
    let mut parked = 0usize;
    for shard in self.shards.iter().copied() {
      parked = parked.saturating_add(self.observe(Some(shard), |s| crate::lease_wait::parked(s))?);
    }
    Ok(parked)
  }

  /// The refusals every shard of this daemon has counted, summed by kind (§4.14). A volume's refusals — a
  /// FUSE or virtio-fs barrier refused, a landing's stages — are counted on the volume's owner shard, which is
  /// the control shard only by chance, so a test asserting one is absent reads them here: through
  /// [`Self::fleet_refusals`] a refusal on another shard read as absent (2026-10-01: a refused FUSE barrier on
  /// shard 1 printed `{}`). Each shard is asked in turn; an observation the daemon could not make is its typed
  /// refusal.
  pub fn refusals_on_every_shard(
    &self,
  ) -> Result<std::collections::BTreeMap<&'static str, u64>, ObserveError> {
    let mut summed = std::collections::BTreeMap::new();
    for shard in self.shards.iter().copied() {
      for (kind, count) in self.observe(Some(shard), |s| s.refusals.clone())? {
        let total: &mut u64 = summed.entry(kind).or_insert(0);
        *total = total.saturating_add(count);
      }
    }
    Ok(summed)
  }

  /// The counters of this daemon's fleet demultiplexers — the probe plane's, then the record plane's
  /// (§4.14): sessions opened for a new source, sessions replaced by their peer's re-dial, handshakes
  /// refused for want of a session slot, and datagrams dropped. Empty for a laptop (no planes); an
  /// observation the daemon could not make is its typed refusal.
  pub fn fleet_demux_counters(
    &self,
  ) -> Result<Vec<slates_transport::demux::DemuxCounters>, ObserveError> {
    // A demultiplexer the observation cannot reach (its shard's context ended) is the state out of reach,
    // never a zeroed row.
    self
      .observe(self.shards.first().copied(), |s| {
        s.demuxes
          .iter()
          .map(|demux| demux.with(slates_transport::demux::Demux::counters))
          .collect::<Option<Vec<_>>>()
      })?
      .ok_or(ObserveError::State(StateAccess::Absent))
  }

  /// Live tasks in the control shard's arena (§4.14), the observation's own task among them. A test reads
  /// it before and after a burst of work to prove the burst's tasks ended — the accept-side serve tasks a
  /// peer's re-dials spawn, in particular, whose share of the arena is sized by `config::with_fleet`.
  /// An observation the daemon could not make is its typed refusal.
  pub fn live_tasks(&self) -> Result<usize, ObserveError> {
    self.observe_shard(self.shards.first().copied(), |shard| shard.live_tasks())
  }

  /// The idle spins each shard has entered since boot, in shard order (§4.7 "Shards poll rings while any
  /// client has activity within the measured idle window"): the spin a shard runs before it parks, which
  /// client activity opens and its window bounds (`spin_hits` + `spin_misses` + `spin_deadlines` of
  /// [`slates_rt::shard::Counters`]). Asked as a one-shot task on each shard, so the asking is no client
  /// activity. A test reads it across a quiet stretch to prove an idle daemon parks rather than spins; an
  /// observation the daemon could not make is its typed refusal.
  pub fn idle_spins(&self) -> Result<Vec<u64>, ObserveError> {
    self
      .shards
      .iter()
      .map(|shard| {
        self.observe_shard(Some(*shard), |ctx| {
          let counters = ctx.counters();
          counters
            .spin_hits
            .saturating_add(counters.spin_misses)
            .saturating_add(counters.spin_deadlines)
        })
      })
      .collect()
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

  /// The candidate holders in the order the object's owner shard uses for its puts (§4.8),
  /// including the committed failure domains. Observers must not reconstruct this order from
  /// a neighbourhood alone: a member's domain may differ from its current voter identity.
  pub fn placement_candidates(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      s.fleet.configuration().place(object).candidates
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
      s.held_content.holds_manifest_for_any_object(&manifest)
    })
  }

  /// The archive this node holds for `object` under `manifest` as a content candidate, encoded as it would travel, or
  /// `Ok(None)` when it holds none: what a holder keeps, which for a sealed volume is an envelope of ciphertext and
  /// keyed names (A-92 piece 3b) — a test or an operator reads it to see what a holder learns. Runs on the control shard
  /// ([`Self::observation`]); the typed refusal when it could not be observed.
  pub fn fleet_held_archive(
    &self,
    object: slates_db::register::ObjectId,
    manifest: [u8; 32],
  ) -> Result<Option<Vec<u8>>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.held_content
        .archive_of(s.store.content.arena(), object, &manifest)
        .map(|archive| archive.encode())
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

  /// Test support: serves one content `request` (an encoded `ContentMessage`) on this node's hold as an
  /// authorized peer would have it served — the production holder path (`crate::content_holder::serve`)
  /// with only the authority decision granted — and returns the reply bytes. A test places a replica on a
  /// daemon with no fleet peer through it, to observe what the hold keeps across a restart (AUD-29-59).
  pub fn serve_content_as_authorized(&self, request: Vec<u8>) -> Result<Vec<u8>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let local = s.fleet.host();
      crate::content_holder::serve(s, local, &request, |_, _, _, _| true)
    })
  }

  /// What this node holds for other owners and what it is charged for it (§4.2; AUD-29-43), on the shard
  /// that keeps its hold: the byte budget's `replicated` charge beside the hold's own account (they must
  /// agree), the index charge, the manifests held, the puts refused at the capacity bound, and the bytes the
  /// shard may still admit. An operator's and a test's view; the typed refusal when the shard does not answer.
  pub fn fleet_replica_account(&self) -> Result<ReplicaAccount, ObserveError> {
    self.observe(self.shards.first().copied(), |s| ReplicaAccount {
      replicated: s.store.budget.replicated(),
      charged: s.held_content.charged_bytes(),
      index: s.held_content.index_bytes(),
      manifests: s.held_content.manifest_count(),
      refused_capacity: s.held_content.refused_capacity(),
      stages: s.held_content.stage_count(),
      admittable: s.store.admittable(),
      hold: s.store.budget.hold(),
    })
  }

  /// Test support: makes this node **forget** the content it holds for `manifest` as a candidate holder —
  /// the manifest record and the chunks only it referenced — the state a holder is in after losing its
  /// anchor (§4.8 "Recovery": a whole-anchor loss holds nothing for others until re-replication fills it; a
  /// warm restart keeps what it acknowledged, A-51), injected
  /// deterministically because an in-process daemon cannot restart (its fleet sockets are leaked to the
  /// process, the same reason `observe_peer_dead` injects a death). The healer (§4.10) must notice and
  /// re-put it. Delivered like [`Self::observe_peer_dead`]: admission retried while the control channel is
  /// full, the drop awaited, both within the observe budget; returns whether the manifest **was held and is
  /// now forgotten** — `Ok(false)` if it was not held, the typed refusal if the daemon could not be reached.
  pub fn drop_held_content(&self, manifest: [u8; 32]) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.held_content.forget_manifest_for_every_object(
        &mut crate::content_holder::hold_space(&mut s.store),
        &manifest,
      )
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

  /// The chain of `green` as the shard that serves it holds it: each version's increment identity in
  /// order (§4.16; AUD-14) — what a test compares between the departed owner and its successor to prove
  /// the full ledger prefix and the original results were preserved through the takeover. Empty for a
  /// green this daemon does not serve; the typed refusal when the shard could not be observed.
  pub fn merge_chain_identities(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<Vec<(u64, [u8; 32])>, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    let volume = slates_db::catalog::VolumeId { bytes: green.bytes };
    self.observe(self.shard_of_object(object), move |s| {
      s.db
        .partition()
        .green_chain(volume)
        .iter()
        .enumerate()
        .filter_map(|(index, bytes)| {
          slates_merge::engine::Increment::decode(bytes)
            .ok()
            .map(|increment| (u64::try_from(index).unwrap_or(u64::MAX) + 1, increment.id))
        })
        .collect()
    })
  }

  /// How many submits of `green` are waiting for their version's merge record to commit at the quorum
  /// before they are answered (§4.16 "Commit"; AUD-11) — the non-vacuity a test asserts while it
  /// withholds a holder's inputs or acknowledgements. Runs a one-shot question on the green's owner
  /// shard; the typed refusal when it could not be observed.
  pub fn merge_awaiting(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<usize, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    let volume = slates_db::catalog::VolumeId { bytes: green.bytes };
    self.observe(self.shard_of_object(object), move |s| {
      crate::merge_service::awaiting_count(s, volume)
    })
  }

  /// What `green` holds in memory beyond its durable chain and what it has charged the shard's budget
  /// for it (§4.2 all-cost admission, §4.16; AUD-16): the current files, the content history retained
  /// for reconstruction (as the engine's running total and recounted from the histories — the balance a
  /// test asserts), the rejected-result cache's bytes, entries and evictions, the retention charge, and
  /// the fold floor. Runs a one-shot question on the green's owner shard; the typed refusal when it
  /// could not be observed.
  pub fn merge_retention(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<crate::merge_service::MergeRetention, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    let volume = slates_db::catalog::VolumeId { bytes: green.bytes };
    self.observe(self.shard_of_object(object), move |s| {
      crate::merge_service::merge_retention(s, volume)
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

  /// Test support: the host memory available that this daemon's pressure sampler reads from now on, in
  /// place of the platform's reading (§4.2; admission.md §5.5) — so a test drives the sampler's
  /// arithmetic (its baseline and shortfall) deterministically. Installed on every shard, since the
  /// sampling shard is the reaper's.
  pub fn inject_available_memory(&self, bytes: u64) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| s.injected_available = Some(bytes))?;
    }
    Ok(())
  }

  /// The baseline this daemon's pressure sampler measures against, once its first sample is taken (§4.2):
  /// the sampling shard's, read from whichever shard holds it.
  pub fn pressure_baseline(&self) -> Result<Option<u64>, ObserveError> {
    let mut baseline = None;
    for shard in self.shards.iter().copied() {
      baseline = baseline.or(self.observe(Some(shard), |s| s.pressure_baseline)?);
    }
    Ok(baseline)
  }

  /// What the slices of granted landings on this daemon's first shard measured (AUD-29-25): how many, their
  /// p50/p99/p999 and maximum, their budget, and how many ran past it by more than their last unit.
  pub fn landing_slices(&self) -> Result<crate::landing::SliceSummary, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.landing.slice_summary())
  }

  /// The pressure hold on this daemon's first shard now (§4.2): what the sampler, or a test, withholds
  /// from new admission there.
  pub fn pressure_hold(&self) -> Result<u64, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.store.budget.hold())
  }

  /// Test support: fences the control shard as a failed consensus publication does (`retention`): from then
  /// on it serves nothing and every ordinary borrow of its state is refused `Fenced`. A test proves what a
  /// fenced shard still owes — its kernel mounts released at the stop — without corrupting an anchor.
  pub fn inject_consensus_failure(&self) -> Result<(), ObserveError> {
    let control = self.shards.first().copied();
    match self.observe(control, |s| {
      s.consensus_failure = Some(slates_anchor::AnchorError::Layout {
        reason: "test support: an injected consensus retention failure",
      });
      s.consensus_ready = false;
    }) {
      // The borrow's own retention check names the fence it just installed.
      Ok(()) | Err(ObserveError::State(state::StateAccess::Retention(_))) => Ok(()),
      Err(other) => Err(other),
    }
  }

  /// Test support: sets the memory-pressure hold on **every** shard's byte budget (§4.2; admission.md
  /// §5.5) — capacity withheld from new admission under host memory pressure. In production
  /// [`refresh_pressure_hold`] sets it from a sampled host shortfall at the liveness cadence; this
  /// drives it directly so a test proves the mechanism without a real low-memory host: a raised hold
  /// refuses a new reservation while an admitted volume's within-entitlement writes still land. The
  /// injected hold is pinned: the sampler leaves those shards' holds alone from then on, so the test is
  /// not raced by the host's next sample.
  /// Installed on every shard (a test need not know which shard a volume routes to). `Ok` once
  /// installed everywhere, else the first shard's typed refusal.
  pub fn inject_pressure_hold(&self, bytes: u64) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.pressure_pinned = true;
        s.store.budget.set_hold(bytes);
      })?;
    }
    Ok(())
  }

  /// The memory-pressure hold now on each shard's byte budget (§4.2), in shard order — the capacity
  /// each withholds from new admission. Zero when the host is not under pressure (or has no sampler).
  /// An observation a shard could not answer is its typed refusal.
  pub fn pressure_holds(&self) -> Result<Vec<u64>, ObserveError> {
    let mut holds = Vec::with_capacity(self.shards.len());
    for shard in self.shards.iter().copied() {
      holds.push(self.observe(Some(shard), |s| s.store.budget.hold())?);
    }
    Ok(holds)
  }

  /// Test support: the next publication on **every** shard's database fails as `fault` names, once
  /// (`Db::inject_publication_fault`; `None` clears) — before the record's append, so the transaction
  /// rolls back, or after it, so the maintenance snapshot is deferred (§4.8 transactions, AC-2.3;
  /// AUD-06). Installed on every shard so a test need not know which shard a verb routes to; the
  /// shard that runs the next verb consumes its fault and the rest keep theirs until cleared.
  /// `Ok` once installed everywhere, else the first shard's typed refusal.
  pub fn inject_publication_fault(
    &self,
    fault: Option<slates_db::replay::PublicationFault>,
  ) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| s.db.inject_publication_fault(fault))?;
    }
    Ok(())
  }

  /// Per shard: transactions rolled back because their record could not be made durable, and
  /// maintenance snapshots deferred after a durable append (`Db::rollbacks`,
  /// `Db::maintenance_failures`; AUD-06) — the non-vacuity counters the publication-failure
  /// regression asserts. An observation a shard could not answer is its typed refusal.
  pub fn db_publication_counters(&self) -> Result<Vec<PublicationCounters>, ObserveError> {
    let mut counters = Vec::with_capacity(self.shards.len());
    for shard in self.shards.iter().copied() {
      counters.push(self.observe(Some(shard), move |s| PublicationCounters {
        shard: shard.0,
        rollbacks: s.db.rollbacks(),
        maintenance_failures: s.db.maintenance_failures(),
        snapshots_taken: s.db.snapshots_taken(),
      })?);
    }
    Ok(counters)
  }

  /// Test support: makes this node's probe serve side leave the **direct** probes of `peers` unanswered
  /// (an empty reply, which the prober reads as a timed-out probe), so a prober among them cannot reach this
  /// node directly while every other peer still can — the asymmetric path loss the indirect-probe stage
  /// exists for (§4.8 "direct probe → k indirect proxies"; AUD-15). Replaces any earlier set; an empty
  /// `peers` restores full service. Relayed traffic is unaffected: a relay's own probe of this node, and
  /// the ping-requests and indirect acknowledgements it carries, are answered as usual. Delivered like
  /// [`Self::inject_discovery_fault`]; `Ok` once installed, else the typed refusal.
  pub fn inject_probe_deafness(&self, peers: &[slates_db::HostId]) -> Result<(), ObserveError> {
    let deaf_to: std::collections::BTreeSet<slates_db::HostId> = peers.iter().copied().collect();
    self.observe(self.shards.first().copied(), move |s| {
      s.probe_deaf_to = deaf_to;
    })
  }

  /// Test support: makes each of this node's campaigns count the record sessions of `voters` as out of their
  /// links for `span_ns` after its round begins, as a discovery page holds one for its round trip, so a test
  /// drives a campaign into sessions that are out for a moment
  /// (`docs/bugs/2026-09-29-a-campaign-asked-no-one-while-a-session-was-out.md`). An empty `voters` disarms
  /// it. `Ok` once installed, else the typed refusal.
  pub fn inject_campaign_session_hold(
    &self,
    voters: &[slates_db::HostId],
    span_ns: u64,
  ) -> Result<(), ObserveError> {
    let voters: std::collections::BTreeSet<slates_db::HostId> = voters.iter().copied().collect();
    self.observe(self.shards.first().copied(), move |s| {
      s.campaign_session_hold = (!voters.is_empty()).then_some((voters, span_ns));
    })
  }

  /// Whether this node's **owner lease** over `object` holds right now (§4.8 "Leases and reads"; AUD-08):
  /// `f` of the object's other candidate holders confirmed this node alive under the installed
  /// configuration within the lease bound (or the bounded startup allowance is still open), and it is not
  /// superseded — so a read of the object's latest state serves rather than refusing `LeaseUnconfirmed`.
  /// A test reads it to see the lease **lapse** on an isolated owner (the non-vacuity witness the refusal
  /// pairs with) and **hold** again on the successor. Runs on the object's owner shard
  /// ([`Self::shard_of_object`]); the typed refusal when that shard could not be observed. `Ok(true)` on a
  /// laptop (`f = 0` needs no confirmation).
  pub fn fleet_lease_holds(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<bool, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      crate::verbs::lease_verdict(s, object).holds()
    })
  }

  /// Test support: the NFS mount path that carries a **mount capability** for the volume named `name`
  /// (§4.13; AUD-01) — `/<name>@<attachment_hex>.<token_hex>`. Mints an attachment for the volume on its
  /// owner shard **as the volume's owner** with the owner's full rights (the durable record the real
  /// `attach` verb writes, through the same `Op::AttachmentAdded`), so a test mounts and reads the volume
  /// over the daemon's loopback port the way a consumer that called `attach` would, and every request's
  /// file handle is validated against that record. In-process only (`Daemon` is the embedding handle,
  /// not a wire surface); the typed refusal when the name's owner shard could not be observed, `Ok(None)`
  /// when no volume has that name there or the secure random could not mint a token.
  pub fn mount_capability(&self, name: &str) -> Result<Option<String>, ObserveError> {
    let partition = verbs::owner_of_name(name, self.shards.len());
    let name = name.to_owned();
    self.observe(self.shards.get(usize::from(partition)).copied(), move |s| {
      let record = s.db.partition().volume_by_name(&name)?.clone();
      let token = verbs::mint_mount_token()?;
      let attachment = verbs::next_attachment_id(s);
      let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
      let op = slates_db::Op::AttachmentAdded {
        record: slates_db::catalog::AttachmentRecord {
          id: attachment,
          volume: record.id,
          // The mount's own attachment (the bridge's, AUD-01): it outlives any client and a restart.
          consumer: slates_db::catalog::Consumer::Bridge,
          snapshot: None,
          form: slates_db::catalog::AttachForm::Root,
          principal: record.owner.clone(),
          rights: verbs::rights_of(&record, &record.owner),
          token,
        },
      };
      // One durable transaction, as the verb path writes it (`Db::begin` … `commit`, AC-2.3): the
      // record is published into anchor-owned RAM, so a handle minted under it still validates on a
      // daemon that restarted over the segment; a commit that could not publish rolls it back.
      s.db.begin();
      s.db.mutate(&mut s.segment, &op, now).ok()?;
      s.db.commit(&mut s.segment).ok()?;
      let token_hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
      Some(format!("/{name}@{attachment:x}.{token_hex}"))
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

  /// The access list of the volume `object` names as this node's catalog records it, or `None` when this
  /// node holds no record of it — what a successor serves a taken-over volume's consumers under
  /// (AUD-29-17). Asked on the volume's owner shard.
  pub fn volume_access(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<Vec<slates_db::catalog::AccessEntry>>, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      s.db
        .partition()
        .volume(slates_db::catalog::VolumeId { bytes: object.0 })
        .map(|record| record.access.clone())
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
    self.stop_parts();
  }

  /// Stops the doorbell and the runtime if they still run. A shard worker that failed is reported on the
  /// daemon's error stream, which its anchor keeps (every worker is still joined and every slot given back,
  /// AUD-29-12); with `panic = "abort"` a release build never reaches here after a worker's panic.
  fn stop_parts(&mut self) {
    // The FUSE mounts end with each shard's serve loop (`EndMounts`), on the shard's own thread as the
    // shutdown cancels it — never as a question queued behind the shard's work, which would answer every
    // question already waiting there instead of terminating it (§4.6; AUD-29-64).
    if let Some(mut doorbell) = self.doorbell.take() {
      doorbell.stop();
    }
    if let Some(runtime) = self.runtime.take()
      && let Err(error) = runtime.shutdown()
    {
      eprintln!("slates-server: stopping the daemon's shards: {error}");
    }
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    self.stop_parts();
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
/// repeat the old member identity. The secret itself never crosses the membership wire. Never zero:
/// nonce zero names the manifest's routing placeholder (`deploy::member_id`), which a live incarnation
/// must never equal — the record link keeps its pending dial across the placeholder's replacement on
/// exactly that promise (`fleet::refresh_record_identity_in`). A zero digest word (odds 2⁻⁶⁴) becomes one.
fn boot_incarnation(secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES]) -> u64 {
  let digest = blake3::keyed_hash(secret, b"slates/member-incarnation/v1");
  let mut word = [0; size_of::<u64>()];
  word.copy_from_slice(&digest.as_bytes()[..size_of::<u64>()]);
  u64::from_le_bytes(word).max(1)
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

/// Where this shard's slice of the anchor's content object starts: its partition's index times the slice's length
/// ([`DaemonConfig::shard_content_layout`]). The shard's order in the configuration names its partition.
fn slice_start(config: &DaemonConfig, partition: u16) -> usize {
  usize::from(partition).saturating_mul(config.shard_content_layout().stride)
}

/// This shard's delta log range (A-68), beside its checkpoint slots: `(0, 0)` when the slots have none (no object, or
/// one too small for the layout).
fn delta_range(config: &DaemonConfig, partition: u16, images: (usize, usize)) -> (usize, usize) {
  if images.1 <= images.0 {
    return (0, 0);
  }
  let layout = config.shard_content_layout();
  let start = slice_start(config, partition);
  (
    start.saturating_add(layout.delta_log.0),
    start.saturating_add(layout.delta_log.1),
  )
}

/// This shard's range of the content object for its recovery images (§4.8), `(0, 0)` without an object or with
/// one too small for the layout.
fn image_range(
  config: &DaemonConfig,
  partition: u16,
  content: Option<&slates_mem::SparseObject>,
) -> (usize, usize) {
  let layout = config.shard_content_layout();
  let start = slice_start(config, partition);
  let range = (
    start.saturating_add(layout.images.0),
    start.saturating_add(layout.images.1),
  );
  match content {
    Some(object) if range.1 <= object.len() => range,
    _ => (0, 0),
  }
}

/// This shard's content range for its recovery images (§4.8).
#[cfg(not(target_os = "linux"))]
fn content_slice(
  config: &DaemonConfig,
  partition: u16,
  content: Option<&slates_mem::SparseObject>,
) -> (usize, usize) {
  image_range(config, partition, content)
}

/// This shard's content range for its recovery images (§4.8), and the FUSE write log at the slice's front with what
/// the previous daemon left in it to replay (A-63, `crate::write_log`), opened against the generation of the
/// committed image, before anything this daemon does can publish and clear it. No log and nothing to replay without
/// a content object.
#[cfg(target_os = "linux")]
fn content_slice(
  config: &DaemonConfig,
  partition: u16,
  content: Option<&mut slates_mem::SparseObject>,
) -> (
  (usize, usize),
  Option<crate::write_log::WriteLog>,
  crate::write_log::Recovered,
) {
  let images = image_range(config, partition, content.as_deref());
  let Some(object) = content.filter(|_| images.1 > images.0) else {
    return ((0, 0), None, crate::write_log::Recovered::default());
  };
  let layout = config.shard_content_layout();
  let log_start = slice_start(config, partition).saturating_add(layout.write_log.0);
  let log_bytes = layout.write_log.1.saturating_sub(layout.write_log.0);
  // The journal's generation — the checkpoint's advanced over its committed deltas (A-68) — since a write logged
  // after a delta is not yet in any image.
  let deltas = delta_range(config, partition, images);
  let generation = slates_vfs::checkpoint_log::committed_generation(
    &crate::verbs::ContentView {
      object,
      start: images.0,
      len: images.1.saturating_sub(images.0),
    },
    &crate::verbs::ContentView {
      object,
      start: deltas.0,
      len: deltas.1.saturating_sub(deltas.0),
    },
  );
  match crate::write_log::WriteLog::open(object, log_start, log_bytes, generation) {
    Some((log, recovered)) => (images, Some(log), recovered),
    None => (images, None, crate::write_log::Recovered::default()),
  }
}

/// This shard's chunk arena (A-98): the pool extents its partition holds from a previous daemon, added under their
/// ids before recovery claims the blocks its image names, and the pool as the source it grows from. Also the capacity
/// of one slice, the scale the operation headroom is derived against.
fn shard_arena(
  config: &DaemonConfig,
  partition: u16,
  env: &[(String, String)],
  identity: &Identity,
) -> Result<(ChunkArena, usize), ServerError> {
  let (handoff, len) = handoff_of(env)?;
  let segment = AnchorSegment::attach(&handoff, len, identity)?;
  let mut pool = crate::content_pool::ContentPool::open(config, partition, env, segment);
  let mut arena = ChunkArena::new(config.content_granule());
  for (id, region) in pool.held()? {
    arena.add_region_at(id, region)?;
  }
  let slice = pool.slice_capacity();
  arena.set_source(Box::new(pool));
  Ok((arena, slice))
}

/// Runs on the shard: attaches the segment, recovers the partition, builds the store and
/// installs the state, then spawns the server loop as a poller.
fn init_shard(
  config: &DaemonConfig,
  env: &[(String, String)],
  identity: &Identity,
  (partition, sealing): (u16, crate::seal_keys::RootState),
  config_shards: &[u16],
  retained: Option<crate::retention::Retained>,
  #[cfg(target_os = "linux")] inherited_fuse: Vec<crate::fuse_hold::HeldDevice>,
) -> Result<(), ServerError> {
  let (handoff, len) = handoff_of(env)?;
  let mut segment = AnchorSegment::attach(&handoff, len, identity)?;
  let mut clock = HostClock::new();
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
  let (db, recovered) = slates_db::replay::recover(&mut segment, partition, config.caps, now)?;
  let (arena, slice_capacity) = shard_arena(config, partition, env, identity)?;
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
  // Against the shard's own slice, not what it holds now: a shard claims extents lazily and may hold none (A-98).
  let capacity = u64::try_from(slice_capacity).unwrap_or(u64::MAX);
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
  // One status capture's room, kept from every volume's records (§4.14): a client ring's status snapshot capacity, the
  // half of its bulk area `slates_ipc::status::snapshot_capacity` bounds a capture by.
  store
    .metadata
    .set_observation_room(slates_ipc::status::snapshot_capacity_of(
      config.region.bulk_bytes,
    ))
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
  // The node's sealing root (A-92), read from this shard's own attachment into a key of its own in the locked region.
  let seal_root = crate::seal_keys::shard_root(&segment, sealing);
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
    fleet.install_configuration(configuration, &council_members);
  }
  // The root group across regions (§4.8, D-14): the regions the fleet spans and the representative host of
  // each (the root voters), driven over the transport by the control shard's config plane
  // (`crate::fleet::drive_root_group`), exactly as the regional council is.
  let (root, node_regions, region_mirrors) = build_root_group(config, host);
  // The anchor-owned content object that survives a restart (§4.8), if the anchor provides one.
  // Shards share the one object, partitioned by index: this shard owns the slice `[start, end)`.
  #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
  let mut content = match AnchorSegment::open_content(env) {
    Some(result) => Some(result?),
    None => None,
  };
  #[cfg(target_os = "linux")]
  let (content_range, write_log, recovered_writes) =
    content_slice(config, partition, content.as_mut());
  #[cfg(not(target_os = "linux"))]
  let content_range = content_slice(config, partition, content.as_ref());
  // The telemetry sink keeps the most recent spans up to one client ring's depth (§4.14): a shard
  // processes at most a ring of in-flight requests, so a ring's depth of recent spans covers the
  // current activity window; older spans are shed (and counted), telemetry being the shed-first class.
  let telemetry_capacity = usize::try_from(config.region.slots).unwrap_or(1).max(1);
  // The grant-issuer secret this daemon minted at start (§4.13), read from the anchor's supervision block
  // before the segment moves into the state; every shard reads the same value, so a `Grant` verifies on
  // whichever shard serves the client.
  let issuer_secret = segment.issuer_secret()?;
  // The attachment counter starts past every attachment recovered with the partition — a host mount's
  // survives a restart (AUD-01) — so a fresh mint never meets a kept record's id.
  let next_attachment = verbs::next_attachment_counter(db.partition());
  let mut state = ShardState {
    shard,
    partition,
    config: config.clone(),
    segment,
    issuer_secret,
    seal_root,
    seal_state: sealing,
    seal_recipient: None,
    pairs_delivered: std::collections::BTreeSet::new(),
    peer_recipients: std::collections::BTreeMap::new(),
    content,
    content_range,
    delta_range: delta_range(config, partition, content_range),
    journal: slates_vfs::checkpoint_log::Journal::default(),
    published_keys: std::collections::BTreeSet::new(),
    published_held: Vec::new(),
    #[cfg(target_os = "linux")]
    write_log,
    #[cfg(target_os = "linux")]
    writes_lost: recovered_writes.everything_lost,
    #[cfg(target_os = "linux")]
    replay: recovered_writes.records,
    #[cfg(target_os = "linux")]
    pending_replies: std::collections::BTreeMap::new(),
    #[cfg(target_os = "linux")]
    recovered_replies: std::collections::BTreeMap::new(),
    write_verifier: now.to_be_bytes(),
    #[cfg(unix)]
    nfs_v4: None,
    #[cfg(unix)]
    nfs_v4_files: None,
    #[cfg(unix)]
    nfs_v4_instance: None,
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
    next_attachment,
    attachments: slates_bridge_core::Attachments::new(),
    mount_attachments: std::collections::BTreeMap::new(),
    #[cfg(target_os = "linux")]
    fuse_mounts: std::collections::BTreeMap::new(),
    stale_fuse_mounts: Vec::new(),
    #[cfg(target_os = "linux")]
    inherited_fuse,
    #[cfg(target_os = "linux")]
    adopt_fuse: Vec::new(),
    snapshot_views: std::collections::BTreeMap::new(),
    #[cfg(unix)]
    guest_devices: Vec::new(),
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
    reply_route: None,
    current_request: None,
    acceptance_deferred: false,
    lease_waiters: crate::lease_wait::LeaseWaiters::default(),
    forwarded_rings: std::collections::BTreeMap::new(),
    telemetry_quota: config.telemetry_spans_per_reply,
    last_drain_ns: now,
    placed_heads: std::collections::BTreeMap::new(),
    formed_probe_peers: std::collections::BTreeSet::new(),
    plane: crate::member_task::PlaneState::default(),
    #[cfg(unix)]
    nfs_service: crate::nfs::ServiceTimes::default(),
    #[cfg(unix)]
    callbacks: crate::callback::Callbacks::default(),
    staging: crate::staging::Staging::default(),
    member_boot_nonce: incarnation,
    learned_members: std::collections::BTreeMap::new(),
    authenticated_members: std::collections::BTreeSet::new(),
    council_death_watch: std::collections::BTreeMap::new(),
    discovery: None,
    enrolled: Vec::new(),
    demuxes: Vec::new(),
    holder_records: std::collections::BTreeMap::new(),
    host_takeovers: std::collections::BTreeMap::new(),
    config_refresh_wanted: false,
    record_sessions: std::collections::BTreeMap::new(),
    link_waiters: std::collections::BTreeMap::new(),
    discovery_withhold_replies: false,
    peer_paths: std::collections::BTreeMap::new(),
    probe_windows: crate::fleet::ProbeWindows::default(),
    probe_deaf_to: std::collections::BTreeSet::new(),
    campaign_session_hold: None,
    record_refused_from: std::collections::BTreeSet::new(),
    // The owner lease starts its bounded startup allowance at boot (§4.8 "Leases and reads", AUD-08): the
    // node has just installed its initial configuration, so within the first membership horizon it serves
    // its objects while its first probe acks accumulate, and no takeover can yet have committed.
    lease: crate::lease::OwnerLease {
      configuration_installed_ns: Some(now),
      ..crate::lease::OwnerLease::default()
    },
    fanned: crate::fleet::Fanned::default(),
    pressure_baseline: None,
    pressure_pinned: false,
    injected_available: None,
    answers_given: crate::lease::AnswersGiven::default(),
    departed_owners: std::collections::BTreeMap::new(),
    green_retention: std::collections::BTreeMap::new(),
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
    pending_tombstones: std::collections::BTreeMap::new(),
    seals: std::collections::BTreeMap::new(),
    put_latency: crate::fleet::LatencyWindow::default(),
    fetch_latency: crate::fleet::LatencyWindow::default(),
    put_outcomes: crate::fleet::PutOutcomes::default(),
    healer: crate::fleet::HealerCursor::default(),
    repairs: 0,
    pending_materializations: std::collections::BTreeMap::new(),
    pending_catalogs: std::collections::BTreeMap::new(),
    fenced_greens: std::collections::BTreeMap::new(),
    pending_green_materializations: std::collections::BTreeMap::new(),
  };
  // The landing counter starts past every landing recovered with the partition (its records are
  // durable and the guard refuses a duplicate id), as the attachment counter does.
  state.landing.next_landing = verbs::next_landing_counter(state.db.partition());
  // The runtime grants, rebuilt from the durable ones in their recorded states (AUD-29-06).
  crate::landing::restore_grants(&mut state);
  if let Some(retained) = retained {
    retained.restore(&mut state)?;
  }
  crate::retention::retain(&mut state)?;
  crate::retention::derive_log_budgets(&mut state);
  // The node's ML-KEM recipient (A-92 piece 4a), on the control shard, whose partition holds its sealed record: opened
  // under the root, or made and recorded now. Unavailable sealing leaves it `None`, reported in status.
  if partition == 0 {
    state.seal_recipient = match crate::seal_keys::recipient(&mut state) {
      Ok(recipient) => Some(recipient),
      Err(e) => {
        eprintln!("slates-server: the node's sealing recipient is unavailable: {e:?}");
        None
      }
    };
  }
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
      "slates-server: shard {shard}: recovered {} volumes from their images ({} refused), {} merge volumes (green chains replayed, works reset), reconciled out {} unrecovered local snapshots and {} attachments, trimmed {} unacknowledged snapshots the images carried, completed {} destroys in flight, corrected {} clone pins, reclaimed {} orphans no holder survived, replayed {} logged writes",
      rebuilt.volumes,
      rebuilt.skipped,
      rebuilt.merge_volumes,
      rebuilt.snapshots_dropped,
      rebuilt.attachments_dropped,
      rebuilt.snapshots_trimmed,
      rebuilt.destroys_completed,
      rebuilt.pins_reconciled,
      rebuilt.orphans_reclaimed,
      rebuilt.writes_replayed
    );
    eprintln!(
      "slates-server: shard {shard}: held {} replicas again from the image{}",
      rebuilt.replicas,
      if rebuilt.replicas_refused {
        " (the held replicas did not recover: the hold starts empty)"
      } else {
        ""
      }
    );
  }
  state::install(state);
  // The FUSE mounts whose devices the anchor held, served again (A-61); then the dead ones of a killed
  // predecessor unmounted, once the shard can run their helpers (AUD-29-64).
  #[cfg(target_os = "linux")]
  crate::fuse::adopt_held();
  #[cfg(target_os = "linux")]
  crate::fuse::unmount_stale();
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
    pace(cadence).await;
    state::with_state(|s| {
      let _ = verbs::expire_leases(s);
      // A destroy whose completion the catalog refused is recorded again here, at the reaper's cadence
      // (`verbs::step_destroys`), rather than in a busy serve round.
      let _ = verbs::step_destroys(s);
    });
    let reaped = crate::reap::sweep(cadence).await;
    if reaped > 0 {
      CLIENTS_REAPED.fetch_add(u64::try_from(reaped).unwrap_or(u64::MAX), Ordering::AcqRel);
    }
    refresh_pressure_hold();
  }
}

/// Samples the host's available memory and sets each shard's pressure hold (§4.2; admission.md §5.5),
/// run on the reap loop's shard at the liveness cadence — the design's "cheap-refresh". The hold is
/// the host-wide shortfall below the boot baseline, divided evenly among the shards (so the total
/// held across the shards is the shortfall, not a multiple of it); it shrinks each shard's admittable
/// so a new reservation is refused under pressure while an admitted volume's within-entitlement writes
/// land, and it releases to zero as the sample recovers toward the baseline. A host with no memory
/// sampler (`memory_available_now` is `None` on an unsupported platform) leaves the hold at zero — the
/// budget then admits exactly as before, no worse than not sampling. The read is one cheap syscall or
/// `/proc` line per cadence, not per request.
fn refresh_pressure_hold() {
  let injected = state::with_state(|s| s.injected_available).flatten();
  let Some(available) = injected.or_else(slates_machine::facts::Facts::memory_available_now) else {
    return;
  };
  // This daemon's first sample fixes its baseline (kept by the sampling shard, so each daemon in a
  // process has its own); a later sample above it only lowers the shortfall.
  let Some((origin, shards, baseline)) = state::with_state(|s| {
    let baseline = *s.pressure_baseline.get_or_insert(available);
    (s.shard, s.shards.clone(), baseline)
  }) else {
    return;
  };
  let shortfall = baseline.saturating_sub(available);
  let per_shard = shortfall / u64::try_from(shards.len().max(1)).unwrap_or(1);
  for shard in shards {
    let _ = crate::xshard::run_on(origin, shard, move |s| {
      if !s.pressure_pinned {
        s.store.budget.set_hold(per_shard);
      }
    });
  }
}

/// Ends the shard's FUSE mounts when its serve loop ends (§4.6 "Linux"; AUD-29-64): the loop is perpetual,
/// so it ends only when the daemon's stop cancels every task on the shard, and its drop runs on the shard's
/// own thread with the shard's state still installed. A mount whose daemon is gone answers every call
/// `ENOTCONN` until someone unmounts it, so the stop unmounts them here — not by a question sent ahead of the
/// shutdown, which waits behind whatever the shard is running and answers the questions queued before it.
#[cfg(target_os = "linux")]
struct EndMounts;

#[cfg(target_os = "linux")]
impl Drop for EndMounts {
  fn drop(&mut self) {
    // A fenced shard refuses every ordinary borrow: its mounts are still released, and their records end
    // at the next start's recovery (a record is never written on a fenced shard).
    if state::with_state(crate::fuse::unmount_all).is_none() {
      let _ = state::with_state_at_shutdown(crate::fuse::unmount_points);
    }
  }
}

/// The shard's server loop: a poller of its clients' rings; serves while there is work, keeps
/// polling for the idle window after its last work (§4.7 "Wake strategy": a shard polls while
/// any client has activity within the window), and idles past it. The window is a multiple of what a
/// wake costs the shard now — its online wake estimate (§4.3) — so it follows the wakes the shard
/// actually pays rather than the boot probe's first guess.
async fn serve_loop() {
  #[cfg(target_os = "linux")]
  let _end_mounts = EndMounts;
  if let Some(task) = futures::current_task() {
    let _ =
      registry::with_current(|ctx| ctx.register_poller(task, Box::new(state::any_ring_ready)));
    state::with_state(|s| s.server_task = Some(task));
  }
  loop {
    let idle_window_ns = derived!(
      futures::wake_cost_ns()
        .unwrap_or(0)
        .saturating_mul(crate::config::IDLE_WINDOW_RATIO),
      "the shard's wake estimate × IDLE_WINDOW_RATIO",
      ["rt.wake_cost_ns", "IDLE_WINDOW_RATIO"]
    )
    .get();
    let (did, within_window) = state::with_state(|s| {
      let did = verbs::serve_round(s);
      let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
      if did {
        s.last_work_ns = now;
      }
      (did, now.saturating_sub(s.last_work_ns) < idle_window_ns)
    })
    .unwrap_or((false, false));
    if did {
      // Client work opens the shard's idle window (§4.7): after this loop goes quiet, the shard itself
      // spins out what is left of the window before it parks, and parks at once outside it.
      registry::with_current(|ctx| ctx.note_activity());
    }
    if did || within_window {
      futures::yield_now().await;
    } else {
      // Announce idle, fence, re-check (the doorbell protocol, `verbs::announce_idle`): a request found
      // pending is served now; one published later is rung, and the ring wakes this poller.
      let serve_instead = state::with_state(verbs::announce_idle).unwrap_or(false);
      if !serve_instead {
        futures::idle().await;
      }
      state::with_state(verbs::announce_polling);
    }
  }
}

/// Restore allocation before any handoff. Every owner can retain local completion identities;
/// the control partition additionally records ids whose client never ran a recorded verb.
async fn restore_client_ids(listener: &mut Listener, origin: u16, shards: &[ShardId]) -> bool {
  let mut highest = 0;
  for shard in shards {
    let Some(retained) = crate::xshard::call_within(
      origin,
      shard.0,
      |state| {
        state
          .db
          .partition()
          .client_id_high_water(state.origin_anchor.0)
      },
      LIVENESS_BUDGET_NS,
    )
    .await
    else {
      INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
      eprintln!(
        "slates-server: client identity recovery refused on shard {}",
        shard.0
      );
      return false;
    };
    highest = highest.max(retained);
  }
  listener.resume_after(highest);
  true
}

/// Persist admission before its region can reach the client. A refused publication or exhausted
/// id space refuses the handoff; no new caller can inherit a retained completion identity (§4.9).
fn reserve_client_identity(client: u32, instance: &str) -> Result<(), slates_ipc::IpcError> {
  let unavailable = |why| slates_ipc::IpcError::DaemonUnavailable {
    endpoint: instance.to_owned(),
    why,
  };
  if client == 0 {
    return Err(unavailable("client identity space exhausted"));
  }
  let reserved = state::with_state(|state| {
    if client
      <= state
        .db
        .partition()
        .client_id_high_water(state.origin_anchor.0)
    {
      return Ok(());
    }
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
    state
      .db
      .mutate(
        &mut state.segment,
        &slates_db::Op::ClientIdReserved { client },
        now,
      )
      .map(|_| ())
  });
  match reserved {
    Some(Ok(())) => Ok(()),
    _ => Err(unavailable("client identity reservation was not published")),
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
  if listener.raw_fd().is_none()
    && let Some(task) = futures::current_task()
  {
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
  if !restore_client_ids(&mut listener, control, &shards).await {
    return;
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
        reserve_client_identity(client_id, &config.instance)?;
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
                status_pages: crate::status_pages::StatusPages::default(),
                end,
                principal,
                client_id,
                pid,
                last_seen_ns,
                retiring: false,
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
      }
    }
    if let Err(error) = wait_for_rendezvous(&listener).await {
      ACCEPTS_FAILED.fetch_add(1, Ordering::AcqRel);
      eprintln!("slates-server: rendezvous readiness failed: {error}");
      return;
    }
  }
}

/// Register only after draining the accept queue. One-shot readiness observes connections
/// already queued and those arriving during registration, without a polling thread (§4.7).
#[cfg(target_os = "linux")]
async fn wait_for_rendezvous(listener: &Listener) -> Result<(), slates_rt::RtError> {
  let raw = listener.raw_fd().ok_or(slates_rt::RtError::DriverLost)?;
  slates_rt::readiness::readable(raw).await
}

/// Shared-memory rendezvous is driven by the word doorbell's registered poller (§4.7).
#[cfg(not(target_os = "linux"))]
async fn wait_for_rendezvous(_listener: &Listener) -> Result<(), slates_rt::RtError> {
  futures::idle().await;
  Ok(())
}

/// The heartbeat: the anchor's `daemon.alive` input, beaten at a cadence inside its budget.
/// Starts one group's handoff for a drain: nothing when this daemon does not lead it; a transfer to `target`
/// (the most caught-up other voter) when there is one, reported `HandedOff` pending the wait; `NoTarget`
/// for a lone voter; the core's typed refusal otherwise.
fn start_handoff(
  leads: bool,
  target: Option<slates_db::HostId>,
  transfer: impl FnOnce(slates_db::HostId) -> Result<(), slates_cluster::raft::TransferRefusal>,
) -> LeadershipHandoff {
  if !leads {
    return LeadershipHandoff::NotLeading;
  }
  let Some(target) = target else {
    return LeadershipHandoff::NoTarget;
  };
  match transfer(target) {
    Ok(()) => LeadershipHandoff::HandedOff { to: target },
    Err(refusal) => LeadershipHandoff::Refused(refusal),
  }
}

/// Where a draining daemon stands in one group's leadership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Office {
  /// It still leads.
  Leading,
  /// It stepped down and knows no leader yet (the successor has not won, or not yet sent its first append).
  Vacant,
  /// It stepped down and knows another leader: the successor is in office.
  Succeeded,
}

impl Office {
  fn of(leads: bool, known_leader: Option<slates_db::HostId>) -> Office {
    match (leads, known_leader) {
      (true, _) => Office::Leading,
      (false, Some(_)) => Office::Succeeded,
      (false, None) => Office::Vacant,
    }
  }
}

/// A started handoff's outcome at the drain's end: still leading, stepped down with no successor seen, or
/// handed off.
fn settle_handoff(started: LeadershipHandoff, office: Office) -> LeadershipHandoff {
  match (started, office) {
    (LeadershipHandoff::HandedOff { .. }, Office::Leading) => LeadershipHandoff::StillLeading,
    (LeadershipHandoff::HandedOff { .. }, Office::Vacant) => LeadershipHandoff::SteppedDown,
    (other, _) => other,
  }
}

async fn heartbeat_loop(segment: AnchorSegment) {
  let mut clock = HostClock::new();
  let mut last: Option<(u64, slates_rt::shard::Counters)> = None;
  loop {
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
    let counters = registry::with_current(|ctx| ctx.counters()).unwrap_or_default();
    if let Some((beat, before)) = last {
      log_late_beat(now.saturating_sub(beat), &before, &counters);
    }
    last = Some((now, counters));
    if let Ok(sup) = segment.supervision() {
      sup.beat(now);
    }
    pace(HEARTBEAT_NS).await;
  }
}

/// Logs a beat that came [`LATE_BEAT_SHARE`] of the liveness budget or more after the one before (§4.14
/// `daemon.alive`): the shard went that long without running its heartbeat task, and past the budget the anchor
/// kills it. With the gap goes what the shard did in it, from its counters `before` and `after`: its steps and
/// polls, each long poll by what held it (a task's own CPU past the quantum, the thread blocked in the kernel,
/// the host keeping a runnable thread off the CPU, or unattributed), the timers it fired, the times it waited,
/// the tasks it spawned and finished and those live, its longest poll and its measured timer overrun. So a near
/// miss says whether the shard was busy, blocked, parked or descheduled — the evidence the KIND lane's lapses
/// were owed (GAPS 2026-09-29: an idle five-replica fleet under the `wan` profile had its daemons killed every
/// minute or two, on an image from before the day's changes as on the day's).
fn log_late_beat(
  gap: u64,
  before: &slates_rt::shard::Counters,
  after: &slates_rt::shard::Counters,
) {
  if gap < LIVENESS_BUDGET_NS / LATE_BEAT_SHARE {
    return;
  }
  let delta =
    |field: fn(&slates_rt::shard::Counters) -> u64| field(after).saturating_sub(field(before));
  eprintln!(
    "slates daemon: a heartbeat came {} ms after the last (the anchor's budget is {} ms): in the gap the shard \
     ran {} steps and {} polls ({} long in a task, {} blocked in the kernel, {} preempted by the host, {} \
     unattributed), fired {} timers, waited {} times, spawned {} tasks and finished {}; {} tasks live; its \
     longest poll is {} ms and its timer overrun {} ms",
    gap / NS_PER_MS,
    LIVENESS_BUDGET_NS / NS_PER_MS,
    delta(|c| c.steps),
    delta(|c| c.polls),
    delta(|c| c.long_steps),
    delta(|c| c.blocked_steps),
    delta(|c| c.preempted_steps),
    delta(|c| c.unattributed_steps),
    delta(|c| c.timers_fired),
    delta(|c| c.waits),
    delta(|c| c.spawns),
    delta(|c| c.completed),
    after.spawns.saturating_sub(after.completed),
    after.longest_step_ns / NS_PER_MS,
    after.scheduler_overrun_ns / NS_PER_MS
  );
}

/// Shape: a beat that late — this share of the liveness budget or more after the last — is logged as a near
/// miss ([`log_late_beat`]): half the budget, so the stalls that stop short of a kill are seen too.
const LATE_BEAT_SHARE: u64 = 2;

/// Format: nanoseconds per millisecond, for the late-beat log.
const NS_PER_MS: u64 = 1_000_000;

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
pub(crate) use tests::{audit_on_shard, audit_on_shard_configured, test_profile};

#[cfg(test)]
mod tests {
  use super::registered_chokepoints;
  use slates_wire::observe::Chokepoint;

  /// Shape: each probe's wall budget for the unit tests' machine profile (milliseconds). The profile is
  /// measured once per test process ([`test_profile`]), so the budget is paid once.
  const PROBE_MS: u64 = 5;

  /// The machine profile every unit test here derives a daemon from, measured once per test process
  /// (§4.1). Production measures the machine once, at the anchor's boot, and derives every
  /// configuration from that one profile; the fixture keeps that shape. When each fixture measured
  /// its own, the wake probe ran beside every other fixture's probe and daemon. On an 18-core host
  /// the concurrent measurements kept as few as 16 wakes, the floor, and their means ran from 1.6 µs
  /// to 53 µs in one run. With six copies of this binary at once (a small CI runner's share of the
  /// CPU), 33 measurements across 18 runs were refused `MeasurementTimeout`, and so was CI run
  /// 36202635768 on its macOS runner
  /// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
  pub(crate) fn test_profile() -> slates_machine::MachineProfile {
    static PROFILE: std::sync::OnceLock<
      Result<slates_machine::MachineProfile, slates_machine::MachineError>,
    > = std::sync::OnceLock::new();
    PROFILE
      .get_or_init(|| {
        slates_machine::MachineProfile::measure(slates_machine::ProfileOptions {
          budget_per_probe: std::time::Duration::from_millis(PROBE_MS),
          codecs: false,
          core_matrix: false,
        })
      })
      .clone()
      .expect("the machine profile measures")
  }

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
    let profile = test_profile();
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
    let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
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
    let profile = test_profile();
    let instance = format!("warm-votes-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
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
        state.council_group = crate::consensus::GroupIdentity::created(
          false,
          &raft,
          encode_regional_configuration(&base),
        );
        let (raft, base) = state.root.join_state().unwrap();
        state.root_group =
          crate::consensus::GroupIdentity::created(true, &raft, encode_root_configuration(&base));
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

  /// §4.8 "Neighbourhood changes" (joint writes) and "Leases and reads" (the joint lease): while an owner's
  /// change is in flight its records go to both cohorts and its lease needs confirmations in both, so it keeps
  /// direct contact — the record session and the probe — with its settled neighbourhood's hosts, not only its
  /// current one's. Do: admit a member that displaces a non-voting host from the owner's neighbourhood, and
  /// install the result. Expect: the displaced host, still in the owner's settled neighbourhood, is kept in
  /// direct contact until the owner settles. Before, it was dropped as outside the neighbourhood, so a joint
  /// write could not reach it and the lease could never be confirmed by it.
  #[test]
  fn an_owner_keeps_direct_contact_with_its_settled_neighbourhood_while_a_change_is_in_flight() {
    use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
    use slates_db::register::{HostId, Quorum, RegionalConfiguration};
    let (displaced, kept) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let members: Vec<HostId> = (1..=4u64).map(|n| HostId(local.0 ^ n)).collect();
      let scatter = 3;
      let formed = |extra: &[HostId]| {
        let mut all = vec![local];
        all.extend_from_slice(extra);
        RegionalConfiguration::formed(
          all,
          Quorum { f: 1 },
          std::collections::BTreeMap::new(),
          scatter,
          false,
        )
      };
      let before = formed(&members).neighbourhoods[&local].hosts.clone();
      // A newcomer that rendezvous ranks into the owner's neighbourhood, displacing one of its hosts.
      let (newcomer, displaced) = (5..=u64::from(u16::MAX))
        .map(|n| HostId(local.0 ^ n))
        .find_map(|newcomer| {
          let mut probe = formed(&members);
          probe.admit(newcomer, None, scatter);
          let after = &probe.neighbourhoods[&local].hosts;
          before
            .iter()
            .find(|host| !after.contains(host))
            .map(|displaced| (newcomer, *displaced))
        })
        .unwrap();
      let mut all = vec![local];
      all.extend_from_slice(&members);
      state.council = RegionalCouncil::new(
        local,
        all,
        vec![local],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      assert!(state.council.propose(Reconfiguration::Admit {
        host: newcomer,
        domain: None,
      }));
      install_council(state);
      let placement = state.fleet.configuration();
      assert!(
        !placement.neighbourhood.contains(&displaced)
          && placement
            .settled
            .as_ref()
            .is_some_and(|settled| settled.hosts.contains(&displaced)),
        "the displaced host left the current neighbourhood and stays in the settled one"
      );
      (
        displaced,
        crate::fleet::keeps_direct_contact_with(state, displaced),
      )
    });
    assert!(
      kept,
      "the owner keeps direct contact with {displaced:?}, still in its settled neighbourhood"
    );
  }

  /// §4.8 Lookup, AC-8.14: a node whose placement has not yet installed its council's newest configuration
  /// still claims an object it owns — ownership moves only when the owner is retired, and the claim is judged
  /// against the council's own membership, so the lag cannot make a claim false. Do: commit a change on this
  /// node's council without installing it, and ask where its own object is. Expect: it claims the object. The
  /// Linux io_uring loop failed the location test on this refusal (`placement_behind`): right after a
  /// takeover the successor's coordinator period runs long, and its placement stayed a version behind its
  /// council (7 against 8) through the lookup.
  #[test]
  fn a_node_whose_placement_lags_its_council_still_claims_what_it_owns() {
    use slates_cluster::config_group::Reconfiguration;
    use slates_db::register::{HostId, ObjectId, RegionId};
    use slates_wire::Wire;
    let claimed = audit_on_shard(|state| {
      let local = state.fleet.host();
      let object = ObjectId::new(local, 1);
      state.node_regions.insert(local, RegionId(0));
      state.fleet.track_object_owner(object, local);
      let installed = state.fleet.configuration().version;
      assert!(state.council.propose(Reconfiguration::Admit {
        host: HostId(local.0 ^ 1),
        domain: None,
      }));
      assert!(state.council.configuration().version > installed);
      let query = crate::owner_location::Query::new(state, object, 0);
      crate::owner_location::serve(state, &query.to_bytes())
    });
    assert!(
      claimed.is_ok(),
      "the owner answers while its placement catches up: {claimed:?}"
    );
  }

  /// §4.8 learner fetch (D-14): a voter with nothing newer answers a caught-up fetch with no bytes
  /// (`serve_fetch`). Folding that reply adopts nothing and refuses nothing. The Linux io_uring loop's runs 125
  /// and 147 of 150 (2026-09-29) failed formation on one such reply arriving after its round: the late fold
  /// decoded it as a join, counted `consensus.join.undecodable` and `consensus_join_refused`, and the three-process
  /// test's refusal allow-list rejected both. The on-time fold skipped empty replies; the late one did not.
  #[test]
  fn an_empty_fetch_reply_is_neither_adopted_nor_a_refused_join() {
    let (refused, version, outcomes) = audit_on_shard(|state| {
      let peer = state.fleet.host();
      let version = state.council.configuration().version;
      let outcomes =
        [false, true].map(|root| crate::consensus::adopt_fetch(state, root, peer, &[]));
      let refused: u64 = state
        .refusals
        .iter()
        .filter(|(name, _)| name.contains("join"))
        .map(|(_, count)| *count)
        .sum();
      (
        refused,
        state.council.configuration().version == version,
        outcomes,
      )
    });
    assert_eq!(refused, 0, "an empty reply is no refused join");
    assert!(version, "an empty reply adopts nothing");
    assert_eq!(outcomes, [crate::consensus::FetchOutcome::Current; 2]);
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
        assert_eq!(
          crate::consensus::adopt_fetch(state, root, peer, &fetch),
          crate::consensus::FetchOutcome::Refused
        );
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
        name: String::new(),
        require_evidence: false,
        owner: slates_db::catalog::Principal::Uid { uid: 0 },
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

  /// Installs the audit node's council configuration into its placement view, as the coordinator's council
  /// sync does each period.
  fn install_council(state: &mut crate::state::ShardState) {
    let local = state.fleet.host();
    let regional = state.council.configuration().clone();
    let configuration = regional.configuration_for(local).unwrap();
    state
      .fleet
      .install_configuration(configuration, &regional.members);
  }

  /// Makes the audit node one of three members of an `f = 1` region whose council it alone votes in, holding
  /// one record of `object` written by the second member; then retires that member and installs the result, so
  /// the object resolves to its successor among the two survivors. Returns the retired member and the successor.
  fn retire_the_owner_of_a_held_object(
    state: &mut crate::state::ShardState,
    object: slates_db::register::ObjectId,
  ) -> (slates_db::HostId, slates_db::HostId) {
    use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
    use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, HostId, Quorum, Record};
    let local = state.fleet.host();
    let departed = HostId(local.0 ^ 1);
    let third = HostId(local.0 ^ 2);
    state.council = RegionalCouncil::new(
      local,
      vec![local, departed, third],
      vec![local],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    install_council(state);
    let generation = state.fleet.configuration().version;
    let mut acceptor = Acceptor::new(
      local,
      Authority {
        generation,
        owner: departed,
      },
    );
    acceptor
      .accept(&Record {
        owner: departed,
        object,
        sequence: 0,
        epoch: FIRST_EPOCH,
        generation,
        value: b"head".to_vec(),
      })
      .unwrap();
    state.holder_records.insert(object, acceptor);
    state.fleet.track_object_owner(object, departed);
    assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
    install_council(state);
    crate::takeover::resolve_held_objects(state, local);
    let successor = state.fleet.object_owner(object).unwrap();
    (departed, successor)
  }

  /// AC-2.5, §4.8 takeover; AUD-10 sibling: a takeover page whose asker is not the session's authenticated peer
  /// promises nothing; the configuration's successor, asking over its own session, is promised the record and
  /// can then commit its adoption through this holder.
  #[test]
  fn a_takeover_page_binds_its_asker_to_the_authenticated_peer() {
    use slates_db::register::{Ack, HostEpoch, HostPrepare, HostPromise, ObjectId, Record};
    let (spoofed, promised, committed) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let object = ObjectId([19; 16]);
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      let generation = state.fleet.configuration().version;
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation,
        after: None,
      };
      let spoofed = crate::takeover::serve_host_prepare(state, local, departed, &prepare.encode());
      let promised =
        crate::takeover::serve_host_prepare(state, local, successor, &prepare.encode());
      let record = Record {
        owner: successor,
        object,
        sequence: 0,
        epoch: prepare.epoch,
        generation,
        value: b"head".to_vec(),
      };
      let committed = crate::fleet::accept_held_record(state, local, successor, &record);
      (spoofed, promised, committed)
    });
    assert!(spoofed.is_empty(), "a forged asker is promised nothing");
    let promise = HostPromise::decode(&promised).expect("the successor is promised");
    assert_eq!(promise.entries.len(), 1, "the held record is listed");
    assert!(promise.next.is_none(), "one page answers in full");
    assert!(
      Ack::decode(&committed).is_ok(),
      "the successor's adoption commits here"
    );
  }

  /// §4.8 "Leases and reads"; AUD-08: a holder answers a successor's takeover page only once the retired owner's
  /// lease — which this holder's own answers to its probes may still be feeding — can have lapsed. The owner's
  /// lease needs `others − f` fresh confirmations so that every `f + 1` promotion quorum contains a confirming
  /// holder; that is safe only if every promising holder applies the gate, not only the successor. While this
  /// holder has answered the retired owner within the membership horizon it promises nothing; once it has not,
  /// it promises.
  #[test]
  fn a_holder_defers_a_promotion_while_its_answers_may_feed_the_departed_owners_lease() {
    use slates_db::register::{HostEpoch, HostPrepare, HostPromise, ObjectId};
    let (fed, lapsed) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let object = ObjectId([23; 16]);
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation: state.fleet.configuration().version,
        after: None,
      }
      .encode();
      let now = slates_machine::clock::monotonic_ns();
      state.answers_given.answered_alive(departed, now);
      let fed = crate::takeover::serve_host_prepare(state, local, successor, &prepare);
      // This holder's last answer to the retired owner is now older than the horizon.
      state
        .answers_given
        .alive_answers
        .insert(departed, now - crate::lease::horizon_ns() - 1);
      let lapsed = crate::takeover::serve_host_prepare(state, local, successor, &prepare);
      (fed, lapsed)
    });
    assert!(
      HostPromise::decode(&fed).is_err(),
      "a holder that answered the retired owner within the horizon promises nothing"
    );
    assert!(
      HostPromise::decode(&lapsed).is_ok(),
      "once its answers can no longer feed the owner's lease, the holder promises"
    );
  }

  /// §4.8 "Leases and reads"; AUD-08: the successor's own copy counts toward a cohort's `f + 1` only once its
  /// gate is open, as a holder's answer does — while this node has answered the retired owner within the horizon
  /// its round learns nothing from itself; once it has not, its copy is learned.
  #[test]
  fn a_successor_counts_its_own_copy_only_once_its_gate_is_open() {
    use slates_db::register::ObjectId;
    let (while_fed, once_lapsed) = audit_on_shard(|state| {
      let local = state.fleet.host();
      // An object whose successor is this node (the successor is one of two survivors, by rendezvous).
      let (departed, object) = (0..=u8::MAX)
        .find_map(|seed| {
          let object = ObjectId([seed; 16]);
          let departed = slates_db::HostId(local.0 ^ 1);
          let third = slates_db::HostId(local.0 ^ 2);
          let mut probe = slates_db::register::RegionalConfiguration::formed(
            vec![local, departed, third],
            slates_db::register::Quorum { f: 1 },
            std::collections::BTreeMap::new(),
            3,
            false,
          );
          probe.take_over(departed, 3);
          (probe.successor(departed, object) == Some(local)).then_some((departed, object))
        })
        .unwrap();
      let (retired, successor) = retire_the_owner_of_a_held_object(state, object);
      assert_eq!((retired, successor), (departed, local));
      let now = slates_machine::clock::monotonic_ns();
      state.answers_given.answered_alive(departed, now);
      crate::takeover::begin_round(state, departed, local).unwrap();
      let while_fed = state.host_takeovers[&departed].outstanding();
      state
        .answers_given
        .alive_answers
        .insert(departed, now - crate::lease::horizon_ns() - 1);
      crate::takeover::begin_round(state, departed, local).unwrap();
      let once_lapsed = state.host_takeovers[&departed].outstanding();
      (while_fed, once_lapsed)
    });
    assert_eq!(
      while_fed, 0,
      "a successor fed the retired owner's lease promises nothing of its own"
    );
    assert_eq!(
      once_lapsed, 1,
      "once its answers can no longer feed the lease, its copy is learned"
    );
  }

  /// Makes the audit node the bootstrap of an `f = 1` region that admits a second member and then a third,
  /// the way `bootstrap` and the leader's admissions form every fleet: the second member's settled
  /// neighbourhood is the two hosts it was admitted beside until it reports its new one placed. Holds one record
  /// of `object` written by the second member while its change is in flight, then takes the second member over
  /// and installs the result. Returns the retired member.
  fn retire_an_owner_settled_on_a_formation_neighbourhood(
    state: &mut crate::state::ShardState,
    object: slates_db::register::ObjectId,
  ) -> slates_db::HostId {
    use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
    use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, HostId, Quorum, Record};
    let local = state.fleet.host();
    let departed = HostId(local.0 ^ 1);
    let third = HostId(local.0 ^ 2);
    state.council = RegionalCouncil::new(
      local,
      vec![local],
      vec![local],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    for host in [departed, third] {
      assert!(
        state
          .council
          .propose(Reconfiguration::Admit { host, domain: None })
      );
    }
    let settled = &state.council.configuration().settled[&departed];
    assert_eq!(
      settled.hosts.len(),
      2,
      "the second member is settled beside the bootstrap alone"
    );
    install_council(state);
    let generation = state.fleet.configuration().version;
    let mut acceptor = Acceptor::new(
      local,
      Authority {
        generation,
        owner: departed,
      },
    );
    acceptor
      .accept(&Record {
        owner: departed,
        object,
        sequence: 0,
        epoch: FIRST_EPOCH,
        generation,
        value: b"head".to_vec(),
      })
      .unwrap();
    state.holder_records.insert(object, acceptor);
    state.fleet.track_object_owner(object, departed);
    assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
    install_council(state);
    crate::takeover::resolve_held_objects(state, local);
    departed
  }

  /// §4.8 "Promotion and takeover" (Flexible Paxos's intersection, research record): an owner retired while its
  /// settled neighbourhood is the two hosts a forming region admitted it beside is recovered from the one
  /// survivor of that cohort. A record committed there is held by both of its hosts, so one promise meets every
  /// commit; requiring `f + 1` live members of a two-host cohort declared the object lost while its survivor held
  /// it — the Linux io_uring loop's run 118 of 150 (2026-09-29): both survivors held the volume's head,
  /// `fleet.takeover.lost` counted it, and no survivor ever served it.
  #[test]
  fn a_takeover_recovers_an_owner_settled_beside_one_host_from_that_host() {
    use slates_db::register::ObjectId;
    let (successor, ready, lost) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let object = ObjectId([29; 16]);
      let departed = retire_an_owner_settled_on_a_formation_neighbourhood(state, object);
      let successor = state.fleet.object_owner(object);
      assert!(crate::takeover::begin_round(state, departed, local).is_some());
      let ready = crate::takeover::ready_objects(state, departed, local);
      let lost = state
        .refusals
        .get("fleet.takeover.lost")
        .copied()
        .unwrap_or(0);
      (successor == Some(local), ready, lost)
    });
    assert!(successor, "the one survivor of the cohort is the successor");
    assert_eq!(lost, 0, "a record its cohort's survivor holds is not lost");
    assert_eq!(
      ready.len(),
      1,
      "the survivor's own promise readies the object"
    );
    assert!(
      ready.iter().all(|(_, adopted)| adopted.value == b"head"),
      "the survivor adopts the record it holds"
    );
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
        name: String::new(),
        require_evidence: false,
        owner: slates_db::catalog::Principal::Uid { uid: 0 },
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
      wake_tracking: None,
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
