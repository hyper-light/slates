//! The rules one principal meets in one repository (A-606): `authz.c`'s `create_user_authz`, `trim_tree`,
//! `finalize_tree` and `lookup`, and `authz_info.c`'s `svn_authz__get_acl_access` and `svn_authz__get_global_rights`
//! (Subversion's `1.14.x` branch at `d434dc14`).
//!
//! The rules that apply to the principal are folded into a tree with one node per rule-path segment, literals by name
//! and the wildcard kinds beside them. A node keeps the rule placed on it (its sequence number and its access) and the
//! least and the most access of every rule at and beneath it. A path is walked level by level: each level's nodes are
//! the children of the last level's that its segment matches, a `**` node staying in reach at every level and its own
//! level too, and a level whose nodes hold no rule inherits the level above's. The rules eclipsed by a later `**` above
//! them are taken out first, as `trim_tree` takes them out, which changes no point answer and keeps the recursive ones
//! Subversion's.
//!
//! Where a level's nodes are reached by two routes (a `**` node by its parent and by its own repetition), Subversion
//! walks the node once per route, so a path with many segments under nested `**` rules costs it more each level; the
//! walk here keeps each node once a level, which changes no answer.

use std::cell::Cell;
use std::collections::BTreeMap;

use super::glob::{self, Segment};
use super::model::{Acl, Model, Selector};
use super::{Access, GlobalRights, Principal, RepositoryPath};

/// The root's place in the tree.
/// Format: the first node made.
const ROOT: usize = 0;

/// The sequence number of the default rule at the root, which every rule of the file outranks.
/// Format: `acl->acl.sequence_number = 0` (`authz_parse.c` 196); rule sections count from one.
const DEFAULT_SEQUENCE: usize = 0;

/// Format: four 64-bit words cover every possible 8-bit byte (`256 / 64`).
pub const BYTE_SET_WORDS: usize = 4;

/// Format: `log2(64)` bits select the bit offset within one `u64` word of a 256-bit byte set.
const BYTE_WORD_SHIFT: u8 = 6;

/// Format: low six bits (`0..64`) index the bit within one `u64` word of a 256-bit byte set.
const BYTE_BIT_MASK: u8 = 63;

/// One token of a compiled single-segment glob pattern (shared with the server's `SvnPolicy` evaluator).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GlobToken {
  /// Matches exactly this byte.
  Byte(u8),
  /// Matches any single byte (`?`).
  AnyByte,
  /// Matches any single byte whose bit is set (`[...]` character class; bit `byte & 63` in word `byte >> 6`).
  ByteSet([u64; BYTE_SET_WORDS]),
  /// Matches zero or more arbitrary bytes (`*`).
  Run,
  /// Never matches any segment (for example an unclosed `[` or trailing `\`).
  Never,
}

/// One segment of a compiled policy read rule ([`PrincipalRules::from_read_rules`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicySegment {
  /// An exact path segment.
  Literal(Box<[u8]>),
  /// Matches any single non-empty segment (`*`).
  AnyOne,
  /// Matches zero or more segments (`**`).
  AnyMany,
  /// Matches a single segment against compiled glob tokens.
  Glob(Box<[GlobToken]>),
}

/// One path-scoped read rule in definition order (earlier to later; later wins at the same depth).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPathRule {
  /// The rule's path segments (empty means the repository root `/`).
  pub segments: Vec<PolicySegment>,
  /// Whether this rule grants read (`true`) or denies read (`false`).
  pub allow: bool,
}

/// Whether `byte` is present in the 256-bit membership table `bits`.
pub fn byte_set_contains(bits: &[u64; BYTE_SET_WORDS], byte: u8) -> bool {
  let word = usize::from(byte >> BYTE_WORD_SHIFT);
  let mask = 1u64 << u32::from(byte & BYTE_BIT_MASK);
  bits.get(word).is_some_and(|entry| (*entry & mask) != 0)
}

/// Whether a single non-run `token` matches `byte`.
fn token_matches_byte(token: GlobToken, byte: u8) -> bool {
  match token {
    GlobToken::Byte(expected) => byte == expected,
    GlobToken::AnyByte => true,
    GlobToken::ByteSet(bits) => byte_set_contains(&bits, byte),
    GlobToken::Run | GlobToken::Never => false,
  }
}

/// Whether compiled `tokens` match the path segment `name` whole (iterative two-pointer wildmatch, no recursion).
fn matches_tokens(tokens: &[GlobToken], name: &[u8]) -> bool {
  if tokens.contains(&GlobToken::Never) {
    return false;
  }
  let (mut ti, mut ni) = (0usize, 0usize);
  let mut star: Option<(usize, usize)> = None;
  while let Some(&byte) = name.get(ni) {
    match tokens.get(ti).copied() {
      Some(GlobToken::Run) => {
        ti = ti.saturating_add(1);
        star = Some((ti, ni));
      }
      Some(token) if token_matches_byte(token, byte) => {
        ti = ti.saturating_add(1);
        ni = ni.saturating_add(1);
      }
      _ => {
        let Some((saved_ti, saved_ni)) = star else {
          return false;
        };
        let next_ni = saved_ni.saturating_add(1);
        ti = saved_ti;
        ni = next_ni;
        star = Some((saved_ti, next_ni));
      }
    }
  }
  tokens
    .get(ti..)
    .is_some_and(|rest| rest.iter().all(|token| *token == GlobToken::Run))
}

