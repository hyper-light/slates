//! The daemon (§2.6 "Boot order" steps 2, 3 and 5): attach or create the segment, start the
//! runtime's shards, recover each shard's partition and install its state, publish the
//! profile, start the doorbell thread, and run the control shard's rendezvous; stop in
//! reverse, joining everything the daemon started.

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(unix)]
use slates_anchor::ENV_NFS_LISTENER;
use slates_anchor::{AnchorSegment, RegionKind};
use slates_db::catalog::Principal;
use slates_ipc::{Adoption, ClientRegion, Listener, Prepared};
use slates_machine::facts::Identity;
use slates_machine::{MachineProfile, derived};
use slates_mem::arena::ChunkArena;
use slates_mem::{Handoff, Slab};
use slates_rt::RtError;
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
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
      slates_machine::log_line!(
        "slates-server: a periodic loop's sleep was refused ({refusal}); the loop stops"
      );
    }
    std::future::pending::<()>().await;
  }
}

/// A perpetual loop's sleep refused ([`pace`]): a tripwire, never an expected count.
const SLEEP_REFUSED: &str = "runtime.sleep_refused";

/// Derived: the heartbeat, the fleet's coordinator period and the cadence the watchdog looks at its shards in (§4.14
/// `daemon.alive`): the liveness window over the election margin, `W / ELECTION_MARGIN` (A-129 item 1, the leader beats
/// ten times per election timeout, and item 2, the death-confirmation window is the election timeout; A-359), so nine
/// beats fit inside a window. 100 ms.
pub const HEARTBEAT_NS: u64 = heartbeat_of(LIVENESS_BUDGET_NS);

/// The heartbeat's rule over a liveness window of `liveness_budget_ns`: `W / ELECTION_MARGIN`, so nine beats fit inside
/// the window ([`HEARTBEAT_NS`] at the daemon's own window). Shared by every beat that shows a peer it is alive within
/// that peer's patience: the anchor's, and a quiet MCP listen stream's keep-alive (`slates_mcp::http`).
pub const fn heartbeat_of(liveness_budget_ns: u64) -> u64 {
  liveness_budget_ns / slates_cluster::timing::ELECTION_MARGIN
}
/// Derived: the liveness window `W` (A-228, A-359): how long a daemon, a shard or a peer may show no progress before it
/// is judged stuck or silent and its recovery begins. It is the recovery budget `T`
/// (`slates_db::replay::RECOVERY_BUDGET_NS`, ratified, GAPS §5): the verdict starts a recovery that costs `T`, so
/// waiting less risks paying `T` for a stall shorter than `T`, and waiting as long as the recovery costs is the
/// deterministic online choice within twice the best (rent or buy [A: Karlin, Manasse, Rudolph & Sleator, "Competitive
/// snoopy caching", Algorithmica 1988], the rule the wake probe's spin-then-park already follows). A second. Not
/// derived from the failover SLO: the fleet's detection and confirmation timers are multiples of this window's
/// heartbeat, so a failover SLO derived from them (Ada's rule, 2026-11-18) cannot also set it. The machine's measured
/// scheduling delay is its floor, checked and logged at boot (`config::liveness_window`).
pub const LIVENESS_BUDGET_NS: u64 = slates_db::replay::RECOVERY_BUDGET_NS;
/// How long an out-of-band observation ([`Daemon::observation`]) has, from its submission through its
/// admission to its answer, before it ends with a deadline naming the stage it reached
/// ([`crate::observe::ObserveError::Deadline`]). A live shard answers within a few coordinator periods
/// (~[`HEARTBEAT_NS`] each), but under heavy CPU load a period stretches toward a liveness budget; ample
/// headroom for a merely-slow shard to answer means an observation is not lost to a false timeout and
/// misread as a real change (a peer "left", a leader "lost"), while a genuinely wedged shard still ends
/// it — typed, so a poll tells the starvation from a fact.
/// Derived: ten liveness budgets ([`LIVENESS_BUDGET_NS`]).
pub const OBSERVE_BUDGET_NS: u64 = 10 * LIVENESS_BUDGET_NS;

/// Derived: how long test support ([`Daemon::hold_record_session`], [`Daemon::ask_live_reports`]) waits for a record
/// session that is not in its link (A-449): the span of one record-plane exchange (`fleet::record_exchange_budget_ns`,
/// the record plane's round budget over the slowest measured path, its whole span), the bound every borrower of a record
/// session runs its exchange under, so a lent session comes back within it; the span a location round gives a lent
/// session (A-437), as the supervisor ruled on 2026-11-07. One still absent after it names a real absence, which the
/// hold reports rather than waits out. Until A-449 it was one liveness window.
pub fn session_wait_ns() -> u64 {
  crate::fleet::record_exchange_budget_ns()
}

/// What [`Daemon::hold_record_session`] found: the session taken (and how long it waited for it), or, after
/// [`session_wait_ns`], why there was none — a link for the peer whose session never came back to it, or no
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

/// What [`Daemon::ask_live_reports`] left of this node's record link to a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkAfter {
  /// The link holds its session, lendable.
  Up,
  /// No link: it was dropped, and its task dials afresh.
  Absent,
  /// Its session is out, lent to the named borrower (`fleet::take_sessions`'s label).
  Lent(&'static str),
}

/// Where the segment comes from, and with it the client rendezvous: what an anchor holds for its daemons (§4.8,
/// A-114), or nothing.
#[derive(Clone, Debug)]
pub enum SegmentSource {
  /// Create a fresh segment and a fresh rendezvous (tests, a daemon run alone without an anchor). Nobody reads such a
  /// daemon's beat, so its watchdog ends its process on a stuck verdict (A-232, [`SegmentSource::supervision`]).
  Create {
    /// The object's name.
    name: String,
  },
  /// Attach the segment the anchor handed over in the environment and adopt the rendezvous it named there.
  FromEnv,
  /// Attach a segment by its handoff and adopt the rendezvous held beside it (an anchor in this process: tests,
  /// embeddings).
  Handoff {
    /// The handoff.
    handoff: Handoff,
    /// The mapped length.
    len: usize,
    /// The content object's handoff and length, if the anchor made one, so its shard content
    /// survives the restart (§4.8). `None` recreates content empty on rebuild as before (BUG-11).
    content: Option<(Handoff, usize)>,
    /// The rendezvous the anchor holds, which every daemon it starts adopts (A-114): a client that connects while
    /// one daemon stops is answered by the next, and on Windows, where a section's name lives while any client still
    /// maps it, no daemon after the first could otherwise make its own.
    rendezvous: Adoption,
  },
}

/// Derived: the CPU a [`Daemon::starve_control_shard`] hold runs each beat over `span_ns` on a shard the watchdog judges
/// by `window_ns` (its own window, A-455: the liveness window raised to the scheduler overrun the shard measured,
/// [`crate::watchdog::shard_window_ns`]): one beat times half that window over the span, at most the whole beat. So the
/// hold's whole CPU is at most half the shard's window and the watchdog, judging the shard spinning only after a whole
/// window of CPU held to the build (never less than the window), never finds it spinning, while each beat's share shows
/// CPU progress at every look, so it is never judged still either (A-228's rule). Until A-552 the share was half the
/// liveness window whatever window the watchdog judged the shard by.
pub fn starved_share_ns(span_ns: u64, window_ns: u64) -> u64 {
  HEARTBEAT_NS
    .saturating_mul(window_ns / 2)
    .checked_div(span_ns)
    .unwrap_or(HEARTBEAT_NS)
    .min(HEARTBEAT_NS)
}

/// Test support: runs the calling thread for `span_ns` as a starved thread runs, a little CPU at a time (A-232): each
/// beat it spins for its share ([`starved_share_ns`]) of the window its shard is judged by, read as the hold begins
/// ([`crate::watchdog::own_window_ns`]; a shard folds its overrun in only when it waits, and a hold inside one poll
/// waits for nothing, so the window stands for the span), and sleeps the rest, so the shard it runs on steps nothing else
/// for the span while the watchdog sees it move every look, alive and never stuck. Returns the span it measured,
/// nanoseconds. [`Daemon::starve_control_shard`] runs it on the control shard; a test that keeps another shard silent
/// past a window runs it in that shard's observation, where a sleep would be a wedge that ends a daemon no anchor
/// supervises (`tests/fleet.rs`, `a_silent_owner_leaves_no_forward_the_lane_admitted_unrun`).
pub fn starve_this_thread(span_ns: u64) -> u64 {
  let share_ns = starved_share_ns(span_ns, crate::watchdog::own_window_ns());
  let begin = slates_rt::futures::now_ns();
  let end = begin.saturating_add(span_ns);
  let mut beat_began = begin;
  while slates_rt::futures::now_ns() < end {
    let share_ends = beat_began.saturating_add(share_ns).min(end);
    while slates_rt::futures::now_ns() < share_ends {
      std::hint::spin_loop();
    }
    let beat_ends = beat_began.saturating_add(HEARTBEAT_NS).min(end);
    let rest = beat_ends.saturating_sub(slates_rt::futures::now_ns());
    // The point of the fixture: off the CPU for the rest of the beat, as a starved thread is.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_nanos(rest));
    beat_began = beat_ends;
  }
  slates_rt::futures::now_ns().saturating_sub(begin)
}

impl SegmentSource {
  /// Whether an anchor reads the beat of a daemon started over this source (A-232), which decides what the watchdog does
  /// on a stuck verdict ([`crate::watchdog::decide`]). `FromEnv` is the anchor across the spawn, which kills a daemon
  /// whose beat lapses. `Handoff` is an anchor in this process: it may read the beat, but it cannot end a stuck daemon
  /// without ending itself, so nothing would, and its watchdog ends the process as a lone daemon's does. `Create` has no
  /// anchor at all. A test that needs a supervised daemon to live on stuck plays the anchor across a spawn
  /// (`crates/server/tests/common/supervised.rs`).
  pub fn supervision(&self) -> crate::watchdog::Supervision {
    match self {
      SegmentSource::FromEnv => crate::watchdog::Supervision::Anchored,
      SegmentSource::Create { .. } | SegmentSource::Handoff { .. } => {
        crate::watchdog::Supervision::Alone
      }
    }
  }

  /// The rendezvous a daemon started over this source adopts (A-114): the one the anchor across the spawn named in the
  /// environment, the one held beside a handoff, or none for a daemon that makes its own.
  fn adoption(&self) -> Result<Option<Adoption>, ServerError> {
    Ok(match self {
      SegmentSource::Create { .. } => None,
      SegmentSource::FromEnv => Adoption::from_env()?,
      SegmentSource::Handoff { rendezvous, .. } => Some(rendezvous.clone()),
    })
  }
}

/// A held register record as a holder keeps it ([`Daemon::fleet_held_record`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRecordView {
  /// The record's position.
  pub sequence: u64,
  /// The epoch it was written under.
  pub epoch: u64,
  /// Its value.
  pub value: Vec<u8>,
  /// The epoch the holder's fence for the object has seen.
  pub fence: u64,
}

/// One peer's record dial at the read ([`Daemon::fleet_record_dials`], `crate::state::RecordDial`): how long the link has
/// been dialing for the session it lacks, how long its current handshake call and its fresh session's first discovery
/// page have run, the handshake budgets its dialer has spent and why its last ended, and the calls made since boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordDialReport {
  /// The peer the link addresses.
  pub peer: slates_db::HostId,
  /// How long the link has been dialing, while it lacks a session.
  pub dialing_for_ns: Option<u64>,
  /// How long its current handshake call has run, while one runs.
  pub call_for_ns: Option<u64>,
  /// How long its fresh session's first discovery page has waited, while it does.
  pub enroll_for_ns: Option<u64>,
  /// Handshake budgets its dialer has spent without establishing.
  pub budgets_spent: u32,
  /// Why its dialer's last budget ended, while that dialer is kept for another call.
  pub stall: Option<slates_transport::endpoint::HandshakeStall>,
  /// Handshake calls made since boot.
  pub calls: u64,
}

/// The daemon.
pub struct Daemon {
  /// Where [`Self::wait_for_stop`] hands the control shard its watch, taken in at the next heartbeat.
  stop_watch_send: std::sync::mpsc::SyncSender<StopWatch>,
  runtime: Option<Runtime>,
  segment: AnchorSegment,
  doorbell: Option<DoorbellThread>,
  /// The liveness watchdog (A-228): the beat's writer, stopped after the shards.
  watchdog: Option<crate::watchdog::Watchdog>,
  /// The writer of the shards' log lines (A-305), stopped after the shards, so it writes every line they queued.
  log_writer: Option<slates_machine::log::LogWriter>,
  config: DaemonConfig,
  shards: Vec<ShardId>,
  /// The loopback port the NFS transport (§4.6) listens on, when it is serving; `None` if the
  /// listener could not be bound. A client mounts `nfs://localhost:PORT`.
  nfs_port: Option<u16>,
  /// The port the version-control servers' HTTP edge listens on, when the configuration asked for one and it started
  /// (A-625 part 3).
  vcs_http_port: Option<u16>,
  /// The version-control servers' SSH edge, when the configuration asked for one and it started (A-684).
  vcs_ssh: Option<VcsSshEdge>,
  /// The port the svn:// listener listens on, when the configuration asked for one and it started (A-656 part 2).
  vcs_svn_port: Option<u16>,
  /// How many times the start beat before the watchdog took the beat over ([`StartBeat`]).
  start_beats: u64,
}

/// The version-control servers' SSH edge as a daemon started it (§4.17.2 "Transports" item 2; A-684): the port it
/// listens on, and its host key's fingerprint as OpenSSH prints one (`ssh-keygen -l -E sha256`, and `ssh` on a key it has
/// not seen): `SHA256:` and the key blob's SHA-256 in base64 without padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VcsSshEdge {
  /// The port.
  pub port: u16,
  /// The host key's fingerprint.
  pub fingerprint: String,
}

/// The start's beat (§2.6 boot step 2, A-312): the supervision block's heartbeat, written by the starting thread after
/// each unit of its work, until the watchdog thread takes the beat over and becomes its one writer
/// ([`crate::watchdog::Watchdog::start`]). The anchor judges a start in its own CPU time until its first beat, and kills
/// one whose thread runs a liveness window of CPU without beating; a beat is the start's progress. Until 2026-11-17
/// nothing beat until the watchdog ran, after the runtime had made a ring for every ordered pair of shards, and on B10's
/// Linux gate at load 70 the anchor killed every start of 407 shards ("ran 1009-1037 ms of CPU on one thread without
/// beating"). Each unit is microseconds, so a start that progresses keeps beating however many shards it starts; one
/// stuck in a unit stops beating, and the anchor judges its process by its CPU from the lapse (A-232). Counted, so the
/// boot log says how often a start beat.
struct StartBeat {
  clock: HostClock,
  beats: u64,
}

impl StartBeat {
  fn new() -> StartBeat {
    StartBeat {
      clock: HostClock::new(),
      beats: 0,
    }
  }

  /// Beats `supervision` now.
  fn beat(&mut self, supervision: &slates_anchor::Supervision<'_>) {
    supervision.beat(slates_vfs::clock::Clock::monotonic_ns(&mut self.clock));
    self.beats = self.beats.saturating_add(1);
  }
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
  /// The shard's idle spin window, nanoseconds, as of its last measured wake (`ShardContext::counters`); 0 until its
  /// first. Beside `wake_cost_ns` it shows whether the window follows the estimate (A-223).
  pub idle_window_ns: u64,
  /// What an idle turn costs a request that lands in it, nanoseconds, as the shard has learned it from its own spins,
  /// as of its last idle spin; 0 until its first measured turn. The shard keeps a closed loop's hold only while this is
  /// below `wake_cost_ns` (A-387, `slates_rt::held_turn::hold_pays`).
  pub held_turn_ns: u64,
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
/// small, bounded number of connections; the OS clamps the backlog to the system maximum anyway.
pub(crate) const NFS_BACKLOG: i32 = 16;

/// The NFS loopback listener to serve: the one a supervising anchor holds and hands over in the
/// environment ([`slates_anchor::ENV_NFS_LISTENER`]), so its port survives a daemon restart (§4.6) —
/// adopted here; failing that (a standalone start or tests) a fresh ephemeral bind. Adopting the
/// anchor's descriptor is the only path that keeps the port stable across restarts.
#[cfg(unix)]
fn nfs_listener() -> Result<TcpListener, RtError> {
  held_listener(ENV_NFS_LISTENER)
    .unwrap_or_else(|| TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), NFS_BACKLOG))
}

/// The listener a supervising anchor holds and handed over under the environment variable `variable` (the NFS
/// listener's, §4.6, the version-control edge's, A-625 part 3, or the svn:// listener's, A-689), adopted so its port
/// survives a daemon restart; none when no anchor handed one over (a standalone start, or tests).
#[cfg(unix)]
pub(crate) fn held_listener(variable: &str) -> Option<Result<TcpListener, RtError>> {
  let raw = std::env::var(variable)
    .ok()
    .and_then(|value| value.parse::<RawFd>().ok())?;
  // SAFETY: the anchor bound this listening socket and handed its descriptor to us across the spawn,
  // inherited at this number; ownership is ours now (the anchor keeps its own copy), so wrapping it
  // in an `OwnedFd` gives it a single owner that closes it on drop.
  let owned = unsafe { OwnedFd::from_raw_fd(raw) };
  Some(TcpListener::from_owned(owned))
}

/// See the Unix arm. On Windows the anchor shares each listener it holds with this daemon as an explicit duplicate made
/// for this process (A-748, `slates_anchor::shared_listeners`): the shares arrive on the pipe the environment names,
/// read once, and the one answering to `variable` is made into this process's listener. None where the anchor shared
/// none for it (a standalone start, tests, or an anchor that could not share it, which said so).
#[cfg(windows)]
pub(crate) fn held_listener(variable: &str) -> Option<Result<TcpListener, RtError>> {
  let share = shared_listeners()
    .iter()
    .find(|share| share.variable == variable)?;
  Some(TcpListener::from_share(&share.bytes))
}

/// The listeners the anchor shared with this daemon (A-748), read whole from the pipe [`slates_anchor::shared_listeners::
/// ENV_SHARED_LISTENERS`] names the first time a listener is asked for, and kept for the process's life. A pipe that
/// cannot be read, or whose bytes are not shares, is said on the log and read as none: each edge then binds its own.
#[cfg(windows)]
fn shared_listeners() -> &'static [slates_anchor::shared_listeners::Share] {
  /// Shape: the records a pipe may hold: one a listener the anchor holds (the HTTP edge's, the SSH edge's, svn://'s).
  const MOST_SHARES: usize = 3;
  static SHARES: std::sync::OnceLock<Vec<slates_anchor::shared_listeners::Share>> =
    std::sync::OnceLock::new();
  SHARES.get_or_init(|| {
    let Some(raw) = std::env::var(slates_anchor::shared_listeners::ENV_SHARED_LISTENERS)
      .ok()
      .and_then(|value| value.parse::<usize>().ok())
    else {
      return Vec::new();
    };
    // SAFETY: the anchor duplicated the pipe's reading end into the explicit handle list of this process's spawn and
    // named its value here (`slates_anchor::Supervisor`'s Windows spawn); nothing else in this process owns it, and it
    // is read once, in this initializer, so this is its single owner.
    let owned = unsafe {
      <std::os::windows::io::OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(
        raw as std::os::windows::io::RawHandle,
      )
    };
    let _ = slates_ipc::win_spawn::clear_inheritance(&owned);
    let mut bytes = Vec::new();
    let read = std::io::Read::read_to_end(&mut std::io::PipeReader::from(owned), &mut bytes);
    match read
      .map_err(|error| error.to_string())
      .and_then(|_| {
        slates_anchor::shared_listeners::decode(&bytes, MOST_SHARES)
          .map_err(|refusal| format!("{refusal:?}"))
      }) {
      Ok(shares) => shares,
      Err(why) => {
        slates_machine::log_line!(
          "slates daemon: the anchor's shared listeners could not be read ({why}); each edge binds its own"
        );
        Vec::new()
      }
    }
  })
}

/// Binds the version-control servers' shared HTTP edge's listener at `address` as the daemon binds its own, its backlog
/// the edge's seats on a shard of `config` (A-625 part 3): what a supervising anchor binds once and holds across its
/// daemons' restarts ([`slates_anchor::ENV_VCS_HTTP`]), so the edge's port never drops while a daemon restarts.
pub fn bind_vcs_http_listener(
  config: &DaemonConfig,
  address: SocketAddrV4,
) -> Result<TcpListener, RtError> {
  crate::vcs_http::bind(config, address)
}

/// Binds the version-control servers' SSH edge's listener at `address` as the daemon binds its own, its backlog the
/// edge's seats on a shard of `config` (A-684): what a supervising anchor binds once and holds across its daemons'
/// restarts ([`slates_anchor::ENV_VCS_SSH`]), beside the host key it keeps in the segment.
pub fn bind_vcs_ssh_listener(
  config: &DaemonConfig,
  address: SocketAddrV4,
) -> Result<TcpListener, RtError> {
  crate::vcs_ssh::bind(config, address)
}

/// Binds the svn:// listener at `address` as the daemon binds its own, its backlog the listener's seats on a shard of
/// `config` (A-689): what a supervising anchor binds once and holds across its daemons' restarts
/// ([`slates_anchor::ENV_VCS_SVN`]), so the listener's port never drops while a daemon restarts.
pub fn bind_vcs_svn_listener(
  config: &DaemonConfig,
  address: SocketAddrV4,
) -> Result<TcpListener, RtError> {
  crate::vcs_svn::bind(config, address)
}

/// A version-control server's listener (the HTTP edge's, A-625 part 3, the SSH edge's, A-684, or the svn:// listener's,
/// A-689): the one a supervising anchor holds and handed over under `variable`, adopted so the port never drops while a
/// daemon restarts (a client that connects then waits in its queue), when it listens where the configuration's
/// `address` says ([`listens_at`]); else, with none handed over, one bound here at `address` with `backlog`. Why there is
/// none, otherwise: a held listener elsewhere is refused, never served, since the server would then answer somewhere the
/// operator did not ask.
pub(crate) fn held_or_bound_listener(
  variable: &str,
  address: SocketAddrV4,
  backlog: i32,
) -> Result<TcpListener, String> {
  match held_listener(variable) {
    Some(Ok(held)) => {
      let at = held
        .local_addr()
        .map_err(|error| format!("the anchor's listener's address is unread: {error}"))?;
      if listens_at(at, address) {
        Ok(held)
      } else {
        Err(format!(
          "the anchor's listener is at {at}, not at {address}"
        ))
      }
    }
    Some(Err(error)) => Err(format!("the anchor's listener was not adopted: {error}")),
    None => TcpListener::bind(address, backlog)
      .map_err(|error| format!("{address} did not bind: {error}")),
  }
}

/// Whether a listener bound at `at` listens where `address` says: at its address, and at its port unless it asks for
/// any (port 0, the system's pick, which an anchor makes once for every daemon it starts).
pub(crate) fn listens_at(at: SocketAddrV4, address: SocketAddrV4) -> bool {
  at.ip() == address.ip() && (address.port() == 0 || at.port() == address.port())
}

/// The NFS loopback listener to serve on Windows: a fresh ephemeral bind, since no anchor holds one there across a
/// restart (`docs/wip/platforms.md`, gap 2), so its port changes with each daemon. It serves the network export
/// (`slates export`) to the clients that reach this host's loopback: Docker Desktop's Linux machine through
/// `host.docker.internal`, and WSL2 in its mirrored networking mode.
#[cfg(windows)]
fn nfs_listener() -> Result<TcpListener, RtError> {
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
/// Accept rounds the rendezvous itself ended early (an `accept` the kernel refused, a claim table that could
/// not be read): counted here and logged once; the round keeps the clients it had accepted, and the next
/// round serves what is still pending.
pub static ACCEPTS_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Claims the rendezvous refused for a reason other than the client bound or another uid (a peer's socket
/// refusing mid-handoff, a region that could not be created), each answered to its claimant: counted here
/// and logged once. Since A-176 such a refusal ends that claim alone; before, it failed its whole accept
/// round, and the clients the round had already accepted and answered were dropped unserved.
pub static CLAIMS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    slates_machine::log_line!(
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
      slates_machine::log_line!(
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
/// (banned item 9); a shard's serve loop and idle sweep are a typed initialization failure instead
/// (`init_shard`), since a shard without them serves nothing.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const LOOP_SPAWN_REFUSED: &str = "daemon.loop_spawn";
/// Format: the status refusal under which the rendezvous loop counts a pending handshake's readiness wait the driver
/// refused (§4.7, A-187): that handshake is then served at the earliest handshake's deadline rather than on its
/// opening's arrival.
#[cfg(target_os = "linux")]
const HANDSHAKE_WAIT_REFUSED: &str = "daemon.handshake_wait";
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

/// Clients found dead and reclaimed: retirements the reaper finished after their hang-up (a health signal; T-2.3, A-475).
pub static CLIENTS_REAPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The loopback port the NFS transport serves on (§4.6), 0 until the listener is bound. Published as
/// a process-global word (like the health signals) so a verb handler on any shard can report it to a
/// client — `slates mount` reads it to run `mount_nfs localhost:PORT`.
pub static NFS_PORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Whether the daemon's Shards stream route bound its socket (macOS, A-161; `start_shards_route`), so a Shards attach can
/// hand its client the stream end: process-global, as [`NFS_PORT`] is, since a production process runs one daemon. Set
/// once its listener binds; never set elsewhere.
pub static SHARDS_ROUTE_BOUND: std::sync::atomic::AtomicBool =
  std::sync::atomic::AtomicBool::new(false);
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
  /// This node's certificate anchor (A-237): the host its accounts' principals name, stable across its restarts, as
  /// `slates status` prints it. A share from another node names this node's accounts by it.
  pub fn anchor(&self) -> Result<u64, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.origin_anchor.0)
  }

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

  /// Who this node knows to vote in its region's council now ([`RegionalCouncil::known_voters`]): on a learner, the voters
  /// its configuration records (A-380). A one-shot control-shard observation.
  ///
  /// [`RegionalCouncil::known_voters`]: slates_cluster::config_group::RegionalCouncil::known_voters
  pub fn council_known_voters(&self) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.council.known_voters())
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

  /// The observability gate (§2.6, §4.14): refused, naming what is missing, unless every chokepoint span has
  /// registered its emitter.
  fn chokepoints_ready() -> Result<(), ServerError> {
    let chokepoints = registered_chokepoints();
    if chokepoints.is_ready() {
      return Ok(());
    }
    Err(ServerError::ChokepointsUnregistered {
      missing: chokepoints
        .missing()
        .into_iter()
        .map(Chokepoint::name)
        .collect(),
    })
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
    Self::chokepoints_ready()?;
    // This daemon's own mounts (A-227), recorded as each is made, so no daemon thread opens a path on one; room for one
    // a live attachment the shards can hold, the bound the anchor's device holds take.
    slates_base::own_mounts::install(config.held_device_bound()).map_err(|error| {
      // The code as the host named it, typed by its numbering (A-539).
      ServerError::Vfs(slates_vfs::error::VfsError::BaseUnavailable(error.code()))
    })?;
    let identity = profile.facts.identity.clone();
    let supervision = source.supervision();
    // The rendezvous comes from wherever the segment does (A-114): an anchor across the spawn names both in the
    // environment, an anchor in this process hands both over, and a daemon with no anchor makes its own.
    let adoption = source.adoption()?;
    // The owner first (A-254, the rule of A-176 and A-178): a second daemon of a live instance is refused here, having
    // created nothing. On macOS a segment's creation unlinks any holder of its name and its drop unlinks the name again,
    // so a refused start that had created one took the live daemon's name with it. Every Linux rendezvous is sealed
    // (§4.7, A-187), so the seal is installed before the listener can answer a claim; a second daemon in this process
    // installs the same one.
    #[cfg(target_os = "linux")]
    slates_seal::install();
    let listener = Listener::open(&config.instance, adoption.as_ref())?;
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
        rendezvous: _,
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
    // The start beats from here until the watchdog takes the beat over (§2.6 boot step 2, A-312; [`StartBeat`]).
    let mut start_beat = StartBeat::new();
    start_beat.beat(&segment.supervision()?);
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
    let region_names = region_name_key(&issuer_secret);
    // The client session secret (`crate::session_key`): adopted from the daemon before this one under the anchor, or
    // made and published by the first, so a client's session key still opens its session after a restart.
    crate::session_key::adopt_or_mint(&mut segment)?;
    // A full crash dump an administrator's setting would write of this daemon (A-239): read and logged once at start.
    let _ = crate::local_dumps::full();
    // The node's sealing root (A-92): adopted from the previous daemon under this anchor, or made and published into
    // the anchor's locked page; sealing reports itself unavailable, never unlocked, where the OS refuses the lock.
    let sealing = crate::seal_keys::init(&mut segment, config.seal_key_slots);
    if let Some((was, now)) = limits::raise_descriptor_limit() {
      slates_machine::log_line!("slates-server: descriptor limit raised from {was} to {now}");
    }
    let retained = crate::retention::load(&segment)?;
    // The writer every shard's log lines go through (A-305): a shard never waits on standard error, where a thread
    // starved while it held the lock every line takes, or a reader slow to take a line, held a shard a window and the
    // watchdog called it wedged. Started before the shards, stopped after them.
    let log_writer = slates_machine::log::LogWriter::start().map_err(|error| {
      ServerError::Ipc(slates_ipc::IpcError::OsRefused {
        call: "spawn the log writer thread",
        code: error.raw_os_error(),
      })
    })?;
    // The block the start beats, held across the runtime's start (`supervision` above says who reads the beat).
    let beat_block = segment.supervision()?;
    start_beat.beat(&beat_block);
    // The control shard first, in start order (`shards.first()`): sized for every forward its lane admits. The start
    // beats after each unit of the runtime's work, its pair rings above all, N(N-1) of them (`slates_rt::runtime`).
    let runtime = Runtime::start_sized(
      &config.runtime,
      &[config.control_shard_capacity().get()],
      &mut || start_beat.beat(&beat_block),
    )?;
    let shards: Vec<ShardId> = runtime.shard_ids().to_vec();
    // The lock ledger's entry for this daemon starts empty, before any shard recovers and reserves again.
    crate::lock_ledger::reset(shards.first().map_or(0, |shard| shard.0));
    // What the anchor held across the restart (A-61, A-113): the devices (FUSE devices on Linux, Shards streams' resync
    // ends on Linux and macOS, A-316, A-339), each moved to the shard that owns its attachment, and the NFS loopback
    // connections, served again on the listener's shard below. Each platform reads the handoff table against every
    // entry its anchor may hold, as the configuration derives the anchor's bounds (A-448, A-460, A-462).
    let handed = crate::anchor_hold::adopt(&config);
    #[cfg(unix)]
    let mut inherited_devices = crate::fuse_hold::split(handed.devices, shards.len()).into_iter();
    // The held copies: the Shards streams' DAX objects (A-365; Windows' sections, A-448), each moved to the shard that
    // owns its attachment; and the writable direct maps' copies (A-197), each to the shard that holds its map, which
    // Linux alone sends the anchor.
    let (other_copies, inherited_dax) = crate::shards::dax_hold::split(handed.copies, shards.len());
    let mut inherited_dax = inherited_dax.into_iter();
    // The NFS send ledgers (A-550, Linux), each judging the held connections on the listener's shard below.
    #[cfg(target_os = "linux")]
    let (other_copies, inherited_ledgers) = crate::held_send::split_ledgers(other_copies);
    #[cfg(not(target_os = "linux"))]
    let inherited_ledgers = crate::held_send::Inherited::default();
    #[cfg(target_os = "linux")]
    let mut inherited_copies = crate::held_copy::split(other_copies, shards.len()).into_iter();
    #[cfg(all(unix, not(target_os = "linux")))]
    for copy in other_copies {
      // No other kind of copy is held here: one is let go at once, its record no shard's.
      let _released = crate::anchor_hold::release_copy(copy.copy);
    }
    // No other kind of copy is held on Windows: a section whose record is no DAX record is an entry no shard adopts,
    // which the control shard releases with the refused mounts (its release is an exchange only a shard can make).
    #[cfg(windows)]
    let mut held_mounts = handed.mounts;
    #[cfg(windows)]
    held_mounts.extend(
      other_copies
        .into_iter()
        .map(|copy| crate::anchor_hold::HandedMount {
          attachment: copy.copy,
          handles: Vec::new(),
          record: copy.record,
          adoptable: false,
        }),
    );
    #[cfg(windows)]
    let mut inherited_winfsp = crate::winfsp::split_held(held_mounts, shards.len()).into_iter();
    // The Shards streams' resync ends (A-433), each to the shard that owns its attachment.
    #[cfg(windows)]
    let mut inherited_resync =
      crate::shards::split_held_resync(handed.resync_ends, shards.len()).into_iter();
    #[cfg(windows)]
    let mut windows_channel = handed.channel;
    #[cfg(windows)]
    configure_windows_hold_owner(windows_channel.is_some(), &shards)?;
    let handed_connections = handed.connections;
    // This daemon's learning gate (A-227): its shards' crossing checks wait on its own shards' learning alone, while it
    // goes on (A-552): a check gives the learning its own shard's window (the one its watchdog judges it by, A-455) to
    // end one more shard's, and parks a beat between looks, so its watchdog sees it move at every look of its own.
    #[cfg(unix)]
    let gate = slates_base::own_mounts::Gate::new(
      shards.len(),
      crate::watchdog::own_window_ns,
      HEARTBEAT_NS,
    );
    // The process thread's stop watch reaches the control shard over this channel, not as a question the shard must
    // answer: a shard busy with recovered work answers late, and a watch refused for it stopped the daemon, whose anchor
    // restarted it into the same work (w-interop's restart loop, 2026-11-20). One slot: one process thread waits.
    let (stop_watch_send, stop_watch_install) = std::sync::mpsc::sync_channel::<StopWatch>(1);
    let mut stop_watch_install = Some(stop_watch_install);
    for (index, shard) in shards.iter().enumerate() {
      let env = segment.handoff_env()?;
      let config = config.clone();
      let identity = identity.clone();
      let partition = u16::try_from(index).unwrap_or(u16::MAX);
      let all: Vec<u16> = shards.iter().map(|s| s.0).collect();
      let retained = retained.clone();
      // The stop watch's channel goes to the control shard, in its state before anything runs on the shard.
      let stop_watch = if index == 0 {
        stop_watch_install.take()
      } else {
        None
      };
      #[cfg(unix)]
      let holds = ShardHolds {
        stop_watch,
        devices: inherited_devices.next().unwrap_or_default(),
        #[cfg(target_os = "linux")]
        copies: inherited_copies.next().unwrap_or_default(),
        dax: inherited_dax.next().unwrap_or_default(),
      };
      #[cfg(windows)]
      let holds = ShardHolds {
        stop_watch,
        winfsp: inherited_winfsp.next().unwrap_or_default(),
        channel: if index == 0 {
          windows_channel.take()
        } else {
          None
        },
        resync: inherited_resync.next().unwrap_or_default(),
        dax: inherited_dax.next().unwrap_or_default(),
      };
      // This shard's learning of the mounts it keeps, taken before it starts so none of the daemon's crossings is
      // judged before the shard has recorded them (A-227). A Linux shard the anchor handed no FUSE device keeps no mount
      // that could wait on it (a kept mount with no device answers `ENOTCONN`), so it counts as learned at once. A
      // macOS shard learns its kept NFS mounts from its catalog, which it reads only as it starts.
      #[cfg(target_os = "linux")]
      let learning = (!holds.devices.is_empty()).then(|| gate.learning());
      #[cfg(target_os = "macos")]
      let learning = Some(gate.learning());
      #[cfg(windows)]
      let learning = ();
      let log_route = log_writer.route();
      runtime.spawn_on(*shard, async move {
        // Before anything this shard logs: its lines go to the daemon's log writer from here on (A-305).
        if let Some(route) = log_route {
          route.install();
        }
        // This daemon's learning gate is entered on the shard's own thread before its start (A-227).
        #[cfg(unix)]
        gate.enter();
        // The start runs alone on its shard (§2.6 boot step 2): nothing else the shard is sent runs until it ends, as
        // behind one long poll, while its recovery yields between slices so the shard keeps stepping (A-228). Built
        // here, on its shard, since it owns the shard's state before installing it, which never moves between threads.
        let start = futures::spawn_alone(start_shard(
          config,
          env,
          identity,
          (partition, sealing, learning),
          all,
          retained,
          holds,
        ));
        if let Err(e) = start.and_then(futures::detach) {
          INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
          slates_machine::log_line!(
            "slates-server: shard {partition}'s start could not be seated: {e}"
          );
        }
      })?;
      start_beat.beat(&beat_block);
    }
    // The doorbell, on the rendezvous opened above: each shard's kick by the id a client's region names it by (A-381),
    // the control shard first.
    let kicks: Vec<(u16, slates_rt::driver::Kick)> = shards
      .iter()
      .filter_map(|s| registry::with_entry(s.0, |e| (s.0, e.kick)))
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
    // The beat's writer (A-228): an owned thread over its own mapping of the segment, judging every shard by its own CPU
    // clock; the control loop's life is the sender it holds, so a refused boot still stops the beat (A-100).
    let (control_life, control_alive) = std::sync::mpsc::channel::<()>();
    let (watch_handoff, watch_len) = handoff_of(&control_env)?;
    let holders: Vec<slates_rt::SlotHolder> = shards
      .iter()
      .filter_map(|shard| registry::holder_of(shard.0))
      .collect();
    // The start's last beat, then the watchdog's thread takes the beat over: one writer at a time.
    start_beat.beat(&beat_block);
    slates_machine::log_line!(
      "slates-server: the start beat {} times over {} shards before the watchdog took the beat over",
      start_beat.beats,
      shards.len()
    );
    let watchdog = crate::watchdog::Watchdog::start(
      AnchorSegment::attach(&watch_handoff, watch_len, &identity)?,
      holders,
      control_alive,
      supervision,
    )
    .map_err(|error| {
      ServerError::Ipc(slates_ipc::IpcError::OsRefused {
        call: "spawn the watchdog thread",
        code: error.raw_os_error(),
      })
    })?;
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
        region_names,
        control_life,
      ));
      // Construct the control future on its owning shard: recovery owns thread-local
      // cross-shard reply registrations, which must never migrate between threads.
      if task.and_then(futures::detach).is_err() {
        INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
        slates_machine::log_line!("slates-server: the control loop could not be seated");
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
    let nfs_port = start_nfs_transport(
      &runtime,
      control,
      &config,
      handed_connections,
      inherited_ledgers,
    )?;
    #[cfg(target_os = "macos")]
    start_shards_route(&runtime, control, &config)?;
    // The version-control servers' HTTP edge, when the operator asked for one (§4.17.2 "Transports" item 1, A-625 part 3):
    // its listener on the control shard beside the NFS transport, every shard keeping its seats.
    let vcs_http_port = crate::vcs_http::start(&runtime, control, &shards, &config)?;
    // And its SSH edge (§4.17.2 "Transports" item 2, A-684): its listener on the control shard, every shard seating the
    // connections that move to it; its host key the fleet node's own, else the one the anchor keeps in the segment, else
    // this lone daemon's own.
    let vcs_ssh = crate::vcs_ssh::start(&runtime, control, &shards, &config, &segment)?;
    // The svn:// listener, when the operator asked for one (§4.17.4 piece 7, A-689, A-695): on the control shard too,
    // every shard keeping its seats for the connections that move to their repository's owner.
    let vcs_svn_port = crate::vcs_svn::start(&runtime, control, &shards, &config)?;
    Ok(Daemon {
      stop_watch_send,
      runtime: Some(runtime),
      segment,
      doorbell,
      watchdog: Some(watchdog),
      log_writer: Some(log_writer),
      config,
      shards,
      nfs_port,
      vcs_http_port,
      vcs_ssh,
      vcs_svn_port,
      start_beats: start_beat.beats,
    })
  }

  /// The loopback port the NFS transport (§4.6) is serving on, if the listener bound; a client mounts
  /// `nfs://localhost:PORT` to reach this daemon's volumes (those on its accepting shard, R8).
  pub fn nfs_port(&self) -> Option<u16> {
    self.nfs_port
  }

  /// The port the version-control servers' HTTP edge listens on (§4.17.2 "Transports" item 1, A-625 part 3), when the
  /// configuration asked for one and it started.
  pub fn vcs_http_port(&self) -> Option<u16> {
    self.vcs_http_port
  }

  /// The version-control servers' SSH edge (§4.17.2 "Transports" item 2, A-684), when the configuration asked for one
  /// and it started: its port and its host key's fingerprint.
  pub fn vcs_ssh(&self) -> Option<&VcsSshEdge> {
    self.vcs_ssh.as_ref()
  }

  /// The port the svn:// listener listens on (§4.17.4 piece 7, A-656 part 2), when the configuration asked for one and it
  /// started.
  pub fn vcs_svn_port(&self) -> Option<u16> {
    self.vcs_svn_port
  }

  /// How many times this daemon's start beat before its watchdog took the beat over (§2.6 boot step 2, A-312): at least
  /// once a unit of the start's work, so at least once for every ordered pair of its shards.
  pub fn start_beats(&self) -> u64 {
    self.start_beats
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

  /// Runs `question` on this daemon's shard at `target` and returns its answer once the shard runs it, while the shard
  /// lives (A-232's rule; `Observation::patient`, looked at every liveness window): however long the shard is busy,
  /// starved or held alone by other work, only its going ends the wait, and a stuck shard is its watchdog's to end. Until
  /// 2026-11-20 the answer was waited for within [`OBSERVE_BUDGET_NS`], ten liveness windows, and a control shard held
  /// alone by recovered work past that budget's CPU refused every default observation, the stop watch's install among
  /// them (w-interop's restart loop). That budget was **generous on purpose**: under noisy,
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
      .pending_patient(target, move || state::try_with_state(question))?
      .wait()
  }

  /// Records `point` as the root mount this daemon published (§4.6, D-1; A-645 P3) on every shard: the point a hook is
  /// handed its repository's view beneath (`ShardState::root_mount`). What the root mount's publication calls once the
  /// mount is made (A-645 (a), owed); until a daemon has published one, a configured hook is refused, typed, for git
  /// and Subversion alike. Refused where a shard does not answer.
  pub fn record_root_mount(&self, point: &std::path::Path) -> Result<(), ObserveError> {
    for shard in &self.shards {
      let point = point.to_path_buf();
      self.observe(Some(*shard), move |state| {
        state.root_mount = Some(point.clone());
      })?;
    }
    Ok(())
  }

  /// Like [`Self::observe`] for a question of the shard's runtime context rather than of its state (a
  /// task count), answered the same way.
  fn observe_shard<T, Q>(&self, target: Option<ShardId>, question: Q) -> Result<T, ObserveError>
  where
    T: Send + 'static,
    Q: FnOnce(&slates_rt::shard::ShardContext) -> T + Clone + Send + 'static,
  {
    self
      .pending_patient(target, move || {
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
  /// The observation of `question` at `target` that waits while the shard lives (`Observation::patient`), looking every
  /// liveness window: the default observations' form ([`Self::observe`]).
  fn pending_patient<T, Q>(
    &self,
    target: Option<ShardId>,
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
    Ok(Observation::patient(holder, LIVENESS_BUDGET_NS, question))
  }

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
    self.flood_control_channel_of(0)
  }

  /// Test support: [`Self::flood_control_channel`] on the shard at `partition` (A-216's refused release: a spawn sent to
  /// a full control channel is refused `ControlFull`). The flood clears by itself once that shard runs.
  pub fn flood_control_channel_of(&self, partition: usize) -> Result<usize, ObserveError> {
    self.flood_with(partition, || async {})
  }

  /// Test support: [`Self::flood_control_channel_of`], each queued task adding one to `ran` when the shard runs it. The
  /// shard runs its channel's messages in the order it took them, so `ran` is its progress through the flood, and a
  /// message sent once the channel has room runs after the flood's last task (a whole channel: 41,125 tasks on an
  /// 18-CPU Mac, drained in 64 ms to 4.33 s under 36 spinning threads, 2026-11-17).
  pub fn flood_control_channel_counting(
    &self,
    partition: usize,
    ran: &'static AtomicU64,
  ) -> Result<usize, ObserveError> {
    self.flood_with(partition, move || async move {
      ran.fetch_add(1, Ordering::Relaxed);
    })
  }

  /// Spawns `task()` on the shard at `partition` until its control channel refuses one as full; how many were queued.
  fn flood_with<F: Future<Output = ()> + Send + 'static>(
    &self,
    partition: usize,
    task: impl Fn() -> F,
  ) -> Result<usize, ObserveError> {
    let shard = self
      .shards
      .get(partition)
      .copied()
      .ok_or(ObserveError::NoTarget)?;
    let runtime = self.runtime.as_ref().ok_or(ObserveError::NoRuntime)?;
    let mut queued: usize = 0;
    loop {
      match runtime.spawn_on(shard, task()) {
        Ok(()) => queued = queued.saturating_add(1),
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

  /// Test support: makes this node forget `object`'s routing and its retired owner's record on its control shard, as a
  /// holder restored from a warm restart did before A-250
  /// (`docs/bugs/2026-11-07-a-holder-restarted-warm-lost-its-copies-routing.md`): its takeover pages, and as a successor
  /// its own learning, leave the copy out, and only a question by name (A-251) reaches it. `Ok` once done, else the
  /// typed refusal.
  pub fn forget_object_routing(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.fleet.forget_object(object);
      s.departed_owners.remove(&object);
    })
  }

  /// Test support: the highest ledger position this node holds of `object`, read from its own records with no routing
  /// in the way; `None` when it holds none. The typed refusal when the control shard could not answer.
  pub fn held_position(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<u64>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let (_, positions) = s.holder_records.get(&object)?.persisted();
      positions
        .into_iter()
        .filter(|(held, _, _, _)| *held == object)
        .map(|(_, sequence, _, _)| sequence)
        .max()
    })
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

  /// This node's council configuration fetches as a learner (`slates_ipc::protocol::LearnerReport`): the version it
  /// holds, the version after its last fetch, how many members that fetch asked and how long ago. A one-shot
  /// control-shard observation.
  pub fn learner_fetches(&self) -> Result<slates_ipc::protocol::LearnerReport, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      crate::verbs::learner_report(s)
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
  /// at the fleet's poll interval, for at most [`session_wait_ns`], as a forward waits for it
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
          let sessions =
            crate::fleet::take_sessions("fleet.lent_to.hold_record_session", |host| host == peer);
          if !sessions.is_empty() {
            break (sessions, SessionHold::Took { waited_ns });
          }
          if waited_ns >= session_wait_ns() {
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

  /// Test support: asks `peers` for a live log's report `rounds` times, as a takeover's phase one does
  /// (`live_ship::ask_reports`), each under a budget of `budget_ns`, then waits `settle_ns` (a record period or more,
  /// so queued stragglers are drained) and reports what is left of this node's record link to each peer
  /// ([`LinkAfter`]), with how many rounds found a peer's session in its link to ask over. A round waits for the
  /// session as [`Self::hold_record_session`] does, for at most the longer of [`session_wait_ns`] and `settle_ns`, since
  /// a link an earlier round dropped is dialed afresh meanwhile; a round that finds none ends the asking. Runs on the control shard, where the record sessions live. A budget too short for any reply makes each
  /// question unanswered, as a reply past the liveness budget is on a lossy path (KIND's wan-loss profile, CI
  /// diag-raft-chaos-final-41).
  pub fn ask_live_reports(
    &self,
    peers: Vec<slates_db::HostId>,
    budget_ns: u64,
    rounds: usize,
    settle_ns: u64,
  ) -> Result<(usize, Vec<(slates_db::HostId, LinkAfter)>), ObserveError> {
    let shard = self.shards.first().copied().ok_or(ObserveError::NoTarget)?;
    let runtime = self.runtime.as_ref().ok_or(ObserveError::NoRuntime)?;
    let (told, answer) =
      std::sync::mpsc::sync_channel::<(usize, Vec<(slates_db::HostId, LinkAfter)>)>(1);
    let receipt = runtime
      .spawn_on_with_receipt(shard, async move {
        let poll_ns = (HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD).max(1);
        // A link dropped by an earlier round is dialed afresh by its task, which can take longer than a session wait.
        let session_wait_ns = session_wait_ns().max(settle_ns);
        let mut asked = 0usize;
        for _ in 0..rounds {
          let began = slates_rt::futures::now_ns();
          let lendable = loop {
            let lendable = crate::state::with_state(|s| {
              peers.iter().any(|peer| {
                s.record_sessions
                  .get(peer)
                  .is_some_and(|link| link.endpoint.is_some())
              })
            })
            .unwrap_or(false);
            if lendable || slates_rt::futures::now_ns().saturating_sub(began) >= session_wait_ns {
              break lendable;
            }
            pace(poll_ns).await;
          };
          if !lendable {
            break;
          }
          let budget = slates_cluster::CommitBudget::hard(budget_ns, poll_ns);
          let object = slates_db::register::ObjectId([0; 16]);
          let _ = crate::live_ship::ask_reports(
            object,
            slates_db::register::HostEpoch(1),
            &peers,
            budget,
          )
          .await;
          asked = asked.saturating_add(1);
        }
        pace(settle_ns).await;
        let after = crate::state::with_state(|s| {
          peers
            .iter()
            .map(|peer| {
              let link = match s.record_sessions.get(peer) {
                None => LinkAfter::Absent,
                Some(link) if link.endpoint.is_some() => LinkAfter::Up,
                Some(link) => LinkAfter::Lent(link.lent_to.unwrap_or("none")),
              };
              (*peer, link)
            })
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        let _ = told.send((asked, after));
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
    // Every round's session wait and question, and the settle, on top of the observe budget.
    let rounds_ns = u64::try_from(rounds)
      .unwrap_or(u64::MAX)
      .saturating_mul(session_wait_ns().max(settle_ns).saturating_add(budget_ns));
    let bound_ns = OBSERVE_BUDGET_NS
      .saturating_add(rounds_ns)
      .saturating_add(settle_ns);
    answer
      .recv_timeout(std::time::Duration::from_nanos(bound_ns))
      .map_err(|_| ObserveError::Deadline {
        stage: ObserveStage::Execution,
        budget_ns: bound_ns,
        attempts: 1,
        waited_ns: bound_ns,
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
    for _ in 0..self
      .config
      .control_shard_capacity()
      .get()
      .tasks
      .saturating_mul(2)
    {
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
          fill.admitted = fill.admitted.saturating_add(1);
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

  /// Each shard's own CPU time now, in nanoseconds (§4.14; `slates_rt::registry::shard_cpu`): what its thread has run,
  /// read from any thread. Over a wait it tells a shard held in its own work (its time moves with the wall) from one
  /// off the CPU (its time barely moves); which of preempted and blocked in a call the latter was, the clock cannot
  /// tell, and `slates_rt::registry::shard_run_state` sampled over the wait does (macOS). In shard order; a shard whose
  /// thread recorded no clock is omitted.
  pub fn shard_cpu_ns(&self) -> Vec<(u16, u64)> {
    self
      .shards
      .iter()
      .filter_map(|shard| {
        registry::holder_of(shard.0)
          .and_then(registry::shard_cpu)
          .map(|reading| (shard.0, reading.spent_ns))
      })
      .collect()
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
          idle_window_ns: entry.pulse.idle_window_ns(),
          held_turn_ns: entry.pulse.held_turn_ns(),
        })
      })
      .collect()
  }

  /// The same-kernel direct maps' copies this daemon holds, on every shard, and their bytes (§4.6, A-171): what a
  /// release, a reap or a refused share leaves, for a test or an operator to hold to its starting value. An
  /// observation the daemon could not make is its typed refusal.
  pub fn direct_map_copies(&self) -> Result<(usize, u64), ObserveError> {
    let mut held = (0usize, 0u64);
    for shard in self.shards.iter().copied() {
      let (copies, bytes) = self.observe(Some(shard), |s| s.store.shards_backings.peers_held())?;
      held = (held.0.saturating_add(copies), held.1.saturating_add(bytes));
    }
    Ok(held)
  }

  /// The content keys this daemon's shards hold, summed (A-208): one for each volume alive and each key a sealed chunk
  /// still names, so a create-and-destroy loop returns it to where it started. An observation the daemon could not make
  /// is its typed refusal.
  pub fn content_keys_held(&self) -> Result<usize, ObserveError> {
    let mut held = 0usize;
    for shard in self.shards.iter().copied() {
      held =
        held.saturating_add(self.observe(Some(shard), |s| s.store.content.cipher_keys_held())?);
    }
    Ok(held)
  }

  /// The VMMs' DAX backings this daemon holds, on every shard, and what every backing counts against the shards' fill
  /// budgets (§4.6, A-163, A-180): what a stream's end lets go, for a test or an operator to hold to its starting value.
  /// An observation the daemon could not make is its typed refusal.
  pub fn dax_backings_held(&self) -> Result<(usize, u64), ObserveError> {
    let mut held = (0usize, 0u64);
    for shard in self.shards.iter().copied() {
      let (backings, charged) =
        self.observe(Some(shard), |s| s.store.shards_backings.vmms_held())?;
      held = (
        held.0.saturating_add(backings),
        held.1.saturating_add(charged),
      );
    }
    Ok(held)
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

  /// The bound one record-plane exchange runs under on the control shard now (`fleet::record_exchange_budget_ns`):
  /// derived from the slowest measured path tail, so it grows with load. A test that waits for an exchange to end at
  /// its deadline reads it, rather than assuming its own wait outlasts it (2026-10-10: an interrupted discovery
  /// exchange that ended at neither an invalidation nor a deadline inside a 30 s wait).
  pub fn record_exchange_budget_ns(&self) -> Result<u64, ObserveError> {
    self.observe(self.shards.first().copied(), |_| {
      crate::fleet::record_exchange_budget_ns()
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

  /// Why this node keeps or drops direct contact with each other member it holds (`fleet::keeps_direct_contact_with`'s
  /// rule, named per peer), with the plane's facts about the pair: whether an epoch keys it (and the peer joined the
  /// detector), and whether this node is the pair's canonical dialer, the lower anchor, which announces its epoch. For a
  /// formation failure's report, so a pair that never meshes says which side dropped the other, why, and whether the
  /// pair was ever keyed (2026-10-10: the next occurrence is to say whether an idle canonical dialer left a pair
  /// unkeyed).
  pub fn fleet_contacts(&self) -> Result<Vec<(slates_db::HostId, String)>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      let host = s.fleet.host();
      s.fleet
        .members()
        .iter()
        .filter(|&&peer| peer != host)
        .map(|&peer| {
          let keyed = s.plane.keyed.get(&peer).copied();
          let canonical =
            crate::member_task::anchor_of(s, peer).map(|anchor| s.origin_anchor < anchor);
          (
            peer,
            format!(
              "{} keyed={keyed:?} canonical={canonical:?}",
              crate::fleet::direct_contact_reason(s, peer)
            ),
          )
        })
        .collect()
    })
  }

  /// Whether this daemon's fleet has formed its **direct probe mesh** (§4.8, [`crate::fleet::ProbeMesh`]): every
  /// member held alive that this node keeps direct contact with — its record neighbourhood and its council and root
  /// voters (`fleet::keeps_direct_contact_with`) — has a live probe session: the handshake completed and the peer was
  /// recorded in `formed_probe_peers`. Unlike [`fleet_members`](Daemon::fleet_members), which reads the
  /// membership's optimistically **seeded** alive set (every configured peer is believed alive from boot,
  /// before any is contacted), this reflects sessions that have actually formed. A formation observer
  /// waits on this so it does not act on a fleet whose mesh is not yet up — for instance retiring a node
  /// that dies before its peers ever probed it, which no survivor could then detect; and it must cover the
  /// consensus voters outside the copyset, or a test proceeds while those probe sessions are still
  /// establishing under load. A laptop (no peers) is trivially meshed. Runs a one-shot question on the
  /// control shard; an observation the daemon could not make is its typed refusal ([`Self::observation`]),
  /// which a caller must treat as *unknown*, never as "not meshed".
  pub fn fleet_meshed(&self) -> Result<bool, ObserveError> {
    // The peers to reach are every member held alive that this node keeps direct contact with, less this node
    // (`fleet::probe_mesh`, the predicate status reports); the mesh is up when every one has a formed probe session
    // and, in a fleet of several, at least one is in contact. A laptop is meshed at once. Judged over the
    // configuration's members instead, it was true over nothing from the bootstrap until the others were admitted.
    self.observe(self.shards.first().copied(), |s| {
      crate::fleet::probe_mesh(s).meshed()
    })
  }

  /// The members held alive that this node keeps direct contact with but has no formed probe session to — what
  /// keeps [`Self::fleet_meshed`] false — with the members it holds alive, for a failure's diagnosis.
  pub fn fleet_unmeshed(
    &self,
  ) -> Result<(Vec<slates_db::HostId>, Vec<slates_db::HostId>), ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      let mesh = crate::fleet::probe_mesh(s);
      (mesh.unmeshed, mesh.alive)
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

  /// Blocks the calling thread until this daemon has a reason to stop: its anchor's stop request (`SUP_STOP`), or `also`
  /// returning true (the caller's own reasons: a signal, a gone parent). Both are asked at the control shard's
  /// heartbeat, which beats at this cadence anyway, so the waiting thread wakes for nothing else. Before 2026-10-10 the
  /// daemon process's thread polled the same three questions every heartbeat on its own, ten wakeups a second at rest
  /// (BENCHMARKS). Returns once a reason holds, or when the control shard's state ends (its watch dropped).
  ///
  /// The watch is handed to the control shard over the channel its start received, and taken in at its next heartbeat:
  /// nothing here waits on the shard's work, so a shard busy with recovered work cannot refuse it. Until 2026-11-20 the
  /// watch was installed by an observation under the observe budget (ten liveness windows, a fixed 10,000 ms), and a
  /// control shard whose recovered encoding held it past that refused the install; the daemon stopped itself, exited 0,
  /// and its anchor restarted it into the same work, 78 times in w-interop's git matrix, its clients waiting meanwhile.
  pub fn wait_for_stop(&self, also: impl Fn() -> bool + Send + 'static) {
    let (wake, woken) = std::sync::mpsc::sync_channel::<()>(1);
    let watch = StopWatch {
      also: Box::new(also),
      wake,
    };
    // A send refused means the control shard's state, and the channel with it, is gone: the daemon is stopping.
    if self.stop_watch_send.send(watch).is_err() {
      return;
    }
    // Either a reason was sent, or the watch's sender went with the control shard's state: both end the wait.
    let _reason_or_gone = woken.recv();
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

  /// For each council member, what this node holds of it on the way to a retirement (§4.8): the fleet's belief (the
  /// liveness the council's death watch reads, as debug text), the membership plane detector's own belief, and the
  /// death watch's count of consecutive drive periods it has held the member dead
  /// (`ShardState::council_death_watch`). A one-shot control-shard question ([`Self::observation`]). Exposed so a
  /// fleet test whose retirement never commits names the step that stopped it: never condemned, condemned and
  /// refuted, or condemned but not held for the window.
  pub fn council_death_view(
    &self,
  ) -> Result<Vec<(slates_db::HostId, String, String, u32)>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.council
        .configuration()
        .members
        .iter()
        .map(|member| {
          let fleet = format!("{:?}", s.fleet.membership().state(*member));
          let detector = s.plane.plane.as_ref().map_or_else(
            || "no plane".to_owned(),
            |plane| {
              format!(
                "{} now_ns={}",
                plane.diagnose(*member),
                slates_machine::clock::monotonic_ns()
              )
            },
          );
          let watched = s.council_death_watch.get(member).copied().unwrap_or(0);
          (*member, fleet, detector, watched)
        })
        .collect()
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

  /// [`Self::fleet_refusals`] within `budget_ns` rather than the generous observation budget: for a reader that
  /// must not wait out a shard a test deliberately holds (a trace line describing the fleet as a wait begins). The
  /// fleet test trace observed a held control shard with the full budget and so waited out the hold before the
  /// poll it was describing began: two traced runs of the hedge test failed at exactly the hold (2026-10-07).
  pub fn fleet_refusals_within(
    &self,
    budget_ns: u64,
  ) -> Result<std::collections::BTreeMap<&'static str, u64>, ObserveError> {
    self
      .observation(self.shards.first().copied(), budget_ns, |s| {
        s.refusals.clone()
      })?
      .wait()
  }

  /// How long this daemon took to serve each NFS call, from the call read to its reply built, summed over every
  /// shard (§4.14): calls served on the shard that read them, then calls forwarded to their volume's owner shard.
  /// What a caller measured beyond these is outside the serve: the kernel client, the socket and the wakes.
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

  /// Whether this daemon admits a write that changes its shard image — a `Create`, a `Snapshot` — or refuses it
  /// `ConsensusNotInitialized` (§4.8): its control shard has imported both groups it belongs to, the root and its
  /// region's council, and is a member of the council's configuration. A fleet test's settled fleet waits for it on
  /// every node, since the council's membership can be committed while a learner's root import is still owed.
  pub fn consensus_ready(&self) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.consensus_ready)
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
  /// test, not a hope. The hold is a task spawned onto the control shard that runs until the span has passed:
  /// the shard is cooperative, so while the hold runs nothing else on that shard
  /// does — its probe serve tasks (so its acknowledgements to every peer's probes stop for the whole span,
  /// then resume), its own probes, its record plane and its consensus loops.
  ///
  /// It runs as a starved thread runs, a little CPU at a time (A-232): each beat it spins for its share and sleeps the
  /// rest, its share being what keeps its whole CPU over the span to half the window the watchdog judges the control
  /// shard by ([`starved_share_ns`], A-552). Until A-232 it spun the whole span, and the watchdog's own rule (A-228) calls a window of
  /// CPU without a step a spinning, stuck shard, not a starved one: an anchor would kill that daemon, and since A-232 a
  /// lone one ends itself. So a span past a window was a stuck daemon, not the live, starved one this models. Admission is retried while the
  /// control channel is full ([`Self::observation`]); the typed refusal when there is no control shard or
  /// the hold was not admitted within the observe budget, otherwise the **admitted** hold, whose answer
  /// ([`Admitted::answer`]) is its **measured span** (nanoseconds) once it ends — so a test proves the
  /// starvation it relied on actually held. Returns once admitted: the caller watches the rest of the
  /// fleet while the shard is held. The hold's budget is the observe budget past the span it holds for.
  pub fn starve_control_shard(&self, span_ns: u64) -> Result<Admitted<u64>, ObserveError> {
    // The hold announces the moment it takes the shard, so this call returns only once the shard is
    // genuinely spinning on it. A question submitted after it is then ordered behind a busy shard that
    // cannot step its scheduler to admit it, so the queued-behind-a-hold path deadlines in the
    // admission stage deterministically — it never slips into execution because the shard admitted the
    // question in the same step it took the hold, which a loaded runner would otherwise do (§4.3). The
    // admission-vs-execution staging the observe oracle asserts rides on this ordering being certain.
    let (spinning, confirm) = std::sync::mpsc::channel::<()>();
    let held = self
      .pending(
        self.shards.first().copied(),
        OBSERVE_BUDGET_NS.saturating_add(span_ns),
        move || {
          let _ = spinning.send(());
          Ok(starve_this_thread(span_ns))
        },
      )?
      .admit()?;
    // Block until the hold reports it is on the shard; the admitted handle is valid regardless, so a
    // barrier that does not confirm within the budget still yields it.
    let _ = confirm.recv_timeout(std::time::Duration::from_nanos(OBSERVE_BUDGET_NS));
    Ok(held)
  }

  /// Test support: **wedges** this daemon's control shard for `span_ns` — a task that sleeps on the shard's
  /// thread, so the shard runs nothing else and, unlike [`Self::starve_control_shard`], **consumes no CPU**
  /// while it does. Returns the admitted hold once it is genuinely on the shard, like its sibling; its answer
  /// is the measured span.
  ///
  /// The two are different injections, for the two halves of the §4.14 rule, and what a test asserts decides
  /// which it wants. An observation's budget is the **target shard's own CPU time** (A-65,
  /// `docs/bugs/2026-10-03-an-observation-read-a-starved-shard-as-wedged.md`): a shard that is working but slow
  /// is given more wall time and answers, and only a shard that is not running ends the observation at its
  /// deadline. So a test that wants a **deadline** must wedge; a spinning hold produces one only on a machine
  /// idle enough to run the spin at the wall clock's pace.
  ///
  /// Measured 2026-10-18 on an 18-core Mac under twenty-four CPU burners: a two-second *spinning* hold advanced
  /// the shard's CPU clock by 153 ms (72 ms to 225 ms), never reached the 300 ms budget, and the observations
  /// that three tests expected to deadline were answered after the hold ended instead. A wedge of the same span
  /// advances it by nothing, which is the condition those tests are about.
  pub fn wedge_control_shard(&self, span_ns: u64) -> Result<Admitted<u64>, ObserveError> {
    // The same barrier as the spinning hold: this call returns once the shard is on it, so a question
    // submitted afterwards is certainly ordered behind it.
    let (sleeping, confirm) = std::sync::mpsc::channel::<()>();
    let held = self
      .pending(
        self.shards.first().copied(),
        OBSERVE_BUDGET_NS.saturating_add(span_ns),
        move || {
          let _ = sleeping.send(());
          let begin = slates_rt::futures::now_ns();
          // The point of the fixture: the shard's thread is off the CPU for the span, not burning it.
          #[allow(clippy::disallowed_methods)]
          std::thread::sleep(std::time::Duration::from_nanos(span_ns));
          Ok(slates_rt::futures::now_ns().saturating_sub(begin))
        },
      )?
      .admit()?;
    let _ = confirm.recv_timeout(std::time::Duration::from_nanos(OBSERVE_BUDGET_NS));
    Ok(held)
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
      s.placed_heads
        .get(&object)
        .is_some_and(|head| head.placed_under(s.fleet.configuration(), object))
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
      crate::content_holder::serve(s, local, &request, |_, _, _, _, _| true)
    })
  }

  /// Test support: holds `record` from `owner` on this node as a holder whose authority check passed — the
  /// production acceptance (`crate::fleet::hold_checked_record`) and whatever stands behind its acknowledgement —
  /// and returns the reply bytes. A test places a held record on a daemon with no fleet peer through it, to observe
  /// what the holder keeps across a restart. The record is written under the holder's installed configuration
  /// version, as an authorized owner's would be.
  pub fn hold_record_as_authorized(
    &self,
    owner: slates_db::register::HostId,
    record: slates_db::register::Record,
  ) -> Result<Vec<u8>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let local = s.fleet.host();
      let record = slates_db::register::Record {
        generation: s.fleet.configuration().version,
        ..record
      };
      crate::fleet::hold_checked_record(s, local, owner, &record)
    })
  }

  /// Test support (A-270): whether the held pieces this node's committed recovery journal carries are exactly what it
  /// holds now (`crate::verbs::committed_held_agrees`), on the shard that holds for other owners.
  pub fn held_journal_agrees(&self) -> Result<bool, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      crate::verbs::committed_held_agrees(s)
    })
  }

  /// Test support (A-367): every position this node holds of `object` as a holder, as its sequence and the epoch it was
  /// accepted at, in sequence order.
  pub fn fleet_held_positions(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Vec<(u64, u64)>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.holder_records
        .get(&object)
        .map(|acceptor| {
          acceptor
            .positions()
            .filter(|(held, ..)| *held == object)
            .map(|(_, sequence, epoch, _)| (sequence, epoch.0))
            .collect()
        })
        .unwrap_or_default()
    })
  }

  /// Test support: the record this node holds of `object` at ledger position `sequence`, with the epoch its fence has
  /// seen; `None` when it holds none there. A test reads what a takeover adopted at a position whatever the successor
  /// wrote after it.
  pub fn fleet_held_record_at(
    &self,
    object: slates_db::register::ObjectId,
    sequence: u64,
  ) -> Result<Option<HeldRecordView>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let acceptor = s.holder_records.get(&object)?;
      let (promised, accepted) = acceptor.persisted();
      accepted
        .into_iter()
        .find(|(held, at, _, _)| *held == object && *at == sequence)
        .map(|(_, sequence, epoch, value)| HeldRecordView {
          sequence,
          epoch: epoch.0,
          value,
          fence: promised.0,
        })
    })
  }

  /// The newest record this node holds of `object` as a holder, with the epoch its fence has seen: what a takeover's
  /// phase one would read from it.
  pub fn fleet_held_record(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<HeldRecordView>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let acceptor = s.holder_records.get(&object)?;
      let (promised, accepted) = acceptor.persisted();
      accepted
        .into_iter()
        .filter(|(held, _, _, _)| *held == object)
        .max_by_key(|(_, sequence, _, _)| *sequence)
        .map(|(_, sequence, epoch, value)| HeldRecordView {
          sequence,
          epoch: epoch.0,
          value,
          fence: promised.0,
        })
    })
  }

  /// The newest head record this node holds of `object` as a mirror holder (`crate::mirror`): its sequence and
  /// epoch, and the mirror holders it names as the content's.
  pub fn fleet_mirror_record(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<(u64, u64, Vec<u64>)>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      let record = s.mirror_records.newest(object)?;
      let holders = crate::head::HeadValue::from_record_bytes(&record.value)
        .map(|head| head.content_holders)
        .unwrap_or_default();
      Some((record.sequence, record.epoch.0, holders))
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

  /// Test support (A-385): how far this node has folded `green`'s histories and what they hold — the engine where this
  /// node serves the green, else its replica as a holder — as the version below which they are folded and their bytes
  /// (`Green::retained_bytes`'s history); `None` where it has neither. Runs a one-shot question on the shard that serves
  /// the green, then on the control shard that holds the replicas; the typed refusal when either could not be observed.
  pub fn merge_history(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<Option<(u64, usize)>, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    let volume = slates_db::catalog::VolumeId { bytes: green.bytes };
    let history = |engine: &slates_merge::engine::Green| {
      (engine.folded_below(), engine.retained_bytes().history)
    };
    let served = self.observe(self.shard_of_object(object), move |s| {
      s.greens.get(&volume).map(|engine| history(&engine))
    })?;
    if served.is_some() {
      return Ok(served);
    }
    self.observe(self.shards.first().copied(), move |s| {
      s.merge.replicas.get(&object).map(history)
    })
  }

  /// The chain of `green` as the shard that serves it holds it: each version's increment identity in
  /// order (§4.16; AUD-14), those folded into its checkpoint (W3) as the checkpoint keeps them — what a test compares between the departed owner and its successor to prove
  /// the full ledger prefix and the original results were preserved through the takeover. Empty for a
  /// green this daemon does not serve; the typed refusal when the shard could not be observed.
  pub fn merge_chain_identities(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<Vec<(u64, [u8; 32])>, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    let volume = slates_db::catalog::VolumeId { bytes: green.bytes };
    let (adopted_through, mut identities) =
      self.observe(self.shard_of_object(object), move |s| {
        // The versions up to the green's checkpoint (W3) are named by the identities it keeps with them; the chain
        // holds the versions after the green's floor: its checkpoint, else its adoption (A-333), else 0.
        let partition = s.db.partition();
        let held = partition.green_floor(volume);
        let mut identities: Vec<(u64, [u8; 32])> = partition
          .green_checkpoint(volume)
          .map(|(_, bytes)| {
            slates_merge::checkpoint::Checkpoint::decode(bytes)
              .map(|checkpoint| checkpoint.accepted)
              .unwrap_or_default()
              .into_iter()
              .map(|(id, version)| (version, id))
              .collect()
          })
          .unwrap_or_default();
        identities.sort_unstable();
        identities.extend(partition.green_chain(volume).iter().enumerate().filter_map(
          |(index, bytes)| {
            slates_merge::chain_entry::decode(bytes).ok().map(|entry| {
              (
                u64::try_from(index)
                  .unwrap_or(u64::MAX)
                  .saturating_add(held)
                  .saturating_add(1),
                entry.increment.id,
              )
            })
          },
        ));
        // A green its takeover adopted from this node's holder ledger, with no checkpoint past the adoption, names the
        // versions up to it by the ledger's records, which the control shard holds.
        let adopted = partition
          .green_adoption(volume)
          .filter(|_| partition.green_checkpoint(volume).is_none())
          .map(|adoption| adoption.version);
        (adopted, identities)
      })?;
    if let Some(through) = adopted_through {
      // Its ledger's, with the versions its durable checkpoint stands for (A-404, A-476).
      let mut ledger = self.observe(self.shards.first().copied(), move |s| {
        crate::merge_origin::ledger_identities(s, object, through)
      })?;
      ledger.append(&mut identities);
      identities = ledger;
    }
    Ok(identities)
  }

  /// The bytes the snapshot of `green`'s partition encodes to now (§4.8): what every snapshot taken when its log
  /// fills writes, and what recovery reads first. A test reads it to hold the partition to what its volumes are rather
  /// than what they have been (W3: a green's chain checkpointed at its reachable floor). Encodes the partition on the
  /// green's owner shard; the typed refusal when it could not be observed.
  pub fn partition_snapshot_bytes(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<usize, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    self.observe(self.shard_of_object(object), move |s| {
      slates_wire::Wire::to_bytes(&s.db.partition().to_snapshot(0)).len()
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

  /// The version of `green` that `work` holds as its floor at the green's owner partition (A-247; A-697 part a: a work
  /// another node holds is admitted by a forwarded ask), as the owner's record keeps it: `None` for no floor. The typed
  /// refusal when the shard could not be observed.
  pub fn work_floor(
    &self,
    green: slates_ipc::protocol::VolumeId,
    work: slates_ipc::protocol::VolumeId,
  ) -> Result<Option<u64>, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    let green = slates_db::catalog::VolumeId { bytes: green.bytes };
    let work = slates_db::catalog::VolumeId { bytes: work.bytes };
    self.observe(self.shard_of_object(object), move |s| {
      s.db.partition().work_floor(green, work)
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

  /// Every floor `green`'s owner partition keeps for its works, as (work, base) (A-247; A-697: a holder's works among
  /// them, whose ids name the holder), for a test. The typed refusal when the shard could not be observed.
  pub fn work_floors_of(
    &self,
    green: slates_ipc::protocol::VolumeId,
  ) -> Result<Vec<(slates_ipc::protocol::VolumeId, u64)>, ObserveError> {
    let object = slates_db::register::ObjectId(green.bytes);
    self.observe(self.shard_of_object(object), move |s| {
      s.db
        .partition()
        .work_floors()
        .filter(|floor| floor.green.bytes == green.bytes)
        .map(|floor| {
          (
            slates_ipc::protocol::VolumeId {
              bytes: floor.work.bytes,
            },
            floor.base,
          )
        })
        .collect()
    })
  }

  /// Test support (A-697, until its last piece puts the rule in its place; never reachable from the wire): with `on`,
  /// every shard of this node serves the works of greens another node owns (`work_home::green_elsewhere`): a creation is
  /// made on its client's partition and admitted by the green's owner over the fleet, and the work's base reads are
  /// answered by this node's replica of the green. `Ok` once every shard has it, else the first typed refusal.
  pub fn serve_works_on_holders(&self, on: bool) -> Result<(), ObserveError> {
    for shard in &self.shards {
      self.observe(Some(*shard), move |s| s.work_home.on_holders = on)?;
    }
    Ok(())
  }

  /// Test support: loses the next verdict or admission the work's shard for `partition` is answered by its green's
  /// owner (A-247; never reachable from the wire), as a crash between the owner's step and the work's would.
  pub fn lose_next_owner_answer(&self, partition: u16) -> Result<(), ObserveError> {
    let shard = self.shards.get(usize::from(partition)).copied();
    self.observe(shard, |s| s.work_home.lose_next_answer = true)
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

  /// Test support: while `withhold` is set, every shard's Shards streams make their replies, publishing those that
  /// wait for a barrier with their effect as always, and write none, as when a daemon dies between a barrier and its
  /// reply (A-316). `Ok` once every shard has it, else the first typed refusal.
  pub fn withhold_shards_replies(&self, withhold: bool) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| s.shards_withhold_replies = withhold)?;
    }
    Ok(())
  }

  /// Test support: while `withhold` is set, no shard sends the control partition what it owes the audit (A-265's
  /// crossing; `LandingState::crossing_withheld`): an owed record stays between its atom's commit and its batch, where a
  /// daemon that died used to lose it. A daemon started over the same anchor sends it from its recovered outbox.
  /// Delivered like [`Self::withhold_shards_replies`]; `Ok` once installed, else the typed refusal.
  pub fn withhold_audit_crossing(&self, withhold: bool) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| s.landing.crossing_withheld = withhold)?;
    }
    Ok(())
  }

  /// Test support: while `withhold` is set, no shard records the control partition's acknowledgement of its appended
  /// audit records (A-265; `LandingState::acknowledgement_withheld`), as if each were lost: the outbox keeps them, and
  /// the deadline keeper's look sends them again (A-554), which the control partition must append nothing twice of.
  /// Delivered like
  /// [`Self::withhold_audit_crossing`]; `Ok` once installed, else the typed refusal.
  pub fn withhold_audit_acknowledgement(&self, withhold: bool) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.landing.acknowledgement_withheld = withhold;
      })?;
    }
    Ok(())
  }

  /// Test support: the quantum the record period's live slices are cut by on every shard (`live_ship::period_slice`), and
  /// a returned holder's walk's turns (`live_ship::reached_slice`), in place of the shard's own step quantum; `None` gives
  /// the shard its own back. Zero ends each slice after one volume, so
  /// a test sees the slicing whatever the machine's wake cost. Delivered like [`Self::inject_live_state_withhold`]; `Ok`
  /// once installed, else the typed refusal.
  pub fn inject_live_period_quantum(&self, quantum_ns: Option<u64>) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.live.period_quantum_ns = quantum_ns;
      })?;
    }
    Ok(())
  }

  /// Test support: the quantum a seal's turns are cut by on every shard (the record period's seal walk and envelope wrap,
  /// `fleet::seal_walk_slice`, and a takeover's envelope open, `fleet::open_slice`), in place of the shard's own step
  /// quantum; `None` gives the shard its own back. Zero ends each turn after one volume or one chunk, so a test sees the
  /// slicing whatever the machine's speed. Delivered like [`Self::inject_live_period_quantum`]; `Ok`
  /// once installed, else the typed refusal.
  pub fn inject_seal_walk_quantum(&self, quantum_ns: Option<u64>) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.seal_walk_quantum_ns = quantum_ns;
      })?;
    }
    Ok(())
  }

  /// Test support: the quantum an install's walk over the held register copies is cut by on every shard
  /// (`fleet::install_slice`, A-398), in place of the shard's own step quantum; `None` gives the shard its own back.
  /// Zero ends each turn after one copy, so a test sees the slicing whatever the machine's speed. `Ok` once installed
  /// on every shard, else the first typed refusal.
  pub fn inject_install_walk_quantum(&self, quantum_ns: Option<u64>) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.install_walk_quantum_ns = quantum_ns;
      })?;
    }
    Ok(())
  }

  /// Test support: every shard waits `pause_ns` between the turns of a live takeover's rebuild it makes
  /// (`fleet::rebuild_in_turns`, g251), so a materialization outlasts the control shard's periods and its later
  /// attempts meet it running; `None` lifts it. `Ok` once installed on every shard, else the first typed refusal.
  pub fn inject_live_turn_pause(&self, pause_ns: Option<u64>) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.live_turn_pause_ns = pause_ns;
      })?;
    }
    Ok(())
  }

  /// Test support: this node is superseded on `volume` at `epoch` (`live_ship::inject_superseded`, never reachable from
  /// the wire): `learned` now, as a holder's answer with a later fence finds it and its mounts and streams are told, or as
  /// a restart over a kept fence finds it, telling none (A-344). So a mount's or a Shards stream's answer to a takeover
  /// (A-346, A-584) is driven without a fleet, each path alone. Delivered on the volume's owner shard; `Ok` once
  /// installed, else the typed refusal.
  pub fn inject_superseded(
    &self,
    volume: slates_ipc::protocol::VolumeId,
    epoch: u64,
    learned: bool,
  ) -> Result<(), ObserveError> {
    let id = slates_db::catalog::VolumeId {
      bytes: volume.bytes,
    };
    self.observe(
      self.shard_of_object(slates_db::register::ObjectId(volume.bytes)),
      move |s| crate::live_ship::inject_superseded(s, id, epoch, learned),
    )
  }

  /// Test support: while `withhold` is set, this node's record serve side holds each live-state reply (a takeover's
  /// report or log fetch) after its request arrived, and answers everything else, probes included, as usual: a holder
  /// alive and in contact whose answer to a takeover misses the asker's budget. Delivered like
  /// [`Self::inject_discovery_fault`]; `Ok` once installed, else the typed refusal.
  pub fn inject_live_state_withhold(&self, withhold: bool) -> Result<(), ObserveError> {
    // On every shard: a record session's serve side runs on whichever shard took its connection.
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.live_state_withhold_replies = withhold;
      })?;
    }
    Ok(())
  }

  /// Test support: the host memory available that this daemon's pressure sampler reads from now on, in
  /// place of the platform's reading (§4.2; admission.md §5.5) — so a test drives the sampler's
  /// arithmetic (its baseline and shortfall) deterministically. Installed on every shard; each reading is a report to
  /// the sampler, as the kernel's would be (A-570), and the first replaces the source the baseline was taken from, so
  /// the baseline is taken again from it.
  pub fn inject_available_memory(&self, bytes: u64) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        if s.injected_available.replace(bytes).is_none() {
          s.pressure_baseline = None;
        }
        crate::pressure::ask(s);
      })?;
    }
    Ok(())
  }

  /// Test support: this process's own resident memory as the pressure sampler reads it from now on, in place of the
  /// platform's reading (§4.2): with [`Daemon::inject_available_memory`], a test drives the sampler's split between
  /// the daemon's own growth and pressure from elsewhere. Installed on every shard, as the available reading is, a
  /// report to the sampler as that is, and the first replaces the resident baseline's source as that does.
  pub fn inject_resident_memory(&self, bytes: u64) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        if s.injected_resident.replace(bytes).is_none() {
          s.pressure_resident_baseline = None;
        }
        crate::pressure::ask(s);
      })?;
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

  /// Test support: runs every shard's publication now, as a transport's barrier does after a mutation (a FUSE `close`,
  /// an NFS procedure): the recovery image then names the content written so far. A test proves what a freed block that
  /// an image names still owes (A-99: its plaintext scrubbed at the commit that releases it) without driving a kernel
  /// mount. `Ok` once every shard has answered, else the first shard's typed refusal.
  pub fn publish_every_shard(&self) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), |s| {
        let _ = crate::verbs::publish_shard(s);
      })?;
    }
    Ok(())
  }

  /// Test support: gives back every shard's dead recovery memory now, as an idle shard's sweep pass does
  /// (`return_dead_recovery`, A-117), rather than waiting for a sweep pass to find the shard idle: the bytes given back
  /// over every shard.
  pub fn give_back_recovery_on_every_shard(&self) -> Result<u64, ObserveError> {
    let mut returned = 0u64;
    for shard in self.shards.iter().copied() {
      // Slice after slice, as an idle sweep pass takes them (one quantum a step), until nothing is left.
      loop {
        let (bytes, more) = self.observe(Some(shard), return_dead_recovery)?;
        returned = returned.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if !more {
          break;
        }
      }
    }
    Ok(returned)
  }

  /// Test support: the bytes of dead recovery memory every shard has still to give back (A-226): what the give-back's
  /// own record says is left, so a test waits on the give-back's completion rather than on how long it usually takes.
  pub fn recovery_left_to_give_back(&self) -> Result<u64, ObserveError> {
    let mut left = 0u64;
    for shard in self.shards.iter().copied() {
      let bytes = self.observe(Some(shard), |s| {
        let (slots_start, slots_end) = s.content_range;
        let (log_start, log_end) = s.delta_range;
        s.journal
          .dead_ranges(
            slots_end.saturating_sub(slots_start),
            log_end.saturating_sub(log_start),
          )
          .map_or(0, |dead| {
            dead.stale_slot.1.saturating_add(dead.log_past_tail.1)
          })
      })?;
      left = left.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }
    Ok(left)
  }

  /// Test support: gives back every shard's dead recovery memory a slice at a time, as an idle shard's sweep pass does
  /// (`return_dead_recovery`, A-226), timing each step on the shard thread's own CPU clock, which a loaded host does
  /// not inflate: the bytes given back, the steps, the CPU each step held its shard, and the step quantum the shard
  /// holds its work to. `None` for the CPU where the shard's thread clock cannot be read (Windows' thread times tick
  /// at about 15.6 ms, too coarse to time a step by).
  pub fn give_back_recovery_by_steps(&self) -> Result<GiveBackSteps, ObserveError> {
    let mut report = GiveBackSteps {
      returned: 0,
      steps: 0,
      step_cpu_ns: Some(Vec::new()),
      quantum_ns: 0,
    };
    for shard in self.shards.iter().copied() {
      loop {
        let (bytes, more, cpu_ns, quantum_ns) = self.observe(Some(shard), |s| {
          let cpu = || {
            registry::with_current(|ctx| ctx.id)
              .and_then(registry::holder_of)
              .and_then(registry::shard_cpu)
              .filter(|_| cfg!(unix))
              .map(|reading| reading.spent_ns)
          };
          let before = cpu();
          let (bytes, more) = return_dead_recovery(s);
          let after = cpu();
          let quantum = futures::step_budget_ns().unwrap_or(s.config.runtime.step_budget_ns);
          (
            bytes,
            more,
            before
              .zip(after)
              .map(|(before, after)| after.saturating_sub(before)),
            quantum,
          )
        })?;
        report.returned = report
          .returned
          .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        report.steps = report.steps.saturating_add(1);
        report.step_cpu_ns = report
          .step_cpu_ns
          .take()
          .zip(cpu_ns)
          .map(|(mut steps, step)| {
            steps.push(step);
            steps
          });
        report.quantum_ns = report.quantum_ns.max(quantum_ns);
        if !more {
          break;
        }
      }
    }
    if let Some(steps) = report.step_cpu_ns.as_mut() {
      steps.sort_unstable();
    }
    Ok(report)
  }

  /// Test support: runs one idle give-back of the first shard's dead recovery memory, as a sweep pass does, from a page
  /// slice and over the whole of that memory as after a restart, with `neighbour` another task taking turns on the
  /// shard the whole time: how many idle steps it took, `None` if the shard could not be asked or did not finish within
  /// the budget. A neighbour that allocates nothing looks idle to the arena, as a granted landing does (A-226). The
  /// answer comes through an atomic, because asking the shard for it would itself be another task on the shard.
  pub fn give_back_beside(&self, neighbour: bool) -> Option<u64> {
    let shard = self.shards.first().copied();
    GIVE_BACK_PROBE.store(GIVE_BACK_PROBE_RUNNING, Ordering::Release);
    self
      .observe(shard, move |s| {
        // Only a shard with a committed image has dead memory to give back.
        let _ = crate::verbs::publish_shard(s);
        s.journal.forget_what_was_given_back();
        s.give_back_rate = GiveBackRate::default();
        s.purge_seen_allocations = s.store.content.arena().allocations();
        if let Ok(task) = futures::spawn(probe_give_back(neighbour)) {
          let _ = futures::detach(task);
        }
      })
      .ok()?;
    let started = slates_machine::clock::monotonic_ns();
    loop {
      let steps = GIVE_BACK_PROBE.load(Ordering::Acquire);
      if steps != GIVE_BACK_PROBE_RUNNING {
        return Some(steps);
      }
      if slates_machine::clock::monotonic_ns().saturating_sub(started) > OBSERVE_BUDGET_NS {
        return None;
      }
      std::thread::yield_now();
    }
  }

  /// Test support: sets the memory-pressure hold on **every** shard's byte budget (§4.2; admission.md
  /// §5.5) — capacity withheld from new admission under host memory pressure. In production
  /// [`refresh_pressure_hold`] sets it from a sampled host shortfall at each report of the host's
  /// memory pressure (A-570, `crate::pressure`); this
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

  /// The bytes of content-pool extents this daemon's memory-pressure hold owns now (§4.2, admission.md §5.5): the part
  /// of the hold withheld from the pool, read through the control shard's attachment. With [`Daemon::pressure_holds`]
  /// (the free room taken from each shard), the whole hold. A pool word the segment refuses to reach is its typed
  /// refusal.
  pub fn pressure_pooled(&self) -> Result<Result<u64, slates_anchor::AnchorError>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      crate::content_pool::hold_bytes(
        &s.segment,
        &crate::content_pool::PoolGeometry::of(&s.config),
      )
    })
  }

  /// The whole memory-pressure hold this daemon withholds now (§4.2, admission.md §5.5): the pool extents the hold owns
  /// ([`Daemon::pressure_pooled`]), the free room taken from each shard ([`Daemon::pressure_holds`]), and the debt the
  /// next shard to grow from the pool takes on (told to every shard alike, so counted once). A pool word the segment
  /// refuses to reach is its typed refusal.
  pub fn pressure_withheld(&self) -> Result<Result<u64, slates_anchor::AnchorError>, ObserveError> {
    let pooled = self.pressure_pooled()?;
    let mut held = 0u64;
    let mut debt = 0u64;
    for shard in self.shards.iter().copied() {
      let (hold, owed) = self.observe(Some(shard), |s| {
        (s.store.budget.hold(), s.store.budget.debt())
      })?;
      held = held.saturating_add(hold);
      debt = debt.max(owed);
    }
    Ok(pooled.map(|pool| pool.saturating_add(held).saturating_add(debt)))
  }

  /// Test support (A-393): every shard's write log is marked overflowed, as a log is when it could not take an
  /// acknowledged write and the publication that would have emptied it was refused (A-63): the next daemon then reports
  /// every file a taken-over attachment had open as having lost writes. A test of what a successor tells its guest sets
  /// the very predicate the log keeps, rather than starving a publication. Whether every shard holding a log marked
  /// it, or the first shard's typed refusal.
  #[cfg(unix)]
  pub fn inject_write_log_overflow(&self) -> Result<bool, ObserveError> {
    let mut marked = true;
    for shard in self.shards.iter().copied() {
      let this = self.observe(Some(shard), |s| {
        match (s.write_log.as_mut(), s.content.as_mut()) {
          (Some(log), Some(object)) => log.overflow(object).is_ok(),
          _ => true,
        }
      })?;
      marked = marked && this;
    }
    Ok(marked)
  }

  /// Test support: every shard refuses (or takes again) the NFSv4 purges sent to it, as an owner that cannot make a
  /// drop durable answers `NFS4ERR_DELAY` (2026-10-10). A test leaves a dropped client's purge owed with it, then proves
  /// the purge outlives a restart.
  pub fn inject_nfs_purge_refusal(&self, refused: bool) -> Result<(), ObserveError> {
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| s.nfs_purges_refused = refused)?;
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

  /// Test support: makes this node hear nothing from `peers` on the probe plane — every datagram from one of them is
  /// dropped once opened, as a lost one would be — the one-way loss from each of them to this node (§4.8 "direct
  /// probe → k indirect proxies"; AUD-15). A peer among them then reaches this node only through a relay, and this
  /// node's own probes of it go unanswered too, since its answers are lost; two nodes deaf to each other are isolated
  /// on the plane, while the record plane stays whole. Replaces any earlier set; an empty `peers` restores full
  /// service. Before 2026-10-10 it only left their direct probes unanswered, while their acknowledgements, relayed
  /// answers and anti-entropy still arrived, so an "isolated" council leader refuted its condemnation through
  /// anti-entropy hundreds of times and was never retired. Delivered like [`Self::inject_discovery_fault`]; `Ok` once
  /// installed, else the typed refusal.
  pub fn inject_probe_deafness(&self, peers: &[slates_db::HostId]) -> Result<(), ObserveError> {
    let deaf_to: std::collections::BTreeSet<slates_db::HostId> = peers.iter().copied().collect();
    self.observe(self.shards.first().copied(), move |s| {
      s.probe_deaf_to = deaf_to;
    })
  }

  /// Test support: makes this node wait `delay_ns` before answering each location query and each forwarded verb, as
  /// a slow or distant owner would (`None` restores immediate answers). Each answer still falls within the asker's
  /// own bound for that step, so a test can drive a forward's total time past a client's reply deadline. Delivered
  /// like [`Self::inject_probe_deafness`]; `Ok` once installed, else the typed refusal.
  pub fn inject_serve_delay(&self, delay_ns: Option<u64>) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_serve_delay_ns = delay_ns;
    })
  }

  /// Test support: makes this node, as a content holder, hold each acknowledgement of a placement `delay_ns` before it
  /// leaves (`None` restores immediate answers). The hold has already taken the content, so an owner whose round stops
  /// collecting at its hedge delay first receives the acknowledgement as a straggler's, as it does from a holder whose
  /// refill outlasts the round under load. A missing set or a refusal goes at once. Delivered like
  /// [`Self::inject_serve_delay`]; `Ok` once installed, else the typed refusal.
  pub fn inject_content_ack_delay(&self, delay_ns: Option<u64>) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_content_ack_delay_ns = delay_ns;
    })
  }

  /// Test support: while `Some(ns)`, this node publishes `ns` as its failover delay in place of its estimate, on every
  /// shard at once (A-363, A-414): the bound of every wait that bounds a failover (a forward, the location round, a held
  /// reply, a takeover's attempts) and the term of the leases that read it (the write lease, a landing's target lease,
  /// a staging buffer), for a test whose subject is that bound's or term's rule. `None` returns to the estimate at the
  /// control shard's next beat, which its fan carries to the others (A-574).
  pub fn inject_failover(&self, ns: Option<u64>) -> Result<(), ObserveError> {
    // On every shard at once, so a wait on any shard reads it before the control shard's next beat fans the same value
    // (A-574).
    for shard in self.shards.iter().copied() {
      self.observe(Some(shard), move |s| {
        s.injected_failover_ns = ns;
        if let Some(ns) = ns {
          s.failover_ns = ns;
        }
      })?;
    }
    Ok(())
  }

  /// The failover delay this node's waits read now, nanoseconds (A-363): the control shard's estimate in whole
  /// coordinator periods, or a test's injection. A one-shot control-shard question; an observation the daemon could not
  /// make is its typed refusal.
  pub fn failover_ns(&self) -> Result<u64, ObserveError> {
    self.observe(self.shards.first().copied(), |s| s.failover_ns)
  }

  /// Test support: while `Some(peer)`, this node's record link to `peer` pages no discovery, so the link's own refresh
  /// never takes that session from its slot (`None` lifts it). Delivered like [`Self::inject_serve_delay`].
  pub fn inject_discovery_hold(&self, peer: Option<slates_db::HostId>) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_discovery_hold = peer;
    })
  }

  /// Test support: makes this node's record session to `peer` drop everything it receives from the forward lane's next
  /// borrow of it: a session whose peer is gone without a word (a restarted process that kept its member id, a reset
  /// that was lost). Once, so the session the link dials to replace it is healthy. Delivered like
  /// [`Self::inject_serve_delay`]; `Ok` once installed, else the typed refusal.
  pub fn inject_session_deafness(&self, peer: slates_db::HostId) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_deaf_session = Some(peer);
      // Only a drop after this one counts toward the recovery a test reads: formation's own failed pages drop sessions
      // too, and their recovery would read as this one's.
      s.dead_session_dropped_at.clear();
      s.dead_session_recovered = None;
    })
  }

  /// Test support: leaves every forwarded verb on `volume` unanswered while set, on the session that keeps carrying
  /// this node's other replies (`None` lifts it, and the held verbs are served then). An owner alive and answering
  /// others but never this exchange, as the 205 submits a restarted primary could not commit were (fixtures' 1,000-client
  /// primary-kill rung, 2026-11-07). Delivered like [`Self::inject_serve_delay`].
  pub fn inject_forward_silence(
    &self,
    volume: Option<slates_ipc::protocol::VolumeId>,
  ) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_forward_silence = volume;
    })
  }

  /// Test support: while `withheld` is set, this node sends no confirmation of a takeover share it has finished
  /// (counted `fleet.takeover.confirmation_withheld_by_fault`), as a confirmation late to commit leaves its share owed
  /// while the other survivors confirm theirs, each confirmation a new configuration (A-353). Lifting it lets the next
  /// period confirm. Delivered like [`Self::inject_takeover_silence`]; `Ok` once installed, else the typed refusal.
  pub fn inject_confirmation_withheld(&self, withheld: bool) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_confirmation_withheld = withheld;
    })
  }

  /// Test support: leaves unanswered every takeover page and question by name this node, as a holder, is asked by one of
  /// `askers` (§4.8 phase one; counted `fleet.takeover.silenced_by_fault`), and every planned move's question by name
  /// (A-291; counted `fleet.move.silenced_by_fault`), while it answers everything else on the same
  /// sessions: a holder cut off from one successor alone, which the council never retires since every other member hears
  /// it. Replaces any earlier set; an empty `askers` restores full service. Delivered like
  /// [`Self::inject_serve_delay`]; `Ok` once installed, else the typed refusal.
  pub fn inject_takeover_silence(&self, askers: &[slates_db::HostId]) -> Result<(), ObserveError> {
    let askers: std::collections::BTreeSet<slates_db::HostId> = askers.iter().copied().collect();
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_takeover_silence = askers;
    })
  }

  /// The owner's sealed manifest for `object` once its content has placed on its holders (§4.10), read on the shard
  /// that owns it; `Ok(None)` before then or when this node does not own it. A test or an operator reads it to follow
  /// a seal before its head ships.
  pub fn fleet_sealed_manifest(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<[u8; 32]>, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      crate::fleet::sealed_manifest(s, object)
    })
  }

  /// Whether this owner's head for `object` is held back because its sealing does not yet cover every live candidate
  /// (it waits for a pair delivery, A-92 piece 4c), read on the owning shard. A test or an operator reads it when a
  /// placement does not complete.
  pub fn fleet_head_awaits_keys(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<bool, ObserveError> {
    self.observe(self.shard_of_object(object), move |s| {
      crate::fleet::head_awaits_keys(s, object)
    })
  }

  /// Test support: while `withhold` is set, this node's owner shards deliver no pair keys to their neighbours, as a
  /// neighbour whose recipient answer is late holds its delivery back (A-92 piece 4b). Setting it also forgets the
  /// pairs each owner shard has delivered, so a head this node seals in the meantime has no key entry for any
  /// neighbour: the state of a head written before its candidates' pairs arrived. Lifting it resumes delivery (the
  /// same pair keys, found again). `Ok` once installed on every shard, else the first typed refusal.
  pub fn inject_pair_withhold(&self, withhold: bool) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_pair_withhold = withhold;
    })?;
    if withhold {
      for shard in self.shards.iter().copied() {
        self.observe(Some(shard), |s| s.pairs_delivered.clear())?;
      }
    }
    Ok(())
  }

  /// Test support: while `anchor` is set, this node's owner shards deliver no pair key to the node at that stable anchor,
  /// as a delivery whose recipient answer never comes back fails each period (`fleet.seal.pair_recipient_unknown`);
  /// every other delivery goes on and no pair delivered is forgotten (unlike [`Self::inject_pair_withhold`]), so a
  /// node replaced at that anchor never receives this node's pairs while the rest of the fleet does. `None` lifts it.
  /// `Ok` once installed on the control shard, where deliveries are made.
  pub fn inject_pair_withhold_to(&self, anchor: Option<u64>) -> Result<(), ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      s.injected_pair_withhold_to = anchor;
    })
  }

  /// Test support: the position of the newest entry of the live log this node holds for `object` as a candidate
  /// (A-200), or `None` while it holds none; read on the control shard, where held logs live.
  pub fn fleet_live_held(
    &self,
    object: slates_db::register::ObjectId,
  ) -> Result<Option<u64>, ObserveError> {
    self.observe(self.shards.first().copied(), move |s| {
      crate::live_ship::held_position(s, object)
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
    // The name's partition says where its volume is served: its own, or a clone's beside its origin (A-236).
    let partition = verbs::owner_of_name(name, self.shards.len());
    let asked = name.to_owned();
    let partition = self.observe(self.shards.get(usize::from(partition)).copied(), move |s| {
      let held = s.db.partition();
      match held.volume_by_name(&asked) {
        Some(_) => s.partition,
        None => held
          .name_alias(&asked)
          .and_then(slates_db::alias::resolve)
          .map_or(s.partition, |volume| {
            verbs::owner_of(slates_ipc::protocol::VolumeId {
              bytes: volume.bytes,
            })
          }),
      }
    })?;
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
          mount: None,
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

  /// The members whose record link has a discovery exchange waiting now (the exchange registers its waker while it
  /// waits, `ShardState::link_waiters`): what a test reads to know an exchange is pending, rather than inferring it from
  /// a borrowed session, which a coordinator dispatch borrows too (2026-10-10: the whole-RAM replacement test took a
  /// dispatch's borrow for a pending exchange, and failed when that survivor had no exchange to end).
  pub fn fleet_pending_discovery(&self) -> Result<Vec<slates_db::HostId>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.link_waiters
        .keys()
        .filter_map(|anchor| s.learned_members.get(anchor).map(|learned| learned.host))
        .collect()
    })
  }

  /// This node's record links (§4.8, `ShardState::record_sessions`): each peer with an entry, and whether
  /// its session is out on a borrow at the moment of the read — a dispatch's, or the link task's own
  /// discovery exchange, which this cannot tell apart ([`Daemon::fleet_pending_discovery`] names a pending exchange).
  /// A test reads it to prove a link to a fresh member exists. The typed refusal when the control shard could not be
  /// observed.
  pub fn fleet_record_links(&self) -> Result<Vec<(slates_db::HostId, bool)>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      s.record_sessions
        .iter()
        .map(|(host, link)| (*host, link.endpoint.is_none()))
        .collect()
    })
  }

  /// This node's record dials at the read ([`RecordDialReport`], `ShardState::record_dials`): what a fleet test whose
  /// formation failed prints, so a link that never came up names whether its dialer sits inside one handshake call, waits
  /// on its first discovery page, keeps failing, or idles. Empty on a laptop; the typed refusal when the control shard
  /// could not be observed.
  pub fn fleet_record_dials(&self) -> Result<Vec<RecordDialReport>, ObserveError> {
    self.observe(self.shards.first().copied(), |s| {
      let now = slates_rt::futures::now_ns();
      s.record_dials
        .iter()
        .map(|(peer, dial)| RecordDialReport {
          peer: *peer,
          dialing_for_ns: dial.dialing_since_ns.map(|since| now.saturating_sub(since)),
          call_for_ns: dial.call_since_ns.map(|since| now.saturating_sub(since)),
          enroll_for_ns: dial.enroll_since_ns.map(|since| now.saturating_sub(since)),
          budgets_spent: dial.budgets_spent,
          stall: dial.stall,
          calls: dial.calls,
        })
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

  /// What the liveness watchdog has done (A-228): its looks, and those that withheld the beat for a stuck shard. `None`
  /// once the daemon has stopped it.
  pub fn watchdog_turns(&self) -> Option<(u64, u64)> {
    self
      .watchdog
      .as_ref()
      .map(|watchdog| watchdog.turns().read())
  }

  /// Test support: the latest episode in which the watchdog withheld the beat for a stuck shard (A-232 step 5,
  /// [`crate::watchdog::Turns::episode`]): the last beat before it and how late the watchdog's looks ran to its verdict.
  pub fn watchdog_episode(&self) -> Option<crate::watchdog::Withheld> {
    self
      .watchdog
      .as_ref()
      .and_then(|watchdog| watchdog.turns().episode())
  }

  /// Test support: puts a memory-pressure level in place of the host's reading the watchdog takes at its looks (A-336,
  /// [`crate::watchdog::MemoryStallSource`]): `Some(true)` the host reports a memory stall, `Some(false)` none, `None`
  /// the host's own reading again. In production the watchdog reads the host (`slates_machine::memory_stall`); this
  /// drives that very reading, as [`Daemon::inject_pressure_hold`] drives admission's, so a test proves the rule without
  /// a host short of memory. `false` once the daemon has stopped its watchdog.
  pub fn inject_memory_stall(&self, pressed: Option<bool>) -> bool {
    self
      .watchdog
      .as_ref()
      .map(|watchdog| watchdog.memory_stall().inject(pressed))
      .is_some()
  }

  /// The least CPU one poll may run without a step before the watchdog judges its shard spinning: the liveness window
  /// held to this build (A-318, [`crate::watchdog::spin_window_ns`]); a shard whose host overruns that window is held to
  /// its own window's, which is longer (A-455, [`crate::watchdog::watch_window_ns`]). `None` once the daemon has stopped
  /// its watchdog.
  pub fn spin_window_ns(&self) -> Option<u64> {
    self
      .watchdog
      .as_ref()
      .map(crate::watchdog::Watchdog::spin_window_ns)
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
    // The window the log writer's drain is judged by (A-552): the widest this daemon's shards are judged by now (A-455),
    // read while they still run. A host that keeps the shards off their CPUs that long keeps the writer's thread off its
    // own as long, so a narrower window would give up a writer that is only starved.
    let shard_ids: Vec<u16> = self.shards.iter().map(|shard| shard.0).collect();
    let log_window_ns = crate::watchdog::widest_window_ns(&shard_ids);
    // The FUSE mounts end with each shard's serve loop (`EndMounts`), on the shard's own thread as the
    // shutdown cancels it — never as a question queued behind the shard's work, which would answer every
    // question already waiting there instead of terminating it (§4.6; AUD-29-64).
    if let Some(mut doorbell) = self.doorbell.take() {
      doorbell.stop();
    }
    // The shutdown ends the control loop before the watchdog stops: told first, it does not take that end for one the
    // anchor should restart (A-228).
    if let Some(watchdog) = self.watchdog.as_ref() {
      watchdog.stopping();
    }
    if let Some(runtime) = self.runtime.take()
      && let Err(error) = runtime.shutdown()
    {
      slates_machine::log_line!("slates-server: stopping the daemon's shards: {error}");
    }
    // Last, so a draining daemon still beats while its shards step (A-228).
    if let Some(mut watchdog) = self.watchdog.take() {
      watchdog.stop();
    }
    // After the shards, whose ended threads let their routes go, so the writer has every line they queued. It is waited
    // for while it writes lines (A-552). A shard that never ended still holds its route, and a standard error that takes
    // nothing holds the writer still: it is given up a window after its last line, then left to the process's end,
    // never waited for without end.
    if let Some(mut log_writer) = self.log_writer.take() {
      log_writer.stop(std::time::Duration::from_nanos(log_window_ns));
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

/// This node's certificate anchor (§4.8, A-237): the fleet's pinned certificate's when it has a fleet, else the machine
/// identity's host id. Stable across the node's restarts. It keys completion origins, and it is the host a principal of
/// this node names, so the rendezvous and the shard state take it from here alone.
pub(crate) fn local_anchor(config: &DaemonConfig, identity: &Identity) -> slates_db::HostId {
  config.fleet.as_ref().map_or_else(
    || slates_db::HostId(host_id_of(identity)),
    |m| m.origin_anchor,
  )
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
  keyed_word(secret, b"slates/member-incarnation/v1").max(1)
}

/// The first eight bytes of `domain`'s keyed hash under `secret`, read as a little-endian word.
fn keyed_word(secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES], domain: &[u8]) -> u64 {
  let digest = blake3::keyed_hash(secret, domain);
  let mut word = [0; size_of::<u64>()];
  let (head, _) = digest.as_bytes().split_at(size_of::<u64>());
  word.copy_from_slice(head);
  u64::from_le_bytes(word)
}

/// The key this daemon names its client regions under (A-176), derived with a keyed-hash domain of its own from the
/// fresh secret minted at every start, so no daemon names a region as an earlier one did. Windows keeps a section's
/// name while any process maps it, and a client reconnecting after a restart still maps the region the dead daemon
/// made it: a name made of the instance and the client's id, which a reconnect keeps, was refused
/// `ERROR_ALREADY_EXISTS`, and the client's session ended `DaemonGone` where it should have resumed (AC-2.3). The name
/// is `slates-cr-`, this key in sixteen hex digits, `-` and the region's number in eight (35 bytes), within the 40 a
/// claim slot's answer holds on macOS and Windows.
fn region_name_key(secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES]) -> u64 {
  keyed_word(secret, b"slates/client-region-name/v1")
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
pub(crate) fn slice_start(config: &DaemonConfig, partition: u16) -> usize {
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

/// This shard's content range for its recovery images (§4.8), and the write log at the slice's front with what the
/// previous daemon left in it to replay (A-63, `crate::write_log`: a FUSE mount's writes on Linux, a Shards stream's on
/// every platform, A-316, A-339, A-402), opened against the generation of the committed image, before anything this
/// daemon does can publish and clear it. No log and nothing to replay without a content object.
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

/// The DAX range table in this shard's slice of the content object (A-365; Windows' sections, A-448), with what the
/// previous daemon recorded in it; none without a content object holding the shard's images (`images`,
/// [`content_slice`]'s range).
fn dax_range_table(
  config: &DaemonConfig,
  partition: u16,
  images: (usize, usize),
  content: Option<&mut slates_mem::SparseObject>,
) -> Option<crate::dax_ranges::RangeTable> {
  let object = content.filter(|_| images.1 > images.0)?;
  let layout = config.shard_content_layout();
  let ranges_start = slice_start(config, partition).saturating_add(layout.dax_ranges.0);
  let ranges_bytes = layout.dax_ranges.1.saturating_sub(layout.dax_ranges.0);
  crate::dax_ranges::RangeTable::open(object, ranges_start, ranges_bytes)
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
  // Published: every capture of this arena is a recovery publication (A-64), so its regions keep their live heads from
  // the start and no verb's step scans a region's state.
  let mut arena = ChunkArena::new(config.content_granule())
    .discarding_from(config.discard_from_bytes)
    .published();
  for (id, region) in pool.held()? {
    arena.add_region_at(id, region)?;
  }
  let slice = pool.slice_capacity();
  arena.set_source(Box::new(pool));
  Ok((arena, slice))
}

/// What the anchor handed back for one shard's partition (Unix): its held devices, FUSE devices (Linux) and Shards
/// streams' resync ends (A-61, A-316, A-339), and on Linux its held writable copies (A-197).
#[cfg(unix)]
#[derive(Debug, Default)]
struct ShardHolds {
  /// Where the process thread's stop watch arrives (the control shard's alone; [`StopWatch`]).
  stop_watch: Option<std::sync::mpsc::Receiver<StopWatch>>,
  /// The held devices.
  devices: Vec<crate::fuse_hold::HeldDevice>,
  /// The held writable copies.
  #[cfg(target_os = "linux")]
  copies: Vec<crate::held_copy::InheritedCopy>,
  /// The held Shards streams' DAX objects (A-365).
  dax: Vec<crate::shards::dax_hold::InheritedDax>,
}

/// What the anchor handed back for one shard's partition (Windows): its WinFsp mounts (A-198), the hold channel (the
/// control shard's alone), and its Shards streams' resync ends (A-433) and DAX sections (A-448).
#[cfg(windows)]
struct ShardHolds {
  /// Where the process thread's stop watch arrives (the control shard's alone; [`StopWatch`]).
  stop_watch: Option<std::sync::mpsc::Receiver<StopWatch>>,
  /// The held WinFsp mounts.
  winfsp: Vec<crate::winfsp::InheritedMount>,
  /// The hold channel's daemon end: the control shard installs it.
  channel: Option<std::os::windows::io::OwnedHandle>,
  /// The held Shards streams' resync ends (A-433).
  resync: Vec<crate::anchor_hold::HandedResync>,
  /// The held Shards streams' DAX sections (A-448).
  dax: Vec<crate::shards::dax_hold::InheritedDax>,
}

/// A shard's start, run alone on its shard ([`futures::spawn_alone`]): its boot fault if the tests set one, then
/// [`init_shard`], and the boot log's line of how long it took and in how many slices.
async fn start_shard(
  config: DaemonConfig,
  env: Vec<(String, String)>,
  identity: Identity,
  (partition, sealing, learning): (u16, crate::seal_keys::RootState, ShardLearning),
  all: Vec<u16>,
  retained: Option<crate::retention::Retained>,
  holds: ShardHolds,
) {
  // How long a shard's start takes (its recovery included) is in the boot log: the control loop's first call to each
  // shard queues behind it (GAPS, boot, 2026-10-05).
  let started = std::time::Instant::now();
  if let Some(fault) = config
    .boot_fault
    .filter(|fault| fault.partition == partition)
  {
    hold_start(fault, config.copy_slice_ns()).await;
  }
  let outcome = init_shard(
    &config,
    &env,
    &identity,
    (partition, sealing, learning),
    &all,
    retained,
    holds,
  )
  .await;
  let took_us = started.elapsed().as_micros();
  match outcome {
    Ok(sliced) => slates_machine::log_line!(
      "slates-server: shard {partition} initialized in {took_us} µs, {} µs of its CPU, in {} slices of {} units; the longest slice {} µs, the most CPU in one {} µs, ending at unit {} (a slice's floor {} µs, {} µs yielding)",
      sliced.cpu_ns.map_or_else(
        || "unread".to_owned(),
        |cpu| (cpu / NANOS_PER_MICRO).to_string()
      ),
      sliced.slices,
      sliced.units,
      sliced.longest_ns / NANOS_PER_MICRO,
      sliced.longest_cpu_ns / NANOS_PER_MICRO,
      sliced.longest_cpu_ended_at,
      sliced.budget_ns / NANOS_PER_MICRO,
      sliced.yielding_ns / NANOS_PER_MICRO
    ),
    Err(e) => {
      INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
      slates_machine::log_line!(
        "slates-server: shard {partition} failed to initialize after {took_us} µs: {e}"
      );
    }
  }
}

/// Format: nanoseconds in a microsecond, for the boot log's slice times.
const NANOS_PER_MICRO: u64 = 1_000;

/// Holds a shard's start as a [`crate::config::BootFault`] says: on the CPU (a long recovery) or asleep (a stuck
/// shard), for its duration. Only the boot tests set one.
///
/// A long recovery runs in slices that each end a step ([`crate::recovery_slices`], A-228), so the busy hold runs in the
/// same slices, under the same budget. Until A-232 it spun its whole span as one poll, which the watchdog's rule calls a
/// spinning, stuck shard, and which an anchor kills: since A-232 a daemon with no anchor in another process ends itself on
/// it, so a hold past a window modelled a stuck start, not the long recovery its tests are about.
async fn hold_start(fault: crate::config::BootFault, slice_ns: u64) {
  let duration = std::time::Duration::from_nanos(fault.for_ns);
  match fault.kind {
    // A stuck shard is a thread blocked without using CPU, which only a blocking wait models (D-9's exception for
    // tests): the boot tests' `Stuck` fault, never set in a deployment.
    #[allow(clippy::disallowed_methods)]
    crate::config::BootFaultKind::Stuck => std::thread::sleep(duration),
    crate::config::BootFaultKind::Busy => {
      let started = std::time::Instant::now();
      let mut slices = crate::recovery_slices::RecoverySlices::begin(slice_ns);
      while started.elapsed() < duration {
        std::hint::spin_loop();
        slices.unit_done().await;
      }
      let _report = slices.finish();
    }
  }
}

/// Seals the store's idle content under keys derived from the node's sealing root (A-99); with no root the content
/// stays in the clear, as the sealing state already reports.
fn install_content_cipher(store: &mut Store, seal_root: Option<&hyper_seal::keys::WrappingKey>) {
  if let Some(cipher) = seal_root.and_then(crate::content_cipher::ShardCipher::new) {
    store.content.set_cipher(Box::new(cipher));
  }
}

/// The shard's operation headroom (§4.2), derived from the chunk window, the in-flight admission limit and the
/// shard's slice capacity; see the derivation's comments.
fn operation_headroom(
  config: &DaemonConfig,
  slice_capacity: usize,
) -> slates_machine::derived::Derived<u64> {
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
  derived!(
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
  )
}

/// A shard's learning of the mounts it keeps (A-227, `slates_base::own_mounts::Learning`), let go once they are
/// recorded; none on Windows, which learns its held volumes before any shard starts.
#[cfg(unix)]
type ShardLearning = Option<slates_base::own_mounts::Learning>;
#[cfg(windows)]
type ShardLearning = ();

/// Runs on the shard: attaches the segment, recovers the partition, builds the store and
/// installs the state, then spawns the server loop as a poller. Its recovery runs in slices
/// (`crate::recovery_slices`), which it reports.
async fn init_shard(
  config: &DaemonConfig,
  env: &[(String, String)],
  identity: &Identity,
  (partition, sealing, learning): (u16, crate::seal_keys::RootState, ShardLearning),
  config_shards: &[u16],
  retained: Option<crate::retention::Retained>,
  holds: ShardHolds,
) -> Result<crate::recovery_slices::SliceReport, ServerError> {
  #[cfg(windows)]
  let ShardHolds {
    stop_watch: stop_watch_install,
    winfsp: inherited_winfsp,
    channel: windows_channel,
    resync: inherited_resync,
    dax: inherited_dax,
  } = holds;
  #[cfg(unix)]
  let inherited_dax = holds.dax;
  #[cfg(unix)]
  let stop_watch_install = holds.stop_watch;
  // The carrier of what this shard sends other shards whose control channels are full (§4.3, `xshard::carrier`):
  // first, so nothing the shard sends is ever without it. Refused, the shard serves nothing: a typed initialization
  // failure, as a refused serve loop is below.
  futures::spawn(crate::xshard::carrier(config.kept_bound().get()))
    .and_then(futures::detach)
    .map_err(ServerError::Runtime)?;
  let mut slices = crate::recovery_slices::RecoverySlices::begin(config.copy_slice_ns());
  let (handoff, len) = handoff_of(env)?;
  let mut segment = AnchorSegment::attach(&handoff, len, identity)?;
  let mut clock = HostClock::new();
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
  let (db, recovered) = slates_db::replay::recover(&mut segment, partition, config.caps, now)?;
  slices.unit_done().await;
  let (arena, slice_capacity) = shard_arena(config, partition, env, identity)?;
  let headroom = operation_headroom(config, slice_capacity);
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
  // Inode numbers are handed out under a published reservation (A-150): set before recovery, so a rebuilt volume asks
  // for its top-up.
  store.inode_reserve = config.inode_reserve;
  // The DAX backings' fill budget (A-163): the shard's content reserve, one copy of everything the shard can hold, the
  // cost the design measured for a copy (slope 1.0 over mapped bytes). Set before any attachment can ask for a backing.
  store
    .shards_backings
    .set_budget(u64::try_from(config.shard_content_layout().arena.1).unwrap_or(u64::MAX));
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
  let origin_anchor = local_anchor(config, identity);
  let issuer_secret = segment.issuer_secret()?;
  // The node's sealing root (A-92), read from this shard's own attachment into a key of its own in the locked region.
  let seal_root = crate::seal_keys::shard_root(&segment, sealing);
  // Idle content is sealed in the arena under keys derived from the root (A-99), before recovery adopts any chunk,
  // so a chunk the last daemon sealed opens. No root (sealing unavailable) keeps content in the clear.
  install_content_cipher(&mut store, seal_root.as_ref());
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
  let mut content = match AnchorSegment::open_content(env) {
    Some(result) => Some(result?),
    None => None,
  };
  let (content_range, write_log, recovered_writes) =
    content_slice(config, partition, content.as_mut());
  // The DAX range table (A-365; Windows' sections, A-448), with what the previous daemon recorded in it.
  let dax_ranges = dax_range_table(config, partition, content_range, content.as_mut());
  // The telemetry sink keeps the most recent spans up to one client ring's depth (§4.14): a shard
  // processes at most a ring of in-flight requests, so a ring's depth of recent spans covers the
  // current activity window; older spans are shed (and counted), telemetry being the shed-first class.
  let telemetry_capacity = usize::try_from(config.region.slots).unwrap_or(1).max(1);
  // The grant-issuer secret this daemon minted at start (§4.13), read from the anchor's supervision block
  // before the segment moves into the state; every shard reads the same value, so a `Grant` verifies on
  // whichever shard serves the client.
  let issuer_secret = segment.issuer_secret()?;
  // The launcher secret its starter handed the anchor (A-285), published before the first daemon: a launcher-class
  // proof verifies under it as under the issuer secret. `None` when the starter gave none.
  let launcher_secret = segment.launcher_secret()?;
  // The client session secret (`crate::session_key`), published before any shard starts: a client's key is checked
  // on the shard that serves it.
  let session_secret = crate::session_key::secret_of(&segment)?;
  // The detection this node's anchor gives it, read at boot as the control shard reads it at every beat (A-574,
  // `fleet::refresh_supervision`): the widest window the watchdog judges this daemon's shards by, then the lapse bound
  // the anchor publishes. Before any peer's detector judges the node, its failover delay is that and the recovery.
  let mut failover = slates_cluster::failover::Failover::new();
  failover.supervise(
    crate::watchdog::widest_window_ns(config_shards),
    segment
      .supervision()
      .ok()
      .and_then(|supervision| supervision.lapse_bound_ns()),
  );
  let failover_ns = crate::fleet::failover_in_periods(failover.estimate().total_ns());
  // The attachment counter starts past every attachment recovered with the partition — a host mount's
  // survives a restart (AUD-01) — so a fresh mint never meets a kept record's id.
  let next_attachment = verbs::next_attachment_counter(db.partition());
  let mut state = ShardState {
    shard,
    partition,
    config: config.clone(),
    segment,
    issuer_secret,
    launcher_secret,
    session_secret,
    seal_root,
    seal_state: sealing,
    seal_recipient: None,
    pairs_delivered: std::collections::BTreeMap::new(),
    member_anchors: std::collections::BTreeMap::new(),
    peer_recipients: std::collections::BTreeMap::new(),
    content,
    purge_seen_allocations: 0,
    content_sweep: crate::content_ranges::Sweep::default(),
    push_turns: crate::git_push::PushTurns::default(),
    burst_seen: None,
    last_burst: 0,
    refill_settled: None,
    give_back_rate: GiveBackRate::default(),
    content_range,
    delta_range: delta_range(config, partition, content_range),
    journal: slates_vfs::checkpoint_log::Journal::default(),
    checkpoint_buffer: Vec::new(),
    published_keys: std::collections::BTreeSet::new(),
    deferred_kept: false,
    published_held: crate::content_holder::PublishedHeld::default(),
    write_log,
    writes_lost: recovered_writes.everything_lost,
    replay: recovered_writes.records,
    pending_replies: std::collections::BTreeMap::new(),
    recovered_replies: std::collections::BTreeMap::new(),
    dax_ranges,
    inherited_dax,
    adopted_dax: std::collections::BTreeMap::new(),
    write_verifier: now.to_be_bytes(),
    nfs_v4: None,
    nfs_v4_files: None,
    recall_queues: std::collections::BTreeMap::new(),
    nfs_v4_instance: None,
    nfs_calls: slates_bridge_nfs::v4::replay::CallRecords::new(config.nfs_v4.record_bytes),
    nfs_call: None,
    nfs_pending_base: None,
    held_nfs_replies: std::collections::BTreeMap::new(),
    held_nfs_replies_changed: false,
    recovered_nfs_replies: std::collections::BTreeMap::new(),
    db,
    fleet,
    durability_shortfall,
    origin_anchor,
    landing: crate::landing::LandingState::default(),
    image: crate::image::ImageState::default(),
    configured_members: Vec::new(),
    departed_owed: Vec::new(),
    departures_owed: false,
    departures_retry_at: None,
    #[cfg(target_os = "linux")]
    held_copies_owed: false,
    direct_map: crate::direct_map::DirectMapState::default(),
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
    unrecovered: std::collections::BTreeMap::new(),
    yielded: std::collections::BTreeSet::new(),
    relinquished: Vec::new(),
    candidate_claims: std::collections::BTreeMap::new(),
    #[cfg(target_os = "linux")]
    fuse_mounts: std::collections::BTreeMap::new(),
    #[cfg(target_os = "linux")]
    learned_fuse: std::collections::BTreeMap::new(),
    #[cfg(target_os = "linux")]
    fuse_helpers: crate::helper_runs::HelperRuns::default(),
    #[cfg(target_os = "macos")]
    own_nfs_mounts: std::collections::BTreeMap::new(),
    #[cfg(target_os = "macos")]
    unmount_watch: false,
    #[cfg(windows)]
    winfsp_mounts: std::collections::BTreeMap::new(),
    #[cfg(windows)]
    inherited_winfsp,
    #[cfg(windows)]
    adopt_winfsp: Vec::new(),
    stale_fuse_mounts: Vec::new(),
    #[cfg(unix)]
    inherited_fuse: holds.devices,
    #[cfg(target_os = "linux")]
    inherited_copies: holds.copies,
    #[cfg(target_os = "linux")]
    adopt_fuse: Vec::new(),
    adopt_shards: Vec::new(),
    #[cfg(windows)]
    inherited_resync,
    snapshot_views: std::collections::BTreeMap::new(),
    #[cfg(unix)]
    guest_devices: Vec::new(),
    shards_streams: std::collections::BTreeMap::new(),
    stream_handoffs: std::collections::BTreeMap::new(),
    #[cfg(target_os = "macos")]
    stream_offers: std::collections::BTreeMap::new(),
    #[cfg(target_os = "macos")]
    map_offers: std::collections::BTreeMap::new(),
    #[cfg(target_os = "macos")]
    route_exchanges: 0,
    clock,
    served: 0,
    refusals: std::collections::BTreeMap::new(),
    deferred: Vec::new(),
    server_task: None,
    dead_clients: std::collections::VecDeque::new(),
    reaper_task: None,
    capture_deadlines: crate::deadlines::CaptureDeadlines::new(),
    deadline_task: None,
    base_hints: crate::base_hints::BaseHints::default(),
    deadline_armed: None,
    destroy_retry_at: None,
    scatters: std::collections::BTreeMap::new(),
    grant_scatters: std::collections::BTreeMap::new(),
    shards: config_shards.to_vec(),
    recovered,
    booted_ns: now,
    status_scatters: std::collections::BTreeMap::new(),
    last_work_ns: now,
    pending_forwards: std::collections::VecDeque::new(),
    greens: crate::greens::Greens::default(),
    works: std::collections::BTreeMap::new(),
    merge: crate::merge_service::MergeShardState::default(),
    ack_scatters: std::collections::BTreeMap::new(),
    scatter_refusals: std::collections::BTreeMap::new(),
    telemetry: slates_wire::observe::SpanSink::with_capacity(telemetry_capacity),
    // The shard's span opener folds in this node's member id and the partition, so every trace and
    // span id it mints is distinct across the daemon and the fleet without coordination (§4.14).
    tracer: slates_wire::observe::Tracer::new(host.0, partition),
    current_span: None,
    forward_hold_ns: 0,
    loop_run: 0,
    last_client_io_ns: 0,
    loop_io: false,
    reply_route: None,
    told_waiters: crate::told::ToldWaiters::default(),
    told_times: crate::told::ToldTimes::default(),
    nfs_writeback: crate::nfs_writeback::WritebackStates::default(),
    told_owed: None,
    current_request: None,
    volume_ids_minted: 0,
    acceptance_deferred: false,
    lease_waiters: crate::lease_wait::LeaseWaiters::default(),
    base_waits: crate::base_wait::BaseWaits::default(),
    work_home: crate::work_home::WorkHome::default(),
    forwarded_rings: std::collections::BTreeMap::new(),
    telemetry_quota: config.telemetry_spans_per_reply,
    last_drain_ns: now,
    placed_heads: std::collections::BTreeMap::new(),
    state_logs: crate::state_log::StateLogs::default(),
    formed_probe_peers: std::collections::BTreeSet::new(),
    plane: crate::member_task::PlaneState::default(),
    nfs_service: crate::nfs::ServiceTimes::default(),
    callbacks: crate::callback::Callbacks::default(),
    forward_lanes: crate::forward_lane::ForwardLanes::default(),
    staging: crate::staging::Staging::default(),
    member_boot_nonce: incarnation,
    member_process_nonce: keyed_word(&issuer_secret, b"slates/member-process/v1"),
    learned_members: std::collections::BTreeMap::new(),
    death_reports: std::collections::BTreeMap::new(),
    death_watch_started_ns: None,
    returned_members: std::collections::BTreeMap::new(),
    dialed_in: std::collections::BTreeMap::new(),
    authenticated_members: std::collections::BTreeSet::new(),
    council_death_watch: std::collections::BTreeMap::new(),
    discovery: None,
    enrolled: Vec::new(),
    demuxes: Vec::new(),
    holder_records: crate::tracked::Tracked::default(),
    live: crate::live_ship::LiveState::default(),
    seal_walk_quantum_ns: None,
    install_walk: None,
    install_walk_quantum_ns: None,
    live_turn_pause_ns: None,
    host_takeovers: std::collections::BTreeMap::new(),
    moves_out: std::collections::BTreeMap::new(),
    carries: std::collections::BTreeMap::new(),
    move_adoptions: std::collections::BTreeMap::new(),
    move_records: std::collections::BTreeMap::new(),
    moves_forgotten: std::collections::BTreeSet::new(),
    releasing: Vec::new(),
    config_refresh_wanted: false,
    council_learner_pace: crate::fleet::LearnerPace::default(),
    learner_fetch: crate::fleet::LearnerFetchLog::default(),
    root_learner_pace: crate::fleet::LearnerPace::default(),
    root_newer_seen: None,
    record_sessions: std::collections::BTreeMap::new(),
    record_dials: std::collections::BTreeMap::new(),
    link_waiters: std::collections::BTreeMap::new(),
    discovery_withhold_replies: false,
    shards_withhold_replies: false,
    live_state_withhold_replies: false,
    peer_paths: std::collections::BTreeMap::new(),
    probe_windows: crate::fleet::ProbeWindows::default(),
    probe_deaf_to: std::collections::BTreeSet::new(),
    injected_serve_delay_ns: None,
    injected_content_ack_delay_ns: None,
    injected_forward_silence: None,
    injected_takeover_silence: std::collections::BTreeSet::new(),
    injected_confirmation_withheld: false,
    injected_deaf_session: None,
    injected_discovery_hold: None,
    dead_session_dropped_at: std::collections::BTreeMap::new(),
    dead_session_recovered: None,
    injected_pair_withhold: false,
    injected_pair_withhold_to: None,
    served_sessions: std::collections::BTreeMap::new(),
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
    injected_resident: None,
    pressure_resident_baseline: None,
    pressure_sent: vec![(0, 0); config_shards.len()],
    pressure_task: None,
    pressure_asked: false,
    stop_watch: None,
    stop_watch_install,
    nfs_purges_refused: false,
    nfs_purge_at: None,
    nfs_purge_owed: false,
    delegation_lapse_at: None,
    idle_sweep_at: None,
    idle_sweeper: None,
    idle_sweep_running: false,
    idle_sweep_seen: (0, 0),
    placement_rearm_at: None,
    answers_given: crate::lease::AnswersGiven::default(),
    departed_owners: std::collections::BTreeMap::new(),
    green_retention: std::collections::BTreeMap::new(),
    read_ahead: crate::verbs::ReadAheadLedger::default(),
    council_timing: slates_cluster::timing::ElectionTiming::floor(),
    root_timing: slates_cluster::timing::ElectionTiming::floor(),
    failover,
    failover_ns,
    injected_failover_ns: None,
    attachment_paces: std::collections::BTreeMap::new(),
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
    work_routes: crate::work_route::WorkRoutes::default(),
    seals: std::collections::BTreeMap::new(),
    mirror_seals: std::collections::BTreeMap::new(),
    mirror_neighbourhood: Vec::new(),
    mirror_records: slates_db::mirror::MirrorRecords::new(),
    mirror_rounds: std::collections::BTreeMap::new(),
    mirror_seeded: std::collections::BTreeSet::new(),
    put_latency: crate::fleet::LatencyWindow::default(),
    mirror_put_latency: crate::fleet::LatencyWindow::default(),
    fetch_latency: crate::fleet::LatencyWindow::default(),
    put_outcomes: crate::fleet::PutOutcomes::default(),
    healer: crate::fleet::HealerCursor::default(),
    repairs: 0,
    pending_materializations: crate::state::PendingMaterializations::default(),
    making_taken_over: std::collections::BTreeMap::new(),
    making_volumes: std::collections::BTreeMap::new(),
    pending_catalogs: std::collections::BTreeMap::new(),
    fenced_greens: std::collections::BTreeMap::new(),
    vcs_stores: crate::vcs_store::VcsStores::with_windows(Box::new(
      crate::vcs_store::arena_windows::ChunkWindows::new(config.page),
    )),
    vcs_usage: slates_vcs::Ledger::new(config.clients_per_shard),
    #[cfg(unix)]
    hook_times: crate::vcs_hooks::HookTimesTable::default(),
    root_mount: None,
    pending_green_materializations: std::collections::BTreeMap::new(),
    pending_repositories: std::collections::BTreeMap::new(),
  };
  // The mounts a predecessor kept that this partition's catalog names, recorded as this daemon's own before anything
  // below reacquires a base or resumes a landing (A-227); then this shard's learning ends, and the process's crossing
  // checks, which wait for every shard's, go on.
  #[cfg(target_os = "linux")]
  crate::fuse::learn_kept(&mut state).map_err(ServerError::Refused)?;
  #[cfg(target_os = "macos")]
  crate::own_nfs::learn(&mut state).map_err(ServerError::Refused)?;
  #[cfg(unix)]
  drop(learning);
  // Windows learned its held volumes before any shard started; it carries no learning to let go.
  #[cfg(windows)]
  let () = learning;
  // The landing counter starts past every landing recovered with the partition (its records are
  // durable and the guard refuses a duplicate id), as the attachment counter does.
  state.landing.next_landing = verbs::next_landing_counter(state.db.partition());
  // The runtime grants, rebuilt from the durable ones in their recorded states (AUD-29-06).
  crate::landing::restore_grants(&mut state);
  // What the partition owed the audit when its predecessor stopped (a batch a crash cut off) is looked at a retry
  // window into the start, by the deadline keeper (A-265, A-554).
  let started_ns = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  crate::landing::arm_owed_audit_look(&mut state, started_ns);
  // The NFSv4 purges a predecessor journaled and its owners never took go out at once (A-570).
  crate::nfs::arm_purges_at_start(&mut state, started_ns);
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
        slates_machine::log_line!(
          "slates-server: the node's sealing recipient is unavailable: {e:?}"
        );
        None
      }
    };
  }
  slices.unit_done().await;
  rebuild_and_report(&mut state, shard, &mut slices).await;
  start_burst_measure(&mut state);
  #[cfg(windows)]
  crate::anchor_hold::install(
    windows_channel,
    *config_shards.first().ok_or(ServerError::NotOnShard)?,
  )
  .map_err(crate::anchor_hold::HoldRefusal::into_server)?;
  state::install(state);
  register_idle_refill(shard);
  // The FUSE mounts whose devices the anchor held, served again (A-61); then the dead ones of a killed
  // predecessor counted and left for their users to unmount (AUD-29-64, A-102).
  #[cfg(target_os = "linux")]
  crate::fuse::adopt_held();
  // The Shards streams whose resync ends the anchor held, served again on fresh streams (A-316, A-339), and every held
  // device no record claimed released.
  crate::shards::adopt_held();
  #[cfg(windows)]
  crate::winfsp::adopt_held();
  #[cfg(target_os = "linux")]
  crate::fuse::leave_stale();
  // A holder could not renew while this daemon was not serving: every CLI attachment starts a fresh term now, before
  // the serve loop answers its first request and before the reaper's first sweep (A-384).
  let _ = state::with_state_counted(verbs::renew_termed_at_start);
  // Detached: the loop lives as long as the shard; nothing joins it (a joinable task stays in
  // the arena after it ends, which would hold the shard's shutdown).
  // A shard whose serve loop or idle sweep the arena refuses serves nothing: a typed initialization failure
  // (surfaced by the boot as `INIT_FAILURES`), never a silently loop-less shard (banned item 9).
  let serve = futures::spawn(serve_loop()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(serve);
  // The idle sweep (A-715): one pass a sweep's interval after what owes it, none while nothing does.
  let sweeper = futures::spawn(crate::idle_sweep::sweeper()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(sweeper);
  // The reaper of the clients whose watches saw them hang up (A-475): idle until the first report.
  let reaper = futures::spawn(crate::reap::reaper()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(reaper);
  // The keeper of the staging buffers' and status captures' deadlines (A-522): idle until the first is set.
  let keeper = futures::spawn(crate::deadlines::keeper()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(keeper);
  // The hint watcher (A-709): drains each overlay's base hints when the shard's set reports its watcher ready.
  let hints = futures::spawn(crate::base_hints::watcher()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(hints);
  // The pressure hold's sampler (A-570): on the control shard it samples at its start and at each report of the host's
  // memory pressure; elsewhere it ends at once.
  let sampler = futures::spawn(crate::pressure::sampler()).map_err(ServerError::Runtime)?;
  let _ = futures::detach(sampler);
  // The NFS mounts this shard learned from its predecessor end their attachments when the kernel lets them go, as a
  // mount bound later does (A-511): the shard's watch of the kernel's unmounts starts once it holds an own mount.
  #[cfg(target_os = "macos")]
  let _ = state::with_state_counted(crate::own_nfs::watch);
  Ok(slices.finish())
}

/// Rebuilds what the shard recovered into live state (`verbs::rebuild_recovered`, in `slices`) and says what it did in
/// the boot log: only what happened, never more.
async fn rebuild_and_report(
  state: &mut ShardState,
  shard: u16,
  slices: &mut crate::recovery_slices::RecoverySlices,
) {
  let rebuilt = verbs::rebuild_recovered(state, slices).await;
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
    slates_machine::log_line!(
      "slates-server: shard {shard}: recovered {} volumes from their images ({} refused), {} merge volumes (green chains and work logs replayed), reconciled out {} unrecovered local snapshots and {} attachments, trimmed {} unacknowledged snapshots the images carried, completed {} destroys in flight, corrected {} clone pins, reclaimed {} orphans no holder survived, took back {} writable copies, replayed {} logged writes",
      rebuilt.volumes,
      rebuilt.skipped,
      rebuilt.merge_volumes,
      rebuilt.snapshots_dropped,
      rebuilt.attachments_dropped,
      rebuilt.snapshots_trimmed,
      rebuilt.destroys_completed,
      rebuilt.pins_reconciled,
      rebuilt.orphans_reclaimed,
      rebuilt.copies_adopted,
      rebuilt.writes_replayed
    );
    slates_machine::log_line!(
      "slates-server: shard {shard}: held {} replicas again from the image{}",
      rebuilt.replicas,
      if rebuilt.replicas_refused {
        " (the held replicas did not recover: the hold starts empty)"
      } else {
        ""
      }
    );
  }
}

/// One pass of the idle sweep of this shard's volumes (A-99; run by `crate::idle_sweep` since A-715, by the reap loop's
/// tick before): every volume ages its writes, then the open extents of files idle a whole pass are sealed in
/// cooperative slices of `archive_slice_bytes` (the bytes a slice hashes in the shard's step quantum; AES-256-GCM seals
/// at a rate of the same order, measured 6.9–7.7 GB/s against BLAKE3's 9–10 here), yielding between slices so the shard
/// keeps serving, until none is left. Whether the pass settled the shard: it reached an idle purge that finished, and no
/// volume owes another pass; otherwise the sweep arms its next.
pub(crate) async fn seal_idle_content() -> bool {
  let aged = state::with_state(|s| {
    for (_, slot) in s.volumes.iter_mut_all() {
      slot.volume.age_writes();
    }
  });
  if aged.is_none() {
    return true;
  }
  let mut sealed_this_tick = 0u64;
  loop {
    let turn = state::with_state(|s| {
      let budget = s.config.archive_slice_bytes().max(1);
      let mut left = 0u64;
      let mut sealed = 0u64;
      for (_, slot) in s.volumes.iter_mut_all() {
        let outcome = slot
          .volume
          .seal_pending(&mut s.store, budget.saturating_sub(sealed));
        sealed = sealed.saturating_add(outcome.sealed_bytes);
        left = left.saturating_add(outcome.left);
        if sealed >= budget {
          break;
        }
      }
      (left, sealed)
    });
    let (left, sealed) = turn.unwrap_or((0, 0));
    sealed_this_tick = sealed_this_tick.saturating_add(sealed);
    if left == 0 {
      break;
    }
    futures::yield_now().await;
  }
  // A freed block a published image may still name is not released, and not zeroed, until the commit that releases it
  // (A-99, `slates_mem::arena`): a seal's moved chunk, a deleted file's blocks, a truncated tail. Each keeps its
  // plaintext in the anchor's RAM until the shard publishes again, and an idle shard publishes nothing of its own. So
  // a tick that sealed, or that finds frees still deferred, publishes once; a shard with neither publishes nothing.
  // Before 2026-10-06 a sealed or deleted idle file's plaintext stayed indefinitely
  // (`a_sealed_files_plaintext_leaves_the_content_object`, `a_deleted_files_plaintext_leaves_the_content_object`).
  // A refused publication is retried by the next tick.
  let deferred =
    state::with_state(|s| s.store.content.arena().deferred_bytes() > 0).unwrap_or(false);
  if sealed_this_tick > 0 || deferred {
    let _ = state::with_state_counted(crate::verbs::publish_shard);
    // The idle test asks whether the arena allocated since the previous tick, so this tick, which ends here, records
    // what it saw (A-214): read only when the purge next ran, it charged the sweep's own allocations to the next tick,
    // and an idle shard reached its purge and its reserve's refill a whole tick later than its idleness.
    let _ = state::with_state(|s| {
      s.purge_seen_allocations = s.store.content.arena().allocations();
      s.count(
        if sealed_this_tick > 0 {
          TICKS_SEALING
        } else {
          TICKS_DEFERRED
        },
        1,
      );
    });
    return false;
  }
  let purged = purge_if_idle().await;
  let owes = state::with_state(|s| {
    s.volumes
      .iter_mut_all()
      .any(|(_, slot)| slot.volume.sweep_owes())
  })
  .unwrap_or(false);
  purged && !owes
}

/// The idle shard's emptied write log, its pages given back (`crate::write_log::WriteLog::purge`).
fn purge_write_log(s: &mut ShardState) -> (usize, bool) {
  purge_write_log_timed(s, &mut shard_thread_cpu_ns)
}

/// [`purge_write_log`], its step timed by `cpu`, the shard thread's own CPU clock ([`shard_thread_cpu_ns`]; a
/// controlled one in the unit tests).
fn purge_write_log_timed(
  s: &mut ShardState,
  cpu: &mut impl FnMut() -> Option<u64>,
) -> (usize, bool) {
  let slice = give_back_slice(s);
  let started = cpu();
  let purged = match (s.content.as_mut(), s.write_log.as_mut()) {
    (Some(object), Some(log)) => log.purge(object, slice),
    _ => crate::write_log::Purged::default(),
  };
  learn_give_back_rate(s, purged.walked, spent_since(started, cpu()));
  (purged.bytes, purged.more)
}

/// The bytes one give-back step may walk (A-226): what the shard gave back in one step quantum of its own CPU, as the
/// steps before measured it ([`GiveBackRate`]), at least a page, so a step holds the shard about one quantum whatever
/// the memory and whatever the host's load.
fn give_back_slice(s: &ShardState) -> usize {
  let page = usize::try_from(s.config.geometry.page).unwrap_or(1).max(1);
  s.give_back_rate.slice_bytes.max(page)
}

/// The shard thread's own CPU clock now, nanoseconds (`slates_rt::registry::calling_thread_cpu`, the clock the
/// watchdog reads): what a give-back step is timed by, since a loaded host stretches the wall time a step takes and
/// not the CPU it spends. `None` where the platform reads none.
fn shard_thread_cpu_ns() -> Option<u64> {
  registry::calling_thread_cpu().map(|reading| reading.spent_ns)
}

/// The CPU a step spent between the readings `started` and `ended`, `None` unless both were read.
fn spent_since(started: Option<u64>, ended: Option<u64>) -> Option<u64> {
  started
    .zip(ended)
    .map(|(started, ended)| ended.saturating_sub(started))
}

/// Learns the next give-back slice from this step's: `walked` bytes for the `spent` nanoseconds of its shard thread's
/// own CPU, scaled to one step quantum ([`GiveBackRate::fold`]). A step that walked nothing, or whose CPU could not be
/// read, teaches nothing.
fn learn_give_back_rate(s: &mut ShardState, walked: usize, spent: Option<u64>) {
  let quantum = futures::step_budget_ns().unwrap_or(s.config.runtime.step_budget_ns);
  s.give_back_rate.fold(walked, spent, quantum);
}

/// The rate the idle give-back has learned of its own steps (A-226, A-491): the bytes one step may walk, and what the
/// steps folded since that slice was learned walked and spent. A step is timed by its shard thread's own CPU, which a
/// loaded host does not stretch, as the per-iteration budget holds a step to its quantum of that CPU. Timed by the wall
/// clock until A-491, a step the host descheduled read as slow by every moment it waited and cut the next slice short
/// by as much, so under load the slice followed the host's scheduling rather than the shard's work (the recovery
/// test's give-back case failed once under load on adacachy and passed on its rerun).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GiveBackRate {
  /// The bytes one step may walk: zero until a rate is learned, when a step takes a page.
  pub(crate) slice_bytes: usize,
  /// The bytes the steps folded since the slice was learned walked.
  walked: u64,
  /// The CPU those steps spent, nanoseconds.
  spent_ns: u64,
}

impl GiveBackRate {
  /// Folds one step that walked `walked` bytes and spent `spent` nanoseconds of its shard thread's CPU (`None` where
  /// the thread's clock reads nothing), learning the slice, the bytes one `quantum_ns` of that CPU walks, once the
  /// steps folded since the last have spent CPU the clock shows. A clock coarser than a step (Windows charges thread
  /// time a scheduler tick at a time) shows most steps' CPU as none and one step's as a whole tick, so the steps are
  /// folded until one shows any: their bytes over the CPU shown.
  pub(crate) fn fold(&mut self, walked: usize, spent: Option<u64>, quantum_ns: u64) {
    let Some(spent) = spent.filter(|_| walked > 0) else {
      return;
    };
    self.walked = self
      .walked
      .saturating_add(u64::try_from(walked).unwrap_or(u64::MAX));
    self.spent_ns = self.spent_ns.saturating_add(spent);
    if self.spent_ns == 0 {
      return;
    }
    let per_quantum = u128::from(self.walked)
      .saturating_mul(u128::from(quantum_ns.max(1)))
      .checked_div(u128::from(self.spent_ns))
      .unwrap_or(0);
    self.slice_bytes = usize::try_from(per_quantum).unwrap_or(usize::MAX);
    self.walked = 0;
    self.spent_ns = 0;
  }
}

/// The idle steps taken so far on this shard.
fn idle_ticks(s: &ShardState) -> u64 {
  s.refusals.get(TICKS_IDLE).copied().unwrap_or(0)
}

/// Format: [`GIVE_BACK_PROBE`] while a probed give-back runs.
const GIVE_BACK_PROBE_RUNNING: u64 = u64::MAX;
/// Test support ([`Daemon::give_back_beside`]): the steps a probed give-back took, or [`GIVE_BACK_PROBE_RUNNING`].
static GIVE_BACK_PROBE: AtomicU64 = AtomicU64::new(GIVE_BACK_PROBE_RUNNING);

/// [`Daemon::give_back_beside`]'s task on the shard.
async fn probe_give_back(neighbour: bool) {
  if neighbour && let Ok(task) = futures::spawn(a_neighbour_taking_turns()) {
    let _ = futures::detach(task);
  }
  let before = state::with_state(|s| idle_ticks(s)).unwrap_or(0);
  let _settled = purge_if_idle().await;
  let after = state::with_state(|s| idle_ticks(s)).unwrap_or(before);
  GIVE_BACK_PROBE.store(after.saturating_sub(before), Ordering::Release);
}

/// Takes a turn on the shard until the probed give-back has finished, allocating nothing.
async fn a_neighbour_taking_turns() {
  while GIVE_BACK_PROBE.load(Ordering::Acquire) == GIVE_BACK_PROBE_RUNNING {
    futures::yield_now().await;
  }
}

/// What [`Daemon::give_back_recovery_by_steps`] measured (test support, A-226).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GiveBackSteps {
  /// Recovery bytes given back over every shard.
  pub returned: u64,
  /// The give-back steps taken.
  pub steps: u64,
  /// The CPU each step held its shard, nanoseconds, off the shard thread's own clock, sorted; `None` where it cannot be
  /// read.
  pub step_cpu_ns: Option<Vec<u64>>,
  /// The step quantum the shards hold their work to, nanoseconds (the largest over the shards).
  pub quantum_ns: u64,
}

/// Counter: content bytes an idle shard gave back to the OS (A-105).
pub const CONTENT_PURGED: &str = "content.purged_bytes";

/// Counter: recovery memory an idle shard gave back because no restart reads it (A-117).
pub const RECOVERY_RETURNED: &str = "recovery.returned_bytes";

/// Gives back one slice of an idle shard's recovery memory that no restart reads (A-117): the checkpoint slot older than
/// the committed one, and its delta log past the tail, each only as far as it was written since its last give-back
/// (`Journal::dead_ranges`, A-226), taken from the top of each down so the journal can record a slice at a time. A
/// slice is the bytes the OS gave back in one step quantum the last time, measured here (`give_back_slice`), so a
/// give-back of memory a predecessor may have written (18 GB a shard on a 128 GiB Mac) holds the shard one quantum a
/// step, not 18 to 21 ms. A busy shard never runs this. A range the object refuses stays owed and is asked for at the
/// next give-back. The bytes given back, and whether any is left.
fn return_dead_recovery(s: &mut ShardState) -> (usize, bool) {
  return_dead_recovery_timed(s, &mut shard_thread_cpu_ns)
}

/// [`return_dead_recovery`], its step timed by `cpu`, the shard thread's own CPU clock ([`shard_thread_cpu_ns`]; a
/// controlled one in the unit tests).
fn return_dead_recovery_timed(
  s: &mut ShardState,
  cpu: &mut impl FnMut() -> Option<u64>,
) -> (usize, bool) {
  let (slots_start, slots_end) = s.content_range;
  let (log_start, log_end) = s.delta_range;
  let Some(dead) = s.journal.dead_ranges(
    slots_end.saturating_sub(slots_start),
    log_end.saturating_sub(log_start),
  ) else {
    return (0, false);
  };
  let slice = give_back_slice(s);
  let started = cpu();
  let Some(object) = s.content.as_mut() else {
    return (0, false);
  };
  let mut left = slice;
  // The top `left` bytes of `(offset, len)` within the region at `base`: given back, the offset they start at (what
  // the journal records), and the bytes the object took. `None` when the object refused.
  let mut give_back = |base: usize, (offset, len): (usize, usize)| -> Option<(usize, usize)> {
    let piece = len.min(left);
    let from = offset.saturating_add(len).saturating_sub(piece);
    if piece == 0 {
      return Some((from, 0));
    }
    let bytes = object.discard(base.checked_add(from)?, piece).ok()?;
    left = left.saturating_sub(piece);
    Some((from, bytes))
  };
  let stale_slot = give_back(slots_start, dead.stale_slot);
  let log_past_tail = give_back(log_start, dead.log_past_tail);
  s.journal.given_back_from(
    stale_slot.map(|(from, _)| from.saturating_sub(dead.stale_slot.0)),
    log_past_tail.map(|(from, _)| from),
  );
  learn_give_back_rate(s, slice.saturating_sub(left), spent_since(started, cpu()));
  let returned = stale_slot
    .map_or(0, |(_, bytes)| bytes)
    .saturating_add(log_past_tail.map_or(0, |(_, bytes)| bytes));
  s.count(
    RECOVERY_RETURNED,
    u64::try_from(returned).unwrap_or(u64::MAX),
  );
  let more = stale_slot.is_some_and(|(from, _)| from > dead.stale_slot.0)
    || log_past_tail.is_some_and(|(from, _)| from > dead.log_past_tail.0);
  (returned, more)
}

/// Gives an idle shard's resident free content, and its emptied write log's pages, back to the OS (A-105): when the
/// content arena allocated nothing since the last sweep pass (`crate::idle_sweep`), its free blocks are purged a slice at a time, yielding
/// between slices so a request that arrives meanwhile is served first. Decay-based, as jemalloc purges dirty pages after
/// a quiet interval: a busy shard keeps its zeroed free blocks resident for the allocations that follow, an idle one
/// stops costing RAM for what it no longer holds. Before 2026-10-06 nothing ever went back: a deleted 128 MiB file left
/// the content object at 168 MiB allocated.
///
/// Except its reserve (A-214): the free blocks the next allocations of a chunk window would take, as many bytes as the
/// shard's last write burst grew its content by ([`prefault_reserve`]). The purge keeps them, and once it is done the
/// refill faults in those of them not resident, a slice of `quantum / page_fault_ns` pages at a time, so a burst after
/// idle writes onto resident pages: a large write over fresh pages spent 41.5% of the shard faulting them in (CI run
/// 41986870989). The work stops as soon as a request makes the shard allocate.
async fn purge_if_idle() -> bool {
  loop {
    let step = state::with_state(|s| {
      let allocations = s.store.content.arena().allocations();
      let idle = allocations == s.purge_seen_allocations;
      s.purge_seen_allocations = allocations;
      if !idle {
        s.count(TICKS_BUSY, 1);
        return None;
      }
      s.count(TICKS_IDLE, 1);
      note_burst(s, allocations);
      let keep = prefault_reserve(s);
      let slice_blocks = usize::try_from(s.config.archive_slice_bytes())
        .unwrap_or(usize::MAX)
        .checked_div(s.config.discard_from_bytes.max(1))
        .unwrap_or(0)
        .max(1);
      let purged = s
        .store
        .content
        .arena_mut()
        .purge_keeping(slice_blocks, &keep);
      let (log, recovery_left) = if purged.more {
        (0, false)
      } else {
        let (recovery, recovery_left) = return_dead_recovery(s);
        let (log, log_left) = purge_write_log(s);
        (log.saturating_add(recovery), recovery_left || log_left)
      };
      let bytes = purged.bytes.saturating_add(log);
      s.count(CONTENT_PURGED, u64::try_from(bytes).unwrap_or(u64::MAX));
      // The give-backs' slices come before the reserve's refill (A-226 after A-214): while any is left, the next turn
      // gives back another.
      if purged.more || recovery_left {
        return Some(true);
      }
      let pages = s.config.prefault_slice_pages();
      let refilled = s.store.content.arena_mut().prefault(&keep, pages);
      s.count(
        CONTENT_PREFAULTED,
        u64::try_from(refilled.bytes).unwrap_or(u64::MAX),
      );
      Some(refilled.more)
    })
    .flatten();
    if step != Some(true) {
      // Settled once a step found the shard idle and left nothing; a busy shard owes the next pass.
      return step == Some(false);
    }
    // Whatever ran across this yield is work of the shard's, and this is only keeping RAM tidy: the rest waits for the
    // next tick (one step a tick, at the worst). The arena's idle test cannot see such work when it allocates nothing,
    // as a granted landing does, and a step of a whole quantum between each of its short ones made it 4 to 5 times
    // slower under load (A-226). The poll this replaced stays gone: a step is no longer than it was.
    let polls_before = futures::polls_so_far();
    futures::yield_now().await;
    let others_ran = polls_before
      .zip(futures::polls_so_far())
      .is_some_and(|(before, after)| after.saturating_sub(before) > 1);
    if others_ran {
      let _ = state::with_state_counted(|s| s.count(GIVE_BACK_LEFT, 1));
      return false;
    }
  }
}

/// Counter: give-backs that left the rest for the next sweep pass because another task ran on the shard (A-226), the
/// non-vacuity count of the check in [`purge_if_idle`].
pub const GIVE_BACK_LEFT: &str = "content.give_back.left_for_next_tick";

/// Counter: sweep passes that sealed content, and so published and left the purge for a later pass (A-105).
pub const TICKS_SEALING: &str = "content.ticks.sealing";
/// Counter: sweep passes that found frees deferred behind the committed image, and so published and left the purge for a
/// later tick (A-64, A-105).
pub const TICKS_DEFERRED: &str = "content.ticks.deferred";
/// Counter: purge steps that found the arena had allocated since the previous sweep pass (A-105).
pub const TICKS_BUSY: &str = "content.ticks.busy";
/// Counter: purge steps that found the shard idle: the purge ran, and the reserve's refill (A-105, A-214).
pub const TICKS_IDLE: &str = "content.ticks.idle";

/// Counter: free content bytes an idle shard faulted in for its reserve (A-214), the refill's non-vacuity count.
pub const CONTENT_PREFAULTED: &str = "content.prefaulted_bytes";

/// Ends a busy period at an idle sweep pass (A-214): when the arena allocated or freed since the last idle pass, the
/// period's net change of allocated bytes decides the reserve. A period that grew the content is the last write burst,
/// its net growth; one that shrank it takes what it freed off the burst, so a written file deleted later takes its
/// reserve with it. Before 2026-11-05 a period ended only when the allocation count moved, which a free does not move:
/// a delete-only period left the reserve the written file's size, and it kept the deleted file's own pages resident
/// (A-205's give-back test on both Windows lanes, main CI 42119714654;
/// `a_file_deleted_after_its_burst_went_idle_takes_its_reserve_with_it`). The measure starts when the shard has
/// recovered (`init_shard`); without one, the first idle tick only takes it.
fn note_burst(s: &mut ShardState, allocations: u64) {
  let allocated = s.store.content.arena().allocated_bytes();
  match s.burst_seen {
    Some((seen, before)) if seen == allocations && before == allocated => {}
    Some((_, before)) => {
      s.last_burst = if allocated >= before {
        allocated.saturating_sub(before)
      } else {
        s.last_burst
          .saturating_sub(before.saturating_sub(allocated))
      };
      s.burst_seen = Some((allocations, allocated));
      s.refill_settled = None;
    }
    None => s.burst_seen = Some((allocations, allocated)),
  }
}

/// Starts the burst measure from the content the shard holds once recovered (A-214): what a restart adopted is not a
/// write burst, and a write made before the first idle tick is.
fn start_burst_measure(state: &mut ShardState) {
  state.burst_seen = Some((
    state.store.content.arena().allocations(),
    state.store.content.arena().allocated_bytes(),
  ));
}

/// Registers the reserve's refill as shard `shard`'s idle work (A-214). Refused, it still refills at each idle reap
/// tick, and says so.
fn register_idle_refill(shard: u16) {
  if let Some(Err(refusal)) =
    registry::with_current(|ctx| ctx.set_idle_work(Box::new(refill_slice)))
  {
    slates_machine::log_line!(
      "slates-server: shard {shard}: the reserve's idle refill was refused ({refusal}); it refills at idle sweep passes only"
    );
  }
}

/// The shard's idle work (A-214, registered with the runtime at the shard's start): one slice of its reserve's refill,
/// `quantum / page_fault_ns` pages, whenever the shard has nothing else to do and its idle spin has run out, so a
/// burst's pages are faulted in the moment the shard goes quiet rather than at the next idle sweep pass, which the incumbent
/// harness's second-long pauses never reached (A-105's test needs a whole quiet tick). Whether it faulted anything or
/// has more to fault; once it finds nothing it stays quiet until the arena allocates again.
fn refill_slice() -> bool {
  state::with_state(|s| {
    let allocations = s.store.content.arena().allocations();
    if s.refill_settled == Some(allocations) {
      return false;
    }
    let keep = prefault_reserve(s);
    let pages = s.config.prefault_slice_pages();
    let refilled = s.store.content.arena_mut().prefault(&keep, pages);
    s.count(
      CONTENT_PREFAULTED,
      u64::try_from(refilled.bytes).unwrap_or(u64::MAX),
    );
    if !refilled.more {
      s.refill_settled = Some(allocations);
    }
    refilled.bytes > 0 || refilled.more
  })
  .unwrap_or(false)
}

/// Derived (A-214): an idle shard's reserve, `min(max(last write burst, the growth since the last idle tick),
/// reserve_per_shard, the arena's free bytes)`, as the
/// free blocks the next allocations of a chunk window would take (`ChunkArena::upcoming`): the burst bounded by the
/// shard's memory share and by what its arena holds free.
fn prefault_reserve(s: &ShardState) -> Vec<slates_mem::arena::KeptRun> {
  let bytes = prefault_reserve_bytes(s);
  if bytes == 0 {
    return Vec::new();
  }
  s.store
    .content
    .arena()
    .upcoming(s.store.content.chunk_bytes(), bytes)
}

/// The bytes of [`prefault_reserve`]: what an idle shard keeps pre-faulted (A-214), also reported per shard by the
/// status's content ranges (A-745).
pub(crate) fn prefault_reserve_bytes(s: &ShardState) -> usize {
  let arena = s.store.content.arena();
  // The busy period under way counts as soon as it has grown past the last one: an idle moment inside it, or right
  // after it, comes before the sweep pass that closes it.
  let running = s.burst_seen.map_or(0, |(_, before)| {
    arena.allocated_bytes().saturating_sub(before)
  });
  s.last_burst
    .max(running)
    .min(usize::try_from(s.config.reserve_per_shard).unwrap_or(usize::MAX))
    .min(arena.free_bytes())
}

/// The pressure hold across a daemon's shards (§4.2, admission.md §5.5), from the host's free memory at the daemon's
/// first sample (`baseline`) and now (`available`), the daemon's own resident growth since then (`own_growth`), and its
/// whole content capacity (`capacity`). The invariant it keeps: what the daemon can still fill must fit in what the
/// host has free, `capacity − filled ≤ available`. So the hold is the smaller of:
/// - the shortfall: the host's drop below the baseline that the daemon did not cause itself (its own growth is
///   committed content, charged already: 2026-10-05);
/// - the daemon's room — its capacity not yet filled — less what the host still has free.
///
/// On a host with no slack at the first sample (as much free as the daemon could fill, or less) the second never binds
/// and the hold is the shortfall, as before. On a roomy host, memory other processes take from the slack beyond
/// anything the daemon could fill holds nothing: a 64 MiB daemon refused a 1 MiB create while the host had 92% of its
/// memory free, because another process's swing of a few hundred megabytes counted as pressure (2026-10-09).
pub(crate) fn pressure_hold(baseline: u64, available: u64, own_growth: u64, capacity: u64) -> u64 {
  let shortfall = baseline
    .saturating_sub(available)
    .saturating_sub(own_growth);
  let room = capacity.saturating_sub(own_growth);
  shortfall.min(room.saturating_sub(available))
}

/// Samples the host's available memory and withholds the daemon's pressure hold from admission (§4.2; admission.md
/// §5.5), run by the control shard's sampler at its start and at each report of the host's memory pressure
/// (`crate::pressure`, A-570; the reap loop's tick ran it once a liveness window until then) — the design's
/// "cheap-refresh", one sampler a daemon (before 2026-10-09 every shard sampled and sent its own reading to every
/// shard, N² cross-shard messages a second on a daemon at rest, each waking a parked shard).
///
/// The hold is the host-wide shortfall below the boot baseline, less the daemon's own resident growth since then (that
/// is committed content, charged already), and never more than the daemon's unfilled room exceeds the host's free
/// memory ([`pressure_hold`]). It is the daemon's, withheld once, wherever the room it holds back lies:
/// - first from the content pool, as extents the hold claims under its own owner (`content_pool::hold_toward`), so no
///   shard can grow into them;
/// - what the pool's whole extents miss, from the free room the shards already hold (each shard's capacity beyond its
///   commitments and headroom), in proportion to that room, as each shard's ledger hold ([`apportion`]);
/// - and what neither covers, as a debt every shard is told, taken on as hold by whichever shard next grows from the
///   pool (`slates_mem::budget::Pressure`).
///
/// Before 2026-10-09 the hold was divided evenly among the shards and each share was claimed only when its own shard
/// admitted, so with the pool (A-98) a volume on one shard of five could fill all but a fifth of it. The hold shrinks
/// admittable only, so a new reservation is refused under pressure while an admitted volume's within-entitlement writes
/// land, and it releases to zero as the sample recovers toward the baseline. A host with no memory sampler
/// (`memory_available_now` is `None` on an unsupported platform) leaves the hold at zero — the budget then admits
/// exactly as before, no worse than not sampling. The read is one cheap syscall or `/proc` line per cadence, not per
/// request; the shards are asked for their free room only while the pool cannot cover the hold.
pub(crate) async fn refresh_pressure_hold() {
  let Some((origin, shards, hold)) = sample_pressure_hold() else {
    return;
  };
  let pooled = state::with_state(|s| {
    let geometry = crate::content_pool::PoolGeometry::of(&s.config);
    crate::content_pool::hold_toward(&s.segment, &geometry, hold)
  });
  let pooled = match pooled {
    Some(Ok(bytes)) => bytes,
    Some(Err(_)) => {
      state::with_state_counted(|s| s.count(PRESSURE_POOL_REFUSED, 1));
      0
    }
    None => return,
  };
  let residual = hold.saturating_sub(pooled);
  let (holds, debt) = if residual == 0 {
    (vec![0; shards.len()], 0)
  } else {
    let free = free_room_of(origin, &shards).await;
    let holds = apportion(residual, &free);
    let placed = holds
      .iter()
      .fold(0u64, |total, part| total.saturating_add(*part));
    (holds, residual.saturating_sub(placed))
  };
  send_pressure_holds(origin, &shards, &holds, debt);
}

/// The control shard's sample of the pressure hold (§4.2): this shard, the daemon's shards and the daemon-wide hold,
/// or `None` off the control shard or on a host with no memory sampler.
fn sample_pressure_hold() -> Option<(u16, Vec<u16>, u64)> {
  if state::with_state(|s| crate::verbs::is_control_shard(s)) != Some(true) {
    return None;
  }
  let injected = state::with_state(|s| s.injected_available).flatten();
  let available = injected.or_else(slates_machine::facts::Facts::memory_available_now)?;
  // This daemon's first sample fixes its baseline (kept by the sampling shard, so each daemon in a
  // process has its own); a later sample above it only lowers the shortfall.
  let resident = state::with_state(|s| s.injected_resident)
    .flatten()
    .or_else(slates_machine::facts::Facts::resident_now);
  let (origin, shards, baseline, resident_baseline, reserve) = state::with_state(|s| {
    let baseline = *s.pressure_baseline.get_or_insert(available);
    let resident_baseline = match resident {
      Some(now) => Some(*s.pressure_resident_baseline.get_or_insert(now)),
      None => s.pressure_resident_baseline,
    };
    (
      s.shard,
      s.shards.clone(),
      baseline,
      resident_baseline,
      s.config.reserve_per_shard,
    )
  })?;
  // The daemon's own growth since the baseline (its content, metadata and heap) is not pressure: those bytes are
  // committed already, so holding them back as well charged every stored byte twice (2026-10-05: one volume refused
  // at 244 MB of a 358 MB pool under a 1 GiB cap, held back 111 MB per shard for memory it had itself filled).
  let own_growth = resident
    .zip(resident_baseline)
    .map_or(0, |(now, then)| now.saturating_sub(then));
  let capacity = reserve.saturating_mul(u64::try_from(shards.len()).unwrap_or(u64::MAX));
  let hold = pressure_hold(baseline, available, own_growth, capacity);
  Some((origin, shards, hold))
}

/// Each shard's free room (§4.2), in shard order: the capacity it holds beyond its commitments and its operation
/// headroom, asked of each shard (`ShardBudget::mark_room`, which marks the read so later growth takes on the debt the
/// refresh sends back). Zero for a shard that does not answer within a heartbeat (counted), and for one whose hold a test
/// pinned, which the sampler leaves alone.
async fn free_room_of(origin: u16, shards: &[u16]) -> Vec<u64> {
  let mut free = Vec::with_capacity(shards.len());
  for &shard in shards {
    // A wall bound, by its reason (A-435): the sampler's question (`crate::pressure`, A-570); a shard that does not
    // answer within a heartbeat reads as no room (counted), and the hold it would have taken goes to the others or to
    // the debt the next growth takes on, so nothing is lost, while a wait in the shard's own time would hold the
    // sample behind one busy shard, past the report that asked for it.
    let room = crate::xshard::call_within(
      origin,
      shard,
      |s| {
        if s.pressure_pinned {
          return 0;
        }
        s.store.budget.mark_room()
      },
      HEARTBEAT_NS,
    )
    .await;
    if room.is_none() {
      state::with_state_counted(|s| s.count(PRESSURE_ROOM_UNANSWERED, 1));
    }
    free.push(room.unwrap_or(0));
  }
  free
}

/// Divides `residual` bytes of a pressure hold among the shards in proportion to the free room each holds (`free`, in
/// shard order), never more than a shard's room (§4.2). While the rooms can absorb it the parts sum to `residual`
/// exactly: the flooring's remainder, fewer bytes than there are shards, goes a byte each to shards with room left, in
/// order. With `residual` at or past the rooms' total, each shard gives up all its room.
pub(crate) fn apportion(residual: u64, free: &[u64]) -> Vec<u64> {
  let total = free
    .iter()
    .fold(0u128, |sum, room| sum.saturating_add(u128::from(*room)));
  if total == 0 {
    return vec![0; free.len()];
  }
  if u128::from(residual) >= total {
    return free.to_vec();
  }
  let mut holds: Vec<u64> = free
    .iter()
    .map(|room| {
      let share = u128::from(residual)
        .saturating_mul(u128::from(*room))
        .checked_div(total)
        .unwrap_or(0);
      u64::try_from(share).unwrap_or(*room).min(*room)
    })
    .collect();
  let given = holds
    .iter()
    .fold(0u64, |sum, hold| sum.saturating_add(*hold));
  let mut left = residual.saturating_sub(given);
  for (hold, room) in holds.iter_mut().zip(free) {
    if left == 0 {
      break;
    }
    if *hold < *room {
      *hold = hold.saturating_add(1);
      left = left.saturating_sub(1);
    }
  }
  holds
}

/// Sends each shard its pressure hold and the debt (`ShardBudget::set_pressure`), only where they differ from the last
/// sent: a daemon at rest sends nothing. A send the target's admission bound refuses is counted and left unrecorded, so
/// the next refresh sends it again.
fn send_pressure_holds(origin: u16, shards: &[u16], holds: &[u64], debt: u64) {
  for (index, (&shard, &hold)) in shards.iter().zip(holds).enumerate() {
    let pressure = (hold, debt);
    if state::with_state(|s| s.pressure_sent.get(index).copied()).flatten() == Some(pressure) {
      continue;
    }
    let sent = crate::xshard::run_on(origin, shard, move |s| {
      if !s.pressure_pinned {
        s.store.budget.set_pressure(hold, debt);
      }
    });
    match sent {
      Ok(()) => {
        state::with_state(|s| {
          if let Some(slot) = s.pressure_sent.get_mut(index) {
            *slot = pressure;
          }
        });
      }
      Err(_) => {
        state::with_state_counted(|s| s.count(PRESSURE_SEND_REFUSED, 1));
      }
    }
  }
}

/// Counter: a pool owner word the pressure hold's claims or releases could not reach (a layout fault); the refresh
/// withholds what it could and tries again at the next one.
const PRESSURE_POOL_REFUSED: &str = "pressure.pool_refused";
/// Counter: a shard that did not answer the sampler's question of its free room within a heartbeat; nothing is taken
/// from it this refresh.
const PRESSURE_ROOM_UNANSWERED: &str = "pressure.room_unanswered";

/// Counter: a pressure hold the sampler could not send a shard (its admission bound refused the message); sent again at
/// the next refresh, so none is lost, but a refusal is never silent.
const PRESSURE_SEND_REFUSED: &str = "pressure.send_refused";

/// Establishes the Windows hold channel's owner before any shard initialization task can use it.
#[cfg(windows)]
fn configure_windows_hold_owner(present: bool, shards: &[ShardId]) -> Result<(), ServerError> {
  if present {
    let owner = shards.first().ok_or(ServerError::NotOnShard)?.0;
    crate::anchor_hold::set_owner(owner)
      .map_err(|error| ServerError::Anchor(slates_anchor::AnchorError::Hold(error)))?;
  }
  Ok(())
}

/// Ends the shard's FUSE mounts when its serve loop ends (§4.6 "Linux"; AUD-29-64): the loop is perpetual,
/// so it ends only when the daemon's stop cancels every task on the shard, and its drop runs on the shard's
/// own thread with the shard's state still installed. Each mount is ended, never unmounted (A-102): it answers every
/// call `ENOTCONN` until its user unmounts it, so no write by path reaches the disk beneath. Ended here, not by a
/// question sent ahead of the shutdown, which waits behind whatever the shard is running and answers the questions
/// queued before it.
#[cfg(target_os = "linux")]
struct EndMounts;

#[cfg(target_os = "linux")]
impl Drop for EndMounts {
  fn drop(&mut self) {
    // A fenced shard refuses every ordinary borrow: its mounts are still released, and their records end
    // at the next start's recovery (a record is never written on a fenced shard).
    if state::with_state(crate::fuse::end_all).is_none() {
      let _ = state::with_state_at_shutdown(crate::fuse::drop_devices);
    }
  }
}

/// Ends the shard's WinFsp mounts when its serve loop ends (§4.6 "Windows"), as `EndMounts` ends the FUSE mounts and
/// for the same reason: on the shard's own thread with its state installed. Each mount's drive is removed and its
/// thread joined (a drive letter has no directory beneath it to expose); a fenced shard writes no record, and the
/// records end at the next start's recovery.
#[cfg(windows)]
struct EndWinFspMounts;

#[cfg(windows)]
impl Drop for EndWinFspMounts {
  fn drop(&mut self) {
    if state::with_state(crate::winfsp::end_all).is_none() {
      let _ = state::with_state_at_shutdown(crate::winfsp::drop_all);
    }
  }
}

/// Ends the shard's Shards streams when its serve loop ends (§4.6, the Shards route; A-161), as `EndMounts` ends its
/// FUSE mounts and for the same reason: on the shard's own thread with its state installed, not by a question queued
/// behind its work. Each attachment's references are swept and its record removed; a fenced shard only closes the
/// streams, and their records end at the next start's recovery.
struct EndStreams;

impl Drop for EndStreams {
  fn drop(&mut self) {
    if state::with_state(crate::shards::end_all).is_none() {
      let _ = state::with_state_at_shutdown(crate::shards::drop_all);
    }
  }
}

/// The shard's server loop: a poller of its clients' rings; serves while there is work, keeps
/// polling for the idle window after its last work (§4.7 "Wake strategy": a shard polls while
/// any client has activity within the window), and idles past it. The window is the shard's own
/// (`ShardContext::idle_window_ns`): R9's floor on every platform, no estimate pricing it, and never past the idle spin's
/// ceiling (`config::NO_ESTIMATE_MULTIPLE`, `config::IDLE_SPIN_CEILING`; A-223, A-361).
async fn serve_loop() {
  #[cfg(target_os = "linux")]
  let _end_mounts = EndMounts;
  #[cfg(windows)]
  let _end_winfsp_mounts = EndWinFspMounts;
  let _end_streams = EndStreams;
  if let Some(task) = futures::current_task() {
    let _ =
      registry::with_current(|ctx| ctx.register_poller(task, Box::new(state::any_ring_ready)));
    state::with_state(|s| s.server_task = Some(task));
  }
  loop {
    let idle_window_ns = derived!(
      futures::idle_window_ns().unwrap_or(crate::config::PROVISION_FLOOR_NS),
      "the shard's idle window: provision_floor_ns, no estimate's multiple, under idle_spin_ceiling_ns",
      ["rt.idle_window_ns"]
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
      // A forward its owner's full control channel refused is kept for the next round (§4.3, "backpressure, never a
      // drop"). Before idling, the loop asks that owner to tell it when the channel has room and tries once more; it
      // idles only if that try is refused too, and the owner's next drain wakes it (`slates_rt::room`). Until
      // 2026-11-07 a kept forward had no wake at all, and a shard whose clients all waited on kept forwards slept
      // until some other request came: at 10,000 clients over one green, requests waited 10 to 42 s with the daemon
      // idle. Until 2026-11-17 the loop polled for the room once per idle window, a timer a window that grows with
      // the wake estimate
      // (`docs/bugs/2026-11-17-a-kept-forward-polled-for-room-and-its-test-timed-the-owners-queue.md`).
      let serve_instead = serve_instead
        || state::with_state_counted(verbs::ask_room_for_kept_forwards).unwrap_or(false);
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
    let Some(retained) = retained_client_ids(origin, shard.0).await else {
      INIT_FAILURES.fetch_add(1, Ordering::AcqRel);
      slates_machine::log_line!(
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

/// Recovers what spans partitions, before any client is admitted: the names' aliases (A-236), the works off their
/// green's shard, then every floor against the work it holds for (A-247).
async fn recover_works_across_partitions(origin: u16, shards: &[ShardId]) {
  let reconciled = reconcile_name_aliases(origin, shards).await;
  if reconciled > 0 {
    slates_machine::log_line!(
      "slates-server: {reconciled} name alias(es) reconciled with their clones at recovery"
    );
  }
  let owed = rebuild_remote_works(origin, shards).await;
  if owed > 0 {
    slates_machine::log_line!(
      "slates-server: {owed} work(s) left unbuilt at recovery: their shards did not answer twice; their records are kept, answering Unpublished, and asked for again as clients are let in"
    );
  }
  drop_pending_creations(origin, shards).await;
  let floors = reconcile_work_floors(origin, shards).await;
  if floors > 0 {
    slates_machine::log_line!(
      "slates-server: {floors} work floor(s) reconciled with their works at recovery"
    );
  }
}

/// Replays each work whose green lives on another partition through that green's owner (A-247), its log being read
/// from the work's partition, and installs the outcome on the work's shard. Every ask is made once more when its answer
/// was lost ([`ask_once_more`], A-513): the list is read from the work's shard, never taken
/// (`verbs::unbuilt_works`), and the install ends the work's record there (`verbs::install_replayed_work`), so a
/// second ask behind a first that ran finds what the first left. Taken, one lost answer left every work of that shard
/// unbuilt for the daemon's life: unserved, every change it acknowledged out of reach, and its floor holding its green's
/// checkpoint at its base. A work whose asks went unanswered twice keeps its record, answers `Unpublished`, and is asked
/// for again as clients are let in ([`ask_again_for_unbuilt_works`], A-533). Returns how many works were left so.
async fn rebuild_remote_works(origin: u16, shards: &[ShardId]) -> usize {
  let mut owed = 0usize;
  for shard in shards.iter().copied() {
    let Some(unbuilt) = ask_once_more(origin, shard.0, |s| crate::verbs::unbuilt_works(s)).await
    else {
      slates_machine::log_line!(
        "slates-server: shard {} did not list its works to rebuild at recovery",
        shard.0
      );
      continue;
    };
    for work in unbuilt {
      if !rebuild_remote_work(origin, shards, shard, work).await {
        owed = owed.saturating_add(1);
      }
    }
  }
  owed
}

/// One work of [`rebuild_remote_works`]: its log read on its shard, replayed through its green's owner, the outcome
/// installed on its shard. Whether every ask was answered.
async fn rebuild_remote_work(
  origin: u16,
  shards: &[ShardId],
  shard: ShardId,
  work: crate::work_home::Unbuilt,
) -> bool {
  let id = work.work;
  let Some(entries) = ask_once_more(origin, shard.0, move |s| {
    s.db.partition().work_log(id).to_vec()
  })
  .await
  else {
    return false;
  };
  // A work this node made as a holder replays over the node's replica of its green, on the control shard (A-697 part
  // e); one whose replica is not here yet waits for it, asked again when it is installed.
  if work.holder {
    let (green, created_at, here) = (work.green, work.created_at, shard.0);
    let Some(control) = shards.first() else {
      return false;
    };
    let Some(replayed) =
      ask_once_more(
        origin,
        control.0,
        move |s| match crate::work_home::holder_replay(s, (green, created_at), &entries, here) {
          crate::work_home::HolderReplay::Replayed(replayed) => Some(replayed),
          crate::work_home::HolderReplay::Later => None,
        },
      )
      .await
    else {
      return false;
    };
    let Some(replayed) = replayed else {
      return true;
    };
    return ask_once_more(origin, shard.0, move |s| {
      crate::verbs::install_replayed_work(s, &work, replayed);
    })
    .await
    .is_some();
  }
  let partition = crate::verbs::owner_of(crate::verbs::to_wire_volume(work.green));
  let Some(owner) = shards.get(usize::from(partition)) else {
    return false;
  };
  let (green, created_at) = (work.green, work.created_at);
  let Some(replayed) = ask_once_more(origin, owner.0, move |s| {
    crate::verbs::owner_replay(s, green, created_at, &entries)
  })
  .await
  else {
    return false;
  };
  ask_once_more(origin, shard.0, move |s| {
    crate::verbs::install_replayed_work(s, &work, replayed);
  })
  .await
  .is_some()
}

/// Format: the refusal-ledger name of a shard the retry of unbuilt works could not reach ([`ask_again_for_unbuilt_works`]):
/// the shard gone, or the carrier refusing the message.
const UNBUILT_ASK_REFUSED: &str = "merge.work_unbuilt_ask_refused";

/// As clients are let in (A-533): tells every shard to ask its works still recorded unbuilt of their greens' owners
/// again (`verbs::ask_owners_to_replay`), by a message each runs when it next runs. A work whose rebuild asks went
/// unanswered twice at recovery is so rebuilt when its shard and its green's owner run again, each run the event, and
/// answers `Unpublished` meanwhile; nothing waits, so the admission is not held, and no timer runs. A shard with no such
/// work does nothing.
fn ask_again_for_unbuilt_works(origin: u16, shards: &[ShardId]) {
  for shard in shards.iter().copied() {
    crate::xshard::run_on_waiting(
      origin,
      shard.0,
      crate::verbs::ask_owners_to_replay,
      UNBUILT_ASK_REFUSED,
    );
  }
}

/// Asks `shard` to run `work` in its own time, and once more when no answer came back (A-513, as A-505 asks a map's
/// send): the second ask runs behind the first on the shard's channel, so a first ask that ran is answered from what it
/// left there. Only for work that reads, or whose second run finds the record its first left.
async fn ask_once_more<T: Send + 'static>(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
) -> Option<T> {
  match crate::xshard::call_in_shard_time(origin, shard, work.clone(), LIVENESS_BUDGET_NS).await {
    Some(answer) => Some(answer),
    None => crate::xshard::call_in_shard_time(origin, shard, work, LIVENESS_BUDGET_NS).await,
  }
}

/// Ends every work creation a restart caught waiting for its green's owner (A-247): its name and id go back, and the
/// client's retry, finding no completion, creates it afresh. Any floor its admission left is dropped by the reconcile
/// after, its work never having been made.
async fn drop_pending_creations(origin: u16, shards: &[ShardId]) {
  for shard in shards.iter().copied() {
    let _ = crate::xshard::call_in_shard_time(
      origin,
      shard.0,
      |s| {
        let pendings: Vec<slates_db::catalog::PendingCreationRecord> =
          s.db.partition().pending_creations().cloned().collect();
        let mut names = Vec::new();
        for pending in pendings {
          // A holder's creation is released at its green's owner before it is dropped (A-697 part e2).
          if !crate::work_home::hold_for_release(s, &pending) {
            names.push(pending.name);
          }
        }
        for name in names {
          s.db.begin();
          let settled = crate::verbs::settle_creation(s, &name);
          if settled.is_err() || s.db.commit(&mut s.segment).is_err() {
            crate::verbs::reconcile_unpublished_effects(s);
          }
        }
      },
      LIVENESS_BUDGET_NS,
    )
    .await;
  }
}

/// Compares every work floor with the work it holds for (A-247): one whose work is gone is dropped, and one that names
/// a base the work no longer holds is moved to the work's. Only a crash between a work's step and its green's leaves
/// either. Run before any client is admitted. Returns how many it changed.
async fn reconcile_work_floors(origin: u16, shards: &[ShardId]) -> usize {
  let mut reconciled = 0usize;
  for shard in shards.iter().copied() {
    let Some(floors) = crate::xshard::call_in_shard_time(
      origin,
      shard.0,
      |s| s.db.partition().work_floors().collect::<Vec<_>>(),
      LIVENESS_BUDGET_NS,
    )
    .await
    else {
      continue;
    };
    for floor in floors {
      let partition = crate::verbs::owner_of(crate::verbs::to_wire_volume(floor.work));
      let Some(home) = shards.get(usize::from(partition)) else {
        continue;
      };
      let work = floor.work;
      let Some((exists, holds)) = crate::xshard::call_in_shard_time(
        origin,
        home.0,
        move |s| {
          (
            s.db.partition().volume(work).is_some(),
            s.works.get(&work).map(|w| w.base_version),
          )
        },
        LIVENESS_BUDGET_NS,
      )
      .await
      else {
        continue;
      };
      let change = match (exists, holds) {
        (false, _) => Some(None),
        (true, Some(base)) if base != floor.base => Some(Some(base)),
        // A record whose work could not be rebuilt keeps its floor: its base may yet be read.
        _ => None,
      };
      if let Some(base) = change {
        let (green, work) = (floor.green, floor.work);
        let moved = crate::xshard::call_in_shard_time(
          origin,
          shard.0,
          move |s| crate::verbs::owner_floor(s, green, work, base),
          LIVENESS_BUDGET_NS,
        )
        .await;
        if moved.is_some() {
          reconciled = reconciled.saturating_add(1);
        }
      }
    }
  }
  reconciled
}

/// A recovery's reconciliation of every name alias (A-236, `slates_db::alias::reconcile`): each shard's aliases asked
/// for, the volume each names asked of its partition, and the alias confirmed when that volume holds the name, released
/// when it does not. Run before any client is admitted, when no clone is between its steps. An alias whose question is
/// unanswered is left as it is, for the next start: a reserved one resolves nothing meanwhile. The aliases reconciled.
async fn reconcile_name_aliases(origin: u16, shards: &[ShardId]) -> usize {
  let mut reconciled = 0usize;
  for shard in shards {
    let Some(aliases) = crate::xshard::call_in_shard_time(
      origin,
      shard.0,
      |s| s.db.partition().name_aliases().cloned().collect::<Vec<_>>(),
      LIVENESS_BUDGET_NS,
    )
    .await
    else {
      continue;
    };
    for alias in aliases {
      let partition = crate::verbs::owner_of(slates_ipc::protocol::VolumeId {
        bytes: alias.volume.bytes,
      });
      let id = alias.volume;
      let volume = match shards.get(usize::from(partition)) {
        Some(owner) => {
          let Some(volume) = crate::xshard::call_in_shard_time(
            origin,
            owner.0,
            move |s| s.db.partition().volume(id).cloned(),
            LIVENESS_BUDGET_NS,
          )
          .await
          else {
            continue;
          };
          volume
        }
        // A partition this daemon does not have holds nothing.
        None => None,
      };
      let op = match slates_db::alias::reconcile(&alias, volume.as_ref()) {
        slates_db::alias::Reconciled::Keep => continue,
        slates_db::alias::Reconciled::Confirm => {
          slates_db::Op::NameAliasConfirmed { name: alias.name }
        }
        slates_db::alias::Reconciled::Release => {
          slates_db::Op::NameAliasReleased { name: alias.name }
        }
      };
      let settled = crate::xshard::call_in_shard_time(
        origin,
        shard.0,
        move |s| crate::verbs::alias_step(s, &op).is_ok(),
        LIVENESS_BUDGET_NS,
      )
      .await;
      if settled == Some(true) {
        reconciled = reconciled.saturating_add(1);
      }
    }
  }
  reconciled
}

/// Shard `shard`'s retained client-id high-water mark. The question is asked once and awaited one liveness window at a
/// time for as long as the shard is still working: it queues behind the shard's start, which runs recovery as one step
/// and may outlast a window (GAPS boot, 2026-10-05). After a window with no answer, the shard's thread must have used
/// CPU in it (its clock's progress moved) for the wait to go on, so a long recovery is waited out and a stuck shard is
/// refused after one quiet window. The wait is bounded by the start's own work, which is bounded (recovery is bounded
/// by the image it replays). `None` when the shard has no state (its start failed), the call is refused, or the shard
/// was quiet for a window, or its CPU clock cannot be read (no way to tell busy from stuck).
async fn retained_client_ids(origin: u16, shard: u16) -> Option<u32> {
  let call = crate::xshard::call_on(origin, shard, || {
    state::with_state(|state| {
      state
        .db
        .partition()
        .client_id_high_water(state.origin_anchor.0)
    })
  })
  .ok()?;
  let mut call = std::pin::pin!(call);
  let holder = registry::holder_of(shard);
  let progress_of = || {
    holder
      .and_then(registry::shard_cpu)
      .map(|reading| reading.progress)
  };
  let mut progress = progress_of();
  loop {
    match futures::within(LIVENESS_BUDGET_NS, call.as_mut()).await {
      Ok(Some(answer)) => return answer,
      Ok(None) => {
        let now = progress_of();
        if now.is_none() || now == progress {
          return None;
        }
        slates_machine::log_line!(
          "slates-server: shard {shard} is still starting; the control loop waits a further window"
        );
        progress = now;
      }
      Err(_) => return None,
    }
  }
}

/// Persist admission before its region can reach the client. A refused publication or exhausted
/// id space refuses the handoff; no new caller can inherit a retained completion identity (§4.9).
/// `true` for an id issued here for the first time, `false` for one issued before (a resumed session, whose
/// client must prove its key before it is served: `crate::session_key`).
fn reserve_client_identity(client: u32, instance: &str) -> Result<bool, slates_ipc::IpcError> {
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
      return Ok(false);
    }
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
    state
      .db
      .mutate(
        &mut state.segment,
        &slates_db::Op::ClientIdReserved { client },
        now,
      )
      .map(|_| true)
  });
  match reserved {
    Some(Ok(fresh)) => Ok(fresh),
    _ => Err(unavailable("client identity reservation was not published")),
  }
}

/// The control shard's loop: the rendezvous, the heartbeat, and a client handed to its shard
/// as a spawned task (sharing by move). Every client region is named under `region_names`
/// ([`region_name_key`]).
async fn control_loop(
  mut listener: Listener,
  config: DaemonConfig,
  env: Vec<(String, String)>,
  identity: Identity,
  shards: Vec<ShardId>,
  region_names: u64,
  control_life: std::sync::mpsc::Sender<()>,
) {
  let Ok((handoff, len)) = handoff_of(&env) else {
    return;
  };
  let Ok(segment) = AnchorSegment::attach(&handoff, len, &identity) else {
    return;
  };
  // This loop runs on the control shard, the first of `shards`; its doorbell flag is that shard's.
  let control = shards.first().map_or(0, |shard| shard.0);
  // The host every accepted client's account belongs to (A-237).
  let anchor = local_anchor(&config, &identity);
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
  // The heartbeat is this loop's child (A-100): it beats while the loop lives and ends with it, cancelled and joined by
  // the runtime when the loop returns or is cancelled. So a refused boot stops beating and the anchor restarts the
  // daemon, and a shutdown that cancels the loop mid-boot leaves no joinable task behind to hold its shard open (a
  // heartbeat kept joinable until the boot was accepted hung `Daemon::stop` that way).
  if futures::spawn_child(heartbeat_loop(segment, control_life)).is_err() {
    crate::fleet::count_refusal(LOOP_SPAWN_REFUSED);
  }
  if !restore_client_ids(&mut listener, control, &shards).await {
    slates_machine::log_line!(
      "slates-server: the boot is refused; the heartbeat stops so the anchor restarts this daemon"
    );
    return;
  }
  // Every shard has recovered and no client is admitted yet, so no clone is between its steps: the names' aliases are
  // reconciled with the clones they name (A-236).
  // Every green a partition holds by its takeover's adoption is rebuilt from this node's ledger first (A-333), so no
  // client meets one half built and every work over one is replayed after it.
  crate::green_adopt::readopt_at_boot(control, &shards).await;
  // Likewise no work is between its steps: a work whose green lives on another partition is replayed through that
  // green's owner, and only then is every floor compared with the work it holds for (A-247).
  recover_works_across_partitions(control, &shards).await;
  ask_again_for_unbuilt_works(control, &shards);
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
  // The regions this daemon has made, which numbers the next one's name. It wraps: a name then repeats only for a
  // region made four billion regions earlier, and only a holder still mapping that one refuses the new connection,
  // typed, as any refused region does; the next attempt takes the next number.
  let mut regions_made: u32 = 0;
  loop {
    let round = listener.accept_pending(
      &|id| state::with_handed(|h| h.contains(&id)),
      &mut |client_id| {
        if state::with_handed(|h| h.len()) >= bound {
          CLIENTS_REFUSED.fetch_add(1, Ordering::AcqRel);
          return Err(slates_ipc::IpcError::TooManyClients { limit: bound });
        }
        let fresh = reserve_client_identity(client_id, &config.instance)?;
        // The id is reserved in the live set **here**, at admission, not once the accept round returns:
        // an accept round serves every pending connection before it returns, so a burst of connects
        // inside one round was admitted against a live set that did not yet count the earlier ones —
        // the bound of one admitted two (2026-09-14). A region that cannot be created gives the
        // reservation back at once.
        let spread = usize::try_from(client_id)
          .unwrap_or(0)
          .checked_rem(shards.len())
          .unwrap_or(0);
        let Some(&shard) = shards.get(spread) else {
          return Err(slates_ipc::IpcError::DaemonUnavailable {
            endpoint: config.instance.clone(),
            why: "the runtime has no shard to serve a client",
          });
        };
        state::with_handed(|h| h.insert(client_id));
        let name = format!("slates-cr-{region_names:016x}-{regions_made:08x}");
        regions_made = regions_made.wrapping_add(1);
        let region = ClientRegion::create(&name, client_id, shard.0, config.region).and_then(|mut region| {
          // An id issued fresh gets its session key in its region, which only its client is handed; an id issued
          // before is resumed, and its client must write its own key there (`crate::session_key`).
          if fresh {
            let key = state::with_state(|state| crate::session_key::key_of(&state.session_secret, client_id))
              .ok_or(slates_ipc::IpcError::DaemonUnavailable {
                endpoint: config.instance.clone(),
                why: "the client's session key has no shard state to be derived on",
              })?;
            region.set_session_key(&key)?;
          }
          Ok(region)
        });
        let region = match region {
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
      // A claim refused, each answered to its claimant, ends that claim alone (A-176). One admitted (its id
      // reserved) whose handoff then failed gives the id back; each kind is counted and its first logged.
      &mut |refusal| match refusal {
        slates_ipc::IpcError::HandoffLost { client_id, cause } => {
          state::with_handed(|h| h.remove(&client_id));
          if HANDOFF_LOST.fetch_add(1, Ordering::AcqRel) == 0 {
            slates_machine::log_line!(
              "slates-server: client {client_id}'s handoff failed after admission: {cause} (first \
               occurrence; later ones are counted)"
            );
          }
        }
        refusal => {
          if CLAIMS_REFUSED.fetch_add(1, Ordering::AcqRel) == 0 {
            slates_machine::log_line!(
              "slates-server: a client's claim was refused: {refusal} (first occurrence; later ones \
               are counted)"
            );
          }
        }
      },
    );
    if let Some(failure) = round.ended
      && ACCEPTS_FAILED.fetch_add(1, Ordering::AcqRel) == 0
    {
      slates_machine::log_line!(
        "slates-server: a rendezvous accept round ended early: {failure} (first occurrence; later \
         ones are counted)"
      );
    }
    {
      for a in round.accepted {
        let shard = ShardId(a.region.shard());
        let principal = Principal::Uid {
          host: anchor,
          uid: a.uid,
        };
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
        let started = a.started;
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut end = slates_ipc::DaemonEnd::new(a.region);
        #[cfg(unix)]
        end.set_completion(completion);
        let request = Box::new(SpawnRequest::new(
          Box::pin(async move {
            // The server task may be idle with its parked flags set on the clients it knew;
            // a wake makes it mark the new client too before it idles again.
            let server = state::with_state(|s| {
              // Its hang-up is watched from its seat on (A-475). A client whose hang-up cannot be watched is not seated,
              // since nothing would reclaim it: its control closes with this task, which its client reads as the daemon
              // gone, and the admission gives its id back.
              let hangup = match crate::hangup::Hangup::of_client(control.as_ref(), pid, started) {
                Ok(hangup) => hangup,
                Err(refusal) => {
                  s.count(crate::reap::UNWATCHABLE, 1);
                  slates_machine::log_line!(
                    "slates-server: client {client_id} is not seated: its hang-up cannot be watched ({refusal})"
                  );
                  return None;
                }
              };
              match s.clients.insert(ClientSlot {
                status_pages: crate::status_pages::StatusPages::default(),
                end,
                principal,
                client_id,
                pid,
                retiring: false,
                watch: None,
                proven: false,
                control,
                revoked: false,
                consumer_policy: None,
                identities: None,
                owner_route: None,
                read_ahead: None,
                read_ahead_pending: 0,
                remote_volumes: Vec::new(),
                maps_sent: std::collections::BTreeSet::new(),
              }) {
                Ok(seat) => {
                  if crate::reap::start_watch(s, seat, client_id, hangup) {
                    admission.seat();
                  }
                }
                Err(e) => {
                  slates_machine::log_line!(
                    "slates-server: client {client_id} refused by the shard's table: {e}"
                  );
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
                slates_machine::log_line!(
                  "slates-server: client {client_id} handed to a shard without state"
                );
              }
            }
          }),
          None,
        ));
        if let Err(e) = registry::send_control(shard.0, Control::Spawn(request)) {
          HANDOFF_LOST.fetch_add(1, Ordering::AcqRel);
          slates_machine::log_line!(
            "slates-server: client {client_id} could not be handed to shard {}: {e}",
            shard.0
          );
        }
      }
    }
    if let Err(error) = wait_for_rendezvous(&listener).await {
      ACCEPTS_FAILED.fetch_add(1, Ordering::AcqRel);
      slates_machine::log_line!("slates-server: rendezvous readiness failed: {error}");
      return;
    }
  }
}

/// Register only after draining the accept queue. One-shot readiness observes connections
/// already queued and those arriving during registration, without a polling thread (§4.7).
/// The wait also ends when a pending handshake's socket is readable (its client's opening has come, A-187) and no later
/// than the earliest pending handshake is due to be given up, so a claimant that never speaks is closed at the claim
/// wait even when nothing else arrives.
#[cfg(target_os = "linux")]
async fn wait_for_rendezvous(listener: &Listener) -> Result<(), slates_rt::RtError> {
  use std::task::Poll;
  let raw = listener.raw_fd().ok_or(slates_rt::RtError::DriverLost)?;
  let (handshakes, due_ns) = listener.pending();
  // The listener's wait first: its refusal is the rendezvous's, as before handshakes were waited on.
  let mut waits: Vec<_> = std::iter::once(raw)
    .chain(handshakes)
    .map(|descriptor| Some(Box::pin(slates_rt::readiness::readable(descriptor))))
    .collect();
  let any_readable = std::future::poll_fn(|cx| {
    for (at, slot) in waits.iter_mut().enumerate() {
      let Some(wait) = slot else {
        continue;
      };
      match wait.as_mut().poll(cx) {
        Poll::Ready(Ok(())) => return Poll::Ready(Ok(())),
        Poll::Ready(Err(refusal)) if at == 0 => return Poll::Ready(Err(refusal)),
        // A handshake's wait the driver refused is left to the deadline below, which ends the wait no later than that
        // handshake is due; counted, never silent (banned item 9).
        Poll::Ready(Err(_)) => {
          *slot = None;
          crate::fleet::count_refusal(HANDSHAKE_WAIT_REFUSED);
        }
        Poll::Pending => {}
      }
    }
    Poll::Pending
  });
  match due_ns {
    Some(due_ns) => futures::within(due_ns, any_readable)
      .await?
      .unwrap_or(Ok(())),
    None => any_readable.await,
  }
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

async fn heartbeat_loop(segment: AnchorSegment, control_life: std::sync::mpsc::Sender<()>) {
  // The watchdog beats while this sender lives (A-228); it goes with this task, so the beat stops with the loop.
  let _control_life = control_life;
  let mut clock = HostClock::new();
  let mut last: Option<(u64, slates_rt::shard::Counters)> = None;
  loop {
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut clock);
    let counters = registry::with_current(|ctx| ctx.counters()).unwrap_or_default();
    if let Some((beat, before)) = last {
      log_late_beat(now.saturating_sub(beat), &before, &counters);
    }
    last = Some((now, counters));
    let (requested, lapse_bound_ns) = match segment.supervision() {
      Ok(sup) => (sup.stop_requested_at().is_some(), sup.lapse_bound_ns()),
      Err(_) => (false, None),
    };
    state::with_state(|s| {
      // A watch the process thread sent since the last beat is taken in now ([`Daemon::wait_for_stop`]).
      if s.stop_watch.is_none()
        && let Some(watch) = s
          .stop_watch_install
          .as_ref()
          .and_then(|install| install.try_recv().ok())
      {
        s.stop_watch = Some(watch);
      }
      if let Some(watch) = s.stop_watch.as_ref()
        && (requested || (watch.also)())
      {
        // Bounded by one: a reason already sent and not yet taken is the same news.
        let _full_or_gone = watch.wake.try_send(());
      }
      // This node's detection by its anchor (A-574): the watchdog's widest window and the anchor's published lapse bound,
      // read at every beat on every node, laptop or fleet, and the failover delay published from them.
      crate::fleet::refresh_supervision(s, lapse_bound_ns);
    });
    // The delay to every shard where it moved: a laptop's other shards read it as a fleet's do (A-574).
    crate::fleet::fan_failover();
    pace(HEARTBEAT_NS).await;
  }
}

/// A process thread's wait for a reason to stop ([`Daemon::wait_for_stop`]), asked at each heartbeat on the control
/// shard.
pub(crate) struct StopWatch {
  /// The caller's own reasons (a signal, a gone parent).
  also: Box<dyn Fn() -> bool + Send>,
  /// Wakes the waiting thread: sent once a reason holds; dropped with the control shard's state.
  wake: std::sync::mpsc::SyncSender<()>,
}

/// Logs a beat that came late, [`LATE_BEAT_SHARE`] of the window the watchdog judges the control shard by or more after
/// the one before ([`beat_is_late`], A-470; §4.14 `daemon.alive`): the shard went that long without running its heartbeat
/// task, and past its window the watchdog judges it. With the gap goes what the shard did in it, from its counters
/// `before` and `after`: its steps and
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
  if !beat_is_late(gap, after.scheduler_overrun_ns) {
    return;
  }
  let delta =
    |field: fn(&slates_rt::shard::Counters) -> u64| field(after).saturating_sub(field(before));
  slates_machine::log_line!(
    "slates daemon: the control shard's beat task ran {} ms after its last turn (a near miss against its {} ms window; \
     the anchor's beat is the watchdog's now): in the gap the shard \
     ran {} steps and {} polls ({} long in a task, {} blocked in the kernel, {} preempted by the host, {} \
     unattributed), fired {} timers, waited {} times, spawned {} tasks and finished {}; {} tasks live; its \
     longest poll is {} ms and its timer overrun {} ms",
    gap / NS_PER_MS,
    crate::watchdog::watch_window_ns(after.scheduler_overrun_ns) / NS_PER_MS,
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

/// Shape: a beat that late — this share of the shard's window or more after the last — is logged as a near
/// miss ([`log_late_beat`]): half the window, so the stalls that stop short of a kill are seen too. A scatter's part
/// waits for room a window of that share at a time (`crate::verbs::PART_ROOM_BUDGET_NS`, A-426).
pub(crate) const LATE_BEAT_SHARE: u64 = 2;

/// Whether the control shard's beat task, run `gap_ns` after its last turn, ran late ([`log_late_beat`], A-470): a
/// [`LATE_BEAT_SHARE`] or more of the window the watchdog judges that shard by now (A-455,
/// `crate::watchdog::watch_window_ns` over `overrun_ns`, the scheduler overrun the shard measured). A gap is a near miss
/// against the verdict the watchdog would reach, and the watchdog reaches it at the shard's own window: on a host that
/// holds the shard off its CPU longer than the liveness window, a gap that window explains is no near miss, and before
/// A-470 every such turn was logged as one, against a window the watchdog no longer judged the shard by. Pure, so the
/// rule is tested on every host.
pub(crate) fn beat_is_late(gap_ns: u64, overrun_ns: u64) -> bool {
  gap_ns >= crate::watchdog::watch_window_ns(overrun_ns) / LATE_BEAT_SHARE
}

#[cfg(test)]
mod late_beat_tests {
  use super::{LIVENESS_BUDGET_NS, beat_is_late};

  /// A-470, A-455, §4.14. Do: judge the control shard's beat gaps against the shard's own window, on a quiet shard (no
  /// overrun measured), on one the host held off its CPU two and a half liveness windows, and on one whose reading ran
  /// away (an hour, a process stopped whole and continued). Expect: on the quiet shard a gap of three fifths of the
  /// liveness window late and two fifths not, as before; on the held one three fifths not late, since its window is two
  /// and a half and the watchdog judges it there, and thirteen tenths late; on the runaway one the ceiling's half the
  /// line, four windows not late and five late. Before A-470 every gap past half the liveness window was logged as a
  /// near miss, whatever window the watchdog judged the shard by.
  #[test]
  fn a_beat_is_late_against_the_shards_own_window() {
    let window = LIVENESS_BUDGET_NS;
    let quiet = 0;
    assert!(beat_is_late(3 * window / 5, quiet), "a quiet shard's gap");
    assert!(
      !beat_is_late(2 * window / 5, quiet),
      "a quiet shard's short gap"
    );
    let held = 5 * window / 2;
    assert!(
      !beat_is_late(3 * window / 5, held),
      "a gap the held shard's window explains"
    );
    assert!(
      beat_is_late(13 * window / 10, held),
      "past half the held shard's window"
    );
    let runaway = 3_600 * window;
    assert!(
      !beat_is_late(4 * window, runaway),
      "under the ceiling's half"
    );
    assert!(beat_is_late(5 * window, runaway), "at the ceiling's half");
  }
}

#[cfg(test)]
mod starved_share_tests {
  use super::{HEARTBEAT_NS, LIVENESS_BUDGET_NS, starved_share_ns};
  use crate::watchdog::watch_window_ns;

  /// Shape: a hold's span: ten liveness windows, a whole number of beats, past every window judged here.
  const SPAN_NS: u64 = 10 * LIVENESS_BUDGET_NS;

  /// A hold's whole CPU over `span_ns` on a shard judged by `window_ns`, as `starve_this_thread` spends it: its share
  /// each beat, over the span's beats.
  fn whole_cpu_ns(span_ns: u64, window_ns: u64) -> u64 {
    span_ns.div_ceil(HEARTBEAT_NS) * starved_share_ns(span_ns, window_ns)
  }

  /// A-552 (R3's class D, `starved_share_ns`; A-232, A-455). Do: take a starved hold's share over ten liveness windows on
  /// a quiet shard and on one whose host held it off its CPU two and a half windows, each judged by the window the
  /// watchdog gives its measured overrun; then over a span shorter than a beat. Expect the hold's whole CPU to be half
  /// the shard's own window in both, so the watchdog judging the shard by that window finds it neither spinning nor
  /// still, and no beat's share past a beat. Before, the hold's whole CPU was half the liveness window whatever window
  /// the watchdog judged its shard by.
  #[test]
  fn a_starved_holds_cpu_is_half_the_window_its_shard_is_judged_by() {
    let quiet = watch_window_ns(0);
    assert_eq!(
      whole_cpu_ns(SPAN_NS, quiet),
      quiet / 2,
      "a quiet shard's hold"
    );
    let held = watch_window_ns(5 * LIVENESS_BUDGET_NS / 2);
    assert_eq!(
      whole_cpu_ns(SPAN_NS, held),
      held / 2,
      "a held shard's hold spins half the window the watchdog judges it by"
    );
    assert_eq!(
      starved_share_ns(HEARTBEAT_NS / 2, held),
      HEARTBEAT_NS,
      "a span under a beat spins a beat at most"
    );
  }
}

#[cfg(test)]
mod laptop_failover_tests {
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use super::{LIVENESS_BUDGET_NS, audit_daemon_of, observe_at, observe_first};

  /// Shape: the lapse bound the test publishes as the daemon's anchor does: four liveness windows, as an anchor whose own
  /// waits ran three windows late measures it, far from any delay the daemon held before.
  const LAPSE_NS: u64 = 4 * LIVENESS_BUDGET_NS;
  /// Shape: how long the test looks for every shard to read the delay: ten seconds, a hundred of the control shard's
  /// beats, far past the beat that reads the bound and the fan that carries the delay.
  const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
  /// Shape: the pause between the test's looks, half a beat.
  const LOOK: std::time::Duration = std::time::Duration::from_millis(50);

  /// A-574 (A-405's laptop detection, derived; R8). Do: start a two-shard daemon with no fleet (`f = 0`, a laptop's),
  /// publish a lapse bound in its supervision block as its anchor does, and look at both shards' failover delay until
  /// they agree. Expect each to be at least the liveness window, then the published lapse and the recovery prior, in
  /// whole periods, and the control shard's detection term to be the watchdog's widest window (from the liveness window
  /// to its ceiling) and then exactly the lapse, read from what was measured: the laptop's detection read on the control
  /// shard's beat from what its watchdog and its anchor measure, and fanned to every shard. Before, a laptop's delay was
  /// the liveness window and the recovery prior, set at boot and never read again.
  #[test]
  fn a_laptops_failover_delay_follows_its_watchdogs_window_and_its_anchors_lapse() {
    let daemon = audit_daemon_of(2);
    daemon
      .segment()
      .supervision()
      .unwrap()
      .record_lapse_bound(LAPSE_NS);
    let floor = crate::fleet::failover_in_periods(
      LIVENESS_BUDGET_NS + LAPSE_NS + slates_db::replay::RECOVERY_BUDGET_NS,
    );
    let began = std::time::Instant::now();
    let read = loop {
      let control = observe_first(&daemon, |s| {
        (s.failover_ns, s.failover.estimate().detection)
      });
      let other = observe_at(&daemon, 1, |s| s.failover_ns);
      if control.0 >= floor && other == control.0 {
        break Some(control);
      }
      if began.elapsed() > WAIT {
        break None;
      }
      // The harness paces its looks; the daemon under test reads on its own beat.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(LOOK);
    };
    daemon.stop();
    let (failover_ns, detection) =
      read.expect("every shard read a delay with the anchor's lapse in it");
    let detection = detection.expect("its anchor detects it");
    let window = detection
      .ns
      .checked_sub(LAPSE_NS)
      .expect("the detection holds the lapse");
    assert!(
      (LIVENESS_BUDGET_NS..=crate::watchdog::WINDOW_CEILING_NS).contains(&window),
      "the watchdog's widest window, then the lapse: {window} ns"
    );
    assert!(
      detection.samples > 0,
      "read from what was measured: {detection:?}"
    );
    println!(
      "a laptop's failover delay {failover_ns} ns: the watchdog's widest window {window} ns, the anchor's lapse \
       {LAPSE_NS} ns, then the recovery"
    );
  }
}

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

/// The NFS transport (§4.6): one loopback listener served on the `control` shard. A supervising anchor holds the
/// listener and hands its descriptor over in the environment, so its port survives a daemon restart; the daemon adopts
/// that when present, or binds a fresh ephemeral one when it runs standalone (tests). Either way the port is known
/// here, before the serve task moves the listener onto the shard, and the cross-shard bridge queue reaches volumes on
/// other shards. The connections the anchor held across the restart (A-113) are served first there, each judged by the
/// newest send ledger it handed back where a send may stop mid-record (`inherited`, Linux, A-550); without a listener
/// nothing could serve them, so each is released, and the ledgers with them — the anchor closes the last copy and the
/// kernel client reconnects to whatever listener comes next, rather than wait on a connection nobody reads. The port,
/// if the listener bound.
fn start_nfs_transport(
  runtime: &Runtime,
  control: ShardId,
  config: &DaemonConfig,
  handed: Vec<crate::anchor_hold::HandedConnection>,
  inherited: crate::held_send::Inherited,
) -> Result<Option<u16>, RtError> {
  let listener = nfs_listener().ok();
  let port = listener
    .as_ref()
    .and_then(|listener| listener.local_addr().ok())
    .map(|address| address.port());
  let (Some(listener), Some(port)) = (listener, port) else {
    crate::nfs::release_handed(handed);
    inherited.release();
    return Ok(None);
  };
  // Publish the port for a verb handler to report to a client (`slates mount`).
  NFS_PORT.store(u32::from(port), Ordering::Release);
  let bound = config.held_connection_bound();
  let cache_line = config.cache_line;
  runtime.spawn_on(control, async move {
    // The connections a dead daemon left are answered first (A-113): their kernel clients are waiting.
    crate::nfs::adopt_held(handed, inherited, port, bound, cache_line);
    match futures::spawn(crate::nfs::serve(listener, port, bound)) {
      Ok(task) => {
        let _ = futures::detach(task);
      }
      Err(_) => {
        crate::fleet::count_refusal(LOOP_SPAWN_REFUSED);
      }
    }
  })?;
  Ok(Some(port))
}

/// The Shards stream's socket route (§4.6, A-161; macOS): its listener bound at its path in the user's own temporary
/// directory and served on the `control` shard, where each connection's exchange runs (`crate::shards_route`). A route
/// that cannot bind (a shared directory, another daemon of this instance answering there) is reported, and the Shards
/// form is then refused `ListenerNotBound`; the daemon serves everything else.
#[cfg(target_os = "macos")]
fn start_shards_route(
  runtime: &Runtime,
  control: ShardId,
  config: &DaemonConfig,
) -> Result<(), RtError> {
  let listener = match slates_ipc::route::Listener::bind(&config.instance) {
    Ok(listener) => listener,
    Err(error) => {
      slates_machine::log_line!(
        "slates-server: the Shards stream route did not bind ({error}); a Shards attach is refused"
      );
      crate::fleet::count_refusal(SHARDS_ROUTE_UNBOUND);
      return Ok(());
    }
  };
  SHARDS_ROUTE_BOUND.store(true, Ordering::Release);
  runtime.spawn_on(control, async move {
    match futures::spawn(crate::shards_route::serve(listener)) {
      Ok(task) => {
        let _ = futures::detach(task);
      }
      Err(_) => {
        crate::fleet::count_refusal(LOOP_SPAWN_REFUSED);
      }
    }
  })?;
  Ok(())
}

/// Counter: the Shards stream route that did not bind its socket (macOS): the Shards form is refused while it stands.
#[cfg(target_os = "macos")]
const SHARDS_ROUTE_UNBOUND: &str = "shards.route_unbound";

/// Raises this process's soft descriptor limit to the hard one (no privilege needed); `(before, after)` when it changed.
/// The daemon calls it at start; the anchor too, since it holds a duplicate of every FUSE device and NFS connection
/// the daemon hands it (A-61, A-113) — at a shell's soft limit (256 on macOS) it refused holds far below its bound.
pub fn raise_descriptor_limit() -> Option<(u64, u64)> {
  limits::raise_descriptor_limit()
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
pub(crate) use tests::{
  audit_daemon, audit_daemon_configured, audit_daemon_of, audit_on_shard,
  audit_on_shard_configured, hold_busy, observe_at, observe_first, test_profile,
};

#[cfg(test)]
mod pressure_tests {
  use super::{apportion, pressure_hold};

  /// Shape: a daemon's whole content capacity in the hold's cases.
  const CAPACITY: u64 = 1 << 30;

  /// admission.md §5.5: with no slack at the first sample (the host had exactly what the daemon could fill), a drop
  /// the daemon did not cause is held whole, and the daemon's own growth is never held.
  #[test]
  fn with_no_slack_a_drop_is_held_whole_and_own_growth_never() {
    assert_eq!(pressure_hold(CAPACITY, CAPACITY - 100, 0, CAPACITY), 100);
    assert_eq!(
      pressure_hold(CAPACITY, CAPACITY - 100, 100, CAPACITY),
      0,
      "its own growth"
    );
    assert_eq!(
      pressure_hold(CAPACITY, CAPACITY - 300, 100, CAPACITY),
      200,
      "the drop beyond its own growth"
    );
  }

  /// admission.md §5.5 (2026-10-09): on a roomy host, memory others take from the slack holds nothing, and holds only
  /// what eats into the daemon's unfilled room.
  #[test]
  fn on_a_roomy_host_only_a_drop_into_the_daemons_room_is_held() {
    let baseline = 4 * CAPACITY;
    assert_eq!(
      pressure_hold(baseline, 3 * CAPACITY, 0, CAPACITY),
      0,
      "the slack"
    );
    assert_eq!(
      pressure_hold(baseline, CAPACITY, 0, CAPACITY),
      0,
      "exactly its room left"
    );
    assert_eq!(
      pressure_hold(baseline, CAPACITY / 2, 0, CAPACITY),
      CAPACITY / 2,
      "half its room taken"
    );
    assert_eq!(
      pressure_hold(baseline, CAPACITY / 4, CAPACITY / 2, CAPACITY),
      CAPACITY / 4,
      "a half-filled daemon needs only its other half backed"
    );
  }

  /// §4.2 (2026-10-09): the part of a hold the pool cannot cover is taken from the shards' free room in proportion to
  /// it. Do: divide 10 bytes over rooms of 30, 10 and 0. Expect 8, 2 and 0: the floor of each share (7, 2, 0) and the
  /// one byte of remainder to the first shard with room left, summing to 10 exactly.
  #[test]
  fn a_residual_hold_is_divided_by_the_shards_free_room_and_sums_exactly() {
    assert_eq!(apportion(10, &[30, 10, 0]), [8, 2, 0]);
  }

  /// §4.2: no shard gives up more than its room. Do: divide more than the rooms hold, and anything over no room. Expect
  /// every room taken whole, then nothing.
  #[test]
  fn a_residual_hold_never_takes_more_than_a_shards_room() {
    assert_eq!(apportion(50, &[30, 10]), [30, 10]);
    assert_eq!(apportion(40, &[30, 10]), [30, 10]);
    assert_eq!(apportion(5, &[0, 0]), [0, 0]);
    assert_eq!(apportion(0, &[7, 9]), [0, 0]);
  }

  /// §4.2: across rooms of every shape, the parts sum to the residual exactly and stay within each room. Do: divide
  /// residuals below each total over rooms that include zeros, ones and the largest values. Expect both, every time.
  #[test]
  fn a_residual_hold_sums_exactly_within_the_rooms_for_any_rooms() {
    let rooms: [&[u64]; 5] = [
      &[1, 1, 1],
      &[u64::MAX, u64::MAX],
      &[3, 0, 5, 7, 0, 11],
      &[1 << 40, 1, 1 << 20],
      &[999_999_937, 2, 1],
    ];
    for room in rooms {
      let total = room.iter().fold(0u128, |sum, each| sum + u128::from(*each));
      let samples = [1u128, total / 3, total / 2, total - 1];
      for residual in samples.map(|each| u64::try_from(each.min(u128::from(u64::MAX))).unwrap()) {
        let holds = apportion(residual, room);
        let given = holds
          .iter()
          .fold(0u128, |sum, each| sum + u128::from(*each));
        assert_eq!(
          given,
          u128::from(residual),
          "{residual} over {room:?}: {holds:?}"
        );
        assert!(
          holds.iter().zip(room).all(|(hold, each)| hold <= each),
          "{holds:?} within {room:?}"
        );
      }
    }
  }
}

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
  /// Measured **patiently** (`MachineProfile::measure_patiently`): the quick bound first, then the probe's own
  /// wall ceiling. Measuring once per process is fewer dice rolled, not a safe roll — and behind a `OnceLock`
  /// the one roll decides the whole binary. On 2026-10-18 a full `cargo test --workspace` refused this probe
  /// and **sixty tests in this binary failed in 1.19 seconds**, every one of them at this line. This fixture was
  /// the site the day's earlier sweep missed, because it lives in a source file rather than under `tests/`.
  pub(crate) fn test_profile() -> slates_machine::MachineProfile {
    static PROFILE: std::sync::OnceLock<
      Result<slates_machine::MachineProfile, slates_machine::MachineError>,
    > = std::sync::OnceLock::new();
    PROFILE
      .get_or_init(|| {
        slates_machine::MachineProfile::measure_patiently(&slates_machine::ProfileOptions {
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
    let daemon = audit_daemon_with(&profile, config, instance);
    let result = daemon.observe(daemon.shards.first().copied(), history);
    daemon.stop();
    result.expect("the audit history completed")
  }

  /// A running one-shard audit daemon, bootstrapped, for a test whose work outlives one observation (a task it spawns
  /// that must still run after the observation returns); the test stops it.
  pub(crate) fn audit_daemon() -> super::Daemon {
    audit_daemon_configured(|_| {})
  }

  /// [`audit_daemon`] with its derived configuration changed by `configure` before it starts, as
  /// [`audit_on_shard_configured`] changes an audit's: a test that must reach a derived bound shrinks what derives it.
  pub(crate) fn audit_daemon_configured(
    configure: impl FnOnce(&mut crate::DaemonConfig),
  ) -> super::Daemon {
    let profile = test_profile();
    static NEXT_KEPT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let instance = format!(
      "audit-kept-{}-{}",
      std::process::id(),
      NEXT_KEPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    configure(&mut config);
    audit_daemon_with(&profile, config, instance)
  }

  /// A running audit daemon of `shards` shards, bootstrapped, for a test of one shard's call to another (a wait judged
  /// in its target's own time, A-432); the test stops it.
  pub(crate) fn audit_daemon_of(shards: u16) -> super::Daemon {
    let profile = test_profile();
    static NEXT_OF: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let instance = format!(
      "audit-of-{}-{}",
      std::process::id(),
      NEXT_OF.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(shards));
    audit_daemon_with(&profile, config, instance)
  }

  /// Shape: a busy poll's cycle ([`hold_busy`]): ten milliseconds.
  const BUSY_CYCLE_NS: u64 = 10_000_000;
  /// Shape: the part of each cycle a busy poll runs on its CPU: a fifth, so the shard runs about a fifth of the poll's
  /// wall time, as a shard starved by its neighbours does (`crates/server/tests/cross_shard_wait.rs`'s control shard).
  const BUSY_ON_CPU_NS: u64 = BUSY_CYCLE_NS / 5;

  /// Holds the calling shard in one poll for `wall_ns`: a fifth of each cycle spinning on its CPU, the rest asleep, so
  /// the shard makes progress at a fifth of the wall's pace while every message on its channel waits for the poll's end
  /// (a busy or starved target, which a wait judged in its own time waits for, A-432).
  // The busy poll stands for a shard held off its CPU most of each cycle: it must sleep on the shard's thread.
  #[allow(clippy::disallowed_methods)]
  pub(crate) fn hold_busy(wall_ns: u64) {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_nanos(wall_ns) {
      let cycle = std::time::Instant::now();
      while cycle.elapsed() < std::time::Duration::from_nanos(BUSY_ON_CPU_NS) {
        std::hint::spin_loop();
      }
      std::thread::sleep(std::time::Duration::from_nanos(
        BUSY_CYCLE_NS - BUSY_ON_CPU_NS,
      ));
    }
  }

  /// Runs `history` on `daemon`'s first shard (an [`audit_daemon`]'s one), as [`audit_on_shard`] does, leaving the
  /// daemon running.
  pub(crate) fn observe_first<T: Send + 'static>(
    daemon: &super::Daemon,
    history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
  ) -> T {
    daemon
      .observe(daemon.shards.first().copied(), history)
      .expect("the observation completed")
  }

  /// [`observe_first`] on the shard of partition `partition`: a test's look at a shard other than the first.
  pub(crate) fn observe_at<T: Send + 'static>(
    daemon: &super::Daemon,
    partition: usize,
    history: impl FnOnce(&mut crate::state::ShardState) -> T + Clone + Send + 'static,
  ) -> T {
    daemon
      .observe(daemon.shards.get(partition).copied(), history)
      .expect("the observation completed")
  }

  /// Starts and bootstraps an audit daemon under `config`.
  fn audit_daemon_with(
    profile: &slates_machine::MachineProfile,
    config: crate::DaemonConfig,
    instance: String,
  ) -> super::Daemon {
    let daemon = super::Daemon::start(
      profile,
      config,
      super::SegmentSource::Create { name: instance },
    )
    .expect("the audit daemon starts");
    daemon
      .bootstrap(true)
      .expect("the audit explicitly creates its initial group");
    daemon
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
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: None,
        rendezvous: rendezvous.adoption(),
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

  /// A-724 piece 8c (main's order of 2026-11-20: an object past one region message is homed in parts, and a home's holds
  /// survive its restart). Do: on a daemon over a retained anchor segment, its own region's one home, push the fixtures'
  /// `create` into a repository (so the successor's recovery frees every restored window nothing holds), then take a put
  /// of an object two and a half region messages long as a region member sends it, a part at a time (`serve_remote`'s
  /// `PutPart`s); stop it; start a successor over the same segment and, once it has recovered the repository, read the
  /// object back a part at a time (`ReadPart`s). Expect: each part but the last answered taken, the last kept; the successor answering every part, the
  /// parts making the object byte for byte. Before the parts, the object could not be sent (one message carries at most
  /// `PART_BYTES`); before the region holds rode the shard image, the successor freed it after its replay.
  #[test]
  fn t_a724_an_object_past_a_region_message_is_homed_in_parts_and_read_back_after_a_restart() {
    use slates_anchor::AnchorSegment;
    use slates_cluster::homes::{HomeMessage, HomeOutcome, PART_BYTES};
    let profile = test_profile();
    let instance = format!("a724-parts-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
      .unwrap()
      .with_content(&super::content_name_of(&instance), config.content_bytes())
      .unwrap();
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      }
    };
    let part = usize::try_from(PART_BYTES).unwrap();
    let object: Vec<u8> = (0..part * 5 / 2)
      .map(|at| u8::try_from(at % 253).unwrap_or(0))
      .collect();
    let identity = slates_vcs::ContentId::of(&object).0;
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    first.bootstrap(true).unwrap();
    let shard = first.shards.first().copied();
    let (sent, got) = std::sync::mpsc::channel();
    let put = object.clone();
    let (made, repository) = std::sync::mpsc::channel();
    first
      .observe(shard, move |state| {
        // A repository beside the object, so the successor's recovery replays its records and then frees every restored
        // window nothing holds (`release_unheld`): the region hold must be what keeps the object.
        let repository = crate::git_push::tests::git_repository(state, "beside");
        crate::git_push::tests::pushed_from_the_fixtures(state, repository, &["create"]);
        let _ = made.send(repository);
      })
      .unwrap();
    let repository = repository.recv().expect("the repository made");
    first
      .observe(shard, move |state| {
        let origin = state.shard;
        let put = put.clone();
        let sent = sent.clone();
        let task = slates_rt::futures::spawn(async move {
          let total = u64::try_from(put.len()).unwrap();
          let mut answers = Vec::new();
          for (at, chunk) in put.chunks(part).enumerate() {
            let request = HomeMessage::PutPart {
              identity,
              total,
              offset: u64::try_from(at * part).unwrap(),
              bytes: chunk.to_vec(),
            }
            .encode();
            let reply = crate::vcs_store::homes::serve_remote(
              origin,
              &request,
              crate::daemon::LIVENESS_BUDGET_NS,
            )
            .await;
            answers.push(HomeMessage::decode(&reply).map(|message| match message {
              HomeMessage::Answer { outcome, .. } => {
                format!("{outcome:?}").chars().take(24).collect::<String>()
              }
              other => format!("{other:?}"),
            }));
          }
          let _ = sent.send(answers);
        })
        .unwrap();
        slates_rt::futures::detach(task).unwrap();
      })
      .unwrap();
    let answers = got.recv().expect("the parts answered");
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    let shard = second.shards.first().copied();
    let (sent, got) = std::sync::mpsc::channel();
    let read = move |state: &mut crate::state::ShardState| {
      let origin = state.shard;
      let sent = sent.clone();
      if let Ok(task) = slates_rt::futures::spawn(async move {
        // The read only once the repository is recovered, so every restored window nothing held is freed by then.
        loop {
          let recovered = crate::state::with_state(|state| {
            state.vcs_stores.unrecovered(repository).is_none()
              && !state.vcs_stores.rebuilding(repository)
              && state.vcs_stores.holds(repository)
          });
          if recovered != Some(false) {
            break;
          }
          slates_rt::futures::yield_now().await;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let outcome = loop {
          let request = HomeMessage::ReadPart {
            identity,
            offset: u64::try_from(bytes.len()).unwrap(),
          }
          .encode();
          let reply = crate::vcs_store::homes::serve_remote(
            origin,
            &request,
            crate::daemon::LIVENESS_BUDGET_NS,
          )
          .await;
          match HomeMessage::decode(&reply) {
            Ok(HomeMessage::Answer {
              outcome: HomeOutcome::Part { total, bytes: got },
              ..
            }) if !got.is_empty() => {
              bytes.extend_from_slice(&got);
              if u64::try_from(bytes.len()).unwrap() >= total {
                break Ok(bytes);
              }
            }
            other => break Err(format!("{other:?}")),
          }
        };
        let _ = sent.send(outcome);
      }) {
        let _ = slates_rt::futures::detach(task);
      }
    };
    let started = std::time::Instant::now();
    let bound = std::time::Duration::from_nanos(10 * crate::daemon::LIVENESS_BUDGET_NS);
    while second.observe(shard, read.clone()).is_err() {
      assert!(
        started.elapsed() < bound,
        "the successor's shard never took the read"
      );
      std::thread::park_timeout(std::time::Duration::from_millis(50));
    }
    let read_back = got.recv().expect("the successor answered");
    second.stop();
    drop(segment);
    let parts = answers.len();
    assert_eq!(parts, 3, "{answers:?}");
    assert!(
      answers[..parts - 1]
        .iter()
        .all(|answer| answer.as_ref().is_ok_and(|a| a.starts_with("Taken"))),
      "{answers:?}"
    );
    assert!(
      answers[parts - 1]
        .as_ref()
        .is_ok_and(|a| a.starts_with("Kept")),
      "{answers:?}"
    );
    assert_eq!(read_back.map(|bytes| bytes == object), Ok(true));
  }

  /// A-727 slice b (the content plane's durability barrier; main's order of 2026-11-20). Do: on a daemon over a retained
  /// anchor segment, with version 3 on, push the fixtures' `create`: its record names its names by identity, appended only
  /// once the names are durable (`vcs_store::content_durable`, through the homing); then stop the daemon without
  /// publishing anything more, as a crash between the push and anything after it, and start a successor over the same
  /// segment. Expect: the successor replays the record whole from the content its image holds: the repository recovered,
  /// `main` at the pushed commit, no replay refused and no names missing, the record version 3. Without the barrier (the
  /// entry appended right after the names were put) the names are in no image and the replay is refused `Names`.
  #[test]
  fn t_a727b_a_crash_after_an_entry_naming_content_by_identity_loses_nothing_it_names() {
    use slates_anchor::AnchorSegment;
    let profile = test_profile();
    let instance = format!("a727b-barrier-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
      .unwrap()
      .with_content(&super::content_name_of(&instance), config.content_bytes())
      .unwrap();
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      }
    };
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    first.bootstrap(true).unwrap();
    let shard = first.shards.first().copied();
    let (sent, got) = std::sync::mpsc::channel();
    first
      .observe(shard, move |state| {
        crate::vcs_store::write_version_3_for_test(true);
        let repository = crate::git_push::tests::git_repository(state, "barrier");
        let request = crate::git_push::tests::chain()
          .into_iter()
          .next()
          .map(|(scenario, _)| scenario.request.to_vec())
          .unwrap();
        let task = slates_rt::futures::spawn(async move {
          let answered = crate::git_push::tests::homed_drive(repository, &request).await;
          let _ = sent.send((repository, answered.map(|_| ())));
        })
        .unwrap();
        slates_rt::futures::detach(task).unwrap();
      })
      .unwrap();
    let (repository, answered) = got.recv().expect("the push answered");
    answered.expect("the push homed its names and answered");
    // The crash: nothing more published.
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    let shard = second.shards.first().copied();
    // The successor's recovery of the repository, waited for on its shard: its event (recovered, or a replay refused),
    // not a time. The watch is asked again while the successor's shard is still starting (`ShardGone`), within a bound
    // derived from the liveness budget.
    let (sent, got) = std::sync::mpsc::channel();
    let watch = move |_: &mut crate::state::ShardState| {
      let sent = sent.clone();
      let task = slates_rt::futures::spawn(async move {
        loop {
          let seen = crate::state::with_state(|state| {
            let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
            let recovered = state.vcs_stores.unrecovered(repository).is_none()
              && !state.vcs_stores.rebuilding(repository)
              && state.vcs_stores.holds(repository);
            let main = state
              .db
              .partition()
              .repositories()
              .branch(repository, "main");
            let versions: Vec<u32> = crate::vcs_store::tests::records(state, repository)
              .iter()
              .map(|record| u32::from_le_bytes(record[4..8].try_into().unwrap()))
              .collect();
            (
              recovered,
              main.is_some(),
              count(crate::vcs_store::REPLAY_REFUSED),
              count(crate::vcs_store::NAMES_UNFETCHED),
              versions,
            )
          });
          if let Some(seen) = seen
            && (seen.0 || seen.2 > 0 || seen.3 > 0)
          {
            let _ = sent.send(seen);
            break;
          }
          slates_rt::futures::yield_now().await;
        }
      });
      if let Ok(task) = task {
        let _ = slates_rt::futures::detach(task);
      }
    };
    let started = std::time::Instant::now();
    let bound = std::time::Duration::from_nanos(10 * crate::daemon::LIVENESS_BUDGET_NS);
    while second.observe(shard, watch.clone()).is_err() {
      assert!(
        started.elapsed() < bound,
        "the successor's shard never took the watch"
      );
      std::thread::park_timeout(std::time::Duration::from_millis(50));
    }
    let seen = got.recv().ok();
    second.stop();
    drop(segment);
    assert_eq!(seen, Some((true, true, 0, 0, vec![3])));
  }

  /// A-727 slice a3b (an object past a window across a warm restart; the identity rule). Do: on a daemon over a retained
  /// anchor segment and content object, push a create of `main` whose one file is 1 MiB (past any node's window); read
  /// the file's object by its identity, the BLAKE3 of its bytes; publish; stop; start a successor and read it again.
  /// Expect: the object whole by its identity on both; the successor taking back every list and name of the content
  /// store from the shard image (none refused), so the records' replay stores no list again.
  #[test]
  fn t_a727_a3b_an_object_past_a_window_survives_a_warm_restart_by_its_identity() {
    use slates_anchor::AnchorSegment;
    const LARGE: usize = 1 << 20;
    let profile = test_profile();
    let instance = format!("a727-a3b-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
      .unwrap()
      .with_content(&super::content_name_of(&instance), config.content_bytes())
      .unwrap();
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      }
    };
    let (request, _) = crate::git_push::tests::large_create(LARGE);
    let blob: Vec<u8> = (0..LARGE)
      .map(|at| u8::try_from(at % 251).unwrap_or(u8::MAX))
      .collect();
    let identity = slates_vcs::ContentId::of(&blob);
    let len = u64::try_from(LARGE).unwrap();
    let read = move |state: &mut crate::state::ShardState| {
      let lent = crate::vcs_store::Lent::new(&mut state.store, &mut state.vcs_stores);
      lent
        .stores
        .content()
        .and_then(|content| content.object_bytes(&identity, len))
    };
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    first.bootstrap(true).unwrap();
    let before = first
      .observe(first.shards.first().copied(), move |state| {
        let repository = crate::git_push::tests::git_repository(state, "large");
        let received = crate::git_push::tests::pushed(state, repository, &request);
        let _ = received;
        crate::vcs_store::paths::whole_build::settle(state);
        read(state)
      })
      .unwrap();
    first.publish_every_shard().unwrap();
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    let shard = second.shards.first().copied();
    let observe = || {
      second
        .observe(shard, move |state| {
          let bytes = read(state);
          let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
          let lists = state
            .vcs_stores
            .content()
            .map(|content| content.counters().lists);
          (
            bytes,
            count(crate::vcs_store::NODES_RESTORED),
            count(crate::vcs_store::NODES_UNRESTORED),
            lists,
          )
        })
        .ok()
    };
    let mut seen = observe();
    for _ in 0..600 {
      if seen.as_ref().is_some_and(|seen| seen.0.is_some()) {
        break;
      }
      std::thread::sleep(std::time::Duration::from_millis(50));
      seen = observe();
    }
    second.stop();
    assert_eq!(
      before.as_deref(),
      Some(blob.as_slice()),
      "the object by its identity before"
    );
    let (after, restored, unrestored, lists) = seen.expect("the successor answered");
    assert_eq!(
      after.as_deref(),
      Some(blob.as_slice()),
      "the object by its identity after"
    );
    assert!(restored > 0, "the lists and names taken back");
    assert_eq!(
      (unrestored, lists),
      (0, Some(0)),
      "none refused, no list stored again"
    );
  }

  /// §4.17.1 "A push's intake" part 2 (main's ruling (B), A-727 slice b's barrier). Do: on a daemon over a retained
  /// anchor segment, land a Subversion work whose text spans several copy slices through `svn_commit::land_work`: its
  /// text put into the content store a slice a turn, its revision appended naming the text by identity only once the
  /// text is durable; then stop the daemon without publishing anything more, as a crash right after the entry, and
  /// start a successor over the same segment. Expect: the successor replays the revision with its stored text held, no
  /// replay refused, and the text read back whole from the store. Without the barrier (the entry appended right after
  /// the puts) the text is in no image and the replay is refused `SvnStored`.
  #[test]
  fn a_subversion_commits_stored_text_survives_a_crash_right_after_its_entry() {
    use slates_anchor::AnchorSegment;
    use slates_svn::commit::drive::TextSink;
    let profile = test_profile();
    let instance = format!("svn-intake-barrier-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
      .unwrap()
      .with_content(&super::content_name_of(&instance), config.content_bytes())
      .unwrap();
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      }
    };
    let bytes: Vec<u8> = (0..3 * 65_536 + 17)
      .map(|at: u32| (at % 251) as u8)
      .collect();
    let text = slates_svn::commit::drive::text_of(&bytes).unwrap();
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    first.bootstrap(true).unwrap();
    let shard = first.shards.first().copied();
    let (sent, got) = std::sync::mpsc::channel();
    let put = bytes.clone();
    first
      .observe(shard, move |state| {
        let (repository, _) = crate::repository::svn_commit::tests::subversion(state, "barrier");
        let task = slates_rt::futures::spawn(async move {
          let texts = slates_svn::commit::drive::HeldTexts::new();
          let staged = texts.stage().unwrap();
          texts.append(staged, &put).unwrap();
          texts.seal(staged, text.identity).unwrap();
          let work = slates_svn::commit::drive::Work {
            head: 0,
            record: slates_svn::index::RevisionRecord {
              transaction: "0-1".to_owned(),
              changes: vec![slates_svn::index::ChangedPath {
                path: "/a.txt".to_owned(),
                action: slates_svn::index::Action::Add,
                kind: slates_svn::index::NodeKind::File,
                copy_from: None,
                text: Some(text),
                properties: None,
                mergeinfo_modified: false,
              }],
            },
            lock_entries: Vec::new(),
            revision_properties: vec![(b"svn:author".to_vec(), b"alice".to_vec())],
            texts: std::iter::once(text.identity).collect(),
            property_lists: std::collections::BTreeMap::new(),
            locks: slates_svn::lock::CommitLocks::default(),
          };
          let owner = crate::repository::svn_commit::tests::owner();
          let landed =
            crate::repository::svn_commit::land_work((&owner, repository), work, &texts).await;
          let _ = sent.send((repository, landed.map(|landed| landed.map(|_| ()))));
        })
        .unwrap();
        slates_rt::futures::detach(task).unwrap();
      })
      .unwrap();
    let (repository, landed) = got
      .recv_timeout(std::time::Duration::from_millis(50).saturating_mul(600))
      .expect("the landing answered");
    assert_eq!(landed.map(|landed| landed.is_ok()), Some(true));
    // The crash: nothing more published.
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    let shard = second.shards.first().copied();
    let identity = text.identity;
    let read = || {
      second
        .observe(shard, move |state| {
          let refused = state
            .refusals
            .get(crate::vcs_store::REPLAY_REFUSED)
            .copied()
            .unwrap_or(0);
          let recovered = state.vcs_stores.unrecovered(repository).is_none()
            && !state.vcs_stores.rebuilding(repository)
            && state.vcs_stores.holds(repository);
          let whole = crate::vcs_store::lent_state(state, |state| {
            let len = state
              .vcs_stores
              .subversion_records(repository)?
              .stored_len(&identity)?;
            let store = state.vcs_stores.content()?;
            store.object_bytes(&store.root_of(&slates_vcs::ContentId(identity)), len)
          });
          (recovered, refused, whole)
        })
        .ok()
    };
    let deadline =
      std::time::Instant::now() + std::time::Duration::from_millis(50).saturating_mul(600);
    let mut seen = read();
    while std::time::Instant::now() < deadline
      && !seen
        .as_ref()
        .is_some_and(|seen| (seen.0 && seen.2.is_some()) || seen.1 > 0)
    {
      std::thread::park_timeout(std::time::Duration::from_millis(50));
      seen = read();
    }
    second.stop();
    drop(segment);
    let seen =
      seen.map(|(recovered, refused, whole)| (recovered, refused, whole == Some(bytes.clone())));
    assert_eq!(seen, Some((true, 0, true)));
  }

  /// A-727 slice a3 (repository windows across a warm restart). Do: on a daemon over a retained anchor segment, make a
  /// git repository and push the fixtures' `create` and `update` into it; publish every shard; stop; start a successor
  /// over the same segment. Expect: every window the first daemon kept (by identity, with its holders and its bytes)
  /// kept by the successor, taken back from the shard image (every one restored, none refused, none left unheld once
  /// the records were replayed), so the replay's puts were answered by the windows kept, none stored again.
  #[test]
  fn t_a727_repository_windows_survive_a_warm_restart_in_the_shard_image() {
    use slates_anchor::AnchorSegment;
    type Windows = Vec<(slates_vcs::ContentId, u64, Vec<u8>)>;
    fn windows(state: &mut crate::state::ShardState) -> Windows {
      let lent = crate::vcs_store::Lent::new(&mut state.store, &mut state.vcs_stores);
      let Some(content) = lent.stores.content() else {
        return Vec::new();
      };
      content
        .identities()
        .filter(|identity| content.form(identity) == Some(slates_vcs::Form::Window))
        .map(|identity| {
          let bytes = content.bytes_of(identity).map(std::borrow::Cow::into_owned);
          (
            *identity,
            content.holders(identity),
            bytes.unwrap_or_default(),
          )
        })
        .collect()
    }
    let profile = test_profile();
    let instance = format!("a727-windows-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    // The content object holds the shard's arena, so the windows' chunks survive with it (§4.8).
    let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
      .unwrap()
      .with_content(&super::content_name_of(&instance), config.content_bytes())
      .unwrap();
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      }
    };
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    first.bootstrap(true).unwrap();
    let shard = first.shards.first().copied();
    let before = first
      .observe(shard, |state| {
        let repository = crate::git_push::tests::git_repository(state, "kept");
        crate::git_push::tests::pushed_from_the_fixtures(state, repository, &["create", "update"]);
        crate::vcs_store::paths::whole_build::settle(state);
        windows(state)
      })
      .unwrap();
    first.publish_every_shard().unwrap();
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    // The successor's own shard: shard ids are the process's, and another test's daemon may take the first's.
    let shard = second.shards.first().copied();
    // A loaded host may finish the repositories' replay after the start returns: read until every window is held
    // again (a restored window is held by no one until its record's replay puts it), bounded.
    let read = || {
      second
        .observe(shard, |state| {
          let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
          let counts = (
            count(crate::vcs_store::WINDOWS_RESTORED),
            count(crate::vcs_store::WINDOWS_UNRESTORED),
            count(crate::vcs_store::WINDOWS_UNHELD),
          );
          (windows(state), counts.0, counts.1, counts.2)
        })
        .ok()
    };
    // A shard still starting refuses the submission (`ShardGone`): read again, as for a replay still under way.
    let mut seen = read();
    // Derived: 600 looks 50 ms apart, the wait the successor is given to answer with every window held.
    let deadline =
      std::time::Instant::now() + std::time::Duration::from_millis(50).saturating_mul(600);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
      if seen.as_ref().is_some_and(|seen| {
        seen.0.len() == before.len() && seen.0.iter().all(|(_, holders, _)| *holders > 0)
      }) {
        break;
      }
      std::thread::park_timeout(std::time::Duration::from_millis(50).min(left));
      seen = read();
    }
    let (after, restored, unrestored, unheld) = seen.expect("the successor answered");
    second.stop();
    assert!(!before.is_empty(), "the pushes kept windows");
    assert_eq!(
      after, before,
      "the successor keeps every window, its holders and its bytes"
    );
    assert_eq!(
      (restored, unrestored, unheld),
      (u64::try_from(before.len()).unwrap(), 0, 0)
    );
  }

  /// A-729 part 2 (an import's record names its contents by identity; main's ruling: since A-727 a3 the content survives
  /// a restart in the shard image, recovered before the records). Do: on a daemon over a retained anchor segment, import
  /// the `states` repository into a Mercurial repository a unit a turn and land it; publish every shard; stop; start a
  /// successor over the same segment. Expect the successor's index of the repository the first's row for row, made
  /// again from its identity record over the windows the image restored: every window restored, none refused, none left
  /// unheld once the records were replayed.
  #[test]
  fn t_a729_an_imported_mercurial_repository_survives_a_warm_restart_by_identity() {
    use crate::vcs_store::hg_import::tests::{created, directory, rows_of, to_its_end};
    use slates_anchor::AnchorSegment;
    let profile = test_profile();
    let instance = format!("a729-identities-{}", std::process::id());
    let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
    let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
      .unwrap()
      .with_content(&super::content_name_of(&instance), config.content_bytes())
      .unwrap();
    let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
    let source = || {
      let (handoff, len) = segment.handoff().unwrap();
      super::SegmentSource::Handoff {
        handoff,
        len,
        content: segment.content_handoff().unwrap(),
        rendezvous: rendezvous.adoption(),
      }
    };
    let first = super::Daemon::start(&profile, config.clone(), source()).unwrap();
    first.bootstrap(true).unwrap();
    let shard = first.shards.first().copied();
    let (id, before) = first
      .observe(shard, |state| {
        let id = created(state, "hg", slates_ipc::protocol::VcsTool::Mercurial);
        let laid = directory(crate::hg_base::tests::STATES.iter().copied());
        assert_eq!(crate::vcs_store::hg_import::begin(state, id, laid), Ok(()));
        assert!(to_its_end(state, id).1.is_ok(), "imported");
        assert_eq!(crate::vcs_store::hg_import::land(state, id), Ok(()));
        (id, rows_of(state, id))
      })
      .unwrap();
    first.publish_every_shard().unwrap();
    first.stop();
    let second = super::Daemon::start(&profile, config, source()).unwrap();
    let shard = second.shards.first().copied();
    let read = || {
      second
        .observe(shard, move |state| {
          let rows = if state.vcs_stores.mercurial(id).is_some() {
            rows_of(state, id)
          } else {
            Vec::new()
          };
          let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
          (
            rows,
            count(crate::vcs_store::WINDOWS_RESTORED),
            count(crate::vcs_store::WINDOWS_UNRESTORED),
            count(crate::vcs_store::WINDOWS_UNHELD),
          )
        })
        .ok()
    };
    let mut seen = read();
    // Derived: 600 looks 50 ms apart, the wait the successor is given to replay the record.
    let deadline =
      std::time::Instant::now() + std::time::Duration::from_millis(50).saturating_mul(600);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
      if seen.as_ref().is_some_and(|seen| seen.0 == before) {
        break;
      }
      std::thread::park_timeout(std::time::Duration::from_millis(50).min(left));
      seen = read();
    }
    let (after, restored, unrestored, unheld) = seen.expect("the successor answered");
    second.stop();
    assert!(!before.is_empty(), "the import kept rows");
    assert_eq!(
      after, before,
      "the successor made the index again by identity"
    );
    assert!(restored > 0, "no window was restored");
    assert_eq!((unrestored, unheld), (0, 0));
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

  /// A-337 (§4.8 direct contact and the plane's keying; the formation flake of
  /// `a_learners_reports_reach_the_council_once_the_voter_it_joined_from_is_gone`, g251's diag run 44262759484, ubuntu,
  /// 2026-11-18): the pair's canonical dialer keeps contact with a peer that holds a record session it dialed to it, and
  /// only the canonical dialer does. Do: make this node a non-voting member of seven at `f = 1`, scatter three, all held
  /// alive, and take a member its role rule drops (no neighbour, no holder, no voter); learn that member under an anchor
  /// above this node's and then below, each with no session it dialed here and with one. Expect `Dialed`, kept, only
  /// with the session and this node the lower anchor (the pair's canonical dialer); `OutsideNeighbourhood`, dropped,
  /// otherwise, so two nodes never keep each other by their sessions alone. Before, the learner in the flake, the pair's
  /// lower anchor and told stale voters, dropped a voter that kept it, and the pair's plane was never keyed.
  #[test]
  fn the_pairs_canonical_dialer_keeps_a_peer_that_dialed_it_and_only_it() {
    use slates_cluster::config_group::RegionalCouncil;
    use slates_cluster::membership::{Liveness, MemberState};
    use slates_cluster::root_group::RootGroup;
    use slates_db::register::{HostId, Quorum, RegionId};
    let (dropped, readings) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let others: Vec<HostId> = (1..=6u64).map(|n| HostId(local.0 ^ (n << 16))).collect();
      let voter = others[0];
      let mut all = vec![local];
      all.extend_from_slice(&others);
      state.council = RegionalCouncil::new(
        voter,
        all,
        vec![voter],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        3,
        false,
      );
      state.root = RootGroup::new(voter, vec![RegionId(0)], vec![voter]);
      install_council(state);
      let alive = MemberState {
        liveness: Liveness::Alive,
        incarnation: 0,
      };
      for peer in &others {
        slates_cluster::fleet::apply_peer_state(&mut state.fleet, *peer, Some(alive));
      }
      let Some(peer) = others
        .iter()
        .copied()
        .find(|peer| crate::fleet::direct_contact_reason(state, *peer) == "OutsideNeighbourhood")
      else {
        return (None, Vec::new());
      };
      let origin = state.origin_anchor;
      let mut readings = Vec::new();
      for anchor in [
        HostId(origin.0.saturating_add(1)),
        HostId(origin.0.saturating_sub(1)),
      ] {
        state.learned_members.clear();
        state.learned_members.insert(
          anchor,
          crate::state::LearnedMember {
            boot_nonce: 1,
            host: peer,
            previous: None,
          },
        );
        for sessions in [0u32, 1] {
          state.dialed_in.clear();
          if sessions > 0 {
            state.dialed_in.insert(anchor, sessions);
          }
          readings.push((
            anchor > origin,
            sessions,
            crate::fleet::direct_contact_reason(state, peer),
            crate::fleet::keeps_direct_contact_with(state, peer),
          ));
        }
      }
      (Some(peer), readings)
    });
    assert!(
      dropped.is_some(),
      "a member of seven at scatter three falls outside this node's role rule"
    );
    let expected: Vec<(bool, u32, String, bool)> = vec![
      (true, 0, "OutsideNeighbourhood".to_owned(), false),
      (true, 1, "Dialed".to_owned(), true),
      (false, 0, "OutsideNeighbourhood".to_owned(), false),
      (false, 1, "OutsideNeighbourhood".to_owned(), false),
    ];
    assert_eq!(
      readings, expected,
      "(the peer's anchor above this node's, sessions it dialed here, the reason, kept)"
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
      crate::owner_location::serve(state, HostId(local.0 ^ 1), &query.to_bytes())
    });
    assert!(
      matches!(claimed, Ok((_, None))),
      "the owner claims its object while its placement catches up: {claimed:?}"
    );
  }

  /// §4.8 Lookup, takeover: a node that adopted a volume's head owns it before its owner shard has materialized it.
  /// Its own client's verb is then refused with the owner shard's retryable `Overloaded`, counted, and never with
  /// `HomedElsewhere`, which names another region (three fleet tests, 2026-10-07, refused a successor's own client so
  /// for a whole wait). Do: route a volume created by another host to this node, as an adoption does, and ask the
  /// forward's first question. Expect: `Overloaded`, and the counter moved. Do: route one to its creator. Expect: no
  /// refusal from this rule (the location round finds the owner).
  #[test]
  fn a_successor_refuses_its_unmaterialized_volume_as_retryable_not_homed_elsewhere() {
    use slates_db::register::{HostId, ObjectId};
    let (adopted, elsewhere, counted) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let adopted = ObjectId::new(HostId(local.0 ^ 1), 1);
      let elsewhere = ObjectId::new(HostId(local.0 ^ 1), 2);
      state.fleet.track_object_owner(adopted, local);
      state
        .fleet
        .track_object_owner(elsewhere, HostId(local.0 ^ 1));
      let adopted_refusal = crate::verbs::owned_unmaterialized(
        state,
        slates_ipc::protocol::VolumeId { bytes: adopted.0 },
      );
      let elsewhere_refusal = crate::verbs::owned_unmaterialized(
        state,
        slates_ipc::protocol::VolumeId { bytes: elsewhere.0 },
      );
      let counted = state
        .refusals
        .get("fleet.owner_location.owned_unmaterialized")
        .copied();
      (adopted_refusal, elsewhere_refusal, counted)
    });
    assert!(
      matches!(
        adopted,
        Some(slates_ipc::protocol::Refusal::Overloaded { .. })
      ),
      "an owned, unmaterialized volume is refused retryably: {adopted:?}"
    );
    assert_eq!(
      elsewhere, None,
      "another host's volume is located, not refused here"
    );
    assert_eq!(counted, Some(1), "the refusal is counted once");
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
        owner: slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        completion: None,
        signature: None,
        folded_below: 0,
        checkpoint_floor: 0,
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

  /// Shape: the content puts the content-delta test holds, one publication each: enough that a delta carrying the whole
  /// hold's image would carry tens of times what one put is.
  const CONTENT_PUTS: u64 = 64;

  /// §4.8 "persistence before reply", A-376: a holder publishes before it acknowledges each content put, and what that
  /// publication carries must be the content it put, never the hold it already carries. Do: on a shard, hold
  /// [`CONTENT_PUTS`] inputs of one green, each placed for its own version and of one size, building after each the delta
  /// a publication would carry and committing it. Expect every delta to carry the same bytes (the put's manifest and its
  /// one new chunk), and the pieces every delta carried, folded in order, to be the hold's whole image. Before: the hold's
  /// image was one piece, re-encoded at every publication and re-put whenever it changed, so the n-th put carried every
  /// input held before it.
  #[test]
  fn a_holders_publication_carries_the_content_it_puts_not_the_whole_hold() {
    let (carried, folded_whole) = audit_on_shard(|state| {
      use slates_db::register::{HostId, ObjectId};
      let green = ObjectId::new(HostId(9), 1);
      state.published_held = crate::content_holder::PublishedHeld::default();
      crate::content_holder::held_committed(state, std::collections::BTreeMap::new(), true);
      let mut folded: std::collections::BTreeMap<
        [u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES],
        Vec<u8>,
      > = crate::content_holder::held_image(state)
        .into_iter()
        .map(|piece| (piece.key, piece.bytes))
        .collect();
      let mut carried = std::collections::BTreeSet::new();
      for version in 1..=CONTENT_PUTS {
        let archive =
          crate::merge_origin::InputsBuilder::whole(&version.to_le_bytes()).finish(version, 4096);
        state
          .held_content
          .hold(
            &mut crate::content_holder::hold_space(&mut state.store),
            green,
            slates_cluster::content::Placed { sequence: version },
            archive,
          )
          .unwrap();
        let changes = crate::content_holder::held_changes(state);
        carried.insert(
          changes
            .put
            .iter()
            .map(|piece| piece.bytes.len())
            .sum::<usize>(),
        );
        for piece in &changes.put {
          folded.insert(piece.key, piece.bytes.clone());
        }
        for key in &changes.removed {
          folded.remove(key);
        }
        crate::content_holder::held_committed(state, changes.whole, false);
      }
      let whole: std::collections::BTreeMap<_, _> = crate::content_holder::held_image(state)
        .into_iter()
        .map(|piece| (piece.key, piece.bytes))
        .collect();
      (carried, folded == whole)
    });
    assert_eq!(
      carried.len(),
      1,
      "every put's delta carries its manifest and its chunk, whatever the hold holds: {carried:?}"
    );
    assert!(folded_whole, "the folded deltas are the hold's whole image");
  }

  /// One step of the held-pieces oracle's history (A-270): a change to what the shard holds, or a publication.
  #[derive(Clone, Copy, Debug)]
  enum HeldStep {
    /// An entry kept after the last of live copy `copy` (made when it holds none).
    Push(u8),
    /// Live copy `copy`'s entries dropped, as a whole entry does.
    Clear(u8),
    /// Live copy `copy` let go.
    Drop(u8),
    /// A register put in place for object `object`: a fresh acceptor that keeps no changes, holding one position.
    Hold(u8),
    /// Object `object`'s register dropped.
    Forget(u8),
    /// The next position of object `object`'s register accepted, its acceptor made, keeping its changes, when none is
    /// held (A-367: the ledger a holder of a green keeps).
    Accept(u8),
    /// Object `object`'s newest position accepted again at a newer epoch, as a takeover re-commits it (A-367).
    Again(u8),
    /// Object `object`'s register compacted below its newest position, as a single-value register is (A-367).
    Compact(u8),
    /// A fresh acceptor that keeps its changes put in place of object `object`'s, holding one position (A-367): its
    /// changes do not account for the published positions it replaced.
    Renew(u8),
    /// Content held for object `object`, placed for the step's index (A-376).
    Put(u8),
    /// Everything held for object `object` released, as a tombstone releases it (A-376).
    Release(u8),
    /// A delta published: what `held_changes` carries, folded over the model.
    Delta,
    /// A checkpoint published: everything held.
    Checkpoint,
  }

  /// Accepts into `acceptor` a record of `object` from the owner its authority names, under its generation: at
  /// `again`, its value unchanged at an epoch one past its fence (a takeover's re-commit); else at the position after its
  /// highest, at its fence's epoch, valued by `at`. Whether it was accepted.
  fn held_accept(
    acceptor: &mut slates_db::register::Acceptor,
    object: slates_db::register::ObjectId,
    at: usize,
    again: Option<u64>,
  ) -> bool {
    use slates_db::register::{HostEpoch, Record};
    let (promised, _) = acceptor.persisted();
    let (sequence, epoch, value) = match again {
      Some(sequence) => {
        let Some(value) = acceptor.value_at(object, sequence).map(<[u8]>::to_vec) else {
          return false;
        };
        (sequence, HostEpoch(promised.0.saturating_add(1)), value)
      }
      None => {
        let next = acceptor
          .highest(object)
          .map_or(1, |(sequence, _)| sequence.saturating_add(1));
        (next, promised, format!("value {at}").into_bytes())
      }
    };
    acceptor
      .accept(&Record {
        owner: acceptor.owner(),
        object,
        sequence,
        epoch,
        generation: acceptor.generation(),
        value,
      })
      .is_ok()
  }

  /// A-270, A-367, the oracle of the incremental held image: do run a generated history of live copies kept, cleared,
  /// let go and made again, and registers held, dropped, grown a position at a time, accepted again at a newer epoch,
  /// compacted and replaced, publishing deltas and checkpoints between them. Expect after every publication the pieces a
  /// restart would fold (each checkpoint's, each delta's put and removed applied in order) to equal everything the shard
  /// holds (`held_image`). A change the tracked maps or an acceptor's changes missed, a live slot reused by a copy made
  /// again, or a key both put and removed fails here as a difference, before it is a record a restart lost.
  #[test]
  fn folding_every_held_delta_over_its_checkpoint_gives_what_the_shard_holds() {
    use proptest::strategy::Strategy;
    let failures = audit_on_shard(|state| {
      use slates_db::register::{Acceptor, Authority, HostId, ObjectId};
      type Model =
        std::collections::BTreeMap<[u8; slates_vfs::recover::HELD_PIECE_KEY_BYTES], Vec<u8>>;
      let local = state.fleet.host();
      let step = (0u8..14, 0u8..3).prop_map(|(kind, which)| match kind {
        0 | 1 => HeldStep::Push(which),
        2 => HeldStep::Clear(which),
        3 => HeldStep::Drop(which),
        4 => HeldStep::Hold(which),
        5 => HeldStep::Forget(which),
        6 | 7 => HeldStep::Accept(which),
        8 => HeldStep::Again(which),
        9 => HeldStep::Compact(which),
        10 => HeldStep::Renew(which),
        12 => HeldStep::Put(which),
        13 => HeldStep::Release(which),
        _ if which == 0 => HeldStep::Checkpoint,
        _ => HeldStep::Delta,
      });
      let mut runner = proptest::test_runner::TestRunner::new(slates_test_seeds::unseeded(
        proptest::test_runner::Config {
          cases: slates_test_seeds::cases(64),
          ..proptest::test_runner::Config::default()
        },
      ));
      // The runner's case is an `Fn`, so each case borrows the one shard's state through a cell.
      let cell = std::cell::RefCell::new(state);
      let outcome = runner.run(&proptest::collection::vec(step, 1..60), |steps| {
        let mut borrowed = cell.borrow_mut();
        let state: &mut crate::state::ShardState = &mut borrowed;
        state.holder_records.clear();
        state.live.held.clear();
        state
          .held_content
          .forget_all(&mut crate::content_holder::hold_space(&mut state.store));
        state.published_held = crate::content_holder::PublishedHeld::default();
        crate::content_holder::held_committed(state, std::collections::BTreeMap::new(), true);
        let mut model: Model = Model::new();
        let object = |which: u8| ObjectId([which.saturating_add(1); 16]);
        for (at, step) in steps.iter().enumerate() {
          match *step {
            HeldStep::Push(which) => {
              let first = state.live.slot_floor;
              state
                .live
                .held
                .entry(object(which))
                .or_insert_with(|| crate::live_ship::Held::for_test(local, object(which), first))
                .push_for_test(format!("entry {at}").into_bytes());
            }
            HeldStep::Clear(which) => {
              if let Some(held) = state.live.held.get_mut(&object(which)) {
                held.clear_for_test();
              }
            }
            HeldStep::Drop(which) => {
              state.live.held.remove(&object(which));
            }
            HeldStep::Hold(which) => {
              let mut fresh = Acceptor::new(
                local,
                Authority {
                  generation: u64::try_from(at).unwrap(),
                  owner: HostId(local.0 ^ 1),
                },
              );
              held_accept(&mut fresh, object(which), at, None);
              state.holder_records.insert(object(which), fresh);
            }
            HeldStep::Forget(which) => {
              state.holder_records.remove(&object(which));
            }
            HeldStep::Accept(which) => {
              let acceptor = state
                .holder_records
                .entry(object(which))
                .or_insert_with(|| {
                  Acceptor::new(
                    local,
                    Authority {
                      generation: 0,
                      owner: HostId(local.0 ^ 1),
                    },
                  )
                  .tracking_changes()
                });
              held_accept(acceptor, object(which), at, None);
            }
            HeldStep::Again(which) => {
              if let Some(acceptor) = state.holder_records.get_mut(&object(which))
                && let Some((sequence, _)) = acceptor.highest(object(which))
              {
                held_accept(acceptor, object(which), at, Some(sequence));
              }
            }
            HeldStep::Compact(which) => {
              if let Some(acceptor) = state.holder_records.get_mut(&object(which))
                && let Some((sequence, _)) = acceptor.highest(object(which))
              {
                acceptor.compact_below(object(which), sequence);
              }
            }
            HeldStep::Put(which) => {
              let index = u64::try_from(at).unwrap();
              let archive =
                crate::merge_origin::InputsBuilder::whole(&index.to_le_bytes()).finish(index, 4096);
              let _ = state.held_content.hold(
                &mut crate::content_holder::hold_space(&mut state.store),
                object(which),
                slates_cluster::content::Placed { sequence: index },
                archive,
              );
            }
            HeldStep::Release(which) => {
              state.held_content.forget_object(
                &mut crate::content_holder::hold_space(&mut state.store),
                object(which),
              );
            }
            HeldStep::Renew(which) => {
              let mut fresh = Acceptor::new(
                local,
                Authority {
                  generation: u64::try_from(at).unwrap(),
                  owner: HostId(local.0 ^ 1),
                },
              )
              .tracking_changes();
              held_accept(&mut fresh, object(which), at, None);
              state.holder_records.insert(object(which), fresh);
            }
            HeldStep::Delta => {
              let changes = crate::content_holder::held_changes(state);
              for piece in &changes.put {
                proptest::prop_assert!(
                  !changes.removed.contains(&piece.key),
                  "step {at}: a key both put and removed"
                );
                model.insert(piece.key, piece.bytes.clone());
              }
              for key in &changes.removed {
                model.remove(key);
              }
              crate::content_holder::held_committed(state, changes.whole, false);
            }
            HeldStep::Checkpoint => {
              let image = crate::content_holder::held_image(state);
              let whole = crate::content_holder::whole_of(&image);
              model = image
                .into_iter()
                .map(|piece| (piece.key, piece.bytes))
                .collect();
              crate::content_holder::held_committed(state, whole, true);
            }
          }
          if matches!(step, HeldStep::Delta | HeldStep::Checkpoint) {
            let held: Model = crate::content_holder::held_image(state)
              .into_iter()
              .map(|piece| (piece.key, piece.bytes))
              .collect();
            proptest::prop_assert_eq!(&model, &held, "step {} ({:?}) of {:?}", at, step, steps);
          }
        }
        Ok(())
      });
      outcome.err().map(|failure| format!("{failure}"))
    });
    assert_eq!(
      failures, None,
      "the folded held pieces equal what the shard holds"
    );
  }

  /// What [`a_give_back_step_learns_its_slice_from_its_own_cpu_not_the_wall_clock`] saw after each step: the slice
  /// learned, and whether dead memory was left (so the step walked a whole slice).
  #[derive(Debug)]
  struct Learned {
    /// The shard's step quantum and page.
    quantum: u64,
    page: usize,
    /// After each step: the slice learned, and whether dead memory was left.
    steps: Vec<(usize, bool)>,
  }

  /// Takes one give-back step of `state`'s dead recovery memory, its CPU clock reading `started` before it and
  /// `ended` after it: the slice then learned, and whether dead memory was left.
  fn give_back_step_timed(
    state: &mut crate::state::ShardState,
    started: u64,
    ended: u64,
  ) -> (usize, bool) {
    let mut readings = [Some(started), Some(ended)].into_iter();
    let mut clock = || readings.next().flatten();
    let (_, more) = super::return_dead_recovery_timed(state, &mut clock);
    (state.give_back_rate.slice_bytes, more)
  }

  /// A-491 (A-226's slice; CLAUDE.md "Bounded work everywhere": a step is held to its shard's quantum of its own CPU).
  /// Do: on a shard whose recovery memory is all dead (an image committed, the journal's record of what it gave back
  /// forgotten, as after a restart), take give-back steps timed by a controlled CPU clock: one spending a quantum, one
  /// half a quantum, then three timed as Windows' coarse thread clock times them, the first two showing no CPU and the
  /// third a quantum. Expect each slice learned to be the bytes the steps walked per quantum of the CPU they spent: a
  /// page, then two, then two still while the clock shows nothing, then the three steps' six pages, whatever wall time
  /// the steps took. Before, a step was timed by the wall clock, so the slice followed the host's scheduling (here the
  /// steps' real wall time, microseconds, not the quantum the clock says they spent).
  #[test]
  fn a_give_back_step_learns_its_slice_from_its_own_cpu_not_the_wall_clock() {
    let seen = audit_on_shard(|state| {
      let _ = crate::verbs::publish_shard(state);
      state.journal.forget_what_was_given_back();
      state.give_back_rate = super::GiveBackRate::default();
      let quantum = slates_rt::futures::step_budget_ns()
        .unwrap_or(state.config.runtime.step_budget_ns)
        .max(2);
      let page = usize::try_from(state.config.geometry.page)
        .unwrap_or(1)
        .max(1);
      let half = quantum / 2;
      let steps = vec![
        give_back_step_timed(state, 0, quantum),
        give_back_step_timed(state, quantum, quantum + half),
        give_back_step_timed(state, 2 * quantum, 2 * quantum),
        give_back_step_timed(state, 2 * quantum, 2 * quantum),
        give_back_step_timed(state, 2 * quantum, 3 * quantum),
      ];
      Learned {
        quantum,
        page,
        steps,
      }
    });
    let per_quantum = |walked: usize, spent: u64| -> usize {
      usize::try_from(
        u128::try_from(walked).unwrap() * u128::from(seen.quantum) / u128::from(spent),
      )
      .unwrap()
    };
    let half = seen.quantum / 2;
    let two = per_quantum(seen.page, half);
    let expected = [seen.page, two, two, two, per_quantum(3 * two, seen.quantum)];
    assert!(
      seen.steps.iter().all(|(_, more)| *more),
      "every step left dead memory, so each walked a whole slice: {seen:?}"
    );
    let learned: Vec<usize> = seen.steps.iter().map(|(slice, _)| *slice).collect();
    assert_eq!(
      learned, expected,
      "each slice is the bytes walked per quantum of the CPU spent, steps a coarse clock shows none of folded into the \
       next it shows: {seen:?}"
    );
  }

  /// §4.8 "Promotion and takeover", the f + 1 rule: do make the audit node the leader-to-be of a three-voter `f = 1`
  /// council, then judge who it may retire as the voters' views change. Expect its own lone death vote for a member the
  /// others hold alive (a member it keyed slowly after a restart) to retire nothing, one other voter's fresh report to
  /// make it `f + 1` and retire the member, and a report below the incarnation the leader holds the member alive at to
  /// count for nothing. Before this rule the leader's own view alone retired a member.
  #[test]
  fn a_lone_voters_suspicion_never_retires_a_member() {
    let (alone, confirmed, refuted) = audit_on_shard(|state| {
      use slates_cluster::config_group::RegionalCouncil;
      use slates_cluster::membership::{Liveness, MemberState};
      use slates_db::register::{HostId, Quorum};
      let local = state.fleet.host();
      let (suspected, other) = (HostId(local.0 ^ 1), HostId(local.0 ^ 2));
      state.council = RegionalCouncil::new(
        local,
        vec![local, suspected, other],
        vec![local, suspected, other],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        3,
        false,
      );
      install_council(state);
      // This node's own stable vote: its death watch past the confirmation window.
      state.council_death_watch.insert(suspected, u32::MAX);
      let alone = crate::fleet::confirmed_dead_council_members(state);
      let now = slates_rt::futures::now_ns();
      state
        .death_reports
        .entry(suspected)
        .or_default()
        .insert(other, (0, now));
      let confirmed = crate::fleet::confirmed_dead_council_members(state);
      // The member refuted at incarnation 3; the report names incarnation 0, so it no longer counts.
      state.fleet.observe(
        suspected,
        MemberState {
          liveness: Liveness::Alive,
          incarnation: 3,
        },
      );
      let refuted = crate::fleet::confirmed_dead_council_members(state);
      (alone, confirmed, refuted)
    });
    assert!(
      alone.is_empty(),
      "one voter's vote retires nothing: {alone:?}"
    );
    assert_eq!(
      confirmed.len(),
      1,
      "f + 1 voters retire the member: {confirmed:?}"
    );
    assert!(
      refuted.is_empty(),
      "a refutation cancels the reports below it: {refuted:?}"
    );
  }

  /// §4.8 "Promotion and takeover", the f + 1 rule's floor: do make the audit node the sole voter of a forming `f = 1`
  /// region of three members (the council widens only by steps that raise the failures it tolerates, so it stays at
  /// its bootstrap alone until both others vote), and hold a member dead past the confirmation window. Expect its own
  /// vote to retire the member: there is no other voter to report, as a joint write needs every member of a cohort
  /// smaller than `f + 1` (`Quorum::committed_within`). Demanding `f + 1` votes from one voter retired nobody, so an
  /// owner killed while its fleet formed was never taken over (`an_owner_killed_while_its_fleet_still_forms_is_taken_over`,
  /// red 3 of 3 alone).
  #[test]
  fn a_sole_voters_own_view_retires_a_member_while_its_council_forms() {
    let confirmed = audit_on_shard(|state| {
      use slates_cluster::config_group::RegionalCouncil;
      use slates_db::register::{HostId, Quorum};
      let local = state.fleet.host();
      let (suspected, other) = (HostId(local.0 ^ 1), HostId(local.0 ^ 2));
      state.council = RegionalCouncil::new(
        local,
        vec![local, suspected, other],
        vec![local],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        3,
        false,
      );
      install_council(state);
      state.council_death_watch.insert(suspected, u32::MAX);
      crate::fleet::confirmed_dead_council_members(state)
    });
    assert_eq!(
      confirmed.len(),
      1,
      "the sole voter's own vote retires the member: {confirmed:?}"
    );
  }

  /// Makes the audit node one of three members of an `f = 1` region whose council it alone votes in, holding
  /// one record of `object` written by the second member; then retires that member and installs the result, so
  /// the object resolves to its successor among the two survivors. Returns the retired member and the successor.
  fn retire_the_owner_of_a_held_object(
    state: &mut crate::state::ShardState,
    object: slates_db::register::ObjectId,
  ) -> (slates_db::HostId, slates_db::HostId) {
    retire_the_owner_of_a_held_record(state, object, 0)
  }

  /// [`retire_the_owner_of_a_held_object`], the held record at ledger position `sequence`.
  fn retire_the_owner_of_a_held_record(
    state: &mut crate::state::ShardState,
    object: slates_db::register::ObjectId,
    sequence: u64,
  ) -> (slates_db::HostId, slates_db::HostId) {
    let local = state.fleet.host();
    retire_a_named_owner_of_a_held_record(
      state,
      object,
      sequence,
      (
        slates_db::HostId(local.0 ^ 1),
        slates_db::HostId(local.0 ^ 2),
      ),
    )
  }

  /// [`retire_the_owner_of_a_held_record`] with the other two members named: `departed`, the owner retired, and `third`.
  fn retire_a_named_owner_of_a_held_record(
    state: &mut crate::state::ShardState,
    object: slates_db::register::ObjectId,
    sequence: u64,
    (departed, third): (slates_db::HostId, slates_db::HostId),
  ) -> (slates_db::HostId, slates_db::HostId) {
    use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
    use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, Quorum, Record};
    let local = state.fleet.host();
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
        sequence,
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
        named: Vec::new(),
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
        named: Vec::new(),
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

  /// A-351 (§4.8 "Leases and reads", AUD-08; the chaos nemesis's deafened owner, seed 6876545201569229162): a holder that
  /// keeps answering a retired owner's probes still promises the owner's objects once its last answer under the owner's
  /// membership is past the horizon. Do: learn a peer on contact; make it the owner of a record this node holds, this
  /// node's last answer to it while it was a member past the horizon; retire it; answer its probe, announcing an older
  /// configuration, as the probes of an owner deaf to every answer keep coming; then answer the successor's takeover
  /// page. Expect: the probe answered with no standing, and the page promising the record. Before, every answer to the
  /// retired owner closed the gate again, so a holder deferred every page for as long as the owner probed, and the
  /// takeover gave the owner's objects up.
  #[test]
  fn a_holder_answering_a_retired_owners_probes_still_promises_its_objects() {
    use slates_db::register::{HostEpoch, HostPrepare, HostPromise, ObjectId};
    let (answered, page) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let anchor = slates_db::HostId(local.0 ^ 0x40);
      let owner = crate::deploy::member_id(anchor, 1);
      crate::fleet::learn_member(state, anchor, 1, owner);
      let object = ObjectId([31; 16]);
      let (departed, successor) = retire_a_named_owner_of_a_held_record(
        state,
        object,
        0,
        (owner, slates_db::HostId(local.0 ^ 2)),
      );
      let now = slates_machine::clock::monotonic_ns();
      state
        .answers_given
        .alive_answers
        .insert(departed, now - crate::lease::horizon_ns() - 1);
      let older = state.council.configuration().version.saturating_sub(1);
      let answered = crate::member_task::answer_probe(state, departed, 1, older);
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation: state.fleet.configuration().version,
        after: None,
        named: Vec::new(),
      }
      .encode();
      let page = crate::takeover::serve_host_prepare(state, local, successor, &prepare);
      (answered, page)
    });
    assert_eq!(
      answered.map(|(_, standing)| standing),
      Some(None),
      "the retired owner is answered, with no standing"
    );
    let promise = HostPromise::decode(&page);
    assert!(
      promise
        .as_ref()
        .is_ok_and(|promise| promise.entries.len() == 1),
      "the holder promises the retired owner's record: {promise:?}"
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

  /// A-251 (b), the holder's side, the warm-restart case (`docs/bugs/2026-11-07-a-holder-restarted-warm-lost-its-copies-routing.md`):
  /// a holder that lost a copy's lineage still lists it, by the authority the copy was written under (shape (i)), and
  /// answers by name for it and for what it holds nothing of. Do: retire the owner of a record this node holds, forget
  /// the copy's lineage, then answer the successor's page and its question by name. Expect: the page lists the copy and
  /// is complete; the answer by name reports the record, and an object this node has no record of as held nothing of.
  #[test]
  fn a_holder_lists_a_copy_whose_lineage_it_lost_and_answers_by_name() {
    use slates_db::register::{HostEpoch, HostPrepare, HostPromise, ObjectId};
    let object = ObjectId([29; 16]);
    let (page, named, absent) = audit_on_shard(move |state| {
      let local = state.fleet.host();
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      state.departed_owners.remove(&object);
      let regional = state.council.configuration().clone();
      let absent = (0..=u8::MAX)
        .map(|seed| ObjectId([seed; 16]))
        .find(|other| *other != object && regional.successor(departed, *other) == Some(successor))
        .unwrap();
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation: state.fleet.configuration().version,
        after: None,
        named: Vec::new(),
      };
      let page = crate::takeover::serve_host_prepare(state, local, successor, &prepare.encode());
      let question = HostPrepare {
        named: vec![object, absent],
        ..prepare
      };
      let named = crate::takeover::serve_host_prepare(state, local, successor, &question.encode());
      (page, named, absent)
    });
    let page = HostPromise::decode(&page).expect("the page answers");
    assert_eq!(
      page
        .entries
        .iter()
        .map(|(listed, _)| *listed)
        .collect::<Vec<_>>(),
      vec![object],
      "the page lists the copy by its authority: {page:?}"
    );
    assert!(page.next.is_none(), "and is complete");
    let named = HostPromise::decode(&named).expect("the question answers");
    assert_eq!(
      named
        .entries
        .iter()
        .map(|(listed, _)| *listed)
        .collect::<Vec<_>>(),
      vec![object],
      "the held copy is reported by name"
    );
    assert_eq!(
      named.nothing,
      vec![absent],
      "and what it does not hold is said to be held nothing of"
    );
  }

  /// A-251 (b), the successor's own copy: its routing can lose a copy it holds as any holder's can, and its own copies
  /// are listed by their authority as a holder's are. Do: at f = 1, this node the successor of an object it holds at
  /// position 1; forget the copy's lineage here; fold the third member's page listing an older record (position 0).
  /// Expect: the object is adopted with this node's newer record, not the third member's older one.
  #[test]
  fn a_successor_whose_routing_lost_its_own_copy_still_adopts_it() {
    use slates_db::register::{Accepted, HostEpoch, HostPromise, ObjectId};
    let ready = audit_on_shard(|state| {
      let local = state.fleet.host();
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
      let third = slates_db::HostId(local.0 ^ 2);
      let (retired, successor) = retire_the_owner_of_a_held_record(state, object, 1);
      assert_eq!((retired, successor), (departed, local));
      state.departed_owners.remove(&object);
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let requests = crate::takeover::begin_round(state, departed, local).unwrap();
      let (_, page_request) = requests
        .iter()
        .find(|(host, _)| *host == third)
        .unwrap()
        .clone();
      let older = HostPromise {
        holder: third,
        departed,
        epoch: page_request.epoch,
        generation: page_request.generation,
        entries: vec![(
          object,
          Accepted {
            sequence: 0,
            epoch: HostEpoch(1),
            value: b"older".to_vec(),
          },
        )],
        next: None,
        fenced: None,
        unpromised: 0,
        nothing: Vec::new(),
      };
      crate::takeover::fold_answers(state, departed, vec![(third, older, false)]);
      crate::takeover::ready_objects(state, departed, local)
    });
    assert_eq!(ready.len(), 1, "{ready:?}");
    assert_eq!(
      ready[0].1.sequence, 1,
      "this node's newer copy, not the third member's older one: {ready:?}"
    );
  }

  /// A-251 (b), shape (i) as one rule, the moved case: an install moves a retired owner's copies to the successor
  /// (`fleet::reconcile_held_authority`), and a holder restored warm then routes such a copy to the successor as a live
  /// owner, so neither its routing nor its authority alone names the retired host. Do: retire the owner of a record this
  /// node holds, run the authority reconcile, forget the copy's lineage, and answer the successor's page. Expect: the
  /// copy's acceptor records the owner it moved from, the page lists the copy, and
  /// `fleet.takeover.listed_by_moved_from` counts it.
  #[test]
  fn a_copy_moved_to_the_successor_is_listed_by_the_owner_it_moved_from() {
    use slates_db::register::{HostEpoch, HostPrepare, HostPromise, ObjectId};
    let object = ObjectId([37; 16]);
    let (moved_from, page, counted) = audit_on_shard(move |state| {
      let local = state.fleet.host();
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      crate::fleet::reconcile_held_authority(state);
      let moved_from = state.holder_records.get(&object).and_then(|acceptor| {
        (acceptor.owner() == successor)
          .then(|| acceptor.moved_from())
          .flatten()
      });
      state.departed_owners.remove(&object);
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation: state.fleet.configuration().version,
        after: None,
        named: Vec::new(),
      };
      let page = crate::takeover::serve_host_prepare(state, local, successor, &prepare.encode());
      let counted = state
        .refusals
        .get("fleet.takeover.listed_by_moved_from")
        .copied()
        .unwrap_or(0);
      (moved_from.map(|from| from == departed), page, counted)
    });
    assert_eq!(
      moved_from,
      Some(true),
      "the install moved the copy from the retired owner"
    );
    let page = HostPromise::decode(&page).expect("the page answers");
    assert_eq!(
      page
        .entries
        .iter()
        .map(|(listed, _)| *listed)
        .collect::<Vec<_>>(),
      vec![object],
      "listed by the owner it moved from: {page:?}"
    );
    assert_eq!(counted, 1);
  }

  /// A-270 with A-251 (b), the moved case reaching the journal only in a delta: a copy whose authority moves after the
  /// last checkpoint is restored from that checkpoint and the delta after it, the move with it. Do: retire the owner of
  /// a record this node holds and publish a checkpoint of the held pieces before the move; run the authority reconcile
  /// (the move); build the delta's held pieces (`held_changes`) and fold them over the checkpoint's as recovery does;
  /// drop the acceptors, routing and lineage and restore them from the folded pieces; answer the successor's page.
  /// Expect: no move at the checkpoint, the register's piece carried by the delta, the restored acceptor naming the
  /// retired owner it moved from, and the page listing the copy. A move whose mark the tracked map missed would leave
  /// the checkpoint's piece in place, and the restored copy would name no move and go unlisted.
  #[test]
  fn a_copy_moved_after_the_checkpoint_is_restored_from_its_delta_and_still_listed() {
    use slates_db::register::{HostEpoch, HostPrepare, HostPromise, ObjectId};
    let object = ObjectId([47; 16]);
    let (before_move, in_delta, restored_from, departed, page) = audit_on_shard(move |state| {
      let local = state.fleet.host();
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      let before_move = state
        .holder_records
        .get(&object)
        .and_then(|acceptor| acceptor.moved_from());
      // The checkpoint, before the move.
      let checkpoint = crate::content_holder::held_image(state);
      crate::content_holder::held_committed(
        state,
        crate::content_holder::whole_of(&checkpoint),
        true,
      );
      let mut journal: std::collections::BTreeMap<_, _> = checkpoint
        .into_iter()
        .map(|piece| (piece.key, piece.bytes))
        .collect();
      // The move, then the delta after it, folded as recovery folds it (`ShardImage::apply`: puts, then removals).
      crate::fleet::reconcile_held_authority(state);
      let changes = crate::content_holder::held_changes(state);
      let in_delta = changes.put.len();
      for piece in &changes.put {
        journal.insert(piece.key, piece.bytes.clone());
      }
      for key in &changes.removed {
        journal.remove(key);
      }
      crate::content_holder::held_committed(state, changes.whole, false);
      let folded: Vec<slates_vfs::recover::HeldPiece> = journal
        .into_iter()
        .map(|(key, bytes)| slates_vfs::recover::HeldPiece { key, bytes })
        .collect();
      state.holder_records.clear();
      state.fleet.forget_object(object);
      state.departed_owners.remove(&object);
      let held = crate::content_holder::split_held(&folded).unwrap();
      assert_eq!(
        crate::content_holder::restore_held_registers(state, local, held),
        1
      );
      let restored_from = state
        .holder_records
        .get(&object)
        .and_then(|acceptor| acceptor.moved_from());
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation: state.fleet.configuration().version,
        after: None,
        named: Vec::new(),
      };
      let page = crate::takeover::serve_host_prepare(state, local, successor, &prepare.encode());
      (before_move, in_delta, restored_from, departed, page)
    });
    assert_eq!(
      before_move, None,
      "the checkpoint was taken before the move"
    );
    assert!(in_delta > 0, "the delta carried the moved register's piece");
    assert_eq!(
      restored_from,
      Some(departed),
      "the checkpoint and its delta kept the owner the copy moved from"
    );
    let page = HostPromise::decode(&page).expect("the page answers");
    assert_eq!(
      page
        .entries
        .iter()
        .map(|(listed, _)| *listed)
        .collect::<Vec<_>>(),
      vec![object],
      "the restored copy is listed: {page:?}"
    );
  }

  /// A-251 (b), the moved case across a warm restart: the held image keeps the retired owner a copy's authority moved
  /// from, so after the restore, which routes the copy to the successor as a live owner, the successor's page still
  /// lists it. Do: retire the owner of a record this node holds; run the authority reconcile; write the held image, drop
  /// the acceptors, routing and lineage, and restore them from the image; answer the successor's page. Expect: the
  /// restored acceptor names the retired owner it moved from, and the page lists the copy.
  #[test]
  fn a_restored_copy_keeps_the_owner_it_moved_from_and_is_still_listed() {
    use slates_db::register::{HostEpoch, HostPrepare, HostPromise, ObjectId};
    let object = ObjectId([43; 16]);
    let (restored_from, departed, page) = audit_on_shard(move |state| {
      let local = state.fleet.host();
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      crate::fleet::reconcile_held_authority(state);
      let image = crate::content_holder::held_image(state);
      state.holder_records.clear();
      state.fleet.forget_object(object);
      state.departed_owners.remove(&object);
      let held = crate::content_holder::split_held(&image).unwrap();
      assert_eq!(
        crate::content_holder::restore_held_registers(state, local, held),
        1
      );
      let restored_from = state
        .holder_records
        .get(&object)
        .and_then(|acceptor| acceptor.moved_from());
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation: state.fleet.configuration().version,
        after: None,
        named: Vec::new(),
      };
      let page = crate::takeover::serve_host_prepare(state, local, successor, &prepare.encode());
      (restored_from, departed, page)
    });
    assert_eq!(
      restored_from,
      Some(departed),
      "the image kept the owner the copy moved from"
    );
    let page = HostPromise::decode(&page).expect("the page answers");
    assert_eq!(
      page
        .entries
        .iter()
        .map(|(listed, _)| *listed)
        .collect::<Vec<_>>(),
      vec![object],
      "the restored copy is listed: {page:?}"
    );
  }

  /// A-251 (b), shape (i) as one rule, the asker's own object: a copy under the asker's own authority, which no install
  /// moved from the retired host's line, is never listed in the retired host's round, though the configuration names the
  /// asker its successor there too. Listing it would raise its fence to the round's epoch and could refuse the asker's
  /// own writes. Do: retire an owner; hold one of the successor's own objects whose successor in the round is the
  /// successor; answer the successor's page. Expect: the page lists only the retired owner's copy.
  #[test]
  fn the_askers_own_object_is_never_listed_in_another_hosts_round() {
    use slates_db::register::{
      Acceptor, Authority, HostEpoch, HostPrepare, HostPromise, ObjectId, Record,
    };
    let object = ObjectId([41; 16]);
    let (page, own) = audit_on_shard(move |state| {
      let local = state.fleet.host();
      let (departed, successor) = retire_the_owner_of_a_held_object(state, object);
      let regional = state.council.configuration().clone();
      let own = (0..=u8::MAX)
        .map(|seed| ObjectId([seed; 16]))
        .find(|other| *other != object && regional.successor(departed, *other) == Some(successor))
        .unwrap();
      let generation = state.fleet.configuration().version;
      let mut askers = Acceptor::new(
        local,
        Authority {
          generation,
          owner: successor,
        },
      );
      askers
        .accept(&Record {
          owner: successor,
          object: own,
          sequence: 0,
          epoch: HostEpoch(1),
          generation,
          value: b"live".to_vec(),
        })
        .unwrap();
      state.holder_records.insert(own, askers);
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let prepare = HostPrepare {
        departed,
        owner: successor,
        epoch: HostEpoch(2),
        generation,
        after: None,
        named: Vec::new(),
      };
      (
        crate::takeover::serve_host_prepare(state, local, successor, &prepare.encode()),
        own,
      )
    });
    let page = HostPromise::decode(&page).expect("the page answers");
    let listed: Vec<ObjectId> = page.entries.iter().map(|(listed, _)| *listed).collect();
    assert_eq!(listed, vec![object], "only the retired owner's copy");
    assert!(!listed.contains(&own));
  }

  /// A-251 (b), the successor's own empty promise: a successor that holds nothing of an object in whose cohort it is
  /// promises that by name to itself, as any member does, since its open gate alone is no promise. Do: at f = 1, this
  /// node the successor of an object it holds nothing of, and the third member's page listing it. Expect: the object is
  /// ready, its cohort's quorum of two made of the third member's report and this node's own named "nothing".
  #[test]
  fn a_successor_holding_nothing_of_an_object_promises_so_by_name() {
    use slates_db::register::{Accepted, HostEpoch, HostPromise, ObjectId};
    let (ready, object) = audit_on_shard(|state| {
      let local = state.fleet.host();
      // An object whose successor is this node (one of two survivors, by rendezvous), held here.
      let held = (0..=u8::MAX)
        .map(|seed| ObjectId([seed; 16]))
        .find(|object| {
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
          probe.successor(departed, *object) == Some(local)
        })
        .unwrap();
      let (departed, successor) = retire_the_owner_of_a_held_object(state, held);
      assert_eq!(successor, local);
      let third = slates_db::HostId(local.0 ^ 2);
      let regional = state.council.configuration().clone();
      let object = (0..=u8::MAX)
        .map(|seed| ObjectId([seed; 16]))
        .find(|other| *other != held && regional.successor(departed, *other) == Some(local))
        .unwrap();
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let requests = crate::takeover::begin_round(state, departed, local).unwrap();
      let (_, page_request) = requests
        .iter()
        .find(|(host, _)| *host == third)
        .unwrap()
        .clone();
      let listed = HostPromise {
        holder: third,
        departed,
        epoch: page_request.epoch,
        generation: page_request.generation,
        entries: vec![(
          object,
          Accepted {
            sequence: 0,
            epoch: HostEpoch(1),
            value: b"head".to_vec(),
          },
        )],
        next: None,
        fenced: None,
        unpromised: 0,
        nothing: Vec::new(),
      };
      crate::takeover::fold_answers(state, departed, vec![(third, listed, false)]);
      (
        crate::takeover::ready_objects(state, departed, local),
        object,
      )
    });
    assert!(
      ready.iter().any(|(ready, _)| *ready == object),
      "the third member's report and this node's own named nothing make the quorum: {ready:?}"
    );
  }

  /// A-251 (b), the successor's side: a complete page that did not list an object promises nothing of it. Do: at f = 1
  /// with this node the successor of an object it holds and a third member its cohort's other survivor, fold the third
  /// member's complete, empty page; then its answer by name. Expect: after the page the object is not ready and the next
  /// round asks the third member about it by name; after the answer (holding nothing of it) the object is ready.
  #[test]
  fn a_complete_page_promises_nothing_of_an_object_it_did_not_list() {
    use slates_db::register::{HostPromise, ObjectId};
    let (ready_after_page, asked, ready_after_answer) = audit_on_shard(|state| {
      let local = state.fleet.host();
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
      let third = slates_db::HostId(local.0 ^ 2);
      let (retired, successor) = retire_the_owner_of_a_held_object(state, object);
      assert_eq!((retired, successor), (departed, local));
      state.answers_given.alive_answers.insert(
        departed,
        slates_machine::clock::monotonic_ns() - crate::lease::horizon_ns() - 1,
      );
      let requests = crate::takeover::begin_round(state, departed, local).unwrap();
      let (_, page_request) = requests
        .iter()
        .find(|(host, _)| *host == third)
        .unwrap()
        .clone();
      let answer = HostPromise {
        holder: third,
        departed,
        epoch: page_request.epoch,
        generation: page_request.generation,
        entries: Vec::new(),
        next: None,
        fenced: None,
        unpromised: 0,
        nothing: Vec::new(),
      };
      crate::takeover::fold_answers(state, departed, vec![(third, answer.clone(), false)]);
      let ready_after_page = crate::takeover::ready_objects(state, departed, local).len();
      let requests = crate::takeover::begin_round(state, departed, local).unwrap();
      let asked = requests
        .iter()
        .find(|(host, _)| *host == third)
        .map(|(_, request)| request.named.clone())
        .unwrap_or_default();
      let named = HostPromise {
        nothing: vec![object],
        ..answer
      };
      crate::takeover::fold_answers(state, departed, vec![(third, named, true)]);
      let ready_after_answer = crate::takeover::ready_objects(state, departed, local);
      (ready_after_page, (asked, object), ready_after_answer)
    });
    assert_eq!(
      ready_after_page, 0,
      "a complete empty page promises nothing of the object"
    );
    assert_eq!(
      asked.0,
      vec![asked.1],
      "the next round asks about it by name"
    );
    assert_eq!(
      ready_after_answer
        .iter()
        .map(|(ready, _)| *ready)
        .collect::<Vec<_>>(),
      vec![asked.1],
      "held nothing of, by name, is a promise"
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

  /// §4.8 "Promotion and takeover", persistence before reply: do make the audit node a holder of a record the second
  /// member of an `f = 1` region wrote, restart it warm through its held image (routing is not persisted), and then
  /// retire that member and install the result. Expect the restored copy to move to its successor as a live holder's
  /// does, and a record the successor adopts it under at the new generation to be accepted. Before the fix the
  /// restored acceptor had no owner in routing: the install neither moved it nor raised its authority, and the
  /// successor's adoption was refused `ForeignGeneration` every period, so a member that died while a voter
  /// restarted was never served (`a_member_down_through_a_voters_restart_is_still_taken_over`, 1,056 refusals in a
  /// minute).
  #[test]
  fn a_restored_holders_copy_moves_to_its_successor_and_accepts_its_adoption() {
    let (moved, accepted) = audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{
        Acceptor, Ack, Authority, FIRST_EPOCH, HostId, ObjectId, Quorum, Record,
      };
      let local = state.fleet.host();
      let departed = HostId(local.0 ^ 1);
      let third = HostId(local.0 ^ 2);
      let object = ObjectId([31; 16]);
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

      // The warm restart: the held image is what survives; the acceptors and the routing do not.
      let image = crate::content_holder::held_image(state);
      state.holder_records.clear();
      state.fleet.forget_object(object);
      let held = crate::content_holder::split_held(&image).unwrap();
      assert_eq!(
        crate::content_holder::restore_held_registers(state, local, held),
        1
      );

      assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      crate::fleet::reconcile_held_authority(state);
      let regional = state.council.configuration().clone();
      let successor = regional.successor(departed, object).unwrap();
      let moved = state.fleet.object_owner(object) == Some(successor);
      let epoch = regional
        .epochs
        .get(&departed)
        .copied()
        .unwrap_or(FIRST_EPOCH);
      let adopted = Record {
        owner: successor,
        object,
        sequence: 0,
        epoch,
        generation: regional.version,
        value: b"head".to_vec(),
      };
      let reply = crate::fleet::accept_held_record(state, local, successor, &adopted);
      let accepted = Ack::decode(&reply).is_ok_and(|ack| ack.binds(&adopted));
      (moved, accepted)
    });
    assert!(moved, "the restored copy moves to its successor");
    assert!(
      accepted,
      "the restored holder accepts the successor's adoption at the new generation"
    );
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
    let (successor, ready, past) = audit_on_shard(|state| {
      let local = state.fleet.host();
      let object = ObjectId([29; 16]);
      let departed = retire_an_owner_settled_on_a_formation_neighbourhood(state, object);
      let successor = state.fleet.object_owner(object);
      assert!(crate::takeover::begin_round(state, departed, local).is_some());
      let ready = crate::takeover::ready_objects(state, departed, local);
      let past = state
        .refusals
        .get("fleet.takeover.recovered_past_tolerance")
        .copied()
        .unwrap_or(0);
      (successor == Some(local), ready, past)
    });
    assert!(successor, "the one survivor of the cohort is the successor");
    assert_eq!(
      past, 0,
      "a cohort its survivor holds the quorum of is within tolerance, not recovered past it"
    );
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

  /// A-309 (A-272 (ii)), red before it: a takeover past its tolerance is recovered by the members that exist now, the
  /// replacement holding the newest record among them, the store-destroyed takeover's shape. Do: in a three-member
  /// `f = 1` region of the audit node A, an owner O and a member B, every neighbourhood the whole region, take B over and
  /// admit a fresh member B', so O's neighbourhood becomes {O, A, B'} while its settled one stays {O, A, B} (O never
  /// reports the new one placed); give the audit node a record of O's object; take O over and run the audit node's round,
  /// then fold in B''s answer, a newer record. Expect O's retirement to keep {O, A, B'} as its current neighbourhood and A
  /// and B' as its survivors, the audit node the object's successor, the object waiting until B' answers and then ready
  /// past tolerance with B''s newer record, counted `fleet.takeover.recovered_past_tolerance`. Before, the settled cohort
  /// {O, A, B} with A alone left was short of its quorum of two, the object was counted lost and never adopted, and
  /// nobody served the volume (diag-live-durability-66's store-destroyed arm counted `fleet.takeover.lost` 6).
  #[test]
  fn a_takeover_past_its_tolerance_is_recovered_by_the_members_that_exist_now() {
    let (survivors, current, successor, waiting, ready, past) = audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{
        Accepted, Acceptor, Authority, FIRST_EPOCH, HostEpoch, HostId, HostPromise, ObjectId,
        Quorum, Record,
      };
      let local = state.fleet.host();
      let (owner, old_b, fresh_b) = (
        HostId(local.0 ^ 1),
        HostId(local.0 ^ 2),
        HostId(local.0 ^ 3),
      );
      // Shape: the candidate floor at `f = 1`, so each neighbourhood is the whole three-member region.
      let scatter = 3;
      state.council = RegionalCouncil::new(
        local,
        vec![local, owner, old_b],
        vec![local],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      install_council(state);
      assert!(state.council.propose(Reconfiguration::TakeOver(old_b)));
      assert!(state.council.propose(Reconfiguration::Admit {
        host: fresh_b,
        domain: None,
      }));
      install_council(state);
      let object = ObjectId::new(owner, 1);
      let generation = state.fleet.configuration().version;
      let mut acceptor = Acceptor::new(local, Authority { generation, owner });
      acceptor
        .accept(&Record {
          owner,
          object,
          sequence: 1,
          epoch: FIRST_EPOCH,
          generation,
          value: b"older".to_vec(),
        })
        .unwrap();
      state.holder_records.insert(object, acceptor);
      state.fleet.track_object_owner(object, owner);
      assert!(state.council.propose(Reconfiguration::TakeOver(owner)));
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let regional = state.council.configuration().clone();
      let survivors = regional.survivors_of(owner);
      let current = regional
        .retired
        .get(&owner)
        .and_then(|retirement| retirement.current.as_ref())
        .map(|current| {
          let mut hosts = current.hosts.clone();
          hosts.sort_unstable_by_key(|host| host.0);
          hosts
        });
      let successor = regional.successor(owner, object);
      let requests = crate::takeover::begin_round(state, owner, local).unwrap_or_default();
      let waiting = crate::takeover::ready_objects(state, owner, local).is_empty();
      let (epoch, round_generation) = requests
        .iter()
        .find(|(host, _)| *host == fresh_b)
        .map(|(_, prepare)| (prepare.epoch, prepare.generation))
        .expect("the round asks the fresh member");
      let promise = HostPromise {
        holder: fresh_b,
        departed: owner,
        epoch,
        generation: round_generation,
        entries: vec![(
          object,
          Accepted {
            sequence: 2,
            epoch: FIRST_EPOCH,
            value: b"newer".to_vec(),
          },
        )],
        next: None,
        fenced: None::<HostEpoch>,
        unpromised: 0,
        nothing: Vec::new(),
      };
      crate::takeover::fold_answers(state, owner, vec![(fresh_b, promise, false)]);
      let ready = crate::takeover::ready_objects(state, owner, local);
      let past = state
        .refusals
        .get("fleet.takeover.recovered_past_tolerance")
        .copied()
        .unwrap_or(0);
      let mut expected = vec![local, fresh_b];
      expected.sort_unstable_by_key(|host| host.0);
      let mut region = vec![owner, local, fresh_b];
      region.sort_unstable_by_key(|host| host.0);
      (
        survivors == expected,
        current == Some(region),
        successor == Some(local),
        waiting,
        ready,
        past,
      )
    });
    assert!(
      survivors,
      "the audit node and the fresh member survive the owner"
    );
    assert!(
      current,
      "the owner's current neighbourhood is kept: its joint writes reached the fresh member"
    );
    assert!(
      successor,
      "the audit node, the settled cohort's one survivor, is the successor"
    );
    assert!(
      waiting,
      "past tolerance the object waits for every surviving holder"
    );
    assert_eq!(ready.len(), 1, "ready once the fresh member answered");
    assert!(
      ready
        .iter()
        .all(|(_, adopted)| adopted.sequence == 2 && adopted.value == b"newer"),
      "the newest record a surviving holder has"
    );
    assert_eq!(past, 1, "counted as recovered past tolerance");
  }

  /// A-309's sibling, red before it: a holder outside the successor's placement lets its stale copy go when the
  /// takeover completes, though the last confirmation dropped the retirement in the version that recorded it. Do: make
  /// the audit node a holder, in a seven-member `f = 1` region whose every neighbourhood is the whole region, of a second
  /// member's object whose successor places it elsewhere; take that member over and install; then have every survivor
  /// confirm, and install again. Expect the copy moved to the successor at the first install and reclaimed at the
  /// second. Before, the holder judged staleness by the kept retirement alone, which the last confirmation had already
  /// dropped, so the copy, its content and its lease-gate record stayed for good.
  #[test]
  fn a_holder_outside_the_successors_placement_lets_its_copy_go_when_the_takeover_completes() {
    let (moved, reclaimed, held) = audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{
        Acceptor, Authority, FIRST_EPOCH, HostId, ObjectId, Quorum, Record,
      };
      let local = state.fleet.host();
      let departed = HostId(local.0 ^ 1);
      let others: Vec<HostId> = (2..7).map(|key| HostId(local.0 ^ key)).collect();
      // Shape: a neighbourhood of every member, so every member survives the departed one and is asked in its round,
      // and the successor's placement is a copyset of three among the six left.
      let scatter = 7;
      let mut members = vec![local, departed];
      members.extend(others);
      // The audit node the council's sole voter, so each proposal commits as it is made.
      state.council = RegionalCouncil::new(
        local,
        members,
        vec![local],
        Quorum { f: 1 },
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      install_council(state);
      let mut retired = state.council.configuration().clone();
      retired.take_over(departed, scatter).unwrap();
      // An object whose successor is another member that places it where the audit node is not (both of its cohorts
      // while its change of neighbourhood is in flight).
      let object = (0..1024u64)
        .map(|index| ObjectId::new(departed, index))
        .find(|object| {
          retired
            .successor(departed, *object)
            .is_some_and(|successor| {
              successor != local
                && retired
                  .configuration_for(successor)
                  .is_some_and(|placement| !placement.place(*object).candidates.contains(&local))
            })
        })
        .expect(
          "some object of the departed member is placed by its successor away from this holder",
        );
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
      let successor = state.council.configuration().successor(departed, object);
      let moved = successor.is_some() && state.fleet.object_owner(object) == successor;
      for survivor in state.council.configuration().survivors_of(departed) {
        state.council.propose(Reconfiguration::Confirm {
          departed,
          successor: survivor,
        });
      }
      assert!(
        state
          .council
          .configuration()
          .completed
          .contains_key(&departed),
        "the takeover completed"
      );
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let reclaimed = state
        .refusals
        .get("fleet.takeover.reclaimed")
        .copied()
        .unwrap_or(0);
      (moved, reclaimed, state.holder_records.contains_key(&object))
    });
    assert!(moved, "the copy moves to its successor at the takeover");
    assert_eq!(reclaimed, 1, "and is reclaimed once the takeover completes");
    assert!(!held, "the stale copy is gone");
  }

  /// The content hazard's fixture (A-335; docs/bugs/2026-11-18-a-stale-copy-reclaim-dropped-content-a-successor-still-
  /// fetched.md): in a seven-member `f = 1` region whose every neighbourhood is the whole region, the audit node the
  /// council's sole voter, the audit node holds a second member's volume head naming the member and itself as the
  /// content's holders (`f + 1` of them, as a seal places it), and that content whole; the volume's successor in the
  /// member's takeover is a third member that places it away from the audit node and holds none of the content. Returns
  /// the member, the volume and the content's manifest identity.
  fn hold_a_head_naming_this_holder(
    state: &mut crate::state::ShardState,
  ) -> (
    slates_db::register::HostId,
    slates_db::register::ObjectId,
    [u8; 32],
  ) {
    use slates_cluster::config_group::RegionalCouncil;
    use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, HostId, ObjectId, Quorum, Record};
    let local = state.fleet.host();
    let departed = HostId(local.0 ^ 1);
    let mut members = vec![local, departed];
    members.extend((2..7).map(|key| HostId(local.0 ^ key)));
    // Shape: a neighbourhood of every member, so the audit node survives the departed one and is asked in its round.
    let scatter = 7;
    state.council = RegionalCouncil::new(
      local,
      members,
      vec![local],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    install_council(state);
    let mut retired = state.council.configuration().clone();
    retired.take_over(departed, scatter).unwrap();
    let volume = (0..1024u64)
      .map(|index| ObjectId::new(departed, index))
      .find(|object| {
        retired
          .successor(departed, *object)
          .is_some_and(|successor| {
            successor != local
              && retired
                .configuration_for(successor)
                .is_some_and(|placement| !placement.place(*object).candidates.contains(&local))
          })
      })
      .expect(
        "some volume of the departed member is placed by its successor away from this holder",
      );
    let archive =
      crate::merge_service::MergeShardState::inputs_archive(b"the volume's sealed bytes", 0, 4096);
    let manifest = archive.manifest_identity();
    let head = crate::head::HeadValue {
      manifest: Some(manifest),
      content_holders: vec![departed.0, local.0],
      sealing: None,
    };
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
        object: volume,
        sequence: 0,
        epoch: FIRST_EPOCH,
        generation,
        value: head.to_record_bytes(),
      })
      .unwrap();
    state.holder_records.insert(volume, acceptor);
    state.fleet.track_object_owner(volume, departed);
    state
      .held_content
      .hold(
        &mut crate::content_holder::hold_space(&mut state.store),
        volume,
        slates_cluster::content::Placed { sequence: 0 },
        archive,
      )
      .expect("the audit shard holds the volume's content");
    (departed, volume, manifest)
  }

  /// A-335, red before it (the bug record named in [`hold_a_head_naming_this_holder`]), and A-538's uncertified case: a
  /// holder outside the successor's placement keeps the copy whose content its head names it for, when the successor
  /// confirms and when the takeover completes, where the takeover is uncertified (read from a configuration an older
  /// build wrote, layout 5, whose successors confirmed before placing the content again). Do: hold such a head and its
  /// content (the fixture); take the departed member over and install; have the volume's successor alone confirm its
  /// share, read the takeover as uncertified, and install; then every other survivor, so the takeover completes, read
  /// it as uncertified again, and install again. Expect the head and the content held after both installs, each counted
  /// `fleet.takeover.kept_holding_content`, and nothing reclaimed. Red with an uncertified takeover's content let go. A successor confirms once it adopted the head
  /// register, before its materialization has fetched the content, and it fetches from the head's holders alone
  /// (`fleet::fetch_into_hold`): before, this holder let the head and the content go at the confirmation, so with the
  /// dead owner the head's only other holder the successor could fetch from nobody, and one failure lost the volume.
  #[test]
  fn a_holder_outside_the_successors_placement_keeps_the_content_its_head_names() {
    let (after_one, after_all, kept, reclaimed) = audit_on_shard(|state| {
      use slates_cluster::config_group::Reconfiguration;
      let local = state.fleet.host();
      let (departed, volume, manifest) = hold_a_head_naming_this_holder(state);
      assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let successor = state
        .council
        .configuration()
        .successor(departed, volume)
        .expect("the volume ranks a survivor");
      assert_eq!(
        state.fleet.object_owner(volume),
        Some(successor),
        "the copy moves to its successor at the takeover"
      );
      let holds = |state: &crate::state::ShardState| {
        state.holder_records.contains_key(&volume)
          && state.held_content.holds_manifest(volume, &manifest)
      };
      assert!(state.council.propose(Reconfiguration::Confirm {
        departed,
        successor,
      }));
      adopt_uncertified(state, departed);
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let after_one = holds(state);
      for survivor in state.council.configuration().survivors_of(departed) {
        state.council.propose(Reconfiguration::Confirm {
          departed,
          successor: survivor,
        });
      }
      assert!(
        state
          .council
          .configuration()
          .completed
          .contains_key(&departed),
        "the takeover completed"
      );
      adopt_uncertified(state, departed);
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        after_one,
        holds(state),
        count("fleet.takeover.kept_holding_content"),
        count("fleet.takeover.reclaimed"),
      )
    });
    assert!(
      after_one,
      "an uncertified confirmation leaves the content its head names, which the successor may still fetch"
    );
    assert!(after_all, "and so does the completed takeover");
    assert_eq!(kept, 2, "each install counts the copy kept for its content");
    assert_eq!(reclaimed, 0, "nothing is reclaimed");
  }

  /// Gives the audit shard's council a learned view of its configuration in which `departed`'s takeover is
  /// uncertified, as a configuration an older build wrote (regional layout 5) is read (A-538). The next proposal's fold
  /// discards it, so a test adopts it again after each.
  fn adopt_uncertified(
    state: &mut crate::state::ShardState,
    departed: slates_db::register::HostId,
  ) {
    let mut view = state.council.configuration().clone();
    if let Some(retirement) = view.retired.get_mut(&departed) {
      retirement.certified = false;
    }
    if let Some(record) = view.completed.get_mut(&departed) {
      record.certified = false;
    }
    view.version = view.version.checked_add(1).unwrap();
    assert!(state.council.adopt(view), "the uncertified view is newer");
  }

  /// A-538, red before it: a holder outside the successor's placement lets a stale copy's content go once the
  /// successor's confirmation certifies its content (it confirms only once it serves the volume from its own placement,
  /// A-529). Do: hold a head naming this holder and its content (the fixture of
  /// [`a_holder_outside_the_successors_placement_keeps_the_content_its_head_names`]); take the departed member over and
  /// install; have the volume's successor confirm its share and install. Expect the head and the content gone at that
  /// install, counted `fleet.takeover.released_certified` once and `fleet.takeover.reclaimed` once, and nothing kept
  /// for its content. Before, the holder kept both for good (A-335), since a confirmation said nothing of content.
  #[test]
  fn a_holder_outside_the_successors_placement_lets_the_content_go_on_a_certified_confirmation() {
    let (held, counts) = audit_on_shard(|state| {
      use slates_cluster::config_group::Reconfiguration;
      let local = state.fleet.host();
      let (departed, volume, manifest) = hold_a_head_naming_this_holder(state);
      assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let successor = state
        .council
        .configuration()
        .successor(departed, volume)
        .expect("the volume ranks a survivor");
      assert!(state.council.propose(Reconfiguration::Confirm {
        departed,
        successor,
      }));
      assert!(
        state.council.configuration().certifies_content(departed),
        "this build's takeover certifies its content"
      );
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        state.holder_records.contains_key(&volume)
          || state.held_content.holds_manifest(volume, &manifest),
        [
          count("fleet.takeover.released_certified"),
          count("fleet.takeover.reclaimed"),
          count("fleet.takeover.kept_holding_content"),
        ],
      )
    });
    assert!(!held, "the stale copy and its content are gone");
    assert_eq!(
      counts,
      [1, 1, 0],
      "released once, reclaimed once, never kept for its content"
    );
  }

  /// A-335, red before it: a holder that installs a takeover only once it completed keeps the copy whose content its head
  /// names it for. Do: hold such a head and its content (the fixture of
  /// [`a_holder_outside_the_successors_placement_keeps_the_content_its_head_names`]); take the departed member over and
  /// have every survivor confirm before the audit node installs anything, then install. Expect the head and the content
  /// held, counted `fleet.takeover.kept_holding_content`. The copy's line ends at the takeover's completion record, which
  /// names the audit node among the survivors that confirmed; before, that let the copy go whole, the content the
  /// adopted head names with it.
  #[test]
  fn a_lagging_holder_keeps_the_content_its_head_names_when_the_takeover_completes() {
    let (held, kept, reclaimed) = audit_on_shard(|state| {
      use slates_cluster::config_group::Reconfiguration;
      let local = state.fleet.host();
      let (departed, volume, manifest) = hold_a_head_naming_this_holder(state);
      assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
      for survivor in state.council.configuration().survivors_of(departed) {
        state.council.propose(Reconfiguration::Confirm {
          departed,
          successor: survivor,
        });
      }
      assert!(
        state.council.configuration().settled_for(departed, local),
        "the takeover completed with the audit node among those that confirmed"
      );
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        state.holder_records.contains_key(&volume)
          && state.held_content.holds_manifest(volume, &manifest),
        count("fleet.takeover.kept_holding_content"),
        count("fleet.takeover.reclaimed"),
      )
    });
    assert!(held, "the copy and the content its head names stay");
    assert_eq!(kept, 1, "counted kept for its content");
    assert_eq!(reclaimed, 0, "nothing is reclaimed");
  }

  /// A-341 (A-309's owed rule, its safe part) and A-484 (its owed piece, the completion record): a holder the takeover
  /// never asked lets its copy go as a stale one goes when the copy's writer's takeover shows the object recovered within
  /// tolerance, judged on its kept retirement or its completion record. Do: make the audit node hold copies of three
  /// objects of a member although it is in none of that member's neighbourhoods (copies an earlier neighbourhood of the
  /// member left there), in a six-member `f = 1` region whose neighbourhoods are three: a plain register and a head
  /// naming this holder for content it holds, both falling to one successor, and a plain register falling to another,
  /// each successor placing them away from the audit node. Take the member over and install; have the first successor
  /// alone confirm and install; then every other survivor, so the takeover completes, give the audit node a fourth copy
  /// of the member's (a lagging holder's, still routed to the member) and install again. Expect all three moved to their
  /// successors at the first install; at the first confirmation the first register let go, and the head too, its content
  /// released since this build's confirmation certifies the successor's content (A-538); on the completed takeover's
  /// record the second register and the fourth copy let go. Counted `fleet.takeover.unasked_covered` four times,
  /// `fleet.takeover.reclaimed` four times, `fleet.takeover.kept_holding_content` never and `fleet.takeover.kept_unasked`
  /// never. Before A-341 every such copy
  /// was kept for good (A-309), and before A-484 every copy first judged on the record, which kept no neighbourhood to
  /// judge by; the lineage model (`crates/db/tests/lineage_model.rs`) found the history where one let go past tolerance
  /// was the last copy of a record placed at `f + 1`.
  #[test]
  fn a_holder_the_takeover_never_asked_lets_go_what_was_recovered_within_tolerance() {
    let (moved, after_one, after_all, counts) = audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{HostId, ObjectId, Quorum, RegionalConfiguration};
      let local = state.fleet.host();
      // Shape: the candidate floor at `f = 1`, so a neighbourhood is three of the six members.
      let scatter = 3;
      let quorum = Quorum { f: 1 };
      // Shape: each candidate region's ids, the departed member's and its four others', apart from the next one's.
      let stride = 8;
      let members_with = |departed: HostId, others: &[HostId]| {
        let mut members = vec![local, departed];
        members.extend(others.iter().copied());
        members
      };
      // A member whose neighbourhood leaves the audit node out, two objects of it whose one successor places them
      // elsewhere too, and a third whose other successor does: rendezvous decides all of them, so search a few regions,
      // each with members of its own (with the other members fixed, whether a successor's neighbourhood holds the audit
      // node is fixed by its random id alone).
      let (departed, others, plain, named, other) = (0..64u64)
        .find_map(|region| {
          let base = region * stride;
          let departed = HostId(local.0 ^ (base + 1));
          let others: Vec<HostId> = (2..6).map(|key| HostId(local.0 ^ (base + key))).collect();
          let formed = RegionalConfiguration::formed(
            members_with(departed, &others),
            quorum,
            std::collections::BTreeMap::new(),
            scatter,
            false,
          );
          if formed.neighbourhoods[&departed].hosts.contains(&local) {
            return None;
          }
          let mut retired = formed.clone();
          retired.take_over(departed, scatter)?;
          let away: Vec<(ObjectId, HostId)> = (0..256u64)
            .map(|index| ObjectId::new(departed, index))
            .filter_map(|object| {
              retired
                .successor(departed, object)
                .filter(|successor| {
                  retired
                    .configuration_for(*successor)
                    .is_some_and(|placement| !placement.place(object).candidates.contains(&local))
                })
                .map(|successor| (object, successor))
            })
            .collect();
          let (plain, first) = *away.first()?;
          let (named, _) = *away
            .iter()
            .skip(1)
            .find(|(_, successor)| *successor == first)?;
          let (other, _) = *away.iter().find(|(_, successor)| *successor != first)?;
          Some((departed, others.clone(), plain, named, other))
        })
        .expect(
          "a member whose neighbourhood and two successors of its objects leave the audit node out",
        );
      state.council = RegionalCouncil::new(
        local,
        members_with(departed, &others),
        vec![local],
        quorum,
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      install_council(state);
      let generation = state.fleet.configuration().version;
      let archive = crate::merge_service::MergeShardState::inputs_archive(
        b"the volume's sealed bytes",
        0,
        4096,
      );
      let manifest = archive.manifest_identity();
      let head = crate::head::HeadValue {
        manifest: Some(manifest),
        content_holders: vec![departed.0, local.0],
        sealing: None,
      };
      for (object, value) in [
        (plain, b"head".to_vec()),
        (named, head.to_record_bytes()),
        (other, b"head".to_vec()),
      ] {
        hold_written_copy(state, (departed, generation), object, value);
      }
      state
        .held_content
        .hold(
          &mut crate::content_holder::hold_space(&mut state.store),
          named,
          slates_cluster::content::Placed { sequence: 0 },
          archive,
        )
        .expect("the audit shard holds the named head's content");
      assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let configuration = state.council.configuration().clone();
      assert!(
        !configuration.survivors_of(departed).contains(&local),
        "the audit node is no survivor: the takeover's rounds never ask it"
      );
      assert!(
        configuration.within_tolerance(departed, plain),
        "one failure at f = 1: the objects' settled cohorts keep their recovery quorums"
      );
      let first = configuration
        .successor(departed, plain)
        .expect("the objects rank a survivor");
      let moved = [plain, named, other].iter().all(|object| {
        state.fleet.object_owner(*object) == configuration.successor(departed, *object)
      });
      assert!(state.council.propose(Reconfiguration::Confirm {
        departed,
        successor: first,
      }));
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let held = |state: &crate::state::ShardState| {
        (
          state.holder_records.contains_key(&plain),
          state.holder_records.contains_key(&named)
            && state.held_content.holds_manifest(named, &manifest),
          state.holder_records.contains_key(&other),
        )
      };
      let after_one = held(state);
      for survivor in state.council.configuration().survivors_of(departed) {
        state.council.propose(Reconfiguration::Confirm {
          departed,
          successor: survivor,
        });
      }
      assert!(
        state
          .council
          .configuration()
          .completed
          .contains_key(&departed),
        "the takeover completed"
      );
      // A copy this holder first judges on the record: a lagging holder's, still routed to the member that wrote it.
      let late = ObjectId::new(departed, 256);
      hold_written_copy(state, (departed, generation), late, b"head".to_vec());
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let after_all = (held(state), state.holder_records.contains_key(&late));
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      let counts = [
        count("fleet.takeover.unasked_covered"),
        count("fleet.takeover.reclaimed"),
        count("fleet.takeover.kept_holding_content"),
        count("fleet.takeover.kept_unasked"),
      ];
      (moved, after_one, after_all, counts)
    });
    assert!(moved, "every copy moves to its successor at the takeover");
    assert_eq!(
      after_one,
      (false, false, true),
      "at the first confirmation the plain register and the named head go, its content released (A-538); the other waits"
    );
    assert_eq!(
      after_all,
      ((false, false, false), false),
      "on the completed takeover's record the other register and the late copy go"
    );
    assert_eq!(
      counts,
      [4, 4, 0, 0],
      "covered four times, reclaimed four times, never kept for content, never kept unasked"
    );
  }

  /// Gives the audit shard a copy of `object` that `writer` wrote under configuration `generation`, routed to `writer`:
  /// its acceptor serves `writer` and accepted the record `value` at sequence 0 (A-341, A-484).
  fn hold_written_copy(
    state: &mut crate::state::ShardState,
    (writer, generation): (slates_db::register::HostId, u64),
    object: slates_db::register::ObjectId,
    value: Vec<u8>,
  ) {
    use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, Record};
    let local = state.fleet.host();
    let mut acceptor = Acceptor::new(
      local,
      Authority {
        generation,
        owner: writer,
      },
    );
    acceptor
      .accept(&Record {
        owner: writer,
        object,
        sequence: 0,
        epoch: FIRST_EPOCH,
        generation,
        value,
      })
      .unwrap();
    state.holder_records.insert(object, acceptor);
    state.fleet.track_object_owner(object, writer);
  }

  /// A-484, red before it: a holder no round asked judges its copy by the first takeover since the copy was written, its
  /// writer's, never by a later one in the object's line. Do: in an eight-member `f = 1` region whose neighbourhoods are
  /// three, make the audit node hold a copy of an object a member wrote, though it is in none of the member's
  /// neighbourhoods; take over another host of the object's cohort (its takeover completes) and then the member, so the
  /// member's takeover is past tolerance for the object; have the object's successor, the cohort's one survivor, confirm
  /// without it (nobody its round asked held it) and report its neighbourhood placed, and install; then take that
  /// successor over, have its own successor confirm, and install. Expect the copy kept both times, counted
  /// `fleet.takeover.kept_unasked` twice and `fleet.takeover.unasked_covered` never: no round learned the object, and this
  /// copy may be its last. Before, the second install judged the copy by the successor's takeover, within tolerance for
  /// the object, and let it go.
  #[test]
  fn a_copy_whose_writers_takeover_was_past_tolerance_is_kept_through_a_later_takeover() {
    let (kept, routed_to_later, counts) = audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{HostId, ObjectId, Quorum, RegionalConfiguration};
      let local = state.fleet.host();
      // Shape: the candidate floor at `f = 1`, so a neighbourhood is three of the eight members.
      let scatter = 3;
      let quorum = Quorum { f: 1 };
      // Shape: each candidate region's ids, the writer's and its six others', apart from the next one's.
      let stride = 8;
      // The history, dry-run on a configuration of each candidate region until one leaves the audit node out of every
      // neighbourhood and placement that would ask it or reach it: rendezvous decides them from the random ids alone.
      let (members, writer, first_gone, successor, later) = (0..256u64)
        .find_map(|region| {
          let base = region * stride;
          let members: Vec<HostId> = std::iter::once(local)
            .chain((1..8).map(|key| HostId(local.0 ^ (base + key))))
            .collect();
          let writer = *members.get(1)?;
          let object = ObjectId::new(writer, 0);
          let mut dry = RegionalConfiguration::formed(
            members.clone(),
            quorum,
            std::collections::BTreeMap::new(),
            scatter,
            false,
          );
          let cohort = dry.configuration_for(writer)?.place(object).candidates;
          let others: Vec<HostId> = cohort
            .iter()
            .copied()
            .filter(|host| *host != writer)
            .collect();
          let [first_gone, successor] = <[HostId; 2]>::try_from(others).ok()?;
          let leaves_out = |dry: &RegionalConfiguration, owner: HostId| {
            dry
              .neighbourhoods
              .get(&owner)
              .is_some_and(|neighbourhood| !neighbourhood.hosts.contains(&local))
              && dry
                .configuration_for(owner)
                .is_some_and(|placement| !placement.place(object).candidates.contains(&local))
          };
          if !leaves_out(&dry, writer) {
            return None;
          }
          dry.take_over(first_gone, scatter)?;
          for survivor in dry.survivors_of(first_gone) {
            dry.confirm(first_gone, survivor);
          }
          if !leaves_out(&dry, writer) {
            return None;
          }
          dry.take_over(writer, scatter)?;
          if dry.successor(writer, object) != Some(successor)
            || dry.survivors_of(writer).contains(&local)
            || dry.within_tolerance(writer, object)
          {
            return None;
          }
          dry.confirm(writer, successor);
          let generation = dry.neighbourhoods.get(&successor)?.generation;
          dry.settle(successor, generation);
          if !leaves_out(&dry, successor) {
            return None;
          }
          dry.take_over(successor, scatter)?;
          let later = dry.successor(successor, object)?;
          (leaves_out(&dry, later)
            && !dry.survivors_of(successor).contains(&local)
            && dry.within_tolerance(successor, object))
          .then_some((members, writer, first_gone, successor, later))
        })
        .expect(
          "a region whose neighbourhoods and placements of the object leave the audit node out",
        );
      let object = ObjectId::new(writer, 0);
      state.council = RegionalCouncil::new(
        local,
        members,
        vec![local],
        quorum,
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      install_council(state);
      let generation = state.fleet.configuration().version;
      hold_written_copy(state, (writer, generation), object, b"head".to_vec());
      let step = |state: &mut crate::state::ShardState, changes: Vec<Reconfiguration>| {
        for change in changes {
          state.council.propose(change);
        }
        install_council(state);
        crate::takeover::resolve_held_objects(state, local);
      };
      step(state, vec![Reconfiguration::TakeOver(first_gone)]);
      let confirmations: Vec<Reconfiguration> = state
        .council
        .configuration()
        .survivors_of(first_gone)
        .into_iter()
        .map(|survivor| Reconfiguration::Confirm {
          departed: first_gone,
          successor: survivor,
        })
        .collect();
      step(state, confirmations);
      step(state, vec![Reconfiguration::TakeOver(writer)]);
      let generation = state
        .council
        .configuration()
        .neighbourhoods
        .get(&successor)
        .map(|neighbourhood| neighbourhood.generation)
        .expect("the successor is a member");
      step(
        state,
        vec![
          Reconfiguration::Confirm {
            departed: writer,
            successor,
          },
          Reconfiguration::Settle {
            owner: successor,
            generation,
          },
        ],
      );
      let kept_first = state.holder_records.contains_key(&object);
      step(state, vec![Reconfiguration::TakeOver(successor)]);
      step(
        state,
        vec![Reconfiguration::Confirm {
          departed: successor,
          successor: later,
        }],
      );
      assert!(
        state
          .council
          .configuration()
          .within_tolerance(successor, object),
        "the later takeover is within tolerance for the object"
      );
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        (kept_first, state.holder_records.contains_key(&object)),
        state.fleet.object_owner(object) == Some(later),
        [
          count("fleet.takeover.kept_unasked"),
          count("fleet.takeover.unasked_covered"),
        ],
      )
    });
    assert_eq!(
      kept,
      (true, true),
      "the copy is kept through both takeovers"
    );
    assert!(
      routed_to_later,
      "the copy follows its line to the later successor"
    );
    assert_eq!(counts, [2, 0], "kept unasked twice, never covered");
  }

  /// A-508, red before it, and A-546's uncertified case: a holder a live owner's neighbourhood settled away from lets the
  /// owner's copies go once the owner's settled cohort for each is whole and at least `f + 1` wide, and keeps whatever
  /// content a head still names it for where the owner's settle is uncertified (read from a configuration an older build
  /// wrote, layout 6 or before, whose owners settled before their content was placed within the neighbourhood). Each
  /// install below reads the owner's settle as uncertified. Red with an uncertified settle's content let go. Do: in a six-member `f = 1` region whose neighbourhoods are three, make the audit node hold a plain register
  /// and a head naming this holder for content it holds, both of a member's own writing, though it is in none of that
  /// member's neighbourhoods, and install; then take over one of the member's neighbours, give the audit node a third
  /// copy of the member's and install; then have the member settle its new neighbourhood and install. Expect the plain
  /// register let go at the first install and the head kept for its content; the third copy kept while the member's
  /// settled cohort lacks a member, and let go once the member settled; counted `fleet.settle.released` twice,
  /// `fleet.settle.kept_outside` twice (the head and the third copy at the second install) and
  /// `fleet.takeover.kept_holding_content` twice. Before, every such copy was kept for good.
  #[test]
  fn a_holder_a_live_owners_neighbourhood_settled_away_from_lets_its_copies_go() {
    let (first, second, third, counts) = audit_on_shard(|state| {
      use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
      use slates_db::register::{HostId, ObjectId, Quorum, RegionalConfiguration};
      let local = state.fleet.host();
      // Shape: the candidate floor at `f = 1`, so a neighbourhood is three of the six members.
      let scatter = 3;
      let quorum = Quorum { f: 1 };
      // Shape: each candidate region's ids, the owner's and its four others', apart from the next one's.
      let stride = 8;
      // A member whose neighbourhoods leave the audit node out, before and after one of its neighbours is taken over:
      // rendezvous decides them from the random ids alone, so search a few regions.
      let (members, owner, neighbour) = (0..256u64)
        .find_map(|region| {
          let base = region * stride;
          let members: Vec<HostId> = std::iter::once(local)
            .chain((1..6).map(|key| HostId(local.0 ^ (base + key))))
            .collect();
          let owner = *members.get(1)?;
          let mut dry = RegionalConfiguration::formed(
            members.clone(),
            quorum,
            std::collections::BTreeMap::new(),
            scatter,
            false,
          );
          if dry.holds_for(owner, local) {
            return None;
          }
          let neighbour = dry
            .neighbourhoods
            .get(&owner)?
            .hosts
            .iter()
            .copied()
            .find(|host| *host != owner)?;
          dry.take_over(neighbour, scatter)?;
          (!dry.holds_for(owner, local)).then_some((members, owner, neighbour))
        })
        .expect("a member whose neighbourhoods leave the audit node out");
      let (plain, named, third) = (
        ObjectId::new(owner, 0),
        ObjectId::new(owner, 1),
        ObjectId::new(owner, 2),
      );
      state.council = RegionalCouncil::new(
        local,
        members,
        vec![local],
        quorum,
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      install_council(state);
      let generation = state.fleet.configuration().version;
      let archive = crate::merge_service::MergeShardState::inputs_archive(
        b"the volume's sealed bytes",
        0,
        4096,
      );
      let manifest = archive.manifest_identity();
      let head = crate::head::HeadValue {
        manifest: Some(manifest),
        content_holders: vec![owner.0, local.0],
        sealing: None,
      };
      hold_written_copy(state, (owner, generation), plain, b"head".to_vec());
      hold_written_copy(state, (owner, generation), named, head.to_record_bytes());
      state
        .held_content
        .hold(
          &mut crate::content_holder::hold_space(&mut state.store),
          named,
          slates_cluster::content::Placed { sequence: 0 },
          archive,
        )
        .expect("the audit shard holds the named head's content");
      adopt_settle_uncertified(state, owner);
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let held = |state: &crate::state::ShardState, object: ObjectId| {
        state.holder_records.contains_key(&object)
      };
      let first = (held(state, plain), held(state, named));
      assert!(state.council.propose(Reconfiguration::TakeOver(neighbour)));
      let generation = state.fleet.configuration().version;
      hold_written_copy(state, (owner, generation), third, b"head".to_vec());
      adopt_settle_uncertified(state, owner);
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let second = held(state, third);
      let settled_generation = state
        .council
        .configuration()
        .neighbourhoods
        .get(&owner)
        .map(|neighbourhood| neighbourhood.generation)
        .expect("the owner is a member");
      assert!(state.council.propose(Reconfiguration::Settle {
        owner,
        generation: settled_generation,
      }));
      adopt_settle_uncertified(state, owner);
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let third_held = (
        held(state, named) && state.held_content.holds_manifest(named, &manifest),
        held(state, third),
      );
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        first,
        second,
        third_held,
        [
          count("fleet.settle.released"),
          count("fleet.settle.kept_outside"),
          count("fleet.takeover.kept_holding_content"),
        ],
      )
    });
    assert_eq!(
      first,
      (false, true),
      "the plain register goes at once, the head stays for its content"
    );
    assert!(
      second,
      "the third copy stays while the owner's settled cohort lacks a member"
    );
    assert_eq!(
      third,
      (true, false),
      "once the owner settled, the head still stays for its content and the third copy goes"
    );
    assert_eq!(
      counts,
      [2, 2, 2],
      "released twice, kept outside twice, kept for content twice"
    );
  }

  /// Gives the audit shard's council a learned view of its configuration in which `owner`'s settle is uncertified, as a
  /// configuration an older build wrote (regional layout 6 or before) is read (A-546). The next proposal's fold discards
  /// it, so a test adopts it again after each.
  fn adopt_settle_uncertified(
    state: &mut crate::state::ShardState,
    owner: slates_db::register::HostId,
  ) {
    let mut view = state.council.configuration().clone();
    view.settle_certified.remove(&owner);
    view.version = view.version.checked_add(1).unwrap();
    assert!(state.council.adopt(view), "the uncertified view is newer");
  }

  /// A-546, red before it: a holder a live owner's neighbourhood settled away from lets go the content a head of the
  /// owner's named it for, once the owner's settle certifies its content (an owner reports its neighbourhood placed only
  /// once every volume it owns is served from within it). Do: in a six-member `f = 1` region make the audit node hold,
  /// though it is in none of a member's neighbourhoods, a head of that member's naming this holder for content it holds,
  /// and install. Expect the head and its content gone at that install, counted `fleet.settle.released_content` once and
  /// `fleet.settle.released` once, and nothing kept for its content. Before, the holder kept both for good (A-508).
  #[test]
  fn a_holder_a_live_owners_certified_settle_left_out_lets_the_content_go() {
    let (held, counts) = audit_on_shard(|state| {
      use slates_cluster::config_group::RegionalCouncil;
      use slates_db::register::{HostId, ObjectId, Quorum, RegionalConfiguration};
      let local = state.fleet.host();
      // Shape: the candidate floor at `f = 1`, so a neighbourhood is three of the six members.
      let scatter = 3;
      let quorum = Quorum { f: 1 };
      // Shape: each candidate region's ids, the owner's and its four others', apart from the next one's.
      let stride = 8;
      let (members, owner) = (0..256u64)
        .find_map(|region| {
          let base = region * stride;
          let members: Vec<HostId> = std::iter::once(local)
            .chain((1..6).map(|key| HostId(local.0 ^ (base + key))))
            .collect();
          let owner = *members.get(1)?;
          let formed = RegionalConfiguration::formed(
            members.clone(),
            quorum,
            std::collections::BTreeMap::new(),
            scatter,
            false,
          );
          (!formed.holds_for(owner, local)).then_some((members, owner))
        })
        .expect("a member whose neighbourhoods leave the audit node out");
      let named = ObjectId::new(owner, 1);
      state.council = RegionalCouncil::new(
        local,
        members,
        vec![local],
        quorum,
        std::collections::BTreeMap::new(),
        scatter,
        false,
      );
      install_council(state);
      assert!(
        state
          .council
          .configuration()
          .settle_certifies_content(owner),
        "this build's settle certifies the owner's content"
      );
      let generation = state.fleet.configuration().version;
      let archive = crate::merge_service::MergeShardState::inputs_archive(
        b"the volume's sealed bytes",
        0,
        4096,
      );
      let manifest = archive.manifest_identity();
      let head = crate::head::HeadValue {
        manifest: Some(manifest),
        content_holders: vec![owner.0, local.0],
        sealing: None,
      };
      hold_written_copy(state, (owner, generation), named, head.to_record_bytes());
      state
        .held_content
        .hold(
          &mut crate::content_holder::hold_space(&mut state.store),
          named,
          slates_cluster::content::Placed { sequence: 0 },
          archive,
        )
        .expect("the audit shard holds the head's content");
      install_council(state);
      crate::takeover::resolve_held_objects(state, local);
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        state.holder_records.contains_key(&named)
          || state.held_content.holds_manifest(named, &manifest),
        [
          count("fleet.settle.released_content"),
          count("fleet.settle.released"),
          count("fleet.takeover.kept_holding_content"),
        ],
      )
    });
    assert!(!held, "the head and its content are gone");
    assert_eq!(
      counts,
      [1, 1, 0],
      "its content released once, the copy released once, never kept for its content"
    );
  }

  /// Measurement (not a gate): the condemnation cost — slates' own takeover work per retired-host object —
  /// for the detection-budget F decision (`docs/wip/fleet-under-load.md` Part 6). The takeover is one batched,
  /// paged phase-one round per retired host (§4.8 "Promotion and takeover", `crate::takeover`), so its work is
  /// O(the retired host's objects), not O(fleet). This drives the sync phase-one locally — a host settled
  /// beside this one on a formation cohort (one promise recovers it), holding `n` objects, is taken over and
  /// this node promises and readies its own share — and reports the per-object cost, which is load-independent
  /// (allocation- and CPU-bound, no wall-clock path). Run:
  /// `cargo test --release -p slates-server condemnation_cost_per_object -- --ignored --nocapture`.
  #[test]
  #[ignore = "measurement, not a gate; prints the per-object takeover cost"]
  fn condemnation_cost_per_object_is_flat_and_sub_microsecond() {
    use slates_cluster::config_group::{Reconfiguration, RegionalCouncil};
    use slates_db::register::{Acceptor, Authority, FIRST_EPOCH, HostId, ObjectId, Quorum, Record};
    for objects in [256usize, 4_096, 16_384] {
      let (readied, resolve_ns, round_ns) = audit_on_shard(move |state| {
        let local = state.fleet.host();
        let departed = HostId(local.0 ^ 1);
        let third = HostId(local.0 ^ 2);
        // A sole-voter council that admits the departed host and a third beside it: the departed is settled on
        // the two-host formation cohort {local, third}, which one promise recovers (Flexible Paxos; the
        // sibling test above), so this node adopts its share with no dispatch — a purely local measurement.
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
        install_council(state);
        let generation = state.fleet.configuration().version;
        for index in 0..objects {
          let object = ObjectId::new(departed, u64::try_from(index).unwrap_or(u64::MAX));
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
        }
        assert!(state.council.propose(Reconfiguration::TakeOver(departed)));
        install_council(state);
        // Phase one, timed: resolve the owners of every held object, then run the successor's round — promise
        // this node's own copies and ready the objects whose cohort is met.
        let t0 = slates_machine::clock::monotonic_ns();
        crate::takeover::resolve_held_objects(state, local);
        let t1 = slates_machine::clock::monotonic_ns();
        let requests = crate::takeover::begin_round(state, departed, local);
        let ready = crate::takeover::ready_objects(state, departed, local);
        let t2 = slates_machine::clock::monotonic_ns();
        assert!(
          requests.is_none_or(|r| r.is_empty()),
          "a formation cohort recovers with no dispatch, so the round stays local"
        );
        (ready.len(), t1.saturating_sub(t0), t2.saturating_sub(t1))
      });
      assert!(readied > 0, "this node adopts the objects that rank to it");
      let resolve_per = resolve_ns as f64 / objects as f64;
      let round_per = round_ns as f64 / readied as f64;
      eprintln!(
        "condemnation cost: {objects} objects held, {readied} ranked here | \
         resolve {resolve_ns} ns ({resolve_per:.1} ns/object) | \
         round {round_ns} ns ({round_per:.1} ns/readied object)"
      );
    }
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
        owner: slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        completion: None,
        signature: None,
        folded_below: 0,
        checkpoint_floor: 0,
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
