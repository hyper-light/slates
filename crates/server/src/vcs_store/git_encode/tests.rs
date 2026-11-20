//! A-718 continued's tests: a fetch's bounded wait for its repository's encoding, and the task that finishes it.

use super::{BEHIND, encode_within};

/// A-718 continued (a fetch never waits past its window). Do: push the fixtures' `create` and `update` into a git
/// repository, leaving its versions unencoded; run a fetch's encoding with a wait of nothing; then let the shard run
/// until the encoding is idle. Expect: the wait ends with the encoding not idle and one fetch counted behind; the task
/// it started encodes every version (none unencoded), then lets the repository go from the set it keeps.
#[test]
fn t_a718_a_fetch_that_stops_waiting_leaves_a_task_that_finishes_the_encoding() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let id = crate::git_push::tests::git_repository(state, "behind");
    crate::git_push::tests::pushed_from_the_fixtures(state, id, &["create", "update"]);
    let unencoded = state
      .vcs_stores
      .git_ref(id)
      .and_then(|(_, store)| store.repository.first_unencoded());
    assert!(unencoded.is_some(), "the pushes left versions unencoded");
    let waited = slates_rt::futures::spawn(async move {
      assert!(
        !encode_within(id, 0).await,
        "a wait of nothing ends before the encoding"
      );
    })
    .unwrap();
    let _ = slates_rt::futures::detach(waited);
    id
  });
  let mut done = None;
  // Derived: 600 looks 20 ms apart, the wait the encoding is given to end.
  let deadline =
    std::time::Instant::now() + std::time::Duration::from_millis(20).saturating_mul(600);
  while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
    let seen = crate::daemon::observe_first(&daemon, move |state| {
      let unencoded = state
        .vcs_stores
        .git_ref(id)
        .and_then(|(_, store)| store.repository.first_unencoded());
      (
        state.refusals.get(BEHIND).copied().unwrap_or(0),
        unencoded,
        state.vcs_stores.encodings.1.contains(&id),
      )
    });
    if seen.0 > 0 && seen.1.is_none() && !seen.2 {
      done = Some(seen);
      break;
    }
    std::thread::park_timeout(std::time::Duration::from_millis(20).min(left));
  }
  daemon.stop();
  assert_eq!(done, Some((1, None, false)));
}
