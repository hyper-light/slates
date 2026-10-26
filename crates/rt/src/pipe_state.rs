//! The decisions behind a Windows pipe end on the runtime (§4.3, §4.6; A-179), kept free of the OS so they are tested
//! on every host. The Win32 calls they drive live in `crate::pipe` and the IOCP driver (`crate::iocp`).
//!
//! **Why completions, not readiness.** Windows reports a named pipe's I/O when it finishes, and the AFD reactor that
//! gives sockets readiness (`crate::afd`) does not take pipes. So the driver keeps at most one read and one write of each
//! end in the kernel at a time, each into a buffer the kernel owns until that operation's completion comes back. Every
//! operation completes through the port, even one that finished at once, so a buffer is never reclaimed twice (the
//! driver does not set `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`; an operation refused at once posts nothing and comes back
//! here directly). These two sides turn that into the runtime's readiness model, in which a task tries an operation, is
//! told it would block, awaits readiness ([`crate::readiness`]) and tries again:
//! - **Reading.** A read the kernel finished leaves its bytes held here, and a try takes them, a part at a time when the
//!   caller's buffer is smaller. A try that finds nothing held starts the next read, into the spare buffer the last read
//!   left, so a steady stream reuses one allocation, and it would block until that read finishes. Readiness is a finished
//!   read: bytes held, the end of the stream, or an error to report. The end of the stream is the peer gone
//!   (`ERROR_BROKEN_PIPE`), and it holds from then on.
//! - **Writing.** A try copies what fits into the side's buffer, hands it to the kernel, and answers that the bytes are
//!   taken. A byte-mode pipe's write finishes whole (measured, `docs/research/measurements/windows-pipe` in Shards, PM
//!   M340), so they are written unless the peer goes first, which the next try reports. While a write is in the kernel
//!   the next try would block. Readiness is its completion.
//! - **Waking.** Each side keeps one waiter: the interest word of the registration made while its operation was in the
//!   kernel. The completion hands it back for the driver to deliver, and a registration made while the side is ready is
//!   delivered at once.
//!
//! The measurements that chose a named pipe for the Shards route on Windows, over AF_UNIX (an abstract address cannot
//! be connected to, so no pair without a socket file) and loopback TCP (two to eight times slower), are Shards' PM
//! M340, run on jupiter 2026-10-26.

// Pure, so tested on every host; only the Windows driver drives it, so elsewhere only the tests call it.
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

/// What a try to read a pipe end found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeRead {
  /// This many bytes were copied into the caller's buffer (none only when that buffer was empty).
  Bytes(usize),
  /// Nothing is held and a read is in the kernel: await readability and try again.
  WouldBlock,
  /// The peer has gone, and every byte it wrote has been taken.
  Ended,
}

/// What a try to write a pipe end did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeWrite {
  /// The kernel took this many bytes. The write finishes whole unless the peer goes first, which the next try reports.
  Taken(usize),
  /// A write is still in the kernel: await writability and try again.
  WouldBlock,
}

/// How one operation ended, as the driver reads its status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
  /// It moved this many bytes.
  Moved(usize),
  /// The peer had gone (`ERROR_BROKEN_PIPE`, `ERROR_NO_DATA`, `ERROR_PIPE_NOT_CONNECTED`).
  PeerGone,
  /// It was cancelled (`ERROR_OPERATION_ABORTED`), because the end is being released.
  Cancelled,
  /// Any other error, by its code.
  Failed(i32),
}

/// How handing the kernel an operation went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Started {
  /// The kernel took it; its completion follows through the port.
  InKernel,
  /// The kernel refused it at once, so no completion follows: the outcome, and the buffer back.
  Refused(Outcome, Vec<u8>),
}

/// What a registration of interest asks of the driver.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Want {
  /// The side is ready: deliver the interest word at once.
  Now,
  /// The side's operation is in the kernel; its completion delivers the word.
  Later,
  /// Nothing is in the kernel: start an operation into this buffer, report it (`started`), and register again.
  Start(Vec<u8>),
}

