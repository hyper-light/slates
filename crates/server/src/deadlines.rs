//! The deadlines of what a shard's clients leave behind, each kept on the shard's timing wheel (A-522; the event-driven
//! reaper's piece 2): a staging buffer expires a lease after its last use (`crate::staging`), and a status capture at its
//! absolute deadline (`crate::status_pages`). One task a shard, the keeper, sleeps on the wheel until the earliest of
//! them, releases what is due then, and sleeps until the next; a deadline set earlier than the one it sleeps for wakes it
//! at once, so it re-arms. Nothing is scanned on a tick: each pass reads only the due entries of two ordered indexes,
//! the staging buffers by expiry and the seats by their capture's deadline. Until A-522 the reap loop's tick released
//! both each liveness window, every client's status capture asked at every tick, and a shard with none woke anyway.
//!
//! **Bounds.** A seat is entered in the capture index once at most: a new capture replaces its entry
//! (`StatusPages::index`), and a seat that ends takes its entry with it ([`forget_seat`]). Staging buffers are bounded by
//! the shard's metadata budget (`crate::staging`).

use std::collections::BTreeSet;

use slates_mem::Handle;
use slates_vfs::clock::Clock;

use crate::state::{self, ClientSlot, ShardState};

/// The shard's index of status-capture deadlines: by deadline, the seat (its slot index and generation).
pub(crate) type CaptureDeadlines = BTreeSet<(u64, u32, u32)>;

/// The shard's keeper (A-522): releases each expired staging buffer and status capture at its deadline, sleeping on the
/// shard's wheel in between, and idles while none is set. One task a shard, made at its start; it ends with the shard's
/// state.
pub(crate) async fn keeper() {
  if state::with_state(|state| state.deadline_task = slates_rt::futures::current_task()).is_none() {
    return;
  }
  loop {
    let Some(next) = state::with_state(release_due) else {
      return;
    };
    match next {
      // Nothing set: woken by the next deadline set (`noted`).
      None => slates_rt::futures::idle().await,
      Some(span) => {
        if slates_rt::futures::within(span, slates_rt::futures::idle())
          .await
          .is_err()
        {
          return;
        }
      }
    }
  }
}

/// Releases what is due now and arms the keeper for the next deadline: the span to it, or `None` when none is set.
fn release_due(state: &mut ShardState) -> Option<u64> {
  let now = state.clock.monotonic_ns();
  state.staging.expire(now, &mut state.store.metadata);
  while let Some(&(at, index, generation)) = state.capture_deadlines.first() {
    if at > now {
      break;
    }
    state.capture_deadlines.pop_first();
    if let Ok(client) = state.clients.get_mut(Handle::from_raw(index, generation)) {
      client.status_pages.unindex(at);
      client.status_pages.expire(now, &mut state.store.metadata);
    }
  }
  let next = [
    state.staging.next_expiry(),
    state.capture_deadlines.first().map(|(at, ..)| *at),
  ]
  .into_iter()
  .flatten()
  .min();
  state.deadline_armed = next;
  next.map(|at| at.saturating_sub(now))
}

/// A deadline was set at `at`: wakes the keeper when it sleeps for a later one, or for none, so it re-arms for this.
pub(crate) fn noted(state: &ShardState, at: u64) {
  if state.deadline_armed.is_none_or(|armed| at < armed)
    && let Some(keeper) = state.deadline_task
  {
    slates_rt::registry::wake(keeper.0);
  }
}

/// Enters `seat`'s new capture deadline `at` in the index, in place of the one it was entered under, and tells the
/// keeper.
pub(crate) fn capture_set(state: &mut ShardState, seat: Handle<ClientSlot>, at: u64) {
  let Ok(client) = state.clients.get_mut(seat) else {
    return;
  };
  let previous = client.status_pages.index(at);
  let (index, generation) = (seat.index(), seat.generation());
  if let Some(previous) = previous {
    state
      .capture_deadlines
      .remove(&(previous, index, generation));
  }
  state.capture_deadlines.insert((at, index, generation));
  noted(state, at);
}

