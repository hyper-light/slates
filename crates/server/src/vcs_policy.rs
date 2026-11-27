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

use std::collections::{HashMap, HashSet, VecDeque};

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Format: bit width of one `u64` word (`u64::BITS`) used by NFA bitsets.
const BITSET_WORD_BITS: usize = 64;

/// Format: `log2(64)` bits select the `u64` word index inside a bitset.
const BITSET_WORD_SHIFT: u32 = 6;

/// Format: low six bits (`0..64`) index the bit within one `u64` word of a bitset.
const BITSET_BIT_MASK: usize = 63;

/// Format: low six bits (`0..64`) index the bit within one `u64` word of a 256-bit byte set.
const BYTE_BIT_MASK: u8 = 63;

/// Format: bytes forbidden inside a Subversion path segment (NUL and `/`).
const EXCLUDED_SEGMENT_BYTES: &[u8] = &[0, b'/'];

/// Format: valid non-NUL, non-slash bytes in a Subversion path segment (`256 - 2 = 254`).
const VALID_SEGMENT_BYTES: usize = (1usize << u8::BITS) - EXCLUDED_SEGMENT_BYTES.len();

/// Derived: upper bound on NFA-Allow × DFA-Deny product states explored per witness query
/// (`VALID_SEGMENT_BYTES^2`), derived from the valid segment byte domain so realistic policies
/// (including zero-deny nondeterministic allow globs, which stay linear in pattern length) run to
/// exact completion while pathological PSPACE-hard multi-deny glob complementation stays bounded
/// and returns [`WitnessOutcome::BudgetExceeded`].
const WITNESS_STATE_BUDGET: usize = VALID_SEGMENT_BYTES * VALID_SEGMENT_BYTES;

/// Format: RFC 3629 §4 highest 1-byte ASCII scalar byte (`0x7F`).
const UTF8_ASCII_MAX: u8 = 0x7F;
/// Format: RFC 3629 §4 lowest UTF-8 continuation byte (`0x80`).
const UTF8_CONT_MIN: u8 = 0x80;
/// Format: RFC 3629 §4 highest second byte after `0xF4` (`0x8F`, caps scalars at `U+10FFFF`).
const UTF8_F4_SECOND_MAX: u8 = 0x8F;
/// Format: RFC 3629 §4 lowest second byte after `0xF0` (`0x90`, rejects overlong 4-byte forms).
const UTF8_F0_SECOND_MIN: u8 = 0x90;
/// Format: RFC 3629 §4 highest second byte after `0xED` (`0x9F`, rejects UTF-16 surrogates).
const UTF8_ED_SECOND_MAX: u8 = 0x9F;
/// Format: RFC 3629 §4 lowest second byte after `0xE0` (`0xA0`, rejects overlong 3-byte forms).
const UTF8_E0_SECOND_MIN: u8 = 0xA0;
/// Format: RFC 3629 §4 highest UTF-8 continuation byte (`0xBF`).
const UTF8_CONT_MAX: u8 = 0xBF;
/// Format: RFC 3629 §4 lowest valid 2-byte lead (`0xC2`, rejects overlong `0xC0..=0xC1`).
const UTF8_LEAD2_MIN: u8 = 0xC2;
/// Format: RFC 3629 §4 highest valid 2-byte lead (`0xDF`).
const UTF8_LEAD2_MAX: u8 = 0xDF;
/// Format: RFC 3629 §4 overlong-guarded 3-byte lead (`0xE0`).
const UTF8_LEAD3_E0: u8 = 0xE0;
/// Format: RFC 3629 §4 lowest unrestricted 3-byte lead below `0xED` (`0xE1`).
const UTF8_LEAD3_LO_MIN: u8 = 0xE1;
/// Format: RFC 3629 §4 highest unrestricted 3-byte lead below `0xED` (`0xEC`).
const UTF8_LEAD3_LO_MAX: u8 = 0xEC;
/// Format: RFC 3629 §4 surrogate-guarded 3-byte lead (`0xED`).
const UTF8_LEAD3_ED: u8 = 0xED;
/// Format: RFC 3629 §4 lowest unrestricted 3-byte lead above `0xED` (`0xEE`).
const UTF8_LEAD3_HI_MIN: u8 = 0xEE;
/// Format: RFC 3629 §4 highest valid 3-byte lead (`0xEF`).
const UTF8_LEAD3_HI_MAX: u8 = 0xEF;
/// Format: RFC 3629 §4 overlong-guarded 4-byte lead (`0xF0`).
const UTF8_LEAD4_F0: u8 = 0xF0;
/// Format: RFC 3629 §4 lowest unrestricted 4-byte lead (`0xF1`).
const UTF8_LEAD4_MID_MIN: u8 = 0xF1;
/// Format: RFC 3629 §4 highest unrestricted 4-byte lead (`0xF3`).
const UTF8_LEAD4_MID_MAX: u8 = 0xF3;
/// Format: RFC 3629 §4 max-scalar-guarded 4-byte lead (`0xF4`).
const UTF8_LEAD4_F4: u8 = 0xF4;

/// One segment of a Subversion path pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
  /// Exactly these bytes.
  Literal(Vec<u8>),
  /// Wildcards within one segment (`*foo*.bar`, `a?c`, `[ab]`, `\*`), compiled once by
  /// [`slates_vcs::wildmatch::Glob`].
  Glob(Glob),
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
      _ if text.bytes().any(is_glob_special) => Self::Glob(Glob::new(text.as_bytes())),
      _ => Self::Literal(text.as_bytes().to_vec()),
    }
  }

  /// Whether this takes the path segment `segment` (never asked of [`Segment::Any`], which takes runs).
  fn takes(&self, segment: &[u8]) -> bool {
    match self {
      Self::Literal(bytes) => bytes == segment,
      Self::Glob(glob) => glob.matches(segment),
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

/// Translates one [`slates_vcs::wildmatch::Token`] for a single slash-free path segment, returning
/// `None` for `Token::Dirs` (zero-byte epsilon on slash-free text).
fn translate_wildmatch_token(
  token: &slates_vcs::wildmatch::Token,
) -> Option<slates_svn::authz::GlobToken> {
  use slates_svn::authz::{BYTE_SET_WORDS, GlobToken};
  use slates_vcs::wildmatch::Token;
  Some(match token {
    Token::Byte(byte) => GlobToken::Byte(*byte),
    Token::One => GlobToken::AnyByte,
    Token::Set(set) => {
      let bits = *set.bits();
      if bits == [0; BYTE_SET_WORDS] {
        GlobToken::Never
      } else {
        GlobToken::ByteSet(bits)
      }
    }
    Token::Run | Token::RunAll => GlobToken::Run,
    Token::Dirs => return None,
    Token::Never => GlobToken::Never,
  })
}

/// Translates `glob`'s canonical compiled [`slates_vcs::wildmatch::Token`] stream into
/// [`slates_svn::authz::GlobToken`]s for a single slash-free Subversion path segment, guaranteeing
/// identical parsing between `SvnPolicy`, `PrincipalRules`, and witness emptiness checks.
fn glob_tokens(glob: &Glob) -> Vec<slates_svn::authz::GlobToken> {
  use slates_svn::authz::GlobToken;
  let mut tokens = Vec::with_capacity(glob.tokens().len());
  for token in glob.tokens() {
    let Some(translated) = translate_wildmatch_token(token) else {
      continue;
    };
    if translated == GlobToken::Never {
      return vec![GlobToken::Never];
    }
    if translated != GlobToken::Run || tokens.last() != Some(&GlobToken::Run) {
      tokens.push(translated);
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
    Segment::Glob(glob) => PolicySegment::Glob(glob_tokens(glob).into_boxed_slice()),
  }
}

/// Dynamic bitset for NFA position sets (no fixed token or segment count cap).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BitSet(Vec<u64>);

impl BitSet {
  /// An empty bitset holding `bits` positions (`0..bits`).
  fn empty(bits: usize) -> Self {
    Self(vec![0u64; bits.div_ceil(BITSET_WORD_BITS)])
  }

  /// Word index and single-bit mask for `bit`.
  fn place(bit: usize) -> (usize, u64) {
    let word = bit.wrapping_shr(BITSET_WORD_SHIFT);
    let shift = u32::try_from(bit & BITSET_BIT_MASK).unwrap_or(0);
    (word, 1u64.wrapping_shl(shift))
  }

  /// Marks `bit` as set.
  fn set(&mut self, bit: usize) {
    let (word, mask) = Self::place(bit);
    if let Some(slot) = self.0.get_mut(word) {
      *slot |= mask;
    }
  }

  /// Clears `bit`.
  fn clear(&mut self, bit: usize) {
    let (word, mask) = Self::place(bit);
    if let Some(slot) = self.0.get_mut(word) {
      *slot &= !mask;
    }
  }

  /// Whether `bit` is set.
  fn get(&self, bit: usize) -> bool {
    let (word, mask) = Self::place(bit);
    self.0.get(word).is_some_and(|slot| (*slot & mask) != 0)
  }
}

/// 256-bit byte membership table matching [`slates_svn::authz::GlobToken::ByteSet`].
type ByteSetWords = [u64; slates_svn::authz::BYTE_SET_WORDS];

/// Marks `byte` in a 256-bit byte membership table.
fn mark_byte(bits: &mut ByteSetWords, byte: u8) {
  let word = usize::from(byte.wrapping_shr(BITSET_WORD_SHIFT));
  let mask = 1u64.wrapping_shl(u32::from(byte & BYTE_BIT_MASK));
  if let Some(slot) = bits.get_mut(word) {
    *slot |= mask;
  }
}

/// Node in a deterministic trie matching all active literal segments at one path level.
#[derive(Default)]
struct TrieNode {
  edges: Vec<(u8, usize)>,
  literal_id: Option<usize>,
}

/// Deterministic trie over deduplicated active literal segments so literal matching contributes
/// one `Option<usize>` state instead of one NFA bitset per literal.
struct LiteralTrie {
  nodes: Vec<TrieNode>,
}

impl LiteralTrie {
  /// Builds a trie over `literals`, assigning each slice its index as `literal_id`.
  fn build(literals: &[&[u8]]) -> Self {
    let mut trie = Self {
      nodes: vec![TrieNode::default()],
    };
    for (lit_idx, &bytes) in literals.iter().enumerate() {
      trie.insert(bytes, lit_idx);
    }
    trie
  }

  /// Inserts one literal segment with identifier `lit_idx`.
  fn insert(&mut self, bytes: &[u8], lit_idx: usize) {
    let mut node = 0usize;
    for &byte in bytes {
      node = self.child(node, byte);
    }
    if let Some(slot) = self.nodes.get_mut(node) {
      slot.literal_id = Some(lit_idx);
    }
  }

  /// Returns or creates the child of `node` on `byte`.
  fn child(&mut self, node: usize, byte: u8) -> usize {
    if let Some(existing) = self.step(Some(node), byte) {
      return existing;
    }
    let next = self.nodes.len();
    self.nodes.push(TrieNode::default());
    if let Some(parent) = self.nodes.get_mut(node) {
      parent.edges.push((byte, next));
    }
    next
  }

  /// Advances `node` on `byte`, returning `None` once off the trie.
  fn step(&self, node: Option<usize>, byte: u8) -> Option<usize> {
    self
      .nodes
      .get(node?)?
      .edges
      .iter()
      .find(|&&(edge, _)| edge == byte)
      .map(|&(_, next)| next)
  }

  /// Literal index terminating at `node`, if any.
  fn literal_at(&self, node: Option<usize>) -> Option<usize> {
    self.nodes.get(node?)?.literal_id
  }
}

/// Exact RFC 3629 §4 byte equivalence classes across all 8 [`Utf8State`] transitions (bytes outside
/// these classes — `0x00`, `'/'`, `0xC0..=0xC1`, `0xF5..=0xFF` — have no valid UTF-8 transition).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Utf8ByteClass {
  /// Valid 1-byte ASCII (`0x01..=0x7F` excluding `'/'`).
  Ascii,
  /// Continuation `0x80..=0x8F` (valid after `0xF4` and `0xED`, invalid after `0xE0` and `0xF0`).
  Cont80to8F,
  /// Continuation `0x90..=0x9F` (valid after `0xF0` and `0xED`, invalid after `0xE0` and `0xF4`).
  Cont90to9F,
  /// Continuation `0xA0..=0xBF` (valid after `0xE0` and `0xF0`, invalid after `0xED` and `0xF4`).
  ContA0toBF,
  /// 2-byte lead (`0xC2..=0xDF`).
  Lead2,
  /// Overlong-guarded 3-byte lead `0xE0`.
  Lead3E0,
  /// Unrestricted 3-byte leads (`0xE1..=0xEC`, `0xEE..=0xEF`).
  Lead3Plain,
  /// Surrogate-guarded 3-byte lead `0xED`.
  Lead3ED,
  /// Overlong-guarded 4-byte lead `0xF0`.
  Lead4F0,
  /// Unrestricted 4-byte leads (`0xF1..=0xF3`).
  Lead4Plain,
  /// Max-scalar-guarded 4-byte lead `0xF4`.
  Lead4F4,
}

impl Utf8ByteClass {
  /// Classifies `byte` into its exact RFC 3629 §4 Subversion segment byte class, returning `None`
  /// for NUL, `'/'`, overlong leads `0xC0..=0xC1`, and out-of-range leads `0xF5..=0xFF`.
  fn of(byte: u8) -> Option<Self> {
    match byte {
      0 | b'/' => None,
      1..=UTF8_ASCII_MAX => Some(Self::Ascii),
      UTF8_CONT_MIN..=UTF8_F4_SECOND_MAX => Some(Self::Cont80to8F),
      UTF8_F0_SECOND_MIN..=UTF8_ED_SECOND_MAX => Some(Self::Cont90to9F),
      UTF8_E0_SECOND_MIN..=UTF8_CONT_MAX => Some(Self::ContA0toBF),
      UTF8_LEAD2_MIN..=UTF8_LEAD2_MAX => Some(Self::Lead2),
      UTF8_LEAD3_E0 => Some(Self::Lead3E0),
      UTF8_LEAD3_LO_MIN..=UTF8_LEAD3_LO_MAX | UTF8_LEAD3_HI_MIN..=UTF8_LEAD3_HI_MAX => {
        Some(Self::Lead3Plain)
      }
      UTF8_LEAD3_ED => Some(Self::Lead3ED),
      UTF8_LEAD4_F0 => Some(Self::Lead4F0),
      UTF8_LEAD4_MID_MIN..=UTF8_LEAD4_MID_MAX => Some(Self::Lead4Plain),
      UTF8_LEAD4_F4 => Some(Self::Lead4F4),
      _ => None,
    }
  }
}

/// Exact 8-state RFC 3629 §4 UTF-8 validator DFA for non-empty slash-free Subversion path segments;
/// accepts ONLY in [`Utf8State::Start`] after at least one byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Utf8State {
  /// At a UTF-8 code-point boundary (initial and ONLY accepting state).
  Start,
  /// Expecting 1 continuation byte (`0x80..=0xBF`) -> `Start`.
  Need1,
  /// After `0xE0`: expecting `0xA0..=0xBF` -> `Need1` (rejects overlong `< U+0800`).
  Need2FromE0,
  /// After `0xE1..=0xEC` or `0xEE..=0xEF`: expecting `0x80..=0xBF` -> `Need1`.
  Need2,
  /// After `0xED`: expecting `0x80..=0x9F` -> `Need1` (rejects UTF-16 surrogates `U+D800..U+DFFF`).
  Need2FromED,
  /// After `0xF0`: expecting `0x90..=0xBF` -> `Need2` (rejects overlong `< U+10000`).
  Need3FromF0,
  /// After `0xF1..=0xF3`: expecting `0x80..=0xBF` -> `Need2`.
  Need3,
  /// After `0xF4`: expecting `0x80..=0x8F` -> `Need2` (rejects `> U+10FFFF`).
  Need3FromF4,
}

impl Utf8State {
  /// Transition out of [`Utf8State::Start`] for classified byte `class`.
  fn step_start(class: Utf8ByteClass) -> Option<Self> {
    match class {
      Utf8ByteClass::Ascii => Some(Self::Start),
      Utf8ByteClass::Lead2 => Some(Self::Need1),
      Utf8ByteClass::Lead3E0 => Some(Self::Need2FromE0),
      Utf8ByteClass::Lead3Plain => Some(Self::Need2),
      Utf8ByteClass::Lead3ED => Some(Self::Need2FromED),
      Utf8ByteClass::Lead4F0 => Some(Self::Need3FromF0),
      Utf8ByteClass::Lead4Plain => Some(Self::Need3),
      Utf8ByteClass::Lead4F4 => Some(Self::Need3FromF4),
      Utf8ByteClass::Cont80to8F | Utf8ByteClass::Cont90to9F | Utf8ByteClass::ContA0toBF => None,
    }
  }

  /// Whether `class` is any RFC 3629 continuation byte (`0x80..=0xBF`).
  fn is_continuation(class: Utf8ByteClass) -> bool {
    matches!(
      class,
      Utf8ByteClass::Cont80to8F | Utf8ByteClass::Cont90to9F | Utf8ByteClass::ContA0toBF
    )
  }

  /// Advances the RFC 3629 §4 UTF-8 validator DFA by one byte, returning `None` on invalid UTF-8,
  /// NUL, or `'/'`.
  fn step(self, byte: u8) -> Option<Self> {
    let class = Utf8ByteClass::of(byte)?;
    match self {
      Self::Start => Self::step_start(class),
      Self::Need1 => Self::is_continuation(class).then_some(Self::Start),
      Self::Need2FromE0 => (class == Utf8ByteClass::ContA0toBF).then_some(Self::Need1),
      Self::Need2 => Self::is_continuation(class).then_some(Self::Need1),
      Self::Need2FromED => matches!(class, Utf8ByteClass::Cont80to8F | Utf8ByteClass::Cont90to9F)
        .then_some(Self::Need1),
      Self::Need3FromF0 => matches!(class, Utf8ByteClass::Cont90to9F | Utf8ByteClass::ContA0toBF)
        .then_some(Self::Need2),
      Self::Need3 => Self::is_continuation(class).then_some(Self::Need2),
      Self::Need3FromF4 => (class == Utf8ByteClass::Cont80to8F).then_some(Self::Need2),
    }
  }
}

/// Whether `bytes` is a non-empty valid RFC 3629 UTF-8 Subversion path segment (no NUL or `'/'`).
fn is_valid_utf8_segment(bytes: &[u8]) -> bool {
  !bytes.is_empty()
    && bytes
      .iter()
      .try_fold(Utf8State::Start, |state, &byte| state.step(byte))
      == Some(Utf8State::Start)
}

/// Outcome of an exact Subversion witness-emptiness query over valid UTF-8 canonical paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WitnessOutcome {
  /// At least one valid UTF-8 canonical path satisfies every queried constraint.
  Found,
  /// Exhaustive search proved zero valid UTF-8 canonical paths satisfy the constraints.
  Empty,
  /// State exploration hit [`WITNESS_STATE_BUDGET`] before finding a witness.
  BudgetExceeded,
}

