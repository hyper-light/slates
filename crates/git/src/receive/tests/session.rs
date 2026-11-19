//! One push over one connection (A-620's session, sans-io): its bytes in any pieces, against the
//! push held whole.

use super::{
  ADVERTISED, HIDDEN, Repo, Scenario, advertisement, answer_to, git_policy, named, received, refs,
  scenario, scenarios, unbounded,
};
use crate::pktline;
use crate::receive::{Received, Refusal, Session, receive};

/// The sizes of the pieces a session is fed a whole push in: a byte, a few, more than a packet's
/// header, and all at once.
const PIECES: [usize; 5] = [1, 3, 7, 61, usize::MAX];

/// What a session gave: its first answer or refusal, and every byte it wrote.
struct Answered {
  /// The first call's result that was not `Ok(None)`, or the end's.
  result: Result<Option<Received>, Refusal>,
  /// Every byte written.
  written: Vec<u8>,
}

/// Feeds `bytes` to a session of a push to `repository` in pieces of `piece` bytes until it answers
/// or refuses, then ends its input if it has not.
fn fed_in_pieces(bytes: &[u8], piece: usize, repository: &Repo) -> Answered {
  let mut policy = git_policy(repository);
  let mut session = Session::new(&ADVERTISED, repository, &HIDDEN, &mut policy);
  let mut written = Vec::new();
  for chunk in bytes.chunks(piece) {
    let result = session.input(chunk, &mut written, &mut unbounded());
    if !matches!(result, Ok(None)) {
      return Answered { result, written };
    }
  }
  let result = session.end(&mut written, &mut unbounded());
  Answered { result, written }
}

/// Asserts that a session of `scenario`'s push, its refs advertised first and its push fed in
/// pieces of `piece` bytes, answers as the last piece arrives with `whole`, the bytes of the
/// advertisement and the answer to the push held whole.
fn assert_answers_as_the_last_piece_arrives(scenario: &Scenario, piece: usize, whole: &[u8]) {
  let label = format!("{} in pieces of {piece}", scenario.name);
  let repository = Repo::from_pack(scenario.before_pack);
  let mut policy = git_policy(&repository);
  let mut session = Session::new(&ADVERTISED, &repository, &HIDDEN, &mut policy);
  let refs = refs(scenario.before_refs);
  let mut written = Vec::new();
  session.advertise(&named(&refs), &mut written).unwrap();
  let pieces: Vec<&[u8]> = scenario.request.chunks(piece).collect();
  let (last, before) = pieces.split_last().unwrap();
  for piece in before {
    let answered = session.input(piece, &mut written, &mut unbounded());
    assert_eq!(answered, Ok(None), "{label}");
  }
  let answered = session.input(last, &mut written, &mut unbounded());
  assert!(matches!(answered, Ok(Some(_))) && session.is_done(), "{label}");
  assert_eq!(written, whole, "{label}");
}

/// A-620 (the session: one push over one connection, sans-io). Do: open a session on each captured
/// push's server, advertise its refs, and feed it the push in pieces of 1, 3, 7 and 61 bytes and
/// whole. Expect: no answer before the last piece; the answer as it arrives, with no end of input
/// asked, the delete's as its request ends (no pack follows it); the advertisement and the answer
/// the very bytes the push held whole is answered with.
#[test]
fn a_session_fed_in_pieces_answers_each_push_as_its_last_byte_arrives() {
  for scenario in scenarios() {
    let repository = Repo::from_pack(scenario.before_pack);
    let whole = [
      advertisement(&scenario),
      answer_to(&received(&scenario, &repository)),
    ]
    .concat();
    for piece in PIECES {
      assert_answers_as_the_last_piece_arrives(&scenario, piece, &whole);
    }
  }
}

