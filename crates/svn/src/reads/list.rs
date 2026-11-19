//! `list` (§4.17.4 "4. Reads": `list` reads the node rows; A-613 part 2): the dirents of a path and of what lies beneath
//! it to a depth, as `svnserve`'s `list` and `svn_repos_list` write them (`serve.c`, `libsvn_repos/list.c`, Subversion
//! 1.15.0), held to stock `svnserve`'s transcripts (`crates/svn/fixtures/reads`, sessions `list` and `hostile-list`):
//! - The parameters are `c(?r)w?l?l`: the path, the revision (the youngest when absent), the depth word, then,
//!   optionally, the dirent fields (each a word, any word but the six ignored; every field when the list is absent)
//!   and the patterns (each a string, read to its first NUL). Another shape ends the connection.
//! - After the authentication request the revision is opened: past the youngest it is refused at once, before `done`.
//!   Everything after is the walk's, and ends with `done` then `( success ( ) )` or the walk's refusal.
//! - The walk is `svn_repos_list`'s: a depth below `empty` refused, "Invalid depth 'N' in svn_repos_list" (165002;
//!   `svn_depth_from_word` reads `exclude` as -1 and `unknown`, or any word it does not know, as -2); an absent path
//!   refused, "Path 'P' not found" (160013); a file listed alone; an empty pattern list answering nothing; the path
//!   itself reported when its name matches; then its entries depth-first, each directory's in byte order of the names,
//!   directories left out at depth `files`, a file that matches no pattern left out, a directory that matches none
//!   walked but not reported, recursion at depth `infinity` alone.
//! - A dirent is `( path kind ( size ) ( has-props ) ( created-rev ) ( date ) ( author ) )`, its path absolute, each
//!   list empty when its field is not asked, the kind `unknown` when it is not; with no field beyond the kind asked it is
//!   `( path kind )` and nothing is read but the kind. Otherwise `fill_dirent` reads every field whichever are asked: a
//!   directory's size is Subversion's invalid size written unsigned, the date the created revision's `svn:date` parsed
//!   and written again (time 0's for a revision without one; a date that does not parse refuses the listing).
//! - A name matches a pattern as `svn_utf__fuzzy_glob_match` matches it: folded, then APR's `apr_fnmatch` with no
//!   flags. The fold is utf8proc's decomposition with case folding and marks stripped, composed again; over ASCII that is
//!   the letters `A` to `Z` lowered and nothing else, which is what this port does. A name beyond ASCII is matched with
//!   its ASCII letters lowered alone: utf8proc's case folding and mark stripping need Unicode's tables, owed (GAPS).
//!
//! The walk is a [`super::Stream`]: each slice ends once the bytes it wrote and the entries it visited reach the room the
//! caller gives, so a slice's output and work are bounded by the room. What it keeps is one level of names per directory
//! on the path from the listed one, as `svn_repos_list` keeps them.

use super::stream::{Progress, Stream};
use super::{
  AUTHOR_PROPERTY, DATE_PROPERTY, DIRECTORY_SIZE, DIRENT_FIELDS, DONE, Failure, Outcome, Reads,
  Repository, SEPARATOR, SVN_ERR_FS_NOT_FOUND, bool_word, c_string_of, child_path, format_time,
  kind_word, parse_time, path_and_revision,
};
use crate::authz::fnmatch;
use crate::index::{Node, NodeKind, Revision};
use crate::ra_svn::{ItemError, Value, write};

/// `SVN_ERR_REPOS_BAD_ARGS`.
/// Format: the repository category's start, 120,000 + 9 × 5,000 = 165,000, plus 2 (`svn_error_codes.h`); recorded
/// (fixture `list`).
const SVN_ERR_REPOS_BAD_ARGS: u32 = 165_002;

/// The kind word of a dirent whose kind was not asked for.
/// Format: `svn_ra_svn__write_dirent` (`marshal.c`); recorded (fixture `list`).
const UNKNOWN_KIND: &str = "unknown";

/// `svn_depth_exclude`, the number `svn_depth_from_word` gives `exclude`.
/// Format: `svn_types.h`'s `svn_depth_t`; recorded (fixture `list`: "Invalid depth '-1' in svn_repos_list").
const DEPTH_EXCLUDE: i8 = -1;

