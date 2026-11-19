//! Mergeinfo (§4.17.4 item 8 and piece 9; A-605): the `svn:mergeinfo` property's value, read and written as Subversion
//! reads and writes it, the range-list algebra Subversion computes merges with, inheritance and elision, and the
//! queries a server answers over the Subversion index ([`crate::index`], A-553).
//!
//! Merge tracking is the client's: `svn merge` asks the server for mergeinfo, logs, location segments and diffs, merges
//! text in its working copy and commits the result as any commit (§4.17.4 item 8; §2 ban 11: the server never merges
//! file contents). What the server owes is the value's grammar at commit, `get-mergeinfo`'s answers, and, for
//! `svn mergeinfo --show-revs`, the very range lists the stock client computes. Everything here is read from
//! Subversion's `1.14.x` branch at `d434dc14` (2026-11-01; `docs/wip/research/vcs-servers-subversion.md` §1 and §7),
//! each rule naming the function it transcribes.
//!
//! **The value** (`svn_mergeinfo.h` 45-110; `libsvn_subr/mergeinfo.c` `svn_mergeinfo_parse`): lines `PATH:RANGES`, a
//! range `N` or `N-M`, either marked `*` when only its own path has it. Read as Subversion reads it:
//! - as a C string, to its first NUL; at commit a NUL inside the value is refused first (`fs-wrap.c`
//!   `verify_mergeinfo`, "unexpected string terminator");
//! - a line's last colon divides its path from its ranges, so a path may hold colons; the path is made canonical and
//!   absolute (`svn_fspath__canonicalize`: empty and `.` names dropped, `..` and every other byte kept), so `trunk:1`,
//!   `/trunk/:1` and `/trunk/.:1` all name `/trunk`, and a path named twice has its ranges merged, never refused;
//! - spaces after the colon are skipped (`isspace` in the C locale, the newline aside), none elsewhere;
//! - a revision is decimal digits alone, at most ten of them and at most 2^31 - 1 (`types.c` `svn_revnum_parse` over
//!   `string.c` `svn__strtoul`: no sign, no space), never 0, and a range's last revision follows its first;
//! - a comma ending the value is refused, one ending a line is not (`parse_rangelist`); a lone or trailing empty line
//!   is refused; the value's ranges are sorted and combined, and two that overlap with different inheritability are
//!   refused (`svn_rangelist__canonicalize`).
//!
//! Every refusal is typed by the message Subversion gives it ([`MergeinfoRefusal`]) and answered `E200020`, the code
//! `svn_mergeinfo_parse` puts on top of whatever refused the value. Written as `svn_mergeinfo_to_string` writes: paths
//! in `svn_path_compare_paths` order (a separator before any other byte, so `/a/b` precedes `/a-b`), one line each,
//! ranges `N` or `N-M` with `*`.
//!
//! **The algebra** is transcribed from `mergeinfo.c`, never restated: the union (`svn_rangelist_merge2`, 1.14's sweep,
//! in which a revision takes the larger of its two kinds), and intersection, removal and difference
//! (`rangelist_intersect_or_remove` with `combine_with_lastrange`), each with Subversion's `consider_inheritance`
//! ([`Compare`]). Transcribed because Subversion's own walk is not exact per revision when inheritance is considered:
//! intersecting `1-10` with `3-4*,5-6` answers nothing where `5-6` is common to both, since the walk passes `1-10` at
//! `3-4*`; and the stock client computes eligible and merged revisions with these very functions
//! (`libsvn_client/mergeinfo.c` `svn_client__mergeinfo_log`), so a server answering for it must compute the same. The
//! tests hold the transcription to Subversion's own vectors (`libsvn_subr/mergeinfo-test.c`) and to the per-revision
//! rule wherever Subversion's is exact.
//!
//! **Inheritance** (`svn_mergeinfo.h` 52-70): a path without mergeinfo of its own inherits its nearest ancestor's, each
//! source path extended by the path's place beneath that ancestor, non-inheritable ranges dropped. **Elision**
//! (`libsvn_client/mergeinfo.c` `should_elide_mergeinfo`): a path's own mergeinfo is removed after a merge when it
//! equals what it would inherit ([`elides`]).
//!
//! **The queries** (FSFS's `tree.c` 4133-4420; `fs-wrap.c` `svn_repos_fs_get_mergeinfo2`): a path's mergeinfo at a
//! revision, explicit, inherited or nearest-ancestor ([`mergeinfo_at`]); the mergeinfo of the nodes beneath it
//! ([`descendants`]), found by the index's per-directory counts (FSFS's `minfo-cnt`), never entering a directory that
//! holds none beneath it; and both for a request's paths ([`catalog`]). As FSFS, a node whose stored value does not
//! parse answers no mergeinfo and stops the search for an ancestor's (Subversion issue #3896); a node whose row says it
//! holds mergeinfo while its property list holds none is corrupt. The index names each node's property list; the green
//! holds the lists and the directories ([`Contents`], §4.17.4 item 1). The caller drops the requested paths the
//! principal may not read, as the repository layer does, and nothing beneath them is filtered (§4.17.4 item 10).
//!
//! Bounds: a parse allocates in proportion to the value, which the wire bounds before it arrives (§4.17.4 item 12, the
//! cap on a property value); each range takes two bytes of it at least. The algebra's results hold at most the ranges
//! of its operands plus one per range, since a removal splits a range at most once per range removed. A path's query
//! walks its depth; the descendants are an iterator that holds one listing per directory on its way down, so the server
//! pulls them a slice at a time under the shard's budget (§4.17.4 "Long reads").
//!
//! No I/O, no clock, no randomness, no panic.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::index::{ContentIdentity, Index, IndexRefusal, Node, NodeKind, PropertyList, Revision};

/// `SVN_ERR_MERGEINFO_PARSE_ERROR`, every refused value's code.
/// Format: the miscellaneous category's start, 120,000 + 16 × 5,000 = 200,000, plus 20 (`svn_error_codes.h` 137-138,
/// 1363-1365); `svn_mergeinfo_parse` returns it as the topmost error whatever refused the value (`mergeinfo.c`).
const SVN_ERR_MERGEINFO_PARSE_ERROR: u32 = 200_020;

/// `SVN_ERR_FS_CORRUPT`, for a node whose row and property list disagree.
/// Format: the filesystem category's start, 120,000 + 8 × 5,000 = 160,000, plus 4 (`svn_error_codes.h` 597-598).
const SVN_ERR_FS_CORRUPT: u32 = 160_004;

/// The byte that ends a line of the value.
/// Format: `NEWLINE` in the grammar (`svn_mergeinfo.h`).
const LINE_END: u8 = b'\n';

/// The byte between a line's path and its ranges; a path may hold it too, so a line's last one divides.
/// Format: `COLON` (`svn_mergeinfo.h`); `parse_pathname` finds "the last colon before END or newline" (`mergeinfo.c`).
const PATH_END: u8 = b':';

/// The byte between two ranges.
/// Format: `COMMA` (`svn_mergeinfo.h`).
const RANGE_SEPARATOR: u8 = b',';

/// The byte between a range's first and last revision.
/// Format: `revisionrange -> REVISION1 "-" REVISION2` (`svn_mergeinfo.h`).
const RANGE_DASH: u8 = b'-';

/// The mark of a non-inheritable range.
/// Format: `SVN_MERGEINFO_NONINHERITABLE_STR` (`svn_mergeinfo.h`).
const NONINHERITABLE_MARK: u8 = b'*';

/// The byte between a path's names.
/// Format: Subversion's path separator (`dirent_uri.c`).
const SEPARATOR: u8 = b'/';

/// The root, the one path with no parent.
/// Format: `svn_fspath__canonicalize` answers `/` for it (`dirent_uri.c`).
const ROOT: &str = "/";

/// The name meaning the directory itself, which canonicalization drops.
/// Format: "Empty or noop segment, so do nothing" (`dirent_uri.c` `canonicalize`).
const CURRENT_DIRECTORY: &[u8] = b".";

/// The byte a C string ends at: the value is read to its first one.
/// Format: `svn_mergeinfo_parse` reads to `input + strlen(input)` (`mergeinfo.c`); `verify_mergeinfo` refuses a value
/// whose `strlen` is not its length (`fs-wrap.c`).
const TERMINATOR: u8 = 0;

