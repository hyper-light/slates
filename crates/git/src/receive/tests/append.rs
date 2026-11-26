//! The appends that move refs (A-620 with A-642: each ref keeps its own log; an atomic push is one
//! entry on each ref it moves, appended only while each is at the position its verdict read).

use std::collections::BTreeMap;

use super::{ADVERTISED, Repo, commit_text, pack_of, request, unbounded, zero};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::receive::{
  ATOMIC_FAILURE, ATOMIC_TRANSACTION_FAILED, Command, Declined, FAILED_TO_UPDATE, Judge,
  NoneHidden, Outcome, Pushed, Reported, message, receive,
};

/// Each ref's own log, as A-642 keeps it: a ref's position is how many entries its log holds. A
/// verdict reads the position; an append adds one entry, only while the ref is still at the
/// position read. A racing ref is moved by another push between this push's verdict on it and the
/// append.
#[derive(Default)]
struct Logs {
  /// Each ref's position, by name; a ref with no entry is at the start.
  positions: BTreeMap<Vec<u8>, u64>,
  /// The refs another push moves before this push's append on them.
  racing: Vec<Vec<u8>>,
  /// The refs whose verdict declines.
  declining: Vec<Vec<u8>>,
  /// Every append made, as the refs it moved.
  appended: Vec<Vec<Vec<u8>>>,
  /// Each ref's rank, by name ([`Judge::rank`]); a ref with none ranks 0.
  ranks: BTreeMap<Vec<u8>, u64>,
}

impl Logs {
  /// Where `name`'s log is.
  fn position(&self, name: &[u8]) -> u64 {
    self.positions.get(name).copied().unwrap_or_default()
  }

  /// Adds one entry to `name`'s log.
  fn advance(&mut self, name: &[u8]) {
    *self.positions.entry(name.to_vec()).or_default() += 1;
  }

  /// Lets another push move `name` first, if it races.
  fn race(&mut self, name: &[u8]) {
    if let Some(at) = self.racing.iter().position(|racing| racing == name) {
      self.racing.remove(at);
      self.advance(name);
    }
  }
}

/// The band-2 line of a ref that moved since it was read, as a test words it.
fn moved_line(name: &[u8]) -> Vec<u8> {
  message(&[b"cannot lock ref '", name, b"': it moved"].concat())
}

impl Judge for Logs {
  type Accepted = u64;

  fn rank(&mut self, command: &Command, _pushed: &Pushed<'_>) -> u64 {
    self.ranks.get(&command.name).copied().unwrap_or_default()
  }

  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<u64, Declined> {
    if self.declining.contains(&command.name) {
      return Err(Declined {
        reason: b"hook declined".to_vec(),
        hook: Vec::new(),
        message: None,
      });
    }
    Ok(self.position(&command.name))
  }

  fn append(&mut self, command: &Command, read: u64) -> Result<Vec<Reported>, Declined> {
    self.race(&command.name);
    if self.position(&command.name) != read {
      return Err(Declined {
        reason: FAILED_TO_UPDATE.to_vec(),
        hook: Vec::new(),
        message: Some(moved_line(&command.name)),
      });
    }
    self.advance(&command.name);
    self.appended.push(vec![command.name.clone()]);
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, u64)>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    for (command, _) in &accepted {
      self.race(&command.name);
    }
    let moved = accepted
      .iter()
      .find(|(command, read)| self.position(&command.name) != *read);
    if let Some((command, _)) = moved {
      return Err(Declined {
        reason: ATOMIC_TRANSACTION_FAILED.to_vec(),
        hook: Vec::new(),
        message: Some(moved_line(&command.name)),
      });
    }
    for (command, _) in &accepted {
      self.advance(&command.name);
    }
    let names = accepted.iter().map(|(command, _)| command.name.clone());
    self.appended.push(names.collect());
    Ok(Vec::new())
  }
}

/// The three refs every push here creates.
const NAMES: [&[u8]; 3] = [b"refs/heads/a", b"refs/heads/b", b"refs/heads/c"];

