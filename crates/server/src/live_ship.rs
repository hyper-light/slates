//! Live shipping in the daemon (§4.10, A-200 "In the daemon"): each publication of a live-shipped volume is appended
//! to the volume's log on its owner shard and shipped to its candidates; the candidates keep the log sealed.
//!
//! **Owner.** The volume's own shard keeps its [`Shipper`] ([`LiveState::shipping`]), so positions are assigned in
//! publication order inside the barrier, where nothing can reorder them. [`published`] is called by the barrier once a
//! publication has committed locally: it builds the entry (the volume's publication record and the content of every
//! extent it names, read in the clear through `Volume::read` and sealed under the volume's lineage key, A-92), appends
//! it, and hands what each holder lacks to a task that ships it over the control shard's sessions (the only shard that
//! holds them, `fleet::take_sessions`). The replies come back to the owner shard and are folded there. The record
//! period re-ships what a holder still lacks ([`period`]).
//!
//! **Holder.** A candidate keeps, per live volume, a [`Replica`] (the fence and the head) and the sealed entries since
//! the last whole state, on the control shard ([`LiveState::held`]). It never opens one. An entry or install that does
//! not pass the chain or the fence is answered with the head held, which the owner reads as what to send next.
//!
//! A delta applies only to the record before it, so a publication that could not be appended (no credit, a read or a
//! seal refused) leaves the volume owing a whole record: its next publication ships the volume's whole image, and that
//! entry is the checkpoint an install is made from ([`Shipper::mark_checkpoint`]).
//!
//! Not yet: a barrier's reply does not wait for durability (A-200's next slice), a holder's replicas do not survive its
//! own daemon's restart, and a takeover does not read them.

use std::collections::{BTreeMap, VecDeque};

use slates_db::live::{
  Admitted, Entry, Head, Install, LiveError, Next, Replica, Shipper, decode_reply, encode_reply,
};
use slates_db::register::{HostId, ObjectId};
use slates_vfs::delta::VolumeRecord;
use slates_vfs::ids::InodeNo;
use slates_vfs::recover::{BodyImage, ExtentSourceImage, InodeImage};
use slates_wire::Wire;

use crate::state::{self, ShardState};

/// Format: an entry's first byte when its record is the volume's whole image (an install may be made from it, and a
/// holder drops what it held before it).
const KIND_WHOLE: u8 = 1;
/// Format: an entry's first byte when its record is a delta over the record before it.
const KIND_DELTA: u8 = 0;
/// Format: the second byte: the body is sealed under the volume's lineage key (A-92).
const BODY_SEALED: u8 = 1;
/// Format: the second byte: the body is in the clear, where this node cannot seal at all (no sealing root), as a
/// seal's archive is placed then (`fleet::envelope_of`).
const BODY_CLEAR: u8 = 0;
/// Derived: the entries a volume's shipper holds that are not yet durable, at most. Shape: one publication in flight
/// and one behind it, the pipelining of a writer that does not wait (A-200's barrier slice derives it from the measured
/// apply rate and round trip; until then a writer that outruns two is shipped a whole record next).
const CREDIT: u64 = 2;
/// Shape: the entries the owner retains for a holder that lags, beyond which it is installed: as many as the credit
/// lets be in flight, twice over.
const RETAIN: usize = 4;

/// Counter: a live volume's publication that could not be shipped as an entry, because its content could not be read,
/// sealed or appended; its next publication ships the whole volume.
const LIVE_REFUSED: &str = "fleet.live.refused";
/// Counter: entries appended to a live volume's log on this owner shard.
const LIVE_APPENDED: &str = "fleet.live.appended";
/// Counter: entries a holder refused to keep (over its bound, unauthorized, malformed).
const LIVE_HOLD_REFUSED: &str = "fleet.live.hold_refused";
/// Counter: entries and installs this node kept as a candidate.
const LIVE_HELD: &str = "fleet.live.held";
/// Counter: advances of a live volume's durable position on this owner shard (held by the owner and `f` candidates).
const LIVE_DURABLE: &str = "fleet.live.durable";

/// A live volume's content runs: the bytes a record's extents name, by inode and file offset.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct LiveRun {
  /// The inode.
  inode: u64,
  /// The file offset.
  offset: u64,
  /// The bytes.
  bytes: Vec<u8>,
}

