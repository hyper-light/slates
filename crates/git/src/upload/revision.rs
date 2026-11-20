//! A `sparse:oid` filter's `<blob-ish>` named as git's `get_oid_with_context` names one in a repository with no work
//! tree, and what git writes to its error stream as it reads the name (A-640 part 4; git v2.50.1's `object-name.c`
//! `get_oid_with_context_1`, `get_oid_1`, `get_oid_basic`, `get_describe_name`, `ref_and_count_parts_valid`,
//! `get_short_oid`, `update_candidates`, `finish_object_disambiguation`, the `disambiguate_*_only` hints,
//! `show_ambiguous_object`, `sort_ambiguous` and `resolve_relative_path`; `refs.c` `expand_ref`; `advice.c` `vadvise`;
//! `pretty.c` `format_subject` and `skip_blank_lines`; `date.c` `show_date`; `tree-walk.c` `get_tree_entry`; held to
//! stock git's own answers in `fixtures/fetch-sparse-*`).
//!
//! **The name.** The whole name is read as a revision first; then a name beginning `:` as an index path, which a served
//! repository has none of, so it names nothing (`:/<text>`, a search of commit messages, is refused by kind); then a
//! revision and the path after the name's first `:` outside a `@{...}` or `^{...}`: the revision peeled through tags
//! and a commit to its tree, then the path walked as `find_tree_entry` walks it (`StoreGraph::tree_entry`). A path
//! beginning `./` or `../` needs a work tree, which a served repository has none of.
//!
//! **A revision** (`get_oid_1`), its suffix read first:
//! - a parent's or a first-parent ancestor's (`<rev>^<n>`, `<rev>~<n>`; `get_parent`, `get_nth_ancestor`): the
//!   revision read for a commit, through tags (`lookup_commit_reference`: git's error for an object that is no commit),
//!   then its parent or ancestor; a count past `INT_MAX` names nothing;
//! - a peel (`<rev>^{commit|tag|tree|blob|object}`, `^{}`, `^{/}`; `peel_onion`, `repo_peel_to_type`, `deref_tag`): the
//!   object peeled through tags and from a commit to its tree to the kind asked (git's error when it meets a tree or a
//!   blob of another), any object, what a chain of tags ends at, or the commit itself for an empty search; git reads
//!   the words after the last `^{` as a prefix, so `^{commit}x}` peels to a commit;
//! - a log's entry (`<rev>@{<n>}`, `@{<date>}`; `get_oid_basic`, `read_ref_at`) in a repository that keeps no log, as a
//!   bare one keeps none: `@{0}` names `HEAD`'s branch's commit, any other count or a date of it is git's death of the
//!   empty log (git reads words as a date as `approxidate_alpha` does), and a named revision's, or `@{-<n>}`, nothing;
//! - a branch mark (`@{upstream}`, `@{u}`, `@{push}`, in any case, anywhere a ref is looked up): git's death, the
//!   branch's upstream and push destination being configured for none;
//! - an object's full name in the repository's format, either case, held or not (`get_oid_basic`); when a ref has that
//!   name too, git warns "refname '<name>' is ambiguous." and prints its advice of refs named like an object;
//! - a ref by git's patterns, `@` naming `HEAD` (`refs::dwim`): the first pattern's, git warning the same way when
//!   another pattern finds a ref too, or when the name also abbreviates exactly one object's name;
//! - `git describe`'s name, `<ref>-<count>-g<abbreviation>`: the last `-g` whose part before it is a ref name git
//!   accepts, a `-` and hexadecimal digits, then its abbreviation read quietly for a commit;
//! - an abbreviated name (`get_short_oid`: 4 digits to 64, either case, the store's search by prefix): the one object
//!   whose name its digits begin; of several, the one the lookup's hint takes when exactly one does (a blob for the
//!   filter's own name, a tree or what peels to one for the revision before a path, a commit for `git describe`'s);
//!   otherwise it is ambiguous and names nothing.
//!
//! **What git writes** of an ambiguous abbreviation, but in a quiet lookup (`git describe`'s; the check behind the
//! warning above): "error: short object ID <digits> is ambiguous", then its advice, "hint: The candidates are:" and a
//! line for each candidate: those the hint takes when two or more do, every one otherwise; tags first, then commits,
//! trees and blobs, each kind in name order; each by its shortest unique abbreviation (`StoreGraph::unique_digits`)
//! and what it is: a commit's author date in the author's own zone and its subject (its first paragraph's lines,
//! each without its trailing white space, joined by spaces, blank lines before it skipped), a tag's tagger date in UTC
//! and its name, a tree or a blob. A `warning:` or `error:` line is formatted as `vfreportf` formats one; the advice of
//! refs named like an object, and each advice line, is one print. Each is charged to the request's admission as it is
//! made, and kept as git's server reads it (`answer::Said`): git reads the candidates between the `error:` line and its
//! advice, and the object a name names, or a tree on a path, after the name.
//!
//! A search of commit messages (`:/<text>`, `^{/<text>}`) stops the reading, refused by kind until a later slice.
//!
//! **Not git's.** git takes a commit's subject from its message re-encoded to UTF-8 by its `encoding` header; this
//! server takes the bytes as they are. git dies of a date before 1970 in its own zone, and writes a year past 9999 in
//! more digits; this server writes the first in the proleptic calendar and the second as the epoch's date.

use super::answer::{ERROR, Said, WARNING, reported};
use super::{Served, refs};
use crate::lfs::Timestamp;
use crate::negotiation::Graph;
use crate::object_name::{Kind, ObjectId};
use crate::objects::commit::Commit;
use crate::objects::tag::Tag;
use crate::objects::{Ident, ObjectRefused, Refusal as ParseRefusal};
use crate::refname;
use crate::store::{Abbrev, StoreGraph};
use crate::{Admission, PastAdmission, to_u64};

/// Format: the byte between a revision and its path, and before an index path.
const COLON: u8 = b':';

/// Format: what a search of commit messages begins with (`:/<text>`).
const SEARCH: &[u8] = b":/";

/// Format: the stages an index path may name (`:<stage>:<path>`).
const STAGES: core::ops::RangeInclusive<u8> = b'0'..=b'3';

/// Format: what a relative path begins with (`resolve_relative_path`).
const RELATIVE: [&[u8]; 2] = [b"./", b"../"];

/// Format: the bytes before a `{` that open a braced suffix, within which a `:` splits nothing.
const BRACED: &[u8] = b"@^";

/// Format: a braced suffix's opening and closing bytes.
const BRACES: (u8, u8) = (b'{', b'}');

/// Format: the byte that, last but a revision's digits, names a parent (`get_oid_1`, `get_parent`).
const PARENT: u8 = b'^';

/// Format: the byte that, last but a revision's digits, names a first-parent ancestor (`get_nth_ancestor`).
const ANCESTOR: u8 = b'~';