/// `svn_depth_unknown`, the number `svn_depth_from_word` gives `unknown` and every word it does not know.
/// Format: `svn_types.h`'s `svn_depth_t`; recorded (fixture `list`: "Invalid depth '-2' in svn_repos_list").
const DEPTH_UNKNOWN: i8 = -2;

/// A depth `svn_repos_list` walks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Depth {
  /// The path alone.
  Empty,
  /// The path and the files in it.
  Files,
  /// The path and everything in it.
  Immediates,
  /// The path and everything beneath it.
  Infinity,
}

/// `svn_depth_from_word`: the depth a word names, or the negative number of one `svn_repos_list` refuses.
fn depth_of(word: &str) -> Result<Depth, i8> {
  match word {
    "empty" => Ok(Depth::Empty),
    "files" => Ok(Depth::Files),
    "immediates" => Ok(Depth::Immediates),
    "infinity" => Ok(Depth::Infinity),
    "exclude" => Err(DEPTH_EXCLUDE),
    _ => Err(DEPTH_UNKNOWN),
  }
}

/// The dirent fields a `list` asks for (`SVN_DIRENT_*`), each written only when asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DirentFields {
  /// `kind`.
  kind: bool,
  /// `size`.
  size: bool,
  /// `has-props`.
  has_properties: bool,
  /// `created-rev`.
  created: bool,
  /// `time`.
  time: bool,
  /// `last-author`.
  author: bool,
}

impl DirentFields {
  /// Every field: what a request without a list asks for (`SVN_DIRENT_ALL`).
  fn all() -> Self {
    Self {
      kind: true,
      size: true,
      has_properties: true,
      created: true,
      time: true,
      author: true,
    }
  }

  /// `parse_dirent_fields`: the fields a list of words names, other words ignored; `None` for an item that is no word,
  /// "Dirent field not a word".
  fn of(words: &[Value<'_>]) -> Option<Self> {
    let mut fields = Self::default();
    let [kind, size, has_properties, created, time, author] = DIRENT_FIELDS;
    for word in words {
      let Value::Word(word) = word else {
        return None;
      };
      fields.kind |= *word == kind;
      fields.size |= *word == size;
      fields.has_properties |= *word == has_properties;
      fields.created |= *word == created;
      fields.time |= *word == time;
      fields.author |= *word == author;
    }
    Some(fields)
  }

  /// Whether a field beyond the kind is asked: then the dirent is filled (`path_info_only` is false) and written whole.
  fn beyond_kind(self) -> bool {
    self.size || self.has_properties || self.created || self.time || self.author
  }
}

/// The patterns of a request: none asked, or the list, each read to its first NUL.
type Patterns = Option<Vec<Vec<u8>>>;

/// A parsed `list` request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ListRequest {
  /// The path, the session's base joined with the given path.
  path: String,
  /// The revision, the youngest when absent.
  revision: Option<i64>,
  /// The depth, or the number of one refused.
  depth: Result<Depth, i8>,
  /// The dirent fields asked for.
  fields: DirentFields,
  /// The patterns.
  patterns: Patterns,
}

impl ListRequest {
  /// `svn_ra_svn__parse_tuple(params, "c(?r)w?l?l")`, then `parse_dirent_fields` and the patterns ("Pattern field not a
  /// string" is malformed data).
  fn parse(params: &[Value<'_>], base: &str) -> Option<Self> {
    let (path, revision) = path_and_revision(params, base)?;
    let Some(Value::Word(word)) = params.get(2) else {
      return None;
    };
    let fields = match params.get(3) {
      None => DirentFields::all(),
      Some(Value::List(words)) => DirentFields::of(words)?,
      Some(_) => return None,
    };
    let patterns = match params.get(4) {
      None => None,
      Some(Value::List(items)) => Some(patterns_of(items)?),
      Some(_) => return None,
    };
    Some(Self {
      path,
      revision,
      depth: depth_of(word),
      fields,
      patterns,
    })
  }
}

/// The patterns of a list, each a string read to its first NUL; `None` for an item that is no string.
fn patterns_of(items: &[Value<'_>]) -> Option<Vec<Vec<u8>>> {
  items
    .iter()
    .map(|item| match item {
      Value::String(bytes) => Some(c_string_of(bytes).to_vec()),
      _ => None,
    })
    .collect()
}

impl<R: Repository + ?Sized> Reads<'_, R> {
  /// `list ( path [ rev ] depth ? ( field ... ) ? ( pattern ... ) )` (`serve.c` `list`): the authentication request, the
  /// revision opened, then a [`Stream`] of the walk.
  pub(super) fn list(&mut self, params: &[Value<'_>]) -> Result<Outcome, ItemError> {
    let Some(request) = ListRequest::parse(params, self.base) else {
      return Ok(Outcome::Close);
    };
    self.trivial_auth()?;
    match self.at_or_youngest(request.revision) {
      Ok(revision) => Ok(Outcome::Streaming(Stream::list(ListStream::new(
        request, revision,
      )))),
      Err(failure) => self.failure(&failure),
    }
  }
}

/// One entry of a directory the walk keeps: its name, its kind, and whether its name matches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ListEntry {
  /// Its name.
  name: String,
  /// Its kind.
  kind: NodeKind,
  /// Whether it is reported.
  matches: bool,
}

/// One directory on the walk's path: its entries in byte order of their names and the next to visit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Level {
  /// The directory's path.
  directory: String,
  /// Its entries the walk keeps.
  entries: Vec<ListEntry>,
  /// The next entry to visit.
  next: usize,
}

