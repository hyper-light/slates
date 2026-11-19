//! Mercurial's pushes landed (A-657; §4.17.3 "Push" steps 3 to 7, decisions 1 to 3 and 6; A-641 part 3): a push's
//! bundle, read and verified by slates-hg, placed by its classifier on the repository's lines and bookmarks (each a
//! green of the repository's namespace), its race checks made positions the append compares (A-642), its phase roots
//! moved in the same entry, one entry appended through A-635's [`RepositoryLogs::append`], its revisions kept in the
//! repository's index once the entry commits, and its reply written as 7.2's server writes it.
//!
//! **The namespace (decided here).** A Mercurial repository's namespace holds two kinds of name, each its own range:
//! a line, one head of a named branch (decision 1), is `branch:<label>:<number>`; a bookmark (decision 2) is
//! `bookmark:<label>:`. The label is the name's bytes, its valid UTF-8 kept and every `:`, `%`, control character and
//! byte that is not UTF-8 written `%` and two upper-case hexadecimal digits, so a label never holds `:`. The names are
//! therefore prefix-free by construction: the two tags differ at their second byte and end in `:`, so no name of one
//! kind begins as one of the other; a label ends at the first `:` after its tag, so `branch:<B>:` begins exactly the
//! lines of branch `B` and no other branch's (`stable` and `stable2`, `a` and `a/b` stay apart), and `bookmark:<M>:`
//! begins exactly one name, so a [`RepositoryCheck::Names`] under it checks that bookmark's presence or absence.
//! Every name a stock client can make reads as itself, since Mercurial refuses `:` in a label (`scmutil.checknewlabel`
//! \[C\]); the escapes only keep a hostile or damaged name from meeting another. A line made by a push takes the lowest
//! number its branch does not use; a Mercurial repository's first line, the green its verb makes, is
//! `branch:default:0`, empty until its first push, whose new head on `default` it takes.
//!
//! **The race checks as positions** (A-642: a check of a log a push does not move compares that log's position). Each
//! check the client sent, judged against the view the push read, is also handed to the append as positions, so it
//! holds where the entry commits: `check:heads`, the names under `branch:` with every line's green at the version read
//! and the markers log's position (an obsolete changeset leaves the served view); `check:updated-heads`, each named
//! head's line at its version and its branch's names; `check:bookmarks`, the bookmark's green at its version and its
//! name, or its name absent; `check:phases`, the phases log's position. Every branch whose lines the push moves, makes
//! or retires has its names checked, every bookmark the push changes its name, and a push that moves phase roots the
//! phases log's position. The phases log is one position for the whole repository, so a phase moved anywhere since the
//! read races a push that checks or moves phases, where 7.2 races only when a node the client listed left its phase:
//! the price of judging a phase by a position, paid only by a push whose read and append a hook's run separates.
//!
//! **Answers.** An append refused because something the push read moved (a line's verdict meeting another push's head,
//! a moved or retired branch, a name taken meanwhile, a check or a state change that no longer holds) is 7.2's race:
//! `error:pushraced`, "remote repository changed while pushing - please try again". Any other refusal is `error:abort`
//! in slates' words naming it.
//!
//! **Two steps.** [`prepare`] reads the repository once (its lines, the bookmarks the push names, the view the caller
//! made) and builds every work, holding nothing of the repository; [`land`] keeps the push's revisions in a fork of the
//! index, appends the entry, and puts the fork in the index's place once the entry committed, or releases it. The
//! admission (the repository's policy and its hooks) runs between the two.

use std::collections::{BTreeMap, BTreeSet, HashSet};

#[cfg(unix)]
use slates_db::catalog::{HookPointName, RepositoryPolicy};
use slates_db::catalog::{
  NameBinding, Principal, RepositoryCheck, RepositoryId, StateChange, StateLog,
  VolumeId as DbVolumeId,
};
use slates_hg::index::HgIndex;
use slates_hg::push::check::{self, CheckRefusal, RACED};
use slates_hg::push::classify::{
  self, BookmarkMove, BookmarkRead, DRAFT, LineMove, LineRead, Plan, RootChange,
};
use slates_hg::push::commit::{self, Unindexed};
use slates_hg::push::reply::{Refused, Replied, Reply};
use slates_hg::push::tree::{self, Move};
use slates_hg::push::verify::{self, Held, Unverified, Verified};
use slates_hg::push::{Asks, Unbundle};
use slates_hg::serve::Repository;
use slates_hg::{Admission, Node};
use slates_ipc::protocol::{DatabaseRefusal, Refusal};
use slates_merge::engine::Increment;
use slates_merge::push::{HEAD_ATTRIBUTE, ROOT};
use slates_vcs::{ContentStore, IndexRefused};

use super::push::{AppendRefused, BranchChange, ChangeRefusal, EntryParts, RepositoryLogs};
#[cfg(unix)]
use crate::hook_driver::{self, Launch};
use crate::state::ShardState;
#[cfg(unix)]
use crate::vcs_hooks::{self, CommittedView, ExitStatus, HookInput, Lesson, Outcome, PendingView};
use crate::vcs_policy::{self, Asked, Name, Namespace, PushRefusal, Right};
use crate::verbs::hex_of;

#[cfg(test)]
mod tests;

/// Format: the tag a line's name begins with, before its branch's label, `:` and its number (module docs).
pub(crate) const LINE_TAG: &str = "branch:";

/// Format: the tag a bookmark's name begins with, before its label and `:` (module docs).
pub(crate) const BOOKMARK_TAG: &str = "bookmark:";

/// Format: the byte that ends a label, which a label never holds.
const END: u8 = b':';

/// Format: the byte an escape begins with, before two upper-case hexadecimal digits.
const ESCAPE: u8 = b'%';