/// The spaces skipped after a line's colon: `isspace` in the C locale, the newline aside.
/// Format: `parse_rangelist` skips while `*curr != '\n' && isspace(*curr)` (`mergeinfo.c`); C11 7.4.1.10 names space,
/// tab, vertical tab, form feed and carriage return besides the newline.
const HORIZONTAL_SPACE: [u8; 5] = *b" \t\x0b\x0c\r";

/// The most digits a revision may have.
/// Format: `svn_revnum_parse` refuses "Revision number longer than 10 digits" (`types.c`).
const MOST_DIGITS: usize = 10;

/// The largest revision a value may name.
/// Format: `svn_revnum_parse` refuses a ten-digit number past `APR_INT32_MAX`, 2^31 - 1, as "Revision number too
/// large" (`types.c`).
const LARGEST_REVISION: Revision = 2_147_483_647;

/// The base a revision is read and written in.
/// Format: `svn__strtoul` reads decimal digits (`string.c`); `range_to_string` writes `%ld` (`mergeinfo.c`).
const DECIMAL: u64 = 10;

/// The protocol's word for inherited mergeinfo.
/// Format: `svn_inheritance_to_word` (`mergeinfo.c`); `get-mergeinfo`'s `inherit:word` (`libsvn_ra_svn/protocol`).
const INHERITED_WORD: &str = "inherited";

/// The protocol's word for a nearest ancestor's mergeinfo.
/// Format: `svn_inheritance_to_word` (`mergeinfo.c`).
const NEAREST_ANCESTOR_WORD: &str = "nearest-ancestor";

/// The protocol's word for a node's own mergeinfo, and its answer to any word it does not know.
/// Format: `svn_inheritance_to_word` and `svn_inheritance_from_word` (`mergeinfo.c`).
const EXPLICIT_WORD: &str = "explicit";

/// One range of a range list: the changes of revisions `start + 1` through `end` (`svn_merge_range_t`, forward only),
/// inheritable by paths beneath, or for its own path alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MergeRange {
  /// The revision before the range's first: Subversion's exclusive start.
  start: Revision,
  /// The range's last revision.
  end: Revision,
  /// Whether paths beneath inherit it; `false` is the `*` mark.
  inheritable: bool,
}

impl MergeRange {
  /// The range of the changes of revisions `start + 1` through `end`, or `None` unless `start` is below `end`.
  pub fn new(start: Revision, end: Revision, inheritable: bool) -> Option<Self> {
    (start < end).then_some(Self {
      start,
      end,
      inheritable,
    })
  }

  /// The revision before its first.
  pub fn start(self) -> Revision {
    self.start
  }

  /// Its last revision.
  pub fn end(self) -> Revision {
    self.end
  }

  /// Whether paths beneath inherit it.
  pub fn inheritable(self) -> bool {
    self.inheritable
  }

  /// Whether it holds the change of `revision` (`svn_merge_range_contains_rev`).
  pub fn contains(self, revision: Revision) -> bool {
    self.start < revision && revision <= self.end
  }
}

/// `svn_sort_compare_ranges`: by start, then by end; inheritability is not compared.
fn compare_ranges(first: &MergeRange, second: &MergeRange) -> Ordering {
  (first.start, first.end).cmp(&(second.start, second.end))
}

/// Whether two ranges of the same revisions differ when one is inheritable and the other not: Subversion's
/// `consider_inheritance`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compare {
  /// They differ (`consider_inheritance` TRUE).
  WithInheritance,
  /// They do not (`consider_inheritance` FALSE).
  WithoutInheritance,
}

impl Compare {
  /// Whether inheritability is compared.
  fn considers(self) -> bool {
    self == Self::WithInheritance
  }
}

/// Two ranges of one list that overlap and differ in inheritability, which no range list may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeOverlap {
  /// The earlier.
  pub first: MergeRange,
  /// The later.
  pub second: MergeRange,
}

/// A range list (`svn_rangelist_t`): ranges sorted by `svn_sort_compare_ranges`, as Subversion's operations leave
/// them. A parsed or canonicalized list is canonical (`svn_rangelist__is_canonical`): no two of its ranges overlap and
/// two that touch differ in inheritability, so equal lists hold the same revisions of the same kinds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RangeList {
  /// The ranges.
  ranges: Vec<MergeRange>,
}

/// What a difference of two range lists found (`svn_rangelist_diff`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeDelta {
  /// In the first list and not the second.
  pub deleted: RangeList,
  /// In the second list and not the first.
  pub added: RangeList,
}

impl RangeList {
  /// The empty list.
  pub fn new() -> Self {
    Self::default()
  }

  /// `ranges` sorted and combined as `svn_rangelist__canonicalize` combines them; refused when two overlap with
  /// different inheritability.
  pub fn canonical(ranges: Vec<MergeRange>) -> Result<Self, RangeOverlap> {
    if is_canonical(&ranges) {
      return Ok(Self { ranges });
    }
    let mut sorted = ranges;
    sorted.sort_by(compare_ranges);
    let mut combined: Vec<MergeRange> = Vec::with_capacity(sorted.len());
    for range in sorted {
      absorb(&mut combined, range)?;
    }
    Ok(Self { ranges: combined })
  }

  /// The ranges, in order.
  pub fn ranges(&self) -> &[MergeRange] {
    &self.ranges
  }

  /// Whether it holds no range.
  pub fn is_empty(&self) -> bool {
    self.ranges.is_empty()
  }

  /// Whether it is canonical (`svn_rangelist__is_canonical`).
  pub fn is_canonical(&self) -> bool {
    is_canonical(&self.ranges)
  }

  /// This list merged with `changes` (`svn_rangelist_merge2`): each revision of either, of the larger kind it has in
  /// them, inheritable over non-inheritable.
  pub fn merge(&self, changes: &Self) -> Self {
    Self {
      ranges: merge_lists(&self.ranges, &changes.ranges),
    }
  }

  /// This list intersected with `other` (`svn_rangelist_intersect(self, other)`); without inheritance, a common range
  /// is non-inheritable only when both are.
  pub fn intersect(&self, other: &Self, compare: Compare) -> Self {
    Self {
      ranges: intersect_or_remove(&self.ranges, &other.ranges, Walk::Intersect, compare),
    }
  }

  /// This list with `eraser`'s ranges removed (`svn_rangelist_remove(eraser, self)`); with inheritance, a range is
  /// removed only by a range of its own kind.
  pub fn remove(&self, eraser: &Self, compare: Compare) -> Self {
    Self {
      ranges: intersect_or_remove(&eraser.ranges, &self.ranges, Walk::Remove, compare),
    }
  }

  /// What `to` lacks of this list and has beyond it (`svn_rangelist_diff(self, to)`).
  pub fn diff(&self, to: &Self, compare: Compare) -> RangeDelta {
    RangeDelta {
      deleted: self.remove(to, compare),
      added: to.remove(self, compare),
    }
  }

  /// The inheritable ranges alone (`svn_rangelist_inheritable2` with no bounds, inheritable TRUE).
  pub fn inheritable_part(&self) -> Self {
    self.of_kind(true)
  }

  /// The non-inheritable ranges alone (`svn_rangelist_inheritable2` with no bounds, inheritable FALSE).
  pub fn non_inheritable_part(&self) -> Self {
    self.of_kind(false)
  }

  /// The list as a value writes it (`svn_rangelist_to_string`).
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut written = Vec::new();
    self.write(&mut written);
    written
  }

  /// The ranges of one inheritability.
  fn of_kind(&self, inheritable: bool) -> Self {
    Self {
      ranges: self
        .ranges
        .iter()
        .copied()
        .filter(|range| range.inheritable == inheritable)
        .collect(),
    }
  }

  /// Appends the list as `svn_rangelist_to_string` writes it: ranges between commas, each `range_to_string`.
  fn write(&self, written: &mut Vec<u8>) {
    for (position, range) in self.ranges.iter().enumerate() {
      if position > 0 {
        written.push(RANGE_SEPARATOR);
      }
      write_range(*range, written);
    }
  }
}

/// `range_to_string` for a forward range: its one revision, or its first and last between a dash, then `*` when it is
/// not inheritable.
fn write_range(range: MergeRange, written: &mut Vec<u8>) {
  let first = range.start.saturating_add(1);
  if first < range.end {
    written.extend_from_slice(first.to_string().as_bytes());
    written.push(RANGE_DASH);
  }
  written.extend_from_slice(range.end.to_string().as_bytes());
  if !range.inheritable {
    written.push(NONINHERITABLE_MARK);
  }
}