impl WitnessOutcome {
  /// Merges two outcomes so [`WitnessOutcome::Found`] short-circuits and
  /// [`WitnessOutcome::BudgetExceeded`] is never silently collapsed into [`WitnessOutcome::Empty`].
  fn merge(self, other: Self) -> Self {
    match (self, other) {
      (Self::Found, _) | (_, Self::Found) => Self::Found,
      (Self::BudgetExceeded, _) | (_, Self::BudgetExceeded) => Self::BudgetExceeded,
      (Self::Empty, Self::Empty) => Self::Empty,
    }
  }
}

/// Positive segment constraint for one right along a path step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PosSegReq<'a> {
  /// Unconstrained (`*` or inherited): matches any non-empty valid UTF-8 segment.
  Any,
  /// Must equal this exact valid UTF-8 byte slice.
  Literal(&'a [u8]),
  /// Must match this compiled segment glob (explored nondeterministically).
  Glob(&'a [slates_svn::authz::GlobToken]),
}

/// Byte predicates collected from active positive globs and active `Deny` literals/globs.
struct BytePredicates {
  /// Singleton byte predicates (literal bytes and `GlobToken::Byte`).
  singletons: ByteSetWords,
  /// Distinct `ByteSet` predicates appearing in participating globs.
  byte_sets: Vec<ByteSetWords>,
}

/// Records `token`'s byte predicates into `singletons` and `byte_sets`.
fn record_glob_token_predicates(
  token: slates_svn::authz::GlobToken,
  singletons: &mut ByteSetWords,
  byte_sets: &mut Vec<ByteSetWords>,
) {
  match token {
    slates_svn::authz::GlobToken::Byte(byte) => mark_byte(singletons, byte),
    slates_svn::authz::GlobToken::ByteSet(bits) if !byte_sets.contains(&bits) => {
      byte_sets.push(bits);
    }
    _ => {}
  }
}

/// Collects singleton predicate bytes and unique `ByteSet` predicates appearing across positive
/// segment requirements `reqs`, active `Deny` `literals`, and active `Deny` `globs`.
fn collect_byte_predicates(
  reqs: [PosSegReq<'_>; 2],
  literals: &[&[u8]],
  globs: &[&[slates_svn::authz::GlobToken]],
) -> BytePredicates {
  let mut singletons = [0u64; slates_svn::authz::BYTE_SET_WORDS];
  for &byte in literals.iter().copied().flatten() {
    mark_byte(&mut singletons, byte);
  }
  let mut byte_sets = Vec::new();
  for req in reqs {
    if let PosSegReq::Glob(tokens) = req {
      for &token in tokens {
        record_glob_token_predicates(token, &mut singletons, &mut byte_sets);
      }
    }
  }
  for &token in globs.iter().copied().flatten() {
    record_glob_token_predicates(token, &mut singletons, &mut byte_sets);
  }
  BytePredicates {
    singletons,
    byte_sets,
  }
}

/// Bitset recording which `byte_sets` contain `byte`.
fn byte_set_signature(byte_sets: &[ByteSetWords], byte: u8) -> BitSet {
  let mut sig = BitSet::empty(byte_sets.len());
  for (idx, bits) in byte_sets.iter().enumerate() {
    if slates_svn::authz::byte_set_contains(bits, byte) {
      sig.set(idx);
    }
  }
  sig
}

/// Exact byte minterm equivalence-class representatives across all valid RFC 3629 §4 Subversion
/// segment bytes, partitioning by BOTH [`Utf8ByteClass`] and active glob predicates, and placing
/// non-singleton classes before singletons so witness searches escape literal/prefix denies first.
fn minterm_representatives(
  reqs: [PosSegReq<'_>; 2],
  literals: &[&[u8]],
  globs: &[&[slates_svn::authz::GlobToken]],
) -> Vec<u8> {
  let BytePredicates {
    singletons,
    byte_sets,
  } = collect_byte_predicates(reqs, literals, globs);
  let valid_bytes = (b'a'..=b'z')
    .chain(1u8..b'a')
    .chain(b'z'.saturating_add(1)..=u8::MAX);
  let mut non_singletons = Vec::new();
  let mut singleton_reps = Vec::new();
  let mut seen_sigs: HashSet<(Utf8ByteClass, BitSet)> = HashSet::new();
  for byte in valid_bytes {
    let Some(utf8_class) = Utf8ByteClass::of(byte) else {
      continue;
    };
    if slates_svn::authz::byte_set_contains(&singletons, byte) {
      singleton_reps.push(byte);
    } else if seen_sigs.insert((utf8_class, byte_set_signature(&byte_sets, byte))) {
      non_singletons.push(byte);
    }
  }
  non_singletons.extend(singleton_reps);
  non_singletons
}

/// Closes `mask` across `GlobToken::Run` epsilon transitions.
fn glob_close_runs(tokens: &[slates_svn::authz::GlobToken], mask: &mut BitSet) {
  for (pos, &token) in tokens.iter().enumerate() {
    if token == slates_svn::authz::GlobToken::Run && mask.get(pos) {
      mask.set(pos.saturating_add(1));
    }
  }
}

/// Initial NFA state bitset for `tokens` before consuming any byte.
fn glob_start_mask(tokens: &[slates_svn::authz::GlobToken]) -> BitSet {
  let mut mask = BitSet::empty(tokens.len().saturating_add(1));
  mask.set(0);
  glob_close_runs(tokens, &mut mask);
  mask
}

/// Whether single-byte token `token` consumes `byte` (valid segment byte != `'/'`).
fn glob_token_accepts_byte(token: slates_svn::authz::GlobToken, byte: u8) -> bool {
  use slates_svn::authz::{GlobToken, byte_set_contains};
  match token {
    GlobToken::Byte(expected) => expected == byte,
    GlobToken::AnyByte => byte != b'/',
    GlobToken::ByteSet(bits) => byte_set_contains(&bits, byte),
    GlobToken::Run | GlobToken::Never => false,
  }
}

/// Advances glob NFA `mask` by one valid segment `byte`.
fn glob_step_mask(tokens: &[slates_svn::authz::GlobToken], mask: &BitSet, byte: u8) -> BitSet {
  let mut next = BitSet::empty(tokens.len().saturating_add(1));
  for (pos, &token) in tokens.iter().enumerate() {
    if !mask.get(pos) {
      continue;
    }
    if token == slates_svn::authz::GlobToken::Run {
      next.set(pos);
    } else if glob_token_accepts_byte(token, byte) {
      next.set(pos.saturating_add(1));
    }
  }
  glob_close_runs(tokens, &mut next);
  next
}

/// Evaluates compiled segment glob `tokens` against concrete segment `bytes`.
fn glob_tokens_match_bytes(tokens: &[slates_svn::authz::GlobToken], bytes: &[u8]) -> bool {
  let mask = bytes
    .iter()
    .fold(glob_start_mask(tokens), |current, &byte| {
      glob_step_mask(tokens, &current, byte)
    });
  mask.get(tokens.len())
}

/// Nondeterministic single-token NFA position for one positive segment requirement (`Any` or
/// `Glob`). Positive globs are NEVER powerset-determinized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PosSegState {
  /// Unconstrained (`*` or inherited): accepts every non-empty valid UTF-8 segment.
  Any,
  /// Single token index `0..=tokens.len()` inside a positive segment glob.
  Glob(usize),
}

/// Whether token position `pos` in collapsed `tokens` (no adjacent `Run`) can accept with zero
/// further bytes.
fn pos_glob_accepts(tokens: &[slates_svn::authz::GlobToken], pos: usize) -> bool {
  pos == tokens.len()
    || (pos.saturating_add(1) == tokens.len()
      && tokens.get(pos) == Some(&slates_svn::authz::GlobToken::Run))
}

/// Nondeterministic single-byte step out of `pos` in collapsed `tokens` (at most 2 next positions,
/// advancing past `Run` first so shortest witnesses are explored first).
fn step_pos_glob(
  tokens: &[slates_svn::authz::GlobToken],
  pos: usize,
  byte: u8,
) -> [Option<usize>; 2] {
  let Some(&token) = tokens.get(pos) else {
    return [None, None];
  };
  if token == slates_svn::authz::GlobToken::Run {
    let skip = tokens
      .get(pos.saturating_add(1))
      .is_some_and(|&next| glob_token_accepts_byte(next, byte))
      .then(|| pos.saturating_add(2));
    return [skip, Some(pos)];
  }
  let advance = glob_token_accepts_byte(token, byte).then(|| pos.saturating_add(1));
  [advance, None]
}

impl PosSegState {
  /// Initial NFA position for positive requirement `req` (`Literal` is handled before BFS).
  fn initial(req: PosSegReq<'_>) -> Self {
    match req {
      PosSegReq::Glob(_) => Self::Glob(0),
      PosSegReq::Any | PosSegReq::Literal(_) => Self::Any,
    }
  }

  /// Whether this position accepts after consuming at least one byte.
  fn accepts(self, req: PosSegReq<'_>) -> bool {
    match (self, req) {
      (Self::Any, _) => true,
      (Self::Glob(pos), PosSegReq::Glob(tokens)) => pos_glob_accepts(tokens, pos),
      _ => false,
    }
  }

  /// Whether no further byte can ever be consumed from this position.
  fn is_done(self, req: PosSegReq<'_>) -> bool {
    match (self, req) {
      (Self::Glob(pos), PosSegReq::Glob(tokens)) => pos >= tokens.len(),
      _ => false,
    }
  }

  /// Nondeterministically advances this position by one valid UTF-8 `byte`.
  fn step(self, req: PosSegReq<'_>, byte: u8) -> [Option<Self>; 2] {
    match (self, req) {
      (Self::Any, _) => [Some(Self::Any), None],
      (Self::Glob(pos), PosSegReq::Glob(tokens)) => {
        let [first, second] = step_pos_glob(tokens, pos, byte);
        [first.map(Self::Glob), second.map(Self::Glob)]
      }
      _ => [None, None],
    }
  }
}

/// Boolean signature of which active `Deny` literal and `Deny` globs match one concrete non-empty
/// valid UTF-8 segment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SegmentSignature {
  literal: Option<usize>,
  globs: BitSet,
}

/// Segment-level NFA-Allow × DFA-Deny × RFC 3629 UTF-8 product state: positive globs stay linear
/// single-token positions while ONLY active `Deny` rules use powerset masks.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SegProductState {
  pos0: PosSegState,
  pos1: PosSegState,
  utf8: Utf8State,
  deny_trie: Option<usize>,
  deny_masks: Vec<BitSet>,
}

