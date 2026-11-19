//! Client retirement across owner shards (§4.7 failure matrix, D-7, D-16), begun at the client's hang-up (A-475). A
//! dead client keeps its seat and id until its attachments have been removed everywhere. Read-only gathers follow
//! already-admitted synchronous forwards; removals name frozen ids, so a delayed cleanup cannot remove a resumed client's
//! new attachment. Work is bounded by the client table and sliced by the runtime batch. Evidence: the two-owner SIGKILL
//! history in `client/tests/reap.rs`, which failed with the former local-partition sweep.
//!
//! **A death is an event (A-475).** Each seated client has a watch, a task of its shard that awaits the client's
//! hang-up through the shard's driver (`crate::hangup`) and reports it once ([`watch`]): a slot that never proved its
//! id is ended there, any other is marked retiring, which closes admission from its ring, and queued for the shard's
//! reaper ([`reaper`]), which retires the dead a batch at a time and idles while none is owed. Until A-475 a sweep each
//! liveness budget asked every client silent for a budget whether its peer was gone (`peer_gone`), so a client killed
//! with `SIGKILL` kept its seat, its attachments and its id for one to two budgets, and every shard woke each second to
//! ask. The jobs that sweep also did went elsewhere: the forwards a dead client left waiting for room go at its report,
//! an unproven slot ends there, and the expiry of abandoned staging buffers and status pages went to the shard's
//! deadline keeper (A-522, `crate::deadlines`). Since A-554 the reaper also ends, on the control shard, the attachments
//! of hosts the fleet let go (A-222), looked at when the fleet's members or their anchors change, where the reap loop's
//! tick looked once a window.

use std::collections::VecDeque;

use slates_db::Op;
use slates_db::catalog::Consumer;
use slates_ipc::protocol::Refusal;
use slates_mem::Handle;
use slates_rt::{RtError, TaskId};
use slates_vfs::clock::Clock;
use slates_wire::request::RequestId;

use crate::hangup::{Ended, Hangup};
use crate::state::{self, ClientSlot, ShardState};

/// Format: the counter of seats given back because their clients hung up (A-475): the event path's non-vacuity count,
/// moved for an unproven slot ended at its report and for a retirement finished by the reaper.
pub(crate) const REAPED_BY_HANGUP: &str = "client.reaped_by_hangup";
/// Format: the counter of clients that wrote on their control socket, which no client does after its opening, ended as
/// a hang-up is (Linux, [`Ended`]). Linux's alone, as `Ended::Broke` is.
#[cfg(target_os = "linux")]
pub(crate) const CONTROL_BROKEN: &str = "client.control_broken";
/// Format: the counter of watches whose wait the shard refused, each client ended as a hang-up is, since nothing could
/// see its death afterwards.
pub(crate) const WATCH_REFUSED: &str = "client.watch_refused";
/// Format: the counter of clients not seated because their hang-up could not be watched (no descriptor for the watch, or
/// no task): nothing would have reclaimed them.
pub(crate) const UNWATCHABLE: &str = "client.unwatchable";
/// Format: the counter of watches a seat ending first could not cancel, a bug signal: a watch its seat names is live.
pub(crate) const WATCH_CANCEL_REFUSED: &str = "client.watch_cancel_refused";
/// Format: the counter of retirements an owner shard deferred, each tried again by the reaper.
const RETIREMENT_DEFERRED: &str = "client.retirement_deferred";

