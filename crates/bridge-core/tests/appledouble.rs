//! The AppleDouble codec (§4.6 "Extended attributes over NFSv3") against the sidecars macOS 26.4.1
//! itself wrote (`tests/vectors/appledouble/README.md`): each decodes to the attributes the commands
//! set; re-encoding reproduces the file byte for byte where macOS's entry order is byte order, and
//! reproduces the same attributes everywhere; a rendered range equals the same range of the whole
//! file; and hostile bytes (every truncation, a bit flip in every header byte, lengths past the file)
//! never decode to a layout reaching outside the file.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::appledouble::{
  Decoded, Encoding, Entry, FINDER_INFO_NAME, Layout, MAX_HEADER_BYTES, Span, decode, encode,
};

/// A file offset as an index (the vectors are a few KiB).
fn at(offset: u64) -> usize {
  usize::try_from(offset).unwrap()
}

fn vector(name: &str) -> Vec<u8> {
  let path = format!(
    "{}/tests/vectors/appledouble/{name}",
    env!("CARGO_MANIFEST_DIR")
  );
  #[allow(clippy::disallowed_methods)] // reading a checked-in test vector
  std::fs::read(path).unwrap()
}

fn decode_all(file: &[u8]) -> Decoded {
  let len = file.len() as u64;
  let prefix = &file[..file.len().min(MAX_HEADER_BYTES)];
  decode(prefix, len, &mut |at, buf: &mut [u8]| {
    let start = usize::try_from(at).unwrap().min(file.len());
    let end = (start + buf.len()).min(file.len());
    buf[..end - start].copy_from_slice(&file[start..end]);
    end - start
  })
}

fn layout(file: &[u8]) -> Layout {
  match decode_all(file) {
    Decoded::Complete(layout) => layout,
    Decoded::Incomplete => panic!("the vector decodes"),
  }
}

/// `(name, value)` pairs of a decoded file, sorted by name.
fn attributes(file: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
  let mut pairs: Vec<_> = layout(file)
    .attributes
    .into_iter()
    .map(|(name, span)| {
      let value = file[at(span.offset)..at(span.end())].to_vec();
      (name, value)
    })
    .collect();
  pairs.sort();
  pairs
}

/// The whole file an encoding renders for these attributes.
fn render_all(encoding: &Encoding, values: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
  let mut out = vec![0u8; usize::try_from(encoding.len).unwrap()];
  let n = encoding.render(0, &mut out, &mut |index, at, buf| {
    let value = &values[index].1;
    buf.copy_from_slice(&value[self::at(at)..self::at(at) + buf.len()]);
  });
  assert_eq!(n, out.len());
  out
}

fn entries(values: &[(Vec<u8>, Vec<u8>)]) -> Vec<Entry> {
  values
    .iter()
    .map(|(name, value)| Entry {
      name: name.clone(),
      len: value.len() as u64,
    })
    .collect()
}

