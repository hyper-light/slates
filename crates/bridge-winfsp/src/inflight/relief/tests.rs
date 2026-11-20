//! The relief receiver against a modelled driver whose completion of one answer waits on another request (B3, A-725).
//! The driver runs on its own thread and the receivers speak to it over channels, as two threads speak to WinFsp's
//! volume: one receive or send at a time each, and a send returns only when the driver has completed what it carried.
//! Sending the answer to a shrink (SetInformation) issues a nested paging read and returns only once that read has been
//! answered, as `CcSetFileSizes` in WinFsp's completion waits on the drive (WinFsp v2.1 `src/sys/fileinfo.c` 1407,
//! `src/sys/file.c` 1852). The protocol runs over the real journal regions ([`super::super::capture`]).

#![allow(
  clippy::unwrap_used,
  clippy::unwrap_in_result,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use super::super::capture::{BYTES, Capture};
use super::super::{
  HINT_AT, KIND_AT, REQUEST_BASE, RESPONSE_MAX, Refusal, Request, SIZE_AT, Scratch, finish_response,
};
use super::{Desk, Hand, Port, Region, Station, serve};

/// Format: FspFsctlTransactSetInformationKind (fsctl.h), the shrink whose completion waits on the drive.
const SET_INFORMATION_KIND: u32 = 8;
/// Format: FspFsctlTransactReadKind (fsctl.h), the paging read the shrink's completion waits on.
const READ_KIND: u32 = 5;
/// Shape: the first request's IRP address; the nested read's follows it.
const FIRST_HINT: u64 = 0x1000;
/// Shape: the relief's beat in the model: short, so the test runs in milliseconds.
const BEAT: Duration = Duration::from_millis(2);
/// Shape: how long the model's idle receive waits before it answers no request, as the driver's transaction timeout does.
const IDLE_RECEIVE: Duration = Duration::from_millis(1);
/// Shape: the bound on the whole exchange. It needs a beat or two; a receiver that never takes the nested read never
/// finishes it.
const WITHIN: Duration = Duration::from_secs(10);

/// What a port asks of the driver.
enum Ask {
  Receive,
  Send(Vec<u8>),
}

/// What the driver answers a port.
enum Answer {
  Batch(Vec<u8>),
  Stopped,
  Sent,
}

/// The modelled driver: requests queued, the port whose send is held until its nested read is answered, and what was
/// completed, in order.
struct Driver {
  asks: Receiver<(usize, Ask)>,
  answers: Vec<SyncSender<Answer>>,
  pending: VecDeque<(u64, u32)>,
  held: Option<usize>,
  completed: Vec<u32>,
  done: SyncSender<Vec<u32>>,
}

impl Driver {
  fn run(mut self) {
    while let Ok((port, ask)) = self.asks.recv() {
      match ask {
        Ask::Receive => self.receive(port),
        Ask::Send(response) => self.complete(port, &response),
      }
    }
  }

  fn receive(&mut self, port: usize) {
    if self.completed.len() == 2 {
      self.answers[port].send(Answer::Stopped).unwrap();
      return;
    }
    let mut batch = Vec::new();
    while let Some((hint, kind)) = self.pending.pop_front() {
      let mut frame = vec![0u8; REQUEST_BASE];
      frame[SIZE_AT..SIZE_AT + 2]
        .copy_from_slice(&u16::try_from(REQUEST_BASE).unwrap().to_le_bytes());
      frame[KIND_AT..KIND_AT + 4].copy_from_slice(&kind.to_le_bytes());
      frame[HINT_AT..HINT_AT + 8].copy_from_slice(&hint.to_le_bytes());
      batch.extend_from_slice(&frame);
    }
    self.answers[port].send(Answer::Batch(batch)).unwrap();
  }

  /// Completes the request the response names. The shrink's completion issues the nested read and holds its sender
  /// until the read is answered.
  fn complete(&mut self, port: usize, response: &[u8]) {
    let kind = u32::from_le_bytes(response[KIND_AT..KIND_AT + 4].try_into().unwrap());
    self.completed.push(kind);
    match kind {
      SET_INFORMATION_KIND => {
        self.pending.push_back((FIRST_HINT + 0x100, READ_KIND));
        self.held = Some(port);
      }
      _ => {
        self.answers[port].send(Answer::Sent).unwrap();
        if let Some(held) = self.held.take() {
          self.answers[held].send(Answer::Sent).unwrap();
          let _ = self.done.try_send(self.completed.clone());
        }
      }
    }
  }
}

/// One receiver's channel to the driver.
struct ModelPort {
  id: usize,
  asks: SyncSender<(usize, Ask)>,
  answers: Receiver<Answer>,
}