/// A rule as placed on a node: its sequence number and what it gives the principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mark {
  sequence: usize,
  access: Access,
}

/// The later of two rules, by sequence number (`combine_access`): the one defined last in the file wins.
fn later(held: Option<Mark>, offered: Option<Mark>) -> Option<Mark> {
  match (held, offered) {
    (Some(held), Some(offered)) => Some(if offered.sequence > held.sequence {
      offered
    } else {
      held
    }),
    (held, None) => held,
    (None, offered) => offered,
  }
}

/// One node of the tree (`authz.c` `node_t`).
#[derive(Clone, Debug)]
struct Node {
  /// The rule placed here, unless eclipsed.
  rule: Option<Mark>,
  /// The least access of the rules at and beneath this node; `Write` when there are none.
  least: Access,
  /// The most access of the rules at and beneath this node; `None` when there are none.
  most: Access,
  literals: BTreeMap<Box<[u8]>, usize>,
  prefixes: BTreeMap<Box<[u8]>, usize>,
  suffixes: BTreeMap<Box<[u8]>, usize>,
  patterns: BTreeMap<Box<[u8]>, usize>,
  globs: BTreeMap<Box<[GlobToken]>, usize>,
  any_one: Option<usize>,
  any_many: Option<usize>,
  /// This node is a `**`, matched again at every level beneath its parent.
  repeats: bool,
}

impl Node {
  /// A node without rules or children.
  fn new(repeats: bool) -> Self {
    Self {
      rule: None,
      least: Access::Write,
      most: Access::None,
      literals: BTreeMap::new(),
      prefixes: BTreeMap::new(),
      suffixes: BTreeMap::new(),
      patterns: BTreeMap::new(),
      globs: BTreeMap::new(),
      any_one: None,
      any_many: None,
      repeats,
    }
  }

  /// The child for `segment`, if there is one.
  fn child(&self, segment: &Segment) -> Option<usize> {
    match segment {
      Segment::Literal(bytes) => self.literals.get(bytes).copied(),
      Segment::Prefix(bytes) => self.prefixes.get(bytes).copied(),
      Segment::Suffix(bytes) => self.suffixes.get(bytes).copied(),
      Segment::Pattern(bytes) => self.patterns.get(bytes).copied(),
      Segment::AnyOne => self.any_one,
      Segment::AnyMany => self.any_many,
    }
  }

  /// The child for policy `segment`, if there is one.
  fn policy_child(&self, segment: &PolicySegment) -> Option<usize> {
    match segment {
      PolicySegment::Literal(bytes) => self.literals.get(bytes).copied(),
      PolicySegment::AnyOne => self.any_one,
      PolicySegment::AnyMany => self.any_many,
      PolicySegment::Glob(tokens) => self.globs.get(tokens).copied(),
    }
  }

  /// Makes `child` the child for `segment`.
  fn attach(&mut self, segment: &Segment, child: usize) {
    match segment {
      Segment::Literal(bytes) => {
        self.literals.insert(bytes.clone(), child);
      }
      Segment::Prefix(bytes) => {
        self.prefixes.insert(bytes.clone(), child);
      }
      Segment::Suffix(bytes) => {
        self.suffixes.insert(bytes.clone(), child);
      }
      Segment::Pattern(bytes) => {
        self.patterns.insert(bytes.clone(), child);
      }
      Segment::AnyOne => self.any_one = Some(child),
      Segment::AnyMany => self.any_many = Some(child),
    }
  }

  /// Makes `child` the child for policy `segment`.
  fn attach_policy(&mut self, segment: &PolicySegment, child: usize) {
    match segment {
      PolicySegment::Literal(bytes) => {
        self.literals.insert(bytes.clone(), child);
      }
      PolicySegment::AnyOne => self.any_one = Some(child),
      PolicySegment::AnyMany => self.any_many = Some(child),
      PolicySegment::Glob(tokens) => {
        self.globs.insert(tokens.clone(), child);
      }
    }
  }

  /// Every child.
  fn children(&self) -> impl Iterator<Item = usize> + '_ {
    self.single_segment_children().chain(self.any_many)
  }

  /// Immediate children matching strictly one segment at depth +1 (everything except `**`).
  fn single_segment_children(&self) -> impl Iterator<Item = usize> + '_ {
    self
      .literals
      .values()
      .chain(self.prefixes.values())
      .chain(self.suffixes.values())
      .chain(self.patterns.values())
      .chain(self.globs.values())
      .copied()
      .chain(self.any_one)
  }
}

/// What a walk knows at one level: the rule that decides it, and the least and most access at and beneath it.
#[derive(Clone, Copy, Debug)]
struct Level {
  rule: Option<Mark>,
  least: Access,
  most: Access,
}