/// What a try to read asks of the driver, beside its answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadStep {
  /// The answer.
  Answer(PipeRead),
  /// The error a finished read reported, for the caller (it holds from then on).
  Failed(i32),
  /// Nothing held and nothing in the kernel: start a read into this buffer, report it (`started`), and answer
  /// [`PipeRead::WouldBlock`] unless the start was refused.
  Start(Vec<u8>),
}

/// What a try to write asks of the driver, beside its answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteStep {
  /// The answer.
  Answer(PipeWrite),
  /// The peer has gone: a write can never be taken again.
  PeerGone,
  /// The error a finished write reported, for the caller (it holds from then on).
  Failed(i32),
  /// Hand the kernel a write of the first `count` bytes of this buffer, report it (`started`), and answer
  /// [`PipeWrite::Taken`] with `count` unless the start was refused.
  Start(Vec<u8>, usize),
}

/// What a read side holds.
#[derive(Debug)]
enum ReadState {
  /// Nothing held, nothing in the kernel.
  Idle,
  /// A read the kernel owns, and the waiter its completion wakes.
  InKernel { waiter: Option<u64> },
  /// A finished read's bytes: the first `filled` of `bytes`, the first `taken` of those handed out.
  Holding {
    bytes: Vec<u8>,
    filled: usize,
    taken: usize,
  },
  /// The peer has gone.
  Ended,
  /// A read failed with this code.
  Failed(i32),
}

/// One pipe end's reading.
#[derive(Debug)]
pub(crate) struct ReadSide {
  state: ReadState,
  /// The buffer the next read fills: the one the last read left once taken whole, kept at its full length, so a steady
  /// stream neither allocates nor zeroes again.
  spare: Option<Vec<u8>>,
  /// How many bytes one read asks for, as the end's owner derived it (its largest frame, for the Shards route).
  capacity: usize,
}

impl ReadSide {
  /// A side whose reads ask for `capacity` bytes each.
  pub(crate) fn new(capacity: usize) -> ReadSide {
    ReadSide {
      state: ReadState::Idle,
      spare: None,
      capacity,
    }
  }

  /// The buffer the next read fills, at the side's capacity.
  fn buffer(&mut self) -> Vec<u8> {
    let mut buffer = self.spare.take().unwrap_or_default();
    // Zeroed only where it grows: a spare is already at the side's capacity.
    buffer.resize(self.capacity, 0);
    buffer
  }

  /// A try to read into `out`.
  pub(crate) fn try_read(&mut self, out: &mut [u8]) -> ReadStep {
    match &mut self.state {
      ReadState::Holding {
        bytes,
        filled,
        taken,
      } => {
        let rest = bytes.get(*taken..*filled).unwrap_or_default();
        let count = rest.len().min(out.len());
        if let (Some(to), Some(from)) = (out.get_mut(..count), rest.get(..count)) {
          to.copy_from_slice(from);
        }
        *taken = taken.saturating_add(count);
        if *taken >= *filled {
          self.spare = Some(std::mem::take(bytes));
          self.state = ReadState::Idle;
        }
        ReadStep::Answer(PipeRead::Bytes(count))
      }
      ReadState::InKernel { .. } => ReadStep::Answer(PipeRead::WouldBlock),
      ReadState::Ended => ReadStep::Answer(PipeRead::Ended),
      ReadState::Failed(code) => ReadStep::Failed(*code),
      ReadState::Idle => ReadStep::Start(self.buffer()),
    }
  }

  /// How handing the kernel the read went.
  pub(crate) fn started(&mut self, started: Started) {
    self.state = match started {
      Started::InKernel => ReadState::InKernel { waiter: None },
      Started::Refused(outcome, buffer) => {
        self.spare = Some(buffer);
        match outcome {
          Outcome::PeerGone => ReadState::Ended,
          Outcome::Failed(code) => ReadState::Failed(code),
          Outcome::Moved(_) | Outcome::Cancelled => ReadState::Idle,
        }
      }
    };
  }

