//! The exchange's tests (A-616, part 2; §4.17.4 "4. Reads" and "5. The wire").
//!
//! The oracle is stock `svnserve` 1.15.0 in the recorded sessions (`crates/svn/fixtures/report/`,
//! each file's SHA-256 in `SHA256SUMS`): each session's report command and everything after it is
//! replayed through an exchange over the fixtures' repository, the client's answer handed over only
//! once the exchange asks for it, as a stock client sends it; what the exchange writes must be what
//! `svnserve` wrote, item for item before and after the drive, and the drive call for call (sets
//! where `svnserve` is in hash order, texts by what they make). Beside them: an early answer before
//! every one of a checkout's calls, every answer `svn_ra_svn__handle_failure_status` reads, budgets
//! down to one byte and one step, the report cut at every byte, and a call after the answer.

use super::*;
use crate::index::Index;
use crate::report::RequestKind;
use crate::report::drive::green::{
  FixtureGuards, Green, guards_of, normalize, repository, texts_by_md5,
};
use crate::report::recorded::{
  Recorded, SESSIONS, command, context, drive_items, finished_report, items, read_drive,
  recorded, report_command, session,
};
use std::collections::BTreeMap;

/// A budget past anything one call of the fixtures writes.
const PLENTY: Budget = Budget {
  room: 1 << 24,
  steps: 1 << 20,
};

/// The smallest budget: one byte, so one of the drive's calls a turn, and one step.
const SMALLEST: Budget = Budget { room: 1, steps: 1 };

/// The sessions the golden replay must meet: every recorded one with a report command (22 stock, 13
/// hand-made) but the malformed one, whose connection ends before `svnserve` flushed anything.
const REPLAYED: usize = 34;

/// A client's success, as a stock client answers `close-edit`.
const SUCCESS_ANSWER: &[u8] = b"( success ( ) ) ";

/// How a replayed exchange ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
  /// The command was answered whole.
  Answered,
  /// The command was refused at its beginning.
  Failed(ReportFailure),
  /// The connection was refused.
  Refused(Refused),
  /// The client's bytes ran out.
  Waiting,
}

/// The fixtures' repository, and the guards a session's drive ran under.
struct Fixture {
  /// The index.
  index: Index,
  /// The green.
  green: Green,
}

/// One replay: the client's report bytes in pieces, the bytes it sends once asked, and the budget.
struct Replay<'r> {
  /// The report's bytes, each piece handed over when the exchange asks for more.
  pieces: Vec<&'r [u8]>,
  /// What the client sends once the report is over and the exchange asks for more.
  answer: Vec<u8>,
  /// The budget of every call.
  budget: Budget,
  /// When set, the early answer handed over after this many turns that wrote.
  early_after: Option<(usize, Vec<u8>)>,
}

/// What a replay wrote, how it ended, and how many of its turns wrote.
type Replayed = (Vec<u8>, Ending, usize);

/// Replays an exchange begun with `request` in `context`.
fn replay(
  fixture: &Fixture,
  guards: &FixtureGuards,
  context: &Context<'_>,
  request: Request,
  mut plan: Replay<'_>,
) -> Replayed {
  let mut out = Vec::new();
  let mut exchange = match Exchange::begin(context, request, &mut out).unwrap() {
    Opened::Reporting(exchange) => exchange,
    Opened::Failed(failure) => return (out, Ending::Failed(failure), 0),
  };
  let sources = Sources {
    index: &fixture.index,
    repository: &fixture.green,
    guards,
  };
  let mut pieces = plan.pieces.into_iter();
  let mut held: Vec<u8> = Vec::new();
  let mut wrote = 0;
  loop {
    match exchange.receive(&sources, &held, &mut out, plan.budget) {
      Ok(Turn::NeedMore { .. }) => match pieces.next() {
        Some(piece) => held.extend_from_slice(piece),
        None if !plan.answer.is_empty() => held.append(&mut plan.answer),
        None => return (out, Ending::Waiting, wrote),
      },
      Ok(Turn::Took { consumed }) => {
        held.drain(..consumed);
      }
      Ok(Turn::Wrote) => {
        wrote += 1;
        if let Some((_, answer)) = plan.early_after.take_if(|(after, _)| *after == wrote) {
          held.extend_from_slice(&answer);
        }
      }
      Ok(Turn::Answered { consumed }) => {
        held.drain(..consumed);
        assert_eq!(
          exchange.receive(&sources, &held, &mut out, plan.budget),
          Err(Refused::OutOfTurn)
        );
        return (out, Ending::Answered, wrote);
      }
      Err(refused) => return (out, Ending::Refused(refused), wrote),
    }
  }
}

