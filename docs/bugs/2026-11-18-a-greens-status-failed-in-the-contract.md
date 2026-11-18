# A green's or a work volume's status failed in the consumer contract (2026-11-18, A-299)

## Description

`slates_api::Instance::status` and `Instance::drift` on a green or a work volume (§4.16) answered
`Err(Protocol(UnknownName { field: "watcher state" }))` instead of the volume's status, with no base. Any consumer
that asked for the status of a merge volume (kiln's merge flows, a review of a green) met a protocol fault for a
healthy volume.

## Root cause

The contract (A-299, piece 4) read an overlay's base from the status reply's text: `watcher` was `"scratch"` for a
volume with no base, `"unavailable"` for an unreadable base, and otherwise a `WatchState` name. The merge service builds
a green's and a work volume's status itself (`crates/server/src/merge_service.rs`) and names its watcher `"merge"`, a
word the reader did not know, so it refused the reply as a protocol fault rather than guess. The text was never a
contract: three producers, each writing its own words.

## Impact

Every status and drift call on a green or a work volume, from slates main 1eb2b055f (2026-11-18) until the fix. Merges,
rebases and approvals were unaffected: they never read a status.

## Fix

- `slates_ipc::protocol::StatusReport` carries the base typed, appended (`base: Option<BaseReport>`, with
  `BaseReport::{Read { drifted, unverified, watcher }, Unreadable}`, `DriftEntry`, `DriftKind`, `WatcherState`). The
  status verb fills it from the base plane's own status (`crates/server/src/verbs.rs`, `base_report`); the merge
  service answers `None`. The `drifted` and `watcher` text stays for people (D-15: append-only).
- `slates-api` reads only the typed field (`crates/api/src/volume.rs`, `status_of`, `base_state_of`); its text reader
  (`base_of`, `drift_line`) is gone, replaced rather than layered. `DriftReport::watcher` is a `Watcher` (CONTRACT 8).

## Evidence

The failing test came first: `a_merge_is_accepted_or_conflicted_typed` (`crates/api/tests/api.rs`) asks for a green's
status and a work's drift; on the kiln runner (macOS arm64, 2026-11-18) it failed `Protocol(UnknownName { field:
"watcher state" })`, then passed with the fix.

## Siblings swept

The contract's other readers of daemon words (`outcome::summary_of`, `LandingState::of_name`, `observe::grant_of`,
`audit_of`) read closed tables written by one function each in `crates/server/src/landing.rs`, which no other producer
writes; each refuses a stranger as a protocol fault, as this reader did. They should move to typed wire fields the same
way (GAPS, A-299's owed list).
