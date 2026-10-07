//! The mirror region's records (§4.10 "Mirroring across regions"; `docs/wip/mirroring.md` piece M4): once a
//! snapshot's content is held at `f + 1` of the owner's neighbourhood in its mirror region, the owner ships the
//! volume's head record — naming those mirror holders as its content holders, and carrying the volume's lineage key
//! wrapped for each of them — and its catalog record, as one shipment to each mirror holder. A holder admits it only
//! under the mirror rule (`slates_db::mirror::admits_mirror_put`: a member of the object's standing home region, at
//! that home's declared mirror), keeps the newest of each in its `MirrorRecords` (ordered by epoch, then sequence),
//! publishes them in its held image, and only then acknowledges (§4.8 persistence before reply). The owner records
//! the snapshot placed in the mirror once `f + 1` mirror holders acknowledged both its content and its records:
//! that is what a promotion's adoption reads, so `await placed(mirror)` never answers before a promoted region could
//! serve the snapshot.

use slates_cluster::{CommitBudget, broadcast};
use slates_db::catalog::{PlacementState, VolumeId as DbVolumeId};
use slates_db::register::{HostId, ObjectId, Quorum, Record};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;
use slates_wire::Wire;

use crate::fleet::{Dispatch, LateReplies, return_sessions, take_sessions};
use crate::head::HeadValue;
use crate::state::{self, ShardState};

/// Format: the stream a mirror shipment rides, after the pair key's (19).
pub(crate) const MIRROR_RECORD_STREAM: u64 = 20;

/// One object's records for the mirror: its head and its catalog, each an encoded [`Record`].
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Shipment {
  head: Vec<u8>,
  catalog: Vec<u8>,
}

/// A mirror holder's acknowledgement of a shipment: the object and the head sequence it now holds durably.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Wire)]
struct ShipmentAck {
  object: [u8; 16],
  sequence: u64,
}

/// The status count of mirror shipments a holder refused: the mirror rule, a malformed record, or an older record
/// than it holds. Format: a refusal name in the daemon's status report.
const SHIPMENT_REFUSED: &str = "fleet.mirror.shipment_refused";

/// The status count of mirror shipments a holder accepted but did not acknowledge because the publish carrying them
/// was refused. Format: a refusal name in the daemon's status report.
const SHIPMENT_UNPUBLISHED: &str = "fleet.mirror.shipment_unpublished";

/// Serves one mirror shipment from `peer` on this holder's control shard: both records the peer's own, the catalog
/// the head's object's, admitted under the mirror rule, kept as the newest of each, published, then acknowledged. An
/// empty reply refuses, counted.
pub(crate) fn serve(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Some((head, catalog)) = decode_shipment(request) else {
    state.count(SHIPMENT_REFUSED, 1);
    return Vec::new();
  };
  let own = state
    .node_regions
    .get(&local)
    .copied()
    .unwrap_or(slates_db::register::RegionId(0));
  let admitted = head.owner == peer
    && catalog.owner == peer
    && catalog.object == head.object.catalog()
    && slates_db::mirror::admits_mirror_put(
      state.root.configuration(),
      &state.region_mirrors,
      &state.node_regions,
      own,
      peer,
      head.object,
    )
    .is_ok();
  if !admitted {
    state.count(SHIPMENT_REFUSED, 1);
    return Vec::new();
  }
  let (object, sequence) = (head.object, head.sequence);
  let accepted =
    state.mirror_records.accept(catalog).is_ok() && state.mirror_records.accept(head).is_ok();
  if !accepted {
    state.count(SHIPMENT_REFUSED, 1);
    return Vec::new();
  }
  if crate::verbs::publish_shard(state).is_err() {
    state.count(SHIPMENT_UNPUBLISHED, 1);
    return Vec::new();
  }
  ShipmentAck {
    object: object.0,
    sequence,
  }
  .to_bytes()
}

/// The two records of a shipment, or `None` for bytes that are not one.
fn decode_shipment(request: &[u8]) -> Option<(Record, Record)> {
  let shipment = Shipment::from_bytes(request).ok()?;
  let head = Record::decode(&shipment.head).ok()?;
  let catalog = Record::decode(&shipment.catalog).ok()?;
  Some((head, catalog))
}