/// The outcomes of a push creating the three refs at one commit, `atomic` or not, judged and
/// appended by `logs`.
fn three_creates(atomic: bool, logs: &mut Logs) -> Vec<Outcome> {
  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let commit = commit_text(tree, &[]);
  let id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let commands: Vec<(ObjectId, ObjectId, &[u8])> =
    NAMES.iter().map(|name| (zero(), id, *name)).collect();
  let capabilities: &[u8] = if atomic {
    b" report-status atomic"
  } else {
    b" report-status"
  };
  let pack = pack_of(&[(Kind::Commit, &commit)]);
  let bytes = request(&[], &commands, capabilities, &[], &pack);
  let received = receive(
    &bytes,
    &ADVERTISED,
    &repository,
    &NoneHidden,
    logs,
    &mut unbounded(),
  );
  received.unwrap().outcomes
}

/// A refusal for `reason` with `line` on band 2.
fn refused(reason: &[u8], line: Option<Vec<u8>>) -> Outcome {
  Outcome::Ng(Declined {
    reason: reason.to_vec(),
    hook: Vec::new(),
    message: line,
  })
}

/// `names` as owned names.
fn owned(names: &[&[u8]]) -> Vec<Vec<u8>> {
  names.iter().map(|name| name.to_vec()).collect()
}

/// A-620 with A-642 (a push without `atomic` moves each ref on its own:
/// `execute_commands_non_atomic`). Do: create three refs, the second moved by another push between
/// its verdict and its append. Expect: the second refused `failed to update ref` with its band-2
/// line; the first and the third applied, each by an append of its own, the second's refusal
/// holding neither.
#[test]
fn without_atomic_each_ref_moves_on_its_own_log() {
  let mut logs = Logs {
    racing: owned(&[b"refs/heads/b"]),
    ..Logs::default()
  };
  let outcomes = three_creates(false, &mut logs);
  let failed = refused(FAILED_TO_UPDATE, Some(moved_line(b"refs/heads/b")));
  let applied = Outcome::Ok(Vec::new());
  assert_eq!(outcomes, [applied.clone(), failed, applied]);
  let each = [owned(&[b"refs/heads/a"]), owned(&[b"refs/heads/c"])];
  assert_eq!(logs.appended, each);
}

/// A-850 (a push without `atomic` decides its commands by rank, its outcomes in the order sent). Do: create three refs
/// ranked c, then a, then b, the second sent (b) declined by its verdict. Expect: c appended before a, b never; the
/// outcomes in the order sent, a applied, b its own reason, c applied.
#[test]
fn without_atomic_commands_are_decided_by_rank_and_reported_as_sent() {
  let mut logs = Logs {
    declining: owned(&[b"refs/heads/b"]),
    ranks: [
      (b"refs/heads/a", 1),
      (b"refs/heads/b", 2),
      (b"refs/heads/c", 0),
    ]
    .into_iter()
    .map(|(name, rank)| (name.to_vec(), rank))
    .collect(),
    ..Logs::default()
  };
  let outcomes = three_creates(false, &mut logs);
  let applied = Outcome::Ok(Vec::new());
  assert_eq!(
    outcomes,
    [applied.clone(), refused(b"hook declined", None), applied]
  );
  let each = [owned(&[b"refs/heads/c"]), owned(&[b"refs/heads/a"])];
  assert_eq!(logs.appended, each);
}

/// A-620 with A-642 (an atomic push is one entry on each ref it moves). Do: create three refs with
/// `atomic`, nothing racing. Expect: every command applied, by one append that moved all three.
#[test]
fn an_atomic_push_appends_to_every_ref_at_once() {
  let mut logs = Logs::default();
  let outcomes = three_creates(true, &mut logs);
  assert_eq!(outcomes, vec![Outcome::Ok(Vec::new()); 3]);
  assert_eq!(logs.appended, [owned(&NAMES)]);
}

