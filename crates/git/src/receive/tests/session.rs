//! One push over one connection (A-620's session, sans-io): its bytes in any pieces, against the
//! push held whole, with the push waiting checked between its reading and its verdicts.

use super::{
  ADVERTISED, HIDDEN, Repo, Scenario, advertisement, answer_to, checked_in_turns, git_policy,
  named, received, refs, scenario, scenarios, unbounded,
};
use crate::pktline;
use crate::receive::{
  Received, Refusal, Requested, Session, receive, write_hook_words, write_report_end,
  write_report_held,
};

/// The sizes of the pieces a session is fed a whole push in: a byte, a few, more than a packet's
/// header, and all at once.
const PIECES: [usize; 5] = [1, 3, 7, 61, usize::MAX];

/// What a session gave: its answer or refusal, and every byte it wrote.
struct Answered {
  /// The refusal that ended it, or what its answer received (`None` when it never answered).
  result: Result<Option<Received>, Refusal>,
  /// Every byte written.
  written: Vec<u8>,
}

/// Feeds `bytes` to a session of a push to `repository` in pieces of `piece` bytes until it is
/// checked or refuses, ends its input if neither, then answers it judged by git's policy.
fn fed_in_pieces(bytes: &[u8], piece: usize, repository: &Repo) -> Answered {
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  let mut written = Vec::new();
  let mut checked = Ok(false);
  for chunk in bytes.chunks(piece) {
    checked = session.input(chunk, repository, &mut unbounded());
    if !matches!(checked, Ok(false)) {
      break;
    }
  }
  if checked == Ok(false) {
    checked = session.end(repository, &mut unbounded());
  }
  if checked == Ok(true) {
    checked = checked_in_turns(&mut session, repository).1;
  }
  let result = checked.and_then(|_| {
    let mut policy = git_policy(repository);
    session.answer(&mut policy, &mut written, &mut unbounded())
  });
  Answered { result, written }
}

/// Asserts that a session of `scenario`'s push, its refs advertised first and its push fed in
/// pieces of `piece` bytes, is checked as the last piece arrives, holding the pushed objects, and
/// once answered has written `whole`, the bytes of the advertisement and the answer to the push
/// held whole.
fn assert_checked_as_the_last_piece_arrives(scenario: &Scenario, piece: usize, whole: &[u8]) {
  let label = format!("{} in pieces of {piece}", scenario.name);
  let repository = Repo::from_pack(scenario.before_pack);
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  let refs = refs(scenario.before_refs);
  let mut written = Vec::new();
  session.advertise(&named(&refs), &mut written).unwrap();
  let pieces: Vec<&[u8]> = scenario.request.chunks(piece).collect();
  let (last, before) = pieces.split_last().unwrap();
  for piece in before {
    let checked = session.input(piece, &repository, &mut unbounded());
    assert_eq!(checked, Ok(false), "{label}");
  }
  assert_eq!(session.input(last, &repository, &mut unbounded()), Ok(true));
  assert_eq!(checked_in_turns(&mut session, &repository).1, Ok(true));
  let pushed = match received(scenario, &repository).unpacked {
    Some(Ok(unpacked)) => unpacked.objects.len(),
    _ => 0,
  };
  let objects = session.checked().map(|checked| checked.objects().len());
  assert_eq!(objects, Some(pushed), "{label}");
  let mut policy = git_policy(&repository);
  let answered = session.answer(&mut policy, &mut written, &mut unbounded());
  assert!(
    matches!(answered, Ok(Some(_))) && session.is_done(),
    "{label}"
  );
  assert_eq!(written, whole, "{label}");
}

/// A-620 (the session: one push over one connection, sans-io). Do: open a session on each captured
/// push's server, advertise its refs, feed it the push in pieces of 1, 3, 7 and 61 bytes and whole,
/// then answer it. Expect: not checked before the last piece; checked as it arrives with no end of
/// input asked, the delete's as its request ends (no pack follows it), holding the pack's objects;
/// the advertisement and the answer the very bytes the push held whole is answered with.
#[test]
fn a_session_fed_in_pieces_is_checked_as_each_push_ends_and_answers_as_held_whole() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let whole = [
      advertisement(&scenario),
      answer_to(&received(&scenario, &repository)),
    ]
    .concat();
    for piece in PIECES {
      assert_checked_as_the_last_piece_arrives(&scenario, piece, &whole);
    }
  }
}

