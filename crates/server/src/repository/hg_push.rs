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
//! repository's index in its store on this shard (A-662's Mercurial arm, `crate::vcs_store`), appends the entry, and
//! puts the fork in the index's place once the entry committed, or releases it, each step over the store lent the
//! shard's room and settled after, so what the push brings is charged to the shard as the store's. The admission (the
//! repository's policy and its hooks) runs between the two.
//!
//! **The driver** ([`unbundle`], A-680 part 2b; 7.2's `wireprotov1server.unbundle` and `exchange.unbundle`): what both
//! edges call with a push's principal, its `heads` argument and its bundle, in one step on the repository's owner
//! shard, so nothing moves between what the push read and its entry. In 7.2's order: the `heads` argument judged
//! against the served view (`crate::vcs_store::hg_view`, the view every client is answered from), a race or a view
//! that cannot be judged answered before the bundle is read (7.2's `pusherr`); the bundle read (`push::read_bundle`);
//! then [`prepare`] over that view and what the store's index holds, [`admitted`], the hook gate, and [`land`]. Its
//! name is hashed from its repository and its bundle, so a push sent again after a lost answer makes the same works.
//!
//! **The hook gate** (§4.17.3 "Hooks"). A repository whose policy configures a Mercurial hook refuses every push,
//! naming the hook, until the hook's view of the push is served (piece 9's revlog view): no push lands that a configured
//! hook was not asked about, and no hook is passed over without a word, as git's push refuses (`crate::git_push`).
//!
//! **Durable** (A-680 part 2c). The entry a push appends holds its lines', bookmarks' and phase roots' moves and its
//! record in A-687's kind (`slates_hg::import::record::write_push`, `Source::Push`), which a recovery replays into the
//! index after the import's (`vcs_store::replay_hg`); a push that brought no revision records none.

use std::collections::{BTreeMap, BTreeSet, HashSet};

#[cfg(unix)]
use slates_db::catalog::{HookPointName, RepositoryPolicy};
use slates_db::catalog::{
  NameBinding, Principal, RepositoryCheck, RepositoryId, StateChange, StateLog, VcsTool,
  VolumeId as DbVolumeId,
};
use slates_hg::import::record;
use slates_hg::index::HgIndex;
use slates_hg::push::check::{self, CheckRefusal, HeadsArgument, Moment, RACED};
use slates_hg::push::classify::ReadChangeset;
use slates_hg::push::classify::{
  self, BookmarkMove, BookmarkRead, DRAFT, LineMove, LineRead, Plan, RootChange,
};
use slates_hg::push::commit::Unindexed;
use slates_hg::push::held::IndexHeld;
use slates_hg::push::intake::{self, Intake, Intaken, Rebase, Refused as IntakeRefused, Stepped};
use slates_hg::push::reply::{Refused, Replied, Reply};
use slates_hg::push::tree::{self, Move};
use slates_hg::push::verify::{Held, Unverified, Verified};
use slates_hg::push::{Asks, ChangegroupPart, Outcome as PushOutcome, Unbundle, read_bundle};
use slates_hg::serve::Repository;
use slates_hg::{Admission, Node};
use slates_ipc::protocol::{DatabaseRefusal, Refusal};
use slates_merge::engine::Increment;
use slates_merge::push::{HEAD_ATTRIBUTE, ROOT};
use slates_vcs::{ContentStore, IndexRefused};

use super::push::{self, AppendRefused, BranchChange, ChangeRefusal, EntryParts, RepositoryLogs};
#[cfg(unix)]
use crate::hook_driver::{self, Launch};
use crate::state::ShardState;
#[cfg(unix)]
use crate::vcs_hooks::{self, CommittedView, ExitStatus, HookInput, Lesson, Outcome, PendingView};
use crate::vcs_policy::{self, Asked, Name, Namespace, PushRefusal, Right};
use crate::vcs_store::hg_view::{self, PUBLISHING};
use crate::vcs_store::{self, VcsStores};
use crate::verbs::hex_of;

