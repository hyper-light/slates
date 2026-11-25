//! The store owner's text cache as its asker sees it (goal 93, main's ruling of 2026-11-25: the memory status's
//! `store.text_cache_*` rows).
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::print_stdout
)]

#[path = "support/owner_harness.rs"]
mod owner_harness;

use owner_harness::{runtime, start};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::texts::{self, Place, TextEntry};

/// Shape: a text's length, past several of the owner's pieces.
const TEXT: usize = 300 << 10;
/// Shape: a read's slice, under the text.
const SLICE: u64 = 64 << 10;
/// Shape: the repository the test lands in.
const REPOSITORY: u128 = 11;

/// A fresh scratch directory for test `name`.
fn scratch(name: &str) -> std::path::PathBuf {
  owner_harness::scratch(BINARY, name)
}

/// Shape: this binary's name, in its scratch directories'.
const BINARY: &str = "owner-text-cache";

/// Shape: the texts the walk test lands and leaves part-way, each [`TEXT`] bytes.
const LEFT_TEXTS: usize = 3;

/// The texts of `count` distinct payloads of [`TEXT`] bytes landed as changeset `sequence`'s, with their identities.
fn left_texts(count: usize) -> (Vec<[u8; 32]>, Vec<(SectionKind, Vec<u8>)>) {
  let mut identities = Vec::new();
  let mut entries = Vec::new();
  let mut payload = Vec::new();
  for at in 0..count {
    let text: Vec<u8> = (0..TEXT).map(|byte| ((byte + at) % 239) as u8).collect();
    let identity = *blake3::hash(&text).as_bytes();
    entries.push(TextEntry {
      path: format!("left-{at}.txt").into_bytes(),
      tool_id: Vec::new(),
      text: identity,
      len: text.len() as u64,
      base: None,
      place: Place {
        payload: identity,
        offset: 0,
        len: text.len() as u64,
      },
      form: texts::Form::Raw,
    });
    identities.push(identity);
    payload.push((SectionKind::Payload, text));
  }
  let mut sections = vec![(SectionKind::Record, b"heads".to_vec())];
  sections.extend(payload);
  sections.push((SectionKind::Texts, texts::encode(&entries)));
  (identities, sections)
}

/// Goal 93 (main's ruling of 2026-11-25: after hg's 57k push the text cache's chunks held 32 MB live, `texts.rs:708`, and
/// nothing reported its reserved bytes, bound or walks). Do: land [`LEFT_TEXTS`] texts of [`TEXT`] bytes; through the
/// owner, read each one's first slice of [`SLICE`] and leave it, as a caller gone mid-walk, and note what the owner's
/// cache was as the last read left it (`owner::text_cache_seen`); then read each text to its end and note it again. Expect the figures to reach the asker (non-vacuity of the memory status's rows): after the reads left
/// part-way, every one a walk in flight, their texts' bytes walked and the bound holding them; once each is read to
/// its end, no walk in flight. The lead it shows: a read its caller leaves part-way holds the cache's bound up until
/// more walks than the owner's room for waiting reads evict it.
#[test]
fn the_owners_text_cache_is_seen_by_its_asker_and_a_read_left_partway_holds_its_bound() {
  let dir = scratch("left");
  let (identities, sections) = left_texts(LEFT_TEXTS);
  let owner = start(&dir);
  let handle = owner.handle();
  let (left, ended) = runtime()
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
      for identity in &identities {
        handle
          .text_slice_into(REPOSITORY, *identity, (0, SLICE), Vec::new())
          .await
          .unwrap()
          .unwrap();
      }
      let left = slates_store::owner::text_cache_seen();
      for identity in &identities {
        let _ = handle
          .text_slice_into(REPOSITORY, *identity, (SLICE, TEXT as u64), Vec::new())
          .await;
      }
      (left, slates_store::owner::text_cache_seen())
    })
    .unwrap();
  owner.stop().unwrap();
  println!(
    "owner_text_cache: {LEFT_TEXTS} reads left part-way: {left:?}; each read to its end: {ended:?}"
  );
  assert_eq!(
    left.walks, LEFT_TEXTS as u64,
    "each read left part-way a walk in flight"
  );
  assert_eq!(
    left.walk_bytes,
    (LEFT_TEXTS * TEXT) as u64,
    "their texts walked"
  );
  assert!(
    left.bound >= left.walk_bytes,
    "the bound holding them: {left:?}"
  );
  assert_eq!(
    ended.walks, 0,
    "the walks read to their end gone: {ended:?}"
  );
}
