//! Tests of a session over a byte channel (A-650 part 2). The golden: the three sessions stock `hg serve --stdio` 7.2
//! answered (`fixtures/serve`), each client's bytes handed in at several cuts, a byte at a time among them, and the
//! answers the channel took held equal to the session's own (which the session's golden holds to 7.2's). Then what the
//! loop adds: one answer held at a time, the window's bound, the end of input, and arbitrary bytes at arbitrary cuts.
//! Then pushes (A-696): the bundle asked for and taken at every cut, each outcome framed as 7.2 frames it over SSH, a
//! race answered before the bundle, a bundle past its room, a push cut short, and nothing served while a push lands.

use super::super::Refusal;
use super::super::getbundle::tests::{Served, cloned, serve_whole, session_stores};
use super::super::session::{Refused, Step};
use super::{PastWindow, Progress, Push, Stdio};
use crate::Admission;
use crate::push::Outcome;
use crate::push::check::HeadsArgument;
use crate::push::reply::{self, Reply};
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
  /// Each push the session handed its caller, and the chunk steps that took its bundle (`Took`, `whole` last).
  pushes: Vec<Push>,
  took: Vec<bool>,
}

/// The outcome the tests' caller lands every push with: accepted, no reply part asked.
fn landed() -> Outcome {
  Outcome::Accepted(Reply::accepted(&[], false))
}

