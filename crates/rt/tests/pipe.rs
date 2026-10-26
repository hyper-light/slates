//! A Windows named pipe's end served through the runtime's IOCP driver (§4.6, A-179): a task adopts the daemon's end
//! of a pair and reads and writes it by completions, awaiting readiness between tries, while a plain thread holds the
//! other end. The pair is made as the daemon makes the Shards route's (a fresh name, its first instance and only one,
//! remote clients refused). The peer's end is synchronous here, so the test's own thread can read and write it
//! directly; the runtime's end is overlapped, as it must be.
#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use slates_rt::error::RtError;
use slates_rt::pipe::{PipeEnd, PipeRead, PipeWrite};
use slates_rt::runtime::{Runtime, RuntimeConfig};
use windows_sys::Win32::Foundation::{
  GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
  PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
  CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Shape: how long a test waits for its task's answer before it fails, rather than hanging the binary.
const WAIT: Duration = Duration::from_secs(20);
/// Shape: the pipe's buffers, small, so a write larger than them waits for the reader.
const PIPE_BUFFER: u32 = 4096;
/// Shape: the most bytes one read of the runtime's end asks for, and one write takes.
const SIDE_BYTES: usize = 1024;

/// The pipes this process has made, which numbers the next one's name.
static PIPES_MADE: AtomicUsize = AtomicUsize::new(0);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// The calling thread's last Win32 error, for a failed call's message.
fn last_error() -> u32 {
  // SAFETY: no arguments; reads the calling thread's last-error value.
  unsafe { GetLastError() }
}

fn wide(text: &str) -> Vec<u16> {
  text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A connected pair: the runtime's end (overlapped) and the peer's (synchronous).
fn pair() -> (OwnedHandle, OwnedHandle) {
  let name = format!(
    r"\\.\pipe\slates-rt-test-{}-{}",
    std::process::id(),
    PIPES_MADE.fetch_add(1, Ordering::Relaxed)
  );
  let wide_name = wide(&name);
  // SAFETY: a NUL-terminated name and plain arguments; the result is checked.
  let server = unsafe {
    CreateNamedPipeW(
      wide_name.as_ptr(),
      PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
      PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
      1,
      PIPE_BUFFER,
      PIPE_BUFFER,
      0,
      std::ptr::null(),
    )
  };
  assert_ne!(
    server,
    INVALID_HANDLE_VALUE,
    "CreateNamedPipeW: {}",
    last_error()
  );
  // SAFETY: a NUL-terminated name and plain arguments; the result is checked.
  let client = unsafe {
    CreateFileW(
      wide_name.as_ptr(),
      GENERIC_READ | GENERIC_WRITE,
      0,
      std::ptr::null(),
      OPEN_EXISTING,
      0,
      std::ptr::null_mut(),
    )
  };
  assert_ne!(
    client,
    INVALID_HANDLE_VALUE,
    "CreateFileW: {}",
    last_error()
  );
  // The client is connected by its open, so the server end needs no ConnectNamedPipe (which would answer
  // ERROR_PIPE_CONNECTED): the runtime's first read starts the conversation.
  // SAFETY: each handle was just made and is owned by this value alone from here on.
  unsafe {
    (
      OwnedHandle::from_raw_handle(server),
      OwnedHandle::from_raw_handle(client),
    )
  }
}

/// Writes all of `bytes` to the peer's synchronous end.
fn peer_write(peer: &OwnedHandle, mut bytes: &[u8]) {
  while !bytes.is_empty() {
    let mut written = 0u32;
    let handle: HANDLE = peer.as_raw_handle();
    // SAFETY: a synchronous handle of this test, the bytes and their length.
    let ok = unsafe {
      WriteFile(
        handle,
        bytes.as_ptr(),
        u32::try_from(bytes.len()).unwrap(),
        &mut written,
        std::ptr::null_mut(),
      )
    };
    assert_ne!(ok, 0, "the peer's write: {}", last_error());
    bytes = &bytes[written as usize..];
  }
}

/// Reads exactly `count` bytes from the peer's synchronous end.
fn peer_read(peer: &OwnedHandle, count: usize) -> Vec<u8> {
  let mut out = vec![0u8; count];
  let mut filled = 0;
  while filled < count {
    let mut read = 0u32;
    let handle: HANDLE = peer.as_raw_handle();
    let rest = &mut out[filled..];
    // SAFETY: a synchronous handle of this test, a buffer and its length.
    let ok = unsafe {
      ReadFile(
        handle,
        rest.as_mut_ptr(),
        u32::try_from(rest.len()).unwrap(),
        &mut read,
        std::ptr::null_mut(),
      )
    };
    assert_ne!(ok, 0, "the peer's read: {}", last_error());
    filled += read as usize;
  }
  out
}

/// Reads the runtime's end until `count` bytes or its end, taking at most `take` bytes a try; returns the bytes and
/// whether the end of the stream was seen.
async fn read_all(end: &PipeEnd, count: usize, take: usize) -> Result<(Vec<u8>, bool), RtError> {
  let mut got = Vec::with_capacity(count);
  let mut buffer = vec![0u8; take];
  while got.len() < count {
    match end.try_read(&mut buffer)? {
      PipeRead::Bytes(n) => got.extend_from_slice(&buffer[..n]),
      PipeRead::WouldBlock => end.readable().await?,
      PipeRead::Ended => return Ok((got, true)),
    }
  }
  Ok((got, false))
}

/// Writes all of `bytes` to the runtime's end; returns how often a try would have blocked, and says so on `first_block`
/// the first time, if given one.
async fn write_all(
  end: &PipeEnd,
  mut bytes: &[u8],
  first_block: Option<&std::sync::mpsc::Sender<()>>,
) -> Result<usize, RtError> {
  let mut blocked = 0;
  while !bytes.is_empty() {
    match end.try_write(bytes)? {
      PipeWrite::Taken(n) => bytes = &bytes[n..],
      PipeWrite::WouldBlock => {
        if blocked == 0
          && let Some(tell) = first_block
        {
          let _ = tell.send(());
        }
        blocked += 1;
        end.writable().await?;
      }
    }
  }
  Ok(blocked)
}

/// A pattern of `count` bytes no two neighbouring windows of which repeat, so a byte out of order shows.
fn pattern(count: usize, seed: u8) -> Vec<u8> {
  (0..count)
    .map(|index| {
      u8::try_from(index & 0xff)
        .unwrap()
        .wrapping_mul(31)
        .wrapping_add(seed)
    })
    .collect()
}

/// A-179: do adopt the runtime's end of a pair in a task, have the peer write a request in three parts, and answer it;
/// then have the peer close. Expect the task to read the request whole and in order, the peer to read the answer whole,
/// and the task to see the end of the stream once the peer has gone.
#[test]
fn a_task_reads_and_writes_a_pipe_end_through_the_driver() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (ours, peer) = pair();
  let request = pattern(3 * 1000, 7);
  let answer = pattern(5000, 11);
  let (expected, reply) = (request.clone(), answer.clone());
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      let (got, ended) = read_all(&end, expected.len(), SIDE_BYTES).await?;
      write_all(&end, &reply, None).await?;
      let (rest, ended_after) = read_all(&end, 1, SIDE_BYTES).await?;
      Ok::<_, RtError>((got, ended, rest, ended_after))
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  for part in request.chunks(1000) {
    peer_write(&peer, part);
  }
  assert_eq!(
    peer_read(&peer, answer.len()),
    answer,
    "the peer read the answer whole and in order"
  );
  drop(peer);
  let (got, ended, rest, ended_after) = rx.recv_timeout(WAIT).expect("the task answered").unwrap();
  assert_eq!(got, request, "the task read the request whole and in order");
  assert!(!ended, "the stream had not ended while the peer was there");
  assert!(
    rest.is_empty() && ended_after,
    "the task saw the end of the stream once the peer had gone"
  );
  rt.shutdown().unwrap();
}

/// A-179: do write ten times the pipe's buffers to a peer that reads only once the writer has had to wait; expect a
/// try to have found the write in the kernel (the would-block path ran: the non-vacuity check), writability to resume
/// it, and every byte to arrive in order.
#[test]
fn a_write_to_a_reader_that_waits_blocks_and_resumes() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (ours, peer) = pair();
  let bytes = pattern(10 * PIPE_BUFFER as usize, 3);
  let sent = bytes.clone();
  let (tx, rx) = channel();
  let (blocked_tx, blocked_rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      write_all(&end, &sent, Some(&blocked_tx)).await
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  blocked_rx
    .recv_timeout(WAIT)
    .expect("the writer had to wait for the reader");
  assert_eq!(
    peer_read(&peer, bytes.len()),
    bytes,
    "every byte arrived in order"
  );
  let blocked = rx.recv_timeout(WAIT).expect("the task answered").unwrap();
  assert!(
    blocked > 0,
    "a try found the write in the kernel and waited (the would-block path ran)"
  );
  rt.shutdown().unwrap();
}

/// A-179: do close the peer's end before the task reads or writes; expect the read to find the end of the stream and
/// a write to be refused as the peer gone (ERROR_BROKEN_PIPE or ERROR_NO_DATA), as `EPIPE` is on Unix.
#[test]
fn a_peer_that_goes_ends_the_stream_and_a_write_says_so() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (ours, peer) = pair();
  drop(peer);
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      let (got, ended) = read_all(&end, 1, SIDE_BYTES).await?;
      let written = write_all(&end, b"anyone?", None).await;
      Ok::<_, RtError>((got, ended, written))
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  let (got, ended, written) = rx.recv_timeout(WAIT).expect("the task answered").unwrap();
  assert!(
    got.is_empty() && ended,
    "the read found the end of the stream"
  );
  match written {
    Err(RtError::DriverRefused {
      code: Some(code), ..
    }) => {
      assert!(
        code == 109 || code == 232,
        "refused as the peer gone, not {code}"
      )
    }
    other => panic!("a write to a gone peer must be refused: {other:?}"),
  }
  rt.shutdown().unwrap();
}

