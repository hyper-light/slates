//! The commit's request and its editor commands, read as `svnserve` reads them (§4.17.4 "5. The wire" and item 3.2,
//! piece 7's commit; A-617).
//!
//! A commit over `svn://` is the main command `commit`, then the client drives the server's commit editor with editor
//! commands until `close-edit` or `abort-edit` (`subversion/libsvn_ra_svn/protocol` §3.1.1 and §3.1.2; Subversion's
//! `1.14.x` branch at `d434dc14`). This module turns each item the item codec ([`crate::ra_svn`]) read into a typed
//! value, refusing exactly what `svnserve` refuses and how:
//! - **The commit's request** (`serve.c` `commit` 1470-1583, its parameters read at 1495-1510):
//!   `( logmsg:string ? ( ( lock-path lock-token ) ... ) keep-locks:bool ? rev-props:proplist )`, read as
//!   `svn_ra_svn__parse_tuple(params, "clb?l")`, or a client before 1.2's one-item form `"c"`, which keeps its locks.
//!   Its revision properties are read as `svn_ra_svn__parse_proplist` reads them (`marshal.c`). Its lock tokens are
//!   read later, after the authentication request, as `add_lock_tokens` reads them ([`CommitRequest::lock_tokens`]): a
//!   malformed one there is a command failure, not the connection's end.
//! - **Each editor command** (`editorp.c` `svn_ra_svn_drive_editor2` 1073-1212 and its handlers 532-935): an item
//!   `( name:word params:list )` read as `svn_ra_svn__read_tuple(conn, pool, "wl", ...)`, then its parameters with the
//!   handler's own prototype, from `open-root`'s `"(?r)s"` to `close-file`'s `"s(?c)"`.
//!
//! Reading a prototype is `marshal.c` `vparse_tuple` (1608-1718): items past the prototype are ignored; `?` lets the
//! rest be absent; `( ... )` is a list whose own `?` lets it be empty; a word, number, string or list where the
//! prototype names another kind is "Malformed network data" ([`EditRefusal::Malformed`],
//! `SVN_ERR_RA_SVN_MALFORMED_DATA`), which ends the connection, since the drive returns it outside a command error. A
//! command no handler knows is "Unknown editor command" ([`EditRefusal::UnknownCommand`],
//! `SVN_ERR_RA_SVN_UNKNOWN_CMD`), which `svnserve` writes as a command failure, the edit aborted (1127-1132, written
//! at 1149-1174). Which command may come when, a token's kind, and every check the commit editor makes are the
//! drive's, not this module's.
//!
//! **Where this is stricter than `svnserve`**, each a hostile input no stock client sends:
//! - a C string (`vparse_tuple`'s `c`: a path, a property's name, a checksum, a copy's source, the log message)
//!   holding a NUL byte is refused, where `svnserve` reads it up to the NUL ([`Malformation::NulInString`]);
//! - a revision (`r`) past what `svn_revnum_t` holds is refused, where `svnserve` casts it to a negative number, which
//!   then reads as no revision at all ([`Malformation::RevisionPastRange`]).
//!
//! Each command also writes itself back as a stock client writes it (`marshal.c` `svn_ra_svn__write_cmd_*` 2020-2265,
//! `client.c` `ra_svn_commit`), so a stock session read and written back is the same bytes (module tests).
//!
//! No I/O, no clock, no randomness, no panic.

use crate::ra_svn::Value;

/// `SVN_ERR_RA_SVN_UNKNOWN_CMD`, "Unknown editor command '%s'".
/// Format: the ra_svn category's start, 120,000 + 18 × 5,000 = 210,000, plus 1 (`svn_error_codes.h`; research notes
/// §6).
const SVN_ERR_RA_SVN_UNKNOWN_CMD: u32 = 210_001;

/// `SVN_ERR_RA_SVN_MALFORMED_DATA`, "Malformed network data".
/// Format: the ra_svn category's start plus 4 (`svn_error_codes.h`).
const SVN_ERR_RA_SVN_MALFORMED_DATA: u32 = 210_004;