/// Starts the watch on the client just seated at `seat` with `hangup`, held by its seat (`ClientSlot::watch`) and
/// detached, since nothing joins it: its seat cancels it at most. A watch the shard's task arena refuses unseats the
/// client again, since nothing would reclaim it: its slot closes here, which its client reads as the daemon gone, and the
/// caller's admission gives its id back. Whether the client stays seated.
pub(crate) fn start_watch(
  state: &mut ShardState,
  seat: Handle<ClientSlot>,
  client_id: u32,
  hangup: Hangup,
) -> bool {
  let started = slates_rt::futures::spawn(watch(seat, client_id, hangup)).and_then(|task| {
    slates_rt::futures::detach(task)
      .map(|()| task)
      // A task its shard cannot detach cannot be kept either: it is cancelled, and its refusal reported below.
      .inspect_err(|_| {
        let _cancelled = slates_rt::futures::cancel(task);
      })
  });
  match started {
    Ok(task) => {
      if let Ok(client) = state.clients.get_mut(seat) {
        client.watch = Some(task);
        return true;
      }
      // The seat went between its insertion and here, which nothing on this shard does; its watch goes with it.
      if slates_rt::futures::cancel(task).is_err() {
        state.count(WATCH_CANCEL_REFUSED, 1);
      }
      false
    }
    Err(refusal) => {
      state.count(UNWATCHABLE, 1);
      slates_machine::log_line!(
        "slates-server: client {client_id} is not seated: its hang-up watch could not be started ({refusal})"
      );
      if let Ok(slot) = state.clients.remove(seat) {
        drop(slot);
      }
      false
    }
  }
}

/// The watch on the client seated at `seat` (A-475): awaits its hang-up, then reports it. The wait is dropped before the
/// watch's descriptor closes, since it borrows it (`crate::hangup`), and the report comes after both.
pub(crate) async fn watch(seat: Handle<ClientSlot>, client_id: u32, hangup: Hangup) {
  let ended = hangup.wait().await;
  drop(hangup);
  let reaper = state::with_state_counted(|state| hung_up(state, seat, client_id, ended)).flatten();
  if let Some(reaper) = reaper {
    slates_rt::registry::wake(reaper.0);
  }
}

/// The watch's report: the client in `seat` is gone (`ended` says how, or why its wait was refused). A slot that never
/// proved its id is ended at once, retiring nothing of the id's (`end_unproven`); any other is marked retiring, its
/// forwards still waiting for room go (nothing will read their replies), and it joins the reaper's queue, bounded by the
/// seats since a seat reports once. Returns the reaper to wake.
fn hung_up(
  state: &mut ShardState,
  seat: Handle<ClientSlot>,
  client_id: u32,
  ended: Result<Ended, RtError>,
) -> Option<TaskId> {
  match ended {
    Ok(Ended::HungUp) => {}
    #[cfg(target_os = "linux")]
    Ok(Ended::Broke) => state.count(CONTROL_BROKEN, 1),
    Err(refusal) => {
      state.count(WATCH_REFUSED, 1);
      slates_machine::log_line!(
        "slates-server: client {client_id}'s hang-up wait was refused ({refusal}); it is ended as a hang-up is"
      );
    }
  }
  let client = state
    .clients
    .get_mut(seat)
    .ok()
    .filter(|client| client.client_id == client_id)?;
  client.watch = None;
  if client.retiring {
    return None;
  }
  if !client.proven {
    end_unproven(state, seat);
    state.count(REAPED_BY_HANGUP, 1);
    return None;
  }
  client.retiring = true;
  let index = seat.index();
  state
    .pending_forwards
    .retain(|forward| !(forward.client_index == index && forward.client_id == client_id));
  state.dead_clients.push_back((seat, client_id));
  state.reaper_task
}

/// One round of the reaper: up to a batch of the dead, with what their retirement needs.
struct Round {
  origin: u16,
  shards: Vec<u16>,
  batch: usize,
  clients: Vec<(Handle<ClientSlot>, u32)>,
}

/// Takes the next round from the reaper's queue: up to `batch` of the dead, the runtime's batch.
fn take_round(state: &mut ShardState) -> Option<Round> {
  if state.dead_clients.is_empty() {
    return None;
  }
  let batch = state.config.runtime.batch.max(1);
  let take = batch.min(state.dead_clients.len());
  let clients = state.dead_clients.drain(..take).collect();
  Some(Round {
    origin: state.shard,
    shards: state.shards.clone(),
    batch,
    clients,
  })
}

