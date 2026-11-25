//! A text's place held as a revlog chunk (goal 93, main's ruling (b) of 2026-11-25: content kept once, on arrival, in the
//! form a stream sends it): read through the process's chunk decoder by the form its row's mark names, never by the
//! chunk's first byte, so a raw text that begins with a chunk type's byte reads as it is. The directories are real ones in
//! the build output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1).
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::staging::Staging;
use slates_store::store::{Contents, Sizing, Store};
use slates_store::texts::{self, Form, Place, TextCache, TextEntry};

fn scratch(name: &str) -> PathBuf {
  let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
    .join(format!("chunk-form-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

fn open(dir: &Path) -> Store {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  Store::open(
    StoreRoot::open(dir, &grant).unwrap(),
    Sizing {
      memory: 8 << 20,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap()
}

/// The daemon's chunk decoder, as `slates-server` installs it: Mercurial's own reader of a revlog chunk.
fn decoder(stored: &[u8], most: u64) -> Option<Vec<u8>> {
  slates_hg::revlog_chunk::decode_within(stored, most)
}

/// `plain` as a revlog keeps it (zstd where it pays, else raw or `u`).
fn chunk(plain: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  slates_hg::revlog_chunk::chunk(plain, slates_hg::revlog_chunk::Pass::Send, &mut out).unwrap();
  out
}

/// A hunk replacing `start..end` of the base with `data` (Mercurial's `mpatch` form).
fn hunk(start: u32, end: u32, data: &[u8]) -> Vec<u8> {
  [
    start.to_be_bytes().as_slice(),
    &end.to_be_bytes(),
    &(data.len() as u32).to_be_bytes(),
    data,
  ]
  .concat()
}

/// Lands changeset `sequence` of `repository`: `stored` its payload, one text of `len` bytes named `text` on `path`,
/// its whole or its edit on `base` held in `form`.
fn land(
  store: &mut Store,
  (repository, sequence): (u128, u64),
  (path, text, len): (&[u8], [u8; 32], u64),
  (base, stored, form): (Option<[u8; 32]>, &[u8], Form),
) {
  let section = texts::encode(&[TextEntry {
    path: path.to_vec(),
    tool_id: Vec::new(),
    text,
    len,
    base,
    place: Place {
      payload: *blake3::hash(stored).as_bytes(),
      offset: 0,
      len: stored.len() as u64,
    },
    form,
  }]);
  store
    .land(
      Header {
        tool: Tool::Mercurial,
        repository,
        sequence,
        judged_over: sequence - 1,
        grant: 1,
        work: sequence,
        time_ns: sequence,
        sections: 0,
      },
      &[
        (SectionKind::Record, Contents::Bytes(b"heads")),
        (SectionKind::Payload, Contents::Bytes(stored)),
        (SectionKind::Texts, Contents::Bytes(&section)),
      ],
      &Staging::new(1 << 20),
    )
    .unwrap();
}

/// Goal 93, main's ruling (b), red 1 (the store's reader). Do: land one text and an edit of it in repository 1 raw and in
/// repository 2 as revlog chunks (the whole compressed, the edit compressed); make each repository's edited text. Expect:
/// the same bytes both ways, the expected text; the chunked ones decoded (non-vacuity: two decodes), the most bytes one
/// decode made the whole's length.
#[test]
fn a_texts_chunked_and_raw_rows_make_the_same_bytes() {
  let _ = texts::install_chunk_decoder(decoder);
  let dir = scratch("same");
  let mut store = open(&dir);
  let whole: Vec<u8> = (0..4_000u32)
    .map(|at| b"abcdefgh"[(at % 8) as usize])
    .collect();
  let data = [b'z'; 80];
  let edit = hunk(100, 180, &data);
  let mut edited = whole.clone();
  edited[100..180].copy_from_slice(&data);
  let (whole_id, edited_id) = (
    *blake3::hash(&whole).as_bytes(),
    *blake3::hash(&edited).as_bytes(),
  );
  for (repository, form, stored_whole, stored_edit) in [
    (1u128, Form::Raw, whole.clone(), edit.clone()),
    (2u128, Form::Chunk, chunk(&whole), chunk(&edit)),
  ] {
    land(
      &mut store,
      (repository, 1),
      (b"f", whole_id, whole.len() as u64),
      (None, &stored_whole, form),
    );
    land(
      &mut store,
      (repository, 2),
      (b"f", edited_id, edited.len() as u64),
      (Some(whole_id), &stored_edit, form),
    );
  }
  let staging = Staging::new(0);
  let mut read = |repository: u128| {
    let mut cache = TextCache::new(1 << 20);
    let text = store
      .text_at(&staging, repository, &edited_id, 0, &mut cache)
      .unwrap()
      .unwrap()
      .to_vec();
    (text, cache.decoded, cache.decoded_most)
  };
  let (raw, raw_decoded, _) = read(1);
  let (chunked, decoded, decoded_most) = read(2);
  assert_eq!(raw, edited, "the raw rows make the text");
  assert_eq!(chunked, edited, "the chunked rows make the same text");
  assert_eq!(
    (raw_decoded, decoded),
    (0, 2),
    "the chunked whole and edit decoded"
  );
  assert_eq!(decoded_most, whole.len() as u64);
  let _ = std::fs::remove_dir_all(&dir);
}

/// Goal 93, main's ruling (b), red 1's trap (main's brief corrected: a first byte cannot name an old raw row's form).
/// Do: with the decoder installed, land raw texts that begin with each byte a chunk's type is (`u`, `x`, `(`, NUL) and
/// read each. Expect: each read as it is, nothing decoded.
#[test]
fn a_raw_text_beginning_with_a_chunk_types_byte_reads_as_it_is() {
  let _ = texts::install_chunk_decoder(decoder);
  let dir = scratch("trap");
  let mut store = open(&dir);
  let staging = Staging::new(0);
  for (sequence, first) in [b'u', b'x', b'(', 0u8].into_iter().enumerate() {
    let text: Vec<u8> = [first]
      .into_iter()
      .chain((0..200u32).map(|at| (at % 7) as u8 + b'a'))
      .collect();
    let identity = *blake3::hash(&text).as_bytes();
    let sequence = sequence as u64 + 1;
    land(
      &mut store,
      (1, sequence),
      (b"t", identity, text.len() as u64),
      (None, &text, Form::Raw),
    );
    let mut cache = TextCache::new(1 << 20);
    let read = store
      .text_at(&staging, 1, &identity, 0, &mut cache)
      .unwrap()
      .unwrap()
      .to_vec();
    assert_eq!(
      read, text,
      "a raw text beginning {first:#04x} read as it is"
    );
    assert_eq!(cache.decoded, 0);
  }
  let _ = std::fs::remove_dir_all(&dir);
}
