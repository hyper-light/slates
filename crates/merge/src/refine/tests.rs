//! A whole-file rewrite recovered as the edit it was (D-O17, A-776): the recovered ops reproduce the writer's file
//! exactly, cut at token boundaries, and two agents' rewrites judged by the real green merge as precisely as their
//! declared edits would.

use std::borrow::Cow;
use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{Grain, Refiner, boundary, ops_of, refine_rewrites};
use crate::engine::{Green, Increment, Outcome};
use crate::increment::{Base, PostStatePlan, VolumeOp, compose_volume_planned};
use crate::ops_doc::{Op, OpKind};
use crate::origin::Origin;

/// A test's offset or length as an index: test texts are far below `usize::MAX`.
fn ix(value: u64) -> usize {
  usize::try_from(value).unwrap()
}

const PATH: &str = "src/lib.rs";

/// The reference applier: the writer's file rebuilt from the base and the ops (base coordinates, any order a path's
/// ops are canonically in), each content op's bytes read at its `src` in `post`.
fn apply(base: &[u8], post: &[u8], ops: &[Op]) -> Vec<u8> {
  let mut sorted = ops.to_vec();
  sorted.sort_by_key(|op| {
    (
      op.at,
      op.kind == OpKind::Delete || op.kind == OpKind::Truncate,
    )
  });
  let mut out = Vec::new();
  let mut cursor = 0usize;
  for op in sorted {
    let at = ix(op.at);
    out.extend_from_slice(&base[cursor..at.max(cursor)]);
    cursor = cursor.max(at);
    let len = ix(op.len);
    match op.kind {
      OpKind::Overwrite => {
        out.extend_from_slice(&post[ix(op.src)..ix(op.src) + len]);
        cursor = at + len;
      }
      OpKind::Insert | OpKind::Extend => out.extend_from_slice(&post[ix(op.src)..ix(op.src) + len]),
      OpKind::Delete | OpKind::Truncate => cursor = at + len,
      other => panic!("not a content op: {other:?}"),
    }
  }
  out.extend_from_slice(&base[cursor..]);
  out
}

fn refined_ops(base: &[u8], post: &[u8], grain: Grain) -> Vec<Op> {
  let mut refiner = Refiner::new();
  let (hunks, _) = refiner.refine_grain(base, post, grain);
  let mut ops = Vec::new();
  ops_of(hunks, (base.len() as u64, post.len() as u64), 0, &mut ops);
  ops
}

/// A source-like text of tokens drawn from a small alphabet, so equal runs recur and the search has choices to make.
fn text() -> impl Strategy<Value = Vec<u8>> {
  let token = prop_oneof![
    Just(&b"fn"[..]),
    Just(&b"let"[..]),
    Just(&b"value"[..]),
    Just(&b"x"[..]),
    Just(&b" "[..]),
    Just(&b"  "[..]),
    Just(&b"\n"[..]),
    Just(&b"("[..]),
    Just(&b")"[..]),
    Just(&b";"[..]),
    Just(&b"42"[..]),
    Just("é".as_bytes()),
  ];
  prop::collection::vec(token, 0..80).prop_map(|tokens| tokens.concat())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// D-O17: do any rewrite of any base at any grain, expect the recovered ops to rebuild the writer's file byte for
  /// byte, and every range the token grain names to start and end on a token boundary of both files.
  #[test]
  fn refining_a_rewrite_reproduces_the_post_state(base in text(), post in text()) {
    for grain in [Grain::Token, Grain::Byte, Grain::Line] {
      let ops = refined_ops(&base, &post, grain);
      prop_assert_eq!(apply(&base, &post, &ops), post.clone(), "{:?}", grain);
    }
    // The anchored search, forced by a budget too small for Myers alone: exact too, and the counter says it ran.
    let mut refiner = Refiner::new();
    let (hunks, cost) = refiner.refine_bounded(&base, &post, Grain::Token, Some(1));
    let mut ops = Vec::new();
    ops_of(hunks, (base.len() as u64, post.len() as u64), 0, &mut ops);
    prop_assert_eq!(apply(&base, &post, &ops), post.clone());
    if cost.kept_whole > 0 {
      prop_assert!(cost.anchored);
    }
    let (hunks, _) = refiner.refine(&base, &post);
    for hunk in hunks {
      for (text, at) in [
        (&base, hunk.base_at), (&base, hunk.base_at + hunk.base_len),
        (&post, hunk.post_at), (&post, hunk.post_at + hunk.post_len),
      ] {
        let at = ix(at);
        prop_assert!(at == 0 || at == text.len() || boundary(Grain::Token, text, at), "{hunk:?}");
      }
    }
  }
}