/// The shard's reaper (A-475): retires the clients whose watches reported them gone, a batch at a time with its turn given
/// back between batches, so a mass death is reaped in slices; idles while none is owed, woken by the next report. A
/// retirement an owner deferred is tried again a window later (the shard's own, A-455), or at the next report if that
/// comes first. On the control shard it also ends the attachments of hosts the fleet let go, when a look is asked for
/// (A-222, A-554: `remote_attach::look_for_departures`), since a departed host's clients are dead clients no watch here
/// can see; and it sends the NFSv4 purges its shard's table owes when the keeper hands it a round (A-570,
/// `crate::nfs::owe_purge_round`). One task a shard, made at its start; it ends with the shard's state.
pub(crate) async fn reaper() {
  if state::with_state(|state| state.reaper_task = slates_rt::futures::current_task()).is_none() {
    return;
  }
  let mut deferred: VecDeque<(Handle<ClientSlot>, u32)> = VecDeque::new();
  loop {
    // A host the fleet no longer counts a member has its clients' attachments ended on every shard (A-222), looked at
    // when the members or their anchors changed, or again a retry window after a look some shard did not answer
    // (A-554).
    let Some(departures) = state::with_state(|state| std::mem::take(&mut state.departures_owed))
    else {
      return;
    };
    if departures {
      crate::remote_attach::sweep_departed().await;
    }
    // A taken-back map no holder claimed within the reconnect budget is reclaimed as a dead client's maps are (A-197),
    // owed by the keeper at its claim's deadline (A-554). Linux alone takes such maps back.
    #[cfg(target_os = "linux")]
    let _reclaimed = crate::held_copy::reclaim_owed().await;
    // A purge round the NFSv4 table owes its owners, handed over by the keeper at its deadline (A-570).
    let Some(purges) = state::with_state(|state| std::mem::take(&mut state.nfs_purge_owed)) else {
      return;
    };
    if purges {
      crate::nfs::purge_owed_here().await;
    }
    let Some(round) = state::with_state(take_round) else {
      return;
    };
    let Some(round) = round else {
      if deferred.is_empty() {
        slates_rt::futures::idle().await;
      } else {
        let window = crate::watchdog::watch_window_ns(slates_rt::futures::scheduler_overrun_ns());
        if slates_rt::futures::within(window, slates_rt::futures::idle())
          .await
          .is_err()
        {
          return;
        }
        let again = std::mem::take(&mut deferred);
        if state::with_state(|state| state.dead_clients.extend(again)).is_none() {
          return;
        }
      }
      continue;
    };
    for (seat, client_id) in round.clients {
      if !retire(round.origin, &round.shards, round.batch, seat, client_id).await {
        deferred.push_back((seat, client_id));
      }
    }
    slates_rt::futures::yield_now().await;
  }
}

/// Retires the client `client_id` in `seat` at every owner, then gives its seat back: whether done (a deferral is tried
/// again). Each owner's calls wait in that owner's own time, by its window.
async fn retire(
  origin: u16,
  shards: &[u16],
  batch: usize,
  seat: Handle<ClientSlot>,
  client_id: u32,
) -> bool {
  let control = shards.first().copied().unwrap_or(origin);
  let result = match remove_on_owners(origin, shards, batch, client_id, owner_window_ns).await {
    // Then its attachments of other hosts' volumes, at their owners (A-222).
    Ok(()) => {
      crate::remote_attach::reap_at_owners(origin, seat, client_id, owner_window_ns(control)).await
    }
    Err(refusal) => Err(refusal),
  };
  state::with_state_counted(|state| match result {
    Ok(()) => {
      if finish(state, seat, client_id) {
        state.count(REAPED_BY_HANGUP, 1);
        crate::daemon::CLIENTS_REAPED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
      }
      true
    }
    Err(refusal) => {
      state.count(RETIREMENT_DEFERRED, 1);
      state.count(crate::verbs::refusal_name(&refusal), 1);
      false
    }
  })
  // A state out of reach is tried again; one that left the shard ends the reaper at its next round.
  .unwrap_or(false)
}

