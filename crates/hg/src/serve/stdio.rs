//! One Mercurial SSH session over a byte channel (A-650 part 2; §4.17.3 "The wire surface", §4.17.2 "Transports" item
//! 2): the standard input and output of `hg -R <repo> serve --stdio`, served a request at a time by
//! [`session::serve`], for the daemon's SSH session channel (`slates-ssh`'s `Session`, A-633, whose listener is A-651)
//! or any other carrier of those bytes.
//!
//! **The loop** is 7.2's `_runsshserver` \[C\] without its blocking reads. The channel's bytes are handed in as they
//! arrive ([`Stdio::receive`]). [`Stdio::step`] serves the request at the front once it is whole and every byte of the
//! last answer has been taken, so the session holds one answer at most, as a process writing to a full pipe stops
//! reading. The caller sends what [`Stdio::stderr`] holds before what [`Stdio::stdout`] holds (7.2 writes an error's
//! words, then the empty line on standard output), says how much of each the channel took ([`Stdio::sent_stderr`],
//! [`Stdio::sent_stdout`]), and gives the channel's window back the input each step consumed. The session ends at an
//! empty line, at a request it cannot read (answered out of band first), or at the end of the client's input
//! ([`Stdio::eof`]) once the whole requests before it are served; once [`Stdio::finished`], the caller ends the
//! channel with exit status [`EXIT_STATUS`], as `hg serve --stdio` exits when its loop ends.
//!
//! **Bounds.** Each request is admitted `room` bytes, which the caller derives from the shard's budget as the HTTP
//! edge derives a request's room: its arguments and its answer are charged to it. The input held is at most the
//! channel's window, `capacity`: the bytes the client may send before the server consumes any, which
//! [`Stdio::receive`] refuses to exceed. A request that needs more input than the window holds could never be read
//! whole, so it is answered out of band and ends the session, where waiting for it would hold the channel forever.
//!
//! **A push** (A-696; 7.2's `wireprotov1server.unbundle`, `sshv1protocolhandler.getpayload` and `_runsshserver`'s
//! answers \[C\]). `unbundle`'s `heads` argument is read and judged against the store before anything else, as 7.2's
//! `check_heads` judges it while "preparing changes": a race is answered as 7.2's `pusherr`, its words as a string,
//! and no bundle is asked for. Otherwise the session answers the empty string that lets the client send its bundle, and
//! takes it as chunks (`<length>\n<data>`, ended by `0\n`) as they arrive, each charged to the push's admission (the
//! request's room, its argument charged first), its consumed bytes given back to the window as any request's are. Once
//! the last chunk is taken the push waits for its caller ([`Stdio::take_push`]), which lands it (the daemon, on the
//! repository's owner shard, through [`crate::push`]'s steps) and hands back what it came to ([`Stdio::pushed`]), framed as 7.2 frames each answer of `unbundle` over SSH: a bundle2 reply, accepted or holding
//! its error part, as it is (`streamreslegacy`); a refusal before the bundle2 was processed as `pusherr`; a bundle1
//! as 7.2 refuses one where bundle2 is required, `abort:` and its `bundle2required` words on standard error and the
//! push's result `0` (`pushres`). No request after the push is served before its answer is made.
//!
//! **Differs from 7.2, on purpose.** A request cut short by the end of input is dropped, not served: 7.2 reads a last
//! line that has no newline and serves it less its last byte. A push's bundle is held within the request's room, where
//! 7.2 spools it to a temporary file of any size: a chunk past it is answered out of band and ends the session, as a
//! request past its admission does. A push whose input ends before its last chunk is dropped and ends the session,
//! where 7.2's `int()` of the empty line fails. 7.2 prints its progress words while it takes a push (`adding
//! changesets`, `added 1 changesets with 1 changes to 1 files`) on standard error, which its client prints after
//! `remote:`; they are no part of the push's answer, and none is printed here, as none is written into a reply over
//! HTTP (A-680 part 2b). A bundle no version of which 7.2 reads is answered as its race is (`pusherr`, its words),
//! where 7.2 aborts on standard error with the push's result `0`; a stock client never sends one.

use super::Refusal;
use super::getbundle::{Answering, Store};
use super::session::{self, BUNDLE2_REQUIRED, Refused, Step, spec_of};
use crate::Admission;
use crate::push::Outcome;
use crate::push::check::{self, CheckRefusal, HeadsArgument, HeadsArgumentRefusal, Moment};
use crate::read::to_u64;
use crate::ssh::{self, Read};

