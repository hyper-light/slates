//! winfstest's reply, parsed (A-214; Part 6 "Conformance" on Windows; §4.6 "Windows"). winfstest is WinFsp's own
//! conformance suite over the Win32 file API: a C driver (`winfstest.c`) makes one call per process and prints its
//! outcome, and twenty scripts (`t/base`, `t/reparse`, `t/streams`) state what each call must answer. Upstream drives
//! the scripts from Python 2.7 (`winfstest.py`, `simpletap.py`), which no CI runner carries and which slates may not
//! add (CLAUDE.md banned item 13), so `cargo xtask conformance` carries a Rust transcription of the scripts and runs
//! the upstream driver, built from its pinned source, exactly as the scripts would. This module is the pure half: the
//! driver's reply read the way `winfstest.py` reads it.
//!
//! The reply's shape is `winfstest.c`'s at the pinned commit (`errprint` and each `do_*` call): the first line is the
//! call's outcome — `0`, or the Win32 error's name (`ERROR_FILE_NOT_FOUND`), or `ERROR(n)` for a code its table lacks —
//! and each further line is one record of `key=value` fields (`FileAttributes=0x20 FileSize=0 …`), a value in double
//! quotes when it can hold spaces (`Data="41 42"`). `winfstest.py`'s `_fstest_res` splits a record into fields by the
//! pattern `(?:[^\s"]|"[^"]*")+`, strips one pair of enclosing quotes, and turns a value into an integer when Python's
//! `int(v, 0)` accepts it; [`parse_reply`] does the same, so a script's comparison (`r[0]["FileAttributes"] == 0x20`)
//! means here what it means upstream. A parser of external bytes, it checks the reply's length before it reads and
//! never panics on any input (Part 6 "Hostile input").

use std::collections::BTreeMap;

/// Derived: the most bytes of one driver reply this parser reads. The largest reply the scripts ask for is a stream
/// listing of 101 streams (`t/streams/02.t`), each line `StreamSize=… StreamName=":strmNN:$DATA"` under 64 bytes, so
/// about 6.5 KiB; a directory listing of four names is smaller. 64 KiB is ten times that, and a longer reply is a
/// broken driver, refused rather than read.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;

/// One field's value, as `int(v, 0)` leaves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
  /// A value Python's `int(v, 0)` accepts (`0x20`, `42`, `-1`).
  Int(i128),
  /// Any other value, with one pair of enclosing quotes stripped.
  Text(String),
}

/// One record: the fields of one line after the first.
pub type Fields = BTreeMap<String, Value>;

/// A driver reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
  /// The first line: `0` or the error's name.
  pub error: String,
  /// The records, one per non-empty later line.
  pub records: Vec<Fields>,
}

/// Why a reply was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyError {
  /// The reply is past [`MAX_REPLY_BYTES`].
  TooLarge {
    /// Its length.
    len: usize,
  },
  /// The reply has no first line: the driver printed nothing, which `winfstest.py` would end the script on.
  Empty,
}

impl std::fmt::Display for ReplyError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ReplyError::TooLarge { len } => {
        write!(
          f,
          "a reply of {len} bytes exceeds the cap of {MAX_REPLY_BYTES}"
        )
      }
      ReplyError::Empty => write!(f, "the driver printed no outcome line"),
    }
  }
}

/// Parses the driver's standard output, as `_fstest_res` does: line ends normalized, the first line the outcome, every
/// later non-empty line a record.
pub fn parse_reply(stdout: &str) -> Result<Reply, ReplyError> {
  if stdout.len() > MAX_REPLY_BYTES {
    return Err(ReplyError::TooLarge { len: stdout.len() });
  }
  let normalized = stdout.replace("\r\n", "\n").replace('\r', "\n");
  let mut lines = normalized.lines();
  let error = lines.next().ok_or(ReplyError::Empty)?.to_owned();
  let records = lines
    .filter(|line| !line.is_empty())
    .map(parse_fields)
    .collect();
  Ok(Reply { error, records })
}

/// The fields of one record line. A token without `=` is skipped: `winfstest.py` would end the script on it, and no
/// record `winfstest.c` prints has one.
pub fn parse_fields(line: &str) -> Fields {
  let mut fields = Fields::new();
  for token in tokens(line) {
    if let Some((key, raw)) = token.split_once('=') {
      fields.insert(key.to_owned(), value_of(raw));
    }
  }
  fields
}