impl SegProductState {
  /// Whether this non-empty state is a valid complete RFC 3629 UTF-8 segment matching both
  /// positive requirements.
  fn accepts(&self, reqs: [PosSegReq<'_>; 2]) -> bool {
    let [req0, req1] = reqs;
    self.utf8 == Utf8State::Start && self.pos0.accepts(req0) && self.pos1.accepts(req1)
  }

  /// Whether neither positive requirement can consume any additional byte.
  fn is_sink(&self, reqs: [PosSegReq<'_>; 2]) -> bool {
    let [req0, req1] = reqs;
    self.pos0.is_done(req0) || self.pos1.is_done(req1)
  }

  /// Accepting [`SegmentSignature`] over active `Deny` literals and `Deny` globs.
  fn deny_signature(
    &self,
    deny_trie: &LiteralTrie,
    deny_globs: &[&[slates_svn::authz::GlobToken]],
  ) -> SegmentSignature {
    let mut accepted = BitSet::empty(deny_globs.len());
    for (idx, (&tokens, mask)) in deny_globs.iter().zip(&self.deny_masks).enumerate() {
      if mask.get(tokens.len()) {
        accepted.set(idx);
      }
    }
    SegmentSignature {
      literal: deny_trie.literal_at(self.deny_trie),
      globs: accepted,
    }
  }
}

/// Mutable BFS frontier state for segment-level NFA-Allow × DFA-Deny × UTF-8 exploration.
struct SegProductBfs {
  queue: VecDeque<SegProductState>,
  visited: HashSet<SegProductState>,
  emitted: HashSet<SegmentSignature>,
  budget_exceeded: bool,
}

/// Compiled `Deny` literal trie, active `Deny` globs, positive requirements, and minterm
/// representatives for one segment-level BFS.
struct SegProductDomain<'a> {
  reqs: [PosSegReq<'a>; 2],
  deny_trie: &'a LiteralTrie,
  deny_globs: &'a [&'a [slates_svn::authz::GlobToken]],
  reps: &'a [u8],
}

/// Evaluates whether concrete segment `bytes` satisfies positive requirement `req`.
fn pos_seg_matches_bytes(req: PosSegReq<'_>, bytes: &[u8]) -> bool {
  match req {
    PosSegReq::Any => true,
    PosSegReq::Literal(expected) => expected == bytes,
    PosSegReq::Glob(tokens) => glob_tokens_match_bytes(tokens, bytes),
  }
}

/// Computes the exact `Deny` [`SegmentSignature`] for concrete valid UTF-8 segment `bytes`.
fn literal_deny_signature(
  bytes: &[u8],
  deny_literals: &[&[u8]],
  deny_globs: &[&[slates_svn::authz::GlobToken]],
) -> SegmentSignature {
  let literal = deny_literals
    .iter()
    .position(|&candidate| candidate == bytes);
  let mut globs = BitSet::empty(deny_globs.len());
  for (idx, &tokens) in deny_globs.iter().enumerate() {
    if glob_tokens_match_bytes(tokens, bytes) {
      globs.set(idx);
    }
  }
  SegmentSignature { literal, globs }
}

/// Direct O(len) segment check when at least one positive requirement pins an exact literal slice.
fn explore_literal_segment(
  bytes: &[u8],
  other_req: PosSegReq<'_>,
  deny_literals: &[&[u8]],
  deny_globs: &[&[slates_svn::authz::GlobToken]],
  on_sig: &mut impl FnMut(&SegmentSignature) -> bool,
) -> WitnessOutcome {
  if !is_valid_utf8_segment(bytes) || !pos_seg_matches_bytes(other_req, bytes) {
    return WitnessOutcome::Empty;
  }
  let sig = literal_deny_signature(bytes, deny_literals, deny_globs);
  if on_sig(&sig) {
    WitnessOutcome::Found
  } else {
    WitnessOutcome::Empty
  }
}

/// O(deny_literals) signature enumeration when both positive requirements are unconstrained (`Any`)
/// and no active `Deny` globs exist: one fresh valid UTF-8 non-literal segment plus each valid
/// `Deny` literal.
fn explore_literal_only_deny_signatures(
  literal_count: usize,
  on_sig: &mut impl FnMut(&SegmentSignature) -> bool,
) -> WitnessOutcome {
  let empty = BitSet::empty(0);
  let found = on_sig(&SegmentSignature {
    literal: None,
    globs: empty.clone(),
  }) || (0..literal_count).any(|idx| {
    on_sig(&SegmentSignature {
      literal: Some(idx),
      globs: empty.clone(),
    })
  });
  if found {
    WitnessOutcome::Found
  } else {
    WitnessOutcome::Empty
  }
}

/// Records one newly reached non-empty segment state into `bfs`, emitting its `Deny` signature when
/// it finishes a valid UTF-8 segment matching both positive requirements.
fn visit_seg_successor(
  bfs: &mut SegProductBfs,
  next: SegProductState,
  domain: &SegProductDomain<'_>,
  on_sig: &mut impl FnMut(&SegmentSignature) -> bool,
) -> bool {
  if bfs.visited.contains(&next) {
    return false;
  }
  if bfs.visited.len() >= WITNESS_STATE_BUDGET {
    bfs.budget_exceeded = true;
    return false;
  }
  if next.accepts(domain.reqs) {
    let sig = next.deny_signature(domain.deny_trie, domain.deny_globs);
    if bfs.emitted.insert(sig.clone()) && on_sig(&sig) {
      return true;
    }
  }
  let sink = next.is_sink(domain.reqs);
  bfs.visited.insert(next.clone());
  if !sink {
    bfs.queue.push_back(next);
  }
  false
}

/// Visits every `(pos0, pos1)` pair generated from one byte transition.
fn visit_seg_pairs(
  bfs: &mut SegProductBfs,
  positions: ([Option<PosSegState>; 2], [Option<PosSegState>; 2]),
  shared: (Utf8State, Option<usize>, &[BitSet]),
  domain: &SegProductDomain<'_>,
  on_sig: &mut impl FnMut(&SegmentSignature) -> bool,
) -> bool {
  let (next_pos0, next_pos1) = positions;
  let (next_utf8, next_trie, next_masks) = shared;
  for pos0 in next_pos0.into_iter().flatten() {
    for pos1 in next_pos1.into_iter().flatten() {
      let next = SegProductState {
        pos0,
        pos1,
        utf8: next_utf8,
        deny_trie: next_trie,
        deny_masks: next_masks.to_vec(),
      };
      if visit_seg_successor(bfs, next, domain, on_sig) {
        return true;
      }
    }
  }
  false
}

/// Advances `state` by one valid UTF-8 representative `byte`, skipping `Deny` mask work whenever
/// either the UTF-8 DFA or a positive NFA rejects `byte`.
fn expand_seg_byte(
  bfs: &mut SegProductBfs,
  state: &SegProductState,
  byte: u8,
  domain: &SegProductDomain<'_>,
  on_sig: &mut impl FnMut(&SegmentSignature) -> bool,
) -> bool {
  let Some(next_utf8) = state.utf8.step(byte) else {
    return false;
  };
  let [req0, req1] = domain.reqs;
  let next_pos0 = state.pos0.step(req0, byte);
  if next_pos0 == [None, None] {
    return false;
  }
  let next_pos1 = state.pos1.step(req1, byte);
  if next_pos1 == [None, None] {
    return false;
  }
  let next_trie = domain.deny_trie.step(state.deny_trie, byte);
  let next_masks: Vec<BitSet> = domain
    .deny_globs
    .iter()
    .zip(&state.deny_masks)
    .map(|(&tokens, mask)| glob_step_mask(tokens, mask, byte))
    .collect();
  visit_seg_pairs(
    bfs,
    (next_pos0, next_pos1),
    (next_utf8, next_trie, &next_masks),
    domain,
    on_sig,
  )
}

/// Runs the bounded segment-level NFA-Allow × DFA-Deny × UTF-8 BFS for non-literal positive
/// requirements and active `Deny` globs.
fn run_seg_product_bfs(
  reqs: [PosSegReq<'_>; 2],
  deny_literals: &[&[u8]],
  deny_globs: &[&[slates_svn::authz::GlobToken]],
  on_sig: &mut impl FnMut(&SegmentSignature) -> bool,
) -> WitnessOutcome {
  let [req0, req1] = reqs;
  let deny_trie = LiteralTrie::build(deny_literals);
  let reps = minterm_representatives(reqs, deny_literals, deny_globs);
  let domain = SegProductDomain {
    reqs,
    deny_trie: &deny_trie,
    deny_globs,
    reps: &reps,
  };
  let start = SegProductState {
    pos0: PosSegState::initial(req0),
    pos1: PosSegState::initial(req1),
    utf8: Utf8State::Start,
    deny_trie: (!deny_literals.is_empty()).then_some(0),
    deny_masks: deny_globs
      .iter()
      .map(|&tokens| glob_start_mask(tokens))
      .collect(),
  };
  let mut bfs = SegProductBfs {
    queue: VecDeque::from([start]),
    visited: HashSet::new(),
    emitted: HashSet::new(),
    budget_exceeded: false,
  };
  while let Some(state) = bfs.queue.pop_front() {
    if domain
      .reps
      .iter()
      .any(|&byte| expand_seg_byte(&mut bfs, &state, byte, &domain, on_sig))
    {
      return WitnessOutcome::Found;
    }
  }
  if bfs.budget_exceeded {
    WitnessOutcome::BudgetExceeded
  } else {
    WitnessOutcome::Empty
  }
}

/// Explores exact realizable non-empty valid RFC 3629 UTF-8 `Deny` [`SegmentSignature`]s matching
/// both positive requirements in `reqs`, invoking `on_sig` for each newly discovered signature.
fn explore_deny_signatures(
  reqs: [PosSegReq<'_>; 2],
  deny_literals: &[&[u8]],
  deny_globs: &[&[slates_svn::authz::GlobToken]],
  mut on_sig: impl FnMut(&SegmentSignature) -> bool,
) -> WitnessOutcome {
  let [req0, req1] = reqs;
  if let PosSegReq::Literal(bytes) = req0 {
    return explore_literal_segment(bytes, req1, deny_literals, deny_globs, &mut on_sig);
  }
  if let PosSegReq::Literal(bytes) = req1 {
    return explore_literal_segment(bytes, req0, deny_literals, deny_globs, &mut on_sig);
  }
  if req0 == PosSegReq::Any && req1 == PosSegReq::Any && deny_globs.is_empty() {
    return explore_literal_only_deny_signatures(deny_literals.len(), &mut on_sig);
  }
  run_seg_product_bfs(reqs, deny_literals, deny_globs, &mut on_sig)
}

/// Normalized path segment used by the exact path-level witness automaton.
#[derive(Clone, Debug, PartialEq, Eq)]
enum NormSegment {
  Literal(Vec<u8>),
  Glob(Vec<slates_svn::authz::GlobToken>),
  One,
  Any,
}

/// Normalizes a [`Segment::Glob`] into a [`NormSegment`], dropping segments that can never match
/// any valid non-empty UTF-8 Subversion path segment.
fn normalize_glob_segment(glob: &Glob) -> Option<NormSegment> {
  use slates_svn::authz::GlobToken;
  let tokens = glob_tokens(glob);
  if tokens.is_empty() || tokens.contains(&GlobToken::Never) {
    return None;
  }
  if tokens.as_slice() == [GlobToken::Run] {
    return Some(NormSegment::One);
  }
  let literal: Option<Vec<u8>> = tokens
    .iter()
    .map(|token| match token {
      GlobToken::Byte(byte) => Some(*byte),
      _ => None,
    })
    .collect();
  match literal {
    Some(bytes) => is_valid_utf8_segment(&bytes).then_some(NormSegment::Literal(bytes)),
    None => Some(NormSegment::Glob(tokens)),
  }
}

/// Normalizes a [`Segment`] for witness solving, returning `None` when the segment can never match
/// any non-empty valid UTF-8 Subversion path segment.
fn normalize_segment(segment: &Segment) -> Option<NormSegment> {
  match segment {
    Segment::Literal(bytes) => {
      is_valid_utf8_segment(bytes).then(|| NormSegment::Literal(bytes.clone()))
    }
    Segment::One => Some(NormSegment::One),
    Segment::Any => Some(NormSegment::Any),
    Segment::Glob(glob) => normalize_glob_segment(glob),
  }
}

/// Normalizes all segments of a rule, collapsing consecutive `**` and dropping unmatchable rules.
fn normalize_rule_segments(segments: &[Segment]) -> Option<Vec<NormSegment>> {
  let mut out = Vec::with_capacity(segments.len());
  for segment in segments {
    let norm = normalize_segment(segment)?;
    if norm != NormSegment::Any || out.last() != Some(&NormSegment::Any) {
      out.push(norm);
    }
  }
  Some(out)
}

/// Closes a `Deny` rule's path-position `mask` across zero-segment `NormSegment::Any` (`**`) steps.
fn path_close_any(segments: &[NormSegment], mask: &mut BitSet) {
  for (pos, segment) in segments.iter().enumerate() {
    if *segment == NormSegment::Any && mask.get(pos) {
      mask.set(pos.saturating_add(1));
    }
  }
}

/// One normalized rule belonging to a single queried right, carrying its policy definition order.
struct RightRule {
  order: usize,
  segments: Vec<NormSegment>,
}

/// All matchable rules speaking to one `(principal, right)` pair, partitioned into `allows` and
/// `denies` while preserving policy definition order.
struct RightRules {
  allow_unruled: bool,
  allows: Vec<RightRule>,
  denies: Vec<RightRule>,
}

/// Compiles all matchable rules of `rules` speaking to `principal` about `right`.
fn compile_right_rules(
  rules: &[Rule<PathPattern>],
  principal: &Principal,
  (right, allow_unruled): (Right, bool),
) -> RightRules {
  let mut allows = Vec::new();
  let mut denies = Vec::new();
  for (order, rule) in rules.iter().enumerate() {
    if !rule.speaks(principal, right) {
      continue;
    }
    let Some(segments) = normalize_rule_segments(&rule.pattern.segments) else {
      continue;
    };
    let compiled = RightRule { order, segments };
    match rule.effect {
      Effect::Allow => allows.push(compiled),
      Effect::Deny => denies.push(compiled),
    }
  }
  RightRules {
    allow_unruled,
    allows,
    denies,
  }
}

/// Whether a normalized rule matches the root path `/` (depth 0).
fn rule_matches_root(segments: &[NormSegment]) -> bool {
  segments.is_empty() || segments == [NormSegment::Any]
}

/// Positive witness choice for one queried right: either an explicit `Allow` rule or `Unruled`
/// (when `allow_unruled` permits unruled paths).
#[derive(Clone, Copy)]
enum AllowChoice<'a> {
  Unruled,
  Rule(&'a RightRule),
}

impl AllowChoice<'_> {
  /// Policy definition index of an explicit `Allow` rule, or `None` for `Unruled`.
  fn order(self) -> Option<usize> {
    match self {
      Self::Unruled => None,
      Self::Rule(rule) => Some(rule.order),
    }
  }
}

/// Nondeterministic state of one positive right choice along a path: positive `Allow` rules track
/// a single linear segment index rather than a powerset bitset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum AllowPos {
  /// Still matching segment index `pos` (`pos < rule.segments.len()`) of the chosen `Allow` rule.
  Matching(usize),
  /// The chosen `Allow` rule (or `Unruled`) has already won at an ancestor depth and now inherits
  /// downward until any `Deny` rule for this right matches.
  Inherited,
}

/// Whether path position `pos` in collapsed `segments` (no adjacent `Any`) reaches the end with
/// zero further segments.
fn can_accept_path(segments: &[NormSegment], pos: usize) -> bool {
  pos == segments.len()
    || (pos.saturating_add(1) == segments.len() && segments.get(pos) == Some(&NormSegment::Any))
}

/// Translates a non-`Any` [`NormSegment`] into its [`PosSegReq`].
fn pos_seg_req_of(segment: &NormSegment) -> PosSegReq<'_> {
  match segment {
    NormSegment::Literal(bytes) => PosSegReq::Literal(bytes),
    NormSegment::Glob(tokens) => PosSegReq::Glob(tokens),
    NormSegment::One | NormSegment::Any => PosSegReq::Any,
  }
}

