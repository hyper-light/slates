//! The `Texts` section's encoding and the edit form, held to their module docs.

use super::{Place, TextEntry, TextsRefused, apply_into, decode, encode};

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
    form: super::Form::Raw,
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