/// One object whose mirror shipment is owed: the shipment's bytes, its head sequence, and the mirror holders of its
/// content still missing its records.
struct Owed {
  object: ObjectId,
  sequence: u64,
  shipment: Vec<u8>,
  targets: Vec<HostId>,
}

/// The shipments this owner shard owes: every mirror job whose content `f + 1` mirror holders hold, to those of them
/// that have not yet acknowledged its records. Built on the owner shard, where the volumes and their catalogs live.
fn owed(state: &ShardState, local: HostId, quorum: Quorum) -> Vec<Owed> {
  let epoch = state.fleet.configuration().host_epoch;
  let mut owed = Vec::new();
  for (object, job) in &state.mirror_seals {
    let Some(manifest) = job.manifest else {
      continue;
    };
    if !job.content.placed(quorum) {
      continue;
    }
    let targets: Vec<HostId> = job
      .content
      .acked
      .iter()
      .copied()
      .filter(|host| !job.records_acked.contains(host))
      .collect();
    if targets.is_empty() {
      continue;
    }
    let id = DbVolumeId { bytes: object.0 };
    let Some(volume) = state.db.partition().volume(id) else {
      continue;
    };
    let value = HeadValue {
      sealing: crate::seal_keys::head_sealing(state, id, &volume.owner),
      manifest: Some(manifest),
      content_holders: job.content.acked.iter().map(|host| host.0).collect(),
    };
    let record = |object: ObjectId, sequence: u64, value: Vec<u8>| Record {
      owner: local,
      object,
      sequence,
      epoch,
      generation: 0,
      value,
    };
    let shipment = Shipment {
      head: record(*object, job.sequence, value.to_record_bytes()).encode(),
      catalog: record(
        object.catalog(),
        volume.catalog_version,
        crate::catalog::CatalogValue::of(volume).to_record_bytes(),
      )
      .encode(),
    };
    owed.push(Owed {
      object: *object,
      sequence: job.sequence,
      shipment: shipment.to_bytes(),
      targets,
    });
  }
  owed
}

/// One period of mirror record shipping for the volumes `shard` owns (run from the control shard, which holds the
/// fleet's sessions): each owed shipment sent to its targets, and every binding acknowledgement folded into its job
/// on the owner shard. A holder that does not answer this period is asked again next period.
pub(crate) async fn ship(
  origin: u16,
  shard: u16,
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let owed = crate::xshard::call_within(
    origin,
    shard,
    move |s| owed(s, local, s.fleet.configuration().quorum),
    crate::daemon::HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for item in owed {
    let sessions = take_sessions(|host| item.targets.contains(&host));
    if sessions.is_empty() {
      continue;
    }
    let sent: Vec<HostId> = sessions.iter().map(|(host, _)| *host).collect();
    let outgoing: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
      .into_iter()
      .map(|(host, endpoint)| (host, item.shipment.clone(), endpoint))
      .collect();
    let (replied, stragglers) =
      broadcast(outgoing, MIRROR_RECORD_STREAM, Priority::Control, budget).await;
    let mut acked = Vec::new();
    let mut recovered = Vec::with_capacity(replied.len());
    for (host, reply, endpoint) in replied {
      if ShipmentAck::from_bytes(&reply.bytes)
        .is_ok_and(|ack| ack.object == item.object.0 && ack.sequence == item.sequence)
      {
        acked.push(host);
      }
      recovered.push((host, endpoint));
    }
    in_flight.push(Dispatch::new(
      sent,
      &recovered,
      stragglers,
      LateReplies::Discard,
    ));
    return_sessions(recovered);
    if acked.is_empty() {
      continue;
    }
    let (object, sequence) = (item.object, item.sequence);
    crate::xshard::run_on_counted(origin, shard, move |s| {
      if let Some(job) = s.mirror_seals.get_mut(&object)
        && job.sequence == sequence
      {
        for host in acked {
          if !job.records_acked.contains(&host) {
            job.records_acked.push(host);
          }
        }
      }
    });
  }
}

/// The mirror holders that hold a mirror job's content and its records: what a promotion can adopt from. Placed
/// once these are `f + 1`.
pub(crate) fn holders_of(job: &crate::head::SealJob) -> Vec<HostId> {
  job
    .content
    .acked
    .iter()
    .copied()
    .filter(|host| job.records_acked.contains(host))
    .collect()
}

/// Whether a snapshot is recorded placed in the mirror: `f + 1` mirror holders hold its content and its records.
pub(crate) fn mirror_placed(placed: &PlacementState, quorum: Quorum) -> bool {
  match placed {
    PlacementState::Placed {
      mirror: Some(mirror),
      ..
    } => quorum.committed(mirror.len()),
    _ => false,
  }
}

// ------------------------------------------------------------------------------------------------ promotion adoption
//
// `docs/wip/mirroring.md` decision 4: once the root group's promotion of a lost region is installed here, the mirror
// region's council leader runs phase one over every council member, waits for all but `f` complete answers, picks each
// object's successor among the holders its newest head record names, and commits the assignments to the council
// log (`ConfigCommand::MirrorAdopt`). A successor seeds the takeover's own materialization queue from its assignment,
// and reports the object adopted (`ConfigCommand::MirrorAdopted`) once the volume is built.

/// Format: the stream a promotion's phase one rides, after the mirror shipment's (20).
pub(crate) const MIRROR_PROMISE_STREAM: u64 = 21;

/// A leader's page request: the lost region, the root version whose promotion of it the answer must stand behind,
/// and where to continue (an object id, empty to begin).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct PromiseRequest {
  lost: u64,
  root_version: u64,
  after: Vec<u8>,
}

