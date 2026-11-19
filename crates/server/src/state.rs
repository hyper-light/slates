//! The shard's state: its volumes, its partition of the database, its clients, and the
//! reserve; one cell per shard thread, borrowed briefly by the shard's tasks (§4.3: one
//! owning shard per volume; §4.8: one writer per partition).

use std::cell::RefCell;
use std::collections::BTreeMap;

use slates_anchor::AnchorSegment;
use slates_base::OsHost;
use slates_db::Db;
use slates_db::catalog::{Principal, VolumeId};
use slates_db::register::{Acceptor, ObjectId};
use slates_ipc::DaemonEnd;
use slates_ipc::protocol::ReplyBody;
use slates_mem::Slab;
use slates_mem::SparseObject;
use slates_merge::engine::Green;
use slates_merge::increment::VolumeOp;
use slates_vfs::volume::{Store, Volume};

use crate::config::DaemonConfig;

/// One client the shard serves.
pub struct ClientSlot {
  /// One ephemeral, metadata-charged daemon status capture on this channel (§4.14).
  pub(crate) status_pages: crate::status_pages::StatusPages,
  /// The ring ends.
  pub end: DaemonEnd,
  /// The principal established at rendezvous.
  pub principal: Principal,
  /// The client id.
  pub client_id: u32,
  /// The client's process id (the hang-up watch's input where no control socket closes: macOS, Windows; A-475).
  pub pid: u32,
  /// Dead ring admission is closed while its attachments are retired on all owner shards (§4.7).
  pub(crate) retiring: bool,
  /// The watch awaiting this client's hang-up (A-475, `crate::reap::watch`): set at admission, cleared at its report,
  /// and cancelled by a seat that ends first (`crate::reap::end_unproven`).
  pub(crate) watch: Option<slates_rt::TaskId>,
  /// Set once the client has proven its id with its session key (`crate::session_key`), checked before its first
  /// request is served. A slot never proven is ended without retiring its id's attachments (`reap::end_unproven`):
  /// they belong to the id's owner, whom a stranger naming the id is not.
  pub(crate) proven: bool,
  /// The control channel, where the platform has one (Linux: the socket whose close is how
  /// the daemon learns of a dead client, and whose peer end closing tells the client the
  /// daemon died; held for the client's life).
  pub control: Option<slates_ipc::rendezvous::platform::Control>,
  /// Set when the consumer this channel attested as has since been revoked by a human (§4.13): every
  /// later verb on the channel refuses `ConsumerRevoked` before any effect. A local flag — the
  /// revocation is fanned to every shard's slots when it commits — so the per-verb gate is one read,
  /// never a cross-shard call on a write path (banned item 10).
  pub revoked: bool,
  /// The write policy of the consumer this channel attested as (A-285): read with the consumer's record at the
  /// attestation, and replaced on every shard's bound slots when its launcher sets it, as a revocation is marked. The
  /// channel's merges and landings are held to it before they are routed (`verbs::consumer_gate`). `None` for an
  /// account's channel, and for a consumer with no policy set.
  pub consumer_policy: Option<slates_db::catalog::WritePolicy>,
  /// The last successful remote route, released with this client. One slot, no global catalog.
  pub(crate) owner_route: Option<crate::owner_location::CachedRoute>,
  /// The read-ahead window of this client's latest forwarded read (`crate::verbs::ReadAhead`): the pages after the
  /// first, answered here without another round trip to the owner. One window, released with this client.
  pub(crate) read_ahead: Option<crate::verbs::ReadAhead>,
  /// The budget charge of this client's read-ahead batches still being fetched (`crate::verbs::ReadAhead`): moved
  /// to its window when a batch is delivered, and credited with the client when it goes — so a batch whose reply
  /// was never carried home (`crate::xshard::send_back`) holds its charge no longer than its client.
  pub(crate) read_ahead_pending: u64,
  /// The volumes of other hosts this client asked for an attachment of in the record form (A-222), so its reap ends
  /// them at their owners (`crate::remote_attach::reap_at_owners`). Bounded by `crate::remote_attach::bound`.
  pub(crate) remote_volumes: Vec<slates_ipc::protocol::VolumeId>,
  /// The direct maps this shard sent the client over its control socket whose replies have not reached it yet (Linux,
  /// A-188, A-505; `crate::direct_map`). A copy's shard that lost the answer to its send asks again, and that ask is
  /// answered from here, so a copy is never sent twice. Each leaves when its reply is delivered here, after every ask
  /// about it has run, since one channel carries the asks and the reply in order. So it holds at most the client's maps
  /// in flight, each within the client's share of its copy's shard, and it is released with this client.
  pub(crate) maps_sent: std::collections::BTreeSet<u64>,
}

impl std::fmt::Debug for ClientSlot {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientSlot")
      .field("client_id", &self.client_id)
      .finish()
  }
}

/// One volume the shard owns: the volume core's object, the host of its base when it has
/// one, and its bounded reservation.
pub struct VolumeSlot {
  /// The id.
  pub id: VolumeId,
  /// The mount name the volume was provisioned under — the friendly name it appears under in the host
  /// root, so a client mounts `/<name>` or reaches it by `cd <name>` (§4.6 "Chosen path"). Unique per
  /// host: a name is created on the one partition `verbs::owner_of_name` routes it to, which is also
  /// the partition the volume's id encodes, so a name routes to its volume with no global index (D-14).
  pub name: String,
  /// The volume.
  pub volume: Volume,
  /// The read-only host of the base directory, for an overlay.
  pub host: Option<OsHost>,
  /// The reservation, for a bounded volume.
  pub reservation: Option<slates_mem::budget::Reservation>,
  /// The inode-version reservation (§4.2 inode dimension): the volume's logical inode allowance
  /// reserved against the shard's version slab, returned on teardown so the slab is never over-offered.
  pub version_credit: Option<slates_mem::budget::VersionCredit>,
  /// The metadata reservation (§4.2 metadata dimension): the volume's records — its journal budget,
  /// its object, its snapshot slab's first segment — reserved against the shard's metadata ledger at
  /// admission, returned on teardown so the class is never over-offered.
  pub metadata_credit: Option<slates_mem::budget::MetadataCredit>,
  /// A strict (`--locked`) volume's entitlement, held against the daemon's lock capacity (§4.2 D-12,
  /// `lock_ledger`); returned when the slot drops.
  pub lock_credit: Option<crate::lock_ledger::LockCredit>,
  /// The seal this volume's owner shard took once it served the volume a planned move handed it (A-291,
  /// `verbs::seal_adopted_head`): the adopted head snapshot the seal superseded and the seal's head epoch. The control
  /// shard learns the seal only from this shard's answer, and an answer lost to a shard that stopped for a window (the
  /// caller forgot the call, and the shard ran it once it resumed) left the control shard asking again, which took a
  /// second seal and left the adopted snapshot behind for good (A-488). A second ask answers this record. It lives and
  /// ends with the slot, so a volume released and made again never carries one adoption's seal into another.
  pub adopted_seal: Option<(slates_db::catalog::SnapshotId, u64)>,
}

impl std::fmt::Debug for VolumeSlot {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VolumeSlot").field("id", &self.id).finish()
  }
}

/// The current authenticated member id and boot nonce for a stable certificate anchor (§4.8).
/// This map learns identities from contact, never from a manifest's routing placeholders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LearnedMember {
  /// The random per-start nonce the peer announced.
  pub boot_nonce: u64,
  /// The member id at that boot_nonce — what membership, ownership and records name the peer by.
  pub host: slates_db::HostId,
  /// The member id this anchor had before its last restart, `None` before any: an id this node saw end, whose RAM copies
  /// (a live log's among them) are gone with its process. A live takeover past tolerance counts on it (A-345,
  /// `live_ship::fetch`): a retired holder not seen restarted may return with its copy. One a anchor, bounded by the
  /// roster as the map is.
  pub previous: Option<slates_db::HostId>,
}

/// One peer's record session as the control shard keeps it (§4.8): the session itself, `None` while it is
/// out on a borrow — the record-plane coordinator's dispatch, or the link task's own discovery exchange —
/// and, for the coordinator's borrows, **which** session is out: the borrowed session's connection id
/// (unique per establishment), so a return refills the slot only with the session that left it. A late
/// return of an older session finds the id changed — or the slot already holding a newer session — and is
/// dropped, counted, never installed over the newer one
/// (`docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`).
pub struct RecordLink {
  /// The session, or `None` while it is borrowed.
  pub endpoint: Option<slates_transport::endpoint::Endpoint>,
  /// The connection id of the session a dispatch borrowed, while it is out; `None` otherwise.
  pub borrowed: Option<slates_transport::endpoint::ConnectionId>,
  /// Who borrowed it, while it is out (`fleet::take_sessions`'s label, a status counter name): what a forward that
  /// finds the session out counts, so a session that never comes back names its holder.
  pub lent_to: Option<&'static str>,
  /// The session's congestion state when it was last returned or established, so status reports it while a borrower
  /// has it out (a cross-region read keeps it borrowed for the whole transfer).
  pub last_seen: Option<slates_ipc::protocol::SessionReport>,
  /// How long this peer takes to answer a live exchange as a holder (A-396): the window the control shard holds its idle
  /// window for through the next one (`live_ship::hold_for_exchange`). Learned again with a new session.
  pub(crate) live_answers: crate::live_hold::AnswerWindow,
}

impl RecordLink {
  /// A link whose session is up and lendable.
  pub fn up(endpoint: slates_transport::endpoint::Endpoint) -> RecordLink {
    RecordLink {
      endpoint: Some(endpoint),
      borrowed: None,
      lent_to: None,
      last_seen: None,
      live_answers: crate::live_hold::AnswerWindow::default(),
    }
  }

  /// The session's congestion state as `endpoint` holds it now.
  pub fn seen(
    peer: slates_db::HostId,
    endpoint: &slates_transport::endpoint::Endpoint,
  ) -> slates_ipc::protocol::SessionReport {
    slates_ipc::protocol::SessionReport {
      peer: peer.0,
      lent: false,
      congestion_window: endpoint.congestion_window(),
      smoothed_rtt_ns: endpoint.smoothed_rtt(),
      pto_ns: endpoint.pto(),
      spurious_losses: endpoint.spurious_losses(),
      persistent_collapses: endpoint.persistent_collapses(),
      bytes_consumed: endpoint.bytes_consumed(),
      path_mtu: endpoint
        .path_mtu()
        .and_then(|mtu| u64::try_from(mtu).ok())
        .unwrap_or(0),
    }
  }

  /// The link to `peer` for a status report: read now when the session is here, else as it was last returned, marked
  /// lent (zero everywhere when it has not been seen yet).
  pub fn report(&self, peer: slates_db::HostId) -> slates_ipc::protocol::SessionReport {
    match &self.endpoint {
      Some(endpoint) => Self::seen(peer, endpoint),
      None => {
        let mut last = self
          .last_seen
          .clone()
          .unwrap_or(slates_ipc::protocol::SessionReport {
            peer: peer.0,
            lent: true,
            congestion_window: 0,
            smoothed_rtt_ns: 0,
            pto_ns: 0,
            spurious_losses: 0,
            persistent_collapses: 0,
            bytes_consumed: 0,
            path_mtu: 0,
          });
        last.lent = true;
        last
      }
    }
  }
}

/// One peer's record dial as its link task (`crate::fleet::establish_record_link`) last left it (§4.8, §4.14): what
/// status and a fleet whose formation failed read to tell a dial still inside one handshake call, one waiting on its
/// first discovery page, and one that keeps failing. A dialer that nobody answers spends a whole handshake budget
/// inside one call, and counts nothing until the call ends; this says so while it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordDial {
  /// When the link began dialing for the session it lacks (the runtime clock), while it lacks one.
  pub dialing_since_ns: Option<u64>,
  /// When its current handshake call began, while one runs.
  pub call_since_ns: Option<u64>,
  /// When its fresh session's first discovery page was sent, while the link waits on it to install the session.
  pub enroll_since_ns: Option<u64>,
  /// Handshake budgets its dialer has spent without establishing (`Endpoint::handshake_budgets_spent`).
  pub budgets_spent: u32,
  /// Why its dialer's last budget ended (`Endpoint::handshake_stall`), while that dialer is kept for another call.
  pub stall: Option<slates_transport::endpoint::HandshakeStall>,
  /// Handshake calls made for this peer since boot: a dial that keeps calling is one that keeps failing.
  pub calls: u64,
}

/// An instant or a span on two clocks: the wall, and the control shard's coordinator periods
/// ([`crate::merge_service::MergeShardState::coordinator_period`], one a period). The periods are the daemon's own
/// time: a daemon kept off its CPUs runs fewer of them a wall second, so a span bounded in them is judged as A-228 judges
/// a starved daemon, by what it did, never by a wall it was kept from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallAndPeriods {
  /// Nanoseconds on the runtime's clock.
  pub ns: u64,
  /// Coordinator periods.
  pub periods: u64,
}

impl WallAndPeriods {
  /// Now, on `state`'s shard: the runtime's clock, and the coordinator periods its state has counted (the control
  /// shard's; another shard's holds the period of the record plane's latest pass over it).
  pub(crate) fn now(state: &ShardState) -> WallAndPeriods {
    WallAndPeriods {
      ns: slates_rt::futures::now_ns(),
      periods: state.merge.coordinator_period,
    }
  }

  /// The span from `earlier` to this instant on each clock, never below zero.
  pub(crate) fn since(self, earlier: WallAndPeriods) -> WallAndPeriods {
    WallAndPeriods {
      ns: self.ns.saturating_sub(earlier.ns),
      periods: self.periods.saturating_sub(earlier.periods),
    }
  }
}

/// A mount capability's place in the shard's attachment registry (GAP-A9-4).
#[derive(Clone, Copy, Debug)]
pub struct MountAttachment {
  /// The registry attachment its requests are admitted under.
  pub registry: slates_bridge_core::AttachmentId,
  /// The boundary a snapshot over it can claim (§4.6): the NFS client buffers acknowledged writes
  /// until its `COMMIT`, so the daemon's records are the server-visible boundary, never a complete one.
  pub boundary: slates_ipc::protocol::SnapshotBoundary,
}

