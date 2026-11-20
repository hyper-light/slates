//! ra_svn's replays (`serve.c` `replay`, `replay_range` and `replay_one_revision`, Subversion 1.15.0; A-703):
//! `replay ( revision:number low-water-mark:number send-deltas:bool )` and
//! `replay-range ( start:number end:number low-water-mark:number send-deltas:bool )`, what `svnsync` reads a source
//! repository by and `svnrdump dump` dumps one by.
//!
//! **The answer.** The trivial authentication request first. Then, for each revision, `replay-range` sends its
//! properties the session may see (`( revprops ( proplist ) )`, `svn_repos_fs_revision_proplist`), and both send the
//! revision's replay over the connection's editor (A-703's [`Replay`], written by the update drive's
//! [`EditorWriter`], a writer a revision so each revision's tokens begin again), then `finish-replay`. The command ends
//! with `( success ( ) )`.
//!
//! **Refusals.** A revision past the youngest: `replay` answers `abort-edit` then its failure, as
//! `replay_one_revision` aborts the editor; `replay-range` answers its failure alone, which reading the revision's
//! properties meets first. Either way the connection goes on.
//!
//! **Bounded.** A call writes at most its budget's room and steps of the replay, and the connection asks again
//! ([`super::Turn::Wrote`]), as a long read's answer is resumed.

use super::{Connection, Established, Phase, Refused, Sources, write_failure};
use crate::index::Revision;
use crate::ra_svn::{self, Value};
use crate::reads;
use crate::report::drive::Call;
use crate::report::drive::replay::Replay;
use crate::report::editor::EditorWriter;
use crate::report::exchange::Budget;

/// `replay`'s command word.
const REPLAY: &str = "replay";
/// `replay-range`'s command word.
const REPLAY_RANGE: &str = "replay-range";
/// ra_svn's true.
const TRUE: &str = "true";
/// ra_svn's false.
const FALSE: &str = "false";
/// The word of a command's success.
const SUCCESS: &str = "success";
/// The word that ends each revision's replay.
const FINISH_REPLAY: &str = "finish-replay";
/// The word of a revision's properties in `replay-range`.
const REVPROPS: &str = "revprops";

/// Whether `name` is one of the replays.
pub(super) fn is_replay(name: &str) -> bool {
  matches!(name, REPLAY | REPLAY_RANGE)
}

/// Where a replay's answer stands.
#[derive(Debug)]
enum Stage {
  /// The next revision begins: its properties, for a range.
  Begin,
  /// The revision's replay is being written.
  Drive(Box<(Replay, EditorWriter)>),
  /// Every revision is replayed: the success is owed.
  Done,
}

/// A replay being answered (module documentation).
#[derive(Debug)]
pub(super) struct Replaying {
  /// The next revision to replay.
  next: Revision,
  /// The last revision to replay.
  last: Revision,
  /// The low-water mark: copies from before it go without their source.
  low_water_mark: Revision,
  /// Whether texts and properties go as deltas.
  send_deltas: bool,
  /// Whether it is `replay-range`, which sends each revision's properties.
  range: bool,
  /// Where its answer stands.
  stage: Stage,
}

