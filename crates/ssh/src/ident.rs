//! The identification exchange (A-594; RFC 4253 §4.2): before any packet, each side sends one line,
//! `SSH-protoversion-softwareversion SP comments CR LF`, and the line without its CR LF enters the exchange hash.
//!
//! **The server's lines.** A server "MAY send other lines of data before sending the version string", lines that "MUST
//! NOT begin with "SSH-"" [B: RFC 4253 §4.2]; [`OwnIdentification`] checks the caller's banner lines against that and
//! refuses a CR, an LF or a NUL inside one, so each is exactly one line. Its own line names slates and its version.
//!
//! **The client's line.** [`IdentificationReader`] reads it a byte at a time, within RFC 4253 §4.2's bound: "The maximum
//! length of the string is 255 characters, including the Carriage Return and Line Feed". It is refused on its 255th byte
//! if that byte does not end it, so the bound holds whatever the client sends. Three choices follow OpenSSH's own reader
//! (`kex_exchange_identification` in `kex.c` at `fa4b2a82`) [C] where the document leaves room:
//! - a line before the identification is refused, since only a server may send one ("Do not accept lines before the SSH
//!   ident from a client");
//! - a bare LF ends the line, which RFC 4253 §4.2 itself allows for older clients ("may want to process the
//!   identification string without expecting the presence of the carriage return character"); a CR not followed by LF,
//!   or a NUL ("The null character MUST NOT be sent"), is refused;
//! - after the protocol version, any printable byte is taken, a minus sign too: the document reserves it out of the
//!   software version, but clients in use put one there and OpenSSH reads past it (`sscanf "SSH-%d.%d-%[^\n]\n"`).
//!
//! The protocol version must be `2.0`, or `1.99`, which RFC 4253 §5.1 makes "identical to "2.0"".

use crate::to_u64;

/// The protocol version slates speaks (RFC 4253 §4.2: "the 'protoversion' MUST be "2.0"").
pub const PROTOCOL_VERSION: &str = "2.0";

/// The older servers' version RFC 4253 §5.1 makes identical to 2.0.
pub const COMPATIBLE_VERSION: &str = "1.99";

/// The longest identification line, CR and LF included (RFC 4253 §4.2).
pub const MAX_LINE: usize = 255;

/// What every identification line starts with.
const PREFIX: &[u8] = b"SSH-";

/// The software version slates names in its own line: `slates_` and the crate's version, with any minus sign made an
/// underscore, since RFC 4253 §4.2 keeps the minus sign out of a software version.
pub fn software_version() -> String {
  format!("slates_{}", env!("CARGO_PKG_VERSION")).replace('-', "_")
}

/// Why an identification line was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentificationRefusal {
  /// The line reached [`MAX_LINE`] bytes without its end.
  TooLong,
  /// A line came before the client's identification.
  LineBeforeIdentification,
  /// A NUL, or a CR not followed by LF, at `at` bytes into the line.
  InvalidByte {
    /// The byte's offset in the line.
    at: u64,
  },
  /// The line is not `SSH-protoversion-softwareversion`, or a field holds a byte RFC 4253 §4.2 does not allow.
  Malformed,
  /// The protocol version is neither 2.0 nor 1.99.
  UnsupportedVersion,
}

/// Why the server's own identification was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnIdentificationRefusal {
  /// The banner line at `index` begins with "SSH-" or holds a CR, an LF or a NUL.
  BannerLine {
    /// The line's position among the banner lines.
    index: usize,
  },
  /// The software version or the comments hold a byte RFC 4253 §4.2 does not allow, or the line passes [`MAX_LINE`].
  Line,
}

/// The server's banner lines and identification line, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnIdentification {
  /// The identification line without CR LF: `V_S` in the exchange hash.
  line: Vec<u8>,
  /// The bytes to send first: each banner line and the identification line, each ending CR LF.
  wire: Vec<u8>,
}

impl OwnIdentification {
  /// The server's identification for `software`, with `comments` when given, after `banner`'s lines.
  pub fn new(
    software: &str,
    comments: Option<&str>,
    banner: &[String],
  ) -> Result<Self, OwnIdentificationRefusal> {
    let mut wire = Vec::new();
    for (index, banner_line) in banner.iter().enumerate() {
      if !is_banner_line(banner_line.as_bytes()) {
        return Err(OwnIdentificationRefusal::BannerLine { index });
      }
      wire.extend_from_slice(banner_line.as_bytes());
      wire.extend_from_slice(b"\r\n");
    }
    let line = identification_line(software, comments)?;
    wire.extend_from_slice(&line);
    wire.extend_from_slice(b"\r\n");
    Ok(Self { line, wire })
  }