/// Asserts that a session fed `bytes` in pieces, ended, and answered gives what receiving them held
/// whole gives: the same refusal and nothing written, or the same push received and its answer
/// written.
fn assert_as_held_whole(bytes: &[u8], repository: &Repo, label: &str) {
  let piece = 1 + bytes.len() % 7;
  let answered = fed_in_pieces(bytes, piece, repository);
  let mut policy = git_policy(repository);
  let whole = receive(
    bytes,
    &ADVERTISED,
    repository,
    &HIDDEN,
    &mut policy,
    &mut unbounded(),
  );
  let written = match &whole {
    Ok(received) => answer_to(received),
    Err(_) => Vec::new(),
  };
  assert_eq!(answered.result, whole.map(Some), "{label}");
  assert_eq!(answered.written, written, "{label}");
}

/// A-620 (the session against the push held whole, at every cut). Do: for every cut of each
/// captured push, feed a session the bytes before it in pieces, end its input, answer it, and
/// receive the same bytes held whole. Expect: the same refusal with nothing written, or the same
/// push received and the same answer written: a cut inside the request refused `Truncated`, one
/// inside the pack answered as git answers a short pack, the whole push checked before its input
/// ends.
#[test]
fn a_session_cut_anywhere_answers_as_the_push_held_whole() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    for cut in 0..=scenario.request.len() {
      let label = format!("{} cut at {cut}", scenario.name);
      assert_as_held_whole(&scenario.request[..cut], &repository, &label);
    }
  }
}

/// A-620 (a refused request ends the session). Do: feed a session a length that is not hex, then
/// a whole push, then end its input and ask for its answer. Expect: the first refused by kind at
/// its byte, the session done; the push after it not read, the end and the answer doing nothing;
/// nothing written.
#[test]
fn a_refused_request_ends_the_session_with_nothing_written() {
  let create = scenario("create");
  let repository = Repo::from_pack(create.before_pack);
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  let refused = session.input(b"00g0", &repository, &mut unbounded());
  let not_hex = Refusal::PktLine(pktline::Refusal::NotHex { at: 2 });
  assert_eq!(refused, Err(not_hex));
  assert!(session.is_done() && session.checked().is_none());
  let after = session.input(create.request, &repository, &mut unbounded());
  assert_eq!(after, Ok(false));
  assert_eq!(session.end(&repository, &mut unbounded()), Ok(false));
  let mut written = Vec::new();
  let mut policy = git_policy(&repository);
  let answered = session.answer(&mut policy, &mut written, &mut unbounded());
  assert_eq!(answered, Ok(None));
  assert!(written.is_empty() && policy.judged.is_empty());
}

/// Asserts that `scenario`'s push, fed to a session with `after` in the same piece and then more
/// bytes, is answered as the push alone is, and that nothing after it is read or written.
fn assert_bytes_after_are_not_read(scenario: &Scenario, after: &[u8]) {
  let repository = Repo::from_pack(scenario.before_pack);
  let alone = received(scenario, &repository);
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  let followed = [scenario.request, after].concat();
  let checked = session.input(&followed, &repository, &mut unbounded());
  assert_eq!(checked, Ok(true), "{}", scenario.name);
  let more = session.input(after, &repository, &mut unbounded());
  assert_eq!(more, Ok(false), "{}", scenario.name);
  assert_eq!(checked_in_turns(&mut session, &repository).1, Ok(true));
  let mut written = Vec::new();
  let mut policy = git_policy(&repository);
  let answered = session.answer(&mut policy, &mut written, &mut unbounded());
  assert_eq!(answered, Ok(Some(alone.clone())), "{}", scenario.name);
  assert_eq!(session.end(&repository, &mut unbounded()), Ok(false));
  assert_eq!(written, answer_to(&alone), "{}", scenario.name);
}

/// A-620 (bytes after a push are not the push's). Do: feed a session each captured push with a
/// flush, a pack's signature and other bytes after it in the same piece, then those bytes again,
/// then answer it and end its input. Expect: checked by the first piece; the answer the push alone
/// is answered with, nothing refused, and nothing more written.
#[test]
fn bytes_after_a_push_are_not_read() {
  for scenario in scenarios() {
    assert_bytes_after_are_not_read(&scenario, b"0000PACK and more");
  }
}

