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
//! **Differs from 7.2, on purpose.** A request cut short by the end of input is dropped, not served: 7.2 reads a last
//! line that has no newline and serves it less its last byte.

use super::getbundle::Store;
use super::session::{self, Refused, Step};
use crate::Admission;
use crate::read::to_u64;
use crate::ssh::{self, Read};

/// Format: the exit status a session ends with: 7.2's `hg serve --stdio` exits 0 when its loop ends, whatever it
/// answered.
pub const EXIT_STATUS: u32 = 0;

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
    }
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
    let mut admission = Admission::new(room);
    match session::serve(
      store,
      &self.input,
      &mut admission,
      &mut self.stdout,
      &mut self.stderr,
    ) {
      Read::Done(step, used) => self.taken(step, used),
      Read::More { needed } if needed > self.capacity => {
        let refused = Refused::PastWindow {
          needed: to_u64(needed),
          capacity: to_u64(self.capacity),
        };
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
      Read::More { .. } if self.input_ended => {
        self.taken(Step::Ended { refused: None }, self.input.len())
      }
      Read::More { .. } => Progress::Waiting,
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

#[cfg(test)]
#[path = "stdio_tests.rs"]
mod tests;