fn named(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<(&[u8], &[u8])> {
  pairs
    .iter()
    .map(|(name, value)| (name.as_slice(), value.as_slice()))
    .collect()
}

/// §4.6: do decode each captured sidecar; expect exactly the attributes the recorded commands set.
#[test]
fn every_captured_sidecar_decodes_to_the_attributes_macos_set() {
  let provenance = attributes(&vector("one-attr.bin"))[0].1.clone();
  assert_eq!(provenance.len(), 11);
  let prov: &[u8] = b"com.apple.provenance";
  assert_eq!(
    named(&attributes(&vector("one-attr.bin"))),
    vec![
      (prov, provenance.as_slice()),
      (&b"com.example.probe"[..], &b"value1"[..])
    ]
  );
  assert_eq!(
    named(&attributes(&vector("two-attrs.bin"))),
    vec![
      (prov, provenance.as_slice()),
      (&b"com.example.probe"[..], &b"value1"[..]),
      (&b"user.second"[..], &b"a longer second value"[..])
    ]
  );
  assert_eq!(
    named(&attributes(&vector("after-remove.bin"))),
    vec![
      (prov, provenance.as_slice()),
      (&b"user.second"[..], &b"a longer second value"[..])
    ]
  );
  let mut finder = vec![0u8; 32];
  finder[9] = 0x10;
  assert_eq!(
    named(&attributes(&vector("finderinfo.bin"))),
    vec![
      (FINDER_INFO_NAME, finder.as_slice()),
      (prov, provenance.as_slice()),
      (&b"user.second"[..], &b"a longer second value"[..])
    ]
  );
  assert_eq!(
    named(&attributes(&vector("dir-exclude.bin"))),
    vec![
      (
        &b"com.apple.metadata:com_apple_backup_excludeItem"[..],
        &b"com.apple.backupd"[..]
      ),
      (prov, provenance.as_slice())
    ]
  );
}

/// §4.6 (golden vectors): do re-encode the attributes each capture holds; expect the file macOS wrote
/// byte for byte where its entry order is byte order (four of five), and the same attributes after a
/// decode of every re-encoding.
#[test]
fn re_encoding_reproduces_what_macos_wrote() {
  for name in [
    "one-attr.bin",
    "two-attrs.bin",
    "after-remove.bin",
    "finderinfo.bin",
    "dir-exclude.bin",
  ] {
    let original = vector(name);
    let values = attributes(&original);
    let encoding = encode(&entries(&values));
    assert_eq!(
      encoding.held.len(),
      values.len(),
      "{name}: every attribute held"
    );
    let rendered = render_all(&encoding, &values);
    assert_eq!(attributes(&rendered), values, "{name}: the same attributes");
    if name != "dir-exclude.bin" {
      assert_eq!(rendered, original, "{name}: byte for byte");
    }
  }
}

/// §4.6: do render every range of an encoding holding a large resource fork; expect each to equal the
/// same range of the whole rendered file, and the file to decode to the same attributes.
#[test]
fn any_rendered_range_equals_that_range_of_the_whole_file() {
  let fork: Vec<u8> = (0..10_000u32).map(|i| i.to_le_bytes()[0]).collect();
  let values = vec![
    (b"com.apple.ResourceFork".to_vec(), fork),
    (b"user.a".to_vec(), b"alpha".to_vec()),
    (FINDER_INFO_NAME.to_vec(), vec![7u8; 32]),
  ];
  let encoding = encode(&entries(&values));
  let whole = render_all(&encoding, &values);
  assert_eq!(attributes(&whole), {
    let mut sorted = values.clone();
    sorted.sort();
    sorted
  });
  for (off, len) in [
    (0u64, 17usize),
    (40, 100),
    (4090, 20),
    (4096, 5000),
    (whole.len() as u64 - 3, 10),
  ] {
    let mut out = vec![0u8; len];
    let n = encoding.render(off, &mut out, &mut |index, at, buf| {
      buf.copy_from_slice(&values[index].1[self::at(at)..self::at(at) + buf.len()]);
    });
    let start = self::at(off);
    assert_eq!(&out[..n], &whole[start..(start + len).min(whole.len())]);
  }
}

/// Hostile input (CLAUDE.md §4): do decode every truncation of a captured sidecar and every
/// single-bit flip of its first 256 bytes; expect no panic, and every layout that does decode to name
/// only spans inside the file.
#[test]
fn hostile_bytes_never_decode_to_spans_outside_the_file() {
  let original = vector("two-attrs.bin");
  let inside = |file: &[u8]| match decode_all(file) {
    Decoded::Complete(layout) => layout
      .attributes
      .iter()
      .all(|(_, span): &(Vec<u8>, Span)| span.end() <= file.len() as u64),
    Decoded::Incomplete => true,
  };
  for cut in 0..original.len() {
    assert!(inside(&original[..cut]), "truncated to {cut}");
    if cut < 0xee2 {
      assert_eq!(
        decode_all(&original[..cut]),
        Decoded::Incomplete,
        "truncated to {cut}"
      );
    }
  }
  for byte in 0..256 {
    for bit in 0..8 {
      let mut flipped = original.clone();
      flipped[byte] ^= 1 << bit;
      assert!(inside(&flipped), "bit {bit} of byte {byte}");
    }
  }
  let mut huge = original.clone();
  // The Finder Info entry's length field (entry 0: type at 0x1a, offset at 0x1e, length at 0x22).
  huge[0x22..0x26].copy_from_slice(&u32::MAX.to_be_bytes());
  assert_eq!(
    decode_all(&huge),
    Decoded::Incomplete,
    "a length past the file"
  );
  assert_eq!(decode_all(&[]), Decoded::Incomplete, "an empty file");
}