/// D-O17: do a whole-file rewrite that changes one identifier in a 2,000-line file, expect one range: that identifier.
#[test]
fn a_one_token_change_in_a_rewrite_is_one_token_range() {
  let base: Vec<u8> = (0..2_000)
    .flat_map(|n| format!("    let value_{n} = compute({n});\n").into_bytes())
    .collect();
  let post = String::from_utf8(base.clone())
    .unwrap()
    .replacen("compute(1000)", "measure(1000)", 1)
    .into_bytes();
  let mut refiner = Refiner::new();
  let (hunks, _) = refiner.refine(&base, &post);
  assert_eq!(hunks.len(), 1, "{hunks:?}");
  let hunk = hunks.get(0).unwrap();
  assert_eq!(
    &base[ix(hunk.base_at)..ix(hunk.base_at + hunk.base_len)],
    b"compute"
  );
  assert_eq!(
    &post[ix(hunk.post_at)..ix(hunk.post_at + hunk.post_len)],
    b"measure"
  );
}

/// The bound: do a rewrite whose difference the budget cannot search, expect the ops still to rebuild the writer's
/// file, with the sub-problem kept as one range and counted (the non-vacuity counter of the fallback).
#[test]
fn a_rewrite_past_the_budget_is_kept_as_one_range_and_still_exact() {
  // Every other token changed: an edit distance far past what STEPS_BASE + 64 a unit buys at this size.
  let base: Vec<u8> = (0..40_000u32)
    .flat_map(|n| format!("a{} ", n % 7).into_bytes())
    .collect();
  let post: Vec<u8> = (0..40_000u32)
    .flat_map(|n| format!("{}{} ", if n % 2 == 0 { "b" } else { "a" }, (n * 13) % 11).into_bytes())
    .collect();
  let mut refiner = Refiner::new();
  let (hunks, cost) = refiner.refine(&base, &post);
  let mut ops = Vec::new();
  ops_of(hunks, (base.len() as u64, post.len() as u64), 0, &mut ops);
  assert!(cost.kept_whole > 0, "{cost:?}");
  let units = (base.len() + post.len()) as u64;
  // Both searches are bounded by their budgets; a round may run past its budget by the diagonals it visits.
  let budgets =
    2 * super::STEPS_BASE + units * (super::STEPS_PER_UNIT + super::ANCHORED_STEPS_PER_UNIT);
  assert!(cost.steps <= 2 * budgets, "{cost:?}");
  assert_eq!(apply(&base, &post, &ops), post);
}

/// A work over `base` that rewrites `PATH` whole into `post`: by truncate-and-write when `rename` is false, by
/// writing a temporary file and renaming it over the original (an editor's save) when true.
fn rewrite(
  base_version: u64,
  base: &[u8],
  post: &[u8],
  rename: bool,
  id: u8,
  refine: bool,
) -> Increment {
  let n = post.len() as u64;
  let journal = if rename {
    vec![
      VolumeOp::Create {
        path: "src/.lib.rs.swp".into(),
      },
      VolumeOp::Extend {
        path: "src/.lib.rs.swp".into(),
        at: 0,
        len: n,
      },
      VolumeOp::Rename {
        from: "src/.lib.rs.swp".into(),
        to: PATH.into(),
      },
    ]
  } else {
    vec![
      VolumeOp::Truncate {
        path: PATH.into(),
        len: 0,
      },
      VolumeOp::Extend {
        path: PATH.into(),
        at: 0,
        len: n,
      },
    ]
  };
  let mut base_of = Base::of_files(vec![(PATH.into(), base.len() as u64)]);
  base_of.dirs.push("src".into());
  let (mut doc, mut plan) = compose_volume_planned(&base_of, &journal).unwrap();
  if refine {
    let done = refine_rewrites(&mut doc, &mut plan, &mut Refiner::new(), &mut |path| {
      (path == PATH).then_some((Cow::Borrowed(base), Cow::Borrowed(post)))
    });
    assert_eq!(done.paths, 1, "the rewrite was recognised: {done:?}");
  }
  let post_state = laid(&doc.ops, &plan, post);
  Increment {
    id: [id; 32],
    base: base_version,
    doc,
    post_state,
    evidence: Vec::new(),
    named: Default::default(),
  }
}

