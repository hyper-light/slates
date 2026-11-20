
#[cfg(test)]
mod owed_audit_tests {
  use std::time::{Duration, Instant};

  use slates_db::catalog::{AuditKind as DbAuditKind, AuditRecord as DbAuditRecord, Principal};
  use slates_vfs::clock::Clock;

  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: how long the test lets its daemon run before the record is owed: past the reap loop's first look, a window
  /// after its start, by a tenth of a window, and short of its second.
  const PAST_THE_FIRST_LOOK_NS: u64 = LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10;
  /// Shape: how long past a window after the send the test waits for the batch to be sent again: half a window.
  const RESENT_WITHIN_NS: u64 = LIVENESS_BUDGET_NS / 2;
  /// Shape: the pause between the test's looks at the shard's count, short beside half a window.
  const LOOK_PAUSE_NS: u64 = 1_000_000;
  /// Shape: the partition the record is owed from: the second shard's, so the control partition is another shard's.
  const OWING: usize = 1;

  /// A-554 (the owed audit off the tick; A-265's resend). Do: on a two-shard daemon past the reap loop's first look,
  /// with the second shard's acknowledgements withheld so its batch stays owed as if each were lost, owe the control
  /// partition one audit record there and send it as a verb's transaction does; watch the shard's resends. Expect: the
  /// batch sent again no sooner than a window after its send, when its acknowledgement could still be on its way, and
  /// within half a window more, by the shard's deadline keeper. The reap loop's tick looked once a window and sent again
  /// a front it had seen at its look before, so a record owed just after a look was sent again nearly two windows on.
  #[test]
  fn an_unacknowledged_audit_batch_is_sent_again_a_window_after_its_send() {
    let daemon = crate::daemon::audit_daemon_of(2);
    // The harness waits out the reap loop's first look; the daemon under test runs on.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_nanos(PAST_THE_FIRST_LOOK_NS));
    let sent = Instant::now();
    crate::daemon::observe_at(&daemon, OWING, |state| {
      state.landing.acknowledgement_withheld = true;
      let now = state.clock.monotonic_ns();
      let record = DbAuditRecord {
        seq: 0,
        at_ns: now,
        kind: DbAuditKind::LandingPlanned,
        principal: Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        grant: None,
        landing: Some(1),
        manifest: None,
        outcome: None,
        signature: None,
      };
      super::write_audit(state, vec![record], now).unwrap();
      super::send_owed_audit(state);
    });
    let resent = loop {
      let count = crate::daemon::observe_at(&daemon, OWING, |state| {
        state
          .refusals
          .get(super::AUDIT_RESENT)
          .copied()
          .unwrap_or(0)
      });
      if count > 0 {
        break Some(sent.elapsed());
      }
      if sent.elapsed() > Duration::from_nanos(LIVENESS_BUDGET_NS + RESENT_WITHIN_NS) {
        break None;
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    };
    daemon.stop();
    let resent = resent.expect("the batch was sent again within half a window past a window after its send");
    println!("an unacknowledged batch was sent again {resent:?} after its send");
    assert!(
      resent >= Duration::from_nanos(LIVENESS_BUDGET_NS),
      "not sent again before its acknowledgement could come: {resent:?}"
    );
  }
}
