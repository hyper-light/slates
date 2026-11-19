//! The deadlines of what a shard's clients leave behind, each kept on the shard's timing wheel (A-522, A-536, A-554;
//! the event-driven reaper's pieces 2 to 4): a staging buffer expires a lease after its last use (`crate::staging`), a
//! status capture at its absolute deadline (`crate::status_pages`), a volume's write lease at its term (D-16, the
//! partition's lease wheel), a CLI attachment at its term's lapse (A-230, the partition's index of terms), a destroy
//! whose completion record was refused at its retry deadline (the serve loop, woken then, steps it again), a grant at
//! the first instant past its term (A-266, the grants' index of terms), and the look at what the shard owes the audit
//! (A-265), a retry window after the record that armed it and again while anything is owed. One task a shard, the
//! keeper, sleeps on the wheel until the earliest of them, releases what is due then, and sleeps until the next; a
//! deadline set earlier than the one it sleeps for wakes it at once, so it re-arms. Nothing is scanned on a tick: each
//! pass reads only the due entries of ordered indexes (the staging buffers by expiry, the seats by their capture's
//! deadline, the lease wheel, the terms). Until A-522, A-536 and A-554 the reap loop's tick released each once a
//! liveness window, every client's status capture asked and every grant walked at every tick, and a shard with none
//! woke anyway.
//!
//! **Woken by whatever sets an earlier deadline.** The staging verbs and the status verb tell the keeper themselves
//! ([`noted`], [`capture_set`]). A lease, a term or a grant is entered by many paths (a take, a renewal, a replay, an
//! issue, a restore), so the partition and the grants table note the earliest they entered and every state borrow
//! closes by handing that to the keeper ([`after_borrow`]): three reads of an `Option` a borrow.
//!
//! **A deadline its job could not end** (a record the catalog refused) is tried again the shard's retry window on
//! ([`retry_window_ns`]), counted `deadline.stuck`, never spun on.
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
  // The leases first, at their own clock read: a lease whose holder keeps a live write attachment is renewed rather than
  // released (`verbs::expire_leases`).
  let _acted = crate::verbs::expire_leases(state);
  // A CLI attachment whose term lapsed ends, within its term of its last use (A-230).
  let _ended = crate::verbs::expire_lapsed_attachments(state);
  let now = state.clock.monotonic_ns();
  state.staging.expire(now, &mut state.store.metadata);
  // A grant past its term ends, recorded expired with its audit record in one atom (A-266), at the first instant past
  // its term (A-554); one whose record is refused stays due, and is tried again a retry window on.
  if state
    .landing
    .grants
    .next_due_ns()
    .is_some_and(|at| at <= now)
  {
    crate::landing::end_expired_grants(state);
  }
  // What this partition owes the audit and has not had acknowledged a retry window after it was sent is sent again
  // (A-265), at its look's deadline (A-554); the look is armed again while anything is owed.
  if state.landing.audit_look_at.is_some_and(|at| at <= now) {
    crate::landing::look_at_owed_audit(state, now);
  }
  // A look at departures some shard did not answer is asked for again at its retry (A-222, A-554): the reaper takes it.
  if state.departures_retry_at.is_some_and(|at| at <= now) {
    state.departures_retry_at = None;
    crate::remote_attach::look_for_departures(state);
  }
  // A taken-back map no holder claimed by its claim's deadline is reclaimed by the reaper (A-197, A-554; Linux).
  #[cfg(target_os = "linux")]
  crate::held_copy::owe_reclaim_if_due(state, now);
  // A purge round the NFSv4 table owes (at a start that owes purges, or a retry window after a round left any owed) is
  // handed to the reaper, which awaits its owners (A-570).
  if state.nfs_purge_at.is_some_and(|at| at <= now) {
    crate::nfs::owe_purge_round(state);
  }
  // §4.6, the backed route's L1 (A-193): the host is asked again for the placements it declined in a way likely to
  // hold, a retry window after the decline (A-570). A declined collapse costs milliseconds of compaction (1,954 and
  // 2,832 µs a step on adacachy at load 90), so a host out of huge pages is asked once a window, never once a range.
  if state.placement_rearm_at.is_some_and(|at| at <= now) {
    state.placement_rearm_at = None;
    state.store.shards_backings.rearm_placement();
  }
  // A refused destroy record's retry: the serve loop steps destroys, so it is woken to (`verbs::step_destroys`).
  if state.destroy_retry_at.is_some_and(|at| at <= now) {
    state.destroy_retry_at = None;
    if let Some(server) = state.server_task {
      slates_rt::registry::wake(server.0);
    }
  }
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
  let earliest = [
    state.staging.next_expiry(),
    state.capture_deadlines.first().map(|(at, ..)| *at),
    state.db.partition_mut().next_lease_expiry(),
    state.db.partition().next_term_lapse(),
    state.destroy_retry_at,
    state.landing.grants.next_due_ns(),
    state.landing.audit_look_at,
    state.departures_retry_at,
    claim_due(state),
    state.nfs_purge_at,
    state.placement_rearm_at,
  ]
  .into_iter()
  .flatten()
  .min();
  if earliest.is_some_and(|at| at <= now) {
    state.count(DEADLINE_STUCK, 1);
  }
  let next = armed_for(earliest, now, retry_window_ns());
  state.deadline_armed = next;
  next.map(|at| at.saturating_sub(now))
}