/// A-620 with A-642 (an atomic push appends only while every ref is at the position read; git's
/// `atomic transaction failed`). Do: create three refs with `atomic`, the second moved by another
/// push between the verdicts and the append. Expect: nothing appended; every command `atomic
/// transaction failed`, the band-2 line once, with the first.
#[test]
fn an_atomic_push_whose_ref_moved_appends_to_none() {
  let mut logs = Logs {
    racing: owned(&[b"refs/heads/b"]),
    ..Logs::default()
  };
  let outcomes = three_creates(true, &mut logs);
  let first = refused(ATOMIC_TRANSACTION_FAILED, Some(moved_line(b"refs/heads/b")));
  let others = refused(ATOMIC_TRANSACTION_FAILED, None);
  assert_eq!(outcomes, [first, others.clone(), others]);
  assert!(logs.appended.is_empty());
}

/// A-620 with A-642 (a verdict moves nothing). Do: create three refs with `atomic`, the third's
/// verdict declined. Expect: nothing appended, though the first two were accepted; those two
/// `atomic push failure`, the third its own reason.
#[test]
fn a_verdict_moves_nothing_and_a_declined_atomic_push_appends_nothing() {
  let mut logs = Logs {
    declining: owned(&[b"refs/heads/c"]),
    ..Logs::default()
  };
  let outcomes = three_creates(true, &mut logs);
  let failure = refused(ATOMIC_FAILURE, None);
  let declined = refused(b"hook declined", None);
  assert_eq!(outcomes, [failure.clone(), failure, declined]);
  assert!(logs.appended.is_empty());
  assert!(logs.positions.is_empty());
}

/// One-commit repository and matching pack shared by D/F ref-conflict scenarios.
struct SingleCommitFixture {
  /// Repository holding the empty tree.
  repository: Repo,
  /// Object id of the root commit carried in `pack`.
  commit_id: ObjectId,
  /// Single-object pack carrying the root commit.
  pack: Vec<u8>,
}

impl SingleCommitFixture {
  /// Builds the empty-tree repository and single-commit pack.
  fn new() -> Self {
    let mut repository = Repo::default();
    let tree = repository.with(Kind::Tree, b"");
    let commit = commit_text(tree, &[]);
    let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
    let pack = pack_of(&[(Kind::Commit, &commit)]);
    Self {
      repository,
      commit_id,
      pack,
    }
  }

  /// Runs `receive` for `commands` (`atomic` or not) through `judge` and returns the outcomes.
  fn push<J: Judge>(
    &self,
    commands: &[(ObjectId, ObjectId, &[u8])],
    atomic: bool,
    judge: &mut J,
  ) -> Vec<Outcome> {
    let capabilities: &[u8] = if atomic {
      b" report-status atomic"
    } else {
      b" report-status"
    };
    let bytes = request(&[], commands, capabilities, &[], &self.pack);
    receive(
      &bytes,
      &ADVERTISED,
      &self.repository,
      &NoneHidden,
      judge,
      &mut unbounded(),
    )
    .unwrap()
    .outcomes
  }
}

/// Asserts `outcome` was refused for `reason`.
fn assert_declined_reason(outcome: &Outcome, reason: &[u8]) {
  match outcome {
    Outcome::Ng(declined) => assert_eq!(declined.reason, reason),
    other => panic!("expected refusal, got {other:?}"),
  }
}

/// Asserts `outcome` was refused for `reason` with a band-2 message containing `needle`.
fn assert_declined_with_message(outcome: &Outcome, reason: &[u8], needle: &str) {
  match outcome {
    Outcome::Ng(declined) => {
      assert_eq!(declined.reason, reason);
      let bytes = declined
        .message
        .as_deref()
        .expect("expected band-2 message");
      let text = String::from_utf8_lossy(bytes);
      assert!(text.contains(needle), "expected {needle:?} in {text:?}");
    }
    other => panic!("expected refusal, got {other:?}"),
  }
}