/// The shard's state.
pub struct ShardState {
  /// The shard: the runtime's id, what messages are addressed to (process-local).
  pub shard: u16,
  /// The partition: this shard's index among the daemon's, what volume ids and client ids
  /// route by (persistent: a restarted daemon's partition holds the same records).
  pub partition: u16,
  /// The configuration.
  pub config: DaemonConfig,
  /// The segment (this shard's mapping).
  pub segment: AnchorSegment,
  /// The anchor-owned content object that holds this shard's recovery image (§4.8), and the byte
  /// range within it this shard owns (shards share one object, partitioned by index). `None` when
  /// the anchor provides no content object (a build or config without anchor-backed recovery); then
  /// a restart recreates content empty as before (BUG-11).
  pub content: Option<SparseObject>,
  /// The content arena's allocation count at the last reap tick: unchanged at the next means the shard allocated
  /// nothing in between, so its free blocks can go back to the OS (A-105, `daemon::purge_if_idle`).
  pub(crate) purge_seen_allocations: u64,
  /// The content arena's allocation count and allocated bytes at the last idle reap tick (A-214), `None` before the
  /// first: a later idle tick with another count ends a busy period, whose net growth is the allocated bytes' rise.
  pub(crate) burst_seen: Option<(u64, usize)>,
  /// The net growth of the content arena's allocated bytes over its last busy period (A-214): the write burst an idle
  /// shard keeps free pages pre-faulted for (`daemon::purge_if_idle`).
  pub(crate) last_burst: usize,
  /// The content arena's allocation count when the reserve's refill last found nothing to fault (A-214): until the
  /// arena allocates again, or the burst measure moves, an idle moment's refill has nothing to do.
  pub(crate) refill_settled: Option<u64>,
  /// What one idle give-back step walks (A-226), the recovery memory's and the write log's alike: the bytes the shard
  /// gave back in one step quantum of its own thread's CPU, measured by the steps for the next (A-491,
  /// `daemon::GiveBackRate`); a page until the first rate is learned.
  pub(crate) give_back_rate: crate::daemon::GiveBackRate,
  /// The half-open byte range `[start, end)` of `content` this shard publishes into and recovers
  /// from; `0..0` when there is no content object.
  pub content_range: (usize, usize),
  /// The half-open byte range `[start, end)` of `content` holding this shard's delta log (A-68), beside the two
  /// checkpoint slots of `content_range`; `0..0` when there is no content object.
  pub delta_range: (usize, usize),
  /// This shard's recovery journal (A-68; `slates_vfs::checkpoint_log`): the committed checkpoint, the log's tail
  /// and what has been logged since. Only this shard publishes into its ranges.
  pub journal: slates_vfs::checkpoint_log::Journal,
  /// The stage a checkpoint is streamed through into its slot (`verbs::publish_checkpoint`; `STREAM_STAGE_BYTES` and
  /// one inode's encoding), which a delta also encodes in; kept so its capacity is reused. Bounded by the stage and the
  /// largest delta frame, which the delta log bounds.
  pub checkpoint_buffer: Vec<u8>,
  /// The volume keys the committed journal holds, so a delta names the volumes gone since (A-68).
  pub published_keys: std::collections::BTreeSet<[u8; 16]>,
  /// Whether the last publication kept a record of a volume it left out, and so kept every deferred block the arena
  /// holds (A-430, `verbs::keep_left_out_records_blocks`): until one that keeps none commits, a publication run only to
  /// relieve the arena releases nothing, and is not run.
  pub(crate) deferred_kept: bool,
  /// What of the held pieces the committed journal holds that a delta cannot tell from the tracked maps alone
  /// (A-270; `crate::content_holder::held_changes`): the whole pieces' bytes and each live copy's published slots.
  pub(crate) published_held: crate::content_holder::PublishedHeld,
  /// The writes acknowledged since the last complete publication, kept whole in the region before `content_range` in
  /// the anchor's content object (A-63; `crate::write_log`); `None` without a content object. The log serves a FUSE
  /// mount taken over across a restart (Linux) and a Shards stream's writes (every platform; A-316, A-339, A-402); an
  /// NFS client resends what a restart lost itself, by its write verifier.
  pub(crate) write_log: Option<crate::write_log::WriteLog>,
  /// What the previous daemon's log held to replay, its writes and `FORGET`s, consumed when the volumes are rebuilt
  /// (A-63).
  pub(crate) replay: Vec<crate::write_log::Logged>,
  /// Whether the previous daemon acknowledged a write it could not log (its log overflowed and the publication that
  /// would have emptied it was refused): every file a taken-over mount had open is then reported as having lost
  /// writes (A-61, A-63), and so is every file a resynced Shards stream's guest had open (A-393). Unix only: a FUSE
  /// mount is Linux's and a resynced stream Unix's, and nothing on Windows outlives its daemon to be told (a Windows
  /// stream's resync is owed, g234), so there the recovered log's verdict has no reader.
  pub(crate) writes_lost: bool,
  /// The barrier reply each FUSE mount (Linux) or resyncable Shards stream (A-316, A-339) is about to deliver (A-61):
  /// recorded before the barrier publishes, so the publication carries it with the effect, and removed once written. At
  /// most one per attachment (each serves one request at a time). A Windows stream's too, since A-433.
  pub(crate) pending_replies: std::collections::BTreeMap<u64, slates_vfs::recover::HeldReply>,
  /// The barrier replies the previous daemon published and never delivered, by attachment (A-61), handed to the
  /// mounts and streams recovery keeps.
  pub(crate) recovered_replies: std::collections::BTreeMap<u64, (u64, Vec<u8>)>,
  /// Each range a Shards stream's held DAX backing has answered, in the region of the anchor's content object after the
  /// delta log (A-365; Windows' sections, A-448; `crate::dax_ranges`): what a successor ingests and never refills. `None`
  /// without a content object.
  pub(crate) dax_ranges: Option<crate::dax_ranges::RangeTable>,
  /// The DAX objects the anchor handed back for this shard's partition (A-365; Windows' sections, A-448;
  /// `crate::shards::dax_hold`), until recovery takes each back or lets it go. Bounded by the anchor's bound for them
  /// (`DaemonConfig::held_dax_bound`).
  pub(crate) inherited_dax: Vec<crate::shards::dax_hold::InheritedDax>,
  /// The DAX objects recovery kept again in the store, by the attachment of the stream they belong to, until the
  /// stream's adoption takes them (`crate::shards::adopt_one`); any left after every adoption is let go. Bounded as
  /// `inherited_dax` is, of which these were part.
  pub(crate) adopted_dax: std::collections::BTreeMap<u64, Vec<crate::shards::dax_hold::AdoptedDax>>,
  /// The NFS write verifier this shard's exports answer WRITE and COMMIT with (RFC 1813
  /// `writeverf3`, §4.6): the shard's boot instant, so it is unique to this daemon instance and a
  /// client that holds unstable writes from before a restart sees it change and re-sends them.
  /// Format: the boot instant's monotonic nanoseconds, big-endian — a monotonic clock never repeats
  /// within a host's uptime, and a host reboot discards every client's unstable state with the
  /// anchor's RAM, so no older instance is ever confused with a newer one.
  pub write_verifier: [u8; size_of::<u64>()],
  /// The NFSv4 state of the listener this shard runs (§4.6 A-35): its clients, sessions and opens,
  /// created with the first v4 call under the configuration's derived bounds. A v4 client's session
  /// outlives its connection, so the state is the listener's, not a connection's. `None` on a shard
  /// that serves no v4 call.
  pub nfs_v4: Option<slates_bridge_nfs::v4::compound::Server>,
  /// The NFSv4 file state of the files this shard owns (§4.6 A-36): their opens and locks, whichever
  /// listener or connection made them, created with the first v4 state call under the configuration's
  /// derived bounds.
  pub nfs_v4_files: Option<slates_bridge_nfs::v4::files::FileState>,
  /// The delegation recalls waiting for each session's back channel (`crate::delegation`; bounded by the delegations
  /// each client holds).
  pub(crate) recall_queues: std::collections::BTreeMap<
    slates_bridge_nfs::v4::types::SessionId,
    crate::delegation::RecallQueue,
  >,
  /// This daemon life's NFSv4 instance on the shard's partition (§4.6 A-37), advanced durably on first
  /// use; every NFSv4 id the shard mints names it.
  pub nfs_v4_instance: Option<u32>,
  /// The call records of the recorded compounds whose calls this shard served (A-127), bounded by the configuration's
  /// derived record bytes; what answers a compound retried after a restart.
  pub(crate) nfs_calls: slates_bridge_nfs::v4::replay::CallRecords,
  /// The call of a recorded compound this shard is serving right now (A-127), set for the synchronous span of one
  /// call so a file-state change it makes carries the compound's record in its own transaction.
  pub(crate) nfs_call: Option<crate::nfs_replay::RecordedCall>,
  /// The NFSv3 replies held for connections the anchor holds (A-128), by (connection, xid): each mutation's reply,
  /// recorded before its barrier publishes so the publication carries it with the effect, and dropped once its turn's
  /// replies are sent. At most a turn's mutations per held connection on this shard.
  pub(crate) held_nfs_replies: std::collections::BTreeMap<(u64, u32), (bool, Vec<u8>)>,
  /// Whether [`ShardState::held_nfs_replies`] changed since the last publication, which a delta then carries whole.
  pub(crate) held_nfs_replies_changed: bool,
  /// The held NFSv3 replies the previous daemon published and never sent (A-128), by (connection, xid): a request it
  /// applied and left unanswered on a held connection is answered from here, never applied twice. Each is taken when
  /// its request is read again; at most the previous image's.
  pub(crate) recovered_nfs_replies: std::collections::BTreeMap<(u64, u32), (bool, Vec<u8>)>,
  /// The partition.
  pub db: Db,
  /// The owner runtime this node takes part in a region as (§4.8, boot step 6): the SWIM membership
  /// view, the configuration group, and the owner's register acceptor, composed by `slates-cluster`'s
  /// `FleetNode`. `FleetNode::solo` at `f = 0` (the laptop) — the same code path a fleet runs (R8), a
  /// membership event folding into the configuration through [`slates_cluster::fleet::FleetNode::observe`]. The
  /// verbs read `fleet.configuration()` for the candidate placement of an object and `placed_heads` for
  /// what the fleet has actually committed; the live probe/gossip loop that drives `observe` and the
  /// cross-node commit run in `crate::fleet` on the control shard.
  pub fleet: slates_cluster::fleet::FleetNode,
  /// The durability the installed configuration **cannot** hold to the operator's declared policy (§4.8
  /// "Placement" — the policy "that gates a refusal"): the measured shortfall, or `None` when the
  /// configuration is within the accepted loss or no policy is declared (the default, and every laptop —
  /// a single copy has no coincident loss, so `f = 0` is never short, R8). Measured at every configuration
  /// install — boot, a council commit, the cross-shard fan — by `DurabilityBound::shortfall`, and read per
  /// write by the verbs as a field (`verbs::dispatch` refuses a write that would commit a new head or seal
  /// `DurabilityUnmet` with these numbers), never recomputed on the write path.
  pub durability_shortfall: Option<crate::config::DurabilityShortfall>,
  /// This node's **stable cert-anchor** — `deploy::host_id_of_certificate` of its own certificate, the id that
  /// does **not** change across a restart (§4.8; task #22 two-id model). Distinct from `fleet.host()`, which
  /// is the **ephemeral** member id (per boot, so a restart is a new member). The anchor keys a client's
  /// **completion record** (the RIFL origin's high half), so a retry meets its record across a daemon restart —
  /// exactly-once survives the ephemeral id changing. Ownership, rendezvous and `ObjectId::creator` use the
  /// ephemeral `fleet.host()`; only the completion origin uses this. On a laptop it is the identity's own
  /// cert anchor, stable per machine (R8).
  pub origin_anchor: slates_db::HostId,
  /// The regional configuration council (§4.8, D-14 — the "configuration master"): the multi-voter Raft the
  /// control shard's config plane drives over the transport to agree on the region's configuration
  /// (`crate::fleet::run_config_council`). A laptop runs a solo council that self-leads (R8). Only the
  /// control shard drives and serves it; other shards hold an inert copy of the same boot state.
  pub council: slates_cluster::config_group::RegionalCouncil,
  /// This member has a committed regional admission and a root configuration (AUD-07).
  /// Copied to owner shards when configuration changes; read without coordination per request.
  pub consensus_ready: bool,
  /// Last atomic anchor publication of both configuration groups (§4.8).
  pub consensus_generation: u64,
  /// Reviewed replacements and their explicit join destinations (§4.8).
  pub(crate) recovery: crate::consensus_recovery::RecoveryState,
  /// A retention failure closes the control shard before its next reply.
  pub consensus_failure: Option<slates_anchor::AnchorError>,
  /// This start's explicit bootstrap, retained for idempotent retries without resetting Raft.
  pub bootstrap_authorized: Option<bool>,
  /// The regional group's immutable identity — the hash of its origin, with the origin
  /// (`crate::consensus::GroupIdentity`); a separate bootstrap cannot replace its log.
  pub council_group: Option<crate::consensus::GroupIdentity>,
  /// The root group's immutable identity, distinct from its current voters.
  pub root_group: Option<crate::consensus::GroupIdentity>,
  /// The **root configuration group** across regions (§4.8, D-14 — "a root group across regions holds region
  /// membership and cross-region promotions"): the multi-voter Raft the control shard's config plane drives
  /// over the transport (among the region representatives) to agree on the `RootConfiguration` — which regions
  /// exist, moved volume homes, and region promotions. A single-region fleet (the default) runs a solo root
  /// group that self-leads (R8). Only the control shard drives and serves it; other shards hold an inert copy.
  pub root: slates_cluster::root_group::RootGroup,
  /// Each fleet member's region (§4.8, D-14), the host→region map the root group's leader reconciles the
  /// region membership from (an alive host's region is an alive region). A host absent is in the sole region
  /// `RegionId(0)`. Set at boot from the fleet membership; empty on a laptop.
  pub node_regions: std::collections::BTreeMap<slates_db::HostId, slates_db::register::RegionId>,
  /// Each region's designated mirror (§4.8 — region-loss promotion): the reconcile leaves a lost mirrored
  /// region for a deliberate operator promotion (not auto-retire), and `Daemon::promote_region` looks up the
  /// mirror to promote here. Set at boot from the fleet membership; empty on a laptop or when none declared.
  pub region_mirrors:
    std::collections::BTreeMap<slates_db::register::RegionId, slates_db::register::RegionId>,
  /// The landing runtime: grants, leases and the audit log (§4.15), mirrored to the
  /// database's durable records.
  pub landing: crate::landing::LandingState,
  /// The image builds and merges running on this shard (§4.6, A-160).
  pub(crate) image: crate::image::ImageState,
  /// The anchors of the fleet's members as the control shard last looked (A-222): a host that leaves them has its
  /// clients' attachments ended on every shard (`crate::remote_attach::sweep_departed`). Bounded by the fleet's members.
  pub(crate) configured_members: Vec<u64>,
  /// Anchors that departed and whose attachments some shard has not yet ended, owed to the next look (A-222). Bounded
  /// by the fleet's members.
  pub(crate) departed_owed: Vec<u64>,
  /// The same-kernel direct maps being filled on this shard (§4.6, A-171).
  pub(crate) direct_map: crate::direct_map::DirectMapState,
  /// The store.
  pub store: Store,
  /// The volumes.
  pub volumes: Slab<VolumeSlot>,
  /// Volumes by id.
  pub by_id: BTreeMap<VolumeId, slates_mem::Handle<VolumeSlot>>,
  /// The clients.
  pub clients: Slab<ClientSlot>,
  /// The next inode prefix a volume takes (unique per volume on the host: the shard in the
  /// high bits and a counter below).
  pub next_prefix: u16,
  /// The next attachment id.
  pub next_attachment: u64,
  /// The shard's attachment registry (§4.4, §4.6 "Writeback and snapshot barrier"; GAP-A9-4): the one
  /// authority record every transport's request on this shard's volumes rides — a mount's request is
  /// admitted (`begin`/`end`) under the registry attachment its capability maps to — so a barrier the
  /// owner runs over a volume (a snapshot) sees every request the seam may still be applying and
  /// closes every live attachment's generation. Bounded by the registry's own cap (a typed `SlabFull`
  /// refusal past it).
  pub attachments: slates_bridge_core::Attachments,
  /// The registry attachment each mount capability's catalog attachment rides on, admitted on its
  /// first request and revoked when the attachment ends (`verbs::end_attachment`). Bounded by the
  /// partition's attachment cap: one entry per live catalog attachment at most.
  pub mount_attachments: BTreeMap<u64, MountAttachment>,
  /// The volumes this shard's catalog holds that recovery could not rebuild, and why (§4.8): each is listed, and every
  /// verb on it answers `Refusal::NotRecovered` with its cause (`verbs::find`). Bounded by the partition's volume cap:
  /// one entry per catalog volume at most, made once at the shard's start.
  pub(crate) unrecovered:
    BTreeMap<slates_db::catalog::VolumeId, slates_ipc::protocol::RecoveryCause>,
  /// The volumes this shard catalogued when its node's host epoch last advanced at a configuration it installed: the
  /// host was retired while away and admitted again under its id, so each was taken over and its successor serves it
  /// (A-334; §4.8 "Authority scope": the bumped epoch fences every object the host owned). A client's verb on one is
  /// forwarded to the successor (`verbs::owned_by_another_host`), and its latest state is never served here
  /// (`verbs::lease_verdict`). Bounded by the partition's volume cap: a subset of the catalog, pruned to it at each
  /// yield (`crate::readmission`).
  pub(crate) yielded: std::collections::BTreeSet<slates_db::catalog::VolumeId>,
  /// The slots of stale copies this shard relinquished to a takeover that named this node again (A-362,
  /// `verbs::materialize_taken_over`), each torn down a destroy slice at a time by the reaper (`verbs::step_destroys`)
  /// after its record went, since the volume made from the adopted state took its id. Bounded by the partition's volume
  /// cap: a relinquished copy was a volume this shard held, and each leaves once its teardown is done.
  pub(crate) relinquished: Vec<VolumeSlot>,
  /// The claims this shard holds for live volumes routed to it that this node holds the log of as a candidate (§4.2 "a
  /// remote holder makes the same admission against its own machine before acknowledging placement"; g229): by volume,
  /// the credits a successor's restore takes over (`verbs::CandidateClaim`, `verbs::make_restored`). Bounded by the live
  /// copies this node holds of volumes routed to this partition, each admitted only once its catalog is held
  /// (`live_ship::admit_claim`): a claim whose copy went is released at the control shard's next reconciliation
  /// (`live_ship::reconcile_claims`).
  pub(crate) candidate_claims:
    std::collections::BTreeMap<slates_db::catalog::VolumeId, crate::verbs::CandidateClaim>,
  /// The Linux FUSE mounts this shard serves, by attachment (§4.6 "Linux"; AUD-29-64; `crate::fuse`). Bounded
  /// by the partition's attachment cap: one entry per live FUSE attachment at most.
  #[cfg(target_os = "linux")]
  pub(crate) fuse_mounts: crate::fuse::FuseMounts,
  /// The FUSE mounts a predecessor kept that this shard's catalog names, recorded as this daemon's own as the shard
  /// starts, before any base or landing is served (A-227); each is taken by its mount's adoption (`crate::fuse`), and
  /// what no adoption takes is let go once adoption ends. Bounded by the partition's attachment cap.
  #[cfg(target_os = "linux")]
  pub(crate) learned_fuse: BTreeMap<u64, crate::fuse::OwnMountRecord>,
  /// The FUSE helpers (`fusermount3`, a mount's and an unmount's) this shard waited on, learned for the next one's
  /// bound (A-525, `crate::helper_runs`). Memory only: a restarted shard learns them again.
  #[cfg(target_os = "linux")]
  pub(crate) fuse_helpers: crate::helper_runs::HelperRuns,
  /// The macOS NFS mounts this shard's attachments are bound to, recorded as this daemon's own (A-227,
  /// `crate::own_nfs`): bound by the CLI or learned at the shard's start, each let go with its attachment. Bounded by
  /// the partition's attachment cap.
  #[cfg(target_os = "macos")]
  pub(crate) own_nfs_mounts: BTreeMap<u64, crate::own_nfs::OwnNfsMount>,
  /// Whether this shard's watch of the kernel's unmounts runs (A-511, `crate::own_nfs::watch`): started once the shard
  /// holds an own NFS mount, ended by the watch itself once it holds none.
  #[cfg(target_os = "macos")]
  pub(crate) unmount_watch: bool,
  /// The Windows WinFsp mounts this shard serves, by attachment (§4.6 "Windows"; `crate::winfsp`). Bounded by the
  /// partition's attachment cap: one entry per live WinFsp attachment at most.
  #[cfg(windows)]
  pub(crate) winfsp_mounts: crate::winfsp::WinFspMounts,
  /// Held Windows mounts not yet matched to catalog records, bounded by the anchor's admitted mount count.
  #[cfg(windows)]
  pub(crate) inherited_winfsp: Vec<crate::winfsp::InheritedMount>,
  /// Catalog records whose held Windows volume will be served again after the shard's recovery.
  #[cfg(windows)]
  pub(crate) adopt_winfsp: Vec<(
    slates_db::catalog::AttachmentRecord,
    crate::winfsp::InheritedMount,
  )>,
  /// The FUSE mounts recovery ended (attachment, mount point): their device died with the old process, so
  /// each dead mount is unmounted once the shard runs (`fuse::unmount_stale`). Filled once, at recovery, and
  /// emptied by that step; bounded by the partition's attachment cap.
  pub(crate) stale_fuse_mounts: Vec<(u64, String)>,
  /// The devices the anchor handed back for this shard's partition (A-61; `crate::fuse_hold`): FUSE devices (Linux) and
  /// Shards streams' resync ends (A-316, A-339), until recovery takes each for its kept attachment or closes it. Bounded
  /// by the anchor's held-device bound.
  #[cfg(unix)]
  pub(crate) inherited_fuse: Vec<crate::fuse_hold::HeldDevice>,
  /// The writable copies the anchor handed back for this shard's partition (A-197; `crate::held_copy`), until recovery
  /// takes each back or lets it go. Bounded by the anchor's held-copy bound.
  #[cfg(target_os = "linux")]
  pub(crate) inherited_copies: Vec<crate::held_copy::InheritedCopy>,
  /// The FUSE mounts recovery kept because the anchor held their devices (A-61), each with its record, served
  /// again once the shard runs (`fuse::adopt_held`). Filled once at recovery and emptied by that step.
  #[cfg(target_os = "linux")]
  pub(crate) adopt_fuse: Vec<(
    slates_db::catalog::AttachmentRecord,
    crate::fuse_hold::HeldDevice,
  )>,
  /// The Shards streams recovery kept because the anchor held their resync ends (A-316, A-339), each with its record,
  /// served again on a fresh stream once the shard runs (`shards::adopt_held`). Filled once at recovery and emptied by
  /// that step; bounded as `inherited_fuse` is, of which these were part.
  #[cfg(unix)]
  pub(crate) adopt_shards: Vec<(
    slates_db::catalog::AttachmentRecord,
    crate::fuse_hold::HeldDevice,
  )>,
  /// The Windows Shards streams recovery kept because the anchor held their resync ends (A-433), each with its record,
  /// served again on a fresh stream once the shard runs (`shards::adopt_held`). Filled once at recovery and emptied by
  /// that step; bounded as `inherited_resync` is, of which these were part.
  #[cfg(windows)]
  pub(crate) adopt_shards: Vec<(
    slates_db::catalog::AttachmentRecord,
    crate::anchor_hold::HandedResync,
  )>,
  /// The Shards streams' resync ends the anchor handed back for this shard's partition (Windows, A-433), until recovery
  /// keeps each whose record it keeps (`adopt_shards`) or `shards::adopt_held` lets it go. Bounded by the anchor's
  /// held-entry bound.
  #[cfg(windows)]
  pub(crate) inherited_resync: Vec<crate::anchor_hold::HandedResync>,
  /// The read-only views of snapshots presented through host mounts and guest devices, by attachment (AUD-29-76;
  /// `crate::snapshot_view`). Bounded by the partition's attachment cap: one per recorded snapshot attachment.
  pub(crate) snapshot_views: std::collections::BTreeMap<u64, crate::snapshot_view::SnapshotView>,
  /// The guest devices this shard serves and the consumer each was admitted for (§4.6, §4.13; AUD-29-73), so a
  /// consumer's revocation reaches its devices. Bounded by the shard's device limit (`clients_per_shard`, the
  /// loop registry's bound): an entry is added when a loop registers and removed when it ends.
  #[cfg(unix)]
  pub(crate) guest_devices: Vec<(
    slates_bridge_virtiofs::serve::DeviceId,
    slates_db::catalog::Principal,
    u64,
  )>,
  /// The Shards fs streams this shard serves, by attachment (§4.6, the Shards route; A-161; `crate::shards`). Bounded
  /// by the shard's client seats (`clients_per_shard`): an attach past them is refused `Overloaded`.
  pub(crate) shards_streams: crate::shards::ShardsStreams,
  /// The Shards stream ends owed to this shard's clients, by the request word of the attach that made each (A-161;
  /// `crate::shards::Handoff`): sent over the client's control socket just before the attach's reply (Linux), or
  /// duplicated into the client's process as it goes (Windows, A-179). Bounded by the
  /// requests the shard's clients may have in flight: an attach past it is refused `Overloaded`.
  pub(crate) stream_handoffs: crate::shards::Handoffs,
  /// The Shards stream ends offered on the socket route, by attachment (A-161, macOS; `crate::shards_route`): each
  /// claimable once with its secret until it expires. Bounded with the handoffs.
  #[cfg(target_os = "macos")]
  pub(crate) stream_offers: crate::shards_route::Offers,
  /// The direct maps' copies offered on the socket route, by mapping (A-173, macOS; `crate::direct_map`): each read-only
  /// descriptor claimable once with its secret until it expires. Bounded by the clients' shares of the direct maps.
  #[cfg(target_os = "macos")]
  pub(crate) map_offers: crate::shards_route::Offers,
  /// The socket route's exchanges running on this shard (the control shard serves them). Bounded by the shard's
  /// client seats: a connection past them is refused `Busy`.
  #[cfg(target_os = "macos")]
  pub(crate) route_exchanges: usize,
  /// The clock.
  pub clock: slates_vfs::clock::HostClock,
  /// Requests served.
  pub served: u64,
  /// Refusals by kind name.
  pub refusals: BTreeMap<&'static str, u64>,
  /// The daemon's grant-issuer secret for this start (§4.13), read from the anchor's supervision block
  /// at shard init — the key a `Grant` request's proof of authority is verified under
  /// (`landing::grant_proof`). Every shard reads the same secret, so a grant verifies on whichever shard
  /// serves the client; it is never sent on any channel.
  pub issuer_secret: [u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  /// The launcher secret its starter handed the anchor (A-285): a launcher-class proof verifies under it as under the
  /// issuer secret, so a launcher keeps its authority across its own restarts and the daemon's. `None` while the anchor
  /// publishes none.
  pub launcher_secret: Option<[u8; slates_anchor::layout::LAUNCHER_SECRET_BYTES]>,
  /// The node's client session secret (`crate::session_key`), read from the anchor's supervision block at shard init:
  /// what a client's session key is derived from and checked against on the shard that serves it. Never sent.
  pub session_secret: [u8; slates_anchor::layout::SESSION_SECRET_BYTES],
  /// This shard's copy of the node's sealing root (A-92), in hyper-seal's locked region; `None` while sealing is
  /// unavailable. Tenant keys on this partition are wrapped under it.
  pub seal_root: Option<hyper_seal::keys::WrappingKey>,
  /// How the daemon came by the root, for the status report.
  pub seal_state: crate::seal_keys::RootState,
  /// The node's ML-KEM-1024 recipient (A-92 piece 4a), on the control shard only (its partition holds the record):
  /// what a successor unwraps a taken-over volume's lineage key with. `None` elsewhere and while sealing is
  /// unavailable.
  pub seal_recipient: Option<hyper_seal::recipient::Recipient>,
  /// The candidate nodes, by stable anchor, this shard's pair key was delivered to in this daemon's life (A-92 piece 4b),
  /// each with the member id it was delivered to, so a node back at its anchor with fresh RAM (a new member id) is
  /// delivered it again; bounded by the fleet's peer capacity.
  pub pairs_delivered: std::collections::BTreeMap<u64, slates_db::HostId>,
  /// The stable anchor of each neighbour, by member id, as this owner shard's pair delivery last resolved them (A-92
  /// piece 4c): a head naming sealed content ships only once it carries a key entry for every remote candidate's
  /// anchor. Refreshed each record period, so a member that restarts under a new id with the same anchor resolves;
  /// bounded by the neighbourhood.
  pub member_anchors: std::collections::BTreeMap<slates_db::HostId, u64>,
  /// The recipient public keys of peers by stable anchor, each with the member it was learned from, asked over their
  /// authenticated sessions and kept (A-92 piece 4b), bounded by the fleet's peer capacity (`fleet::kept_recipient`).
  pub peer_recipients: BTreeMap<u64, (slates_db::HostId, hyper_seal::recipient::RecipientPublic)>,
  /// Replies waiting for the client's ring (full, or the reply came from another shard), by
  /// client slot; `recorded` says the completion record already exists (at the owner
  /// partition of a forwarded verb), so this shard must not record it again.
  pub deferred: Vec<Deferred>,
  /// The shard's server task, woken when a client is added while it idles.
  pub server_task: Option<slates_rt::TaskId>,
  /// The clients whose watches reported them gone and whose retirement is owed, each with its id (A-475): a seat joins
  /// once, at its report, so the queue is bounded by the shard's seats (`clients_per_shard`). The reaper takes them a
  /// batch at a time (`crate::reap::reaper`).
  pub(crate) dead_clients: std::collections::VecDeque<(slates_mem::Handle<ClientSlot>, u32)>,
  /// The shard's reaper task, woken when a client's watch reports it gone while the reaper idles (A-475).
  pub(crate) reaper_task: Option<slates_rt::TaskId>,
  /// The seats whose status capture has a deadline, by it (A-522, `crate::deadlines`): a seat once at most.
  pub(crate) capture_deadlines: crate::deadlines::CaptureDeadlines,
  /// The shard's deadline keeper, woken when a deadline is set earlier than the one it sleeps for (A-522).
  pub(crate) deadline_task: Option<slates_rt::TaskId>,
  /// The deadline the keeper sleeps until, `None` while none is set (`crate::deadlines::noted` reads it).
  pub(crate) deadline_armed: Option<u64>,
  /// Listing pages in flight: by request word, the client slot, the shards still to answer, and
  /// their parts so far, each at most a page (a scatter-gather over owners, §4.8 "Lookup"; §4.4 `list`).
  pub(crate) scatters: BTreeMap<u64, (u32, usize, crate::verbs::ListParts)>,
  /// `grants` reads in flight: the caller's grants live on the shards that presented their landings (a
  /// grant record is written where its landing was), so the read is a scatter-gather like a listing — by
  /// request word, the client slot, the shards still to answer, and the summaries so far.
  pub(crate) grant_scatters: BTreeMap<
    u64,
    (
      u32,
      usize,
      Vec<crate::verbs::PagePart<slates_ipc::protocol::GrantSummary>>,
    ),
  >,
  /// Every shard of the daemon, for the scatter.
  pub shards: Vec<u16>,
  /// What the last start's recovery found (the status reports it).
  pub recovered: slates_db::replay::Recovered,
  /// When this shard's state was installed (the freshness of what recovery measured).
  pub booted_ns: u64,
  /// Daemon status requests in flight: by request word, the client slot, the shards still
  /// to answer, and the parts so far.
  pub status_scatters: BTreeMap<u64, (u32, usize, Vec<slates_ipc::protocol::ShardReport>)>,
  /// Acknowledgements whose other shards' releases are still out (A-216): by request word, the shards still to
  /// answer. The acknowledgement itself was answered when this shard released; an entry goes when its last part is
  /// back. Bounded by [`crate::verbs::ack_scatter_bound`]: past it the other shards' parts are not sent, and are
  /// counted refused and left to the next watermark.
  pub ack_scatters: BTreeMap<u64, usize>,
  /// The listings, grants and status scatters a part of which could not be sent, by request word, and the first shard
  /// it was for: the scatter answers `Overloaded { shard }` when its last part is settled. Bounded by those three
  /// tables, whose entries it follows.
  pub scatter_refusals: BTreeMap<u64, u16>,
  /// When this shard last did work for a client (served, forwarded, or ran a forwarded verb);
  /// the server loop polls for the idle window past it before parking (§4.7).
  pub last_work_ns: u64,
  /// Forwards refused by a full control channel, kept to retry (backpressure, never a drop);
  /// bounded by the clients' credit, refused typed beyond it.
  pub pending_forwards: std::collections::VecDeque<PendingForward>,
  /// Green volumes' merge engines (§4.16): the in-memory chain and per-path state a green owns,
  /// keyed by its id. A green is not a store-backed VFS tree; its merged content lives here.
  pub greens: BTreeMap<VolumeId, Green>,
  /// Greens whose recovered history did not reproduce what was acknowledged (AUD-29-18), each with why:
  /// no engine is installed and every verb naming one is refused typed, while its durable origin and chain
  /// stay untouched as the evidence a reviewed recovery works from. Bounded by the partition's greens.
  pub fenced_greens: BTreeMap<VolumeId, crate::merge_service::GreenFence>,
  /// Work volumes' declared operations (§4.16): each work over a green accumulates the operations an
  /// agent declares (through `edit`) and the bytes they name, composed into an increment on submit.
  pub works: BTreeMap<VolumeId, WorkState>,
  /// The bytes each green's **retention** — its content history beyond the current files and its
  /// rejected-result cache (`Green::retained_bytes`) — has charged to the shard's budget
  /// (`ShardBudget::charge_retention`; §4.2 all-cost admission; AUD-16). The credit authority: a
  /// green never credits more than it charged, and the settle after every submit, fold, pin move and
  /// work retirement moves this to exactly what the engine holds. Balanced by construction; bounded
  /// by the greens.
  pub green_retention: BTreeMap<VolumeId, u64>,
  /// The read-ahead windows this shard's clients hold or are fetching (`crate::verbs::ReadAhead`): the bytes charged,
  /// never past `DaemonConfig::read_ahead_bytes`. Charged before a batch is fetched, credited as each window is
  /// replaced, dropped or its client goes.
  pub(crate) read_ahead: crate::verbs::ReadAheadLedger,
  /// The merge plane's service state (§4.16, `crate::merge_service`): version-pinned green
  /// attachments, and the fleet's pending merge records and holder replicas.
  pub merge: crate::merge_service::MergeShardState,
  /// This shard's bounded telemetry ring (§4.14): the chokepoint spans emitted on the shard, held
  /// shed-first (the newest kept, the oldest dropped and counted), drained by the `Telemetry` verb
  /// (`crate::telemetry`) in batches bounded to one reply. Per-shard and thread-local, so it needs no
  /// lock (R2) — the design's "per-shard rings"; the status scatter is their aggregation.
  pub telemetry: slates_wire::observe::SpanSink,
  /// This shard's opener of spans (§4.14, the three-id law): it mints the shard's trace and span ids
  /// (the node and partition folded in, so they are distinct daemon- and fleet-wide) and is the only
  /// way to open a span — a root from the request it serves, a child from the context that caused it.
  pub tracer: slates_wire::observe::Tracer,
  /// The innermost open span's context while the shard serves (§4.14): the `ring.request` span from
  /// the slot read, then the `shard.op` span while its verb runs — so a chokepoint deeper in a verb (a
  /// `log.append`, a `merge.verdict`, a `land.entry`) opens its span *within* the one that caused it,
  /// sharing its request and trace and naming it, without threading the context through every handler.
  /// The shard is single-threaded and runs one verb at a time with no awaits inside (§4.7), so this is
  /// unambiguous; `None` between requests, and for work whose cause crossed a boundary that carried
  /// none (the span then declares its cause missing).
  pub current_span: Option<slates_wire::observe::SpanContext>,
  /// The idle window a forward made for the request now being served holds its owner open for (§4.7; A-326), 0 for
  /// none (`verbs::loop_hold`). Set by `serve_client` around each request, as `current_span` is; 0 between requests.
  pub forward_hold_ns: u64,
  /// The requests this shard has read in a row, each within F of its last client I/O: the length of its closed loop so
  /// far, which caps a forward's hold (`verbs::loop_hold`; A-326). 0 after a pause longer than F.
  pub loop_run: u64,
  /// The end of this shard's last round of its clients' loop I/O (their requests read, the replies to them written),
  /// one clock read a round: what judges whether a request continues a closed loop (`verbs::loop_hold`; A-326). Unlike
  /// `last_work_ns`, the shard's own background work (a destroy's slices, a lease's expiry, a kept forward's retry)
  /// never moves it, nor a client's acknowledgement after a pause, or a paced client's pause would look like a loop's.
  /// 0 before the first.
  pub last_client_io_ns: u64,
  /// Whether the round now running read a request of a client's loop or wrote a reply to one (`verbs::serve_round`
  /// stamps `last_client_io_ns` at its end when it did; A-326).
  pub loop_io: bool,
  /// Where the verb now running would reply (§4.16 "Commit"; AUD-11): the shard and ring slot the
  /// reply is written to, set by the serve path or the forward task before the verb runs and cleared
  /// after it, so a verb whose acceptance must wait for a fleet commit (`submit` at `f > 0`) can hand
  /// its reply to the merge plane to deliver once the record is placed. `None` for a verb forwarded
  /// from another node (its reply travels back on the fleet exchange that carried it).
  pub(crate) reply_route: Option<crate::merge_service::ReplyRoute>,
  /// The writers parked until every FUSE mount of their volume has been told of their change, and the replies held for
  /// them (AC-3.3; `crate::told`). Bounded by the tasks waiting and the shard's client credit.
  pub(crate) told_waiters: crate::told::ToldWaiters,
  /// How long this shard's writers waited for their volume's FUSE mounts to be told, and the back-off after overdue
  /// waits: what the told bound is learned from (AC-3.3, A-431; `crate::told::bound_ns`). Two words and an estimator.
  pub(crate) told_times: crate::told::ToldTimes,
  /// What each NFS connection's client cache holds of the files it touched, by volume (A-192; `crate::nfs_writeback`).
  /// Bounded by the metadata ledger and the listener's connections.
  pub(crate) nfs_writeback: crate::nfs_writeback::WritebackStates,
  /// What the reply of the verb `run_recorded` just ran owes its volume's FUSE mounts, when the verb has no ring route
  /// to hold its reply on: a verb forwarded from another node, whose exchange awaits it on this shard before answering
  /// (`verbs::serve_forward`; AC-3.3). Cleared by each `run_recorded`, taken by its caller.
  pub(crate) told_owed: Option<crate::told::Owed>,
  /// The completion key of the recorded verb now running — its origin (the requesting node's stable
  /// anchor) and request id — set by `run_recorded` for the verb's duration, so a verb that defers its
  /// acceptance (AUD-11) records the completion under the same key later. `None` between verbs.
  pub(crate) current_request: Option<(u64, slates_wire::request::RequestId)>,
  /// Set by a verb that deferred its reply and its completion record to a later commit (AUD-11), read
  /// and cleared by `run_recorded`: the verb's effects commit now, its completion is recorded when the
  /// deferred reply resolves, and no reply is written for it now.
  pub(crate) acceptance_deferred: bool,
  /// Verbs serving an object's latest state parked until its owner lease confirms (`crate::lease_wait`; §4.8
  /// "Leases and reads"), bounded by the shard's client credit.
  pub(crate) lease_waiters: crate::lease_wait::LeaseWaiters,
  /// A shard's parked work verbs and the answer for the one now running (A-247; `crate::work_home`).
  pub(crate) work_home: crate::work_home::WorkHome,
  /// The `ring.request` spans of requests this shard forwarded to another shard, or scattered, by request
  /// word (§4.14): opened at the slot read, ended when the reply comes back through `deliver` and is
  /// written, so a forwarded reply's ring span is timed from read to reply like a local one. Bounded by
  /// the forwards in flight — the clients' credit (`clients_per_shard × slots`, the same bound
  /// `pending_forwards` keeps); past it a span is shed and counted rather than held.
  pub forwarded_rings: BTreeMap<u64, slates_wire::observe::OpenSpan>,
  /// Derived: how many spans one `Telemetry` reply carries (`crate::telemetry::spans_per_reply`): what
  /// one bulk chunk holds past the report's fixed part.
  pub telemetry_quota: usize,
  /// When this shard's ring was last drained (boot at first), so a batch reports the window it covers.
  pub last_drain_ns: u64,
  /// The region placement the fleet has committed for each object this node owns (§4.8): the acknowledging
  /// set the control-shard membership loop recorded when it replicated the object's head record to its
  /// candidate holders and reached the quorum. The placement authority the verbs read (`region_placed`,
  /// `await_placed`) consults this — a head with a stored quorum placement is region-placed; without one it
  /// is the local append (`f = 0`, or not yet replicated). Empty on a laptop (no fleet loop runs).
  pub placed_heads: BTreeMap<ObjectId, crate::head::PlacedHead>,
  /// The fleet peers this node has established a live probe session with (§4.8): a peer is inserted the
  /// moment its probe session's handshake completes, so this is the set of peers the direct mesh has
  /// actually formed to — distinct from the membership's optimistically **seeded** alive set, which holds
  /// every configured peer from boot before any is contacted. The daemon reads it (`fleet_meshed`) to
  /// tell whether the fleet's direct mesh is up, which a formation observer must wait for rather than the
  /// seeded view. Empty on a laptop (no fleet loop runs).
  pub formed_probe_peers: std::collections::BTreeSet<slates_db::HostId>,
  /// The membership plane (A-67 H-2): the one detector and its sealed datagram plane, the peers' plane addresses, and
  /// the epochs keyed from each pair's canonical record session (`crate::member_task`). Empty on a laptop.
  pub(crate) plane: crate::member_task::PlaneState,
  /// How long this shard took to serve each NFS call it read, served here or forwarded to its owner (§4.14).
  pub nfs_service: crate::nfs::ServiceTimes,
  /// The NFSv4.1 back channels of the connections this shard serves (`crate::callback`).
  pub(crate) callbacks: crate::callback::Callbacks,
  /// The forwards to each peer, multiplexed over its record session (`crate::forward_lane`; the control shard's).
  pub(crate) forward_lanes: crate::forward_lane::ForwardLanes,
  /// The staging buffers of edits too large for one request, for the works this shard owns (`crate::staging`).
  pub(crate) staging: crate::staging::Staging,
  /// This start's random nonce, announced with its derived member id on SWIM contact (§4.8).
  /// It survives a warm restart with the complete retained Raft state; whole-anchor loss changes it.
  pub member_boot_nonce: u64,
  /// A word fresh at every start of this daemon process (keyed from the issuer secret minted at each start), announced
  /// with this node's identity so a peer tells a restarted process from a redial of this one (`member_task::Announcement`).
  pub member_process_nonce: u64,
  /// One authenticated identity per rostered certificate anchor, bounded by the roster.
  /// A different nonce retires the previous id; nonces cannot establish numeric age order.
  /// Only the control shard updates it. Empty before contact, and on a laptop.
  pub learned_members: BTreeMap<slates_db::HostId, LearnedMember>,
  /// Members this node holds dead that have since announced themselves over an authenticated record session, by the
  /// incarnation they died at: their process is alive (an anchor's warm restart keeps the member id), so contact with
  /// them is kept until they refute the death (`fleet::direct_contact`'s `Returned`). Only the control shard updates it;
  /// bounded by the fleet's peer capacity.
  pub returned_members: BTreeMap<slates_db::HostId, u64>,
  /// The record sessions each peer dialed to this node that this node serves now, by the peer's stable anchor
  /// (`fleet::serve_peer_records`, each counted by its `fleet::DialedIn` while it runs): a peer dials and keeps a session
  /// only while it keeps direct contact with this node, which the pair's canonical dialer honours
  /// (`fleet::direct_contact`'s `Dialed`, A-337). Only the control shard updates it; bounded by the roster, an anchor
  /// with no session left removed.
  pub dialed_in: BTreeMap<slates_db::HostId, u32>,
  /// Reverse index of the current authenticated identities, bounded by learned_members.
  /// Relayed alive gossip may refute a failure, but cannot revive a superseded RAM identity.
  pub authenticated_members: std::collections::BTreeSet<slates_db::HostId>,
  /// Consecutive coordinator periods each council member has been observed **dead**, while this node
  /// leads the council: the death-confirmation window before an irreversible retirement. A member seen
  /// alive again is removed (its count reset), so a transient false death — a live member briefly
  /// unreachable during a re-election or session churn — never reaches the window. Bounded by the council
  /// membership; only the leader writes it (`fleet::stable_dead_council_members`).
  pub(crate) council_death_watch: BTreeMap<slates_db::HostId, u32>,
  /// The council members other voters report holding dead (§4.8 "Promotion and takeover", the f + 1 rule): per
  /// member, per reporting voter, the incarnation it was held dead at and when the report came. The leader retires a
  /// member only once its own death watch and these reach `f + 1` voters (`fleet::confirmed_dead_council_members`).
  /// Bounded by the council's members times its voters; a report older than a membership horizon is dropped.
  pub(crate) death_reports: BTreeMap<slates_db::HostId, BTreeMap<slates_db::HostId, (u64, u64)>>,
  /// When this node's death watch first ran: a configured member never keyed since then, for a membership horizon,
  /// is this node's own death vote (`fleet::unreached_since_start`).
  pub(crate) death_watch_started_ns: Option<u64>,
  /// The control shard's bounded candidate table and enrollment authority.
  pub(crate) discovery: Option<crate::discovery::Discovery>,
  /// Authenticated candidate addresses included in the anchor publication.
  pub(crate) enrolled: Vec<crate::discovery::Announcement>,
  /// The two shard-owned fleet socket demultiplexers, by their handles (reached only while this shard runs:
  /// AUD-29-08).
  pub demuxes: Vec<slates_transport::demux::DemuxId>,
  /// The register records this node holds as a **candidate holder** for other owners' objects (§4.8
  /// "records are sent to all candidates; committed at `f + 1`"): one durable [`Acceptor`] per object
  /// this node backs, keyed by the object. A peer's record commit — served on the per-peer record socket
  /// by `crate::fleet::serve_peer_records` — is accepted into the object's acceptor and **stored here**,
  /// so the record survives the serve task (the task-local acceptor it replaced held nothing a survivor
  /// could read). It is exactly the state phase-one recovery reads on a takeover: when the owner dies, the
  /// survivor that rendezvous ranks first for the object promotes over the holders, each answering from
  /// this hold, and adopts the newest committed record (§4.8 "the new owner runs phase one … adopts the
  /// newest reported record").
  ///
  /// **One acceptor per object** (not per owner): each object has a single owner at a time, so its
  /// acceptor carries one [`slates_db::register::Authority`] — the object's current owner under the
  /// configuration generation — which a takeover re-installs to the successor with
  /// [`slates_db::register::Acceptor::install_authority`]. This keeps every acceptor within the register's
  /// "one authorized owner per generation" model (the *per-object authority* a single acceptor would need
  /// to serve several owners at once is the owed refinement noted on [`slates_db::register::Authority`]),
  /// and lets a promotion for an object route to its hold by object id regardless of which peer socket
  /// carried the message. The acceptor's authority owner is the socket's TLS-authenticated peer, so a
  /// record whose owner field is not that peer is refused. Empty on a laptop (no fleet loop runs). Tracked
  /// (`crate::tracked`), so a publication carries only the registers reached since the last (A-270).
  pub(crate) holder_records: crate::tracked::Tracked<ObjectId, Acceptor>,
  /// Live shipping (§4.10, A-200; [`crate::live_ship`]): the live volumes this shard owns and ships, and on the control
  /// shard what this node holds of others' as a candidate.
  pub(crate) live: crate::live_ship::LiveState,
  /// Test support (`Daemon::inject_seal_walk_quantum`, never set in production): the quantum a seal's turns are cut by in
  /// place of the shard's step quantum: the record period's seal walk and envelope wrap (`fleet::seal_walk_slice`), and a
  /// takeover's envelope open (`fleet::open_slice`, A-347).
  pub(crate) seal_walk_quantum_ns: Option<u64>,
  /// On the control shard: the install of the committed configuration on this holder's register copies, while its walk
  /// is owed ([`crate::fleet::InstallWalk`], A-398): the version it brings the copies to, where it stands, and whether
  /// a task walks it now. `None` once every copy has taken the installed version.
  pub(crate) install_walk: Option<crate::fleet::InstallWalk>,
  /// Test support (`Daemon::inject_install_walk_quantum`, never set in production): the quantum an install's walk over
  /// the held copies is cut by in place of the shard's step quantum (`fleet::install_slice`).
  pub(crate) install_walk_quantum_ns: Option<u64>,
  /// Test support (`Daemon::inject_live_turn_pause`, never set in production): a wait between the turns of a live
  /// takeover's rebuild on this owner shard (`fleet::rebuild_in_turns`, g251), so a test's materialization outlasts
  /// the control shard's periods and its later attempts meet it running.
  pub(crate) live_turn_pause_ns: Option<u64>,
  /// This node's takeovers in progress (§4.8 "Promotion and takeover"; [`crate::takeover`]): for each kept
  /// retirement whose survivors include this node and that it has not confirmed its share of yet, the round —
  /// its epoch, each asked member's progress, the objects learned and adopted. Only the control shard drives it;
  /// bounded by the kept retirements, each entry dropped once this node no longer owes a confirmation. Empty on
  /// a laptop (no fleet loop runs) and whenever no takeover is outstanding.
  pub host_takeovers: BTreeMap<slates_db::HostId, crate::takeover::HostTakeover>,
  /// This shard's volumes frozen for a planned move to another member (§4.10, A-291; [`crate::moves`]): each one's
  /// target and the head its move waits to see placed. Bounded by the shard's volumes; an entry leaves when its volume
  /// is released to the target, or thaws.
  pub(crate) moves_out: crate::moves::MovesOut,
  /// Where each captured clone of this shard stands in carrying its capture for its newest head (A-544,
  /// `crate::carry`). Bounded by the shard's volumes; an entry leaves once its record is in.
  pub(crate) carries: crate::carry::Carries,
  /// The adoptions of volumes the configuration moved to this node (A-291), driven on the control shard only: per
  /// volume, its round's epoch, the records its cohort reported and its seal. Bounded by the moved-object exceptions
  /// naming this node; an entry leaves once its move is adopted.
  pub(crate) move_adoptions: BTreeMap<ObjectId, crate::moves::MoveAdoption>,
  /// The newest records of each volume this node moved away and still keeps (A-291), captured from its owner's acceptor
  /// when it reported the move, by the volume's object and then by register: what it answers the new owner's phase one
  /// with. Kept on the control shard only, bounded by the shards' outgoing moves; an entry leaves with the release.
  pub(crate) move_records: BTreeMap<ObjectId, BTreeMap<ObjectId, slates_db::register::Accepted>>,
  /// Destroyed volumes this node owned under a moved-object exception, reported forgotten to the council until the
  /// configuration drops their exceptions (A-291), on the control shard only. Bounded by the exceptions naming this node.
  pub(crate) moves_forgotten: std::collections::BTreeSet<ObjectId>,
  /// Slots of volumes released to the member a move named (A-291), freed in the destroy's slices
  /// ([`crate::verbs::step_destroys`]); their records already left. Each carries a clone's pin on its origin's snapshot,
  /// given back once the slot is gone. Bounded by the shard's volumes.
  pub(crate) releasing: Vec<(slates_mem::Handle<VolumeSlot>, crate::verbs::OriginPin)>,
  /// A learner has evidence its configuration is behind the region and should refresh from a voter next
  /// period (§4.8 the reactive piggyback). Set when this node **received** a record naming a newer
  /// configuration generation than its own (the holder-behind case, [`crate::fleet::accept_held_record`]),
  /// or when a holder **refused** one of this node's records as `ConfigurationStale` naming a newer version
  /// (the stale-sender case, surfaced by `commit_record` and read in `crate::fleet::ship_head`). The
  /// record-plane coordinator fetches once when it is set (or when this node's own SWIM view diverges from
  /// its installed membership) and clears it — so an idle learner whose view matches its configuration sends
  /// nothing, replacing the per-period conditional poll. Only meaningful on the control shard (which drives
  /// and serves the council); inert elsewhere and on a laptop.
  pub config_refresh_wanted: bool,
  /// When an uninitialized council learner fetches next, and the wait after it (`fleet::next_learner_wait`).
  pub(crate) council_learner_pace: crate::fleet::LearnerPace,
  /// This node's last council configuration fetch as a learner: when it asked, how many, and the version it held after
  /// (`fleet::drive_learner_fetch`), for status.
  pub(crate) learner_fetch: crate::fleet::LearnerFetchLog,
  /// When an uninitialized root learner fetches next, and the wait after it.
  pub(crate) root_learner_pace: crate::fleet::LearnerPace,
  /// On the control shard: the newest root configuration version a peer showed this node it holds, past the one this
  /// node holds, and that peer (§4.8 Lookup, D-14 learners, A-489): a location query is bound to its asker's root view,
  /// so one bound to a newer view than this node's says this node is behind and who is not. The root learner's next
  /// period fetches from that peer and takes it (`fleet::root_learner_fetch`). One entry, the newest version seen.
  pub(crate) root_newer_seen: Option<(u64, slates_db::register::HostId)>,
  /// This node's client record sessions to its candidate holder peers (§4.8), keyed by peer host: each
  /// per-peer link task (`crate::fleet::establish_record_link`) brings its session up on one socket and
  /// installs it here as `Some`; the record-plane coordinator borrows a session for each dispatch — leaving
  /// the entry `None` while it is out — and returns it after, straggler sessions being recovered later. An
  /// absent entry means no session (never established, or lost — a borrow that ended without a return),
  /// which the link task re-establishes; a retired peer's entry is removed with it. Empty on a laptop (no
  /// fleet loop runs).
  pub record_sessions: BTreeMap<slates_db::HostId, RecordLink>,
  /// Each peer's record dial ([`RecordDial`]), as its link task last left it: read by status and by a fleet whose
  /// formation failed. One entry a peer, bounded by the fleet's peer capacity as `pairs_delivered` is.
  pub record_dials: BTreeMap<slates_db::HostId, RecordDial>,
  /// The congestion state of each session a peer dialed to this node, as its serve loop last saw it after a request,
  /// keyed by the peer's stable anchor: the connections a peer's forwards and reads ride, whose sender here is this
  /// node. One a peer, replaced on a re-dial, so bounded by the roster. Empty on a laptop.
  pub served_sessions: BTreeMap<u64, slates_ipc::protocol::SessionReport>,
  /// The waker of the discovery exchange pending on each anchor's record link, if one is — at most one per
  /// anchor (the link task drives one exchange at a time), bounded by the roster — so a peer change learned
  /// on contact or folded by the detector wakes the exchange to re-check its link at once rather than at
  /// its deadline (`crate::fleet::wake_link_waiter`). Registered by the exchange while it waits, removed by
  /// it when it ends, whatever the outcome.
  pub link_waiters: BTreeMap<slates_db::HostId, std::task::Waker>,
  /// Test support (never reachable from the wire): while set, the record serve side holds every discovery
  /// reply after its request arrived, so a peer's exchange to this node stays pending with its endpoint
  /// borrowed — the interrupted-discovery restart the replacement-voter regression forces by stopping this
  /// node while it holds (`Daemon::inject_discovery_fault`).
  pub discovery_withhold_replies: bool,
  /// Test support (never reachable from the wire): a Shards stream's replies are made, and published with their
  /// effect where they wait for a barrier, but never written, as when a daemon dies between a barrier and its reply
  /// (A-316's successor test); counted `shards.reply_withheld`.
  pub shards_withhold_replies: bool,
  /// Test support (never reachable from the wire): while set, the record serve side holds every live-state reply (a
  /// takeover's report or log fetch, `slates_cluster::live::LIVE_STATE_STREAM`) after its request arrived, while every
  /// other stream and the probe plane answer as usual: a holder alive and in contact whose report misses an asker's
  /// budget, as one retransmission on a lossy path made it (`Daemon::inject_live_state_withhold`).
  pub live_state_withhold_replies: bool,
  /// The measured **path** to each peer (§4.8 "Derived constants": "election timeout ≥ 10 × broadcast RTT
  /// p99"; `slates_cluster::timing::PathRtt`): the transport's RFC 9002 estimator over every round trip
  /// this node timed to that peer — its SWIM probe's acknowledgement each period, and every consensus
  /// round's reply, timely or late. One estimate per path, read by the probe's deadline law
  /// (`crate::fleet::ProbeTiming`), the council's and root group's election timing, and the coordinator's
  /// round budget. Bounded by the roster: an entry per rostered peer, removed with a retired id. Empty on a
  /// laptop (no peers, no fleet loop).
  pub peer_paths: BTreeMap<slates_db::HostId, slates_cluster::timing::PathRtt>,
  /// Completed probes and live uses of the measured scheduler floor (§4.8), observed through
  /// `Daemon::fleet_probe_windows`. Fixed-size counters on their owning control shard.
  pub probe_windows: crate::fleet::ProbeWindows,
  /// Test support (never reachable from the wire): peers this node hears nothing from on the probe plane — the one-way
  /// loss from each of them, which the indirect-probe and isolation regressions impose without touching the transport.
  /// Empty in production (`Daemon::inject_probe_deafness`).
  pub probe_deaf_to: std::collections::BTreeSet<slates_db::HostId>,
  /// Test support (never reachable from the wire): how long this node waits before answering each location query
  /// and each forwarded verb, as a slow or distant owner would, so a test drives a forward's total time past the
  /// client's reply deadline while each step stays inside its own bound. `None` in production
  /// (`Daemon::inject_serve_delay`).
  pub injected_serve_delay_ns: Option<u64>,
  /// Test support (never reachable from the wire): how long this node, as a content holder, holds each acknowledgement
  /// of a placement before it leaves, after the hold already took the content, so the owner's round stops collecting at
  /// its hedge delay first and folds the acknowledgement as a straggler's. A missing set or a refusal goes at once.
  /// `None` in production (`Daemon::inject_content_ack_delay`).
  pub injected_content_ack_delay_ns: Option<u64>,
  /// Test support (never reachable from the wire): a volume whose forwarded verbs this node leaves unanswered while
  /// set, the session carrying its other replies as before, as an owner alive but never answering one exchange is to
  /// its asker (`Daemon::inject_forward_silence`). `None` in production.
  pub injected_forward_silence: Option<slates_ipc::protocol::VolumeId>,
  /// Test support (never reachable from the wire): the successors whose takeover pages and questions by name this
  /// holder leaves unanswered while set, as a holder cut off from one successor alone is to it: the council never
  /// retires it, since every other member hears it (`Daemon::inject_takeover_silence`). Empty in production.
  pub injected_takeover_silence: std::collections::BTreeSet<slates_db::HostId>,
  /// Test support (never reachable from the wire): while set, this node, as a successor, sends no confirmation of a
  /// takeover share it has finished, as a confirmation late to commit leaves the share owed while other members confirm
  /// theirs (`Daemon::inject_confirmation_withheld`, A-353). `false` in production.
  pub injected_confirmation_withheld: bool,
  /// Test support (never reachable from the wire): the peer whose record session is made deaf by the forward lane's next
  /// borrow of it, once (`Daemon::inject_session_deafness`). `None` in production.
  pub injected_deaf_session: Option<slates_db::HostId>,
  /// Test support (never reachable from the wire): a peer whose record link pages no discovery while set, so a test can
  /// hold the one rescue that does not depend on a borrower (`Daemon::inject_discovery_hold`). `None` in production.
  pub injected_discovery_hold: Option<slates_db::HostId>,
  /// When each peer's record session was last dropped as failed or silent (a peer's reset, the transport's silence rule,
  /// a discovery page that failed), on the wall and in coordinator periods, until the forward lane's first reply on its
  /// replacement logs and counts the gap. One entry a peer, so bounded by the roster.
  pub dead_session_dropped_at: BTreeMap<slates_db::HostId, WallAndPeriods>,
  /// How long after the last silent session's drop the forward lane's first reply on its replacement came, on the wall
  /// and in the control shard's own periods (logged too): what a test or an operator reads to see the lane back on a live
  /// session. `None` until it has happened.
  pub dead_session_recovered: Option<WallAndPeriods>,
  /// Test support: while set, this control shard delivers no pair keys (`Daemon::inject_pair_withhold`), as a
  /// neighbour's recipient answer that has not yet arrived holds a delivery back.
  pub injected_pair_withhold: bool,
  /// Test support: the anchor whose node this control shard's owner shards deliver no pair key to
  /// (`Daemon::inject_pair_withhold_to`), every other delivery going on and nothing delivered forgotten: a delivery
  /// whose recipient answer never comes back. `None` in production.
  pub injected_pair_withhold_to: Option<u64>,
  /// Test support (never reachable from the wire): voters whose record sessions each of this node's
  /// campaigns counts as out of their links, as a discovery page holds one, for the span after the
  /// campaign's round begins (`fleet::take_campaign_sessions`); so a test drives a campaign into sessions
  /// that are out for a moment. `None` in production (`Daemon::inject_campaign_session_hold`).
  pub campaign_session_hold: Option<(std::collections::BTreeSet<slates_db::HostId>, u64)>,
  /// Test support (never reachable from the wire): owners whose register records this holder refuses, as a
  /// holder that never received them would — no acknowledgement, counted — while the owner's council and
  /// report traffic on the same session still flows (`fleet::accept_held_record`). Empty in production
  /// (`Daemon::inject_record_refusal`).
  pub record_refused_from: std::collections::BTreeSet<slates_db::HostId>,
  /// This node's owner-lease evidence (§4.8 "Leases and reads"; AUD-08): each peer's latest direct
  /// acknowledgement that reported this node alive under the installed configuration version, and any
  /// newer version a peer announced. Written on the control shard by the probe tasks
  /// (`fleet::probe_and_apply`), fanned to every shard each period with the configuration
  /// (`fleet::fan_configs_to_shards`), read per request by the verbs (`verbs::dispatch`) and the mount
  /// bridge (`crate::nfs`) against the host clock. Bounded by the members; empty on a laptop, where
  /// `f = 0` needs no confirmation.
  pub lease: crate::lease::OwnerLease,
  /// What the control shard last delivered to each other shard's configuration fan (AUD-29-29): a period
  /// fans a shard only the parts that changed since, and records a delivery only once its bounded channel
  /// accepted it, so a refused fan is sent again the next period. Bounded by the shards.
  pub(crate) fanned: crate::fleet::Fanned,
  /// The host memory available at this daemon's first pressure sample (§4.2; admission.md §5.5): the
  /// baseline its pressure hold measures a shortfall against, kept by the shard that samples. Per daemon,
  /// never process-wide: several daemons in one process (the fleet tests) each measure from their own
  /// start, so one daemon's growth is not another's pressure.
  pub(crate) pressure_baseline: Option<u64>,
  /// A pressure hold a test injected (`Daemon::inject_pressure_hold`), which the sampler then leaves in
  /// place, so a test that drives the mechanism is not raced by the host's sample.
  pub(crate) pressure_pinned: bool,
  /// A host memory reading a test injected (`Daemon::inject_available_memory`), which the sampler uses in
  /// place of the platform's, so a test of the pressure arithmetic is not at the mercy of the host.
  pub(crate) injected_available: Option<u64>,
  /// This process's resident memory a test injected (`Daemon::inject_resident_memory`), which the sampler uses in
  /// place of the platform's reading.
  pub(crate) injected_resident: Option<u64>,
  /// The process's resident memory at the pressure sampler's first sample (§4.2): its growth since then is the
  /// daemon's own use, subtracted from the host's shortfall so only pressure from elsewhere is held back.
  pub(crate) pressure_resident_baseline: Option<u64>,
  /// The pressure hold and debt the sampler last sent each shard, in shard order (§4.2; kept on the control shard, the
  /// one sampler): they are sent only when they change, so a daemon at rest sends none. Every shard's hold and debt
  /// start at zero at boot, and so does this. Bounded by the shards.
  pub(crate) pressure_sent: Vec<(u64, u64)>,
  /// The process thread's wait for a reason to stop (`Daemon::wait_for_stop`), asked at each heartbeat on the control
  /// shard; `None` until a process installs it (and on every other shard).
  pub(crate) stop_watch: Option<crate::daemon::StopWatch>,
  /// Test support: this shard refuses every NFSv4 purge sent to it (`Daemon::inject_nfs_purge_refusal`), as an owner
  /// that cannot make the drop durable does, so a test can leave a purge owed across a restart.
  pub(crate) nfs_purges_refused: bool,
  /// The holder side of the owner lease (§4.8; AUD-08): when this node last answered each peer's direct
  /// probe reporting it alive, and the newest configuration version each announced — the evidence that
  /// gates a successor's promotion of a departed owner's objects at this holder
  /// (`takeover::serve_host_prepare`). Only the control shard writes it; bounded by the members.
  pub answers_given: crate::lease::AnswersGiven,
  /// The held objects whose owner a committed configuration retired and whose successor has not yet
  /// re-committed them here: the departed owner and the version that retired it, so the promotion gate knows
  /// whom this node answered and what that owner must have seen. Kept at **every** holder of such an object,
  /// not only its successor, because every promising holder applies the gate (`takeover::serve_host_prepare`),
  /// and it names the retired host whose takeover round lists the object. Set at the configuration install
  /// (`takeover::resolve_held_objects`), removed when this holder accepts a record of the object from its new
  /// owner (`fleet::accept_held_record`), when the successor adopts it (`takeover::record_adoption`), or when a
  /// stale copy is reclaimed. Bounded by
  /// the held objects; only the control shard touches it.
  pub departed_owners: BTreeMap<ObjectId, crate::lease::DepartedOwner>,
  /// The configuration council's election timing as derived this period from the paths to its other
  /// voters (the floor with none measured): the base and span in coordinator periods, the tail and spread
  /// they came from, and the samples behind them — what `Daemon::council_timing` reports. Only the control
  /// shard derives it; a laptop's stays at the floor.
  pub council_timing: slates_cluster::timing::ElectionTiming,
  /// The root group's election timing, derived as the council's is over the root voters' paths.
  pub root_timing: slates_cluster::timing::ElectionTiming,
  /// The failover delay as this node estimates it (§4.8 "Derived constants", A-363): the coordinator's measured period,
  /// the takeovers and volumes it timed, and the detection and confirmation terms the last period derived from the
  /// detector and the council's timing. Live on the control shard, which runs the coordinator; a laptop's, and every
  /// other shard's, is the recovery prior alone.
  pub failover: slates_cluster::failover::Failover,
  /// The failover delay the waits that bound a failover read on this shard (A-363): a forward's bound, the location
  /// round's cap, a held reply's total, a takeover's attempts. The control shard's estimate in whole coordinator periods,
  /// set by its refresh each period and fanned to every other shard (`fleet::Fan`) when it moves; the recovery prior
  /// until the first fan. Nothing sets it but the estimate (or a test's injection, [`ShardState::injected_failover_ns`]).
  pub failover_ns: u64,
  /// Test support: while `Some`, the control shard publishes this as its failover delay in place of its estimate, so a
  /// test can hold a wait's bound below a path's floor, or short enough to watch a give-up (`Daemon::inject_failover`).
  /// `None` in every deployment.
  pub injected_failover_ns: Option<u64>,
  /// Each CLI attachment's renewals as this, its owner shard, has seen them (A-384): when it was last used and the gaps
  /// between its uses, which its term follows (`verbs::attachment_term`). In memory only: a restart starts each again,
  /// with a fresh term. Bounded by the partition's attachments: an entry leaves when its attachment ends.
  pub attachment_paces: BTreeMap<u64, AttachmentPace>,
  /// What this node holds as a **content candidate** for other owners' snapshots (§4.10 "Content
  /// replication"): the distinct chunks by identity and each manifest held whole, verified before
  /// anything is stored — served on the record session's content stream by the fleet loop. It is what
  /// a takeover successor materializes a taken-over volume from, and what a reader fetches by identity.
  /// Empty on a laptop (no fleet loop runs).
  pub held_content: slates_cluster::content::ContentHold,
  /// The seals in progress for volumes this node owns (§4.10), by object: each walks the volume's newest
  /// snapshot into its archive in bounded slices, puts the archive to the content candidates until
  /// `f + 1` hold it, and is dropped once the head naming it places and the snapshot is recorded placed.
  /// Empty on a laptop and whenever every owned snapshot is placed.
  pub seals: BTreeMap<ObjectId, crate::head::SealJob>,
  /// The mirror shipments in progress for volumes this node owns (§4.10 "Mirroring across regions";
  /// `docs/wip/mirroring.md`), by object: a placed head snapshot's archive put to the owner's neighbourhood in the
  /// mirror region until `f + 1` there hold it, then recorded (`SnapshotPlaced { mirror }`) and dropped. One per
  /// object, the newest placed snapshot superseding an older one still shipping. Empty with no mirror declared.
  pub mirror_seals: BTreeMap<ObjectId, crate::head::SealJob>,
  /// This node's neighbourhood in its region's declared mirror region (`slates_db::mirror::mirror_neighbourhood`),
  /// recomputed once a record period from the mirror region's members held alive: the hosts mirror shipments go to.
  /// Direct contact keeps them already: a member of another region is never in this region's council configuration.
  /// Empty with no mirror declared or none of its members alive.
  pub mirror_neighbourhood: Vec<slates_db::HostId>,
  /// The records this node holds as a mirror holder for owners in the region it mirrors (`crate::mirror`): each
  /// object's newest head and catalog record, kept on the control shard and published in its held image before
  /// a shipment is acknowledged. What a promotion's phase one reads. Empty outside a mirror region.
  pub mirror_records: slates_db::mirror::MirrorRecords,
  /// The promotion rounds this node leads as its council's leader, by lost region (`crate::mirror`). Bounded by the
  /// regions promoted to this one.
  pub(crate) mirror_rounds: BTreeMap<slates_db::register::RegionId, crate::mirror::PromotionRound>,
  /// The promoted objects assigned to this node that it has seeded into its materialization queue, until their
  /// assignment is dropped (`crate::mirror`). Bounded by the council's adoptions.
  pub(crate) mirror_seeded: std::collections::BTreeSet<ObjectId>,
  /// The measured put latency of this node's **content class** (§4.8 "Derived constants": "hedge delay =
  /// measured p95 put latency per class"): one reading per binding content acknowledgement this owner
  /// shard has collected — the time from the round's dispatch to that holder's verified acknowledgement.
  /// Its p95 is the hedge trigger: how long the first content round to `f + 1` candidates is given before
  /// the remaining candidates are hedged. Bounded to a window of the newest readings; empty until the
  /// first content acknowledgement, and on a laptop, where no content round runs — the trigger is then
  /// one period (R8, the same code with an empty window).
  pub put_latency: crate::fleet::LatencyWindow,
  /// The measured put latency of content rounds to the mirror region (`fleet::content_work`'s mirror track): kept
  /// apart from [`Self::put_latency`], because a home round's p95 on a local network is far inside one round trip to
  /// another region, and a mirror round hedged and judged stalled at it never got past its offer.
  pub mirror_put_latency: crate::fleet::LatencyWindow,
  /// The measured fetch latency of the content class on this shard (A-91): each chunk's time from its first
  /// request to its verified arrival, whose p95 is the hedge trigger for a fetch's chunks. Empty until the first
  /// fetch, and on a laptop, where none runs — the trigger is then one period (R8).
  pub fetch_latency: crate::fleet::LatencyWindow,
  /// The measured **put-failure rate** of this owner shard's content class (§4.8 "Derived constants":
  /// "healer cadence from the measured put-failure rate"): how many content rounds ended placed and how
  /// many ended short (uncertain at the deadline, or every holder answering short of quorum), over the
  /// shard's lifetime. The healer's cadence is derived from their ratio (`crate::fleet::heal_period_ns`):
  /// a shard whose puts fail often walks its placed content sooner. Both zero on a laptop.
  pub put_outcomes: crate::fleet::PutOutcomes,
  /// The healer's position (§4.10 "anti-entropy … the healer"): which owned volume's placed snapshot it
  /// re-offers next, in id order, and when it last did — one snapshot per healer period, a bounded slice
  /// of the walk over everything this node has placed. Idle on a laptop (nothing is placed remotely).
  pub healer: crate::fleet::HealerCursor,
  /// How many placed snapshots the healer has **repaired** — re-put to a recorded holder that answered an
  /// offer with chunks it lacked — the non-vacuity counter a test reads (`fleet_repairs`): a healer that
  /// walks but never repairs anything cannot masquerade as working.
  pub repairs: u64,
  /// Taken-over objects whose head this node adopted and placed but whose **content** it has not yet
  /// materialized into a served volume (§4.10 "Promotion and takeover" → serve): the adopted head
  /// value, kept until the manifest's archive is held (already, as a content candidate, or fetched
  /// from a recorded holder) and the volume is created under its original id, with the attempts made, which
  /// `fleet::materialize_pending` bounds. Empty once every takeover serves or is given up on.
  pub pending_materializations: BTreeMap<ObjectId, PendingVolume>,
  /// On a taken-over volume's owner shard, its materialization made in turns of this shard's clock by a task of its own
  /// (g251, `fleet::make_taken`; a live log's or a placed seal's): running, with the turns it has taken, or ended, until
  /// the control shard's next attempt reads the end and forgets it. A later attempt joins a running one and never
  /// gathers or makes the volume again. At most one a volume this shard is making, which the takeover's pending
  /// materializations bound.
  pub making_taken_over: BTreeMap<ObjectId, Making>,
  /// On a taken-over volume's owner shard, the volume its materialization is making (g251, `verbs::MakingVolume`):
  /// created and admitted, its restored tree laid in a piece a turn, until it is recorded and published or discarded
  /// with its credits given back. Held here between turns, not by the task alone, so a refusal or a give-up leaves
  /// nothing behind. At most one a volume in `making_taken_over`.
  pub making_volumes: BTreeMap<slates_db::catalog::VolumeId, crate::verbs::MakingVolume>,
  /// The catalog register this node adopted for each taken-over volume (keyed by the volume's object), with
  /// the adopted record's sequence (AUD-29-17): a pending materialization waits for it, and the successor's
  /// record continues the register from that sequence. Bounded by the objects this node owns.
  pub pending_catalogs: BTreeMap<ObjectId, (u64, crate::catalog::CatalogValue)>,
  /// Taken-over **greens** whose newest merge record this node adopted, awaiting materialization into
  /// an owned green on the shard the id routes to (§4.16 owner-loss recovery; AUD-14): the catalog
  /// record, the origin, the chain and the engine rebuilt from this node's own accepted records and
  /// held inputs. Each carries the attempts made, which `fleet::materialize_pending_greens` bounds. Bounded by the
  /// objects this node owns; empty on a laptop.
  pub pending_green_materializations: BTreeMap<ObjectId, PendingGreen>,
  /// Taken-over volumes whose register this node adopted as a **tombstone** (AUD-29-43), each with the tombstone's
  /// sequence and the attempts to record it: awaiting `Op::TombstoneAdopted` on the shard the id routes to, from where
  /// the record plane ships the tombstone's remaining stages. Never materialized. One given up on past the failover
  /// SLO's attempts stays, marked, so the volume is still known destroyed here and its catalog never rebuilt
  /// (`fleet::adopt_pending_tombstones`). Bounded by the objects this node owns. A work whose route this node retired
  /// joins them (A-288, `crate::work_route`).
  pub pending_tombstones: BTreeMap<ObjectId, PendingTombstone>,
  /// On the control shard: the work route keeper's nudge and waker (A-288, `crate::work_route`).
  pub(crate) work_routes: crate::work_route::WorkRoutes,
}

/// A work volume's accumulated declared operations (§4.16), composed into an increment on submit.
pub struct WorkState {
  /// The green this work is over.
  pub green: VolumeId,
  /// The green version the work is based on.
  pub base_version: u64,
  /// The declared operations, in order.
  pub journal: Vec<VolumeOp>,
  /// The work's files as a delta over its base (`crate::work_files`): the files it changed since `base_version`,
  /// every other file the green's there — the post-state the increment's content ops name by range (a mounted work
  /// would keep this in its VFS tree; here `edit` and `declare` maintain it directly).
  pub files: crate::work_files::WorkFiles,
  /// Bumped by every change to `content` (an edit, a namespace declaration, a submit or rebase moving the work):
  /// the stamp a paged read of the work's head compares between pages (`ReadRange`).
  pub revision: u64,
  /// The bytes this work is charged against the shard's budget: its content's and journal's footprint
  /// (`crate::work_charge`), released when it shrinks, resets or is destroyed.
  pub charged: u64,
  /// The base entries of the paths the work fetched (A-247), each as `Green::path_base` gave it at the work's base
  /// version, so the work composes its increment off its green's shard (`Base::for_names`). Cleared with the base;
  /// charged with the work.
  pub entries: std::collections::BTreeMap<String, slates_merge::increment::Base>,
  /// The work's last conflicting verdict (§4.16, A-267), kept for `Conflicts` until its next submit or rebase replaces
  /// it, and cleared when one succeeds. Its windows are conflicts of the work's own declared operations, so their
  /// number is bounded by the journal the merge budget charges; a restart forgets it, and a read of it is then refused
  /// `VerdictReplaced`.
  pub verdict: Option<KeptVerdict>,
}

/// A work's kept conflicting verdict (§4.16, A-267): its id, which the `Submitted` or `Rebased` reply named, and every
/// window of it, in the verdict's order.
#[derive(Clone, Debug)]
pub struct KeptVerdict {
  /// The verdict's id: the shard's monotonic clock when it was judged, and past the work's last.
  pub id: u64,
  /// Every window of the verdict.
  pub windows: Vec<slates_ipc::protocol::MergeWindow>,
}

/// A reply waiting to be written into a client's ring.
#[derive(Clone, Debug)]
pub struct Deferred {
  /// The client's slot index.
  pub client_index: u32,
  /// The request word.
  pub request: u64,
  /// The reply.
  pub reply: ReplyBody,
  /// Whether its completion record exists already (the owner partition recorded it).
  pub recorded: bool,
  /// The request's open `ring.request` span (§4.14), opened at the slot read and ended when this
  /// deferred reply is finally written — a reply deferred by a full ring, or one that came back from
  /// another shard, is timed from read to reply like a synchronous one. `None` only for a reply whose
  /// ring span was shed at the forwarding bound (counted as loss) or never opened (a forward that
  /// never reached its owner after the bound).
  pub ring: Option<slates_wire::observe::OpenSpan>,
}

/// A forward waiting for room on the owner shard's control channel.
pub struct PendingForward {
  /// The client's slot index.
  pub client_index: u32,
  /// The request word.
  pub request: u64,
  /// The client id.
  pub client_id: u32,
  /// The principal.
  pub principal: Principal,
  /// The body.
  pub body: slates_ipc::protocol::RequestBody,
  /// The owner shard (the runtime's id).
  pub owner: u16,
  /// The origin's `ring.request` span context (§4.14), carried to the owner so the verb's `shard.op`
  /// opens within it — one trace across the shard boundary, its cause named.
  pub cause: Option<slates_wire::observe::SpanContext>,
  /// The idle window the forward holds its owner open for, 0 for none (`ShardState::forward_hold_ns`; A-326).
  pub hold_ns: u64,
}

impl std::fmt::Debug for PendingForward {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PendingForward")
      .field("client_id", &self.client_id)
      .field("owner", &self.owner)
      .finish()
  }
}

