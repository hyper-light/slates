//! A command's parameters read as `svnserve` reads them: `marshal.c`'s `vparse_tuple` (Subversion's
//! `1.14.x` at `d434dc14`, 1.15.0 alike), ported for the reporter's commands (A-616).
//!
//! A format names one field a code: `c` and `s` a string, `w` a word, `b` a boolean word, `3` a
//! tristate word, `n` a number, `r` a revision, `l` a list; `(` and `)` a nested list; `?` that the
//! tuple may stop there. `vparse_tuple` reads the items and the format side by side. An item past
//! the format is ignored; an item of another kind than its code refuses the tuple; items that stop
//! at a `?` leave every field after it absent, up to the end of the list they stop in; items that
//! stop anywhere else refuse it. A number past 2^63 - 1 is no revision, as `svn_revnum_t` casts it:
//! the reporter reads such a revision as none.
//!
//! The formats are the reporter's own constants, so the nesting this reads through is theirs, never
//! the client's. No panic.

use crate::index::Revision;
use crate::ra_svn::Value;

/// The word a boolean or a tristate reads as true.
/// Format: `marshal.c`'s `str_true`.
const TRUE_WORD: &str = "true";

/// The word a boolean or a tristate reads as false.
/// Format: `marshal.c`'s `str_false`.
const FALSE_WORD: &str = "false";

/// One field as `vparse_tuple` assigns it; an absent optional field is the default it assigns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field<'a> {
  /// `c` or `s`: a string, or none.
  String(Option<&'a [u8]>),
  /// `w`: a word, or none.
  Word(Option<&'a str>),
  /// `b`: a boolean word; absent, false.
  Boolean(bool),
  /// `3`: a tristate word; absent, unknown.
  Tristate(Option<bool>),
  /// `r`: a revision; absent, or past what `svn_revnum_t` holds, none.
  Revision(Option<Revision>),
  /// `n`: a number, or none.
  Number(Option<u64>),
  /// `l`: a list, or none.
  List(Option<&'a [Value<'a>]>),
}

/// Why a tuple was refused: "Malformed network data" (`SVN_ERR_RA_SVN_MALFORMED_DATA`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Malformed;

/// Reads `items` against `format` as `vparse_tuple` reads them: every field of the format, in
/// order, or the refusal.
pub(super) fn read<'a>(items: &'a [Value<'a>], format: &[u8]) -> Result<Vec<Field<'a>>, Malformed> {
  let mut reader = Reader {
    format,
    at: 0,
    fields: Vec::new(),
  };
  reader.list(items)?;
  Ok(reader.fields)
}

/// What one code made of one item.
enum Step {
  /// The item was the code's kind and its field is assigned.
  Assigned,
  /// The code ends the list being read: the items past it are ignored.
  EndOfList,
  /// The item is not the code's kind.
  Mismatch,
}

/// A place in the format and the fields read so far.
struct Reader<'f, 'a> {
  format: &'f [u8],
  at: usize,
  fields: Vec<Field<'a>>,
}

impl<'a> Reader<'_, 'a> {
  /// Reads one list's items from the format's place, leaving the place at the `)` that ends the
  /// list's format, or at the format's end.
  fn list(&mut self, items: &'a [Value<'a>]) -> Result<(), Malformed> {
    let mut count = 0usize;
    while let (Some(&first), Some(item)) = (self.format.get(self.at), items.get(count)) {
      let code = if first == b'?' {
        self.at = self.at.saturating_add(1);
        match self.format.get(self.at) {
          Some(&code) => code,
          None => break,
        }
      } else {
        first
      };
      match self.field(code, item)? {
        Step::Assigned => {}
        Step::EndOfList => return Ok(()),
        Step::Mismatch => break,
      }
      self.at = self.at.saturating_add(1);
      count = count.saturating_add(1);
    }
    if self.format.get(self.at) == Some(&b'?') {
      return self.absent_rest();
    }
    match self.format.get(self.at) {
      None | Some(b')') => Ok(()),
      Some(_) => Err(Malformed),
    }
  }

  /// Assigns `code`'s field from `item`, recursing into a nested list.
  fn field(&mut self, code: u8, item: &'a Value<'a>) -> Result<Step, Malformed> {
    let field = match (code, item) {
      (b'(', Value::List(items)) => {
        self.at = self.at.saturating_add(1);
        self.list(items)?;
        return Ok(Step::Assigned);
      }
      (b')', _) => return Ok(Step::EndOfList),
      (b'c' | b's', Value::String(bytes)) => Field::String(Some(bytes)),
      (b'w', Value::Word(word)) => Field::Word(Some(word)),
      (b'b', Value::Word(word)) => match truth(word) {
        Some(value) => Field::Boolean(value),
        None => return Ok(Step::Mismatch),
      },
      (b'3', Value::Word(word)) => match truth(word) {
        Some(value) => Field::Tristate(Some(value)),
        None => return Ok(Step::Mismatch),
      },
      (b'n', Value::Number(number)) => Field::Number(Some(*number)),
      (b'r', Value::Number(number)) => Field::Revision(revision(*number)),
      (b'l', Value::List(items)) => Field::List(Some(items)),
      _ => return Ok(Step::Mismatch),
    };
    self.fields.push(field);
    Ok(Step::Assigned)
  }

  /// Assigns every field from the place to the end of the list it is in as absent, as
  /// `vparse_tuple` does when the items stop at a `?`.
  fn absent_rest(&mut self) -> Result<(), Malformed> {
    let mut nesting = 0usize;
    while let Some(&code) = self.format.get(self.at) {
      match code {
        b'?' => {}
        b'(' => nesting = nesting.saturating_add(1),
        b')' => match nesting.checked_sub(1) {
          Some(outer) => nesting = outer,
          None => return Ok(()),
        },
        code => self.fields.push(absent(code).ok_or(Malformed)?),
      }
      self.at = self.at.saturating_add(1);
    }
    Ok(())
  }
}

/// The field an absent optional `code` assigns.
fn absent(code: u8) -> Option<Field<'static>> {
  Some(match code {
    b'c' | b's' => Field::String(None),
    b'w' => Field::Word(None),
    b'b' => Field::Boolean(false),
    b'3' => Field::Tristate(None),
    b'r' => Field::Revision(None),
    b'n' => Field::Number(None),
    b'l' => Field::List(None),
    _ => return None,
  })
}

/// The truth a boolean word names, or none for another word.
fn truth(word: &str) -> Option<bool> {
  match word {
    TRUE_WORD => Some(true),
    FALSE_WORD => Some(false),
    _ => None,
  }
}

/// A number read as a revision: `svn_revnum_t` is a signed 64-bit integer, so a number past
/// 2^63 - 1 casts to a negative one, which names no revision (`SVN_IS_VALID_REVNUM`).
fn revision(number: u64) -> Option<Revision> {
  i64::try_from(number).is_ok().then_some(number)
}