/// Format: the hexadecimal digits an escape writes, and the bits one of them holds.
const HEX: &[u8; 16] = b"0123456789ABCDEF";
/// Format: see [`HEX`].
const NIBBLE: u32 = 4;
/// Format: see [`HEX`].
const LOW_NIBBLE: u8 = 0x0f;

/// Format: the most bytes a character's UTF-8 takes.
const UTF8_MAX: usize = 4;

/// Slates' words for a refusal 7.2 has no words of its own for, each before what it names.
const NOT_AVAILABLE: &[u8] = b"a branch of this repository is not available here now; try again";
/// See [`NOT_AVAILABLE`].
const VIEW_REFUSED: &[u8] = b"the repository's history could not be read";
/// See [`NOT_AVAILABLE`].
const REFUSED: &[u8] = b"push refused: ";

/// Format: `acl`'s words for a name it refuses (`hgext/acl.py`, 7.2, read from Homebrew's 7.2 on 2026-11-19): `acl:
/// user "%s" denied on branch "%s" (changeset "%s")`, `not allowed on` for a list that leaves the user out, bookmarks
/// alike (a branch's changeset `short`'s twelve digits, a bookmark's the pushkey's `new`: all forty, or none when
/// deleted); the user here is the principal's record until §4.13's Mercurial identity names it.
const ACL_USER: &[u8] = b"acl: user \"";
/// Format: see [`ACL_USER`].
const ACL_DENIED: &[u8] = b"\" denied on ";
/// Format: see [`ACL_USER`].
const ACL_NOT_ALLOWED: &[u8] = b"\" not allowed on ";
/// Format: see [`ACL_USER`].
const ACL_CHANGESET: &[u8] = b" (changeset \"";
/// Format: see [`ACL_USER`].
const ACL_END: &[u8] = b"\")";
/// Format: the hexadecimal digits of 7.2's `short`, a node's first six bytes, which `acl` writes a changeset as.
const SHORT_DIGITS: usize = 12;
/// hgweb's words for a push its principal holds no right to make (`hgweb/common.py` `checkauthz`).
const NOT_AUTHORIZED: &[u8] = b"push not authorized";
/// Slates' words: the repository's policy holds a rule its matcher cannot read (the guard keeps such rules out).
const POLICY_UNREADABLE: &[u8] = b"the repository's policy cannot be read";

/// Appends `byte` escaped.
fn escape(byte: u8, out: &mut String) {
  out.push(char::from(ESCAPE));
  let high = byte.checked_shr(NIBBLE).unwrap_or_default();
  out.extend(HEX.get(usize::from(high)).map(|digit| char::from(*digit)));
  out.extend(
    HEX
      .get(usize::from(byte & LOW_NIBBLE))
      .map(|digit| char::from(*digit)),
  );
}

/// Appends the label of a Mercurial name (module docs).
fn label(bytes: &[u8], out: &mut String) {
  for chunk in bytes.utf8_chunks() {
    for character in chunk.valid().chars() {
      let escaped =
        character == char::from(END) || character == char::from(ESCAPE) || character.is_control();
      if !escaped {
        out.push(character);
        continue;
      }
      let mut buffer = [0; UTF8_MAX];
      for byte in character.encode_utf8(&mut buffer).bytes() {
        escape(byte, out);
      }
    }
    for byte in chunk.invalid() {
      escape(*byte, out);
    }
  }
}

/// The name's bytes a label holds; none for a text no label is.
fn unlabel(text: &str) -> Option<Vec<u8>> {
  let digit = |byte: u8| {
    HEX
      .iter()
      .position(|held| *held == byte)
      .and_then(|at| u8::try_from(at).ok())
  };
  let mut out = Vec::with_capacity(text.len());
  let mut bytes = text.bytes();
  while let Some(byte) = bytes.next() {
    if byte == END {
      return None;
    }
    if byte != ESCAPE {
      out.push(byte);
      continue;
    }
    let high = digit(bytes.next()?)?;
    let low = digit(bytes.next()?)?;
    out.push(high.checked_shl(NIBBLE)? | low);
  }
  Some(out)
}

/// The names of every line of `branch` begin with this.
pub(crate) fn branch_prefix(branch: &[u8]) -> String {
  let mut name = String::from(LINE_TAG);
  label(branch, &mut name);
  name.push(char::from(END));
  name
}

/// The name of line `number` of `branch` (module docs).
pub(crate) fn line_name(branch: &[u8], number: u32) -> String {
  let mut name = branch_prefix(branch);
  name.push_str(&number.to_string());
  name
}

/// The name of the bookmark `name` (module docs).
pub(crate) fn bookmark_name(name: &[u8]) -> String {
  let mut named = String::from(BOOKMARK_TAG);
  label(name, &mut named);
  named.push(char::from(END));
  named
}

/// The branch and number a line's name names; none for a name [`line_name`] does not write exactly so (another
/// spelling of a label or a number is no line's name, so no two names read as one line).
pub(crate) fn line_of(name: &str) -> Option<(Vec<u8>, u32)> {
  let rest = name.strip_prefix(LINE_TAG)?;
  let (label, number) = rest.split_once(char::from(END))?;
  let branch = unlabel(label)?;
  let number: u32 = number.parse().ok()?;
  (line_name(&branch, number) == name).then_some((branch, number))
}

/// The bookmark a bookmark's name names (A-680 part 2: the served view lists the register's bookmarks); none for a name
/// [`bookmark_name`] does not write exactly so, as [`line_of`] reads a line's.
pub(crate) fn bookmark_of(name: &str) -> Option<Vec<u8>> {
  let label = name
    .strip_prefix(BOOKMARK_TAG)?
    .strip_suffix(char::from(END))?;
  let bookmark = unlabel(label)?;
  (bookmark_name(&bookmark) == name).then_some(bookmark)
}