  /// A read the kernel finished, its buffer back: the waiter to wake, if one registered. A completion that finds no
  /// read in the kernel (it cannot happen while the driver keeps one at a time) keeps the buffer and wakes no one.
  pub(crate) fn finished(&mut self, outcome: Outcome, buffer: Vec<u8>) -> Option<u64> {
    let ReadState::InKernel { waiter } = self.state else {
      self.spare.get_or_insert(buffer);
      return None;
    };
    self.state = match outcome {
      Outcome::Moved(count) if count > 0 => ReadState::Holding {
        filled: count.min(buffer.len()),
        bytes: buffer,
        taken: 0,
      },
      other => {
        self.spare = Some(buffer);
        match other {
          Outcome::PeerGone => ReadState::Ended,
          Outcome::Failed(code) => ReadState::Failed(code),
          Outcome::Moved(_) | Outcome::Cancelled => ReadState::Idle,
        }
      }
    };
    waiter
  }

  /// A registration of readability by `interest`.
  pub(crate) fn want(&mut self, interest: u64) -> Want {
    match &mut self.state {
      ReadState::Holding { .. } | ReadState::Ended | ReadState::Failed(_) => Want::Now,
      ReadState::InKernel { waiter } => {
        *waiter = Some(interest);
        Want::Later
      }
      ReadState::Idle => Want::Start(self.buffer()),
    }
  }

  /// Whether a read is in the kernel (its buffer the kernel's until the completion).
  pub(crate) fn in_kernel(&self) -> bool {
    matches!(self.state, ReadState::InKernel { .. })
  }
}

/// What a write side holds.
#[derive(Debug)]
enum WriteState {
  /// Nothing in the kernel.
  Idle,
  /// A write the kernel owns, and the waiter its completion wakes.
  InKernel { waiter: Option<u64> },
  /// The peer has gone.
  Ended,
  /// A write failed with this code.
  Failed(i32),
}

/// One pipe end's writing.
#[derive(Debug)]
pub(crate) struct WriteSide {
  state: WriteState,
  /// The buffer the next write is copied into, kept at its full length.
  spare: Option<Vec<u8>>,
  /// The most bytes one write takes.
  capacity: usize,
}

impl WriteSide {
  /// A side whose writes take up to `capacity` bytes each.
  pub(crate) fn new(capacity: usize) -> WriteSide {
    WriteSide {
      state: WriteState::Idle,
      spare: None,
      capacity,
    }
  }

  /// A try to write `bytes`.
  pub(crate) fn try_write(&mut self, bytes: &[u8]) -> WriteStep {
    match self.state {
      WriteState::InKernel { .. } => WriteStep::Answer(PipeWrite::WouldBlock),
      WriteState::Ended => WriteStep::PeerGone,
      WriteState::Failed(code) => WriteStep::Failed(code),
      WriteState::Idle => {
        let mut buffer = self.spare.take().unwrap_or_default();
        buffer.resize(self.capacity, 0);
        let count = bytes.len().min(self.capacity);
        if let (Some(to), Some(from)) = (buffer.get_mut(..count), bytes.get(..count)) {
          to.copy_from_slice(from);
        }
        WriteStep::Start(buffer, count)
      }
    }
  }

  /// How handing the kernel the write went.
  pub(crate) fn started(&mut self, started: Started) {
    self.state = match started {
      Started::InKernel => WriteState::InKernel { waiter: None },
      Started::Refused(outcome, buffer) => {
        self.spare = Some(buffer);
        match outcome {
          Outcome::PeerGone => WriteState::Ended,
          Outcome::Failed(code) => WriteState::Failed(code),
          Outcome::Moved(_) | Outcome::Cancelled => WriteState::Idle,
        }
      }
    };
  }

  /// A write the kernel finished, its buffer back: the waiter to wake, if one registered.
  pub(crate) fn finished(&mut self, outcome: Outcome, buffer: Vec<u8>) -> Option<u64> {
    self.spare = Some(buffer);
    let WriteState::InKernel { waiter } = self.state else {
      return None;
    };
    self.state = match outcome {
      Outcome::Moved(_) | Outcome::Cancelled => WriteState::Idle,
      Outcome::PeerGone => WriteState::Ended,
      Outcome::Failed(code) => WriteState::Failed(code),
    };
    waiter
  }

  /// A registration of writability by `interest`.
  pub(crate) fn want(&mut self, interest: u64) -> Want {
    match &mut self.state {
      WriteState::InKernel { waiter } => {
        *waiter = Some(interest);
        Want::Later
      }
      WriteState::Idle | WriteState::Ended | WriteState::Failed(_) => Want::Now,
    }
  }

