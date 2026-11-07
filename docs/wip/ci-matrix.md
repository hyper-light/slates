# The CI coverage matrix: which lanes run each test target

Every test target in the workspace (each `lib`, `bin` and `tests/*.rs` target `cargo metadata` lists), and which lane of
`.github/workflows/ci.yml` runs it on which platform. Made 2026-11-06 for VFS plan Step 15 ("CI green on every
platform"), from the tree at `agents/release-wincov` (main a2cf9af7 + this change). The table is a snapshot: it is not yet
checked by a test, so it can drift; the follow-up proposed below is a `cargo xtask` check that regenerates it and fails on
drift (CLAUDE.md §4, doc-truth tests).

## How to read it

- **builds where**: the target file's own top-level `#![cfg(...)]` (a lib or bin has none here; a crate gated inside is
  counted as building).
- **tests (ignored, env-gated)**: the `#[test]` functions in the target's file, how many are `#[ignore]`d, and how many
  skip unless a `SLATES_TEST_*` variable is set (directly or through a helper such as `live_chaos`'s `gated`). A lib's
  unit tests live in its modules, so its count reads 0.
- **lane cells**: `all` — a lane runs every test that is not `#[ignore]`d (with the variables its gated tests need);
  `part` — a name filter or `--exact` runs some; `skips` — the target is built and run, but every test skips for a
  variable no lane sets; `—` — no lane runs it on that platform although it builds there; `n/b` — it does not build there.
- **lanes**: *ubuntu x64* is `gates` (`cargo test --workspace`), `conformance`, `live-guest` and the Linux-only
  nightly lanes; *ubuntu arm64* is `linux-arm64`; *macOS arm64* is `gates` and `conformance` on `macos-latest`; *Windows x64*
  is `windows-lint` (every push) and `windows-nightly`; *Windows arm64* is `windows-arm64`. The lanes marked
  `if: github.event_name != 'pull_request'` (both arm64 lanes, `windows-nightly`, shuttle, TSan, KIND) run on every push
  to main and on the nightly, not on pull requests.
- `#[ignore]`d tests run only where a step names them with `--ignored` (the model and explorer searches in `gates`).

## Totals (319 targets)

| lane | all | part | skips | — (no lane) | n/b |
|---|---|---|---|---|---|
| ubuntu x64 | 299 | 0 | 5 | 0 | 15 |
| ubuntu arm64 | 1 | 4 | 0 | 299 | 15 |
| macOS arm64 | 282 | 0 | 5 | 0 | 32 |
| Windows x64 | 40 | 1 | 0 | 217 | 61 |
| Windows arm64 | 41 | 1 | 0 | 216 | 61 |

With this change both Windows lanes run `crates/server/tests/fleet.rs` (85 passed, 3 ignored on the runners:
diagnostics run 42340483175). `slates-server`'s unit tests run there too, one test at a time, landed with the Windows landing
writer (its failing tests were two landing tests that `landing.rs`'s Windows arm refused, `Unsupported { feature: "landing" }`).
The table below predates that step: its `slates-server` lib row reads `—` on Windows.

## What compiling the whole workspace for Windows found

`cargo xwin clippy --target x86_64-pc-windows-msvc --workspace --all-targets --keep-going` (2026-11-06; no lane runs it:
`windows-lint` lints a crate list, and `slates-cli` without `--all-targets`):

- **Fixed here:** the `slates` binary's unit tests named `rustix::process` (`crates/cli/src/dumps.rs`, 6 errors), and
  `slates-bridge-fuse`'s `abi` test named `rustix::param` (2 errors). Both now build for Windows (each gate says why).
- **Open:** three `slates-cli` benchmark examples do not compile for Windows (`nfs_writeback_ab`, `nfs_large_write`,
  `vfs_tails`: `slates_bridge_nfs`, `rustix::fs`, `rustix::time`). `cargo test -p slates-cli` builds examples, so a Windows
  lane must select `--lib --bins --tests` until they are gated.
- **Open (warnings, an error under `-D warnings`):** `crates/bridge-virtiofs/src/device.rs` (`opcode` unused, `DaxServed`
  never used) and `crates/land/examples/land_bench.rs` (two unused imports), on Windows only.

## The gaps, and the cheapest lane for each

1. **Five CLI suites run nowhere** (every test skips for `SLATES_TEST_CLI`, which only the CLI steps set):
   - `held_rendezvous` (1 test), `merge_kill` (1) and `nfs_held` (3): add them to `gates`' "The slates CLI flow" step,
     which already sets `SLATES_TEST_CLI=1` on ubuntu and macOS. A few daemon starts each.
   - `live_fleet` (11 tests, three daemons each): a step in `gates` for Linux on main and the nightly
     (`if: github.event_name != 'pull_request' && runner.os == 'Linux'`), one test at a time.
   - `live_chaos` (13 flows; it runs many fleets in one process): the nightly only (`github.event_name == 'schedule'`),
     on ubuntu, after raising the descriptor soft limit to the hard one (`ulimit -n "$(ulimit -Hn)"`): a systemd unit's
     1,024 failed it.
2. **ubuntu arm64 runs 5 of 304 targets that build there:** one `cargo test --workspace` step in `linux-arm64`, with the
   `gates` Test step's environment. The runner exists and already builds the workspace for its named tests.
3. **Windows runs 41 (x86_64) and 42 (arm64) of the 258 targets that build there.** The pure crates (`slates-vfs`, `-merge`, `-db`, `-cluster`,
   `-archive`, `-image`, `-transport`, `-wire`, `-machine`, `-land`'s oracles) need nothing Windows lacks: one step in
   `windows-nightly` and `windows-arm64`, `cargo test --workspace --lib --bins --tests --exclude slates-cli -- --test-threads=1`
   (one thread, as every daemon suite on Windows), after one jupiter run of it to find what fails first. Daemon-spawning
   targets already listed in the lanes keep their own steps.
4. **Follow-up:** the table as a `cargo xtask` doc-truth check (it parses `ci.yml` and `cargo metadata` as this snapshot
   did), so a new test target that no lane runs fails CI instead of waiting for an audit.

## The table

| crate | target | builds where | tests (ignored, env-gated) | ubuntu x64 | ubuntu arm64 | macOS arm64 | Windows x64 | Windows arm64 |
|---|---|---|---|---|---|---|---|---|
| slates-anchor | test `anchor` | — | 28 (10, 0) | all | — | all | all | all |
| slates-anchor | test `handles_windows` | windows | 17 (9, 0) | n/b | n/b | n/b | all | all |
| slates-anchor | lib | — | 0 (0, 0) | all | — | all | all | all |
| slates-archive | test `archive` | — | 20 (0, 0) | all | — | all | — | — |
| slates-archive | test `manifest` | — | 20 (0, 0) | all | — | all | — | — |
| slates-archive | test `restore` | — | 13 (0, 0) | all | — | all | — | — |
| slates-archive | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-archive | test `store` | — | 4 (0, 0) | all | — | all | — | — |
| slates-archive | test `transfer` | — | 3 (0, 0) | all | — | all | — | — |
| slates-base | test `host` | — | 13 (0, 0) | all | — | all | all | — |
| slates-base | lib | — | 2 (0, 0) | all | — | all | all | — |
| slates-bridge-core | test `admission` | — | 5 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `appledouble` | — | 4 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `appledouble_view` | — | 7 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `authority` | — | 2 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `barrier` | — | 4 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `base_overlay` | — | 14 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `data_changes` | — | 2 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `dax_backing` | unix | 4 (0, 0) | all | — | all | n/b | n/b |
| slates-bridge-core | test `invalidation` | — | 12 (0, 0) | all | — | all | — | — |
| slates-bridge-core | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `takeover_release` | — | 6 (0, 0) | all | — | all | — | — |
| slates-bridge-core | test `volume_bridge` | — | 37 (0, 0) | all | — | all | — | — |
| slates-bridge-fskit | test `shim` | — | 13 (0, 0) | all | — | all | — | — |
| slates-bridge-fskit | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `abi` | — | 24 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `base_overlay` | unix | 3 (0, 0) | all | — | all | n/b | n/b |
| slates-bridge-fuse | test `codec` | — | 13 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `coherence` | — | 3 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `coherence_mount` | target_os = "linux" | 7 (0, 0) | all | — | n/b | n/b | n/b |
| slates-bridge-fuse | test `creation` | — | 7 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `dispatch` | — | 24 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `handshake` | unix | 6 (1, 0) | all | — | all | n/b | n/b |
| slates-bridge-fuse | test `ipc_mount` | target_os = "linux" | 1 (0, 0) | all | — | n/b | n/b | n/b |
| slates-bridge-fuse | test `kernel_changes` | — | 3 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `mount_owner` | — | 1 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `oci_container` | target_os = "linux" | 1 (0, 0) | all | — | n/b | n/b | n/b |
| slates-bridge-fuse | test `owner_turn` | target_os = "linux" | 1 (0, 0) | all | — | n/b | n/b | n/b |
| slates-bridge-fuse | test `parse_fuzz` | — | 1 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `shards_fs` | — | 2 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `takeover_round` | target_os = "linux" | 2 (0, 0) | all | — | n/b | n/b | n/b |
| slates-bridge-fuse | test `volume_bridge` | — | 10 (0, 0) | all | — | all | — | — |
| slates-bridge-fuse | test `writeback` | — | 6 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `async_loopback` | — | 2 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `attr_floor` | — | 1 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `auth` | — | 4 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `client` | — | 3 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `handle` | — | 8 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `loopback` | — | 1 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `mount` | — | 4 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `multi` | — | 2 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `nfs` | — | 6 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `procedures` | — | 50 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `v4` | — | 38 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `v4_delegation` | — | 16 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `v4_session` | — | 15 (0, 0) | all | — | all | — | — |
| slates-bridge-nfs | test `wire` | — | 16 (0, 0) | all | — | all | — | — |
| slates-bridge-oci | test `runtime` | — | 4 (0, 0) | all | — | all | — | — |
| slates-bridge-oci | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-bridge-oci | test `verify` | — | 11 (0, 0) | all | — | all | — | — |
| slates-bridge-virtiofs | test `admission` | — | 11 (0, 0) | all | — | all | — | — |
| slates-bridge-virtiofs | test `device` | — | 35 (0, 0) | all | — | all | — | — |
| slates-bridge-virtiofs | test `serve` | unix | 12 (0, 0) | all | — | all | n/b | n/b |
| slates-bridge-virtiofs | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-bridge-virtiofs | test `virtqueue` | — | 14 (0, 0) | all | — | all | — | — |
| slates-bridge-winfsp | test `batch_allocations` | — | 1 (0, 0) | all | — | all | — | — |
| slates-bridge-winfsp | lib | — | 6 (0, 0) | all | — | all | all | all |
| slates-cli | test `cli` | unix | 93 (1, 76) | all | part | all | n/b | n/b |
| slates-cli | test `fs_server` | unix | 6 (0, 5) | all | — | all | n/b | n/b |
| slates-cli | test `guest_dax` | target_os = "linux" | 3 (0, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `guest_image` | target_os = "linux" | 7 (0, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `guest_vhost` | target_os = "linux" | 8 (0, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `held_rendezvous` | — | 1 (0, 1) | skips | — | skips | all | all |
| slates-cli | test `live_chaos` | unix | 15 (0, 15) | skips | — | skips | n/b | n/b |
| slates-cli | test `live_fleet` | unix | 18 (0, 18) | skips | — | skips | n/b | n/b |
| slates-cli | test `live_winfsp` | windows | 1 (0, 1) | n/b | n/b | n/b | — | — |
| slates-cli | test `load_fleet` | target_os = "linux" | 1 (0, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `load_scale` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-cli | test `merge_kill` | unix | 1 (0, 1) | skips | — | skips | n/b | n/b |
| slates-cli | test `nfs_held` | unix | 4 (0, 4) | skips | — | skips | n/b | n/b |
| slates-cli | test `nfs_v4_restart` | target_os = "linux" | 1 (0, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `pmem` | target_os = "linux" | 6 (0, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `shards` | windows | 1 (0, 1) | n/b | n/b | n/b | all | all |
| slates-cli | bin `slates` | — | 0 (0, 0) | all | — | all | — | — |
| slates-cli | test `start_judgment` | windows | 2 (0, 2) | n/b | n/b | n/b | all | all |
| slates-cli | test `vm_harness` | target_os = "linux" | 2 (1, 0) | all | — | n/b | n/b | n/b |
| slates-cli | test `winfsp` | windows | 16 (1, 15) | n/b | n/b | n/b | all | all |
| slates-cli | test `winfsp_semantics` | windows | 7 (0, 7) | n/b | n/b | n/b | all | all |
| slates-client | test `acknowledge` | — | 2 (0, 0) | all | — | all | — | — |
| slates-client | test `async_core` | unix | 2 (0, 0) | all | — | all | n/b | n/b |
| slates-client | test `cipher_keys` | — | 1 (0, 0) | all | — | all | — | — |
| slates-client | test `client` | — | 38 (0, 0) | all | part | all | all | all |
| slates-client | test `consumer` | unix | 5 (2, 0) | all | — | all | n/b | n/b |
| slates-client | test `driver` | — | 2 (0, 0) | all | — | all | all | all |
| slates-client | test `image` | — | 3 (0, 0) | all | — | all | — | — |
| slates-client | test `merge_chaos` | — | 1 (0, 0) | all | — | all | — | — |
| slates-client | test `merge_identity` | — | 1 (0, 0) | all | — | all | — | — |
| slates-client | test `merge_post_state` | — | 2 (0, 0) | all | — | all | — | — |
| slates-client | test `netns` | target_os = "linux" | 1 (0, 0) | all | part | n/b | n/b | n/b |
| slates-client | test `presentation` | — | 1 (0, 0) | all | — | all | — | — |
| slates-client | test `reap` | — | 1 (0, 0) | all | — | all | — | — |
| slates-client | test `reap_map` | any(target_os = "linux", target_os = "macos") | 4 (0, 0) | all | all | all | n/b | n/b |
| slates-client | test `shards` | — | 16 (2, 0) | all | — | all | all | all |
| slates-client | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-cluster | test `commit` | — | 10 (0, 0) | all | — | all | — | — |
| slates-cluster | test `config_group_live` | — | 2 (0, 0) | all | — | all | — | — |
| slates-cluster | test `content` | — | 6 (0, 0) | all | — | all | — | — |
| slates-cluster | test `envelope` | — | 5 (0, 0) | all | — | all | — | — |
| slates-cluster | test `explore` | — | 3 (2, 0) | all | — | all | — | — |
| slates-cluster | test `extend` | — | 5 (0, 0) | all | — | all | — | — |
| slates-cluster | test `fast_track` | — | 2 (1, 0) | all | — | all | — | — |
| slates-cluster | test `fleet_live` | — | 2 (0, 0) | all | — | all | — | — |
| slates-cluster | test `ledger_promote` | — | 2 (0, 0) | all | — | all | — | — |
| slates-cluster | test `live` | — | 1 (0, 0) | all | — | all | — | — |
| slates-cluster | test `member_plane` | — | 10 (0, 0) | all | — | all | — | — |
| slates-cluster | test `multilog` | — | 2 (1, 0) | all | — | all | — | — |
| slates-cluster | test `multilog_timed` | — | 2 (1, 0) | all | — | all | — | — |
| slates-cluster | test `pipelining` | — | 3 (2, 0) | all | — | all | — | — |
| slates-cluster | test `prefix_model` | — | 4 (3, 0) | all | — | all | — | — |
| slates-cluster | test `prevote` | — | 4 (0, 0) | all | — | all | — | — |
| slates-cluster | test `priority` | — | 5 (1, 0) | all | — | all | — | — |
| slates-cluster | test `publication_cost` | — | 1 (1, 0) | all | — | all | — | — |
| slates-cluster | test `raft` | — | 8 (0, 0) | all | — | all | — | — |
| slates-cluster | test `raft_live` | — | 1 (0, 0) | all | — | all | — | — |
| slates-cluster | test `root_group_live` | — | 1 (0, 0) | all | — | all | — | — |
| slates-cluster | test `sealed` | — | 6 (0, 0) | all | — | all | — | — |
| slates-cluster | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-cluster | test `slot_model` | — | 6 (3, 0) | all | — | all | — | — |
| slates-cluster | test `wan_election` | — | 4 (1, 0) | all | — | all | — | — |
| slates-conformance | test `matrix` | — | 5 (1, 0) | all | — | all | — | — |
| slates-conformance | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-db | test `alias` | — | 5 (0, 0) | all | — | all | — | — |
| slates-db | test `ledger` | — | 16 (0, 0) | all | — | all | — | — |
| slates-db | test `live` | — | 10 (0, 0) | all | — | all | — | — |
| slates-db | test `mirror` | — | 6 (0, 0) | all | — | all | — | — |
| slates-db | test `model` | — | 15 (0, 0) | all | — | all | — | — |
| slates-db | test `promote_oracle` | — | 2 (0, 0) | all | — | all | — | — |
| slates-db | test `publication` | — | 2 (0, 0) | all | — | all | — | — |
| slates-db | test `reconfig` | — | 7 (0, 0) | all | — | all | — | — |
| slates-db | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-guest-probe | bin `slates-guest-probe` | — | 0 (0, 0) | all | — | all | — | — |
| slates-guest-probe | bin `slates-pod-probe` | — | 0 (0, 0) | all | — | all | — | — |
| slates-guest-probe | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-image | test `e2fsck` | target_os = "linux" | 2 (0, 0) | all | — | n/b | n/b | n/b |
| slates-image | test `hostile` | — | 3 (0, 0) | all | — | all | — | — |
| slates-image | test `layout` | — | 4 (0, 0) | all | — | all | — | — |
| slates-image | test `round_trip` | — | 9 (1, 0) | all | — | all | — | — |
| slates-image | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-image | test `steps` | — | 6 (0, 0) | all | — | all | — | — |
| slates-ipc | test `delivery` | — | 5 (2, 0) | all | — | all | all | all |
| slates-ipc | test `rendezvous` | — | 22 (0, 0) | all | part | all | all | all |
| slates-ipc | test `rings` | — | 13 (0, 0) | all | — | all | all | all |
| slates-ipc | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-ipc | test `status` | — | 3 (0, 0) | all | — | all | — | — |
| slates-ipc | test `stream_exec` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-ipc | test `stream_spawn` | windows | 1 (0, 0) | n/b | n/b | n/b | all | all |
| slates-land | test `durability` | — | 9 (0, 0) | all | — | all | — | — |
| slates-land | test `grant_binding` | — | 4 (0, 0) | all | — | all | — | — |
| slates-land | test `lease` | — | 2 (0, 0) | all | — | all | — | — |
| slates-land | test `oracle` | — | 29 (0, 0) | all | — | all | — | — |
| slates-land | test `os` | unix | 10 (0, 0) | all | — | all | n/b | n/b |
| slates-land | test `os_escape` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-land | test `os_removal` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-land | test `removal` | — | 2 (0, 0) | all | — | all | — | — |
| slates-land | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-land | test `source` | — | 4 (0, 0) | all | — | all | — | — |
| slates-machine | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-mcp | test `mcp` | — | 1 (0, 0) | all | — | all | — | — |
| slates-mcp | lib | — | 4 (0, 0) | all | — | all | — | — |
| slates-mem | test `capacity` | — | 2 (0, 0) | all | — | all | all | all |
| slates-mem | test `dax_layout` | — | 5 (0, 0) | all | — | all | all | all |
| slates-mem | test `dax_object` | unix | 9 (0, 0) | all | — | all | n/b | n/b |
| slates-mem | test `dax_section` | windows | 2 (0, 0) | n/b | n/b | n/b | all | all |
| slates-mem | test `extent_ownership` | — | 2 (0, 0) | all | — | all | all | all |
| slates-mem | test `give_back` | windows | 1 (0, 0) | n/b | n/b | n/b | all | all |
| slates-mem | test `held_section` | windows | 1 (0, 0) | n/b | n/b | n/b | all | all |
| slates-mem | test `lock_on_fault` | — | 3 (0, 0) | all | — | all | all | all |
| slates-mem | test `no_alloc` | — | 2 (0, 0) | all | — | all | all | all |
| slates-mem | test `peer_object` | unix | 10 (0, 0) | all | — | all | n/b | n/b |
| slates-mem | test `peer_section` | windows | 4 (0, 0) | n/b | n/b | n/b | all | all |
| slates-mem | test `race_canary` | — | 1 (2, 0) | all | — | all | all | all |
| slates-mem | test `shared_region` | — | 6 (0, 0) | all | — | all | all | all |
| slates-mem | lib | — | 0 (0, 0) | all | — | all | all | all |
| slates-merge | test `base_for` | — | 2 (0, 0) | all | — | all | — | — |
| slates-merge | test `checkpoint` | — | 3 (0, 0) | all | — | all | — | — |
| slates-merge | test `derive` | — | 9 (0, 0) | all | — | all | — | — |
| slates-merge | test `engine` | — | 46 (0, 0) | all | — | all | — | — |
| slates-merge | test `identity` | — | 3 (0, 0) | all | — | all | — | — |
| slates-merge | test `increment` | — | 42 (0, 0) | all | — | all | — | — |
| slates-merge | test `map` | — | 10 (0, 0) | all | — | all | — | — |
| slates-merge | test `ops_doc` | — | 12 (0, 0) | all | — | all | — | — |
| slates-merge | test `origin` | — | 7 (0, 0) | all | — | all | — | — |
| slates-merge | test `rebase` | — | 2 (0, 0) | all | — | all | — | — |
| slates-merge | test `shuttle_green` | shuttle | 1 (0, 0) | n/b | n/b | n/b | n/b | n/b |
| slates-merge | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-merge | test `special` | — | 14 (0, 0) | all | — | all | — | — |
| slates-merge | test `splice` | — | 6 (0, 0) | all | — | all | — | — |
| slates-merge | test `verdict` | — | 9 (0, 0) | all | — | all | — | — |
| slates-rt | test `admission` | — | 6 (0, 0) | all | — | all | — | — |
| slates-rt | test `burst` | — | 1 (0, 0) | all | — | all | — | — |
| slates-rt | test `cross_shard` | — | 2 (0, 0) | all | — | all | — | — |
| slates-rt | test `differential` | — | 4 (0, 0) | all | — | all | — | — |
| slates-rt | test `foreign_wake` | — | 1 (0, 0) | all | — | all | — | — |
| slates-rt | test `idle_spin` | — | 1 (0, 0) | all | — | all | — | — |
| slates-rt | test `idle_work` | — | 1 (0, 0) | all | — | all | — | — |
| slates-rt | test `lifecycle` | — | 4 (0, 0) | all | — | all | — | — |
| slates-rt | test `memory` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-rt | test `overrun` | — | 3 (0, 0) | all | — | all | all | all |
| slates-rt | test `ownership` | — | 5 (0, 0) | all | — | all | — | — |
| slates-rt | test `pipe` | windows | 6 (0, 0) | n/b | n/b | n/b | all | all |
| slates-rt | test `placement` | — | 3 (0, 0) | all | — | all | — | — |
| slates-rt | test `pollers` | — | 2 (0, 0) | all | — | all | — | — |
| slates-rt | test `readiness` | — | 3 (0, 0) | all | — | all | — | — |
| slates-rt | test `reclaim` | — | 6 (0, 0) | all | — | all | — | — |
| slates-rt | test `registry_contention` | — | 1 (0, 0) | all | — | all | — | — |
| slates-rt | test `sim_memory` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-rt | test `sim_path` | — | 10 (0, 0) | all | — | all | — | — |
| slates-rt | lib | — | 0 (0, 0) | all | — | all | part | part |
| slates-rt | test `tcp` | — | 11 (0, 0) | all | — | all | all | all |
| slates-rt | test `timer_allocations` | — | 2 (0, 0) | all | — | all | — | — |
| slates-rt | test `timers` | — | 6 (0, 0) | all | — | all | — | — |
| slates-rt | test `udp` | — | 15 (0, 0) | all | — | all | all | all |
| slates-rt | test `wake_estimate` | — | 4 (0, 0) | all | — | all | — | — |
| slates-seal | lib | — | 8 (0, 0) | all | — | all | — | — |
| slates-server | test `ack_release` | unix | 6 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `attach_forms` | unix | 9 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `attachment_term` | — | 1 (0, 0) | all | — | all | — | — |
| slates-server | test `checkout` | unix | 3 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `consumers` | — | 1 (0, 0) | all | — | all | — | — |
| slates-server | test `cross_shard_wait` | — | 7 (0, 0) | all | — | all | — | — |
| slates-server | test `daemon` | unix | 25 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `fleet` | — | 96 (5, 0) | all | — | all | all | all |
| slates-server | test `forward_retry` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `fuse_idle` | target_os = "linux" | 1 (0, 0) | all | — | n/b | n/b | n/b |
| slates-server | test `fuse_mount` | target_os = "linux" | 3 (0, 0) | all | — | n/b | n/b | n/b |
| slates-server | test `fuse_told` | target_os = "linux" | 3 (0, 0) | all | — | n/b | n/b | n/b |
| slates-server | test `give_back` | windows | 1 (0, 0) | n/b | n/b | n/b | all | all |
| slates-server | test `green_checkpoint` | unix | 4 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `green_post_state` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `green_retention` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `landing_fairness` | unix | 3 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `lease` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `live_ship` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `liveness` | — | 7 (1, 0) | all | — | all | — | — |
| slates-server | test `merge_load` | — | 5 (0, 0) | all | — | all | — | — |
| slates-server | test `nfs_hostile` | — | 19 (0, 0) | all | — | all | all | all |
| slates-server | test `nfs_mount` | unix | 40 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `nfs_tls` | — | 8 (0, 0) | all | — | all | all | all |
| slates-server | test `nfs_v4_kernel` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `nfs_writeback` | — | 7 (0, 0) | all | — | all | all | all |
| slates-server | test `observe` | — | 8 (0, 0) | all | — | all | all | — |
| slates-server | test `pin_fairness` | unix | 1 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `recovery` | unix | 48 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `seal` | — | 2 (0, 0) | all | — | all | — | — |
| slates-server | test `shards` | unix | 28 (1, 0) | all | — | all | n/b | n/b |
| slates-server | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-server | test `snapshot_landing` | unix | 3 (0, 0) | all | — | all | n/b | n/b |
| slates-server | test `virtiofs` | unix | 30 (1, 0) | all | — | all | n/b | n/b |
| slates-test-seeds | lib | — | 4 (0, 0) | all | — | all | — | — |
| slates-transport | test `admission` | — | 3 (0, 0) | all | — | all | — | — |
| slates-transport | test `amplification` | — | 2 (0, 0) | all | — | all | — | — |
| slates-transport | test `exchanges` | — | 5 (0, 0) | all | — | all | — | — |
| slates-transport | test `handshake_levels` | — | 4 (0, 0) | all | — | all | — | — |
| slates-transport | test `key_update` | — | 1 (0, 0) | all | — | all | — | — |
| slates-transport | test `receive_refusal` | — | 3 (0, 0) | all | — | all | — | — |
| slates-transport | test `session` | — | 17 (0, 0) | all | — | all | — | — |
| slates-transport | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-vfs | test `allocate` | — | 5 (0, 0) | all | — | all | — | — |
| slates-vfs | test `base` | — | 46 (0, 0) | all | — | all | — | — |
| slates-vfs | test `base_identity` | — | 6 (0, 0) | all | — | all | — | — |
| slates-vfs | test `change` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `change_waiters` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `charge` | — | 2 (0, 0) | all | — | all | — | — |
| slates-vfs | test `charge_oracle` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `checkpoint_log` | — | 5 (0, 0) | all | — | all | — | — |
| slates-vfs | test `chunk_ownership` | — | 6 (0, 0) | all | — | all | — | — |
| slates-vfs | test `clones` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `content_in_place` | — | 18 (0, 0) | all | — | all | — | — |
| slates-vfs | test `counter_reserve` | — | 3 (0, 0) | all | — | all | — | — |
| slates-vfs | test `coverage` | — | 5 (0, 0) | all | — | all | — | — |
| slates-vfs | test `delta_cost` | — | 5 (0, 0) | all | — | all | — | — |
| slates-vfs | test `derive` | — | 8 (0, 0) | all | — | all | — | — |
| slates-vfs | test `differential` | unix | 3 (0, 0) | all | — | all | n/b | n/b |
| slates-vfs | test `discard` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `edges` | — | 17 (0, 0) | all | — | all | — | — |
| slates-vfs | test `entitlement` | — | 3 (0, 0) | all | — | all | — | — |
| slates-vfs | test `image` | — | 6 (0, 0) | all | — | all | — | — |
| slates-vfs | test `image_cost` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `journal_cost` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `lifetime` | — | 14 (0, 0) | all | — | all | — | — |
| slates-vfs | test `metadata` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `model` | — | 13 (1, 0) | all | — | all | — | — |
| slates-vfs | test `namespace_refusal` | — | 4 (0, 0) | all | — | all | — | — |
| slates-vfs | test `recall_gate` | — | 3 (0, 0) | all | — | all | — | — |
| slates-vfs | test `recover` | — | 45 (0, 0) | all | — | all | — | — |
| slates-vfs | test `reference_model` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `refused_partway` | — | 4 (0, 0) | all | — | all | — | — |
| slates-vfs | test `rename_escape` | — | 5 (0, 0) | all | — | all | — | — |
| slates-vfs | test `retained_bytes` | — | 9 (0, 0) | all | — | all | — | — |
| slates-vfs | test `retention` | — | 3 (0, 0) | all | — | all | — | — |
| slates-vfs | test `scope` | — | 1 (0, 0) | all | — | all | — | — |
| slates-vfs | test `seek` | — | 3 (0, 0) | all | — | all | — | — |
| slates-vfs | test `shards_backing` | unix | 11 (1, 0) | all | — | all | n/b | n/b |
| slates-vfs | test `shuttle_clones` | shuttle | 1 (0, 0) | n/b | n/b | n/b | n/b | n/b |
| slates-vfs | lib | — | 0 (0, 0) | all | — | all | — | — |
| slates-vfs | test `snapshot_diff` | — | 3 (0, 0) | all | — | all | — | — |
| slates-vfs | test `special` | — | 7 (0, 0) | all | — | all | — | — |
| slates-vfs | test `tenant_isolation` | — | 2 (0, 0) | all | — | all | — | — |
| slates-vfs | test `xattr` | — | 11 (0, 0) | all | — | all | — | — |
| slates-wire | test `compile_fail` | — | 1 (0, 0) | all | — | all | — | all |
| slates-wire | test `golden` | — | 6 (0, 0) | all | — | all | — | all |
| slates-wire | lib | — | 0 (0, 0) | all | — | all | — | all |
| slates-wire | test `toolchain` | — | 1 (0, 0) | all | — | all | — | all |
| slates-wire-derive | lib | — | 0 (0, 0) | all | — | all | — | — |
| xtask | test `helm_chart` | — | 6 (1, 0) | all | — | all | — | — |
| xtask | bin `release` | — | 0 (0, 0) | all | — | all | — | — |
| xtask | bin `xtask` | — | 6 (1, 0) | all | — | all | — | — |