/// One shipped publication: the record as the owner's journal holds it, and the content its extents name.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct LivePublication {
  /// The record (`VolumeRecord`'s wire bytes).
  record: Vec<u8>,
  /// The content.
  runs: Vec<LiveRun>,
}

/// A sealed body: the sealed chunk's keyed name and bytes (`slates_cluster::sealed`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
struct SealedBody {
  /// The keyed name.
  name: Vec<u8>,
  /// The sealed bytes.
  bytes: Vec<u8>,
}

/// A live volume on its owner shard.
#[derive(Debug)]
pub(crate) struct LiveVolume {
  shipper: Shipper,
  /// Its next publication must ship the whole volume (a delta would not apply to what the holders hold).
  owes_whole: bool,
  /// The last whole entry's payload, and its head: what an install sends.
  whole: Option<(Head, Vec<u8>)>,
}

/// What a candidate keeps of one live volume.
#[derive(Debug)]
pub(crate) struct Held {
  replica: Replica,
  /// The sealed payloads since the last whole one (that one first), in order.
  entries: VecDeque<Vec<u8>>,
  /// Their bytes.
  bytes: usize,
}

/// The node's live-shipping state, on every shard: the volumes this shard owns and ships, and (on the control shard)
/// what this node holds for others.
#[derive(Debug, Default)]
pub(crate) struct LiveState {
  /// The live volumes this shard owns, by volume bytes.
  pub(crate) shipping: BTreeMap<[u8; 16], LiveVolume>,
  /// What this node holds as a candidate, by object.
  pub(crate) held: BTreeMap<ObjectId, Held>,
  /// The live volumes the publication in progress recorded, with their record (`None`: the publication was a
  /// checkpoint, so the whole image ships): shipped once it commits ([`ship_captured`]), dropped if it is refused.
  pub(crate) captured: Vec<(slates_db::catalog::VolumeId, Option<Vec<u8>>)>,
}

/// Notes each live-shipped volume of a committed checkpoint, whose whole image ships ([`ship_captured`]).
pub(crate) fn note_checkpoint(state: &mut ShardState, volumes: &[slates_db::catalog::VolumeId]) {
  for id in volumes {
    if is_live(state, *id) {
      state.live.captured.push((*id, None));
    }
  }
}

/// Ships what the publication that just committed recorded of each live-shipped volume ([`published`]).
pub(crate) fn ship_captured(state: &mut ShardState) {
  for (id, record) in std::mem::take(&mut state.live.captured) {
    published(state, id, record.as_deref());
  }
}

/// What to send one holder, built on the owner shard and sent from the control shard.
struct Send {
  object: ObjectId,
  holder: HostId,
  stream: u64,
  bytes: Vec<u8>,
}

/// Whether the volume with `id` is live-shipped, by its policy on this partition.
fn is_live(state: &ShardState, id: slates_db::catalog::VolumeId) -> bool {
  state
    .db
    .partition()
    .volume(id)
    .is_some_and(|volume| volume.policy.live_shipped)
}

/// The barrier's hook (A-200): `id`'s publication committed locally; `record` is its encoded record as the journal
/// took it, or `None` when the publication was a checkpoint (the whole image is built here). Does nothing for a volume
/// that is not live-shipped, or on a node with no fleet.
pub(crate) fn published(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  record: Option<&[u8]>,
) {
  if !is_live(state, id) {
    state.live.shipping.remove(&id.bytes);
    return;
  }
  let object = ObjectId(id.bytes);
  let configuration = state.fleet.configuration();
  let local = configuration.owner;
  let holders: Vec<HostId> = configuration
    .place(object)
    .candidates
    .into_iter()
    .filter(|host| *host != local)
    .collect();
  let (epoch, quorum) = (configuration.host_epoch, configuration.quorum);
  let live = state
    .live
    .shipping
    .entry(id.bytes)
    .or_insert_with(|| LiveVolume {
      shipper: Shipper::new(
        (local, object, epoch),
        quorum,
        &holders,
        None,
        (CREDIT, RETAIN),
      ),
      owes_whole: true,
      whole: None,
    });
  let whole = live.owes_whole || record.is_none();
  let built = if whole {
    whole_record(state, id)
  } else {
    record.map(<[u8]>::to_vec)
  };
  let Some(record) = built else {
    refuse(state, id);
    return;
  };
  let Some(payload) = payload(state, id, whole, record) else {
    refuse(state, id);
    return;
  };
  let Some(live) = state.live.shipping.get_mut(&id.bytes) else {
    return;
  };
  match live.shipper.append(payload.clone()) {
    Ok(entry) => {
      live.owes_whole = false;
      if whole {
        let head = entry.head();
        live.shipper.mark_checkpoint(head);
        live.whole = Some((head, payload));
      }
      state.count(LIVE_APPENDED, 1);
      let sends = pending_sends(state, id.bytes);
      dispatch(state, sends);
    }
    Err(_) => refuse(state, id),
  }
}