/// `svn_rangelist__is_canonical`: every range forward, none overlapping the next, two that touch of different kinds.
fn is_canonical(ranges: &[MergeRange]) -> bool {
  ranges.iter().all(|range| range.start < range.end)
    && ranges.windows(2).all(|pair| match pair {
      [first, second] => {
        first.end < second.start
          || (first.end == second.start && first.inheritable != second.inheritable)
      }
      _ => true,
    })
}

/// One step of `svn_rangelist__canonicalize`'s loop over sorted ranges: `range` combined into the last when they touch
/// or overlap with one kind, kept apart when they only touch with two, refused when they overlap with two.
fn absorb(combined: &mut Vec<MergeRange>, range: MergeRange) -> Result<(), RangeOverlap> {
  let Some(last) = combined.last_mut() else {
    combined.push(range);
    return Ok(());
  };
  let touch = last.start <= range.end && range.start <= last.end;
  if !touch {
    combined.push(range);
  } else if range.start < last.end && range.inheritable != last.inheritable {
    return Err(RangeOverlap {
      first: *last,
      second: range,
    });
  } else if last.inheritable == range.inheritable {
    last.end = last.end.max(range.end);
  } else {
    combined.push(range);
  }
  Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// The union: 1.14's sweep (`rangelist_merge`, `rangelist_interval_iterator_t`, `rangelist_builder_t`).

/// What covers an interval of revisions in the sweep, ordered as Subversion orders them: a gap below a non-inheritable
/// range below an inheritable one (`MI_NONE`, `MI_NON_INHERITABLE`, `MI_INHERITABLE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cover {
  /// No range.
  Gap,
  /// A non-inheritable range.
  NonInheritable,
  /// An inheritable range.
  Inheritable,
}

/// An interval of the sweep: the revisions after `start` through `end`, and what covers them.
#[derive(Clone, Copy, Debug)]
struct Interval {
  /// The revision before its first.
  start: Revision,
  /// Its last revision.
  end: Revision,
  /// What covers it.
  cover: Cover,
}

/// A walk over one list's intervals: the gap before each range, then the range, empty ones skipped
/// (`rangelist_interval_iterator_t`).
#[derive(Clone, Copy, Debug)]
struct Intervals<'list> {
  /// The list.
  ranges: &'list [MergeRange],
  /// The range the walk is at, or the gap before it.
  position: usize,
  /// Whether the walk is at the range rather than the gap before it.
  in_range: bool,
}

impl<'list> Intervals<'list> {
  /// The walk at its first non-empty interval (`rlii_first`), or `None` for an empty list.
  fn first(ranges: &'list [MergeRange]) -> Option<Self> {
    Self {
      ranges,
      position: 0,
      in_range: false,
    }
    .settle()
  }

  /// The walk at its next non-empty interval (`rlii_next`), or `None` past the last range.
  fn following(self) -> Option<Self> {
    self.step()?.settle()
  }

  /// The interval the walk is at (`rlii_update`), or `None` past the last range.
  fn current(&self) -> Option<Interval> {
    let range = self.ranges.get(self.position)?;
    if self.in_range {
      let cover = if range.inheritable {
        Cover::Inheritable
      } else {
        Cover::NonInheritable
      };
      return Some(Interval {
        start: range.start,
        end: range.end,
        cover,
      });
    }
    let start = self
      .position
      .checked_sub(1)
      .and_then(|previous| self.ranges.get(previous))
      .map_or(0, |previous| previous.end);
    Some(Interval {
      start,
      end: range.start,
      cover: Cover::Gap,
    })
  }

  /// One interval on, empty or not (`rlii_next_any_interval`).
  fn step(mut self) -> Option<Self> {
    if self.position >= self.ranges.len() {
      return None;
    }
    if self.in_range {
      self.position = self.position.saturating_add(1);
    }
    self.in_range = !self.in_range;
    Some(self)
  }

  /// The walk at the first non-empty interval from where it is.
  fn settle(self) -> Option<Self> {
    let mut walk = self;
    loop {
      let interval = walk.current()?;
      if interval.start != interval.end {
        return Some(walk);
      }
      walk = walk.step()?;
    }
  }
}

/// The sweep's output: consecutive intervals of one cover joined, each range pushed when its cover changes
/// (`rangelist_builder_t`).
#[derive(Debug)]
struct Runs {
  /// The ranges made.
  ranges: Vec<MergeRange>,
  /// The interval accumulating, a gap ending at revision 0 at first.
  accumulating: Interval,
}

impl Runs {
  /// A builder accumulating nothing.
  fn new() -> Self {
    Self {
      ranges: Vec::new(),
      accumulating: Interval {
        start: 0,
        end: 0,
        cover: Cover::Gap,
      },
    }
  }

  /// `rl_builder_add_interval`: the interval joined to the accumulating one when their covers match, else the
  /// accumulating one pushed and this one begun. An empty interval adds nothing.
  fn add(&mut self, interval: Interval) {
    if interval.start >= interval.end {
      return;
    }
    if interval.cover == self.accumulating.cover {
      self.accumulating.end = interval.end;
    } else {
      self.flush();
      self.accumulating = interval;
    }
  }

  /// `rl_builder_flush`: the accumulating interval pushed when a range covers it.
  fn flush(&mut self) {
    let interval = self.accumulating;
    if interval.cover == Cover::Gap {
      return;
    }
    if let Some(range) = MergeRange::new(
      interval.start,
      interval.end,
      interval.cover == Cover::Inheritable,
    ) {
      self.ranges.push(range);
    }
  }

  /// The ranges made, the last flushed.
  fn finish(mut self) -> Vec<MergeRange> {
    self.flush();
    self.ranges
  }
}

/// `rangelist_merge`: both lists swept together, each revision taking the larger cover it has in them.
fn merge_lists(first: &[MergeRange], second: &[MergeRange]) -> Vec<MergeRange> {
  let mut walks = (Intervals::first(first), Intervals::first(second));
  let mut runs = Runs::new();
  let mut last_end: Revision = 0;
  loop {
    let currents = (
      walks.0.as_ref().and_then(Intervals::current),
      walks.1.as_ref().and_then(Intervals::current),
    );
    let (next_end, cover) = match currents {
      (None, None) => break,
      (Some(one), None) | (None, Some(one)) => (one.end, one.cover),
      (Some(one), Some(other)) => (one.end.min(other.end), one.cover.max(other.cover)),
    };
    runs.add(Interval {
      start: last_end,
      end: next_end,
      cover,
    });
    if currents.0.is_some_and(|interval| interval.end <= next_end) {
      walks.0 = walks.0.and_then(Intervals::following);
    }
    if currents.1.is_some_and(|interval| interval.end <= next_end) {
      walks.1 = walks.1.and_then(Intervals::following);
    }
    last_end = next_end;
  }
  runs.finish()
}

// ---------------------------------------------------------------------------------------------------------------------
// Intersection and removal: `rangelist_intersect_or_remove`, `combine_with_lastrange`, `combine_ranges`.

/// Which of its two answers `rangelist_intersect_or_remove` gives (its `do_remove`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
  /// The ranges common to both lists.
  Intersect,
  /// The second list's ranges the first does not hold.
  Remove,
}

/// `combine_ranges`: the one range two ranges that touch or overlap make, when their inheritability allows; it is
/// non-inheritable only when both are.
fn combine(first: MergeRange, second: MergeRange, compare: Compare) -> Option<MergeRange> {
  let touch = first.start <= second.end && second.start <= first.end;
  let allowed = !compare.considers() || first.inheritable == second.inheritable;
  (touch && allowed).then(|| MergeRange {
    start: first.start.min(second.start),
    end: first.end.max(second.end),
    inheritable: first.inheritable || second.inheritable,
  })
}

/// `range_contains`: whether `outer` holds all of `inner`, of its kind when inheritability is compared.
fn range_contains(outer: MergeRange, inner: MergeRange, compare: Compare) -> bool {
  outer.start <= inner.start
    && inner.end <= outer.end
    && (!compare.considers() || outer.inheritable == inner.inheritable)
}

