//! The race checks a push carries, judged against the served view the push read (A-641; §4.17.3 "Push" step 4 and
//! decision 6), in 7.2's own words (`bundle2_part_handlers.py` \[C\]): `check:heads` (the heads are exactly these),
//! `check:updated-heads` (each of these is still a branch's head), `check:bookmarks` (each bookmark still names its node,
//! or is still missing) and `check:phases` (each node is still in its phase), in the order 7.2's handlers run them, the
//! parts' stream order; and `unbundle`'s own `heads` argument as `exchange.check_heads` reads it, the race check of
//! bundle1 and of clients older than bundle2's.
//!
//! A check holds or not at the view the push read. Where the push lands (A-642), each green and log a check read is
//! compared again at the position the push read it at, so what was judged here still holds when the push lands (the
//! landing is the piece's next part).
//!
//! **7.2's edge cases, kept.** An empty repository's heads are the null node alone (`localrepo.heads` gives
//! `[nullid]`), so a client that saw an empty repository sends it; and the null node alone is a branch head only of an
//! empty repository (`branchmap.all_nodes_are_heads`). `check:phases` is judged phase by phase in 7.2's order (public,
//! draft, secret, archived, internal), each phase's nodes in the payload's order, as `phases.binarydecode`'s mapping
//! hands them to the handler; the null node is public. A node the view does not hold is a race in the plain words:
//! 7.2 looks the node up and fails with an exception, its server's answer an error.
//!
//! **Bounds.** Every walk of the view and every list kept is charged to the request's admission first.

use std::collections::BTreeMap;

use super::{Asks, BookmarkAt, Unbundle};
use crate::phases::{Phase, PhaseHead};
use crate::read::to_u64;
use crate::serve::discovery::{branch_head_revs, head_revs};
use crate::serve::{self, Repository, Rev, node_of};
use crate::{Admission, NULL_NODE, Node, PastAdmission, node};

/// Format: the words of 7.2's race checks (`handlecheckheads`, `handlecheckupdatedheads`), and the start of the
/// bookmarks' and phases' longer words (`handlecheckbookmarks`, `handlecheckphases`).
pub const RACED: &[u8] = b"remote repository changed while pushing - please try again";

/// Format: the hexadecimal digits of 7.2's `short`, its node's first six bytes (`hex(node[:6])`).
const SHORT_DIGITS: usize = 12;

/// Format: the hexadecimal digits of a whole node.
const NODE_DIGITS: usize = 40;

/// Format: the base of a `heads` argument's digits, and the digits that write one byte.
const HEX_RADIX: u32 = 16;
/// Format: see [`HEX_RADIX`].
const BYTE_DIGITS: usize = 2;

/// Format: the separator of a `heads` argument's items (`wireprototypes.decodelist`).
const SPACE: u8 = b' ';

/// Format: the order 7.2 judges `check:phases` in, its `allphases` (`phases.py`).
const PHASE_ORDER: [Phase; 5] = [
  Phase::Public,
  Phase::Draft,
  Phase::Secret,
  Phase::Archived,
  Phase::Internal,
];

/// Format: `unbundle`'s `heads` argument's two words besides nodes (`exchange.check_heads`), and the start and end of
/// its race's words (`b'repository changed while %s - please try again' % context`).
const FORCE: &[u8] = b"force";
/// Format: see [`FORCE`].
const HASHED: &[u8] = b"hashed";
/// Format: see [`FORCE`].
const CHANGED_WHILE: &[u8] = b"repository changed while ";
/// Format: see [`FORCE`].
const TRY_AGAIN: &[u8] = b" - please try again";

/// Why a push's checks did not hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckRefusal {
  /// The repository moved since the client read it: 7.2's words for the check that found it, which the reply's
  /// `error:pushraced` carries.
  Raced {
    /// The words.
    words: Vec<u8>,
  },
  /// The view broke its own rules, or judging it would take more than the request's admission had left.
  View(serve::Refusal),
}

impl From<serve::Refusal> for CheckRefusal {
  fn from(refusal: serve::Refusal) -> Self {
    Self::View(refusal)
  }
}

impl From<PastAdmission> for CheckRefusal {
  fn from(past: PastAdmission) -> Self {
    Self::View(serve::Refusal::PastAdmission(past))
  }
}

/// Judges every check `unbundle` carries against `view`, in stream order; the first that does not hold refuses the push
/// in 7.2's words.
pub fn judge(
  unbundle: &Unbundle,
  view: &impl Repository,
  admission: &mut Admission,
) -> Result<(), CheckRefusal> {
  for taken in &unbundle.parts {
    match &taken.asks {
      Asks::CheckHeads(claimed) => all_heads(claimed, view, admission)?,
      Asks::CheckUpdatedHeads(claimed) => updated_heads(claimed, view, admission)?,
      Asks::CheckBookmarks(claimed) => bookmarks(claimed, view, admission)?,
      Asks::CheckPhases(claimed) => phases(claimed, view)?,
      _ => {}
    }
  }
  Ok(())
}

/// A race in `words`.
fn raced(words: Vec<u8>) -> CheckRefusal {
  CheckRefusal::Raced { words }
}

