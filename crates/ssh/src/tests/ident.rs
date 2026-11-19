//! The identification exchange (A-594; RFC 4253 §4.2, §5.1): clients' lines read whole and a byte at a time, the 255-byte
//! bound at its edge, and every refusal by its kind.

use proptest::prelude::*;

use crate::ident::{
  IdentificationReader, IdentificationRefusal, MAX_LINE, OwnIdentification,
  OwnIdentificationRefusal, software_version,
};

/// Reads `input` whole: the bytes consumed and the line, or the refusal.
fn read_whole(input: &[u8]) -> Result<(usize, Option<Vec<u8>>), IdentificationRefusal> {
  IdentificationReader::new().read(input)
}

/// Reads `input` a byte at a time: the bytes consumed and the line, or the refusal.
fn read_bytewise(input: &[u8]) -> Result<(usize, Option<Vec<u8>>), IdentificationRefusal> {
  let mut reader = IdentificationReader::new();
  let mut consumed = 0;
  for byte in input {
    let (used, line) = reader.read(std::slice::from_ref(byte))?;
    consumed += used;
    if line.is_some() {
      return Ok((consumed, line));
    }
  }
  Ok((consumed, None))
}

/// A-594: do read stock clients' identification lines (OpenSSH with and without comments, a minus sign in the software
/// version as Java clients send, a `1.99` server-style version), whole and a byte at a time, with packet bytes after.
/// Expect each line without its CR LF, and the bytes after it left unread.
#[test]
fn clients_lines_are_read_whole_or_bytewise() {
  let lines: [&[u8]; 4] = [
    b"SSH-2.0-OpenSSH_10.2",
    b"SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13.5",
    b"SSH-2.0-JSCH-0.1.54",
    b"SSH-1.99-OldServer_1.0",
  ];
  for line in lines {
    let mut input = line.to_vec();
    input.extend_from_slice(b"\r\n\x00\x00\x01\x2c");
    let expected = Ok((line.len() + 2, Some(line.to_vec())));
    assert_eq!(
      read_whole(&input),
      expected,
      "{:?}",
      String::from_utf8_lossy(line)
    );
    assert_eq!(
      read_bytewise(&input),
      expected,
      "{:?}",
      String::from_utf8_lossy(line)
    );
  }
}

/// A-594: do read a line ended by a bare LF, which RFC 4253 §4.2 lets a server accept from older clients. Expect the
/// line.
#[test]
fn a_bare_line_feed_ends_the_line() {
  assert_eq!(
    read_whole(b"SSH-2.0-Old\n"),
    Ok((12, Some(b"SSH-2.0-Old".to_vec())))
  );
}

/// A-594 hostile (RFC 4253 §4.2: "The maximum length of the string is 255 characters, including the Carriage Return and
/// Line Feed"): do read lines of exactly 255 bytes with CR LF and with a bare LF, then one byte longer, and a stream that
/// never ends its line. Expect the first two read and the others refused on their 255th byte, before any more is read.
#[test]
fn the_line_is_bounded_at_255_bytes() {
  let prefix = b"SSH-2.0-";
  let fits = |terminator: &[u8]| {
    let mut line = prefix.to_vec();
    line.resize(MAX_LINE - terminator.len(), b'x');
    line.extend_from_slice(terminator);
    line
  };
  let crlf = fits(b"\r\n");
  assert_eq!(
    read_whole(&crlf),
    Ok((MAX_LINE, Some(crlf[..MAX_LINE - 2].to_vec())))
  );
  let lf = fits(b"\n");
  assert_eq!(
    read_whole(&lf),
    Ok((MAX_LINE, Some(lf[..MAX_LINE - 1].to_vec())))
  );
  let mut over = prefix.to_vec();
  over.resize(MAX_LINE - 1, b'x');
  over.extend_from_slice(b"\r\n");
  assert_eq!(read_whole(&over), Err(IdentificationRefusal::TooLong));
  let endless = vec![b'x'; 10 * MAX_LINE];
  let mut reader = IdentificationReader::new();
  assert_eq!(
    reader.read(&endless[..MAX_LINE - 1]),
    Ok((MAX_LINE - 1, None))
  );
  assert_eq!(
    reader.read(&endless[MAX_LINE - 1..]),
    Err(IdentificationRefusal::TooLong)
  );
}

