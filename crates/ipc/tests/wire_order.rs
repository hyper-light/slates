//! The protocol's wire order (D-15, append-only; §4.12 "Versioning a consumer pins", A-299). A wire enum's tag is its
//! variant's index (`crates/wire-derive/src/lib.rs`, `expand_enum`), so a variant inserted before another, removed,
//! renamed or moved changes a published tag.
//!
//! Frames carry their body's schema hash, so two builds that disagree refuse each other whole. But a completion record
//! keeps its reply unframed (`CompletionRecord::result`). A daemon restarted on a newer build over the anchor's segment
//! would decode an old reply under the new tags, so a retried request could be answered with another reply than the one
//! recorded. It happened once: 04d7ea465 (A-285) inserted `SubmitHeld` and `EditSubmitHeld` before `SubmitApproved`,
//! moving its tag from 69 to 71 (g267's finding, 2026-11-18). `SubmitApproved` stays at 71: moving it back would be a
//! second move.
//!
//! This test holds every wire enum of the protocol to its published tags (`tests/wire_order.txt`, one `Enum tag
//! Variant` a line). A variant appended is published in the change that appends it, by the `--ignored` writer, which
//! publishes appends alone. Anything else fails, naming the enum and the first tag that moved.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

use std::collections::BTreeMap;

use slates_ipc::protocol::{
  AttachRequest, AttachTransport, BaseReport, CauseRecord, ConflictClass, Conformance,
  DeleteWhileOpen, DriftKind, EditBytes, EntryKind, Established, GrantScope, HostAnswer,
  HostPathReason, Intent, IpcNodeKind, KernelCache, LandingDegradation, NamePolicy, OwnMountUse,
  PlannedAction, PolicySubject, Principal, ReadAt, ReadWritePolicy, RecoveryCause, Refusal,
  ReplyBody, RequestBody, Residency, Scope, SignatureFault, SignerKey, SizeClass, SnapshotBoundary,
  TargetPathConstraint, UnsupportedReason, Verification, VolumeRefusal, WatcherState, WorkOp,
  WriteAct, WriteMode,
};
use slates_wire::Wire;

/// Format: the published tags, one `Enum tag Variant` a line, sorted by enum and tag.
const PUBLISHED: &str = include_str!("wire_order.txt");

/// Format: the protocol's source, where its wire enums are declared.
const PROTOCOL: &str = include_str!("../src/protocol.rs");

/// Each wire enum's schema (`enum Name{Variant{field:Type,..},..}`), which names its variants in tag order.
fn schemas() -> Vec<&'static str> {
  vec![
    <AttachRequest as Wire>::SCHEMA,
    <AttachTransport as Wire>::SCHEMA,
    <BaseReport as Wire>::SCHEMA,
    <CauseRecord as Wire>::SCHEMA,
    <ConflictClass as Wire>::SCHEMA,
    <Conformance as Wire>::SCHEMA,
    <DeleteWhileOpen as Wire>::SCHEMA,
    <DriftKind as Wire>::SCHEMA,
    <EditBytes as Wire>::SCHEMA,
    <EntryKind as Wire>::SCHEMA,
    <Established as Wire>::SCHEMA,
    <GrantScope as Wire>::SCHEMA,
    <HostAnswer as Wire>::SCHEMA,
    <HostPathReason as Wire>::SCHEMA,
    <Intent as Wire>::SCHEMA,
    <IpcNodeKind as Wire>::SCHEMA,
    <KernelCache as Wire>::SCHEMA,
    <LandingDegradation as Wire>::SCHEMA,
    <NamePolicy as Wire>::SCHEMA,
    <OwnMountUse as Wire>::SCHEMA,
    <PlannedAction as Wire>::SCHEMA,
    <PolicySubject as Wire>::SCHEMA,
    <Principal as Wire>::SCHEMA,
    <ReadAt as Wire>::SCHEMA,
    <ReadWritePolicy as Wire>::SCHEMA,
    <RecoveryCause as Wire>::SCHEMA,
    <Refusal as Wire>::SCHEMA,
    <ReplyBody as Wire>::SCHEMA,
    <RequestBody as Wire>::SCHEMA,
    <Residency as Wire>::SCHEMA,
    <Scope as Wire>::SCHEMA,
    <SignatureFault as Wire>::SCHEMA,
    <SignerKey as Wire>::SCHEMA,
    <SizeClass as Wire>::SCHEMA,
    <SnapshotBoundary as Wire>::SCHEMA,
    <TargetPathConstraint as Wire>::SCHEMA,
    <UnsupportedReason as Wire>::SCHEMA,
    <Verification as Wire>::SCHEMA,
    <VolumeRefusal as Wire>::SCHEMA,
    <WatcherState as Wire>::SCHEMA,
    <WorkOp as Wire>::SCHEMA,
    <WriteAct as Wire>::SCHEMA,
    <WriteMode as Wire>::SCHEMA,
  ]
}