/// One nondeterministic step out of an [`AllowPos`] across the next path segment.
#[derive(Clone, Copy)]
struct PosBranch<'a> {
  req: PosSegReq<'a>,
  next_pos: Option<usize>,
}

/// Computes the 0..=2 nondeterministic positive segment branches out of `pos` for `choice`,
/// placing advancing branches before `**` self-loops so shortest witnesses are explored first.
fn pos_branches<'a>(choice: AllowChoice<'a>, pos: AllowPos) -> [Option<PosBranch<'a>>; 2] {
  let AllowPos::Matching(at) = pos else {
    return [
      Some(PosBranch {
        req: PosSegReq::Any,
        next_pos: None,
      }),
      None,
    ];
  };
  let AllowChoice::Rule(rule) = choice else {
    return [None, None];
  };
  let Some(segment) = rule.segments.get(at) else {
    return [None, None];
  };
  if *segment == NormSegment::Any {
    let advance = rule
      .segments
      .get(at.saturating_add(1))
      .map(|next_seg| PosBranch {
        req: pos_seg_req_of(next_seg),
        next_pos: Some(at.saturating_add(2)),
      });
    let stay = Some(PosBranch {
      req: PosSegReq::Any,
      next_pos: Some(at),
    });
    return [advance, stay];
  }
  [
    Some(PosBranch {
      req: pos_seg_req_of(segment),
      next_pos: Some(at.saturating_add(1)),
    }),
    None,
  ]
}

/// Resolves the 0..=2 resulting [`AllowPos`] states from taking a branch to `next_pos`, given
/// whether any `Deny` (`deny_any`) or any strictly later `Deny` (`deny_gt`) matched at the new
/// depth.
fn resolve_allow_pos(
  choice: AllowChoice<'_>,
  next_pos: Option<usize>,
  deny_any: bool,
  deny_gt: bool,
) -> [Option<AllowPos>; 2] {
  let Some(pos) = next_pos else {
    return [(!deny_any).then_some(AllowPos::Inherited), None];
  };
  let AllowChoice::Rule(rule) = choice else {
    return [None, None];
  };
  let inherited = (can_accept_path(&rule.segments, pos) && !deny_gt).then_some(AllowPos::Inherited);
  let matching = (pos < rule.segments.len()).then_some(AllowPos::Matching(pos));
  [inherited, matching]
}

/// Initial [`AllowPos`] states at depth 0 (`/`) for `choice`.
fn initial_allow_positions(
  choice: AllowChoice<'_>,
  deny_any: bool,
  deny_gt: bool,
) -> [Option<AllowPos>; 2] {
  match choice {
    AllowChoice::Unruled => [(!deny_any).then_some(AllowPos::Inherited), None],
    AllowChoice::Rule(_) => resolve_allow_pos(choice, Some(0), deny_any, deny_gt),
  }
}

/// One queried right's positive choice paired with the `Deny` rules that can override it.
struct RightSlot<'a> {
  choice: AllowChoice<'a>,
  denies: &'a [&'a RightRule],
}

/// Path-level NFA-Allow × DFA-Deny state: positive right choices keep a single linear [`AllowPos`]
/// while ONLY the relevant `Deny` rules carry powerset bitsets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PathPairState {
  pos0: AllowPos,
  pos1: Option<AllowPos>,
  denies0: Vec<BitSet>,
  denies1: Vec<BitSet>,
}

