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
  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let commit = commit_text(tree, &[]);
  let id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let pack = pack_of(&[(Kind::Commit, &commit)]);

  let push = |commands: &[(ObjectId, ObjectId, &[u8])], atomic: bool, logs: &mut Logs| {
    let caps: &[u8] = if atomic {
      b" report-status atomic"
    } else {
      b" report-status"
    };
    let bytes = request(&[], commands, caps, &[], &pack);
    receive(
      &bytes,
      &ADVERTISED,
      &repository,
      &NoneHidden,
      logs,
      &mut unbounded(),
    )
    .unwrap()
    .outcomes
  };

  // 1. Atomic: refs/heads/topic + refs/heads/topic/docs -> whole batch rolls back.
  let mut logs = Logs::default();
  let outcomes = push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    true,
    &mut logs,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined)
      if declined.reason == FAILED_TO_UPDATE
        && declined
          .message
          .as_deref()
          .is_some_and(|m| String::from_utf8_lossy(m).contains("exists; cannot create"))
  ));
  assert!(logs.appended.is_empty());

  // 2. Atomic reversed order with ASCII-between sibling `refs/heads/topic-1` ('-' < '/').
  let mut logs = Logs::default();
  let outcomes = push(
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
  assert!(matches!(
    &outcomes[2],
    Outcome::Ng(declined) if declined.reason == FAILED_TO_UPDATE
  ));
  assert!(logs.appended.is_empty());

  // 3. Non-conflicting prefix shares MUST succeed: topic vs topic2, and refs/heads/topic vs refs/tags/topic/docs.
  let mut logs = Logs::default();
  let outcomes = push(
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

  // 4. Non-atomic: first succeeds, conflicting second refused, unrelated third succeeds.
  let mut logs = Logs::default();
  let outcomes = push(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
      (zero(), id, b"refs/heads/other"),
    ],
    false,
    &mut logs,
  );
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined) if declined.reason == FAILED_TO_UPDATE
  ));
  assert_eq!(outcomes[2], Outcome::Ok(Vec::new()));
  assert_eq!(
    logs.appended,
    [owned(&[b"refs/heads/topic"]), owned(&[b"refs/heads/other"])]
  );

  // 5. Non-atomic: when the first ref is declined by the judge, the second is not blocked.
  let mut logs = Logs {
    declining: owned(&[b"refs/heads/topic"]),
    ..Logs::default()
  };
  let outcomes = push(
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

/// §4.17.2 "A push" (cross-push D/F ref namespace conflicts against pre-existing refs, including
/// delete-then-create replacement and atomic rollback): do test `refs_conflict` boundary cases,
/// existing `refs/heads/topic` vs creating `refs/heads/topic/docs` (and vice versa), atomic
/// delete-then-create allowed, partial delete of multi-child directory refused, full delete of
/// multi-child directory allowed, and non-atomic delete-then-create (both succeeding and when the
/// delete is declined). Expect: every post-batch D/F conflict is rejected (rolling back `--atomic`
/// batches cleanly), while clean replacements and non-conflicting names succeed.
#[test]
fn cross_push_df_ref_namespace_conflicts_and_delete_replace() {
  assert!(crate::receive::refs_conflict(
    b"refs/heads/topic",
    b"refs/heads/topic/docs"
  ));
  assert!(crate::receive::refs_conflict(
    b"refs/heads/topic/docs",
    b"refs/heads/topic"
  ));
  assert!(crate::receive::refs_conflict(
    b"refs/heads/topic",
    b"refs/heads/topic/a/b"
  ));
  assert!(!crate::receive::refs_conflict(
    b"refs/heads/topic",
    b"refs/heads/topic2"
  ));
  assert!(!crate::receive::refs_conflict(
    b"refs/heads/topic",
    b"refs/heads/topic"
  ));
  assert!(!crate::receive::refs_conflict(
    b"refs/heads/a/b",
    b"refs/heads/a/c"
  ));
  assert!(!crate::receive::refs_conflict(
    b"refs/heads/topic",
    b"refs/tags/topic/docs"
  ));

  let mut repository = Repo::default();
  let tree = repository.with(Kind::Tree, b"");
  let commit = commit_text(tree, &[]);
  let id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let pack = pack_of(&[(Kind::Commit, &commit)]);

  let run =
    |commands: &[(ObjectId, ObjectId, &[u8])], atomic: bool, judge: &mut ExistingRefsJudge| {
      let caps: &[u8] = if atomic {
        b" report-status atomic"
      } else {
        b" report-status"
      };
      let bytes = request(&[], commands, caps, &[], &pack);
      receive(
        &bytes,
        &ADVERTISED,
        &repository,
        &NoneHidden,
        judge,
        &mut unbounded(),
      )
      .unwrap()
      .outcomes
    };

  // 1. Existing `refs/heads/topic`: atomic push of `good` + `topic/docs` rolls back whole batch.
  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic".to_vec()].into_iter().collect(),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(
    &[
      (zero(), id, b"refs/heads/good"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    true,
    &mut judge,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined)
      if declined.reason == FAILED_TO_UPDATE
        && declined
          .message
          .as_deref()
          .is_some_and(|m| String::from_utf8_lossy(m).contains("'refs/heads/topic' exists"))
  ));
  assert!(judge.appended.is_empty());

  // 2. Existing `refs/heads/topic/docs`: atomic push creating `refs/heads/topic` is refused.
  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic/docs".to_vec()].into_iter().collect(),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(&[(zero(), id, b"refs/heads/topic")], true, &mut judge);
  assert!(matches!(
    &outcomes[0],
    Outcome::Ng(declined) if declined.reason == FAILED_TO_UPDATE
  ));
  assert!(judge.appended.is_empty());

  // 3. Existing `refs/heads/topic`: atomic delete `topic` + create `topic/docs` is ALLOWED.
  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic".to_vec()].into_iter().collect(),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(
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

  // 4. Existing `topic/a` + `topic/b`: deleting only `topic/a` and creating `topic` is REFUSED.
  let mut judge = ExistingRefsJudge {
    existing: [
      b"refs/heads/topic/a".to_vec(),
      b"refs/heads/topic/b".to_vec(),
    ]
    .into_iter()
    .collect(),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(
    &[
      (id, zero(), b"refs/heads/topic/a"),
      (zero(), id, b"refs/heads/topic"),
    ],
    true,
    &mut judge,
  );
  assert_eq!(outcomes[0], refused(ATOMIC_FAILURE, None));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined) if declined.reason == FAILED_TO_UPDATE
  ));
  assert!(judge.appended.is_empty());

  // 5. Existing `topic/a` + `topic/b`: deleting BOTH and creating `topic` is ALLOWED.
  let outcomes = run(
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

  // 6. Non-atomic: existing `topic`, push `good` + `topic/docs` -> `good` OK, `topic/docs` refused.
  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic".to_vec()].into_iter().collect(),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(
    &[
      (zero(), id, b"refs/heads/good"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined) if declined.reason == FAILED_TO_UPDATE
  ));

  // 7. Non-atomic: delete `topic` then create `topic/docs` -> both succeed.
  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic".to_vec()].into_iter().collect(),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(
    &[
      (id, zero(), b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes, vec![Outcome::Ok(Vec::new()); 2]);

  // 8. Non-atomic: delete `topic` is declined by judge -> creating `topic/docs` MUST be refused.
  let mut judge = ExistingRefsJudge {
    existing: [b"refs/heads/topic".to_vec()].into_iter().collect(),
    declining: owned(&[b"refs/heads/topic"]),
    ..ExistingRefsJudge::default()
  };
  let outcomes = run(
    &[
      (id, zero(), b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/docs"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes[0], refused(b"hook declined", None));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined) if declined.reason == FAILED_TO_UPDATE
  ));

  // 9. Funny refname (`refs/heads/topic/`) must report `funny refname`, NOT D/F conflict,
  // even alongside valid `refs/heads/topic`.
  let mut judge = ExistingRefsJudge::default();
  let outcomes = run(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/"),
    ],
    false,
    &mut judge,
  );
  assert_eq!(outcomes[0], Outcome::Ok(Vec::new()));
  assert!(matches!(
    &outcomes[1],
    Outcome::Ng(declined) if declined.reason == b"funny refname"
  ));
  let mut atomic_judge = ExistingRefsJudge::default();
  let atomic_outcomes = run(
    &[
      (zero(), id, b"refs/heads/topic"),
      (zero(), id, b"refs/heads/topic/"),
    ],
    true,
    &mut atomic_judge,
  );
  assert_eq!(atomic_outcomes[0], refused(ATOMIC_FAILURE, None));
  assert!(matches!(
    &atomic_outcomes[1],
    Outcome::Ng(declined) if declined.reason == b"funny refname"
  ));
  assert!(atomic_judge.appended.is_empty());
}