/// A line or a bookmark as a push read it: its name, its green, the version the green was at and its head there.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Named {
  /// Its name in the namespace.
  name: String,
  /// The green it binds.
  green: DbVolumeId,
  /// The green's version: the position the push read (A-642).
  version: u64,
  /// The green's head there; none for an empty line.
  head: Option<Node>,
}

/// What a push read of the repository.
#[derive(Debug, Default)]
struct Read {
  /// Every name under the lines' tag and its green, in name order, as `check:heads`'s names check lists them.
  names: Vec<(String, DbVolumeId)>,
  /// The lines among them, in name order: each as read, its branch and its number.
  lines: Vec<(Named, Vec<u8>, u32)>,
  /// Each bookmark the push names, by name, as read where it is bound.
  marks: Vec<(Vec<u8>, Option<Named>)>,
  /// The phases log's position, and the markers log's.
  logs: (u64, u64),
}

/// `green` as read, bound to `name`.
fn named(state: &ShardState, (name, green): (&str, DbVolumeId)) -> Named {
  let version = state.db.partition().green_head(green);
  let head = state.greens.get(&green).and_then(|engine| {
    engine
      .xattr(ROOT, HEAD_ATTRIBUTE)
      .and_then(|bytes| Node::try_from(bytes).ok())
  });
  Named {
    name: name.to_owned(),
    green,
    version,
    head,
  }
}

/// The bookmarks a push names: in its `bookmarks` parts, its `check:bookmarks` parts and its `bookmarks` pushkeys.
fn named_marks(unbundle: &Unbundle) -> Vec<Vec<u8>> {
  let mut names: Vec<Vec<u8>> = Vec::new();
  for taken in &unbundle.parts {
    let named: Vec<&[u8]> = match &taken.asks {
      Asks::Bookmarks(entries) | Asks::CheckBookmarks(entries) => {
        entries.iter().map(|entry| entry.name.as_slice()).collect()
      }
      Asks::Pushkey(ask) if ask.namespace == b"bookmarks" => vec![ask.key.as_slice()],
      _ => Vec::new(),
    };
    for name in named {
      if !names.iter().any(|held| held == name) {
        names.push(name.to_vec());
      }
    }
  }
  names
}

impl Read {
  /// What `unbundle` reads of `repository` on this shard.
  fn of(state: &ShardState, repository: RepositoryId, unbundle: &Unbundle) -> Read {
    let repositories = state.db.partition().repositories();
    let names: Vec<(String, DbVolumeId)> = repositories
      .branches(repository, LINE_TAG)
      .map(|(name, green)| (name.to_owned(), green))
      .collect();
    let lines = names
      .iter()
      .filter_map(|(name, green)| {
        let (branch, number) = line_of(name)?;
        Some((named(state, (name, *green)), branch, number))
      })
      .collect();
    let marks = named_marks(unbundle)
      .into_iter()
      .map(|mark| {
        let name = bookmark_name(&mark);
        let bound = repositories.branch(repository, &name);
        (mark, bound.map(|green| named(state, (&name, green))))
      })
      .collect();
    let position = |log| {
      repositories
        .log_position(repository, log)
        .unwrap_or_default()
    };
    Read {
      names,
      lines,
      marks,
      logs: (position(StateLog::Phases), position(StateLog::Markers)),
    }
  }

  /// The lines as the classifier reads them, in the same order.
  fn line_reads(&self) -> Vec<LineRead> {
    self
      .lines
      .iter()
      .map(|(named, branch, number)| LineRead {
        branch: branch.clone(),
        number: *number,
        head: named.head,
      })
      .collect()
  }

  /// The bound bookmarks as the classifier reads them.
  fn mark_reads(&self) -> Vec<BookmarkRead> {
    self
      .marks
      .iter()
      .filter_map(|(mark, named)| {
        let head = named.as_ref()?.head?;
        Some(BookmarkRead {
          name: mark.clone(),
          head,
        })
      })
      .collect()
  }

  /// The line at `line` among the lines read.
  fn line(&self, line: usize) -> Result<&Named, Refused> {
    self
      .lines
      .get(line)
      .map(|(named, ..)| named)
      .ok_or_else(|| abort(NOT_AVAILABLE.to_vec()))
  }

  /// The bookmark `mark` as read, where it is bound.
  fn mark(&self, mark: &[u8]) -> Result<&Named, Refused> {
    self
      .marks
      .iter()
      .find(|(held, _)| held == mark)
      .and_then(|(_, named)| named.as_ref())
      .ok_or_else(|| abort(NOT_AVAILABLE.to_vec()))
  }

  /// The check that the names under `prefix` are exactly those read.
  fn names_under(&self, prefix: String) -> RepositoryCheck {
    let bound = self
      .names
      .iter()
      .filter(|(name, _)| name.starts_with(&prefix))
      .map(|(name, green)| NameBinding {
        name: name.clone(),
        green: *green,
      })
      .collect();
    RepositoryCheck::Names { prefix, bound }
  }

  /// The check that bookmark `mark` is bound as read, or absent.
  fn mark_name(&self, mark: &[u8]) -> RepositoryCheck {
    let prefix = bookmark_name(mark);
    let bound = self
      .marks
      .iter()
      .find(|(held, _)| held == mark)
      .and_then(|(_, named)| named.as_ref())
      .map(|named| NameBinding {
        name: named.name.clone(),
        green: named.green,
      })
      .into_iter()
      .collect();
    RepositoryCheck::Names { prefix, bound }
  }
}

/// `error:abort` with slates' words.
fn abort(message: Vec<u8>) -> Refused {
  Refused::Abort {
    message,
    hint: None,
  }
}

/// Slates' words for a refusal: `push refused: ` and what refused it.
fn refused_words(what: &impl core::fmt::Debug) -> Vec<u8> {
  let mut words = REFUSED.to_vec();
  words.extend_from_slice(format!("{what:?}").as_bytes());
  words
}