/// `range_intersect`: whether two ranges share a revision, of one kind when inheritability is compared.
fn range_intersect(first: MergeRange, second: MergeRange, compare: Compare) -> bool {
  first.start < second.end
    && second.start < first.end
    && (!compare.considers() || first.inheritable == second.inheritable)
}

/// How two forward ranges meet (`intersection_type_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Meeting {
  /// They neither overlap nor touch.
  Apart,
  /// They are the same revisions.
  Equal,
  /// They touch without overlapping.
  Adjoining,
  /// They overlap and each has revisions the other lacks.
  Overlapping,
  /// One holds all of the other.
  Subset,
}

/// `get_type_of_intersection`.
fn meeting(first: MergeRange, second: MergeRange) -> Meeting {
  if !(first.start <= second.end && second.start <= first.end) {
    Meeting::Apart
  } else if first.start == second.start && first.end == second.end {
    Meeting::Equal
  } else if first.end == second.start || second.end == first.start {
    Meeting::Adjoining
  } else if (first.start <= second.start && first.end >= second.end)
    || (second.start <= first.start && second.end >= first.end)
  {
    Meeting::Subset
  } else {
    Meeting::Overlapping
  }
}

/// Sets the last range of `ranges`.
fn set_last(ranges: &mut [MergeRange], range: MergeRange) {
  if let Some(last) = ranges.last_mut() {
    *last = range;
  }
}

/// `combine_with_lastrange`: `new_range` added to `ranges`, combined with the last as Subversion combines them, and
/// the list sorted again when the pieces left it out of order.
fn combine_with_last(new_range: MergeRange, ranges: &mut Vec<MergeRange>, compare: Compare) {
  let Some(last) = ranges.last().copied() else {
    ranges.push(new_range);
    return;
  };
  if let Some(combined) = combine(last, new_range, compare) {
    set_last(ranges, combined);
    return;
  }
  if !compare.considers() {
    ranges.push(new_range);
    return;
  }
  let sorted = match meeting(new_range, last) {
    Meeting::Apart | Meeting::Adjoining => {
      ranges.push(new_range);
      compare_ranges(&last, &new_range) == Ordering::Less
    }
    Meeting::Equal => {
      set_last(
        ranges,
        MergeRange {
          inheritable: true,
          ..last
        },
      );
      true
    }
    Meeting::Overlapping => split_overlap(last, new_range, ranges),
    Meeting::Subset => split_subset(last, new_range, ranges),
  };
  if !sorted {
    ranges.sort_by(compare_ranges);
  }
}

/// `combine_with_lastrange`'s overlapping case: the last range taken off, the older of the two cut where the newer
/// begins unless it is inheritable, in which case the newer starts where it ends; answers whether the pieces kept
/// order.
fn split_overlap(last: MergeRange, new_range: MergeRange, ranges: &mut Vec<MergeRange>) -> bool {
  ranges.pop();
  let (mut older, mut newer) = if new_range.start < last.start {
    (new_range, last)
  } else {
    (last, new_range)
  };
  if older.inheritable {
    newer.start = older.end;
  } else {
    older.end = newer.start;
  }
  ranges.push(older);
  ranges.push(newer);
  compare_ranges(&last, &older) == Ordering::Less
    && compare_ranges(&older, &newer) == Ordering::Less
}

/// `combine_with_lastrange`'s subset case: the last range taken off, the pieces the superset and the subset make pushed
/// in order; answers whether they kept order against the last range and each other.
fn split_subset(last: MergeRange, new_range: MergeRange, ranges: &mut Vec<MergeRange>) -> bool {
  ranges.pop();
  let (outer, inner) = if new_range.start < last.start || new_range.end > last.end {
    (new_range, last)
  } else {
    (last, new_range)
  };
  let mut previous = last;
  let mut sorted = true;
  for piece in subset_pieces(outer, inner).into_iter().flatten() {
    ranges.push(piece);
    sorted = sorted && compare_ranges(&previous, &piece) == Ordering::Less;
    previous = piece;
  }
  sorted
}

/// The pieces of a superset `outer` and a subset `inner` of another kind: one range when the superset is inheritable;
/// otherwise the superset's non-inheritable parts around the subset made inheritable.
fn subset_pieces(outer: MergeRange, inner: MergeRange) -> [Option<MergeRange>; 3] {
  let piece = |start, end, inheritable| MergeRange::new(start, end, inheritable);
  if outer.inheritable {
    [
      piece(outer.start.min(inner.start), outer.end.max(inner.end), true),
      None,
      None,
    ]
  } else if outer.start == inner.start {
    [
      piece(outer.start, inner.end, true),
      piece(inner.end, outer.end, outer.inheritable),
      None,
    ]
  } else if outer.end == inner.end {
    [
      piece(outer.start, inner.start, outer.inheritable),
      piece(inner.start, inner.end, true),
      None,
    ]
  } else {
    [
      piece(outer.start, inner.start, outer.inheritable),
      piece(inner.start, inner.end, true),
      piece(inner.end, outer.end, outer.inheritable),
    ]
  }
}

/// The state of `rangelist_intersect_or_remove`'s walk: where it is in each list, and its working copy of the second
/// list's range, which the walk cuts as the first list's ranges pass over it.
#[derive(Debug)]
struct IntersectOrRemove<'lists> {
  /// The second list (the whiteboard when removing).
  second: &'lists [MergeRange],
  /// The answer the walk gives.
  walk: Walk,
  /// The inheritability rule.
  compare: Compare,
  /// The answer so far.
  output: Vec<MergeRange>,
  /// The first list's range at hand.
  at_first: usize,
  /// The second list's range at hand.
  at_second: usize,
  /// Which of the second list's ranges `working` copies.
  working_of: Option<usize>,
  /// The working copy of the second list's range at hand.
  working: MergeRange,
}

/// `rangelist_intersect_or_remove`.
fn intersect_or_remove(
  first: &[MergeRange],
  second: &[MergeRange],
  walk: Walk,
  compare: Compare,
) -> Vec<MergeRange> {
  let mut state = IntersectOrRemove {
    second,
    walk,
    compare,
    output: Vec::new(),
    at_first: 0,
    at_second: 0,
    working_of: None,
    working: MergeRange {
      start: 0,
      end: 0,
      inheritable: false,
    },
  };
  while let (Some(&one), Some(&two)) = (first.get(state.at_first), second.get(state.at_second)) {
    if state.working_of != Some(state.at_second) {
      state.working = two;
      state.working_of = Some(state.at_second);
    }
    state.step(one, state.working);
  }
  if walk == Walk::Remove {
    state.copy_rest();
  }
  state.output
}

