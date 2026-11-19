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