/// The greatest revision a revision number may name: what `svn_revnum_t`, a C `long`, holds on the 64-bit platforms
/// slates ships.
/// Format: `svn_types.h` `typedef long svn_revnum_t`; `vparse_tuple` casts an `r` number to it (1643-1644).
const LARGEST_REVISION: u64 = i64::MAX.unsigned_abs();

/// The word that is a boolean's true.
/// Format: `marshal.c` `str_true`, which `vparse_tuple`'s `b` compares with.
const TRUE: &str = "true";

/// The word that is a boolean's false.
/// Format: `marshal.c` `str_false`.
const FALSE: &str = "false";

/// The NUL byte, which ends a C string.
/// Format: C's string terminator, where `svnserve` stops reading a `c` item.
const NUL: u8 = 0;

/// What a refused item is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Malformation {
  /// Its shape is not the prototype's: an item missing, or of another kind than the prototype names (`vparse_tuple`'s
  /// "Malformed network data").
  Shape,
  /// A C string holding a NUL byte (stricter than `svnserve`, module documentation).
  NulInString,
  /// A revision past what `svn_revnum_t` holds (stricter than `svnserve`, module documentation).
  RevisionPastRange,
}

/// Why an item is not a request or a command, each answered with a Subversion error
/// ([`EditRefusal::subversion_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditRefusal {
  /// The item is not one `svnserve` reads (`SVN_ERR_RA_SVN_MALFORMED_DATA`): the connection ends.
  Malformed(Malformation),
  /// An editor command no handler knows (`SVN_ERR_RA_SVN_UNKNOWN_CMD`): a command failure, the edit aborted.
  UnknownCommand {
    /// The command's name.
    name: String,
  },
}

impl EditRefusal {
  /// The number of the Subversion error the refusal stands for.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::Malformed(_) => SVN_ERR_RA_SVN_MALFORMED_DATA,
      Self::UnknownCommand { .. } => SVN_ERR_RA_SVN_UNKNOWN_CMD,
    }
  }

  /// Whether `svnserve` ends the connection at it, rather than writing a command failure.
  pub fn ends_connection(&self) -> bool {
    matches!(self, Self::Malformed(_))
  }
}

/// The refusal for a shape that is not the prototype's.
fn shape() -> EditRefusal {
  EditRefusal::Malformed(Malformation::Shape)
}

/// A directory's or file's token: the string the client named it by when it opened it.
pub type Token<'a> = &'a [u8];

/// A copy's source as an editor command names it: a URL in a commit, an fspath in a replay (`editorp.c` 601-609), and
/// the revision it is read in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopiedFrom<'a> {
  /// The source's URL or path.
  pub path: &'a [u8],
  /// The source's revision.
  pub revision: u64,
}

