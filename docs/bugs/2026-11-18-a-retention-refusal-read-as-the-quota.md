# A snapshot's retention refused read as the volume's quota (2026-11-18, A-299, found by A-342's diagnosis)

## Description

kiln's review of an agent's volume snapshots it, then reads the volume's files at the head. In kiln's microVM drive the
read was refused `a bound is reached: Quota`, so every reader of that message went looking at the volume's quota
(kiln's RAM/4 maximum), which was never the bound. The bound was the shard's version slab. After the snapshot, the
head's first change of a file the snapshot holds needs a retained version from unpromised capacity, and there was
none (A-342's record: one dynamic volume had reserved the whole slab).

## Root cause

Every retention refusal of the volume core answered `VfsError::NoSpace`, the quota's own error, which the daemon sends
as `Refusal::NoSpace` and the contract classes `Exhausted(Quota)`. That covers:
- a retained version's charge, or the volume's retention allowance (`make_current_inode`);
- a retained byte charge (`secure_retention`);
- a rename's retention preflight;
- the recovery's re-charge (`reestablish_retention`);
- the retention half of a verb's admission (`admit_needs`).

§4.2 names the refusal apart (`RetentionBudgetExceeded`): retention is freed by destroying a snapshot, not by deleting
data or raising the quota. So the one error sent a person to the wrong fix.

## Impact

Every consumer that met a retention refusal since the retention charges were built (GAP-A9-1) read it as its quota.
That is the API's `Exhausted(Quota)` and the wire's `NoSpace`; at a mount it was and stays `ENOSPC`. kiln's review
misdiagnosis on 2026-11-18 is the one seen.

## Fix

- The volume core answers `VfsError::RetentionExceeded` at each of those sites (`crates/vfs/src/volume.rs`).
  `admit_needs` now names its bound: the retention when the copy-ups alone do not fit, else the dynamic volume's
  growth (`NoSpace`, as before).
- At every mount it is `ENOSPC`, as `NoSpace` is: FUSE, NFS, WinFsp's `STATUS_DISK_FULL`, FSKit.
- The wire appends `VolumeRefusal::RetentionExceeded`. The contract classes it `Exhausted::Volume(RetentionExceeded)`,
  apart from `Exhausted::Quota` (`CONTRACT` 18).

## Evidence

- `a_snapshots_retention_refused_is_not_the_quota` (`crates/api/tests/api.rs`) runs on a daemon whose version slab is
  capped at 64 slots. A dynamic overlay grows until its growth is refused `NoSpace`; then a change to a file its
  snapshot holds is refused `Volume { RetentionExceeded }`, and the file is unchanged.
- The volume core's retention tests now expect `RetentionExceeded`: `retention.rs`, `retained_bytes.rs`,
  `recover.rs`'s retention allowance. `tenant_isolation.rs` and `namespace_refusal.rs` accept it as a clean refusal.
- Red-checked: with the sites answering `NoSpace` again, those tests fail.

## Siblings

- `Error`'s rendering still prints most classes by their `Debug` (`a bound is reached: Volume(RetentionExceeded)`).
  Words per class are owed (GAPS, A-299).