/// A member's page of its mirror records of the lost region's objects: each object's head then its catalog record
/// (encoded), and where the next page starts (empty when this one ends the answer).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct PromisePage {
  holder: u64,
  root_version: u64,
  entries: Vec<Vec<u8>>,
  next: Vec<u8>,
}

/// One lost region's adoption round on the council leader: each member's cursor (complete once a page names no next
/// page), the newest head and catalog records learned per object, and the objects already proposed. Bounded by the
/// council's members and the lost region's objects this region mirrors.
#[derive(Debug, Default)]
pub(crate) struct PromotionRound {
  cursors: std::collections::BTreeMap<HostId, (Vec<u8>, bool)>,
  newest: std::collections::BTreeMap<ObjectId, (Record, Record)>,
  proposed: std::collections::BTreeSet<ObjectId>,
}

/// The region this node is in.
fn own_region(state: &ShardState, local: HostId) -> slates_db::register::RegionId {
  state
    .node_regions
    .get(&local)
    .copied()
    .unwrap_or(slates_db::register::RegionId(0))
}

/// The home region of `object` as its creator's, moved by the root's homes, before promotions: the region a mirror
/// record of it was shipped from.
fn shipped_from(state: &ShardState, object: ObjectId) -> slates_db::register::RegionId {
  let root = state.root.configuration();
  root.homes.get(&object).copied().unwrap_or_else(|| {
    state
      .node_regions
      .get(&object.creator())
      .copied()
      .unwrap_or(slates_db::register::RegionId(0))
  })
}

/// The status count of promise requests a member refused: an asker outside the council, or a promotion this member
/// has not installed (its answer would not stand behind the fence). Format: a refusal name in the status report.
const PROMISE_REFUSED: &str = "fleet.mirror.promise_refused";