impl ShardState {
  /// Adds `by` to the count of refusal or event `kind`, saturating (a count never wraps to a small number). Adding zero
  /// changes nothing: an entry stands in the status report's counter map only once something happened (2026-10-10: a
  /// boot's zero restores and an idle tick's zero purges stood there as events, and a formation check that reads the
  /// map failed on them).
  pub(crate) fn count(&mut self, kind: &'static str, by: u64) {
    if by == 0 {
      return;
    }
    let count = self.refusals.entry(kind).or_insert(0);
    *count = count.saturating_add(by);
  }
}

impl std::fmt::Debug for ShardState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardState")
      .field("shard", &self.shard)
      .field("volumes", &self.by_id.len())
      .field("clients", &self.clients.iter().count())
      .finish()
  }
}

thread_local! {
  static STATE: RefCell<Option<ShardState>> = const { RefCell::new(None) };
  /// The control shard's set of live client ids (handed out and not yet reclaimed), so a
  /// wanted id that is live is not given twice; bounded by the daemon's client capacity.
  static HANDED: RefCell<std::collections::BTreeSet<u32>> =
    const { RefCell::new(std::collections::BTreeSet::new()) };
}

/// Borrows the control shard's set of live client ids (on the calling thread).
pub fn with_handed<R>(f: impl FnOnce(&mut std::collections::BTreeSet<u32>) -> R) -> R {
  HANDED.with(|cell| f(&mut cell.borrow_mut()))
}