/// A-594 hostile: do read a line before the identification (an HTTP request, an empty line), a CR not followed by LF, a
/// NUL, versions other than 2.0 and 1.99, and lines with no software version or with a control byte in it. Expect each
/// refused by its kind.
#[test]
fn bad_lines_are_refused_by_kind() {
  let cases: [(&[u8], IdentificationRefusal); 9] = [
    (
      b"GET / HTTP/1.1\r\n",
      IdentificationRefusal::LineBeforeIdentification,
    ),
    (b"\r\n", IdentificationRefusal::LineBeforeIdentification),
    (
      b"SSH-2.0-a\rb\r\n",
      IdentificationRefusal::InvalidByte { at: 10 },
    ),
    (
      b"SSH-2.0-a\x00\r\n",
      IdentificationRefusal::InvalidByte { at: 9 },
    ),
    (
      b"SSH-1.5-old\r\n",
      IdentificationRefusal::UnsupportedVersion,
    ),
    (
      b"SSH-3.0-new\r\n",
      IdentificationRefusal::UnsupportedVersion,
    ),
    (b"SSH-2.0-\r\n", IdentificationRefusal::Malformed),
    (b"SSH-2.0\r\n", IdentificationRefusal::Malformed),
    (b"SSH-2.0- leading\r\n", IdentificationRefusal::Malformed),
  ];
  for (input, refusal) in cases {
    assert_eq!(
      read_whole(input),
      Err(refusal),
      "{:?}",
      String::from_utf8_lossy(input)
    );
  }
  assert_eq!(
    read_whole(b"SSH-2.0-a\x07b\r\n"),
    Err(IdentificationRefusal::Malformed)
  );
}

/// A-594: do build the server's identification with two banner lines and comments. Expect the banner lines first, each
/// ending CR LF, then `SSH-2.0-slates_<version> <comments>` ending CR LF, whose software version holds no minus sign; and
/// that line alone, without CR LF, as `V_S`.
#[test]
fn the_servers_identification_follows_its_banner() {
  let banner = vec![
    "Welcome to slates".to_string(),
    "Git, Mercurial and Subversion over SSH".to_string(),
  ];
  let own = OwnIdentification::new(&software_version(), Some("git"), &banner).unwrap();
  let line = format!("SSH-2.0-{} git", software_version());
  assert!(!software_version().contains('-'));
  assert_eq!(own.line(), line.as_bytes());
  let wire = format!("Welcome to slates\r\nGit, Mercurial and Subversion over SSH\r\n{line}\r\n");
  assert_eq!(own.wire(), wire.as_bytes());
  assert_eq!(
    read_whole(own.wire()),
    Err(IdentificationRefusal::LineBeforeIdentification)
  );
}

/// A-594: do build the server's identification with a banner line that begins "SSH-", one holding a CR, a software
/// version with a minus sign or a space, and comments with a control byte. Expect each refused, naming the banner line.
#[test]
fn bad_banners_and_identifications_are_refused() {
  let refused = |banner: &[&str], software: &str, comments: Option<&str>| {
    let banner: Vec<String> = banner.iter().map(|line| line.to_string()).collect();
    OwnIdentification::new(software, comments, &banner).unwrap_err()
  };
  assert_eq!(
    refused(&["ok", "SSH-2.0-fake"], "slates", None),
    OwnIdentificationRefusal::BannerLine { index: 1 }
  );
  assert_eq!(
    refused(&["a\rb"], "slates", None),
    OwnIdentificationRefusal::BannerLine { index: 0 }
  );
  assert_eq!(
    refused(&[], "sla-tes", None),
    OwnIdentificationRefusal::Line
  );
  assert_eq!(
    refused(&[], "sla tes", None),
    OwnIdentificationRefusal::Line
  );
  assert_eq!(
    refused(&[], "slates", Some("a\x01")),
    OwnIdentificationRefusal::Line
  );
  assert_eq!(
    refused(&[], &"s".repeat(MAX_LINE), None),
    OwnIdentificationRefusal::Line
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-594 hostile: do read any bytes as a client's identification, whole and a byte at a time. Expect the same outcome
  /// both ways, never a panic, and never more than 255 bytes consumed for a line.
  #[test]
  fn any_bytes_read_the_same_whole_or_bytewise(input in proptest::collection::vec(any::<u8>(), 0..400)) {
    let whole = read_whole(&input);
    prop_assert_eq!(&whole, &read_bytewise(&input));
    if let Ok((consumed, Some(_))) = whole {
      prop_assert!(consumed <= MAX_LINE);
    }
  }
}