/// Derived: the window a reap's call waits in its owner shard's own time (A-432's rule): the owner's own watch window
/// (A-455), the liveness window raised to the scheduler overrun the owner measured and capped at the ceiling, read from
/// its registry pulse. Until A-475 the reap's cadence, the liveness window itself.
fn owner_window_ns(shard: u16) -> u64 {
  crate::watchdog::watch_window_ns(
    slates_rt::registry::with_entry(shard, |entry| entry.pulse.scheduler_overrun_ns()).unwrap_or(0),
  )
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
/// Each call waits in its shard's own time (A-432; R3's class A, A-65): a window of `window_of(shard)`, then on while the
/// shard works or waits for a CPU, until it has run that window of CPU (held to this build), so a shard alive and busy
/// (a long write-back, a starved thread) is waited for, and only one that stopped, or ran its window without answering,
/// defers the reap `Overloaded`, which the reaper retries rather than removing an attachment its stores still need.
/// Each waited one window on the wall, so a busy owner deferred every reap that met it and the next sweep sent the same
/// write-back again beside the first. The window is each owner's own (A-475, [`owner_window_ns`]), where it was the reap
/// loop's cadence.
pub(crate) async fn remove_on_owners(
  origin: u16,
  shards: &[u16],
  batch: usize,
  client: u32,
  window_of: impl Fn(u16) -> u64,
) -> Result<(), Refusal> {
  for &shard in shards {
    let budget_ns = window_of(shard);
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
/// are its owner's, and a slot that never proved the id is not its owner; it served nothing, so it left no work. Ended
/// by its watch's report, or by the session gate while its client lives, whose watch it cancels (A-475).
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
  cancel_watch(state, &mut client);
  crate::deadlines::forget_seat(state, handle, &client);
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
  cancel_watch(state, &mut client);
  crate::deadlines::forget_seat(state, handle, &client);
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

/// Cancels the watch of a seat that ends before its client hung up (a stranger's slot the session gate ends): no task is
/// left waiting on a client nobody seats, and its wait goes before its descriptor closes, since the wait borrows it
/// (`crate::hangup`). A seat whose watch reported names none. A refused cancel is counted, a bug signal.
fn cancel_watch(state: &mut ShardState, client: &mut ClientSlot) {
  if let Some(watch) = client.watch.take()
    && slates_rt::futures::cancel(watch).is_err()
  {
    state.count(WATCH_CANCEL_REFUSED, 1);
  }
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
        let reaped = super::remove_on_owners(origin, &[target], 1, CLIENT, |_| BUDGET_NS).await;
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

  /// Shape: the id of the client the hang-up test seats, beside the audit daemon's own, which seat none.
  const WATCHED: u32 = 11;
  /// Shape: how long the hang-up test waits for the seat to be given back: a liveness budget, the earliest the sweep this
  /// replaced asked about a client; the event path takes milliseconds.
  const RETIRED_WITHIN_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: the pause between the test's looks at the daemon's seats, short beside the bound.
  const LOOK_PAUSE_NS: u64 = 1_000_000;

  /// Seats client [`WATCHED`] on `state`'s shard, proven, with process `pid` and its hang-up watched by `hangup`, as an
  /// admission does (A-475); its region is the shard's geometry, made here.
  fn seat(state: &mut crate::state::ShardState, pid: u32, hangup: crate::hangup::Hangup) {
    static REGIONS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
      "slates-cr-hangup-{}-{}",
      std::process::id(),
      REGIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut region =
      slates_ipc::ClientRegion::create(&name, WATCHED, state.shard, state.config.region).unwrap();
    #[cfg(target_os = "macos")]
    region.unlink_name().unwrap();
    let seat = state
      .clients
      .insert(crate::state::ClientSlot {
        status_pages: crate::status_pages::StatusPages::default(),
        end: slates_ipc::DaemonEnd::new(region),
        principal: slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        client_id: WATCHED,
        pid,
        retiring: false,
        watch: None,
        proven: true,
        control: None,
        revoked: false,
        consumer_policy: None,
        identities: None,
        owner_route: None,
        read_ahead: None,
        read_ahead_pending: 0,
        remote_volumes: Vec::new(),
        maps_sent: std::collections::BTreeSet::new(),
      })
      .unwrap();
    assert!(super::start_watch(state, seat, WATCHED, hangup));
  }

  /// Seats a watched client whose control socket's other end a child process holds as its standard input (Linux): the
  /// pair is made on the shard, its client end sent back for the child.
  #[cfg(target_os = "linux")]
  fn seat_client_process(daemon: &crate::daemon::Daemon) -> std::process::Child {
    use std::os::fd::AsFd;
    let (sent, received) = std::sync::mpsc::channel();
    crate::daemon::observe_first(daemon, move |state| {
      let (seat_end, client_end) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
      )
      .unwrap();
      let hangup = crate::hangup::Hangup::of_socket(seat_end.as_fd()).unwrap();
      seat(state, 0, hangup);
      sent.send(client_end).unwrap();
    });
    let client_end = received.recv().unwrap();
    std::process::Command::new("sleep")
      .arg("30")
      .stdin(std::process::Stdio::from(client_end))
      .spawn()
      .unwrap()
  }

  /// Seats a client process watched by its process (macOS, Windows).
  #[cfg(any(target_os = "macos", windows))]
  fn seat_client_process(daemon: &crate::daemon::Daemon) -> std::process::Child {
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    #[cfg(windows)]
    let child = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", "30", "127.0.0.1"])
      .spawn()
      .unwrap();
    let pid = child.id();
    crate::daemon::observe_first(daemon, move |state| {
      let started = slates_ipc::exit_watch::ExitWatch::start_of(pid)
        .unwrap()
        .unwrap();
      let hangup = crate::hangup::Hangup::of_process(pid, started).unwrap();
      seat(state, pid, hangup);
    });
    child
  }

  /// A-475 ("a client killed with SIGKILL is retired within one event turn, not a liveness budget"). Do: on a one-shard
  /// daemon, seat a client that proved its id, its hang-up watched (Linux: its control socket, the other end held by a
  /// child process; macOS and Windows: the child process), then kill the child with `SIGKILL`. Expect: the seat is given
  /// back and `client.reaped_by_hangup` moves by one (the event path's non-vacuity counter), with the reaped health
  /// signal, well inside a liveness budget of the kill. The sweep this replaced asked about a client only once it had
  /// been silent a budget, so it found this one a budget or two after the kill.
  #[test]
  fn a_client_killed_with_sigkill_is_retired_at_its_hang_up_not_a_budget_later() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let reaped_before = crate::daemon::CLIENTS_REAPED.load(std::sync::atomic::Ordering::Acquire);
    let mut child = seat_client_process(&daemon);
    let killed = std::time::Instant::now();
    child.kill().unwrap();
    let retired = loop {
      let (seated, reaped) = crate::daemon::observe_first(&daemon, |state| {
        (
          state
            .clients
            .iter()
            .any(|(_, client)| client.client_id == WATCHED),
          state
            .refusals
            .get(super::REAPED_BY_HANGUP)
            .copied()
            .unwrap_or(0),
        )
      });
      if !seated {
        break Some((killed.elapsed(), reaped));
      }
      if killed.elapsed() > Duration::from_nanos(RETIRED_WITHIN_NS) {
        break None;
      }
      // The harness paces its looks; the daemon under test waits on its drivers.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    };
    child.wait().unwrap();
    daemon.stop();
    let (after, reaped) =
      retired.expect("the seat was given back within a liveness budget of the kill");
    println!("a client killed with SIGKILL: its seat given back {after:?} after the kill");
    assert_eq!(reaped, 1, "client.reaped_by_hangup moved by one");
    assert!(
      crate::daemon::CLIENTS_REAPED.load(std::sync::atomic::Ordering::Acquire) > reaped_before,
      "the reaped health signal moved"
    );
  }
}
