//! A page an NFS client writes back over another path's newer change is refused, never applied (§4.6 "NFS v3", A-192:
//! the winner of the A/B Ada asked for on 2026-10-31 against the sync-point coherence it replaces).
//!
//! **What it answers.** An NFS v3 client caches a file's pages and writes a dirty page back whole: the program's stores
//! with the rest of the page as the client fetched it. When another path changed the file in between, that write-back
//! carries the old bytes and puts them back over the newer write, silently. Apple's client does exactly this for a page a
//! program dirtied through a shared map and did not sync (measured 3 of 3 on CI's macOS runner, diagnostics run
//! 40821794698: the other path's 16,384 bytes lost), NFS v3 has no recall that could make it flush first, and Apple's
//! NFSv4.1 recall returns the delegation without flushing (GAPS).
//!
//! **Measured (A-192).** Against the sync-point coherence it replaces, on the adversarial suite (`tests/nfs_writeback.rs`)
//! this passes 6 of 6 cases and that 2 of 6; through Apple's own client (macOS 26.6.2, CI's runner) the other path's
//! write survives every order of a store and a write, where it was lost in every one. Only while another path writes
//! does a write-back cost more: 2 to 3 % at the median, and up to 10 % fewer write-backs a second on a loaded Mac. The
//! states cost under 1 % of the daemon's memory (`docs/wip/BENCHMARKS.md`).
//!
//! **The rule.** The server keeps, for each connection, what the client's cache of each file it touched is as of: A-140's
//! byte states ([`PageStates`]), fed here instead of by a FUSE mount's rounds.
//! - A READ the client is answered makes the bytes it returned fresh as of the journal's head.
//! - The connection's own changes refresh what they wrote ([`OwnRecords`]).
//! - Another path's change to bytes the client fetched earlier makes them stale.
//! - A byte the client never fetched is absent: no change makes it stale, so a write the client makes without reading
//!   first is a write, never a stale copy, and is applied.
//!
//! A WRITE whose range holds stale bytes that differ from the file's bytes now would put old bytes back over a newer
//! change, so it is refused, `NFS3ERR_IO`, and nothing of it applied: the other path's change survives. Stale bytes equal
//! to the file's are no harm, and the WRITE is applied. Nothing is inferred: the server never picks bytes out of a write
//! (CLAUDE.md, banned item 11), it applies the write whole or refuses it whole, the verdicts of D-27. Where the states
//! cannot tell (a file lost for want of room, a journal that moved past what the connection consumed), the WRITE is
//! refused too, counted.
//!
//! **What it cannot do.** Apple's client takes the refusal and tells the program nothing: its `msync` and `close` answered
//! 0 in every measured case. So the program's stores in a refused page are gone without a word. When they came before
//! the other path's write, that is the order of events (the later write wins); when they came after, through a page
//! the client had not refetched, they were the newer write and are lost — the case only a byte-level merge could keep,
//! which is banned. A program that needs both kept maps through the socket route (A-186).
//!
//! **Across a takeover (A-231).** A connection the anchor holds outlives its daemon, so its states must too. Each
//! publication carries the held connections' states ([`carry`]), every tracker consumed first; a successor takes them
//! back once its volumes are rebuilt ([`restore`]), each byte placed before its own journal's first record, so every
//! change since the publication marks the bytes it touched stale. What it cannot know is judged stale too: a connection
//! the anchor handed back treats every file made before the takeover as possibly cached, so a change since makes those
//! bytes stale, and a file it first tracks late is taken from the journal, or as unknown once the journal no longer
//! reaches back.
//!
//! **Keeping the states current.** One tracker per volume with connections tracking its files: a journal reader (A-157),
//! so a record waits for it, and a task parked on the volume's change generation that consumes each change into every
//! connection's states as it is made, so the states never depend on how long the journal keeps a record. A WRITE
//! consumes what is pending before it is judged. A tracker can close before its task first runs and a new one open for
//! the volume, so each tracker has an identity and its task keeps only the tracker it was spawned for.
//!
//! **Bounds.** The states are charged to the shard's metadata ledger, [`RANGE_BYTES`] a range, as a FUSE mount's are
//! (A-155); a file whose ranges the ledger refuses is lost, and its writes are refused until its connection ends. A
//! connection's states go when it ends, however it ends, on the shards its READs and WRITEs were served on and no other
//! ([`Footprint`]). The connections are the listener's, bounded by its admission.

use std::collections::BTreeMap;

