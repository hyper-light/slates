//! A tag's name (A-590), checked as git's tag check checks it: `refs/tags/<name>` by `check_refname_format` with no
//! flags (refs.c), the rules `git check-ref-format` documents, by git's own table of what each byte does in a name.

/// What refs.c's `refname_disposition` table says of a byte inside a ref name's component.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Disposition {
  /// Allowed.
  Plain,
  /// Ends the component: a `/`, or the NUL that ends a C string.
  End,
  /// `.`: refused after another `.`.
  Dot,
  /// `{`: refused after `@`.
  Brace,
  /// `*`: refused, since this check is not a refspec pattern's.
  Star,
  /// Refused anywhere.
  Forbidden,
}

/// Format: the printable bytes `refname_disposition` forbids anywhere (its 4s besides the control characters): space,
/// `:`, `?`, `[`, `\`, `^` and `~`.
const FORBIDDEN: &[u8] = b" :?[\\^~";

/// Format: the suffix a component may not end with (refs.c's `LOCK_SUFFIX`).
const LOCK_SUFFIX: &[u8] = b".lock";

/// refs.c's `refname_disposition` for `byte`: the control characters and DEL are forbidden with [`FORBIDDEN`], and the
/// bytes from 0x80 up are allowed.
fn disposition(byte: u8) -> Disposition {
  match byte {
    b'\0' | b'/' => Disposition::End,
    b'.' => Disposition::Dot,
    b'{' => Disposition::Brace,
    b'*' => Disposition::Star,
    _ if byte.is_ascii_control() || FORBIDDEN.contains(&byte) => Disposition::Forbidden,
    _ => Disposition::Plain,
  }
}

/// Whether one component, the bytes between two `/`, is allowed (`check_refname_component`): none of its bytes
/// forbidden, no `..` and no `@{` in it, not empty, not starting with `.`, not ending with `.lock`.
fn component_ok(component: &[u8]) -> bool {
  let mut last = b'\0';
  for byte in component {
    match disposition(*byte) {
      Disposition::Forbidden | Disposition::Star | Disposition::End => return false,
      Disposition::Dot if last == b'.' => return false,
      Disposition::Brace if last == b'@' => return false,
      Disposition::Plain | Disposition::Dot | Disposition::Brace => {}
    }
    last = *byte;
  }
  !component.is_empty() && component.first() != Some(&b'.') && !component.ends_with(LOCK_SUFFIX)
}

/// Whether `refs/tags/<name>` is a ref name git accepts (`check_or_sanitize_refname` with no flags): `refs` and `tags`
/// are, so each component of `name` must be, and the last may not end with `.`. The name is never the whole ref name,
/// so git's rule against a ref named `@` alone does not reach it, and there are always the two components git asks
/// for.
pub(crate) fn tag_name_ok(name: &[u8]) -> bool {
  name.split(|byte| *byte == b'/').all(component_ok) && name.last() != Some(&b'.')
}