/// Steps `stdio` until it waits, sending each answer whole as the channel would, standard error first, and landing
/// each push it hands over as [`landed`].
fn drive(stdio: &mut Stdio, served: &Served, taken: &mut Taken) {
  loop {
    taken.stderr.extend_from_slice(stdio.stderr());
    stdio.sent_stderr(stdio.stderr().len());
    taken.stdout.extend_from_slice(stdio.stdout());
    stdio.sent_stdout(stdio.stdout().len());
    let progress = stdio.step(served, ROOM);
    if let Some(push) = stdio.take_push() {
      let mut left = push.admission;
      taken.pushes.push(push);
      stdio.pushed(&landed(), &mut left);
    }
    match progress {
      Progress::Waiting => return,
      Progress::Stepped { step, consumed } => {
        taken.consumed += consumed;
        taken.steps.push(step);
      }
      Progress::Took { consumed, whole } => {
        taken.consumed += consumed;
        taken.took.push(whole);
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
    ..Taken::default()
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

/// Format: a bundle2 push's request as 7.2's client writes it: `unbundle`, its `heads` argument `force` in hexadecimal.
const UNBUNDLE_FORCE: &[u8] = b"unbundle\nheads 10\n666f726365";
/// Format: a bundle as a push's chunks carry it: `<length>\n<data>` twice, then the empty chunk that ends it.
const CHUNKED: &[u8] = b"5\nHG20\x003\n\0\0\x000\n";
/// Format: the bundle [`CHUNKED`] carries, its chunks joined.
const BUNDLE: &[u8] = b"HG20\0\0\0\0";
/// Format: the empty answer that asks a client for its bundle (`sshv1protocolhandler.getpayload`).
const SEND_IT: &[u8] = b"0\n";
/// Format: an empty bundle2 stream: the reply to a push that asked for none ([`landed`]).
const EMPTY_REPLY: &[u8] = b"HG20\0\0\0\0\0\0\0\0";

/// A push as its client sends it: `unbundle` with `force`, then its bundle in chunks.
fn push_input() -> Vec<u8> {
  [UNBUNDLE_FORCE, CHUNKED].concat()
}

/// A-696 (the module doc's push). Do: hand a session `unbundle` with `force` and a bundle in two chunks, then the end of
/// input, a byte, seven bytes, a kilobyte and the whole at a time, landing the push it hands over. Expect at every cut:
/// the empty answer asking for the bundle, then the reply as it is; one push handed over, its `heads` `force`, its
/// bundle the chunks joined, its room the request's less the argument and the bundle; three chunk steps, the last whole;
/// every byte consumed; the session finished.
#[test]
fn a_push_is_asked_for_and_taken_whole_at_every_cut() {
  let served = cloned();
  let input = push_input();
  let mut room = Admission::new(ROOM);
  room.charge_items::<&[u8]>(1).unwrap();
  room.charge_items::<Vec<u8>>(1).unwrap();
  room.charge(5).unwrap();
  room.charge(8).unwrap();
  let wanted = Taken {
    stdout: [SEND_IT, EMPTY_REPLY].concat(),
    consumed: input.len(),
    steps: vec![
      Step::Answered { refused: None },
      Step::Ended { refused: None },
    ],
    pushes: vec![Push {
      heads: HeadsArgument::Force,
      bundle: BUNDLE.to_vec(),
      admission: room,
    }],
    took: vec![false, false, true],
    ..Taken::default()
  };
  for cut in [1, 7, 1024, input.len()] {
    let mut stdio = Stdio::new(input.len());
    let mut taken = Taken::default();
    for piece in input.chunks(cut) {
      stdio.receive(piece).unwrap();
      drive(&mut stdio, &served, &mut taken);
    }
    stdio.eof();
    drive(&mut stdio, &served, &mut taken);
    assert_eq!((&taken, stdio.finished()), (&wanted, true), "cut {cut}");
  }
}

/// Takes `stdio`'s push whole from [`push_input`] and makes its answer from `outcome`: what the channel then takes,
/// standard error and standard output, past the empty answer asking for the bundle.
fn framed(outcome: &Outcome) -> (Vec<u8>, Vec<u8>) {
  let served = cloned();
  let mut stdio = Stdio::new(64);
  stdio.receive(&push_input()).unwrap();
  while stdio.take_push().is_none() {
    stdio.sent_stdout(stdio.stdout().len());
    assert_ne!(stdio.step(&served, ROOM), Progress::Waiting);
  }
  stdio.sent_stdout(stdio.stdout().len());
  stdio.pushed(outcome, &mut Admission::new(ROOM));
  (stdio.stderr().to_vec(), stdio.stdout().to_vec())
}

/// A-696 (7.2's `_runsshserver` answers to `unbundle`: `streamreslegacy`, `pusherr`, and `pushres` after an abort).
/// Do: land a push accepted, one refused with an error part, one refused before its bundle2 was processed, and a
/// bundle1. Expect: each reply as it is on standard output; the refusal before processing as a string of its words;
/// the bundle1 as 7.2 aborts it where bundle2 is required, `abort:` and its `bundle2required` words on standard error,
/// then the empty string and the result `0`.
#[test]
fn each_outcome_is_framed_as_7_2_frames_it_over_ssh() {
  let raced = Outcome::refused(&reply::Refused::PushRaced {
    message: b"remote repository changed while pushing - please try again".to_vec(),
  });
  let Outcome::Refused(raced_reply) = &raced else {
    panic!("a refusal holds its reply");
  };
  let mut raced_bytes = Vec::new();
  raced_reply.encode(&mut raced_bytes).unwrap();
  let bundle2_required = b"abort: incompatible Mercurial client; bundle2 required\n\
    (see https://www.mercurial-scm.org/wiki/IncompatibleClient)\n";
  assert_eq!(
    [
      framed(&landed()),
      framed(&raced),
      framed(&Outcome::Unprocessed(
        b"stream: not a Mercurial bundle".to_vec()
      )),
      framed(&Outcome::Bundle1),
    ],
    [
      (Vec::new(), EMPTY_REPLY.to_vec()),
      (Vec::new(), raced_bytes),
      (Vec::new(), b"30\nstream: not a Mercurial bundle".to_vec()),
      (bundle2_required.to_vec(), b"0\n1\n0".to_vec()),
    ]
  );
}

/// A-696 (7.2's `check_heads` while "preparing changes", before `getpayload`). Do: send `unbundle` whose `heads` names a
/// node that is no head of the store, then `heads`. Expect: the race answered as a string of 7.2's words, no bundle
/// asked for and no push handed over; the session going on, `heads` answered after it.
#[test]
fn a_race_before_the_bundle_is_answered_as_7_2s_pusherr() {
  let served = cloned();
  let mut stdio = Stdio::new(128);
  let request = [&b"unbundle\nheads 40\n"[..], &[b'0'; 40], b"heads\n"].concat();
  stdio.receive(&request).unwrap();
  let mut taken = Taken::default();
  drive(&mut stdio, &served, &mut taken);
  let words = b"repository changed while preparing changes - please try again";
  let raced = [format!("{}\n", words.len()).as_bytes(), words].concat();
  assert!(taken.stdout.starts_with(&raced), "{taken:?}");
  assert_eq!(
    (taken.steps, taken.pushes, taken.took, taken.consumed),
    (
      vec![
        Step::Answered {
          refused: Some(Refused::Push)
        },
        Step::Answered { refused: None },
      ],
      Vec::new(),
      Vec::new(),
      request.len()
    )
  );
}

/// A-696 (the module doc's bounds on a push). Do: over a room of 1,000 bytes, send `unbundle` and a chunk's length line
/// naming 1,000 bytes. Expect: the bundle asked for; the chunk refused out of band as past what the push's room has
/// left once its argument is charged, before its bytes arrive, which ends the session, no push handed over.
#[test]
fn a_bundle_past_its_room_ends_the_session_out_of_band() {
  let served = cloned();
  let mut stdio = Stdio::new(128);
  stdio
    .receive(&[UNBUNDLE_FORCE, b"1000\n"].concat())
    .unwrap();
  let asked = stdio.step(&served, 1000);
  let send_it = stdio.stdout().to_vec();
  stdio.sent_stdout(send_it.len());
  let refused = stdio.step_push();
  let ended = matches!(
    refused,
    Progress::Stepped {
      step: Step::Ended {
        refused: Some(Refused::Request(_) | Refused::Serve(Refusal::PastAdmission(_)))
      },
      ..
    }
  );
  let taken = stdio.take_push();
  assert_eq!(
    (asked, send_it, ended, stdio.stdout(), taken),
    (
      Progress::Stepped {
        step: Step::Answered { refused: None },
        consumed: UNBUNDLE_FORCE.len()
      },
      SEND_IT.to_vec(),
      true,
      &b"\n"[..],
      None
    )
  );
  assert!(stdio.ended() && stdio.stderr().ends_with(b"\n-\n"));
}

/// A-696 (a client gone mid-push). Do: send `unbundle` and the bundle's first chunk, then end the input. Expect: the
/// session ended with no push handed over and nothing answered past the empty answer asking for the bundle.
#[test]
fn a_push_whose_input_ends_before_its_last_chunk_ends_the_session() {
  let served = cloned();
  let mut stdio = Stdio::new(64);
  stdio
    .receive(&[UNBUNDLE_FORCE, b"5\nHG20\0"].concat())
    .unwrap();
  stdio.eof();
  let mut taken = Taken::default();
  drive(&mut stdio, &served, &mut taken);
  assert_eq!(
    (taken.stdout, taken.pushes, taken.took, stdio.finished()),
    (SEND_IT.to_vec(), Vec::new(), vec![false], true)
  );
}

/// A-696 (one answer at a time: no request after a push is served before its answer). Do: send a push and `heads` at
/// once and step until the push is whole, then step again before landing it; then land it and step. Expect: the steps
/// before the landing wait with `heads` unanswered; after it, the reply, then `heads`' answer.
#[test]
fn no_request_after_a_push_is_served_before_its_answer() {
  let served = cloned();
  let mut stdio = Stdio::new(64);
  stdio
    .receive(&[&push_input()[..], b"heads\n"].concat())
    .unwrap();
  let mut whole = false;
  for _ in 0..=push_input().len() {
    stdio.sent_stdout(stdio.stdout().len());
    if matches!(
      stdio.step(&served, ROOM),
      Progress::Took { whole: true, .. }
    ) {
      whole = true;
      break;
    }
  }
  assert!(whole, "the push was taken whole");
  let waiting = (
    stdio.step(&served, ROOM),
    stdio.step_push(),
    stdio.stdout().len(),
  );
  let push = stdio.take_push();
  let still = stdio.step(&served, ROOM);
  stdio.pushed(&landed(), &mut Admission::new(ROOM));
  let reply = stdio.stdout().to_vec();
  stdio.sent_stdout(reply.len());
  let heads = stdio.step(&served, ROOM);
  assert_eq!(
    (waiting, push.is_some(), still, reply, heads),
    (
      (Progress::Waiting, Progress::Waiting, 0),
      true,
      Progress::Waiting,
      EMPTY_REPLY.to_vec(),
      Progress::Stepped {
        step: Step::Answered { refused: None },
        consumed: 6
      }
    )
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