  /// Whether a write is in the kernel.
  pub(crate) fn in_kernel(&self) -> bool {
    matches!(self.state, WriteState::InKernel { .. })
  }
}

#[cfg(test)]
mod tests {
  //! The sides against a model of a pipe: the bytes the kernel's reads delivered, in order, must be exactly the bytes
  //! the tries took, a waiter registered while an operation was in the kernel must be handed back by its completion,
  //! and the end of the stream must hold once seen. Every sequence of the alphabet below up to a depth is driven
  //! (exhaustive, not sampled), so no ordering of tries, registrations and completions is left out.
  #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

  use super::*;

  /// Shape: bytes one read asks for (small, so a delivery fills it and a take splits it).
  const CAPACITY: usize = 3;
  /// Shape: how deep the exhaustive walk goes: every sequence of this many events (fewer under Miri, which runs each
  /// step thousands of times slower; the shallow walk still holds every state the sides have).
  const DEPTH: usize = if cfg!(miri) { 3 } else { 6 };

  /// One thing that can happen to a read side.
  #[derive(Clone, Copy, Debug)]
  enum Event {
    /// A try with a buffer of this many bytes; a start it asks for the kernel takes.
    Try(usize),
    /// A try whose start, if it asks for one, the kernel refuses at once: the peer gone.
    TryRefused,
    /// A registration of readability; a start it asks for the kernel takes.
    Want,
    /// The read in the kernel finishes with this many bytes (0 is a finished read of nothing).
    Deliver(usize),
    /// The read in the kernel finishes: the peer gone.
    Gone,
    /// The read in the kernel finishes: an error.
    Fail,
    /// The read in the kernel finishes: cancelled.
    Cancel,
  }

  const EVENTS: [Event; 10] = [
    Event::Try(1),
    Event::Try(CAPACITY),
    Event::TryRefused,
    Event::Want,
    Event::Deliver(0),
    Event::Deliver(1),
    Event::Deliver(CAPACITY),
    Event::Gone,
    Event::Fail,
    Event::Cancel,
  ];

  /// The model's view of one run: what was delivered, what was taken, the registered waiter, whether the end was seen.
  #[derive(Default)]
  struct Model {
    delivered: Vec<u8>,
    taken: Vec<u8>,
    waiter: Option<u64>,
    ended: bool,
    failed: bool,
    next_byte: u8,
    next_word: u64,
  }

  /// Drives one event, checking each answer against the model. A completion only happens while a read is in the
  /// kernel, as the port delivers only what was submitted.
  fn step(side: &mut ReadSide, model: &mut Model, event: Event) {
    match event {
      Event::Try(size) => try_once(side, model, size, false),
      Event::TryRefused => try_once(side, model, 1, true),
      Event::Want => want_once(side, model),
      Event::Deliver(_) | Event::Gone | Event::Fail | Event::Cancel => {
        finish_once(side, model, event)
      }
    }
    assert_eq!(
      model.taken,
      model.delivered[..model.taken.len()],
      "taken in delivery order"
    );
  }

  /// A try with a buffer of `size` bytes; a start it asks for is refused (the peer gone) when `refused`.
  fn try_once(side: &mut ReadSide, model: &mut Model, size: usize, refused: bool) {
    let mut out = vec![0u8; size];
    match side.try_read(&mut out) {
      ReadStep::Answer(PipeRead::Bytes(count)) => {
        assert!(count > 0 && count <= size, "a take hands out what fits");
        model.taken.extend_from_slice(&out[..count]);
      }
      ReadStep::Answer(PipeRead::WouldBlock) => {
        assert!(side.in_kernel(), "only an in-kernel read blocks")
      }
      ReadStep::Answer(PipeRead::Ended) => assert!(model.ended, "the end only after the peer went"),
      ReadStep::Failed(_) => assert!(model.failed, "a failure only after one was reported"),
      ReadStep::Start(buffer) => {
        assert!(
          !model.ended && !model.failed,
          "nothing starts after the end or a failure"
        );
        assert_eq!(
          buffer.len(),
          CAPACITY,
          "a read asks for the side's capacity"
        );
        if refused {
          side.started(Started::Refused(Outcome::PeerGone, buffer));
          model.ended = true;
        } else {
          side.started(Started::InKernel);
        }
      }
    }
  }