// Its history builders serve the edge's push tests too (`crate::vcs_http`'s daemon edge, A-680 part 2b).
#[cfg(test)]
pub(crate) mod tests;

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
/// Slates' words: the repository's store is not held on this shard (the lender lends none it does not hold).
const NO_STORE: &[u8] = b"the repository's store is not held here";
/// Slates' words for a push whose fork was made from an index another push replaced before it landed (A-720 part 2a; a
/// later part re-bases it instead).
const MOVED: &[u8] = b"the repository changed while this push was taken; push again";
/// Slates' words for a push whose changesets its fork could not give back (A-720).
const UNREAD: &[u8] = b"the push's changesets could not be read back";
/// Slates' words for a push whose record could not be written (A-680 part 2c).
const UNRECORDED: &[u8] = b"the push's record could not be written";
/// Slates' words for a push refused while a Mercurial hook is configured (module docs, "The hook gate"), before the
/// hook's configured name and after it.
const HOOK_NOT_RUN: &[u8] = b"the repository's ";
/// See [`HOOK_NOT_RUN`].
const HOOK_NOT_RUN_AFTER: &[u8] = b" hook cannot run here yet: its view of the push is not served";

/// Format: the BLAKE3 context a Mercurial push's name is derived in, from its repository and its bundle (module docs,
/// "The driver").
const PUSH_NAME_CONTEXT: &str = "slates 2026-11-19 a Mercurial push's name (A-680)";

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
  /// The fork its changegroup was taken into (A-720), none for a push without one.
  forked: Option<Forked>,
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
  (brought, held_changesets): (&[ReadChangeset], &[Node]),
  terms: Terms,
  admission: &mut Admission,
) -> Result<Prepared, Refused> {
  check::judge(unbundle, view, admission).map_err(|refused| match refused {
    CheckRefusal::Raced { words } => Refused::PushRaced { message: words },
    CheckRefusal::View(_) => abort(VIEW_REFUSED.to_vec()),
  })?;
  // The push's revisions are the fork's (A-720): every text the works read is read through `held`, the fork.
  let verified = Verified::default();
  let read = Read::of(state, repository, unbundle);
  let (publishing, ..) = terms;
  let plan = classify::classify(
    unbundle,
    (brought, held_changesets),
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
    forked: None,
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

/// Lands `prepared` on `repository` by `principal` (module docs, "Two steps"): its revisions kept in a fork of the
/// repository's index in its store on this shard, its entry appended, and the fork put in the index's place once the
/// entry committed, or released.
pub(crate) fn land(
  state: &mut ShardState,
  (principal, repository): (&Principal, RepositoryId),
  prepared: Prepared,
  admission: &mut Admission,
) -> Landed {
  let Prepared {
    changes,
    parts,
    forked,
    replied,
    wanted,
    ..
  } = prepared;
  // A fork made from an index another push replaced since names its parents by numbers that index no longer gives:
  // never put in the index's place (A-720 part 2b re-bases it before its landing).
  if forked
    .as_ref()
    .is_some_and(|forked| is_stale(state, repository, forked))
  {
    let released = match forked {
      Some(Forked { fork, .. }) => release_index(state, fork),
      None => Ok(()),
    };
    return Landed {
      answer: Err(abort(MOVED.to_vec())),
      released,
    };
  }
  let Some(Forked { fork, intaken, .. }) = forked else {
    let appended = RepositoryLogs::append(state, principal, repository, changes, parts);
    return Landed {
      answer: appended
        .map(|_| Reply::accepted(&replied, wanted))
        .map_err(|refused| append_refused(&refused)),
      released: Ok(()),
    };
  };
  // The push's record, the revisions it kept written from the fork in the order kept, within the room the landing runs
  // within (A-680 part 2c, A-720); a push that kept none records none.
  let written = (!intaken.logs.is_empty()).then(|| {
    state
      .vcs_stores
      .mercurial_ref(repository)
      .map(|(content, _)| record::write_kept(&fork, content, &intaken.logs, admission.left()))
  });
  let record = match written {
    None => None,
    Some(Some(Ok(bytes))) => Some(bytes),
    Some(_) => {
      return Landed {
        answer: Err(abort(UNRECORDED.to_vec())),
        released: release_index(state, fork),
      };
    }
  };
  let appended = match record {
    Some(record) => push::push_hg(state, (principal, repository), changes, parts, record),
    None => RepositoryLogs::append(state, principal, repository, changes, parts),
  };
  match appended {
    Ok(_) => Landed {
      answer: Ok(Reply::accepted(&replied, wanted)),
      released: replace_index(state, repository, fork),
    },
    Err(refused) => Landed {
      answer: Err(append_refused(&refused)),
      released: release_index(state, fork),
    },
  }
}

/// Puts `fork` in repository `repository`'s index's place and gives back the index it replaces, settling the shard's
/// charge; a store no longer held gives the fork back instead.
fn replace_index(
  state: &mut ShardState,
  repository: RepositoryId,
  fork: HgIndex,
) -> Result<(), IndexRefused> {
  vcs_store::within_budget(state, 0, |stores| {
    let replaced = match stores.mercurial(repository) {
      Some((content, store)) => core::mem::replace(&mut store.index, fork).release(content),
      None => return release_in(stores, fork),
    };
    stores.index_changed(repository);
    replaced
  })
}

/// Gives back a fork no entry committed, settling the shard's charge.
fn release_index(state: &mut ShardState, index: HgIndex) -> Result<(), IndexRefused> {
  vcs_store::within_budget(state, 0, |stores| release_in(stores, index))
}

/// Gives back `index`'s holds in the content store, where one is made.
fn release_in(stores: &mut VcsStores, index: HgIndex) -> Result<(), IndexRefused> {
  stores
    .content_mut()
    .map_or(Ok(()), |content| index.release(content))
}

/// What a push brings over a transport (module docs, "The driver"): who sends it, its `heads` argument read, and its
/// bundle's bytes.
pub(crate) type Pushing<'p> = (&'p Principal, &'p HeadsArgument, &'p [u8]);

/// A push's changegroup taken into its own fork (A-720): the fork, what it brought, and the generation of the index it
/// was forked from, so a landing knows whether another push landed meanwhile.
#[derive(Debug)]
pub(crate) struct Forked {
  /// The fork: the repository's revisions as they were, and the push's.
  pub(crate) fork: HgIndex,
  /// What the push brought.
  pub(crate) intaken: Intaken,
  /// The generation of the index the fork was made from (`vcs_store::VcsStores::generation_of`).
  pub(crate) generation: u64,
}

/// One `unbundle` on `repository`'s owner shard (module docs, "The driver"; A-720): its `heads` argument judged and its
/// bundle read over the served view; its changegroup taken into its own fork a slice at a time, each slice within the
/// shard's step quantum and a yield between, so a push of any size never holds the shard past a step and pushes to one
/// repository are taken side by side; then, in one step, judged over the view as it is at landing and landed. What it
/// came to, as 7.2 answers it before a transport frames the answer; none when no served view could be made.
pub(crate) async fn unbundle(
  repository: RepositoryId,
  (principal, heads, bundle): Pushing<'_>,
  room: &mut Admission,
) -> Option<PushOutcome> {
  let fronted = crate::state::with_state(|state| front(state, repository, (heads, bundle), room));
  let unbundle = match fronted.flatten()? {
    Ok(unbundle) => unbundle,
    Err(outcome) => return Some(outcome),
  };
  let forked = match unbundle.changegroup() {
    None => None,
    Some((_, part)) => match take_sliced(repository, part, lend_of(bundle), room).await? {
      Ok(forked) => Some(forked),
      Err(outcome) => return Some(outcome),
    },
  };
  // Landed in one step against the index as it is then; a fork another push made stale meanwhile is re-based by node
  // (A-720 part 2b), a slice at a time, and the landing tried again.
  let mut forked = forked;
  loop {
    let tried = crate::state::with_state(|state| match forked.take() {
      Some(stale) if is_stale(state, repository, &stale) => Err(stale),
      fresh => Ok(landing(
        state,
        repository,
        (principal, bundle),
        &unbundle,
        fresh,
        room,
      )),
    });
    match tried? {
      Ok(outcome) => return outcome,
      Err(stale) => match rebase_sliced(repository, stale).await? {
        Ok(rebased) => forked = Some(rebased),
        Err(outcome) => return Some(outcome),
      },
    }
  }
}

/// Whether `forked` was made from an index another push has replaced since (A-720 part 2b).
fn is_stale(state: &ShardState, repository: RepositoryId, forked: &Forked) -> bool {
  state.vcs_stores.generation_of(repository) != Some(forked.generation)
}

/// A stale fork's re-base while it runs (A-720 part 2b), given back by its drop if it is not finished.
struct Rebasing {
  /// The re-base, until it is finished or given back.
  rebase: Option<Rebase>,
  /// The generation of the index it re-bases onto.
  generation: u64,
}

impl Rebasing {
  /// One slice: rows re-appended until the re-base is done or the shard's quantum is spent; none when the content
  /// store is gone.
  fn slice(&mut self, state: &mut ShardState) -> Option<Result<Stepped, Unindexed>> {
    let rebase = self.rebase.as_mut()?;
    let quantum = vcs_store::hg_import::quantum_of(state);
    let start = slates_rt::futures::now_ns();
    vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut()?;
      Some(loop {
        match rebase.step(content) {
          Ok(Stepped::More) if slates_rt::futures::now_ns().saturating_sub(start) < quantum => {}
          other => break other,
        }
      })
    })
  }

  /// The re-based fork, the stale one given back; none, counted, when its release is refused or the store is gone.
  fn finish_in(mut self, state: &mut ShardState) -> Option<Forked> {
    let rebase = self.rebase.take()?;
    let generation = self.generation;
    let finished = vcs_store::within_budget(state, 0, |stores| {
      stores.content_mut().map(|content| rebase.finish(content))
    });
    match finished {
      Some(Ok((fork, intaken))) => Some(Forked {
        fork,
        intaken,
        generation,
      }),
      _ => {
        state.count(vcs_store::RELEASE_REFUSED, 1);
        None
      }
    }
  }
}

