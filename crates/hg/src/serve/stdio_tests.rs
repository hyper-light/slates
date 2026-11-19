//! Tests of a session over a byte channel (A-650 part 2). The golden: the three sessions stock `hg serve --stdio` 7.2
//! answered (`fixtures/serve`), each client's bytes handed in at several cuts, a byte at a time among them, and the
//! answers the channel took held equal to the session's own (which the session's golden holds to 7.2's). Then what the
//! loop adds: one answer held at a time, the window's bound, the end of input, and arbitrary bytes at arbitrary cuts.

use super::super::getbundle::tests::{Served, cloned, serve_whole, session_stores};
use super::super::session::{Refused, Step};
use super::{PastWindow, Progress, Stdio};
use proptest::prelude::*;

/// Format: a request's room, as the session's tests admit one.
const ROOM: u64 = 1 << 20;

/// What a channel took from a session: standard error and standard output in the order sent, the input the session
/// consumed, and each step.
#[derive(Debug, Default, PartialEq, Eq)]
struct Taken {
  stderr: Vec<u8>,
  stdout: Vec<u8>,
  consumed: usize,
  steps: Vec<Step>,
}

/// Steps `stdio` until it waits, sending each answer whole as the channel would, standard error first.
fn drive(stdio: &mut Stdio, served: &Served, taken: &mut Taken) {
  loop {
    taken.stderr.extend_from_slice(stdio.stderr());
    stdio.sent_stderr(stdio.stderr().len());
    taken.stdout.extend_from_slice(stdio.stdout());
    stdio.sent_stdout(stdio.stdout().len());
    match stdio.step(served, ROOM) {
      Progress::Waiting => return,
      Progress::Stepped { step, consumed } => {
        taken.consumed += consumed;
        taken.steps.push(step);
      }
    }
  }
}

/// A session over a window of `capacity` handed `input` in pieces of `cut` bytes, driven after each, then the end of
/// input; what the channel took, and whether the session finished.
fn session_at_cut(served: &Served, input: &[u8], cut: usize, capacity: usize) -> (Taken, bool) {
  let mut stdio = Stdio::new(capacity);
  let mut taken = Taken::default();
  for piece in input.chunks(cut) {
    stdio.receive(piece).unwrap();
    drive(&mut stdio, served, &mut taken);
  }
  stdio.eof();
  drive(&mut stdio, served, &mut taken);
  (taken, stdio.finished())
}

/// What a session driven over `client`'s bytes and the empty line should give the channel: each answer the session
/// gives the same bytes whole, in order, every request answered and the empty line ending it, all consumed.
fn expected(served: &Served, client: &[u8]) -> (Taken, bool) {
  let answers = serve_whole(served, client);
  let mut steps = vec![Step::Answered { refused: None }; answers.len()];
  steps.push(Step::Ended { refused: None });
  let taken = Taken {
    stderr: Vec::new(),
    stdout: answers.concat(),
    consumed: client.len() + 1,
    steps,
  };
  (taken, true)
}

/// AC-HG.1 (golden), A-650 part 2: do hand each captured session's client bytes, then the empty line that ends a
/// session, to a session over a channel, a byte, seven bytes, a kilobyte and the whole at a time, sending every answer
/// as it is made. Expect, at every cut, the bytes on standard output equal to what the session answers the same bytes
/// whole (each of its answers 7.2's, by the session's golden), nothing on standard error, every request answered and
/// the empty line ending the session, all the input consumed, and the session finished.
#[test]
fn every_captured_session_is_served_alike_at_every_cut() {
  for (served, client) in session_stores() {
    let input = [client, b"\n"].concat();
    let wanted = expected(&served, client);
    for cut in [1, 7, 1024, input.len()] {
      assert_eq!(
        session_at_cut(&served, &input, cut, input.len()),
        wanted,
        "cut {cut}"
      );
    }
  }
}

