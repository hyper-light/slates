//! Ref names as the protocol carries them (A-582, part 2; §4.17.2 "The repository in git's terms", item 6): `HEAD`, or a
//! name under `refs/` that git's own `check_refname_format` passes [gitprotocol-common "refname"; C: `refs.c`
//! `refname_disposition`, `check_refname_component`, `check_or_sanitize_refname`, v2.50.1].
//!
//! The document lists the rules; git's check adds two it leaves out, which no ref git makes can break: a name has no
//! empty component (`refs//x`, `refs/x/`), and no component ends with `.lock` (the document says it of the whole name).
//! A name is bytes, and any byte past ASCII is allowed, as git allows it.

/// Format: the one name outside `refs/` the protocol carries [gitprotocol-common: `refname = "HEAD"`].
const HEAD: &[u8] = b"HEAD";

/// Format: what every other name begins with [gitprotocol-common: `refname /= "refs/" <see discussion below>`].
const REFS: &[u8] = b"refs/";

/// Format: the byte between a name's components.
const SLASH: u8 = b'/';

/// Format: no component begins with it, and no name ends with it.
const DOT: u8 = b'.';

/// Format: no component ends with it (`LOCK_SUFFIX`).
const LOCK: &[u8] = b".lock";

/// Format: the pairs no component holds: `..` and `@{`.
const PAIRS: [&[u8]; 2] = [b"..", b"@{"];

/// Format: the printable bytes no name holds (`refname_disposition` 4 and 5, the pattern `*` refused as a name's):
/// space, `:`, `?`, `[`, `\`, `^`, `~` and `*`.
const FORBIDDEN: &[u8] = b" :?[\\^~*";

/// Format: the last ASCII control byte; every byte up to it is one, and so is DEL.
const LAST_CONTROL: u8 = 0x1f;

/// Format: DEL, ASCII's other control byte.
const DEL: u8 = 0x7f;

/// Format: the length of a pair [`PAIRS`] holds.
const PAIR_LEN: usize = 2;

/// Whether `name` is a ref name the protocol may carry.
pub(super) fn is_valid(name: &[u8]) -> bool {
  name == HEAD
    || (name.starts_with(REFS)
      && name.last() != Some(&DOT)
      && name.split(|byte| *byte == SLASH).all(component_is_valid))
}

/// Whether one component passes: not empty, not beginning with a dot, not ending with `.lock`, no forbidden byte and
/// no forbidden pair.
fn component_is_valid(component: &[u8]) -> bool {
  !component.is_empty()
    && component.first() != Some(&DOT)
    && !component.ends_with(LOCK)
    && !component.iter().any(|byte| is_forbidden(*byte))
    && !component
      .windows(PAIR_LEN)
      .any(|pair| PAIRS.contains(&pair))
}

/// Whether `byte` is one no name holds: a control byte or a forbidden printable one.
fn is_forbidden(byte: u8) -> bool {
  byte <= LAST_CONTROL || byte == DEL || FORBIDDEN.contains(&byte)
}

#[cfg(test)]
mod tests {
  use super::is_valid;

  /// A-582 part 2 (gitprotocol-common's list, and the two rules git's `check_refname_format` adds): do check names
  /// that pass and one breaking each rule. Expect: `HEAD` and names under `refs/` pass, bytes past ASCII too; every
  /// broken rule refused.
  #[test]
  fn a_ref_name_is_head_or_under_refs_by_gits_own_rules() {
    for name in [
      &b"HEAD"[..],
      b"refs/heads/main",
      b"refs/tags/v1.0",
      b"refs/heads/a.b/c-d_e",
      b"refs/for/main",
      "refs/heads/caf\u{e9}".as_bytes(),
      b"refs/heads/x.lockx",
      b"refs/heads/@",
    ] {
      assert!(is_valid(name), "{name:?}");
    }
    for name in [
      &b""[..],
      b"head",
      b"main",
      b"refs",
      b"refs/",
      b"refs//x",
      b"refs/x/",
      b"refs/.x",
      b"refs/x/.y",
      b"refs/x.",
      b"refs/a..b",
      b"refs/x.lock",
      b"refs/x.lock/y",
      b"refs/a@{b",
      b"refs/a b",
      b"refs/a:b",
      b"refs/a?b",
      b"refs/a[b",
      b"refs/a\\b",
      b"refs/a^b",
      b"refs/a~b",
      b"refs/a*b",
      b"refs/a\tb",
      b"refs/a\x7fb",
      b"refs/a\0b",
    ] {
      assert!(!is_valid(name), "{name:?}");
    }
  }
}