/// Verifies atomic intra-batch D/F conflicts roll back the entire batch in both ref orders,
/// including across an ASCII-intervening sibling (`'-' < '/'`).
fn assert_atomic_intra_batch_df_conflicts(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut logs = Logs::default();
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    true,
    &mut logs,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert_declined_with_message(&outcomes[1], FAILED_TO_UPDATE, "exists; cannot create");
  assert!(logs.appended.is_empty());

  let mut logs = Logs::default();
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic/docs"),
      (zero(), id, b"refs/heads/topic-1"),
      (zero(), id, b"refs/heads/topic"),
    ],
    true,
    &mut logs,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert_eq!(outcomes[1], refused(ATOMIC_FAILURE, None));
  assert_declined_reason(&outcomes[2], FAILED_TO_UPDATE);
  assert!(logs.appended.is_empty());
}

/// Verifies non-conflicting prefix shares (`topic` vs `topic2` and `refs/tags/topic/docs`) succeed.
fn assert_non_conflicting_prefix_batch(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut logs = Logs::default();
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic2"),
      (zero(), id, b"refs/tags/topic/docs"),
    ],
    true,
    &mut logs,
  );
  assert_eq!(outcomes, vec![Outcome::Ok(Vec::new()); 3]);
  assert_eq!(logs.appended.len(), 1);
}

/// Verifies non-atomic intra-batch D/F conflict refuses only the conflicting second ref.
fn assert_non_atomic_df_conflict_batch(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut logs = Logs::default();
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
      (zero(), id, b"refs/heads/other"),
    ],
    false,
    &mut logs,
  );
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert_declined_reason(&outcomes[1], FAILED_TO_UPDATE);
  assert_eq!(outcomes[2], Outcome::Ok(Vec::new()));
  assert_eq!(
    logs.appended,
    [owned(&[b"refs/heads/topic"]), owned(&[b"refs/heads/other"])]
  );
}

/// Verifies a non-atomic judge refusal on a parent ref leaves its child ref unblocked.
fn assert_non_atomic_declined_ref_does_not_block_child(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut logs = Logs {
    declining: owned(&[b"refs/heads/topic"]),
    ..Logs::default()
  };
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut logs,
  );
  assert_eq!(outcomes[0], refused(b"hook declined", None));
  assert_eq!(outcomes[1], Outcome::Ok(Vec::new()));
  assert_eq!(logs.appended, [owned(&[b"refs/heads/topic/docs"])]);
}

/// §4.17.2 "A push" (files-backend ref directory/file namespace conflicts): do push intra-batch
/// conflicting refs (`refs/heads/topic` and `refs/heads/topic/docs`, both orders, including an
/// ASCII-intervening sibling `refs/heads/topic-1` where `'-' < '/'`), non-conflicting prefix
/// shares (`refs/heads/topic` and `refs/heads/topic2`, `refs/tags/topic/docs`), and non-atomic
/// sibling creates. Expect: conflicting pairs are refused `failed to update ref` with Git's
/// `exists; cannot create` message, `--atomic` rolls back the entire batch with `atomic push
/// failure`, non-conflicting names succeed, and without `--atomic` a declined first ref does not
/// block the second.
#[test]
fn df_ref_namespace_conflicts_are_refused_and_atomic_batches_roll_back() {
  let fixture = SingleCommitFixture::new();
  assert_atomic_intra_batch_df_conflicts(&fixture);
  assert_non_conflicting_prefix_batch(&fixture);
  assert_non_atomic_df_conflict_batch(&fixture);
  assert_non_atomic_declined_ref_does_not_block_child(&fixture);
}

/// A test judge holding pre-existing repository ref names so cross-push D/F namespace conflict
/// checks (including delete-then-create replacement and atomic rollback) are verified end to end.
#[derive(Default)]
struct ExistingRefsJudge {
  /// Refs already present in the repository before the push.
  existing: std::collections::BTreeSet<Vec<u8>>,
  /// Refs whose verdict declines.
  declining: Vec<Vec<u8>>,
  /// Applied commands recorded as batches (one entry per non-atomic append, one vec per atomic).
  appended: Vec<Vec<Vec<u8>>>,
}