impl Level {
  /// A level no node has reached yet.
  const EMPTY: Self = Self {
    rule: None,
    least: Access::Write,
    most: Access::None,
  };

  /// Takes in a node reached at this level (`add_next_node`).
  fn absorb(&mut self, node: &Node) {
    self.rule = later(self.rule, node.rule);
    self.least = self.least.min(node.least);
    self.most = self.most.max(node.most);
  }

  /// The access the deciding rule gives.
  fn access(self) -> Access {
    self.rule.map_or(Access::None, |mark| mark.access)
  }
}

/// The rules one principal meets in one repository, compiled to answer every path ([`super::Authz::rules`]).
#[derive(Clone, Debug)]
pub struct PrincipalRules {
  nodes: Vec<Node>,
  /// The access at every path and beneath it, when every rule for the principal gives the same.
  uniform: Option<Access>,
  /// The answer to a question without a path.
  anywhere: Access,
  /// How many answers the uniform access gave without a walk (the fast path's non-vacuity counter).
  uniform_answers: Cell<u64>,
}

impl PrincipalRules {
  /// Compiles the rules of `model` that apply to `principal` in `repository` (`create_user_authz`).
  pub(super) fn compile(model: &Model, repository: &[u8], principal: Principal<'_>) -> Self {
    let membership = Membership::of(model, principal);
    let mut tree = Tree {
      nodes: vec![Node::new(false)],
    };
    for (segments, mark) in chosen_rules(model, repository, principal, &membership) {
      tree.insert(segments, mark);
    }
    tree.place(
      ROOT,
      Mark {
        sequence: DEFAULT_SEQUENCE,
        access: Access::None,
      },
    );
    tree.trim();
    tree.summarize();
    let uniform = tree
      .nodes
      .get(ROOT)
      .and_then(|root| (root.least == root.most).then_some(root.least));
    let rights = global_rights_with(model, repository, principal, &membership);
    Self {
      nodes: tree.nodes,
      uniform,
      anywhere: rights.every_path.max(rights.some_path),
      uniform_answers: Cell::new(0),
    }
  }

  /// Compiles a principal's path-scoped read rules directly from slates' `SvnPolicy` (deepest match wins,
  /// last-defined wins at equal depth, `default_read` governs `/` when unruled).
  pub fn from_read_rules(default_read: bool, rules: &[ReadPathRule]) -> Self {
    let mut tree = Tree {
      nodes: vec![Node::new(false)],
    };
    for (index, rule) in rules.iter().enumerate() {
      let mark = Mark {
        sequence: index.saturating_add(1),
        access: if rule.allow {
          Access::Read
        } else {
          Access::None
        },
      };
      tree.insert_policy(&rule.segments, mark);
    }
    tree.place(
      ROOT,
      Mark {
        sequence: DEFAULT_SEQUENCE,
        access: if default_read {
          Access::Read
        } else {
          Access::None
        },
      },
    );
    tree.trim();
    tree.trim_single_segment_wildcards();
    tree.summarize();
    let uniform = tree
      .nodes
      .get(ROOT)
      .and_then(|root| (root.least == root.most).then_some(root.least));
    let anywhere = tree.nodes.get(ROOT).map_or(Access::None, |root| root.most);
    Self {
      nodes: tree.nodes,
      uniform,
      anywhere,
      uniform_answers: Cell::new(0),
    }
  }

  /// Rules denying every path (fail-closed default when a repository record is absent or unreadable).
  pub fn deny_all() -> Self {
    Self::from_read_rules(false, &[])
  }

  /// What the principal may do at `path`.
  pub fn access(&self, path: RepositoryPath<'_>) -> Access {
    if let Some(uniform) = self.uniform_answer() {
      return uniform;
    }
    self.walk(path).access()
  }

  /// What the principal may do at `path` and everywhere beneath it (`svn_authz_recursive`): the least access among the
  /// path's and that of every rule that could govern a path beneath it.
  pub fn access_beneath(&self, path: RepositoryPath<'_>) -> Access {
    if let Some(uniform) = self.uniform_answer() {
      return uniform;
    }
    let level = self.walk(path);
    level.least.min(level.access())
  }

  /// What the principal may do somewhere in the repository, as Subversion answers a question without a path: the most
  /// access any rule for the principal gives.
  pub fn access_anywhere(&self) -> Access {
    self.anywhere
  }

  /// The access at every path and beneath it, when every rule for the principal gives the same: then a server answers
  /// every question with one comparison and asks nothing per path.
  pub fn uniform(&self) -> Option<Access> {
    self.uniform
  }

  /// How many answers [`Self::access`] and [`Self::access_beneath`] gave from [`Self::uniform`] without a walk.
  pub fn uniform_answers(&self) -> u64 {
    self.uniform_answers.get()
  }

  /// The uniform access, counted, if there is one.
  fn uniform_answer(&self) -> Option<Access> {
    let uniform = self.uniform?;
    self
      .uniform_answers
      .set(self.uniform_answers.get().saturating_add(1));
    Some(uniform)
  }

