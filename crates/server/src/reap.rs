//! Client retirement across owner shards (§4.7 failure matrix, D-7, D-16). A dead client keeps
//! its seat and id until its attachments have been removed everywhere. Read-only gathers
//! follow already-admitted synchronous forwards; removals name frozen ids, so a delayed
//! cleanup cannot remove a resumed client's new attachment. Work is bounded by the existing
//! client/attachment tables and sliced by the runtime batch. Evidence: the two-owner SIGKILL
//! history in `client/tests/reap.rs`, which failed with the former local-partition sweep.

use slates_db::Op;
use slates_db::catalog::Consumer;
use slates_ipc::protocol::Refusal;
use slates_mem::Handle;
use slates_vfs::clock::Clock;
use slates_wire::request::RequestId;

use crate::state::{self, ClientSlot, ShardState};

/// Finds dead clients and reserves their seats through owner cleanup. Failed cleanup is retried
/// by the existing owned reaper task at its next cadence; no detached retirement task is created.
pub(crate) async fn sweep(silence_ns: u64) -> usize {
  let Some((origin, shards, batch, clients)) = state::with_state(|state| {
    let clients = begin(state, silence_ns);
    (
      state.shard,
      state.shards.clone(),
      state.config.runtime.batch.max(1),
      clients,
    )
  }) else {
    return 0;
  };
  let mut reaped = 0;
  for (handle, client_id) in clients {
    let result = remove_on_owners(origin, &shards, batch, client_id, silence_ns).await;
    state::with_state(|state| match result {
      Ok(()) => {
        if finish(state, handle, client_id) {
          reaped += 1;
        }
      }
      Err(refusal) => {
        *state
          .refusals
          .entry("client.retirement_deferred")
          .or_insert(0) += 1;
        *state
          .refusals
          .entry(crate::verbs::refusal_name(&refusal))
          .or_insert(0) += 1;
      }
    });
    slates_rt::futures::yield_now().await;
  }
  reaped
}

/// Freeze admission from dead rings before sending any owner barrier. The vector and retained
/// seats are bounded by `clients_per_shard`; a refusal never returns the id early.
fn begin(state: &mut ShardState, silence_ns: u64) -> Vec<(Handle<ClientSlot>, u32)> {
  let now = state.clock.monotonic_ns();
  let mut clients = Vec::new();
  for (handle, client) in state.clients.iter_mut_all() {
    client.status_pages.expire(now, &mut state.store.metadata);
    if client.retiring
      || (now.saturating_sub(client.last_seen_ns) >= silence_ns
        && crate::peer::peer_gone(client.control.as_ref(), client.pid))
    {
      client.retiring = true;
      clients.push((handle, client.client_id));
    }
  }
  let seats = &state.clients;
  state.pending_forwards.retain(|forward| {
    seats
      .generation_at(forward.client_index)
      .and_then(|generation| {
        seats
          .get(Handle::from_raw(forward.client_index, generation))
          .ok()
      })
      .is_some_and(|client| !client.retiring && client.client_id == forward.client_id)
  });
  clients
}

/// A read-only gather followed by bounded removals. Cancellation may leave an admitted callback
/// running later; only frozen attachment ids cross the mutating boundary, never a live client query.
async fn remove_on_owners(
  origin: u16,
  shards: &[u16],
  batch: usize,
  client: u32,
  deadline: u64,
) -> Result<(), Refusal> {
  for &shard in shards {
    let attachments = crate::xshard::call_within(
      origin,
      shard,
      move |state| state.db.partition().attachments_of_client(client),
      deadline,
    )
    .await
    .ok_or(Refusal::Overloaded { shard })?;
    for ids in attachments.chunks(batch) {
      let ids = ids.to_vec();
      crate::xshard::call_within(
        origin,
        shard,
        move |state| remove_batch(state, client, &ids),
        deadline,
      )
      .await
      .ok_or(Refusal::Overloaded { shard })??;
      slates_rt::futures::yield_now().await;
    }
  }
  Ok(())
}

/// A recorded removal releases its green pin but does not shorten its write lease (D-16).
fn remove_batch(state: &mut ShardState, client: u32, attachments: &[u64]) -> Result<(), Refusal> {
  let now = state.clock.monotonic_ns();
  for &id in attachments {
    if !state.db.partition().attachment(id).is_some_and(
      |record| matches!(record.consumer, Consumer::Sdk { client: owner } if owner == client),
    ) {
      // An explicit detach or an earlier acknowledged removal already ended this exact record.
      continue;
    }
    state
      .db
      .mutate(&mut state.segment, &Op::AttachmentRemoved { id }, now)
      .map_err(|error| crate::error::refusal_of_db(&error))?;
    crate::merge_service::forget_attachment(state, id);
  }
  Ok(())
}

/// Return the seat last, after every owner replied. Slot generation and client id are checked
/// before release; all work remaining from the old ring carries its request identity.
fn finish(state: &mut ShardState, handle: Handle<ClientSlot>, client_id: u32) -> bool {
  if !state
    .clients
    .get(handle)
    .is_ok_and(|client| client.retiring && client.client_id == client_id)
  {
    return false;
  }
  state
    .deferred
    .retain(|reply| reply.client_index != handle.index());
  state
    .forwarded_rings
    .retain(|request, _| RequestId::from_word(*request).client != client_id);
  let Ok(mut client) = state.clients.remove(handle) else {
    return false;
  };
  client.status_pages.clear(&mut state.store.metadata);
  if let Some(control) = state.shards.first().copied() {
    crate::daemon::release_client_id(client_id, control);
  }
  true
}