/// Answers one page of a promotion's phase one (`docs/wip/mirroring.md` decision 4) for `peer`, a council member: only
/// once this member has installed the root's promotion of the lost region to its own (so it refuses every later
/// record from there, M1's `HomePromoted`), its mirror records of that region's objects from the cursor on, within one
/// page budget. Empty refuses, counted.
pub(crate) fn serve_promise(
  state: &mut ShardState,
  local: HostId,
  peer: HostId,
  request: &[u8],
) -> Vec<u8> {
  let Ok(request) = PromiseRequest::from_bytes(request) else {
    state.count(PROMISE_REFUSED, 1);
    return Vec::new();
  };
  let lost = slates_db::register::RegionId(request.lost);
  let own = own_region(state, local);
  let root = state.root.configuration();
  let installed = root.promotions.get(&lost) == Some(&own) && root.version >= request.root_version;
  if !installed || !state.council.configuration().members.contains(&peer) {
    state.count(PROMISE_REFUSED, 1);
    return Vec::new();
  }
  let root_version = root.version;
  let after = <[u8; 16]>::try_from(request.after.as_slice())
    .ok()
    .map(ObjectId);
  let budget = crate::takeover::page_budget();
  let mut entries = Vec::new();
  let mut used = 0usize;
  let mut next = Vec::new();
  for (object, head) in state.mirror_records.iter() {
    if object.is_catalog() || after.is_some_and(|after| *object <= after) {
      continue;
    }
    if shipped_from(state, *object) != lost {
      continue;
    }
    let Some(catalog) = state.mirror_records.newest(object.catalog()) else {
      continue;
    };
    let (head, catalog) = (head.encode(), catalog.encode());
    let size = head.len().saturating_add(catalog.len());
    if used > 0 && used.saturating_add(size) > budget {
      next = after_of(&entries);
      break;
    }
    used = used.saturating_add(size);
    entries.push(head);
    entries.push(catalog);
  }
  PromisePage {
    holder: local.0,
    root_version,
    entries,
    next,
  }
  .to_bytes()
}

/// The cursor after a page's last head record.
fn after_of(entries: &[Vec<u8>]) -> Vec<u8> {
  // Entries alternate head, catalog: the last head is the second-to-last entry.
  entries
    .get(entries.len().saturating_sub(2))
    .and_then(|head| Record::decode(head).ok())
    .map(|head| head.object.0.to_vec())
    .unwrap_or_default()
}

/// Folds one member's page into a round: its records learned, keeping the newest head per object by (epoch,
/// sequence) with the catalog shipped beside it, and its cursor advanced (complete once the page names no next).
fn fold_page(round: &mut PromotionRound, member: HostId, page: &PromisePage) {
  let mut entries = page.entries.iter();
  while let (Some(head), Some(catalog)) = (entries.next(), entries.next()) {
    let (Ok(head), Ok(catalog)) = (Record::decode(head), Record::decode(catalog)) else {
      continue;
    };
    let newer = round
      .newest
      .get(&head.object)
      .is_none_or(|(held, _)| (head.epoch.0, head.sequence) > (held.epoch.0, held.sequence));
    if newer {
      round.newest.insert(head.object, (head, catalog));
    }
  }
  round
    .cursors
    .insert(member, (page.next.clone(), page.next.is_empty()));
}

/// The successor of `object` from its newest head: the first, by rendezvous, of the holders the head names that are
/// council members — each holds the content and was given the volume's lineage key wrapped for it.
fn successor_of(head: &Record, members: &[HostId]) -> Option<HostId> {
  let holders: Vec<HostId> = HeadValue::from_record_bytes(&head.value)?
    .content_holders
    .into_iter()
    .map(HostId)
    .filter(|host| members.contains(host))
    .collect();
  slates_db::register::rendezvous_first(&holders, head.object)
}

