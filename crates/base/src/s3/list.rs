//! One `ListObjectsV2` response page read (A-464; §4.15 "An S3 base"): its objects (key, size, ETag, last-modified),
//! its common prefixes, and whether more follow under which continuation token. The response is AWS's XML
//! (`docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html`, fetched 2026-11-18): a `ListBucketResult` root,
//! `Contents` and `CommonPrefixes` children, `IsTruncated` with `NextContinuationToken` when more keys remain, and the
//! keys URL-encoded when the request asked `encoding-type=url`, which a base always asks, since XML 1.0 cannot carry
//! every character a key may hold (the page's own reason).
//!
//! **Hostile input (§4.9).** A page is bytes from the network, read as untrusted:
//! - its length is checked against the page's cap ([`page_cap`]) before a byte is parsed;
//! - the reader is a small iterative one for the subset S3 sends (a declaration, elements with attributes, text and
//!   the five named entities and character references), with no recursion; a document type, a comment, a CDATA section
//!   or a processing instruction past the declaration is refused, so no entity is ever declared or expanded;
//! - nesting is held to the response syntax's depth ([`MAX_DEPTH`]), the entries to the page's `max-keys`, every number
//!   to its type, and every percent-decoded key to UTF-8;
//! - a truncated page must name its continuation, and a page cut short anywhere is refused, never read as whole.
//!
//! Elements the syntax adds later (owners, checksums, restore status) are passed over, so a newer S3 still reads.

/// Format: the most keys one page holds: `max-keys`' documented ceiling and its default ("up to 1,000 key names").
pub const MAX_KEYS: u64 = 1_000;
/// Format: an object key's longest UTF-8 encoding in bytes (S3's object key guide: "a maximum length of 1,024
/// bytes").
pub const KEY_MAX_BYTES: u64 = 1_024;
/// Format: the bytes URL encoding writes for one byte at most (`%XX`).
const ENCODED_BYTE_MAX: u64 = 3;
/// Format: the deepest element the response syntax names, counting the root: `ListBucketResult`, `Contents`,
/// `RestoreStatus`, `IsRestoreInProgress`.
pub const MAX_DEPTH: usize = 4;
/// Format: the bytes of one `Contents` element's markup and fields other than its key, at most, from the response
/// syntax, about 1,200: every documented child's tags (`Key`, `LastModified`, `ETag`, `Size`, `StorageClass`, the ten
/// checksum algorithms and the checksum type, `Owner` with `DisplayName` and `ID`, `RestoreStatus` with its two
/// children, about 600 bytes of tags), a multipart ETag (38 bytes) with its quotes as entities (12), a 24-byte
/// timestamp, a 20-digit size, an owner's 64-hex id and a display name of as many, a restore's date, and the
/// indentation S3 writes, rounded up to a power of two.
const ENTRY_OVERHEAD_BYTES: u64 = 2_048;
/// Format: the bytes of a page's own markup and short fields, at most, about 400: the declaration, the root with its
/// namespace, `Name` (a bucket name, 63 bytes at most), `KeyCount`, `MaxKeys`, `IsTruncated`, `Delimiter`,
/// `EncodingType` and the empty tags of the long fields, rounded up to a power of two. The long fields are counted
/// apart ([`LONG_FIELDS`]).
const PAGE_OVERHEAD_BYTES: u64 = 1_024;
/// Format: the page's fields that may be as long as an encoded key: `Prefix` and `StartAfter` (keys), and the two
/// continuation tokens, which S3 calls obfuscated and does not bound; held to a key's encoded length, which the tokens
/// observed are well within (tens of bytes; an assumption, flagged for the client's run, part 2).
const LONG_FIELDS: u64 = 4;

/// Derived: the most bytes a `ListObjectsV2` page asked for `max_keys` keys (URL-encoded) can hold: each entry a key at
/// its longest encoding (`KEY_MAX_BYTES × 3`) and an entry's markup ([`ENTRY_OVERHEAD_BYTES`]), the page's own
/// ([`PAGE_OVERHEAD_BYTES`]) and its long fields at a key's encoded length each ([`LONG_FIELDS`]). A longer page is
/// refused before it is parsed: 5,133,312 bytes, about 4.9 MiB, for a thousand keys.
pub fn page_cap(max_keys: u64) -> u64 {
  let encoded_key = KEY_MAX_BYTES.saturating_mul(ENCODED_BYTE_MAX);
  max_keys
    .saturating_mul(encoded_key.saturating_add(ENTRY_OVERHEAD_BYTES))
    .saturating_add(PAGE_OVERHEAD_BYTES)
    .saturating_add(encoded_key.saturating_mul(LONG_FIELDS))
}