/// Format: the exit status a session ends with: 7.2's `hg serve --stdio` exits 0 when its loop ends, whatever it
/// answered.
pub const EXIT_STATUS: u32 = 0;

/// Format: the command a push's bundle is sent with, and the argument its spec names (`unbundle`, `heads`).
const UNBUNDLE: &[u8] = b"unbundle";
/// Format: see [`UNBUNDLE`].
const HEADS: &[u8] = b"heads";
/// Format: the command line a push begins with: its name, then the newline that ends a command line.
const UNBUNDLE_LINE: &[u8] = b"unbundle\n";

/// Format: what 7.2's `Abort.format` writes before an abort's words on standard error.
const ABORT: &[u8] = b"abort: ";
/// Format: the result 7.2 answers a push it aborted before its bundle2 was processed with (`pushres(0, ...)`).
const ABORTED: i64 = 0;

/// One session's input, its answers not yet sent, and whether it has ended.
#[derive(Debug)]
pub struct Stdio {
  /// The most input the session may hold: the channel's window.
  capacity: usize,
  /// The client's bytes not yet served.
  input: Vec<u8>,
  /// Answer bytes for standard output, not yet taken by the channel.
  stdout: Vec<u8>,
  /// Error bytes for standard error, not yet taken by the channel.
  stderr: Vec<u8>,
  /// Whether the client's input has ended.
  input_ended: bool,
  /// Whether the session has ended.
  ended: bool,
  /// The push the session is taking, or that waits for its caller (the module doc's push).
  push: Pushing,
  /// The room a push's bundle is admitted, where its caller names one past a request's (A-711: what the repository's
  /// shard can hold).
  push_room: Option<u64>,
  /// A `getbundle`'s answer being made a slice at a time (A-745): the session serves no other request until it ends.
  answering: Option<Box<Answering>>,
}

/// A push this session took whole (the module doc's push): its `heads` argument, its bundle's bytes, and what is left of
/// the request's room once both are held, within which its caller lands it.
#[derive(Debug, PartialEq, Eq)]
pub struct Push {
  /// The `heads` argument, read.
  pub heads: HeadsArgument,
  /// The bundle's bytes, its chunks joined.
  pub bundle: Vec<u8>,
  /// The request's room left.
  pub admission: Admission,
}

/// Where a session's push is (the module doc's push).
#[derive(Debug, Default)]
enum Pushing {
  /// None is under way.
  #[default]
  None,
  /// Its bundle's chunks are being taken.
  Taking(Push),
  /// Taken whole, it waits for its caller ([`Stdio::take_push`]).
  Whole(Push),
  /// Its caller has it; its answer is not yet made ([`Stdio::pushed`]).
  Landing,
}

/// Input past the channel's window: the carrier let the client send more than the session may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PastWindow {
  /// The bytes the session would have held.
  pub held: u64,
  /// The window.
  pub capacity: u64,
}

/// What [`Stdio::step`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// Nothing: the request at the front is not yet whole, an answer waits to be sent, or the session has ended.
  Waiting,
  /// A request was answered, or the session ended; `consumed` bytes of input were taken, which the caller gives back
  /// to the channel's window.
  Stepped {
    /// What the step did.
    step: Step,
    /// The input bytes it took.
    consumed: usize,
  },
  /// A slice of a `getbundle`'s answer was made (A-745); `more` while its answer goes on.
  Streamed {
    /// Whether more of the answer follows.
    more: bool,
  },
  /// A chunk of a push's bundle was taken (the module doc's push); `consumed` bytes of input, which the caller gives
  /// back to the channel's window. Once `whole`, the push waits for its caller ([`Stdio::take_push`]).
  Took {
    /// The input bytes it took.
    consumed: usize,
    /// Whether it was the last, empty chunk.
    whole: bool,
  },
}

impl Stdio {
  /// A session over a channel whose window is `capacity` bytes.
  pub fn new(capacity: usize) -> Self {
    Self {
      capacity,
      input: Vec::new(),
      stdout: Vec::new(),
      stderr: Vec::new(),
      input_ended: false,
      ended: false,
      push: Pushing::None,
      push_room: None,
      answering: None,
    }
  }

  /// Admits each push from now on `room` bytes where that is more than a request's (A-711): a push is bounded by what
  /// its repository's shard can hold, never by one request's room.
  pub fn admit_pushes(&mut self, room: u64) {
    self.push_room = Some(room);
  }