impl IntersectOrRemove<'_> {
  /// One turn of the walk's loop with the first list's range `one` and the working copy `two` of the second's.
  fn step(&mut self, one: MergeRange, two: MergeRange) {
    if range_contains(one, two, self.compare) {
      if self.walk == Walk::Intersect {
        self.add(MergeRange {
          inheritable: two.inheritable || one.inheritable,
          ..two
        });
      }
      self.at_second = self.at_second.saturating_add(1);
      if two.start == one.start && two.end == one.end {
        self.at_first = self.at_first.saturating_add(1);
      }
    } else if range_intersect(one, two, self.compare) {
      self.cut(one, two);
    } else if compare_ranges(&one, &two) == Ordering::Less {
      self.at_first = self.at_first.saturating_add(1);
    } else {
      if self.walk == Walk::Remove {
        self.keep(two);
      }
      self.at_second = self.at_second.saturating_add(1);
    }
  }

  /// The walk's case of two ranges sharing revisions without the first holding the second: the part before the first
  /// range answered, the part beyond it kept to test against the next.
  fn cut(&mut self, one: MergeRange, two: MergeRange) {
    let shared = two.inheritable || one.inheritable;
    if two.start < one.start {
      let before = match self.walk {
        Walk::Remove => MergeRange::new(two.start, one.start, two.inheritable),
        Walk::Intersect => MergeRange::new(one.start, two.end.min(one.end), shared),
      };
      if let Some(range) = before {
        self.add(range);
      }
    }
    if two.end > one.end {
      if self.walk == Walk::Intersect
        && let Some(range) = MergeRange::new(two.start.max(one.start), one.end, shared)
      {
        self.add(range);
      }
      self.working = MergeRange {
        start: one.end,
        end: two.end,
        inheritable: two.inheritable,
      };
    } else {
      self.at_second = self.at_second.saturating_add(1);
    }
  }

  /// A second-list range the first passed without touching, kept by a removal: combined into the last answered range
  /// when `combine_ranges` allows, else pushed.
  fn keep(&mut self, two: MergeRange) {
    let combined = self
      .output
      .last()
      .and_then(|&last| combine(last, two, self.compare));
    match combined {
      Some(range) => set_last(&mut self.output, range),
      None => self.output.push(two),
    }
  }

  /// Adds an answered range as `combine_with_lastrange` adds it.
  fn add(&mut self, range: MergeRange) {
    combine_with_last(range, &mut self.output, self.compare);
  }

  /// A removal's end: the working copy kept if the walk had cut it, then the second list's untouched ranges.
  fn copy_rest(&mut self) {
    if self.working_of == Some(self.at_second) && self.at_second < self.second.len() {
      self.add(self.working);
      self.at_second = self.at_second.saturating_add(1);
    }
    let second = self.second;
    for &range in second.iter().skip(self.at_second) {
      self.add(range);
    }
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Paths.

/// A merge source path: absolute and canonical (`svn_fspath__canonicalize`), compared as `svn_path_compare_paths`
/// compares, a separator before any other byte, so a path's children follow it before any later sibling of its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourcePath(Box<[u8]>);

impl SourcePath {
  /// The canonical form of `path`, read as a C string to its first NUL: empty and `.` names dropped, a leading
  /// separator added (`svn_fspath__canonicalize`).
  pub fn canonical(path: &[u8]) -> Self {
    let path = path
      .split(|&byte| byte == TERMINATOR)
      .next()
      .unwrap_or_default();
    let mut canonical = vec![SEPARATOR];
    for name in path
      .split(|&byte| byte == SEPARATOR)
      .filter(|name| !name.is_empty() && *name != CURRENT_DIRECTORY)
    {
      if canonical.len() > 1 {
        canonical.push(SEPARATOR);
      }
      canonical.extend_from_slice(name);
    }
    Self(canonical.into_boxed_slice())
  }

  /// Its bytes.
  pub fn as_bytes(&self) -> &[u8] {
    &self.0
  }

  /// The path `relative` names beneath it (`svn_fspath__join`); itself for an empty `relative`.
  pub fn joined(&self, relative: &str) -> Self {
    if relative.is_empty() {
      return self.clone();
    }
    let mut joined = self.0.to_vec();
    if joined.as_slice() != ROOT.as_bytes() {
      joined.push(SEPARATOR);
    }
    joined.extend_from_slice(relative.as_bytes());
    Self(joined.into_boxed_slice())
  }
}

/// A byte's place in `svn_path_compare_paths`' order: the separator first, every other byte after it in its own order.
fn path_rank(byte: u8) -> u16 {
  if byte == SEPARATOR {
    0
  } else {
    u16::from(byte).saturating_add(1)
  }
}

impl Ord for SourcePath {
  fn cmp(&self, other: &Self) -> Ordering {
    self
      .0
      .iter()
      .map(|&byte| path_rank(byte))
      .cmp(other.0.iter().map(|&byte| path_rank(byte)))
  }
}

impl PartialOrd for SourcePath {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Mergeinfo.

/// Which byte stood where a range's end was due: after a revision (Subversion's "Invalid character found in revision
/// list") or after a range and its mark ("Invalid character found in range list").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InvalidAfter {
  /// After a revision.
  Revision,
  /// After a range or its `*`.
  Range,
}

/// Why a value was refused, each kind the message `svn_mergeinfo_parse` refuses it with; every one is answered
/// `SVN_ERR_MERGEINFO_PARSE_ERROR` ([`MergeinfoRefusal::subversion_code`]). `at` is a byte offset in the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeinfoRefusal {
  /// A NUL inside the value at commit: "contains unexpected string terminator" (`verify_mergeinfo`).
  Terminator {
    /// Where.
    at: usize,
  },
  /// A line without a colon: "Pathname not terminated by ':'".
  PathNotTerminated {
    /// The line's start.
    at: usize,
  },
  /// No digit where a revision is due: "Invalid revision number found parsing", or "Negative revision number found
  /// parsing" when a `-` stands there (`svn_revnum_parse`).
  RevisionMissing {
    /// Where.
    at: usize,
    /// Whether a `-` stands there.
    negative: bool,
  },
  /// "Revision number longer than 10 digits".
  RevisionTooLong {
    /// Where the revision starts.
    at: usize,
  },
  /// "Revision number too large": ten digits past 2^31 - 1.
  RevisionTooLarge {
    /// Where the revision starts.
    at: usize,
  },
  /// "Invalid revision number '0' found in range list".
  RevisionZero {
    /// Where the revision starts.
    at: usize,
  },
  /// A byte where a range's end is due.
  InvalidCharacter {
    /// Where.
    at: usize,
    /// The byte.
    byte: u8,
    /// Which message it gets.
    after: InvalidAfter,
  },
  /// "Unable to parse reversed revision range".
  ReversedRange {
    /// Where the range starts.
    at: usize,
    /// Its first revision.
    first: Revision,
    /// Its last.
    last: Revision,
  },
  /// "Unable to parse revision range ... with same start and end revisions".
  SameStartAndEnd {
    /// Where the range starts.
    at: usize,
    /// The revision.
    revision: Revision,
  },
  /// A comma ending the value: "Range list parsing ended before hitting newline".
  RangeListUnterminated {
    /// Where the value ends.
    at: usize,
  },
  /// A path with no ranges: "Mergeinfo for '%s' maps to an empty revision range".
  NoRanges {
    /// The line's start.
    at: usize,
  },
  /// "Unable to parse overlapping revision ranges ... with different inheritance types".
  OverlappingInheritance {
    /// The line's start.
    at: usize,
    /// The two ranges.
    overlap: RangeOverlap,
  },
}

impl MergeinfoRefusal {
  /// The number of the Subversion error a client is answered with.
  pub fn subversion_code(&self) -> u32 {
    SVN_ERR_MERGEINFO_PARSE_ERROR
  }
}

/// Mergeinfo (`svn_mergeinfo_t`): merge source paths, each with a non-empty range list, in `svn_path_compare_paths`
/// order. Empty mergeinfo, which holds no path, differs from no mergeinfo (`None` where one is asked), as Subversion
/// keeps them apart (`svn_mergeinfo.h`: "Note that a path may have no mergeinfo, this is not the same as empty
/// mergeinfo").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mergeinfo {
  /// Each source's ranges.
  sources: BTreeMap<SourcePath, RangeList>,
}

/// What a difference of two mergeinfos found (`svn_mergeinfo_diff2`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeinfoDelta {
  /// In the first and not the second.
  pub deleted: Mergeinfo,
  /// In the second and not the first.
  pub added: Mergeinfo,
}

impl Mergeinfo {
  /// The value read as `svn_mergeinfo_parse` reads it, to its first NUL; refused, typed, as it refuses.
  pub fn parse(value: &[u8]) -> Result<Self, MergeinfoRefusal> {
    let mut cursor = Cursor::new(value);
    let mut mergeinfo = Self::default();
    while !cursor.at_end() {
      mergeinfo.parse_line(&mut cursor)?;
    }
    Ok(mergeinfo)
  }

  /// The value a commit sets checked as `verify_mergeinfo` checks it: a NUL inside it refused, then the value parsed.
  pub fn verify(value: &[u8]) -> Result<Self, MergeinfoRefusal> {
    if let Some(at) = value.iter().position(|&byte| byte == TERMINATOR) {
      return Err(MergeinfoRefusal::Terminator { at });
    }
    Self::parse(value)
  }

  /// The value as `svn_mergeinfo_to_string` writes it: one line a path, in path order, empty for empty mergeinfo.
  pub fn to_bytes(&self) -> Vec<u8> {
    let mut written = Vec::new();
    for (position, (path, ranges)) in self.sources.iter().enumerate() {
      if position > 0 {
        written.push(LINE_END);
      }
      written.extend_from_slice(path.as_bytes());
      written.push(PATH_END);
      ranges.write(&mut written);
    }
    written
  }

  /// Whether it holds no path: empty mergeinfo.
  pub fn is_empty(&self) -> bool {
    self.sources.is_empty()
  }

  /// How many paths it holds.
  pub fn len(&self) -> usize {
    self.sources.len()
  }