/// A-620 (a request of no command: `cmd_receive_pack` reads no pack and reports nothing). Do: feed
/// a session a lone flush, then answer it. Expect: checked at once, with no command and no pack;
/// received with no outcome; nothing written; the session done.
#[test]
fn a_request_of_no_command_is_received_with_nothing_written() {
  let create = scenario("create");
  let repository = Repo::from_pack(create.before_pack);
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  assert_eq!(
    session.input(b"0000", &repository, &mut unbounded()),
    Ok(true)
  );
  let mut written = Vec::new();
  let mut policy = git_policy(&repository);
  let answered = session.answer(&mut policy, &mut written, &mut unbounded());
  let received = answered.unwrap().unwrap();
  assert!(received.request.commands.is_empty() && received.unpacked.is_none());
  assert!(received.outcomes.is_empty());
  assert!(written.is_empty() && session.is_done());
}

/// A-620 (the push waits checked, holding nothing of the caller's). Do: feed a session the hidden
/// push whole against its server's objects, end its input (an HTTP body's end comes before its
/// answer), then drop that repository and answer judged from a second copy. Expect: the end keeping
/// the push checked, holding the pack's objects and the hidden ref's refusal; answered from the
/// copy as the push held whole is.
#[test]
fn a_checked_push_waits_for_its_verdicts_holding_nothing_of_the_callers() {
  let hidden = scenario("hidden");
  let first = Repo::from_pack(hidden.before_pack);
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  assert_eq!(
    session.input(hidden.request, &first, &mut unbounded()),
    Ok(true)
  );
  assert_eq!(session.end(&first, &mut unbounded()), Ok(false));
  assert_eq!(checked_in_turns(&mut session, &first).1, Ok(true));
  drop(first);
  let checked = session.checked().unwrap();
  assert!(!checked.objects().is_empty());
  let denied = checked.early.first().cloned().flatten();
  assert!(
    matches!(denied, Some(crate::receive::Outcome::Ng(_))),
    "{denied:?}"
  );
  let second = Repo::from_pack(hidden.before_pack);
  let mut written = Vec::new();
  let mut policy = git_policy(&second);
  let answered = session.answer(&mut policy, &mut written, &mut unbounded());
  assert_eq!(answered, Ok(Some(received(&hidden, &second))));
  assert_eq!(written, answer_to(&received(&hidden, &second)));
}

/// A connection's state as an edge's task keeps it between the bytes it is handed: the session
/// alone, owning its parts.
struct Kept {
  /// The session.
  session: Session<'static, super::HideRefs>,
}

/// A-654 (a session owns its advertisement and its hidden refs, so a caller keeps it across its own
/// steps). Do: make a session from an advertisement and a predicate that go out of scope at once,
/// keep it in a struct of the caller's, and feed it the create push in pieces of seven bytes
/// through that struct; then answer it. Expect: checked as its last piece arrives, and its answer and every
/// byte it wrote those of a session borrowing its parts.
#[test]
fn a_session_owning_its_parts_is_kept_by_its_caller_across_calls() {
  let scenario = scenario("create");
  let repository = Repo::from_pack(scenario.before_pack);
  let borrowed = fed_in_pieces(scenario.request, 7, &repository);
  let mut kept = {
    let advertised = ADVERTISED;
    let hidden = HIDDEN;
    Kept {
      session: Session::new(&advertised, hidden),
    }
  };
  let mut checked = Ok(false);
  for chunk in scenario.request.chunks(7) {
    checked = kept.session.input(chunk, &repository, &mut unbounded());
    if !matches!(checked, Ok(false)) {
      break;
    }
  }
  assert_eq!(checked, Ok(true));
  assert_eq!(checked_in_turns(&mut kept.session, &repository).1, Ok(true));
  let mut written = Vec::new();
  let mut policy = git_policy(&repository);
  let answered = kept
    .session
    .answer(&mut policy, &mut written, &mut unbounded());
  assert_eq!(answered, borrowed.result);
  assert_eq!(written, borrowed.written);
}

/// A-665 part 3 (git's `cmd_receive_pack`: its report, then `post-receive`, then its last flush).
/// Do: decide each scenario's push with `Session::decide`, then write its report held and its end.
/// Expect: the very bytes `Session::answer` writes for it.
#[test]
fn a_held_report_and_its_end_are_the_answer_written_whole() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let whole = fed_in_pieces(scenario.request, usize::MAX, &repository).written;
    let mut session = Session::new(&ADVERTISED, &HIDDEN);
    let checked = session.input(scenario.request, &repository, &mut unbounded());
    if checked == Ok(false) {
      let _ = session.end(&repository, &mut unbounded());
    }
    let _ = checked_in_turns(&mut session, &repository);
    let mut policy = git_policy(&repository);
    let mut written = Vec::new();
    if let Ok(Some(received)) = session.decide(&mut policy, &mut unbounded()) {
      write_report_held(&received, &mut written).unwrap();
      write_report_end(&received, &mut written).unwrap();
    }
    assert_eq!(written, whole, "{}", scenario.name);
  }
}