  /// Takes the channel's next bytes of the client's input; refused, taking nothing, past the window. Bytes that come
  /// after the session has ended are dropped, as a process that has exited reads none.
  pub fn receive(&mut self, data: &[u8]) -> Result<(), PastWindow> {
    if self.ended {
      return Ok(());
    }
    let held = self.input.len().saturating_add(data.len());
    if held > self.capacity {
      return Err(PastWindow {
        held: to_u64(held),
        capacity: to_u64(self.capacity),
      });
    }
    self.input.extend_from_slice(data);
    Ok(())
  }

  /// The client's input has ended: the session ends once the whole requests before the end are served.
  pub fn eof(&mut self) {
    self.input_ended = true;
  }

  /// Serves the request at the front of the input, each admitted `room` bytes, when it is whole and nothing of the
  /// last answer waits to be sent (the module doc).
  pub fn step(&mut self, store: &impl Store, room: u64) -> Progress {
    if self.ended || !self.stdout.is_empty() || !self.stderr.is_empty() {
      return Progress::Waiting;
    }
    if !matches!(self.push, Pushing::None) {
      return self.step_push();
    }
    if self.input.starts_with(UNBUNDLE_LINE) {
      return self.unbundle(store, room);
    }
    if self.answering.is_some() {
      return self.stream(store, room);
    }
    let mut admission = Admission::new(room);
    match session::begin_serving(
      store,
      &self.input,
      &mut admission,
      (&mut self.stdout, &mut self.stderr),
      &mut self.answering,
    ) {
      Read::Done(step, used) => self.taken(step, used),
      Read::More { needed } => self.more(needed),
    }
  }

  /// Whether a `getbundle`'s answer is being made (A-745): [`Stdio::stream`] makes its next slice.
  pub fn streaming(&self) -> bool {
    self.answering.is_some()
  }

  /// Makes the next slice of the `getbundle` answer being made, within `room` (A-745), after what its standard output
  /// holds (its caller bounds how much a turn makes): [`Progress::Streamed`], whether more follow; refused, the session
  /// ends unfinished, as 7.2 fails mid-stream.
  pub fn stream(&mut self, store: &impl Store, room: u64) -> Progress {
    if self.ended {
      return Progress::Waiting;
    }
    let Some(answering) = self.answering.as_mut() else {
      return Progress::Waiting;
    };
    let mut admission = Admission::new(room);
    match answering.step(store, &mut admission, &mut self.stdout) {
      Ok(true) => Progress::Streamed { more: true },
      Ok(false) => {
        self.answering = None;
        Progress::Streamed { more: false }
      }
      Err(refusal) => {
        self.answering = None;
        self.ended = true;
        Progress::Stepped {
          step: Step::Ended {
            refused: Some(Refused::Serve(refusal)),
          },
          consumed: 0,
        }
      }
    }
  }

  /// Whether a push's bundle is being taken: its chunks need no store, so a caller may step them with
  /// [`Stdio::step_push`] and make no view of the repository for each.
  pub fn taking_push(&self) -> bool {
    matches!(self.push, Pushing::Taking(_))
  }

  /// Takes the next chunk of the push whose bundle is being taken (the module doc's push), when nothing of the last
  /// answer waits to be sent; waits while no bundle is being taken.
  pub fn step_push(&mut self) -> Progress {
    if self.ended || !self.stdout.is_empty() || !self.stderr.is_empty() {
      return Progress::Waiting;
    }
    match core::mem::take(&mut self.push) {
      Pushing::Taking(push) => self.take_chunk(push),
      other => {
        self.push = other;
        Progress::Waiting
      }
    }
  }

  /// What the session does with a request or a chunk not yet whole that needs `needed` bytes of input: refuses it out of
  /// band, ending the session, past the window; ends the session at the end of input; else waits.
  fn more(&mut self, needed: usize) -> Progress {
    if needed > self.capacity {
      let refused = Refused::PastWindow {
        needed: to_u64(needed),
        capacity: to_u64(self.capacity),
      };
      return self.end_refused(refused);
    }
    if self.input_ended {
      self.push = Pushing::None;
      return self.taken(Step::Ended { refused: None }, self.input.len());
    }
    Progress::Waiting
  }

  /// Answers `refused` out of band and ends the session, the rest of the input taken.
  fn end_refused(&mut self, refused: Refused) -> Progress {
    self.push = Pushing::None;
    ssh::respond_error(
      &session::message(&refused),
      &mut self.stdout,
      &mut self.stderr,
    );
    self.taken(
      Step::Ended {
        refused: Some(refused),
      },
      self.input.len(),
    )
  }