/// A push ready to land: the entry it appends, what it keeps in the index, its reply, what its admission asks and what
/// its hooks read.
#[derive(Debug)]
pub(crate) struct Prepared {
  /// The branches it changes.
  changes: Vec<BranchChange>,
  /// The entry's checks and state.
  parts: EntryParts,
  /// The revisions it keeps in the index.
  verified: Verified,
  /// The reply's parts, in stream order.
  replied: Vec<Replied>,
  /// Whether the client asked for a reply (`replycaps`).
  wanted: bool,
  /// The first and last changesets it adds, which its hooks name (`HG_NODE`, `HG_NODE_LAST`).
  pub(crate) added: Option<(Node, Node)>,
  /// Each name it changes as its admission asks about it.
  pub(crate) asked: Vec<Ask>,
  /// Its changegroup part's id, which a hook's `output` part names.
  pub(crate) changegroup: Option<u32>,
  /// Its transaction's id, which every hook of it reads (`HG_TXNID`; [`transaction_id`]).
  pub(crate) transaction: String,
}

/// One name a push changes as its admission asks about it (A-622 part 2's `admit`): its namespace and label, the right
/// its change needs, and the changeset `acl` names when it refuses (the first the push brings on a branch, none for a
/// branch it brings none on; the node a bookmark moves to, none for a bookmark deleted).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Ask {
  /// The namespace.
  pub(crate) namespace: Namespace,
  /// The name's label.
  pub(crate) name: String,
  /// The right its change needs.
  pub(crate) right: Right,
  /// The changeset a refusal names.
  pub(crate) changeset: Option<Node>,
}

impl Prepared {
  /// What the push asks the repository's policy, in `admit`'s form.
  pub(crate) fn asks(&self) -> Vec<Asked<'_>> {
    self
      .asked
      .iter()
      .map(|ask| Asked {
        name: Name::Hg(ask.namespace, &ask.name),
        right: ask.right,
        // The requirements (A-644) are git's alone: a Mercurial change brings nothing they read.
        brings: None,
      })
      .collect()
  }

  /// Adds the `output` parts admission hooks wrote, after the changegroup's reply, as 7.2 writes them after the reply of
  /// the part whose handler ran the hooks.
  pub(crate) fn with_output(&mut self, output: Vec<Replied>) {
    let at = self
      .replied
      .iter()
      .position(|replied| matches!(replied, Replied::Changegroup { .. }))
      .map_or(self.replied.len(), |at| at.saturating_add(1));
    let tail = self.replied.split_off(at.min(self.replied.len()));
    self.replied.extend(output);
    self.replied.extend(tail);
  }
}

/// What `prepare` needs besides the push: the view the push read and what the repository holds (its index).
pub(crate) type Sources<'s, V, H> = (&'s V, &'s H);

/// The push's own terms: whether the repository publishes, the push's name (every work's identity begins with it), and
/// the room each work is built within.
pub(crate) type Terms = (bool, [u8; 32], u64);

/// Prepares `unbundle` for `repository` (module docs): its checks judged against `view`, its changegroup verified
/// against `held`, its changesets placed, every work built and the entry made; or 7.2's answer refusing it.
pub(crate) fn prepare<V: Repository, H: Held>(
  state: &ShardState,
  repository: RepositoryId,
  unbundle: &Unbundle,
  (view, held): Sources<'_, V, H>,
  terms: Terms,
  admission: &mut Admission,
) -> Result<Prepared, Refused> {
  check::judge(unbundle, view, admission).map_err(|refused| match refused {
    CheckRefusal::Raced { words } => Refused::PushRaced { message: words },
    CheckRefusal::View(_) => abort(VIEW_REFUSED.to_vec()),
  })?;
  let verified = match unbundle.changegroup() {
    Some((_, part)) => {
      verify::verify(part, held, admission).map_err(|fault| abort(unverified_words(&fault)))?
    }
    None => Verified::default(),
  };
  let read = Read::of(state, repository, unbundle);
  let (publishing, ..) = terms;
  let plan = classify::classify(
    unbundle,
    (&verified.changesets, &verified.held_changesets),
    view,
    (&read.line_reads(), &read.mark_reads()),
    publishing,
    admission,
  )
  .map_err(|refused| refused.refused())?;
  let changes = changes(state, (&plan, &read), (&verified, held), terms, admission)?;
  let parts = entry_parts(unbundle, &plan, &read);
  Ok(Prepared {
    changes,
    parts,
    replied: plan.replied.clone(),
    wanted: unbundle.reply_caps().is_some(),
    added: plan.added,
    asked: asked(&plan, &read),
    changegroup: unbundle.changegroup().map(|(id, _)| id),
    transaction: transaction_id(&terms.1),
    verified,
  })
}

/// Format: the BLAKE3 context a push's Mercurial transaction id is derived from its name in.
const TRANSACTION_CONTEXT: &str = "slates 2026-11-19 a Mercurial push's transaction id";
/// Format: what 7.2's transaction id begins with, before the hexadecimal of a SHA-1's digest (`localrepo.transaction`).
const TRANSACTION_TAG: &str = "TXN:";

/// The push's transaction id (`HG_TXNID`), the same for every hook of the push and another for every other push: `TXN:`
/// and forty hexadecimal digits, as 7.2 writes the SHA-1 of its transaction's start, here a BLAKE3 key derived from
/// the push's name, whose first twenty bytes a node's hexadecimal writes.
pub(crate) fn transaction_id(name: &[u8; 32]) -> String {
  let mut hasher = blake3::Hasher::new_derive_key(TRANSACTION_CONTEXT);
  hasher.update(name);
  let digest = hasher.finalize();
  let mut id = TRANSACTION_TAG.to_owned();
  if let Some(node) = digest
    .as_bytes()
    .first_chunk::<{ core::mem::size_of::<Node>() }>()
  {
    id.push_str(&hex_of(node));
  }
  id
}

