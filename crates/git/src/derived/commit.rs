//! A version's git commit as derived data (A-664; A-647; §4.17.1 "A push is a work" items 3 and 4; §4.17.2 "Pieces
//! (git)" piece 13).
//!
//! **Two kinds of version** ([`Authored`]). A version a stock client pushed has the client's own commit as its head
//! (§4.17.1 item 3's head operation): its value names that commit and nothing else, so the client ends where it thinks
//! it is (item 4). A version the server turns into git's objects (an agent's work through the SDK or a mount; later a
//! merged review request or a server-side rebase) carries no head operation, and its commit is made here ([`Fields`]):
//! - `tree`: the version's tree, the [`super::Trees`] value of the same version, whose node the commit's node names so
//!   the trees live as long as the commit;
//! - a `parent` for each of the version's parents, in its history's order, each that parent's commit: the previous
//!   version's, whether a client pushed it or the server made it;
//! - `author` and `committer`: the merging principal's git identity from its record (§4.13), [`Identity`], and the
//!   merge record's time in seconds since the epoch, in UTC (`+0000`), never a holder's clock;
//! - the submit's message, its last line completed with a newline as `git commit-tree -m` completes it.
//!
//! It is written as git's `commit_tree` writes a commit \[C: commit.c `commit_tree_extended`, git v2.50.1\]: `tree`, each
//! `parent`, `author`, `committer`, an empty line, the message; no `encoding` header, since every field is UTF-8
//! (git's default), and no signature. Then it is checked as a fetching client checks it (git's strict fsck), named by
//! git's rule, and its bytes kept as content in the shared store, where the git store's row for the same object names
//! the same content (A-614).
//!
//! **An identity git writes** ([`Identity::new`]). git composes an identity from a name and an email it cleans first
//! \[C: ident.c `fmt_ident`, `strbuf_addstr_without_crud`, git v2.50.1, read 2026-11-19\]: it strips "crud" from both ends
//! of each (a byte up to the space, and `,`, `:`, `;`, `<`, `>`, `"`, `\`, `'`), drops a newline, `<` and `>` from inside,
//! and refuses an empty name ("empty ident name ... not allowed"). An identity is refused here where git would change
//! it or refuse it, so the bytes a principal's record holds are the bytes every commit carries. An empty email is
//! git's too (`<>`).
//!
//! **Refusals** ([`CommitsRefused`]): no commit recorded for the version, a pushed commit of another format, a tree or a
//! parent that does not read as this type's, a time past the latest date git reads, a commit strict fsck refuses (a
//! message holding a NUL: `nulInCommit`), a SHA-1 collision, a store of another window, and the store's own.
//!
//! No I/O, no clock, no randomness, no panic.

use std::marker::PhantomData;
use std::mem::size_of;

use slates_vcs::{ContentId, ContentRefused, ContentStore, DerivedType, Graph};

use super::tree::tree_id;
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::objects::commit;
use crate::objects::fsck::{FsckId, Strictness};

/// Format: the type's name and its format's version; the object format's name, a space and the store's window (eight
/// bytes, big-endian) follow, since a made commit's node names its object as the store cuts it.
const TYPE_NAME: &[u8] = b"slates-git commits 1 ";

/// Format: the first byte of a pushed version's node: the id of the commit a stock client made, naming no content.
const PUSHED: u8 = b'P';

/// Format: the first byte of a made version's node: the id of the commit the server made and its object's length; the
/// node names the object's content, then the version's tree.
const MADE: u8 = b'M';

/// Format: the bytes of a made commit's object length, little-endian.
const LENGTH_BYTES: usize = size_of::<u64>();

/// Format: the contents a made commit's node names: its object, then the version's tree.
const MADE_NAMES: usize = 2;

/// Format: the time zone a made commit's dates are written in: UTC, the merge record's.
const UTC: &[u8] = b"+0000";

/// Format: the bytes git drops from inside an identity (ident.c's `strbuf_addstr_without_crud`), with the NUL that ends
/// a C string.
const DELIMITERS: &[u8] = b"\0\n<>";

/// Format: the bytes past the space that git strips from an identity's ends (ident.c's `crud`).
const CRUD: &[u8] = b",:;<>\"\\'";

/// Format: the highest byte git strips from an identity's ends by value alone (ident.c's `crud`: `c <= 32`).
const CRUD_THROUGH: u8 = b' ';