  /// Serves the `unbundle` at the front of the input (the module doc's push): its `heads` argument read and judged
  /// against `store`, a race answered as 7.2's `pusherr`, else the empty answer that asks for the bundle.
  fn unbundle(&mut self, store: &impl Store, room: u64) -> Progress {
    let mut admission = Admission::new(self.push_room.map_or(room, |push| push.max(room)));
    let (heads, used) = match heads_of(&self.input, &mut admission) {
      Ok(Read::Done(heads, used)) => (heads, used),
      Ok(Read::More { needed }) => return self.more(needed),
      Err((refused, Some(used))) => return self.answered_out_of_band(refused, used),
      Err((refused, None)) => return self.end_refused(refused),
    };
    let mut judging = admission;
    let raced = match check::judge_heads_argument(&heads, store, Moment::Preparing, &mut judging) {
      Ok(()) => None,
      Err(CheckRefusal::Raced { words }) => Some(words),
      Err(CheckRefusal::View(refusal)) => {
        return self.answered_out_of_band(Refused::Serve(refusal), used);
      }
    };
    if let Some(words) = raced {
      ssh::respond(&words, &mut self.stdout);
      let step = Step::Answered {
        refused: Some(Refused::Push),
      };
      return self.taken(step, used);
    }
    ssh::respond(&[], &mut self.stdout);
    self.push = Pushing::Taking(Push {
      heads,
      bundle: Vec::new(),
      admission,
    });
    self.taken(Step::Answered { refused: None }, used)
  }

  /// Answers `refused` out of band, the session going on past the `used` bytes of its request.
  fn answered_out_of_band(&mut self, refused: Refused, used: usize) -> Progress {
    ssh::respond_error(
      &session::message(&refused),
      &mut self.stdout,
      &mut self.stderr,
    );
    self.taken(
      Step::Answered {
        refused: Some(refused),
      },
      used,
    )
  }

  /// Takes the chunk of `push`'s bundle at the front of the input (the module doc's push), each charged to its
  /// admission; the push whole at the empty chunk.
  fn take_chunk(&mut self, mut push: Push) -> Progress {
    let (data, used) = match ssh::chunk(&self.input, &push.admission) {
      Ok(Read::Done(data, used)) => (data, used),
      Ok(Read::More { needed }) => {
        self.push = Pushing::Taking(push);
        return self.more(needed);
      }
      Err(refusal) => return self.end_refused(Refused::Request(refusal)),
    };
    let Some(data) = data else {
      self.push = Pushing::Whole(push);
      return self.took(used, true);
    };
    if let Err(past) = push.admission.charge(to_u64(data.len())) {
      return self.end_refused(Refused::from(past));
    }
    push.bundle.extend_from_slice(data);
    self.push = Pushing::Taking(push);
    self.took(used, false)
  }

  /// Drops the `used` input bytes a chunk took.
  fn took(&mut self, used: usize, whole: bool) -> Progress {
    let consumed = used.min(self.input.len());
    self.input.drain(..consumed);
    Progress::Took { consumed, whole }
  }

  /// The push taken whole, for its caller to land (the module doc's push); none when none waits. Until its answer is
  /// made ([`Stdio::pushed`]) the session serves nothing more.
  pub fn take_push(&mut self) -> Option<Push> {
    match core::mem::take(&mut self.push) {
      Pushing::Whole(push) => {
        self.push = Pushing::Landing;
        Some(push)
      }
      other => {
        self.push = other;
        None
      }
    }
  }