/// One object a page lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listed {
  /// Its key, decoded.
  pub key: String,
  /// Its size in bytes.
  pub size: u64,
  /// Its ETag, quotes and all, as S3 states it: the identity of its content (S3's object type: "The ETag reflects
  /// changes only to the contents of an object").
  pub etag: String,
  /// When it was made, as S3 states it (ISO 8601).
  pub last_modified: String,
}

/// One page read: its objects and common prefixes in S3's order, and the continuation token when more follow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListPage {
  /// The objects, by key in UTF-8 binary order as S3 lists them.
  pub objects: Vec<Listed>,
  /// The common prefixes (a delimited listing's subdirectories), decoded.
  pub prefixes: Vec<String>,
  /// Whether more keys remain.
  pub truncated: bool,
  /// The token that continues the listing, present exactly when it is truncated.
  pub next_token: Option<String>,
}

/// A page refused, by its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
  /// Longer than the page's cap: refused before parsing.
  TooLong {
    /// The page's length.
    len: u64,
    /// The cap.
    cap: u64,
  },
  /// Not UTF-8, or a decoded key that is not.
  NotUtf8,
  /// Markup that does not parse, at a byte offset: an unclosed tag, a mismatched end tag, text outside the root, a
  /// page cut short.
  Malformed {
    /// Where.
    at: usize,
  },
  /// A construct S3 never sends and a reader must not honour: a document type, a comment, a CDATA section or a
  /// processing instruction past the declaration.
  Unsupported {
    /// Where.
    at: usize,
  },
  /// An entity or character reference that names nothing, or no character.
  BadEntity {
    /// Where.
    at: usize,
  },
  /// Nested deeper than the response syntax ([`MAX_DEPTH`]).
  TooDeep,
  /// A root other than `ListBucketResult`.
  NotAListing,
  /// An object without one of the fields a base needs.
  MissingField {
    /// The field.
    field: &'static str,
  },
  /// A number or a truth value that does not parse as its type.
  BadValue {
    /// The field.
    field: &'static str,
  },
  /// More entries than the page's `max-keys`.
  TooManyEntries {
    /// How many it held.
    count: usize,
  },
  /// A truncated page that names no continuation token.
  TruncatedWithoutToken,
  /// An encoding the base never asks for, or a percent escape that does not decode.
  BadEncoding,
}

/// Format: the elements of a listing this reader keeps, by their path under the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
  IsTruncated,
  NextContinuationToken,
  EncodingType,
  Key,
  Size,
  ETag,
  LastModified,
  CommonPrefix,
}

impl Field {
  /// The field a path of element names names, if the reader keeps it.
  fn of(path: &[String]) -> Option<Field> {
    let names: Vec<&str> = path.iter().map(String::as_str).collect();
    match names.as_slice() {
      ["ListBucketResult", "IsTruncated"] => Some(Field::IsTruncated),
      ["ListBucketResult", "NextContinuationToken"] => Some(Field::NextContinuationToken),
      ["ListBucketResult", "EncodingType"] => Some(Field::EncodingType),
      ["ListBucketResult", "Contents", "Key"] => Some(Field::Key),
      ["ListBucketResult", "Contents", "Size"] => Some(Field::Size),
      ["ListBucketResult", "Contents", "ETag"] => Some(Field::ETag),
      ["ListBucketResult", "Contents", "LastModified"] => Some(Field::LastModified),
      ["ListBucketResult", "CommonPrefixes", "Prefix"] => Some(Field::CommonPrefix),
      _ => None,
    }
  }
}

/// An object being read: each field as its element's text, until the element closes.
#[derive(Default)]
struct Reading {
  key: Option<String>,
  size: Option<String>,
  etag: Option<String>,
  last_modified: Option<String>,
}

