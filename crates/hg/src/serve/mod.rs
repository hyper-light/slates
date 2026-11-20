//! Read serving's logic (A-619; §4.17.3 "Pull and clone" items 1 to 3, piece 4's first part): what a server answers to
//! discovery (`heads`, `known`, `between`, `branches`, `branchmap`; [`discovery`]) and to `listkeys`, and which
//! changesets a `getbundle` sends, with the phases and bookmarks beside them ([`pull`]), all over a [`Repository`]: the
//! served view's changelog graph and names, which the index (A-588's tables, later) presents as a test's history does.
//!
//! **Sources** (2026-11-19; Mercurial 7.2's code \[C\], `mercurial-7.2.tar.gz`, SHA-256 `a250227e…`).
//! `wireprotov1server.py` (`heads`, `known`, `between`, `branches`, `branchmap`, `listkeys`), `localrepo.py`
//! (`heads`: `reversed(headrevs())`; `known`; `between`; `branches`), `branchmap.py` (a branch's heads in revision
//! order, closed ones kept), `exchange.py` (`_computeoutgoing`, `_getbundlephasespart`, `_getbundlebookmarkpart`),
//! `discovery.py` (`outgoing`), `phases.py` (`listphases`), `bookmarks.py` (`listbookmarks`, `listbinbookmarks`),
//! `pushkey.py` (the namespaces), and `setdiscovery.py`, whose sampling the tests drive against these answers.
//!
//! **Transports.** [`session`] answers SSH version 1's requests one at a time from a buffer (A-619 part 3), and
//! [`stdio`] runs such a session over a byte channel, the SSH server's session channel or any other carrier of
//! `hg serve --stdio`'s standard streams (A-650 part 2). [`http`] answers HTTP version 1's requests with the same
//! commands, framed as 7.2 frames them over HTTP (A-650 part 1).
//!
//! **The served view.** A [`Repository`] numbers the changesets it serves `0` to `count() - 1`, every parent before
//! its child, as a revlog numbers them; it holds no secret and no hidden changeset (§4.17.3 "Pull and clone" item 2),
//! and since a visible changeset's ancestors are visible, that numbering is the served view's own. [`indexed`] makes
//! one, with its [`getbundle::Store`], from a repository's index, its content store and its register's state, held to
//! 7.2's `served` filter (A-680 part 1).
//!
//! **Hostile input and work (§4.9).** A client's node lists are read within the request's [`Admission`], each item
//! exactly 40 hexadecimal digits (7.2's `bin` would take an empty item between two spaces, and answer it unknown), the
//! decoded list charged before it is built. Every walk's state is charged before it is made: a set of the served view's
//! revisions, a bit each. A walk that meets a parent not before its child, or a revision past the view, is refused as
//! the graph's fault rather than followed, so no repository can make a walk loop.

pub mod discovery;
pub mod getbundle;
pub mod http;
pub mod indexed;
pub mod pull;
pub mod session;
pub mod stdio;

use crate::read::to_u64;
use crate::{Admission, NODE_LEN, Node, PastAdmission, phases::Phase, pushkey};

/// A changeset's revision in the served view: its position, 0 first, every parent's before its child's.
pub type Rev = u32;

/// The served view of a repository (§4.17.3 "Pull and clone": no secret and no hidden changeset), as read serving reads
/// it.
pub trait Repository {
  /// How many changesets the served view holds; its revisions are `0` to `count() - 1`.
  fn count(&self) -> Rev;

  /// The node of a revision, `None` past the view.
  fn node(&self, rev: Rev) -> Option<Node>;

  /// The revision of a node, `None` where the served view does not hold it (the null node among them).
  fn rev(&self, node: &Node) -> Option<Rev>;

  /// A revision's parents, each before it; `None` for a null parent.
  fn parents(&self, rev: Rev) -> [Option<Rev>; 2];

  /// The named branch a revision is on (`default` where its extras name none).
  fn branch(&self, rev: Rev) -> &[u8];

  /// Whether a revision closes its branch (`close` among its extras).
  fn closes(&self, rev: Rev) -> bool;

  /// A revision's phase: public or draft, in a served view.
  fn phase(&self, rev: Rev) -> Phase;

  /// Whether the repository publishes what it serves and receives (7.2's `phases.publish`, true by default).
  fn publishing(&self) -> bool;

