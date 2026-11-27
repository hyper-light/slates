//! The `Texts` section's encoding and the edit form, held to their module docs.

use super::{
  Place, TextCache, TextEntry, TextRow, TextsRefused, apply_and_verify_chain_step, apply_into,
  decode, encode,
};
use crate::log::LogError;
use crate::store::StoreError;

fn entry(path: &[u8], base: Option<[u8; 32]>) -> TextEntry {
  TextEntry {
    path: path.to_vec(),
    tool_id: vec![0xab; 20],
    text: [1; 32],
    len: 11,
    base,
    place: Place {
      payload: [2; 32],
      offset: 7,
      len: 23,
    },
  }
}

/// A hunk replacing `start..end` of the base with `data`.
fn hunk(start: u32, end: u32, data: &[u8]) -> Vec<u8> {
  [
    start.to_be_bytes().as_slice(),
    &end.to_be_bytes(),
    &u32::try_from(data.len()).unwrap().to_be_bytes(),
    data,
  ]
  .concat()
}

/// A-645 on A-767 (the agreed `Texts` format). Do: encode a whole, an edit and one of Mercurial's own logs as paths;
/// read them back; read every truncation; change the mark to a byte of no kind; add a byte after the end. Expect: read
/// back as written, the bytes pinned at their first entry's head, every truncation refused, the mark refused by its
/// byte, the trailing byte refused.
#[test]
fn a_texts_section_reads_back_as_written_and_is_refused_by_kind() {
  let entries = vec![
    entry(b"README", None),
    entry(b"src/lib.rs", Some([3; 32])),
    entry(b"\0changelog", Some([4; 32])),
  ];
  let bytes = encode(&entries);
  assert_eq!(bytes.get(..8), Some(3u64.to_le_bytes().as_slice()));
  assert_eq!(bytes.get(8..16), Some(6u64.to_le_bytes().as_slice()));
  assert_eq!(bytes.get(16..22), Some(b"README".as_slice()));
  assert_eq!(decode(&bytes), Ok(entries.clone()));
  for end in 0..bytes.len() {
    assert!(decode(&bytes[..end]).is_err(), "cut at {end}");
  }
  let mark = 8 + 8 + 6 + 8 + 20 + 32 + 8;
  let mut bad = bytes.clone();
  bad[mark] = 9;
  assert_eq!(decode(&bad), Err(TextsRefused::Mark(9)));
  let mut trailing = bytes;
  trailing.push(0);
  assert_eq!(decode(&trailing), Err(TextsRefused::Trailing));
  assert_eq!(decode(&u64::MAX.to_le_bytes()), Err(TextsRefused::Count));
}

/// The one edit form (Mercurial's hunks). Do: apply an edit of two hunks, an empty edit, and edits whose hunk is out of
/// order, past the base, cut short, or that make another length than asked. Expect: the text, the base itself, and each
/// refusal by kind with nothing written.
#[test]
fn an_edit_makes_its_text_or_is_refused_before_writing() {
  let base = b"hello, quiet world";
  let edit = [hunk(0, 5, b"HELLO"), hunk(7, 12, b"loud")].concat();
  let mut out = b"left over".to_vec();
  apply_into(base, &edit, 17, &mut out).unwrap();
  assert_eq!(out, b"HELLO, loud world");
  apply_into(base, &[], 18, &mut out).unwrap();
  assert_eq!(out, base);
  let mut untouched = b"kept".to_vec();
  for (edit, len, refused) in [
    (
      [hunk(7, 12, b""), hunk(0, 5, b"")].concat(),
      8,
      TextsRefused::Hunk,
    ),
    (hunk(10, 30, b""), 10, TextsRefused::Hunk),
    (hunk(0, 1, b"x")[..12].to_vec(), 18, TextsRefused::Hunk),
    (hunk(0, 5, b"HELLO"), 99, TextsRefused::Length),
  ] {
    assert_eq!(apply_into(base, &edit, len, &mut untouched), Err(refused));
    assert_eq!(untouched, b"kept");
  }
}

/// Round-4 A5 (`make_chain` BLAKE3 check): Do: run `apply_and_verify_chain_step` on both a whole
/// text and a delta edit whose materialized bytes have a 1-bit flip relative to `identity`, then
/// run it with the matching bytes.
/// Expect: mismatched bytes return `Err(StoreError::Log(LogError::Corrupt("a text's bytes")))` and
/// NEVER poison `TextCache` (`cache.get(&identity).is_none()`, `cache.made == 0`); matching bytes
/// succeed and populate `TextCache`.
#[test]
fn make_chain_step_verifies_blake3_before_caching_and_never_poisons_text_cache() {
  let whole = b"authentic mercurial full text".to_vec();
  let whole_id = *blake3::hash(&whole).as_bytes();
  let mut bad_whole = whole.clone();
  bad_whole[5] ^= 0x01;
  let dummy_place = Place {
    payload: [0; 32],
    offset: 0,
    len: whole.len() as u64,
  };
  let whole_row = TextRow {
    sequence: 1,
    len: whole.len() as u64,
    base: None,
    place: dummy_place,
  };
  let mut cache = TextCache::new(1 << 20);
  let mut next = Vec::new();
  let err = apply_and_verify_chain_step(
    &whole_id,
    &whole_row,
    &bad_whole,
    &[],
    &mut next,
    &mut cache,
  );
  assert!(matches!(
    err,
    Err(StoreError::Log(LogError::Corrupt("a text's bytes")))
  ));
  assert_eq!((cache.get(&whole_id), cache.made), (None, 0));

  apply_and_verify_chain_step(&whole_id, &whole_row, &whole, &[], &mut next, &mut cache).unwrap();
  assert_eq!(
    (cache.get(&whole_id), cache.made),
    (Some(whole.as_slice()), 1)
  );

  let edited_expected = b"authentic subversion full text".to_vec();
  let edited_id = *blake3::hash(&edited_expected).as_bytes();
  let bad_edit = hunk(10, 19, b"subversioX");
  let edit_row = TextRow {
    sequence: 2,
    len: edited_expected.len() as u64,
    base: Some(whole_id),
    place: dummy_place,
  };
  let edit_err = apply_and_verify_chain_step(
    &edited_id, &edit_row, &bad_edit, &whole, &mut next, &mut cache,
  );
  assert!(matches!(
    edit_err,
    Err(StoreError::Log(LogError::Corrupt("a text's bytes")))
  ));
  assert_eq!((cache.get(&edited_id), cache.made), (None, 1));
}