/// An enum's name and its variants in tag order, read from its schema: the variants are the schema's parts at depth
/// zero, each a name and its fields in braces.
fn variants_of(schema: &str) -> (String, Vec<String>) {
  let body = schema.strip_prefix("enum ").expect("an enum's schema");
  let open = body.find('{').expect("an enum's schema opens its variants");
  let name = body[..open].to_owned();
  let inner = &body[open + 1..body.len() - 1];
  let mut variants = Vec::new();
  let mut depth = 0i32;
  let mut start = 0;
  for (at, character) in inner.char_indices() {
    match character {
      '{' | '(' | '<' | '[' => depth += 1,
      '}' | ')' | '>' | ']' => depth -= 1,
      ',' if depth == 0 => {
        variants.push(variant_name(&inner[start..at]));
        start = at + 1;
      }
      _ => {}
    }
  }
  if start < inner.len() {
    variants.push(variant_name(&inner[start..]));
  }
  (name, variants)
}

/// A variant's name: its part of the schema before its fields.
fn variant_name(part: &str) -> String {
  part[..part.find('{').unwrap_or(part.len())].to_owned()
}

/// Every wire enum as this build numbers it.
fn current() -> BTreeMap<String, Vec<String>> {
  schemas().into_iter().map(variants_of).collect()
}

/// Every wire enum as published.
fn published() -> BTreeMap<String, Vec<String>> {
  let mut enums: BTreeMap<String, Vec<String>> = BTreeMap::new();
  for line in PUBLISHED.lines().filter(|line| !line.trim().is_empty()) {
    let mut words = line.split_whitespace();
    let (Some(name), Some(tag), Some(variant)) = (words.next(), words.next(), words.next()) else {
      panic!("a published line is `Enum tag Variant`: {line:?}");
    };
    let variants = enums.entry(name.to_owned()).or_default();
    assert_eq!(
      tag.parse::<usize>().ok(),
      Some(variants.len()),
      "the published tags of {name} run from 0 in order: {line:?}"
    );
    variants.push(variant.to_owned());
  }
  enums
}

/// What changed in one enum from its published tags: nothing, variants appended at its end, or a published tag that
/// moved (the first, with what it was and what holds it now).
enum Change {
  None,
  Appended(Vec<String>),
  Moved {
    tag: usize,
    was: String,
    now: Option<String>,
  },
}

/// How `now` differs from `then`.
fn change_of(then: &[String], now: &[String]) -> Change {
  if let Some(tag) = (0..then.len()).find(|tag| now.get(*tag) != then.get(*tag)) {
    return Change::Moved {
      tag,
      was: then[tag].clone(),
      now: now.get(tag).cloned(),
    };
  }
  if now.len() > then.len() {
    Change::Appended(now[then.len()..].to_vec())
  } else {
    Change::None
  }
}