impl Drop for Rebasing {
  fn drop(&mut self) {
    if let Some(rebase) = self.rebase.take() {
      // Dropped mid-re-base (its connection ended): both forks given back on the shard that holds the store.
      crate::state::with_state(|state| {
        let released = vcs_store::within_budget(state, 0, |stores| {
          stores
            .content_mut()
            .map_or(Ok(()), |content| rebase.abandon(content))
        });
        if released.is_err() {
          state.count(vcs_store::RELEASE_REFUSED, 1);
        }
      })
      .unwrap_or_default();
    }
  }
}

/// Starts re-basing `stale` onto `repository`'s index as it is now; none when its store is not held here.
fn begin_rebase(
  state: &mut ShardState,
  repository: RepositoryId,
  stale: Forked,
) -> Option<Result<Rebasing, PushOutcome>> {
  let Forked { fork, intaken, .. } = stale;
  let begun = vcs_store::within_budget(state, 0, |stores| {
    let Some(generation) = stores.generation_of(repository) else {
      return Err((None, fork));
    };
    let Some((content, store)) = stores.mercurial(repository) else {
      return Err((None, fork));
    };
    Rebase::begin(fork, &intaken, &mut store.index, content)
      .map(|rebase| Rebasing {
        rebase: Some(rebase),
        generation,
      })
      .map_err(|(refused, fork)| (Some(refused), fork))
  });
  match begun {
    Ok(rebasing) => Some(Ok(rebasing)),
    Err((refused, fork)) => {
      if release_index(state, fork).is_err() {
        state.count(vcs_store::RELEASE_REFUSED, 1);
      }
      refused.map(|refused| Err(PushOutcome::refused(&abort(unindexed_words(&refused)))))
    }
  }
}