/// A-665 part 3 (git's `copy_to_sideband`). Do: write 300 bytes of a hook's words for a push that
/// asked for side-band-64k, and for one that did not. Expect: three band-2 packets of 128, 128 and
/// 44 bytes, the words in order; nothing for the push not multiplexed, whose words git sends to
/// its own standard error.
#[test]
fn a_hooks_words_go_to_band_two_in_gits_pieces_only_when_multiplexed() {
  let words: Vec<u8> = (0..300_u32)
    .map(|at| b'a' + u8::try_from(at % 26).unwrap())
    .collect();
  let multiplexed = Requested {
    side_band_64k: true,
    ..Requested::default()
  };
  let mut out = Vec::new();
  write_hook_words(&multiplexed, &words, &mut out);
  let mut rest = out.as_slice();
  let mut pieces = Vec::new();
  let mut relayed = Vec::new();
  while !rest.is_empty() {
    let len = usize::from_str_radix(std::str::from_utf8(&rest[..4]).unwrap(), 16).unwrap();
    assert_eq!(rest[4], 2, "band 2");
    pieces.push(len - 5);
    relayed.extend_from_slice(&rest[5..len]);
    rest = &rest[len..];
  }
  assert_eq!(pieces, [128, 128, 44]);
  assert_eq!(relayed, words);
  let mut none = Vec::new();
  write_hook_words(&Requested::default(), &words, &mut none);
  assert!(none.is_empty());
}

/// The objects `scenario`'s pack brings once read whole, none when it brings no pack or is refused.
fn objects_of(scenario: &Scenario, repository: &Repo) -> usize {
  match received(scenario, repository).unpacked {
    Some(Ok(unpacked)) => unpacked.objects.len(),
    _ => 0,
  }
}

/// A-719 (found by use: stock git's push of git/git, 607 MiB, ran its pack's checks in the step
/// that took the pack's last bytes, and its shard ran past the watchdog's window). Do: feed a
/// session each captured push whole, end its input if that did not read it whole, then make its
/// checks in turns that each end after one unit of work, and answer it. Expect: read whole, its
/// checks not yet made, when its pack ended; at least two turns an object it brought (each object is
/// read by fsck and by the walk's first pass; a complete push needs no propagation, and an incomplete
/// one's takes a turn a frame, A-722 part 4: the propagation ran whole in one turn, 57.9 ms at 100
/// concurrent v1.5.0 pushes); answered, and its bytes written, as the push held whole is.
#[test]
fn a_pushs_checks_are_made_a_turn_at_a_time_and_answer_as_made_whole() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let whole = received(&scenario, &repository);
    let mut session = Session::new(&ADVERTISED, &HIDDEN);
    let mut read = session.input(scenario.request, &repository, &mut unbounded());
    if read == Ok(false) {
      read = session.end(&repository, &mut unbounded());
    }
    let (packed, objects) = (session.is_checking(), objects_of(&scenario, &repository));
    let (turns, checked) = checked_in_turns(&mut session, &repository);
    assert_eq!((read, checked), (Ok(true), Ok(true)), "{}", scenario.name);
    assert!(
      !packed || turns >= 2 * objects,
      "{}: {turns} turns, {objects} objects",
      scenario.name
    );
    let mut written = Vec::new();
    let mut policy = git_policy(&repository);
    let answered = session.answer(&mut policy, &mut written, &mut unbounded());
    assert_eq!(answered, Ok(Some(whole.clone())), "{}", scenario.name);
    assert_eq!(written, answer_to(&whole), "{}", scenario.name);
  }
}

/// A push's objects as a store holds them while a landed pack's window is let go (goal 93 item 2b): each read back
/// whole, but for the object at `missing`, which answers none with its let-go named ([`crate::away`]) until its window
/// is brought back (`back`).
struct LetGoOnce {
  /// The push's objects, in pack order.
  objects: Vec<crate::unpack::Object>,
  /// The object read through the let-go window.
  missing: usize,
  /// Whether the window was brought back.
  back: core::cell::Cell<bool>,
  /// The let-go the last read to answer none met.
  away: core::cell::Cell<Option<u64>>,
}

/// Shape: the let-go number [`LetGoOnce`]'s window was given back under.
const LET_GO: u64 = 7;