  /// The level that decides `path` (`init_lockup_state` and `lookup`; the root, with no segments, is the level before
  /// the first segment).
  fn walk(&self, path: RepositoryPath<'_>) -> Level {
    let Some(root) = self.nodes.get(ROOT) else {
      return Level::EMPTY;
    };
    let mut level = Level {
      rule: root.rule,
      least: root.least,
      most: root.most,
    };
    let mut current = vec![ROOT];
    if let Some(many) = root.any_many
      && let Some(many_node) = self.nodes.get(many)
    {
      level.rule = later(level.rule, many_node.rule);
      current.push(many);
    }
    for name in path.segments() {
      if current.is_empty() {
        break;
      }
      let (next, here) = self.descend(&current, name, level);
      current = next;
      level = here;
    }
    level
  }

  /// The nodes and the level that the segment `name` reaches from `current`, whose level is `above`.
  fn descend(&self, current: &[usize], name: &[u8], above: Level) -> (Vec<usize>, Level) {
    let mut here = Level::EMPTY;
    let mut next = Vec::new();
    for node in current {
      self.step(*node, name, &mut here, &mut next);
    }
    if here.rule.is_none() {
      let inherited = above.access();
      here.rule = above.rule;
      here.least = here.least.min(inherited);
      here.most = here.most.max(inherited);
    }
    next.sort_unstable();
    next.dedup();
    (next, here)
  }

  /// Every child of `id` that `name` matches, and `id` itself when it is a `**`.
  fn step(&self, id: usize, name: &[u8], here: &mut Level, next: &mut Vec<usize>) {
    let Some(node) = self.nodes.get(id) else {
      return;
    };
    let exact = node.literals.get(name).copied();
    for child in exact.into_iter().chain(node.any_one) {
      self.reach(child, here, next);
    }
    if node.repeats {
      self.reach(id, here, next);
    }
    self.step_patterns(node, name, here, next);
  }

  /// The prefix, suffix, pattern and policy-glob children of `node` that `name` matches.
  fn step_patterns(&self, node: &Node, name: &[u8], here: &mut Level, next: &mut Vec<usize>) {
    let matched = matching_prefixes(node, name)
      .chain(matching_suffixes(node, name))
      .chain(matching_patterns(node, name))
      .chain(matching_globs(node, name));
    for child in matched {
      self.reach(child, here, next);
    }
  }

  /// Takes in the node `id`, and its `**` child, which matches no segment at all (`add_next_node`).
  fn reach(&self, id: usize, here: &mut Level, next: &mut Vec<usize>) {
    let Some(node) = self.nodes.get(id) else {
      return;
    };
    here.absorb(node);
    next.push(id);
    if let Some(many) = node.any_many
      && let Some(many_node) = self.nodes.get(many)
    {
      here.absorb(many_node);
      next.push(many);
    }
  }
}

/// The prefix children of `node` whose prefix `name` starts with (`add_prefix_matches`): one lookup per length of
/// `name`, none when there are no prefixes.
fn matching_prefixes<'n>(node: &'n Node, name: &'n [u8]) -> impl Iterator<Item = usize> + 'n {
  let longest = if node.prefixes.is_empty() {
    0
  } else {
    name.len()
  };
  (1..=longest).filter_map(move |length| {
    name
      .get(..length)
      .and_then(|head| node.prefixes.get(head))
      .copied()
  })
}

/// The suffix children of `node` whose suffix `name` ends with: one lookup per length of `name`, none when there are
/// no suffixes. The segment is never reversed in place (module documentation of `authz`, item 2).
fn matching_suffixes<'n>(node: &'n Node, name: &'n [u8]) -> impl Iterator<Item = usize> + 'n {
  let starts = if node.suffixes.is_empty() {
    0
  } else {
    name.len()
  };
  (0..starts).filter_map(move |start| {
    name
      .get(start..)
      .and_then(|tail| node.suffixes.get(tail))
      .copied()
  })
}

/// The pattern children of `node` that `name` matches (`add_complex_matches`).
fn matching_patterns<'n>(node: &'n Node, name: &'n [u8]) -> impl Iterator<Item = usize> + 'n {
  node
    .patterns
    .iter()
    .filter(move |(pattern, _)| glob::fnmatch(pattern, name))
    .map(|(_, child)| *child)
}

/// The compiled policy-glob children of `node` that `name` matches.
fn matching_globs<'n>(node: &'n Node, name: &'n [u8]) -> impl Iterator<Item = usize> + 'n {
  node
    .globs
    .iter()
    .filter(move |(tokens, _)| matches_tokens(tokens, name))
    .map(|(_, child)| *child)
}

/// Whether `tokens` is a non-empty single-segment wildcard matching every non-empty segment
/// (`*` or `***`, composed solely of [`GlobToken::Run`]).
fn is_run_only_glob(tokens: &[GlobToken]) -> bool {
  !tokens.is_empty() && tokens.iter().all(|token| *token == GlobToken::Run)
}