/// `stale` re-based a slice at a time, a yield between slices (A-720 part 2b); none when the store went away.
async fn rebase_sliced(
  repository: RepositoryId,
  stale: Forked,
) -> Option<Result<Forked, PushOutcome>> {
  let begun = crate::state::with_state(|state| begin_rebase(state, repository, stale));
  let mut rebasing = match begun.flatten()? {
    Ok(rebasing) => rebasing,
    Err(outcome) => return Some(Err(outcome)),
  };
  loop {
    let sliced = crate::state::with_state(|state| rebasing.slice(state));
    match sliced.flatten()? {
      Ok(Stepped::More) => slates_rt::futures::yield_now().await,
      Ok(Stepped::Done) => {
        return crate::state::with_state(|state| rebasing.finish_in(state))
          .flatten()
          .map(Ok);
      }
      Err(refused) => return Some(Err(PushOutcome::refused(&abort(unindexed_words(&refused))))),
    }
  }
}

/// [`unbundle`] in one call, every slice at once: the driver's steps for a caller that holds the shard's state (the
/// tests), never the daemon's path.
#[cfg(test)]
pub(crate) fn unbundle_in(
  state: &mut ShardState,
  repository: RepositoryId,
  (principal, heads, bundle): Pushing<'_>,
  room: &mut Admission,
) -> Option<PushOutcome> {
  let unbundle = match front(state, repository, (heads, bundle), room)? {
    Ok(unbundle) => unbundle,
    Err(outcome) => return Some(outcome),
  };
  let forked = match unbundle.changegroup() {
    None => None,
    Some((_, part)) => match take_whole(state, repository, part, room)? {
      Ok(forked) => Some(forked),
      Err(refused) => return Some(PushOutcome::refused(&refused)),
    },
  };
  landing(
    state,
    repository,
    (principal, bundle),
    &unbundle,
    forked,
    room,
  )
}