/// A publication that could not become an entry: counted, and the volume owes a whole record next.
fn refuse(state: &mut ShardState, id: slates_db::catalog::VolumeId) {
  state.count(LIVE_REFUSED, 1);
  if let Some(live) = state.live.shipping.get_mut(&id.bytes) {
    live.owes_whole = true;
  }
}

/// The volume's whole image as a record's wire bytes.
fn whole_record(state: &mut ShardState, id: slates_db::catalog::VolumeId) -> Option<Vec<u8>> {
  let handle = *state.by_id.get(&id)?;
  let ShardState { volumes, store, .. } = state;
  let slot = volumes.get_mut(handle).ok()?;
  let host = slot
    .host
    .as_mut()
    .map(|host| host as &mut dyn slates_vfs::host::HostFs);
  let image = slot.volume.to_image(store, host).ok()?;
  Some(VolumeRecord::Full { image }.to_bytes())
}

/// The entry payload of `record`: its kind and body byte, then the body (the record and its content runs), sealed
/// under the volume's lineage key where this node seals.
fn payload(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  whole: bool,
  record: Vec<u8>,
) -> Option<Vec<u8>> {
  let runs = content_runs(state, id, &record)?;
  let body = LivePublication { record, runs }.to_bytes();
  let kind = if whole { KIND_WHOLE } else { KIND_DELTA };
  if state.seal_root.is_none() {
    let mut payload = vec![kind, BODY_CLEAR];
    payload.extend_from_slice(&body);
    return Some(payload);
  }
  let account = crate::seal_keys::tenant_of(&state.db.partition().volume(id)?.owner);
  let lineage = crate::seal_keys::lineage(state, id, account).ok()?;
  let namer = crate::seal_keys::namer(state, id, account).ok()?;
  let chunk = slates_archive::Archive::raw_chunk(body);
  let segment = u32::try_from(rustix::param::page_size()).ok()?;
  let sealed = slates_cluster::sealed::seal_chunk(&lineage, &namer, &chunk, segment).ok()?;
  let mut payload = vec![kind, BODY_SEALED];
  payload.extend_from_slice(
    &SealedBody {
      name: sealed.name.to_vec(),
      bytes: sealed.bytes,
    }
    .to_bytes(),
  );
  Some(payload)
}

/// The content of every extent `record`'s inode images name, read through the volume (sealed chunks open into the
/// buffer, A-99). `None` when a read is refused: the entry would be missing bytes, so it is not made.
fn content_runs(
  state: &ShardState,
  id: slates_db::catalog::VolumeId,
  record: &[u8],
) -> Option<Vec<LiveRun>> {
  let decoded = <VolumeRecord as Wire>::decode(&mut &record[..]).ok()?;
  let inodes: Vec<&InodeImage> = match &decoded {
    VolumeRecord::Full { image } => image.inodes.iter().collect(),
    VolumeRecord::Delta { delta } => delta
      .inodes
      .iter()
      .chain(delta.patches.iter().map(|patch| &patch.inode))
      .collect(),
  };
  let handle = *state.by_id.get(&id)?;
  let slot = state.volumes.get(handle).ok()?;
  let mut runs = Vec::new();
  for inode in inodes {
    let BodyImage::Chunked { extents, open } = &inode.body else {
      continue;
    };
    let mut spans: Vec<(u64, u64)> = extents
      .iter()
      .filter(|extent| matches!(extent.source, ExtentSourceImage::Chunk { .. }))
      .map(|extent| (extent.offset, extent.len))
      .collect();
    if let Some(open) = open {
      spans.push((open.offset, open.len));
    }
    for (offset, len) in spans {
      let mut bytes = vec![0u8; usize::try_from(len).ok()?];
      let read = slot
        .volume
        .read(&state.store, InodeNo(inode.no), offset, &mut bytes)
        .ok()?;
      bytes.truncate(read);
      runs.push(LiveRun {
        inode: inode.no,
        offset,
        bytes,
      });
    }
  }
  Some(runs)
}