  /// A registration of readability; a start it asks for the kernel takes.
  fn want_once(side: &mut ReadSide, model: &mut Model) {
    model.next_word += 1;
    let word = model.next_word;
    match side.want(word) {
      Want::Now => assert!(
        !side.in_kernel(),
        "ready only when nothing is in the kernel"
      ),
      Want::Later => model.waiter = Some(word),
      Want::Start(buffer) => {
        assert_eq!(
          buffer.len(),
          CAPACITY,
          "a read asks for the side's capacity"
        );
        side.started(Started::InKernel);
        assert_eq!(
          side.want(word),
          Want::Later,
          "registered again once a read is in the kernel"
        );
        model.waiter = Some(word);
      }
    }
  }

  /// The read in the kernel finishes as `event` says, if one is in the kernel.
  fn finish_once(side: &mut ReadSide, model: &mut Model, event: Event) {
    if !side.in_kernel() {
      return;
    }
    let mut buffer = vec![0u8; CAPACITY];
    let outcome = match event {
      Event::Deliver(count) => {
        for byte in buffer.iter_mut().take(count) {
          *byte = model.next_byte;
          model.delivered.push(model.next_byte);
          model.next_byte = model.next_byte.wrapping_add(1);
        }
        Outcome::Moved(count)
      }
      Event::Gone => {
        model.ended = true;
        Outcome::PeerGone
      }
      Event::Fail => {
        model.failed = true;
        Outcome::Failed(1)
      }
      _ => Outcome::Cancelled,
    };
    let woken = side.finished(outcome, buffer);
    assert_eq!(
      woken,
      model.waiter.take(),
      "the completion hands back the waiter registered while in the kernel"
    );
  }

  /// Takes every byte still held once the walk is over, as a reader would, so "taken" can be compared whole.
  fn drain(side: &mut ReadSide, model: &mut Model) {
    loop {
      let mut out = vec![0u8; CAPACITY];
      match side.try_read(&mut out) {
        ReadStep::Answer(PipeRead::Bytes(count)) => model.taken.extend_from_slice(&out[..count]),
        ReadStep::Start(buffer) => {
          side.started(Started::Refused(Outcome::Cancelled, buffer));
          return;
        }
        _ => return,
      }
    }
  }

  fn walk(side: &mut ReadSide, model: &mut Model, depth: usize, runs: &mut u64) {
    if depth == 0 {
      drain(side, model);
      assert_eq!(
        model.taken, model.delivered,
        "every delivered byte is taken, once, in order"
      );
      *runs += 1;
      return;
    }
    for event in EVENTS {
      let mut side_copy = clone_side(side);
      let mut model_copy = clone_model(model);
      step(&mut side_copy, &mut model_copy, event);
      walk(&mut side_copy, &mut model_copy, depth - 1, runs);
    }
  }

  fn clone_side(side: &ReadSide) -> ReadSide {
    let state = match &side.state {
      ReadState::Idle => ReadState::Idle,
      ReadState::InKernel { waiter } => ReadState::InKernel { waiter: *waiter },
      ReadState::Holding {
        bytes,
        filled,
        taken,
      } => ReadState::Holding {
        bytes: bytes.clone(),
        filled: *filled,
        taken: *taken,
      },
      ReadState::Ended => ReadState::Ended,
      ReadState::Failed(code) => ReadState::Failed(*code),
    };
    ReadSide {
      state,
      spare: side.spare.clone(),
      capacity: side.capacity,
    }
  }

  fn clone_model(model: &Model) -> Model {
    Model {
      delivered: model.delivered.clone(),
      taken: model.taken.clone(),
      waiter: model.waiter,
      ended: model.ended,
      failed: model.failed,
      next_byte: model.next_byte,
      next_word: model.next_word,
    }
  }