/// Derived: the room each slice of a push's intake is lent from the arena's pool beyond what the shard could admit
/// already (`vcs_store::within_budget`): its bundle's length, the bytes its revisions arrived in.
fn lend_of(bundle: &[u8]) -> u64 {
  u64::try_from(bundle.len()).unwrap_or(u64::MAX)
}

/// The driver's first step (module docs, "The driver"): `heads` judged at the served view before the bundle is read
/// (7.2's `pusherr` for a race), then the bundle read; none when no view could be made.
fn front(
  state: &ShardState,
  repository: RepositoryId,
  (heads, bundle): (&HeadsArgument, &[u8]),
  room: &mut Admission,
) -> Option<Result<Unbundle, PushOutcome>> {
  hg_view::with_view(state, repository, room, |view, _, room| {
    heads_hold(heads, view, room)?;
    read_bundle(bundle, room)
  })
}

/// A push's intake while it is taken (A-720), given back by its drop if it is not finished: a push whose connection
/// ends mid-intake leaves the store as it was.
pub(crate) struct Taking<'g> {
  /// The intake, until it is finished or given back.
  intake: Option<Intake<'g>>,
  /// The generation of the index it forked.
  generation: u64,
}

impl Taking<'_> {
  /// One slice: units taken until the intake is done or the shard's quantum is spent, within the room `lend` gives the
  /// stores; none when the content store is gone.
  fn slice(
    &mut self,
    state: &mut ShardState,
    lend: u64,
    room: &mut Admission,
  ) -> Option<Result<Stepped, IntakeRefused>> {
    let intake = self.intake.as_mut()?;
    let quantum = vcs_store::hg_import::quantum_of(state);
    let start = slates_rt::futures::now_ns();
    vcs_store::within_budget(state, lend, |stores| {
      let content = stores.content_mut()?;
      Some(loop {
        match intake.step(content, room) {
          Ok(Stepped::More) if slates_rt::futures::now_ns().saturating_sub(start) < quantum => {}
          other => break other,
        }
      })
    })
  }

  /// The fork once every unit is taken.
  fn finish(mut self) -> Option<Forked> {
    let intake = self.intake.take()?;
    match intake.finish() {
      Ok((fork, intaken)) => Some(Forked {
        fork,
        intaken,
        generation: self.generation,
      }),
      Err(intake) => {
        self.intake = Some(intake);
        None
      }
    }
  }

  /// Gives the fork back over `state`, counted where its release is refused.
  fn abandon_in(mut self, state: &mut ShardState) {
    if let Some(intake) = self.intake.take() {
      abandon(state, intake);
    }
  }
}

impl Drop for Taking<'_> {
  fn drop(&mut self) {
    if let Some(intake) = self.intake.take() {
      // Dropped mid-intake (its connection ended): given back on the shard that holds its store.
      crate::state::with_state(|state| abandon(state, intake)).unwrap_or_default();
    }
  }
}

/// Gives `intake`'s fork back, settling the shard's charge; a refused release counted.
fn abandon(state: &mut ShardState, intake: Intake<'_>) {
  let released = vcs_store::within_budget(state, 0, |stores| {
    stores
      .content_mut()
      .map_or(Ok(()), |content| intake.abandon(content))
  });
  if released.is_err() {
    state.count(vcs_store::RELEASE_REFUSED, 1);
  }
}