/// Format: the most a parent's or an ancestor's count may be (`get_oid_1`: `INT_MAX`).
const MOST_COUNT: u32 = i32::MAX.unsigned_abs();

/// Format: the base of a count's digits.
const COUNT_RADIX: u32 = 10;

/// Format: the digit zero, a count's least digit and the count of a log's current entry (`@{0}`).
const ZERO: u8 = b'0';

/// Format: what opens a peel after a revision (`peel_onion`).
const PEEL_OPEN: &[u8] = b"^{";

/// Format: the shortest name that holds a peel (`peel_onion`: "len < 4"): a byte, `^{` and `}`.
const PEEL_LEAST: usize = 4;

/// Format: each peel's words after `^{` (`peel_onion`'s `starts_with` tests): the kinds, any object, none (what tags
/// name), and a search.
const PEELS: [(&[u8], Peel); 7] = [
  (b"commit}", Peel::To(Kind::Commit)),
  (b"tag}", Peel::To(Kind::Tag)),
  (b"tree}", Peel::To(Kind::Tree)),
  (b"blob}", Peel::To(Kind::Blob)),
  (b"object}", Peel::Object),
  (b"}", Peel::Untagged),
  (b"/", Peel::Search),
];

/// Format: git's error for a peel that meets an object of another kind (`repo_peel_to_type`), around the name, the kind
/// asked and the kind met.
const PEEL_MISMATCH: [&[u8]; 3] = [
  b": expected ",
  b" type, but the object dereferences to ",
  b" type",
];

/// Format: git's error for an object that is no commit where a commit is asked (`object_as_type`), around the object's
/// name and its kind.
const NOT_A_COMMIT: [&[u8]; 3] = [b"object ", b" is a ", b", not a commit"];

/// Format: what opens a log's count or date, or a branch mark, after a revision.
const AT_BRACE: &[u8] = b"@{";

/// Format: how far before a name's end git begins its search for `@{` (`get_oid_basic`: `len - 4`).
const AT_BRACE_FROM_END: usize = 4;

/// Format: what follows `@{` for a branch checked out before (`@{-<n>}`).
const PRIOR: u8 = b'-';

/// Format: the marks that ask a branch's upstream or its push destination (`upstream_mark`, `push_mark`), read in any
/// case as a name's prefix.
const MARKS: [&[u8]; 3] = [b"@{upstream}", b"@{u}", b"@{push}"];

/// Format: what begins a branch's full name (`branch_get`).
const BRANCHES: &[u8] = b"refs/heads/";

/// Format: the months' names, whose first three letters or more a date may write (`date.c` `month_names`,
/// `approxidate_alpha`).
const MONTHS: [&[u8]; 12] = [
  b"January",
  b"February",
  b"March",
  b"April",
  b"May",
  b"June",
  b"July",
  b"August",
  b"September",
  b"October",
  b"November",
  b"December",
];

/// Format: the fewest letters of a month's name a date must write (`approxidate_alpha`: "match >= 3").
const MONTH_LEAST: usize = 3;

/// Format: the words git reads as a date whole (`date.c` `special`).
const SPECIAL_DATES: [&[u8]; 8] = [
  b"yesterday",
  b"noon",
  b"midnight",
  b"tea",
  b"PM",
  b"AM",
  b"never",
  b"now",
];

/// Format: the numbers' names git reads in a date, one to ten (`date.c` `number_name`), and `last`, which it reads as
/// one.
const NUMBER_NAMES: [&[u8]; 11] = [
  b"one", b"two", b"three", b"four", b"five", b"six", b"seven", b"eight", b"nine", b"ten", b"last",
];

/// Format: what `git describe` writes between its count and its abbreviation (`get_describe_name`).
const DESCRIBE: &[u8] = b"-g";

/// Format: what `git describe` writes between its ref and its count (`ref_and_count_parts_valid`).
const COUNT_DASH: &[u8] = b"-";

/// Format: the revision git reads as `HEAD` (`interpret_empty_at`).
const AT: &[u8] = b"@";

/// Format: the byte that begins a branch mark or a log's suffix.
const AT_BYTE: u8 = b'@';

/// Format: `HEAD`'s name.
const HEAD: &[u8] = b"HEAD";

/// Format: git's warning when a name names a ref and something else (`get_oid_basic`'s `warn_msg`), around the name.
const AMBIGUOUS_REF: [&[u8]; 2] = [b"refname '", b"' is ambiguous."];

/// Format: git's advice after that warning when the name is an object's full name too (`get_oid_basic`'s
/// `object_name_msg`, printed with a newline after it; `advice.objectNameWarning` is on unless configured off).
const OBJECT_NAME_ADVICE: &str = concat!(
  "Git normally never creates a ref that ends with 40 hex characters\n",
  "because it will be ignored when you just specify 40-hex. These refs\n",
  "may be created by mistake. For example,\n",
  "\n",
  "  git switch -c $br $(git rev-parse ...)\n",
  "\n",
  "where \"$br\" is somehow empty and a 40-hex ref is created. Please\n",
  "examine these refs and maybe delete them. Turn this message off by\n",
  "running \"git config set advice.objectNameWarning false\"\n",
);

/// Format: git's error when an abbreviated name names more than one object (`get_short_oid`), around its digits.
const AMBIGUOUS_SHORT: [&[u8]; 2] = [b"short object ID ", b" is ambiguous"];

/// Format: what begins each line of git's advice (`vadvise`), and the space after it before a line with text.
const HINT: [&[u8]; 2] = [b"hint:", b" "];

/// Format: the advice's first line, before its candidates (`get_short_oid`: "The candidates are:\n%s").
const CANDIDATES: &[u8] = b"The candidates are:";

/// Format: what begins each candidate's line within the advice (`show_ambiguous_object`: "  %s\n").
const CANDIDATE_INDENT: &[u8] = b"  ";

/// Format: what git writes of a commit after its abbreviation, before its date (`show_ambiguous_object`).
const COMMIT_WORDS: &[u8] = b" commit ";

/// Format: what git writes of a tag after its abbreviation, before its date.
const TAG_WORDS: &[u8] = b" tag ";

/// Format: what git writes between a commit's or a tag's date and its subject or name.
const DATE_DASH: &[u8] = b" - ";

/// Format: what git writes of a tree after its abbreviation.
const TREE_WORDS: &[u8] = b" tree";

/// Format: what git writes of a blob after its abbreviation.
const BLOB_WORDS: &[u8] = b" blob";

/// Format: what git writes of a tag it cannot parse after its abbreviation.
const BAD_TAG_WORDS: &[u8] = b" [bad tag, could not parse it]";

/// Format: the order git lists candidates' kinds in (`sort_ambiguous`: `type % 4`, tags first).
const LISTED_ORDER: [Kind; 4] = [Kind::Tag, Kind::Commit, Kind::Tree, Kind::Blob];

