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
use crate::state::ShardState;

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

/// A push ready to land: the entry it appends, what it keeps in the index, and its reply. What its admission asks and
/// what its hooks name come with the policy and the hooks (A-657 part 4).
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
    verified,
  })
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
  }
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
