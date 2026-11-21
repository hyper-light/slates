# w-shared: closed items (goal 93's shared pieces), base origin/main ed77b1d30

Each item below is on origin/main (`git merge-base --is-ancestor <commit> origin/main` holds for each).

1. **Inputs held at B** (A-697 (1): B holds its own composed inputs, so A's record to B is record-only).
   Commit f26544b65 (landed by fb692bdd5). Test: `cargo test -p slates-server --test fleet
   a_holders_composed_inputs_are_never_shipped_back_to_it`. It asserts `merge.inputs.held_by_composer` >= 1 and
   that no input chunks are shipped to the composer (`merge.inputs.chunks_shipped` stays 0 toward B).
2. **Sleep-and-look waits, family one** (a lent record session's return wakes its borrower).
   Commit c03a8953b (landed by 86e23c4ef). Test: `cargo test -p slates-server --test fleet
   a_forward_waiting_for_its_lent_session_is_woken_by_its_return`. It asserts `fleet.session_wait.returned` >= 1.
3. **Sleep-and-look waits, family two** (a dispatch's stragglers are woken by their reports).
   Commit 54b9d8b2b (landed by ab30bfb75). Test: `cargo test -p slates-cluster --test live
   a_lane_that_ends_with_a_holder_unanswered_gets_every_session_back`. It asserts woken >= 1 and ticked == 0.

Still open, in close-list order:
- Completion: A-697 f, then A-658 3c.
- Robustness: family three (the FUSE helper's exit is woken by a pidfd or NOTE_EXIT watch; agents/w-shared-sweep-3
  2aa0d1782 is waiting for its Linux run, r6), then exit 4 at 64 MiB.
- Performance: the lane's ~200 MB/s, then the content-plane-versus-lane A/B.

The sleep-and-look item closes when family three lands.

## Closed 2026-11-20 ~18:05 PST: A-697 part f (takeover + routing flip; the hook deleted in the same change)
Commit a9e789e54 on origin/main (landed as 56f33c6f6). `Daemon::serve_works_on_holders` / `WorkHome::on_holders` are
gone. The rule "replicated here" (`work_home::green_elsewhere`) is in force: the control shard reads `merge.replicas`,
and every other shard reads the fanned `WorkHome::holding`. The list is fanned per period and at once on a replica's
first version.
Tests (macOS arm64, local; on the merged tree):
- `cargo test -p slates-server --test fleet a_holders_work_is_committed_by_its_greens_successor_after_the_owner_dies`
  (new, 3 nodes at f = 1; the survivor that doesn't take over submits after A dies; answered v2; counters
  `merge.work.computed_on_holder` and `merge.verdict.taken_by_position` at the successor moved).
- The two-node e2e on production routing:
  `a_holders_work_is_committed_by_the_verdict_its_replica_computed_taken_by_position_at_the_owner`.
- Filters: fleet `a_holder` + `a_works_route_ends` 26 of 26; lib `work_home` + `fleet::tests` 23 of 23 (new
  `a_replica_taken_reaches_the_other_shards_with_its_owner_once`); fmt clean.
- Red, with the rule off: both holder tests failed ("the work's id names B", "the work was made on the holder").
A-697 is complete with this (parts a1-f). Please mark it met.

## Landed 2026-11-20 ~19:40 PST: the lane's instrument (replication_bench's bulk row), not the lane item itself
Commit 3a9e5bf21 on origin/main (squash of agents/w-shared-lane). The bulk row times one N MiB request on a warm
session, one direction, and prints goodput, path MTU, congestion window and `send_stops` deltas. BENCHMARKS has the
command and no numbers yet: the first run was at load 74. Focused check: built, and ran at 1 MiB locally (debug);
fmt clean.
The lane item (~200 MB/s) stays open, under performance in the close order. The lead: the congestion window bounds the
lane on loopback (64 MiB: about 6,800 window stops), and the suspect is the UDP receive buffer left at the OS default
(~85 datagrams; this Mac has dropped 11.4 M datagrams on full socket buffers since boot).

## Landed 2026-11-20 ~22:50 PST, toward exit 4 (the item stays open until r9 reports): a version's inputs archived once
Commit 8f04c9130 on origin/main (landed as c990989f7). Before, every shipment pass of a later version copied its
increment out of the chain and rebuilt and rehashed its whole inputs archive in one owner step: a 64 MiB version took
at least 64 passes of about 128 MB copied and 64 MB hashed. Now the manifest is made once and the chunks are lent from
the chain (`merge.inputs.manifest_made`).
Tests (macOS arm64, local; on the merged tree):
- new lib oracle `a_shipment_cut_from_lent_bytes_is_the_whole_archives_byte_for_byte`;
- new cluster oracle `a_lent_raw_chunks_request_is_the_held_chunks_byte_for_byte`;
- new fleet `a_version_crossing_in_several_shipments_is_archived_once` (red: made 3 over 3 shipments);
- merge_service + merge_origin 32 of 32.
Exit 4 closes when kiln r9's 64-320 MiB reruns show no watchdog end (or name another cause, which I'll fix next).

## Closed 2026-11-21 ~00:35 PST (machine clock): exit 4 at 64 MiB (robustness)
Commit c592a81f0 on origin/main (landed as 5ab1c23b5). Cause, named by the watchdog and a `sample` profile: the
deriver composed a path by walking a flat piece list from its front for every declared op, so a file written a few KiB
a write cost the square of its writes. A 128 MiB submit spun shard 1 for 1.0 to 1.2 s in one poll (exit 4), on a quiet
Studio-1 as well as a loaded MacBook. Fix: blocked piece list (about √ops a step), new runs merged.
Measurement: A-697's release bench on Studio-1 at 64, 128, 256 and 320 MiB, every size passing, exit 0 (before: exit 4 at
128 to 320). BENCHMARKS has the phase table. Tests: slates-merge --test derive 10 of 10 including the new cost bound
(red: 55,109,413 examined against 12,000,000); slates-merge lib 248 of 248.
The earlier record-plane piece (8f04c9130) was real (inputs rebuilt per shipment) but was not exit 4's cause.
Found, in GAPS, not on my list: the holder's forward runs at ~105-150 MB/s in this bench (320 MiB in 3,047 ms) against
the transport's 1.3 GB/s, and the control shard's beat task runs 0.5-1.4 s late during it. That bears on the lane item;
see inbox/main-from-w-shared-lane.md.

## 11:40 PST CLOSED: sweep family three (FUSE helper exits awaited by their exit watch), main c5051eb51
- Linux (adacachy, Linux 7.2.9, kiln r25, cb2eb2447 = origin/main + family three): fmt clean; fuse_mount 8/8 green on a
  real FUSE host; red with every exit watch refused: a_volume_mounted_through_fuse... fails at its assertion (typed
  ExitUnwatched, no hang). slates-bridge-fuse: every suite green but takeover_round, 2 of 2 failing in code family three
  does not touch (main's A-795 removed the S3 base from bridge.rs/channel.rs since); in GAPS, main alone checked by r29.
- Landed over a docs-only merge (no rerun, fast landing rule). Branch and worktree deleted.

## 11:47 PST CLOSED (code): A-826, lane (A) 3b, content ahead by identity, main 156007dff
- A holder's SubmitComputed names its increment (identity, length); the owner fetches it from the work's member on the
  content plane and holds it once. Red at 8f73b6928 (r26: the request was the old form). Green: holder fleet tests
  39/39 and server lib 1102 (+1 flaky content_ranges idle sweep, in GAPS) on c51be45 (r28); merged tree type-checks,
  fmt clean and the 3 holder-submit tests green on 8992c0823 (r30). Landed over a further main merge that touched
  protocol.rs and fleet tests: kiln r31 checks main's build and those tests.
- Bug found on the way: record sessions are keyed by member id, the forward names the peer's anchor; the fetch goes to
  the work's creator.
- w-git, w-hg, w-svn told (inbox) to try holder pushes past 320 MiB. A-826 landed in amendments.tsv. Branch and
  worktree deleted.
- Before/after for 320 and 512 MiB: kiln r32, interleaved release A/B 65133486d (before) vs main (after) at
  64/128/256/320/512 MiB x 10 rounds. Then the owner-step slicing with a derived bound (performance, last).

## 12:40 PST: takeover_round's 2 Linux failures are main's (kiln r29, origin/main 3b664fb alone, adacachy)
`slates-bridge-fuse --test takeover_round`: a_restored_mount_tells_its_kernel_the_whole_takeover_set_in_slices_each_inode_once
(takeover_round.rs:259: the zero-budget round's slices [1, 1, ...] where it expects [2, 1, ...]) and
a_kernel_that_cannot_bump_its_epoch_still_gets_every_inode_and_is_counted (:294: 41 slices, expects 40). The same on
family three's branch, so not family three's. GAPS updated on main. Needs an owner; not on my list.