/// Whether a policy rule contains an unmatchable glob token (`GlobToken::Never`).
fn rule_is_unmatchable(segments: &[PolicySegment]) -> bool {
  segments.iter().any(|segment| match segment {
    PolicySegment::Glob(tokens) => tokens.contains(&GlobToken::Never),
    PolicySegment::Literal(_) | PolicySegment::AnyOne | PolicySegment::AnyMany => false,
  })
}

/// The tree while it is built.
struct Tree {
  nodes: Vec<Node>,
}

impl Tree {
  /// Places `mark` at the node for `segments`, making the nodes on the way (`insert_path`).
  fn insert(&mut self, segments: &[Segment], mark: Mark) {
    let mut node = ROOT;
    for segment in segments {
      node = self.child(node, segment);
    }
    self.place(node, mark);
  }

  /// Places `mark` at the node for policy `segments`, collapsing adjacent `**` segments and skipping
  /// rules containing [`GlobToken::Never`].
  fn insert_policy(&mut self, segments: &[PolicySegment], mark: Mark) {
    if rule_is_unmatchable(segments) {
      return;
    }
    let mut node = ROOT;
    let mut prev_many = false;
    for segment in segments {
      let is_many = matches!(segment, PolicySegment::AnyMany);
      if prev_many && is_many {
        continue;
      }
      prev_many = is_many;
      node = self.policy_child(node, segment);
    }
    self.place(node, mark);
  }

  /// The child of `node` for policy `segment`, made if missing. A child always comes after its parent in `nodes`.
  fn policy_child(&mut self, node: usize, segment: &PolicySegment) -> usize {
    if let Some(existing) = self
      .nodes
      .get(node)
      .and_then(|parent| parent.policy_child(segment))
    {
      return existing;
    }
    let made = self.nodes.len();
    self
      .nodes
      .push(Node::new(matches!(segment, PolicySegment::AnyMany)));
    if let Some(parent) = self.nodes.get_mut(node) {
      parent.attach_policy(segment, made);
    }
    made
  }

  /// The child of `node` for `segment`, made if missing. A child always comes after its parent in `nodes`.
  fn child(&mut self, node: usize, segment: &Segment) -> usize {
    if let Some(existing) = self
      .nodes
      .get(node)
      .and_then(|parent| parent.child(segment))
    {
      return existing;
    }
    let made = self.nodes.len();
    self
      .nodes
      .push(Node::new(matches!(segment, Segment::AnyMany)));
    if let Some(parent) = self.nodes.get_mut(node) {
      parent.attach(segment, made);
    }
    made
  }

  /// Places `mark` at `node` unless a later rule is there.
  fn place(&mut self, node: usize, mark: Mark) {
    if let Some(slot) = self.nodes.get_mut(node) {
      slot.rule = later(slot.rule, Some(mark));
    }
  }

  /// Takes out every rule a later `**` rule above it eclipses (`trim_tree`): a node's rule goes when a `**` child of
  /// it or of a node above it holds a later rule.
  fn trim(&mut self) {
    let mut pending: Vec<(usize, Option<usize>)> = vec![(ROOT, None)];
    while let Some((id, eclipsing)) = pending.pop() {
      let Some(node) = self.nodes.get(id) else {
        continue;
      };
      let own = node
        .any_many
        .and_then(|many| self.nodes.get(many))
        .and_then(|many| many.rule)
        .map(|mark| mark.sequence);
      let latest = eclipsing.max(own);
      let children: Vec<usize> = node.children().collect();
      if let Some(node) = self.nodes.get_mut(id)
        && node.rule.is_some_and(|mark| Some(mark.sequence) < latest)
      {
        node.rule = None;
      }
      pending.extend(children.into_iter().map(|child| (child, latest)));
    }
  }

  /// Highest rule sequence among immediate `*` or [`GlobToken::Run`]-only glob children of `node`
  /// that match every non-empty segment at depth +1.
  fn single_wildcard_sequence(&self, node: &Node) -> Option<usize> {
    let star_seq = node
      .any_one
      .and_then(|child| self.nodes.get(child))
      .and_then(|child| child.rule)
      .map(|mark| mark.sequence);
    let glob_seq = node
      .globs
      .iter()
      .filter(|(tokens, _)| is_run_only_glob(tokens))
      .filter_map(|(_, &child)| self.nodes.get(child))
      .filter_map(|child| child.rule)
      .map(|mark| mark.sequence)
      .max();
    star_seq.max(glob_seq)
  }

  /// Clears rules on immediate single-segment `children` strictly older than `eclipsing`.
  fn clear_eclipsed_children(&mut self, children: &[usize], eclipsing: usize) {
    for &child_id in children {
      if let Some(child) = self.nodes.get_mut(child_id)
        && child.rule.is_some_and(|mark| mark.sequence < eclipsing)
      {
        child.rule = None;
      }
    }
  }