/// Every way this build differs from the published tags: the appends, and the moves.
fn differences() -> (Vec<String>, Vec<String>) {
  let (current, published) = (current(), published());
  let (mut appended, mut moved) = (Vec::new(), Vec::new());
  for (name, now) in &current {
    match published.get(name).map(|then| change_of(then, now)) {
      None => appended.push(format!("{name} is a wire enum not yet published")),
      Some(Change::None) => {}
      Some(Change::Appended(tail)) => appended.push(format!("{name} appended {tail:?}")),
      Some(Change::Moved { tag, was, now }) => moved.push(format!(
        "{name}'s tag {tag} was {was} and is now {}: a variant was inserted, removed, renamed or reordered",
        now.as_deref().unwrap_or("gone")
      )),
    }
  }
  moved.extend(
    published
      .keys()
      .filter(|name| !current.contains_key(*name))
      .map(|name| format!("{name} is published but no longer a wire enum this test holds")),
  );
  (appended, moved)
}

/// D-15, A-299: do read every wire enum's schema and compare its variants with the published tags. Expect them equal:
/// a published tag that moved fails, to be undone; an appended variant fails until its change publishes it.
#[test]
fn every_wire_enum_keeps_its_published_tags() {
  let (appended, moved) = differences();
  assert!(
    moved.is_empty(),
    "published wire tags moved (D-15: append only; a reply recorded under the old tags would be misread):\n{}",
    moved.join("\n")
  );
  assert!(
    appended.is_empty(),
    "variants appended but not published; publish them with `cargo test -p slates-ipc --test wire_order -- \
     --ignored publish_appended_variants`:\n{}",
    appended.join("\n")
  );
}

/// The enums the protocol's source derives `Wire` for, in declaration order: an enum declared after a `#[derive(..)]`
/// line naming `Wire`.
fn declared_wire_enums(source: &str) -> Vec<String> {
  let mut declared = Vec::new();
  let mut derives_wire = false;
  for line in source.lines().map(str::trim) {
    if line.starts_with("#[derive(") {
      derives_wire = line.contains("Wire");
      continue;
    }
    if let Some(rest) = line.strip_prefix("pub enum ")
      && derives_wire
    {
      declared.extend(
        rest
          .split(|c: char| !c.is_alphanumeric() && c != '_')
          .next()
          .map(str::to_owned),
      );
    }
    if line.starts_with("pub enum ") || line.starts_with("pub struct ") || line.starts_with("impl ")
    {
      derives_wire = false;
    }
  }
  declared
}

/// D-15, A-299: do find every enum the protocol's source derives `Wire` for. Expect this test to hold each, and only
/// those, so a new wire enum cannot go unpublished.
#[test]
fn every_wire_enum_of_the_protocol_is_held_here() {
  let declared = declared_wire_enums(PROTOCOL);
  let held = current();
  let missing: Vec<&String> = declared
    .iter()
    .filter(|name| !held.contains_key(*name))
    .collect();
  assert!(
    missing.is_empty(),
    "wire enums of the protocol this test does not hold; add each to `schemas`: {missing:?}"
  );
  assert_eq!(
    declared.len(),
    held.len(),
    "the protocol's wire enums, each held once"
  );
}

/// The deliberate writer: publishes appended variants into `tests/wire_order.txt`. It refuses while any published tag
/// moved, so a move is never published by running it. Ignored, so a normal run never touches the tree.
#[test]
#[ignore = "rewrites crates/ipc/tests/wire_order.txt with appended variants; run deliberately with --ignored"]
fn publish_appended_variants() {
  let (_, moved) = differences();
  assert!(
    moved.is_empty(),
    "a published tag moved; undo the move, never publish it:\n{}",
    moved.join("\n")
  );
  let lines: Vec<String> = current()
    .into_iter()
    .flat_map(|(name, variants)| {
      variants
        .into_iter()
        .enumerate()
        .map(move |(tag, variant)| format!("{name} {tag} {variant}"))
    })
    .collect();
  let text = lines.join("\n") + "\n";
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire_order.txt");
  // The design's `--ignored` writer rewriting a tracked file in the repository (CLAUDE.md §4 "Doc-truth tests");
  // shipped code never reaches this call.
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, text).expect("tests/wire_order.txt is writable");
}
