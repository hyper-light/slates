//! Another host's attachment, recorded and served on the volume's owner (§4.10 "Ownership follows the writer", D-16,
//! A-222). A client on node B naming a volume node A owns attaches it in the record form through the fleet's forward,
//! as any volume-scoped write is forwarded; A records the attachment under `Consumer::Remote { host, client }`, `host`
//! the forwarding peer's authenticated member id (never a claim of the request's), and takes the write lease as for a
//! local writer. The file verbs then forward as they already did, and A serves them only through an attachment that is
//! the requester's own: a remote one from its own host, a local one from this node (`may_use`). Only the record form
//! crosses hosts: every other form hands the client something of the owner's host, so it is refused by its kind.
//!
//! **How a remote attachment ends.** A detach routed by its volume (`DetachOf`; an attachment id names its owner
//! partition, not its host); its host's reap of the dead client (`RemoteReap`, which B sends the owner of each volume
//! it remembers the client attached, a set bounded per client); its host's retirement from the fleet (`end_retired`);
//! and the volume's destroy, as every attachment. A silent holder's write lease ends at its term, `failover_slo_ns`
//! (`verbs::take_write_lease`, D-16), whatever ends its record.
//!
//! **R8.** At one node nothing is forwarded, no attachment is `Remote`, and every rule here holds the same way for a
//! local requester: the requester's host is this node's, and a local attachment is its own.

use slates_db::Op;
use slates_db::catalog::{AttachmentRecord, Consumer};
use slates_ipc::protocol::{AttachRequest, AttachTransport, Refusal, UnsupportedReason};

use crate::state::ShardState;

/// The transport a form asks for, for its refusal by kind.
fn transport_of(form: &AttachRequest) -> AttachTransport {
  match form {
    AttachRequest::Root => AttachTransport::Root,
    AttachRequest::HostMount
    | AttachRequest::ScopedHostMount { .. }
    | AttachRequest::BoundHostMount { .. } => AttachTransport::NfsLoopback,
    AttachRequest::FuseMount { .. }
    | AttachRequest::ScopedFuseMount { .. }
    | AttachRequest::SharedFuseMount { .. } => AttachTransport::Fuse,
    AttachRequest::WinFspMount { .. } => AttachTransport::WinFsp,
    AttachRequest::Oci { .. } => AttachTransport::Oci,
    AttachRequest::Guest { transport } => *transport,
    AttachRequest::ShardsFs { .. } => AttachTransport::ShardsFs,
    AttachRequest::VhostUserFs { .. } => AttachTransport::VirtioFsInheritedDescriptor,
  }
}

/// The refusal a form other than the record form gets when its volume's owner is another host: by its kind.
pub(crate) fn foreign_form(form: &AttachRequest) -> Option<Refusal> {
  if matches!(form, AttachRequest::Root) {
    return None;
  }
  Some(Refusal::AttachmentUnsupported {
    transport: transport_of(form),
    reason: UnsupportedReason::OwnedByAnotherHost,
  })
}

/// Whether `volume` is, as far as this node can tell without asking, another host's: created by another host, in a
/// fleet with another member, and not owned here after a takeover (§4.8 "Lookup": the creator is the id's high half).
fn elsewhere(state: &ShardState, volume: slates_ipc::protocol::VolumeId) -> bool {
  let local = state.fleet.host();
  let object = slates_db::register::ObjectId(volume.bytes);
  object.creator() != local
    && state.fleet.object_owner(object) != Some(local)
    && state.fleet.members().iter().any(|member| *member != local)
}

/// The refusal, on the client's node, of an attach in a form other than the record form of another host's volume:
/// by its kind, before anything is sent (A-222). `None` for every other request.
pub(crate) fn foreign_attach(
  state: &ShardState,
  body: &slates_ipc::protocol::RequestBody,
) -> Option<Refusal> {
  let slates_ipc::protocol::RequestBody::Attach { volume, form, .. } = body else {
    return None;
  };
  let refusal = foreign_form(form)?;
  elsewhere(state, *volume).then_some(refusal)
}

/// The host a request being served came from: the forwarding peer for a verb forwarded from another node, this node
/// for a local client's.
fn requester_host(state: &ShardState) -> u64 {
  state
    .current_request
    .map_or(state.fleet.host().0, |(origin, _)| origin)
}