/// One editor command (`protocol` §3.1.2), its strings borrowed from the bytes it was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditCommand<'a> {
  /// `target-rev ( rev )`.
  TargetRev {
    /// The revision.
    revision: u64,
  },
  /// `open-root ( [ rev ] root-token )`.
  OpenRoot {
    /// The revision the client holds the root at, if it named one.
    revision: Option<u64>,
    /// The root's token.
    root: Token<'a>,
  },
  /// `delete-entry ( path [ rev ] dir-token )`.
  DeleteEntry {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The revision the client holds the entry at, if it named one.
    revision: Option<u64>,
    /// The parent directory's token.
    directory: Token<'a>,
  },
  /// `add-dir ( path parent-token child-token [ copy-path copy-rev ] )`.
  AddDir {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The parent directory's token.
    parent: Token<'a>,
    /// The new directory's token.
    child: Token<'a>,
    /// Its copy source, when it is a copy.
    copied_from: Option<CopiedFrom<'a>>,
  },
  /// `open-dir ( path parent-token child-token [ rev ] )`.
  OpenDir {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The parent directory's token.
    parent: Token<'a>,
    /// The directory's token.
    child: Token<'a>,
    /// The revision the client holds it at, if it named one.
    revision: Option<u64>,
  },
  /// `change-dir-prop ( dir-token name [ value ] )`.
  ChangeDirProp {
    /// The directory's token.
    directory: Token<'a>,
    /// The property's name.
    name: &'a [u8],
    /// Its new value; none deletes it.
    value: Option<&'a [u8]>,
  },
  /// `close-dir ( dir-token )`.
  CloseDir {
    /// The directory's token.
    directory: Token<'a>,
  },
  /// `absent-dir ( path parent-token )`.
  AbsentDir {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The parent directory's token.
    parent: Token<'a>,
  },
  /// `add-file ( path dir-token file-token [ copy-path copy-rev ] )`.
  AddFile {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The parent directory's token.
    directory: Token<'a>,
    /// The new file's token.
    file: Token<'a>,
    /// Its copy source, when it is a copy.
    copied_from: Option<CopiedFrom<'a>>,
  },
  /// `open-file ( path dir-token file-token [ rev ] )`.
  OpenFile {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The parent directory's token.
    directory: Token<'a>,
    /// The file's token.
    file: Token<'a>,
    /// The revision the client holds it at, if it named one.
    revision: Option<u64>,
  },
  /// `apply-textdelta ( file-token [ base-checksum ] )`.
  ApplyTextdelta {
    /// The file's token.
    file: Token<'a>,
    /// The hex MD5 of the text the delta is against, if the client sent one.
    base_checksum: Option<&'a [u8]>,
  },
  /// `textdelta-chunk ( file-token chunk )`.
  TextdeltaChunk {
    /// The file's token.
    file: Token<'a>,
    /// The next bytes of the svndiff stream.
    chunk: &'a [u8],
  },
  /// `textdelta-end ( file-token )`.
  TextdeltaEnd {
    /// The file's token.
    file: Token<'a>,
  },
  /// `change-file-prop ( file-token name [ value ] )`.
  ChangeFileProp {
    /// The file's token.
    file: Token<'a>,
    /// The property's name.
    name: &'a [u8],
    /// Its new value; none deletes it.
    value: Option<&'a [u8]>,
  },
  /// `close-file ( file-token [ text-checksum ] )`.
  CloseFile {
    /// The file's token.
    file: Token<'a>,
    /// The hex MD5 of the file's new text, if the client sent one.
    text_checksum: Option<&'a [u8]>,
  },
  /// `absent-file ( path parent-token )`.
  AbsentFile {
    /// The path, relative to the session's.
    path: &'a [u8],
    /// The parent directory's token.
    parent: Token<'a>,
  },
  /// `close-edit ( )`.
  CloseEdit,
  /// `abort-edit ( )`.
  AbortEdit,
  /// `finish-replay ( )`: a replay's end, which a commit's drive does not take (`editorp.c` 922-935).
  FinishReplay,
  /// `failure ( ... )`: the peer's error status in place of a command (`editorp.c` 1115-1126).
  Failure,
}

/// The commit's request (`protocol` §3.1.1), its strings borrowed from the bytes it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRequest<'v, 'a> {
  /// The log message, which `svnserve` uses only when no revision properties came (`serve.c` 1526-1534).
  pub log_message: &'a [u8],
  /// The rest, absent in a client before 1.2's one-item form.
  pub rest: Option<CommitRest<'v, 'a>>,
}

/// The commit request's items after its log message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRest<'v, 'a> {
  /// The lock tokens as the client sent them, read only after the authentication request
  /// ([`CommitRequest::lock_tokens`]).
  pub lock_tokens: &'v [Value<'a>],
  /// Whether the client keeps its locks after the commit.
  pub keep_locks: bool,
  /// The revision properties, each name and value in the order sent; absent before 1.5.
  pub revision_properties: Option<Vec<(&'a [u8], &'a [u8])>>,
}