  /// Takes out immediate single-segment child rules eclipsed by a later same-level `*` or
  /// [`GlobToken::Run`]-only glob child (without touching `**` children or deeper descendants),
  /// so `[Allow Read /a, Deny Read :glob:/*]` does not over-approximate `access_anywhere()`.
  fn trim_single_segment_wildcards(&mut self) {
    for id in 0..self.nodes.len() {
      let Some((eclipsing, children)) = self.nodes.get(id).and_then(|node| {
        let eclipsing = self.single_wildcard_sequence(node)?;
        Some((
          eclipsing,
          node.single_segment_children().collect::<Vec<_>>(),
        ))
      }) else {
        continue;
      };
      self.clear_eclipsed_children(&children, eclipsing);
    }
  }

  /// Sets every node's least and most access over its subtree (`finalize_tree`), children before parents.
  fn summarize(&mut self) {
    for id in (0..self.nodes.len()).rev() {
      let Some(node) = self.nodes.get(id) else {
        continue;
      };
      let (mut least, mut most) = node.rule.map_or((Access::Write, Access::None), |mark| {
        (mark.access, mark.access)
      });
      for child in node.children() {
        if let Some(child) = self.nodes.get(child) {
          least = least.min(child.least);
          most = most.max(child.most);
        }
      }
      if let Some(node) = self.nodes.get_mut(id) {
        node.least = least;
        node.most = most;
      }
    }
  }
}

/// The groups a principal belongs to.
struct Membership {
  groups: Vec<bool>,
}

impl Membership {
  /// The groups of `principal`; the anonymous user belongs to none.
  fn of(model: &Model, principal: Principal<'_>) -> Self {
    Self {
      groups: match principal {
        Principal::Anonymous => Vec::new(),
        Principal::User(user) => model.groups().membership(user),
      },
    }
  }

  /// Whether the principal belongs to `group`.
  fn contains(&self, group: usize) -> bool {
    self.groups.get(group).copied().unwrap_or(false)
  }
}

/// What `acl` gives `principal`, if it speaks for the principal at all (`svn_authz__get_acl_access`): the anonymous
/// user's entry for the anonymous user; for an authenticated user, the `$authenticated` entry and every entry that
/// names the user or a group of the user's, or inverted, does not, combined.
fn acl_access(acl: &Acl, principal: Principal<'_>, membership: &Membership) -> Option<Access> {
  match principal {
    Principal::Anonymous => acl.anonymous,
    Principal::User(user) => acl
      .entries
      .iter()
      .filter(|entry| {
        let named = match &entry.selector {
          Selector::User(name) => **name == *user,
          Selector::Group(group) => membership.contains(*group),
        };
        named != entry.inverted
      })
      .fold(acl.authenticated, |held, entry| {
        Some(held.map_or(entry.access, |held| held.max(entry.access)))
      }),
  }
}

/// The rules that apply to `principal` in `repository`, one per path: where a global section and the repository's own
/// have one path, the repository's if it speaks for the principal and the global one otherwise (`create_user_authz`,
/// 1.14.1, issue 4762).
fn chosen_rules<'m>(
  model: &'m Model,
  repository: &[u8],
  principal: Principal<'_>,
  membership: &Membership,
) -> Vec<(&'m [Segment], Mark)> {
  let mut paths: BTreeMap<&'m [Segment], (Option<Mark>, Option<Mark>)> = BTreeMap::new();
  for acl in model.acls() {
    let global = acl.repository.is_empty();
    if !global && *acl.repository != *repository {
      continue;
    }
    let Some(access) = acl_access(acl, principal, membership) else {
      continue;
    };
    let mark = Mark {
      sequence: acl.sequence,
      access,
    };
    let (global_mark, own_mark) = paths.entry(&acl.segments).or_default();
    if global {
      *global_mark = Some(mark);
    } else {
      *own_mark = Some(mark);
    }
  }
  paths
    .into_iter()
    .filter_map(|(segments, (global, own))| own.or(global).map(|mark| (segments, mark)))
    .collect()
}

/// Accumulated rights (`authz.h` `authz_rights_t`).
#[derive(Clone, Copy, Debug)]
struct Rights {
  least: Access,
  most: Access,
}

impl Rights {
  /// Rights before any rule (`init_rights`).
  const UNSET: Self = Self {
    least: Access::Write,
    most: Access::None,
  };

  /// These rights with one more rule's `access` (`update_rights`).
  fn with(self, access: Access) -> Self {
    Self {
      least: self.least.min(access),
      most: self.most.max(access),
    }
  }

  /// These rights and `other` together (`combine_rights`).
  fn combined(self, other: Self) -> Self {
    Self {
      least: self.least.min(other.least),
      most: self.most.max(other.most),
    }
  }
}

/// Rights accumulated over rules, as `update_global_rights` keeps them: for rules naming no repository, and for rules
/// naming the repository asked about.
#[derive(Clone, Copy, Debug)]
struct Accumulated {
  any: Rights,
  named: Option<Rights>,
}

