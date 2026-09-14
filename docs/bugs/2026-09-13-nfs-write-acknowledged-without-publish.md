# A mount-transport write was acknowledged `FILE_SYNC` and lost on daemon restart

Date: 2026-09-13. Status: fixed on `agent/recovery-restart` (GAP-A9-6 / BUG-11, the data-plane
sibling). Design: §2.6 boot step 2, §4.8 "Recovery" and the A-9 invariants, D-18; AC-2.12 / T-2.14;
docs/wip/recovery.md §4 "Data-plane content barrier".

## Description

The daemon's NFS transport (`crates/server/src/nfs.rs`) answered every WRITE `FILE_SYNC` and every
COMMIT `NFS3_OK`, and the procedure docs said "slates lands every write in the anchor segment
synchronously". Neither was true: the shard's recovery image was published only by control verbs
(`verbs::dispatch` after create / snapshot / clone / resize / destroy / merge verbs). A file written
over the mount transport after the last control verb was acknowledged as stable and gone after a
daemon restart.

Failing test, before the fix (`cargo test -p slates-server --test recovery -- --exact
acknowledged_content_and_its_snapshot_survive_a_daemon_restart_byte_for_byte`, 2026-09-13): write
`BEFORE` over NFS, snapshot through the client, write `AFTER` over the same file (acknowledged
`FILE_SYNC`), stop the daemon, start a second over the same anchor segment and content object, read
the file over NFS — it read `BEFORE` (the bytes the snapshot verb's publish had captured), not
`AFTER`. The snapshot count and the head were right (the catalog and the snapshot-time image), which
is exactly why "metadata completion tests cannot establish filesystem recovery" (GAP-A9-6).

## Root cause

No barrier on the data plane. `publish_shard` — the only path that puts a volume's bytes into
anchor-owned RAM — was called from `verbs::dispatch` alone; the mount transport's `serve_local`
served a mutating procedure and returned its reply with no publish. The WRITE handler decoded and
discarded `stable_how`; the write verifier was derived from the volume id, constant across a
restart, so a client could never learn that unstable writes were lost.

## Impact

Every byte written through a mount (the live macOS `mount_nfs` path included) between the last
control verb and a daemon crash was lost while acknowledged stable — D-18's "local acknowledgement
promises daemon-restart survival only when bytes ... are recoverable from anchor-owned RAM"
violated on the main data path. The NFS durability gate in docs/wip/recovery.md recorded the
`FILE_SYNC` claim as owed-to-become-truthful; it was not.

## Exact edits

- `crates/bridge-nfs/src/procedures.rs`: `UNSTABLE` is honoured — an `UNSTABLE` write is answered
  `UNSTABLE`, a `DATA_SYNC`/`FILE_SYNC` write `FILE_SYNC`; `Export::set_write_verifier` (the host's
  per-boot verifier); `write_stable_how` (the host's barrier reads the request's level);
  `io_failure_reply` (the `NFS3ERR_IO` failure shape per mutating procedure); the WRITE and COMMIT
  docs state the barrier contract instead of the false synchronous-landing claim.
- `crates/server/src/nfs.rs`: `needs_barrier` (every mutating procedure that succeeded, except an
  `UNSTABLE` write) and `barrier` — `publish_shard` on the owner shard before the reply leaves;
  a refused publish replaces the reply with `NFS3ERR_IO`; a committed image that does not carry
  the touched volume (an overlay, the owed base gate) counts `BARRIER_UNCAPTURED`; the export gets
  the shard's write verifier.
- `crates/server/src/state.rs`: `ShardState::write_verifier` (the shard's boot instant).
- `crates/server/src/daemon.rs`: the verifier at init; `PUBLISH_REFUSED` and `BARRIER_UNCAPTURED`
  counters; the boot message says what recovery did.
- `crates/server/src/verbs.rs`: `publish_shard` returns `Result<Published, VfsError>` (typed, never
  swallowed); `dispatch` logs and counts a refused publish; `trim_unrecorded_snapshots` (the catalog
  is the authority: a snapshot the image carries that no record acknowledged is dropped at
  recovery, AC-2.3); the recovery docs rewritten to state the design.
- `crates/vfs/src/volume.rs`: `Volume::snapshot_ids`.
- `crates/server/tests/recovery.rs`: the oracle (AC-2.12 / T-2.14).

## Siblings found (not fixed here)

- A clone's name is owned by its origin's partition, not the partition `owner_of_name` routes the
  name to (`verbs::clone` runs on the origin, checks uniqueness only there); an NFS `MNT`/root
  `LOOKUP` of such a name routes elsewhere and answers `NOENT`, and the same name can be created on
  its hashed partition. See `2026-09-13-clone-name-not-routable-by-hash.md`.
- A control verb whose publish is refused (`NoSpace`: the image outgrew its slot) still commits its
  effect and completion record — the database transaction has no undo. Counted (`PUBLISH_REFUSED`)
  and surfaced; the §4.2 admission sizing of the content-object slot is the closure
  (docs/wip/recovery.md).
- An overlay volume's diverged state is not in the image (`to_image` refuses a base-backed body),
  so its stable acknowledgements over the mount are counted `BARRIER_UNCAPTURED` rather than made
  true or refused — the owed base gate; an owner decision is recorded in the recovery status.