/// Which part of an identity a refusal names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityPart {
  /// The name.
  Name,
  /// The email.
  Email,
}

/// Why a name and an email are not an identity git writes as given (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityRefused {
  /// The name is empty, which git refuses ("empty ident name (for <%s>) not allowed").
  EmptyName,
  /// `part` holds `byte`, a NUL, a newline, `<` or `>`: git drops the last three from inside an identity, and a NUL
  /// ends it.
  Delimiter {
    /// The part.
    part: IdentityPart,
    /// The byte.
    byte: u8,
  },
  /// `part` begins or ends with `byte`, which git strips from an identity's ends.
  Crud {
    /// The part.
    part: IdentityPart,
    /// The byte.
    byte: u8,
  },
}

/// A principal's git identity as the server writes it, author and committer alike (§4.13; §4.17.1 item 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity<'a> {
  name: &'a str,
  email: &'a str,
}

impl<'a> Identity<'a> {
  /// `name` and `email` as an identity git writes byte for byte (module docs), or why not.
  pub fn new(name: &'a str, email: &'a str) -> Result<Identity<'a>, IdentityRefused> {
    if name.is_empty() {
      return Err(IdentityRefused::EmptyName);
    }
    as_git_writes(IdentityPart::Name, name.as_bytes())?;
    as_git_writes(IdentityPart::Email, email.as_bytes())?;
    Ok(Identity { name, email })
  }

  /// The name.
  pub fn name(&self) -> &'a str {
    self.name
  }

  /// The email.
  pub fn email(&self) -> &'a str {
    self.email
  }
}

/// Whether git strips `byte` from an identity's ends.
fn crud(byte: u8) -> bool {
  byte <= CRUD_THROUGH || CRUD.contains(&byte)
}

/// Refuses a part of an identity that git would not write as it is.
fn as_git_writes(part: IdentityPart, bytes: &[u8]) -> Result<(), IdentityRefused> {
  if let Some(byte) = bytes.iter().find(|byte| DELIMITERS.contains(byte)) {
    return Err(IdentityRefused::Delimiter { part, byte: *byte });
  }
  match [bytes.first(), bytes.last()]
    .into_iter()
    .flatten()
    .find(|byte| crud(**byte))
  {
    Some(byte) => Err(IdentityRefused::Crud { part, byte: *byte }),
    None => Ok(()),
  }
}

/// What the server makes a version's commit of (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fields<'a> {
  /// The version's tree: the root of its [`super::Trees`] value, held by the store.
  pub tree: ContentId,
  /// The merging principal's identity: author and committer.
  pub identity: Identity<'a>,
  /// The merge record's time: seconds since the epoch, in UTC.
  pub seconds: u64,
  /// The submit's message.
  pub message: &'a str,
}

/// How a version's commit is had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authored<'a> {
  /// A stock client's push made the version: its head is the client's commit, this one.
  Pushed(ObjectId),
  /// The server makes the version's commit from these.
  Server(Fields<'a>),
}

/// A history whose versions' commits the type reads.
pub trait CommitSource: Graph {
  /// How `version`'s commit is had; `None` when the history holds no such version.
  fn commit(&self, version: Self::Version) -> Option<Authored<'_>>;
}

/// Why the commits refused (module docs). Each leaves the store as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitsRefused {
  /// The history holds no commit for the version.
  NoCommit,
  /// A pushed version's commit, or a parent's, is not of the repository's format.
  ForeignId {
    /// The id.
    id: ObjectId,
  },
  /// The node `node` is not what the commit reads from it: the version's tree, or a parent's commit.
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// The merge record's time is past the latest date git reads (`i64::MAX` seconds, date.c's `date_overflows`).
  DateOverflow {
    /// The time, in seconds.
    seconds: u64,
  },
  /// The commit made fails git's strict fsck with `finding`, a NUL in its message (`nulInCommit`) above all.
  Fsck {
    /// The first finding strict fsck refuses.
    finding: FsckId,
  },
  /// The commit's SHA-1 is part of a collision attack, as git's detection refuses it.
  Collision {
    /// Its plain SHA-1.
    sha1: ObjectId,
  },
  /// The store's window is not the one the type was declared with.
  WindowMismatch {
    /// The window declared.
    declared: usize,
    /// The store's.
    store: usize,
  },
  /// The store refused.
  Content(ContentRefused),
}