/// A recorded session's report bytes and the answer its client sent after them: the items from the
/// report command to `finish-report`, then the next item, which a client that answered early sent
/// with its report.
fn client_parts(recorded: Recorded, index: usize) -> (Vec<u8>, Vec<u8>) {
  let items = items(recorded.client);
  let finish = items
    .iter()
    .enumerate()
    .skip(index + 1)
    .find(|(_, item)| command(item).is_some_and(|(name, _)| name == "finish-report"))
    .map_or(items.len() - 1, |(at, _)| at);
  let report = items[index + 1..=finish].concat();
  let answer = items.get(finish + 1).map_or_else(Vec::new, |item| item.to_vec());
  if recorded.name == "hostile-report-early-edit-response.1" {
    ([report, answer].concat(), Vec::new())
  } else {
    (report, answer)
  }
}

/// Whether a recorded session holds a report command.
fn has_report(recorded: Recorded) -> bool {
  items(recorded.client)
    .iter()
    .any(|item| command(item).is_some_and(|(name, _)| RequestKind::of(&name).is_some()))
}

/// The guards a session's drive ran under.
fn session_guards(recorded: Recorded) -> FixtureGuards {
  if recorded.name == "update-with-lock.1" {
    guards_of(recorded.name, &finished_report(recorded))
  } else {
    FixtureGuards::anonymous()
  }
}

/// Replays a recorded session's report command through an exchange with `budget`, the report in
/// `cuts`' pieces.
fn replay_session(fixture: &Fixture, recorded: Recorded, budget: Budget, cuts: &[usize]) -> Replayed {
  let (session_path, index, request) = report_command(recorded);
  let established = session();
  let context = context(&established, &session_path, recorded.client);
  let (report, answer) = client_parts(recorded, index);
  let pieces = cut(&report, cuts);
  let replay_of = Replay {
    pieces,
    answer,
    budget,
    early_after: None,
  };
  replay(fixture, &session_guards(recorded), &context, request, replay_of)
}

/// `bytes` in the pieces `cuts` makes.
fn cut<'b>(bytes: &'b [u8], cuts: &[usize]) -> Vec<&'b [u8]> {
  let mut pieces = Vec::new();
  let mut start = 0;
  for at in cuts {
    let end = (*at).clamp(start, bytes.len());
    pieces.push(&bytes[start..end]);
    start = end;
  }
  pieces.push(&bytes[start..]);
  pieces
}

/// The fixtures' repository.
fn fixture() -> Fixture {
  let (index, green) = repository();
  Fixture { index, green }
}

/// The span of `items` that is the drive: from the first editor command while they are editor
/// commands.
fn drive_span(items: &[&[u8]]) -> (usize, usize) {
  let joined: Vec<u8> = items.concat();
  let drive = drive_items(&joined);
  let start = items
    .iter()
    .position(|item| drive.first() == Some(item))
    .unwrap_or(items.len());
  (start, start + drive.len())
}

/// The items `svnserve` writes for the handshake, before a session's first command's answer.
const SERVER_HANDSHAKE_ITEMS: usize = 4;