/// The post-state the seal lays from `plan` (as the server's `assemble_post_state` does for content ops).
fn laid(ops: &[Op], plan: &PostStatePlan, file: &[u8]) -> Vec<u8> {
  let mut post = Vec::new();
  for op in ops {
    if matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend) {
      let from = ix(plan[&op.src]);
      let end = ix(op.src + op.len);
      if post.len() < end {
        post.resize(end, 0);
      }
      post[ix(op.src)..end].copy_from_slice(&file[from..from + ix(op.len)]);
    }
  }
  post
}

fn green_of(base: &[u8]) -> Green {
  Green::with_origin(Origin {
    files: vec![(PATH.into(), base.to_vec())],
    dirs: vec!["src".into()],
    ..Origin::default()
  })
}

fn source() -> String {
  (0..200)
    .map(|n| format!("fn f{n}(a: u32) -> u32 {{\n    a + {n}\n}}\n"))
    .collect()
}

/// Both agents' rewrites judged in turn from one base: the outcomes and the green's file after.
fn judge(
  base: &str,
  first: &str,
  second: &str,
  (rename, refine): (bool, bool),
) -> (Outcome, Outcome, String) {
  let mut green = green_of(base.as_bytes());
  let one = green.submit(&rewrite(
    0,
    base.as_bytes(),
    first.as_bytes(),
    rename,
    1,
    refine,
  ));
  let two = green.submit(&rewrite(
    0,
    base.as_bytes(),
    second.as_bytes(),
    !rename,
    2,
    refine,
  ));
  let content = String::from_utf8(green.content(PATH).unwrap().to_vec()).unwrap();
  (one, two, content)
}

/// D-O17 (the red case): two agents save whole files, one changing a line near the top and one a line near the end
/// (one through truncate-and-write, one through an editor's write-and-rename); expect both accepted and the green to
/// hold both edits. Unrefined, the second is an `Overlap` conflict: each declared the whole file replaced.
#[test]
fn two_rewrites_of_different_lines_merge_as_their_edits_would() {
  let base = source();
  let first = base.replacen("a + 3\n", "a * 3\n", 1);
  let second = base.replacen("a + 190\n", "a - 190\n", 1);
  let both = first.replacen("a + 190\n", "a - 190\n", 1);
  let (one, two, _) = judge(&base, &first, &second, (false, false));
  assert!(matches!(one, Outcome::Accepted { .. }), "{one:?}");
  assert!(
    matches!(two, Outcome::Conflict { .. }),
    "unrefined: {two:?}"
  );
  both_edits_held(&base, (&first, &second), &both);
}

/// Refined, through truncate-and-write and through write-and-rename: both of `first` and `second` accepted over `base`,
/// the green holding `both`.
fn both_edits_held(base: &str, (first, second): (&str, &str), both: &str) {
  for rename in [false, true] {
    let (one, two, content) = judge(base, first, second, (rename, true));
    assert!(matches!(one, Outcome::Accepted { .. }), "{one:?}");
    assert!(matches!(two, Outcome::Accepted { .. }), "{two:?}");
    assert_eq!(content, both);
  }
}

/// D-O17 with D-27 holding: two agents change different tokens of one line, expect both accepted; two change the same
/// token differently, expect a conflict, never a merge of the two.
#[test]
fn different_tokens_of_one_line_merge_and_one_token_conflicts() {
  let base = source();
  let first = base.replacen("fn f7(a: u32)", "fn g7(a: u32)", 1);
  let second = base.replacen("fn f7(a: u32)", "fn f7(a: u64)", 1);
  let (one, two, content) = judge(&base, &first, &second, (true, true));
  assert!(
    matches!(one, Outcome::Accepted { .. }) && matches!(two, Outcome::Accepted { .. }),
    "{two:?}"
  );
  assert_eq!(content, base.replacen("fn f7(a: u32)", "fn g7(a: u64)", 1));
  let clash = base.replacen("fn f7(a: u32)", "fn h7(a: u32)", 1);
  let (_, two, content) = judge(&base, &first, &clash, (true, true));
  assert!(matches!(two, Outcome::Conflict { .. }), "{two:?}");
  assert_eq!(content, first);
}