/// The page as read, before its keys are decoded.
#[derive(Default)]
struct Raw {
  objects: Vec<Reading>,
  prefixes: Vec<String>,
  truncated: Option<String>,
  next_token: Option<String>,
  encoding: Option<String>,
}

/// Reads one `ListObjectsV2` page of at most `cap` bytes (refused unread past it) and at most `max_keys` entries.
pub fn parse_page(bytes: &[u8], cap: u64, max_keys: u64) -> Result<ListPage, ListError> {
  let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  if len > cap {
    return Err(ListError::TooLong { len, cap });
  }
  let text = std::str::from_utf8(bytes).map_err(|_| ListError::NotUtf8)?;
  let raw = read(text)?;
  finish(raw, max_keys)
}

/// The page's markup walked once: the fields kept, by path.
fn read(text: &str) -> Result<Raw, ListError> {
  let mut raw = Raw::default();
  let mut path: Vec<String> = Vec::new();
  let mut current = String::new();
  let mut rooted = false;
  let mut at = 0usize;
  let bytes = text.as_bytes();
  while at < bytes.len() {
    let rest = text.get(at..).ok_or(ListError::Malformed { at })?;
    if !rest.starts_with('<') {
      let end = rest
        .find('<')
        .map_or(bytes.len(), |off| at.saturating_add(off));
      let run = text.get(at..end).ok_or(ListError::Malformed { at })?;
      if path.is_empty() {
        if !run.trim().is_empty() {
          return Err(ListError::Malformed { at });
        }
      } else {
        decode_entities(run, at, &mut current)?;
      }
      at = end;
      continue;
    }
    if rest.starts_with("<?") {
      // The declaration alone, first: anything else is a processing instruction no listing holds.
      if at != 0 || !rest.starts_with("<?xml") {
        return Err(ListError::Unsupported { at });
      }
      let end = rest.find("?>").ok_or(ListError::Malformed { at })?;
      at = at.saturating_add(end).saturating_add(2);
      continue;
    }
    if rest.starts_with("<!") {
      return Err(ListError::Unsupported { at });
    }
    let close = tag_end(rest).ok_or(ListError::Malformed { at })?;
    let tag = rest.get(1..close).ok_or(ListError::Malformed { at })?;
    let after = at.saturating_add(close).saturating_add(1);
    if let Some(name) = tag.strip_prefix('/') {
      let name = name.trim_end();
      if path.last().map(String::as_str) != Some(name) {
        return Err(ListError::Malformed { at });
      }
      close_element(&mut raw, &path, std::mem::take(&mut current))?;
      path.pop();
    } else {
      let (name, empty) = match tag.strip_suffix('/') {
        Some(inner) => (element_name(inner, at)?, true),
        None => (element_name(tag, at)?, false),
      };
      if path.is_empty() {
        if rooted {
          return Err(ListError::Malformed { at });
        }
        if name != "ListBucketResult" {
          return Err(ListError::NotAListing);
        }
        rooted = true;
      }
      if path.len() >= MAX_DEPTH {
        return Err(ListError::TooDeep);
      }
      path.push(name.to_owned());
      current.clear();
      open_element(&mut raw, &path);
      if empty {
        close_element(&mut raw, &path, String::new())?;
        path.pop();
      }
    }
    at = after;
  }
  if !path.is_empty() || !rooted {
    return Err(ListError::Malformed { at: bytes.len() });
  }
  Ok(raw)
}

/// Where a tag that starts `rest` (at its `<`) ends: its `>`, past any quoted attribute value.
fn tag_end(rest: &str) -> Option<usize> {
  let mut quote: Option<u8> = None;
  for (at, &byte) in rest.as_bytes().iter().enumerate().skip(1) {
    match (quote, byte) {
      (Some(open), _) if byte == open => quote = None,
      (Some(_), _) => {}
      (None, b'"' | b'\'') => quote = Some(byte),
      (None, b'>') => return Some(at),
      (None, b'<') => return None,
      _ => {}
    }
  }
  None
}