/// Asserts that a session fed `bytes` in pieces and then ended gives what receiving them held whole
/// gives: the same refusal and nothing written, or the same push received and its answer written.
fn assert_as_held_whole(bytes: &[u8], repository: &Repo, label: &str) {
  let piece = 1 + bytes.len() % 7;
  let answered = fed_in_pieces(bytes, piece, repository);
  let mut policy = git_policy(repository);
  let whole = receive(bytes, &ADVERTISED, repository, &HIDDEN, &mut policy, &mut unbounded());
  let written = match &whole {
    Ok(received) => answer_to(received),
    Err(_) => Vec::new(),
  };
  assert_eq!(answered.result, whole.map(Some), "{label}");
  assert_eq!(answered.written, written, "{label}");
}

/// A-620 (the session against the push held whole, at every cut). Do: for every cut of each
/// captured push, feed a session the bytes before it in pieces, end its input, and receive the
/// same bytes held whole. Expect: the same refusal with nothing written, or the same push received
/// and the same answer written: a cut inside the request refused `Truncated`, one inside the pack
/// answered as git answers a short pack, the whole push answered before its input ends.
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
/// a whole push, then end its input. Expect: the first refused by kind at its byte, the session
/// done; the push after it not read and the end doing nothing; nothing written.
#[test]
fn a_refused_request_ends_the_session_with_nothing_written() {
  let create = scenario("create");
  let repository = Repo::from_pack(create.before_pack);
  let mut policy = git_policy(&repository);
  let mut session = Session::new(&ADVERTISED, &repository, &HIDDEN, &mut policy);
  let mut written = Vec::new();
  let refused = session.input(b"00g0", &mut written, &mut unbounded());
  let not_hex = Refusal::PktLine(pktline::Refusal::NotHex { at: 2 });
  assert_eq!(refused, Err(not_hex));
  assert!(session.is_done());
  let after = session.input(create.request, &mut written, &mut unbounded());
  assert_eq!(after, Ok(None));
  assert_eq!(session.end(&mut written, &mut unbounded()), Ok(None));
  assert!(written.is_empty());
}

/// Asserts that `scenario`'s push, fed to a session with `after` in the same piece and then more
/// bytes, is answered as the push alone is, and that nothing after it is read or written.
fn assert_bytes_after_are_not_read(scenario: &Scenario, after: &[u8]) {
  let repository = Repo::from_pack(scenario.before_pack);
  let alone = received(scenario, &repository);
  let mut policy = git_policy(&repository);
  let mut session = Session::new(&ADVERTISED, &repository, &HIDDEN, &mut policy);
  let mut written = Vec::new();
  let followed = [scenario.request, after].concat();
  let answered = session.input(&followed, &mut written, &mut unbounded());
  assert_eq!(answered, Ok(Some(alone.clone())), "{}", scenario.name);
  assert_eq!(written, answer_to(&alone), "{}", scenario.name);
  let more = session.input(after, &mut written, &mut unbounded());
  assert_eq!(more, Ok(None), "{}", scenario.name);
  assert_eq!(session.end(&mut written, &mut unbounded()), Ok(None));
  assert_eq!(written, answer_to(&alone), "{}", scenario.name);
}

/// A-620 (bytes after a push are not the push's). Do: feed a session each captured push with a
/// flush, a pack's signature and other bytes after it in the same piece, then those bytes again,
/// then end its input. Expect: the answer the push alone is answered with, nothing refused, and
/// nothing more written.
#[test]
fn bytes_after_a_push_are_not_read() {
  for scenario in scenarios() {
    assert_bytes_after_are_not_read(&scenario, b"0000PACK and more");
  }
}

/// A-620 (a request of no command: `cmd_receive_pack` reads no pack and reports nothing). Do: feed
/// a session a lone flush. Expect: the push received at once, with no command and no pack;
/// nothing written; the session done.
#[test]
fn a_request_of_no_command_is_received_with_nothing_written() {
  let create = scenario("create");
  let repository = Repo::from_pack(create.before_pack);
  let mut policy = git_policy(&repository);
  let mut session = Session::new(&ADVERTISED, &repository, &HIDDEN, &mut policy);
  let mut written = Vec::new();
  let answered = session.input(b"0000", &mut written, &mut unbounded());
  let received = answered.unwrap().unwrap();
  assert!(received.request.commands.is_empty() && received.unpacked.is_none());
  assert!(received.outcomes.is_empty());
  assert!(written.is_empty() && session.is_done());
}