/// Why the commit's lock tokens are refused: `add_lock_tokens`' messages (`serve.c` 1360-1409), each
/// `SVN_ERR_RA_SVN_MALFORMED_DATA` written as a command failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LockTokensRefusal {
  /// "Lock tokens aren't a list of lists".
  NotListOfLists,
  /// "Lock path isn't a string".
  PathNotString,
  /// "Lock token isn't a string".
  TokenNotString,
  /// A pair of fewer than two items, which `add_lock_tokens` reads past its end (stricter than `svnserve`, module
  /// documentation).
  ShortPair,
  /// A path or token holding a NUL byte (stricter than `svnserve`).
  NulInString,
}

impl LockTokensRefusal {
  /// The number of the Subversion error the refusal stands for.
  pub fn subversion_code(self) -> u32 {
    SVN_ERR_RA_SVN_MALFORMED_DATA
  }
}

/// A list's items, read in order as `vparse_tuple` reads a prototype.
struct Tuple<'v, 'a> {
  /// The items.
  items: &'v [Value<'a>],
  /// The next one to read.
  next: usize,
}

impl<'v, 'a> Tuple<'v, 'a> {
  /// The items of `items`.
  fn of(items: &'v [Value<'a>]) -> Self {
    Self { items, next: 0 }
  }

  /// The next item; its absence is a shape the prototype refuses.
  fn item(&mut self) -> Result<&'v Value<'a>, EditRefusal> {
    let item = self.items.get(self.next).ok_or_else(shape)?;
    self.next = self.next.saturating_add(1);
    Ok(item)
  }

  /// Whether every item has been read.
  fn done(&self) -> bool {
    self.next >= self.items.len()
  }

  /// `s`: a string, any bytes.
  fn string(&mut self) -> Result<&'a [u8], EditRefusal> {
    match self.item()? {
      Value::String(bytes) => Ok(*bytes),
      _ => Err(shape()),
    }
  }

  /// `c`: a string read as a C string, a NUL byte in it refused.
  fn cstring(&mut self) -> Result<&'a [u8], EditRefusal> {
    let bytes = self.string()?;
    if bytes.contains(&NUL) {
      return Err(EditRefusal::Malformed(Malformation::NulInString));
    }
    Ok(bytes)
  }

  /// `w`: a word.
  fn word(&mut self) -> Result<&'a str, EditRefusal> {
    match self.item()? {
      Value::Word(word) => Ok(*word),
      _ => Err(shape()),
    }
  }

  /// `b`: the word `true` or `false`.
  fn boolean(&mut self) -> Result<bool, EditRefusal> {
    match self.word()? {
      TRUE => Ok(true),
      FALSE => Ok(false),
      _ => Err(shape()),
    }
  }

  /// `r`: a number read as a revision, refused past what `svn_revnum_t` holds.
  fn revision(&mut self) -> Result<u64, EditRefusal> {
    match self.item()? {
      Value::Number(number) if *number <= LARGEST_REVISION => Ok(*number),
      Value::Number(_) => Err(EditRefusal::Malformed(Malformation::RevisionPastRange)),
      _ => Err(shape()),
    }
  }

  /// `l`: a list.
  fn list(&mut self) -> Result<&'v [Value<'a>], EditRefusal> {
    match self.item()? {
      Value::List(items) => Ok(items.as_slice()),
      _ => Err(shape()),
    }
  }

  /// `( ? ... )`: a list that is empty, or holds what `read` reads from its start.
  fn optional<T>(
    &mut self,
    read: impl FnOnce(&mut Tuple<'v, 'a>) -> Result<T, EditRefusal>,
  ) -> Result<Option<T>, EditRefusal> {
    let items = self.list()?;
    if items.is_empty() {
      return Ok(None);
    }
    read(&mut Tuple::of(items)).map(Some)
  }
}

