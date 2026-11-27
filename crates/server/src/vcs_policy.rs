//! §4.17.1's policy by name pattern (shared piece 7, A-597): a repository's rules, each allowing or denying one right
//! to some principals on the names a pattern covers, and the one rule that decides a question, found by the
//! precedence the repository's own tool documents.
//!
//! **Names, patterns and precedence, in each tool's terms.**
//! - git: a ref name, covered as `git for-each-ref` covers one: "either using fnmatch(3) or literally, in the latter
//!   case matching completely or from the beginning up to a slash" [B: `for-each-ref-options.adoc` at git e1ec09b23],
//!   a pattern ending in `/` covering every ref beneath it and the glob being `wildmatch` with `WM_PATHNAME` [C:
//!   `ref-filter.c` `match_name_as_path`; [`slates_vcs::wildmatch`]]. The rule that decides is the last that covers the
//!   name: "If you have multiple hideRefs values, later entries override earlier ones" [B: `config/transfer.adoc`
//!   `transfer.hideRefs`; C: `refs.c` `ref_is_hidden` reads the entries from the last], the one precedence git
//!   documents among a server's ref patterns.
//! - Mercurial: a branch, bookmark or topic name, each kind its own namespace, covered as the `acl` extension covers
//!   one: "a branch name, or an asterisk, to match any branch" [C: `hgext/acl.py` in Mercurial 7.2.4, and the same for
//!   bookmarks]. The precedence is acl's order, "Deny list ... Allow list": a rule that denies decides first, and where
//!   any rule allows the right in the namespace, the rules that allow it are a list a principal must be on, since acl's
//!   `buildmatch` answers `util.never` to a user with no entry in a present allow section ("not allowed"). acl has no
//!   topic rules, and the topic extension's options read for §4.17.3 give none, so topics take bookmarks' form.
//! - Subversion: a repository path, covered as an authz section covers one: `/path` literally, `:glob:/path` with `*`
//!   for exactly one segment, `**` for any number of segments, and classic wildcards within a segment [B: Subversion
//!   1.10 release notes, "Improved path-based authorization"], a segment's wildcards read by [`slates_vcs::wildmatch`].
//!   The precedence is authz's lookup [C: `libsvn_repos/authz.c` at fd4e82b00]: it walks the path a segment at a time
//!   and at each depth takes the rule matching there "with the highest SEQUENCE_NUMBER ..., i.e. the latest one defined
//!   in the authz file", inheriting the depth above where none matches; so the deepest depth at which a rule matches
//!   decides, and of the rules matching there the one defined last (a `**` matches at every depth it spans, which is
//!   how a later `/trunk/**` eclipses an earlier `/trunk/a`, as `trim_tree` does). A rule that does not name the
//!   principal takes no part (`process_acl` skips an ACL with nothing for the user).
//!
//! **What a rule is.** A right ([`Right`]: read, submit, rewrite, delete; §4.17.1 "Policy"), an effect (allow or deny)
//! and the principals it speaks to ([`Who`]: everyone, or entries naming a principal or every principal but one, acl's
//! `!user` and authz's `~user`). One rule is one statement: where an authz file's section gives a user the union of
//! every line naming them, that file is read by Subversion's own authorization (§4.17.4 item 10), not here.
//!
//! **What a verdict is.** The deciding rule's position and effect, or [`Verdict::Unruled`] when no rule speaks to the
//! question, which leaves it to the repository's own rule: its owner holds every right (A-575) and another principal
//! what its capability grants (§4.13). Mercurial adds [`Verdict::NotListed`].
//!
//! **The shape.** Each policy is indexed once when it is made: git's rules by their pattern's bytes (a name can be
//! covered literally only by itself or a prefix ending at a slash) with its globs kept apart under the bytes each must
//! begin with; Mercurial's by namespace and name, with `*` apart; Subversion's literal rules by their canonical path,
//! one look-up a depth, with the rules holding wildcards matched by the positions each segment can reach. The tests'
//! oracles try every rule in order, as each tool's documentation states the rule, with git's own matcher transcribed
//! (`crate::wildmatch_reference`, slates-vcs's own file), and every generated history must get the same verdict
//! from both.
//!
//! **The push's check point (A-622 part 2).** [`admit`] is the one function every push path calls before it submits
//! its register entry (git's receive-pack, Subversion's commit editor, Mercurial's unbundle): the repository's policy,
//! as its record holds it (A-622 part 1), compiled for its tool, and each change the push asks for judged in order (the
//! name and the right its change needs, which the push's own tool decides: a create or a fast-forward submits, a move
//! to a head that does not descend from the current one rewrites, a removal deletes). The first change refused is the
//! push's typed refusal ([`PushRefusal`]), naming it by index and the rule that decided. A change no rule speaks to is
//! its repository's owner's alone, who holds every right a policy leaves unruled (A-575, §4.13).
//!
//! **A protected name's requirements (A-644; §4.17.2 "Protected branches, in git's terms").** Two rights are
//! requirements rather than permissions: bringing a name a merge commit ([`Right::MergeCommit`]) and bringing it a
//! commit no trusted signer signed ([`Right::UnsignedCommit`]). A rule that denies one imposes it (a linear history,
//! signed commits), a later rule that allows it lifts it for whom it names (git's precedence: the last covering rule),
//! and where no rule speaks nothing is required, so a policy that never names them asks nothing more of a push than it
//! did. A requirement is judged after the change's own right, in the design's order (a fast-forward or a rewrite, a
//! delete, then a linear history, then signatures), and only where a rule imposes it: the change's [`Brings`] is asked
//! then and not before, so a name no requirement covers costs no walk and no signature. The trusted signers are the
//! record's (`RepositoryPolicy::signers`), read as A-321's keys once a push. The requirements are git's alone (the
//! database's guard holds them to git), so their refusals name git's commits.

use std::collections::HashMap;

use slates_db::catalog::{Principal, RepositoryRecord};
use slates_git::object_name::ObjectId;
use slates_git::store::{StoreRefused, Unsigned};
use slates_vcs::wildmatch::{Glob, is_glob_special};

use crate::signed_work::{SignatureRefusal, TrustedKey};

/// A right a rule allows or denies (§4.17.1 "Policy": "who may read, who may submit, whether a rewrite ... or a delete
/// is allowed").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Right {
  /// Read the branch: fetch it, clone it, attach one of its versions.
  Read,
  /// Submit a work to it: a push that moves it on, a commit under the path.
  Submit,
  /// Move it to a head that does not descend from its current one: git's non-fast-forward, Mercurial's forced push.
  Rewrite,
  /// Delete it: its name retired from the namespace, its green kept (§4.17.1 "The repository and its greens" item 5).
  Delete,
  /// Bring it a merge commit, one of more than one parent (A-644): a requirement, judged only where a rule denies it.
  MergeCommit,
  /// Bring it a commit no trusted signer signed (A-644): a requirement, as `MergeCommit` is.
  UnsignedCommit,
}

/// Whether a rule allows its right or denies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
  /// The rule allows the right.
  Allow,
  /// The rule denies the right.
  Deny,
}

/// One entry of the principals a rule speaks to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
  /// This principal.
  Is(Principal),
  /// Every principal but this one (acl's `!user`, authz's `~user`).
  IsNot(Principal),
}

/// The principals a rule speaks to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Who {
  /// Every principal (acl's and authz's `*`).
  Anyone,
  /// Each principal one of these entries names: as acl reads a comma-separated list, a principal is named when any
  /// entry names it.
  AnyOf(Vec<Entry>),
}

impl Who {
  /// Whether `principal` is one this names.
  pub(crate) fn names(&self, principal: &Principal) -> bool {
    match self {
      Self::Anyone => true,
      Self::AnyOf(entries) => entries.iter().any(|entry| match entry {
        Entry::Is(named) => named == principal,
        Entry::IsNot(named) => named != principal,
      }),
    }
  }
}

/// One rule: on the names `pattern` covers, `effect` for `right` to the principals `who` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rule<P> {
  /// The names it covers, in the repository's tool's form.
  pub(crate) pattern: P,
  /// The right it decides.
  pub(crate) right: Right,
  /// Whether it allows or denies the right.
  pub(crate) effect: Effect,
  /// The principals it speaks to.
  pub(crate) who: Who,
}

impl<P> Rule<P> {
  /// Whether this rule speaks to `principal` about `right`.
  fn speaks(&self, principal: &Principal, right: Right) -> bool {
    self.right == right && self.who.names(principal)
  }

  /// The verdict this rule gives as the deciding one, at `position` in its policy.
  fn decides(&self, position: usize) -> Verdict {
    match self.effect {
      Effect::Allow => Verdict::Allowed { rule: position },
      Effect::Deny => Verdict::Denied { rule: position },
    }
  }
}

/// What a policy decides about one principal's right on one name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
  /// No rule speaks to it: the repository's own rule stands (its owner holds every right, A-575; another principal
  /// what its capability grants, §4.13).
  Unruled,
  /// The rule at this position in the policy allows it.
  Allowed {
    /// The deciding rule's position.
    rule: usize,
  },
  /// The rule at this position denies it.
  Denied {
    /// The deciding rule's position.
    rule: usize,
  },
  /// Mercurial: rules allow this right in the name's namespace and none that covers the name names this principal, so
  /// it is refused, as acl refuses a user "not allowed" on a branch.
  NotListed,
}

/// Why a pattern is refused where it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PatternRefused {
  /// It is empty: git's literal test reads the byte before a pattern's end, an empty Mercurial name is no name, and a
  /// Subversion path needs its `/`.
  Empty,
  /// It holds a NUL: no name does (the namespace refuses one, A-515), and git's own matcher would end the pattern there.
  HoldsNul,
  /// A Subversion path that does not begin with `/`, as every authz section's path does.
  NotAbsolute,
}

/// Refuses an empty pattern or one holding a NUL.
fn refuse_empty_or_nul(bytes: &[u8]) -> Result<(), PatternRefused> {
  if bytes.is_empty() {
    return Err(PatternRefused::Empty);
  }
  if bytes.contains(&0) {
    return Err(PatternRefused::HoldsNul);
  }
  Ok(())
}

/// Whether the rule at `position` of `rules` speaks to `principal` about `right`.
fn speaks<P>(rules: &[Rule<P>], position: usize, principal: &Principal, right: Right) -> bool {
  rules
    .get(position)
    .is_some_and(|rule| rule.speaks(principal, right))
}

/// The verdict of the deciding rule at `position`, or [`Verdict::Unruled`] when none decides.
fn decide<P>(rules: &[Rule<P>], position: Option<usize>) -> Verdict {
  position
    .and_then(|position| rules.get(position).map(|rule| rule.decides(position)))
    .unwrap_or(Verdict::Unruled)
}

/// A git ref pattern (the module doc): its bytes, tested literally against every name, and its glob when it holds one
/// of git's glob specials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefPattern {
  /// The pattern's bytes.
  text: Vec<u8>,
  /// The compiled glob, for a pattern holding a glob special.
  glob: Option<Glob>,
}

impl RefPattern {
  /// `text` as a ref pattern; refused when empty or holding a NUL.
  pub(crate) fn parse(text: &str) -> Result<Self, PatternRefused> {
    let bytes = text.as_bytes();
    refuse_empty_or_nul(bytes)?;
    let glob = bytes
      .iter()
      .any(|&byte| is_glob_special(byte))
      .then(|| Glob::new(bytes));
    Ok(Self {
      text: bytes.to_vec(),
      glob,
    })
  }

  /// Whether this covers ref `name`: literally (the whole name, or up to a slash, or beneath a pattern ending in one),
  /// or by its glob.
  pub(crate) fn covers(&self, name: &[u8]) -> bool {
    covers_literally(&self.text, name) || self.glob.as_ref().is_some_and(|glob| glob.matches(name))
  }

  /// The bytes before the first glob special: every name the glob matches begins with them.
  fn lead(&self) -> &[u8] {
    let end = self
      .text
      .iter()
      .position(|&byte| is_glob_special(byte))
      .unwrap_or(self.text.len());
    self.text.get(..end).unwrap_or_default()
  }
}

/// `match_name_as_path`'s literal test: `name` begins with `pattern`, and ends there, or goes on with a `/`, or the
/// pattern itself ends with one.
fn covers_literally(pattern: &[u8], name: &[u8]) -> bool {
  name.strip_prefix(pattern).is_some_and(|rest| {
    rest.is_empty() || rest.first() == Some(&b'/') || pattern.last() == Some(&b'/')
  })
}

/// The prefixes of `name` a pattern must equal to cover it literally: the whole name, and at each `/`, the bytes
/// before it and the bytes through it.
fn literal_keys(name: &[u8]) -> impl Iterator<Item = &[u8]> {
  let cuts = name
    .iter()
    .enumerate()
    .filter(|&(_, &byte)| byte == b'/')
    .flat_map(|(at, _)| [Some(at), at.checked_add(1)])
    .flatten();
  std::iter::once(name.len())
    .chain(cuts)
    .filter_map(move |end| name.get(..end))
}

/// A git repository's rules, indexed (the module doc).
#[derive(Clone, Debug)]
pub(crate) struct GitPolicy {
  /// The rules, in the order they were given.
  rules: Vec<Rule<RefPattern>>,
  /// Each rule's position under its pattern's bytes, for the literal test.
  literal: HashMap<Vec<u8>, Vec<usize>>,
  /// The positions of the rules holding a glob, each with the bytes a name must begin with to match it.
  globs: Vec<(Vec<u8>, usize)>,
}

impl GitPolicy {
  /// `rules`, indexed.
  pub(crate) fn new(rules: Vec<Rule<RefPattern>>) -> Self {
    let mut literal: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut globs = Vec::new();
    for (position, rule) in rules.iter().enumerate() {
      literal
        .entry(rule.pattern.text.clone())
        .or_default()
        .push(position);
      if rule.pattern.glob.is_some() {
        globs.push((rule.pattern.lead().to_vec(), position));
      }
    }
    Self {
      rules,
      literal,
      globs,
    }
  }

  /// The verdict on `principal`'s `right` on ref `name`: the last rule that covers the name and speaks to them.
  pub(crate) fn verdict(&self, name: &[u8], principal: &Principal, right: Right) -> Verdict {
    let last = self
      .literal_candidates(name)
      .chain(self.glob_candidates(name))
      .filter(|&position| speaks(&self.rules, position, principal, right))
      .max();
    decide(&self.rules, last)
  }

  /// The rules covering `name` literally.
  fn literal_candidates<'a>(&'a self, name: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    literal_keys(name)
      .filter_map(|key| self.literal.get(key))
      .flatten()
      .copied()
  }

  /// The rules whose glob matches `name`.
  fn glob_candidates<'a>(&'a self, name: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    self
      .globs
      .iter()
      .filter(move |(lead, _)| name.starts_with(lead))
      .map(|&(_, position)| position)
      .filter(move |&position| {
        self
          .rules
          .get(position)
          .and_then(|rule| rule.pattern.glob.as_ref())
          .is_some_and(|glob| glob.matches(name))
      })
  }
}

/// The Mercurial namespace a name belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Namespace {
  /// Named branches (acl's `acl.deny.branches` and `acl.allow.branches`).
  Branch,
  /// Bookmarks (`acl.deny.bookmarks` and `acl.allow.bookmarks`).
  Bookmark,
  /// Topics (hg-evolve's), in bookmarks' form (the module doc).
  Topic,
}

/// A Mercurial name pattern: one name, or `*` for every name of its namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HgPattern {
  /// `*`: every name of the namespace.
  Any(Namespace),
  /// One name.
  Name(Namespace, String),
}

impl HgPattern {
  /// `text` as a pattern of `namespace` (`*` for every name); refused when empty or holding a NUL.
  pub(crate) fn parse(namespace: Namespace, text: &str) -> Result<Self, PatternRefused> {
    refuse_empty_or_nul(text.as_bytes())?;
    Ok(if text == "*" {
      Self::Any(namespace)
    } else {
      Self::Name(namespace, text.to_owned())
    })
  }

  /// The namespace it covers names of.
  fn namespace(&self) -> Namespace {
    match self {
      Self::Any(namespace) | Self::Name(namespace, _) => *namespace,
    }
  }
}

/// A Mercurial repository's rules, indexed (the module doc).
#[derive(Clone, Debug)]
pub(crate) struct HgPolicy {
  /// The rules, in the order they were given.
  rules: Vec<Rule<HgPattern>>,
  /// The positions of the rules naming one name, by namespace and name.
  named: HashMap<Namespace, HashMap<String, Vec<usize>>>,
  /// The positions of the `*` rules, by namespace.
  any: HashMap<Namespace, Vec<usize>>,
  /// The namespaces and rights some rule allows: there, the rules that allow are a list.
  listed: Vec<(Namespace, Right)>,
}

impl HgPolicy {
  /// `rules`, indexed.
  pub(crate) fn new(rules: Vec<Rule<HgPattern>>) -> Self {
    let mut named: HashMap<Namespace, HashMap<String, Vec<usize>>> = HashMap::new();
    let mut any: HashMap<Namespace, Vec<usize>> = HashMap::new();
    let mut listed = Vec::new();
    for (position, rule) in rules.iter().enumerate() {
      match &rule.pattern {
        HgPattern::Any(namespace) => any.entry(*namespace).or_default().push(position),
        HgPattern::Name(namespace, name) => named
          .entry(*namespace)
          .or_default()
          .entry(name.clone())
          .or_default()
          .push(position),
      }
      let key = (rule.pattern.namespace(), rule.right);
      if rule.effect == Effect::Allow && !listed.contains(&key) {
        listed.push(key);
      }
    }
    Self {
      rules,
      named,
      any,
      listed,
    }
  }

