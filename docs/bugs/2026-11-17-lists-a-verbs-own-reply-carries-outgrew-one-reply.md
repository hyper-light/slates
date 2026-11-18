# Lists a verb's own reply carries outgrew one reply (2026-11-17)

Goal g247 (every reply list bounded and paged), A-267. Reproduced by use on main 0366b68a5 merged with the re-landed
A-263 to A-266 (`crates/server/tests/reply_bounds.rs`, macOS arm64, kiln runner d1323898, 2026-11-17).

## Description

A reply is one bulk chunk, 4,096 bytes (§4.3; `crates/server/src/config.rs` `BULK_CHUNK_BYTES`). When a reply is
larger, `crates/server/src/verbs.rs` (the reply writer) sends `Refused { BadRequest { reason: "reply too large for the
bulk area" } }` in its place. Four verbs answered a list in their own reply with no bound, and each was refused that way
at 400 entries with names of about sixty bytes:

- `advance` of a green attachment over a span that changed 400 paths: `Advanced { invalidated }`;
- `rewitness` of 400 drifted entries: `Rewitnessed { paths }`;
- a landing presentation with 400 conflicts: `GrantRequired { conflicts }`;
- a granted landing whose step-4 verdict found 400 conflicts: `Refused { LandingConflict { entries } }`.

The test's first run, red on all four: `Err(Refused(BadRequest { reason: "reply too large for the bulk area" }))`.

## Root cause

Each list was built whole from the verb's work (`Green::changed_between`, `Volume::paths_changed_between`, the base
plane's rewitness, the engine's preliminary or step-4 verdict) and put in the reply as it was. Nothing compared it with
what a reply holds. A-263 to A-266 paged the lists that are reads (volumes, changed paths, the audit, grants), and
A-267's first part bounded a status's drift; these four ride in the reply of a verb with an effect, so they were left.

## Impact

- **An effect with a lost reply.** The advance had re-pinned the attachment and the rewitness had re-witnessed every
  entry when the reply was refused, so the caller was told `BadRequest` for something that happened.
- **A recorded presentation nobody could name.** The presentation was in the table, its audit record written, and the
  reply that named its landing id refused: the landing could be neither granted nor read.
- **A typed refusal turned untyped.** A granted landing refused for its conflicts answered `BadRequest`, so a consumer
  could not tell "the disk changed, present again" from a malformed request.

## Edits

- `crates/ipc/src/protocol.rs`: `Advanced::invalidated_elided`, `Rewitnessed::paths_elided`,
  `GrantRequired::conflicts_elided`, `Refusal::LandingConflict::entries_elided` (each a `u32`, each documented with what
  a reader does past it).
- `crates/server/src/verbs.rs`: `carried(items, room)` (the longest prefix one reply holds, the rest counted; unlike a
  page's `fitting`, no item is forced in, so one long path is counted and never sent as a reply no chunk holds),
  `reply_room(empty)` (a chunk less the reply's framing with its list empty; a sequence's length prefix and a count are
  fixed-width on the wire, so the bound is exact), and `advanced_reply`, used by the green advance
  (`crates/server/src/merge_service.rs`) and the snapshot mount's (`crates/server/src/snapshot_view.rs`); the rewitness
  reply.
- `crates/server/src/landing.rs`: the presentation's reply (its landing record still counts every conflict) and the
  step-4 refusal.
- `crates/client/src/client.rs`: `Advanced::invalidated_elided`, `Landing::GrantRequired::conflicts_elided`, and
  `rewitness` answering `Rewitnessed { paths, paths_elided }`.
- `crates/cli/src/verbs.rs`, `crates/mcp/src/lib.rs`, `crates/sdk-python/src/lib.rs`, `crates/sdk-node/src/lib.rs`: each
  shows the count (text lines say how many more; JSON and the SDKs' objects carry it).
- `crates/api/src`: `Instance::rewitness` answers `Rewitnessed`, `Stale::Disk::elided`, `LandingReport::degraded_elided`,
  and a presentation is clean only when it carries and counts no conflict; `CONTRACT` = 11.

## Siblings

- **A finished landing's degraded cells**, fixed here: `LandingOutcome::degraded` carried every cell, and a host that
  refuses a directory's sync (as some network filesystems do) meets one a touched directory, so a landing over a few
  hundred directories answered "reply too large" after it had run. `landing::landed_reply` carries what fits and
  `degraded_elided` counts the rest; read by every surface (the CLI's line, MCP's and the Node SDK's field,
  `slates-api`'s `LandingReport::degraded_elided`). Its unit test builds 400 such cells.
- **A status's mounts**, fixed here: the budget (a quarter of a reply) counted each mount's path and not its whole
  encoding (its attachment and its length prefix, twelve bytes more), so 400 mounts at paths of two to four bytes
  outgrew the status reply. `mounts_of` now counts each mount's encoding. Reproduced by use
  (`a_status_with_more_mounts_than_its_budget_holds_is_answered`).
- `Submitted` and `Rebased` carry a verdict's conflict windows the same way, refused at 400 windows by the same test's
  first form. They need every window, not a count, since a work resolves each, so they are kept with the work and paged
  from it (next, A-267).