/// Format: the bytes git's `isspace` takes as white space, which a subject's lines lose at their ends (git's own
/// `sane_ctype`: space, tab, LF and CR).
const GIT_SPACE: &[u8] = b" \t\n\r";

/// Format: the byte that ends a line.
const NEWLINE: u8 = b'\n';

/// Format: what git puts between a subject's lines (`format_subject`'s separator for `%s`).
const SUBJECT_JOIN: &[u8] = b" ";

/// Format: the byte at which git, reading a message as a C string, stops.
const NUL: u8 = 0;

/// Format: a time zone's sign for a zone west of UTC, as an identity writes it (`+hhmm` or `-hhmm`).
const WEST: u8 = b'-';

/// Format: the base of a zone's digits.
const DECIMAL: i64 = 10;

/// Format: what splits a zone's `hhmm`, read as one number, into hours and minutes (`gm_time_t`).
const ZONE_HOURS: i64 = 100;

/// Format: the minutes of an hour.
const HOUR_MINUTES: i64 = 60;

/// Format: the seconds of a minute.
const MINUTE_SECONDS: i64 = 60;

/// What a `<blob-ish>` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Named {
  /// An object, held or not.
  Object(ObjectId),
  /// Nothing: `get_oid_with_context` finds no object.
  Nothing,
  /// A search of commit messages, which this server does not resolve yet.
  Form,
  /// A relative path, which needs a work tree.
  Relative,
  /// git dies reading the name: a branch's upstream or push destination asked, or a count or a date of a log the
  /// repository keeps none of.
  Died(Refusal),
}

/// Why reading a name stops short: past the request's admission, git dying as it reads it, or a search of commit
/// messages this server does not resolve yet.
#[derive(Debug)]
enum Stop {
  /// Past the request's admission.
  Past(PastAdmission),
  /// git dies so.
  Died(Refusal),
  /// A search of commit messages.
  Form,
}

impl From<PastAdmission> for Stop {
  fn from(past: PastAdmission) -> Stop {
    Stop::Past(past)
  }
}

/// What an abbreviated name's lookup is for (`get_short_oid`'s disambiguation hint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hint {
  /// Anything: no hint (a peel to a blob, a tag or any object, or to what a tag names).
  Any,
  /// A blob (`GET_OID_BLOB`: the filter's own name).
  Blob,
  /// A tree, a commit, or a tag that peels to either (`GET_OID_TREEISH`: the revision before a path, a peel to a tree).
  Treeish,
  /// A commit, or a tag that peels to one (`GET_OID_COMMITTISH`: a parent's or an ancestor's revision, a peel to a
  /// commit).
  Committish,
  /// A commit (`GET_OID_COMMIT`: `git describe`'s abbreviation).
  Commit,
}

/// What a peel asks for (`peel_onion`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Peel {
  /// An object of this kind, peeling tags and a commit to its tree on the way (`^{commit}`, `^{tag}`, `^{tree}`,
  /// `^{blob}`).
  To(Kind),
  /// Whatever the revision names (`^{object}`).
  Object,
  /// What a chain of tags ends at (`^{}`).
  Untagged,
  /// The newest commit from the revision's whose message matches (`^{/<text>}`), the revision itself for no text.
  Search,
}

/// What a name ending in `@{...}` asks (`get_oid_basic`'s reflog part): `None` when its last `@{` is a branch mark or
/// none closes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Logged<'n> {
  /// No reflog's suffix.
  None,
  /// `@{-<n>}` after a revision: git reads no such name.
  PriorNotFirst,
  /// A revision's log entry by a count or a date: the revision (empty for `HEAD`'s branch) and what the braces hold.
  Entry {
    /// The revision.
    base: &'n [u8],
    /// The count or the date.
    spec: &'n [u8],
  },
}

/// Whether a lookup by abbreviation writes git's lines when it is ambiguous (`GET_OID_QUIETLY` unset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loud {
  /// It writes them.
  Yes,
  /// It is quiet.
  No,
}

/// What git decides of an abbreviated name's objects (`finish_object_disambiguation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Short {
  /// No object's name begins with its digits.
  Missing,
  /// The object it names.
  Found(ObjectId),
  /// Several, none chosen: git lists every one (`all`), or only those the hint takes when two or more do.
  Ambiguous {
    /// Whether every one is listed.
    all: bool,
  },
}

/// An abbreviated name's objects, counted as `update_candidates` counts them with a hint: how many its digits begin and
/// the first, and how many of those the hint takes and the first; each count stops mattering past two.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
  /// The objects whose names begin with the digits.
  matched: usize,
  /// The first of them.
  first: Option<ObjectId>,
  /// Those the hint takes.
  taken: usize,
  /// The first of those.
  first_taken: Option<ObjectId>,
}

impl Tally {
  /// Whether more objects could change the decision: not once two match and, with a hint, two are taken.
  fn settled(&self, hint: Hint) -> bool {
    self.matched > 1 && (hint == Hint::Any || self.taken > 1)
  }

  /// git's decision (`update_candidates`, `finish_object_disambiguation`): none; one, whatever it is, unchecked; of
  /// several, the one the hint takes when exactly one does, and otherwise ambiguous.
  fn decided(&self, hint: Hint) -> Short {
    let Some(first) = self.first else {
      return Short::Missing;
    };
    if self.matched == 1 {
      return Short::Found(first);
    }
    if hint == Hint::Any {
      return Short::Ambiguous { all: true };
    }
    match (self.taken, self.first_taken) {
      (1, Some(taken)) => Short::Found(taken),
      (0, _) | (1, None) => Short::Ambiguous { all: true },
      _ => Short::Ambiguous { all: false },
    }
  }
}

/// A candidate git lists: where its kind is listed, its name and its kind.
type Candidate = (usize, ObjectId, Kind);

/// Reads a `<blob-ish>` as git does, keeping what git writes to its error stream as it reads it (module docs).
pub(super) struct Resolver<'a, 'g> {
  /// The repository's objects.
  graph: &'a StoreGraph<'g>,
  /// The repository and its refs.
  served: &'a Served<'g>,
  /// What git wrote, as its server reads it.
  said: Said,
}

impl<'a, 'g> Resolver<'a, 'g> {
  /// A reader over `graph` and `served`, having written nothing.
  pub(super) fn new(graph: &'a StoreGraph<'g>, served: &'a Served<'g>) -> Resolver<'a, 'g> {
    Resolver {
      graph,
      served,
      said: Said::default(),
    }
  }

  /// What git wrote to its error stream while it read, as its server reads it.
  pub(super) fn said(self) -> Said {
    self.said
  }

  /// What `name` names (`get_oid_with_context_1` for a blob): the whole name as a revision first; then an index path;
  /// then a revision and the path after its `:`. What git writes meanwhile is kept, each write charged to `admission`;
  /// git's death as it reads the name is answered as [`Named::Died`].
  pub(super) fn blob_ish(
    &mut self,
    name: &[u8],
    admission: &mut Admission,
  ) -> Result<Named, PastAdmission> {
    match self.named(name, admission) {
      Ok(named) => Ok(named),
      Err(Stop::Past(past)) => Err(past),
      Err(Stop::Died(refusal)) => Ok(Named::Died(refusal)),
      Err(Stop::Form) => Ok(Named::Form),
    }
  }