/// Slates' words for a changegroup verification refused, 7.2's where it has them.
fn unverified_words(fault: &Unverified) -> Vec<u8> {
  refused_words(fault)
}

/// The work moving `named` to `head`, built against the version read.
fn work<H: Held>(
  state: &ShardState,
  (named, head): (&Named, &Node),
  (verified, held): (&Verified, &H),
  (_, name, room): Terms,
  admission: &mut Admission,
) -> Result<Increment, Refused> {
  let engine = state
    .greens
    .get(&named.green)
    .ok_or_else(|| abort(NOT_AVAILABLE.to_vec()))?;
  let moved = Move {
    name,
    base_version: named.version,
    base: named.head.as_ref(),
    head,
  };
  tree::work(&moved, &engine, (verified, held), room, admission)
    .map(|built| built.increment)
    .map_err(|refused| abort(refused_words(&refused)))
}

/// The branches the plan changes, as A-635's append takes them.
fn changes<H: Held>(
  state: &ShardState,
  (plan, read): (&Plan, &Read),
  revisions: (&Verified, &H),
  terms: Terms,
  admission: &mut Admission,
) -> Result<Vec<BranchChange>, Refused> {
  let mut changes = Vec::new();
  for moved in &plan.lines {
    changes.push(match moved {
      LineMove::Moved { line, head } => {
        let named = read.line(*line)?;
        BranchChange::Update {
          green: named.green,
          work: work(state, (named, head), revisions, terms, admission)?,
        }
      }
      LineMove::Retired { line } => {
        let named = read.line(*line)?;
        BranchChange::Delete {
          name: named.name.clone(),
          version: named.version,
        }
      }
    });
  }
  for made in &plan.created {
    let base = read.line(made.base)?;
    changes.push(BranchChange::Create {
      name: line_name(&made.branch, made.number),
      from: base.green,
      version: base.version,
      work: Some(work(
        state,
        (base, &made.head),
        revisions,
        terms,
        admission,
      )?),
    });
  }
  for moved in &plan.bookmarks {
    changes.push(mark_change(
      state,
      (moved, read),
      revisions,
      terms,
      admission,
    )?);
  }
  Ok(changes)
}

/// One bookmark's change as A-635's append takes it: a move on its own green, a green made from a line's version (no
/// work where that version's head is the bookmark's node), or its name retired.
fn mark_change<H: Held>(
  state: &ShardState,
  (moved, read): (&BookmarkMove, &Read),
  revisions: (&Verified, &H),
  terms: Terms,
  admission: &mut Admission,
) -> Result<BranchChange, Refused> {
  Ok(match moved {
    BookmarkMove::Moved { name, to, .. } => {
      let named = read.mark(name)?;
      BranchChange::Update {
        green: named.green,
        work: work(state, (named, to), revisions, terms, admission)?,
      }
    }
    BookmarkMove::Created { name, to, base } => {
      let base = read.line(*base)?;
      let work = if base.head == Some(*to) {
        None
      } else {
        Some(work(state, (base, to), revisions, terms, admission)?)
      };
      BranchChange::Create {
        name: bookmark_name(name),
        from: base.green,
        version: base.version,
        work,
      }
    }
    BookmarkMove::Deleted { name, .. } => {
      let named = read.mark(name)?;
      BranchChange::Delete {
        name: named.name.clone(),
        version: named.version,
      }
    }
  })
}

/// Pushes `check` unless the entry has it already.
fn check_once(checks: &mut Vec<RepositoryCheck>, check: RepositoryCheck) {
  if !checks.contains(&check) {
    checks.push(check);
  }
}

/// The checks the client's own check parts become (module docs).
fn client_checks(unbundle: &Unbundle, read: &Read, checks: &mut Vec<RepositoryCheck>) {
  let head_of = |named: &Named| RepositoryCheck::Head {
    green: named.green,
    version: named.version,
  };
  for taken in &unbundle.parts {
    match &taken.asks {
      Asks::CheckHeads(_) => {
        check_once(checks, read.names_under(LINE_TAG.to_owned()));
        for (named, ..) in &read.lines {
          check_once(checks, head_of(named));
        }
        let (_, markers) = read.logs;
        check_once(
          checks,
          RepositoryCheck::Log {
            log: StateLog::Markers,
            position: markers,
          },
        );
      }
      Asks::CheckUpdatedHeads(nodes) => {
        for (named, branch, _) in read
          .lines
          .iter()
          .filter(|(named, ..)| named.head.is_some_and(|head| nodes.contains(&head)))
        {
          check_once(checks, head_of(named));
          check_once(checks, read.names_under(branch_prefix(branch)));
        }
      }
      Asks::CheckBookmarks(entries) => {
        for entry in entries {
          check_once(checks, read.mark_name(&entry.name));
          if let Ok(named) = read.mark(&entry.name) {
            check_once(checks, head_of(named));
          }
        }
      }
      Asks::CheckPhases(_) => {
        let (phases, _) = read.logs;
        check_once(
          checks,
          RepositoryCheck::Log {
            log: StateLog::Phases,
            position: phases,
          },
        );
      }
      _ => {}
    }
  }
}