  /// The ranges of the source at `path`, canonical form.
  pub fn get(&self, path: &SourcePath) -> Option<&RangeList> {
    self.sources.get(path)
  }

  /// The sources and their ranges, in path order.
  pub fn iter(&self) -> impl Iterator<Item = (&SourcePath, &RangeList)> {
    self.sources.iter()
  }

  /// Adds `ranges` to the source at `path`, merged with what it holds there (`svn_mergeinfo_merge2` of one source); an
  /// empty list adds nothing, since no source maps to one.
  pub fn insert(&mut self, path: SourcePath, ranges: RangeList) {
    if ranges.is_empty() {
      return;
    }
    let merged = match self.sources.get(&path) {
      Some(held) => held.merge(&ranges),
      None => ranges,
    };
    self.sources.insert(path, merged);
  }

  /// This mergeinfo merged with `changes` (`svn_mergeinfo_merge2`).
  pub fn merge(&self, changes: &Self) -> Self {
    let mut merged = self.clone();
    for (path, ranges) in &changes.sources {
      merged.insert(path.clone(), ranges.clone());
    }
    merged
  }

  /// The ranges of the sources both hold, intersected (`svn_mergeinfo_intersect2(self, other)`).
  pub fn intersect(&self, other: &Self, compare: Compare) -> Self {
    let mut common = Self::default();
    for (path, ranges) in &self.sources {
      if let Some(others) = other.sources.get(path) {
        common.put(path, ranges.intersect(others, compare));
      }
    }
    common
  }

  /// This mergeinfo with `eraser`'s ranges removed (`svn_mergeinfo_remove2(eraser, self)`).
  pub fn remove(&self, eraser: &Self, compare: Compare) -> Self {
    let mut left = Self::default();
    for (path, ranges) in &self.sources {
      match eraser.sources.get(path) {
        Some(erased) => left.put(path, ranges.remove(erased, compare)),
        None => left.put(path, ranges.clone()),
      }
    }
    left
  }

  /// What `to` lacks of `from` and has beyond it (`svn_mergeinfo_diff2`); no mergeinfo on either side is empty.
  pub fn diff(from: Option<&Self>, to: Option<&Self>, compare: Compare) -> MergeinfoDelta {
    let empty = Self::default();
    let (from, to) = (from.unwrap_or(&empty), to.unwrap_or(&empty));
    MergeinfoDelta {
      deleted: from.remove(to, compare),
      added: to.remove(from, compare),
    }
  }

  /// Whether two mergeinfos are equal as `svn_mergeinfo__equals` judges: both none, or the same sources each with
  /// ranges no difference separates.
  pub fn equals(first: Option<&Self>, second: Option<&Self>, compare: Compare) -> bool {
    match (first, second) {
      (None, None) => true,
      (Some(first), Some(second)) => {
        first.len() == second.len()
          && first.sources.iter().all(|(path, ranges)| {
            second
              .sources
              .get(path)
              .is_some_and(|others| lists_equal(ranges, others, compare))
          })
      }
      _ => false,
    }
  }

  /// The inheritable ranges alone, sources left with none dropped (`svn_mergeinfo_inheritable2` with no path and no
  /// bounds, inheritable TRUE): what a path beneath inherits.
  pub fn inheritable_part(&self) -> Self {
    self.mapped(RangeList::inheritable_part)
  }

  /// The non-inheritable ranges alone, sources left with none dropped (`svn_mergeinfo_inheritable2`, inheritable
  /// FALSE).
  pub fn non_inheritable_part(&self) -> Self {
    self.mapped(RangeList::non_inheritable_part)
  }

  /// Every source extended by `relative` (`svn_fs__append_to_merged_froms`, `svn_mergeinfo__add_suffix_to_mergeinfo`):
  /// what an ancestor's mergeinfo says of the path `relative` beneath it.
  pub fn with_suffix(&self, relative: &str) -> Self {
    Self {
      sources: self
        .sources
        .iter()
        .map(|(path, ranges)| (path.joined(relative), ranges.clone()))
        .collect(),
    }
  }

  /// Each source's list mapped, the empty ones dropped.
  fn mapped(&self, map: impl Fn(&RangeList) -> RangeList) -> Self {
    let mut mapped = Self::default();
    for (path, ranges) in &self.sources {
      mapped.put(path, map(ranges));
    }
    mapped
  }

  /// Sets the source at `path` to `ranges` unless they are empty.
  fn put(&mut self, path: &SourcePath, ranges: RangeList) {
    if !ranges.is_empty() {
      self.sources.insert(path.clone(), ranges);
    }
  }

  /// `parse_revision_line`: one line's path, its ranges, its newline; the ranges canonicalized and merged into the
  /// source's.
  fn parse_line(&mut self, cursor: &mut Cursor<'_>) -> Result<(), MergeinfoRefusal> {
    let line = cursor.position;
    let path = cursor.pathname()?;
    let ranges = cursor.ranges()?;
    if ranges.is_empty() {
      return Err(MergeinfoRefusal::NoRanges { at: line });
    }
    if !cursor.at_end() {
      cursor.advance();
    }
    let ranges = RangeList::canonical(ranges)
      .map_err(|overlap| MergeinfoRefusal::OverlappingInheritance { at: line, overlap })?;
    self.insert(path, ranges);
    Ok(())
  }
}

/// `svn_mergeinfo__equals`' comparison of one source's lists: equal range by range, an inheritability that differs
/// deciding at once when it is compared; else equal when their difference is empty.
fn lists_equal(first: &RangeList, second: &RangeList, compare: Compare) -> bool {
  if first.ranges.len() == second.ranges.len() {
    let mut matched = true;
    for (one, other) in first.ranges.iter().zip(&second.ranges) {
      if one.start != other.start || one.end != other.end {
        matched = false;
        break;
      }
      if compare.considers() && one.inheritable != other.inheritable {
        return false;
      }
    }
    if matched {
      return true;
    }
  }
  let delta = first.diff(second, compare);
  delta.deleted.is_empty() && delta.added.is_empty()
}

// ---------------------------------------------------------------------------------------------------------------------
// The parser.

/// What ended a range in `parse_rangelist`: a comma, with more to come, or the line's end.
#[derive(Clone, Copy, Debug)]
enum Element {
  /// A comma followed it.
  More(MergeRange),
  /// The line or the value ended after it.
  Last(MergeRange),
}

/// A value read as a C string: its bytes before the first NUL, and the place reached.
#[derive(Debug)]
struct Cursor<'value> {
  /// The bytes before the first NUL.
  bytes: &'value [u8],
  /// The place reached.
  position: usize,
}

impl<'value> Cursor<'value> {
  /// A cursor at the start of `value`'s C string.
  fn new(value: &'value [u8]) -> Self {
    Self {
      bytes: value
        .split(|&byte| byte == TERMINATOR)
        .next()
        .unwrap_or_default(),
      position: 0,
    }
  }

  /// Whether the string is used up.
  fn at_end(&self) -> bool {
    self.position >= self.bytes.len()
  }

  /// The byte at the place reached; a NUL at the end, as C reads its terminator.
  fn peek(&self) -> u8 {
    self.bytes.get(self.position).copied().unwrap_or(TERMINATOR)
  }

  /// One byte on.
  fn advance(&mut self) {
    self.position = self.position.saturating_add(1);
  }