  /// [`Resolver::blob_ish`]'s reading, stopped by git's death or a search this server does not resolve yet.
  fn named(&mut self, name: &[u8], admission: &mut Admission) -> Result<Named, Stop> {
    if let Some(id) = self.revision(name, Hint::Blob, admission)? {
      return Ok(Named::Object(id));
    }
    if let Some(index_path) = name.strip_prefix(&[COLON]) {
      return Ok(in_index(name, index_path));
    }
    let Some((rev, path)) = split(name) else {
      return Ok(Named::Nothing);
    };
    let Some(id) = self.revision(rev, Hint::Treeish, admission)? else {
      return Ok(Named::Nothing);
    };
    if is_relative(path) {
      return Ok(Named::Relative);
    }
    self.said.read();
    Ok(
      tree_of(self.graph, &id)
        .and_then(|tree| walked(self.graph, tree, path))
        .map_or(Named::Nothing, Named::Object),
    )
  }

  /// The object the revision `rev` names (`get_oid_1`): a parent's or an ancestor's suffix first, then a peel, then an
  /// object's full name or a ref (`get_oid_basic`), `git describe`'s name, and an abbreviated name looked up for what
  /// `hint` takes. `None` when it names none.
  fn revision(
    &mut self,
    rev: &[u8],
    hint: Hint,
    admission: &mut Admission,
  ) -> Result<Option<ObjectId>, Stop> {
    if let Some((base, mark, count)) = suffix(rev) {
      return self.lineage(base, mark, count, admission);
    }
    if let Some(id) = self.peel(rev, admission)? {
      return Ok(Some(id));
    }
    if let Some(id) = self.basic(rev, admission)? {
      return Ok(Some(id));
    }
    if let Some(id) = self.described(rev, admission)? {
      return Ok(Some(id));
    }
    Ok(self.short(rev, hint, Loud::Yes, admission)?)
  }

  /// A parent's (`get_parent`) or a first-parent ancestor's (`get_nth_ancestor`) commit: `base` read for a commit, the
  /// commit it names, then its `count`-th parent (itself for none) or its `count`-th first-parent ancestor. `None` for
  /// a count past git's largest, a revision that names no commit, or a parent or an ancestor it does not have; the walk
  /// ends at the root, so it takes no more steps than the history is deep.
  fn lineage(
    &mut self,
    base: &[u8],
    mark: u8,
    count: Option<u32>,
    admission: &mut Admission,
  ) -> Result<Option<ObjectId>, Stop> {
    let Some(count) = count else {
      return Ok(None);
    };
    let Some(id) = self.revision(base, Hint::Committish, admission)? else {
      return Ok(None);
    };
    let Some(commit) = self.commit_named(&id, admission)? else {
      return Ok(None);
    };
    let found = if mark == PARENT {
      match count.checked_sub(1) {
        None => Some(commit),
        Some(index) => usize::try_from(index)
          .ok()
          .and_then(|index| self.graph.parents(commit).nth(index)),
      }
    } else {
      (0..count).try_fold(commit, |commit, _| self.graph.parents(commit).next())
    };
    Ok(found.map(|node| self.graph.id(node)))
  }

  /// The commit `id` names, by position, through a chain of tags (`lookup_commit_reference`): `None` for an object the
  /// store does not hold, and, after git's error naming what it is, for one that is no commit.
  fn commit_named(
    &mut self,
    id: &ObjectId,
    admission: &mut Admission,
  ) -> Result<Option<u64>, PastAdmission> {
    let Some(node) = self.graph.node(id) else {
      return Ok(None);
    };
    self.said.read();
    let Some(peeled) = untagged(self.graph, node) else {
      return Ok(None);
    };
    let kind = self.graph.kind(peeled);
    if kind == Kind::Commit {
      return Ok(Some(peeled));
    }
    let mut words = NOT_A_COMMIT[0].to_vec();
    self.graph.id(peeled).write_hex(&mut words);
    words.extend_from_slice(NOT_A_COMMIT[1]);
    words.extend_from_slice(kind.name());
    words.extend_from_slice(NOT_A_COMMIT[2]);
    self.say(reported(ERROR, &words), admission)?;
    Ok(None)
  }

  /// What a peel names (`peel_onion`): `None` when `rev` ends in none of git's peels (its last `^{`, closed by `}`),
  /// its revision names nothing the store holds, or its object peels to no object of the kind asked (after git's
  /// error). A search with text stops the reading ([`Stop::Form`]); with none it names the commit.
  fn peel(&mut self, rev: &[u8], admission: &mut Admission) -> Result<Option<ObjectId>, Stop> {
    if rev.len() < PEEL_LEAST || rev.last() != Some(&BRACES.1) {
      return Ok(None);
    }
    let Some(open) = (1..rev.len()).rev().find(|at| {
      rev
        .get(at.saturating_sub(1)..at.saturating_add(1))
        .is_some_and(|pair| pair == PEEL_OPEN)
    }) else {
      return Ok(None);
    };
    let words = rev.get(open.saturating_add(1)..).unwrap_or_default();
    let Some(peel) = PEELS
      .iter()
      .find(|(prefix, _)| words.starts_with(prefix))
      .map(|(_, peel)| *peel)
    else {
      return Ok(None);
    };
    let base = rev.get(..open.saturating_sub(1)).unwrap_or_default();
    let base_hint = match peel {
      Peel::To(Kind::Commit) | Peel::Search => Hint::Committish,
      Peel::To(Kind::Tree) => Hint::Treeish,
      Peel::To(_) | Peel::Object | Peel::Untagged => Hint::Any,
    };
    let Some(outer) = self.revision(base, base_hint, admission)? else {
      return Ok(None);
    };
    let Some(node) = self.graph.node(&outer) else {
      return Ok(None);
    };
    self.said.read();
    let peeled = match peel {
      Peel::Object => Some(node),
      Peel::Untagged => untagged(self.graph, node),
      Peel::To(kind) => self.peeled_to(rev, node, kind, admission)?,
      Peel::Search => {
        let commit = self.peeled_to(rev, node, Kind::Commit, admission)?;
        if commit.is_some() && words.get(1) != Some(&BRACES.1) {
          return Err(Stop::Form);
        }
        commit
      }
    };
    Ok(peeled.map(|node| self.graph.id(node)))
  }