/// The entry's checks and state (module docs): the client's checks as positions, the names of every branch and
/// bookmark the push changes, the phases log where it moves roots, and the roots' changes.
fn entry_parts(unbundle: &Unbundle, plan: &Plan, read: &Read) -> EntryParts {
  let mut checks = Vec::new();
  client_checks(unbundle, read, &mut checks);
  for branch in &plan.touched {
    check_once(&mut checks, read.names_under(branch_prefix(branch)));
  }
  for moved in &plan.bookmarks {
    let (BookmarkMove::Moved { name, .. }
    | BookmarkMove::Created { name, .. }
    | BookmarkMove::Deleted { name, .. }) = moved;
    check_once(&mut checks, read.mark_name(name));
  }
  if !plan.roots.is_empty() {
    let (phases, _) = read.logs;
    check_once(
      &mut checks,
      RepositoryCheck::Log {
        log: StateLog::Phases,
        position: phases,
      },
    );
  }
  let state = plan
    .roots
    .iter()
    .map(|change| match change {
      RootChange::Removed(node) => StateChange::PhaseRootRemoved {
        phase: DRAFT,
        node: node.to_vec(),
      },
      RootChange::Added(node) => StateChange::PhaseRootAdded {
        phase: DRAFT,
        node: node.to_vec(),
      },
    })
    .collect();
  EntryParts {
    checks,
    names: Vec::new(),
    state,
    objects: Vec::new(),
  }
}

/// The label of a name as the policy reads it (`Name::Hg` takes text).
fn policy_name(name: &[u8]) -> String {
  let mut text = String::new();
  label(name, &mut text);
  text
}

/// Each name the plan changes and the right its change needs (`vcs_policy::admit`), once each, in the order 7.2's `acl`
/// meets them: a line moved, merged away or made on a new branch submits to its branch, a line made beside another head
/// of its branch rewrites it (Mercurial's forced push); a bookmark moved forward or made submits, moved elsewhere
/// rewrites, and deleted deletes. The branches come first, in the order the push brings its first changeset on each,
/// since `acl`'s `_txnhook` walks the new changesets in revision order and refuses at the first a rule refuses (a
/// branch the push brings none on, whose line a merge retires, after them); then the bookmarks, whose part 7.2 applies
/// after the changegroup's.
fn asked(plan: &Plan, read: &Read) -> Vec<Ask> {
  let firsts: BTreeMap<&[u8], (usize, Node)> = plan
    .firsts
    .iter()
    .enumerate()
    .map(|(arrival, (branch, node))| (branch.as_slice(), (arrival, *node)))
    .collect();
  let headed: BTreeSet<&[u8]> = read
    .lines
    .iter()
    .filter(|(named, _, _)| named.head.is_some())
    .map(|(_, branch, _)| branch.as_slice())
    .collect();
  let mut branches: Vec<(usize, Ask)> = Vec::new();
  for moved in &plan.lines {
    let (LineMove::Moved { line, .. } | LineMove::Retired { line }) = moved;
    if let Some((_, branch, _)) = read.lines.get(*line) {
      branches.push(branch_ask(&firsts, branch, Right::Submit));
    }
  }
  for made in &plan.created {
    let right = if headed.contains(made.branch.as_slice()) {
      Right::Rewrite
    } else {
      Right::Submit
    };
    branches.push(branch_ask(&firsts, &made.branch, right));
  }
  branches.sort_by_key(|(arrival, _)| *arrival);
  let mut seen = HashSet::new();
  branches
    .into_iter()
    .map(|(_, ask)| ask)
    .chain(plan.bookmarks.iter().map(bookmark_ask))
    .filter(|ask| seen.insert(ask.clone()))
    .collect()
}

/// The ask about `branch` for `right`, and where `acl` meets the branch: the arrival of the first changeset the push
/// brings on it (`Plan::firsts`), which the ask names; past every arrival, naming none, when it brings none on it.
fn branch_ask(
  firsts: &BTreeMap<&[u8], (usize, Node)>,
  branch: &[u8],
  right: Right,
) -> (usize, Ask) {
  let first = firsts.get(branch).copied();
  let ask = Ask {
    namespace: Namespace::Branch,
    name: policy_name(branch),
    right,
    changeset: first.map(|(_, node)| node),
  };
  (first.map_or(usize::MAX, |(arrival, _)| arrival), ask)
}

/// The ask about a bookmark the plan moves, makes or deletes, naming the node `acl` names: the one it moves to, none for
/// a deletion (7.2 hands `prepushkey` an empty `new`, `bundle2_part_handlers.handlebookmark`).
fn bookmark_ask(moved: &BookmarkMove) -> Ask {
  let (name, right, changeset) = match moved {
    BookmarkMove::Moved {
      name, to, forward, ..
    } => {
      let right = if *forward {
        Right::Submit
      } else {
        Right::Rewrite
      };
      (name, right, Some(*to))
    }
    BookmarkMove::Created { name, to, .. } => (name, Right::Submit, Some(*to)),
    BookmarkMove::Deleted { name, .. } => (name, Right::Delete, None),
  };
  Ask {
    namespace: Namespace::Bookmark,
    name: policy_name(name),
    right,
    changeset,
  }
}

/// The repository's policy asked about every name a prepared push changes, before its verdict (A-622 part 2's
/// `vcs_policy::admit`, §4.17.3 "Push" step 5): refused in `acl`'s words for a rule that denies a name or a list that
/// leaves the principal out, in hgweb's for a principal no rule lets push.
pub(crate) fn admitted(
  state: &ShardState,
  (principal, repository): (&Principal, RepositoryId),
  prepared: &Prepared,
) -> Result<(), Refused> {
  let record = state
    .db
    .partition()
    .repositories()
    .repository(repository)
    .ok_or_else(|| abort(NOT_AUTHORIZED.to_vec()))?;
  vcs_policy::admit(record, principal, &prepared.asks())
    .map_err(|refusal| policy_refused(refusal, &prepared.asked, principal))
}

