//! Special dotfile blob checks as git's fsck checks them (A-590; §4.17.2 "A push", item 4; git v2.56.1 `fsck.c`'s
//! `fsck_gitmodules_fn` and `fsck_gitattributes_fn`, `submodule.c`'s `check_submodule_name` and
//! `check_submodule_url`, `config.c`, and `attr.h`'s `ATTR_MAX_FILE_SIZE` / `ATTR_MAX_LINE_LENGTH`).
//!
//! A tree naming `.gitmodules` or `.gitattributes` (in any spelling HFS+ or NTFS resolves, [`super::tree::Checked`])
//! names blobs whose contents a push must check strictly before accepting:
//! - `.gitmodules`: at most 1 MiB (`FSCK_GITMODULES_MAX_SIZE`), parsed as git config (`gitmodulesParse` on syntax
//!   faults), with `[submodule "<name>"]` held to git's security checks: `name` non-empty and free of `..` path
//!   components (`gitmodulesName`, CVE-2018-11235), `path` not beginning with `-` (`gitmodulesPath`), `url` not
//!   beginning with `-` and free of newlines, NULs and percent-encoded control bytes (`gitmodulesUrl`,
//!   CVE-2020-5260 / CVE-2020-11008), and `update` not beginning with `!` (`gitmodulesUpdate`).
//! - `.gitattributes`: at most 100 MiB (`ATTR_MAX_FILE_SIZE`) and every line shorter than 2048 bytes
//!   (`ATTR_MAX_LINE_LENGTH`).

use super::fsck::{FsckId, Report, Strictness};

/// Format: the largest `.gitmodules` blob git's fsck will parse (git v2.56.1 `fsck.c` `FSCK_GITMODULES_MAX_SIZE`,
/// 1 MiB).
pub const GITMODULES_MAX_SIZE: usize = 1 << 20;

/// Format: the largest `.gitattributes` blob git will parse (git v2.56.1 `attr.h` `ATTR_MAX_FILE_SIZE`, 100 MiB).
pub const GITATTRIBUTES_MAX_SIZE: usize = 100 * 1024 * 1024;

/// Format: the line length at or above which git refuses a `.gitattributes` line (git v2.56.1 `attr.h`
/// `ATTR_MAX_LINE_LENGTH`, 2048 bytes excluding the trailing LF).
pub const GITATTRIBUTES_MAX_LINE_LENGTH: usize = 2048;

/// Checks `bytes`, the content of a `.gitattributes` blob, as git's `fsck_blob` / `fsck_gitattributes` checks it
/// under `strictness`: size cap, then line-length cap.
pub fn fsck_gitattributes(bytes: &[u8], strictness: Strictness) -> Report {
  let mut report = Report::new(strictness);
  if bytes.len() > GITATTRIBUTES_MAX_SIZE {
    report.report(FsckId::GitattributesLarge);
    return report;
  }
  for line in bytes.split(|byte| *byte == b'\n') {
    if line.len() >= GITATTRIBUTES_MAX_LINE_LENGTH {
      report.report(FsckId::GitattributesLineLength);
      break;
    }
  }
  report
}

/// Checks `bytes`, the content of a `.gitmodules` blob, as git's `fsck_gitmodules_fn` checks it under `strictness`:
/// size cap, then git config parsing with every `[submodule "<name>"]` entry checked.
pub fn fsck_gitmodules(bytes: &[u8], strictness: Strictness) -> Report {
  let mut report = Report::new(strictness);
  if bytes.len() > GITMODULES_MAX_SIZE {
    report.report(FsckId::GitmodulesLarge);
    return report;
  }
  if parse_config(bytes, &mut report).is_err() {
    report.report(FsckId::GitmodulesParse);
  }
  report
}

/// One parsed `[section]` or `[section "subsection"]` header.
struct Section {
  /// Lowercased ASCII section name.
  name: Vec<u8>,
  /// Optional subsection name (exact case, unescaped).
  subsection: Option<Vec<u8>>,
}