  /// The bytes from the place reached.
  fn rest(&self) -> &'value [u8] {
    self.bytes.get(self.position..).unwrap_or_default()
  }

  /// `parse_pathname`: the path before the line's last colon, canonical; the cursor left past that colon.
  fn pathname(&mut self) -> Result<SourcePath, MergeinfoRefusal> {
    let rest = self.rest();
    let line = rest
      .split(|&byte| byte == LINE_END)
      .next()
      .unwrap_or_default();
    let colon = line
      .iter()
      .rposition(|&byte| byte == PATH_END)
      .ok_or(MergeinfoRefusal::PathNotTerminated { at: self.position })?;
    let path = SourcePath::canonical(line.get(..colon).unwrap_or_default());
    self.position = self.position.saturating_add(colon).saturating_add(1);
    Ok(path)
  }

  /// `parse_rangelist`: the line's ranges, after the spaces that may open them; the cursor left at the line's newline
  /// or the value's end.
  fn ranges(&mut self) -> Result<Vec<MergeRange>, MergeinfoRefusal> {
    while !self.at_end() && HORIZONTAL_SPACE.contains(&self.peek()) {
      self.advance();
    }
    let mut ranges = Vec::new();
    while !self.at_end() && self.peek() != LINE_END {
      match self.element()? {
        Element::More(range) => ranges.push(range),
        Element::Last(range) => {
          ranges.push(range);
          return Ok(ranges);
        }
      }
    }
    if !ranges.is_empty() && self.peek() != LINE_END {
      return Err(MergeinfoRefusal::RangeListUnterminated { at: self.position });
    }
    Ok(ranges)
  }

  /// One range of `parse_rangelist`'s loop: its revisions, then what ends it.
  fn element(&mut self) -> Result<Element, MergeinfoRefusal> {
    let at = self.position;
    let first = self.revision()?;
    let after = self.peek();
    let ends = [RANGE_DASH, LINE_END, RANGE_SEPARATOR, NONINHERITABLE_MARK];
    if !ends.contains(&after) && !self.at_end() {
      return Err(MergeinfoRefusal::InvalidCharacter {
        at: self.position,
        byte: after,
        after: InvalidAfter::Revision,
      });
    }
    let start = first
      .checked_sub(1)
      .ok_or(MergeinfoRefusal::RevisionZero { at })?;
    let end = if after == RANGE_DASH {
      self.advance();
      self.last_revision(at, first)?
    } else {
      first
    };
    self.terminator(MergeRange {
      start,
      end,
      inheritable: true,
    })
  }

  /// A range's last revision, after its dash: past its first, or refused.
  fn last_revision(&mut self, at: usize, first: Revision) -> Result<Revision, MergeinfoRefusal> {
    let last = self.revision()?;
    match first.cmp(&last) {
      Ordering::Less => Ok(last),
      Ordering::Equal => Err(MergeinfoRefusal::SameStartAndEnd {
        at,
        revision: first,
      }),
      Ordering::Greater => Err(MergeinfoRefusal::ReversedRange { at, first, last }),
    }
  }

  /// What ends a range: the line's end, a comma, or a `*` and then one of those.
  fn terminator(&mut self, range: MergeRange) -> Result<Element, MergeinfoRefusal> {
    if self.at_end() || self.peek() == LINE_END {
      return Ok(Element::Last(range));
    }
    match self.peek() {
      RANGE_SEPARATOR => {
        self.advance();
        Ok(Element::More(range))
      }
      NONINHERITABLE_MARK => {
        self.advance();
        self.after_mark(MergeRange {
          inheritable: false,
          ..range
        })
      }
      byte => Err(self.invalid_in_range_list(byte)),
    }
  }

  /// What may follow a `*`: the line's end or a comma.
  fn after_mark(&mut self, range: MergeRange) -> Result<Element, MergeinfoRefusal> {
    if self.at_end() || self.peek() == LINE_END {
      return Ok(Element::Last(range));
    }
    match self.peek() {
      RANGE_SEPARATOR => {
        self.advance();
        Ok(Element::More(range))
      }
      byte => Err(self.invalid_in_range_list(byte)),
    }
  }

  /// "Invalid character found in range list" at the place reached.
  fn invalid_in_range_list(&self, byte: u8) -> MergeinfoRefusal {
    MergeinfoRefusal::InvalidCharacter {
      at: self.position,
      byte,
      after: InvalidAfter::Range,
    }
  }

  /// `svn_revnum_parse` over `svn__strtoul`: decimal digits, at most ten, at most 2^31 - 1; the cursor left past them.
  fn revision(&mut self) -> Result<Revision, MergeinfoRefusal> {
    let at = self.position;
    let rest = self.rest();
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 {
      return Err(MergeinfoRefusal::RevisionMissing {
        at,
        negative: rest.first() == Some(&RANGE_DASH),
      });
    }
    if digits > MOST_DIGITS {
      return Err(MergeinfoRefusal::RevisionTooLong { at });
    }
    let value = rest
      .iter()
      .take(digits)
      .try_fold(0_u64, |value, &digit| {
        value
          .checked_mul(DECIMAL)?
          .checked_add(u64::from(digit.checked_sub(b'0')?))
      })
      .filter(|&value| value <= LARGEST_REVISION)
      .ok_or(MergeinfoRefusal::RevisionTooLarge { at })?;
    self.position = at.saturating_add(digits);
    Ok(value)
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Inheritance, elision and the queries.

/// Which mergeinfo a query answers for a path (`svn_mergeinfo_inheritance_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MergeinfoInheritance {
  /// The path's own mergeinfo, or none.
  Explicit,
  /// The path's own, else its nearest ancestor's as the path inherits it.
  Inherited,
  /// Its nearest ancestor's as the path inherits it, whatever the path holds itself.
  NearestAncestor,
}

impl MergeinfoInheritance {
  /// The mode a protocol word names; any word Subversion does not know is explicit (`svn_inheritance_from_word`).
  pub fn from_word(word: &[u8]) -> Self {
    if word == INHERITED_WORD.as_bytes() {
      Self::Inherited
    } else if word == NEAREST_ANCESTOR_WORD.as_bytes() {
      Self::NearestAncestor
    } else {
      Self::Explicit
    }
  }

  /// The mode's protocol word (`svn_inheritance_to_word`).
  pub fn word(self) -> &'static str {
    match self {
      Self::Explicit => EXPLICIT_WORD,
      Self::Inherited => INHERITED_WORD,
      Self::NearestAncestor => NEAREST_ANCESTOR_WORD,
    }
  }
}

/// Whether a node's own mergeinfo elides to what it would inherit, as a merge then removes it
/// (`libsvn_client/mergeinfo.c` `should_elide_mergeinfo`): none never elides; empty mergeinfo elides to none or empty;
/// any other never elides to none or empty, and otherwise when the two are equal with inheritance compared.
/// `inherited` is the node's nearest-ancestor mergeinfo as the node inherits it ([`mergeinfo_at`] with
/// [`MergeinfoInheritance::NearestAncestor`]).
pub fn elides(explicit: Option<&Mergeinfo>, inherited: Option<&Mergeinfo>) -> bool {
  let Some(explicit) = explicit else {
    return false;
  };
  let inherits_nothing = inherited.is_none_or(Mergeinfo::is_empty);
  if explicit.is_empty() {
    inherits_nothing
  } else {
    !inherits_nothing && Mergeinfo::equals(inherited, Some(explicit), Compare::WithInheritance)
  }
}

/// What the queries read from the green beyond the index: a directory's names and a property list's `svn:mergeinfo`
/// (§4.17.4 item 1: a node's properties are one list held as content, named from its row).
pub trait Contents {
  /// The names in the directory at `directory` in `revision`, in any order; `None` when the green holds no such
  /// directory there.
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>>;

  /// The value of `svn:mergeinfo` in the property list held as `properties`; `None` when the list holds none.
  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]>;
}

/// Why a mergeinfo query was refused, each answered with a Subversion error ([`QueryRefusal::subversion_code`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryRefusal {
  /// The index refused the question: a path out of canonical form, a revision past the youngest, or a requested path
  /// absent in the revision (`open_path` with no flags; `SVN_ERR_FS_NOT_FOUND`).
  Index(IndexRefusal),
  /// A node whose row says it holds `svn:mergeinfo` while its property list holds none: "claims to have mergeinfo but
  /// doesn't" (`SVN_ERR_FS_CORRUPT`).
  ClaimsMergeinfo {
    /// The node's path.
    path: String,
    /// The revision.
    revision: Revision,
  },
  /// The green and the index disagree about a directory: the green lists no such directory, or lists a name the index
  /// does not hold (`SVN_ERR_FS_CORRUPT`).
  ContentsDisagree {
    /// The path.
    path: String,
    /// The revision.
    revision: Revision,
  },
}

impl QueryRefusal {
  /// The number of the Subversion error a client is answered with.
  pub fn subversion_code(&self) -> u32 {
    match self {
      Self::Index(refusal) => refusal.subversion_code(),
      Self::ClaimsMergeinfo { .. } | Self::ContentsDisagree { .. } => SVN_ERR_FS_CORRUPT,
    }
  }
}

impl From<IndexRefusal> for QueryRefusal {
  fn from(refusal: IndexRefusal) -> Self {
    Self::Index(refusal)
  }
}