/// The client items before a session's first command: the handshake's answer and the
/// authentication's.
const CLIENT_HANDSHAKE_ITEMS: usize = 2;

/// Asserts that an exchange wrote what `svnserve` wrote in its session: the items before the
/// drive and after it byte for byte, the drive call for call in the shared form. A session without a
/// drive is a hand-made one whose report command came first, answered right after the handshake.
fn assert_as_svnserve(recorded: Recorded, ours: &[u8], texts: &BTreeMap<[u8; 16], Vec<u8>>) {
  let name = recorded.name;
  let (ours, theirs) = (items(ours), items(recorded.server));
  let (our_start, our_end) = drive_span(&ours);
  let (their_start, their_end) = if our_start == ours.len() {
    assert_eq!(report_command(recorded).1, CLIENT_HANDSHAKE_ITEMS, "{name}: the first command");
    let answered = SERVER_HANDSHAKE_ITEMS + our_start;
    (answered, answered)
  } else {
    drive_span(&theirs)
  };
  let before = their_start.checked_sub(our_start).unwrap();
  assert_eq!(ours[..our_start], theirs[before..their_start], "{name}: before the drive");
  let our_drive = read_drive(&ours[our_start..our_end]).calls;
  let their_drive = read_drive(&theirs[their_start..their_end]).calls;
  assert_eq!(normalize(our_drive, texts), normalize(their_drive, texts), "{name}: the drive");
  let after = &ours[our_end..];
  assert_eq!(after, &theirs[their_end..their_end + after.len()], "{name}: after the drive");
}

/// A-616 part 2 (`accept_report`, `editorp.c` against stock `svnserve` 1.15.0): replay every
/// recorded report command and what follows it through an exchange, the client's answer handed over
/// once asked; expect `svnserve`'s items before and after the drive byte for byte and its drive call
/// for call, every replayed session met.
#[test]
fn every_recorded_exchange_is_answered_as_svnserve_answered_it() {
  let fixture = fixture();
  let texts = texts_by_md5(&fixture.green);
  let mut replayed = 0;
  for recorded in SESSIONS.iter().copied().filter(|recorded| has_report(*recorded)) {
    let (out, ending, _) = replay_session(&fixture, recorded, PLENTY, &[]);
    if let Ending::Refused(refused) = ending {
      assert_eq!(recorded.name, "hostile-report-malformed-set-path.1", "{refused:?}");
      continue;
    }
    replayed += 1;
    assert!(matches!(ending, Ending::Answered | Ending::Failed(_)), "{}: {ending:?}", recorded.name);
    assert_as_svnserve(recorded, &out, &texts);
  }
  assert_eq!(replayed, REPLAYED);
}

/// A-616 part 2 (bounded turns): replay a recorded checkout and an update with one byte and one step
/// a turn, then with the report cut at every byte; expect what the whole budget and the whole report
/// wrote, every time.
#[test]
fn an_exchange_writes_alike_under_any_budget_and_any_cut() {
  let fixture = fixture();
  for name in ["checkout-r1.1", "update-to-head.1"] {
    let source = recorded(name);
    let (whole, ending, _) = replay_session(&fixture, source, PLENTY, &[]);
    assert_eq!(ending, Ending::Answered, "{name}");
    assert_eq!(replay_session(&fixture, source, SMALLEST, &[]).0, whole, "{name}: smallest budget");
    let (_, index, _) = report_command(source);
    let report_length = client_parts(source, index).0.len();
    for at in 0..report_length {
      assert_eq!(replay_session(&fixture, source, PLENTY, &[at]).0, whole, "{name}: cut at {at}");
    }
  }
}

/// The turns a recorded checkout's exchange writes with one call a turn: one for each of its calls
/// before `close-edit`.
fn checkout_turns(fixture: &Fixture) -> usize {
  replay_session(fixture, recorded("checkout-r1.1"), SMALLEST, &[]).2
}