  /// Makes the answer to the push its caller took, from what it came to, framed as 7.2 frames each answer of
  /// `unbundle` over SSH (the module doc's push), each answer's bytes charged to `admission` first; one that cannot be
  /// made is answered out of band. The session then goes on.
  pub fn pushed(&mut self, outcome: &Outcome, admission: &mut Admission) {
    self.push = Pushing::None;
    let framed = match outcome {
      Outcome::Accepted(reply) | Outcome::Refused(reply) => reply
        .encoded_len()
        .map_err(|_| unencodable())
        .and_then(|length| charged(admission, length))
        .and_then(|()| {
          let mut stream = Vec::new();
          reply
            .encode(&mut stream)
            .map(|()| (Vec::new(), stream))
            .map_err(|_| unencodable())
        }),
      Outcome::Unprocessed(words) => charged(admission, ssh::respond_len(words)).map(|()| {
        let mut stdout = Vec::new();
        ssh::respond(words, &mut stdout);
        (Vec::new(), stdout)
      }),
      Outcome::Bundle1 => {
        let stderr = [ABORT, BUNDLE2_REQUIRED].concat();
        let mut stdout = Vec::new();
        ssh::respond_push(ABORTED, &mut stdout);
        charged(admission, to_u64(stderr.len().saturating_add(stdout.len())))
          .map(|()| (stderr, stdout))
      }
    };
    match framed {
      Ok((stderr, stdout)) => {
        self.stderr.extend_from_slice(&stderr);
        self.stdout.extend_from_slice(&stdout);
      }
      Err(refused) => ssh::respond_error(
        &session::message(&refused),
        &mut self.stdout,
        &mut self.stderr,
      ),
    }
  }

  /// Drops the `used` input bytes a step took, and ends the session where the step did.
  fn taken(&mut self, step: Step, used: usize) -> Progress {
    let consumed = used.min(self.input.len());
    self.input.drain(..consumed);
    if matches!(step, Step::Ended { .. }) {
      self.ended = true;
    }
    Progress::Stepped { step, consumed }
  }

  /// The answer bytes for standard output not yet taken: sent after [`Stdio::stderr`]'s.
  pub fn stdout(&self) -> &[u8] {
    &self.stdout
  }

  /// The error bytes for standard error not yet taken: sent before [`Stdio::stdout`]'s.
  pub fn stderr(&self) -> &[u8] {
    &self.stderr
  }

  /// The channel took the first `count` bytes of [`Stdio::stdout`].
  pub fn sent_stdout(&mut self, count: usize) {
    self.stdout.drain(..count.min(self.stdout.len()));
  }

  /// The channel took the first `count` bytes of [`Stdio::stderr`].
  pub fn sent_stderr(&mut self, count: usize) {
    self.stderr.drain(..count.min(self.stderr.len()));
  }

  /// Whether the session has ended; its last answer may still wait to be sent.
  pub fn ended(&self) -> bool {
    self.ended
  }

  /// Whether the session has ended and every answer was taken: the caller ends the channel with [`EXIT_STATUS`].
  pub fn finished(&self) -> bool {
    self.ended && self.stdout.is_empty() && self.stderr.is_empty()
  }
}

/// What reading a push's request found (the module doc's push): its `heads` argument and the input bytes the request
/// took; or a refusal, answered out of band with the session going on past the bytes it names, or ending it where it
/// names none.
type HeadsRead = Result<Read<HeadsArgument>, (Refused, Option<usize>)>;

/// Reads the `unbundle` request at the front of `input` (its command line, then its `heads` argument by its spec), its
/// argument charged to `admission`.
fn heads_of(input: &[u8], admission: &mut Admission) -> HeadsRead {
  let line = UNBUNDLE_LINE.len();
  let Some(spec) = spec_of(UNBUNDLE) else {
    return Err((Refused::NotServed, Some(line)));
  };
  let rest = input.get(line..).unwrap_or_default();
  let (arguments, taken) = match ssh::arguments(rest, spec, admission) {
    Ok(Read::Done(arguments, taken)) => (arguments, taken),
    Ok(Read::More { needed }) => {
      return Ok(Read::More {
        needed: line.saturating_add(needed),
      });
    }
    Err(refusal) => return Err((Refused::Request(refusal), None)),
  };
  let used = line.saturating_add(taken);
  let value = arguments
    .get(spec, HEADS)
    .ok_or((Refused::MissingArgument { name: HEADS }, Some(used)))?;
  check::heads_argument(value, admission)
    .map(|heads| Read::Done(heads, used))
    .map_err(|refusal| {
      let refused = match refusal {
        HeadsArgumentRefusal::NotHex { at } => Refused::Serve(Refusal::NodeList { at }),
        HeadsArgumentRefusal::PastAdmission(past) => Refused::from(past),
      };
      (refused, Some(used))
    })
}

/// `length` bytes charged to `admission`, refused past it.
fn charged(admission: &mut Admission, length: u64) -> Result<(), Refused> {
  admission.charge(length).map_err(Refused::from)
}

/// The refusal of a push's reply that cannot be written.
fn unencodable() -> Refused {
  Refused::Serve(Refusal::Unencodable {
    what: "a push's reply",
  })
}

#[cfg(test)]
#[path = "stdio_tests.rs"]
mod tests;