/// The node at `path` in `revision`, refused when absent (`open_path` with no flags).
fn present<'index>(
  index: &'index Index,
  path: &str,
  revision: Revision,
) -> Result<Node<'index>, QueryRefusal> {
  index.node(path, revision)?.ok_or_else(|| {
    QueryRefusal::Index(IndexRefusal::NotFound {
      path: path.to_owned(),
      revision,
    })
  })
}

/// A path's places from itself up to the root, each with the path relative to it: FSFS's parent-path chain, walked
/// from the node up.
fn places_upward(path: &str) -> impl Iterator<Item = (&str, &str)> {
  let above = path
    .rmatch_indices(char::from(SEPARATOR))
    .filter(move |_| path != ROOT)
    .filter_map(move |(cut, _)| {
      let place = if cut == 0 { ROOT } else { path.get(..cut)? };
      Some((place, path.get(cut.checked_add(1)?..)?))
    });
  std::iter::once((path, "")).chain(above)
}

/// The value of `svn:mergeinfo` a node holding it has, refused as corrupt when its property list holds none.
fn held_value<'contents, C: Contents + ?Sized>(
  contents: &'contents C,
  node: &Node<'_>,
  path: &str,
  revision: Revision,
) -> Result<&'contents [u8], QueryRefusal> {
  let claims = || QueryRefusal::ClaimsMergeinfo {
    path: path.to_owned(),
    revision,
  };
  match node.properties() {
    PropertyList::Held {
      identity,
      has_mergeinfo: true,
    } => contents.mergeinfo_value(&identity).ok_or_else(claims),
    PropertyList::Held { .. } | PropertyList::Empty => Err(claims()),
  }
}

/// The mergeinfo of `path` in `revision` (FSFS's `get_mergeinfo_for_path_internal`, adjusting what is inherited, as
/// `svn_repos_fs_get_mergeinfo2` asks): the nearest node holding `svn:mergeinfo` from the path up (from its parent up
/// for [`MergeinfoInheritance::NearestAncestor`], the path alone for [`MergeinfoInheritance::Explicit`]); an ancestor's
/// as the path inherits it, its non-inheritable ranges dropped and its sources extended by the path beneath it. `None`
/// when no such node is found, and when the one found holds a value that does not parse (Subversion issue #3896: the
/// search stops there). Refused when the path is absent, and when a node's row and property list disagree.
pub fn mergeinfo_at<C: Contents + ?Sized>(
  index: &Index,
  contents: &C,
  path: &str,
  revision: Revision,
  inheritance: MergeinfoInheritance,
) -> Result<Option<Mergeinfo>, QueryRefusal> {
  present(index, path, revision)?;
  let mut places = places_upward(path);
  if inheritance == MergeinfoInheritance::NearestAncestor {
    places.next();
  }
  for (place, relative) in places {
    let node = present(index, place, revision)?;
    if node.has_mergeinfo() {
      let value = held_value(contents, &node, place, revision)?;
      // Issue #3896: a value that does not parse is no mergeinfo, and the search ends with it.
      let Ok(mergeinfo) = Mergeinfo::parse(value) else {
        return Ok(None);
      };
      return Ok(Some(if relative.is_empty() {
        mergeinfo
      } else {
        mergeinfo.inheritable_part().with_suffix(relative)
      }));
    }
    if inheritance == MergeinfoInheritance::Explicit {
      break;
    }
  }
  Ok(None)
}

/// Whether nodes beneath `node` hold `svn:mergeinfo` (`svn_fs_fs__dag_has_descendants_with_mergeinfo`): a directory
/// whose count exceeds one, or is one that is not its own.
fn holds_mergeinfo_beneath(node: &Node<'_>) -> bool {
  let count = node.mergeinfo_count();
  node.kind() == NodeKind::Directory && (count > 1 || (count == 1 && !node.has_mergeinfo()))
}

/// The path of `name` in `directory`.
fn child_path(directory: &str, name: &str) -> String {
  if directory == ROOT {
    format!("{ROOT}{name}")
  } else {
    format!("{directory}{}{name}", char::from(SEPARATOR))
  }
}

/// The mergeinfo of the nodes beneath a path, as FSFS's crawl finds it (`add_descendant_mergeinfo`,
/// `crawl_directory_dag_for_mergeinfo`): each node holding `svn:mergeinfo` whose value parses, before what lies beneath
/// it, a directory's names in byte order; a directory entered only when nodes beneath it hold mergeinfo. Each `next`
/// lists at most one directory, so a caller pulls the crawl a slice at a time; after a refusal it yields nothing more.
#[derive(Debug)]
pub struct Descendants<'query, C: ?Sized> {
  /// The index.
  index: &'query Index,
  /// The green.
  contents: &'query C,
  /// The revision.
  revision: Revision,
  /// The directories entered and not yet left, each with the names left to visit.
  pending: Vec<(String, std::vec::IntoIter<String>)>,
}

/// The crawl of the nodes beneath `path` in `revision` ([`Descendants`]); refused when the path is absent.
pub fn descendants<'query, C: Contents + ?Sized>(
  index: &'query Index,
  contents: &'query C,
  path: &str,
  revision: Revision,
) -> Result<Descendants<'query, C>, QueryRefusal> {
  let node = present(index, path, revision)?;
  let mut crawl = Descendants {
    index,
    contents,
    revision,
    pending: Vec::new(),
  };
  if holds_mergeinfo_beneath(&node) {
    crawl.enter(path)?;
  }
  Ok(crawl)
}

impl<C: Contents + ?Sized> Descendants<'_, C> {
  /// Lists `directory` and sets its names to be visited next.
  fn enter(&mut self, directory: &str) -> Result<(), QueryRefusal> {
    let mut names = self
      .contents
      .entries(directory, self.revision)
      .ok_or_else(|| QueryRefusal::ContentsDisagree {
        path: directory.to_owned(),
        revision: self.revision,
      })?;
    names.sort_unstable();
    self.pending.push((directory.to_owned(), names.into_iter()));
    Ok(())
  }

  /// Visits one node: its mergeinfo if it holds a value that parses, and its directory entered when nodes beneath it
  /// hold some.
  fn visit(&mut self, path: &str) -> Result<Option<Mergeinfo>, QueryRefusal> {
    let node =
      self
        .index
        .node(path, self.revision)?
        .ok_or_else(|| QueryRefusal::ContentsDisagree {
          path: path.to_owned(),
          revision: self.revision,
        })?;
    let mergeinfo = if node.has_mergeinfo() {
      // Issue #3896: a value that does not parse is skipped.
      Mergeinfo::parse(held_value(self.contents, &node, path, self.revision)?).ok()
    } else {
      None
    };
    if holds_mergeinfo_beneath(&node) {
      self.enter(path)?;
    }
    Ok(mergeinfo)
  }
}

impl<C: Contents + ?Sized> Iterator for Descendants<'_, C> {
  type Item = Result<(String, Mergeinfo), QueryRefusal>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      let (directory, names) = self.pending.last_mut()?;
      let Some(name) = names.next() else {
        self.pending.pop();
        continue;
      };
      let path = child_path(directory, &name);
      match self.visit(&path) {
        Ok(Some(mergeinfo)) => return Some(Ok((path, mergeinfo))),
        Ok(None) => {}
        Err(refusal) => {
          self.pending.clear();
          return Some(Err(refusal));
        }
      }
    }
  }
}

/// `get-mergeinfo`'s answer (`svn_repos_fs_get_mergeinfo2` over FSFS's `get_mergeinfos_for_paths`): for each path in
/// the request's order, its mergeinfo when it has some, then with `include_descendants` the mergeinfo beneath it. The
/// caller has dropped the paths the principal may not read, as the repository layer drops them.
pub fn catalog<C: Contents + ?Sized>(
  index: &Index,
  contents: &C,
  paths: &[&str],
  revision: Revision,
  inheritance: MergeinfoInheritance,
  include_descendants: bool,
) -> Result<Vec<(String, Mergeinfo)>, QueryRefusal> {
  let mut answer = Vec::new();
  for &path in paths {
    if let Some(mergeinfo) = mergeinfo_at(index, contents, path, revision, inheritance)? {
      answer.push((path.to_owned(), mergeinfo));
    }
    if include_descendants {
      for found in descendants(index, contents, path, revision)? {
        answer.push(found?);
      }
    }
  }
  Ok(answer)
}

#[cfg(test)]
mod tests;