/// `(?r)`: a revision the client may leave out.
fn optional_revision(tuple: &mut Tuple<'_, '_>) -> Result<Option<u64>, EditRefusal> {
  tuple.optional(Tuple::revision)
}

/// `(?cr)`: a copy's source, or none.
fn optional_copy<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<Option<CopiedFrom<'a>>, EditRefusal> {
  tuple.optional(|inner| {
    Ok(CopiedFrom {
      path: inner.cstring()?,
      revision: inner.revision()?,
    })
  })
}

/// `(?s)`: a property's value, or none.
fn optional_value<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<Option<&'a [u8]>, EditRefusal> {
  tuple.optional(Tuple::string)
}

/// `(?c)`: a checksum, or none.
fn optional_checksum<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<Option<&'a [u8]>, EditRefusal> {
  tuple.optional(Tuple::cstring)
}

/// The command's name and parameters: `svn_ra_svn__read_tuple(conn, pool, "wl", ...)`.
fn command<'v, 'a>(item: &'v Value<'a>) -> Result<(&'a str, &'v [Value<'a>]), EditRefusal> {
  let Value::List(items) = item else {
    return Err(shape());
  };
  let mut tuple = Tuple::of(items);
  Ok((tuple.word()?, tuple.list()?))
}

impl<'a> EditCommand<'a> {
  /// The editor command `item` is, read with its handler's prototype.
  ///
  /// # Errors
  /// [`EditRefusal::Malformed`] for an item that is no command or whose parameters are not its prototype's, and
  /// [`EditRefusal::UnknownCommand`] for a name no handler knows.
  pub fn read(item: &Value<'a>) -> Result<Self, EditRefusal> {
    let (name, params) = command(item)?;
    let tuple = &mut Tuple::of(params);
    match name {
      "target-rev" => Ok(Self::TargetRev {
        revision: tuple.revision()?,
      }),
      "open-root" => open_root(tuple),
      "delete-entry" => delete_entry(tuple),
      "add-dir" => add_dir(tuple),
      "open-dir" => open_dir(tuple),
      "change-dir-prop" => change_dir_prop(tuple),
      "close-dir" => tuple.string().map(|directory| Self::CloseDir { directory }),
      "absent-dir" => absent(tuple).map(|(path, parent)| Self::AbsentDir { path, parent }),
      "add-file" => add_file(tuple),
      "open-file" => open_file(tuple),
      "apply-textdelta" => apply_textdelta(tuple),
      "textdelta-chunk" => textdelta_chunk(tuple),
      "textdelta-end" => tuple.string().map(|file| Self::TextdeltaEnd { file }),
      "change-file-prop" => change_file_prop(tuple),
      "close-file" => close_file(tuple),
      "absent-file" => absent(tuple).map(|(path, parent)| Self::AbsentFile { path, parent }),
      "close-edit" => Ok(Self::CloseEdit),
      "abort-edit" => Ok(Self::AbortEdit),
      "finish-replay" => Ok(Self::FinishReplay),
      "failure" => Ok(Self::Failure),
      _ => Err(EditRefusal::UnknownCommand {
        name: name.to_owned(),
      }),
    }
  }

  /// The command's name, as the protocol spells it.
  pub fn name(&self) -> &'static str {
    self.parts().0
  }

  /// The command as a stock client writes it (`marshal.c` `svn_ra_svn__write_cmd_*`). A failure is written with no
  /// parameters, since a commit's drive keeps none of them.
  pub fn to_value(&self) -> Value<'a> {
    let (name, params) = self.parts();
    Value::List(vec![Value::Word(name), Value::List(params)])
  }

  /// The command's name and its parameters' items.
  fn parts(&self) -> (&'static str, Vec<Value<'a>>) {
    match *self {
      Self::TargetRev { revision } => ("target-rev", vec![Value::Number(revision)]),
      Self::OpenRoot { revision, root } => (
        "open-root",
        vec![optional_number(revision), Value::String(root)],
      ),
      Self::DeleteEntry {
        path,
        revision,
        directory,
      } => (
        "delete-entry",
        vec![
          Value::String(path),
          optional_number(revision),
          Value::String(directory),
        ],
      ),
      Self::AddDir {
        path,
        parent,
        child,
        copied_from,
      } => ("add-dir", added(path, parent, child, copied_from)),
      Self::OpenDir {
        path,
        parent,
        child,
        revision,
      } => ("open-dir", opened(path, parent, child, revision)),
      Self::ChangeDirProp {
        directory,
        name,
        value,
      } => ("change-dir-prop", changed(directory, name, value)),
      Self::CloseDir { directory } => ("close-dir", vec![Value::String(directory)]),
      Self::AbsentDir { path, parent } => (
        "absent-dir",
        vec![Value::String(path), Value::String(parent)],
      ),
      Self::AddFile {
        path,
        directory,
        file,
        copied_from,
      } => ("add-file", added(path, directory, file, copied_from)),
      Self::OpenFile {
        path,
        directory,
        file,
        revision,
      } => ("open-file", opened(path, directory, file, revision)),
      Self::ApplyTextdelta {
        file,
        base_checksum,
      } => (
        "apply-textdelta",
        vec![Value::String(file), optional_string(base_checksum)],
      ),
      Self::TextdeltaChunk { file, chunk } => (
        "textdelta-chunk",
        vec![Value::String(file), Value::String(chunk)],
      ),
      Self::TextdeltaEnd { file } => ("textdelta-end", vec![Value::String(file)]),
      Self::ChangeFileProp { file, name, value } => {
        ("change-file-prop", changed(file, name, value))
      }
      Self::CloseFile {
        file,
        text_checksum,
      } => (
        "close-file",
        vec![Value::String(file), optional_string(text_checksum)],
      ),
      Self::AbsentFile { path, parent } => (
        "absent-file",
        vec![Value::String(path), Value::String(parent)],
      ),
      Self::CloseEdit => ("close-edit", Vec::new()),
      Self::AbortEdit => ("abort-edit", Vec::new()),
      Self::FinishReplay => ("finish-replay", Vec::new()),
      Self::Failure => ("failure", Vec::new()),
    }
  }
}