/// Mutable BFS frontier for path-level NFA-Allow × DFA-Deny search.
struct PathPairBfs {
  queue: VecDeque<PathPairState>,
  visited: HashSet<PathPairState>,
  budget_exceeded: bool,
}

/// Active `Deny` segment patterns (`Literal` and `Glob`) referenced by reachable positions in the
/// current `Deny` masks, deduplicated for segment-level exploration.
struct ActivePatterns<'a> {
  literals: Vec<&'a [u8]>,
  globs: Vec<&'a [slates_svn::authz::GlobToken]>,
}

/// Interns one active `Deny` segment pattern into `active`.
fn record_active_segment<'a>(segment: &'a NormSegment, active: &mut ActivePatterns<'a>) {
  match segment {
    NormSegment::Literal(bytes) if !active.literals.contains(&bytes.as_slice()) => {
      active.literals.push(bytes.as_slice());
    }
    NormSegment::Glob(tokens) if !active.globs.contains(&tokens.as_slice()) => {
      active.globs.push(tokens.as_slice());
    }
    _ => {}
  }
}

/// Appends active `Deny` segment patterns from `denies`/`masks` into `active`.
fn collect_deny_patterns_into<'a>(
  denies: &[&'a RightRule],
  masks: &[BitSet],
  active: &mut ActivePatterns<'a>,
) {
  for (&rule, mask) in denies.iter().zip(masks) {
    for (pos, segment) in rule.segments.iter().enumerate() {
      if mask.get(pos) {
        record_active_segment(segment, active);
      }
    }
  }
}

/// Collects all distinct active `Deny` `Literal` and `Glob` segment patterns across both slots.
fn collect_active_deny_patterns<'a>(
  slot0: &RightSlot<'a>,
  slot1: Option<&RightSlot<'a>>,
  state: &PathPairState,
) -> ActivePatterns<'a> {
  let mut active = ActivePatterns {
    literals: Vec::new(),
    globs: Vec::new(),
  };
  collect_deny_patterns_into(slot0.denies, &state.denies0, &mut active);
  if let Some(s1) = slot1 {
    collect_deny_patterns_into(s1.denies, &state.denies1, &mut active);
  }
  active
}

/// Whether `Deny` segment `segment` at an active position consumes a segment realizing `sig`.
fn norm_segment_matches_sig(
  segment: &NormSegment,
  active: &ActivePatterns<'_>,
  sig: &SegmentSignature,
) -> bool {
  match segment {
    NormSegment::One => true,
    NormSegment::Literal(bytes) => sig
      .literal
      .and_then(|idx| active.literals.get(idx))
      .is_some_and(|&matched| matched == bytes.as_slice()),
    NormSegment::Glob(tokens) => active
      .globs
      .iter()
      .position(|&candidate| candidate == tokens.as_slice())
      .is_some_and(|idx| sig.globs.get(idx)),
    NormSegment::Any => false,
  }
}

/// Advances one `Deny` rule's path position bitset across one valid UTF-8 segment matching `sig`.
fn advance_deny_rule_mask(
  rule: &RightRule,
  mask: &BitSet,
  active: &ActivePatterns<'_>,
  sig: &SegmentSignature,
) -> BitSet {
  let mut next = BitSet::empty(rule.segments.len().saturating_add(1));
  for (pos, segment) in rule.segments.iter().enumerate() {
    if !mask.get(pos) {
      continue;
    }
    if *segment == NormSegment::Any {
      next.set(pos);
    } else if norm_segment_matches_sig(segment, active, sig) {
      next.set(pos.saturating_add(1));
    }
  }
  path_close_any(&rule.segments, &mut next);
  next
}

/// Computes depth-0 `Deny` bitsets and `(deny_any_matched, deny_gt_matched)` for `slot`.
fn initial_deny_list(slot: &RightSlot<'_>) -> (Vec<BitSet>, bool, bool) {
  let allow_order = slot.choice.order();
  let mut masks = Vec::with_capacity(slot.denies.len());
  let (mut deny_any, mut deny_gt) = (false, false);
  for &rule in slot.denies {
    let mut mask = BitSet::empty(rule.segments.len().saturating_add(1));
    mask.set(0);
    path_close_any(&rule.segments, &mut mask);
    if mask.get(rule.segments.len()) {
      deny_any = true;
      if allow_order.is_none_or(|ord| rule.order > ord) {
        deny_gt = true;
      }
    }
    mask.clear(rule.segments.len());
    masks.push(mask);
  }
  (masks, deny_any, deny_gt)
}

/// Advances `slot`'s `Deny` bitsets across one segment realizing `sig`, returning the cleared next
/// masks alongside `(deny_any_matched, deny_gt_matched)` at the new depth.
fn step_deny_list(
  slot: &RightSlot<'_>,
  masks: &[BitSet],
  active: &ActivePatterns<'_>,
  sig: &SegmentSignature,
) -> (Vec<BitSet>, bool, bool) {
  let allow_order = slot.choice.order();
  let mut next_masks = Vec::with_capacity(slot.denies.len());
  let (mut deny_any, mut deny_gt) = (false, false);
  for (&rule, mask) in slot.denies.iter().zip(masks) {
    let mut next = advance_deny_rule_mask(rule, mask, active, sig);
    if next.get(rule.segments.len()) {
      deny_any = true;
      if allow_order.is_none_or(|ord| rule.order > ord) {
        deny_gt = true;
      }
    }
    next.clear(rule.segments.len());
    next_masks.push(next);
  }
  (next_masks, deny_any, deny_gt)
}

/// Inserts a non-accepting [`PathPairState`] into `bfs` respecting [`WITNESS_STATE_BUDGET`].
fn enqueue_path_state(bfs: &mut PathPairBfs, next: PathPairState) {
  if bfs.visited.contains(&next) {
    return;
  }
  if bfs.visited.len() >= WITNESS_STATE_BUDGET {
    bfs.budget_exceeded = true;
    return;
  }
  bfs.visited.insert(next.clone());
  bfs.queue.push_back(next);
}

/// Enqueues one `pos0` paired with every `pos1` in `p1_list`, returning `true` on acceptance.
fn enqueue_two_right_pos0(
  bfs: &mut PathPairBfs,
  pos0: AllowPos,
  p1_list: [Option<AllowPos>; 2],
  denies: (&[BitSet], &[BitSet]),
) -> bool {
  let (denies0, denies1) = denies;
  for pos1 in p1_list.into_iter().flatten() {
    if pos0 == AllowPos::Inherited && pos1 == AllowPos::Inherited {
      return true;
    }
    enqueue_path_state(
      bfs,
      PathPairState {
        pos0,
        pos1: Some(pos1),
        denies0: denies0.to_vec(),
        denies1: denies1.to_vec(),
      },
    );
  }
  false
}

/// Enqueues all `(pos0, pos1)` combinations produced at one depth, returning `true` immediately when
/// both queried rights reach [`AllowPos::Inherited`].
fn enqueue_path_positions(
  bfs: &mut PathPairBfs,
  pos0s: [Option<AllowPos>; 2],
  pos1s: Option<[Option<AllowPos>; 2]>,
  denies: (&[BitSet], &[BitSet]),
) -> bool {
  for pos0 in pos0s.into_iter().flatten() {
    match pos1s {
      None => {
        if pos0 == AllowPos::Inherited {
          return true;
        }
        enqueue_path_state(
          bfs,
          PathPairState {
            pos0,
            pos1: None,
            denies0: denies.0.to_vec(),
            denies1: Vec::new(),
          },
        );
      }
      Some(p1_list) => {
        if enqueue_two_right_pos0(bfs, pos0, p1_list, denies) {
          return true;
        }
      }
    }
  }
  false
}

/// Applies one realized `Deny` segment signature `sig` to `(b0, b1)` out of `state`, returning
/// `true` when an accepting path state is reached.
fn apply_path_sig(
  slots: (&RightSlot<'_>, Option<&RightSlot<'_>>),
  branches: (PosBranch<'_>, Option<PosBranch<'_>>),
  state: &PathPairState,
  active: &ActivePatterns<'_>,
  sig: &SegmentSignature,
  bfs: &mut PathPairBfs,
) -> bool {
  let (slot0, slot1) = slots;
  let (b0, b1) = branches;
  let (next_d0, any0, gt0) = step_deny_list(slot0, &state.denies0, active, sig);
  let pos0s = resolve_allow_pos(slot0.choice, b0.next_pos, any0, gt0);
  if pos0s == [None, None] {
    return false;
  }
  let Some((s1, branch1)) = slot1.zip(b1) else {
    return enqueue_path_positions(bfs, pos0s, None, (&next_d0, &[]));
  };
  let (next_d1, any1, gt1) = step_deny_list(s1, &state.denies1, active, sig);
  let pos1s = resolve_allow_pos(s1.choice, branch1.next_pos, any1, gt1);
  if pos1s == [None, None] {
    return false;
  }
  enqueue_path_positions(bfs, pos0s, Some(pos1s), (&next_d0, &next_d1))
}

/// Expands one positive branch pair `(b0, b1)` from `state` over all realizable valid UTF-8 `Deny`
/// signatures.
fn expand_branch_pair(
  slots: (&RightSlot<'_>, Option<&RightSlot<'_>>),
  branches: (PosBranch<'_>, Option<PosBranch<'_>>),
  state: &PathPairState,
  active: &ActivePatterns<'_>,
  bfs: &mut PathPairBfs,
) -> WitnessOutcome {
  let reqs = [
    branches.0.req,
    branches.1.map_or(PosSegReq::Any, |branch| branch.req),
  ];
  explore_deny_signatures(reqs, &active.literals, &active.globs, |sig| {
    apply_path_sig(slots, branches, state, active, sig, bfs)
  })
}

/// Expands one right-0 branch `b0` paired with every available right-1 branch in `branches1`.
fn expand_branch_for_b0(
  slots: (&RightSlot<'_>, Option<&RightSlot<'_>>),
  b0: PosBranch<'_>,
  branches1: Option<[Option<PosBranch<'_>>; 2]>,
  state: &PathPairState,
  active: &ActivePatterns<'_>,
  bfs: &mut PathPairBfs,
) -> WitnessOutcome {
  let Some(b1_list) = branches1 else {
    return expand_branch_pair((slots.0, None), (b0, None), state, active, bfs);
  };
  let mut outcome = WitnessOutcome::Empty;
  for b1 in b1_list.into_iter().flatten() {
    outcome = outcome.merge(expand_branch_pair(
      slots,
      (b0, Some(b1)),
      state,
      active,
      bfs,
    ));
    if outcome == WitnessOutcome::Found {
      return WitnessOutcome::Found;
    }
  }
  outcome
}

/// Expands all positive branch pairs out of `state`.
fn expand_path_pair_state(
  slot0: &RightSlot<'_>,
  slot1: Option<&RightSlot<'_>>,
  state: &PathPairState,
  bfs: &mut PathPairBfs,
) -> WitnessOutcome {
  let active = collect_active_deny_patterns(slot0, slot1, state);
  let branches0 = pos_branches(slot0.choice, state.pos0);
  let branches1 = slot1
    .zip(state.pos1)
    .map(|(s1, p1)| pos_branches(s1.choice, p1));
  let mut outcome = WitnessOutcome::Empty;
  for b0 in branches0.into_iter().flatten() {
    outcome = outcome.merge(expand_branch_for_b0(
      (slot0, slot1),
      b0,
      branches1,
      state,
      &active,
      bfs,
    ));
    if outcome == WitnessOutcome::Found {
      return WitnessOutcome::Found;
    }
  }
  outcome
}

/// Seeds depth-0 initial states into `bfs`, returning `Some(Found)` when `/` already satisfies the
/// query or `Some(Empty)` when no initial position exists.
fn seed_allow_slots(
  slot0: &RightSlot<'_>,
  slot1: Option<&RightSlot<'_>>,
  bfs: &mut PathPairBfs,
) -> Option<WitnessOutcome> {
  let (d0, any0, gt0) = initial_deny_list(slot0);
  let pos0s = initial_allow_positions(slot0.choice, any0, gt0);
  if pos0s == [None, None] {
    return Some(WitnessOutcome::Empty);
  }
  let accepted_root = match slot1 {
    None => enqueue_path_positions(bfs, pos0s, None, (&d0, &[])),
    Some(s1) => {
      let (d1, any1, gt1) = initial_deny_list(s1);
      let pos1s = initial_allow_positions(s1.choice, any1, gt1);
      if pos1s == [None, None] {
        return Some(WitnessOutcome::Empty);
      }
      enqueue_path_positions(bfs, pos0s, Some(pos1s), (&d0, &d1))
    }
  };
  accepted_root.then_some(WitnessOutcome::Found)
}

/// Runs exact NFA-Allow × DFA-Deny path-level BFS for one chosen positive rule (or pair of
/// positive rules).
fn search_allow_slots(slot0: &RightSlot<'_>, slot1: Option<&RightSlot<'_>>) -> WitnessOutcome {
  let mut bfs = PathPairBfs {
    queue: VecDeque::new(),
    visited: HashSet::new(),
    budget_exceeded: false,
  };
  if let Some(done) = seed_allow_slots(slot0, slot1, &mut bfs) {
    return done;
  }
  while let Some(state) = bfs.queue.pop_front() {
    match expand_path_pair_state(slot0, slot1, &state, &mut bfs) {
      WitnessOutcome::Found => return WitnessOutcome::Found,
      WitnessOutcome::BudgetExceeded => bfs.budget_exceeded = true,
      WitnessOutcome::Empty => {}
    }
  }
  if bfs.budget_exceeded {
    WitnessOutcome::BudgetExceeded
  } else {
    WitnessOutcome::Empty
  }
}

/// Collects all viable positive [`AllowChoice`] candidates for `right`, newest `Allow` rules first
/// so unshadowed rules find witnesses immediately.
fn positive_choices(right: &RightRules) -> Vec<AllowChoice<'_>> {
  let mut choices = Vec::with_capacity(right.allows.len().saturating_add(1));
  if right.allow_unruled
    && !right
      .denies
      .iter()
      .any(|deny| rule_matches_root(&deny.segments))
  {
    choices.push(AllowChoice::Unruled);
  }
  for allow in right.allows.iter().rev() {
    choices.push(AllowChoice::Rule(allow));
  }
  choices
}