/// Installs the state on the calling shard thread.
pub fn install(state: ShardState) {
  STATE.with(|cell| *cell.borrow_mut() = Some(state));
}

/// Takes the state off the calling shard thread (shutdown).
pub fn take() -> Option<ShardState> {
  STATE.with(|cell| cell.borrow_mut().take())
}

/// Why a borrow of the shard's state was refused ([`try_with_state`]): the four ways the state can be
/// out of reach, kept apart because an observer must tell a shard whose state is gone from one that
/// is merely busy or that answered and then failed its retention check (§4.14; `crate::observe`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateAccess {
  /// The state is already borrowed on this thread: a nested borrow, a bug the caller counts.
  Borrowed,
  /// This thread has no state: it runs no shard, or its shard's state was taken (shutdown).
  Absent,
  /// The shard is fenced: consensus recovery failed and it serves nothing (`consensus_failure`).
  Fenced,
  /// The borrow ran to its end, but the retention check every borrow closes with refused, so the
  /// borrow's result was discarded; the check's own refusal.
  Retention(slates_anchor::AnchorError),
}

impl std::fmt::Display for StateAccess {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      StateAccess::Borrowed => f.write_str("already borrowed on this thread (a nested borrow)"),
      StateAccess::Absent => f.write_str("absent (this thread holds no shard state)"),
      StateAccess::Fenced => f.write_str("fenced (consensus recovery failed on this shard)"),
      StateAccess::Retention(refusal) => {
        write!(
          f,
          "discarded by the retention check after the borrow: {refusal}"
        )
      }
    }
  }
}