/// What each holder of the volume lacks, as messages: entries, or an install from the last whole entry.
fn pending_sends(state: &ShardState, volume: [u8; 16]) -> Vec<Send> {
  let Some(live) = state.live.shipping.get(&volume) else {
    return Vec::new();
  };
  let object = ObjectId(volume);
  let mut sends = Vec::new();
  for holder in live.shipper.holders() {
    match live.shipper.pending(*holder) {
      Next::Entries(entries) => {
        for entry in entries {
          sends.push(Send {
            object,
            holder: *holder,
            stream: slates_cluster::live::LIVE_ENTRY_STREAM,
            bytes: entry.encode(),
          });
        }
      }
      Next::Install => {
        if let Some((head, payload)) = &live.whole {
          let install = Install {
            owner: live.shipper.owner(),
            object,
            epoch: live.shipper.epoch(),
            head: *head,
            state: payload.clone(),
          };
          sends.push(Send {
            object,
            holder: *holder,
            stream: slates_cluster::live::LIVE_INSTALL_STREAM,
            bytes: install.encode(),
          });
        }
      }
      Next::Nothing | Next::Fenced(_) => {}
    }
  }
  sends
}

/// Ships `sends` from the control shard and folds the replies on this (the owner) shard, in a task of this shard: the
/// barrier that called it does not wait (A-200's next slice makes it). The task is bounded by the dispatch's budget and
/// detached, so nothing joins it and its slot is reaped when it ends.
fn dispatch(state: &ShardState, sends: Vec<Send>) {
  if sends.is_empty() {
    return;
  }
  let (origin, control) = (
    state.shard,
    state.shards.first().copied().unwrap_or(state.shard),
  );
  let spawned = slates_rt::futures::spawn(async move {
    let replies = send_from_control(origin, control, sends).await;
    let _ = state::with_state(|s| fold_into(s, replies));
  });
  if let Ok(task) = spawned {
    let _ = slates_rt::futures::detach(task);
  }
}

/// Sends each message over the control shard's session to its holder and brings back every reply that came in the
/// dispatch's budget; the stragglers' replies follow through [`fold_into`] when they come.
async fn send_from_control(
  origin: u16,
  control: u16,
  sends: Vec<Send>,
) -> Vec<(ObjectId, HostId, Vec<u8>)> {
  let budget = crate::fleet::live_budget();
  let deadline = budget.max_deadline_ns().saturating_mul(2);
  crate::xshard::task_within(
    origin,
    control,
    move || async move {
      let mut replies = Vec::new();
      for send in sends {
        let sessions =
          crate::fleet::take_sessions("fleet.lent_to.live_ship", |host| host == send.holder);
        let exchanged =
          slates_cluster::live::exchange((send.stream, &send.bytes), sessions, budget, |_, _| true)
            .await;
        crate::fleet::return_sessions(exchanged.reusable);
        replies.extend(
          exchanged
            .replies
            .into_iter()
            .map(|(host, bytes)| (send.object, host, bytes)),
        );
        settle_stragglers(origin, send.object, exchanged.stragglers);
      }
      replies
    },
    deadline,
  )
  .await
  .unwrap_or_default()
}