/// A-179: do take 64 KiB a byte at a time; expect every byte in order (a finished read handed out in parts).
#[test]
fn a_read_taken_a_byte_at_a_time_hands_out_every_byte_in_order() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (ours, peer) = pair();
  let bytes = pattern(64 * 1024, 5);
  let count = bytes.len();
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      read_all(&end, count, 1).await
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  peer_write(&peer, &bytes);
  let (got, _) = rx.recv_timeout(WAIT).expect("the task answered").unwrap();
  assert_eq!(got, bytes, "every byte, in order");
  rt.shutdown().unwrap();
}

/// A-179: do drop an end while its read is in the kernel, then adopt a new pair's end (its handle may reuse the
/// value) and run a round trip over it. Expect the old peer to see the pipe broken, the cancelled read's completion to
/// free its block without waking anyone or touching the new end, and the new round trip to complete whole.
#[test]
fn an_end_dropped_with_a_read_in_the_kernel_leaves_the_next_end_whole() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (ours, old_peer) = pair();
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      let mut buffer = [0u8; 16];
      let first = end.try_read(&mut buffer)?;
      drop(end);
      Ok::<_, RtError>(first)
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  let first = rx.recv_timeout(WAIT).expect("the task answered").unwrap();
  assert_eq!(
    first,
    PipeRead::WouldBlock,
    "the read was in the kernel when the end was dropped"
  );
  let mut probe = [0u8; 1];
  let mut read = 0u32;
  // SAFETY: the old peer's synchronous handle, a buffer and its length.
  let ok = unsafe {
    ReadFile(
      old_peer.as_raw_handle(),
      probe.as_mut_ptr(),
      1,
      &mut read,
      std::ptr::null_mut(),
    )
  };
  assert_eq!(
    ok, 0,
    "the old peer's read fails once the runtime's end is gone"
  );
  let (ours, peer) = pair();
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      let (got, _) = read_all(&end, 4, SIDE_BYTES).await?;
      write_all(&end, &got, None).await?;
      Ok::<_, RtError>(got)
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  peer_write(&peer, b"ping");
  assert_eq!(peer_read(&peer, 4), b"ping", "the new end echoed whole");
  assert_eq!(
    rx.recv_timeout(WAIT).expect("the task answered").unwrap(),
    b"ping"
  );
  rt.shutdown().unwrap();
}