  /// The object of `wanted`'s kind that the object at `node` peels to (`repo_peel_to_type`): itself when of that kind,
  /// then through tags and from a commit to its tree; after git's error naming `rev`, `None` at a tree or a blob of
  /// another kind, and silently for a tag whose object the store does not hold.
  fn peeled_to(
    &mut self,
    rev: &[u8],
    node: u64,
    wanted: Kind,
    admission: &mut Admission,
  ) -> Result<Option<u64>, PastAdmission> {
    if self.graph.kind(node) == wanted {
      return Ok(Some(node));
    }
    let Some(mut node) = untagged(self.graph, node) else {
      return Ok(None);
    };
    if self.graph.kind(node) == Kind::Commit && wanted != Kind::Commit {
      match self.graph.commit(node) {
        Some(row) => node = row.tree,
        None => return Ok(None),
      }
    }
    let kind = self.graph.kind(node);
    if kind == wanted {
      return Ok(Some(node));
    }
    let words = [
      rev,
      PEEL_MISMATCH[0],
      wanted.name(),
      PEEL_MISMATCH[1],
      kind.name(),
      PEEL_MISMATCH[2],
    ]
    .concat();
    self.say(reported(ERROR, &words), admission)?;
    Ok(None)
  }

  /// `get_oid_basic`: an object's full name in the repository's format, warned of when a ref has that name too; a log's
  /// entry ([`Resolver::log_entry`]); or the first ref git's patterns find for `rev` (`@` read as `HEAD`), warned of
  /// when another pattern finds one too or `rev` abbreviates exactly one object's name, git dying first on a branch
  /// mark in it ([`Resolver::marks`]). `None` when it is none of them.
  fn basic(&mut self, rev: &[u8], admission: &mut Admission) -> Result<Option<ObjectId>, Stop> {
    let format = self.served.repository.format();
    if rev.len() == format.hex_len()
      && let Ok(id) = ObjectId::from_hex(format, rev)
    {
      if refs::dwim(self.graph, self.served, rev).found > 0 {
        self.warn_ambiguous(rev, admission)?;
        self.say(OBJECT_NAME_ADVICE.as_bytes().to_vec(), admission)?;
      }
      return Ok(Some(id));
    }
    match logged(rev) {
      Logged::PriorNotFirst => return Ok(None),
      Logged::Entry { base, spec } => return self.log_entry(base, spec, admission),
      Logged::None => {}
    }
    if ambiguous_path(rev) {
      return Ok(None);
    }
    self.marks(rev)?;
    let interpreted = if rev == AT { HEAD } else { rev };
    let dwim = refs::dwim(self.graph, self.served, interpreted);
    let Some(node) = dwim.first else {
      return Ok(None);
    };
    if dwim.found > 1 || self.abbreviates_one(rev) {
      self.warn_ambiguous(rev, admission)?;
    }
    Ok(Some(self.graph.id(node)))
  }

  /// A log's entry (`get_oid_basic` with a reflog's suffix) in a repository that keeps no log, as a bare one keeps
  /// none: for an empty `base`, `HEAD`'s branch's current commit for a count of zero, git's death of the empty log for
  /// any other count or a date (`read_ref_at`), and nothing for words git reads as no date; for a named revision
  /// nothing, as no log of it is found (`repo_dwim_log`), git dying first on a branch mark in it.
  fn log_entry(
    &mut self,
    base: &[u8],
    spec: &[u8],
    admission: &mut Admission,
  ) -> Result<Option<ObjectId>, Stop> {
    if !base.is_empty() {
      if !ambiguous_path(base) {
        self.marks(base)?;
      }
      return Ok(None);
    }
    let dwim = refs::dwim(self.graph, self.served, HEAD);
    let Some(node) = dwim.first else {
      return Ok(None);
    };
    if dwim.found > 1 {
      self.warn_ambiguous(base, admission)?;
    }
    let counted = spec.iter().all(u8::is_ascii_digit);
    if counted && spec.iter().all(|digit| *digit == ZERO) {
      return Ok(Some(self.graph.id(node)));
    }
    if counted || is_date(spec) {
      return Err(Stop::Died(Refusal::SparseEmptyLog {
        refname: self.served.head.to_vec(),
      }));
    }
    Ok(None)
  }

  /// Dies as git's `substitute_branch_name` dies on a branch mark in `name` (`repo_interpret_branch_name`,
  /// `interpret_branch_mark`): at each `@` from the first, `@` alone or before `@{` reads as `HEAD`
  /// (`interpret_empty_at`), and an upstream or push mark with no `:` before it asks of the branch before it (`HEAD`'s
  /// for none), which a served repository configures neither for.
  fn marks(&self, name: &[u8]) -> Result<(), Stop> {
    for (at, _) in name
      .iter()
      .enumerate()
      .filter(|(_, byte)| **byte == AT_BYTE)
    {
      if at == 0 && empty_at(name) {
        return self.marks(&[HEAD, name.get(1..).unwrap_or_default()].concat());
      }
      let before = name.get(..at).unwrap_or_default();
      if is_mark(name.get(at..).unwrap_or_default()) && !before.contains(&COLON) {
        return Err(Stop::Died(self.no_upstream(before)));
      }
    }
    Ok(())
  }

  /// git's death asking the upstream or the push destination of the branch `before` names (`branch_get`,
  /// `branch_get_upstream`; a push destination's default is the upstream): `HEAD`'s branch for none or `HEAD`; a
  /// `HEAD` that names no branch; a branch the repository does not hold; or one it holds, with no upstream configured.
  fn no_upstream(&self, before: &[u8]) -> Refusal {
    let branch = if before.is_empty() || before == HEAD {
      match self.served.head.strip_prefix(BRANCHES) {
        Some(branch) => branch,
        None => return Refusal::SparseDetached,
      }
    } else {
      before
    };
    let full = [BRANCHES, branch].concat();
    if refs::named(self.graph, self.served.refs, &full).is_some() {
      Refusal::SparseNoUpstream {
        branch: branch.to_vec(),
      }
    } else {
      Refusal::SparseNoBranch {
        branch: branch.to_vec(),
      }
    }
  }

  /// Whether `rev` abbreviates exactly one object's name (`get_short_oid` with `GET_OID_QUIETLY` alone).
  fn abbreviates_one(&self, rev: &[u8]) -> bool {
    Abbrev::parse(rev).is_some_and(|abbrev| {
      let mut objects = self.graph.abbreviated(&abbrev);
      objects.next().is_some() && objects.next().is_none()
    })
  }

  /// The object `git describe`'s name `rev` names (`get_describe_name`): the last `-g` with a ref and a count before it
  /// (`ref_and_count_parts_valid`), git looking no nearer the start than leaves a byte before it, then the digits after
  /// it read quietly for a commit. `None` when there is no such `-g` or its digits name nothing.
  fn described(
    &mut self,
    rev: &[u8],
    admission: &mut Admission,
  ) -> Result<Option<ObjectId>, PastAdmission> {
    let found = (DESCRIBE.len()..rev.len()).rev().find(|at| {
      let dash = at.saturating_sub(COUNT_DASH.len());
      rev.get(dash..at.saturating_add(1)) == Some(DESCRIBE)
        && rev.get(..dash).is_some_and(counted_ref)
    });
    let Some(digits) = found.and_then(|at| rev.get(at.saturating_add(1)..)) else {
      return Ok(None);
    };
    self.short(digits, Hint::Commit, Loud::No, admission)
  }

