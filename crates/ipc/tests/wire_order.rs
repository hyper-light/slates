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
//! Variant` a line), with the guard's shared half (`support/wire_order.rs`), which the records' guard uses too. A
//! variant appended is published in the change that appends it, by the `--ignored` writer, which publishes appends
//! alone. Anything else fails, naming the enum and the first tag that moved.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

#[path = "support/wire_order.rs"]
mod order;

use slates_ipc::protocol::{
  AttachRequest, AttachTransport, AuditKind, BaseReport, CauseRecord, ConflictClass, Conformance,
  DatabaseRefusal, DeleteWhileOpen, DriftKind, EditBytes, EntryKind, Established, GrantScope,
  GrantState, HostAnswer, HostPathReason, Intent, IpcNodeKind, KernelCache, LandingDegradation,
  LandingStage, MoveRefusal, NamePolicy, OwnMountUse, PlannedAction, PolicySubject, Principal,
  ReadAt, ReadWritePolicy, RecoveryCause, Refusal, ReplyBody, RequestBody, Residency, Scope,
  SignatureFault, SignerKey, SizeClass, SnapshotBoundary, TargetPathConstraint, TargetRefusal,
  UnsupportedReason, Verification, VolumeRefusal, WatcherState, WorkOp, WriteAct, WriteMode,
};
use slates_wire::Wire;

/// Format: the published tags, one `Enum tag Variant` a line, sorted by enum and tag.
const PUBLISHED: &str = include_str!("wire_order.txt");

/// Each wire enum's schema (`enum Name{Variant{field:Type,..},..}`), which names its variants in tag order.
fn schemas() -> Vec<&'static str> {
  vec![
    <AttachRequest as Wire>::SCHEMA,
    <AttachTransport as Wire>::SCHEMA,
    <AuditKind as Wire>::SCHEMA,
    <BaseReport as Wire>::SCHEMA,
    <CauseRecord as Wire>::SCHEMA,
    <ConflictClass as Wire>::SCHEMA,
    <DatabaseRefusal as Wire>::SCHEMA,
    <Conformance as Wire>::SCHEMA,
    <DeleteWhileOpen as Wire>::SCHEMA,
    <DriftKind as Wire>::SCHEMA,
    <EditBytes as Wire>::SCHEMA,
    <EntryKind as Wire>::SCHEMA,
    <Established as Wire>::SCHEMA,
    <GrantScope as Wire>::SCHEMA,
    <GrantState as Wire>::SCHEMA,
    <HostAnswer as Wire>::SCHEMA,
    <HostPathReason as Wire>::SCHEMA,
    <Intent as Wire>::SCHEMA,
    <IpcNodeKind as Wire>::SCHEMA,
    <KernelCache as Wire>::SCHEMA,
    <LandingDegradation as Wire>::SCHEMA,
    <LandingStage as Wire>::SCHEMA,
    <MoveRefusal as Wire>::SCHEMA,
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
    <TargetRefusal as Wire>::SCHEMA,
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

/// D-15, A-299: do read every wire enum's schema and compare its variants with the published tags. Expect them equal:
/// a published tag that moved fails, to be undone; an appended variant fails until its change publishes it.
#[test]
fn every_wire_enum_keeps_its_published_tags() {
  order::assert_published("slates-ipc", &schemas(), PUBLISHED);
}

/// D-15, A-299: do find every enum the crate's sources derive `Wire` for. Expect this test to hold each, and only
/// those, so a new wire enum cannot go unpublished.
#[test]
fn every_wire_enum_of_the_protocol_is_held_here() {
  order::assert_held(
    &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
    &schemas(),
  );
}

/// The deliberate writer: publishes appended variants into `tests/wire_order.txt`, refusing while any published tag
/// moved. Ignored, so a normal run never touches the tree.
#[test]
#[ignore = "rewrites crates/ipc/tests/wire_order.txt with appended variants; run deliberately with --ignored"]
fn publish_appended_variants() {
  let text = order::publication(&schemas(), PUBLISHED);
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire_order.txt");
  // The design's `--ignored` writer rewriting a tracked file in the repository (CLAUDE.md §4 "Doc-truth tests");
  // shipped code never reaches this call.
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, text).expect("tests/wire_order.txt is writable");
}