/// Derived: the retry window (A-536, named by A-554): how long after a refusal a deadline its job could not end, or a
/// refused record's retry (`verbs::step_destroys`), is tried again. It is the shard's window (A-455,
/// `watchdog::watch_window_ns`): the liveness window `W`, the recovery budget by rent or buy (A-359), raised to the
/// scheduler overrun this shard measured, never past the observation budget. A refusal's cause (a full segment, a
/// partner's full channel) is cleared by work that a live shard shows progress on within its window, so a retry sooner
/// meets the same refusal and spends a pass, and the watchdog would call a job held longer a stall; a host that keeps
/// the shard off its CPU longer than `W` lengthens it as much.
pub(crate) fn retry_window_ns() -> u64 {
  crate::watchdog::watch_window_ns(slates_rt::futures::scheduler_overrun_ns())
}

/// When the next taken-back map's claim is due (A-197, A-554; `crate::held_copy::next_claim_deadline`): Linux alone
/// takes writable copies back from its anchor.
#[cfg(target_os = "linux")]
fn claim_due(state: &ShardState) -> Option<u64> {
  crate::held_copy::next_claim_deadline(state)
}

/// When the next taken-back map's claim is due: never, where no copy is taken back (macOS and Windows, A-197).
#[cfg(not(target_os = "linux"))]
fn claim_due(_state: &ShardState) -> Option<u64> {
  None
}

/// When the keeper sleeps until, given the earliest deadline its pass left: that deadline, or, for one still past after
/// the pass (a job that could not end it: a record the catalog refused), `window` after `now` ([`retry_window_ns`]), so
/// it is tried again a window on and never spun on. The red check of a retry the keeper never took found the spin: the
/// watchdog ended the test daemon, its shard 3.5 s on its CPU without a step (A-536).
fn armed_for(earliest: Option<u64>, now: u64, window: u64) -> Option<u64> {
  earliest.map(|at| {
    if at <= now {
      now.saturating_add(window)
    } else {
      at
    }
  })
}

/// Format: the counter of the keeper's passes that left a deadline past due (a job's record refused), each tried again a
/// retry window later ([`armed_for`], [`retry_window_ns`]).
const DEADLINE_STUCK: &str = "deadline.stuck";