/// Parses a git config buffer (`config.c`'s `git_parse_source`), validating every `[submodule "<name>"]` entry into
/// `report`. Returns `Err(())` on a config syntax error (reported as `gitmodulesParse`).
fn parse_config(bytes: &[u8], report: &mut Report) -> Result<(), ()> {
  let mut at = 0_usize;
  let mut section: Option<Section> = None;
  while at < bytes.len() {
    skip_ws(bytes, &mut at);
    let Some(&byte) = bytes.get(at) else {
      break;
    };
    match byte {
      b'\n' => at = at.saturating_add(1),
      b'#' | b';' => skip_to_eol(bytes, &mut at),
      b'[' => section = Some(parse_section(bytes, &mut at)?),
      _ => {
        let Some(current) = section.as_ref() else {
          return Err(());
        };
        let (key, value) = parse_variable(bytes, &mut at)?;
        check_entry(current, &key, value.as_deref(), report);
      }
    }
  }
  Ok(())
}

/// Checks one parsed config entry inside `section` against git's `fsck_gitmodules_fn` rules.
fn check_entry(section: &Section, key: &[u8], value: Option<&[u8]>, report: &mut Report) {
  if section.name != b"submodule" {
    return;
  }
  let Some(name) = section.subsection.as_deref() else {
    return;
  };
  if !submodule_name_ok(name) {
    report.report(FsckId::GitmodulesName);
  }
  let Some(value) = value else {
    return;
  };
  if key.eq_ignore_ascii_case(b"path") && looks_like_option(value) {
    report.report(FsckId::GitmodulesPath);
  } else if key.eq_ignore_ascii_case(b"url") && !submodule_url_ok(value) {
    report.report(FsckId::GitmodulesUrl);
  } else if key.eq_ignore_ascii_case(b"update") && value.starts_with(b"!") {
    report.report(FsckId::GitmodulesUpdate);
  }
}

/// Whether `name` is allowed as a submodule name (`submodule.c`'s `check_submodule_name`, CVE-2018-11235): non-empty
/// and containing no `..` component separated by `/` or `\`.
pub fn submodule_name_ok(name: &[u8]) -> bool {
  if name.is_empty() {
    return false;
  }
  !name
    .split(|byte| *byte == b'/' || *byte == b'\\')
    .any(|part| part == b"..")
}

/// Whether `value` starts with `-` (`looks_like_command_line_option`).
fn looks_like_option(value: &[u8]) -> bool {
  value.starts_with(b"-")
}

/// Format: URI scheme delimiter `"://"` checked by Git's `check_submodule_url`.
const SCHEME_SEPARATOR: &[u8] = b"://";

/// Format: byte length of one RFC 3986 percent-encoded triple `%XX`.
const PERCENT_ESCAPE_BYTES: usize = 3;

/// Format: hexadecimal value of `'a'`/`'A'` in base 16.
const HEX_ALPHA_VALUE: u8 = 10;

/// Whether `url` is allowed as a submodule URL (`submodule.c`'s `check_submodule_url`, CVE-2020-5260 /
/// CVE-2020-11008): not beginning with `-`, free of literal LF/CR/NUL, and (when carrying a `://` scheme) free of
/// percent-encoded LF/CR/NUL (`%0a`, `%0d`, `%00`).
pub fn submodule_url_ok(url: &[u8]) -> bool {
  if looks_like_option(url) || url.iter().any(|b| matches!(*b, b'\n' | b'\r' | b'\0')) {
    return false;
  }
  if url
    .windows(SCHEME_SEPARATOR.len())
    .any(|window| window == SCHEME_SEPARATOR)
    && has_percent_encoded_control(url)
  {
    return false;
  }
  true
}