impl Accumulated {
  /// Accumulates `rules`, each a rule's repository and the access it gives, for `repository`.
  fn of<'r>(rules: impl Iterator<Item = (&'r [u8], Access)>, repository: &[u8]) -> Self {
    let mut accumulated = Self {
      any: Rights::UNSET,
      named: None,
    };
    for (rule_repository, access) in rules {
      if rule_repository.is_empty() {
        accumulated.any = accumulated.any.with(access);
      } else if rule_repository == repository {
        accumulated.named = Some(accumulated.named.unwrap_or(Rights::UNSET).with(access));
      }
    }
    accumulated
  }

  /// The rights for `repository`, and whether they are explicit (`resolve_global_rights`).
  fn resolve(self, repository: &[u8]) -> (Rights, bool) {
    if repository.is_empty() {
      return (self.any, true);
    }
    match self.named {
      Some(named) => (named.combined(self.any), true),
      None => (self.any, false),
    }
  }
}

/// What `principal` may do anywhere in `repository`, as Subversion accumulates it (`svn_authz__get_global_rights`).
pub(super) fn global_rights(
  model: &Model,
  repository: &[u8],
  principal: Principal<'_>,
) -> GlobalRights {
  let membership = Membership::of(model, principal);
  global_rights_with(model, repository, principal, &membership)
}

/// [`global_rights`] with the principal's groups known.
fn global_rights_with(
  model: &Model,
  repository: &[u8],
  principal: Principal<'_>,
  membership: &Membership,
) -> GlobalRights {
  let (rights, explicit) = match principal {
    Principal::Anonymous => anonymous_rights(model, repository),
    Principal::User(user) => user_rights(model, repository, user, membership),
  };
  GlobalRights {
    every_path: rights.least,
    some_path: rights.most,
    explicit,
  }
}

/// The rules' repositories and the access each gives, where `pick` finds one.
fn picked(
  model: &Model,
  pick: impl Fn(&Acl) -> Option<Access>,
) -> impl Iterator<Item = (&[u8], Access)> {
  model
    .acls()
    .iter()
    .filter_map(move |acl| pick(acl).map(|access| (&*acl.repository, access)))
}

/// The anonymous user's rights: from every rule that speaks for it.
fn anonymous_rights(model: &Model, repository: &[u8]) -> (Rights, bool) {
  if !model.acls().iter().any(|acl| acl.anonymous.is_some()) {
    let nothing = Rights {
      least: Access::None,
      most: Access::None,
    };
    return (nothing, false);
  }
  Accumulated::of(picked(model, |acl| acl.anonymous), repository).resolve(repository)
}

/// An authenticated user's rights: from the rules that speak for the user when the file names the user, else from its
/// inverted entries; then with every authenticated user's.
fn user_rights(
  model: &Model,
  repository: &[u8],
  user: &[u8],
  membership: &Membership,
) -> (Rights, bool) {
  let principal = Principal::User(user);
  let own = if model.knows(user) {
    Some(
      Accumulated::of(
        picked(model, |acl| acl_access(acl, principal, membership)),
        repository,
      )
      .resolve(repository),
    )
  } else if model.acls().iter().any(|acl| acl.inverted.is_some()) {
    Some(Accumulated::of(picked(model, |acl| acl.inverted), repository).resolve(repository))
  } else {
    None
  };
  if !model.acls().iter().any(|acl| acl.authenticated.is_some()) {
    return own.unwrap_or((Rights::UNSET, false));
  }
  let (everyone, everyone_explicit) =
    Accumulated::of(picked(model, |acl| acl.authenticated), repository).resolve(repository);
  match own {
    Some((rights, explicit)) => (rights.combined(everyone), explicit || everyone_explicit),
    None => (everyone, everyone_explicit),
  }
}

#[cfg(test)]
mod read_rules_tests {
  use super::*;
  use crate::authz::may_read;

  fn lit(text: &str) -> PolicySegment {
    PolicySegment::Literal(text.as_bytes().into())
  }

  fn bitset_of(bytes: &[u8]) -> [u64; BYTE_SET_WORDS] {
    let mut bits = [0u64; BYTE_SET_WORDS];
    for &byte in bytes {
      bits[usize::from(byte >> BYTE_WORD_SHIFT)] |= 1u64 << u32::from(byte & BYTE_BIT_MASK);
    }
    bits
  }

  fn assert_read_cases(rules: &PrincipalRules, cases: &[(&str, bool)]) {
    for &(path, expected) in cases {
      assert_eq!(may_read(Some(rules), path), expected, "path {path}");
    }
  }

  fn assert_uniform_fast_paths() {
    let deny = PrincipalRules::deny_all();
    assert_eq!(deny.uniform(), Some(Access::None));
    assert_read_cases(&deny, &[("/", false), ("/trunk/a.txt", false)]);
    assert_eq!(deny.uniform_answers(), 2);

    let owner = PrincipalRules::from_read_rules(true, &[]);
    assert_eq!(owner.uniform(), Some(Access::Read));
    assert_read_cases(&owner, &[("/anything/at/all", true)]);
    assert_eq!(owner.uniform_answers(), 1);
  }