impl Port<Refusal> for ModelPort {
  fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>, Refusal> {
    self.asks.send((self.id, Ask::Receive)).unwrap();
    match self.answers.recv().unwrap() {
      Answer::Batch(batch) => {
        if batch.is_empty() {
          std::thread::sleep(IDLE_RECEIVE);
        }
        buffer[..batch.len()].copy_from_slice(&batch);
        Ok(Some(batch.len()))
      }
      Answer::Stopped => Ok(None),
      Answer::Sent => panic!("a receive answered as a send"),
    }
  }

  fn send(&mut self, response: &[u8]) -> Result<(), Refusal> {
    self
      .asks
      .send((self.id, Ask::Send(response.to_vec())))
      .unwrap();
    match self.answers.recv().unwrap() {
      Answer::Sent => Ok(()),
      _ => panic!("a send answered as a receive"),
    }
  }
}

/// The modelled shard: answers every request with success, its own kind and hint.
struct ModelStation;

impl Station<Refusal> for ModelStation {
  fn operate(
    &mut self,
    request: &Request<'_>,
    response: &mut [u8],
    _replaying: bool,
  ) -> Result<usize, Refusal> {
    let header = request.cancelled_response()?;
    response[..header.len()].copy_from_slice(&header);
    finish_response(response, 0)
  }

  fn post_confirm(&mut self, _context: u64) -> Result<(), Refusal> {
    Ok(())
  }

  fn confirmed(&mut self) -> Result<(), Refusal> {
    Ok(())
  }

  fn rename_open_names(&mut self, _from: &str, _to: &str) -> Result<(), Refusal> {
    Ok(())
  }
}

/// A journal region in plain memory.
struct Journal(Vec<u8>);

impl Region for Journal {
  fn capture(&mut self) -> Result<Capture<'_>, Refusal> {
    Capture::new(&mut self.0)
  }
}

/// B3 (A-725). Do: serve a drive whose one request is a shrink, whose answer's completion waits on a nested read the
/// driver issues only once that answer is sent. Expect: both completed, the shrink first and then the read, and the
/// relief counted as having taken the station from the held send. Before the relief, the one receiver was itself held
/// in the shrink's send, the read reached no one, and fsx seed 1 over the drive stopped for good (CI windows-latest,
/// 2026-11-20).
#[test]
fn a_send_whose_completion_waits_on_another_request_is_relieved() {
  let (asks, asked) = sync_channel(2);
  let (primary_answers, primary_answered) = sync_channel(1);
  let (relief_answers, relief_answered) = sync_channel(1);
  let (done, finished) = sync_channel(1);
  let driver = Driver {
    asks: asked,
    answers: vec![primary_answers, relief_answers],
    pending: VecDeque::from([(FIRST_HINT, SET_INFORMATION_KIND)]),
    held: None,
    completed: Vec::new(),
    done,
  };
  let driver_thread = std::thread::spawn(move || driver.run());
  let (relieved, relieved_count) = sync_channel(1);
  let relief_asks = asks.clone();
  let server = std::thread::spawn(move || {
    let hand = Hand::default();
    let (mut primary_journal, mut relief_journal) =
      (Journal(vec![0; BYTES]), Journal(vec![0; BYTES]));
    let (mut primary_response, mut relief_response) =
      (vec![0u8; RESPONSE_MAX], vec![0u8; RESPONSE_MAX]);
    let served = serve(
      &hand,
      (
        Desk {
          port: ModelPort {
            id: 0,
            asks,
            answers: primary_answered,
          },
          region: &mut primary_journal,
          scratch: Scratch::new().unwrap(),
          response: &mut primary_response,
        },
        Desk {
          port: ModelPort {
            id: 1,
            asks: relief_asks,
            answers: relief_answered,
          },
          region: &mut relief_journal,
          scratch: Scratch::new().unwrap(),
          response: &mut relief_response,
        },
      ),
      Box::new(ModelStation),
      BEAT,
      |_| Ok(()),
    );
    relieved.send(hand.relieved()).unwrap();
    served
  });
  let started = Instant::now();
  let completed = finished.recv_timeout(WITHIN).unwrap_or_else(|_| {
    panic!(
      "the shrink's answer never completed in {:?}: its nested read reached no receiver",
      started.elapsed()
    )
  });
  assert_eq!(completed, vec![SET_INFORMATION_KIND, READ_KIND]);
  server.join().unwrap().unwrap();
  assert!(
    relieved_count.recv().unwrap() >= 1,
    "the relief took no station"
  );
  drop(driver_thread);
}