/// Whether `url` contains a `%XX` escape decoding to LF (`0x0a`), CR (`0x0d`), or NUL (`0x00`).
fn has_percent_encoded_control(url: &[u8]) -> bool {
  let mut at = 0_usize;
  while at < url.len() {
    if url.get(at) == Some(&b'%')
      && let (Some(&hi), Some(&lo)) = (url.get(at.saturating_add(1)), url.get(at.saturating_add(2)))
      && let (Some(h), Some(l)) = (hex_nibble(hi), hex_nibble(lo))
    {
      let decoded = (h << 4) | l;
      if matches!(decoded, b'\n' | b'\r' | b'\0') {
        return true;
      }
      at = at.saturating_add(PERCENT_ESCAPE_BYTES);
      continue;
    }
    at = at.saturating_add(1);
  }
  false
}

/// Value of one ASCII hex digit.
fn hex_nibble(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte.saturating_sub(b'0')),
    b'a'..=b'f' => Some(byte.saturating_sub(b'a').saturating_add(HEX_ALPHA_VALUE)),
    b'A'..=b'F' => Some(byte.saturating_sub(b'A').saturating_add(HEX_ALPHA_VALUE)),
    _ => None,
  }
}

/// Skips horizontal whitespace (spaces, tabs, and CR).
fn skip_ws(bytes: &[u8], at: &mut usize) {
  while bytes
    .get(*at)
    .is_some_and(|byte| matches!(*byte, b' ' | b'\t' | b'\r'))
  {
    *at = at.saturating_add(1);
  }
}

/// Advances to the end of the current line (consuming the trailing LF if present).
fn skip_to_eol(bytes: &[u8], at: &mut usize) {
  while let Some(&byte) = bytes.get(*at) {
    *at = at.saturating_add(1);
    if byte == b'\n' {
      break;
    }
  }
}

/// Parses a `[section]`, `[section "subsection"]`, or legacy `[section.subsection]` header starting at `[`.
fn parse_section(bytes: &[u8], at: &mut usize) -> Result<Section, ()> {
  *at = at.saturating_add(1);
  let mut name = Vec::new();
  while let Some(&byte) = bytes.get(*at) {
    if byte.is_ascii_alphanumeric() || byte == b'-' {
      name.push(byte.to_ascii_lowercase());
      *at = at.saturating_add(1);
    } else {
      break;
    }
  }
  if name.is_empty() {
    return Err(());
  }
  let mut subsection = None;
  match bytes.get(*at) {
    Some(&b']') => {
      *at = at.saturating_add(1);
    }
    Some(&b'.') => {
      *at = at.saturating_add(1);
      subsection = Some(parse_legacy_subsection(bytes, at)?);
    }
    Some(&b' ' | &b'\t') => {
      skip_ws(bytes, at);
      if bytes.get(*at) != Some(&b'"') {
        return Err(());
      }
      subsection = Some(parse_quoted_subsection(bytes, at)?);
      if bytes.get(*at) != Some(&b']') {
        return Err(());
      }
      *at = at.saturating_add(1);
    }
    _ => return Err(()),
  }
  Ok(Section { name, subsection })
}

/// Parses a legacy `[section.subsection]` tail up to its closing `]`.
fn parse_legacy_subsection(bytes: &[u8], at: &mut usize) -> Result<Vec<u8>, ()> {
  let mut sub = Vec::new();
  while let Some(&byte) = bytes.get(*at) {
    if byte == b']' {
      *at = at.saturating_add(1);
      return Ok(sub);
    }
    if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.' {
      sub.push(byte.to_ascii_lowercase());
      *at = at.saturating_add(1);
    } else {
      return Err(());
    }
  }
  Err(())
}

/// Parses a quoted `"subsection"` inside a section header (`config.c`'s `get_extended_base_var`): `\x` yields `x`
/// for any non-newline byte; an unescaped newline or EOF before the closing `"` is a parse error.
fn parse_quoted_subsection(bytes: &[u8], at: &mut usize) -> Result<Vec<u8>, ()> {
  *at = at.saturating_add(1);
  let mut sub = Vec::new();
  while let Some(&byte) = bytes.get(*at) {
    *at = at.saturating_add(1);
    match byte {
      b'"' => return Ok(sub),
      b'\n' => return Err(()),
      b'\\' => {
        let Some(&escaped) = bytes.get(*at) else {
          return Err(());
        };
        if escaped == b'\n' {
          return Err(());
        }
        *at = at.saturating_add(1);
        sub.push(escaped);
      }
      other => sub.push(other),
    }
  }
  Err(())
}

