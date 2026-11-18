# A dynamic volume reserved the whole version slab, so a read after its snapshot was refused NoSpace

**Date:** 2026-11-18. **Area:** `crates/server/src/verbs.rs` (`admit`, the clone, the takeover, the recovery and
the resize of a dynamic volume), `crates/vfs/src/volume.rs` (`next_no`, `admit_needs`), `crates/mem/src/budget.rs`
(`VersionBudget`); design §4.2. **Found by:** kiln's end-to-end test of an agent in a Shards microVM on a Slates
volume (`crates/kiln/tests/slates_e2e.rs`, kiln runs r1481, r1594 and r1617 on HVF, the Mac Studio): the review of
the agent's work was refused `its review: reading .gitignore: a bound is reached: Quota`.

## Description

kiln makes each agent's volume an overlay of its project, dynamic, with a maximum of a quarter of the machine's
memory (24 GiB on the 96 GiB Studio). The agent's microVM made its three edits in the volume, served with no host
mount. kiln's review of a volume with no mount snapshots it, presents the snapshot's landing, and reads each listed
file at the head; reading the project's untouched `.gitignore` was refused `NoSpace` (`Exhausted(Quota)` in
slates-api). It failed the same way in 3 of 3 runs.

## Root cause

Shown by the daemon's own state at the refusal (kiln's e2e prints slates-api's status and daemon report at a failed
step since this bug; r1617):

- The volume held 54 bytes (`referenced 54 B, unique 54 B, attachments 1`).
- Its shard: `volumes 1, version slots 7456540, committed versions 7456539`, `no_space` 1.

So one near-empty volume had committed every version slot but the copy-up headroom. `admit` reserved a dynamic
volume's whole inode allowance at creation ("Bounded and dynamic both reserve their whole logical allowance"), and
the allowance is derived from the volume's maximum (A-109) and capped at the slab: 24 GiB at about 2.3 KB a slot is
more than the slab's 7,456,539 reservable slots.

The read then needed a slot no one had promised. The first lookup of an untouched base file gives its entry an inode
in its directory (`Overlay::materialize`), which makes the directory current. The review's snapshot pins the
directory's version, so making it current retains that version: `make_current_inode` charges one retained version
(`store.versions.charge_retention(1)`) from the unpromised capacity, which was zero. The read itself copies nothing
up: §4.15's rule that untouched entries are served from disk holds (`Overlay::read` reads the base through its
descriptor, charging nothing).

The same root had shown before, in GAPS' AUD-29-25 row (a sized landing volume refused its own landing, 2026-10-01)
and in `docs/bugs/2026-11-05-a-volume-promised-the-whole-version-slab-is-refused-at-recovery.md`. The row left the
admission unchanged and asked: should a dynamic volume reserve version slots for its whole maximum up front, when it
reserves no bytes up front?

Reproduced below kiln, red on main (f6c515447, run r1634, macOS arm64), in
`crates/server/tests/dynamic_versions.rs`:

- A dynamic overlay volume whose maximum derives the whole 64-slot slab: after a write and a snapshot, the first read
  of an untouched base file was refused `NoSpace`.
- A dynamic volume beside a bounded claim of a quarter of the arena was refused `BudgetExceeded { available: 48 }`,
  wanting its maximum's 63 slots.

## Impact

- Every kiln agent volume took a whole shard's version slab, so a kiln instance of two shards could hold two agent
  volumes; the third was refused.
- Any snapshot-then-change of such a volume was refused: kiln's review of a microVM's volume, a landing of the
  volume itself (the 2026-10-01 sibling), and a recovery that rebuilds a snapshot's directories (the 2026-11-05
  sibling).

## Exact edits (A-342)

The question was decided on 2026-11-18 by the supervisor under §4.2's rule, and Ada may veto it. §4.2 holds that only
a bounded claim is sacred: "Dynamic growth, prefetch, caches and other clients consume only unpromised capacity". A
dynamic volume's inode allowance is now a ceiling it reserves nothing of, as its byte maximum is. What a dynamic volume
no longer gets is a guaranteed inode count up to its maximum: past the unpromised slots its next inode is refused
`ENOSPC`, as its next byte is past the unpromised bytes.

- `crates/mem/src/budget.rs`: `VersionBudget::grow`, a dynamic volume's hold taken from the unpromised slots only,
  whole or refused.
- `crates/vfs/src/volume.rs`: `Volume::holds_versions` and `version_hold`. `hold_versions` (the owner's opt-in, taking
  slots for the live inodes now) and `adopt_version_hold` (slots the owner took already). `next_no` grows the hold by
  one for an inode issued past it, refused `NoSpace` when none is unpromised. `admit_needs` admits the fresh inodes'
  growth with the retention, both from the unpromised capacity. A hold never shrinks while its volume lives.
- `crates/server/src/verbs.rs`:
  - `admit` reserves only a bounded claim's allowance; a dynamic volume holds its root's slot as its last fallible
    step.
  - A dynamic clone takes slots for the inodes it inherits before it pins the snapshot, then adopts them as its hold.
  - A taken-over or recovered dynamic volume holds slots for the inodes it was rebuilt with.
  - A resize moves only a bounded claim's reservation.
  - Teardown and every discard of an admitted volume return the hold (`release_version_hold`).
- `crates/server/tests/daemon.rs`: the three capped-slab scenarios fill the slab with a bounded claim and a dynamic
  volume grown into the rest, where before one dynamic volume's maximum filled it.

## Proof

`crates/server/tests/dynamic_versions.rs`, both tests red before and green after; the capped-slab scenarios;
`VersionBudget`'s unit test `a_dynamic_hold_grows_into_unpromised_slots_only_and_returns_whole`.

## Siblings

- The 2026-11-05 recovery refusal: a dynamic volume no longer holds the whole slab, so rebuilding its snapshot's
  directories finds unpromised slots. A bounded claim whose allowance takes the whole slab can still meet it.
- The 2026-10-01 landing-fairness sizing, `SIZED_VOLUME_BYTES`: kept, since it sizes the test's volumes to their files
  and costs nothing now.