/// Takes the entry of a seat that ends (`client`, its slot already out of the table) out of the index, so the index
/// holds live seats alone.
pub(crate) fn forget_seat(state: &mut ShardState, seat: Handle<ClientSlot>, client: &ClientSlot) {
  if let Some(at) = client.status_pages.indexed() {
    state
      .capture_deadlines
      .remove(&(at, seat.index(), seat.generation()));
  }
}

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use slates_db::catalog::{Principal, VolumeId};
  use slates_mem::Handle;
  use slates_vfs::clock::Clock;

  use crate::state::{ClientSlot, ShardState};

  /// Shape: how far ahead the tests set a deadline: a tenth of a liveness window, so the reap loop's tick, once a
  /// window, could not be what released it in time.
  const SOON_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 10;
  /// Shape: how long a test looks for the release: half a liveness window, past the deadline and short of the tick.
  const RELEASED_WITHIN_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 2;
  /// Shape: the pause between the test's looks, short beside the deadline.
  const LOOK_PAUSE_NS: u64 = 1_000_000;
  /// Shape: the bytes of the staged buffer: any nonzero length.
  const STAGED_BYTES: u64 = 64;

  /// The principal the tests stage and seat as.
  fn principal() -> Principal {
    Principal::Uid {
      host: slates_db::HostId(0),
      uid: 0,
    }
  }

  /// Looks at `daemon`'s shard with `look` until it answers true or [`RELEASED_WITHIN_NS`] passes: how long that took.
  fn look_until(
    daemon: &crate::daemon::Daemon,
    since: Instant,
    look: impl Fn(&mut ShardState) -> bool + Clone + Send + 'static,
  ) -> Option<Duration> {
    loop {
      if crate::daemon::observe_first(daemon, look.clone()) {
        return Some(since.elapsed());
      }
      if since.elapsed() > Duration::from_nanos(RELEASED_WITHIN_NS) {
        return None;
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    }
  }

  /// A-522. Do: on a one-shard daemon, begin a staging buffer expiring a tenth of a liveness window from now, its
  /// deadline set as the staging verbs set it, and look until it is gone. Expect: released within half a window, by the
  /// keeper at its deadline: the reap loop's tick, which released it before, runs once a window, and no longer does.
  #[test]
  fn an_abandoned_staging_buffer_is_released_at_its_expiry_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let begun = Instant::now();
    crate::daemon::observe_first(&daemon, |state| {
      let expires = state.clock.monotonic_ns().saturating_add(SOON_NS);
      state
        .staging
        .begin(
          (VolumeId { bytes: [9; 16] }, &principal()),
          STAGED_BYTES,
          (state.partition, expires),
          &mut state.store.metadata,
        )
        .unwrap();
      super::noted(state, expires);
    });
    let released = look_until(&daemon, begun, |state| state.staging.len() == 0);
    daemon.stop();
    let released = released.expect("the buffer was released within half a liveness window");
    println!("a staging buffer due in {SOON_NS} ns released {released:?} after it began");
  }

  /// Seats a client that never proved its id on `state`'s shard, as an admission does (its watch left out): its seat.
  fn seat(state: &mut ShardState, client_id: u32) -> Handle<ClientSlot> {
    static REGIONS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
      "slates-cr-deadline-{}-{}",
      std::process::id(),
      REGIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut region =
      slates_ipc::ClientRegion::create(&name, client_id, state.shard, state.config.region).unwrap();
    #[cfg(target_os = "macos")]
    region.unlink_name().unwrap();
    state
      .clients
      .insert(ClientSlot {
        status_pages: crate::status_pages::StatusPages::default(),
        end: slates_ipc::DaemonEnd::new(region),
        principal: principal(),
        client_id,
        pid: 0,
        retiring: false,
        watch: None,
        proven: false,
        control: None,
        revoked: false,
        consumer_policy: None,
        owner_route: None,
        read_ahead: None,
        read_ahead_pending: 0,
        remote_volumes: Vec::new(),
        maps_sent: std::collections::BTreeSet::new(),
      })
      .unwrap()
  }

  /// Shape: the seats the capture test makes, beside the audit daemon's own, which seat none.
  const KEPT: u32 = 21;
  /// Shape: see [`KEPT`].
  const ENDED: u32 = 22;

  /// A-522. Do: on a one-shard daemon, seat two clients and make a status capture for each, its deadline a tenth of a
  /// liveness window away, entered as `send_reply` enters it (A-525); end the second seat at once, as the session gate
  /// does.
  /// Expect: the ended seat takes its entry out of the index at once; the kept seat's capture released within half a
  /// window, its entry gone, by the keeper at its deadline, where the reap loop's tick asked every client each window.
  #[test]
  fn an_abandoned_status_capture_is_released_at_its_deadline_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let begun = Instant::now();
    let entries_after_the_end = crate::daemon::observe_first(&daemon, |state| {
      let expires = state.clock.monotonic_ns().saturating_add(SOON_NS);
      for client_id in [KEPT, ENDED] {
        let seat = seat(state, client_id);
        if let Ok(client) = state.clients.get_mut(seat) {
          let reply = slates_ipc::protocol::ReplyBody::Acknowledged;
          let capacity = slates_ipc::protocol::encode_body(&reply).len();
          client.status_pages.begin(1, &mut state.store.metadata);
          client
            .status_pages
            .capture(1, &reply, (capacity, expires), &mut state.store.metadata)
            .unwrap();
        }
        super::capture_set(state, seat, expires);
        if client_id == ENDED {
          crate::reap::end_unproven(state, seat);
        }
      }
      state.capture_deadlines.len()
    });
    let released = look_until(&daemon, begun, |state| {
      state.capture_deadlines.is_empty()
        && state
          .clients
          .iter()
          .filter(|(_, client)| client.client_id == KEPT)
          .all(|(_, client)| {
            !client.status_pages.pending() && client.status_pages.indexed().is_none()
          })
    });
    daemon.stop();
    assert_eq!(
      entries_after_the_end, 1,
      "the ended seat's entry left the index with it"
    );
    let released = released.expect("the capture was released within half a liveness window");
    println!("a status capture due in {SOON_NS} ns released {released:?} after it began");
  }

  /// Shape: the client the status-capture test seats.
  const ASKING: u32 = 23;
  /// Shape: the failover delay the status-capture test holds its shard to: a tenth of a liveness window, so its capture
  /// is let go well inside the half window [`look_until`] looks for.
  const DELAY_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 10;

  /// `report` grown by copies of its shard's part past two of `seat`'s status pages, within its snapshot bound: a
  /// capture its first page leaves held.
  fn pages_long(
    state: &mut ShardState,
    seat: Handle<ClientSlot>,
    mut report: slates_ipc::protocol::ReplyBody,
  ) -> slates_ipc::protocol::ReplyBody {
    let client = state.clients.get_mut(seat).unwrap();
    let page = slates_ipc::status::page_capacity(client.end.region());
    let bound = slates_ipc::status::snapshot_capacity(client.end.region());
    let slates_ipc::protocol::ReplyBody::DaemonStatus { report: daemon } = &report else {
      panic!("a status report: {report:?}");
    };
    let part = daemon.shards.first().cloned().unwrap();
    while slates_ipc::protocol::encode_body(&report).len() <= 2 * page {
      if let slates_ipc::protocol::ReplyBody::DaemonStatus { report: daemon } = &mut report {
        daemon.shards.push(part.clone());
      }
    }
    assert!(
      slates_ipc::protocol::encode_body(&report).len() <= bound,
      "the grown report still fits the snapshot bound"
    );
    report
  }

  /// A-525 (A-414's owed list; A-522's keeper). Do: on a one-shard daemon whose failover delay is a tenth of a liveness
  /// window, seat a client and ask its status; capture the report, grown to several pages, only two delays later, as a
  /// scatter on a starved daemon does; then look until the shard lets the capture go. Expect: the first page served at
  /// the capture, and the capture held a delay from its capture, then let go by the keeper, within half a window. Before,
  /// the deadline counted from the request by the operator's ten-second placeholder, so the capture was held ten seconds;
  /// counted from the request by the delay, the first page would have been refused.
  #[test]
  fn a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let request = slates_wire::request::RequestId {
      client: ASKING,
      sequence: 1,
    }
    .word();
    let (asked, report) = crate::daemon::observe_first(&daemon, move |state| {
      state.failover_ns = DELAY_NS;
      let asked = seat(state, ASKING);
      let served = crate::verbs::status_on_channel(
        state,
        asked,
        request,
        &slates_ipc::protocol::RequestBody::DaemonStatus,
      );
      let Some(crate::verbs::Served::Reply(report)) = served else {
        panic!("a one-shard daemon answers its status at once");
      };
      (asked, report)
    });
    // The scatter of a starved daemon: two delays pass before the report is captured.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_nanos(2 * DELAY_NS));
    let captured = Instant::now();
    let first = crate::daemon::observe_first(&daemon, move |state| {
      let mut reply = pages_long(state, asked, report);
      crate::verbs::send_reply(state, asked.index(), request, &mut reply);
      reply
    });
    let released = look_until(&daemon, captured, move |state| {
      state
        .clients
        .get_mut(asked)
        .is_ok_and(|client| !client.status_pages.pending())
    });
    daemon.stop();
    assert!(
      matches!(
        first,
        slates_ipc::protocol::ReplyBody::DaemonStatusPage { .. }
      ),
      "the first page is served at the capture: {first:?}"
    );
    let released = released.expect("the capture was let go within half a window of its capture");
    assert!(
      released >= Duration::from_nanos(DELAY_NS),
      "held a failover delay from its capture: let go {released:?} after it"
    );
  }
}