use slates_bridge_fuse::coherence::{
  MOST_ADDED_BY_AN_UPDATE, OwnRecords, PageStates, RANGE_BYTES, WritebackVerdict, judge_writeback,
};
use slates_db::catalog::VolumeId;
use slates_mem::budget::MetadataCredit;
use slates_vfs::ids::InodeNo;

use crate::state::{self, ShardState};

/// Format: write-backs refused because they would put stale bytes back over another path's newer change: the rule's
/// non-vacuity count.
const CONFLICTS: &str = "nfs.writeback_conflicts";
/// Format: write-backs refused because the states could not tell (a file lost for room, a journal overrun). Zero is the
/// healthy reading.
const UNKNOWN: &str = "nfs.writeback_unknown";
/// Format: write-backs applied over stale bytes that equal the file's bytes now.
const IDENTICAL: &str = "nfs.writeback_identical";
/// Format: trackers whose consumer found the journal past what it had consumed: every connection's states lost. Zero is
/// the healthy reading.
const OVERRUNS: &str = "nfs.writeback_overruns";
/// Format: files whose states the metadata ledger refused room for. Zero is the healthy reading.
const LOST_FOR_ROOM: &str = "nfs.writeback_states_lost";
/// Format: trackers whose consumer task could not be spawned, kept current only as their connections call. Zero is the
/// healthy reading.
const KEEPER_REFUSED: &str = "nfs.writeback_keeper_refused";
/// Format: consumer tasks that found a newer tracker of their volume in place of the one they were spawned for, and
/// ended rather than take it: a tracker closed before its task first ran, and a new one opened.
const STALE_KEEPERS: &str = "nfs.writeback_keepers_stale";

/// Format: held connections' byte states taken back from the last publication after a takeover (A-231): the carry's
/// non-vacuity count.
const RESTORED: &str = "nfs.writeback_states_restored";

/// Format: the point of a byte the client never fetched: no record can follow it, so no change makes the byte stale.
const ABSENT: u64 = u64::MAX;

/// Format: the journal reader key of a volume's tracker (A-157). A registry attachment's key packs its slab index into
/// the high half and its generation into the low (`slates_bridge_core::AttachmentId::key`); the attachments slab is
/// bounded far below `u32::MAX` entries, so no attachment's key is this.
const READER_KEY: u64 = u64::MAX;

/// The shard's write-back states: a tracker per volume with connections tracking its files. One field on
/// [`ShardState`].
#[derive(Debug, Default)]
pub(crate) struct WritebackStates {
  trackers: BTreeMap<VolumeId, Tracker>,
  /// The identity the next tracker made takes.
  next_tracker: u64,
  /// The connections the anchor holds among those tracked (A-231): only their states outlive the daemon, carried in each
  /// publication. Bounded by the anchor's hold bound; a connection leaves it when it ends.
  held: std::collections::BTreeSet<u64>,
  /// The connections this daemon's anchor handed it at its start, held for a daemon before it (A-231): their clients may
  /// hold pages fetched before the takeover. Bounded as [`WritebackStates::held`].
  inherited: std::collections::BTreeSet<u64>,
  /// Each volume of this shard's first inode number not issued at the takeover (A-231): a file below it may be cached
  /// by an inherited connection's client. Bounded by the shard's volumes; set once, at recovery.
  takeover_below: BTreeMap<VolumeId, u64>,
}

/// One volume's tracker: how far its journal is consumed, its change generation as of then, and each connection's
/// states.
#[derive(Debug, Default)]
struct Tracker {
  /// Which tracker of its volume this is: its consumer task keeps only the tracker it was spawned for. A tracker can
  /// close before its task first runs and a new one open for the volume; found by the volume alone, the old task took
  /// the new tracker as its own, and the one of the two that parked first was never woken again.
  id: u64,
  cursor: u64,
  seen: u64,
  connections: BTreeMap<u64, Connection>,
  /// The consumer task's waker as it last parked: woken when the tracker closes, so the task ends then rather than at
  /// the volume's next change, which may never come.
  keeper: Option<std::task::Waker>,
}

/// One connection's states for one volume: its byte states, the records its own calls made, and the metadata ledger
/// bytes its states were admitted.
#[derive(Debug, Default)]
struct Connection {
  states: PageStates,
  own: OwnRecords,
  charged: u64,
  /// Taken back after a takeover (A-231): the first inode number its volume had not issued then. What the connection
  /// fetched after the last publication was not carried, so a file below it, which may have been cached before the
  /// takeover, is taken as fetched before the successor's first record; a file at or past it was made since.
  restored_below: Option<u64>,
  /// First tracked here after its tracker had consumed changes made since the takeover (A-231): a file made before the
  /// takeover is taken back from the journal's changes to it since the successor's first record, or as unknown once the
  /// journal no longer reaches back.
  late: bool,
}

