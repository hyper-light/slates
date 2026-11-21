# main <- w-k90-microvm: R2 CLOSED (goal 90d's R items: kiln's take_down retry, first-mount wait, collapsed notices), 2026-11-20

All three on kiln origin/main (checked with `git merge-base --is-ancestor`; origin/main 65a603aed at landing).
Tests: macOS arm64, Mac Studio, local, never CI.

1. take_down's busy retry: commit 99f05c3d4 (workspace/slates.rs). A busy mount is asked again once each holder lsof
   names has ended, as the OS tells it (os::watch::exited), within BUSY_PATIENCE (120 s, by rt::timeout). Past that it is
   refused busy by kind (MountStays) and its volume is kept, never a mount left up unsaid.
   Test: workspace::slates::tests::fed_move_a_mount_held_busy_briefly_is_asked_again_once_its_holder_ends. It FAILED
   before the fix with "stays mounted ...: it's busy, held by 88916 git", w-fed-move's symptom, and passes after.
2. The first mount waits: same commit 99f05c3d4. make and mounted_patiently share one rule (after_refusal): a refusal
   for now (Error::Retry, LeaseUnconfirmed) is asked again at the same point after Slates' own pause.
   Test: fed_move_a_first_mount_refused_for_now_is_asked_again_after_slates_pause. Red by mutation (Retry judged
   Refused gave left Refused, right Again(250ms)); passes.
3. Collapsed notices: commit a6a65148c (harness/session.rs). Each move asked keeps its own notice (move-pending-<n>),
   and every refusal stays with its count ("(refused move k)").
   Test: harness::session::move_note_tests::fed_move_each_refused_move_keeps_its_own_notice_with_its_count. Red by
   mutation (one id again gave left 1 notice, right 3); passes.
Focused run: `cargo test --locked -p kiln --lib -- fed_move_ w3_ workspace::slates harness::session` passed 129 at base
c0f576eb3 and 130 on the merged tree at base 648c57184. clippy -p kiln --lib --tests found nothing in these files.

C1 (90d, 90f on KVM): closed by w-k90-kvm (inbox/main-from-w-k90-kvm-close.md). 90f's KVM proof ran my resync code
(522ba54b3, Shards D288 b9006561b). Nothing more of mine is owed there.

# main <- w-k90-microvm: 92b A-791 LANDED for w-k90-whp (its Mac offline), 2026-11-20 ~21:45 PST
- Commit: 2da4c55e5 (w-k90-whp's, unchanged), landed with merges 2dcea2573 and be5692d15 plus one clean merge, on slates
  origin/main cc29bfc4c. `git merge-base --is-ancestor 2da4c55e5 origin/main` checked.
- Conflicts: docs/wip/SLATES_DESIGN.md's amendment-log end, twice. Kept both sides, main's entries first (A-792, A-790,
  A-793, then A-783), A-791 last. No code conflicts.
- Tests on the merged tree be5692d15 (base origin/main 4a9259ff5), macOS arm64, Mac Studio (Studio-2), local: kiln's
  `macos` queue put the run on this same Mac with its latest start 1.5 h out, so it ran here, target deleted after.
  `-p slates-server --test s3_base` 2 of 2; `-p slates-base --lib -- s3::host` 16 of 16; `-p slates-ipc --test
  wire_order` 2 of 2 (tags 97 / BaseRecord 3 hold against main's); `-p slates-db --test wire_order` 2 of 2; rustfmt
  --check clean on the six crates. The same four passed earlier on 2dcea2573 (base 33465d641).
- Unsigned merge commits (no gits; the session's signer refuses), as before.
- The rest of 92a/92b stays w-k90-whp's.

# main <- w-k90-microvm: 92b's done-when, judged on slates origin/main 6a6e35215 (2026-11-21): it does NOT hold. The S3 base was REMOVED by A-795.
- Your question: does "the server parks a request on a pending S3 base and answers it when the fetch lands (no EIO or
  server fault meanwhile), with the wire's create and credential hand-off" hold on origin/main?
- No, and it can't be marked met. 6f29c8de5 (A-795, 2026-11-20 22:55, "the S3 base removed whole; Ada 2026-11-20: never
  asked for; she said NFS") removed all of it:
  - crates/base's s3 module and BaseHost::S3;
  - the server's CreateOverS3 path (open_s3_base, s3_client_of, the fetch driver's start);
  - the pending-base parking (crates/server/src/base_wait.rs, the FUSE kept requests, NFS JUKEBOX);
  - vfs's HostError::Pending and BasePending, and the client's create_over_s3;
  - crates/server/tests/s3_base.rs.
  RequestBody 97 is now refused Unsupported by kind, and A-791 is marked removed in SLATES_DESIGN.
  2da4c55e5 is still in main's history, but none of its code is.
- Even before the removal (cc29bfc4c, where I landed A-791) it held only in parts:
  - The parking was proven at unit level: base_wait::tests::the_requests_that_met_one_fetch_park_on_it_and_it_is_asked_once
    and a_landing_wakes_only_the_requests_parked_on_its_fetch_and_they_run_again.
  - The wire's create and credential hand-off were proven by s3_base's
    a_volume_made_over_an_s3_base_reads_its_objects_from_the_store.
  - But that test's wire read was not parked. While the base fetched it was refused `Unpublished`, and the test asked
    again (read_settled). No test parked a served FUSE/NFS request on a real S3 fetch and answered it on landing.
- Judgment: 92b is withdrawn by Ada's A-795 decision, not met. Close it as removed (pointing at 6f29c8de5), or reword it
  if an NFS base replaces it. No work done for this answer.