/// Where the walk stands.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Walk {
  /// Not begun: the path's checks and its own dirent come first.
  Start,
  /// Walking: the directories on the path, the deepest last.
  Levels(Vec<Level>),
  /// Done, `done` and the answer written.
  Ended,
}

/// A `list` walk still to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ListStream {
  /// The listed path.
  path: String,
  /// The revision.
  revision: Revision,
  /// The depth, or the number of one refused.
  depth: Result<Depth, i8>,
  /// The dirent fields asked for.
  fields: DirentFields,
  /// The patterns.
  patterns: Patterns,
  /// Where the walk stands.
  walk: Walk,
}

impl ListStream {
  /// A walk of a request in a revision, not begun.
  fn new(request: ListRequest, revision: Revision) -> Self {
    Self {
      path: request.path,
      revision,
      depth: request.depth,
      fields: request.fields,
      patterns: request.patterns,
      walk: Walk::Start,
    }
  }

  /// Walks until the bytes written and the entries visited reach `room`, at least one step, or until the walk ends.
  pub(super) fn resume<R: Repository + ?Sized>(
    &mut self,
    reads: &mut Reads<'_, R>,
    room: usize,
  ) -> Result<Progress, ItemError> {
    let start = reads.out.len();
    let mut visited = 0_usize;
    loop {
      let walk = std::mem::replace(&mut self.walk, Walk::Ended);
      if walk == Walk::Ended {
        return Ok(Progress::Done);
      }
      self.walk = self.advance(walk, reads)?;
      visited = visited.saturating_add(1);
      if self.walk == Walk::Ended {
        return Ok(Progress::Done);
      }
      if reads
        .out
        .len()
        .saturating_sub(start)
        .saturating_add(visited)
        >= room
      {
        return Ok(Progress::More);
      }
    }
  }

  /// One step of the walk; a refusal ends it with `done` and the refusal.
  fn advance<R: Repository + ?Sized>(
    &self,
    walk: Walk,
    reads: &mut Reads<'_, R>,
  ) -> Result<Walk, ItemError> {
    let stepped = match walk {
      Walk::Start => self.begin(reads),
      Walk::Levels(levels) => self.visit(levels, reads),
      Walk::Ended => return Ok(Walk::Ended),
    };
    match stepped {
      Ok(walk) => Ok(walk),
      Err(Stop::Refused(failure)) => end(reads, Some(&failure)),
      Err(Stop::Unwritten(error)) => Err(error),
    }
  }

  /// `svn_repos_list` before the walk: the depth refused below `empty`, the path refused when absent, nothing for an
  /// empty pattern list, the path's own dirent when its name matches, then its entries unless it is a file or the depth
  /// is `empty`.
  fn begin<R: Repository + ?Sized>(&self, reads: &mut Reads<'_, R>) -> Result<Walk, Stop> {
    let depth = self.depth.map_err(|number| {
      let message = format!("Invalid depth '{number}' in svn_repos_list");
      Failure::new(SVN_ERR_REPOS_BAD_ARGS, message)
    })?;
    let node = reads
      .node(&self.path, self.revision)?
      .ok_or_else(|| self.not_found())?;
    if self.patterns.as_ref().is_some_and(Vec::is_empty) {
      return Ok(end(reads, None)?);
    }
    if self.matches(basename(&self.path)) {
      self.report(reads, &self.path, &node)?;
    }
    if node.kind() == NodeKind::File || depth == Depth::Empty {
      return Ok(end(reads, None)?);
    }
    Ok(Walk::Levels(vec![self.level(reads, &self.path, depth)?]))
  }