/// Judges connection `connection`'s WRITE of `data` at `at` to inode `ino` of `volume`, consuming every pending change
/// first, by the rule every transport shares ([`judge_writeback`], A-248): applied whole, or refused whole when a stale
/// byte differs from the file's or the states cannot tell. A volume not on this shard is applied: its call is refused
/// where it is served.
pub(crate) fn judge(
  s: &mut ShardState,
  connection: u64,
  (volume, ino): (VolumeId, u64),
  at: u64,
  data: &[u8],
) -> WritebackVerdict {
  if !tracking(s, volume) {
    return WritebackVerdict::Apply;
  }
  consume(s, volume);
  let history = prepare(s, connection, (volume, ino));
  let range = at..at.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
  let stale = {
    let ShardState {
      nfs_writeback,
      store,
      ..
    } = &mut *s;
    let Some(tracker) = nfs_writeback.trackers.get_mut(&volume) else {
      return WritebackVerdict::Apply;
    };
    let conn = tracker.connections.entry(connection).or_default();
    open_untracked(&mut store.metadata, conn, ino, &history);
    let lost = conn.states.take_lost_for_room();
    let stale = conn.states.stale_after(ino, range, ABSENT, &[]);
    (stale, lost)
  };
  s.count(LOST_FOR_ROOM, stale.1);
  let verdict = {
    let slot = s
      .by_id
      .get(&volume)
      .and_then(|&handle| s.volumes.get(handle).ok());
    judge_writeback(stale.0.as_deref(), at, data, |offset, len, out| {
      let (Some(slot), Ok(len)) = (slot, usize::try_from(len)) else {
        return false;
      };
      out.clear();
      out.resize(len, 0);
      match slot.volume.read(&s.store, InodeNo(ino), offset, out) {
        Ok(read) => {
          out.truncate(read);
          true
        }
        Err(_) => false,
      }
    })
  };
  match verdict {
    WritebackVerdict::Conflict => s.count(CONFLICTS, 1),
    WritebackVerdict::Unknown => s.count(UNKNOWN, 1),
    WritebackVerdict::Identical => s.count(IDENTICAL, 1),
    WritebackVerdict::Apply => {}
  }
  verdict
}

/// Connection `connection` was answered bytes `range` of inode `ino` of `volume`: fresh as of the journal's head.
pub(crate) fn fetched(
  s: &mut ShardState,
  connection: u64,
  (volume, ino): (VolumeId, u64),
  range: std::ops::Range<u64>,
) {
  if range.is_empty() || !tracking(s, volume) {
    return;
  }
  let Some(head) = head_of(s, volume) else {
    return;
  };
  let history = prepare(s, connection, (volume, ino));
  let lost = {
    let ShardState {
      nfs_writeback,
      store,
      ..
    } = &mut *s;
    let Some(tracker) = nfs_writeback.trackers.get_mut(&volume) else {
      return;
    };
    let conn = tracker.connections.entry(connection).or_default();
    make_room(
      &mut store.metadata,
      conn,
      MOST_ADDED_BY_AN_UPDATE.saturating_add(1),
    );
    open_untracked(&mut store.metadata, conn, ino, &history);
    conn.states.fetched(ino, range, head);
    conn.states.take_lost_for_room()
  };
  s.count(LOST_FOR_ROOM, lost);
}

/// What an untracked file is to a connection it is first named on (A-192, A-231):
/// - never fetched (`ABSENT`), so a write it makes without reading is a write: a connection not inherited, or a file
///   made since the takeover;
/// - for an inherited connection and a file made before the takeover, fetched before the successor's first record:
///   with no change since, when its tracker consumed every change since the takeover into it (not [`Connection::late`]);
///   else with the journal's changes to it since then (`history`), or every byte unknown once the journal no longer
///   reaches back.
fn open_untracked(
  ledger: &mut slates_mem::budget::MetadataBudget,
  conn: &mut Connection,
  ino: u64,
  history: &Option<Option<Changes>>,
) {
  if conn.states.tracks(ino) {
    return;
  }
  if !conn.restored_below.is_some_and(|below| ino < below) {
    make_room(ledger, conn, 1);
    conn.states.opened(ino, ABSENT);
    return;
  }
  match (conn.late, history) {
    (true, Some(Some(changes))) => {
      make_room(
        ledger,
        conn,
        changes
          .len()
          .saturating_mul(MOST_ADDED_BY_AN_UPDATE)
          .saturating_add(1),
      );
      conn.states.adopted(ino, 0, Some(changes));
    }
    (true, _) => {
      make_room(ledger, conn, 1);
      conn.states.opened_unknown(ino, 0);
    }
    (false, _) => {
      make_room(ledger, conn, 1);
      conn.states.opened(ino, 0);
    }
  }
}

