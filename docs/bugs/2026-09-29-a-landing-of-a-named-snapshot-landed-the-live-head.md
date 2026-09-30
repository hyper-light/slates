# A landing of a named snapshot landed the live head (2026-09-29, AUD-29-02)

## Description

`land(volume, snapshot = S, target)` recorded S in the landing and grant records, then handed the engine
the live, mutable volume. The engine planned, presented, validated and wrote the head's current overlay.
If the volume had been written since S, the bytes landed were not S's, and the durable records claimed S.
A snapshot the volume never had was not refused either, because nothing looked it up. The human approved
the head's manifest, so no unapproved bytes were written; but a request to land an immutable state was
silently turned into landing another one, and the record lied about which.

## Root cause

- `land_verb_unix` computed `db_snapshot` for the records only.
- `slates_land::engine::land` takes a `Volume` and plans from `Volume::diverged` (the head's tree) and reads
  through the head (`read_overlay_bytes`).
- Planning an older snapshot exactly needs more than the snapshot's tree, which the VFS already reads
  (`resolve_in`, `read_in`, `stat_in`, `readlink_in`, `readdir_in`). It also needs the base plane's
  witnesses as the snapshot froze them: the file, whiteout and redirect witnesses a verdict is computed
  from. Those live in head-only maps (`BasePlane::witnesses`, `whiteouts`, `redirects`) that are not
  versioned per snapshot.

## Impact

Landing any snapshot other than the head's current state landed the head instead. A later edit could
reach the disk under an earlier snapshot's name, and the durable landing and grant records named a state
that was not the one written.

## Exact edits

- `crates/vfs/src/journal.rs`: `OpLog::oldest_retained_seq`.
- `crates/vfs/src/volume.rs`: `Volume::unchanged_since(snapshot)`. The head still holds exactly the
  snapshot's state when nothing but snapshots has been journaled since (§4.5: every mutation appends a
  declared operation). It answers `false` when retention dropped any record since, because then nothing
  vouches for the head.
- `crates/server/src/landing.rs`: `named_snapshot_is_head`, run before the target is opened:
  - a snapshot the catalog does not hold for this volume is `NotFound`;
  - a head changed since the snapshot is refused `Unsupported { "landing a snapshot the volume has changed
    since" }` before any host access;
  - otherwise the head, which is the snapshot's state, lands under its name.

  An unnamed landing still lands the live head, as the SDKs document.

## Evidence

- Failing test first: `verbs::tests::a_landing_of_a_named_snapshot_never_lands_a_head_changed_since`. A
  snapshot of "v1" then a head write of "v2" went on to the target and would have landed v2 as the
  snapshot (`TargetUnavailable`, the target being a nonexistent path). It now refuses before the target is
  reached. So does a snapshot the volume never had, and an unchanged snapshot still goes on to its target.
- `crates/vfs/tests/model.rs` `the_head_is_unchanged_since_a_snapshot_until_something_else_is_journaled`:
  a later snapshot keeps the head unchanged, a write changes it, and records dropped since the snapshot
  make it "changed". That last case cannot pass vacuously: without the retention check, the remaining
  snapshot records would answer `true`.

## Owed, then done (2026-09-30)

Exact landing of an older snapshot, done in two changes:
- **A-48:** the base plane's witnesses are frozen per snapshot, versioned by epoch alongside the tree
  they describe.
  `docs/bugs/2026-09-30-a-clone-of-an-older-snapshot-was-judged-by-the-heads-witnesses.md` records the
  clone lost update found on the way.
- **A-49:** the engine's source is explicit (`slates_land::source::Source`) through plan, verdict and
  write, and the advance is relative to the source. An entry the head still holds exactly as the
  snapshot did leaves the overlay; a file the head changed since stays private, rebased onto what landed
  (the fingerprint its placement left and the snapshot's identity); a head whiteout keeps deleting it,
  against the landed fingerprint; anything else stays for a later verdict.
  - The server lands a named snapshot as that source. The `Unsupported` refusal and
    `Volume::unchanged_since` are gone.
  - Red: the daemon test `crates/server/tests/snapshot_landing.rs` on `57a1f2f` was refused
    `Unsupported`.
  - Green: the snapshot's bytes land, the head's edit stays private, and the head then lands over them
    with no conflict. The engine tests in `crates/land/tests/source.rs` cover the same plus a later
    delete and an unknown snapshot.

Still owed: the durable landing and grant records of an unnamed landing name the volume's head snapshot
while the live head lands. They cannot gain a source field without a catalog format version, since a
`Wire` struct cannot grow a field.