/// Exact witness search for a single queried right: checks unruled root in O(|denies|), then tests
/// each `Allow` rule nondeterministically against ONLY strictly later `Deny` rules (`deny.order >
/// allow.order`).
fn witness_single_right(right: &RightRules) -> WitnessOutcome {
  if right.allow_unruled
    && !right
      .denies
      .iter()
      .any(|deny| rule_matches_root(&deny.segments))
  {
    return WitnessOutcome::Found;
  }
  let mut outcome = WitnessOutcome::Empty;
  for allow in right.allows.iter().rev() {
    let overriding: Vec<&RightRule> = right
      .denies
      .iter()
      .filter(|deny| deny.order > allow.order)
      .collect();
    let slot = RightSlot {
      choice: AllowChoice::Rule(allow),
      denies: &overriding,
    };
    outcome = outcome.merge(search_allow_slots(&slot, None));
    if outcome == WitnessOutcome::Found {
      return WitnessOutcome::Found;
    }
  }
  outcome
}

/// Exact witness search for two simultaneously queried rights (`Read` and one write right): checks
/// each `(choice0, choice1)` positive pair nondeterministically against each right's `Deny` rules.
fn witness_two_rights(right0: &RightRules, right1: &RightRules) -> WitnessOutcome {
  let choices0 = positive_choices(right0);
  let choices1 = positive_choices(right1);
  if choices0.is_empty() || choices1.is_empty() {
    return WitnessOutcome::Empty;
  }
  let denies0: Vec<&RightRule> = right0.denies.iter().collect();
  let denies1: Vec<&RightRule> = right1.denies.iter().collect();
  let mut outcome = WitnessOutcome::Empty;
  for &c0 in &choices0 {
    let slot0 = RightSlot {
      choice: c0,
      denies: &denies0,
    };
    for &c1 in &choices1 {
      let slot1 = RightSlot {
        choice: c1,
        denies: &denies1,
      };
      outcome = outcome.merge(search_allow_slots(&slot0, Some(&slot1)));
      if outcome == WitnessOutcome::Found {
        return WitnessOutcome::Found;
      }
    }
  }
  outcome
}