  /// `svn_repos_list`'s refusal of an absent path.
  fn not_found(&self) -> Failure {
    Failure::new(
      SVN_ERR_FS_NOT_FOUND,
      format!("Path '{}' not found", self.path),
    )
  }

  /// `do_list`'s next entry: its dirent when it matches, then its own entries at depth `infinity`; the deepest
  /// directory left when its entries are done, and the walk's end when the last is.
  fn visit<R: Repository + ?Sized>(
    &self,
    mut levels: Vec<Level>,
    reads: &mut Reads<'_, R>,
  ) -> Result<Walk, Stop> {
    let Some((path, entry)) = next_entry(&mut levels) else {
      return if levels.is_empty() {
        Ok(end(reads, None)?)
      } else {
        Ok(Walk::Levels(levels))
      };
    };
    if entry.matches {
      let node = reads
        .node(&path, self.revision)?
        .ok_or_else(Failure::corrupt)?;
      self.report(reads, &path, &node)?;
    }
    if self.depth == Ok(Depth::Infinity) && entry.kind == NodeKind::Directory {
      levels.push(self.level(reads, &path, Depth::Infinity)?);
    }
    Ok(Walk::Levels(levels))
  }

  /// `do_list`'s entries of a directory: directories left out at depth `files`, a file whose name matches no pattern
  /// left out, the rest in byte order of their names (`compare_filtered_dirent`'s `strcmp`).
  fn level<R: Repository + ?Sized>(
    &self,
    reads: &Reads<'_, R>,
    directory: &str,
    depth: Depth,
  ) -> Result<Level, Failure> {
    let names = reads
      .repository
      .entries(directory, self.revision)
      .ok_or_else(Failure::corrupt)?;
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
      let kind = reads
        .node(&child_path(directory, &name), self.revision)?
        .ok_or_else(Failure::corrupt)?
        .kind();
      let matches = self.matches(&name);
      let left_out = (kind == NodeKind::Directory && depth == Depth::Files)
        || (kind == NodeKind::File && !matches);
      if !left_out {
        entries.push(ListEntry {
          name,
          kind,
          matches,
        });
      }
    }
    entries.sort_by(|first, second| first.name.cmp(&second.name));
    Ok(Level {
      directory: directory.to_owned(),
      entries,
      next: 0,
    })
  }

  /// `matches_any`: whether a name, folded, matches a pattern; every name does when no patterns were given.
  fn matches(&self, name: &str) -> bool {
    self.patterns.as_ref().is_none_or(|patterns| {
      let folded = fold(name);
      patterns.iter().any(|pattern| fnmatch(pattern, &folded))
    })
  }

  /// `report_dirent`: a node's dirent written, or the refusal its fill met.
  fn report<R: Repository + ?Sized>(
    &self,
    reads: &mut Reads<'_, R>,
    path: &str,
    node: &Node<'_>,
  ) -> Result<(), Stop> {
    let dirent = self.dirent(reads, node)?;
    write(&dirent.value(path), reads.out)?;
    Ok(())
  }

  /// `fill_dirent`, when a field beyond the kind is asked: the size, whether there are properties, the created revision,
  /// its date parsed (time 0's when it has none) and its author.
  fn dirent<R: Repository + ?Sized>(
    &self,
    reads: &Reads<'_, R>,
    node: &Node<'_>,
  ) -> Result<ListDirent, Failure> {
    let mut dirent = ListDirent {
      fields: self.fields,
      kind: node.kind(),
      size: 0,
      has_properties: false,
      created: 0,
      date: String::new(),
      author: None,
    };
    if !self.fields.beyond_kind() {
      return Ok(dirent);
    }
    dirent.size = match node.kind() {
      NodeKind::Directory => DIRECTORY_SIZE,
      NodeKind::File => node.text().map_or(0, |text| text.size),
    };
    dirent.has_properties = reads.has_properties(node);
    dirent.created = node.created_revision();
    let time = match reads.revision_property(dirent.created, DATE_PROPERTY) {
      Some(date) => parse_time(c_string_of(date)).ok_or_else(Failure::bogus_date)?,
      None => 0,
    };
    dirent.date = format_time(time);
    dirent.author = reads
      .revision_property(dirent.created, AUTHOR_PROPERTY)
      .map(<[u8]>::to_vec);
    Ok(dirent)
  }
}