/// A file's changes in a journal, each with its sequence and whether it is the connection's own.
type Changes = Vec<(std::ops::Range<u64>, u64, bool)>;

/// The changes to `ino` the successor's journal holds since its first record, each told the connection's own by `own`;
/// `None` when the journal no longer reaches its first record (A-231).
fn history(log: &slates_vfs::journal::OpLog, ino: u64, own: &OwnRecords) -> Option<Changes> {
  if log.oldest_seq() > 1 {
    return None;
  }
  Some(
    log
      .since(0)
      .filter_map(slates_bridge_core::changed::data_change)
      .filter(|change| change.ino == ino)
      .map(|change| (change.range, change.seq, own.took(change.seq)))
      .collect(),
  )
}

/// Prepares connection `connection`'s entry in `volume`'s tracker for a call naming `ino`: an inherited connection first
/// tracked here learns the takeover's bound and that it came late, and the journal's history of `ino` is read when that
/// connection will need it ([`open_untracked`]). `Some(history)` only then.
fn prepare(
  s: &mut ShardState,
  connection: u64,
  (volume, ino): (VolumeId, u64),
) -> Option<Option<Changes>> {
  let inherited = s.nfs_writeback.inherited.contains(&connection);
  let below = s.nfs_writeback.takeover_below.get(&volume).copied();
  let tracker = s.nfs_writeback.trackers.get_mut(&volume)?;
  let cursor = tracker.cursor;
  let conn = tracker.connections.entry(connection).or_default();
  if inherited && conn.restored_below.is_none() {
    conn.restored_below = below;
    conn.late = cursor > 0;
  }
  if !conn.late || conn.states.tracks(ino) || !conn.restored_below.is_some_and(|below| ino < below)
  {
    return None;
  }
  let own = std::mem::take(&mut conn.own);
  let changes = s
    .by_id
    .get(&volume)
    .and_then(|&handle| s.volumes.get(handle).ok())
    .and_then(|slot| history(slot.volume.op_log(), ino, &own));
  if let Some(conn) = s
    .nfs_writeback
    .trackers
    .get_mut(&volume)
    .and_then(|tracker| tracker.connections.get_mut(&connection))
  {
    conn.own = own;
  }
  Some(changes)
}

/// The journal's head of `volume` now, when it lives on this shard.
pub(crate) fn head_of(s: &ShardState, volume: VolumeId) -> Option<u64> {
  let &handle = s.by_id.get(&volume)?;
  Some(s.volumes.get(handle).ok()?.volume.op_log().head_seq())
}

/// Connection `connection` made a call on `volume` while its journal moved from `before`: the records since are its own,
/// and refresh what they wrote in its client's cache. Nothing for a connection tracking nothing there.
pub(crate) fn made(s: &mut ShardState, connection: u64, volume: VolumeId, before: u64) {
  let Some(&handle) = s.by_id.get(&volume) else {
    return;
  };
  let Ok(slot) = s.volumes.get(handle) else {
    return;
  };
  let log = slot.volume.op_log();
  let (after, capacity) = (log.head_seq(), log.capacity());
  if let Some(conn) = s
    .nfs_writeback
    .trackers
    .get_mut(&volume)
    .and_then(|tracker| tracker.connections.get_mut(&connection))
  {
    conn.own.record(before, after, capacity);
  }
}

/// Format: connection ends refused by a shard their states are kept on (its control channel full past a coordinator
/// period, or the shard gone): the states are left until that tracker closes. Zero is the healthy reading.
const CLOSE_REFUSED: &str = "nfs.writeback_close_refused";

/// Format: connection ends sent to another shard, where the connection's states are kept: what a connection's end costs
/// the other shards. A connection that read and wrote one shard's volumes ends where its states are and sends nothing.
const ENDS_SENT: &str = "nfs.writeback_ends_sent";

/// Where a connection's states are kept: the shards its READs and WRITEs were served on, each once. The connection
/// carries it from its first call to its end, across every move between shards. Dropped — the connection ended, its
/// move was refused, or its task was cancelled — it drops the states there and only there: inline on this shard, by a
/// message to each other one. So a connection leaves nothing behind however it ends, and a client that opens and drops
/// connections that read nothing costs no other shard anything. Before, an end went to every shard: a task on each
/// shard's control channel, the channel forwarded calls are admitted through.
#[derive(Debug)]
pub(crate) struct Footprint {
  connection: u64,
  shards: Vec<u16>,
}