  /// The verdict on `principal`'s `right` on `name` of `namespace`, in acl's order: the first rule that denies it,
  /// then, where rules allow the right in the namespace, the first that allows it, or [`Verdict::NotListed`].
  pub(crate) fn verdict(
    &self,
    namespace: Namespace,
    name: &str,
    principal: &Principal,
    right: Right,
  ) -> Verdict {
    let speaking = self.speaking(namespace, name, principal, right);
    let first = |effect: Effect| {
      speaking.iter().copied().find(|&position| {
        self
          .rules
          .get(position)
          .is_some_and(|rule| rule.effect == effect)
      })
    };
    if let Some(rule) = first(Effect::Deny) {
      return Verdict::Denied { rule };
    }
    if !self.listed.contains(&(namespace, right)) {
      return Verdict::Unruled;
    }
    first(Effect::Allow).map_or(Verdict::NotListed, |rule| Verdict::Allowed { rule })
  }

  /// The positions, in order, of the rules covering `name` in `namespace` that speak to `principal` about `right`.
  fn speaking(
    &self,
    namespace: Namespace,
    name: &str,
    principal: &Principal,
    right: Right,
  ) -> Vec<usize> {
    let named = self
      .named
      .get(&namespace)
      .and_then(|names| names.get(name))
      .into_iter()
      .flatten();
    let any = self.any.get(&namespace).into_iter().flatten();
    let mut positions: Vec<usize> = named
      .chain(any)
      .copied()
      .filter(|&position| speaks(&self.rules, position, principal, right))
      .collect();
    positions.sort_unstable();
    positions
  }
}

/// Format: the prefix authz gives a section whose path may hold wildcards (`[:glob:/path]`, the 1.10 release notes).
const GLOB_FORM: &str = ":glob:";

/// Format: POSIX character-class open delimiter inside a glob bracket expression (`[:class:]`).
const POSIX_CLASS_OPEN: &[u8] = b"[:";

/// Format: an escaped byte in a glob pattern (`\x`) spans two raw bytes.
const ESCAPED_BYTE_LEN: usize = b"\\x".len();

/// Format: `log2(64)` bits select the bit offset within one `u64` word of a 256-bit byte set.
const BYTE_WORD_SHIFT: u8 = 6;

/// Format: low six bits (`0..64`) index the bit within one `u64` word of a 256-bit byte set.
const BYTE_BIT_MASK: u8 = 63;

/// Format: diverse printable ASCII byte families (no `/`, `.`, or NUL) used to synthesize distinct
/// witness segments avoiding literal and glob `Deny` rules.
const PROBE_SEEDS: &[u8] = b"za0_~Am9-Z!";

/// Derived: per-slot witness probe cap equals the number of distinct byte families in [`PROBE_SEEDS`].
const MAX_SLOT_CANDIDATES: usize = PROBE_SEEDS.len();

/// Derived: bounded Cartesian pair-variation probe cap (`PROBE_SEEDS.len() * PROBE_SEEDS.len()`).
const MAX_PAIR_PROBES: usize = PROBE_SEEDS.len() * PROBE_SEEDS.len();

/// One segment of a Subversion path pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
  /// Exactly these bytes.
  Literal(Vec<u8>),
  /// Wildcards within one segment (`*foo*.bar`, `a?c`, `[ab]`, `\*`), retaining the raw segment
  /// bytes so concrete witness paths can be synthesized for effective-policy checks.
  Glob {
    /// Raw segment pattern bytes.
    raw: Vec<u8>,
    /// Compiled wildmatch matcher.
    glob: Glob,
  },
  /// `*` alone: any one segment.
  One,
  /// `**` alone: any number of segments, none included.
  Any,
}

impl Segment {
  /// `text` as a glob form's segment.
  fn of_glob(text: &str) -> Self {
    match text {
      "*" => Self::One,
      "**" => Self::Any,
      _ if text.bytes().any(is_glob_special) => Self::Glob {
        raw: text.as_bytes().to_vec(),
        glob: Glob::new(text.as_bytes()),
      },
      _ => Self::Literal(text.as_bytes().to_vec()),
    }
  }

  /// Whether this takes the path segment `segment` (never asked of [`Segment::Any`], which takes runs).
  fn takes(&self, segment: &[u8]) -> bool {
    match self {
      Self::Literal(bytes) => bytes == segment,
      Self::Glob { glob, .. } => glob.matches(segment),
      Self::One => !segment.is_empty(),
      Self::Any => true,
    }
  }

  /// The depths this reaches in `path` from the depths in `from`: one segment further where it takes the segment there,
  /// or for `**`, every depth at or past one reached.
  fn advance(&self, path: &[&[u8]], from: &[bool]) -> Vec<bool> {
    let mut to = vec![false; from.len()];
    if *self == Self::Any {
      let mut earlier = false;
      for (slot, &was) in to.iter_mut().zip(from) {
        earlier = earlier || was;
        *slot = earlier;
      }
      return to;
    }
    for (depth, (&was, segment)) in from.iter().zip(path).enumerate() {
      if was
        && self.takes(segment)
        && let Some(slot) = to.get_mut(depth.saturating_add(1))
      {
        *slot = true;
      }
    }
    to
  }
}

/// A Subversion path pattern (the module doc): `/path`, every segment literal, or `:glob:/path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PathPattern {
  /// The segments, the root's being none.
  segments: Vec<Segment>,
}

impl PathPattern {
  /// `text` as a path pattern: `:glob:` before the path for the glob form. Repeated slashes count as one and a
  /// trailing slash as none, as authz's lookup reads a path. Refused when empty, holding a NUL, or not beginning with
  /// `/`.
  pub(crate) fn parse(text: &str) -> Result<Self, PatternRefused> {
    refuse_empty_or_nul(text.as_bytes())?;
    let (glob, path) = match text.strip_prefix(GLOB_FORM) {
      Some(path) => (true, path),
      None => (false, text),
    };
    let rest = path.strip_prefix('/').ok_or(PatternRefused::NotAbsolute)?;
    let segments = rest
      .split('/')
      .filter(|segment| !segment.is_empty())
      .map(|segment| {
        if glob {
          Segment::of_glob(segment)
        } else {
          Segment::Literal(segment.as_bytes().to_vec())
        }
      })
      .collect();
    Ok(Self { segments })
  }

  /// The canonical path of a pattern with no wildcard (empty for the root, `/a/b` below it), which the index keys it
  /// by; none for a pattern holding one.
  fn literal_key(&self) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    for segment in &self.segments {
      let Segment::Literal(bytes) = segment else {
        return None;
      };
      key.push(b'/');
      key.extend_from_slice(bytes);
    }
    Some(key)
  }

  /// The deepest depth at which this matches the leading segments of `path` whole: the depths each segment of the
  /// pattern reaches, in turn, from the root.
  fn deepest(&self, path: &[&[u8]]) -> Option<usize> {
    let mut reached = vec![false; path.len().checked_add(1)?];
    *reached.first_mut()? = true;
    for segment in &self.segments {
      reached = segment.advance(path, &reached);
      if !reached.contains(&true) {
        return None;
      }
    }
    reached.iter().rposition(|&was| was)
  }
}

/// Advances past a POSIX character class `[:name:]` starting at `open`, matching
/// `slates_vcs::wildmatch::class`: closed by the first `]` after `[:` only when preceded by `:`.
fn skip_posix_class(raw: &[u8], open: usize) -> Option<usize> {
  let name_start = open.saturating_add(POSIX_CLASS_OPEN.len());
  if raw.get(open..name_start) != Some(POSIX_CLASS_OPEN) {
    return None;
  }
  let offset = raw
    .get(name_start..)?
    .iter()
    .position(|&byte| byte == b']')?;
  let close = name_start.saturating_add(offset);
  let colon = close
    .checked_sub(1)
    .filter(|&at| at >= name_start && raw.get(at) == Some(&b':'))?;
  let _ = colon;
  Some(close.saturating_add(1))
}

/// Finds the index immediately after the closing `]` of a glob bracket expression starting at
/// `open`, respecting leading `!`/`^`, a literal first `]`, `\` escapes, and `[:class:]`.
fn bracket_end(raw: &[u8], open: usize) -> Option<usize> {
  let mut at = open.saturating_add(1);
  if matches!(raw.get(at), Some(b'!' | b'^')) {
    at = at.saturating_add(1);
  }
  if raw.get(at) == Some(&b']') {
    at = at.saturating_add(1);
  }
  while let Some(&byte) = raw.get(at) {
    match byte {
      b'\\' => {
        raw.get(at.saturating_add(1))?;
        at = at.saturating_add(ESCAPED_BYTE_LEN);
      }
      b'[' => at = skip_posix_class(raw, at).unwrap_or_else(|| at.saturating_add(1)),
      b']' => return Some(at.saturating_add(1)),
      _ => at = at.saturating_add(1),
    }
  }
  None
}

/// Compiles one `[...]` bracket expression slice into a 256-bit byte set via [`Glob::new`], or
/// `None` when the bracket matches no byte (empty range or unknown POSIX class).
fn bracket_byte_set(slice: &[u8]) -> Option<[u64; slates_svn::authz::BYTE_SET_WORDS]> {
  let sub = Glob::new(slice);
  let mut bits = [0u64; slates_svn::authz::BYTE_SET_WORDS];
  let mut any = false;
  for byte in 0u8..=u8::MAX {
    if sub.matches(&[byte]) {
      let word = usize::from(byte >> BYTE_WORD_SHIFT);
      if let Some(slot) = bits.get_mut(word) {
        *slot |= 1u64 << u32::from(byte & BYTE_BIT_MASK);
        any = true;
      }
    }
  }
  any.then_some(bits)
}

/// Compiles the bracket expression starting at `at` into one [`slates_svn::authz::GlobToken`]
/// and the next index in `raw`.
fn bracket_token(raw: &[u8], at: usize) -> (slates_svn::authz::GlobToken, usize) {
  use slates_svn::authz::GlobToken;
  let Some(end) = bracket_end(raw, at) else {
    return (GlobToken::Never, raw.len());
  };
  let Some(slice) = raw.get(at..end) else {
    return (GlobToken::Never, raw.len());
  };
  let token = bracket_byte_set(slice).map_or(GlobToken::Never, GlobToken::ByteSet);
  (token, end)
}

/// Decodes one [`slates_svn::authz::GlobToken`] at `at` and returns `(token, next_at)`.
fn next_glob_token(raw: &[u8], at: usize) -> (slates_svn::authz::GlobToken, usize) {
  use slates_svn::authz::GlobToken;
  match raw.get(at).copied() {
    Some(b'\\') => {
      raw
        .get(at.saturating_add(1))
        .map_or((GlobToken::Never, raw.len()), |&escaped| {
          (
            GlobToken::Byte(escaped),
            at.saturating_add(ESCAPED_BYTE_LEN),
          )
        })
    }
    Some(b'*') => (GlobToken::Run, at.saturating_add(1)),
    Some(b'?') => (GlobToken::AnyByte, at.saturating_add(1)),
    Some(b'[') => bracket_token(raw, at),
    Some(other) => (GlobToken::Byte(other), at.saturating_add(1)),
    None => (GlobToken::Never, raw.len()),
  }
}

/// Compiles a single-segment glob pattern into [`slates_svn::authz::GlobToken`]s, shared by both
/// effective-policy witness synthesis and `PrincipalRules::from_read_rules`.
fn compile_glob_tokens(raw: &[u8]) -> Vec<slates_svn::authz::GlobToken> {
  use slates_svn::authz::GlobToken;
  let mut tokens = Vec::new();
  let mut at = 0usize;
  while at < raw.len() {
    let (token, next) = next_glob_token(raw, at);
    at = next;
    if token == GlobToken::Never {
      tokens.push(GlobToken::Never);
      break;
    }
    if !(token == GlobToken::Run && tokens.last() == Some(&GlobToken::Run)) {
      tokens.push(token);
    }
  }
  tokens
}

/// Converts a compiled [`Segment`] into a [`slates_svn::authz::PolicySegment`] for `PrincipalRules`.
fn policy_segment_of(segment: &Segment) -> slates_svn::authz::PolicySegment {
  use slates_svn::authz::PolicySegment;
  match segment {
    Segment::Literal(bytes) => PolicySegment::Literal(bytes.as_slice().into()),
    Segment::One => PolicySegment::AnyOne,
    Segment::Any => PolicySegment::AnyMany,
    Segment::Glob { raw, .. } => PolicySegment::Glob(compile_glob_tokens(raw).into_boxed_slice()),
  }
}

/// Pushes `candidate` into `list` when not already present and `list.len() < cap`.
fn push_unique_bytes(list: &mut Vec<Vec<u8>>, candidate: Vec<u8>, cap: usize) {
  if list.len() < cap && !list.iter().any(|held| held == &candidate) {
    list.push(candidate);
  }
}

/// Whether `candidate` matches any `Literal` or `Glob` segment appearing in `deny_rules`.
fn segment_matches_deny(candidate: &[u8], deny_rules: &[&Rule<PathPattern>]) -> bool {
  deny_rules.iter().any(|rule| {
    rule.pattern.segments.iter().any(|segment| match segment {
      Segment::Literal(bytes) => bytes.as_slice() == candidate,
      Segment::Glob { glob, .. } => glob.matches(candidate),
      Segment::One | Segment::Any => false,
    })
  })
}

/// Longest literal segment length across `rules`.
fn max_literal_len(rules: &[&Rule<PathPattern>]) -> usize {
  rules
    .iter()
    .flat_map(|rule| rule.pattern.segments.iter())
    .filter_map(|segment| match segment {
      Segment::Literal(bytes) => Some(bytes.len()),
      _ => None,
    })
    .max()
    .unwrap_or(0)
}

/// Builds a bounded pool of non-empty ASCII witness segments prioritizing candidates that match no
/// `Literal` or `Glob` segment in `deny_rules`, followed by diverse family fallbacks and literal
/// segments from `allow_rules` (to uncover cross-rule wildcard intersections).
fn fresh_segments(
  deny_rules: &[&Rule<PathPattern>],
  allow_rules: &[&Rule<PathPattern>],
) -> Vec<Vec<u8>> {
  let cap = MAX_SLOT_CANDIDATES.saturating_add(MAX_SLOT_CANDIDATES);
  let long_len = max_literal_len(deny_rules).saturating_add(1);
  let mut pool = Vec::new();
  for &seed in PROBE_SEEDS {
    let single = vec![seed];
    if !segment_matches_deny(&single, deny_rules) {
      push_unique_bytes(&mut pool, single, cap);
    }
  }
  for &seed in PROBE_SEEDS {
    let repeated = vec![seed; long_len];
    if !segment_matches_deny(&repeated, deny_rules) {
      push_unique_bytes(&mut pool, repeated, cap);
    }
  }
  for &seed in PROBE_SEEDS {
    push_unique_bytes(&mut pool, vec![seed], cap);
  }
  push_unique_bytes(&mut pool, vec![b'z'; long_len], cap);
  append_allow_literals(&mut pool, allow_rules, cap);
  pool
}

/// Appends distinct literal segments from `allow_rules` up to `cap` so overlapping wildcard rules
/// across rights (such as `:glob:/a/*` and `:glob:/*/b`) share concrete probes.
fn append_allow_literals(pool: &mut Vec<Vec<u8>>, allow_rules: &[&Rule<PathPattern>], cap: usize) {
  for rule in allow_rules {
    for segment in &rule.pattern.segments {
      if let Segment::Literal(bytes) = segment
        && !bytes.is_empty()
      {
        push_unique_bytes(pool, bytes.clone(), cap);
      }
    }
  }
}

/// Candidate non-slash, non-NUL bytes matching one variable byte token (`AnyByte` or `ByteSet`),
/// capped at [`MAX_SLOT_CANDIDATES`].
fn slot_bytes(token: slates_svn::authz::GlobToken) -> Vec<u8> {
  use slates_svn::authz::{GlobToken, byte_set_contains};
  let bits = match token {
    GlobToken::AnyByte => return PROBE_SEEDS.to_vec(),
    GlobToken::ByteSet(bits) => bits,
    GlobToken::Byte(_) | GlobToken::Run | GlobToken::Never => return Vec::new(),
  };
  let mut chosen = Vec::new();
  let preferred = PROBE_SEEDS
    .iter()
    .copied()
    .chain(b'a'..=b'z')
    .chain(b'A'..=b'Z')
    .chain(b'0'..=b'9')
    .chain(1u8..=u8::MAX);
  for byte in preferred {
    if chosen.len() >= MAX_SLOT_CANDIDATES {
      break;
    }
    if byte != b'/' && byte_set_contains(&bits, byte) && !chosen.contains(&byte) {
      chosen.push(byte);
    }
  }
  chosen
}

/// Instantiates `tokens` into one concrete segment using `slot_picks` for `AnyByte`/`ByteSet`
/// tokens and `star_fill` for `Run` tokens.
fn instantiate_glob(
  tokens: &[slates_svn::authz::GlobToken],
  slot_picks: &[u8],
  star_fill: &[u8],
) -> Vec<u8> {
  use slates_svn::authz::GlobToken;
  let mut out = Vec::new();
  let mut slot = 0usize;
  for &token in tokens {
    match token {
      GlobToken::Byte(byte) => out.push(byte),
      GlobToken::AnyByte | GlobToken::ByteSet(_) => {
        if let Some(&byte) = slot_picks.get(slot) {
          out.push(byte);
        }
        slot = slot.saturating_add(1);
      }
      GlobToken::Run => out.extend_from_slice(star_fill),
      GlobToken::Never => {}
    }
  }
  out
}