/// A start tag's element name: the text before its first space, its attributes passed over (S3's root carries its
/// namespace). A name that is empty or starts with something no element name does is refused.
fn element_name(tag: &str, at: usize) -> Result<&str, ListError> {
  let name = tag
    .split(|c: char| c.is_ascii_whitespace())
    .next()
    .unwrap_or_default();
  if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
    return Err(ListError::Malformed { at });
  }
  Ok(name)
}

/// Starts what an element opens: an object for a `Contents`.
fn open_element(raw: &mut Raw, path: &[String]) {
  let names: Vec<&str> = path.iter().map(String::as_str).collect();
  if names.as_slice() == ["ListBucketResult", "Contents"] {
    raw.objects.push(Reading::default());
  }
}

/// Keeps what an element closing at `path` held, if the reader keeps it; a field given twice is refused.
fn close_element(raw: &mut Raw, path: &[String], text: String) -> Result<(), ListError> {
  let Some(field) = Field::of(path) else {
    return Ok(());
  };
  let slot = match field {
    Field::IsTruncated => &mut raw.truncated,
    Field::NextContinuationToken => &mut raw.next_token,
    Field::EncodingType => &mut raw.encoding,
    Field::CommonPrefix => {
      raw.prefixes.push(text);
      return Ok(());
    }
    Field::Key | Field::Size | Field::ETag | Field::LastModified => {
      let object = raw
        .objects
        .last_mut()
        .ok_or(ListError::MissingField { field: "Contents" })?;
      match field {
        Field::Key => &mut object.key,
        Field::Size => &mut object.size,
        Field::ETag => &mut object.etag,
        _ => &mut object.last_modified,
      }
    }
  };
  if slot.replace(text).is_some() {
    return Err(ListError::BadValue {
      field: "a field given twice",
    });
  }
  Ok(())
}

/// Format: the longest entity a listing names, `&apos;`'s name, or a character reference's, `#x10FFFF`'s, between `&`
/// and `;`.
const ENTITY_MAX: usize = 8;

/// Appends `run`'s text to `out` with its entities and character references decoded; one that names nothing, no
/// character, or never closes is refused at its offset (from `base`).
fn decode_entities(run: &str, base: usize, out: &mut String) -> Result<(), ListError> {
  let mut rest = run;
  let mut offset = base;
  while let Some(amp) = rest.find('&') {
    out.push_str(rest.get(..amp).unwrap_or_default());
    let at = offset.saturating_add(amp);
    let after = rest.get(amp.saturating_add(1)..).unwrap_or_default();
    let semi = after
      .find(';')
      .filter(|&semi| semi <= ENTITY_MAX)
      .ok_or(ListError::BadEntity { at })?;
    let name = after.get(..semi).unwrap_or_default();
    out.push(entity(name).ok_or(ListError::BadEntity { at })?);
    let consumed = amp.saturating_add(semi).saturating_add(2);
    rest = rest.get(consumed..).unwrap_or_default();
    offset = offset.saturating_add(consumed);
  }
  out.push_str(rest);
  Ok(())
}

/// The character an entity's name or a character reference names: XML's five named entities, `#N` and `#xN`; never
/// NUL, a surrogate or past Unicode.
fn entity(name: &str) -> Option<char> {
  let code = match name {
    "lt" => return Some('<'),
    "gt" => return Some('>'),
    "amp" => return Some('&'),
    "quot" => return Some('"'),
    "apos" => return Some('\''),
    _ => match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
      Some(hex) if !hex.is_empty() => u32::from_str_radix(hex, 16).ok()?,
      _ => {
        let decimal = name.strip_prefix('#')?;
        if decimal.is_empty() || !decimal.bytes().all(|b| b.is_ascii_digit()) {
          return None;
        }
        decimal.parse::<u32>().ok()?
      }
    },
  };
  char::from_u32(code).filter(|&c| c != '\0')
}

