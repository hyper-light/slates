# The volume core's refusals reached consumers as words (2026-11-18, A-299, found by A-310)

## Description

Twenty kinds of volume-core refusal reached every client as `Refusal::BadRequest { reason: "<the error's name>" }`:
a pinned snapshot's destroy (`"Pinned"`), a volume a green's origin holds, a guest-mapped file's change
(`"DaxMapped"`), a scoped mount of a file (`"NotDirectory"`), a non-empty directory's removal, a stale handle, and the
rest. The consumer contract (A-299) could only pass them on as `Error::Invalid(Invalid::Request(note))`, so kiln could
not tell "end the clone first" from "the request is malformed", and A-310's simulator had to answer the word to
conform.

## Root cause

`refusal_of_vfs` (`crates/server/src/error.rs`) named some volume-core errors and sent every other through a catch-all
arm, `other => Refusal::BadRequest { reason: format!("{other:?}") }`. A catch-all is an uncategorized refusal
(CLAUDE.md §3), and it also meant a new volume-core error compiled without anyone deciding its class.
`VfsError::BarrierIncomplete`, which the wire has its own variant for, went through it too.

## Impact

Every verb whose volume-core refusal was one of those: snapshot destroys, rewitness, the file verbs and mounts, since
the daemon began. A client saw a typed refusal class only for the errors named before the catch-all.

## Fix

- The wire appends `Refusal::Volume { refusal: VolumeRefusal }`, one variant per volume-core error the others do not
  name (the journal's and the recovery image's numbers kept).
- `refusal_of_vfs` is exhaustive: each error maps to its variant, and `BarrierIncomplete` to the wire's own. No
  catch-all remains, so a new error does not compile until it is classified.
- `slates-api` classes each by what a consumer does: `Busy::Volume` (`Pinned`, `DaxMapped`), `Stale::Volume`,
  `Denied::Volume`, `Exhausted::Volume`, `Unusable::Volume`, `Invalid::Volume`. The simulator answers a pinned
  destroy `Busy::Volume(Pinned)`, as the daemon does.

## Evidence

- `crates/server/tests/recovery.rs`: the two recovered-pin tests now expect `Refusal::Volume { refusal: Pinned }`.
- `crates/server/tests/fuse_mount.rs` (Linux): a scoped mount of a file expects `Volume { NotDirectory }`.
- `crates/api/tests/api.rs`, `the_seam_conforms`: a pinned snapshot's destroy is `Busy::Volume(Pinned)` from both
  the daemon and the simulator.
- `each_volume_core_refusal_is_typed_into_its_class` (unit) places every reason in its class.

## Siblings

The other words the daemon sends in place of types are tracked in GAPS under A-299: `TargetUnavailable`'s reason,
`Unsupported`'s feature, `Unpublished`'s reason, `ImageRefused`'s reason, `TableAtCapacity`'s table, and the
landing-state, action, audit-kind and grant-state words the contract reads from closed tables.

## Siblings found after the fix (2026-11-18, the integrator's report)

The catch-all's removal left five arms that never reached `refusal_of_vfs`: each matched the volume core's directory
error first and answered in words. The integrator found the first while fixing
`a_writable_map_needs_a_write_attachment_and_a_file` (1128e2222):
- `crates/server/src/direct_map.rs`: a read map of a directory (`Begun::Directory`, two arms) answered
  `BadRequest { "the path is a directory" }`. The variant is gone; the fill answers `Begun::Vfs(IsDirectory)`, which
  the existing arm maps.
- `crates/server/src/merge_service.rs`: a whole read (`read_plain`) and a paged read (`read_plain_range`) of a
  directory answered the same words.
- `crates/server/src/listing.rs`: a listing of a file (`plain_page`) answered `BadRequest { "the path is not a
  directory" }`.

Each is now `Refusal::Volume { IsDirectory }` or `{ NotDirectory }`. The tests came first, one per site, in
`crates/client/tests/client.rs`: `a_read_map_of_a_directory_is_refused_typed`,
`a_read_of_a_directory_is_refused_typed_whole_or_paged` (since g247's landing `a_paged_read_of_a_directory_is_refused_typed`:
the whole-file `Read` and `read_plain` are retired by A-267, the verb refused `Unsupported` in its published wire
place) and `listing_a_file_is_refused_typed`.

The sweep for every other spelling found no further volume-core error in words. It did find 11 arms in
`crates/server/src/verbs.rs` (lines 5885, 5917, 6113, 6198, 8955, 8998, 9206, 10414, 10487, 11608 and 11632 on
f64b8c33c) that send a memory error other than `BudgetExceeded`, or a registry insert's error, at admission as
`BadRequest { e.to_string() }`. That is the same fault in another class, and it was fixed the same day. Each of the
11, the two arms that answered a full registry with `BadRequest { "volume registry full at N" }`, and a database's
memory refusal now go through `refusal_of_mem` (`crates/server/src/error.rs`). It types them as the volume core types
`VfsError::Memory`: `BudgetExceeded` with what is available, a lock refusal with what was locked, and the rest
`Refusal::Volume { Memory }`.

No test reaches these arms by use today. Each reservation refuses only `BudgetExceeded` (`Ledger::take`), and the
registry shares its bound, `caps.volumes`, with the database's volume table, which a create checks first. So their
proof is the mapping's unit test, `every_memory_refusal_at_admission_is_typed`.
Still owed: `refusal_of_db`'s last catch-all sends `Corrupt`, `NfsInstanceRegressed`, `CheckpointOutOfRange`,
`SealKeyMalformed` and `Wire` as words.

