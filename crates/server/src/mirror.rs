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
use crate::state::ShardState;

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