/// `check:heads`: the view's heads, sorted, are exactly `claimed`, sorted (`handlecheckheads`).
fn all_heads(
  claimed: &[Node],
  view: &impl Repository,
  admission: &mut Admission,
) -> Result<(), CheckRefusal> {
  let mut held = heads_now(view, admission)?;
  admission.charge_items::<Node>(claimed.len())?;
  let mut claimed = claimed.to_vec();
  held.sort_unstable();
  claimed.sort_unstable();
  if held == claimed {
    Ok(())
  } else {
    Err(raced(RACED.to_vec()))
  }
}

/// The view's heads as 7.2's `repo.heads()` gives them: newest first, and the null node alone for an empty repository.
fn heads_now(view: &impl Repository, admission: &mut Admission) -> Result<Vec<Node>, CheckRefusal> {
  let revs = head_revs(view, admission)?;
  admission.charge_items::<Node>(revs.len().max(1))?;
  if revs.is_empty() {
    return Ok(vec![NULL_NODE]);
  }
  nodes_of(view, revs.iter().rev())
}

/// The nodes of `revs`, in their order.
fn nodes_of<'r>(
  view: &impl Repository,
  revs: impl Iterator<Item = &'r Rev>,
) -> Result<Vec<Node>, CheckRefusal> {
  revs
    .map(|rev| node_of(view, *rev).map_err(CheckRefusal::View))
    .collect()
}

/// `check:updated-heads`: each node of `claimed` is the head of some branch, closed ones included; the null node alone
/// only in an empty repository (`handlecheckupdatedheads`, `all_nodes_are_heads`).
fn updated_heads(
  claimed: &[Node],
  view: &impl Repository,
  admission: &mut Admission,
) -> Result<(), CheckRefusal> {
  if claimed == [NULL_NODE] {
    return if view.count() == 0 {
      Ok(())
    } else {
      Err(raced(RACED.to_vec()))
    };
  }
  let revs = branch_head_revs(view, admission)?;
  admission.charge_items::<Node>(revs.len())?;
  let mut heads = nodes_of(view, revs.iter())?;
  heads.sort_unstable();
  if claimed.iter().all(|node| heads.binary_search(node).is_ok()) {
    Ok(())
  } else {
    Err(raced(RACED.to_vec()))
  }
}

/// `check:bookmarks`: each bookmark of `claimed` names its node now, or is missing where it says none
/// (`handlecheckbookmarks`).
fn bookmarks(
  claimed: &[BookmarkAt],
  view: &impl Repository,
  admission: &mut Admission,
) -> Result<(), CheckRefusal> {
  admission.charge_items::<(&[u8], Node)>(view.bookmarks().count())?;
  let held: BTreeMap<&[u8], Node> = view.bookmarks().collect();
  for mark in claimed {
    let now = held.get(mark.name.as_slice()).copied();
    if now != mark.node {
      return Err(raced(bookmark_words(mark, now)));
    }
  }
  Ok(())
}

/// 7.2's words for a bookmark that is not where the client saw it: set where it saw none, missing where it saw one, or
/// moved from the node it saw to the node it names now.
fn bookmark_words(mark: &BookmarkAt, now: Option<Node>) -> Vec<u8> {
  let mut words = RACED.to_vec();
  let name = |words: &mut Vec<u8>| {
    words.extend_from_slice(b" (bookmark \"");
    words.extend_from_slice(&mark.name);
  };
  match (mark.node, now) {
    (None, Some(now)) => {
      name(&mut words);
      words.extend_from_slice(b"\" set on ");
      short(&now, &mut words);
      words.extend_from_slice(b", expected missing)");
    }
    (Some(saw), None) => {
      name(&mut words);
      words.extend_from_slice(b"\" is missing, expected ");
      short(&saw, &mut words);
      words.push(b')');
    }
    (Some(saw), Some(now)) => {
      name(&mut words);
      words.extend_from_slice(b"\" move from ");
      short(&saw, &mut words);
      words.extend_from_slice(b" to ");
      short(&now, &mut words);
      words.push(b')');
    }
    // Not a race: the caller asks only where the two differ.
    (None, None) => {}
  }
  words
}

/// `check:phases`: each node of `claimed` is in its phase now, judged phase by phase in 7.2's order
/// (`handlecheckphases`).
fn phases(claimed: &[PhaseHead], view: &impl Repository) -> Result<(), CheckRefusal> {
  for phase in PHASE_ORDER {
    for entry in claimed.iter().filter(|entry| entry.phase == phase) {
      let actual = if entry.node == NULL_NODE {
        Phase::Public
      } else {
        let rev = view.rev(&entry.node).ok_or_else(|| raced(RACED.to_vec()))?;
        view.phase(rev)
      };
      if actual != entry.phase {
        return Err(raced(phase_words(entry, actual)));
      }
    }
  }
  Ok(())
}

/// 7.2's words for a node not in the phase the client saw it in.
fn phase_words(entry: &PhaseHead, actual: Phase) -> Vec<u8> {
  let mut words = RACED.to_vec();
  words.extend_from_slice(b" (");
  short(&entry.node, &mut words);
  words.extend_from_slice(b" is ");
  words.extend_from_slice(actual.name().as_bytes());
  words.extend_from_slice(b" expected ");
  words.extend_from_slice(entry.phase.name().as_bytes());
  words.push(b')');
  words
}