/// The tokens of a line by the pattern `(?:[^\s"]|"[^"]*")+`: a run of characters that are neither white space nor a
/// quote, with any quoted stretch (quotes included) joined in. A quote with no partner matches nothing, so it ends the
/// token before it and is passed over.
fn tokens(line: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut current = String::new();
  let mut rest = line;
  while let Some(next) = rest.chars().next() {
    let width = next.len_utf8();
    if next.is_whitespace() {
      flush(&mut current, &mut out);
      rest = rest.get(width..).unwrap_or_default();
    } else if next == '"' {
      let after = rest.get(width..).unwrap_or_default();
      match after.find('"') {
        Some(close) => {
          current.push('"');
          current.push_str(after.get(..close).unwrap_or_default());
          current.push('"');
          rest = after.get(close.saturating_add(1)..).unwrap_or_default();
        }
        None => {
          flush(&mut current, &mut out);
          rest = after;
        }
      }
    } else {
      current.push(next);
      rest = rest.get(width..).unwrap_or_default();
    }
  }
  flush(&mut current, &mut out);
  out
}

fn flush(current: &mut String, out: &mut Vec<String>) {
  if !current.is_empty() {
    out.push(std::mem::take(current));
  }
}

/// A raw value as `winfstest.py` keeps it: one pair of enclosing quotes stripped, else an integer when `int(v, 0)`
/// takes it, else the text.
fn value_of(raw: &str) -> Value {
  if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
    let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
    return Value::Text(inner.to_owned());
  }
  match python_int(raw) {
    Some(number) => Value::Int(number),
    None => Value::Text(raw.to_owned()),
  }
}

/// Python 2's `int(text, 0)`: surrounding white space, an optional sign, then `0x`/`0o`/`0b` and digits of that base,
/// or a decimal with no leading zero, or zeros alone. Python 2 also reads a bare leading zero as octal (`017`); no
/// field `winfstest.c` prints has one, and such a value stays text here.
fn python_int(text: &str) -> Option<i128> {
  let text = text.trim();
  let (negative, digits) = match text.strip_prefix('-') {
    Some(rest) => (true, rest),
    None => (false, text.strip_prefix('+').unwrap_or(text)),
  };
  let lower = digits.to_ascii_lowercase();
  let (radix, body) = if let Some(body) = lower.strip_prefix("0x") {
    (16, body)
  } else if let Some(body) = lower.strip_prefix("0o") {
    (8, body)
  } else if let Some(body) = lower.strip_prefix("0b") {
    (2, body)
  } else if lower.len() > 1 && lower.starts_with('0') && !lower.chars().all(|c| c == '0') {
    return None;
  } else {
    (10, lower.as_str())
  };
  if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
    return None;
  }
  let magnitude = i128::from_str_radix(body, radix).ok()?;
  if negative {
    magnitude.checked_neg()
  } else {
    Some(magnitude)
  }
}

impl Reply {
  /// The first record's field `key`, as a script's `r[0][key]` reads it.
  pub fn first(&self, key: &str) -> Option<&Value> {
    self.records.first().and_then(|record| record.get(key))
  }

  /// The first record's integer field `key`.
  pub fn first_int(&self, key: &str) -> Option<i128> {
    match self.first(key) {
      Some(Value::Int(number)) => Some(*number),
      _ => None,
    }
  }

  /// The first record's text field `key`.
  pub fn first_text(&self, key: &str) -> Option<&str> {
    match self.first(key) {
      Some(Value::Text(text)) => Some(text),
      _ => None,
    }
  }

  /// The set of one text field over every record, as a script's `set(l[key] for l in r)` makes it.
  pub fn text_set(&self, key: &str) -> std::collections::BTreeSet<String> {
    self
      .records
      .iter()
      .filter_map(|record| match record.get(key) {
        Some(Value::Text(text)) => Some(text.clone()),
        Some(Value::Int(number)) => Some(number.to_string()),
        None => None,
      })
      .collect()
  }