  /// The object the abbreviated name `digits` names for what `hint` takes (`get_short_oid`): `None` when it is no
  /// abbreviation, names nothing, or is ambiguous, git's lines for the last written when `loud` says so.
  fn short(
    &mut self,
    digits: &[u8],
    hint: Hint,
    loud: Loud,
    admission: &mut Admission,
  ) -> Result<Option<ObjectId>, PastAdmission> {
    let Some(abbrev) = Abbrev::parse(digits) else {
      return Ok(None);
    };
    match self.tally(&abbrev, hint).decided(hint) {
      Short::Missing => Ok(None),
      Short::Found(id) => Ok(Some(id)),
      Short::Ambiguous { all } => {
        if loud == Loud::Yes {
          self.list(&abbrev, hint, all, admission)?;
        }
        Ok(None)
      }
    }
  }

  /// The objects `abbrev`'s digits begin, counted until more could not change git's decision.
  fn tally(&self, abbrev: &Abbrev, hint: Hint) -> Tally {
    let mut tally = Tally::default();
    for (id, node, kind) in self.graph.abbreviated(abbrev) {
      tally.matched = tally.matched.saturating_add(1);
      tally.first.get_or_insert(id);
      if self.takes(hint, node, kind) {
        tally.taken = tally.taken.saturating_add(1);
        tally.first_taken.get_or_insert(id);
      }
      if tally.settled(hint) {
        break;
      }
    }
    tally
  }

  /// Whether `hint` takes the object at `node`, of `kind` (the `disambiguate_*_only` hints): a tag is peeled through
  /// every tag it names in turn for a commit's or a tree's hint.
  fn takes(&self, hint: Hint, node: u64, kind: Kind) -> bool {
    let peeled = || untagged(self.graph, node).map(|peeled| self.graph.kind(peeled));
    match hint {
      Hint::Any => true,
      Hint::Blob => kind == Kind::Blob,
      Hint::Commit => kind == Kind::Commit,
      Hint::Committish => peeled() == Some(Kind::Commit),
      Hint::Treeish => matches!(peeled(), Some(Kind::Tree | Kind::Commit)),
    }
  }

  /// Writes git's lines for an ambiguous abbreviation (`get_short_oid`): its error, then its advice listing every
  /// candidate (`all`) or those `hint` takes, in git's order, each described; the list charged to `admission` as it
  /// grows.
  fn list(
    &mut self,
    abbrev: &Abbrev,
    hint: Hint,
    all: bool,
    admission: &mut Admission,
  ) -> Result<(), PastAdmission> {
    let words = [AMBIGUOUS_SHORT[0], &abbrev.hex(), AMBIGUOUS_SHORT[1]].concat();
    self.say(reported(ERROR, &words), admission)?;
    self.said.read();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (id, node, kind) in self.graph.abbreviated(abbrev) {
      if all || self.takes(hint, node, kind) {
        admission.charge_items::<Candidate>(1)?;
        let rank = LISTED_ORDER
          .iter()
          .position(|listed| *listed == kind)
          .unwrap_or(LISTED_ORDER.len());
        candidates.push((rank, id, kind));
      }
    }
    candidates.sort_unstable_by_key(|(rank, id, _)| (*rank, *id));
    self.say(hinted(CANDIDATES), admission)?;
    for (_, id, kind) in candidates {
      let line = [CANDIDATE_INDENT, &self.candidate(&id, kind, admission)?].concat();
      self.say(hinted(&line), admission)?;
    }
    Ok(())
  }

  /// What git writes of a candidate (`show_ambiguous_object`): its shortest unique abbreviation, then a commit's author
  /// date and subject, a tag's date and name, or its kind.
  fn candidate(
    &self,
    id: &ObjectId,
    kind: Kind,
    admission: &mut Admission,
  ) -> Result<Vec<u8>, PastAdmission> {
    let mut line = Vec::new();
    id.write_hex(&mut line);
    line.truncate(self.graph.unique_digits(id));
    match kind {
      Kind::Commit => self.describe_commit(id, &mut line, admission)?,
      Kind::Tag => self.describe_tag(id, &mut line, admission)?,
      Kind::Tree => line.extend_from_slice(TREE_WORDS),
      Kind::Blob => line.extend_from_slice(BLOB_WORDS),
    }
    Ok(line)
  }

  /// A commit's words after its abbreviation: ` commit <author date> - <subject>`, the date in the author's zone. A
  /// commit the store cannot read or parse shows neither (git's format of a commit it cannot parse shows nothing).
  fn describe_commit(
    &self,
    id: &ObjectId,
    line: &mut Vec<u8>,
    admission: &mut Admission,
  ) -> Result<(), PastAdmission> {
    line.extend_from_slice(COMMIT_WORDS);
    let bytes = self
      .graph
      .object_bytes(id, Kind::Commit, admission)?
      .unwrap_or_default();
    let commit = parsed(Commit::parse(&bytes, admission))?;
    if let Some(commit) = &commit {
      put_short_date(commit.author(), Zone::Own, line);
    }
    line.extend_from_slice(DATE_DASH);
    if let Some(message) = commit.as_ref().and_then(Commit::message) {
      line.extend_from_slice(&subject(message));
    }
    Ok(())
  }

  /// A tag's words after its abbreviation: ` tag <tagger date> - <name>`, the date in UTC (`show_date` with no zone);
  /// or, for a tag git cannot parse (no name), its words for that.
  fn describe_tag(
    &self,
    id: &ObjectId,
    line: &mut Vec<u8>,
    admission: &mut Admission,
  ) -> Result<(), PastAdmission> {
    let bytes = self
      .graph
      .object_bytes(id, Kind::Tag, admission)?
      .unwrap_or_default();
    let tag = parsed(Tag::parse(&bytes, admission))?;
    let Some((tag, name)) = tag.and_then(|tag| tag.name().map(|name| (tag, name))) else {
      line.extend_from_slice(BAD_TAG_WORDS);
      return Ok(());
    };
    line.extend_from_slice(TAG_WORDS);
    put_short_date(tag.tagger(), Zone::Utc, line);
    line.extend_from_slice(DATE_DASH);
    line.extend_from_slice(name);
    Ok(())
  }

  /// Writes git's warning that `rev` names a ref and something else.
  fn warn_ambiguous(&mut self, rev: &[u8], admission: &mut Admission) -> Result<(), PastAdmission> {
    let words = [AMBIGUOUS_REF[0], rev, AMBIGUOUS_REF[1]].concat();
    self.say(reported(WARNING, &words), admission)
  }