impl From<ContentRefused> for CommitsRefused {
  fn from(refused: ContentRefused) -> CommitsRefused {
    CommitsRefused::Content(refused)
  }
}

/// What the commits have done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitCounters {
  /// Commits the server made.
  pub made: u64,
  /// Versions whose commit a stock client pushed.
  pub pushed: u64,
}

/// The commits, declared over a history `S` (module docs).
#[derive(Debug)]
pub struct Commits<S> {
  format: ObjectFormat,
  name: Vec<u8>,
  mode: slates_vcs::Mode,
  window: usize,
  counters: CommitCounters,
  history: PhantomData<fn(&S)>,
}

impl<S> Commits<S> {
  /// The commits of a repository of `format`, over a store of `window`, computed by `mode`.
  pub fn declare(format: ObjectFormat, window: usize, mode: slates_vcs::Mode) -> Commits<S> {
    let mut name = TYPE_NAME.to_vec();
    name.extend_from_slice(format.name().as_bytes());
    name.push(b' ');
    name.extend_from_slice(&crate::to_u64(window).to_be_bytes());
    Commits {
      format,
      name,
      mode,
      window,
      counters: CommitCounters::default(),
      history: PhantomData,
    }
  }

  /// What the commits have done.
  pub fn counters(&self) -> CommitCounters {
    self.counters
  }

  /// The id of the commit the node `node` names: a pushed version's or a made one's.
  pub fn id(&self, store: &ContentStore, node: ContentId) -> Result<ObjectId, CommitsRefused> {
    read_node(store, self.format, node).map(|(id, _)| id)
  }

  /// The bytes of the commit the node `node` names, when the server made it (git hashes them after the header); `None`
  /// for a pushed version's, whose bytes the git store holds.
  pub fn object(
    &self,
    store: &ContentStore,
    node: ContentId,
  ) -> Result<Option<Vec<u8>>, CommitsRefused> {
    let malformed = CommitsRefused::Malformed { node };
    let (_, len) = read_node(store, self.format, node)?;
    let Some(len) = len else {
      return Ok(None);
    };
    let derived = store.derived(&node).ok_or_else(|| malformed.clone())?;
    let object = derived.name(0).ok_or_else(|| malformed.clone())?;
    store.object_bytes(&object, len).map(Some).ok_or(malformed)
  }

  /// The node of a pushed version's commit `id`: held once by the caller.
  pub fn pushed(
    &mut self,
    store: &mut ContentStore,
    id: ObjectId,
  ) -> Result<ContentId, CommitsRefused> {
    self.same_window(store)?;
    if id.format() != self.format {
      return Err(CommitsRefused::ForeignId { id });
    }
    let mut payload = vec![PUSHED];
    payload.extend_from_slice(id.as_bytes());
    let node = store.put_derived(&[], &payload)?.identity;
    self.counters.pushed = self.counters.pushed.saturating_add(1);
    Ok(node)
  }

  /// The node of the commit the server makes of `fields` over `parents` (module docs): held once by the caller.
  pub fn make(
    &mut self,
    store: &mut ContentStore,
    fields: &Fields<'_>,
    parents: &[ObjectId],
  ) -> Result<ContentId, CommitsRefused> {
    self.same_window(store)?;
    let text = self.text(store, fields, parents)?;
    let report = commit::fsck(&text, self.format, Strictness::Strict);
    if let Some(finding) = report.refusal() {
      return Err(CommitsRefused::Fsck { finding });
    }
    let id = name_of(self.format, Kind::Commit, &text)
      .map_err(|attack| CommitsRefused::Collision { sha1: attack.sha1 })?;
    let len = u64::try_from(text.len()).map_err(|_| ContentRefused::CountExhausted)?;
    let mut payload = vec![MADE];
    payload.extend_from_slice(id.as_bytes());
    payload.extend_from_slice(&len.to_le_bytes());
    let object = store.put_object(&text)?;
    let node = store.put_derived(&[object, fields.tree], &payload);
    // The node holds the object now; the put's own hold is given back whatever the answer, so a refusal leaves nothing.
    let released = store.release(&object);
    let node = node?.identity;
    released?;
    self.counters.made = self.counters.made.saturating_add(1);
    Ok(node)
  }