/// A-179: do disconnect the runtime's end while its read is in the kernel, as a detach ends a stream. Expect the
/// task's awaited read to wake and find the end of the stream, and the peer's next read to fail (it is cut off), with
/// the runtime's end still open until the task drops it.
#[test]
fn a_disconnected_end_wakes_its_reader_with_the_end_of_the_stream() {
  let rt = Runtime::start(&config()).unwrap();
  let id = rt.shard_ids()[0];
  let (ours, peer) = pair();
  let (tx, rx) = channel();
  rt.spawn_on(id, async move {
    let outcome = async {
      let end = PipeEnd::adopt(ours, SIDE_BYTES, SIDE_BYTES)?;
      let mut buffer = [0u8; 16];
      assert_eq!(
        end.try_read(&mut buffer)?,
        PipeRead::WouldBlock,
        "a read is in the kernel"
      );
      slates_rt::futures::sleep(20_000_000).await?;
      end.disconnect()?;
      read_all(&end, 1, SIDE_BYTES).await
    }
    .await;
    let _ = tx.send(outcome);
  })
  .unwrap();
  let (got, ended) = rx.recv_timeout(WAIT).expect("the task answered").unwrap();
  assert!(
    got.is_empty() && ended,
    "the reader woke to the end of the stream"
  );
  let mut probe = [0u8; 1];
  let mut read = 0u32;
  // SAFETY: the peer's synchronous handle, a buffer and its length.
  let ok = unsafe {
    ReadFile(
      peer.as_raw_handle(),
      probe.as_mut_ptr(),
      1,
      &mut read,
      std::ptr::null_mut(),
    )
  };
  assert_eq!(ok, 0, "the peer is cut off");
  rt.shutdown().unwrap();
}