impl ExistingRefsJudge {
  /// Builds a judge pre-populated with `existing` repository ref names.
  fn with_existing(existing: &[&[u8]]) -> Self {
    Self {
      existing: existing.iter().map(|name| name.to_vec()).collect(),
      ..Self::default()
    }
  }
}

impl Judge for ExistingRefsJudge {
  type Accepted = ();

  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<(), Declined> {
    if self.declining.contains(&command.name) {
      return Err(Declined {
        reason: b"hook declined".to_vec(),
        hook: Vec::new(),
        message: None,
      });
    }
    Ok(())
  }

  fn append(&mut self, command: &Command, (): ()) -> Result<Vec<Reported>, Declined> {
    if command.is_delete() {
      self.existing.remove(&command.name);
    } else {
      self.existing.insert(command.name.clone());
    }
    self.appended.push(vec![command.name.clone()]);
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, ())>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    for (command, _) in &accepted {
      if command.is_delete() {
        self.existing.remove(&command.name);
      }
    }
    for (command, _) in &accepted {
      if !command.is_delete() {
        self.existing.insert(command.name.clone());
      }
    }
    self
      .appended
      .push(accepted.iter().map(|(c, _)| c.name.clone()).collect());
    Ok(Vec::new())
  }

  fn ref_conflict(&self, name: &[u8], deleted: &[&[u8]]) -> Option<Vec<u8>> {
    self
      .existing
      .iter()
      .find(|existing| {
        !deleted.contains(&existing.as_slice())
          && crate::receive::refs_conflict(existing.as_slice(), name)
      })
      .cloned()
  }
}

/// Verifies `refs_conflict` boundary cases across ancestor/descendant and sibling prefixes.
fn assert_refs_conflict_boundaries() {
  let cases: [(&[u8], &[u8], bool); 7] = [
    (b"refs/heads/topic", b"refs/heads/topic/docs", true),
    (b"refs/heads/topic/docs", b"refs/heads/topic", true),
    (b"refs/heads/topic", b"refs/heads/topic/a/b", true),
    (b"refs/heads/topic", b"refs/heads/topic2", false),
    (b"refs/heads/topic", b"refs/heads/topic", false),
    (b"refs/heads/a/b", b"refs/heads/a/c", false),
    (b"refs/heads/topic", b"refs/tags/topic/docs", false),
  ];
  for (left, right, expected) in cases {
    assert_eq!(crate::receive::refs_conflict(left, right), expected);
  }
}

/// Verifies atomic pushes creating a child or parent of a pre-existing ref roll back cleanly.
fn assert_atomic_cross_push_df_conflicts(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut judge = ExistingRefsJudge::with_existing(&[b"refs/heads/topic"]);
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/good"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    true,
    &mut judge,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert_declined_with_message(&outcomes[1], FAILED_TO_UPDATE, "'refs/heads/topic' exists");
  assert!(judge.appended.is_empty());

  let mut judge = ExistingRefsJudge::with_existing(&[b"refs/heads/topic/docs"]);
  let outcomes = fixture.push(&[(zero(), id, b"refs/heads/topic")], true, &mut judge);
  assert_declined_reason(&outcomes[0], FAILED_TO_UPDATE);
  assert!(judge.appended.is_empty());
}