/// A dirent as `svn_ra_svn__write_dirent` writes it, with the fields asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ListDirent {
  /// The fields asked for.
  fields: DirentFields,
  /// The node's kind.
  kind: NodeKind,
  /// Its size: a text's length, or a directory's [`DIRECTORY_SIZE`].
  size: u64,
  /// Whether it has properties.
  has_properties: bool,
  /// Its created revision.
  created: Revision,
  /// That revision's date, written again.
  date: String,
  /// That revision's author.
  author: Option<Vec<u8>>,
}

impl ListDirent {
  /// `( path kind )` when no field beyond the kind is asked, else `( path kind ( size ) ( has-props ) ( created-rev )
  /// ( date ) ( author ) )`, each list empty when its field is not asked.
  fn value<'value>(&'value self, path: &'value str) -> Value<'value> {
    let kind = if self.fields.kind {
      kind_word(self.kind)
    } else {
      UNKNOWN_KIND
    };
    let mut items = vec![Value::String(path.as_bytes()), Value::Word(kind)];
    if self.fields.beyond_kind() {
      let fields = self.fields;
      items.push(optional(fields.size.then_some(Value::Number(self.size))));
      items.push(optional(
        fields
          .has_properties
          .then_some(Value::Word(bool_word(self.has_properties))),
      ));
      items.push(optional(
        fields.created.then_some(Value::Number(self.created)),
      ));
      items.push(optional(
        fields.time.then_some(Value::String(self.date.as_bytes())),
      ));
      let author = self.author.as_deref().filter(|_| fields.author);
      items.push(optional(author.map(Value::String)));
    }
    Value::List(items)
  }
}

/// Why a step of the walk stopped: a refusal the answer carries after `done`, or an item that could not be written.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
  /// A refusal: the walk ends with `done` and it.
  Refused(Failure),
  /// An item the writer refused.
  Unwritten(ItemError),
}

impl From<Failure> for Stop {
  fn from(failure: Failure) -> Self {
    Self::Refused(failure)
  }
}

impl From<ItemError> for Stop {
  fn from(error: ItemError) -> Self {
    Self::Unwritten(error)
  }
}

/// The deepest directory's next entry, with its path, the directory moved past it; `None` when that directory is done,
/// which is then left (or when no directory is left).
fn next_entry(levels: &mut Vec<Level>) -> Option<(String, ListEntry)> {
  let level = levels.last_mut()?;
  let Some(entry) = level.entries.get(level.next).cloned() else {
    levels.pop();
    return None;
  };
  level.next = level.next.saturating_add(1);
  Some((child_path(&level.directory, &entry.name), entry))
}

/// A list of one item, or an empty one.
fn optional(item: Option<Value<'_>>) -> Value<'_> {
  Value::List(item.into_iter().collect())
}

/// `svn_dirent_basename`: the last name of a path, empty for the root.
fn basename(path: &str) -> &str {
  path.rsplit(SEPARATOR).next().unwrap_or_default()
}

/// `svn_utf__xfrm` with case folding and marks stripped, over ASCII: the letters `A` to `Z` lowered (module
/// documentation for a name beyond ASCII).
fn fold(name: &str) -> Vec<u8> {
  name.bytes().map(|byte| byte.to_ascii_lowercase()).collect()
}

/// The walk's end: `done`, then `( success ( ) )` or the refusal.
fn end<R: Repository + ?Sized>(
  reads: &mut Reads<'_, R>,
  failure: Option<&Failure>,
) -> Result<Walk, ItemError> {
  write(&Value::Word(DONE), reads.out)?;
  match failure {
    None => reads.success(Vec::new())?,
    Some(failure) => reads.failure(failure)?,
  };
  Ok(Walk::Ended)
}