/// Borrows the state, naming the refusal when it cannot: the calling thread has no state, the state is
/// already borrowed (a nested borrow is a bug, counted by the caller), the shard is fenced, or the
/// retention check that closes every borrow refused (the borrow ran; its result is discarded).
pub fn try_with_state<R>(f: impl FnOnce(&mut ShardState) -> R) -> Result<R, StateAccess> {
  STATE.with(|cell| {
    let mut guard = cell.try_borrow_mut().map_err(|_| StateAccess::Borrowed)?;
    let state = guard.as_mut().ok_or(StateAccess::Absent)?;
    if state.consensus_failure.is_some() {
      return Err(StateAccess::Fenced);
    }
    let result = f(state);
    crate::retention::retain(state).map_err(StateAccess::Retention)?;
    Ok(result)
  })
}

/// Borrows the state for the shutdown's release of what the process holds in the kernel (a FUSE mount, a Shards
/// stream, a WinFsp mount), whether or not the shard is fenced (§4.6 "Linux"; AUD-29-64): a fenced shard writes no
/// record, but its mounts and streams must not outlive it. Nothing may write a record through this borrow; the records
/// of what it releases end at the next start's recovery. `None` when the thread holds no state or it is borrowed.
pub(crate) fn with_state_at_shutdown<R>(f: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
  STATE.with(|cell| {
    let mut guard = cell.try_borrow_mut().ok()?;
    guard.as_mut().map(f)
  })
}

