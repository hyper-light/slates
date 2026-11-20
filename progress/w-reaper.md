# w-reaper progress (event-driven reaper goal). Supervisor "slates-7a".

## RESUME HERE (checkpointed 2026-11-19 on main's order to move into kiln)
- Branch agents/w-reaper-hints, pushed: 971a41303 (one WIP commit over origin/main 3bcaccf91). Worktree on this Mac:
  ~/slates-work/w-reaper-hints (clean after the commit). Nothing in flight; no kiln run started.
- Amendment A-709 reserved in amendments.tsv (row "in progress"; next free A-710).
- Piece A (base hints by readiness + delegation lapse at its deadline) is in progress. DONE in 971a41303:
  - crates/base/src/hint_set.rs: HintSet (Linux epoll EPOLLIN|ONESHOT, add-or-modify re-arm; macOS kqueue EVFILT_READ
    EV_ADD|EV_ONESHOT, key in udata); kevent_now(queue, changes, out: impl Buffer) is the one kevent unsafe block, the
    macOS watcher's register/drain now use it (unsafe-budget slates-base 11 -> 10, comment updated). Tests
    hint_set:: 2/2 GREEN (macOS arm64, this Mac, local, CARGO_TARGET_DIR=~/.cache/w-reaper-local-target).
  - OsHost::hint_source / BaseHost::hint_source (cfg unix); Watcher::source on both platforms.
  - crates/server/src/base_hints.rs: BaseHints {set, task, owed, asked} on ShardState.base_hints; watcher() task
    (opens the set, enrols all volumes, races readable(set fd) vs `asked`, drain_reported -> drain_volume per key
    (stale handle skipped, host lent out -> owed), re-arm via enrol, counters base.hints_drained / hints_refused /
    nfs4.recall.outside_asked / hint_set_refused / hint_arm_refused / hint_wait_refused, then delegation::drain);
    after_borrow wakes it when an owed volume's host is back (called in state::try_with_state after deadlines').
  - daemon.rs: watcher spawned beside the keeper; process_base_hints + its consts deleted; reap_loop now only
    seal_idle_content; its doc says A-709. config.rs LOOP_TASKS_PER_SHARD 8 -> 9.
  - bridge-nfs: Delegations/FileState::earliest_recall_ns + test
    the_earliest_unreturned_recall_names_when_its_revocation_falls_due (tests/v4_delegation.rs; NOT RUN yet).
  - delegation.rs: arm_lapse at drain's end sets state.delegation_lapse_at (span from drain's now, on state.clock) and
    notes the keeper; deadlines.rs release_due drains at it, and it is in the earliest list.
- NEXT STEPS for piece A:
  1. verbs.rs: call `crate::base_hints::enrol(state, h)` after `state.by_id.insert(id, h)` at the three VolumeSlot
     insertion sites (create ~6697, clone ~10603, recovery ~18575).
  2. Compile: `CARGO_TARGET_DIR=~/.cache/w-reaper-local-target CARGO_BUILD_JOBS=6 cargo test --locked -p slates-server
     --lib base_hints` (fix compile errors; Windows arms of base_hints are no-ops, file for main).
  3. Server test in base_hints.rs: audit_daemon_of(1); create an overlay with `base: Some(<pid-named scratch dir>)`
     (see git_hooks/tests.rs ScratchDir), list its root so the watcher watches it, write a file into the dir from the
     test, look_until (deadlines.rs pattern) the counter base.hints_drained > 0 within LIVENESS_BUDGET_NS/2; red =
     the old tick (or the watcher's spawn removed).
  4. Run bridge-nfs: `cargo test --locked -p slates-bridge-nfs --test v4_delegation earliest_unreturned`.
  5. fmt for slates-base, slates-server, slates-bridge-nfs; docs: §4.7 status line, GAPS reaper-series line (hints done,
     idle seal left), amendment A-709 in the design's amendment log + amendments.tsv "landed"; squash into one
     area-prefixed commit on a fresh branch from origin/main (cherry-pick), merge origin/main, push HEAD:main.
- Then piece B: idle seal + purge by aging deadline on the keeper (seal_idle_content/purge_if_idle in daemon.rs).
- Then piece C: delete reap_loop + pace + its tick, test that no reap waits on it, §4.7 says so, LOOP_TASKS -1.
- Finding (read, not chased): delegation::drain also revoked lapsed delegations on the tick; handled in piece A by
  the keeper deadline above.

## State found at start
- On main already (g255): A-475 (dead clients by hang-up), A-522, A-536, A-554, A-570. Queue item 1 DONE on main.
- reap_loop on main ran process_base_hints + delegation::drain and seal_idle_content at LIVENESS_BUDGET_NS.