impl Footprint {
  /// Connection `connection`'s footprint, before its first call.
  pub(crate) fn of(connection: u64) -> Footprint {
    Footprint {
      connection,
      shards: Vec::new(),
    }
  }

  /// The connection whose states these are.
  pub(crate) fn connection(&self) -> u64 {
    self.connection
  }

  /// Notes that a call of the connection is served on `shard`, so it may keep states there. Each shard is noted once, so
  /// the list is bounded by the shard count.
  pub(crate) fn note(&mut self, shard: u16) {
    if !self.shards.contains(&shard) {
      self.shards.push(shard);
    }
  }
}

impl Drop for Footprint {
  fn drop(&mut self) {
    // Off a shard the states went with the shard's own state.
    let Some(this) = slates_rt::registry::current_shard() else {
      return;
    };
    let connection = self.connection;
    for &shard in &self.shards {
      if shard != this {
        crate::fleet::count_refusal(ENDS_SENT);
      }
      crate::xshard::run_on_waiting(this, shard, move |s| ended(s, connection), CLOSE_REFUSED);
    }
  }
}

/// What connection `connection` is: one the anchor holds (A-128), whose states are carried in each publication, and one
/// it handed this daemon at its start, whose client may hold pages fetched before the takeover (A-231).
pub(crate) fn note_connection(s: &mut ShardState, connection: u64, held: bool, inherited: bool) {
  if held {
    s.nfs_writeback.held.insert(connection);
  }
  if inherited {
    s.nfs_writeback.inherited.insert(connection);
  }
}

/// The held connections' byte states, for a publication (A-231): for each, a copy of its states with every change its
/// tracker has not consumed yet applied to the copy, so the states carried are as of this moment and nothing the
/// publication carries is left out. The live states and their cursor are left alone: a publication runs inside a call,
/// before that call's own records are noted ([`made`]), and consuming them here took the connection's own write for
/// another path's, so its next write-back of the same page was refused as stale (main CI 42395077767, fsx over the macOS
/// NFS mount; `a_held_connections_own_write_backs_are_never_refused_as_stale`: 19 of 20 refused). In the copy a record
/// not yet noted counts as another path's, which only makes a successor more careful.
pub(crate) fn carry(s: &ShardState) -> Vec<slates_vfs::recover::HeldNfsStates> {
  if s.nfs_writeback.held.is_empty() {
    return Vec::new();
  }
  let held = &s.nfs_writeback.held;
  let mut carried = Vec::new();
  for (volume, tracker) in &s.nfs_writeback.trackers {
    let Some(log) = s
      .by_id
      .get(volume)
      .and_then(|&handle| s.volumes.get(handle).ok())
      .map(|slot| slot.volume.op_log())
    else {
      continue;
    };
    let head = log.head_seq();
    let overran = tracker.cursor.saturating_add(1) < log.oldest_seq();
    let pending: Vec<slates_bridge_core::DataChange> = if head > tracker.cursor && !overran {
      log
        .since(tracker.cursor)
        .filter_map(slates_bridge_core::changed::data_change)
        .collect()
    } else {
      Vec::new()
    };
    for (&connection, conn) in &tracker.connections {
      if !held.contains(&connection) {
        continue;
      }
      let mut states = conn.states.clone();
      // The copy is not charged to the ledger: it lives only while this publication is encoded.
      states.grow_room(usize::MAX.saturating_sub(states.room()));
      if overran {
        states.lose_all();
      } else if head > tracker.cursor {
        for change in &pending {
          states.changed(
            change.ino,
            change.range.clone(),
            change.seq,
            conn.own.took(change.seq),
          );
        }
        states.consumed(head);
      }
      carried.push(slates_vfs::recover::HeldNfsStates {
        connection,
        volume: volume.bytes,
        files: states.carried(ABSENT),
      });
    }
  }
  carried
}