/// Starts taking `part` into a fork of `repository`'s index (A-720); none when its store is not held here.
fn begin_intake<'g>(
  state: &mut ShardState,
  repository: RepositoryId,
  part: &'g ChangegroupPart,
  lend: u64,
  room: &mut Admission,
) -> Option<Result<Taking<'g>, Refused>> {
  vcs_store::within_budget(state, lend, |stores| {
    let generation = stores.generation_of(repository)?;
    let (content, store) = stores.mercurial(repository)?;
    Some(match Intake::begin(part, &mut store.index, content, room) {
      Ok(intake) => Ok(Taking {
        intake: Some(intake),
        generation,
      }),
      Err(refused) => Err(intake_refused(&refused)),
    })
  })
}

/// `part` taken into its fork a slice at a time, a yield between slices (A-720); none when the store went away.
async fn take_sliced(
  repository: RepositoryId,
  part: &ChangegroupPart,
  lend: u64,
  room: &mut Admission,
) -> Option<Result<Forked, PushOutcome>> {
  let begun = crate::state::with_state(|state| begin_intake(state, repository, part, lend, room));
  let mut taking = match begun.flatten()? {
    Ok(taking) => taking,
    Err(refused) => return Some(Err(PushOutcome::refused(&refused))),
  };
  loop {
    let sliced = crate::state::with_state(|state| taking.slice(state, lend, room));
    match sliced.flatten()? {
      Ok(Stepped::More) => slates_rt::futures::yield_now().await,
      Ok(Stepped::Done) => return taking.finish().map(Ok),
      Err(refused) => return Some(Err(intake_outcome(&refused))),
    }
  }
}

/// `part` taken into its fork every slice at once: [`take_sliced`]'s steps for a caller that holds the shard's state.
#[cfg(test)]
pub(crate) fn take_whole(
  state: &mut ShardState,
  repository: RepositoryId,
  part: &ChangegroupPart,
  room: &mut Admission,
) -> Option<Result<Forked, Refused>> {
  let lend = u64::MAX;
  let mut taking = match begin_intake(state, repository, part, lend, room)? {
    Ok(taking) => taking,
    Err(refused) => return Some(Err(refused)),
  };
  loop {
    match taking.slice(state, lend, room) {
      Some(Ok(Stepped::More)) => {}
      Some(Ok(Stepped::Done)) => return taking.finish().map(Ok),
      Some(Err(refused)) => {
        taking.abandon_in(state);
        return Some(Err(intake_refused(&refused)));
      }
      None => {
        taking.abandon_in(state);
        return None;
      }
    }
  }
}

/// A prepared push landed as the driver lands it once its intake is done (A-720 part 2b): a fork another push made stale
/// re-based first, every row at once, then judged at landing and landed: the driver's steps for the tests, which land
/// pushes side by side with no runtime.
#[cfg(test)]
pub(crate) fn land_in(
  state: &mut ShardState,
  repository: RepositoryId,
  (principal, bundle): (&Principal, &[u8]),
  unbundle: &Unbundle,
  forked: Option<Forked>,
  room: &mut Admission,
) -> Option<PushOutcome> {
  let forked = match forked {
    Some(stale) if is_stale(state, repository, &stale) => {
      let mut rebasing = match begin_rebase(state, repository, stale)? {
        Ok(rebasing) => rebasing,
        Err(outcome) => return Some(outcome),
      };
      loop {
        match rebasing.slice(state)? {
          Ok(Stepped::More) => {}
          Ok(Stepped::Done) => break,
          Err(refused) => return Some(PushOutcome::refused(&abort(unindexed_words(&refused)))),
        }
      }
      Some(rebasing.finish_in(state)?)
    }
    fresh => fresh,
  };
  landing(
    state,
    repository,
    (principal, bundle),
    unbundle,
    forked,
    room,
  )
}

/// What an intake's refusal answers: verification's and the keeping's words, as 7.2 aborts.
fn intake_refused(refused: &IntakeRefused) -> Refused {
  abort(match refused {
    IntakeRefused::Unverified(fault) => unverified_words(fault),
    IntakeRefused::Unindexed(fault) => unindexed_words(fault),
  })
}

/// [`intake_refused`] as the push's outcome.
fn intake_outcome(refused: &IntakeRefused) -> PushOutcome {
  PushOutcome::refused(&intake_refused(refused))
}