/// D-O17: a formatter pass (every body re-indented) beside another agent's edit of one token in a body, expect both
/// accepted and the green to hold the formatted file with the edit.
#[test]
fn a_formatter_pass_beside_an_edit_merges() {
  let base = source();
  let formatted = base.replace("\n    a", "\n  a");
  let edited = base.replacen("a + 42\n", "a + 4200\n", 1);
  let (one, two, content) = judge(&base, &formatted, &edited, (false, true));
  assert!(
    matches!(one, Outcome::Accepted { .. }) && matches!(two, Outcome::Accepted { .. }),
    "{two:?}"
  );
  assert_eq!(content, formatted.replacen("a + 42\n", "a + 4200\n", 1));
}

/// A path the lookup cannot answer, or whose bytes disagree with the declared lengths, keeps its replacement.
#[test]
fn a_rewrite_whose_bytes_are_not_known_keeps_its_replacement() {
  let base = source();
  let post = base.replacen("a + 3\n", "a * 3\n", 1);
  let journal = vec![
    VolumeOp::Truncate {
      path: PATH.into(),
      len: 0,
    },
    VolumeOp::Extend {
      path: PATH.into(),
      at: 0,
      len: post.len() as u64,
    },
  ];
  let (doc, plan) = compose_volume_planned(
    &Base::of_files(vec![(PATH.into(), base.len() as u64)]),
    &journal,
  )
  .unwrap();
  let (mut unknown, mut unknown_plan) = (doc.clone(), plan.clone());
  let done = refine_rewrites(
    &mut unknown,
    &mut unknown_plan,
    &mut Refiner::new(),
    &mut |_| None,
  );
  assert_eq!(
    (done.paths, &unknown.ops, &unknown_plan),
    (0, &doc.ops, &plan)
  );
  let (mut short, mut short_plan) = (doc.clone(), plan.clone());
  let wrong = &post.as_bytes()[1..];
  let done = refine_rewrites(
    &mut short,
    &mut short_plan,
    &mut Refiner::new(),
    &mut |_| Some((Cow::Borrowed(base.as_bytes()), Cow::Borrowed(wrong))),
  );
  assert_eq!((done.paths, &short.ops), (0, &doc.ops));
  let _: BTreeMap<u64, u64> = short_plan;
}

/// The anchored search: do a re-indent of every line of a 2,000-line file (Myers alone runs out), expect it anchored,
/// nothing kept whole, and every recovered range only blanks, so an edit of any identifier beside it stays disjoint.
#[test]
fn a_reindent_of_every_line_is_anchored_and_touches_only_blanks() {
  let base: Vec<u8> = (0..2_000)
    .flat_map(|n| format!("  let value_{n} = compute({n});\n").into_bytes())
    .collect();
  let post = String::from_utf8(base.clone())
    .unwrap()
    .replace("  let", "    let")
    .into_bytes();
  let mut refiner = Refiner::new();
  let (hunks, cost) = refiner.refine(&base, &post);
  assert!(cost.anchored, "{cost:?}");
  assert_eq!(cost.kept_whole, 0, "{cost:?}");
  assert_eq!(hunks.len(), 2_000);
  for hunk in hunks {
    let added = &post[ix(hunk.post_at)..ix(hunk.post_at + hunk.post_len)];
    let removed = &base[ix(hunk.base_at)..ix(hunk.base_at + hunk.base_len)];
    assert!(added.iter().chain(removed).all(|b| *b == b' '), "{hunk:?}");
  }
  // Memory: the scratch is linear in the two files: units (four bytes each, both files) and the anchors' table (one
  // entry a distinct unit). Measured 5.3 bytes a byte here, a token every 3.4 bytes; 2.6 on the A/B's corpus.
  let bound = 8 * (base.len() + post.len()) + (64 << 10);
  assert!(
    refiner.scratch_bytes() <= bound,
    "{} > {bound}",
    refiner.scratch_bytes()
  );
  refiner.release();
  assert_eq!(refiner.scratch_bytes(), 0);
}