/// Exact witness-emptiness check: whether at least one valid RFC 3629 UTF-8 canonical repository
/// path satisfies every `(right, allow_unruled)` constraint in `queries` simultaneously, using
/// two-level NFA-Allow × DFA-Deny × UTF-8 product BFS.
fn witness_satisfying_rules(
  rules: &[Rule<PathPattern>],
  principal: &Principal,
  queries: &[(Right, bool)],
) -> WitnessOutcome {
  match queries {
    [] => WitnessOutcome::Found,
    [q0] => witness_single_right(&compile_right_rules(rules, principal, *q0)),
    [q0, q1] => witness_two_rights(
      &compile_right_rules(rules, principal, *q0),
      &compile_right_rules(rules, principal, *q1),
    ),
    _ => WitnessOutcome::Empty,
  }
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

  /// Exact witness-emptiness check returning typed [`WitnessOutcome`] over valid UTF-8 paths.
  fn witness_satisfying(&self, principal: &Principal, queries: &[(Right, bool)]) -> WitnessOutcome {
    witness_satisfying_rules(&self.rules, principal, queries)
  }

  /// Exact witness-emptiness check: logs on [`WitnessOutcome::BudgetExceeded`] and fails closed.
  fn exists_path_satisfying(&self, principal: &Principal, queries: &[(Right, bool)]) -> bool {
    match self.witness_satisfying(principal, queries) {
      WitnessOutcome::Found => true,
      WitnessOutcome::Empty => false,
      WitnessOutcome::BudgetExceeded => {
        slates_machine::log_line!(
          "slates-server: svn witness search exceeded state budget ({WITNESS_STATE_BUDGET}) across {} rules; failing closed",
          self.rules.len()
        );
        false
      }
    }
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
  if !policy.exists_path_satisfying(principal, &[(Right::Read, is_owner)]) {
    return Ok(SvnAccess::None);
  }
  let write_queries = [
    (Right::Submit, is_owner),
    (Right::Rewrite, false),
    (Right::Delete, false),
  ];
  let can_write = write_queries
    .into_iter()
    .any(|write| policy.exists_path_satisfying(principal, &[(Right::Read, is_owner), write]));
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

  /// Round-5 Blocker 2 (repro `repro_svn_glob_bracket_range_bypass.py`): Do compile
  /// `[Allow Read / Anyone, Deny Read :glob:/[a-[:alpha:]] STRANGER]` into both `SvnPolicy` and
  /// `PrincipalRules`. Expect: `a-[` parses as a range closing at the first `]` with trailing
  /// literal `]`, so `/a]/secret.txt`, `/a]`, `/l]/x`, and `/:]/x` are denied by BOTH evaluators
  /// while `/b]/secret.txt` and `/a/secret.txt` remain allowed.
  #[test]
  fn round5_blocker2_bracket_range_with_posix_class_matches_wildmatch() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
    let stranger = principal(STRANGER);
    let rec = record(
      VcsTool::Subversion,
      vec![
        svn_path_rule(
          "/",
          PolicyRight::Read,
          PolicyEffect::Allow,
          PolicyWho::Anyone,
        ),
        svn_path_rule(
          ":glob:/[a-[:alpha:]]",
          PolicyRight::Read,
          PolicyEffect::Deny,
          record_only(&[STRANGER]),
        ),
      ],
    );
    let rules = super::svn_read_rules(&rec, &stranger);
    let cases = [
      ("/a]/secret.txt", false),
      ("/a]", false),
      ("/l]/secret.txt", false),
      ("/:]/secret.txt", false),
      ("/b]/secret.txt", true),
      ("/a/secret.txt", true),
      ("/public/ok.txt", true),
    ];
    for (path, expected) in cases {
      let policy_ans = super::svn_path_read_allowed(&rec, &stranger, path.as_bytes()).unwrap();
      let tree_ans = slates_svn::authz::may_read(Some(&rules), path);
      assert_eq!(policy_ans, expected, "SvnPolicy mismatch at {path}");
      assert_eq!(tree_ans, expected, "PrincipalRules mismatch at {path}");
    }
    let allow_only = record(
      VcsTool::Subversion,
      vec![svn_path_rule(
        ":glob:/[a-[:alpha:]]",
        PolicyRight::Read,
        PolicyEffect::Allow,
        record_only(&[STRANGER]),
      )],
    );
    assert_eq!(
      super::svn_access(&allow_only, &stranger),
      Ok(super::SvnAccess::Read)
    );
    let allow_rules = super::svn_read_rules(&allow_only, &stranger);
    assert!(slates_svn::authz::may_read(Some(&allow_rules), "/a]"));
  }

  /// One `(label, rules, expected_access)` test vector for `svn_access`.
  type SvnAccessCase<'a> = (
    &'a str,
    Vec<slates_db::catalog::PolicyRule>,
    super::SvnAccess,
  );

  fn assert_svn_access_cases(cases: &[SvnAccessCase<'_>]) {
    let stranger = principal(STRANGER);
    for (label, rules, expected) in cases {
      let rec = record(slates_db::catalog::VcsTool::Subversion, rules.clone());
      assert_eq!(super::svn_access(&rec, &stranger), Ok(*expected), "{label}");
    }
  }

  /// Round-5 Blocker 3 (all 5 reproduced glob lockout cases + exact eclipsed cases): Do evaluate
  /// all 5 Rung-5 lockout policies plus exact-emptiness eclipsed policies and multi-level overlap.
  /// Expect: every non-empty policy grants its exact `Read`/`Write` access (monotonically under
  /// unrelated `Deny` rules), while genuinely empty/eclipsed policies return `SvnAccess::None`.
  #[test]
  fn round5_blocker3_all_five_reproduced_glob_lockout_cases_and_eclipsed() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho};
    let allow_r =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone);
    let deny_r =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Deny, PolicyWho::Anyone);
    let allow_s = |p: &str| {
      svn_path_rule(
        p,
        PolicyRight::Submit,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      )
    };
    let deny_s = |p: &str| {
      svn_path_rule(
        p,
        PolicyRight::Submit,
        PolicyEffect::Deny,
        PolicyWho::Anyone,
      )
    };

    assert_svn_access_cases(&[
      (
        "case 1: /* minus /?",
        vec![allow_r(":glob:/*"), deny_r(":glob:/?")],
        super::SvnAccess::Read,
      ),
      (
        "case 1 + unrelated /zz monotonic",
        vec![allow_r(":glob:/*"), deny_r(":glob:/?"), deny_r("/zz")],
        super::SvnAccess::Read,
      ),
      (
        "case 2: /*/* minus /*/?",
        vec![allow_r(":glob:/*/*"), deny_r(":glob:/*/?")],
        super::SvnAccess::Read,
      ),
      (
        "case 3: Allow Read / + Allow Submit /* minus Deny Submit /?",
        vec![allow_r("/"), allow_s(":glob:/*"), deny_s(":glob:/?")],
        super::SvnAccess::Write,
      ),
      (
        "case 4: Deny /secret + Allow Read /trunk/* + Allow Submit /*/docs",
        vec![
          deny_r("/secret"),
          allow_r(":glob:/trunk/*"),
          allow_s(":glob:/*/docs"),
        ],
        super::SvnAccess::Write,
      ),
      (
        "case 5a: /[a-z] minus /[a-mz]",
        vec![allow_r(":glob:/[a-z]"), deny_r(":glob:/[a-mz]")],
        super::SvnAccess::Read,
      ),
      (
        "case 5b: /? minus /[za0_~Am9Z!-]",
        vec![allow_r(":glob:/?"), deny_r(":glob:/[za0_~Am9Z!-]")],
        super::SvnAccess::Read,
      ),
      (
        "eclipsed: /[ab] minus /a and /b",
        vec![allow_r(":glob:/[ab]"), deny_r("/a"), deny_r("/b")],
        super::SvnAccess::None,
      ),
      (
        "eclipsed: /[a-b] minus /[a-c]",
        vec![allow_r(":glob:/[a-b]"), deny_r(":glob:/[a-c]")],
        super::SvnAccess::None,
      ),
      (
        "wildmatch reversed range /[z-a] still matches 'z'",
        vec![allow_r(":glob:/[z-a]")],
        super::SvnAccess::Read,
      ),
      (
        "eclipsed: slash-only bracket /[/] matches no byte under WM_PATHNAME",
        vec![allow_r(":glob:/[/]")],
        super::SvnAccess::None,
      ),
      (
        "eclipsed: /? minus /*",
        vec![allow_r(":glob:/?"), deny_r(":glob:/*")],
        super::SvnAccess::None,
      ),
      (
        "eclipsed: /trunk/* minus /trunk/**",
        vec![allow_r(":glob:/trunk/*"), deny_r(":glob:/trunk/**")],
        super::SvnAccess::None,
      ),
      (
        "multi-level unique witness /b/d",
        vec![
          allow_r(":glob:/[a-b]/[c-d]"),
          deny_r("/a/c"),
          deny_r("/a/d"),
          deny_r("/b/c"),
          allow_s("/b/d"),
        ],
        super::SvnAccess::Write,
      ),
      (
        "multi-level unique witness eclipsed by Deny /b/d",
        vec![
          allow_r(":glob:/[a-b]/[c-d]"),
          deny_r("/a/c"),
          deny_r("/a/d"),
          deny_r("/b/c"),
          deny_r("/b/d"),
          allow_s("/b/d"),
        ],
        super::SvnAccess::None,
      ),
    ]);
  }

  /// Brute-force reference oracle for `svn_access` over an exhaustive path universe `paths`.
  fn brute_force_svn_access(
    rec: &slates_db::catalog::RepositoryRecord,
    user: &Principal,
    paths: &[Vec<u8>],
  ) -> super::SvnAccess {
    let mut can_read = false;
    let mut can_write = false;
    for path in paths {
      if super::svn_path_read_allowed(rec, user, path).unwrap() {
        can_read = true;
        if super::svn_path_write_allowed(rec, user, path, false).unwrap()
          || super::svn_path_write_allowed(rec, user, path, true).unwrap()
        {
          can_write = true;
          break;
        }
      }
    }
    match (can_read, can_write) {
      (_, true) => super::SvnAccess::Write,
      (true, false) => super::SvnAccess::Read,
      (false, false) => super::SvnAccess::None,
    }
  }

  /// Complete finite segment universe for patterns mentioning only byte `'a'` (plus wildcards of
  /// fixed length <= 2), where `'d'` represents the entire complement of `{'a'}` in valid segment bytes.
  const TWO_BYTE_SEGMENTS: [&[u8]; 10] = [
    b"a", b"d", b"aa", b"ad", b"da", b"dd", b"ada", b"add", b"dda", b"ddd",
  ];

  /// Complete finite segment universe for patterns mentioning only `'a'` and `'b'` (fixed len <= 2),
  /// where `'d'` represents the entire complement of `{'a', 'b'}` in valid segment bytes.
  const THREE_BYTE_SEGMENTS: [&[u8]; 21] = [
    b"a", b"b", b"d", b"aa", b"ab", b"ad", b"ba", b"bb", b"bd", b"da", b"db", b"dd", b"ada",
    b"adb", b"add", b"bda", b"bdb", b"bdd", b"dda", b"ddb", b"ddd",
  ];

  /// Builds every canonical path of depth `0..=max_depth` from `segments`.
  fn enumerate_paths(segments: &[&[u8]], max_depth: usize) -> Vec<Vec<u8>> {
    let mut all = vec![b"/".to_vec()];
    let mut level: Vec<Vec<u8>> = vec![Vec::new()];
    for _ in 0..max_depth {
      let mut next = Vec::with_capacity(level.len().saturating_mul(segments.len()));
      for prefix in &level {
        for &seg in segments {
          let mut path = Vec::with_capacity(prefix.len() + 1 + seg.len());
          path.extend_from_slice(prefix);
          path.push(b'/');
          path.extend_from_slice(seg);
          next.push(path);
        }
      }
      all.extend_from_slice(&next);
      level = next;
    }
    all
  }

  fn oracle_segment_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
      "a", "aa", "*", "**", "?", "??", "a*", "*a", "[a]", "[!a]",
    ])
  }

  fn three_byte_segment_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
      "a", "b", "ab", "*", "?", "??", "a*", "*b", "[a-b]", "[!a]",
    ])
  }

  fn svn_oracle_right() -> impl Strategy<Value = slates_db::catalog::PolicyRight> {
    use slates_db::catalog::PolicyRight;
    prop::sample::select(vec![
      PolicyRight::Read,
      PolicyRight::Submit,
      PolicyRight::Rewrite,
      PolicyRight::Delete,
    ])
  }

  fn svn_oracle_effect() -> impl Strategy<Value = slates_db::catalog::PolicyEffect> {
    use slates_db::catalog::PolicyEffect;
    prop::sample::select(vec![PolicyEffect::Allow, PolicyEffect::Deny])
  }

  fn svn_oracle_rule(
    glob_seg: impl Strategy<Value = &'static str> + 'static,
    lit_seg: impl Strategy<Value = &'static str> + 'static,
    max_segs: usize,
  ) -> impl Strategy<Value = slates_db::catalog::PolicyRule> {
    let glob_path = prop::collection::vec(glob_seg, 0..=max_segs).prop_map(|segs| {
      if segs.is_empty() {
        String::from(":glob:/")
      } else {
        format!(":glob:/{}", segs.join("/"))
      }
    });
    let lit_path = prop::collection::vec(lit_seg, 0..=max_segs).prop_map(|segs| {
      if segs.is_empty() {
        String::from("/")
      } else {
        format!("/{}", segs.join("/"))
      }
    });
    let pattern = (any::<bool>(), glob_path, lit_path)
      .prop_map(|(use_glob, glob, lit)| if use_glob { glob } else { lit });
    (pattern, svn_oracle_right(), svn_oracle_effect()).prop_map(|(pattern, right, effect)| {
      svn_path_rule(
        &pattern,
        right,
        effect,
        slates_db::catalog::PolicyWho::Anyone,
      )
    })
  }

  fn two_byte_literal_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["a", "aa"])
  }

  fn three_byte_literal_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["a", "b", "ab"])
  }

  fn diff_glob_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
      "a",
      "b",
      "1",
      "*",
      "**",
      "***",
      "?",
      "[",
      "]",
      "!",
      "^",
      "-",
      "\\",
      ":",
      "[:alpha:]",
      "[:digit:]",
      "[:bogus:]",
      "[a-[:alpha:]]",
      "[a-b]",
      "[z-a]",
      "[!a]",
      "\\*",
    ])
  }

  fn diff_seg_byte() -> impl Strategy<Value = u8> {
    prop::sample::select(vec![
      b'a', b'b', b'c', b'1', b'-', b']', b'[', b'\\', b':', b'!', b'*', b'?',
    ])
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 16 } else { 2048 }),
      ..ProptestConfig::default()
    }))]

    /// Round-5 Blocker 2 (proptest differential): Do compile generated single-segment glob
    /// patterns (including `[a-[:alpha:]]`, `[!a]`, `\*`, unclosed `[`, unknown `[:bogus:]`, `**`,
    /// `?`, `[z-a]`) into `PrincipalRules` via `svn_read_rules` and test against generated non-empty
    /// segment bytes. Expect: `PrincipalRules` (`GlobToken`) matches bit-for-bit identically to
    /// `slates_vcs::wildmatch::Glob::new(pat).matches(seg)`.
    #[test]
    fn round5_blocker2_glob_tokens_match_wildmatch_on_generated_segments(
      pieces in prop::collection::vec(diff_glob_piece(), 1..=4),
      seg in prop::collection::vec(diff_seg_byte(), 1..=4),
    ) {
      use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho, VcsTool};
      let pat_str: String = pieces.concat();
      let rule = svn_path_rule(
        &format!(":glob:/{pat_str}"),
        PolicyRight::Read,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      );
      let rec = record(VcsTool::Subversion, vec![rule]);
      let rules = super::svn_read_rules(&rec, &principal(STRANGER));
      let mut path = Vec::with_capacity(1 + seg.len());
      path.push(b'/');
      path.extend_from_slice(&seg);
      let repo_path = slates_svn::authz::RepositoryPath::new(&path).unwrap();
      let tree_matched = rules.access(repo_path) == slates_svn::authz::Access::Read;
      let wild_matched = if pat_str == "*" || pat_str == "**" {
        true
      } else {
        super::Glob::new(pat_str.as_bytes()).matches(&seg)
      };
      prop_assert_eq!(
        tree_matched,
        wild_matched,
        "pattern={:?}, seg={:?}",
        pat_str,
        String::from_utf8_lossy(&seg)
      );
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 8 } else { 512 }),
      ..ProptestConfig::default()
    }))]

    /// Round-5 Blocker 3 (brute-force proptest oracle, depth <= 3 over complete 2-byte+complement
    /// universe): Do generate random multi-rule `Allow`/`Deny` glob policies (`*`, `**`, `?`, `??`,
    /// `a*`, `*a`, `[a]`, `[!a]`, literals across `Read`/`Submit`/`Rewrite`/`Delete`, owner and
    /// stranger). Expect: `svn_access` agrees with brute-force `SvnPolicy::verdict` enumeration on
    /// every generated policy.
    #[test]
    fn round5_blocker3_svn_access_agrees_with_brute_force_oracle(
      rules in prop::collection::vec(
        svn_oracle_rule(oracle_segment_piece(), two_byte_literal_piece(), 2),
        0..=5,
      ),
      is_owner in any::<bool>(),
    ) {
      let rec = record(slates_db::catalog::VcsTool::Subversion, rules.clone());
      let user = principal(if is_owner { OWNER } else { STRANGER });
      let paths = enumerate_paths(&TWO_BYTE_SEGMENTS, 3);
      let expected = brute_force_svn_access(&rec, &user, &paths);
      let actual = super::svn_access(&rec, &user).unwrap();
      prop_assert_eq!(actual, expected, "owner={}, rules={:?}", is_owner, rules);
    }

    /// Round-5 Blocker 3 (brute-force proptest oracle, 3-byte+complement universe with `[a-b]` and
    /// `a*`/`*b`): Do generate random multi-rule policies across `{a, b}` + complement `'d'`.
    /// Expect: `svn_access` agrees with brute-force `SvnPolicy::verdict` enumeration on every case.
    #[test]
    fn round5_blocker3_three_byte_alphabet_oracle(
      rules in prop::collection::vec(
        svn_oracle_rule(three_byte_segment_piece(), three_byte_literal_piece(), 1),
        0..=5,
      ),
      is_owner in any::<bool>(),
    ) {
      let rec = record(slates_db::catalog::VcsTool::Subversion, rules.clone());
      let user = principal(if is_owner { OWNER } else { STRANGER });
      let paths = enumerate_paths(&THREE_BYTE_SEGMENTS, 2);
      let expected = brute_force_svn_access(&rec, &user, &paths);
      let actual = super::svn_access(&rec, &user).unwrap();
      prop_assert_eq!(actual, expected, "owner={}, rules={:?}", is_owner, rules);
    }
  }

  /// Helper compiling `rules` into an `SvnPolicy` for direct `WitnessOutcome` checks.
  fn compiled_svn(rules: Vec<slates_db::catalog::PolicyRule>) -> SvnPolicy {
    let rec = record(slates_db::catalog::VcsTool::Subversion, rules);
    let super::Compiled::Subversion(policy) = super::Compiled::of(&rec).unwrap() else {
      panic!("expected Subversion policy");
    };
    policy
  }

  /// Round-6 Blocker 2 (repro `repro_witness_budget_lockout.py` Cases A/B/C + typed
  /// `WitnessOutcome` on PSPACE-hard multi-deny): Do evaluate 0-deny nondeterministic allow globs
  /// (`/*a?????????????????`, `Allow Read /` + `Allow Submit /*a?????????????????`,
  /// `/**/a/*/*/.../*` with 17 `*`), dual complex allow globs, eclipsed complex globs, and a
  /// pathological self-negated 17-wildcard deny glob. Expect: Cases A/B/C and dual complex allow
  /// globs find witnesses (`WitnessOutcome::Found`) without exhausting `WITNESS_STATE_BUDGET`;
  /// eclipsed globs prove `WitnessOutcome::Empty`; and a genuinely PSPACE-hard deny reports
  /// `WitnessOutcome::BudgetExceeded` and fails closed to `SvnAccess::None`.
  #[test]
  fn round6_blocker2_nfa_allow_dfa_deny_solves_cases_a_b_c_and_reports_budget_outcome() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho};
    let allow_r =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone);
    let deny_r =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Deny, PolicyWho::Anyone);
    let allow_s = |p: &str| {
      svn_path_rule(
        p,
        PolicyRight::Submit,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      )
    };
    let pat_17_q = ":glob:/*a?????????????????";
    let pat_17_q_b = ":glob:/*b?????????????????";
    let pat_16_q_b = ":glob:/*b????????????????";
    let pat_17_seg = ":glob:/**/a/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*";

    assert_svn_access_cases(&[
      (
        "Case A: single 0-deny *a?{17} Read",
        vec![allow_r(pat_17_q)],
        super::SvnAccess::Read,
      ),
      (
        "Case B: Read / + 0-deny Submit *a?{17}",
        vec![allow_r("/"), allow_s(pat_17_q)],
        super::SvnAccess::Write,
      ),
      (
        "Case C: single 0-deny /**/a/*^17 Read",
        vec![allow_r(pat_17_seg)],
        super::SvnAccess::Read,
      ),
      (
        "Overlapping complex 0-deny allows: Read *a?{17} + Submit *b?{16}",
        vec![allow_r(pat_17_q), allow_s(pat_16_q_b)],
        super::SvnAccess::Write,
      ),
      (
        "Disjoint complex 0-deny allows: Read *a?{17} + Submit *b?{17} (18th-from-end conflict)",
        vec![allow_r(pat_17_q), allow_s(pat_17_q_b)],
        super::SvnAccess::Read,
      ),
      (
        "Complex allow eclipsed by Deny /*",
        vec![allow_r(pat_17_q), deny_r(":glob:/*")],
        super::SvnAccess::None,
      ),
    ]);

    let stranger = principal(STRANGER);
    assert_eq!(
      compiled_svn(vec![allow_r(pat_17_q)]).witness_satisfying(&stranger, &[(Right::Read, false)]),
      super::WitnessOutcome::Found
    );
    assert_eq!(
      compiled_svn(vec![allow_r(pat_17_q), allow_s(pat_17_q_b)])
        .witness_satisfying(&stranger, &[(Right::Read, false), (Right::Submit, false)]),
      super::WitnessOutcome::Empty
    );
    assert_eq!(
      compiled_svn(vec![allow_r(pat_17_q), deny_r(":glob:/*")])
        .witness_satisfying(&stranger, &[(Right::Read, false)]),
      super::WitnessOutcome::Empty
    );
    // Pathological self-negated 17-wildcard Deny forces powerset complementation > budget.
    let pathological = vec![allow_r(pat_17_q), deny_r(pat_17_q)];
    assert_eq!(
      compiled_svn(pathological.clone()).witness_satisfying(&stranger, &[(Right::Read, false)]),
      super::WitnessOutcome::BudgetExceeded
    );
    assert_svn_access_cases(&[(
      "BudgetExceeded fails closed to SvnAccess::None",
      pathological,
      super::SvnAccess::None,
    )]);
  }

  /// Independent standard-library oracle for whether `bytes` is a valid non-empty slash-free UTF-8
  /// Subversion path segment.
  fn std_valid_utf8_segment(bytes: &[u8]) -> bool {
    !bytes.is_empty()
      && !bytes.contains(&0)
      && !bytes.contains(&b'/')
      && std::str::from_utf8(bytes).is_ok()
  }

  /// Round-6 Blocker 3 (exhaustive RFC 3629 UTF-8 validator DFA oracle): Do test
  /// `is_valid_utf8_segment` against `std::str::from_utf8` across ALL 256 1-byte sequences, ALL
  /// 65,536 2-byte sequences, and critical 3-byte/4-byte boundary vectors (overlong `E0`/`F0`,
  /// UTF-16 surrogates `ED A0..=BF`, `> U+10FFFF` `F4 90..=BF`/`F5..=FF`). Expect: 100% exact
  /// agreement on every byte sequence.
  #[test]
  fn round6_blocker3_utf8_dfa_matches_std_str_from_utf8() {
    for b0 in 0u8..=u8::MAX {
      assert_eq!(
        super::is_valid_utf8_segment(&[b0]),
        std_valid_utf8_segment(&[b0]),
        "1-byte mismatch for 0x{b0:02x}"
      );
      for b1 in 0u8..=u8::MAX {
        assert_eq!(
          super::is_valid_utf8_segment(&[b0, b1]),
          std_valid_utf8_segment(&[b0, b1]),
          "2-byte mismatch for [0x{b0:02x}, 0x{b1:02x}]"
        );
      }
    }
    let multi_byte_cases: &[&[u8]] = &[
      &[0xE0, 0x80, 0x80],
      &[0xE0, 0x9F, 0xBF],
      &[0xE0, 0xA0, 0x80],
      &[0xE0, 0xBF, 0xBF],
      &[0xED, 0x9F, 0xBF],
      &[0xED, 0xA0, 0x80],
      &[0xED, 0xBF, 0xBF],
      &[0xEE, 0x80, 0x80],
      &[0xEF, 0xBF, 0xBF],
      &[0xF0, 0x80, 0x80, 0x80],
      &[0xF0, 0x8F, 0xBF, 0xBF],
      &[0xF0, 0x90, 0x80, 0x80],
      &[0xF1, 0x80, 0x80, 0x80],
      &[0xF4, 0x8F, 0xBF, 0xBF],
      &[0xF4, 0x90, 0x80, 0x80],
      &[0xF5, 0x80, 0x80, 0x80],
      b"a\xC2\x80",
      b"a\xE2\x82\xAC",
      b"a\xF0\x9F\x98\x80",
      b"a\xC2",
      b"a\x80",
    ];
    for &sample in multi_byte_cases {
      assert_eq!(
        super::is_valid_utf8_segment(sample),
        std_valid_utf8_segment(sample),
        "multi-byte mismatch for {sample:02x?}"
      );
    }
  }

  /// Round-6 Blocker 3 (repro `repro_non_utf8_phantom_witness.py` + positive multi-byte UTF-8
  /// witnesses): Do evaluate `Allow :glob:/?` minus `Deny :glob:/[[:cntrl:][:print:]]` (Read and
  /// Read+Submit), single-byte non-ASCII bracket `/[é]`, and positive 2/3/4-byte UTF-8 glob
  /// complements (`/??`, `/???`, `/????`, `/[é][é]`). Expect: zero-valid-UTF-8 policies return
  /// `SvnAccess::None` (`WitnessOutcome::Empty`), while 2/3/4-byte UTF-8 witnesses (`"\u{0080}"`,
  /// `"é"`, `"€"`, `"\u{10000}"`) are found.
  #[test]
  fn round6_blocker3_rejects_non_utf8_phantom_witness_and_finds_multibyte_utf8() {
    use slates_db::catalog::{PolicyEffect, PolicyRight, PolicyWho};
    let allow_r =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Allow, PolicyWho::Anyone);
    let deny_r =
      |p: &str| svn_path_rule(p, PolicyRight::Read, PolicyEffect::Deny, PolicyWho::Anyone);
    let allow_s = |p: &str| {
      svn_path_rule(
        p,
        PolicyRight::Submit,
        PolicyEffect::Allow,
        PolicyWho::Anyone,
      )
    };
    let deny_s = |p: &str| {
      svn_path_rule(
        p,
        PolicyRight::Submit,
        PolicyEffect::Deny,
        PolicyWho::Anyone,
      )
    };
    let ascii = "[[:cntrl:][:print:]]";
    let ascii2 = format!(":glob:/{ascii}{ascii}");
    let ascii3 = format!(":glob:/{ascii}{ascii}{ascii}");
    let ascii4 = format!(":glob:/{ascii}{ascii}{ascii}{ascii}");

    assert_svn_access_cases(&[
      (
        "Repro: Allow Read+Submit /? minus Deny Read+Submit /[[:cntrl:][:print:]] -> None",
        vec![
          allow_r(":glob:/?"),
          deny_r(&format!(":glob:/{ascii}")),
          allow_s(":glob:/?"),
          deny_s(&format!(":glob:/{ascii}")),
        ],
        super::SvnAccess::None,
      ),
      (
        "1-byte Read /? minus ASCII -> None",
        vec![allow_r(":glob:/?"), deny_r(&format!(":glob:/{ascii}"))],
        super::SvnAccess::None,
      ),
      (
        "2-byte UTF-8 witness: Allow Read /?? minus 2x ASCII -> Read (\\u{0080})",
        vec![allow_r(":glob:/??"), deny_r(&ascii2)],
        super::SvnAccess::Read,
      ),
      (
        "2-byte UTF-8 Write witness: Read+Submit /?? minus 2x ASCII -> Write",
        vec![
          allow_r(":glob:/??"),
          deny_r(&ascii2),
          allow_s(":glob:/??"),
          deny_s(&ascii2),
        ],
        super::SvnAccess::Write,
      ),
      (
        "3-byte UTF-8 witness: Allow Read /??? minus 3x ASCII -> Read",
        vec![allow_r(":glob:/???"), deny_r(&ascii3)],
        super::SvnAccess::Read,
      ),
      (
        "4-byte UTF-8 witness: Allow Read /???? minus 4x ASCII -> Read",
        vec![allow_r(":glob:/????"), deny_r(&ascii4)],
        super::SvnAccess::Read,
      ),
      (
        "Single-byte non-ASCII bracket /[é] has no 1-byte valid UTF-8 witness",
        vec![allow_r(":glob:/[é]")],
        super::SvnAccess::None,
      ),
      (
        "Two-byte bracket /[é][é] matches valid UTF-8 \"é\"",
        vec![allow_r(":glob:/[é][é]")],
        super::SvnAccess::Read,
      ),
    ]);
  }

  /// Complete valid RFC 3629 UTF-8 segment universe for `utf8_oracle_segment_piece()` covering
  /// 1-byte ASCII (`a`, `d`), 2-byte UTF-8 (`\u{0080}`, `é`), 3-byte UTF-8 (`€`), and length-3/4
  /// combinations.
  const UTF8_ORACLE_SEGMENTS: [&[u8]; 22] = [
    b"a",
    b"d",
    b"aa",
    b"ad",
    b"da",
    b"dd",
    "\u{0080}".as_bytes(),
    "é".as_bytes(),
    b"aaa",
    b"add",
    "a\u{0080}".as_bytes(),
    "\u{0080}a".as_bytes(),
    "d\u{0080}".as_bytes(),
    "\u{0080}d".as_bytes(),
    "aé".as_bytes(),
    "éa".as_bytes(),
    "€".as_bytes(),
    b"aaaa",
    b"aadd",
    "\u{0080}\u{0080}".as_bytes(),
    "a€".as_bytes(),
    "€a".as_bytes(),
  ];

  fn utf8_oracle_segment_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
      "a",
      "\u{0080}",
      "*",
      "**",
      "?",
      "??",
      "???",
      "[[:cntrl:][:print:]]",
      "[[:cntrl:][:print:]][[:cntrl:][:print:]]",
      "[!a]",
      "a*",
    ])
  }

  fn utf8_oracle_literal_piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["a", "\u{0080}"])
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(if cfg!(miri) { 8 } else { 512 }),
      ..ProptestConfig::default()
    }))]

    /// Round-6 Blocker 3 (valid-UTF-8 multi-byte brute-force proptest oracle): Do generate random
    /// multi-rule policies mixing `?`, `??`, `???`, `[[:cntrl:][:print:]]`, `[!a]`, `a*`, `*`,
    /// `**`, ASCII `"a"`, and 2-byte UTF-8 `"\u{0080}"`. Expect: the NFA-Allow × DFA-Deny × UTF-8
    /// witness solver agrees 100% with brute-force `SvnPolicy::verdict` enumeration over valid
    /// UTF-8 paths.
    #[test]
    fn round6_blocker3_utf8_multibyte_proptest_oracle(
      rules in prop::collection::vec(
        svn_oracle_rule(utf8_oracle_segment_piece(), utf8_oracle_literal_piece(), 1),
        0..=5,
      ),
      is_owner in any::<bool>(),
    ) {
      let rec = record(slates_db::catalog::VcsTool::Subversion, rules.clone());
      let user = principal(if is_owner { OWNER } else { STRANGER });
      let paths = enumerate_paths(&UTF8_ORACLE_SEGMENTS, 2);
      let expected = brute_force_svn_access(&rec, &user, &paths);
      let actual = super::svn_access(&rec, &user).unwrap();
      prop_assert_eq!(actual, expected, "owner={}, rules={:?}", is_owner, rules);
    }
  }
}