/// Appends 7.2's `short` of `node`: its first six bytes in lower-case hexadecimal.
fn short(node: &Node, out: &mut Vec<u8>) {
  let mut hex = Vec::with_capacity(NODE_DIGITS);
  node::write_hex(node, &mut hex);
  out.extend(hex.iter().take(SHORT_DIGITS));
}

/// `unbundle`'s `heads` argument, as 7.2's `decodelist` reads it and `check_heads` tells its forms apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadsArgument {
  /// `force`: no race check (a bundle2 push sends it, and a forced bundle1 push).
  Force,
  /// `hashed` and a digest: the SHA-1 of the heads the client saw, sorted and joined.
  Hashed(Vec<u8>),
  /// The heads the client saw, in the order the server listed them (or any other values sent).
  Listed(Vec<Vec<u8>>),
}

/// Why a `heads` argument could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadsArgumentRefusal {
  /// An item that is not an even number of hexadecimal digits (7.2's `bin` fails on it).
  NotHex {
    /// The item's position in the list.
    at: u64,
  },
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
}

/// When the `heads` argument is judged, which 7.2's words name (`check_heads`'s `context`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
  /// Before the bundle is read (`wireprotov1server.unbundle`: "preparing changes").
  Preparing,
  /// After it was read, as the push lands (`exchange.unbundle`: "uploading changes").
  Uploading,
}

impl Moment {
  /// 7.2's word for the moment.
  fn words(self) -> &'static [u8] {
    match self {
      Self::Preparing => b"preparing changes",
      Self::Uploading => b"uploading changes",
    }
  }
}

/// Reads `unbundle`'s `heads` argument (`decodelist`: hexadecimal items separated by spaces, none in an empty value),
/// each item and the list charged to `admission` first.
pub fn heads_argument(
  value: &[u8],
  admission: &mut Admission,
) -> Result<HeadsArgument, HeadsArgumentRefusal> {
  if value.is_empty() {
    return Ok(HeadsArgument::Listed(Vec::new()));
  }
  let items = value.split(|byte| *byte == SPACE);
  let count = items.clone().count();
  let bytes = items.clone().fold(0_u64, |total, item| {
    total.saturating_add(to_u64(
      item.len().checked_div(BYTE_DIGITS).unwrap_or_default(),
    ))
  });
  admission
    .charge_items::<Vec<u8>>(count)
    .and_then(|()| admission.charge(bytes))
    .map_err(HeadsArgumentRefusal::PastAdmission)?;
  let mut decoded = Vec::with_capacity(count);
  for (at, item) in items.enumerate() {
    decoded.push(unhex(item).ok_or(HeadsArgumentRefusal::NotHex { at: to_u64(at) })?);
  }
  Ok(match decoded.as_slice() {
    [only] if only == FORCE => HeadsArgument::Force,
    [first, digest] if first == HASHED => HeadsArgument::Hashed(digest.clone()),
    _ => HeadsArgument::Listed(decoded),
  })
}

/// The bytes an even number of hexadecimal digits, of either case, write; none for anything else (`binascii.unhexlify`).
fn unhex(digits: &[u8]) -> Option<Vec<u8>> {
  let (pairs, rest) = digits.as_chunks::<BYTE_DIGITS>();
  if !rest.is_empty() {
    return None;
  }
  pairs
    .iter()
    .map(|[high, low]| {
      let high = char::from(*high).to_digit(HEX_RADIX)?;
      let low = char::from(*low).to_digit(HEX_RADIX)?;
      u8::try_from(high.checked_mul(HEX_RADIX)?.checked_add(low)?).ok()
    })
    .collect()
}

/// Judges `unbundle`'s `heads` argument against `view` at `moment` (`exchange.check_heads`): `force` holds; a list holds
/// when it is the view's heads in the order `heads` lists them; a digest when it is the SHA-1 of the view's heads,
/// sorted and joined. A race is answered in 7.2's words for the moment.
pub fn judge_heads_argument(
  argument: &HeadsArgument,
  view: &impl Repository,
  moment: Moment,
  admission: &mut Admission,
) -> Result<(), CheckRefusal> {
  let holds = match argument {
    HeadsArgument::Force => true,
    HeadsArgument::Listed(listed) => {
      let heads = heads_now(view, admission)?;
      listed.len() == heads.len()
        && listed
          .iter()
          .zip(&heads)
          .all(|(item, head)| item.as_slice() == head.as_slice())
    }
    HeadsArgument::Hashed(digest) => {
      let mut heads = heads_now(view, admission)?;
      heads.sort_unstable();
      admission.charge_items::<Node>(heads.len())?;
      let joined = heads.concat();
      node::sha1(&joined).is_ok_and(|sha| digest.as_slice() == sha.as_slice())
    }
  };
  if holds {
    return Ok(());
  }
  Err(raced([CHANGED_WHILE, moment.words(), TRY_AGAIN].concat()))
}
