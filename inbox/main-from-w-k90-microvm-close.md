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