/// [`prepare`] over `view` with `unbundle`'s changegroup taken whole into a fork first, the prepared push holding the
/// fork: the driver's steps for the tests, which judge a push over a view of their own.
#[cfg(test)]
pub(crate) fn prepare_whole<V: Repository>(
  state: &mut ShardState,
  repository: RepositoryId,
  unbundle: &Unbundle,
  view: &V,
  terms: Terms,
  room: &mut Admission,
) -> Result<Prepared, Refused> {
  let forked = forked_whole(state, repository, unbundle, room)?;
  let prepared = match state.vcs_stores.mercurial_ref(repository) {
    None => Err(abort(NO_STORE.to_vec())),
    Some((content, store)) => {
      let held = IndexHeld::new(
        forked.as_ref().map_or(&store.index, |forked| &forked.fork),
        content,
      );
      brought_of(forked.as_ref(), content, room).and_then(|(brought, held_changesets)| {
        prepare(
          state,
          repository,
          unbundle,
          (view, &held),
          (&brought, &held_changesets),
          terms,
          room,
        )
      })
    }
  };
  attach(state, prepared, forked)
}

/// [`prepare_whole`] over the served view.
#[cfg(test)]
pub(crate) fn prepare_served(
  state: &mut ShardState,
  repository: RepositoryId,
  unbundle: &Unbundle,
  terms: Terms,
  room: &mut Admission,
) -> Result<Prepared, Refused> {
  let forked = forked_whole(state, repository, unbundle, room)?;
  let over = forked.as_ref().map(|forked| &forked.fork);
  let prepared = hg_view::with_view_over(
    state,
    repository,
    over,
    room,
    |view, held, content, room| {
      brought_of(forked.as_ref(), content, room).and_then(|(brought, held_changesets)| {
        prepare(
          state,
          repository,
          unbundle,
          (view, &held),
          (&brought, &held_changesets),
          terms,
          room,
        )
      })
    },
  )
  .unwrap_or_else(|| Err(abort(NO_STORE.to_vec())));
  attach(state, prepared, forked)
}

/// `unbundle`'s changegroup taken whole into a fork, none without one.
#[cfg(test)]
fn forked_whole(
  state: &mut ShardState,
  repository: RepositoryId,
  unbundle: &Unbundle,
  room: &mut Admission,
) -> Result<Option<Forked>, Refused> {
  let Some((_, part)) = unbundle.changegroup() else {
    return Ok(None);
  };
  match take_whole(state, repository, part, room) {
    Some(Ok(forked)) => Ok(Some(forked)),
    Some(Err(refused)) => Err(refused),
    None => Err(abort(NO_STORE.to_vec())),
  }
}

/// The prepared push holding `forked`, or the refusal with the fork given back.
#[cfg(test)]
fn attach(
  state: &mut ShardState,
  prepared: Result<Prepared, Refused>,
  forked: Option<Forked>,
) -> Result<Prepared, Refused> {
  match prepared {
    Ok(mut prepared) => {
      prepared.forked = forked;
      Ok(prepared)
    }
    Err(refused) => {
      if let Some(forked) = forked {
        if release_index(state, forked.fork).is_err() {
          state.count(vcs_store::RELEASE_REFUSED, 1);
        }
      }
      Err(refused)
    }
  }
}

/// The changesets a fork's push brought, read back from its rows, and those its changegroup carried that were held:
/// none without a fork.
fn brought_of(
  forked: Option<&Forked>,
  content: &ContentStore,
  room: &mut Admission,
) -> Result<(Vec<ReadChangeset>, Vec<Node>), Refused> {
  let Some(forked) = forked else {
    return Ok((Vec::new(), Vec::new()));
  };
  let read = intake::read_changesets(&forked.fork, content, &forked.intaken, room)
    .map_err(|past| abort(unverified_words(&Unverified::PastAdmission(past))))?
    .ok_or_else(|| abort(UNREAD.to_vec()))?;
  Ok((read, forked.intaken.held_changesets.clone()))
}