/// The page read: each object's fields typed and its key decoded, the prefixes decoded, the counts and the continuation
/// checked.
fn finish(raw: Raw, max_keys: u64) -> Result<ListPage, ListError> {
  let url = match raw.encoding.as_deref() {
    None => false,
    Some("url") => true,
    Some(_) => return Err(ListError::BadEncoding),
  };
  let decode = |text: String| if url { url_decode(&text) } else { Ok(text) };
  let count = raw.objects.len().saturating_add(raw.prefixes.len());
  if u64::try_from(count).unwrap_or(u64::MAX) > max_keys {
    return Err(ListError::TooManyEntries { count });
  }
  let mut objects = Vec::with_capacity(raw.objects.len());
  for reading in raw.objects {
    let key = decode(
      reading
        .key
        .ok_or(ListError::MissingField { field: "Key" })?,
    )?;
    let size = reading
      .size
      .ok_or(ListError::MissingField { field: "Size" })?;
    let size = size
      .trim()
      .parse::<u64>()
      .ok()
      .filter(|_| size.trim().bytes().all(|b| b.is_ascii_digit()))
      .ok_or(ListError::BadValue { field: "Size" })?;
    objects.push(Listed {
      key,
      size,
      etag: reading
        .etag
        .ok_or(ListError::MissingField { field: "ETag" })?,
      last_modified: reading.last_modified.ok_or(ListError::MissingField {
        field: "LastModified",
      })?,
    });
  }
  let prefixes = raw
    .prefixes
    .into_iter()
    .map(decode)
    .collect::<Result<Vec<_>, _>>()?;
  let truncated = match raw.truncated.as_deref().map(str::trim) {
    None | Some("false") => false,
    Some("true") => true,
    Some(_) => {
      return Err(ListError::BadValue {
        field: "IsTruncated",
      });
    }
  };
  let next_token = raw.next_token.filter(|token| !token.is_empty());
  if truncated && next_token.is_none() {
    return Err(ListError::TruncatedWithoutToken);
  }
  Ok(ListPage {
    objects,
    prefixes,
    truncated,
    next_token: next_token.filter(|_| truncated),
  })
}

/// A key S3 URL-encoded as a form value decoded: `%XX` to its byte, `+` to a space (S3 encodes a space so, and a `+`
/// as `%2B`: AWS's SDKs decode a listing's keys with form decoding, botocore's `unquote_plus`, tier C), the bytes held
/// to UTF-8.
fn url_decode(text: &str) -> Result<String, ListError> {
  let bytes = text.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut at = 0usize;
  while let Some(&byte) = bytes.get(at) {
    match byte {
      b'%' => {
        let hex = text
          .get(at.saturating_add(1)..at.saturating_add(3))
          .ok_or(ListError::BadEncoding)?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
          return Err(ListError::BadEncoding);
        }
        out.push(u8::from_str_radix(hex, 16).map_err(|_| ListError::BadEncoding)?);
        at = at.saturating_add(3);
      }
      b'+' => {
        out.push(b' ');
        at = at.saturating_add(1);
      }
      other => {
        out.push(other);
        at = at.saturating_add(1);
      }
    }
  }
  String::from_utf8(out).map_err(|_| ListError::NotUtf8)
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::*;

  /// The cap a test reads pages under: a full page's.
  fn cap() -> u64 {
    page_cap(MAX_KEYS)
  }

  /// `text` read as a page of a thousand keys at most.
  fn page(text: &str) -> Result<ListPage, ListError> {
    parse_page(text.as_bytes(), cap(), MAX_KEYS)
  }

  /// Format: the API reference's second sample response (`start-after`), verbatim but for its HTTP head.
  const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>quotes</Name>
  <Prefix>E</Prefix>
  <StartAfter>ExampleGuide.pdf</StartAfter>
  <KeyCount>1</KeyCount>
  <MaxKeys>3</MaxKeys>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>ExampleObject.txt</Key>
    <LastModified>2013-09-17T18:07:53.000Z</LastModified>
    <ETag>"599bab3ed2c697f1d26842727561fd94"</ETag>
    <Size>857</Size>
    <StorageClass>REDUCED_REDUNDANCY</StorageClass>
  </Contents>
</ListBucketResult>"#;

  /// Format: the API reference's delimited sample response (`delimiter=/`), its rolled-up prefix as the page gives it.
  const DELIMITED: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>example-bucket</Name>
  <Prefix></Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <Delimiter>/</Delimiter>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>sample.jpg</Key>
    <LastModified>2011-02-26T01:56:20.000Z</LastModified>
    <ETag>"bf1d737a4d46a19f3bced6905cc8b902"</ETag>
    <Size>142863</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <CommonPrefixes>
    <Prefix>photos/</Prefix>
  </CommonPrefixes>