  /// The commit's bytes as git's `commit_tree` writes them (module docs).
  fn text(
    &self,
    store: &ContentStore,
    fields: &Fields<'_>,
    parents: &[ObjectId],
  ) -> Result<Vec<u8>, CommitsRefused> {
    let tree = tree_id(store, self.format, fields.tree)
      .ok_or(CommitsRefused::Malformed { node: fields.tree })?;
    if i64::try_from(fields.seconds).is_err() {
      return Err(CommitsRefused::DateOverflow {
        seconds: fields.seconds,
      });
    }
    if let Some(parent) = parents.iter().find(|parent| parent.format() != self.format) {
      return Err(CommitsRefused::ForeignId { id: *parent });
    }
    let mut text = Vec::new();
    text.extend_from_slice(b"tree ");
    tree.write_hex(&mut text);
    text.push(b'\n');
    for parent in parents {
      text.extend_from_slice(b"parent ");
      parent.write_hex(&mut text);
      text.push(b'\n');
    }
    for role in [&b"author "[..], &b"committer "[..]] {
      text.extend_from_slice(role);
      write_ident(&mut text, fields);
      text.push(b'\n');
    }
    text.push(b'\n');
    text.extend_from_slice(fields.message.as_bytes());
    if !fields.message.is_empty() && !fields.message.ends_with('\n') {
      text.push(b'\n');
    }
    Ok(text)
  }

  /// Refuses a store whose window is not the one declared.
  fn same_window(&self, store: &ContentStore) -> Result<(), CommitsRefused> {
    if store.window() == self.window {
      Ok(())
    } else {
      Err(CommitsRefused::WindowMismatch {
        declared: self.window,
        store: store.window(),
      })
    }
  }
}

impl<S: CommitSource> DerivedType for Commits<S> {
  type Source = S;
  type Refused = CommitsRefused;

  fn name(&self) -> &[u8] {
    &self.name
  }

  fn mode(&self) -> slates_vcs::Mode {
    self.mode
  }

  fn derive(
    &mut self,
    source: &S,
    store: &mut ContentStore,
    version: S::Version,
    parents: &[ContentId],
  ) -> Result<ContentId, CommitsRefused> {
    let reading: &ContentStore = store;
    let parents = parents
      .iter()
      .map(|parent| self.id(reading, *parent))
      .collect::<Result<Vec<ObjectId>, CommitsRefused>>()?;
    match source.commit(version).ok_or(CommitsRefused::NoCommit)? {
      Authored::Pushed(id) => self.pushed(store, id),
      Authored::Server(fields) => self.make(store, &fields, &parents),
    }
  }
}

/// Writes `name <email> seconds +0000`, an identity line's value as git writes it.
fn write_ident(text: &mut Vec<u8>, fields: &Fields<'_>) {
  text.extend_from_slice(fields.identity.name().as_bytes());
  text.extend_from_slice(b" <");
  text.extend_from_slice(fields.identity.email().as_bytes());
  text.extend_from_slice(b"> ");
  text.extend_from_slice(fields.seconds.to_string().as_bytes());
  text.push(b' ');
  text.extend_from_slice(UTC);
}

/// The id a commit's node holds and, for a made commit, its object's length.
fn read_node(
  store: &ContentStore,
  format: ObjectFormat,
  node: ContentId,
) -> Result<(ObjectId, Option<u64>), CommitsRefused> {
  let malformed = CommitsRefused::Malformed { node };
  let derived = store.derived(&node).ok_or_else(|| malformed.clone())?;
  let (tag, rest) = derived
    .payload()
    .split_first()
    .ok_or_else(|| malformed.clone())?;
  let (id, rest) = rest
    .split_at_checked(format.raw_len())
    .ok_or_else(|| malformed.clone())?;
  let id = ObjectId::from_bytes(format, id).ok_or_else(|| malformed.clone())?;
  match (
    *tag,
    derived.len(),
    rest.split_first_chunk::<LENGTH_BYTES>(),
  ) {
    (PUSHED, 0, _) if rest.is_empty() => Ok((id, None)),
    (MADE, MADE_NAMES, Some((len, []))) => Ok((id, Some(u64::from_le_bytes(*len)))),
    _ => Err(malformed),
  }
}