/// One period of the promotion rounds this node leads (`docs/wip/mirroring.md` decision 4): for each lost region the
/// root promoted to this node's region, the members still answering asked for their next page, every answer folded,
/// and — once all members but `f` answered completely — the next page budget of assignments proposed to the council.
/// Only a caught-up council leader runs it. Returns the dispatches of the round's stragglers.
pub(crate) async fn drive_promotion(local: HostId, budget: CommitBudget) -> Vec<Dispatch> {
  let Some(work) = state::with_state(|s| promotion_requests(s, local)).flatten() else {
    return Vec::new();
  };
  let mut dispatches = Vec::new();
  for (lost, root_version, requests) in work {
    // This node is a member too: its own page is read directly.
    let own = requests
      .iter()
      .find(|(host, _)| *host == local)
      .map(|(_, request)| request.clone());
    if let Some(request) = own {
      state::with_state(|s| {
        let reply = serve_promise(s, local, local, &request.to_bytes());
        if let Ok(page) = PromisePage::from_bytes(&reply)
          && let Some(round) = s.mirror_rounds.get_mut(&lost)
        {
          fold_page(round, local, &page);
        }
      });
    }
    let wanted: std::collections::BTreeMap<HostId, PromiseRequest> = requests
      .into_iter()
      .filter(|(host, _)| *host != local)
      .collect();
    let sessions = take_sessions(|host| wanted.contains_key(&host));
    if !sessions.is_empty() {
      let sent: Vec<HostId> = sessions.iter().map(|(host, _)| *host).collect();
      let outgoing: Vec<(HostId, Vec<u8>, Endpoint)> = sessions
        .into_iter()
        .filter_map(|(host, endpoint)| {
          wanted
            .get(&host)
            .map(|request| (host, request.to_bytes(), endpoint))
        })
        .collect();
      let (replied, stragglers) =
        broadcast(outgoing, MIRROR_PROMISE_STREAM, Priority::Control, budget).await;
      let mut recovered = Vec::with_capacity(replied.len());
      let mut pages = Vec::new();
      for (host, reply, endpoint) in replied {
        if let Ok(page) = PromisePage::from_bytes(&reply.bytes)
          && page.holder == host.0
          && page.root_version >= root_version
        {
          pages.push((host, page));
        }
        recovered.push((host, endpoint));
      }
      dispatches.push(Dispatch::new(
        sent,
        &recovered,
        stragglers,
        LateReplies::Discard,
      ));
      return_sessions(recovered);
      state::with_state(|s| {
        if let Some(round) = s.mirror_rounds.get_mut(&lost) {
          for (host, page) in &pages {
            fold_page(round, *host, page);
          }
        }
      });
    }
    state::with_state(|s| propose_adoptions(s, lost));
  }
  dispatches
}

/// One lost region's page requests this period: the region, the root version they stand behind, and each member's.
type RoundRequests = (
  slates_db::register::RegionId,
  u64,
  Vec<(HostId, PromiseRequest)>,
);

/// The page requests this leader's rounds owe this period, by lost region with the root version they stand behind:
/// one per council member whose answer is not complete. `None` unless this node leads a caught-up council.
fn promotion_requests(state: &mut ShardState, local: HostId) -> Option<Vec<RoundRequests>> {
  if !state.council.is_leader() || !state.council.caught_up() {
    return None;
  }
  let own = own_region(state, local);
  let root = state.root.configuration().clone();
  let members = state.council.configuration().members.clone();
  let mut work = Vec::new();
  for (lost, mirror) in &root.promotions {
    if *mirror != own {
      continue;
    }
    let round = state.mirror_rounds.entry(*lost).or_default();
    let requests: Vec<(HostId, PromiseRequest)> = members
      .iter()
      .filter(|member| {
        !round
          .cursors
          .get(member)
          .is_some_and(|(_, complete)| *complete)
      })
      .map(|member| {
        let after = round
          .cursors
          .get(member)
          .map(|(after, _)| after.clone())
          .unwrap_or_default();
        (
          *member,
          PromiseRequest {
            lost: lost.0,
            root_version: root.version,
            after,
          },
        )
      })
      .collect();
    work.push((*lost, root.version, requests));
  }
  // A round for a region no longer promoted here is dropped.
  state
    .mirror_rounds
    .retain(|lost, _| root.promotions.get(lost) == Some(&own));
  Some(work)
}

/// Proposes the next page budget of `lost`'s assignments once all council members but `f` answered completely: each
/// object's successor by [`successor_of`], skipping one with no member among its holders (counted: it cannot be
/// served here).
fn propose_adoptions(state: &mut ShardState, lost: slates_db::register::RegionId) {
  let members = state.council.configuration().members.clone();
  let f = usize::try_from(state.council.configuration().quorum.f).unwrap_or(usize::MAX);
  let needed = members.len().saturating_sub(f).max(1);
  let Some(round) = state.mirror_rounds.get(&lost) else {
    return;
  };
  let complete = members
    .iter()
    .filter(|member| {
      round
        .cursors
        .get(member)
        .is_some_and(|(_, complete)| *complete)
    })
    .count();
  if complete < needed {
    return;
  }
  let budget = crate::takeover::page_budget();
  let mut used = 0usize;
  let mut proposals = Vec::new();
  let mut unservable = 0u64;
  for (object, (head, catalog)) in &round.newest {
    if round.proposed.contains(object) {
      continue;
    }
    let Some(successor) = successor_of(head, &members) else {
      unservable = unservable.saturating_add(1);
      continue;
    };
    let adoption = slates_db::register::Adoption {
      successor,
      head: head.encode(),
      catalog: catalog.encode(),
    };
    let size = adoption.head.len().saturating_add(adoption.catalog.len());
    if used > 0 && used.saturating_add(size) > budget {
      break;
    }
    used = used.saturating_add(size);
    proposals.push((*object, adoption));
  }
  for (object, adoption) in proposals {
    let assigned = state.council.configuration().adoptions.get(&object) == Some(&adoption);
    let proposed = assigned
      || state
        .council
        .propose(slates_cluster::config_group::Reconfiguration::MirrorAdopt { object, adoption });
    if proposed && let Some(round) = state.mirror_rounds.get_mut(&lost) {
      round.proposed.insert(object);
    }
  }
  if unservable > 0 {
    state.count(PROMOTION_UNSERVABLE, unservable);
  }
}