/// Verifies atomic delete of `refs/heads/topic` paired with creating `refs/heads/topic/docs` succeeds.
fn assert_atomic_single_delete_replace(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut judge = ExistingRefsJudge::with_existing(&[b"refs/heads/topic"]);
  let outcomes = fixture.push(
    &[
      (id, zero(), b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    true,
    &mut judge,
  );
  assert_eq!(outcomes, vec![Outcome::Ok(Vec::new()); 2]);
  assert!(!judge.existing.contains(b"refs/heads/topic".as_slice()));
  assert!(judge.existing.contains(b"refs/heads/topic/docs".as_slice()));
}

/// Verifies partial delete of a multi-child directory rolls back while deleting every child allows
/// creating the parent ref.
fn assert_atomic_multi_child_delete_replace(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut judge = ExistingRefsJudge::with_existing(&[b"refs/heads/topic/a", b"refs/heads/topic/b"]);
  let outcomes = fixture.push(
    &[
      (id, zero(), b"refs/heads/topic/a"),
      (zero(), id, b"refs/heads/topic"),
    ],
    true,
    &mut judge,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert_declined_reason(&outcomes[1], FAILED_TO_UPDATE);
  assert!(judge.appended.is_empty());

  let outcomes = fixture.push(
    &[
      (id, zero(), b"refs/heads/topic/a"),
      (id, zero(), b"refs/heads/topic/b"),
      (zero(), id, b"refs/heads/topic"),
    ],
    true,
    &mut judge,
  );
  assert_eq!(outcomes, vec![Outcome::Ok(Vec::new()); 3]);
  assert!(judge.existing.contains(b"refs/heads/topic".as_slice()));
}

/// Verifies non-atomic cross-push conflicts, clean delete-then-create, and declined-delete blocking.
fn assert_non_atomic_cross_push_df_and_replace(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut judge = ExistingRefsJudge::with_existing(&[b"refs/heads/topic"]);
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/good"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert_declined_reason(&outcomes[1], FAILED_TO_UPDATE);

  let mut judge = ExistingRefsJudge::with_existing(&[b"refs/heads/topic"]);
  let outcomes = fixture.push(
    &[
      (id, zero(), b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes, vec![Outcome::Ok(Vec::new()); 2]);

  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic".to_vec()].into_iter().collect(),
    declining: owned(&[b"refs/heads/topic"]),
    ..ExistingRefsJudge::default()
  };
  let outcomes = fixture.push(
    &[
      (id, zero(), b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes[0], refused(b"hook declined", None));
  assert_declined_reason(&outcomes[1], FAILED_TO_UPDATE);
}

/// Verifies `funny refname` (`refs/heads/topic/`) is reported ahead of D/F conflict checks.
fn assert_funny_refname_precedes_df_conflict(fixture: &SingleCommitFixture) {
  let id = fixture.commit_id;
  let mut judge = ExistingRefsJudge::default();
  let outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert_declined_reason(&outcomes[1], b"funny refname");

  let mut atomic_judge = ExistingRefsJudge::default();
  let atomic_outcomes = fixture.push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/"),
    ],
    true,
    &mut atomic_judge,
  );
  assert_eq!(atomic_outcomes[0], refused(ATOMIC_FAILURE, None));
  assert_declined_reason(&atomic_outcomes[1], b"funny refname");
  assert!(atomic_judge.appended.is_empty());
}

/// §4.17.2 "A push" (cross-push D/F ref namespace conflicts against pre-existing refs, including
/// delete-then-create replacement and atomic rollback): do test `refs_conflict` boundary cases,
/// existing `refs/heads/topic` vs creating `refs/heads/topic/docs` (and vice versa), atomic
/// delete-then-create allowed, partial delete of multi-child directory refused, full delete of
/// multi-child directory allowed, and non-atomic delete-then-create (both succeeding and when the
/// delete is declined). Expect: every post-batch D/F conflict is rejected (rolling back `--atomic`
/// batches cleanly), while clean replacements and non-conflicting names succeed.
#[test]
fn cross_push_df_ref_namespace_conflicts_and_delete_replace() {
  assert_refs_conflict_boundaries();
  let fixture = SingleCommitFixture::new();
  assert_atomic_cross_push_df_conflicts(&fixture);
  assert_atomic_single_delete_replace(&fixture);
  assert_atomic_multi_child_delete_replace(&fixture);
  assert_non_atomic_cross_push_df_and_replace(&fixture);
  assert_funny_refname_precedes_df_conflict(&fixture);
}