/// Collects valid, deduplicated segment candidates for `glob`, prioritizing those matching no deny
/// rule segment.
fn glob_segment_candidates(
  raw: &[u8],
  glob: &Glob,
  fresh_pool: &[Vec<u8>],
  deny_rules: &[&Rule<PathPattern>],
) -> Vec<Vec<u8>> {
  use slates_svn::authz::GlobToken;
  let tokens = compile_glob_tokens(raw);
  if tokens.contains(&GlobToken::Never) {
    return Vec::new();
  }
  let mut slots: Vec<Vec<u8>> = Vec::new();
  for &token in &tokens {
    if matches!(token, GlobToken::AnyByte | GlobToken::ByteSet(_)) {
      let bytes = slot_bytes(token);
      if bytes.is_empty() {
        return Vec::new();
      }
      slots.push(bytes);
    }
  }
  let cap = MAX_SLOT_CANDIDATES.saturating_add(MAX_SLOT_CANDIDATES);
  let mut raw_candidates = Vec::new();
  collect_glob_variants(&tokens, &slots, fresh_pool, glob, cap, &mut raw_candidates);
  prioritize_non_colliding(raw_candidates, deny_rules)
}

/// Pushes a candidate segment if non-empty, valid, and matched by `glob`.
fn record_glob_candidate(candidate: Vec<u8>, glob: &Glob, cap: usize, out: &mut Vec<Vec<u8>>) {
  if !candidate.is_empty()
    && !candidate.contains(&b'/')
    && !candidate.contains(&0)
    && glob.matches(&candidate)
  {
    push_unique_bytes(out, candidate, cap);
  }
}

/// Generates per-slot byte variants for `slot_idx` against `base_picks` and `fills`.
fn collect_slot_variants(
  tokens: &[slates_svn::authz::GlobToken],
  base_picks: &[u8],
  slot_idx: usize,
  choices: &[u8],
  alt_fill: Option<&[u8]>,
  glob: &Glob,
  out: &mut Vec<Vec<u8>>,
) {
  let cap = MAX_SLOT_CANDIDATES.saturating_add(MAX_SLOT_CANDIDATES);
  for &byte in choices.iter().skip(1) {
    let mut picks = base_picks.to_vec();
    if let Some(target) = picks.get_mut(slot_idx) {
      *target = byte;
    }
    record_glob_candidate(instantiate_glob(tokens, &picks, b""), glob, cap, out);
    if let Some(fill) = alt_fill {
      record_glob_candidate(instantiate_glob(tokens, &picks, fill), glob, cap, out);
    }
  }
}

/// Generates base star-fill variants and per-slot byte variants for compiled `tokens`.
fn collect_glob_variants(
  tokens: &[slates_svn::authz::GlobToken],
  slots: &[Vec<u8>],
  fresh_pool: &[Vec<u8>],
  glob: &Glob,
  cap: usize,
  out: &mut Vec<Vec<u8>>,
) {
  let has_run = tokens.contains(&slates_svn::authz::GlobToken::Run);
  let base_picks: Vec<u8> = slots
    .iter()
    .filter_map(|choices| choices.first().copied())
    .collect();
  let mut fills: Vec<&[u8]> = vec![b""];
  if has_run {
    fills.extend(
      fresh_pool
        .iter()
        .take(MAX_SLOT_CANDIDATES)
        .map(Vec::as_slice),
    );
  }
  for fill in &fills {
    let built = instantiate_glob(tokens, &base_picks, fill);
    record_glob_candidate(built, glob, cap, out);
  }
  let alt_fill = has_run.then(|| fills.get(1).copied().unwrap_or(b""));
  for (slot_idx, choices) in slots.iter().enumerate() {
    collect_slot_variants(tokens, &base_picks, slot_idx, choices, alt_fill, glob, out);
  }
}

/// Orders candidates that match no `Literal`/`Glob` segment in `deny_rules` ahead of colliding ones.
fn prioritize_non_colliding(
  candidates: Vec<Vec<u8>>,
  deny_rules: &[&Rule<PathPattern>],
) -> Vec<Vec<u8>> {
  let (mut clean, mut colliding) = (Vec::new(), Vec::new());
  for candidate in candidates {
    if segment_matches_deny(&candidate, deny_rules) {
      colliding.push(candidate);
    } else {
      clean.push(candidate);
    }
  }
  clean.extend(colliding);
  clean
}

/// Builds bounded `**` repetition schedules covering all-0, all-1, all-`deep_expand`, and
/// single-slot variations across every `**` segment in a pattern.
fn any_expansions(any_count: usize, deep_expand: usize) -> Vec<Vec<usize>> {
  if any_count == 0 {
    return vec![Vec::new()];
  }
  let mut schedules = Vec::new();
  let mut push = |schedule: Vec<usize>| {
    if !schedules.contains(&schedule) {
      schedules.push(schedule);
    }
  };
  push(vec![0; any_count]);
  push(vec![1; any_count]);
  push(vec![deep_expand; any_count]);
  for slot in 0..any_count {
    for &(base, varied) in &[(0, 1), (0, deep_expand), (1, 0), (1, deep_expand)] {
      let mut schedule = vec![base; any_count];
      if let Some(entry) = schedule.get_mut(slot) {
        *entry = varied;
      }
      push(schedule);
    }
  }
  schedules
}

/// Assembles a concrete canonical repository path from `pattern`, per-segment choices `chosen`,
/// `**` repeat counts `any_counts`, and filler `fresh_fill`.
fn assemble_witness(
  segments: &[Segment],
  chosen: &[Option<&[u8]>],
  any_counts: &[usize],
  fresh_fill: &[u8],
) -> Vec<u8> {
  let mut path = Vec::new();
  let mut any_idx = 0usize;
  for (seg_idx, segment) in segments.iter().enumerate() {
    if *segment == Segment::Any {
      let repeats = any_counts.get(any_idx).copied().unwrap_or(0);
      any_idx = any_idx.saturating_add(1);
      for _ in 0..repeats {
        path.push(b'/');
        path.extend_from_slice(fresh_fill);
      }
      continue;
    }
    if let Some(Some(bytes)) = chosen.get(seg_idx) {
      path.push(b'/');
      path.extend_from_slice(bytes);
    }
  }
  if path.is_empty() {
    path.push(b'/');
  }
  path
}

/// Computes per-segment concrete candidate lists for `pattern`, returning `None` if any non-`**`
/// segment cannot match any concrete byte sequence (such as an unclosed `[`).
fn pattern_segment_candidates(
  pattern: &PathPattern,
  fresh_pool: &[Vec<u8>],
  deny_rules: &[&Rule<PathPattern>],
) -> Option<Vec<Vec<Vec<u8>>>> {
  let mut lists = Vec::with_capacity(pattern.segments.len());
  for segment in &pattern.segments {
    let candidates = match segment {
      Segment::Literal(bytes) => vec![bytes.clone()],
      Segment::One => fresh_pool.to_vec(),
      Segment::Any => Vec::new(),
      Segment::Glob { raw, glob } => {
        let built = glob_segment_candidates(raw, glob, fresh_pool, deny_rules);
        if built.is_empty() {
          return None;
        }
        built
      }
    };
    lists.push(candidates);
  }
  Some(lists)
}

/// Tests concrete witnesses generated from `pattern` against `accepts`: Pass 1 tests the best
/// candidate per segment across all `**` schedules and `fresh_pool` fills; Pass 2 varies each
/// individual variable segment; Pass 3 varies pairs of multi-candidate segments up to
/// [`MAX_PAIR_PROBES`].
fn try_pattern_witnesses(
  pattern: &PathPattern,
  fresh_pool: &[Vec<u8>],
  deny_rules: &[&Rule<PathPattern>],
  deep_expand: usize,
  accepts: &impl Fn(&[u8]) -> bool,
) -> bool {
  let Some(lists) = pattern_segment_candidates(pattern, fresh_pool, deny_rules) else {
    return false;
  };
  let any_count = pattern
    .segments
    .iter()
    .filter(|segment| **segment == Segment::Any)
    .count();
  let schedules = any_expansions(any_count, deep_expand);
  let base: Vec<Option<&[u8]>> = lists
    .iter()
    .map(|items| items.first().map(Vec::as_slice))
    .collect();
  if try_default_witnesses(&pattern.segments, &base, &schedules, fresh_pool, accepts) {
    return true;
  }
  let fill = fresh_pool.first().map_or(b"z".as_slice(), Vec::as_slice);
  if try_single_segment_variants(&pattern.segments, &lists, &base, &schedules, fill, accepts) {
    return true;
  }
  try_pair_segment_variants(&pattern.segments, &lists, &base, &schedules, fill, accepts)
}

/// Pass 1: checks `base` segment choices across every `**` schedule and `fresh_pool` filler.
fn try_default_witnesses(
  segments: &[Segment],
  base: &[Option<&[u8]>],
  schedules: &[Vec<usize>],
  fresh_pool: &[Vec<u8>],
  accepts: &impl Fn(&[u8]) -> bool,
) -> bool {
  let fillers: &[Vec<u8>] = if schedules.iter().any(|s| s.iter().any(|&n| n > 0)) {
    fresh_pool
  } else {
    fresh_pool.get(..1).unwrap_or(fresh_pool)
  };
  for schedule in schedules {
    for fill in fillers {
      let path = assemble_witness(segments, base, schedule, fill);
      if accepts(&path) {
        return true;
      }
    }
  }
  false
}

/// Whether `chosen` satisfies `accepts` under any `**` repetition schedule in `schedules`.
fn matches_any_schedule(
  segments: &[Segment],
  chosen: &[Option<&[u8]>],
  schedules: &[Vec<usize>],
  fill: &[u8],
  accepts: &impl Fn(&[u8]) -> bool,
) -> bool {
  schedules
    .iter()
    .any(|schedule| accepts(&assemble_witness(segments, chosen, schedule, fill)))
}

/// Pass 2: varies each individual segment with >1 candidate across every `**` schedule.
fn try_single_segment_variants(
  segments: &[Segment],
  lists: &[Vec<Vec<u8>>],
  base: &[Option<&[u8]>],
  schedules: &[Vec<usize>],
  fill: &[u8],
  accepts: &impl Fn(&[u8]) -> bool,
) -> bool {
  for (seg_idx, choices) in lists.iter().enumerate() {
    for alt in choices.iter().skip(1) {
      let mut chosen = base.to_vec();
      if let Some(slot) = chosen.get_mut(seg_idx) {
        *slot = Some(alt.as_slice());
      }
      if matches_any_schedule(segments, &chosen, schedules, fill, accepts) {
        return true;
      }
    }
  }
  false
}

/// Clones `base` with two segment slots replaced by `a` and `b`.
fn with_pair_choices<'a>(
  base: &[Option<&'a [u8]>],
  first: (usize, &'a [u8]),
  second: (usize, &'a [u8]),
) -> Vec<Option<&'a [u8]>> {
  let mut chosen = base.to_vec();
  if let Some(slot) = chosen.get_mut(first.0) {
    *slot = Some(first.1);
  }
  if let Some(slot) = chosen.get_mut(second.0) {
    *slot = Some(second.1);
  }
  chosen
}

/// Pass 3: varies pairs of the first two multi-candidate segments up to [`MAX_PAIR_PROBES`].
fn try_pair_segment_variants(
  segments: &[Segment],
  lists: &[Vec<Vec<u8>>],
  base: &[Option<&[u8]>],
  schedules: &[Vec<usize>],
  fill: &[u8],
  accepts: &impl Fn(&[u8]) -> bool,
) -> bool {
  let mut multi = lists
    .iter()
    .enumerate()
    .filter_map(|(idx, choices)| (choices.len() > 1).then_some(idx));
  let (Some(first), Some(second)) = (multi.next(), multi.next()) else {
    return false;
  };
  let (Some(first_list), Some(second_list)) = (lists.get(first), lists.get(second)) else {
    return false;
  };
  let schedule = schedules.first().map_or(&[][..], Vec::as_slice);
  let pairs = first_list
    .iter()
    .flat_map(|a| {
      second_list
        .iter()
        .map(move |b| (a.as_slice(), b.as_slice()))
    })
    .take(MAX_PAIR_PROBES);
  for (a, b) in pairs {
    let chosen = with_pair_choices(base, (first, a), (second, b));
    if accepts(&assemble_witness(segments, &chosen, schedule, fill)) {
      return true;
    }
  }
  false
}

/// A repository path's segments, as authz's lookup reads them: repeated slashes as one, leading and trailing ones as
/// none.
fn path_segments(path: &[u8]) -> Vec<&[u8]> {
  path
    .split(|&byte| byte == b'/')
    .filter(|segment| !segment.is_empty())
    .collect()
}

/// `segments` joined as a canonical path, a `/` before each, and where each depth's prefix of it ends (the root's, at
/// depth 0, is empty).
fn canonical(segments: &[&[u8]]) -> (Vec<u8>, Vec<usize>) {
  let mut joined = Vec::new();
  let mut ends = vec![0];
  for segment in segments {
    joined.push(b'/');
    joined.extend_from_slice(segment);
    ends.push(joined.len());
  }
  (joined, ends)
}

/// A Subversion repository's rules, indexed (the module doc).
#[derive(Clone, Debug)]
pub(crate) struct SvnPolicy {
  /// The rules, in the order they were given.
  rules: Vec<Rule<PathPattern>>,
  /// The positions of the rules with no wildcard, by canonical path.
  literal: HashMap<Vec<u8>, Vec<usize>>,
  /// The positions of the rules holding a wildcard.
  globs: Vec<usize>,
}

impl SvnPolicy {
  /// `rules`, indexed.
  pub(crate) fn new(rules: Vec<Rule<PathPattern>>) -> Self {
    let mut literal: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut globs = Vec::new();
    for (position, rule) in rules.iter().enumerate() {
      match rule.pattern.literal_key() {
        Some(key) => literal.entry(key).or_default().push(position),
        None => globs.push(position),
      }
    }
    Self {
      rules,
      literal,
      globs,
    }
  }

  /// The verdict on `principal`'s `right` at `path`: of the rules speaking to them, the one matching at the deepest
  /// depth, and of those the one defined last.
  pub(crate) fn verdict(&self, path: &[u8], principal: &Principal, right: Right) -> Verdict {
    let segments = path_segments(path);
    let (joined, ends) = canonical(&segments);
    let literal = ends.iter().enumerate().flat_map(|(depth, &end)| {
      joined
        .get(..end)
        .and_then(|key| self.literal.get(key))
        .into_iter()
        .flatten()
        .map(move |&position| (depth, position))
    });
    let globs = self.globs.iter().filter_map(|&position| {
      let rule = self.rules.get(position)?;
      Some((rule.pattern.deepest(&segments)?, position))
    });
    let best = literal
      .chain(globs)
      .filter(|&(_, position)| speaks(&self.rules, position, principal, right))
      .max();
    decide(&self.rules, best.map(|(_, position)| position))
  }

  /// Whether at least one concrete repository path satisfies `accepts`, searching `/` and
  /// synthesized witnesses from every `Allow` rule matching any right in `rights` for `principal`.
  fn exists_witness(
    &self,
    principal: &Principal,
    rights: &[Right],
    accepts: impl Fn(&[u8]) -> bool,
  ) -> bool {
    if accepts(b"/") {
      return true;
    }
    let speaks_any =
      |rule: &&Rule<PathPattern>| rights.iter().any(|&right| rule.speaks(principal, right));
    let deny_rules: Vec<&Rule<PathPattern>> = self
      .rules
      .iter()
      .filter(|rule| rule.effect == Effect::Deny && speaks_any(rule))
      .collect();
    let allow_rules: Vec<&Rule<PathPattern>> = self
      .rules
      .iter()
      .filter(|rule| rule.effect == Effect::Allow && speaks_any(rule))
      .collect();
    let fresh_pool = fresh_segments(&deny_rules, &allow_rules);
    let deep_expand = self
      .rules
      .iter()
      .map(|rule| rule.pattern.segments.len())
      .max()
      .unwrap_or(0)
      .saturating_add(1);
    allow_rules.iter().any(|rule| {
      try_pattern_witnesses(
        &rule.pattern,
        &fresh_pool,
        &deny_rules,
        deep_expand,
        &accepts,
      )
    })
  }
}

/// One name a push changes, the right its change needs, as the push's own tool classified it (A-622 part 2), and what
/// it brings, which a requirement judges (A-644).
#[derive(Clone, Copy)]
pub(crate) struct Asked<'a> {
  /// The name.
  pub(crate) name: Name<'a>,
  /// The right the change needs.
  pub(crate) right: Right,
  /// What the change brings to the name, asked only where a rule imposes a requirement; none for a change that brings
  /// nothing a requirement reads (a delete, or a tool that has no requirements).
  pub(crate) brings: Option<&'a dyn Brings>,
}

/// What a change brings to its name, which a requirement judges (A-644): A-614's store's answers on git's push path
/// (`crate::git_protect::StoreBrings`). Each is asked at most once a change, and only where a rule imposes its
/// requirement.
pub(crate) trait Brings {
  /// The first merge commit the change brings.
  fn merge_commit(&self) -> Result<Option<ObjectId>, StoreRefused>;

  /// The first commit the change brings that none of `signers` signed, and why.
  fn unsigned_commit(
    &self,
    signers: &[TrustedKey],
  ) -> Result<Option<(ObjectId, Unsigned<SignatureRefusal>)>, StoreRefused>;
}