/// `open-root`'s `"(?r)s"` (`editorp.c` 555).
fn open_root<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let revision = optional_revision(tuple)?;
  Ok(EditCommand::OpenRoot {
    revision,
    root: tuple.string()?,
  })
}

/// `delete-entry`'s `"c(?r)s"` (574).
fn delete_entry<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let path = tuple.cstring()?;
  let revision = optional_revision(tuple)?;
  Ok(EditCommand::DeleteEntry {
    path,
    revision,
    directory: tuple.string()?,
  })
}

/// A path and two tokens, the start of `add-dir`'s, `open-dir`'s, `add-file`'s and `open-file`'s `"css..."`.
fn node<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<(&'a [u8], Token<'a>, Token<'a>), EditRefusal> {
  let path = tuple.cstring()?;
  let parent = tuple.string()?;
  Ok((path, parent, tuple.string()?))
}

/// `add-dir`'s `"css(?cr)"` (595).
fn add_dir<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let (path, parent, child) = node(tuple)?;
  Ok(EditCommand::AddDir {
    path,
    parent,
    child,
    copied_from: optional_copy(tuple)?,
  })
}

/// `open-dir`'s `"css(?r)"` (630).
fn open_dir<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let (path, parent, child) = node(tuple)?;
  Ok(EditCommand::OpenDir {
    path,
    parent,
    child,
    revision: optional_revision(tuple)?,
  })
}

/// `add-file`'s `"css(?cr)"` (710).
fn add_file<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let (path, directory, file) = node(tuple)?;
  Ok(EditCommand::AddFile {
    path,
    directory,
    file,
    copied_from: optional_copy(tuple)?,
  })
}