/// A-650 part 2: do send two requests at once and step twice without taking the first answer, then take it and step
/// again. Expect the first answered, the second left waiting while the first answer waits (so the session never holds
/// two answers), and answered once the first was taken, each step consuming its own request's bytes.
#[test]
fn a_session_holds_one_answer_until_the_channel_takes_it() {
  let served = cloned();
  let mut stdio = Stdio::new(64);
  stdio.receive(b"heads\nheads\n").unwrap();
  let first = stdio.step(&served, ROOM);
  let answer = stdio.stdout().to_vec();
  let held = (stdio.step(&served, ROOM), stdio.stdout().to_vec());
  stdio.sent_stdout(answer.len());
  let second = stdio.step(&served, ROOM);
  let answered = Progress::Stepped {
    step: Step::Answered { refused: None },
    consumed: 6,
  };
  assert_eq!(
    (first, held, second, stdio.stdout()),
    (
      answered,
      (Progress::Waiting, answer.clone()),
      answered,
      answer.as_slice()
    )
  );
}

/// AC-HG.9 (hostile input), A-650 part 2: do hand a session over a 24-byte window 25 bytes, then a request whose
/// argument says 81 bytes follow (98 in all, past the window). Expect the 25 bytes refused with nothing taken, and the
/// request answered out of band (the empty line on standard output, the words and `\n-\n` on standard error) naming
/// what it needs, the session ended with its input consumed rather than waiting for bytes the window forbids.
#[test]
fn a_request_past_the_window_is_refused_and_ends_the_session() {
  let served = cloned();
  let mut stdio = Stdio::new(24);
  let past = stdio.receive(&[b'x'; 25]);
  let request = b"between\npairs 81\n";
  stdio.receive(request).unwrap();
  let step = stdio.step(&served, ROOM);
  let refused = Refused::PastWindow {
    needed: 98,
    capacity: 24,
  };
  let ended = Progress::Stepped {
    step: Step::Ended {
      refused: Some(refused),
    },
    consumed: request.len(),
  };
  let wanted_past = Err(PastWindow {
    held: 25,
    capacity: 24,
  });
  assert_eq!(
    (past, step, stdio.stdout(), stdio.ended(), stdio.finished()),
    (wanted_past, ended, &b"\n"[..], true, false)
  );
  assert!(stdio.stderr().ends_with(b"\n-\n") && stdio.stderr().len() > 3);
}

/// A-650 part 2: do end the input after a whole request and part of another, then hand the ended session more bytes.
/// Expect the whole request answered, the part dropped unanswered as the session ends with all its input consumed, the
/// later bytes dropped, and the session finished once its answer is taken.
#[test]
fn the_end_of_input_ends_the_session_after_its_whole_requests() {
  let served = cloned();
  let mut stdio = Stdio::new(64);
  stdio.receive(b"heads\nhea").unwrap();
  stdio.eof();
  let mut taken = Taken::default();
  drive(&mut stdio, &served, &mut taken);
  let finished = stdio.finished();
  let later = (stdio.receive(b"heads\n"), stdio.step(&served, ROOM));
  let steps = vec![
    Step::Answered { refused: None },
    Step::Ended { refused: None },
  ];
  assert_eq!(
    (taken.steps, taken.consumed, finished, later, stdio.stdout()),
    (steps, 9, true, (Ok(()), Progress::Waiting), &b""[..])
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// AC-HG.9, A-650 part 2: do hand a session arbitrary bytes at arbitrary cuts over a generated window, then end its
  /// input. Expect no panic; the input held never past the window (a refused piece taking nothing); every byte the
  /// session consumed one it was handed; and the session finished once its input ended.
  #[test]
  fn arbitrary_bytes_at_arbitrary_cuts_never_break_the_loop(
    bytes in proptest::collection::vec(any::<u8>(), 0..256),
    cut in 1_usize..32,
    capacity in 1_usize..128,
  ) {
    let served = cloned();
    let mut stdio = Stdio::new(capacity);
    let mut taken = Taken::default();
    let mut handed = 0_usize;
    for piece in bytes.chunks(cut) {
      if stdio.receive(piece).is_ok() && !stdio.ended() {
        handed += piece.len();
      }
      drive(&mut stdio, &served, &mut taken);
      prop_assert!(taken.consumed <= handed);
      prop_assert!(handed - taken.consumed <= capacity);
    }
    stdio.eof();
    drive(&mut stdio, &served, &mut taken);
    prop_assert!(stdio.finished());
  }
}