/// The status count of promoted objects whose newest head names no council member among its holders: nothing in
/// this region holds what it would serve. Format: a refusal name in the daemon's status report.
const PROMOTION_UNSERVABLE: &str = "fleet.mirror.promotion_unservable";

/// One period of this node's assigned adoptions (`docs/wip/mirroring.md` decision 4): each object the council assigned
/// to it is seeded into the takeover's materialization queue once (its head and catalog placements under this node,
/// the adopted catalog and head), and — once the queue has built it — owned in the routing and reported adopted.
pub(crate) async fn adopt_assigned(
  local: HostId,
  budget: CommitBudget,
  in_flight: &mut Vec<Dispatch>,
) {
  let reports = state::with_state(|s| seed_assigned(s, local)).unwrap_or_default();
  for object in reports {
    crate::fleet::send_council_report(
      slates_cluster::config_group::ConfigCommand::MirrorAdopted {
        object,
        successor: local,
      },
      local,
      budget,
      in_flight,
    )
    .await;
  }
}

/// Seeds every assignment to `local` not yet seeded, and returns the seeded objects the materialization queue has
/// since built: the reports owed. A seeded object no longer assigned is forgotten.
fn seed_assigned(state: &mut ShardState, local: HostId) -> Vec<ObjectId> {
  let assigned: Vec<(ObjectId, slates_db::register::Adoption)> = state
    .council
    .configuration()
    .adoptions
    .iter()
    .filter(|(_, adoption)| adoption.successor == local)
    .map(|(object, adoption)| (*object, adoption.clone()))
    .collect();
  state
    .mirror_seeded
    .retain(|object| assigned.iter().any(|(held, _)| held == object));
  let mut reports = Vec::new();
  for (object, adoption) in assigned {
    if state.mirror_seeded.contains(&object) {
      // Built: this node now owns it and claims it to lookups; never before, or a lookup routed here would find no
      // volume yet.
      if !state.pending_materializations.contains_key(&object) {
        state.fleet.track_object_owner(object, local);
        reports.push(object);
      }
      continue;
    }
    let (Ok(head), Ok(catalog)) = (
      Record::decode(&adoption.head),
      Record::decode(&adoption.catalog),
    ) else {
      continue;
    };
    let (Some(value), Some(catalog_value)) = (
      HeadValue::from_record_bytes(&head.value),
      crate::catalog::CatalogValue::from_record_bytes(&catalog.value),
    ) else {
      continue;
    };
    let config = state.fleet.configuration();
    let epoch = config.host_epoch;
    let placed = |sequence: u64, object: ObjectId| crate::head::PlacedHead {
      sequence,
      epoch,
      placement: slates_db::register::Placement {
        acked: vec![local],
        ..config.place(object)
      },
    };
    let head_placed = placed(head.sequence, object);
    let catalog_placed = placed(catalog.sequence, object.catalog());
    state.placed_heads.insert(object, head_placed);
    state.placed_heads.insert(object.catalog(), catalog_placed);
    state
      .pending_catalogs
      .insert(object, (catalog.sequence, catalog_value));
    state.pending_materializations.insert(object, value);
    state.mirror_seeded.insert(object);
    state.count(PROMOTION_SEEDED, 1);
  }
  reports
}

/// The status count of promoted objects this node began adopting. Format: a counter name in the status report.
const PROMOTION_SEEDED: &str = "fleet.mirror.promotion_seeded";