/// `open-file`'s `"css(?r)"` (746).
fn open_file<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let (path, directory, file) = node(tuple)?;
  Ok(EditCommand::OpenFile {
    path,
    directory,
    file,
    revision: optional_revision(tuple)?,
  })
}

/// A token, a name and a value or none: `change-dir-prop`'s and `change-file-prop`'s `"sc(?s)"`.
fn property<'a>(
  tuple: &mut Tuple<'_, 'a>,
) -> Result<(Token<'a>, &'a [u8], Option<&'a [u8]>), EditRefusal> {
  let token = tuple.string()?;
  let name = tuple.cstring()?;
  Ok((token, name, optional_value(tuple)?))
}

/// `change-dir-prop`'s `"sc(?s)"` (652).
fn change_dir_prop<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let (directory, name, value) = property(tuple)?;
  Ok(EditCommand::ChangeDirProp {
    directory,
    name,
    value,
  })
}

/// `change-file-prop`'s `"sc(?s)"` (838).
fn change_file_prop<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let (file, name, value) = property(tuple)?;
  Ok(EditCommand::ChangeFileProp { file, name, value })
}

/// `apply-textdelta`'s `"s(?c)"` (774).
fn apply_textdelta<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let file = tuple.string()?;
  Ok(EditCommand::ApplyTextdelta {
    file,
    base_checksum: optional_checksum(tuple)?,
  })
}

/// `textdelta-chunk`'s `"ss"` (798).
fn textdelta_chunk<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let file = tuple.string()?;
  Ok(EditCommand::TextdeltaChunk {
    file,
    chunk: tuple.string()?,
  })
}

/// `close-file`'s `"s(?c)"` (856).
fn close_file<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<EditCommand<'a>, EditRefusal> {
  let file = tuple.string()?;
  Ok(EditCommand::CloseFile {
    file,
    text_checksum: optional_checksum(tuple)?,
  })
}

/// `absent-dir`'s and `absent-file`'s `"cs"` (691, 878).
fn absent<'a>(tuple: &mut Tuple<'_, 'a>) -> Result<(&'a [u8], Token<'a>), EditRefusal> {
  let path = tuple.cstring()?;
  Ok((path, tuple.string()?))
}

/// `( [ number ] )`, as `write_tuple_revision_opt` within a list writes it.
fn optional_number<'a>(number: Option<u64>) -> Value<'a> {
  Value::List(number.map(Value::Number).into_iter().collect())
}

/// `( [ string ] )`, as `write_tuple_cstring_opt` and `write_tuple_string_opt_list` write it.
fn optional_string(string: Option<&[u8]>) -> Value<'_> {
  Value::List(string.map(Value::String).into_iter().collect())
}

/// `write_cmd_add_node`'s items.
fn added<'a>(
  path: &'a [u8],
  parent: Token<'a>,
  child: Token<'a>,
  copied_from: Option<CopiedFrom<'a>>,
) -> Vec<Value<'a>> {
  let copy = copied_from
    .map(|source| vec![Value::String(source.path), Value::Number(source.revision)])
    .unwrap_or_default();
  vec![
    Value::String(path),
    Value::String(parent),
    Value::String(child),
    Value::List(copy),
  ]
}

/// `write_cmd_open_node`'s items.
fn opened<'a>(
  path: &'a [u8],
  parent: Token<'a>,
  child: Token<'a>,
  revision: Option<u64>,
) -> Vec<Value<'a>> {
  vec![
    Value::String(path),
    Value::String(parent),
    Value::String(child),
    optional_number(revision),
  ]
}

/// `write_cmd_change_node_prop`'s items.
fn changed<'a>(token: Token<'a>, name: &'a [u8], value: Option<&'a [u8]>) -> Vec<Value<'a>> {
  vec![
    Value::String(token),
    Value::String(name),
    optional_string(value),
  ]
}