/// Takes back the held connections' byte states the last publication carried (A-231), once the volumes are rebuilt:
/// each fetched byte placed before every record the successor's journal holds, so a change made after that publication
/// (replayed, or made since) marks the bytes it touched stale when consumed, here at once while the journal still holds
/// it. A journal that no longer reaches its first record loses every file, whose writes are then refused. The states
/// taken back.
pub(crate) fn restore(
  s: &mut ShardState,
  carried: Vec<slates_vfs::recover::HeldNfsStates>,
) -> usize {
  // Every volume's takeover bound, for the connections the anchor handed back that carried nothing for it.
  let bounds: Vec<(VolumeId, u64)> = s
    .by_id
    .iter()
    .filter_map(|(&volume, &handle)| {
      s.volumes
        .get(handle)
        .ok()
        .map(|slot| (volume, slot.volume.issued_below().0))
    })
    .collect();
  s.nfs_writeback.takeover_below.extend(bounds);
  let mut restored = 0usize;
  let mut volumes = Vec::new();
  for states in carried {
    let volume = VolumeId {
      bytes: states.volume,
    };
    if !tracking(s, volume) {
      continue;
    }
    let Some(issued_below) = s.nfs_writeback.takeover_below.get(&volume).copied() else {
      continue;
    };
    let ShardState {
      nfs_writeback,
      store,
      ..
    } = &mut *s;
    let Some(tracker) = nfs_writeback.trackers.get_mut(&volume) else {
      continue;
    };
    // Before every record the successor's journal holds: its replayed writes come after the publication.
    tracker.cursor = 0;
    let conn = tracker.connections.entry(states.connection).or_default();
    let ranges: usize = states.files.iter().map(|file| file.ranges.len()).sum();
    make_room(&mut store.metadata, conn, ranges);
    // Every byte as fetched before the successor's first record, the never-fetched ones too: what the connection
    // fetched after the last publication was not carried, so a byte carried as never fetched may have been since.
    conn.states.restored(&states.files, 0, 0);
    conn.restored_below = Some(issued_below);
    nfs_writeback.held.insert(states.connection);
    nfs_writeback.inherited.insert(states.connection);
    volumes.push(volume);
    restored = restored.saturating_add(1);
  }
  for volume in volumes {
    consume(s, volume);
  }
  if restored > 0 {
    s.count(RESTORED, u64::try_from(restored).unwrap_or(u64::MAX));
  }
  restored
}

/// Connection `connection` ended: its states go from every tracker of this shard, their ledger bytes returned, and a
/// tracker left with no connection closes.
fn ended(s: &mut ShardState, connection: u64) {
  s.nfs_writeback.held.remove(&connection);
  s.nfs_writeback.inherited.remove(&connection);
  let mut emptied = Vec::new();
  let mut credit = 0u64;
  for (volume, tracker) in &mut s.nfs_writeback.trackers {
    if let Some(conn) = tracker.connections.remove(&connection) {
      credit = credit.saturating_add(conn.charged);
    }
    if tracker.connections.is_empty() {
      emptied.push(*volume);
    }
  }
  if credit > 0 {
    s.store.metadata.release(MetadataCredit { bytes: credit });
  }
  for volume in emptied {
    close(s, volume);
  }
}

/// Removes `volume`'s tracker and closes its journal reader. Its consumer task finds it gone and ends.
fn close(s: &mut ShardState, volume: VolumeId) {
  if let Some(tracker) = s.nfs_writeback.trackers.remove(&volume) {
    let credit: u64 = tracker.connections.values().map(|conn| conn.charged).sum();
    if credit > 0 {
      s.store.metadata.release(MetadataCredit { bytes: credit });
    }
    // The consumer finds its tracker gone and ends (the hostile NFS test caught it outliving its last connection).
    if let Some(keeper) = tracker.keeper {
      keeper.wake();
    }
  }
  if let Some(&handle) = s.by_id.get(&volume)
    && let Ok(slot) = s.volumes.get_mut(handle)
  {
    slot.volume.close_journal_reader(READER_KEY);
  }
}

/// Whether `volume` has a tracker on this shard, made now if it lives here and has none: a journal reader opened at the
/// head and a consumer task spawned. `false` for a volume not here.
fn tracking(s: &mut ShardState, volume: VolumeId) -> bool {
  if s.nfs_writeback.trackers.contains_key(&volume) {
    return true;
  }
  let Some(&handle) = s.by_id.get(&volume) else {
    return false;
  };
  let Ok(slot) = s.volumes.get_mut(handle) else {
    return false;
  };
  let head = slot.volume.op_log().head_seq();
  let seen = slot.volume.change_generation();
  slot
    .volume
    .open_journal_reader(READER_KEY, crate::daemon::LIVENESS_BUDGET_NS);
  let id = s.nfs_writeback.next_tracker;
  // Identities need only differ among the trackers whose tasks are alive at once; wrapping past 2^64 cannot meet one.
  s.nfs_writeback.next_tracker = id.wrapping_add(1);
  s.nfs_writeback.trackers.insert(
    volume,
    Tracker {
      id,
      cursor: head,
      seen,
      connections: BTreeMap::new(),
      keeper: None,
    },
  );
  if slates_rt::futures::spawn(keep(volume, id))
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    s.count(KEEPER_REFUSED, 1);
  }
  true
}