/// Borrows the state; `None` for any refusal [`try_with_state`] names — the form for a caller that
/// acts the same whichever it was.
pub fn with_state<R>(f: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
  try_with_state(f).ok()
}

std::thread_local! {
  /// Borrows on this shard thread refused in a way that lost a step ([`with_state_counted`]): a nested borrow (the
  /// closure never ran) and a retention check refused after the closure ran (its effect stands, its result was
  /// discarded). Per thread because each shard reports its own (a process-wide count would be summed once per shard).
  static LOST_STEPS: std::cell::Cell<LostSteps> = const { std::cell::Cell::new(LostSteps { borrowed: 0, retention: 0 }) };
}

/// The borrows this shard thread refused in a way that lost a step, by kind ([`with_state_counted`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LostSteps {
  /// Nested borrows: the closure never ran.
  pub borrowed: u64,
  /// Retention checks refused after the closure ran.
  pub retention: u64,
}

/// [`with_state`] for a caller with nothing to do on a refusal (a counter, a cleanup, a drain), which before
/// 2026-10-07 discarded it at 57 sites: the refusals that lose a step are counted on this thread ([`lost_steps`],
/// reported in status as `state.borrow_refused` and `state.retention_refused`). A thread with no state, or a fenced
/// shard, is not a lost step: there is nothing to do there.
pub fn with_state_counted<R>(f: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
  let refusal = match try_with_state(f) {
    Ok(result) => return Some(result),
    Err(refusal) => refusal,
  };
  LOST_STEPS.with(|cell| {
    let mut lost = cell.get();
    match refusal {
      StateAccess::Borrowed => lost.borrowed = lost.borrowed.saturating_add(1),
      StateAccess::Retention(_) => lost.retention = lost.retention.saturating_add(1),
      StateAccess::Absent | StateAccess::Fenced => {}
    }
    cell.set(lost);
  });
  None
}