/// The answer to a push the policy refused.
fn policy_refused(refusal: PushRefusal, asked: &[Ask], principal: &Principal) -> Refused {
  let named = |index: u32| usize::try_from(index).ok().and_then(|at| asked.get(at));
  let words = match refusal {
    PushRefusal::Denied { index, .. } => {
      named(index).map(|ask| acl_words(principal, ask, ACL_DENIED))
    }
    PushRefusal::NotListed { index } => {
      named(index).map(|ask| acl_words(principal, ask, ACL_NOT_ALLOWED))
    }
    PushRefusal::NotPermitted { .. } | PushRefusal::OtherTool { .. } => None,
    PushRefusal::Unreadable { .. } => Some(POLICY_UNREADABLE.to_vec()),
    // A requirement's refusals (A-644) never reach a Mercurial push: the database's guard holds the requirement
    // rules to git, and a Mercurial change brings nothing they read (`Prepared::asks`). Answered as hgweb answers a
    // refusal it does not word.
    PushRefusal::MergeCommit { .. }
    | PushRefusal::UnsignedCommit { .. }
    | PushRefusal::Unjudged { .. } => None,
  };
  abort(words.unwrap_or_else(|| NOT_AUTHORIZED.to_vec()))
}

/// `acl`'s words refusing `ask` to `principal` (`verdict` its `denied on` or `not allowed on`): a branch's changeset in
/// `short`'s digits, as `_txnhook` writes a changeset, none for a branch the push brings no changeset on; a bookmark's
/// as `_pkhook` writes the pushkey's `new`, every digit of the node it moves to, none for a deletion.
fn acl_words(principal: &Principal, ask: &Ask, verdict: &[u8]) -> Vec<u8> {
  let kind: &[u8] = match ask.namespace {
    Namespace::Branch => b"branch",
    Namespace::Bookmark => b"bookmark",
    Namespace::Topic => b"topic",
  };
  let mut words = ACL_USER.to_vec();
  words.extend_from_slice(format!("{principal:?}").as_bytes());
  words.extend_from_slice(verdict);
  words.extend_from_slice(kind);
  words.extend_from_slice(b" \"");
  words.extend_from_slice(ask.name.as_bytes());
  words.push(b'"');
  let hex = |node: Node| hex_of(&node).into_bytes();
  let named = match ask.namespace {
    Namespace::Bookmark => Some(ask.changeset.map(hex).unwrap_or_default()),
    Namespace::Branch | Namespace::Topic => ask.changeset.map(|node| {
      let mut digits = hex(node);
      digits.truncate(SHORT_DIGITS);
      digits
    }),
  };
  if let Some(digits) = named {
    words.extend_from_slice(ACL_CHANGESET);
    words.extend_from_slice(&digits);
    words.extend_from_slice(ACL_END);
  }
  words
}

/// One hook the repository's policy configures, as a push runs it: its configured name, its program, and what it is
/// started with (A-597's builders).
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(crate) struct Hook {
  /// Its configured name (`pretxnchangegroup.acl`).
  pub(crate) name: String,
  /// Its program.
  pub(crate) program: std::path::PathBuf,
  /// Its point's arguments, environment and standard input.
  pub(crate) input: HookInput,
}

/// What a push's hooks name of it: the first and last changesets it adds (none when it adds none), and its
/// transaction's id ([`Prepared`]'s).
#[cfg(unix)]
pub(crate) type Added<'a> = (Option<(Node, Node)>, &'a str);

