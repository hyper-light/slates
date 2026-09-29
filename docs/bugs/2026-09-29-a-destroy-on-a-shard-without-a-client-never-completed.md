# A destroy on a shard without a client never completed (2026-09-29)

## Description

The provisioning histogram (`cargo run --release -p slates-client --example provision_bench`, the R9 /
AC-2.1 gate) could not run. It aborted in its first form with `Refused(BudgetExceeded { available: 1478 })`
and 51 lines of "a volume was not imaged, skipped: ESTALE". It did the same at `fd7b7b0` and at `cead594`.
Its last recorded run is 2026-09-05.

A refusal diagnostic in a scratch copy of the bench read the daemon's status at the refusal, the 2,213th
create/destroy pair of the one-client form:

| Partition | Live volumes | Version slots committed |
|---|---|---|
| 0 | 832 | 3,028,480 of 3,029,959 |
| 4 | 831 | 3,024,840 |
| 1 | 453 | — |
| 2 | 43 | — |
| 3 | 0 | 0 |

Every `destroy` had answered `Destroyed`.

## Root cause

Two defects, one exposing the other.

**Destroys were stepped only for the owner shard's own clients.**

- A volume's owner is the partition its name hashes to (`owner_of_name`), so a client's creates land on
  every shard, and a destroy is forwarded to the owner.
- The destroy verb marks the volume `Destroying` and returns. Its cooperative slices run in
  `step_destroys`, which runs only inside `serve_round`.
- `serve_round` runs only when the shard's serve loop runs, and that happens only for that shard's own
  clients' rings.
- So a destroy forwarded to a shard with no client of its own never ran a slice. The volume stayed
  `Destroying` with every reservation held. The bench's client sat on partition 3, which alone completed
  its destroys.
- The reaper's cadence did not step destroys either.

**Every publish imaged the volumes being destroyed.**

- Every mutating verb republishes the shard's recovery image (`publish_shard`), and the publish walked
  every volume, including those mid-destroy.
- A volume whose slices had released part of its tree failed `ESTALE`. It was counted as skipped
  (`PUBLISH_SKIPPED`, a health signal meant for base-backed overlays) and printed a line — per volume, per
  publish, on the verb's latency path.
- Once destroys completed, the bench still printed 31,378 such lines in one run: the few volumes caught
  mid-destroy by the next create's publish.
- A destroying volume needs no image. Its catalog record says `Destroying`, and recovery completes a
  recorded destroy from the catalog (`complete_recovered_destroys`); the rebuild filters such volumes out.

**Found beside them.** `step_destroys` discarded a refused `VolumeDestroyed` record (`let _ =`) and let
go of the volume's slot and credits anyway, while the catalog still held it.

## Impact

- **Admission leaked.** Any destroy that reached a shard without a client of its own left its bytes,
  versions and metadata reserved until one of that shard's clients spoke. A long-lived daemon serving
  clients on a few shards could exhaust the others' admission with volumes it had acknowledged destroying.
- **The R9 gate could not run at all.**
- **Publishes wasted work.** Every publish during a destroy imaged a dead tree, raised a false health
  signal and wrote a log line inside a verb.

## Exact edits

- `crates/server/src/verbs.rs`:
  - `destroy` wakes the owner's serve task, whose rounds step the destroy until it completes.
  - `step_destroys` keeps a round going only while a slice made progress. A refused `VolumeDestroyed`
    record keeps the volume's slot and credits and is counted by the refusal's name.
  - `publish_shard` leaves out volumes in `Destroying` or `Destroyed` (`Published::destroying`), and a
    control verb's barrier accepts a touched volume that is being destroyed.
  - A volume that genuinely cannot be imaged is counted every time and logged once.
- `crates/server/src/daemon.rs`: `reap_loop` steps destroys at its cadence, which retries a refused record
  without a busy round (the reaper's existing retry schedule for failed cleanup).
- `crates/server/src/nfs.rs`: the barrier test's `Published` gains the field.

## Evidence

- Failing tests first:
  - `crates/server/tests/daemon.rs` `a_destroy_completes_on_an_owner_shard_that_has_no_client` failed with
    `[(4, 4194304), (0, 0)]` (volumes and committed bytes per shard) still held after 5 s. It passes, all
    zero, in 1.2 s.
  - `verbs::tests::a_publish_leaves_out_a_volume_being_destroyed` failed with the volume in `skipped` and
    the ESTALE line printed. It passes with nothing skipped.
- Suites: server library 123, daemon 16, recovery 6, NFS mount 12, observe 6, attach forms 4,
  virtio-fs 2, fleet 59 of 59 (261 s at host load 17), CLI 13 of 13.
- The provisioning histogram now runs to its end with no "not imaged" line. It fails the 50 µs floor
  on this loaded host (load 10–14, other sessions' model checks and clusters):

  | Form | p50 | p99 |
  |---|---|---|
  | One spinning client | 52 µs | 177 µs |
  | Status round trip | 16 µs | — |
  | Recorded 2026-09-05 (quiet host) | ~9 µs | ~25 µs |

  The gap between create and status says the create path itself regressed. Its cause is recorded in GAPS
  and measured next, not assumed.