/// The borrows this shard thread refused in a way that lost a step, so far.
pub fn lost_steps() -> LostSteps {
  LOST_STEPS.with(std::cell::Cell::get)
}

/// Whether any client ring on this thread holds a request (the poller's question).
pub fn any_ring_ready() -> bool {
  with_state(|s| ring_ready_in(s)).unwrap_or(false)
}

/// [`any_ring_ready`] over a state already borrowed: a live client's command ring holds a request, or
/// deferred work waits — what the serve loop re-checks after announcing idle (`verbs::announce_idle`).
pub fn ring_ready_in(s: &ShardState) -> bool {
  s.clients.iter().any(|(_, c)| {
    !c.retiring
      && c
        .end
        .region()
        .cmd()
        .depth(c.end.region().object())
        .is_ok_and(|d| d > 0)
  }) || !s.deferred.is_empty()
}

/// Counter: a reply [`deliver`] dropped because its client's slot was gone, retiring or reused by another client.
pub(crate) const REPLY_UNDELIVERABLE: &str = "reply.undeliverable";

/// A reply that came back from another shard (or a scatter's part): queued for the client's
/// ring and the server task woken; dropped and counted ([`REPLY_UNDELIVERABLE`]) when its client is gone.
pub fn deliver(client_index: u32, request: u64, reply: ReplyBody, recorded: bool) {
  let server = with_state(|s| {
    // The origin's `ring.request` span (§4.14), opened at the slot read and kept while the request was
    // away on another shard; it ends when this reply is written into the client's ring.
    let ring = s.forwarded_rings.remove(&request);
    let recipient = s
      .clients
      .generation_at(client_index)
      .and_then(|generation| {
        s.clients
          .get(slates_mem::Handle::from_raw(client_index, generation))
          .ok()
      });
    if !recipient.is_some_and(|client| {
      !client.retiring
        && client.client_id == slates_wire::request::RequestId::from_word(request).client
    }) {
      // Its client is gone or its slot reused: nobody can take the reply. Counted, since a client still waiting would
      // otherwise wait with nothing to show where its reply went.
      s.count(REPLY_UNDELIVERABLE, 1);
      return None;
    }
    s.deferred.push(Deferred {
      client_index,
      request,
      reply,
      recorded,
      ring,
    });
    s.server_task
  })
  .flatten();
  if let Some(task) = server {
    slates_rt::registry::wake(task.0);
  }
}

/// A taken-over green awaiting materialization (§4.16 owner-loss recovery; `ShardState::pending_green_materializations`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingGreen {
  /// Its newest merge record, as adopted.
  pub merge: crate::merge_service::MergeRecordValue,
  /// The retired owner whose takeover round adopted it, `None` for one no round adopted. While that round still pages,
  /// a wait for the green's catalog register counts against the bound only when the round has not advanced since the
  /// previous attempt: the register may come on a later page (`takeover::rounds_still_paging`, [`round_advanced`]).
  pub departed: Option<slates_db::register::HostId>,
  /// The survivors other than this node that reported the adopted version in the takeover's round: each accepted it,
  /// and a holder accepts a merge record only once it holds the inputs it names (A-244), so these hold every input
  /// this node may lack (`fleet::fetch_green_inputs`). Bounded by the fleet's members.
  pub sources: Vec<slates_db::register::HostId>,
  /// The materializations attempted since the last that made progress, one a fleet period: past the failover SLO's
  /// worth the green is given up on, every verb on it answering `Refusal::NotRecovered`
  /// (`fleet::materialize_pending_greens`).
  pub attempts: u32,
  /// The fewest of its inputs found not held here so far (`fleet::fetch_green_inputs`), `None` before the first look.
  /// Progress is a new low: only then do the attempts start again, so an input fetched, released and fetched again
  /// cannot hold the green pending without end; the attempts start again at most once per input the ledger names.
  pub fewest_unheld: Option<usize>,
  /// The page advances of its still-paging takeover round at the last attempt that waited on its catalog register,
  /// `None` before the first ([`PendingGreen::record_round`]).
  pub round_seen: Option<u64>,
}

impl PendingGreen {
  /// A green just adopted with its newest merge record `merge` by `departed`'s takeover round, whose version `sources`
  /// reported: no attempt made yet.
  pub fn adopted(
    merge: crate::merge_service::MergeRecordValue,
    departed: Option<slates_db::register::HostId>,
    sources: Vec<slates_db::register::HostId>,
  ) -> PendingGreen {
    PendingGreen {
      merge,
      departed,
      sources,
      attempts: 0,
      fewest_unheld: None,
      round_seen: None,
    }
  }