/// A name as each tool's push names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Name<'a> {
  /// A git ref's full name.
  Ref(&'a [u8]),
  /// A Mercurial branch, bookmark or topic.
  Hg(Namespace, &'a str),
  /// A Subversion repository path.
  Path(&'a [u8]),
}

/// Why a repository's policy refuses a push (A-622 part 2), naming the change it asked for by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PushRefusal {
  /// Rule `rule` of the policy denies the change its right.
  Denied {
    /// The change's index among those the push asked for.
    index: u32,
    /// The deciding rule's index among the policy's rules.
    rule: u32,
  },
  /// Mercurial: rules allow the right in the name's namespace and none names the principal on the name (acl's "not
  /// allowed").
  NotListed {
    /// The change's index.
    index: u32,
  },
  /// No rule speaks to the change, and the principal is not the repository's owner, who alone holds what a policy leaves
  /// unruled (A-575).
  NotPermitted {
    /// The change's index.
    index: u32,
  },
  /// The name is of a kind the repository's tool does not have (a Mercurial name pushed to a git repository).
  OtherTool {
    /// The change's index.
    index: u32,
  },
  /// The policy holds a rule its tool's matcher cannot read. The database's guard keeps such rules out of the log
  /// (A-622 part 1), so this is a damaged record, and the push is refused rather than judged without the rule.
  Unreadable {
    /// The rule's index among the policy's rules.
    rule: u32,
  },
  /// Rule `rule` keeps the name's history linear, and the change brings the merge commit `commit` (A-644).
  MergeCommit {
    /// The change's index.
    index: u32,
    /// The rule that imposes the requirement.
    rule: u32,
    /// The first merge commit the change brings.
    commit: ObjectId,
  },
  /// Rule `rule` requires signed commits, and the change brings `commit`, which no trusted signer signed (A-644).
  UnsignedCommit {
    /// The change's index.
    index: u32,
    /// The rule that imposes the requirement.
    rule: u32,
    /// The first such commit the change brings.
    commit: ObjectId,
    /// Why its signature is not a trusted signer's.
    why: Unsigned<SignatureRefusal>,
  },
  /// What the change brings could not be read to judge a requirement: its store refused, a store whose index and
  /// content disagree, and the push is refused rather than judged without it (A-644).
  Unjudged {
    /// The change's index.
    index: u32,
    /// The store's refusal.
    refused: StoreRefused,
  },
}

/// An index as a refusal names it.
fn index_of(index: usize) -> u32 {
  u32::try_from(index).unwrap_or(u32::MAX)
}

/// A repository's policy compiled for its tool's names (A-622 part 2).
#[derive(Clone, Debug)]
pub(crate) enum Compiled {
  /// A git repository's.
  Git(GitPolicy),
  /// A Mercurial repository's.
  Mercurial(HgPolicy),
  /// A Subversion repository's.
  Subversion(SvnPolicy),
}

impl Compiled {
  /// The rules of `record`'s policy compiled for its tool; `Unreadable` naming the first rule its tool's matcher cannot
  /// read.
  pub(crate) fn of(record: &RepositoryRecord) -> Result<Self, PushRefusal> {
    use slates_db::catalog::VcsTool;
    let rules = &record.policy.rules;
    Ok(match record.tool {
      VcsTool::Git => Self::Git(GitPolicy::new(compiled_rules(rules, |rule| {
        match rule.names {
          slates_db::catalog::PolicyNames::Refs => RefPattern::parse(&rule.pattern).ok(),
          _ => None,
        }
      })?)),
      VcsTool::Mercurial => Self::Mercurial(HgPolicy::new(compiled_rules(rules, |rule| {
        let namespace = match rule.names {
          slates_db::catalog::PolicyNames::Branches => Namespace::Branch,
          slates_db::catalog::PolicyNames::Bookmarks => Namespace::Bookmark,
          slates_db::catalog::PolicyNames::Topics => Namespace::Topic,
          _ => return None,
        };
        HgPattern::parse(namespace, &rule.pattern).ok()
      })?)),
      VcsTool::Subversion => Self::Subversion(compile_svn_policy(rules)?),
    })
  }

  /// The verdict on `principal`'s `right` on `name`, or none for a name of another tool.
  pub(crate) fn verdict(
    &self,
    name: Name<'_>,
    principal: &Principal,
    right: Right,
  ) -> Option<Verdict> {
    match (self, name) {
      (Self::Git(policy), Name::Ref(name)) => Some(policy.verdict(name, principal, right)),
      (Self::Mercurial(policy), Name::Hg(namespace, name)) => {
        Some(policy.verdict(namespace, name, principal, right))
      }
      (Self::Subversion(policy), Name::Path(path)) => Some(policy.verdict(path, principal, right)),
      _ => None,
    }
  }
}

/// `rules` compiled by `pattern`, which reads a rule's pattern in its tool's form or answers none; `Unreadable` naming
/// the first it cannot read.
fn compiled_rules<P>(
  rules: &[slates_db::catalog::PolicyRule],
  pattern: impl Fn(&slates_db::catalog::PolicyRule) -> Option<P>,
) -> Result<Vec<Rule<P>>, PushRefusal> {
  rules
    .iter()
    .enumerate()
    .map(|(index, rule)| {
      let pattern = pattern(rule).ok_or(PushRefusal::Unreadable {
        rule: index_of(index),
      })?;
      Ok(Rule {
        pattern,
        right: right_of(rule.right),
        effect: match rule.effect {
          slates_db::catalog::PolicyEffect::Allow => Effect::Allow,
          slates_db::catalog::PolicyEffect::Deny => Effect::Deny,
        },
        who: who_of(&rule.who),
      })
    })
    .collect()
}

/// The matcher's form of a right the record names.
fn right_of(right: slates_db::catalog::PolicyRight) -> Right {
  match right {
    slates_db::catalog::PolicyRight::Read => Right::Read,
    slates_db::catalog::PolicyRight::Submit => Right::Submit,
    slates_db::catalog::PolicyRight::Rewrite => Right::Rewrite,
    slates_db::catalog::PolicyRight::Delete => Right::Delete,
    slates_db::catalog::PolicyRight::MergeCommit => Right::MergeCommit,
    slates_db::catalog::PolicyRight::UnsignedCommit => Right::UnsignedCommit,
  }
}

/// The matcher's form of the principals a record's rule speaks to.
fn who_of(who: &slates_db::catalog::PolicyWho) -> Who {
  match who {
    slates_db::catalog::PolicyWho::Anyone => Who::Anyone,
    slates_db::catalog::PolicyWho::AnyOf { entries } => Who::AnyOf(
      entries
        .iter()
        .map(|entry| match entry {
          slates_db::catalog::PolicyEntry::Is { principal } => Entry::Is(principal.clone()),
          slates_db::catalog::PolicyEntry::IsNot { principal } => Entry::IsNot(principal.clone()),
        })
        .collect(),
    ),
  }
}

/// The check point every push calls before it submits its register entry (the module doc, A-622 part 2): each change
/// in `asked`, in order, judged by `record`'s policy for `principal`, its own right first and then the requirements a
/// rule imposes on it (A-644); the first refused is the push's refusal. A change no rule speaks to is the owner's alone.
pub(crate) fn admit(
  record: &RepositoryRecord,
  principal: &Principal,
  asked: &[Asked<'_>],
) -> Result<(), PushRefusal> {
  let compiled = Compiled::of(record)?;
  let mut signers: Option<Vec<TrustedKey>> = None;
  for (index, change) in asked.iter().enumerate() {
    let index = index_of(index);
    admit_right(&compiled, record, principal, change, index)?;
    if let Some(brings) = change.brings {
      let judged = Judged {
        compiled: &compiled,
        record,
        principal,
      };
      judged.requirements(change.name, (brings, index), &mut signers)?;
    }
  }
  Ok(())
}

/// Whether `principal` holds `change`'s own right on its name (A-622 part 2): over a policy compiled once, as git's
/// receive-pack hides each ref its pusher may not read (A-654).
pub(crate) fn admit_right(
  compiled: &Compiled,
  record: &RepositoryRecord,
  principal: &Principal,
  change: &Asked<'_>,
  index: u32,
) -> Result<(), PushRefusal> {
  match compiled.verdict(change.name, principal, change.right) {
    None => Err(PushRefusal::OtherTool { index }),
    Some(Verdict::Allowed { .. }) => Ok(()),
    Some(Verdict::Unruled) if *principal == record.owner => Ok(()),
    Some(Verdict::Unruled) => Err(PushRefusal::NotPermitted { index }),
    Some(Verdict::NotListed) => Err(PushRefusal::NotListed { index }),
    Some(Verdict::Denied { rule }) => Err(PushRefusal::Denied {
      index,
      rule: index_of(rule),
    }),
  }
}

/// One push's requirements, judged against its repository's compiled policy for its principal (A-644).
struct Judged<'j> {
  compiled: &'j Compiled,
  record: &'j RepositoryRecord,
  principal: &'j Principal,
}

impl Judged<'_> {
  /// The rule that imposes `requirement` on the principal at `name`: the deciding rule, where it denies. A rule that
  /// allows it, or none, imposes nothing.
  fn imposed(&self, name: Name<'_>, requirement: Right) -> Option<u32> {
    match self.compiled.verdict(name, self.principal, requirement) {
      Some(Verdict::Denied { rule }) => Some(index_of(rule)),
      _ => None,
    }
  }

  /// The requirements imposed at `name`, each judged by what change `index` brings: a linear history, then signed
  /// commits under the record's signers, read once a push into `signers`.
  fn requirements(
    &self,
    name: Name<'_>,
    (brings, index): (&dyn Brings, u32),
    signers: &mut Option<Vec<TrustedKey>>,
  ) -> Result<(), PushRefusal> {
    let unjudged = |refused| PushRefusal::Unjudged { index, refused };
    if let Some(rule) = self.imposed(name, Right::MergeCommit)
      && let Some(commit) = brings.merge_commit().map_err(unjudged)?
    {
      return Err(PushRefusal::MergeCommit {
        index,
        rule,
        commit,
      });
    }
    let Some(rule) = self.imposed(name, Right::UnsignedCommit) else {
      return Ok(());
    };
    let keys =
      signers.get_or_insert_with(|| crate::signed_work::trusted_of(&self.record.policy.signers));
    match brings.unsigned_commit(keys).map_err(unjudged)? {
      Some((commit, why)) => Err(PushRefusal::UnsignedCommit {
        index,
        rule,
        commit,
        why,
      }),
      None => Ok(()),
    }
  }
}

/// Whether `principal` may reach `record`'s repository for any of `rights` at all (A-625 part 2): its owner always, who
/// holds every right a policy leaves unruled (A-575); another principal when some rule of the policy allows it one of
/// them on some name. The gate an edge asks before it starts a service (§4.17.2 "Access": a repository the principal may
/// not reach is `403`); each name is then judged by the rule that decides it, the service's own check ([`admit`] for a
/// push). `Unreadable` for a policy its tool's matcher cannot read, never judged without the rule.
pub(crate) fn reaches(
  record: &RepositoryRecord,
  principal: &Principal,
  rights: &[Right],
) -> Result<bool, PushRefusal> {
  let _readable = Compiled::of(record)?;
  if *principal == record.owner {
    return Ok(true);
  }
  Ok(record.policy.rules.iter().any(|rule| {
    matches!(rule.effect, slates_db::catalog::PolicyEffect::Allow)
      && rights.contains(&right_of(rule.right))
      && who_of(&rule.who).names(principal)
  }))
}

/// Effective repository-level Subversion access for a principal after evaluating `Allow` and
/// `Deny` rules in [`SvnPolicy`] precedence order (`None < Read < Write`, matching Subversion's
/// model where write implies read).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SvnAccess {
  /// No effective `Read` access on any path (including `Submit`-only without `Read`).
  None,
  /// Effective `Read` access on at least one path, but no effective write right.
  Read,
  /// Effective `Read` access and at least one effective write right (`Submit`, `Rewrite`, or
  /// `Delete`).
  Write,
}

/// Compiles `rules` as a Subversion [`SvnPolicy`], returning `Unreadable` naming the first rule
/// whose name kind or path pattern is invalid for Subversion.
fn compile_svn_policy(rules: &[slates_db::catalog::PolicyRule]) -> Result<SvnPolicy, PushRefusal> {
  Ok(SvnPolicy::new(compiled_rules(rules, |rule| {
    match rule.names {
      slates_db::catalog::PolicyNames::Paths => PathPattern::parse(&rule.pattern).ok(),
      _ => None,
    }
  })?))
}

/// Evaluates `principal`'s effective repository-level Subversion access on `record`, honoring
/// `PolicyEffect::Deny` precedence and requiring at least one concrete path that is BOTH readable
/// and writable before granting [`SvnAccess::Write`] (`None < Read < Write`).
pub(crate) fn svn_access(
  record: &RepositoryRecord,
  principal: &Principal,
) -> Result<SvnAccess, PushRefusal> {
  let Compiled::Subversion(policy) = Compiled::of(record)? else {
    return Ok(SvnAccess::None);
  };
  let is_owner = *principal == record.owner;
  let readable =
    |path: &[u8]| verdict_allows(policy.verdict(path, principal, Right::Read), is_owner);
  if !policy.exists_witness(principal, &[Right::Read], readable) {
    return Ok(SvnAccess::None);
  }
  let read_and_write = |path: &[u8]| {
    svn_path_writable(&policy, is_owner, principal, path, false)
      || svn_path_writable(&policy, is_owner, principal, path, true)
  };
  let can_write = policy.exists_witness(
    principal,
    &[Right::Read, Right::Submit, Right::Rewrite, Right::Delete],
    read_and_write,
  );
  Ok(if can_write {
    SvnAccess::Write
  } else {
    SvnAccess::Read
  })
}

/// Compiles `principal`'s per-path Subversion read rules from `record` into
/// [`slates_svn::authz::PrincipalRules`] for all three transports (HTTP WebDAV, `svn://`, and
/// `svn+ssh://`). Fails closed to [`slates_svn::authz::PrincipalRules::deny_all`] if `record` is
/// not a Subversion repository or its policy is unreadable.
pub(crate) fn svn_read_rules(
  record: &RepositoryRecord,
  principal: &Principal,
) -> slates_svn::authz::PrincipalRules {
  let Ok(Compiled::Subversion(policy)) = Compiled::of(record) else {
    return slates_svn::authz::PrincipalRules::deny_all();
  };
  let is_owner = *principal == record.owner;
  let read_rules: Vec<slates_svn::authz::ReadPathRule> = policy
    .rules
    .iter()
    .filter(|rule| rule.speaks(principal, Right::Read))
    .map(|rule| slates_svn::authz::ReadPathRule {
      segments: rule
        .pattern
        .segments
        .iter()
        .map(policy_segment_of)
        .collect(),
      allow: rule.effect == Effect::Allow,
    })
    .collect();
  slates_svn::authz::PrincipalRules::from_read_rules(is_owner, &read_rules)
}

/// Fail-closed wrapper around [`svn_read_rules`]: returns
/// [`slates_svn::authz::PrincipalRules::deny_all`] when no repository record is held.
pub(crate) fn svn_read_rules_for(
  record: Option<&RepositoryRecord>,
  principal: &Principal,
) -> slates_svn::authz::PrincipalRules {
  record.map_or_else(slates_svn::authz::PrincipalRules::deny_all, |record| {
    svn_read_rules(record, principal)
  })
}

/// Whether `verdict` allows access when `allow_unruled` permits [`Verdict::Unruled`].
fn verdict_allows(verdict: Verdict, allow_unruled: bool) -> bool {
  matches!(verdict, Verdict::Allowed { .. })
    || (allow_unruled && matches!(verdict, Verdict::Unruled))
}

/// Checks whether `principal` holds both `Read` and write permission at `path` under `policy`.
/// When `deleting` is true, an explicit `Deny Delete` overrides `Submit`, and an explicit
/// `Allow Delete` suffices without `Submit`.
fn svn_path_writable(
  policy: &SvnPolicy,
  is_owner: bool,
  principal: &Principal,
  path: &[u8],
  deleting: bool,
) -> bool {
  if !verdict_allows(policy.verdict(path, principal, Right::Read), is_owner) {
    return false;
  }
  if deleting {
    return match policy.verdict(path, principal, Right::Delete) {
      Verdict::Allowed { .. } => true,
      Verdict::Denied { .. } | Verdict::NotListed => false,
      Verdict::Unruled => verdict_allows(policy.verdict(path, principal, Right::Submit), is_owner),
    };
  }
  verdict_allows(policy.verdict(path, principal, Right::Submit), is_owner)
    || verdict_allows(policy.verdict(path, principal, Right::Rewrite), false)
}

/// Evaluates whether `principal` may read `path` in Subversion `record` under [`SvnPolicy`].
pub(crate) fn svn_path_read_allowed(
  record: &RepositoryRecord,
  principal: &Principal,
  path: &[u8],
) -> Result<bool, PushRefusal> {
  let Compiled::Subversion(policy) = Compiled::of(record)? else {
    return Ok(false);
  };
  let is_owner = *principal == record.owner;
  Ok(verdict_allows(
    policy.verdict(path, principal, Right::Read),
    is_owner,
  ))
}

/// Evaluates whether `principal` may write (or delete when `deleting`) `path` in Subversion
/// `record` under [`SvnPolicy`], requiring effective `Read` as well as write permission.
pub(crate) fn svn_path_write_allowed(
  record: &RepositoryRecord,
  principal: &Principal,
  path: &[u8],
  deleting: bool,
) -> Result<bool, PushRefusal> {
  let Compiled::Subversion(policy) = Compiled::of(record)? else {
    return Ok(false);
  };
  let is_owner = *principal == record.owner;
  Ok(svn_path_writable(
    &policy, is_owner, principal, path, deleting,
  ))
}

/// Joins a canonical Subversion directory path and one child segment without doubling `/`.
fn join_svn_child(parent: &str, child: &str) -> String {
  if parent == "/" || parent.is_empty() {
    format!("/{child}")
  } else {
    format!("{parent}/{child}")
  }
}