/// Replays the recorded checkout with `answer` handed over after `turns` turns that wrote.
fn early(fixture: &Fixture, turns: usize, answer: &[u8]) -> Replayed {
  let source = recorded("checkout-r1.1");
  let (session_path, index, request) = report_command(source);
  let established = session();
  let context = context(&established, &session_path, source.client);
  let (report, _) = client_parts(source, index);
  let replay_of = Replay {
    pieces: vec![&report],
    answer: Vec::new(),
    budget: SMALLEST,
    early_after: Some((turns, answer.to_vec())),
  };
  replay(fixture, &FixtureGuards::anonymous(), &context, request, replay_of)
}

/// A-616 part 2 (`check_for_error_internal`): answer the recorded checkout's drive early, after
/// each of its calls in turn, with a success and with a failure; expect each time the calls before
/// it, then `abort-edit`, then 210004 "Successful edit status returned too soon" or the client's
/// error written back; and before `close-edit` none is early, so the edit closes.
#[test]
fn an_early_answer_aborts_the_edit_wherever_it_comes() {
  let fixture = fixture();
  let failure = b"( failure ( ( 155000 4:mine 0: 0 ) ) ) ";
  let too_soon = b"( failure ( ( 210004 40:Successful edit status returned too soon 0: 0 ) ) ) ";
  let turns = checkout_turns(&fixture);
  for after in 1..turns {
    for (answer, written) in [(SUCCESS_ANSWER, &too_soon[..]), (failure, &failure[..])] {
      let (out, ending, _) = early(&fixture, after, answer);
      assert_eq!(ending, Ending::Answered, "after {after}");
      let ends = [&b"( abort-edit ( ) ) "[..], written].concat();
      assert!(out.ends_with(&ends), "after {after}: {}", String::from_utf8_lossy(&out));
    }
  }
  let (out, ending, _) = early(&fixture, turns, SUCCESS_ANSWER);
  assert_eq!(ending, Ending::Answered);
  assert!(out.ends_with(b"( close-edit ( ) ) ( success ( ) ) "));
}

/// Replays the hand-made session of an absent target, its drive ending at `close-edit`, with
/// `answer` sent to it.
fn answered(fixture: &Fixture, name: &str, answer: &[u8]) -> Vec<u8> {
  let source = recorded(name);
  let (session_path, index, request) = report_command(source);
  let established = session();
  let context = context(&established, &session_path, source.client);
  let (report, _) = client_parts(source, index);
  let replay_of = Replay {
    pieces: vec![&report],
    answer: answer.to_vec(),
    budget: PLENTY,
    early_after: None,
  };
  let (out, ending, _) = replay(fixture, &FixtureGuards::anonymous(), &context, request, replay_of);
  assert_eq!(ending, Ending::Answered, "{}", String::from_utf8_lossy(answer));
  out
}

/// One answer to `close-edit` and the end the exchange writes after it.
type AnswerCase = (&'static [u8], &'static [u8]);

/// The answers `svn_ra_svn__read_cmd_response` and `svn_ra_svn__handle_failure_status` tell apart,
/// each with what follows `close-edit`.
const CLOSE_ANSWERS: [AnswerCase; 13] = [
  (b"( success ( ) ) ", b"( success ( ) ) "),
  (b"( success ( 5:extra ) ) ", b"( success ( ) ) "),
  (
    b"( failure ( ( 155000 4:mine 6:file.c 42 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 155000 4:mine 6:file.c 42 ) ) ) ",
  ),
  (
    b"( failure ( ( 210000 0: 0: 0 ) ( 155000 4:mine 0: 0 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 155000 4:mine 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( ( 155000 4:mine 0: 0 ) ( 160000 3:two 0: 0 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 155000 4:mine 0: 0 ) ( 160000 3:two 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( ( 210000 0: 0: 0 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 210004 20:Malformed error list 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 210004 16:Empty error list 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( 5 ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 210004 20:Malformed error list 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( ( five 4:mine 0: 0 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 210004 22:Malformed network data 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( ( 155000 5:mi\0ne 0: 0 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 155000 2:mi 0: 0 ) ) ) ",
  ),
  (
    b"( failure ( ( 4294967295 1:x 0: 0 ) ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 18446744073709551615 1:x 0: 0 ) ) ) ",
  ),
  (
    b"( maybe ( ) ) ",
    b"( abort-edit ( ) ) ( failure ( ( 210004 42:Unknown status 'maybe' in command response 0: 0 ) ) ) ",
  ),
  (
    b"( success ) ",
    b"( abort-edit ( ) ) ( failure ( ( 210004 22:Malformed network data 0: 0 ) ) ) ",
  ),
];

