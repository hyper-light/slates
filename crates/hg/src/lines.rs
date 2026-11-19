//! Lines as Python's `bytes.splitlines()` cuts them (A-569; A-545's listkeys reader, moved here so the capability
//! blob shares it): a line ends at `\n`, at `\r`, or at `\r\n` taken as one, and the end is not part of the line; a
//! last line without an end is a line, and nothing after a last end is one.

/// Format: a line end.
const NEWLINE: u8 = b'\n';

/// Format: a line end, and with a newline after it one end.
const RETURN: u8 = b'\r';

/// The lines of a text, in order.
#[derive(Clone, Debug)]
pub(crate) struct Lines<'a> {
  /// The text left.
  rest: &'a [u8],
}

/// The lines of `text`.
pub(crate) fn lines(text: &[u8]) -> Lines<'_> {
  Lines { rest: text }
}

impl<'a> Iterator for Lines<'a> {
  type Item = &'a [u8];

  fn next(&mut self) -> Option<&'a [u8]> {
    if self.rest.is_empty() {
      return None;
    }
    let end = self
      .rest
      .iter()
      .position(|byte| *byte == NEWLINE || *byte == RETURN)
      .unwrap_or(self.rest.len());
    let line = self.rest.get(..end)?;
    let skip = match (self.rest.get(end), self.rest.get(end.saturating_add(1))) {
      (Some(&RETURN), Some(&NEWLINE)) => 2,
      (Some(_), _) => 1,
      (None, _) => 0,
    };
    self.rest = self
      .rest
      .get(end.saturating_add(skip)..)
      .unwrap_or_default();
    Some(line)
  }
}

#[cfg(test)]
mod tests {
  use super::lines;

  /// A-569: do cut text ending in each line end, with an empty line, and without a last end. Expect Python's
  /// `splitlines()`: `\r\n` one end, the empty line kept, no line after a last end.
  #[test]
  fn lines_end_as_python_ends_them() {
    let cut: Vec<&[u8]> = lines(b"a\r\nb\rc\n\nd").collect();
    assert_eq!(cut, [&b"a"[..], b"b", b"c", b"", b"d"]);
    assert_eq!(lines(b"a\n").count(), 1);
    assert_eq!(lines(b"").count(), 0);
    assert_eq!(lines(b"\n").collect::<Vec<_>>(), [&b""[..]]);
  }
}