  /// The records in one line each, for a case's diagnostic.
  pub fn describe(&self) -> String {
    let records: Vec<String> = self
      .records
      .iter()
      .map(|record| {
        record
          .iter()
          .map(|(key, value)| match value {
            Value::Int(number) => format!("{key}={number:#x}"),
            Value::Text(text) => format!("{key}={text:?}"),
          })
          .collect::<Vec<_>>()
          .join(" ")
      })
      .collect();
    format!("[{}]", records.join("; "))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A real `GetFileInformation` reply (winfstest.c's format: `FileAttributes=%#x`, times as text, sizes in decimal)
  /// with Windows line ends parses into the outcome and one record whose numbers are integers and whose times are text.
  #[test]
  fn a_file_information_reply_reads_as_the_script_reads_it() {
    let stdout = "0\r\nFileAttributes=0x20 CreationTime=2026-11-05T06:01:02Z LastAccessTime=2026-11-05T06:01:02Z \
      LastWriteTime=2026-11-05T06:01:02Z VolumeSerialNumber=0x5d1e FileSize=42 NumberOfLinks=1 FileIndex=0x1a2b\r\n";
    let reply = parse_reply(stdout).unwrap();
    assert_eq!(reply.error, "0");
    assert_eq!(reply.records.len(), 1);
    assert_eq!(reply.first_int("FileAttributes"), Some(0x20));
    assert_eq!(reply.first_int("FileSize"), Some(42));
    assert_eq!(reply.first_int("FileIndex"), Some(0x1a2b));
    assert_eq!(
      reply.first_text("LastWriteTime"),
      Some("2026-11-05T06:01:02Z")
    );
  }

  /// Quoted values keep their spaces and lose their quotes; a stream listing's records make the set a script tests.
  #[test]
  fn quoted_values_and_listings_read_whole() {
    let read = parse_reply("0\nLength=5 Data=\"41 42 43 44 45\"\n").unwrap();
    assert_eq!(read.first_int("Length"), Some(5));
    assert_eq!(read.first_text("Data"), Some("41 42 43 44 45"));
    let streams = parse_reply(
      "0\nStreamSize=0 StreamName=\"::$DATA\"\n\nStreamSize=0 StreamName=\":foo:$DATA\"\n",
    )
    .unwrap();
    let names = streams.text_set("StreamName");
    assert_eq!(names.len(), 2);
    assert!(names.contains("::$DATA") && names.contains(":foo:$DATA"));
  }

  /// An error reply carries the error's name and no records; a code the table lacks is `ERROR(n)`.
  #[test]
  fn an_error_reply_is_its_name() {
    let reply = parse_reply("ERROR_FILE_NOT_FOUND\r\n").unwrap();
    assert_eq!(reply.error, "ERROR_FILE_NOT_FOUND");
    assert!(reply.records.is_empty());
    assert_eq!(parse_reply("ERROR(4394)\n").unwrap().error, "ERROR(4394)");
  }

  /// `int(v, 0)`'s rules: hex, octal and binary prefixes, signs, zeros alone; a leading zero, a GUID, an empty value
  /// and a bare prefix stay text.
  #[test]
  fn integers_are_what_python_int_base_zero_accepts() {
    assert_eq!(value_of("0x420"), Value::Int(0x420));
    assert_eq!(value_of("0X1A"), Value::Int(0x1a));
    assert_eq!(value_of("0"), Value::Int(0));
    assert_eq!(value_of("000"), Value::Int(0));
    assert_eq!(value_of("-7"), Value::Int(-7));
    assert_eq!(value_of("0b101"), Value::Int(5));
    assert_eq!(value_of("0o17"), Value::Int(15));
    assert_eq!(value_of("017"), Value::Text("017".to_owned()));
    assert_eq!(value_of("0x"), Value::Text("0x".to_owned()));
    assert_eq!(value_of(""), Value::Text(String::new()));
    assert_eq!(
      value_of("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}"),
      Value::Text("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}".to_owned())
    );
    assert_eq!(
      value_of("IO_REPARSE_TAG_SYMLINK"),
      Value::Text("IO_REPARSE_TAG_SYMLINK".to_owned())
    );
  }

  /// Hostile input: an oversized reply is refused before it is read, an empty one has no outcome, an unpaired quote and
  /// tokens without `=` are passed over, and an integer past `i128` stays text; nothing panics.
  #[test]
  fn hostile_replies_are_refused_or_read_without_panic() {
    let huge = "0\n".to_owned() + &"x".repeat(MAX_REPLY_BYTES);
    assert_eq!(
      parse_reply(&huge),
      Err(ReplyError::TooLarge { len: huge.len() })
    );
    assert_eq!(parse_reply(""), Err(ReplyError::Empty));
    let torn = parse_reply("0\nA=1 B=\"unterminated C=2 lonely\n").unwrap();
    assert_eq!(torn.first_int("A"), Some(1));
    assert_eq!(
      torn.first_text("B"),
      Some(""),
      "an unpaired quote ends B's token: B= is empty text"
    );
    assert_eq!(torn.first_int("C"), Some(2));
    let wide = format!("0\nN=0x{}\n", "f".repeat(40));
    assert!(matches!(
      parse_reply(&wide).unwrap().first("N"),
      Some(Value::Text(_))
    ));
    assert!(parse_reply("0\n\u{feff}=\u{0}\"\"\"").is_ok());
  }
}