/// Maps `path` under the deepest strict ancestor in `changes` carrying `copy_from`, so deleting a
/// subdirectory copied earlier in the same commit still checks its inherited descendants.
fn copied_ancestor(
  changes: &[slates_svn::index::ChangedPath],
  path: &str,
) -> Option<(String, slates_svn::index::Revision)> {
  let mut best: Option<(usize, String, slates_svn::index::Revision)> = None;
  for change in changes {
    let Some(copy_from) = &change.copy_from else {
      continue;
    };
    let trimmed = change.path.trim_end_matches('/');
    let suffix = if trimmed.is_empty() {
      (path != "/").then_some(path)
    } else {
      path
        .strip_prefix(trimmed)
        .filter(|rest| rest.starts_with('/'))
    };
    let Some(suffix) = suffix else {
      continue;
    };
    let mapped = format!("{}{suffix}", copy_from.path.trim_end_matches('/'));
    if best
      .as_ref()
      .is_none_or(|(len, _, _)| trimmed.len() >= *len)
    {
      best = Some((trimmed.len(), mapped, copy_from.revision));
    }
  }
  best.map(|(_, mapped, rev)| (mapped, rev))
}

/// Bundled Subversion policy context for commit-time path and subtree checks.
struct SvnAuthorizer<'a> {
  policy: &'a SvnPolicy,
  is_owner: bool,
  principal: &'a Principal,
}

impl SvnAuthorizer<'_> {
  /// Whether `principal` may read `path`.
  fn readable(&self, path: &[u8]) -> bool {
    verdict_allows(
      self.policy.verdict(path, self.principal, Right::Read),
      self.is_owner,
    )
  }

  /// Whether `principal` may write (or delete when `deleting`) `path`.
  fn writable(&self, path: &[u8], deleting: bool) -> bool {
    svn_path_writable(self.policy, self.is_owner, self.principal, path, deleting)
  }

  /// Checks the changed path itself, requiring BOTH delete and write for `Action::Replace`.
  fn admit_target(&self, change: &slates_svn::index::ChangedPath) -> bool {
    use slates_svn::index::Action;
    let path = change.path.as_bytes();
    match change.action {
      Action::Delete => self.writable(path, true),
      Action::Replace => self.writable(path, true) && self.writable(path, false),
      Action::Add | Action::Modify => self.writable(path, false),
    }
  }

  /// Checks one directory's immediate entries, pushing subdirectories onto `stack`.
  fn visit_entries(
    index: &slates_svn::index::Index,
    (src_dir, dst_dir): (&str, &str),
    entries: Vec<(String, slates_svn::index::NodeRevisionId)>,
    stack: &mut Vec<(String, String)>,
    check: &impl Fn(&str, &str) -> bool,
  ) -> bool {
    for (name, node_id) in entries {
      let src_child = join_svn_child(src_dir, &name);
      let dst_child = join_svn_child(dst_dir, &name);
      if !check(&src_child, &dst_child) {
        return false;
      }
      let Ok(node) = index.node_revision(node_id) else {
        return false;
      };
      if node.kind() == slates_svn::index::NodeKind::Directory {
        stack.push((src_child, dst_child));
      }
    }
    true
  }

  /// Iteratively checks every existing descendant under `(src_root, rev)` mapped to `dst_root`,
  /// requiring `check(src_desc, dst_desc)` on every descendant and failing closed on index errors.
  fn walk_subtree(
    &self,
    index: &slates_svn::index::Index,
    src_root: &str,
    rev: slates_svn::index::Revision,
    dst_root: &str,
    check: impl Fn(&str, &str) -> bool,
  ) -> bool {
    let mut stack = vec![(src_root.to_owned(), dst_root.to_owned())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
      let Ok(listing) = index.listing(&src_dir, rev) else {
        return false;
      };
      let Some(entries) = listing else {
        continue;
      };
      if !Self::visit_entries(index, (&src_dir, &dst_dir), entries, &mut stack, &check) {
        return false;
      }
    }
    true
  }

  /// Checks `copy_from` source readability and destination writability across the copied subtree.
  fn admit_copy(
    &self,
    index: &slates_svn::index::Index,
    change: &slates_svn::index::ChangedPath,
    skip_subtrees: bool,
  ) -> bool {
    let Some(copy_from) = &change.copy_from else {
      return true;
    };
    if !self.readable(copy_from.path.as_bytes()) {
      return false;
    }
    skip_subtrees
      || self.walk_subtree(
        index,
        &copy_from.path,
        copy_from.revision,
        &change.path,
        |src, dst| self.readable(src.as_bytes()) && self.writable(dst.as_bytes(), false),
      )
  }

  /// Checks deleted/replaced descendants at `base` and under any intra-commit copied ancestor.
  fn admit_delete_subtree(
    &self,
    index: &slates_svn::index::Index,
    base: slates_svn::index::Revision,
    changes: &[slates_svn::index::ChangedPath],
    change: &slates_svn::index::ChangedPath,
    skip_subtrees: bool,
  ) -> bool {
    let deletes_tree = matches!(
      change.action,
      slates_svn::index::Action::Delete | slates_svn::index::Action::Replace
    );
    if skip_subtrees || !deletes_tree {
      return true;
    }
    if !self.walk_subtree(index, &change.path, base, &change.path, |_, dst| {
      self.writable(dst.as_bytes(), true)
    }) {
      return false;
    }
    match copied_ancestor(changes, &change.path) {
      Some((src_mapped, src_rev)) => {
        self.walk_subtree(index, &src_mapped, src_rev, &change.path, |_, dst| {
          self.writable(dst.as_bytes(), true)
        })
      }
      None => true,
    }
  }

  /// Checks one changed path and any affected directory descendants on `Delete`, `Replace`, or
  /// `copy_from`.
  fn admit_change(
    &self,
    index: &slates_svn::index::Index,
    base: slates_svn::index::Revision,
    changes: &[slates_svn::index::ChangedPath],
    change: &slates_svn::index::ChangedPath,
    skip_subtrees: bool,
  ) -> bool {
    self.admit_target(change)
      && self.admit_copy(index, change, skip_subtrees)
      && self.admit_delete_subtree(index, base, changes, change, skip_subtrees)
  }
}