  /// Keeps `write`, one write git makes to its error stream, charged to `admission` first.
  fn say(&mut self, write: Vec<u8>, admission: &mut Admission) -> Result<(), PastAdmission> {
    admission.charge(to_u64(write.len()))?;
    self.said.write(&write);
    Ok(())
  }
}

/// A parse's object, `None` for bytes git's parser refuses; refused only past the admission.
fn parsed<T>(parse: Result<T, ObjectRefused>) -> Result<Option<T>, PastAdmission> {
  match parse {
    Ok(object) => Ok(Some(object)),
    Err(ObjectRefused {
      refusal: ParseRefusal::PastAdmission(past),
      ..
    }) => Err(past),
    Err(_) => Ok(None),
  }
}

/// The zone a date is shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
  /// The identity's own (a commit's `%ad`).
  Own,
  /// UTC (a tag's date, which git shows with no zone).
  Utc,
}

/// Writes the date of the identity `ident` as git's short date mode shows it (`show_date` with `DATE_SHORT`):
/// `YYYY-MM-DD` of its instant moved into `zone`; an identity git's parser does not read shows the epoch's date, as
/// git shows an identity with no date.
fn put_short_date(ident: Option<&[u8]>, zone: Zone, out: &mut Vec<u8>) {
  let (seconds, offset) = ident
    .and_then(|line| Ident::parse(line).ok())
    .map_or((0, 0), |ident| {
      let offset = match zone {
        Zone::Own => zone_seconds(ident.zone),
        Zone::Utc => 0,
      };
      (ident.seconds, offset)
    });
  let local = i64::try_from(seconds)
    .ok()
    .and_then(|seconds| seconds.checked_add(offset));
  if let Some(instant) = local
    .and_then(Timestamp::from_unix)
    .or_else(|| Timestamp::from_unix(0))
  {
    instant.put_date(out);
  }
}

/// The seconds east of UTC of a zone written `+hhmm` or `-hhmm`, read as git reads it (`gm_time_t`: the digits as one
/// number, its hundreds hours and the rest minutes, west of UTC for `-`).
fn zone_seconds(zone: &[u8]) -> i64 {
  let value = zone
    .get(1..)
    .unwrap_or_default()
    .iter()
    .take_while(|byte| byte.is_ascii_digit())
    .fold(0_i64, |value, digit| {
      value
        .saturating_mul(DECIMAL)
        .saturating_add(i64::from(digit.saturating_sub(b'0')))
    });
  let minutes = value
    .checked_div(ZONE_HOURS)
    .unwrap_or_default()
    .saturating_mul(HOUR_MINUTES)
    .saturating_add(value.checked_rem(ZONE_HOURS).unwrap_or_default());
  let seconds = minutes.saturating_mul(MINUTE_SECONDS);
  if zone.first() == Some(&WEST) {
    seconds.saturating_neg()
  } else {
    seconds
  }
}

/// git's subject of a commit's `message` (`skip_blank_lines`, then `format_subject` with a space between lines): the
/// lines of its first paragraph, read as a C string to its first NUL, each without its trailing white space, joined by
/// single spaces.
fn subject(message: &[u8]) -> Vec<u8> {
  let text = message
    .split(|byte| *byte == NUL)
    .next()
    .unwrap_or_default();
  let mut subject = Vec::new();
  let paragraph = text
    .split_inclusive(|byte| *byte == NEWLINE)
    .map(trimmed)
    .skip_while(|line| line.is_empty())
    .take_while(|line| !line.is_empty());
  for line in paragraph {
    if !subject.is_empty() {
      subject.extend_from_slice(SUBJECT_JOIN);
    }
    subject.extend_from_slice(line);
  }
  subject
}

/// `line` without its trailing white space, as git's `isspace` reads it.
fn trimmed(line: &[u8]) -> &[u8] {
  let end = line
    .iter()
    .rposition(|byte| !GIT_SPACE.contains(byte))
    .map_or(0, |last| last.saturating_add(1));
  line.get(..end).unwrap_or_default()
}

/// A line of git's advice as `vadvise` prints it: `hint:`, a space before text that is not empty, the text, a newline.
fn hinted(text: &[u8]) -> Vec<u8> {
  let space: &[u8] = if text.is_empty() { b"" } else { HINT[1] };
  [HINT[0], space, text, &[NEWLINE]].concat()
}

/// What `:<index path>` names in a repository with no index: a search of commit messages is a form this server does
/// not resolve; a relative path needs a work tree; any other path names nothing. `name` is the whole name.
fn in_index(name: &[u8], index_path: &[u8]) -> Named {
  if name.len() > SEARCH.len() && name.starts_with(SEARCH) {
    return Named::Form;
  }
  let path = match index_path {
    [stage, COLON, rest @ ..] if STAGES.contains(stage) => rest,
    _ => index_path,
  };
  if is_relative(path) {
    Named::Relative
  } else {
    Named::Nothing
  }
}

/// Whether `path` is relative, as `resolve_relative_path` reads one.
fn is_relative(path: &[u8]) -> bool {
  RELATIVE.iter().any(|prefix| path.starts_with(prefix))
}

/// `name` split at its first `:` outside a `@{...}` or `^{...}`: the revision before it and the path after it.
fn split(name: &[u8]) -> Option<(&[u8], &[u8])> {
  let (open, close) = BRACES;
  let mut depth = 0_usize;
  let mut at = 0_usize;
  while let Some(&byte) = name.get(at) {
    let next = at.saturating_add(1);
    if BRACED.contains(&byte) && name.get(next) == Some(&open) {
      depth = depth.saturating_add(1);
      at = next;
    } else if depth > 0 && byte == close {
      depth = depth.saturating_sub(1);
    } else if depth == 0 && byte == COLON {
      return Some((name.get(..at)?, name.get(next..)?));
    }
    at = at.saturating_add(1);
  }
  None
}

/// Whether `head` is `<ref>-<count>` (`ref_and_count_parts_valid`): the count one hexadecimal digit or more, and the
/// ref a one-level name `check_refname_format` passes (its components' rule, and not `@` alone).
fn counted_ref(head: &[u8]) -> bool {
  let count = head
    .iter()
    .rev()
    .take_while(|byte| byte.is_ascii_hexdigit())
    .count();
  let Some(name) = head
    .get(..head.len().saturating_sub(count))
    .and_then(|rest| rest.strip_suffix(COUNT_DASH))
  else {
    return false;
  };
  count > 0 && name != AT && refname::tag_name_is_valid(name)
}

/// The tree `id` names, peeled as `read_object_with_reference` peels for a tree: the tree itself, a commit's root tree,
/// or what a chain of tags ends at. `None` for a blob, an object the store does not hold, and a chain of tags that
/// comes back on itself, which a store whose names are hashes cannot hold.
fn tree_of(graph: &StoreGraph<'_>, id: &ObjectId) -> Option<u64> {
  let node = graph.node(id)?;
  let node = match graph.kind(node) {
    Kind::Tag => refs::peeled_node(graph, node)?,
    _ => node,
  };
  match graph.kind(node) {
    Kind::Tree => Some(node),
    Kind::Commit => graph.commit(node).map(|row| row.tree),
    Kind::Tag | Kind::Blob => None,
  }
}