impl crate::unpack::Contents for LetGoOnce {
  fn content(
    &self,
    at: usize,
    id: &crate::object_name::ObjectId,
  ) -> Option<std::borrow::Cow<'_, [u8]>> {
    self.away.set(None);
    if at == self.missing && !self.back.get() {
      self.away.set(Some(LET_GO));
      return None;
    }
    let object = self.objects.get(at).filter(|object| object.id == *id)?;
    Some(std::borrow::Cow::Borrowed(object.content.as_slice()))
  }

  fn content_away(&self) -> Option<u64> {
    self.away.get()
  }
}

/// A clock that never moves: a quantum over it is never spent.
struct Still;

impl crate::store::learn::Clock for Still {
  fn now_ns(&mut self) -> u64 {
    0
  }
}

/// Goal 93 item 2b, B1's stop (main's order of 2026-11-26: the cause of B1's failures under load, from main's lines of
/// agents/w-git-b1-debug's runs 12, 13 and 15). Do: read the captured `create` whole into a session; check it in one turn
/// whose quantum is never spent, its commit's bytes read through a let-go window; bring the window back and check it
/// again. Expect: the first turn not done and not refused, the walk stopped for the window; the second done, the push
/// answered as held whole. Red before: the check's own turn loop took the walk again after its stop, met the same
/// let-go with nothing brought back between, and the commit was `Missing` (`missing necessary objects`).
#[test]
fn a_checks_turn_ends_at_its_walks_stop_on_a_let_go_window_and_is_whole_once_it_is_back() {
  let scenario = scenario("create");
  let repository = Repo::from_pack(scenario.before_pack);
  let whole = received(&scenario, &repository);
  let mut session = Session::new(&ADVERTISED, &HIDDEN);
  let mut read = session.input(scenario.request, &repository, &mut unbounded());
  if read == Ok(false) {
    read = session.end(&repository, &mut unbounded());
  }
  assert_eq!(read, Ok(true));
  resolved(&mut session, &repository);
  let contents = let_go_once(&scenario, crate::object_name::Kind::Commit);
  assert_eq!(
    turned_still(&mut session, (&repository, &contents)),
    Ok(false),
    "stopped, not refused"
  );
  assert!(session.is_checking(), "its checks still to make");
  contents.back.set(true);
  assert_eq!(
    turned_still(&mut session, (&repository, &contents)),
    Ok(true)
  );
  let mut written = Vec::new();
  let answered = session.answer(&mut git_policy(&repository), &mut written, &mut unbounded());
  assert_eq!(answered, Ok(Some(whole)));
}

/// `scenario`'s pushed objects in pack order, the first of `kind` read through a let-go window ([`LetGoOnce`]).
fn let_go_once(scenario: &Scenario, kind: crate::object_name::Kind) -> LetGoOnce {
  let at = scenario
    .request
    .windows(4)
    .position(|magic| magic == b"PACK")
    .unwrap();
  let pack = &scenario.request[at..];
  let objects = crate::unpack::read_whole(
    pack,
    crate::object_name::ObjectFormat::Sha1,
    &crate::unpack::NoIndex,
    &mut unbounded(),
  )
  .unwrap()
  .objects;
  let missing = objects
    .iter()
    .position(|object| object.kind == kind)
    .unwrap();
  LetGoOnce {
    objects,
    missing,
    back: core::cell::Cell::new(false),
    away: core::cell::Cell::new(None),
  }
}

/// `session`'s pack's waiting deltas resolved against `repository` (a turn of [`Session::check_turn`] while it is
/// resolving), so its checks are what its next turn makes.
fn resolved(session: &mut Session<'_, impl crate::receive::HiddenRefs>, repository: &Repo) {
  while session.is_resolving() {
    let mut clock = Still;
    let quantum = crate::store::hook_view::Quantum {
      clock: &mut clock,
      ns: u64::MAX,
    };
    assert_eq!(
      session.check_turn(repository, &mut unbounded(), quantum),
      Ok(false)
    );
  }
}

/// One turn of `session`'s checks against `repository`, its objects read from `contents`, over a quantum never spent.
fn turned_still(
  session: &mut Session<'_, impl crate::receive::HiddenRefs>,
  (repository, contents): (&Repo, &LetGoOnce),
) -> Result<bool, Refusal> {
  let mut clock = Still;
  let quantum = crate::store::hook_view::Quantum {
    clock: &mut clock,
    ns: u64::MAX,
  };
  session.check_turn_with((repository, contents), &mut unbounded(), quantum)
}