/// Returns a dispatch's stragglers' sessions to the pool as they come back, and their replies to the owner shard, in a
/// detached task bounded by the dispatch's own span (each straggler's exchange ends at its deadline).
fn settle_stragglers(origin: u16, object: ObjectId, mut stragglers: slates_cluster::Stragglers) {
  let spawned = slates_rt::futures::spawn(async move {
    loop {
      let (late, done) = stragglers.recover_replies();
      let mut replies = Vec::new();
      let mut sessions = Vec::new();
      for (host, reply, endpoint) in late {
        replies.push((object, host, reply.bytes));
        sessions.push((host, endpoint));
      }
      crate::fleet::return_sessions(sessions);
      if !replies.is_empty() {
        let control = state::with_state(|s| s.shard).unwrap_or(origin);
        crate::xshard::run_on_counted(control, origin, move |s| fold_into(s, replies));
      }
      if done {
        return;
      }
      if slates_rt::futures::within(
        crate::fleet::live_budget().max_deadline_ns(),
        slates_rt::futures::idle(),
      )
      .await
      .is_err()
      {
        return;
      }
    }
  });
  if let Ok(task) = spawned {
    let _ = slates_rt::futures::detach(task);
  }
}

/// Folds replies into the owner shard's shippers and ships what the holders still lack.
fn fold_into(state: &mut ShardState, replies: Vec<(ObjectId, HostId, Vec<u8>)>) {
  let mut touched = Vec::new();
  for (object, host, bytes) in replies {
    let Some(live) = state.live.shipping.get_mut(&object.0) else {
      continue;
    };
    let before = live.shipper.durable();
    let reply = decode_reply(&bytes);
    // Only an answer that names a head moves anything; a refusal (a holder that does not hold the volume's catalog
    // yet, or over its bound) is left to the period. Re-shipping on a refusal looped at the network's speed: 982 and
    // 5,501 refusals on the two holders of one five-write run (2026-11-05, `live_fleet`).
    let progressed = matches!(
      reply,
      Ok(Some(_)) | Err(LiveError::Behind { head: Some(_) })
    );
    if let Next::Fenced(_) = live.shipper.on_reply(host, reply) {
      // A later epoch holds the volume: this owner was superseded and ships no more of it.
      state.live.shipping.remove(&object.0);
      continue;
    }
    if live.shipper.durable() > before {
      state.count(LIVE_DURABLE, 1);
    }
    if progressed && !touched.contains(&object.0) {
      touched.push(object.0);
    }
  }
  let mut sends = Vec::new();
  for volume in touched {
    sends.extend(pending_sends(state, volume));
  }
  dispatch(state, sends);
}

/// The record period's turn on `shard` (run from the control shard): re-ships what each holder of each live volume
/// there still lacks, so a lost message or a holder that was away is caught up without waiting for the next write.
pub(crate) async fn period(origin: u16, shard: u16) {
  let _ = crate::xshard::call_within(
    origin,
    shard,
    |s| {
      let volumes: Vec<[u8; 16]> = s.live.shipping.keys().copied().collect();
      // A volume that owes a whole record ships one once its credit allows: a burst that outran the credit left its
      // last writes unshipped until another write came, and none might (three of five writes, `live_fleet`).
      for volume in &volumes {
        let owed = s
          .live
          .shipping
          .get(volume)
          .is_some_and(|live| live.owes_whole && live.shipper.in_flight() < CREDIT);
        if owed {
          published(s, slates_db::catalog::VolumeId { bytes: *volume }, None);
        }
      }
      let mut sends = Vec::new();
      for volume in volumes {
        sends.extend(pending_sends(s, volume));
      }
      dispatch(s, sends);
    },
    crate::fleet::live_budget().max_deadline_ns(),
  )
  .await;
}

/// This shard's live logs for `status` (A-200): each it ships, then each it holds as a candidate.
pub(crate) fn reports(state: &ShardState) -> Vec<slates_ipc::protocol::LiveReport> {
  let shipping =
    state
      .live
      .shipping
      .iter()
      .map(|(volume, live)| slates_ipc::protocol::LiveReport {
        volume: slates_ipc::protocol::VolumeId { bytes: *volume },
        holding: false,
        head: live.shipper.head().map(|head| head.position),
        durable: live.shipper.durable(),
        owed: live.owes_whole,
      });
  let held = state
    .live
    .held
    .iter()
    .map(|(object, held)| slates_ipc::protocol::LiveReport {
      volume: slates_ipc::protocol::VolumeId { bytes: object.0 },
      holding: true,
      head: held.replica.head().map(|head| head.position),
      durable: None,
      owed: false,
    });
  shipping.chain(held).collect()
}