/// A-616 part 2 (`ra_svn_close_edit`, `svn_ra_svn__read_cmd_response`,
/// `svn_ra_svn__handle_failure_status`): answer `close-edit` every way the client may, success and
/// failure with wrappers, chains, C strings, numbers past an `int`, unknown statuses and shapes;
/// expect after `close-edit` exactly what `svnserve` writes for each.
#[test]
fn every_answer_to_close_edit_is_read_as_svnserve_reads_it() {
  let fixture = fixture();
  for (answer, ending) in CLOSE_ANSWERS {
    let out = answered(&fixture, "hostile-update-target-absent.1", answer);
    let expected = [&b"( close-edit ( ) ) "[..], ending].concat();
    assert!(
      out.ends_with(&expected),
      "{}: {}",
      String::from_utf8_lossy(answer),
      String::from_utf8_lossy(&out)
    );
  }
}

/// A-616 part 2 (`finish_report`'s `svn_error_compose_create` with `ra_svn_abort_edit`): answer a
/// drive's own `abort-edit` (a missing path) with a success and with a failure; expect the drive's
/// error alone, then the drive's error with the client's after it.
#[test]
fn a_drives_refusal_is_written_with_the_clients_answer_after_it() {
  let fixture = fixture();
  let message = "Working copy path 'missing' does not exist in repository";
  let missing = format!("( 160013 {}:{message} 0: 0 ) ", message.len()).into_bytes();
  let alone = answered(&fixture, "hostile-report-missing-path.1", SUCCESS_ANSWER);
  let expected = [&b"( abort-edit ( ) ) ( failure ( "[..], &missing, b") ) "].concat();
  assert!(alone.ends_with(&expected), "{}", String::from_utf8_lossy(&alone));
  let theirs = b"( failure ( ( 155000 4:mine 0: 0 ) ) ) ";
  let both = answered(&fixture, "hostile-report-missing-path.1", theirs);
  let expected = [
    &b"( abort-edit ( ) ) ( failure ( "[..],
    &missing,
    b"( 155000 4:mine 0: 0 ) ) ) ",
  ]
  .concat();
  assert!(both.ends_with(&expected), "{}", String::from_utf8_lossy(&both));
}

/// A-616 part 2 (bounds): answer `close-edit` with an item nested four lists deep and with bytes
/// that are no item; expect the connection refused, nothing written after `close-edit`.
#[test]
fn an_answer_past_its_bounds_ends_the_connection() {
  let fixture = fixture();
  for answer in [&b"( success ( ( ( ) ) ) ) "[..], b") "] {
    let source = recorded("hostile-update-target-absent.1");
    let (session_path, index, request) = report_command(source);
    let established = session();
    let context = context(&established, &session_path, source.client);
    let (report, _) = client_parts(source, index);
    let replay_of = Replay {
      pieces: vec![&report],
      answer: answer.to_vec(),
      budget: PLENTY,
      early_after: None,
    };
    let (out, ending, _) =
      replay(&fixture, &FixtureGuards::anonymous(), &context, request, replay_of);
    assert!(matches!(ending, Ending::Refused(Refused::Item(_))), "{ending:?}");
    assert!(out.ends_with(b"( close-edit ( ) ) "));
  }
}