impl<'v, 'a> CommitRequest<'v, 'a> {
  /// The request `params`, the `commit` command's parameter list, is: `"c"` when it holds one item, else `"clb?l"`, its
  /// revision properties read as `svn_ra_svn__parse_proplist` reads them (each a list `"ss"`).
  ///
  /// # Errors
  /// [`EditRefusal::Malformed`] for parameters that are not the prototype's, which ends the connection.
  pub fn read(params: &'v [Value<'a>]) -> Result<Self, EditRefusal> {
    let mut tuple = Tuple::of(params);
    let log_message = tuple.cstring()?;
    if params.len() == 1 {
      return Ok(Self {
        log_message,
        rest: None,
      });
    }
    let lock_tokens = tuple.list()?;
    let keep_locks = tuple.boolean()?;
    let revision_properties = if tuple.done() {
      None
    } else {
      Some(proplist(tuple.list()?)?)
    };
    Ok(Self {
      log_message,
      rest: Some(CommitRest {
        lock_tokens,
        keep_locks,
        revision_properties,
      }),
    })
  }

  /// Whether the client keeps its locks: the one-item form always does (`serve.c` 1501).
  pub fn keep_locks(&self) -> bool {
    self.rest.as_ref().is_none_or(|rest| rest.keep_locks)
  }

  /// The lock tokens, each a path and a token in the order sent, read as `add_lock_tokens` reads them.
  ///
  /// # Errors
  /// [`LockTokensRefusal`] for a list that is not one of pairs of strings, which `svnserve` writes as a command
  /// failure.
  pub fn lock_tokens(&self) -> Result<Vec<(&'a [u8], &'a [u8])>, LockTokensRefusal> {
    let Some(rest) = &self.rest else {
      return Ok(Vec::new());
    };
    rest.lock_tokens.iter().map(lock_token).collect()
  }

  /// The request as a stock client writes it (`client.c` `ra_svn_commit`).
  pub fn to_value(&self) -> Value<'a> {
    let mut params = vec![Value::String(self.log_message)];
    if let Some(rest) = &self.rest {
      params.push(Value::List(rest.lock_tokens.to_vec()));
      params.push(Value::Word(if rest.keep_locks { TRUE } else { FALSE }));
      if let Some(properties) = &rest.revision_properties {
        let pairs = properties
          .iter()
          .map(|&(name, value)| Value::List(vec![Value::String(name), Value::String(value)]))
          .collect();
        params.push(Value::List(pairs));
      }
    }
    Value::List(vec![Value::Word("commit"), Value::List(params)])
  }
}

/// `svn_ra_svn__parse_proplist`: each item a list read as `"ss"`.
fn proplist<'a>(items: &[Value<'a>]) -> Result<Vec<(&'a [u8], &'a [u8])>, EditRefusal> {
  items
    .iter()
    .map(|item| {
      let Value::List(pair) = item else {
        return Err(shape());
      };
      let mut tuple = Tuple::of(pair);
      Ok((tuple.string()?, tuple.string()?))
    })
    .collect()
}

/// One lock token, as `add_lock_tokens` reads it: a list whose first item is the path and second the token.
fn lock_token<'a>(item: &Value<'a>) -> Result<(&'a [u8], &'a [u8]), LockTokensRefusal> {
  let Value::List(pair) = item else {
    return Err(LockTokensRefusal::NotListOfLists);
  };
  let Some(path_item) = pair.first() else {
    return Err(LockTokensRefusal::ShortPair);
  };
  let Value::String(path) = path_item else {
    return Err(LockTokensRefusal::PathNotString);
  };
  let Some(token_item) = pair.get(1) else {
    return Err(LockTokensRefusal::ShortPair);
  };
  let Value::String(token) = token_item else {
    return Err(LockTokensRefusal::TokenNotString);
  };
  if path.contains(&NUL) || token.contains(&NUL) {
    return Err(LockTokensRefusal::NulInString);
  }
  Ok((*path, *token))
}

#[cfg(test)]
mod tests;