  /// The repository's bookmarks, each its name and the node it names, in any order; one naming a changeset the served
  /// view does not hold, or with a divergent name (`crate::bookmarks::divergent`, A-690), is left out wherever
  /// bookmarks are answered, as 7.2 leaves it out.
  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)>;

  /// The repository's stored draft roots whose phase is still draft, each by node, in any order: read past the served
  /// filter, from the whole changelog, so a hidden one is among them, as 7.2's `listphases` lists them for `listkeys
  /// phases` (`phases.py` 1049-1058: each root of `_phaseroots[draft]` from `repo.unfiltered().changelog`, kept where
  /// `phase(root) <= draft`; A-691).
  fn draft_roots(&self) -> impl Iterator<Item = Node>;
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// An item of a node list that is not 40 hexadecimal digits.
  NodeList {
    /// The item's position in the list.
    at: u64,
  },
  /// A `between` pair that is not two nodes joined by `-`.
  Pair {
    /// The pair's position in the list.
    at: u64,
  },
  /// A node the served view does not hold where one must be: a `between` pair's top, a `branches` node, a head a
  /// `getbundle` asks for.
  UnknownNode {
    /// The node.
    node: Node,
  },
  /// A name the answer's format cannot carry: a bookmark holding a tab or a line end, which 7.2 refuses to make.
  Unencodable {
    /// What could not be written.
    what: &'static str,
  },
  /// A `getbundle` that names no bundle capability starting `HG2` (`bundle2requested`): a bundle1 client, which is not
  /// served here.
  NoBundle2,
  /// Something asked that the client cannot read: no changegroup version, bookmarks, phases or marker format both
  /// sides hold (7.2 aborts: "no common ... exchange method").
  NoCommonExchange {
    /// What is missing.
    what: &'static str,
  },
  /// The store lacks what its graph names: a changeset's text, a manifest or file revision, or holds one no reader
  /// takes.
  Store {
    /// What is missing or unreadable.
    what: &'static str,
  },
  /// The repository's graph broke its own rule at this revision: a parent not before its child, or a revision past
  /// the view.
  Graph {
    /// The revision.
    rev: Rev,
  },
  /// More room than the request's admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// A node list as 7.2's `decodelist` reads one: items separated by `separator`, none in an empty value, each exactly 40
/// hexadecimal digits in either case; the list charged to `admission` before it is built.
pub(crate) fn node_list(
  list: &[u8],
  separator: u8,
  admission: &mut Admission,
) -> Result<Vec<Node>, Refusal> {
  if list.is_empty() {
    return Ok(Vec::new());
  }
  let items = list.split(|byte| *byte == separator);
  admission.charge_items::<Node>(items.clone().count())?;
  let mut nodes = Vec::with_capacity(items.clone().count());
  for (at, item) in items.enumerate() {
    match pushkey::hex_node(item) {
      Ok(Some(node)) if item.len() == NODE_LEN.saturating_mul(2) => nodes.push(node),
      _ => return Err(Refusal::NodeList { at: to_u64(at) }),
    }
  }
  Ok(nodes)
}

/// A revision's parents, each checked to come before it.
pub(crate) fn parents_of(repo: &impl Repository, rev: Rev) -> Result<[Option<Rev>; 2], Refusal> {
  let parents = repo.parents(rev);
  if rev >= repo.count() || parents.iter().flatten().any(|parent| *parent >= rev) {
    return Err(Refusal::Graph { rev });
  }
  Ok(parents)
}

/// A revision's node, refused as the graph's fault past the view.
pub(crate) fn node_of(repo: &impl Repository, rev: Rev) -> Result<Node, Refusal> {
  repo.node(rev).ok_or(Refusal::Graph { rev })
}

/// A set of the served view's revisions, a bit each, its words charged before they are made.
pub(crate) struct RevSet {
  /// The bits, `u64::BITS` revisions a word.
  words: Vec<u64>,
}

impl RevSet {
  /// An empty set over `count` revisions.
  pub(crate) fn new(count: Rev, admission: &mut Admission) -> Result<Self, Refusal> {
    let words = usize::try_from(count.div_ceil(u64::BITS)).unwrap_or(usize::MAX);
    admission.charge_items::<u64>(words)?;
    Ok(Self {
      words: vec![0; words],
    })
  }

  /// The word holding `rev` and `rev`'s bit in it.
  fn place(rev: Rev) -> (usize, u64) {
    let word =
      usize::try_from(rev.checked_div(u64::BITS).unwrap_or_default()).unwrap_or(usize::MAX);
    let bit = 1_u64
      .checked_shl(rev.checked_rem(u64::BITS).unwrap_or_default())
      .unwrap_or_default();
    (word, bit)
  }

  /// Adds `rev`; a revision past the set is not held.
  pub(crate) fn insert(&mut self, rev: Rev) {
    let (word, bit) = Self::place(rev);
    if let Some(word) = self.words.get_mut(word) {
      *word |= bit;
    }
  }

  /// Removes `rev`.
  pub(crate) fn remove(&mut self, rev: Rev) {
    let (word, bit) = Self::place(rev);
    if let Some(word) = self.words.get_mut(word) {
      *word &= !bit;
    }
  }

  /// Whether the set holds `rev`.
  pub(crate) fn contains(&self, rev: Rev) -> bool {
    let (word, bit) = Self::place(rev);
    self.words.get(word).is_some_and(|word| word & bit != 0)
  }
}

#[cfg(test)]
mod tests;