/// The object at `path` below the tree at `tree` (`get_tree_entry`): the tree itself for an empty path; otherwise
/// each entry git's walk finds (`Tree::find`), descending into a directory's tree, peeled as git peels it, while
/// path is left below it. `None` when an entry is not found, or its tree is not one the store holds. Each step takes a
/// component of `path`, so the walk ends within its length.
fn walked(graph: &StoreGraph<'_>, tree: u64, path: &[u8]) -> Option<ObjectId> {
  let mut tree = tree;
  let mut left = path;
  while !left.is_empty() {
    let (entry, below) = graph.tree_entry(tree, left)?;
    if below.is_empty() {
      return Some(entry);
    }
    tree = tree_of(graph, &entry)?;
    left = below;
  }
  Some(graph.id(tree))
}

/// The object at `node` with every tag it names peeled (`deref_tag`): itself when it is no tag; `None` for a chain of
/// tags whose end the store does not hold.
fn untagged(graph: &StoreGraph<'_>, node: u64) -> Option<u64> {
  match graph.kind(node) {
    Kind::Tag => refs::peeled_node(graph, node),
    Kind::Commit | Kind::Tree | Kind::Blob => Some(node),
  }
}

/// A parent's or an ancestor's suffix (`get_oid_1`): the revision before the last `~` or `^` that only digits follow,
/// that byte, and the count the digits write, one for none, `None` past git's largest. `None` for a name with no such
/// suffix.
fn suffix(rev: &[u8]) -> Option<(&[u8], u8, Option<u32>)> {
  let digits = rev
    .iter()
    .rev()
    .take_while(|byte| byte.is_ascii_digit())
    .count();
  let at = rev.len().checked_sub(digits)?.checked_sub(1)?;
  let mark = *rev.get(at)?;
  if mark != PARENT && mark != ANCESTOR {
    return None;
  }
  let written = rev.get(at.saturating_add(1)..)?;
  let count = if written.is_empty() {
    Some(1)
  } else {
    count_of(written)
  };
  Some((rev.get(..at)?, mark, count))
}

/// The count `digits` write in decimal, `None` past git's largest (`unsigned_mult_overflows`, `INT_MAX`).
fn count_of(digits: &[u8]) -> Option<u32> {
  digits
    .iter()
    .try_fold(0_u32, |count, digit| {
      count
        .checked_mul(COUNT_RADIX)?
        .checked_add(u32::from(digit.saturating_sub(ZERO)))
    })
    .filter(|count| *count <= MOST_COUNT)
}

/// What the last `@{` of `rev` asks (`get_oid_basic`), searched from four bytes before its end down to its start, in a
/// name that ends with `}`: a branch mark or `@{-<n>}` at the start ask no log's entry; `@{-<n>}` after a revision is
/// read as nothing.
fn logged(rev: &[u8]) -> Logged<'_> {
  if rev.last() != Some(&BRACES.1) {
    return Logged::None;
  }
  let Some(from) = rev.len().checked_sub(AT_BRACE_FROM_END) else {
    return Logged::None;
  };
  let Some(at) = (0..=from).rev().find(|at| {
    rev
      .get(*at..)
      .is_some_and(|rest| rest.starts_with(AT_BRACE))
  }) else {
    return Logged::None;
  };
  let after = at.saturating_add(AT_BRACE.len());
  if rev.get(after) == Some(&PRIOR) {
    return if at == 0 {
      Logged::None
    } else {
      Logged::PriorNotFirst
    };
  }
  if is_mark(rev.get(at..).unwrap_or_default()) {
    return Logged::None;
  }
  Logged::Entry {
    base: rev.get(..at).unwrap_or_default(),
    spec: rev
      .get(after..rev.len().saturating_sub(1))
      .unwrap_or_default(),
  }
}

/// Whether `rest` begins with a branch mark, in any case (`at_mark`).
fn is_mark(rest: &[u8]) -> bool {
  MARKS.iter().any(|mark| {
    rest
      .get(..mark.len())
      .is_some_and(|begins| begins.eq_ignore_ascii_case(mark))
  })
}

/// Whether `name`'s first byte, an `@`, reads as `HEAD` (`interpret_empty_at`): `@` alone, or before `@{`.
fn empty_at(name: &[u8]) -> bool {
  if name.get(1) == Some(&BRACES.0) {
    return false;
  }
  let rest = name.get(1..).unwrap_or_default();
  match rest.iter().position(|byte| *byte == AT_BYTE) {
    None => rest.is_empty(),
    Some(0) => rest.get(1) == Some(&BRACES.0),
    Some(_) => false,
  }
}

/// Whether `path` has an empty component, or one of dots alone, git's names that are no ref's (`ambiguous_path`): a
/// `/` with nothing but dots since the start or the last `/`, or such an end.
fn ambiguous_path(path: &[u8]) -> bool {
  let mut component_dots = true;
  for byte in path {
    match byte {
      b'/' if component_dots => return true,
      b'/' => component_dots = true,
      b'.' => {}
      _ => component_dots = false,
    }
  }
  component_dots
}

/// Whether git reads `spec` as a date (`approxidate_careful` sets no error, `approxidate_str` "touched"): a digit
/// anywhere, or a run of letters that begins a month's name in three letters or more, or is one of git's special
/// words or a number's name (`approxidate_alpha`, `match_string`).
fn is_date(spec: &[u8]) -> bool {
  spec.iter().any(u8::is_ascii_digit)
    || (0..spec.len()).any(|at| {
      let starts_run = spec.get(at).is_some_and(u8::is_ascii_alphabetic)
        && (at == 0
          || spec
            .get(at.saturating_sub(1))
            .is_some_and(|byte| !byte.is_ascii_alphabetic()));
      let rest = spec.get(at..).unwrap_or_default();
      starts_run
        && (MONTHS
          .iter()
          .any(|month| matched(rest, month) >= MONTH_LEAST)
          || SPECIAL_DATES
            .iter()
            .chain(NUMBER_NAMES.iter())
            .any(|word| matched(rest, word) == word.len()))
    })
}

/// How many of `date`'s bytes match `word` in any case before a byte that is no letter or digit (`match_string`): none
/// when a letter or digit differs, or follows the whole word.
fn matched(date: &[u8], word: &[u8]) -> usize {
  let mut count = 0_usize;
  for (at, byte) in date.iter().enumerate() {
    match word.get(at) {
      Some(letter) if byte.eq_ignore_ascii_case(letter) => count = count.saturating_add(1),
      _ if !byte.is_ascii_alphanumeric() => break,
      _ => return 0,
    }
  }
  count
}