/// Parses one `key` or `key = value` line (`config.c`'s `get_value` / `parse_value`).
fn parse_variable(bytes: &[u8], at: &mut usize) -> Result<(Vec<u8>, Option<Vec<u8>>), ()> {
  let Some(&first) = bytes.get(*at) else {
    return Err(());
  };
  if !first.is_ascii_alphabetic() {
    return Err(());
  }
  let mut key = Vec::new();
  while let Some(&byte) = bytes.get(*at) {
    if byte.is_ascii_alphanumeric() || byte == b'-' {
      key.push(byte.to_ascii_lowercase());
      *at = at.saturating_add(1);
    } else {
      break;
    }
  }
  skip_ws(bytes, at);
  match bytes.get(*at) {
    None => Ok((key, None)),
    Some(&b'\n') => {
      *at = at.saturating_add(1);
      Ok((key, None))
    }
    Some(&b'#' | &b';') => {
      skip_to_eol(bytes, at);
      Ok((key, None))
    }
    Some(&b'=') => {
      *at = at.saturating_add(1);
      let value = parse_value(bytes, at)?;
      Ok((key, Some(value)))
    }
    _ => Err(()),
  }
}

/// Parses the value after `=` (`config.c`'s `parse_value`): trims unquoted leading/trailing spaces and tabs, handles
/// quotes, line continuations, and git config escape sequences (`\\`, `\"`, `\n`, `\t`, `\b`).
fn parse_value(bytes: &[u8], at: &mut usize) -> Result<Vec<u8>, ()> {
  skip_ws(bytes, at);
  let mut value = Vec::new();
  let mut pending_space = Vec::new();
  let mut quoted = false;
  while let Some(&byte) = bytes.get(*at) {
    *at = at.saturating_add(1);
    match byte {
      b'\n' => {
        if quoted {
          return Err(());
        }
        return Ok(value);
      }
      b'#' | b';' if !quoted => {
        skip_to_eol(bytes, at);
        return Ok(value);
      }
      b'"' => {
        if !pending_space.is_empty() {
          value.extend_from_slice(&pending_space);
          pending_space.clear();
        }
        quoted = !quoted;
      }
      b'\\' => {
        if !pending_space.is_empty() {
          value.extend_from_slice(&pending_space);
          pending_space.clear();
        }
        parse_escape(bytes, at, &mut value)?;
      }
      b' ' | b'\t' if !quoted => pending_space.push(byte),
      b'\r' if !quoted && bytes.get(*at) == Some(&b'\n') => {}
      other => {
        if !pending_space.is_empty() {
          value.extend_from_slice(&pending_space);
          pending_space.clear();
        }
        value.push(other);
      }
    }
  }
  if quoted {
    return Err(());
  }
  Ok(value)
}

/// Parses one backslash escape inside a config value (`config.c`'s `parse_value`).
fn parse_escape(bytes: &[u8], at: &mut usize, value: &mut Vec<u8>) -> Result<(), ()> {
  let Some(&next) = bytes.get(*at) else {
    return Err(());
  };
  *at = at.saturating_add(1);
  match next {
    b'\n' => Ok(()),
    b'\r' if bytes.get(*at) == Some(&b'\n') => {
      *at = at.saturating_add(1);
      Ok(())
    }
    b'\\' => {
      value.push(b'\\');
      Ok(())
    }
    b'"' => {
      value.push(b'"');
      Ok(())
    }
    b'n' => {
      value.push(b'\n');
      Ok(())
    }
    b't' => {
      value.push(b'\t');
      Ok(())
    }
    b'b' => {
      value.push(b'\x08');
      Ok(())
    }
    _ => Err(()),
  }
}
