//! The records' wire order (D-15, append-only; GAPS, g267's finding of 2026-11-18): the database's log keeps every
//! record Wire-encoded, and a wire enum's tag is its variant's index (`crates/wire-derive/src/lib.rs`, `expand_enum`).
//! A variant inserted before another, removed, renamed or moved changes a published tag, so a daemon of a newer build
//! replaying an older log over the anchor's segment would read one record as another.
//!
//! This test holds every wire enum of the records to its published tags (`tests/wire_order.txt`, one `Enum tag
//! Variant` a line), with the protocol guard's shared half (`crates/ipc/tests/support/wire_order.rs`). A variant
//! appended is published in the change that appends it, by the `--ignored` writer, which publishes appends alone.
//! Anything else fails, naming the enum and the first tag that moved.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

#[path = "../../ipc/tests/support/wire_order.rs"]
mod order;

use slates_db::catalog::{
  AttachForm, AuditKind, BaseRecord, Consumer, GrantScope, GrantState, GrantSurface, HookPointName, LandingState, LogName, MemberOrigin, NameChange, NamePolicy, ObjectFormat, PlacementState, PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyWho, Principal, RepositoryCheck, Role, SealKeyOwner, SizeClass, StateChange, StateLog, VcsTool, VolumeState, WriteMode,
};
use slates_db::op::Op;
use slates_wire::Wire;

/// Format: the published tags, one `Enum tag Variant` a line, sorted by enum and tag.
const PUBLISHED: &str = include_str!("wire_order.txt");

/// Each wire enum's schema (`enum Name{Variant{field:Type,..},..}`), which names its variants in tag order.
fn schemas() -> Vec<&'static str> {
  vec![
    <AttachForm as Wire>::SCHEMA,
    <AuditKind as Wire>::SCHEMA,
    <BaseRecord as Wire>::SCHEMA,
    <Consumer as Wire>::SCHEMA,
    <GrantScope as Wire>::SCHEMA,
    <GrantState as Wire>::SCHEMA,
    <GrantSurface as Wire>::SCHEMA,
    <HookPointName as Wire>::SCHEMA,
    <LandingState as Wire>::SCHEMA,
    <LogName as Wire>::SCHEMA,
    <MemberOrigin as Wire>::SCHEMA,
    <NameChange as Wire>::SCHEMA,
    <NamePolicy as Wire>::SCHEMA,
    <ObjectFormat as Wire>::SCHEMA,
    <Op as Wire>::SCHEMA,
    <PlacementState as Wire>::SCHEMA,
    <PolicyEffect as Wire>::SCHEMA,
    <PolicyEntry as Wire>::SCHEMA,
    <PolicyNames as Wire>::SCHEMA,
    <PolicyRight as Wire>::SCHEMA,
    <PolicyWho as Wire>::SCHEMA,
    <Principal as Wire>::SCHEMA,
    <RepositoryCheck as Wire>::SCHEMA,
    <Role as Wire>::SCHEMA,
    <SealKeyOwner as Wire>::SCHEMA,
    <SizeClass as Wire>::SCHEMA,
    <StateChange as Wire>::SCHEMA,
    <StateLog as Wire>::SCHEMA,
    <VcsTool as Wire>::SCHEMA,
    <VolumeState as Wire>::SCHEMA,
    <WriteMode as Wire>::SCHEMA,
  ]
}

/// D-15: do read every record enum's schema and compare its variants with the published tags. Expect them equal: a
/// published tag that moved fails, to be undone; an appended variant fails until its change publishes it.
#[test]
fn every_record_enum_keeps_its_published_tags() {
  order::assert_published("slates-db", &schemas(), PUBLISHED);
}

/// D-15: do find every enum the crate's sources derive `Wire` for. Expect this test to hold each, and only those, so a
/// new record enum cannot go unpublished.
#[test]
fn every_wire_enum_of_the_records_is_held_here() {
  order::assert_held(
    &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
    &schemas(),
  );
}

/// The deliberate writer: publishes appended variants into `tests/wire_order.txt`, refusing while any published tag
/// moved. Ignored, so a normal run never touches the tree.
#[test]
#[ignore = "rewrites crates/db/tests/wire_order.txt with appended variants; run deliberately with --ignored"]
fn publish_appended_variants() {
  let text = order::publication(&schemas(), PUBLISHED);
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire_order.txt");
  // The design's `--ignored` writer rewriting a tracked file in the repository (CLAUDE.md §4 "Doc-truth tests");
  // shipped code never reaches this call.
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, text).expect("tests/wire_order.txt is writable");
}