/// Consumes every change to `volume` since its tracker's cursor into each connection's states: another path's makes the
/// bytes a client fetched before it stale, a connection's own refreshes them. A cursor the journal has moved past loses
/// every connection's states, which refuse their writes from then on.
fn consume(s: &mut ShardState, volume: VolumeId) {
  let ShardState {
    nfs_writeback,
    by_id,
    volumes,
    store,
    ..
  } = &mut *s;
  let Some(tracker) = nfs_writeback.trackers.get_mut(&volume) else {
    return;
  };
  let Some(slot) = by_id
    .get(&volume)
    .and_then(|&handle| volumes.get_mut(handle).ok())
  else {
    return;
  };
  let log = slot.volume.op_log();
  let head = log.head_seq();
  tracker.seen = slot.volume.change_generation();
  if head <= tracker.cursor {
    return;
  }
  let mut overran = false;
  let mut lost = 0u64;
  if tracker.cursor.saturating_add(1) < log.oldest_seq() {
    overran = true;
    for conn in tracker.connections.values_mut() {
      conn.states.lose_all();
    }
  } else {
    let changes: Vec<slates_bridge_core::DataChange> = log
      .since(tracker.cursor)
      .filter_map(slates_bridge_core::changed::data_change)
      .collect();
    for conn in tracker.connections.values_mut() {
      for change in &changes {
        make_room(&mut store.metadata, conn, MOST_ADDED_BY_AN_UPDATE);
        // A connection taken back after a takeover tracks a file made before it from its first change since: the
        // client may hold pages of it fetched before the takeover, which the change makes stale (A-231).
        if conn.restored_below.is_some_and(|below| change.ino < below)
          && !conn.states.tracks(change.ino)
        {
          let past = conn.late.then(|| history(log, change.ino, &conn.own));
          open_untracked(&mut store.metadata, conn, change.ino, &past);
        }
        let own = conn.own.took(change.seq);
        conn
          .states
          .changed(change.ino, change.range.clone(), change.seq, own);
      }
      conn.states.consumed(head);
      lost = lost.saturating_add(conn.states.take_lost_for_room());
    }
  }
  tracker.cursor = head;
  slot.volume.note_journal_round(READER_KEY, head);
  if overran {
    s.count(OVERRUNS, 1);
  }
  s.count(LOST_FOR_ROOM, lost);
}

/// Grows `conn`'s room from the ledger so `more` ranges beyond those it holds fit, a page of ranges at a time (A-155):
/// refused, the update that needed it loses its file.
fn make_room(ledger: &mut slates_mem::budget::MetadataBudget, conn: &mut Connection, more: usize) {
  let short = conn
    .states
    .held()
    .saturating_add(more)
    .saturating_sub(conn.states.room());
  if short == 0 {
    return;
  }
  let per_page = ranges_per_page();
  let ranges = short.div_ceil(per_page).saturating_mul(per_page);
  let bytes = u64::try_from(ranges.saturating_mul(RANGE_BYTES)).unwrap_or(u64::MAX);
  if ledger.reserve(bytes).is_ok() {
    conn.states.grow_room(ranges);
    conn.charged = conn.charged.saturating_add(bytes);
  }
}

/// Derived: the byte-state ranges one page of the machine's memory holds — its page size over [`RANGE_BYTES`], at least
/// one — the granule the room grows by, as a FUSE mount's does.
fn ranges_per_page() -> usize {
  usize::try_from(slates_machine::facts::Facts::page_bytes())
    .unwrap_or(1)
    .checked_div(RANGE_BYTES)
    .unwrap_or(1)
    .max(1)
}

/// The consumer of `volume`'s tracker `id`: parked on the volume's change generation, consuming each change as it is
/// made, until that tracker closes or the volume goes.
async fn keep(volume: VolumeId, id: u64) {
  loop {
    let woke = std::future::poll_fn(|cx| {
      match state::with_state_counted(|s| park(s, (volume, id), cx.waker())) {
        Some(Some(true)) => std::task::Poll::Ready(true),
        Some(Some(false)) => std::task::Poll::Pending,
        // The tracker closed, the volume went, or the shard's state is out of reach.
        _ => std::task::Poll::Ready(false),
      }
    })
    .await;
    if !woke {
      return;
    }
    let _ = state::with_state_counted(|s| consume(s, volume));
  }
}