/// The driver's last step (A-720): a fork made from an index another push has replaced since is refused; else the push
/// judged over the served view as it is now, its texts read through its fork, admitted, and landed. Every refusal gives
/// the fork back.
fn landing(
  state: &mut ShardState,
  repository: RepositoryId,
  (principal, bundle): (&Principal, &[u8]),
  unbundle: &Unbundle,
  forked: Option<Forked>,
  room: &mut Admission,
) -> Option<PushOutcome> {
  let moved = forked
    .as_ref()
    .is_some_and(|forked| state.vcs_stores.generation_of(repository) != Some(forked.generation));
  let judged = if moved {
    Some(Err(PushOutcome::Unprocessed(MOVED.to_vec())))
  } else {
    judged_over(
      state,
      repository,
      (principal, bundle),
      unbundle,
      forked.as_ref(),
      room,
    )
  };
  let mut prepared = match judged {
    Some(Ok(prepared)) => prepared,
    other => {
      if let Some(forked) = forked
        && release_index(state, forked.fork).is_err()
      {
        state.count(vcs_store::RELEASE_REFUSED, 1);
      }
      return other.map(|judged| {
        judged
          .err()
          .unwrap_or(PushOutcome::Unprocessed(MOVED.to_vec()))
      });
    }
  };
  prepared.forked = forked;
  let landed = land(state, (principal, repository), prepared, room);
  if landed.released.is_err() {
    state.count(vcs_store::RELEASE_REFUSED, 1);
  }
  Some(match landed.answer {
    Ok(reply) => PushOutcome::Accepted(reply),
    Err(refused) => PushOutcome::refused(&refused),
  })
}

/// The landing's judging, in 7.2's order (module docs, "The driver"): over the served view as it is at landing, the
/// push's changesets read back from its fork, its texts read through it; prepared and admitted, or what refused it; none
/// when no view could be made.
fn judged_over(
  state: &ShardState,
  repository: RepositoryId,
  (principal, bundle): (&Principal, &[u8]),
  unbundle: &Unbundle,
  forked: Option<&Forked>,
  room: &mut Admission,
) -> Option<Result<Prepared, PushOutcome>> {
  let over = forked.map(|forked| &forked.fork);
  hg_view::with_view_over(
    state,
    repository,
    over,
    room,
    |view, held, content, room| {
      let (brought, held_changesets) =
        brought_of(forked, content, room).map_err(|refused| PushOutcome::refused(&refused))?;
      let terms = (PUBLISHING, push_name(repository, bundle), room.left());
      let sources = (view, &held);
      let prepared = prepare(
        state,
        repository,
        unbundle,
        sources,
        (&brought, &held_changesets),
        terms,
        room,
      )
      .map_err(|refused| PushOutcome::refused(&refused))?;
      admitted(state, (principal, repository), &prepared)
        .map_err(|refused| PushOutcome::refused(&refused))?;
      match hook_refused(state, repository) {
        Some(refused) => Err(PushOutcome::refused(&refused)),
        None => Ok(prepared),
      }
    },
  )
}

/// Whether `unbundle`'s `heads` argument holds at `view` before the bundle is read (7.2's `check_heads`, "preparing
/// changes"): a race, and a view that cannot be judged, are refused before the bundle is processed, as 7.2's `pusherr`.
fn heads_hold(
  heads: &HeadsArgument,
  view: &impl Repository,
  room: &mut Admission,
) -> Result<(), PushOutcome> {
  check::judge_heads_argument(heads, view, Moment::Preparing, room).map_err(|refused| match refused
  {
    CheckRefusal::Raced { words } => PushOutcome::Unprocessed(words),
    CheckRefusal::View(_) => PushOutcome::Unprocessed(VIEW_REFUSED.to_vec()),
  })
}

/// A push's name (module docs, "The driver"; A-618: every work's identity begins with it), hashed from its repository
/// and its bundle's bytes.
fn push_name(repository: RepositoryId, bundle: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(PUSH_NAME_CONTEXT);
  hasher.update(&repository.bytes);
  hasher.update(bundle);
  *hasher.finalize().as_bytes()
}

/// The refusal of a push to `repository` while its policy configures a Mercurial hook (module docs, "The hook gate"),
/// naming the first.
fn hook_refused(state: &ShardState, repository: RepositoryId) -> Option<Refused> {
  let record = state.db.partition().repositories().repository(repository)?;
  let hook = record
    .policy
    .hooks
    .iter()
    .find(|hook| hook.point.tool() == VcsTool::Mercurial)?;
  let mut words = HOOK_NOT_RUN.to_vec();
  words.extend_from_slice(hook.name.as_bytes());
  words.extend_from_slice(HOOK_NOT_RUN_AFTER);
  Some(abort(words))
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
