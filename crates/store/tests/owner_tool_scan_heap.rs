//! A store owner's ask of versions by tool id, its allocations read across it (goal 93, main's ruling of 2026-11-25:
//! hg's 57k clone's scan churn). Alone in its binary: the allocator's counters are the process's.
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::print_stdout
)]

use slates_mem::large::{LargeMapped, heap};

#[path = "support/owner_harness.rs"]
mod owner_harness;

use owner_harness::{runtime, start};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::texts::{self, Place, TextEntry};

/// Shape: the repository the test lands in.
const REPOSITORY: u128 = 11;

/// A fresh scratch directory for test `name`.
fn scratch(name: &str) -> std::path::PathBuf {
  owner_harness::scratch(BINARY, name)
}

/// Shape: this binary's name, in its scratch directories'.
const BINARY: &str = "owner-tool-scan";

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: the versions of one path the tool-id scan test lands, each named by its tool id.
const TOOL_VERSIONS: usize = 2_000;
/// Shape: the asks the tool-id scan test measures, after a first that warms the store.
const TOOL_ASKS: usize = 8;

/// The texts of [`TOOL_VERSIONS`] versions of `f`, each named by its tool id, landed in one payload.
fn tool_versions() -> (Vec<Vec<u8>>, Vec<(SectionKind, Vec<u8>)>) {
  let mut ids = Vec::new();
  let mut entries = Vec::new();
  let mut payload = Vec::new();
  for at in 0..TOOL_VERSIONS {
    let text = format!("version {at} of f\n").into_bytes();
    let identity = *blake3::hash(&text).as_bytes();
    let id = format!("n{at:05}").into_bytes();
    entries.push(TextEntry {
      path: b"f".to_vec(),
      tool_id: id.clone(),
      text: identity,
      len: text.len() as u64,
      base: None,
      place: Place {
        payload: *blake3::hash(b"tool-payload").as_bytes(),
        offset: payload.len() as u64,
        len: text.len() as u64,
      },
      form: texts::Form::Raw,
    });
    payload.extend_from_slice(&text);
    ids.push(id);
  }
  let identity = *blake3::hash(&payload).as_bytes();
  for entry in &mut entries {
    entry.place.payload = identity;
  }
  let sections = vec![
    (SectionKind::Record, b"heads".to_vec()),
    (SectionKind::Payload, payload),
    (SectionKind::Texts, texts::encode(&entries)),
  ];
  (ids, sections)
}

/// Goal 93 (main's ruling of 2026-11-25: hg's 57k clone's system churn was 3.72 GB, 55.8% of it `reserve` under the
/// engine's `scan_inner`, beside `texts_by_tools`' own sites). Do: land [`TOOL_VERSIONS`] versions of one path named by
/// tool id; through the owner, ask for every one by its tool id once, then [`TOOL_ASKS`] times more, each ask built
/// before the reading, and read the process's allocated bytes across those asks. Expect every version found, and each
/// ask allocating no more than its answer and the ask's own maps: the scan's rows laid in buffers the store keeps, no
/// row's tool id copied. Red before: each scan's rows in fresh buffers grown from nothing, each row's tool id copied
/// twice.
#[test]
fn an_ask_by_tool_id_scans_into_the_stores_own_buffers() {
  let dir = scratch("tools");
  let (ids, sections) = tool_versions();
  let owner = start(&dir);
  let handle = owner.handle();
  let asks: Vec<Vec<(Vec<u8>, Vec<Vec<u8>>)>> = (0..=TOOL_ASKS)
    .map(|_| vec![(b"f".to_vec(), ids.clone())])
    .collect();
  let (found, allocated) = runtime()
    .block_on(async move {
      handle
        .land(
          Header {
            tool: Tool::Mercurial,
            repository: REPOSITORY,
            sequence: 1,
            judged_over: 0,
            grant: 1,
            work: 1,
            time_ns: 1,
            sections: 0,
          },
          sections,
        )
        .await
        .unwrap();
      let mut asks = asks.into_iter();
      let warm = handle
        .texts_by_tool(REPOSITORY, asks.next().unwrap())
        .await
        .unwrap();
      let before = heap().system_allocated;
      let mut found = 0;
      for ask in asks {
        let answered = handle.texts_by_tool(REPOSITORY, ask).await.unwrap();
        found = answered[0].iter().filter(|row| row.is_some()).count();
        drop(answered);
      }
      drop(warm);
      (found, (heap().system_allocated - before) / TOOL_ASKS as u64)
    })
    .unwrap();
  owner.stop().unwrap();
  let answer =
    (TOOL_VERSIONS * size_of::<Option<([u8; 32], slates_store::texts::TextRow)>>()) as u64;
  println!(
    "owner_tool_scan_heap: an ask of {TOOL_VERSIONS} versions by tool id allocated {allocated} B (its answer {answer} B)"
  );
  assert_eq!(found, TOOL_VERSIONS, "every version found");
  // Derived: the answer, and the asks' order beside it (a place, 8 bytes, against an answer's 136 an id), with the
  // request's own few buffers: under half as much again. Before, 5.2 times it.
  assert!(
    allocated * 2 < answer * 3,
    "an ask allocated {allocated} B for an answer of {answer} B"
  );
}