  fn build_precedence_and_glob_rules() -> PrincipalRules {
    let ab_glob = PolicySegment::Glob(Box::new([GlobToken::ByteSet(bitset_of(b"ab"))]));
    let a_star = PolicySegment::Glob(Box::new([GlobToken::Byte(b'a'), GlobToken::Run]));
    let broken = PolicySegment::Glob(Box::new([GlobToken::Byte(b'z'), GlobToken::Never]));
    PrincipalRules::from_read_rules(
      false,
      &[
        ReadPathRule {
          segments: vec![],
          allow: true,
        },
        ReadPathRule {
          segments: vec![lit("secret")],
          allow: false,
        },
        ReadPathRule {
          segments: vec![lit("secret"), lit("open")],
          allow: true,
        },
        ReadPathRule {
          segments: vec![lit("pub"), lit("secret")],
          allow: false,
        },
        ReadPathRule {
          segments: vec![lit("trunk"), a_star],
          allow: false,
        },
        ReadPathRule {
          segments: vec![lit("pick"), ab_glob],
          allow: false,
        },
        ReadPathRule {
          segments: vec![lit("x"), PolicySegment::AnyMany, lit("vault")],
          allow: false,
        },
        ReadPathRule {
          segments: vec![lit("bad"), broken],
          allow: false,
        },
      ],
    )
  }

  /// Round-4 Blocker 2 (`PrincipalRules::from_read_rules` unit check): Do build rules with root
  /// allow, subpath deny, nested allow, single-segment globs, `**`, and unmatchable `GlobToken::Never`,
  /// plus `deny_all` and uniform fast-path. Expect: deepest-match and last-defined-wins match
  /// Subversion semantics, uniform fast-path fires when all rules agree, and `GlobToken::Never`
  /// never matches or pollutes uniform access.
  #[test]
  fn from_read_rules_evaluates_precedence_globs_and_uniform_fast_path() {
    assert_uniform_fast_paths();
    let rules = build_precedence_and_glob_rules();
    assert_eq!(rules.uniform(), None);
    assert_read_cases(
      &rules,
      &[
        ("/", true),
        ("/pub/ok.txt", true),
        ("/secret", false),
        ("/secret/top.txt", false),
        ("/secret/open/readme.txt", true),
        ("/pub/secret/vault.txt", false),
        ("/trunk/alpha.txt", false),
        ("/trunk/zebra.txt", true),
        ("/pick/a/f.txt", false),
        ("/pick/c/f.txt", true),
        ("/x/deep/mid/vault/key", false),
        ("/bad/z", true),
      ],
    );
  }

  /// Round-5 Suggestion 2 (`Tree::trim` single-segment wildcard eclipsing): Do compile
  /// `[Allow Read /a, Deny Read :glob:/*]` and `[Allow Read /a, Deny Read :glob:/***]` plus
  /// non-eclipsed `[Allow Read /a/b, Deny Read :glob:/*]` and `[Allow Read :glob:/**, Deny Read :glob:/*]`.
  /// Expect: same-level `*` and `GlobToken::Run`-only globs eclipse earlier sibling child rules so
  /// `access_anywhere()` is `Access::None` and `uniform()` is `Some(Access::None)`, while deeper
  /// descendants (`/a/b`) and `**` rules are preserved.
  #[test]
  fn round5_suggestion2_single_star_trims_eclipsed_sibling_without_trimming_deeper_or_double_star()
  {
    let eclipsed_star = PrincipalRules::from_read_rules(
      false,
      &[
        ReadPathRule {
          segments: vec![lit("a")],
          allow: true,
        },
        ReadPathRule {
          segments: vec![PolicySegment::AnyOne],
          allow: false,
        },
      ],
    );
    assert_eq!(eclipsed_star.access_anywhere(), Access::None);
    assert_eq!(eclipsed_star.uniform(), Some(Access::None));

    let run_only = PolicySegment::Glob(Box::new([GlobToken::Run]));
    let eclipsed_run = PrincipalRules::from_read_rules(
      false,
      &[
        ReadPathRule {
          segments: vec![lit("a")],
          allow: true,
        },
        ReadPathRule {
          segments: vec![run_only],
          allow: false,
        },
      ],
    );
    assert_eq!(eclipsed_run.access_anywhere(), Access::None);
    assert_eq!(eclipsed_run.uniform(), Some(Access::None));

    let deeper_survives = PrincipalRules::from_read_rules(
      false,
      &[
        ReadPathRule {
          segments: vec![lit("a"), lit("b")],
          allow: true,
        },
        ReadPathRule {
          segments: vec![PolicySegment::AnyOne],
          allow: false,
        },
      ],
    );
    assert_eq!(deeper_survives.access_anywhere(), Access::Read);
    assert_read_cases(&deeper_survives, &[("/a", false), ("/a/b", true)]);

    let double_star_survives = PrincipalRules::from_read_rules(
      false,
      &[
        ReadPathRule {
          segments: vec![PolicySegment::AnyMany],
          allow: true,
        },
        ReadPathRule {
          segments: vec![PolicySegment::AnyOne],
          allow: false,
        },
      ],
    );
    assert_eq!(double_star_survives.access_anywhere(), Access::Read);
    assert_read_cases(
      &double_star_survives,
      &[("/", true), ("/a", false), ("/a/b", true)],
    );
  }
}