/// The hooks `policy` configures at `point`, in the order it gives them, each started with what `input` makes for its
/// name; none when the push added no changeset (7.2 runs its changegroup hooks only then).
#[cfg(unix)]
fn configured(
  policy: &RepositoryPolicy,
  (point, (added, transaction)): (HookPointName, Added<'_>),
  url: &str,
  input: impl Fn(&str, &vcs_hooks::Changegroup) -> HookInput,
) -> Vec<Hook> {
  let Some((first, last)) = added else {
    return Vec::new();
  };
  let changes = vcs_hooks::Changegroup {
    first: hex_of(&first),
    last: hex_of(&last),
    url: url.to_owned(),
    transaction: transaction.to_owned(),
  };
  policy
    .hooks
    .iter()
    .filter(|hook| hook.point == point)
    .map(|hook| Hook {
      name: hook.name.clone(),
      program: std::path::PathBuf::from(&hook.program),
      input: input(&hook.name, &changes),
    })
    .collect()
}

/// The `pretxnchangegroup` hooks a prepared push runs before its verdict (§4.17.3 "Hooks": admission), over its pending
/// view, `HG_NODE` and `HG_NODE_LAST` the first and last changesets it adds, `HG_TXNID` its transaction, `HG_URL` the
/// transport's `url`.
#[cfg(unix)]
pub(crate) fn admission_hooks(
  policy: &RepositoryPolicy,
  added: Added<'_>,
  (view, url): (PendingView, &str),
) -> Vec<Hook> {
  configured(
    policy,
    (HookPointName::HgPretxnchangegroup, added),
    url,
    |name, changes| vcs_hooks::hg_pretxnchangegroup(view, name, changes),
  )
}

/// The `changegroup` hooks a landed push runs after its commit (§4.17.3 "Hooks": observing), over the committed view,
/// in the admission hooks' transaction.
#[cfg(unix)]
pub(crate) fn observing_hooks(
  policy: &RepositoryPolicy,
  added: Added<'_>,
  (view, url): (CommittedView, &str),
) -> Vec<Hook> {
  configured(
    policy,
    (HookPointName::HgChangegroup, added),
    url,
    |name, changes| vcs_hooks::hg_changegroup(view, name, changes),
  )
}

/// What running a push's hooks answered: the `output` parts their words make, the first refusal's answer, and each
/// run's lesson for its hook's learned times, in order.
#[cfg(unix)]
#[derive(Debug, Default)]
pub(crate) struct HooksRan {
  /// The `output` parts.
  pub(crate) output: Vec<Replied>,
  /// The first refusal, in 7.2's words.
  pub(crate) refused: Option<Refused>,
  /// Each run's lesson.
  pub(crate) lessons: Vec<Lesson>,
}

/// How a hook that did not accept ended, in 7.2's words (`hook.py` `_exthook`, `procutil.explainexit`: `%s hook %s`,
/// `exited with status %d` or `killed by signal %d`), and slates' for a run past its learned bound, a client gone, or a
/// program that could not be started.
#[cfg(unix)]
fn hook_words(name: &str, outcome: &Outcome) -> Vec<u8> {
  let status = |status: ExitStatus| match status {
    ExitStatus::Code(code) => format!("exited with status {code}"),
    ExitStatus::Signal(signal) => format!("killed by signal {signal}"),
  };
  let desc = match outcome {
    Outcome::Refused { status: ended, .. } => status(*ended),
    Outcome::TimedOut { bound_ns, .. } => format!("ran past its bound of {bound_ns} ns"),
    Outcome::Unstartable { cause } => format!("could not be started ({cause:?})"),
    Outcome::Abandoned => "was ended: its client left".to_owned(),
    Outcome::Accepted { .. } => "accepted".to_owned(),
  };
  format!("{name} hook {desc}").into_bytes()
}

/// Runs `hooks` in their order (§4.17.3 "Hooks"), each held to its bound in `bounds` (its learned times', none before
/// its first finished run) and over the view at `view_root`, its words kept up to `words_cap` and written as `output`
/// parts naming `in_reply_to`; the first that does not accept stops the rest and answers in 7.2's words, as 7.2 stops
/// at a throwing hook. Ended at once when `client_gone` resolves.
#[cfg(unix)]
pub(crate) async fn run_hooks<G: core::future::Future<Output = ()> + Unpin>(
  (hooks, bounds): (&[Hook], &[Option<u64>]),
  (view_root, words_cap, in_reply_to): (&std::path::Path, usize, Option<u32>),
  client_gone: &mut G,
) -> HooksRan {
  let mut ran = HooksRan::default();
  for (at, hook) in hooks.iter().enumerate() {
    let launch = Launch {
      program: &hook.program,
      input: &hook.input,
      view_root,
      words_cap,
    };
    let bound = bounds.get(at).copied().flatten();
    let ended = hook_driver::run(launch, bound, &mut *client_gone).await;
    ran.lessons.push(ended.lesson);
    let words = match &ended.outcome {
      Outcome::Accepted { words }
      | Outcome::Refused { words, .. }
      | Outcome::TimedOut { words, .. } => words.kept.clone(),
      Outcome::Abandoned | Outcome::Unstartable { .. } => Vec::new(),
    };
    if !words.is_empty() {
      ran.output.push(Replied::Output { in_reply_to, words });
    }
    if !matches!(ended.outcome, Outcome::Accepted { .. }) {
      ran.refused = Some(abort(hook_words(&hook.name, &ended.outcome)));
      break;
    }
  }
  ran
}

/// What a landing answered, and what it gave back.
#[derive(Debug)]
pub(crate) struct Landed {
  /// The reply to an accepted push, or the error part refusing it.
  pub(crate) answer: Result<Reply, Refused>,
  /// The release of the index the landing let go of: the one replaced once the push committed, or the fork when it did
  /// not; refused, the store's count of holds is off, which its caller reports.
  pub(crate) released: Result<(), IndexRefused>,
}

/// The repository's objects a landing keeps the push's revisions in: its index and the store of its contents.
pub(crate) type Objects<'o> = (&'o mut HgIndex, &'o mut ContentStore);

/// Lands `prepared` on `repository`'s logs by `principal` (module docs): its revisions kept in a fork of the index, its
/// entry appended, and the fork put in the index's place once the entry committed, or released.
pub(crate) fn land(
  logs: &mut impl RepositoryLogs,
  (principal, repository): (&Principal, RepositoryId),
  prepared: Prepared,
  (index, store): Objects<'_>,
  admission: &mut Admission,
) -> Landed {
  let fork = match commit::fork_with(index, store, &prepared.verified, admission) {
    Ok(fork) => fork,
    Err(fault) => {
      return Landed {
        answer: Err(abort(unindexed_words(&fault))),
        released: Ok(()),
      };
    }
  };
  match logs.append(principal, repository, prepared.changes, prepared.parts) {
    Ok(_) => {
      let replaced = core::mem::replace(index, fork);
      Landed {
        answer: Ok(Reply::accepted(&prepared.replied, prepared.wanted)),
        released: replaced.release(store),
      }
    }
    Err(refused) => Landed {
      answer: Err(append_refused(&refused)),
      released: fork.release(store),
    },
  }
}

/// Slates' words for revisions the index could not keep.
fn unindexed_words(fault: &Unindexed) -> Vec<u8> {
  refused_words(fault)
}

/// Whether a change was refused because something the push read moved.
fn raced_change(why: &ChangeRefusal) -> bool {
  match why {
    ChangeRefusal::Conflict { .. } | ChangeRefusal::Moved { .. } => true,
    ChangeRefusal::Refused(refusal) => raced(refusal),
    ChangeRefusal::Twice | ChangeRefusal::ApprovalRequired { .. } => false,
  }
}

/// Whether a refusal says that something the push read moved: a green or a log no longer at the position read, a check
/// or a state change that no longer holds, a name taken or retired meanwhile.
fn raced(refusal: &Refusal) -> bool {
  matches!(
    refusal,
    Refusal::Database {
      refusal: DatabaseRefusal::Moved { .. }
        | DatabaseRefusal::CheckFailed { .. }
        | DatabaseRefusal::StateChanged { .. }
    } | Refusal::AlreadyExists { .. }
      | Refusal::NotFound
  )
}

/// 7.2's answer to a refused append (module docs).
fn append_refused(refused: &AppendRefused) -> Refused {
  let race = match refused {
    AppendRefused::Changes(changes) => changes.iter().any(|change| raced_change(&change.why)),
    AppendRefused::Entry(refusal) => raced(refusal),
    AppendRefused::Repository(_) => false,
  };
  if race {
    Refused::PushRaced {
      message: RACED.to_vec(),
    }
  } else {
    abort(refused_words(refused))
  }
}
