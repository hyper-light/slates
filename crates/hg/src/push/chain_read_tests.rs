//! A manifest's chain read over its deltas, held to the text the deltas make applied one after another, on the stack a
//! shard runs on.

use slates_vcs::ContentStore;

use super::{Chain, Cursor, Reading};
use crate::delta::{self, BaseRead};
use crate::read::to_u64;

/// Shape: the stack a shard thread runs on: the runtime builds its threads with the standard library's default
/// (`slates_rt`'s `runtime.rs`, `std::thread::Builder` with no `stack_size`), 2 MiB [B: the standard library's
/// `std::thread` docs, "Stack size": "The default stack size is platform-dependent and subject to change. Currently, it
/// is 2 MiB on all Tier-1 platforms"].
const SHARD_STACK_BYTES: usize = 2 << 20;

/// Shape: the lines of the deep chain's whole text, each `line NNNNN\n`: a manifest of a thousand files.
const LINES: usize = 1_000;

/// Shape: the deep chain's deltas, each rewriting its first line, the rest never touched (as a manifest's other files
/// are by a line of one-file edits), so a read of those lines goes through every delta to the whole text: chained as the
/// push keeps them (bounded by bytes, never by links; main's order of 2026-11-25 after a 2,000-changeset push overflowed shard 0's
/// stack in a debug build).
const DEPTH: usize = 10_000;

/// Shape: the pieces the deep chain is read in: a byte, a few, a line's worth, a window.
const PIECES: [usize; 4] = [1, 7, 64, 4_096];

/// Format: the variable naming the role a re-invoked test binary plays ([`a_deep_chain_is_read_on_a_shards_stack`]).
const ROLE: &str = "SLATES_HG_CHAIN_READ_ROLE";

/// The deep chain's whole text, its deltas (each rewriting the first line to `edit k`), and the text they make.
fn deep_chain() -> (Vec<u8>, Vec<Vec<u8>>, Vec<u8>) {
  let root: Vec<u8> = (0..LINES)
    .flat_map(|line| format!("line {line:05}\n").into_bytes())
    .collect();
  let mut text = root.clone();
  let mut deltas = Vec::with_capacity(DEPTH);
  for k in 0..DEPTH {
    let line = 0;
    let start = text
      .split_inclusive(|byte| *byte == b'\n')
      .take(line)
      .map(<[u8]>::len)
      .sum::<usize>();
    let end = start
      + text[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap()
      + 1;
    let replaced = format!("edit {k:05}\n").into_bytes();
    let mut made = Vec::new();
    delta::encode_hunk(
      &delta::Hunk {
        start: u32::try_from(start).unwrap(),
        end: u32::try_from(end).unwrap(),
        data: &replaced,
      },
      &mut made,
    )
    .unwrap();
    text = delta::apply(&text, &made, &mut crate::Admission::new(u64::MAX)).unwrap();
    deltas.push(made);
  }
  (root, deltas, text)
}

/// The deep chain read whole through [`Reading`], in pieces of `piece` bytes front to back: what it read, none where a
/// read was refused.
fn read_deep(piece: usize) -> Option<(Vec<u8>, Vec<u8>)> {
  let (root, deltas, text) = deep_chain();
  let mut store = ContentStore::new(1 << 26, 1 << 12).unwrap();
  let object = store.put_object(&root).unwrap();
  let chain = Chain::of_deltas(
    &store,
    (object, to_u64(root.len())),
    deltas.iter().map(Vec::as_slice),
  )?;
  let mut cursor = Cursor::default();
  let mut reading = Reading {
    chain: &chain,
    cursor: &mut cursor,
    store: &store,
  };
  let mut read = Vec::new();
  let len = reading.len();
  let mut at = 0u64;
  while at < len {
    let n = usize::try_from(len - at).unwrap().min(piece);
    reading.read_into(at, n, &mut read)?;
    at += to_u64(n);
  }
  Some((read, text))
}

/// Main's order of 2026-11-25 (a 2,000-changeset push overflowed a debug shard's stack: the chain reader recursed a
/// frame a delta, its depth the user's history). Do: read a chain of [`DEPTH`] one-line deltas over a [`LINES`]-line
/// text through the reader, front to back in pieces of each of [`PIECES`] bytes, in a re-invoked test binary (an
/// overflow aborts its process, so the red fails this test, not the binary), on a thread with a shard's stack
/// ([`SHARD_STACK_BYTES`]). Expect: the child succeeds, each read the text the deltas make applied one after another.
/// Red before: the child aborted on the stack's overflow.
#[test]
fn a_deep_chain_is_read_on_a_shards_stack() {
  if std::env::var_os(ROLE).is_some() {
    for piece in PIECES {
      let read = std::thread::Builder::new()
        .stack_size(SHARD_STACK_BYTES)
        .spawn(move || read_deep(piece))
        .unwrap()
        .join()
        .unwrap();
      let (read, text) = read.expect("the chain read");
      assert_eq!(
        read, text,
        "the chain's text read in pieces of {piece} bytes"
      );
    }
    return;
  }
  let child = std::process::Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "push::chain_read::tests::a_deep_chain_is_read_on_a_shards_stack",
      "--nocapture",
      "--test-threads",
      "1",
    ])
    .env(ROLE, "1")
    .output()
    .unwrap();
  assert!(
    child.status.success(),
    "the read on a shard's stack: {:?}\n{}",
    child.status,
    String::from_utf8_lossy(&child.stderr)
  );
}