  /// The identification line without CR LF (`V_S`).
  pub fn line(&self) -> &[u8] {
    &self.line
  }

  /// The bytes to send before the first packet.
  pub fn wire(&self) -> &[u8] {
    &self.wire
  }
}

/// `SSH-2.0-software[ SP comments]`, checked against RFC 4253 §4.2.
fn identification_line(
  software: &str,
  comments: Option<&str>,
) -> Result<Vec<u8>, OwnIdentificationRefusal> {
  let software_allowed = !software.is_empty()
    && software
      .bytes()
      .all(|byte| byte.is_ascii_graphic() && byte != b'-');
  let comments_allowed =
    comments.is_none_or(|text| !text.is_empty() && text.bytes().all(is_line_byte));
  if !software_allowed || !comments_allowed {
    return Err(OwnIdentificationRefusal::Line);
  }
  let mut line = Vec::new();
  line.extend_from_slice(PREFIX);
  line.extend_from_slice(PROTOCOL_VERSION.as_bytes());
  line.push(b'-');
  line.extend_from_slice(software.as_bytes());
  if let Some(text) = comments {
    line.push(b' ');
    line.extend_from_slice(text.as_bytes());
  }
  if line.len().saturating_add(2) > MAX_LINE {
    return Err(OwnIdentificationRefusal::Line);
  }
  Ok(line)
}

/// Whether `line` may be sent before the identification: not "SSH-", and no CR, LF or NUL inside.
fn is_banner_line(line: &[u8]) -> bool {
  !line.starts_with(PREFIX) && !line.iter().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
}

/// Whether `byte` may stand in the software version's field or the comments: printable US-ASCII or a space.
fn is_line_byte(byte: u8) -> bool {
  byte == b' ' || byte.is_ascii_graphic()
}

/// Reads the client's identification line a byte at a time.
#[derive(Clone, Debug, Default)]
pub struct IdentificationReader {
  /// The line's bytes so far, without CR or LF.
  line: Vec<u8>,
  /// The bytes of the line consumed so far, a CR included.
  consumed: usize,
  /// Whether the last byte was a CR, which only an LF may follow.
  carriage_return: bool,
}

impl IdentificationReader {
  /// A reader at the start of the client's first line.
  pub fn new() -> Self {
    Self::default()
  }

  /// Reads from `input` up to the end of the line: the bytes consumed, and the line without CR LF once it is whole and
  /// checked. Bytes after the line's end are left for the packets.
  pub fn read(&mut self, input: &[u8]) -> Result<(usize, Option<Vec<u8>>), IdentificationRefusal> {
    for (index, byte) in input.iter().enumerate() {
      let used = index.saturating_add(1);
      self.consumed = self.consumed.saturating_add(1);
      if *byte == b'\n' {
        let line = core::mem::take(&mut self.line);
        return check_line(&line).map(|()| (used, Some(line)));
      }
      self.take_byte(*byte)?;
    }
    Ok((input.len(), None))
  }

  /// Takes one byte that does not end the line.
  fn take_byte(&mut self, byte: u8) -> Result<(), IdentificationRefusal> {
    if self.consumed >= MAX_LINE {
      return Err(IdentificationRefusal::TooLong);
    }
    if byte == 0 || self.carriage_return {
      return Err(IdentificationRefusal::InvalidByte {
        at: to_u64(self.consumed.saturating_sub(1)),
      });
    }
    if byte == b'\r' {
      self.carriage_return = true;
      return Ok(());
    }
    self.line.push(byte);
    Ok(())
  }
}

/// Checks a whole client line: "SSH-", a version of 2.0 or 1.99, a minus sign, a software version.
fn check_line(line: &[u8]) -> Result<(), IdentificationRefusal> {
  let Some(rest) = line.strip_prefix(PREFIX) else {
    return Err(IdentificationRefusal::LineBeforeIdentification);
  };
  let Some(dash) = rest.iter().position(|byte| *byte == b'-') else {
    return Err(IdentificationRefusal::Malformed);
  };
  let (version, software) = rest.split_at(dash);
  let software = software.get(1..).unwrap_or(&[]);
  if version != PROTOCOL_VERSION.as_bytes() && version != COMPATIBLE_VERSION.as_bytes() {
    return Err(IdentificationRefusal::UnsupportedVersion);
  }
  let named = software.first().is_some_and(u8::is_ascii_graphic);
  if !named || !software.iter().all(|byte| is_line_byte(*byte)) {
    return Err(IdentificationRefusal::Malformed);
  }
  Ok(())
}