/// Whether `volume` changed past what its tracker `id` consumed (`Some(true)`), parking `waker` until it does when it
/// has not (`Some(false)`); `None` when that tracker or the volume is gone.
fn park(
  s: &mut ShardState,
  (volume, id): (VolumeId, u64),
  waker: &std::task::Waker,
) -> Option<bool> {
  if s.nfs_writeback.trackers.get(&volume)?.id != id {
    s.count(STALE_KEEPERS, 1);
    return None;
  }
  let tracker = s.nfs_writeback.trackers.get_mut(&volume)?;
  if !tracker
    .keeper
    .as_ref()
    .is_some_and(|keeper| keeper.will_wake(waker))
  {
    tracker.keeper = Some(waker.clone());
  }
  let seen = tracker.seen;
  let &handle = s.by_id.get(&volume)?;
  let slot = s.volumes.get_mut(handle).ok()?;
  Some(slot.volume.wait_for_change(seen, waker))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: rounds of the race, so a keeper left behind in each shows as a count well past any task the shard's own
  /// loops start and end meanwhile.
  const ROUNDS: u64 = 8;
  /// Shape: the inode the connections' states are about; any number works, since the states never read the file.
  const INODE: u64 = 2;
  /// Shape: the bytes a connection is said to have fetched: one page.
  const FETCHED: std::ops::Range<u64> = 0..16_384;
  /// Shape: how long the shard may take to end the keepers, as the NFS tests allow their connections' tasks.
  const SETTLE: std::time::Duration = std::time::Duration::from_secs(10);
  /// Shape: the pause between two looks at the shard's live tasks while it ends them.
  const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

  /// A-192, found by the hostile NFS test on a loaded Linux box (`9 live before, 16 after`). Do, eight times on one
  /// volume of an audit daemon: in one turn of its shard, open a tracker for a connection, end the connection (so the
  /// tracker closes before its keeper ever ran) and open a new tracker for a second connection; let the shard run what
  /// that turn spawned; then end the second connection. Expect: every keeper ends, so the shard's live tasks come back
  /// to where they were, and each round's first keeper met the newer tracker (`nfs.writeback_keepers_stale`, 8).
  /// Before, the first keeper found the second tracker by its volume and kept it as its own; the tracker woke at its
  /// close only the keeper that parked last, and the other stayed parked on the volume until its next change.
  #[test]
  // The test thread waits off the shard while the shard ends the keepers.
  #[allow(clippy::disallowed_methods)]
  fn a_keeper_ends_with_the_tracker_it_was_spawned_for() {
    let daemon = crate::daemon::audit_daemon();
    let volume = crate::daemon::observe_first(&daemon, |state| {
      let made = crate::verbs::dispatch(
        state,
        0,
        &slates_db::catalog::Principal::Uid { uid: 501 },
        slates_ipc::protocol::RequestBody::Create {
          name: "keepers".to_owned(),
          size: slates_ipc::protocol::SizeClass::Bounded { limit: 4 << 20 },
          names: slates_ipc::protocol::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      state
        .by_id
        .keys()
        .next()
        .copied()
        .ok_or_else(|| format!("the create answered {made:?}"))
    })
    .expect("the volume was made");
    let live = || -> u64 {
      daemon
        .shard_pulses()
        .iter()
        .map(|pulse| pulse.spawns.saturating_sub(pulse.completed))
        .sum()
    };
    let baseline = live();
    for round in 0..ROUNDS {
      let (first, second) = (2 * round + 1, 2 * round + 2);
      crate::daemon::observe_first(&daemon, move |state| {
        fetched(state, first, (volume, INODE), FETCHED);
        ended(state, first);
        fetched(state, second, (volume, INODE), FETCHED);
      });
      // The shard runs the keepers that turn spawned before it takes the next question.
      crate::daemon::observe_first(&daemon, |_| ());
      crate::daemon::observe_first(&daemon, move |state| ended(state, second));
    }
    let settled_at = std::time::Instant::now();
    while live() > baseline && settled_at.elapsed() < SETTLE {
      std::thread::sleep(SETTLE_POLL);
    }
    let after = live();
    let stale = crate::daemon::observe_first(&daemon, |state| {
      state
        .refusals
        .get("nfs.writeback_keepers_stale")
        .copied()
        .unwrap_or(0)
    });
    daemon.stop();
    assert!(
      after <= baseline,
      "every keeper ends with its tracker: {after} tasks live after, {baseline} before"
    );
    assert_eq!(
      stale, ROUNDS,
      "non-vacuity: each round's first keeper met the second round's tracker in place of its own"
    );
  }
}
