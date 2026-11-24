# A test's repository creation held its shard through the store's commit (2026-11-24, A-767 item 7, §4.14 `daemon.alive`)

Found by use: w-tsan on adacachy2 (debug build, load 47). One of 100 plain runs of a `vcs_http::daemon_edge` test, and
once a whole-module run, ended in the daemon's watchdog: exit 4, "shard 1 is wedged, unmoved for 1013 ms against its
1000 ms window, in task spawned at daemon_edge.rs:1596, at its poll 1". The exit ends the whole test binary, so every
test running beside it fails with it. It is a test defect. The product's watchdog judged the shard correctly.

## Description

`daemon_edge.rs:1596` is `create_empty_hg`, which made its repository in a synchronous history on the repository's
shard: `observe_at(daemon, REPOSITORY, |state| test_store::create(state, ..))`. On a daemon that keeps a store,
`test_store::create` calls `repository::create_blocking` (`#[cfg(test)]`). That function lands the creation's changeset
through `test_store::land_answered_blocking`, which runs the landing on a thread of its own and `join`s it. The shard's
own thread waits in that `join` until the store's owner commits the changeset.

A commit ends with one `fdatasync` (`slates_store::log::Log::commit_open`). While it runs, the shard makes no step, uses
no CPU and is not runnable: it sleeps in a futex. The watchdog's rule (§4.14, `crates/server/src/watchdog.rs`) calls
such a shard wedged once a whole window passes that way, as it should a host call that never returns. On a box whose
disk is busy with other builds, an `fdatasync` can outlast the 1000 ms window, and the test binary ends.

## Measured

adacachy2, main ddfb469ee, `slates_server` unit-test binary, the test
`a_mercurial_push_past_its_first_admission_waits_for_room_and_lands_whole` (its first step is `create_empty_hg`):
- Plain: passed 3 of 3 (0.53 to 0.65 s).
- Starved by a CPU quota on a user scope (`systemd-run --user --scope -p CPUQuota=10%` and `5%`): passed 6 of 6
  (2.6 to 6.9 s). Under a quota every shard's own waits come back late, and each shard's window rises with the
  scheduler overrun it measures (A-455), so a starved process shows no wedge.
- The store's `fdatasync` alone delayed by 1.5 s (`LD_PRELOAD` of a ten-line shim that sleeps before calling glibc's
  `fdatasync`; built with the system `cc` in a scratch folder outside the tree): exit 4 in 3 of 3 runs, "shard 1 is
  wedged, unmoved for 1011 ms against its 1000 ms window, in task spawned at
  crates/server/src/vcs_http/tests/daemon_edge.rs:1596, at its poll 1", w-tsan's verdict word for word.

## Root cause

A test history that holds a shard and waits for another thread's I/O: `create_blocking`'s `join` on the shard's own
thread. The creation itself does no unsliced work; the shard spent no CPU in the window.

## Fix

`crates/server/src/vcs_http/tests/daemon_edge.rs`: `create`, `create_empty_hg` and the creation in
`create_hg_from_states` make their repositories with `test_store::create_on`, as `create_svn_hooked` already did. The
creation is a client's (`routed_create`, answered by its reply route from a task of the shard), and the test's thread
waits for it to be recorded with short observations. The shard is never held for the store's commit.

- With the fix and the same 1.5 s `fdatasync`: passed 3 of 3 (12.4 to 12.6 s; every landing still waits out the delay).
- The whole module, plain: 53 passed, 0 failed, 1 ignored (24 s).

## Siblings

Every test history that lands on the store from a shard's own thread waits the same way: `test_store::create`,
`land_blocking`, `land_answered_blocking`, `land_segments_blocking` and `hg_import::land_imported`, called inside
`observe_at`, `observe_first` or `audit_on_shard` histories. `create_hg_from_states` still lands its import through
`land_imported` in such a history. About 45 sites in 25 files; their conversion follows this change (main's ruling of
2026-11-24: "Fix all ~45 sibling sites that block a shard, in the same pattern, or list any that can't move with the
reason").