thread_local! {
  /// The counting clock's reads: each read is one tick of time, so a turn's deadline is a count of asks.
  static NOW: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

fn counting_clock() -> u64 {
  NOW.with(|now| {
    let at = now.get() + 1;
    now.set(at);
    at
  })
}

/// A source-like file of `lines` lines, and the same file re-indented with one token changed in every hundredth line:
/// a formatter's pass and a codemod's edits at once, the refinement's heaviest path (Myers runs out, then anchors).
fn reformatted(lines: usize) -> (Vec<u8>, Vec<u8>) {
  let base: String = (0..lines)
    .map(|n| format!("  let value_{n} = compute({n}, other_{});\n", n % 97))
    .collect();
  let post: String = base
    .split_inclusive('\n')
    .enumerate()
    .map(|(n, line)| {
      let line = format!("  {line}");
      if n % 100 == 0 {
        line.replacen("compute", "measure", 1)
      } else {
        line
      }
    })
    .collect();
  (base.into_bytes(), post.into_bytes())
}

/// A-787 (§4.3's step budget; CLAUDE.md §3 "Bounded work everywhere"). Do: refine a 3 MiB whole-file replacement (every
/// line re-indented, every hundredth line's call renamed) driven by a pace whose turns end after a few asks, a turn at a
/// time. Expect: it spans many turns, no unit does more than a unit's work (three times `WORK_PER_TICK` at most: the
/// last charge before an ask is at most a unit's work itself), and the document and plan equal the unpaced
/// refinement's. Unsliced, the whole refinement was one unit.
#[test]
fn a_several_mib_rewrite_is_refined_a_bounded_unit_at_a_time() {
  let (base, post) = reformatted(72_000);
  assert!(base.len() > 3 << 20, "{}", base.len());
  let journal = vec![
    VolumeOp::Truncate {
      path: PATH.into(),
      len: 0,
    },
    VolumeOp::Extend {
      path: PATH.into(),
      at: 0,
      len: post.len() as u64,
    },
  ];
  let (doc, plan) = compose_volume_planned(
    &Base::of_files(vec![(PATH.into(), base.len() as u64)]),
    &journal,
  )
  .unwrap();
  let lend =
    |path: &str| (path == PATH).then(|| (Cow::Borrowed(&base[..]), Cow::Borrowed(&post[..])));
  // Unpaced: the reference.
  let (mut whole, mut whole_plan) = (doc.clone(), plan.clone());
  let unpaced = refine_rewrites(&mut whole, &mut whole_plan, &mut Refiner::new(), &mut |p| {
    lend(p)
  });
  assert_eq!(unpaced.paths, 1);
  // Paced: a turn ends a few asks in.
  let (mut sliced, mut sliced_plan) = (doc.clone(), plan.clone());
  let mut refiner = Refiner::new();
  let mut files = |p: &str| lend(p);
  let ticks = crate::pace::Ticks;
  let pace = crate::pace::Pace::new(counting_clock);
  let mut work = Box::pin(super::refine_rewrites_paced(
    &mut sliced,
    &mut sliced_plan,
    &mut refiner,
    (&mut files, &ticks),
  ));
  let mut turns = 0u64;
  let done = loop {
    turns += 1;
    pace.turn(counting_clock() + 8);
    if let core::task::Poll::Ready(done) = pace.drive(work.as_mut()) {
      break done;
    }
  };
  drop(work);
  assert!(turns > 100, "turns {turns}");
  assert!(
    done.longest_unit_work <= 3 * super::WORK_PER_TICK,
    "{done:?}"
  );
  // And absolutely, whatever the constant: no unit does a 64th of the file's work (unsliced, one unit did it all).
  assert!(done.longest_unit_work < base.len() as u64 / 64, "{done:?}");
  assert_eq!(
    (done.paths, done.refined_bytes),
    (unpaced.paths, unpaced.refined_bytes)
  );
  assert_eq!(sliced.ops, whole.ops);
  assert_eq!(sliced_plan, whole_plan);
}