/// The bound on what a candidate keeps of one volume: its whole record and the deltas after it are each at most the
/// volume's size, and the owner ships a whole record again once its own journal takes a checkpoint, which it does once
/// the deltas since the last reach the checkpoint's size (`publish_captured`).
/// Derived: twice the volume's size limit from its catalog record; `None` (refused) for a volume whose catalog this
/// node does not hold yet (the period ships again once the catalog record arrives).
fn hold_bound(state: &ShardState, object: ObjectId) -> Option<usize> {
  let catalog = object.catalog();
  let (_, value) = state.holder_records.get(&catalog)?.highest(catalog)?;
  let catalog = crate::catalog::CatalogValue::from_record_bytes(value)?;
  let limit = match catalog.size {
    slates_db::catalog::SizeClass::Bounded { limit } => limit,
    slates_db::catalog::SizeClass::Dynamic { max } => max,
  };
  usize::try_from(limit.saturating_mul(2)).ok()
}

/// A candidate's answer to an entry from `peer` (A-200): authorized as a content placement (`fleet::content_authorized`),
/// checked against the replica's chain and fence, kept within its bound, answered with the head held.
pub(crate) fn serve_entry(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Some(bound) = admitted(state, local, peer, slates_db::live::object_of(request)) else {
    state.count(LIVE_HOLD_REFUSED, 1);
    return encode_reply(&Err(LiveError::Unauthorized));
  };
  let reply = Entry::decode(request, bound).and_then(|entry| {
    let held = held_entry(state, peer, entry.object);
    match held.replica.check(&entry)? {
      Admitted::Apply(head) => {
        keep(held, &entry.payload, bound)?;
        held.replica.commit(entry.epoch, head);
        Ok(Some(head))
      }
      Admitted::Held => Ok(held.replica.head()),
    }
  });
  match reply {
    Err(_) => state.count(LIVE_HOLD_REFUSED, 1),
    Ok(_) => state.count(LIVE_HELD, 1),
  }
  encode_reply(&reply)
}

/// A candidate's answer to an install from `peer`, as [`serve_entry`]: the install's state replaces what it held.
pub(crate) fn serve_install(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Some(bound) = admitted(state, local, peer, slates_db::live::object_of(request)) else {
    state.count(LIVE_HOLD_REFUSED, 1);
    return encode_reply(&Err(LiveError::Unauthorized));
  };
  let reply = Install::decode(request, bound).and_then(|install| {
    let held = held_entry(state, peer, install.object);
    match held.replica.check_install(&install)? {
      Admitted::Apply(head) => {
        held.entries.clear();
        held.bytes = 0;
        keep(held, &install.state, bound)?;
        held.replica.commit(install.epoch, head);
        Ok(Some(head))
      }
      Admitted::Held => Ok(held.replica.head()),
    }
  });
  match reply {
    Err(_) => state.count(LIVE_HOLD_REFUSED, 1),
    Ok(_) => state.count(LIVE_HELD, 1),
  }
  encode_reply(&reply)
}

/// The bound for a message naming `object`, when `peer` may place it on this node.
fn admitted(
  state: &ShardState,
  local: HostId,
  peer: HostId,
  object: Option<ObjectId>,
) -> Option<usize> {
  let object = object?;
  let regional = state.council.configuration();
  let placing = slates_cluster::content::ContentAccess::Place;
  crate::fleet::content_authorized(
    regional,
    &state.holder_records,
    local,
    peer,
    placing,
    object,
  )
  .then(|| hold_bound(state, object))
  .flatten()
}

fn held_entry(state: &mut ShardState, owner: HostId, object: ObjectId) -> &mut Held {
  state.live.held.entry(object).or_insert_with(|| Held {
    replica: Replica::new(owner, object),
    entries: VecDeque::new(),
    bytes: 0,
  })
}

/// Keeps `payload` after what is held (a whole one first drops what came before it), within `bound`.
fn keep(held: &mut Held, payload: &[u8], bound: usize) -> Result<(), LiveError> {
  if payload.first() == Some(&KIND_WHOLE) {
    held.entries.clear();
    held.bytes = 0;
  }
  let bytes = held
    .bytes
    .checked_add(payload.len())
    .ok_or(LiveError::Malformed)?;
  if bytes > bound {
    return Err(LiveError::Malformed);
  }
  held.entries.push_back(payload.to_vec());
  held.bytes = bytes;
  Ok(())
}
