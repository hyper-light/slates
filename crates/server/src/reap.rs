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
  let mut reaped: usize = 0;
  for (handle, client_id) in clients {
    let result = match remove_on_owners(origin, &shards, batch, client_id, silence_ns).await {
      // Then its attachments of other hosts' volumes, at their owners (A-222).
      Ok(()) => crate::remote_attach::reap_at_owners(origin, handle, client_id, silence_ns).await,
      Err(refusal) => Err(refusal),
    };
    state::with_state(|state| match result {
      Ok(()) => {
        if finish(state, handle, client_id) {
          reaped = reaped.saturating_add(1);
        }
      }
      Err(refusal) => {
        state.count("client.retirement_deferred", 1);
        state.count(crate::verbs::refusal_name(&refusal), 1);
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
  // Abandoned staging buffers are released on this cadence too, a lease after their last use (`crate::staging`).
  state.staging.expire(now, &mut state.store.metadata);
  let mut clients = Vec::new();
  let mut unproven = Vec::new();
  for (handle, client) in state.clients.iter_mut_all() {
    client.status_pages.expire(now, &mut state.store.metadata);
    let dead = now.saturating_sub(client.last_seen_ns) >= silence_ns
      && crate::peer::peer_gone(client.control.as_ref(), client.pid);
    // A dead slot that never proved its id is ended without retiring anything of the id's (`end_unproven`).
    if dead && !client.proven && !client.retiring {
      unproven.push(handle);
    } else if client.retiring || dead {
      client.retiring = true;
      clients.push((handle, client.client_id));
    }
  }
  for handle in unproven {
    end_unproven(state, handle);
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
/// On each shard the client's direct maps held there (§4.6, A-188) bracket the removal: a writable map's stores reach
/// its file only through the attachment it was made under, so they are written back first (A-186), a store to a shared
/// map outliving its process; and the copies are let go once the attachments are gone. On macOS the offers the client
/// never claimed on the socket route go last (A-492, `crate::shards_route::let_go_of_client`): an offer waits for its
/// claim while its client lives, and a stream's offer holds the end its stream waits on, so letting it go ends the stream
/// and its attachment, which no removal of the client's own attachments reaches (a guest's).
///
/// Each call waits in its shard's own time (A-432; R3's class A, A-65): a window of `budget_ns`, then on while the
/// shard works or waits for a CPU, until it has run `budget_ns` of CPU (held to this build), so a shard alive and busy
/// (a long write-back, a starved thread) is waited for, and only one that stopped, or ran its budget without answering,
/// defers the reap `Overloaded`, which the next sweep retries rather than removing an attachment its stores still need.
/// Each waited one window on the wall, so a busy owner deferred every reap that met it and the next sweep sent the same
/// write-back again beside the first.
pub(crate) async fn remove_on_owners(
  origin: u16,
  shards: &[u16],
  batch: usize,
  client: u32,
  budget_ns: u64,
) -> Result<(), Refusal> {
  for &shard in shards {
    crate::xshard::task_in_shard_time(
      origin,
      shard,
      move || crate::direct_map::write_back_held(client),
      budget_ns,
    )
    .await
    .ok_or(Refusal::Overloaded { shard })?;
    // The client's presentations there are abandoned with it (AUD-29-07): nothing waits on them now.
    crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |state| crate::landing::abandon_presentations(state, client),
      budget_ns,
    )
    .await
    .ok_or(Refusal::Overloaded { shard })?;
    let attachments = crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |state| state.db.partition().attachments_of_client(client),
      budget_ns,
    )
    .await
    .ok_or(Refusal::Overloaded { shard })?;
    for ids in attachments.chunks(batch) {
      let ids = ids.to_vec();
      crate::xshard::call_in_shard_time(
        origin,
        shard,
        move |state| remove_batch(state, client, &ids),
        budget_ns,
      )
      .await
      .ok_or(Refusal::Overloaded { shard })??;
      slates_rt::futures::yield_now().await;
    }
    crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |state| crate::direct_map::let_go_held(state, client),
      budget_ns,
    )
    .await
    .ok_or(Refusal::Overloaded { shard })?;
    #[cfg(target_os = "macos")]
    crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |state| crate::shards_route::let_go_of_client(state, client),
      budget_ns,
    )
    .await
    .ok_or(Refusal::Overloaded { shard })?;
  }
  Ok(())
}

/// A recorded removal releases its green pin but does not shorten its write lease (D-16).
pub(crate) fn remove_batch(
  state: &mut ShardState,
  client: u32,
  attachments: &[u64],
) -> Result<(), Refusal> {
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

/// Ends a slot whose client never proved its id (`crate::session_key`): its ring and seat go and the id is given back
/// for its owner to resume, but nothing of the id's is retired. The attachments and completion records under the id
/// are its owner's, and a slot that never proved the id is not its owner; it served nothing, so it left no work.
pub(crate) fn end_unproven(state: &mut ShardState, handle: Handle<ClientSlot>) {
  if state.clients.get(handle).is_ok_and(|client| client.proven) {
    return;
  }
  state
    .deferred
    .retain(|reply| reply.client_index != handle.index());
  let Ok(mut client) = state.clients.remove(handle) else {
    return;
  };
  client.status_pages.clear(&mut state.store.metadata);
  if let Some(control) = state.shards.first().copied() {
    crate::daemon::release_client_id(client.client_id, control);
  }
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
  // The files' copies the client was handed by the direct map were let go on every shard that held them, after its
  // attachments there (`remove_on_owners`, A-188).
  state.read_ahead.credit(client.read_ahead_pending);
  crate::verbs::release_read_ahead(&mut state.read_ahead, &mut client);
  if let Some(control) = state.shards.first().copied() {
    crate::daemon::release_client_id(client_id, control);
  }
  true
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  /// Shape: the budget each of the reap's calls is waited in this test: a tenth of a second, short beside its target's
  /// busy poll.
  const BUDGET_NS: u64 = 100_000_000;
  /// Shape: the target's one busy poll on the wall: three budgets, so a call waited one budget on the wall gives up.
  const BUSY_NS: u64 = 3 * BUDGET_NS;
  /// Shape: the client the reap retires, which holds nothing on the target, so each call answers once it runs.
  const CLIENT: u32 = 7;
  /// Shape: how long the test thread waits for the reap's answer: ten busy polls.
  const ANSWER_WAIT_NS: u64 = 10 * BUSY_NS;

  /// R3's class A (A-432). Do: on a two-shard daemon, hold the second shard in one poll for three budgets of wall time,
  /// on its CPU a fifth of it (a busy or starved shard), and from the first shard retire a client there, each call
  /// waited with a budget's window. Expect: the reap ends `Ok`, after more than a budget of waiting. Each call waited a
  /// budget on the wall, so the reap was deferred `Overloaded` with its target alive and working, and the next sweep
  /// sent the same work again beside the first.
  #[test]
  fn a_reaps_calls_wait_for_a_shard_that_works_past_their_first_window() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |state| {
      let origin = state.shard;
      let target = state
        .shards
        .iter()
        .copied()
        .find(|&shard| shard != origin)
        .unwrap();
      // The busy poll goes first on the target's channel, so each of the reap's calls waits behind it.
      let busy = crate::xshard::call_on(origin, target, || {
        crate::daemon::hold_busy(BUSY_NS);
        Some(())
      })
      .unwrap();
      drop(busy);
      let reap = slates_rt::futures::spawn(async move {
        let started = std::time::Instant::now();
        let reaped = super::remove_on_owners(origin, &[target], 1, CLIENT, BUDGET_NS).await;
        let _ = sent.send((reaped, started.elapsed()));
      })
      .unwrap();
      slates_rt::futures::detach(reap).unwrap();
    });
    let answer = answered.recv_timeout(Duration::from_nanos(ANSWER_WAIT_NS));
    daemon.stop();
    let (reaped, waited) = answer.expect("the reap answered");
    println!("a reap behind a busy poll of {BUSY_NS} ns: {reaped:?} after {waited:?}");
    assert!(
      reaped.is_ok(),
      "the reap of a client on a shard alive and working was deferred: {reaped:?} after {waited:?}"
    );
    assert!(
      waited > Duration::from_nanos(BUDGET_NS),
      "the reap ended within one window ({waited:?}), before its target's busy poll did"
    );
  }
}