</ListBucketResult>"#;

  /// A-464: do read the API reference's sample responses. Expect each object's key, size, ETag (its quotes kept) and
  /// time, the delimited page's rolled-up prefix, and no continuation.
  #[test]
  fn the_api_references_sample_pages_read_as_they_say() {
    assert_eq!(
      page(SAMPLE).unwrap(),
      ListPage {
        objects: vec![Listed {
          key: "ExampleObject.txt".to_owned(),
          size: 857,
          etag: "\"599bab3ed2c697f1d26842727561fd94\"".to_owned(),
          last_modified: "2013-09-17T18:07:53.000Z".to_owned(),
        }],
        prefixes: Vec::new(),
        truncated: false,
        next_token: None,
      }
    );
    let delimited = page(DELIMITED).unwrap();
    assert_eq!(delimited.objects.len(), 1);
    assert_eq!(delimited.objects[0].key, "sample.jpg");
    assert_eq!(delimited.prefixes, vec!["photos/".to_owned()]);
  }

  /// A-464: do read a truncated page whose keys S3 URL-encoded and whose ETags carry their quotes as entities, with an
  /// owner and a checksum this reader passes over. Expect each key decoded (`%28` to `(`, `+` to a space, `%2B` to `+`,
  /// a multi-byte character), the ETag's quotes decoded, and the continuation token kept.
  #[test]
  fn an_encoded_truncated_page_decodes_its_keys_and_keeps_its_token() {
    let text = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix/><KeyCount>2</KeyCount>
<MaxKeys>2</MaxKeys><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated>
<Contents><Key>test_file%283%29.png</Key><LastModified>2026-11-18T00:00:00.000Z</LastModified>
<ETag>&quot;0123456789abcdef0123456789abcdef-3&quot;</ETag><ChecksumAlgorithm>CRC32</ChecksumAlgorithm><Size>12</Size>
<Owner><ID>abc</ID><DisplayName>d</DisplayName></Owner><StorageClass>STANDARD</StorageClass></Contents>
<Contents><Key>a+b%2Bc/%C3%A9t%C3%A9.txt</Key><LastModified>2026-11-18T00:00:01.000Z</LastModified>
<ETag>&#34;x&#x22;</ETag><Size>0</Size></Contents>
<NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken></ListBucketResult>"#;
    let read = parse_page(text.as_bytes(), cap(), 2).unwrap();
    let keys: Vec<&str> = read.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["test_file(3).png", "a b+c/été.txt"]);
    assert_eq!(
      read.objects[0].etag,
      "\"0123456789abcdef0123456789abcdef-3\""
    );
    assert_eq!(read.objects[1].etag, "\"x\"");
    assert!(read.truncated);
    assert_eq!(
      read.next_token.as_deref(),
      Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
    );
  }

  /// A-464 (hostile input): do read pages each broken one way: longer than the cap; not UTF-8; a document type
  /// declaring an entity (the billion-laughs shape); a comment; CDATA; a second processing instruction; nesting past
  /// the syntax's depth; a root other than a listing; an end tag that matches nothing; text after the root; an unknown
  /// entity, an unclosed one, and a reference to NUL; a size negative or past `u64`; a truth value neither true nor
  /// false; an object without a key; a field given twice; more entries than `max-keys`; a truncated page without its
  /// token; an unknown encoding, a bad percent escape, and an escape that decodes to invalid UTF-8. Expect each refused
  /// by its kind, never a panic and never a page.
  #[test]
  fn every_hostile_page_is_refused_by_its_kind() {
    let wrap = |inner: &str| format!("<ListBucketResult>{inner}</ListBucketResult>");
    let object = |key: &str, size: &str| {
      format!(
        "<Contents><Key>{key}</Key><Size>{size}</Size><ETag>e</ETag><LastModified>t</LastModified></Contents>"
      )
    };
    let big = "x".repeat(usize::try_from(cap()).unwrap() + 1);
    assert_eq!(
      page(&big),
      Err(ListError::TooLong {
        len: cap() + 1,
        cap: cap()
      })
    );
    assert_eq!(
      parse_page(&[0xff, 0xfe], cap(), MAX_KEYS),
      Err(ListError::NotUtf8)
    );
    let cases: Vec<(String, ListError)> = vec![
      (
        r#"<!DOCTYPE l [<!ENTITY a "aaaa">]><ListBucketResult>&a;</ListBucketResult>"#.to_owned(),
        ListError::Unsupported { at: 0 },
      ),
      (wrap("<!-- c -->"), ListError::Unsupported { at: 18 }),
      (
        wrap("<Contents><Key><![CDATA[k]]></Key></Contents>"),
        ListError::Unsupported { at: 33 },
      ),
      (
        format!("<?xml version=\"1.0\"?>{}<?pi x?>", wrap("")),
        ListError::Unsupported { at: 58 },
      ),
      (
        wrap("<Contents><Owner><ID><Deeper>x</Deeper></ID></Owner></Contents>"),
        ListError::TooDeep,
      ),
      (
        "<Error><Code>AccessDenied</Code></Error>".to_owned(),
        ListError::NotAListing,
      ),
      (wrap("</Contents>"), ListError::Malformed { at: 18 }),
      (
        format!("{}trailing", wrap("")),
        ListError::Malformed { at: 37 },
      ),
      (
        wrap(&object("a&bogus;", "1")),
        ListError::BadEntity { at: 34 },
      ),
      (wrap(&object("a&amp", "1")), ListError::BadEntity { at: 34 }),
      (wrap(&object("a&#0;", "1")), ListError::BadEntity { at: 34 }),
      (
        wrap(&object("k", "-1")),
        ListError::BadValue { field: "Size" },
      ),
      (
        wrap(&object("k", "18446744073709551616")),
        ListError::BadValue { field: "Size" },
      ),
      (
        wrap("<IsTruncated>maybe</IsTruncated>"),
        ListError::BadValue {
          field: "IsTruncated",
        },
      ),
      (
        wrap("<Contents><Size>1</Size><ETag>e</ETag><LastModified>t</LastModified></Contents>"),
        ListError::MissingField { field: "Key" },
      ),
      (
        wrap("<Contents><Key>a</Key><Key>b</Key><Size>1</Size></Contents>"),
        ListError::BadValue {
          field: "a field given twice",
        },
      ),
      (
        wrap("<IsTruncated>true</IsTruncated>"),
        ListError::TruncatedWithoutToken,
      ),
      (
        wrap(&format!(
          "<EncodingType>base64</EncodingType>{}",
          object("k", "1")
        )),
        ListError::BadEncoding,
      ),
      (
        wrap(&format!(
          "<EncodingType>url</EncodingType>{}",
          object("a%2", "1")
        )),
        ListError::BadEncoding,
      ),
      (
        wrap(&format!(
          "<EncodingType>url</EncodingType>{}",
          object("a%C3", "1")
        )),
        ListError::NotUtf8,
      ),
    ];
    for (text, refusal) in cases {
      assert_eq!(page(&text), Err(refusal), "{text}");
    }
    let three = wrap(&format!(
      "{}{}{}",
      object("a", "1"),
      object("b", "1"),
      object("c", "1")
    ));
    assert_eq!(
      parse_page(three.as_bytes(), cap(), 2),
      Err(ListError::TooManyEntries { count: 3 })
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-464 (hostile input): do cut the sample page anywhere short of its end. Expect it refused, never read as a
    /// whole page.
    #[test]
    fn a_page_cut_short_anywhere_is_refused(cut in 0usize..SAMPLE.len()) {
      prop_assert!(page(&SAMPLE[..cut]).is_err());
    }

    /// A-464 (hostile input): do read arbitrary bytes, and the sample page with one byte changed anywhere. Expect a
    /// page or a typed refusal, never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(
      bytes in proptest::collection::vec(any::<u8>(), 0..512),
      at in 0usize..SAMPLE.len(),
      byte in any::<u8>(),
    ) {
      let _ = parse_page(&bytes, cap(), MAX_KEYS);
      let mut changed = SAMPLE.as_bytes().to_vec();
      changed[at] = byte;
      let _ = parse_page(&changed, cap(), MAX_KEYS);
    }
  }
}