/// The consumer an attachment in `form` is recorded under: a local client's (`verbs::consumer_of`), or, for a verb
/// forwarded from another node (dispatched with no local client), the requesting host's client, by the host's
/// authenticated id. A forwarded form other than the record form is refused by its kind.
pub(crate) fn consumer(
  state: &ShardState,
  form: &AttachRequest,
  client_id: u32,
) -> Result<Consumer, Refusal> {
  if client_id != crate::verbs::FORWARDED_CLIENT {
    return crate::verbs::consumer_of(form, client_id);
  }
  if let Some(refusal) = foreign_form(form) {
    return Err(refusal);
  }
  let client = state.current_request.map_or(0, |(_, id)| id.client);
  Ok(Consumer::Remote {
    host: requester_host(state),
    client,
  })
}

/// A forwarded verb that is neither a forwardable read nor write, on the owner: a peer's reap of its dead client
/// (`RemoteReap`), which ends that client's attachments under the peer's authenticated id and keeps no completion
/// record (it is idempotent); any other is refused `Unsupported`.
pub(crate) async fn serve_unforwardable(
  control: u16,
  shard: u16,
  host: u64,
  body: slates_ipc::protocol::RequestBody,
) -> slates_ipc::protocol::ReplyBody {
  let slates_ipc::protocol::RequestBody::RemoteReap { volume, client } = body else {
    return crate::verbs::refused(Refusal::Unsupported {
      feature: "forwarded verb".to_owned(),
    });
  };
  let volume = crate::verbs::to_db_volume(volume);
  let ended = crate::xshard::call_within(
    control,
    shard,
    move |state| reap(state, host, volume, client),
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await;
  match ended {
    Some(Ok(_)) => slates_ipc::protocol::ReplyBody::Detached,
    Some(Err(refusal)) => crate::verbs::refused(refusal),
    None => crate::verbs::refused(Refusal::Overloaded { shard }),
  }
}

/// Whether the request being served may act through `record`: an attachment of another host only from that host, and
/// any other only from this node. A forward naming another host's attachment, or this node's, is refused.
pub(crate) fn may_use(state: &ShardState, record: &AttachmentRecord) -> bool {
  let requester = requester_host(state);
  // A local client's request is keyed by this node's anchor id (its completion records' key), and a same-node
  // request by its member id; any other origin is a fleet peer's, authenticated by its session.
  let local = requester == state.origin_anchor.0 || requester == state.fleet.host().0;
  match record.consumer {
    Consumer::Remote { host, .. } => !local && host == requester,
    Consumer::Sdk { .. }
    | Consumer::Bridge
    | Consumer::Launcher
    | Consumer::Mount { .. }
    | Consumer::Guest => local,
  }
}

/// Ends, on this owner shard, every attachment of `volume` that `host`'s `client` holds (`RemoteReap`); the number
/// ended. `host` is the authenticated forwarding peer's, so a node ends only its own clients' attachments.
pub(crate) fn reap(
  state: &mut ShardState,
  host: u64,
  volume: slates_db::catalog::VolumeId,
  client: u32,
) -> Result<u64, Refusal> {
  let ended: Vec<AttachmentRecord> = state
    .db
    .partition()
    .attachments_of(volume)
    .into_iter()
    .filter(|record| record.consumer == Consumer::Remote { host, client })
    .cloned()
    .collect();
  end_all(state, &ended)
}

/// The anchors that left the fleet's members between two looks: in `previous` and not in `now`. A host in neither, one
/// the fleet has not configured yet (a configuration installed while the fleet forms names fewer members than it will),
/// has not departed, though its clients may already hold attachments here: it forwarded over its session before the
/// council configured it. An empty `now` (no configuration yet) departs nobody.
pub(crate) fn departed(previous: &[u64], now: &[u64]) -> Vec<u64> {
  if now.is_empty() {
    return Vec::new();
  }
  previous
    .iter()
    .copied()
    .filter(|host| !now.contains(host))
    .collect()
}

/// On the control shard, where the fleet's learned identities live: the anchors of hosts that left the fleet's members
/// since the last look ([`departed`]). A remote attachment names its host by the forwarding peer's certificate anchor
/// (the id a forward authenticates), while a configuration names members by their current member ids, so each member
/// is mapped to its anchor (`member_task::anchor_of`); a member not learned yet maps to none and departs nobody. The
/// departed anchors are kept owed until every shard has ended their attachments ([`settle_departed`]).
pub(crate) fn departed_anchors(state: &mut ShardState) -> Vec<u64> {
  let members: Vec<slates_db::register::HostId> = state.fleet.members().to_vec();
  let now: Vec<u64> = members
    .iter()
    .filter_map(|member| crate::member_task::anchor_of(state, *member))
    .map(|anchor| anchor.0)
    .collect();
  let mut gone = departed(&state.configured_members, &now);
  gone.append(&mut state.departed_owed);
  gone.sort_unstable();
  gone.dedup();
  if !now.is_empty() {
    state.configured_members = now;
  }
  state.departed_owed.clone_from(&gone);
  gone
}

/// Records that every shard ended the departed anchors' attachments: nothing is owed until another departs.
pub(crate) fn settle_departed(state: &mut ShardState) {
  state.departed_owed.clear();
}

/// Ends, on this shard, every attachment whose host is one of the departed `anchors`: the fleet retired those hosts
/// (§4.8 "Promotion and takeover"), and no reap of their clients will come. A walk of the shard's attachments; the
/// number ended, or the refusal.
pub(crate) fn end_hosts(state: &mut ShardState, anchors: &[u64]) -> Result<u64, Refusal> {
  if anchors.is_empty() {
    return Ok(0);
  }
  let ended: Vec<AttachmentRecord> = state
    .db
    .partition()
    .remote_attachments()
    .into_iter()
    .filter(
      |record| matches!(record.consumer, Consumer::Remote { host, .. } if anchors.contains(&host)),
    )
    .cloned()
    .collect();
  end_all(state, &ended)
}

/// The reap loop's look at departures (A-222), run on the control shard each liveness budget: the departed anchors,
/// then their attachments ended on every shard, each through a call bounded by the budget. Owed again at the next look
/// unless every shard answered.
pub(crate) async fn sweep_departed() {
  let Some((control, shards, gone)) = crate::state::with_state(|state| {
    let control = state.shards.first().copied()?;
    if state.shard != control {
      return None;
    }
    Some((control, state.shards.clone(), departed_anchors(state)))
  })
  .flatten() else {
    return;
  };
  if gone.is_empty() {
    return;
  }
  let mut settled = true;
  for shard in shards {
    let anchors = gone.clone();
    let ended = crate::xshard::call_within(
      control,
      shard,
      move |state| end_hosts(state, &anchors),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    if !matches!(ended, Some(Ok(_))) {
      settled = false;
    }
  }
  if settled {
    let _ = crate::state::with_state(settle_departed);
  }
}

/// Ends each of `records` as a reap ends a dead local client's (`reap::remove_batch`): the record removed, the green
/// pin forgotten, the write lease left to its term (D-16); and the registry attachment its file verbs were admitted
/// under revoked, so no later request is admitted under it.
fn end_all(state: &mut ShardState, records: &[AttachmentRecord]) -> Result<u64, Refusal> {
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  let mut ended: u64 = 0;
  for record in records {
    state
      .db
      .mutate(
        &mut state.segment,
        &Op::AttachmentRemoved { id: record.id },
        now,
      )
      .map_err(|error| crate::error::refusal_of_db(&error))?;
    crate::merge_service::forget_attachment(state, record.id);
    if let Some(mount) = state.mount_attachments.remove(&record.id) {
      state.attachments.revoke(mount.registry);
      state.attachments.drain(mount.registry);
    }
    ended = ended.saturating_add(1);
  }
  if ended > 0 {
    state.count(REMOTE_ENDED, ended);
  }
  Ok(ended)
}

/// Counter: another host's attachments ended without a detach, by its host's reap or retirement.
const REMOTE_ENDED: &str = "remote_attach.ended";

/// The most volumes of other hosts one client's node remembers it attached (`ClientSlot::remote_volumes`), each so the
/// node's reap of the client can end the attachment at its owner: as many as the client's ring has request slots.
pub(crate) fn bound(state: &ShardState) -> u64 {
  slates_machine::derived!(
    u64::from(state.config.region.slots),
    "the client's ring slots (one attach a slot; a client past it attaches in a loop)",
    ["region.slots"]
  )
  .get()
}

/// Remembers, on the client's node, that `client` asks another host's owner for an attachment of `body`'s volume in
/// the record form, so its reap reaches that owner (`reap_at_owners`). Refused `RemoteAttachmentsFull` past [`bound`],
/// before anything is sent.
pub(crate) fn remember(
  state: &mut ShardState,
  client: slates_mem::Handle<crate::state::ClientSlot>,
  body: &slates_ipc::protocol::RequestBody,
) -> Result<(), Refusal> {
  let slates_ipc::protocol::RequestBody::Attach {
    volume,
    form: AttachRequest::Root,
    ..
  } = body
  else {
    return Ok(());
  };
  // Only another host's volume is remembered; the reap skips one this node owns by then.
  if !elsewhere(state, *volume) {
    return Ok(());
  }
  let bound = bound(state);
  let Ok(slot) = state.clients.get_mut(client) else {
    return Ok(());
  };
  if slot.remote_volumes.contains(volume) {
    return Ok(());
  }
  if u64::try_from(slot.remote_volumes.len()).unwrap_or(u64::MAX) >= bound {
    return Err(Refusal::RemoteAttachmentsFull { bound });
  }
  slot.remote_volumes.push(*volume);
  Ok(())
}

/// The reap of a dead client, at the owners of the volumes it attached from this node (`RemoteReap`, routed by each
/// volume as a forwarded verb is): the owner ends every attachment the client holds there. A volume that is gone
/// counts as reaped; any other refusal defers the reap, which its sweep retries. Within `deadline_ns`.
pub(crate) async fn reap_at_owners(
  origin: u16,
  handle: slates_mem::Handle<crate::state::ClientSlot>,
  client: u32,
  deadline_ns: u64,
) -> Result<(), Refusal> {
  let prepared = crate::state::with_state(|state| {
    let slot = state.clients.get(handle).ok()?;
    let principal = slot.principal.clone();
    let local = state.fleet.host();
    // A volume this node owns by now (a takeover) has its attachments ended by the local reap.
    let volumes: Vec<slates_ipc::protocol::VolumeId> = slot
      .remote_volumes
      .iter()
      .copied()
      .filter(|volume| {
        let object = slates_db::register::ObjectId(volume.bytes);
        object.creator() != local && state.fleet.object_owner(object) != Some(local)
      })
      .collect();
    let control = state.shards.first().copied()?;
    let region = state
      .node_regions
      .get(&state.fleet.host())
      .map_or(0, |region| region.0);
    let requests: Vec<(slates_ipc::protocol::VolumeId, Vec<u8>)> = volumes
      .into_iter()
      .map(|volume| {
        let body = slates_ipc::protocol::RequestBody::RemoteReap { volume, client };
        (
          volume,
          crate::verbs::forwarded_request(state, 0, principal.clone(), body),
        )
      })
      .collect();
    Some((control, region, requests))
  })
  .flatten();
  let Some((control, region, requests)) = prepared else {
    return Ok(());
  };
  if requests.is_empty() {
    return Ok(());
  }
  crate::xshard::task_within(
    origin,
    control,
    move || async move {
      for (volume, request) in requests {
        let (replies, _) =
          crate::verbs::resolve_and_forward(volume, region, None, vec![request]).await;
        match replies.first() {
          slates_ipc::protocol::ReplyBody::Detached
          | slates_ipc::protocol::ReplyBody::Refused {
            refusal: Refusal::NotFound | Refusal::Destroying,
          } => {}
          slates_ipc::protocol::ReplyBody::Refused { refusal } => return Err(refusal.clone()),
          _ => {
            return Err(Refusal::BadRequest {
              reason: "a remote reap answered other than detached".to_owned(),
            });
          }
        }
      }
      Ok(())
    },
    deadline_ns,
  )
  .await
  .unwrap_or(Err(Refusal::Overloaded { shard: control }))
}

#[cfg(test)]
mod tests {
  use super::departed;

  /// A-222: a host the fleet has not configured yet has not departed; only one among the members at the last look and
  /// absent at this one has. Main CI 42335884062 and its loaded reproduction: following configuration installs missed
  /// the retirement (it arrives by several install paths), and ending every non-member ended a forming host's fresh
  /// attachment (1 of 20 runs).
  #[test]
  fn only_a_host_that_was_a_member_and_is_not_now_has_departed() {
    assert_eq!(departed(&[1, 2], &[1, 2, 3]), Vec::<u64>::new());
    assert_eq!(departed(&[1, 2, 3], &[1, 3]), vec![2]);
    assert_eq!(departed(&[], &[1]), Vec::<u64>::new());
    assert_eq!(departed(&[1, 2], &[]), Vec::<u64>::new());
  }
}
