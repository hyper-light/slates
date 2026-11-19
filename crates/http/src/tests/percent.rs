use super::{BadEscape, Plus, decode_into, decoded_len, decodes_to, form_fields, names_any};

/// What `raw` decodes to read with `plus`.
fn decoded(raw: &[u8], plus: Plus) -> Result<Vec<u8>, BadEscape> {
  let mut out = Vec::new();
  decode_into(raw, plus, &mut out).map(|()| out)
}

/// A-595: do decode escapes of either case among plain bytes, a `+` each way, and malformed escapes. Expect each escape
/// read as its byte, `+` a space in a form alone, and a cut or non-hexadecimal escape refused with nothing appended.
#[test]
fn escapes_decode_to_their_bytes_and_malformed_ones_are_refused() {
  assert_eq!(decoded(b"%41b%2f%2F%00c", Plus::Plus), Ok(b"Ab//\0c".to_vec()));
  assert_eq!(decoded(b"a+b%2B", Plus::Plus), Ok(b"a+b+".to_vec()));
  assert_eq!(decoded(b"a+b%2B", Plus::Space), Ok(b"a b+".to_vec()));
  assert_eq!(decoded(b"", Plus::Space), Ok(Vec::new()));
  let cases: [(&[u8], BadEscape); 5] = [
    (b"%", BadEscape::Truncated),
    (b"abc%4", BadEscape::Truncated),
    (b"%G1", BadEscape::NotHex),
    (b"%1G", BadEscape::NotHex),
    (b"%+1", BadEscape::NotHex),
  ];
  for (raw, refusal) in cases {
    let mut out = b"kept".to_vec();
    assert_eq!(decode_into(raw, Plus::Plus, &mut out), Err(refusal), "{raw:?}");
    assert_eq!(out, b"kept", "{raw:?}");
    assert_eq!(decoded_len(raw), Err(refusal), "{raw:?}");
  }
}

/// A-595: do count and compare decodings without building them. Expect the count to equal the decoding's length and the
/// comparison to equal comparing the decoding.
#[test]
fn a_decoding_is_counted_and_compared_without_being_built() {
  for raw in [&b"git-upload-pack"[..], b"git%2Dupload%2dpack", b"%67it+x", b""] {
    assert_eq!(decoded_len(raw), Ok(decoded(raw, Plus::Plus).unwrap().len()), "{raw:?}");
  }
  assert_eq!(decodes_to(b"git%2Dupload-pack", Plus::Space, b"git-upload-pack"), Ok(true));
  assert_eq!(decodes_to(b"git-upload-pac", Plus::Space, b"git-upload-pack"), Ok(false));
  assert_eq!(decodes_to(b"git-upload-packs", Plus::Space, b"git-upload-pack"), Ok(false));
  assert_eq!(decodes_to(b"a+b", Plus::Space, b"a b"), Ok(true));
  assert_eq!(decodes_to(b"a+b", Plus::Plus, b"a b"), Ok(false));
  assert_eq!(decodes_to(b"%zz", Plus::Space, b"x"), Err(BadEscape::NotHex));
}

/// A-595: do ask whether a path's escapes name a slash or a NUL. Expect an escaped one found in either case, and a plain
/// one or a malformed escape not.
#[test]
fn an_escape_naming_a_byte_is_found() {
  let refused: &[u8] = b"/\0";
  assert!(names_any(b"/a%2Fb", refused));
  assert!(names_any(b"/a%2fb", refused));
  assert!(names_any(b"/a%00", refused));
  assert!(!names_any(b"/a/b/%41", refused));
  assert!(!names_any(b"/a%2", refused));
}

/// A-595: do split a query into its form fields. Expect `name=value` pairs in order, an empty value for a field with no
/// `=`, the first `=` splitting, and empty fields skipped.
#[test]
fn a_query_splits_into_its_form_fields() {
  let fields: Vec<(&[u8], &[u8])> = form_fields(b"service=git-upload-pack&&flag&a=b=c&").collect();
  let expected: Vec<(&[u8], &[u8])> = vec![
    (b"service", b"git-upload-pack"),
    (b"flag", b""),
    (b"a", b"b=c"),
  ];
  assert_eq!(fields, expected);
  assert_eq!(form_fields(b"").count(), 0);
}