  /// A-179: do every sequence of tries, refused starts, registrations and completions up to `DEPTH`; expect each byte
  /// the kernel delivered taken exactly once and in order, every waiter registered while a read was in the kernel
  /// handed back by its completion, nothing started after the end or a failure, and the end holding once seen.
  #[test]
  fn a_read_side_hands_out_exactly_what_its_reads_delivered_and_wakes_its_waiter() {
    let mut runs = 0;
    walk(
      &mut ReadSide::new(CAPACITY),
      &mut Model::default(),
      DEPTH,
      &mut runs,
    );
    let alphabet = u64::try_from(EVENTS.len()).unwrap();
    assert_eq!(
      runs,
      alphabet.pow(u32::try_from(DEPTH).unwrap()),
      "every sequence was walked"
    );
  }

  /// A-179: do take a delivery a byte at a time; expect the bytes in order, and the read's buffer kept as the next
  /// read's (no allocation per read in a steady stream).
  #[test]
  fn a_held_read_is_taken_in_parts_and_its_buffer_is_reused() {
    let mut side = ReadSide::new(CAPACITY);
    let ReadStep::Start(buffer) = side.try_read(&mut [0u8; 1]) else {
      panic!("an idle side starts a read")
    };
    let address = buffer.as_ptr();
    side.started(Started::InKernel);
    let mut filled = buffer;
    filled.copy_from_slice(&[7, 8, 9]);
    assert_eq!(side.finished(Outcome::Moved(3), filled), None);
    let mut taken = Vec::new();
    for _ in 0..3 {
      let mut out = [0u8; 1];
      assert_eq!(
        side.try_read(&mut out),
        ReadStep::Answer(PipeRead::Bytes(1))
      );
      taken.push(out[0]);
    }
    assert_eq!(taken, [7, 8, 9]);
    let ReadStep::Start(again) = side.try_read(&mut [0u8; 1]) else {
      panic!("a side taken whole starts the next read")
    };
    assert_eq!(
      again.as_ptr(),
      address,
      "the next read fills the same buffer"
    );
  }

  /// A-179: do write while a write is in the kernel, then finish it; expect the second try to block, the waiter
  /// registered meanwhile woken by the completion, and the next try taken.
  #[test]
  fn a_write_side_blocks_while_its_write_is_in_the_kernel() {
    let mut side = WriteSide::new(4);
    let WriteStep::Start(buffer, count) = side.try_write(b"abcdef") else {
      panic!("an idle side starts a write")
    };
    assert_eq!(
      (count, &buffer[..count]),
      (4, &b"abcd"[..]),
      "a write takes up to the side's capacity"
    );
    side.started(Started::InKernel);
    assert!(
      side.in_kernel(),
      "the write is the kernel's until its completion"
    );
    assert_eq!(
      side.try_write(b"ef"),
      WriteStep::Answer(PipeWrite::WouldBlock)
    );
    assert_eq!(side.want(41), Want::Later);
    assert_eq!(side.finished(Outcome::Moved(4), buffer), Some(41));
    assert!(matches!(side.try_write(b"ef"), WriteStep::Start(_, 2)));
  }

  /// A-179: do finish a write with the peer gone; expect every later try refused as the peer gone, and a registration
  /// answered at once (so a task awaiting writability learns of it).
  #[test]
  fn a_write_whose_peer_went_refuses_every_later_try() {
    let mut side = WriteSide::new(4);
    let WriteStep::Start(buffer, _) = side.try_write(b"ab") else {
      panic!("an idle side starts a write")
    };
    side.started(Started::InKernel);
    assert_eq!(side.want(7), Want::Later);
    assert_eq!(side.finished(Outcome::PeerGone, buffer), Some(7));
    assert_eq!(side.try_write(b"cd"), WriteStep::PeerGone);
    assert_eq!(side.want(8), Want::Now);
  }

  /// A-179: do refuse a write's start at once (the kernel posts nothing); expect its buffer kept for the next try and
  /// the failure reported from then on.
  #[test]
  fn a_write_refused_at_once_keeps_its_buffer_and_reports_the_failure() {
    let mut side = WriteSide::new(4);
    let WriteStep::Start(buffer, _) = side.try_write(b"ab") else {
      panic!("an idle side starts a write")
    };
    side.started(Started::Refused(Outcome::Failed(5), buffer));
    assert_eq!(side.try_write(b"cd"), WriteStep::Failed(5));
    assert!(side.spare.is_some(), "the refused write's buffer was kept");
  }
}