/// Checks whether `principal` is allowed by `policy` (owned by `owner`) to commit every changed
/// path, deleted/replaced directory descendant at `base`, and copied source subtree in `changes`.
pub(crate) fn admit_svn_changes(
  owner: &Principal,
  policy: &slates_db::catalog::RepositoryPolicy,
  principal: &Principal,
  tree: (&slates_svn::index::Index, slates_svn::index::Revision),
  changes: &[slates_svn::index::ChangedPath],
) -> Result<(), PushRefusal> {
  let compiled = compile_svn_policy(&policy.rules)?;
  let is_owner = principal == owner;
  let skip_subtrees = compiled.rules.is_empty() && is_owner;
  let authorizer = SvnAuthorizer {
    policy: &compiled,
    is_owner,
    principal,
  };
  let (index, base) = tree;
  for (index_pos, change) in changes.iter().enumerate() {
    if !authorizer.admit_change(index, base, changes, change, skip_subtrees) {
      return Err(PushRefusal::NotPermitted {
        index: index_of(index_pos),
      });
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;
  use slates_db::HostId;

  use super::{
    Effect, Entry, GitPolicy, HgPattern, HgPolicy, Namespace, PathPattern, PatternRefused,
    Principal, RefPattern, Right, Rule, SvnPolicy, Verdict, Who,
  };
  use crate::wildmatch_reference as reference;

  /// The principal with uid `uid` on host 0.
  fn principal(uid: u32) -> Principal {
    Principal::Uid {
      host: HostId(0),
      uid,
    }
  }

  /// A rule over `pattern`.
  fn rule<P>(pattern: P, right: Right, effect: Effect, who: Who) -> Rule<P> {
    Rule {
      pattern,
      right,
      effect,
      who,
    }
  }

  /// `Who` naming exactly these uids.
  fn only(uids: &[u32]) -> Who {
    Who::AnyOf(uids.iter().map(|&uid| Entry::Is(principal(uid))).collect())
  }

  /// A git policy of `(pattern, effect, who)` rules, every one about submitting.
  fn git(rules: &[(&str, Effect, Who)]) -> GitPolicy {
    GitPolicy::new(
      rules
        .iter()
        .map(|(pattern, effect, who)| {
          rule(
            RefPattern::parse(pattern).unwrap(),
            Right::Submit,
            *effect,
            who.clone(),
          )
        })
        .collect(),
    )
  }

  /// A-597 (git's literal and glob coverage, as for-each-ref's documentation and `match_name_as_path` state it). Do:
  /// ask which refs literal patterns and globs cover. Expect: `refs/heads` covers itself and every ref beneath it but
  /// not `refs/headsx`; `refs/heads/mas` does not cover `refs/heads/master` (a literal stops only at a slash);
  /// `refs/heads/` covers what is beneath it; `refs/heads/release/*` stays in its segment while `refs/heads/release/**`
  /// does not.
  #[test]
  fn a_git_pattern_covers_what_for_each_ref_matches() {
    let cases: &[(&str, &str, bool)] = &[
      ("refs/heads", "refs/heads", true),
      ("refs/heads", "refs/heads/main", true),
      ("refs/heads", "refs/heads/feature/x", true),
      ("refs/heads", "refs/headsx", false),
      ("refs/heads/mas", "refs/heads/master", false),
      ("refs/heads/", "refs/heads/main", true),
      ("refs/heads/release/*", "refs/heads/release/1.0", true),
      (
        "refs/heads/release/*",
        "refs/heads/release/1.0/hotfix",
        false,
      ),
      (
        "refs/heads/release/**",
        "refs/heads/release/1.0/hotfix",
        true,
      ),
      ("refs/tags/v[0-9]*", "refs/tags/v1.2", true),
      ("refs/tags/v[0-9]*", "refs/tags/vx", false),
    ];
    for &(pattern, name, covered) in cases {
      assert_eq!(
        RefPattern::parse(pattern).unwrap().covers(name.as_bytes()),
        covered,
        "{pattern} covering {name}"
      );
    }
  }

  /// A-597 (git's precedence: "later entries override earlier ones", `transfer.hideRefs`). Do: deny submitting to the
  /// release branches to everyone, then allow it to one principal; then the same rules the other way round. Expect: in
  /// the first order the allowed principal is allowed by the later rule and another is denied by the earlier one; in
  /// the second the later denial decides for both; and a ref no rule covers is unruled.
  #[test]
  fn the_last_git_rule_that_speaks_decides() {
    let (alice, bob) = (principal(1), principal(2));
    let denied_then_allowed = git(&[
      ("refs/heads/release/*", Effect::Deny, Who::Anyone),
      ("refs/heads/release", Effect::Allow, only(&[1])),
    ]);
    let release = b"refs/heads/release/2.0";
    assert_eq!(
      denied_then_allowed.verdict(release, &alice, Right::Submit),
      Verdict::Allowed { rule: 1 }
    );
    assert_eq!(
      denied_then_allowed.verdict(release, &bob, Right::Submit),
      Verdict::Denied { rule: 0 }
    );
    assert_eq!(
      denied_then_allowed.verdict(release, &bob, Right::Rewrite),
      Verdict::Unruled
    );
    let allowed_then_denied = git(&[
      ("refs/heads/release", Effect::Allow, only(&[1])),
      ("refs/heads/release/*", Effect::Deny, Who::Anyone),
    ]);
    assert_eq!(
      allowed_then_denied.verdict(release, &alice, Right::Submit),
      Verdict::Denied { rule: 1 }
    );
    assert_eq!(
      allowed_then_denied.verdict(b"refs/heads/main", &alice, Right::Submit),
      Verdict::Unruled
    );
  }

  /// The principals of acl's documented example: `bad-user`, `user-1`, `user-2`, `user-3`, `super-user`, `gollum`, and
  /// `other` (no entry anywhere).
  const BAD: u32 = 10;
  /// `user-1`.
  const USER_1: u32 = 11;
  /// `user-2`.
  const USER_2: u32 = 12;
  /// `user-3`.
  const USER_3: u32 = 13;
  /// `super-user`.
  const SUPER: u32 = 14;
  /// `gollum`.
  const GOLLUM: u32 = 15;
  /// A principal no entry names.
  const OTHER: u32 = 16;

  /// A-597 (Mercurial's precedence, acl's documented example in `hgext/acl.py`, 7.2.4). Do: read its branch sections as
  /// rules (`[acl.deny.branches]`: `frozen-branch = *`, `* = bad-user`; `[acl.allow.branches]`: `branch-a = user-1,
  /// user-2, user-3`, `branch-b = user-1`, `* = super-user`, `branch-for-tests = *`) and ask the cases its comments
  /// state. Expect: everyone is denied the frozen branch, the super user too; the bad user is denied everywhere, even on
  /// the branch open to everyone; user-2 is on branch-a's list and not branch-b's; the super user is allowed on any
  /// other branch; a principal on no list is refused, "not allowed"; bookmarks have no rules, so they are unruled.
  #[test]
  fn acls_documented_example_reads_as_acl_reads_it() {
    let branch = |name: &str| HgPattern::parse(Namespace::Branch, name).unwrap();
    let policy = HgPolicy::new(vec![
      rule(
        branch("frozen-branch"),
        Right::Submit,
        Effect::Deny,
        Who::Anyone,
      ),
      rule(branch("*"), Right::Submit, Effect::Deny, only(&[BAD])),
      rule(
        branch("branch-a"),
        Right::Submit,
        Effect::Allow,
        only(&[USER_1, USER_2, USER_3]),
      ),
      rule(
        branch("branch-b"),
        Right::Submit,
        Effect::Allow,
        only(&[USER_1]),
      ),
      rule(branch("*"), Right::Submit, Effect::Allow, only(&[SUPER])),
      rule(
        branch("branch-for-tests"),
        Right::Submit,
        Effect::Allow,
        Who::Anyone,
      ),
    ]);
    let ask = |name: &str, uid: u32| {
      policy.verdict(Namespace::Branch, name, &principal(uid), Right::Submit)
    };
    assert_eq!(ask("frozen-branch", SUPER), Verdict::Denied { rule: 0 });
    assert_eq!(ask("branch-for-tests", BAD), Verdict::Denied { rule: 1 });
    assert_eq!(ask("branch-a", USER_2), Verdict::Allowed { rule: 2 });
    assert_eq!(ask("branch-b", USER_2), Verdict::NotListed);
    assert_eq!(ask("branch-b", USER_1), Verdict::Allowed { rule: 3 });
    assert_eq!(ask("default", SUPER), Verdict::Allowed { rule: 4 });
    assert_eq!(ask("branch-for-tests", OTHER), Verdict::Allowed { rule: 5 });
    assert_eq!(ask("default", OTHER), Verdict::NotListed);
    assert_eq!(
      policy.verdict(
        Namespace::Bookmark,
        "default",
        &principal(OTHER),
        Right::Submit
      ),
      Verdict::Unruled
    );
  }

  /// A-597 (acl's `!` example: "Deny access to branch "ring" to anyone but user "gollum"", with no allow rules). Do:
  /// deny submitting to `ring` to every principal but gollum. Expect: gollum is unruled on `ring` (no rule speaks to
  /// them), another principal is denied there, and both are unruled on any other branch, "anyone else can still commit
  /// to any other branch".
  #[test]
  fn acls_inverted_entry_denies_everyone_but_the_named_principal() {
    let policy = HgPolicy::new(vec![rule(
      HgPattern::parse(Namespace::Branch, "ring").unwrap(),
      Right::Submit,
      Effect::Deny,
      Who::AnyOf(vec![Entry::IsNot(principal(GOLLUM))]),
    )]);
    let ask = |name: &str, uid: u32| {
      policy.verdict(Namespace::Branch, name, &principal(uid), Right::Submit)
    };
    assert_eq!(ask("ring", GOLLUM), Verdict::Unruled);
    assert_eq!(ask("ring", OTHER), Verdict::Denied { rule: 0 });
    assert_eq!(ask("lake", OTHER), Verdict::Unruled);
    assert_eq!(ask("lake", GOLLUM), Verdict::Unruled);
  }

  /// A Subversion policy of `(pattern, effect)` rules about submitting, every one speaking to everyone.
  fn svn(rules: &[(&str, Effect)]) -> SvnPolicy {
    SvnPolicy::new(
      rules
        .iter()
        .map(|(pattern, effect)| {
          rule(
            PathPattern::parse(pattern).unwrap(),
            Right::Submit,
            *effect,
            Who::Anyone,
          )
        })
        .collect(),
    )
  }

  /// A-597 (Subversion's precedence, authz's lookup in `authz.c`). Do: allow submitting under `/trunk`, deny it under
  /// `/trunk/secret`, and ask at paths at and beneath each, and beside them. Expect: the deeper rule decides beneath it
  /// whatever its order, `/trunk`'s rule decides the rest of the trunk (inherited), and a path neither covers is
  /// unruled.
  #[test]
  fn the_deepest_subversion_rule_decides() {
    let anyone = principal(OTHER);
    for policy in [
      svn(&[("/trunk", Effect::Allow), ("/trunk/secret", Effect::Deny)]),
      svn(&[("/trunk/secret", Effect::Deny), ("/trunk", Effect::Allow)]),
    ] {
      assert_the_deeper_rule_decides_beneath_it(&policy, &anyone);
    }
  }

  /// The expectations of [`the_deepest_subversion_rule_decides`] for one order of its two rules.
  fn assert_the_deeper_rule_decides_beneath_it(policy: &SvnPolicy, anyone: &Principal) {
    let ask = |path: &str| policy.verdict(path.as_bytes(), anyone, Right::Submit);
    assert!(matches!(
      ask("/trunk/secret/plans.txt"),
      Verdict::Denied { .. }
    ));
    assert!(matches!(ask("/trunk/secret"), Verdict::Denied { .. }));
    assert!(matches!(ask("/trunk/src/main.c"), Verdict::Allowed { .. }));
    assert!(matches!(ask("//trunk//src/"), Verdict::Allowed { .. }));
    assert_eq!(ask("/branches/x"), Verdict::Unruled);
  }

  /// A-597 (authz's sequence numbers, and a `**` rule's depth, `authz.c`'s `lookup` and `trim_tree`). Do: deny
  /// `/trunk/a`, then allow `:glob:/trunk/**`; then the same two the other way round. Expect: at `/trunk/a` both rules
  /// match at the same depth, so the one defined last decides: allowed in the first order (`trim_tree` eclipses the
  /// earlier rule), denied in the second. At `/trunk` the `**` rule decides, matching no segment. Beneath `/trunk/a`
  /// the `**` rule matches deeper than the literal rule in either order, so it decides there, as authz's lookup adds a
  /// `**` node again at every level: a later literal rule does not shield what is beneath it from an earlier `**`.
  #[test]
  fn at_one_depth_the_subversion_rule_defined_last_decides_and_a_double_star_reaches_deeper() {
    let anyone = principal(OTHER);
    let eclipsed = svn(&[
      ("/trunk/a", Effect::Deny),
      (":glob:/trunk/**", Effect::Allow),
    ]);
    assert_eq!(
      eclipsed.verdict(b"/trunk/a", &anyone, Right::Submit),
      Verdict::Allowed { rule: 1 }
    );
    assert_eq!(
      eclipsed.verdict(b"/trunk", &anyone, Right::Submit),
      Verdict::Allowed { rule: 1 }
    );
    let not_eclipsed = svn(&[
      (":glob:/trunk/**", Effect::Allow),
      ("/trunk/a", Effect::Deny),
    ]);
    assert_eq!(
      not_eclipsed.verdict(b"/trunk/a", &anyone, Right::Submit),
      Verdict::Denied { rule: 1 }
    );
    assert_eq!(
      not_eclipsed.verdict(b"/trunk/a/b.c", &anyone, Right::Submit),
      Verdict::Allowed { rule: 0 }
    );
  }

  /// A-597 (the 1.10 release notes: "/*/**/* will match any path which contains at least 2 segments and is equivalent
  /// to /**/*/* as well as /*/*/**"; and "In these rules, /path is always matched literally"). Do: ask each of the three
  /// glob patterns, and the literal `/*/*`, about paths of one to four segments. Expect: each glob covers exactly the
  /// paths of two segments or more, and the literal pattern covers only a path whose segments are stars.
  #[test]
  fn the_release_notes_equivalent_globs_cover_the_same_paths() {
    let anyone = principal(OTHER);
    for pattern in [":glob:/*/**/*", ":glob:/**/*/*", ":glob:/*/*/**"] {
      let policy = svn(&[(pattern, Effect::Allow)]);
      for (path, covered) in [
        ("/a", false),
        ("/a/b", true),
        ("/a/b/c", true),
        ("/a/b/c/d", true),
      ] {
        assert_eq!(
          policy.verdict(path.as_bytes(), &anyone, Right::Submit) != Verdict::Unruled,
          covered,
          "{pattern} at {path}"
        );
      }
    }
    let literal = svn(&[("/*/*", Effect::Allow)]);
    assert_eq!(
      literal.verdict(b"/a/b", &anyone, Right::Submit),
      Verdict::Unruled
    );
    assert_eq!(
      literal.verdict(b"/*/*/c", &anyone, Right::Submit),
      Verdict::Allowed { rule: 0 }
    );
  }

  /// A-597 (typed refusals where a pattern is read). Do: read empty patterns, patterns holding a NUL, and a Subversion
  /// path not beginning with `/`. Expect: each refused by its kind, nothing read.
  #[test]
  fn a_pattern_no_name_could_match_is_refused_by_kind() {
    assert_eq!(RefPattern::parse(""), Err(PatternRefused::Empty));
    assert_eq!(RefPattern::parse("refs/\0"), Err(PatternRefused::HoldsNul));
    assert_eq!(
      HgPattern::parse(Namespace::Topic, ""),
      Err(PatternRefused::Empty)
    );
    assert_eq!(
      HgPattern::parse(Namespace::Bookmark, "a\0b"),
      Err(PatternRefused::HoldsNul)
    );
    assert_eq!(
      PathPattern::parse("trunk"),
      Err(PatternRefused::NotAbsolute)
    );
    assert_eq!(
      PathPattern::parse(":glob:trunk/**"),
      Err(PatternRefused::NotAbsolute)
    );
    assert_eq!(PathPattern::parse(""), Err(PatternRefused::Empty));
  }

  /// The principals generated rules and questions are about.
  const PRINCIPALS: [u32; 3] = [1, 2, 3];

  /// A generated `Who`: everyone, or entries naming or excepting the principals.
  fn who() -> impl Strategy<Value = Who> {
    prop::strategy::Union::new([
      Just(Who::Anyone).boxed(),
      prop::collection::vec(
        (prop::sample::select(PRINCIPALS.to_vec()), any::<bool>()),
        1..3,
      )
      .prop_map(|entries| {
        Who::AnyOf(
          entries
            .into_iter()
            .map(|(uid, is)| {
              if is {
                Entry::Is(principal(uid))
              } else {
                Entry::IsNot(principal(uid))
              }
            })
            .collect(),
        )
      })
      .boxed(),
    ])
  }

  /// A generated right.
  fn right() -> impl Strategy<Value = Right> {
    prop::sample::select(vec![
      Right::Read,
      Right::Submit,
      Right::Rewrite,
      Right::Delete,
    ])
  }

  /// A generated effect.
  fn effect() -> impl Strategy<Value = Effect> {
    prop::sample::select(vec![Effect::Allow, Effect::Deny])
  }

  /// A rule's text, right, effect and principals, generated from `pattern`.
  fn rule_text(
    pattern: impl Strategy<Value = String>,
  ) -> impl Strategy<Value = (String, Right, Effect, Who)> {
    (pattern, right(), effect(), who())
  }

  /// Text joined from 1 to `most` of `pieces`.
  fn joined(pieces: &'static [&'static str], most: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(pieces), 1..=most).prop_map(|parts| parts.concat())
  }

  /// git pattern pieces: literal parts, slashes and every glob form.
  const GIT_PIECES: &[&str] = &[
    "refs", "/", "heads", "tags", "main", "a", "b", "*", "**", "?", "[ab]", "[!a]",
  ];
  /// git name pieces.
  const GIT_NAME_PIECES: &[&str] = &["refs", "/", "heads", "tags", "main", "a", "b"];

  /// `match_name_as_path`'s literal test, transcribed from `ref-filter.c` for the oracle.
  fn oracle_literal(pattern: &[u8], name: &[u8]) -> bool {
    let plen = pattern.len();
    plen <= name.len()
      && name[..plen] == *pattern
      && (name.len() == plen || name[plen] == b'/' || pattern[plen - 1] == b'/')
  }

  /// The git oracle: every rule in order, the last that covers the name (git's literal test, then git's own
  /// `dowild`) and speaks to the principal deciding.
  fn git_oracle(
    rules: &[(String, Right, Effect, Who)],
    name: &[u8],
    asker: &Principal,
    asked: Right,
  ) -> Verdict {
    let mut verdict = Verdict::Unruled;
    for (position, (pattern, right, effect, who)) in rules.iter().enumerate() {
      let covers =
        oracle_literal(pattern.as_bytes(), name) || reference::wildmatch(pattern.as_bytes(), name);
      if covers && *right == asked && who.names(asker) {
        verdict = match effect {
          Effect::Allow => Verdict::Allowed { rule: position },
          Effect::Deny => Verdict::Denied { rule: position },
        };
      }
    }
    verdict
  }

  /// Mercurial name pieces, `*` among them.
  const HG_NAMES: &[&str] = &["default", "stable", "frozen", "*"];
  /// The names Mercurial questions are about.
  const HG_ASKED: &[&str] = &["default", "stable", "frozen", "other"];
  /// The namespaces.
  const NAMESPACES: [Namespace; 3] = [Namespace::Branch, Namespace::Bookmark, Namespace::Topic];

  /// The Mercurial oracle: acl's order, every rule in order: the first denial that covers the name and speaks to the
  /// principal decides; else, when any rule of the namespace allows the right, the first allowing rule that covers and
  /// speaks, or not listed; else unruled.
  fn hg_oracle(
    rules: &[(Namespace, String, Right, Effect, Who)],
    asked_in: Namespace,
    name: &str,
    asker: &Principal,
    asked: Right,
  ) -> Verdict {
    let speaks = |(namespace, pattern, right, _, who): &(Namespace, String, Right, Effect, Who)| {
      *namespace == asked_in
        && (pattern == "*" || pattern == name)
        && *right == asked
        && who.names(asker)
    };
    for (position, entry) in rules.iter().enumerate() {
      if entry.3 == Effect::Deny && speaks(entry) {
        return Verdict::Denied { rule: position };
      }
    }
    let listed = rules.iter().any(|(namespace, _, right, effect, _)| {
      *namespace == asked_in && *right == asked && *effect == Effect::Allow
    });
    if !listed {
      return Verdict::Unruled;
    }
    for (position, entry) in rules.iter().enumerate() {
      if entry.3 == Effect::Allow && speaks(entry) {
        return Verdict::Allowed { rule: position };
      }
    }
    Verdict::NotListed
  }

  /// Subversion pattern segments: literal names, `*`, `**`, and wildcards within a segment.
  const SVN_SEGMENTS: &[&str] = &["trunk", "tags", "a", "b", "*", "**", "a*", "*b", "?"];
  /// Subversion path segments.
  const SVN_PATH_SEGMENTS: &[&str] = &["trunk", "tags", "a", "b", "ab"];

  /// A generated Subversion pattern: the glob form or the literal one, over 0 to 3 segments.
  fn svn_pattern() -> impl Strategy<Value = String> {
    (
      any::<bool>(),
      prop::collection::vec(prop::sample::select(SVN_SEGMENTS), 0..4),
    )
      .prop_map(|(glob, segments)| {
        let path = format!("/{}", segments.join("/"));
        if glob { format!(":glob:{path}") } else { path }
      })
  }

  /// Whether the oracle's pattern segments match `path` whole (`*` one non-empty segment, `**` any number, a segment
  /// with wildcards by git's `dowild`, any other byte for byte), by trying every split.
  fn oracle_segments(pattern: &[(bool, &str)], path: &[&str]) -> bool {
    let Some((&(glob, first), rest)) = pattern.split_first() else {
      return path.is_empty();
    };
    if glob && first == "**" {
      return (0..=path.len()).any(|taken| oracle_segments(rest, &path[taken..]));
    }
    let Some((segment, after)) = path.split_first() else {
      return false;
    };
    let takes = if glob && first == "*" {
      !segment.is_empty()
    } else if glob && first.bytes().any(|byte| b"*?[\\".contains(&byte)) {
      reference::wildmatch(first.as_bytes(), segment.as_bytes())
    } else {
      first == *segment
    };
    takes && oracle_segments(rest, after)
  }

  /// The Subversion oracle: every rule in order, each tried at every depth from the path's full length down; the rule
  /// speaking to the principal at the deepest depth decides, and at that depth the one defined last.
  fn svn_oracle(
    rules: &[(String, Right, Effect, Who)],
    path: &[&str],
    asker: &Principal,
    asked: Right,
  ) -> Verdict {
    let mut best: Option<(usize, usize, Effect)> = None;
    for (position, (text, right, effect, who)) in rules.iter().enumerate() {
      if *right != asked || !who.names(asker) {
        continue;
      }
      let (glob, rest) = match text.strip_prefix(":glob:") {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
      };
      let pattern: Vec<(bool, &str)> = rest
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| (glob, segment))
        .collect();
      let deepest = (0..=path.len())
        .rev()
        .find(|&depth| oracle_segments(&pattern, &path[..depth]));
      if let Some(depth) = deepest
        && best.is_none_or(|(best_depth, best_position, _)| {
          (depth, position) > (best_depth, best_position)
        })
      {
        best = Some((depth, position, *effect));
      }
    }
    match best {
      None => Verdict::Unruled,
      Some((_, rule, Effect::Allow)) => Verdict::Allowed { rule },
      Some((_, rule, Effect::Deny)) => Verdict::Denied { rule },
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 8 } else { 1024 }),
      ..ProptestConfig::default()
    }))]

    /// A-597 (the git oracle). Do: index generated rules over every glob form and ask generated questions. Expect:
    /// every verdict is the one trying every rule in order gives, with git's literal test and git's own `dowild`.
    #[test]
    fn a_git_policy_decides_as_trying_every_rule_in_order_does(
      rules in prop::collection::vec(rule_text(joined(GIT_PIECES, 5)), 0..8),
      names in prop::collection::vec(joined(GIT_NAME_PIECES, 6), 1..6),
      asker in prop::sample::select(PRINCIPALS.to_vec()),
      asked in right(),
    ) {
      let policy = GitPolicy::new(
        rules
          .iter()
          .map(|(pattern, right, effect, who)| {
            rule(RefPattern::parse(pattern).unwrap(), *right, *effect, who.clone())
          })
          .collect(),
      );
      let asker = principal(asker);
      for name in &names {
        prop_assert_eq!(
          policy.verdict(name.as_bytes(), &asker, asked),
          git_oracle(&rules, name.as_bytes(), &asker, asked),
          "{} under {:?}",
          name,
          rules
        );
      }
    }

    /// A-597 (the Mercurial oracle). Do: index generated rules over the three namespaces and ask generated questions.
    /// Expect: every verdict is the one acl's order gives trying every rule in order.
    #[test]
    fn a_mercurial_policy_decides_as_acls_order_over_every_rule_does(
      rules in prop::collection::vec(
        (prop::sample::select(NAMESPACES.to_vec()), prop::sample::select(HG_NAMES), right(), effect(), who()),
        0..10,
      ),
      asked_in in prop::sample::select(NAMESPACES.to_vec()),
      name in prop::sample::select(HG_ASKED),
      asker in prop::sample::select(PRINCIPALS.to_vec()),
      asked in right(),
    ) {
      let rules: Vec<(Namespace, String, Right, Effect, Who)> = rules
        .into_iter()
        .map(|(namespace, pattern, right, effect, who)| (namespace, pattern.to_owned(), right, effect, who))
        .collect();
      let policy = HgPolicy::new(
        rules
          .iter()
          .map(|(namespace, pattern, right, effect, who)| {
            rule(HgPattern::parse(*namespace, pattern).unwrap(), *right, *effect, who.clone())
          })
          .collect(),
      );
      let asker = principal(asker);
      prop_assert_eq!(
        policy.verdict(asked_in, name, &asker, asked),
        hg_oracle(&rules, asked_in, name, &asker, asked)
      );
    }

    /// A-597 (the Subversion oracle). Do: index generated literal and glob rules and ask about generated paths.
    /// Expect: every verdict is the one trying every rule at every depth gives, by authz's precedence.
    #[test]
    fn a_subversion_policy_decides_as_trying_every_rule_at_every_depth_does(
      rules in prop::collection::vec(rule_text(svn_pattern()), 0..8),
      paths in prop::collection::vec(prop::collection::vec(prop::sample::select(SVN_PATH_SEGMENTS), 0..5), 1..6),
      asker in prop::sample::select(PRINCIPALS.to_vec()),
      asked in right(),
    ) {
      let policy = SvnPolicy::new(
        rules
          .iter()
          .map(|(pattern, right, effect, who)| {
            rule(PathPattern::parse(pattern).unwrap(), *right, *effect, who.clone())
          })
          .collect(),
      );
      let asker = principal(asker);
      for path in &paths {
        let text = format!("/{}", path.join("/"));
        prop_assert_eq!(
          policy.verdict(text.as_bytes(), &asker, asked),
          svn_oracle(&rules, path, &asker, asked),
          "{} under {:?}",
          text,
          rules
        );
      }
    }
  }

  /// A record's rule: `effect` for `right` on `pattern` of `names`, to `who` (A-622 part 2).
  fn record_rule(
    names: slates_db::catalog::PolicyNames,
    pattern: &str,
    (right, effect): (
      slates_db::catalog::PolicyRight,
      slates_db::catalog::PolicyEffect,
    ),
    who: slates_db::catalog::PolicyWho,
  ) -> slates_db::catalog::PolicyRule {
    slates_db::catalog::PolicyRule {
      names,
      pattern: pattern.to_owned(),
      right,
      effect,
      who,
    }
  }

  /// The record's form of `who` naming exactly these uids.
  fn record_only(uids: &[u32]) -> slates_db::catalog::PolicyWho {
    slates_db::catalog::PolicyWho::AnyOf {
      entries: uids
        .iter()
        .map(|&uid| slates_db::catalog::PolicyEntry::Is {
          principal: principal(uid),
        })
        .collect(),
    }
  }

  /// Shape: the repository owner's uid in the check point's tests.
  const OWNER: u32 = 1;
  /// Shape: a principal other than the owner.
  const STRANGER: u32 = 2;
  /// Shape: a principal no rule names.
  const UNNAMED: u32 = 3;

  /// A repository record of `tool` owned by `OWNER`, its policy `rules` at version 1.
  fn record(
    tool: slates_db::catalog::VcsTool,
    rules: Vec<slates_db::catalog::PolicyRule>,
  ) -> super::RepositoryRecord {
    super::RepositoryRecord {
      index_bits: 0,
      id: slates_db::catalog::RepositoryId { bytes: [0; 16] },
      name: "repository".to_owned(),
      tool,
      format: slates_db::catalog::ObjectFormat::Sha1,
      default_branch: None,
      owner: principal(OWNER),
      policy: slates_db::catalog::RepositoryPolicy {
        version: 1,
        rules,
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
        hg_publishing: true,
      },
    }
  }

  /// A change of git ref `name` needing `right`.
  fn git_change(name: &str, right: Right) -> super::Asked<'_> {
    super::Asked {
      name: super::Name::Ref(name.as_bytes()),
      right,
      brings: None,
    }
  }

  /// A-622 part 2 (the check point, git). Do: with rules letting one stranger submit to `refs/heads/feature/*` and
  /// denying everyone the rewrite of `refs/heads/main`, ask for the owner's fast-forward and rewrite of `main`, the
  /// stranger's push to a feature branch and to `main`, a push of two changes whose second is refused, and a Mercurial
  /// name. Expect: the owner's fast-forward admitted (no rule speaks, so it is the owner's), its rewrite denied by rule 1;
  /// the stranger admitted on the feature branch by rule 0 and refused `NotPermitted` on `main`; the two-change push
  /// refused at its second change; the Mercurial name refused as another tool's.
  #[test]
  fn a_push_is_admitted_by_its_repositorys_policy_and_its_owners_rights() {
    use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight, PolicyWho, VcsTool};
    let git = record(
      VcsTool::Git,
      vec![
        record_rule(
          PolicyNames::Refs,
          "refs/heads/feature/*",
          (PolicyRight::Submit, PolicyEffect::Allow),
          record_only(&[STRANGER]),
        ),
        record_rule(
          PolicyNames::Refs,
          "refs/heads/main",
          (PolicyRight::Rewrite, PolicyEffect::Deny),
          PolicyWho::Anyone,
        ),
      ],
    );
    let (owner, stranger) = (principal(OWNER), principal(STRANGER));
    let main = "refs/heads/main";
    assert_eq!(
      super::admit(&git, &owner, &[git_change(main, Right::Submit)]),
      Ok(())
    );
    assert_eq!(
      super::admit(&git, &owner, &[git_change(main, Right::Rewrite)]),
      Err(super::PushRefusal::Denied { index: 0, rule: 1 })
    );
    let feature = "refs/heads/feature/x";
    assert_eq!(
      super::admit(&git, &stranger, &[git_change(feature, Right::Submit)]),
      Ok(())
    );
    assert_eq!(
      super::admit(&git, &stranger, &[git_change(main, Right::Submit)]),
      Err(super::PushRefusal::NotPermitted { index: 0 })
    );
    assert_eq!(
      super::admit(
        &git,
        &owner,
        &[
          git_change(feature, Right::Submit),
          git_change(main, Right::Rewrite)
        ]
      ),
      Err(super::PushRefusal::Denied { index: 1, rule: 1 })
    );
    let other_tool = super::Asked {
      name: super::Name::Hg(Namespace::Branch, "default"),
      right: Right::Submit,
      brings: None,
    };
    assert_eq!(
      super::admit(&git, &owner, &[other_tool]),
      Err(super::PushRefusal::OtherTool { index: 0 })
    );
  }

  /// A-622 part 2 (the check point, Mercurial and Subversion). Do: in a Mercurial repository whose rule lets one
  /// stranger submit to branch `stable`, ask for the stranger's, a third principal's and the owner's push to `stable`,
  /// and the owner's and the stranger's push to a bookmark no rule names; in a Subversion repository letting everyone
  /// submit under `/trunk` and denying everyone `:glob:/tags/**`, ask for commits there and beside them. Expect: on
  /// `stable` the stranger admitted and the others refused `NotListed`, the owner too (acl's allow list binds everyone);
  /// the bookmark the owner's alone; the trunk open to everyone, the tags closed to the owner by rule 1, a path neither
  /// covers the owner's alone.
  #[test]
  fn a_push_is_judged_by_acls_order_and_authzs_depth_at_the_check_point() {
    use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight, PolicyWho, VcsTool};
    let hg = record(
      VcsTool::Mercurial,
      vec![record_rule(
        PolicyNames::Branches,
        "stable",
        (PolicyRight::Submit, PolicyEffect::Allow),
        record_only(&[STRANGER]),
      )],
    );
    let push = |namespace, name, uid: u32| {
      super::admit(
        &hg,
        &principal(uid),
        &[super::Asked {
          name: super::Name::Hg(namespace, name),
          right: Right::Submit,
          brings: None,
        }],
      )
    };
    let not_listed = Err(super::PushRefusal::NotListed { index: 0 });
    assert_eq!(push(Namespace::Branch, "stable", STRANGER), Ok(()));
    assert_eq!(push(Namespace::Branch, "stable", OTHER), not_listed);
    assert_eq!(push(Namespace::Branch, "stable", OWNER), not_listed);
    assert_eq!(push(Namespace::Bookmark, "feature", OWNER), Ok(()));
    assert_eq!(
      push(Namespace::Bookmark, "feature", STRANGER),
      Err(super::PushRefusal::NotPermitted { index: 0 })
    );
    let svn = record(
      VcsTool::Subversion,
      vec![
        record_rule(
          PolicyNames::Paths,
          "/trunk",
          (PolicyRight::Submit, PolicyEffect::Allow),
          PolicyWho::Anyone,
        ),
        record_rule(
          PolicyNames::Paths,
          ":glob:/tags/**",
          (PolicyRight::Submit, PolicyEffect::Deny),
          PolicyWho::Anyone,
        ),
      ],
    );
    let commit = |path: &str, uid: u32| {
      super::admit(
        &svn,
        &principal(uid),
        &[super::Asked {
          name: super::Name::Path(path.as_bytes()),
          right: Right::Submit,
          brings: None,
        }],
      )
    };
    assert_eq!(commit("/trunk/src/main.c", STRANGER), Ok(()));
    assert_eq!(
      commit("/tags/1.0/main.c", OWNER),
      Err(super::PushRefusal::Denied { index: 0, rule: 1 })
    );
    assert_eq!(commit("/branches/b/main.c", OWNER), Ok(()));
    assert_eq!(
      commit("/branches/b/main.c", STRANGER),
      Err(super::PushRefusal::NotPermitted { index: 0 })
    );
  }

  /// A-622 part 2. Do: judge a push against a git repository's record holding a branch rule, which its guard would have
  /// refused (a damaged record). Expect: refused `Unreadable` naming the rule, never judged without it.
  #[test]
  fn a_push_against_a_policy_its_matcher_cannot_read_is_refused() {
    use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight, PolicyWho, VcsTool};
    let damaged = record(
      VcsTool::Git,
      vec![record_rule(
        PolicyNames::Branches,
        "main",
        (PolicyRight::Submit, PolicyEffect::Deny),
        PolicyWho::Anyone,
      )],
    );
    assert_eq!(
      super::admit(
        &damaged,
        &principal(OWNER),
        &[git_change("refs/heads/main", Right::Submit)]
      ),
      Err(super::PushRefusal::Unreadable { rule: 0 })
    );
  }

  /// A-625 part 2 (the edge's gate). Do: with rules letting one stranger read `refs/heads/feature/*` and denying everyone
  /// the rewrite of `refs/heads/main`, ask whether the owner, that stranger and a principal no rule names may reach the
  /// git repository to read and to push; then ask of a damaged record. Expect: the owner reaches it for every right; the
  /// stranger to read alone (a rule allows it that), not to push (no rule allows it a push's right, and a deny admits no
  /// one); the unnamed principal for neither; the damaged record refused `Unreadable`, never judged without its rule.
  #[test]
  fn an_edge_admits_a_principal_some_rule_lets_reach_the_repository() {
    use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight, PolicyWho, VcsTool};
    let git = record(
      VcsTool::Git,
      vec![
        record_rule(
          PolicyNames::Refs,
          "refs/heads/feature/*",
          (PolicyRight::Read, PolicyEffect::Allow),
          record_only(&[STRANGER]),
        ),
        record_rule(
          PolicyNames::Refs,
          "refs/heads/main",
          (PolicyRight::Rewrite, PolicyEffect::Deny),
          PolicyWho::Anyone,
        ),
      ],
    );
    let (read, push) = (
      &[Right::Read][..],
      &[Right::Submit, Right::Rewrite, Right::Delete][..],
    );
    let reach = |uid: u32| {
      (
        super::reaches(&git, &principal(uid), read),
        super::reaches(&git, &principal(uid), push),
      )
    };
    assert_eq!(reach(OWNER), (Ok(true), Ok(true)));
    assert_eq!(reach(STRANGER), (Ok(true), Ok(false)));
    assert_eq!(reach(UNNAMED), (Ok(false), Ok(false)));
    let damaged = record(
      VcsTool::Git,
      vec![record_rule(
        PolicyNames::Branches,
        "main",
        (PolicyRight::Read, PolicyEffect::Allow),
        PolicyWho::Anyone,
      )],
    );
    assert_eq!(
      super::reaches(&damaged, &principal(OWNER), read),
      Err(super::PushRefusal::Unreadable { rule: 0 })
    );
  }

  /// What a test change brings: a merge commit or none, an unsigned commit or none, or a store's refusal; each
  /// question counted (merge commits, signatures) and the signers handed kept, in SSH wire form.
  struct Bringing {
    merge: Option<super::ObjectId>,
    unsigned: Option<(super::ObjectId, super::Unsigned<super::SignatureRefusal>)>,
    refused: Option<super::StoreRefused>,
    asked: std::cell::Cell<(u32, u32)>,
    signers: std::cell::RefCell<Vec<Vec<u8>>>,
  }

  impl Bringing {
    /// A change bringing `merge` and `unsigned`.
    fn of(
      merge: Option<super::ObjectId>,
      unsigned: Option<(super::ObjectId, super::Unsigned<super::SignatureRefusal>)>,
    ) -> Bringing {
      Bringing {
        merge,
        unsigned,
        refused: None,
        asked: std::cell::Cell::new((0, 0)),
        signers: std::cell::RefCell::new(Vec::new()),
      }
    }
  }

  impl super::Brings for Bringing {
    fn merge_commit(&self) -> Result<Option<super::ObjectId>, super::StoreRefused> {
      let (merges, signatures) = self.asked.get();
      self.asked.set((merges + 1, signatures));
      self.refused.map_or(Ok(self.merge), Err)
    }

    fn unsigned_commit(
      &self,
      signers: &[super::TrustedKey],
    ) -> Result<
      Option<(super::ObjectId, super::Unsigned<super::SignatureRefusal>)>,
      super::StoreRefused,
    > {
      let (merges, signatures) = self.asked.get();
      self.asked.set((merges, signatures + 1));
      let wire = signers.iter().map(|key| key.wire().to_vec()).collect();
      self.signers.replace(wire);
      self.refused.map_or(Ok(self.unsigned), Err)
    }
  }

  /// A commit's id, `fill` in every byte.
  fn commit_id(fill: u8) -> super::ObjectId {
    let hex = format!("{fill:02x}").repeat(20);
    super::ObjectId::from_hex(slates_git::object_name::ObjectFormat::Sha1, hex.as_bytes()).unwrap()
  }

  /// An `ssh-ed25519` public key in SSH wire form, `fill` in every byte of its key.
  fn signer_key(fill: u8) -> Vec<u8> {
    let mut blob = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
    blob.extend_from_slice(&[fill; 32]);
    blob
  }

  /// A git change of `name` needing `right`, bringing `brings`.
  fn bringing<'a>(name: &'a str, right: Right, brings: &'a Bringing) -> super::Asked<'a> {
    super::Asked {
      name: super::Name::Ref(name.as_bytes()),
      right,
      brings: Some(brings),
    }
  }

  /// A rule over git ref pattern `pattern`.
  fn git_rule(
    pattern: &str,
    (right, effect): (
      slates_db::catalog::PolicyRight,
      slates_db::catalog::PolicyEffect,
    ),
    who: slates_db::catalog::PolicyWho,
  ) -> slates_db::catalog::PolicyRule {
    record_rule(
      slates_db::catalog::PolicyNames::Refs,
      pattern,
      (right, effect),
      who,
    )
  }

  /// One push's verdict on `asked` by `uid`, and how often its one change's facts were asked.
  fn judged(
    record: &super::RepositoryRecord,
    uid: u32,
    (name, right, brings): (&str, Right, &Bringing),
  ) -> (Result<(), super::PushRefusal>, (u32, u32)) {
    let verdict = super::admit(record, &principal(uid), &[bringing(name, right, brings)]);
    (verdict, brings.asked.get())
  }

  /// A-644 (§4.17.2 "Protected branches": a linear history, a requirement). Do: in a git repository whose rules let a
  /// stranger submit under `refs/heads/`, deny everyone merge commits on `main`, lift that for the stranger in a later
  /// rule, and deny everyone `main`'s rewrite, push a merge commit to `main` as the owner and as the stranger, to a
  /// feature branch, and as a rewrite of `main`. Expect: the owner refused `MergeCommit` naming rule 1 and the commit,
  /// the change's facts asked once; the stranger admitted by rule 2 and the feature branch admitted, neither asked; the
  /// rewrite refused by its own right's rule 3 before any requirement is asked.
  #[test]
  fn a_requirement_binds_only_where_a_rule_imposes_it() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let git = record(
      VcsTool::Git,
      vec![
        git_rule(
          "refs/heads/",
          (PolicyRight::Submit, PolicyEffect::Allow),
          record_only(&[STRANGER]),
        ),
        git_rule(
          "refs/heads/main",
          (PolicyRight::MergeCommit, PolicyEffect::Deny),
          PolicyWho::Anyone,
        ),
        git_rule(
          "refs/heads/main",
          (PolicyRight::MergeCommit, PolicyEffect::Allow),
          record_only(&[STRANGER]),
        ),
        git_rule(
          "refs/heads/main",
          (PolicyRight::Rewrite, PolicyEffect::Deny),
          PolicyWho::Anyone,
        ),
      ],
    );
    let merge = commit_id(7);
    let main = "refs/heads/main";
    let cases = [
      (OWNER, main, Right::Submit),
      (STRANGER, main, Right::Submit),
      (OWNER, "refs/heads/feature", Right::Submit),
      (OWNER, main, Right::Rewrite),
    ];
    let answers: Vec<_> = cases
      .iter()
      .map(|(uid, name, right)| {
        judged(
          &git,
          *uid,
          (*name, *right, &Bringing::of(Some(merge), None)),
        )
      })
      .collect();
    let refused = super::PushRefusal::MergeCommit {
      index: 0,
      rule: 1,
      commit: merge,
    };
    let rewrite = super::PushRefusal::Denied { index: 0, rule: 3 };
    let expected = vec![
      (Err(refused), (1, 0)),
      (Ok(()), (0, 0)),
      (Ok(()), (0, 0)),
      (Err(rewrite), (0, 0)),
    ];
    assert_eq!(answers, expected);
  }

  /// A-644 (signed commits, a requirement judged under the record's signers). Do: in a git repository whose rule denies
  /// everyone unsigned commits on `main` and whose policy trusts two keys, push to `main` a change bringing a commit
  /// another key signed, one bringing none unsigned, and a push of a feature branch then `main`. Expect: the first
  /// refused `UnsignedCommit` with the commit and why, the verifier handed exactly the record's two keys; the second
  /// admitted; the two-change push refused at its second change, its first never asked.
  #[test]
  fn signed_commits_are_judged_under_the_records_signers() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let mut git = record(
      VcsTool::Git,
      vec![git_rule(
        "refs/heads/main",
        (PolicyRight::UnsignedCommit, PolicyEffect::Deny),
        PolicyWho::Anyone,
      )],
    );
    git.policy.signers = vec![signer_key(1), signer_key(2)];
    let commit = commit_id(9);
    let why = super::Unsigned::Ssh(super::SignatureRefusal::UnregisteredKey);
    let unsigned = Bringing::of(None, Some((commit, why)));
    let main = "refs/heads/main";
    let refused = super::PushRefusal::UnsignedCommit {
      index: 0,
      rule: 0,
      commit,
      why,
    };
    assert_eq!(
      judged(&git, OWNER, (main, Right::Submit, &unsigned)),
      (Err(refused), (0, 1))
    );
    assert_eq!(unsigned.signers.take(), vec![signer_key(1), signer_key(2)]);
    let signed = Bringing::of(None, None);
    assert_eq!(
      judged(&git, OWNER, (main, Right::Submit, &signed)),
      (Ok(()), (0, 1))
    );
    let (feature, again) = (
      Bringing::of(None, None),
      Bringing::of(None, Some((commit, why))),
    );
    let push = [
      bringing("refs/heads/feature", Right::Submit, &feature),
      bringing(main, Right::Submit, &again),
    ];
    let second = super::PushRefusal::UnsignedCommit {
      index: 1,
      rule: 0,
      commit,
      why,
    };
    assert_eq!(super::admit(&git, &principal(OWNER), &push), Err(second));
    assert_eq!((feature.asked.get(), again.asked.get()), ((0, 0), (0, 1)));
  }

  /// A-644 (the design's order: a linear history, then signatures; a damaged store never judged around). Do: with both
  /// requirements imposed on `main`, push a change bringing a merge commit and an unsigned one; then one whose store
  /// refuses. Expect: `MergeCommit`, the signatures never asked; then `Unjudged` with the store's refusal.
  #[test]
  fn a_linear_history_is_judged_before_signatures_and_a_store_refusal_is_typed() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let deny = |right| {
      git_rule(
        "refs/heads/main",
        (right, PolicyEffect::Deny),
        PolicyWho::Anyone,
      )
    };
    let git = record(
      VcsTool::Git,
      vec![
        deny(PolicyRight::UnsignedCommit),
        deny(PolicyRight::MergeCommit),
      ],
    );
    let (merge, other) = (commit_id(3), commit_id(4));
    let both = Bringing::of(Some(merge), Some((other, super::Unsigned::Missing)));
    let main = "refs/heads/main";
    let refused = super::PushRefusal::MergeCommit {
      index: 0,
      rule: 1,
      commit: merge,
    };
    assert_eq!(
      judged(&git, OWNER, (main, Right::Submit, &both)),
      (Err(refused), (1, 0))
    );
    let broken = Bringing {
      refused: Some(super::StoreRefused::Unsealed),
      ..Bringing::of(None, None)
    };
    let unjudged = super::PushRefusal::Unjudged {
      index: 0,
      refused: super::StoreRefused::Unsealed,
    };
    assert_eq!(
      judged(&git, OWNER, (main, Right::Submit, &broken)),
      (Err(unjudged), (1, 0))
    );
  }

  fn svn_path_rule(
    pattern: &str,
    right: slates_db::catalog::PolicyRight,
    effect: slates_db::catalog::PolicyEffect,
    who: slates_db::catalog::PolicyWho,
  ) -> slates_db::catalog::PolicyRule {
    record_rule(
      slates_db::catalog::PolicyNames::Paths,
      pattern,
      (right, effect),
      who,
    )
  }

  fn assert_svn_access_core_cases() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let (owner, stranger, other) = (principal(OWNER), principal(STRANGER), principal(UNNAMED));
    let empty = record(VcsTool::Subversion, vec![]);
    let deny_read = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Deny,
          record_only(&[STRANGER]),
        ),
      ],
    );
    let deny_submit = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Deny,
          record_only(&[STRANGER]),
        ),
      ],
    );
    let submit_only = record(
      VcsTool::Subversion,
      vec![svn_path_rule(
        "/",
        PolicyRight::Submit,
        PolicyEffect::Allow,
        record_only(&[STRANGER]),
      )],
    );
    let owner_deny_submit = record(
      VcsTool::Subversion,
      vec![svn_path_rule(
        "/",
        PolicyRight::Submit,
        PolicyEffect::Deny,
        record_only(&[OWNER]),
      )],
    );
    let sub_allow = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/trunk",
          PolicyRight::Read,
          PolicyEffect::Allow,
          record_only(&[STRANGER]),
        ),
      ],
    );
    assert_eq!(
      [
        super::svn_access(&empty, &owner),
        super::svn_access(&empty, &stranger),
        super::svn_access(&deny_read, &stranger),
        super::svn_access(&deny_read, &other),
        super::svn_access(&deny_submit, &stranger),
        super::svn_access(&deny_submit, &other),
        super::svn_access(&submit_only, &stranger),
        super::svn_access(&owner_deny_submit, &owner),
        super::svn_access(&sub_allow, &stranger),
        super::svn_access(&sub_allow, &other),
      ],
      [
        Ok(super::SvnAccess::Write),
        Ok(super::SvnAccess::None),
        Ok(super::SvnAccess::None),
        Ok(super::SvnAccess::Read),
        Ok(super::SvnAccess::Read),
        Ok(super::SvnAccess::Write),
        Ok(super::SvnAccess::None),
        Ok(super::SvnAccess::Read),
        Ok(super::SvnAccess::Read),
        Ok(super::SvnAccess::None),
      ]
    );
  }

  fn assert_svn_access_glob_and_damaged_cases() {
    use slates_db::catalog::{PolicyEffect, PolicyNames, PolicyRight, PolicyWho, VcsTool};
    let (owner, stranger) = (principal(OWNER), principal(STRANGER));
    let glob_survives = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          ":glob:/trunk/*",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/trunk/a",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    let double_star_survives = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          ":glob:/trunk/**",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/trunk",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    let double_star_eclipsed = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          ":glob:/trunk/**",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          ":glob:/trunk/**",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    let bracket_allow = record(
      VcsTool::Subversion,
      vec![svn_path_rule(
        ":glob:/[[:alpha:]]/file?",
        PolicyRight::Read,
        PolicyEffect::Allow,
        record_only(&[STRANGER]),
      )],
    );
    let damaged = record(
      VcsTool::Subversion,
      vec![record_rule(
        PolicyNames::Branches,
        "main",
        (PolicyRight::Read, PolicyEffect::Allow),
        PolicyWho::Anyone,
      )],
    );
    assert_eq!(
      [
        super::svn_access(&glob_survives, &stranger),
        super::svn_access(&double_star_survives, &stranger),
        super::svn_access(&double_star_eclipsed, &stranger),
        super::svn_access(&bracket_allow, &stranger),
        super::svn_access(&damaged, &owner),
      ],
      [
        Ok(super::SvnAccess::Read),
        Ok(super::SvnAccess::Read),
        Ok(super::SvnAccess::None),
        Ok(super::SvnAccess::Read),
        Err(super::PushRefusal::Unreadable { rule: 0 }),
      ]
    );
  }

  fn assert_svn_commit_change_admission() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    use slates_svn::index::{Action, ChangedPath, CopySource, NodeKind};
    let (owner, stranger) = (principal(OWNER), principal(STRANGER));
    let commit_policy = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/tags",
          PolicyRight::Submit,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/secret",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/trunk",
          PolicyRight::Delete,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    let change = |path: &str, action: Action, copy_from: Option<&str>| ChangedPath {
      path: path.to_owned(),
      action,
      kind: NodeKind::File,
      copy_from: copy_from.map(|src| CopySource {
        path: src.to_owned(),
        revision: 1,
      }),
      text: None,
      properties: None,
      mergeinfo_modified: false,
    };
    let empty = slates_svn::index::Index::new();
    let admitted = |c: ChangedPath| {
      super::admit_svn_changes(&owner, &commit_policy.policy, &stranger, (&empty, 0), &[c]).is_ok()
    };
    assert_eq!(
      [
        admitted(change("/trunk/ok.txt", Action::Add, None)),
        admitted(change("/tags/v1", Action::Add, None)),
        admitted(change(
          "/trunk/leak.txt",
          Action::Add,
          Some("/secret/key.txt")
        )),
        admitted(change("/trunk/old.txt", Action::Delete, None)),
      ],
      [true, false, false, false]
    );
  }

  /// Round-4 Blocker 2 (`svn_access`, `svn_path_read_allowed`, `svn_path_write_allowed`,
  /// `admit_svn_changes`): Do evaluate root `Deny Read`/`Deny Submit`, owner `Deny Submit`,
  /// `Submit`-only, subpath `Allow` under root `Deny`, glob `Allow` vs literal/glob `Deny`,
  /// damaged records, and per-change commit admission (including `copy_from` read check and
  /// `Action::Delete`). Expect: exact `SvnPolicy` precedence everywhere, write requires read,
  /// and damaged policies fail closed with `Unreadable`.
  #[test]
  fn round4_blocker2_svn_access_and_per_path_policy_honor_deny_globs_and_submit_only() {
    assert_svn_access_core_cases();
    assert_svn_access_glob_and_damaged_cases();
    assert_svn_commit_change_admission();
  }

  fn assert_glob_policy_grants_read(
    label: &str,
    rules: Vec<slates_db::catalog::PolicyRule>,
    allowed_path: &[u8],
    denied_path: &[u8],
  ) {
    let rec = record(slates_db::catalog::VcsTool::Subversion, rules);
    let user = principal(STRANGER);
    assert_eq!(
      super::svn_access(&rec, &user),
      Ok(super::SvnAccess::Read),
      "{label}: svn_access must grant Read"
    );
    assert_eq!(
      super::svn_path_read_allowed(&rec, &user, allowed_path),
      Ok(true),
      "{label}: allowed path must be readable"
    );
    assert_eq!(
      super::svn_path_read_allowed(&rec, &user, denied_path),
      Ok(false),
      "{label}: denied path must be refused"
    );
  }

  /// Round-4 Blocker 1 (regression in `5713cee6d`): Do evaluate all 5 legitimate allow+deny glob
  /// policies (`*` vs `a*`, `[ab]` vs `/a`, `file?` vs `/filea`, `/x/**/y/**` vs `/x/**/y`,
  /// `foo*` vs `/foo` + `/fooa`) plus truly eclipsed and malformed globs. Expect: all 5 grant
  /// `SvnAccess::Read`, while eclipsed/malformed globs return `SvnAccess::None`.
  #[test]
  fn round4_blocker1_glob_witness_grants_five_legitimate_policies_and_blocks_eclipsed() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let allow =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone);
    let deny = |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Deny, PolicyWho::Anyone);

    assert_glob_policy_grants_read(
      "1: /trunk/* minus a*",
      vec![allow(":glob:/trunk/*"), deny(":glob:/trunk/a*")],
      b"/trunk/zebra.txt",
      b"/trunk/apple.txt",
    );
    assert_glob_policy_grants_read(
      "2: /[ab] minus /a",
      vec![allow(":glob:/[ab]"), deny("/a")],
      b"/b/ok.txt",
      b"/a/no.txt",
    );
    assert_glob_policy_grants_read(
      "3: /file? minus /filea",
      vec![allow(":glob:/file?"), deny("/filea")],
      b"/fileb",
      b"/filea",
    );
    assert_glob_policy_grants_read(
      "4: /x/**/y/** minus /x/**/y",
      vec![allow(":glob:/x/**/y/**"), deny(":glob:/x/**/y")],
      b"/x/mid/y/ok.txt",
      b"/x/mid/y",
    );
    assert_glob_policy_grants_read(
      "5: /trunk/foo* minus foo and fooa",
      vec![
        allow(":glob:/trunk/foo*"),
        deny("/trunk/foo"),
        deny("/trunk/fooa"),
      ],
      b"/trunk/foobar.txt",
      b"/trunk/fooa",
    );

    let eclipsed_cases = [
      vec![allow(":glob:/trunk/*"), deny(":glob:/trunk/*")],
      vec![allow("/a"), deny("/a")],
      vec![allow(":glob:/[unclosed")],
      vec![allow(":glob:/trailing\\")],
    ];
    for rules in eclipsed_cases {
      let rec = record(VcsTool::Subversion, rules);
      assert_eq!(
        super::svn_access(&rec, &principal(STRANGER)),
        Ok(super::SvnAccess::None)
      );
    }
  }

  /// Round-4 Blocker 4 (joint Read+Write path requirement in `svn_access`): Do evaluate disjoint
  /// `[Deny Read /, Allow Read /public, Deny Submit /, Allow Submit /private]` and overlapping
  /// nested/glob policies. Expect: disjoint returns `SvnAccess::Read` (never `Write`); overlapping
  /// returns `SvnAccess::Write`.
  #[test]
  fn round4_blocker4_svn_access_requires_common_read_and_write_path() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let stranger = principal(STRANGER);
    let disjoint = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/public",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/private",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
      ],
    );
    assert_eq!(
      super::svn_access(&disjoint, &stranger),
      Ok(super::SvnAccess::Read)
    );

    let nested = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/pub",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/pub/write",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
      ],
    );
    assert_eq!(
      super::svn_access(&nested, &stranger),
      Ok(super::SvnAccess::Write)
    );

    let glob_overlap = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          ":glob:/[ab]",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/a",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          ":glob:/[bc]",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/c",
          PolicyRight::Submit,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    assert_eq!(
      super::svn_access(&glob_overlap, &stranger),
      Ok(super::SvnAccess::Write)
    );
  }

  /// Round-4 Blocker 3 (red-first `Action::Replace`): Do submit an `Action::Replace` on `/trunk/f`
  /// under `Deny Delete /trunk` and under `Deny Submit /trunk` + `Allow Delete /trunk`. Expect:
  /// refused in BOTH cases because `Replace` requires both delete and write permission.
  #[test]
  fn round4_blocker3_action_replace_requires_both_delete_and_write() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    use slates_svn::index::{Action, ChangedPath, NodeKind};
    let (owner, stranger) = (principal(OWNER), principal(STRANGER));
    let deny_delete = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/trunk",
          PolicyRight::Delete,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    let delete_only = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/trunk",
          PolicyRight::Delete,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
      ],
    );
    let replace = ChangedPath {
      path: "/trunk/f".to_owned(),
      action: Action::Replace,
      kind: NodeKind::File,
      copy_from: None,
      text: None,
      properties: None,
      mergeinfo_modified: false,
    };
    let empty = slates_svn::index::Index::new();
    assert!(
      super::admit_svn_changes(
        &owner,
        &deny_delete.policy,
        &stranger,
        (&empty, 0),
        std::slice::from_ref(&replace)
      )
      .is_err(),
      "Action::Replace must be refused under Deny Delete /trunk"
    );
    assert!(
      super::admit_svn_changes(
        &owner,
        &delete_only.policy,
        &stranger,
        (&empty, 0),
        &[replace]
      )
      .is_err(),
      "Action::Replace must be refused when user only has Delete without Submit/Rewrite"
    );
  }

  fn build_subtree_index() -> slates_svn::index::Index {
    use slates_svn::index::{Action, ChangedPath, Index, NodeKind, RevisionRecord};
    let mut index = Index::new();
    let add = |path: &str, kind: NodeKind| ChangedPath {
      path: path.to_owned(),
      action: Action::Add,
      kind,
      copy_from: None,
      text: None,
      properties: None,
      mergeinfo_modified: false,
    };
    let rec = RevisionRecord {
      transaction: "0-1".to_owned(),
      changes: vec![
        add("/dir", NodeKind::Directory),
        add("/dir/ok.txt", NodeKind::File),
        add("/dir/protected", NodeKind::Directory),
        add("/dir/protected/keep.txt", NodeKind::File),
        add("/pub", NodeKind::Directory),
        add("/pub/ok.txt", NodeKind::File),
        add("/pub/secret", NodeKind::Directory),
        add("/pub/secret/vault.txt", NodeKind::File),
        add("/clean", NodeKind::Directory),
        add("/clean/item.txt", NodeKind::File),
      ],
    };
    index.append(rec, 1 << 10).unwrap();
    index
  }

  /// Round-4 Blocker 3 (directory Delete, Replace, and copy_from subtree checks): Do delete or
  /// replace `/dir` containing denied `/dir/protected/keep.txt`, copy `/pub` containing denied
  /// `/pub/secret/vault.txt` to `/exfil`, copy `/clean` into a destination with a denied child
  /// `/dst/item.txt`, and delete/copy `/clean` where all descendants are allowed. Expect: all
  /// subtree bypasses are refused while permitted subtree operations succeed.
  #[test]
  fn round4_blocker3_directory_delete_replace_and_copy_from_check_descendants() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    use slates_svn::index::{Action, ChangedPath, CopySource, NodeKind};
    let index = build_subtree_index();
    let (owner, stranger) = (principal(OWNER), principal(STRANGER));
    let pol = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/",
          PolicyRight::Submit,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/dir/protected",
          PolicyRight::Delete,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/pub/secret",
          PolicyRight::Read,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          "/dst/item.txt",
          PolicyRight::Submit,
          PolicyEffect::Deny,
          PolicyWho::Anyone,
        ),
      ],
    );
    let ch = |path: &str, action: Action, copy_from: Option<&str>| ChangedPath {
      path: path.to_owned(),
      action,
      kind: NodeKind::Directory,
      copy_from: copy_from.map(|src| CopySource {
        path: src.to_owned(),
        revision: 1,
      }),
      text: None,
      properties: None,
      mergeinfo_modified: false,
    };
    let check = |c: ChangedPath| {
      super::admit_svn_changes(&owner, &pol.policy, &stranger, (&index, 1), &[c]).is_ok()
    };
    assert!(
      !check(ch("/dir", Action::Delete, None)),
      "dir delete must check denied descendant"
    );
    assert!(
      !check(ch("/dir", Action::Replace, None)),
      "dir replace must check denied descendant"
    );
    assert!(
      !check(ch("/exfil", Action::Add, Some("/pub"))),
      "dir copy_from must check denied source descendant"
    );
    assert!(
      !check(ch("/dst", Action::Add, Some("/clean"))),
      "dir copy_from must check denied dest descendant"
    );
    assert!(
      check(ch("/clean", Action::Delete, None)),
      "allowed dir delete must succeed"
    );
    assert!(
      check(ch("/copied", Action::Add, Some("/clean"))),
      "allowed dir copy must succeed"
    );
  }

  /// Round-4 Blocker 2 (equivalence between `svn_read_rules` and `svn_path_read_allowed`): Do
  /// evaluate both on root, literals, subpaths, `Deny` under `Allow`, `Allow` under `Deny`, owner,
  /// and globs (`*`, `**`, `?`, `[ab]`, `[[:alpha:]]`, `\x`, unclosed `[`, trailing `\`). Expect:
  /// bit-exact agreement on every path.
  #[test]
  fn round4_blocker2_svn_read_rules_matches_svn_path_read_allowed() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let allow =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone);
    let deny = |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Deny, PolicyWho::Anyone);
    let rec = record(
      VcsTool::Subversion,
      vec![
        allow("/"),
        deny("/secret"),
        allow("/secret/public"),
        deny("/pub/secret"),
        deny(":glob:/trunk/a*"),
        deny(":glob:/pick/[ab]"),
        deny(":glob:/file?"),
        allow("/fileb"),
        deny(":glob:/x/**/vault"),
        allow(":glob:/x/**/vault/open"),
        deny(":glob:/cls/[[:digit:]]"),
        deny(":glob:/esc/\\*star"),
        deny(":glob:/bad/[unclosed"),
        deny(":glob:/bad/trail\\"),
      ],
    );
    let paths = [
      "/",
      "/pub/ok.txt",
      "/pub/secret",
      "/pub/secret/vault.txt",
      "/secret",
      "/secret/top.txt",
      "/secret/public/ok.txt",
      "/trunk/apple.txt",
      "/trunk/zebra.txt",
      "/pick/a/f",
      "/pick/c/f",
      "/filea",
      "/fileb",
      "/x/vault/k",
      "/x/m/n/vault/k",
      "/x/m/vault/open/ok",
      "/cls/7",
      "/cls/z",
      "/esc/*star",
      "/esc/xstar",
      "/bad/[unclosed",
      "/bad/trail\\",
    ];
    for user in [principal(OWNER), principal(STRANGER)] {
      let compiled = super::svn_read_rules(&rec, &user);
      for path in paths {
        let expected = super::svn_path_read_allowed(&rec, &user, path.as_bytes()).unwrap();
        let actual = slates_svn::authz::may_read(Some(&compiled), path);
        assert_eq!(actual, expected, "mismatch for {user:?} at {path}");
      }
    }
  }
}