/// Reads the replay `name` asks with `params`; none where a parameter is not what its prototype names (`rrb`, `rrrb`),
/// which `svnserve` reads as malformed.
fn read(name: &str, params: &[Value<'_>]) -> Option<Replaying> {
  let (first, last, low_water_mark, deltas, range) = match (name, params) {
    (
      REPLAY,
      [
        Value::Number(revision),
        Value::Number(low),
        Value::Word(deltas),
        ..,
      ],
    ) => (*revision, *revision, *low, *deltas, false),
    (
      REPLAY_RANGE,
      [
        Value::Number(first),
        Value::Number(last),
        Value::Number(low),
        Value::Word(deltas),
        ..,
      ],
    ) => (*first, *last, *low, *deltas, true),
    _ => return None,
  };
  let send_deltas = match deltas {
    TRUE => true,
    FALSE => false,
    _ => return None,
  };
  Some(Replaying {
    next: first,
    last,
    low_water_mark,
    send_deltas,
    range,
    stage: if first > last {
      Stage::Done
    } else {
      Stage::Begin
    },
  })
}

impl Connection {
  /// Begins the replay `name` asks with `params` (module documentation): the trivial authentication request written,
  /// the answer to go on at the next call.
  pub(super) fn begin_replay(
    &self,
    name: &str,
    params: &[Value<'_>],
    out: &mut Vec<u8>,
  ) -> Result<Replaying, Refused> {
    let replaying = read(name, params).ok_or(Refused::Malformed)?;
    let request = Value::List(vec![
      Value::Word(SUCCESS),
      Value::List(vec![Value::List(Vec::new()), Value::String(b"")]),
    ]);
    ra_svn::write(&request, out).map_err(Refused::Item)?;
    Ok(replaying)
  }

  /// Goes on with a replay's answer within `budget` (module documentation).
  pub(super) fn replaying<'a, R>(
    &mut self,
    mut established: Box<Established>,
    mut replaying: Box<Replaying>,
    sources: &Sources<'_, R>,
    out: &mut Vec<u8>,
    budget: Budget,
  ) -> Result<super::Turn<'a>, Refused>
  where
    R: reads::Repository + ?Sized,
  {
    let start = out.len();
    let mut steps = budget.steps;
    let capabilities = established.session.capabilities;
    let base = established.base.clone();
    let most_open = self.limits.most_open;
    let rules = established.compiled_read_rules(&sources.policy).cloned();
    loop {
      if out.len().saturating_sub(start) >= budget.room || steps == 0 {
        self.phase = Phase::Replaying(established, replaying);
        return Ok(super::Turn::Wrote);
      }
      steps = steps.saturating_sub(1);
      match std::mem::replace(&mut replaying.stage, Stage::Done) {
        Stage::Begin => {
          if replaying.range {
            let properties = reads::readable_revision_properties(
              sources.index,
              sources.repository,
              rules.as_ref(),
              replaying.next,
            );
            match properties {
              Ok(list) => {
                let item = Value::List(vec![Value::Word(REVPROPS), list]);
                ra_svn::write(&item, out).map_err(Refused::Item)?;
              }
              Err(failure) => {
                write_failure(&[failure], out)?;
                self.phase = Phase::Commands(established);
                return Ok(super::Turn::Wrote);
              }
            }
          }
          let replay = Replay::new(
            replaying.next,
            &base,
            replaying.low_water_mark,
            replaying.send_deltas,
          );
          let writer = EditorWriter::new(&capabilities, most_open);
          replaying.stage = Stage::Drive(Box::new((replay, writer)));
        }
        Stage::Drive(mut driving) => {
          let (replay, writer) = &mut *driving;
          match replay.next(sources.index, sources.repository, rules.as_ref()) {
            Ok(Some(call)) => {
              writer
                .write(&call, out)
                .map_err(|refused| Refused::Report(crate::report::Refused::Editor(refused)))?;
              replaying.stage = Stage::Drive(driving);
            }
            Ok(None) => {
              let finish = Value::List(vec![Value::Word(FINISH_REPLAY), Value::List(Vec::new())]);
              ra_svn::write(&finish, out).map_err(Refused::Item)?;
              replaying.next = replaying.next.saturating_add(1);
              replaying.stage = if replaying.next > replaying.last {
                Stage::Done
              } else {
                Stage::Begin
              };
            }
            Err(failure) => {
              writer
                .write(&Call::AbortEdit, out)
                .map_err(|refused| Refused::Report(crate::report::Refused::Editor(refused)))?;
              write_failure(&[(failure.code, failure.message)], out)?;
              self.phase = Phase::Commands(established);
              return Ok(super::Turn::Wrote);
            }
          }
        }
        Stage::Done => {
          let success = Value::List(vec![Value::Word(SUCCESS), Value::List(Vec::new())]);
          ra_svn::write(&success, out).map_err(Refused::Item)?;
          self.phase = Phase::Commands(established);
          return Ok(super::Turn::Wrote);
        }
      }
    }
  }
}