/// Closes every state borrow (`state::try_with_state`): a lease, a CLI attachment's term or a grant the borrow entered
/// to lapse before the deadline the keeper sleeps for wakes it, so it re-arms (A-536, A-554). The keeper's own pass
/// notes the leases it renews, after it armed for them, so they wake nothing.
pub(crate) fn after_borrow(state: &mut ShardState) {
  // The fleet's members changed: the control shard's reaper looks for hosts that left (A-222, A-554).
  if state.fleet.take_members_changed() {
    crate::remote_attach::look_for_departures(state);
  }
  // The host declined a huge-page placement in a way likely to hold: it is asked again a retry window on (A-570).
  if state.store.shards_backings.take_declined() && state.placement_rearm_at.is_none() {
    let at = state.clock.monotonic_ns().saturating_add(retry_window_ns());
    state.placement_rearm_at = Some(at);
    noted(state, at);
  }
  let partition = state.db.partition_mut();
  let earliest = [
    partition.take_noted_lease(),
    partition.take_noted_term(),
    state.landing.grants.take_noted(),
  ]
  .into_iter()
  .flatten()
  .min();
  if let Some(at) = earliest {
    noted(state, at);
  }
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

  /// A-536 (leases off the tick). Do: on a one-shard daemon, create a volume and record its write lease to lapse a tenth
  /// of a liveness window from now, no attachment holding it, as a take records it; look until it is released. Expect:
  /// released within half a window, by the keeper at the lease wheel's tick for it. The reap loop's tick, which released
  /// it before, runs once a window, and an idle daemon runs no serve round, whose own pass also releases leases.
  #[test]
  fn a_lapsed_write_lease_is_released_at_its_term_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let begun = Instant::now();
    let volume = crate::daemon::observe_first(&daemon, |state| {
      let volume = crate::verbs::to_db_volume(created(state, "lapsing"));
      let now = state.clock.monotonic_ns();
      state
        .db
        .mutate(
          &mut state.segment,
          &slates_db::Op::LeaseTaken {
            volume,
            lease: slates_db::catalog::LeaseRecord {
              holder: principal(),
              epoch: 1,
              expires_ns: now.saturating_add(SOON_NS),
            },
          },
          now,
        )
        .unwrap();
      volume
    });
    let released = look_until(&daemon, begun, move |state| {
      state
        .db
        .partition()
        .volume(volume)
        .is_some_and(|record| record.lease.is_none())
    });
    daemon.stop();
    let released = released.expect("the lease was released within half a liveness window");
    println!("a write lease due in {SOON_NS} ns released {released:?} after it was recorded");
  }

  /// A-536. Do: arm the keeper for no deadline, for one ahead of now, for one at now and for one behind it, as a pass
  /// leaves them. Expect: none, that one, and the window after now for both the others: a deadline its job could not end
  /// is tried again a window on, never spun on (job 3's red check found the spin).
  #[test]
  fn a_deadline_still_past_after_its_pass_is_tried_again_a_window_on() {
    assert_eq!(super::armed_for(None, 10, 5), None);
    assert_eq!(super::armed_for(Some(12), 10, 5), Some(12));
    assert_eq!(super::armed_for(Some(10), 10, 5), Some(15));
    assert_eq!(super::armed_for(Some(3), 10, 5), Some(15));
  }

  /// A-536 (destroys off the tick). Do: on a one-shard daemon, set a refused destroy record's retry a tenth of a
  /// liveness window from now, as `verbs::step_destroys` sets it at a refusal; look until the keeper has taken it.
  /// Expect: taken within half a window, at its deadline, the serve loop woken then to step the destroy again. Before,
  /// the reap loop's tick stepped destroys once a window, whether a record was refused or not.
  #[test]
  fn a_refused_destroys_retry_is_kept_at_its_deadline_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let begun = Instant::now();
    crate::daemon::observe_first(&daemon, |state| {
      let at = state.clock.monotonic_ns().saturating_add(SOON_NS);
      state.destroy_retry_at = Some(at);
      super::noted(state, at);
    });
    let taken = look_until(&daemon, begun, |state| state.destroy_retry_at.is_none());
    daemon.stop();
    let taken = taken.expect("the retry was kept within half a liveness window");
    println!("a destroy's retry due in {SOON_NS} ns kept {taken:?} after it was set");
  }

  /// Shape: the landing the grant test presents, by its id.
  const LANDING: u64 = 7;

  /// A landing presented to [`principal`] and awaiting its grant, as the landing verb leaves one (§4.15 step 3): what
  /// `landing::issue_grant` binds.
  fn awaiting() -> crate::landing::Awaiting {
    crate::landing::Awaiting {
      manifest: [7; 32],
      binding: slates_land::grant::GrantBinding {
        consumer: principal().key().into_boxed_slice(),
        volume: [9; 16],
        snapshot: 1,
        target: slates_land::grant::TargetIdentity {
          key: "/landing".into(),
          device: 1,
          inode: 2,
        },
      },
      volume: VolumeId { bytes: [9; 16] },
      snapshot: slates_db::catalog::SnapshotId { value: 1 },
      target: "/landing".to_owned(),
      principal: principal(),
      client: 1,
      entries: Vec::new(),
      conflicts: std::collections::BTreeMap::new(),
    }
  }

  /// A-554 (grants off the tick). Do: on a one-shard daemon, present a landing awaiting its grant and issue the grant
  /// under the volume's policy with a term of a tenth of a liveness window, as a bypass presentation does; look until it
  /// has left the shard's table and the durable one and its expiry is in the audit. Expect: ended within half a window,
  /// by the keeper at the first instant past its term, recorded and audited in one atom (A-266). The reap loop's tick,
  /// which ended grants past their term before, runs once a window.
  #[test]
  fn a_grant_past_its_term_ends_at_its_term_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let begun = Instant::now();
    let grant = crate::daemon::observe_first(&daemon, |state| {
      state.landing.awaiting.insert(LANDING, awaiting());
      crate::landing::issue_grant(
        state,
        &principal(),
        LANDING,
        (slates_ipc::protocol::GrantScope::Once, SOON_NS),
        crate::landing::Issuer::Policy,
      )
      .unwrap()
    });
    let ended = look_until(&daemon, begun, move |state| {
      state
        .landing
        .grants
        .get(slates_land::grant::GrantId(grant))
        .is_none()
        && state.db.partition().grant(grant).is_none()
        && state.db.partition().audit().any(|record| {
          record.grant == Some(grant) && record.kind == slates_db::catalog::AuditKind::GrantExpired
        })
    });
    daemon.stop();
    let ended = ended.expect("the grant ended and was audited within half a liveness window");
    println!("a grant with a {SOON_NS} ns term ended {ended:?} after it was issued");
  }

  /// Shape: the certificate anchors and member ids of the two peers the departure test's control shard learns: one that
  /// stays a member and one that leaves.
  const STAYING_ANCHOR: u64 = 0x51_0001;
  /// Shape: see [`STAYING_ANCHOR`].
  const STAYING_MEMBER: u64 = 0x51_0002;
  /// Shape: see [`STAYING_ANCHOR`].
  const LEAVING_ANCHOR: u64 = 0x1e_0001;
  /// Shape: see [`STAYING_ANCHOR`].
  const LEAVING_MEMBER: u64 = 0x1e_0002;
  /// Shape: how long the departure test waits for the control shard to count the leaving peer among the members: a
  /// window and a half, past the reap loop's first look a window into the start.
  const COUNTED_WITHIN_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 2 * 3;

  /// A-554 (departed hosts off the tick; A-222). Do: on a one-shard daemon, learn two peers (each anchor's member id,
  /// as the control shard learns them on contact), configure both members beside the node, and record an attachment of
  /// one of the leaving peer's clients on a volume here; wait until the control shard counts the leaving peer's anchor
  /// among the members'; then install a configuration without it, as the council's commit of its retirement does, and
  /// look until the attachment is gone. Expect: ended within half a window of the install, by a look the change itself
  /// asked for. The reap loop's tick looked once a window, and its look was what counted the peer, so the install that
  /// followed it waited a whole window for the next.
  #[test]
  fn a_departed_hosts_attachment_ends_when_the_fleet_lets_it_go_without_a_tick() {
    use slates_db::HostId;
    let daemon = crate::daemon::audit_daemon_of(1);
    let attachment = crate::daemon::observe_first(&daemon, |state| {
      for (anchor, member) in [
        (STAYING_ANCHOR, STAYING_MEMBER),
        (LEAVING_ANCHOR, LEAVING_MEMBER),
      ] {
        state.learned_members.insert(
          HostId(anchor),
          crate::state::LearnedMember {
            boot_nonce: 1,
            host: HostId(member),
            previous: None,
          },
        );
      }
      let local = state.fleet.host();
      let configuration = state.fleet.configuration().clone();
      state.fleet.install_configuration(
        configuration,
        &[local, HostId(STAYING_MEMBER), HostId(LEAVING_MEMBER)],
      );
      let volume = crate::verbs::to_db_volume(created(state, "departing"));
      let record = state.db.partition().volume(volume).unwrap().clone();
      let id = crate::verbs::next_attachment_id(state);
      let now = state.clock.monotonic_ns();
      let added = slates_db::Op::AttachmentAdded {
        record: slates_db::catalog::AttachmentRecord {
          id,
          volume: record.id,
          consumer: slates_db::catalog::Consumer::Remote {
            host: LEAVING_ANCHOR,
            client: 1,
          },
          snapshot: None,
          form: slates_db::catalog::AttachForm::Root,
          principal: record.owner.clone(),
          rights: crate::verbs::rights_of(&record, &record.owner),
          token: [0; 16],
          mount: None,
        },
      };
      state.db.mutate(&mut state.segment, &added, now).unwrap();
      id
    });
    // The control shard's count of the members' anchors is what a departure is judged against (A-222); the test waits
    // for it to name the leaving peer before the peer leaves.
    let begun = Instant::now();
    while !crate::daemon::observe_first(&daemon, |state| {
      state.configured_members.contains(&LEAVING_ANCHOR)
    }) {
      assert!(
        begun.elapsed() < Duration::from_nanos(COUNTED_WITHIN_NS),
        "the control shard counted the leaving peer among the members"
      );
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    }
    let retired = Instant::now();
    crate::daemon::observe_first(&daemon, |state| {
      let local = state.fleet.host();
      let configuration = state.fleet.configuration().clone();
      state
        .fleet
        .install_configuration(configuration, &[local, HostId(STAYING_MEMBER)]);
    });
    let ended = look_until(&daemon, retired, move |state| {
      state.db.partition().attachment(attachment).is_none()
    });
    daemon.stop();
    let ended = ended.expect("the departed peer's attachment ended within half a liveness window");
    println!(
      "a departed peer's attachment ended {ended:?} after the configuration without it was installed"
    );
  }

  /// Shape: how long the re-arm test lets its daemon run before the decline: past the reap loop's first look a window
  /// into the start, by half a window, so the loop's next look comes half a window after the decline.
  const PAST_THE_FIRST_LOOK_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 2 * 3;

  /// A-570 (the huge-page re-arm off the tick). Do: on a one-shard daemon past the reap loop's first look, remember a
  /// lasting decline of a huge-page placement, as a declined collapse does (`ShardsBackings::place`); watch for the
  /// re-arm. Expect: re-armed no sooner than a window after the decline and within half a window more, by the keeper.
  /// The reap loop's tick re-armed at its looks whatever their phase: half a window after the decline here, and at once
  /// after a decline just before a look, which asked the host again before it could have freed anything.
  #[test]
  fn a_declined_placement_is_asked_again_a_window_after_the_decline() {
    let daemon = crate::daemon::audit_daemon_of(1);
    // The harness waits out the reap loop's first look; the daemon under test runs on.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_nanos(PAST_THE_FIRST_LOOK_NS));
    let declined = Instant::now();
    crate::daemon::observe_first(&daemon, |state| {
      state.store.shards_backings.remember_decline();
    });
    let rearmed = loop {
      if !crate::daemon::observe_first(&daemon, |state| {
        state.store.shards_backings.placement_declined()
      }) {
        break Some(declined.elapsed());
      }
      if declined.elapsed()
        > Duration::from_nanos(crate::daemon::LIVENESS_BUDGET_NS + RELEASED_WITHIN_NS)
      {
        break None;
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    };
    daemon.stop();
    let rearmed = rearmed.expect("re-armed within half a window past a window after the decline");
    println!("a declined placement re-armed {rearmed:?} after the decline");
    assert!(
      rearmed >= Duration::from_nanos(crate::daemon::LIVENESS_BUDGET_NS),
      "not asked again before a window after the decline: {rearmed:?}"
    );
  }

  /// A volume named `name` that [`principal`] creates on `state`'s shard, through the verb: its id.
  fn created(state: &mut ShardState, name: &str) -> slates_ipc::protocol::VolumeId {
    let reply = crate::verbs::dispatch(
      state,
      1,
      &principal(),
      slates_ipc::protocol::RequestBody::Create {
        name: name.to_owned(),
        size: slates_ipc::protocol::SizeClass::Bounded { limit: 1 << 20 },
        names: slates_ipc::protocol::NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let slates_ipc::protocol::ReplyBody::Created { id } = reply else {
      panic!("the volume was not created: {reply:?}")
    };
    id
  }

  /// A-536 (CLI attachments off the tick). Do: on a one-shard daemon whose failover delay, a CLI attachment's term, is a
  /// tenth of a liveness window, create a volume and attach it for reading in the CLI's form; look until the attachment
  /// is gone. Expect: ended within half a window, by the keeper at its term's lapse, found in the partition's index of
  /// terms. The reap loop's tick, which walked every attachment for a lapsed term once a window, no longer does.
  #[test]
  fn a_lapsed_cli_attachment_ends_at_its_term_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let begun = Instant::now();
    let attachment = crate::daemon::observe_first(&daemon, |state| {
      state.failover_ns = SOON_NS;
      let volume = created(state, "termed");
      let reply = crate::verbs::dispatch(
        state,
        1,
        &principal(),
        slates_ipc::protocol::RequestBody::Attach {
          volume,
          snapshot: None,
          intent: slates_ipc::protocol::Intent::Read,
          form: slates_ipc::protocol::AttachRequest::Termed,
        },
      );
      let slates_ipc::protocol::ReplyBody::Attached { attachment, .. } = reply else {
        panic!("the volume was not attached: {reply:?}")
      };
      attachment
    });
    let ended = look_until(&daemon, begun, move |state| {
      state.db.partition().attachment(attachment).is_none()
    });
    daemon.stop();
    let ended = ended.expect("the attachment ended within half a liveness window");
    println!("a CLI attachment with a {SOON_NS} ns term ended {ended:?} after it was made");
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
        identities: None,
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