  /// Records that `unheld` of its inputs are still not held here after an attempt's fetches; whether that is progress
  /// (a new low, after a first look), which starts its attempts again.
  pub fn record_unheld(&mut self, unheld: usize) -> bool {
    let progress = self.fewest_unheld.is_some_and(|fewest| unheld < fewest);
    if progress {
      self.attempts = 0;
    }
    if self.fewest_unheld.is_none_or(|fewest| unheld < fewest) {
      self.fewest_unheld = Some(unheld);
    }
    progress
  }

  /// Records, after an attempt that waited on its catalog register, that the still-paging takeover round which adopted
  /// it has advanced `advances` pages so far; whether the round advanced since the previous such attempt (after a
  /// first), which is progress and starts its attempts again ([`round_advanced`]).
  pub fn record_round(&mut self, advances: u64) -> bool {
    let progress = round_advanced(&mut self.round_seen, advances);
    if progress {
      self.attempts = 0;
    }
    progress
  }
}

/// Where a taken-over volume's materialization in turns stands on its owner shard (`ShardState::making_taken_over`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Making {
  /// Its task runs: the turns it has taken so far.
  Running {
    /// The turns taken, one a slice of its owner shard's clock: what tells a materialization that moves from one stuck.
    turns: u64,
  },
  /// Its task made the volume and published it.
  Made,
  /// Its task ended refused, named by the refusal's counter.
  Refused(&'static str),
}

/// A taken-over volume's tombstone awaiting its record on the shard the volume's id routes to (§4.4 destroy, §4.10
/// status AUD-29-43; `ShardState::pending_tombstones`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingTombstone {
  /// The tombstone's sequence: one past every sequence the volume's registers used.
  pub sequence: u64,
  /// The attempts to record it so far, one a fleet period (`fleet::adopt_pending_tombstones`).
  pub attempts: u32,
  /// Whether it was given up on: past the failover SLO's attempts it is attempted no more, and stays only so that the
  /// volume is still known destroyed here (`takeover::record_adoption` never rebuilds its catalog).
  pub given_up: bool,
}

impl PendingTombstone {
  /// A tombstone just adopted at `sequence`: no attempt made yet.
  pub fn adopted(sequence: u64) -> PendingTombstone {
    PendingTombstone {
      sequence,
      attempts: 0,
      given_up: false,
    }
  }

  /// Counts an attempt that did not record it against `bound` (`fleet::takeover_attempts`); whether this attempt gives
  /// it up. One already given up counts nothing more.
  pub fn count_attempt(&mut self, bound: u32) -> bool {
    if self.given_up {
      return false;
    }
    self.attempts = self.attempts.saturating_add(1);
    self.given_up = self.attempts >= bound;
    self.given_up
  }
}

/// A CLI attachment's pace (A-384): when it was last used, and the running estimate of the gaps between its uses
/// (RFC 6298's estimator, as the failover estimate's terms are), so its term is its holder's own.
#[derive(Debug, Default)]
pub struct AttachmentPace {
  /// The shard's clock at its last use, or at its making.
  pub last_use_ns: u64,
  /// The gaps between its uses.
  pub gaps: slates_cluster::failover::RunningDuration,
}

impl AttachmentPace {
  /// A pace begun by a use at `now_ns`, no gap measured yet.
  pub fn begun(now_ns: u64) -> AttachmentPace {
    AttachmentPace {
      last_use_ns: now_ns,
      gaps: slates_cluster::failover::RunningDuration::new(),
    }
  }

  /// A use at `now_ns`: the gap since the last is a sample. A clock that read earlier is no sample.
  pub fn used(&mut self, now_ns: u64) {
    if let Some(gap) = now_ns.checked_sub(self.last_use_ns) {
      self.gaps.on_sample(gap);
    }
    self.last_use_ns = now_ns;
  }
}

/// A taken-over volume awaiting materialization (§4.8 "Promotion and takeover", §4.10;
/// `ShardState::pending_materializations`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingVolume {
  /// Its adopted head.
  pub head: crate::head::HeadValue,
  /// The retired owner whose takeover round adopted it, `None` for one no round adopted (a mirror's promotion seeds
  /// its catalog with its head). While that round still pages, a wait for the volume's catalog register or that
  /// register's placement counts against the bound only when the round has not advanced since the previous attempt:
  /// the register may come on a later page (`takeover::rounds_still_paging`, [`round_advanced`]).
  pub departed: Option<slates_db::register::HostId>,
  /// The materializations attempted since the last that made progress, one a fleet period: past the failover SLO's
  /// worth the volume is given up on, every verb on it answering `Refusal::NotRecovered`
  /// (`fleet::materialize_pending`).
  pub attempts: u32,
  /// The most chunks of its head's content found staged here after an attempt's fetch so far, `None` before the first
  /// look. Progress is a new high: only then do the attempts start again, so a fetch cut by its deadline each period
  /// is given up only once it stops advancing, and the count starts again at most once per chunk the content names.
  pub most_staged: Option<usize>,
  /// The page advances of its still-paging takeover round at the last attempt that waited on its catalog register,
  /// `None` before the first ([`PendingVolume::record_round`]).
  pub round_seen: Option<u64>,
  /// The hosts the retired owner placed this volume on, the members of its cohorts in the takeover that adopted it,
  /// settled and current (A-309): where its live log was shipped besides the recorded holders, so a live takeover past
  /// tolerance asks every surviving one of them (`live_ship::fetch`). Taken from the configuration when the volume is
  /// adopted, since the retirement may complete and be let go before the log is fetched. Empty for a volume no round
  /// adopted (a mirror's promotion).
  pub cohort_hosts: Vec<slates_db::register::HostId>,
  /// Set when its head register was adopted past tolerance (A-309, A-332): the first cohort short of its recovery quorum,
  /// which the volume's loss report names once it is served (`slates_db::catalog::TakeoverPastTolerance::registers`).
  /// `None` within tolerance and for a volume no round adopted.
  pub registers_short: Option<slates_db::catalog::RecoveryShortfall>,
  /// When it was adopted, on the shard's clock (zero off a shard): its recovery is timed from here to its service, into
  /// the failover estimate (A-363).
  pub adopted_ns: u64,
  /// The turns its owner shard's materialization had taken at the last attempt that found it running, `None` before
  /// the first ([`PendingVolume::record_turns`]).
  pub turns_seen: Option<u64>,
  /// The head and sealing its live log was fetched at by the attempt that began its materialization (g251), for the
  /// end's note of a refused rebuild (`live_ship::note_rebuild`); `None` before a live materialization began.
  pub live_adopted: Option<(slates_db::live::Head, Option<crate::head::HeadSealing>)>,
}

impl PendingVolume {
  /// A volume just adopted with its head `head` by `departed`'s takeover round, the retired owner having placed it on
  /// `cohort_hosts`: no attempt made yet, adopted now.
  pub fn adopted(
    head: crate::head::HeadValue,
    departed: Option<slates_db::register::HostId>,
    cohort_hosts: Vec<slates_db::register::HostId>,
  ) -> PendingVolume {
    PendingVolume {
      head,
      departed,
      attempts: 0,
      most_staged: None,
      round_seen: None,
      cohort_hosts,
      registers_short: None,
      adopted_ns: slates_rt::futures::now_ns(),
      turns_seen: None,
      live_adopted: None,
    }
  }

  /// Records, after an attempt that found its owner shard's materialization running (g251), that it has taken `turns`
  /// turns so far; whether it took more since the previous such attempt (after a first), which is progress and starts
  /// its attempts again, so a long volume's materialization is never given up on while it moves.
  pub fn record_turns(&mut self, turns: u64) -> bool {
    let progress = self.turns_seen.is_some_and(|seen| turns > seen);
    if progress {
      self.attempts = 0;
    }
    if self.turns_seen.is_none_or(|seen| turns > seen) {
      self.turns_seen = Some(turns);
    }
    progress
  }

  /// Records that `staged` chunks of its content are staged here after an attempt's fetch; whether that is progress (a
  /// new high, after a first look), which starts its attempts again.
  pub fn record_staged(&mut self, staged: usize) -> bool {
    let progress = self.most_staged.is_some_and(|most| staged > most);
    if progress {
      self.attempts = 0;
    }
    if self.most_staged.is_none_or(|most| staged > most) {
      self.most_staged = Some(staged);
    }
    progress
  }

  /// Records, after an attempt that waited on its catalog register or that register's placement, that the
  /// still-paging takeover round which adopted it has advanced `advances` pages so far; whether the round advanced
  /// since the previous such attempt (after a first), which is progress and starts its attempts again
  /// ([`round_advanced`]).
  pub fn record_round(&mut self, advances: u64) -> bool {
    let progress = round_advanced(&mut self.round_seen, advances);
    if progress {
      self.attempts = 0;
    }
    progress
  }
}

/// Whether a still-paging takeover round, read at `advances` page advances (`takeover::rounds_still_paging`), advanced
/// since an object waiting on it last read it at `seen`, which it then reads. A first look is no advance. A round's
/// count only grows, but a round begun anew counts from zero, so any change is an advance. An object waits on its round
/// only for a register the round's later pages may bring (its catalog register sorts after every head of its creator's
/// partition, and a round reads a page a member a period): while the round advances, the wait is progress and its
/// attempts start again; while the round stands still (a member that never answers, or answers without moving on), the
/// wait counts toward the bound like any other.
fn round_advanced(seen: &mut Option<u64>, advances: u64) -> bool {
  let advanced = seen.is_some_and(|seen| seen != advances);
  *seen = Some(advances);
  advanced
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A green's adopted merge record, for a pending green.
  fn adopted_record() -> crate::merge_service::MergeRecordValue {
    crate::merge_service::MergeRecordValue {
      version: 3,
      increment: [5u8; 32],
      base: 2,
      inputs: Some([9u8; 32]),
      identity: [7u8; 32],
      evidence: Vec::new(),
      name: "green".to_owned(),
      require_evidence: false,
      owner: slates_db::catalog::Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1000,
      },
      completion: None,
      signature: None,
      folded_below: 0,
      checkpoint_floor: 0,
    }
  }

  /// §4.8 takeover, CLAUDE.md §2 item 8 (no unbounded retry): a taken-over green's attempts start again only when an
  /// attempt leaves fewer of its inputs unheld than ever before. Do: record 3 unheld at the first look, make attempts,
  /// then record 3, 2, 3 (an input released and fetched again), 2 and 0. Expect progress only at 2 and at 0, each
  /// starting the attempts again, and none at the first look or at a count already reached, so an input that keeps
  /// being lost cannot keep the green pending without end.
  #[test]
  fn a_greens_attempts_start_again_only_at_a_new_low_of_unheld_inputs() {
    let mut pending = PendingGreen::adopted(adopted_record(), None, Vec::new());
    let mut seen = Vec::new();
    for unheld in [3, 3, 2, 3, 2, 0] {
      pending.attempts = 5;
      let progress = pending.record_unheld(unheld);
      seen.push((unheld, progress, pending.attempts));
    }
    assert_eq!(
      seen,
      vec![
        (3, false, 5),
        (3, false, 5),
        (2, true, 0),
        (3, false, 5),
        (2, false, 5),
        (0, true, 0),
      ]
    );
  }

  /// §4.8 takeover, CLAUDE.md §2 item 8: a taken-over volume's attempts start again only when its staged content
  /// reaches a new high. Do: record 4 chunks staged at the first look, make attempts, then record 4, 9, 2 (the stage
  /// dropped and fetched again), 9 and 10. Expect progress only at 9 and at 10, so a fetch cut by its deadline each
  /// period is given up only once it stops advancing, and a stage that keeps being lost cannot keep the volume pending
  /// without end.
  #[test]
  fn a_volumes_attempts_start_again_only_at_a_new_high_of_staged_content() {
    let mut pending = PendingVolume::adopted(
      crate::head::HeadValue {
        manifest: Some([9u8; 32]),
        content_holders: vec![7],
        sealing: None,
      },
      None,
      Vec::new(),
    );
    let mut seen = Vec::new();
    for staged in [4, 4, 9, 2, 9, 10] {
      pending.attempts = 7;
      let progress = pending.record_staged(staged);
      seen.push((staged, progress, pending.attempts));
    }
    assert_eq!(
      seen,
      vec![
        (4, false, 7),
        (4, false, 7),
        (9, true, 0),
        (2, false, 7),
        (9, false, 7),
        (10, true, 0),
      ]
    );
  }

  /// §4.10 status AUD-29-43, CLAUDE.md §2 item 8 (no unbounded retry): a taken-over tombstone its owner shard does not
  /// record is given up on at the bound, once, and then counts no more attempts. Do: count attempts against a bound of
  /// 3, then twice more. Expect the third to give it up and the rest to count nothing, the attempts staying at 3.
  #[test]
  fn a_tombstone_is_given_up_once_at_its_bound_and_counts_nothing_after() {
    let mut tombstone = PendingTombstone::adopted(9);
    let given_up: Vec<bool> = (0..5).map(|_| tombstone.count_attempt(3)).collect();
    assert_eq!(given_up, vec![false, false, true, false, false]);
    assert_eq!(
      tombstone,
      PendingTombstone {
        sequence: 9,
        attempts: 3,
        given_up: true,
      }
    );
  }

  /// §4.8 takeover, CLAUDE.md §2 item 8: a taken-over object waiting on its catalog register while the round that
  /// adopted it pages starts its attempts again only when the round has advanced since its previous look, for a volume
  /// and a green alike. Do: read the round at 3 page advances at the first look, then 3, 4, 4, 9 and, the round begun
  /// anew, 0. Expect progress at 4, 9 and 0 and nowhere else, so a round that stands still lets the attempts count, and
  /// one that keeps paging keeps the object pending only while it moves on.
  #[test]
  fn a_wait_on_a_paging_round_is_progress_only_while_the_round_advances() {
    let mut volume = PendingVolume::adopted(
      crate::head::HeadValue {
        manifest: None,
        content_holders: Vec::new(),
        sealing: None,
      },
      Some(slates_db::register::HostId(5)),
      Vec::new(),
    );
    let mut green = PendingGreen::adopted(
      adopted_record(),
      Some(slates_db::register::HostId(5)),
      Vec::new(),
    );
    let (mut volume_seen, mut green_seen) = (Vec::new(), Vec::new());
    for advances in [3, 3, 4, 4, 9, 0] {
      volume.attempts = 6;
      green.attempts = 6;
      let progress = volume.record_round(advances);
      volume_seen.push((advances, progress, volume.attempts));
      let progress = green.record_round(advances);
      green_seen.push((advances, progress, green.attempts));
    }
    let expected = vec![
      (3, false, 6),
      (3, false, 6),
      (4, true, 0),
      (4, false, 6),
      (9, true, 0),
      (0, true, 0),
    ];
    assert_eq!(volume_seen, expected, "a volume");
    assert_eq!(green_seen, expected, "a green");
  }
}
