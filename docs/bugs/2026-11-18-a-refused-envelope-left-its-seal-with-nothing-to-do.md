# A refused envelope left its seal with nothing to do (2026-11-18)

Contracts: §4.10 "Content replication" and the A-92 record's piece 3b ("a key or a seal that refuses drops the seal
(`fleet.seal.envelope_refused`) for the next period"), and A-276 (a heal that cannot finish ends, so the healer comes
back to the volume).

## Description

On a node that seals, a seal's archive is wrapped in an envelope before its content is put. When a key or the seal
refused the wrap, the seal was counted `fleet.seal.envelope_refused` once and never tried again: the snapshot stayed
`Local` and its head never placed, whatever the keys did next, until a newer snapshot superseded it. Found by reading
the code while slicing the wrap (g230's predecessor's note, "likely bug, untested"), then shown by a test.

## Root cause

`fleet::advance_seal` set the job's walk to `None` when the walk finished, then called `envelope_of`; on its refusal it
returned `false` and kept the job, with no walk and no archive. The next period's `advance_seal` read "no walk" as
"archive complete" and returned `true`; `content_work` found no archive to take (`job.archive.take()?`) and returned
nothing. Every later period did the same. The healer passes over a volume that has a job, and `sealable_head` keeps a
healing job at its snapshot, so a heal refused this way shut the healer out of the volume for good as well.

## Impact

A sealed volume whose envelope was refused once (a key that would not unwrap or record, a full key region, the random
source) was never placed while that snapshot was its head: an owner loss lost everything since the last placed seal,
with no auto-seal to supersede the stuck snapshot. A heal refused this way left a placed volume unhealed for good.

## Exact edits

- `crates/server/src/fleet.rs`: a refused envelope drops the seal job from its track's map, so the next period's walk
  finds none and starts the seal again from its archive walk (A-322). With A-323 the drop lives in `wrap_slice`'s
  refusal, which every turn of a wrap reaches.
- `crates/server/tests/fleet.rs`: `a_seal_whose_envelope_was_refused_is_started_again_and_places`.
- `docs/wip/SLATES_DESIGN.md` (§4.10's status, the A-92 record, the amendment) and `docs/wip/GAPS.md`.

## Evidence

The test, on a Mac Studio (kiln runs r1213 and r1233, 2026-11-18): three nodes at `f = 1`; the volume's naming key
recorded as a well-formed record wrapped under the node's root (an altered record the namer refuses); a snapshot; the
record destroyed once two refusals were counted.

- On main's code: one refusal counted (`refused 1`), and the snapshot never placed after the record was destroyed:
  `placed false` when the wait gave up, 425.7 s.
- With the job dropped: `refused 2`, the record destroyed, the snapshot placed: 2.2 s, in each of three runs.

## Siblings

- A content round's archive is carried back to the owner shard by `xshard::run_on_counted` (`fleet::put_seal_content`).
  A full control channel drops it, counted `xshard.run_refused`, and leaves the job with no archive exactly as here,
  until a newer snapshot supersedes it. Open (GAPS).
- A seal whose archive walk refuses keeps its spent job: for a seal that is the design's "stays `Local`" (§4.4 coverage
  is never upgraded, and a re-walk would refuse again); for a heal the same job would shut the healer out of the
  volume, but a heal walks a snapshot that was walked whole once already, and the walk of a frozen snapshot is
  deterministic (`slates_vfs::export`), so it does not meet that refusal while its snapshot stands. Reported, unchanged.
