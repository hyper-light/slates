# Every function on every platform

> **Rule (Ada, 2026-10-26):** every slates function works on macOS arm64, Linux x86_64 and arm64, and Windows
> x86_64 and arm64. macOS x86_64 is the one exception. A piece that exists on some platforms only is a gap to build
> on the others. It is never a reason to close a goal on the platforms that have it. A cell may say "impossible" only
> with the platform's own primary source or a measured refusal, as the DAX matrix requires (`virtiofs.md` §8).

This is the inventory of where each function stands, made 2026-10-26 from four sources:
- the daemon's own transport report, a pure table over the platform (`crates/server/src/transports.rs`);
- every `cfg` gate and platform refusal in production code (`grep` of `crates/*/src`);
- what CI's lanes build and run (`.github/workflows/ci.yml`);
- the DAX matrix (`virtiofs.md` §8, A-164), which this does not repeat.

Reconciled 2026-10-31 (plan step 1) with main's CI run 41249775368 (slates 9cf0cc31, every job green), the test lists
of both arm64 lanes in that run, the conformance records (`conformance.md`) and the DAX matrix's recorded runs.

Each cell says one of the following, and a cell may judge its parts separately:
- **built**: the code exists, and a CI lane, a test or a recorded run proves it on that platform, named in the cell;
- **built, unrun**: the code exists, but nothing has run it on that platform;
- **missing**: no code for that platform;
- **impossible**: with the platform's own source;
- **declined**: Ada decided it is not to be done, with the date.

Platforms: mac = macOS arm64; lx = Linux x86_64; la = Linux arm64; wx = Windows x86_64; wa = Windows arm64.

A lane is a job of `.github/workflows/ci.yml`, named by its key:
- `gates`: macOS and Linux x86_64; `cargo test --workspace`, then the CLI flow (`crates/cli/tests/cli.rs` with
  `SLATES_TEST_CLI=1`);
- `windows-lint` and `windows-nightly`: Windows x86_64;
- `windows-arm64` and `linux-arm64`: each runs a named list of suites;
- `live-guest` (QEMU) and `kind`: Linux x86_64;
- `conformance` and `sdk`: macOS and Linux x86_64.

A recorded run is a CI diagnostics run, cited by its id, or a run on a named machine, cited by its date.

## Foundational gaps

Each of these blocks several functions below, so it comes first.

1. **TCP in the runtime on Windows: closed 2026-10-26 (A-182).** The Windows runtime had UDP (AFD) and named pipes
   (IOCP, A-179) but no TCP (`#[cfg(not(windows))] pub mod tcp`), which took the MCP HTTP edge, the NFS network export
   and every TCP service off Windows. `tcp` now rides the socket seam on every platform. `crates/rt/tests/tcp.rs`
   passes 10 of 10 on jupiter (Windows x86_64), 9 of 9 on Linux x86_64 and macOS, and runs in the three Windows CI
   lanes. The services on it are the next rows to build.
2. **Handing sockets and handles to a restarted daemon on Windows.** The anchor holds the fleet's serve sockets, the
   NFS listener and the FUSE devices across a daemon restart by descriptor inheritance (`crates/cli/src/anchor.rs`,
   all `#[cfg(unix)]`). Windows fleet ports and the NFS listener are still unheld. The WinFsp mount's control,
   temporary drive-link and unnamed open-ledger handles now pass through the anchor (A-198, Step 9.3; since A-206 a mount's
   file system is deleted only after the anchor releases them, 0 of 20 left held per lane, run 41978665514): the successor
   serves that same kernel volume and resolves old contexts by stable file identity. Diagnostics [41422186336](https://github.com/hyper-light/slates/actions/runs/41422186336) lost zero drives of twenty on both architectures; every iteration required a restored-context read, and the last daemon counted four. All five live WinFsp tests, 36 bridge tests, 32 active anchor tests, native clippy and Windows docs passed on each runner. With only the control-handle hold omitted, the unchanged oracle failed its first post-kill read with Windows error 995 on both runners ([41422382101](https://github.com/hyper-light/slates/actions/runs/41422382101)); the still-held link alone left a dangling drive letter.
   Step 9.4 now refuses new unprivileged process opens for duplication and ACL changes at both holders: all six
   requests denied on each architecture, with the twenty-kill oracle retained ([41432451084](https://github.com/hyper-light/slates/actions/runs/41432451084));
   the same normalized probe fails without protection ([41432477849](https://github.com/hyper-light/slates/actions/runs/41432477849)).
   The single owned receiver now settles a retained request before any fresh receive, with one counted settlement,
   original and fresh bytes returned in 0.15 s on x86_64 and 0.16 s on arm64
   ([41457226598](https://github.com/hyper-light/slates/actions/runs/41457226598)); the same tree retains zero lost
   drives of twenty per architecture. An independent second daemon is refused with Windows error 183. Final untraced diagnostics
   [41484542209](https://github.com/hyper-light/slates/actions/runs/41484542209) pass nine live CLI tests per
   architecture, zero drive losses of twenty, both renamed-view cases, six foreign-rights denials, one counted captured
   read, all 54 bridge tests, 32 active anchor tests, native clippy and private Windows docs. Original kernel names are
   held independently of namespace homes; private purges address their files by identity, and every completed round
   rearms its volume subscription before parking. The WinFsp portion is closed; the three Windows CI lanes retain
   these request-recovery and live-mount histories. The
   source audit found that processing requests do not time out; A-198 rejects the guard-only receive journal. Windows socket inheritance remains separate work through
   `WSADuplicateSocketW` or an inherited handle list.

## Functions

| function | mac | lx | la | wx | wa |
|---|---|---|---|---|---|
| Daemon, client and the verbs over the ring (volumes, attach, read, write, versions, work and merge, status) | built: `gates` (`crates/client/tests/client.rs`, the server's suites) | built: `gates` | built for what the maps need: `linux-arm64` runs the client suite's 20 map tests, `reap_map`'s 4, the namespace tests and 7 CLI tests, which create, attach, write, map and destroy; the hermeticity record on Docker Desktop's arm64 Linux kernel also took a snapshot (`conformance.md`, 2026-09-20). Unrun there: the lifecycle of the typed verbs (`the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed`), work and merge | built: `windows-lint` and `windows-nightly` run the client suite whole, one test at a time (`crates/client/tests/client.rs`, `crates/client/tests/driver.rs`) | built: `windows-arm64` runs the same suite, 31 tests and the driver's 2 (run 41249775368) |
| Landing and grants (`slates land`, `grant`, `audit`) | built: `gates` (`crates/land/tests/oracle.rs`, `crates/land/tests/os.rs`) | built: `gates`, and `os.rs` again in release with `kill -9` | built: a recorded run, the hermeticity record on Docker Desktop's arm64 Linux kernel (`conformance.md`, 2026-09-20): `slates land`, `grant` and `land --grant`, 6 of 6 entries landed and verified on disk. No lane runs it there | missing: `crates/land/src/os.rs` is Unix-only. Being built: `os_windows.rs`, G-shards-windows step 5 (slates-36) | missing (the same) |
| Host mount | built (NFS loopback): `gates`' CLI flow and `conformance` | built (FUSE): `gates`' CLI flow and `conformance` | built (FUSE): `linux-arm64` (`a_read_only_map_through_a_mount_follows_another_paths_change`, `a_shared_map_through_a_mount_writes_back_and_follows_another_paths_change`) | built (WinFsp): `windows-lint` and `windows-nightly` (`crates/cli/tests/winfsp.rs`, `a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs`); jupiter has no WinFsp | built (WinFsp's arm64 driver): `windows-arm64` runs `crates/cli/tests/winfsp.rs`, 4 of 4 (run 41249775368) |
| A mount that outlives a daemon restart | built: the anchor holds the NFS listener and every accepted connection (A-113, 2026-10-06); `crates/cli/tests/nfs_held.rs` (`an_nfs_connection_outlives_its_daemons_kill_and_every_request_is_answered`) kills daemons under a userspace NFS client, since a kernel mount meeting a dead daemon has panicked this Mac. It is gated on `SLATES_TEST_CLI=1` and no lane runs it | built: the anchor holds the FUSE device (A-61); `gates`' CLI flow (`a_replacement_the_dead_daemon_never_delivered_does_not_outlive_the_takeover`, `a_fuse_mount_whose_daemon_was_killed_is_ended_by_the_restarted_daemon`) | built: a recorded run on the nested arm64 KVM host (G-arm64-guest, 2026-10-27), `a_daemon_killed_under_a_pmem_guests_live_map_is_taken_over_and_the_guest_goes_on`: the daemon killed under a guest's live map of an image file in a FUSE mount and taken over, the host's next write in the guest's map 133 µs later. No lane runs it there | missing: foundational gap 2 | missing (the same) |
| A daemon restarted from the anchor's own image after an in-place upgrade (A-145) | missing: the anchor refuses the restart (`ExecutableReplaced`), as macOS gives a process no link to its running image; `gates`' CLI flow asserts the refusal (`a_daemon_killed_after_its_anchors_executable_is_replaced_restarts_from_the_anchors_own_image`) | built: `gates`' CLI flow (the same test) | built, unrun | missing: the anchor restarts from the path and checks nothing, since a running executable can be renamed aside on Windows and std has no stable file identity to check it against (`crates/anchor/src/supervise.rs`, `own_image`; owed, GAPS) | missing (the same) |
| The network export (`slates export`, a fleet node's NFS export, Kubernetes PersistentVolumes) | built: `gates` (`crates/server/tests/nfs_hostile.rs`, `crates/server/tests/nfs_tls.rs`); the loopback mount is the same server | built: `gates`, and `kind` (kubelet mounts it over RPC-with-TLS) | built: a recorded run, the KIND export leg on GitHub's `ubuntu-24.04-arm` (diagnostics run 40580252064, 2026-10-27; the DAX matrix's Kubernetes arm64 row). No lane on main runs it there | built (A-184): `windows-lint` and `windows-nightly` run the hostile-client, RPC-with-TLS and write-back suites (`nfs_hostile`, `nfs_tls`, `nfs_writeback`); a connection frames requests in its own inbox. Owed: a real Linux kernel client mounting it | built: `windows-arm64` runs the same three suites, 17, 8 and 7 tests (run 41249775368) |
| MCP server | built (stdio, loopback HTTP): `gates` (`crates/mcp/tests/mcp.rs`) | built: `gates` | built, unrun | built (A-183): stdio and the loopback HTTP edge, one code path. A recorded run on jupiter, 2026-10-26: `crates/mcp/tests/mcp.rs` passes through the HTTP edge and stops at landing's `Unsupported`. It joins the Windows lanes when landing runs there | built, unrun: `windows-arm64` lints `slates-mcp`, and no Windows lane runs its test (wx's reason) |
| Fleet node (consensus, transport over UDP, takeover) | built: `gates` (`crates/server/tests/fleet.rs`) | built: `gates` and `kind` | built, unrun | built in part: the daemon's transport runs over AFD UDP, on a socket no child inherits and no other socket can bind on any address (A-183; `crates/rt/tests/udp.rs` in `windows-lint` and `windows-nightly`), and serves its export (A-184). Unrun as a node: no fleet test runs on Windows. Missing: the anchor holds no serve sockets (gap 2) | the same as wx; `crates/rt/tests/udp.rs` runs in `windows-arm64` |
| `slates exec` (a program sees a volume at a path) | missing: "needs Linux user and mount namespaces" (`crates/cli/src/main.rs`) | built: `gates`' CLI flow (`the_launcher_refuses_a_path_it_would_have_to_create_and_leaves_the_host_untouched`) | built, unrun | missing | missing |
| Container bind (the OCI attachment) | built (the NFS mount bound in): recorded runs through Docker Desktop on this Mac (`conformance.md`'s OCI container row, 2026-10-01: pjdfstest, fsx, fsstress and the workloads through the bind). CI's macOS runner has no Docker, so the CLI flow's container leg skips loudly there (`an_oci_container_consumes_the_host_attachment_through_the_runtime_bind`) | built: `gates`' CLI flow (`a_container_maps_a_file_through_the_bound_mount_and_follows_another_paths_change`, `a_rootless_container_maps_a_file_through_the_mount_and_follows_another_paths_change`) | built: `linux-arm64` (the same two tests) | built, unrun for Docker Desktop's Linux containers: the export (A-184) is the way in, through `host.docker.internal`, and no Docker Desktop on Windows has mounted it yet. Impossible for process-isolated containers, measured: Docker's engine cannot see a WinFsp drive (G-mount step 4); `a_winfsp_folder_bound_into_a_process_isolated_container` asserts the refusal in `windows-lint` and `windows-nightly` | the same as wx, by the same sources; the refusal's test skips loudly in `windows-arm64`, whose image has no Docker engine (run 41249775368) |
| `slates oci-runtime` and `oci-check` (judging the engine and the bind source) | built: `gates`' CLI flow (`a_verified_container_source_is_checked_again_before_it_is_bound`) | built: `gates`' CLI flow (`a_linux_container_reaches_the_shared_mount_as_its_own_ids`) | built, unrun | missing: "facilities this platform does not have" (`crates/cli/src/main.rs`) | missing |
| `slates vhost-user` (a VMM attaches slates' own virtio-fs device) | declined (Ada, 2026-10-30): no vhost-user QEMU on this Mac. QEMU builds vhost-user on macOS only when asked (`meson.build`: off Linux it is off unless `--enable-vhost-user`), Homebrew's 11.1.3 does not ask, and the back end is Linux's (memfds, eventfds) | built: `live-guest` (`crates/cli/tests/guest_vhost.rs` in QEMU 11.1 with KVM), and crosvm on the box (gated) | built: a recorded run, `crates/cli/tests/guest_vhost.rs` 5 of 5 in QEMU 11.1.2 under its own emulation on GitHub's `ubuntu-24.04-arm` (A-189; diagnostics run 40883925064, job 123456468166). No lane on main runs it there | impossible: QEMU builds no vhost-user on Windows (`meson.build` at v11.1.0, lines 227–230: "vhost-user is not available on Windows"); crosvm speaks vhost-user there only over its own pipe transport, for block, gpu, sound and net devices, and has no fs device (its `src/sys/windows.rs` at 678337f67d, read 2026-10-31: "fs control tube not supported on Windows"); Hyper-V offers no vhost-user device (the DAX matrix's Hyper-V row) | impossible (the same) |
| Shards route (`slates shards`, the daemon's stream, DAX) | built: `gates` (`crates/client/tests/shards.rs`), and a guest on this Mac (Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes`, the DAX matrix's Shards row) | built: `gates`, and a guest on adacachy (G-shards-linux) | built: a recorded run, Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes` over the daemon's stream on the nested arm64 KVM host (G-arm64-guest, 2026-10-27). No lane runs the Shards suites on arm64 Linux | built: `windows-lint` and `windows-nightly` run `crates/client/tests/shards.rs` and `crates/cli/tests/shards.rs`. Proven with a guest (G-shards-windows, 2026-10-27): the daemon's pipe (A-179), `slates shards` handing it as `{handle}` (A-181), the DAX answer (A-180), and Shards' device and WHP window (its `gpu` 3b1e52a4, bc032060). On GitHub's windows-2025 a guest mounts, reads and writes a volume (3 of 3, run 40455011746) and maps a file with DAX: it stores and syncs, sees another attachment's change and a cut's zeros (3 of 3 in each of runs 40503725694, 40505101140 and 40521211082), red-checked with the window off (run 40521211082) | built: `windows-arm64` runs `crates/client/tests/shards.rs` (11 tests) and the CLI's hand-off (`slates_shards_hands_a_program_a_working_handle_and_the_attachment_ends_with_it`; run 41249775368). No guest: Shards has no hypervisor backend on Windows arm64 (the DAX matrix's Shards row: none) |
| Direct map (A-135: a same-kernel process maps a file with no copy) | built (A-173): `gates` (`crates/client/tests/client.rs`) | built (A-171): `gates` | built: `linux-arm64` (the client suite's 20 map tests and `reap_map`) | built (A-175): `windows-lint` and `windows-nightly` | built: `windows-arm64` (the client suite whole; its map tests first passed 20 of 20 in run 40525669353) |
| Base overlay (a volume over a host directory, read only) | built: `gates` (`crates/base/tests/host.rs`) | built: `gates`, and `host.rs` again over the disk | built, unrun | built: `windows-lint` runs `cargo test -p slates-base` (`crates/base/src/windows.rs`) | built, unrun: `windows-arm64` lints `slates-base` and runs no test of it |
| Disk images (`slates image build`, `merge`) and Firecracker | image verbs: built, unrun (`crates/cli/tests/guest_image.rs` is Linux's); Firecracker: none (Linux KVM only) | built (A-160, A-165): `live-guest` (`crates/cli/tests/guest_image.rs` in QEMU 11.1), and Firecracker and Cloud Hypervisor on the box (gated) | built: recorded runs, `crates/cli/tests/guest_image.rs` 4 of 4 with Firecracker v1.17.0 on the nested arm64 KVM host (G-arm64-guest, 2026-10-27), and its drive form in QEMU 11.1.2 on `ubuntu-24.04-arm` (diagnostics run 40883925064, job 123482450147). No lane runs it there | image verbs: built, unrun; Firecracker: none | image verbs: built, unrun; Firecracker: none |
| Python and Node SDKs | built: `sdk` (the wheel and the addon, each by use over a live daemon) | built: `sdk` | built, unrun | built, unrun: `windows-lint` lints them; no by-use test | built, unrun |
| Crash dumps keep no secret | built, unrun: `RLIMIT_CORE` 0 (`crates/cli/src/dumps.rs`); its test is Linux's | built: `gates`' CLI flow (`the_anchor_and_its_daemon_exclude_themselves_from_core_dumps`) | built, unrun | missing, and Ada's call (GAPS `8a-win-dumps`, OPEN): no unprivileged `RLIMIT_CORE`, so `exclude_from_dumps` is `Ok(())` (`crates/cli/src/dumps.rs`). Unprivileged steps exist for the decision: `WerSetFlags(WER_FAULT_REPORTING_FLAG_NOHEAP)`, and `WerRegisterExcludedMemoryBlock` for each region as it is made | missing (the same) |
| Hermeticity tracer (no write outside a granted target) | built: `conformance`'s macOS leg (`sudo eslogger`, `xtask/src/conformance/hermeticity.rs`; CI's first record 2026-10-24) | built: `conformance` | built: recorded runs over FUSE and NFSv4.2 on Docker Desktop's arm64 Linux kernel (`conformance.md`, 2026-09-20 and 2026-09-27). No lane runs it there | missing (a branch exists: `windows-hermeticity-tracer`) | missing |
| Conformance suites | built: `conformance` (pjdfstest over NFS, its non-root cases; fsx, fsstress, the workloads) | built: `conformance` (pjdfstest, fsx, fsstress, the workloads) | built: `linux-arm64` runs fsx, fsstress and pjdfstest over each Linux transport (the NFSv3 adapter, the NFSv4.2 server, the container bind of the FUSE mount), each judged against the x86_64 lane's reviewed lists (A-214). First run on GitHub's `ubuntu-24.04-arm` (diagnostics run 41975493604, 2026-11-05): pjdfstest 8,798 cases a transport, 0 unexpected failures and 0 listed cases passing on each (1,800, 1,799 and 1,798 expected); fsx 10,000 operations and fsstress 4 × 500 passed on each. The workloads and the tracer there are recorded runs only (`conformance.md`, 2026-09-20) | built (A-214): `windows-lint` runs WinFsp's winfstest over the drive, 628 cases judged against the reviewed list (374 listed, each with its reason; the same verdicts on both architectures, run 42046112993), the WinFsp fsx port (its open wedge at operation 164 listed), and `crates/cli/tests/winfsp_semantics.rs` (7 tests). pjdfstest and fsstress do not apply | built: `windows-arm64` runs the same three |

## Order of work

1. **TCP in the Windows runtime** (gap 1): done, A-182. The MCP HTTP edge on Windows and the Windows datagram socket
   made non-inheritable and exclusive: done, A-183. The network export on Windows: done, A-184; owed there, a real
   Linux kernel client mounting it (Docker Desktop's machine or WSL2).
2. **Holding a restarted daemon's sockets and mounts on Windows** (gap 2): the fleet's serve sockets, then a WinFsp
   mount that outlives its daemon.
3. **The anchor's restart from its own image** (A-145) on macOS, and its check on Windows; crash dumps on Windows
   once Ada decides `8a-win-dumps`.
4. **`slates exec` on macOS and Windows**, and `oci-runtime`/`oci-check` on Windows.
5. **Every built-but-unrun cell gets its run**: on Linux arm64 the lifecycle test, work and merge, A-145's restart,
   `slates exec`, `oci-runtime`, MCP, the fleet node, the base overlay, the SDKs and the crash-dump test, and a lane
   for what only recorded runs prove there (landing, the mount across a restart, the export, `vhost-user`, the Shards
   route, the image form, the conformance suites and the tracer); on Windows arm64 the base overlay and MCP; the image
   verbs on macOS and Windows; the SDKs' by-use tests on Windows; `nfs_held.rs` in macOS's CLI flow; and the
   hermeticity tracer and a conformance suite on Windows.
6. **`slates vhost-user` off Linux: nothing to build.** Declined on macOS (Ada, 2026-10-30) and impossible on Windows
   (the row's sources).

Owners: G-shards-windows steps 2 and 5 are slates-00's and slates-36's (Ada, 2026-10-26); the rest of this list is
slates-d7's unless Ada assigns it otherwise. Each item, as it lands, updates its row here, the gap ledger and its
design section in the same change.

## Tests the tables name (generated)

Every test a cell above names, and where the tree defines it; another project's tests and paths are listed and not
looked for. `cargo xtask structural` and `cargo test -p xtask` refuse a cell that names a test or a path the tree
lacks, giving its line (`xtask/src/named_tests.rs`). After a named test moves,
`cargo test -p xtask -- --ignored regenerate_the_named_tests` rewrites this list.

<!-- named-tests:begin -->
- `a_container_maps_a_file_through_the_bound_mount_and_follows_another_paths_change`: `crates/cli/tests/cli.rs`
- `a_daemon_killed_after_its_anchors_executable_is_replaced_restarts_from_the_anchors_own_image`: `crates/cli/tests/cli.rs`
- `a_daemon_killed_under_a_pmem_guests_live_map_is_taken_over_and_the_guest_goes_on`: `crates/cli/tests/guest_image.rs`
- `a_fuse_mount_whose_daemon_was_killed_is_ended_by_the_restarted_daemon`: `crates/cli/tests/cli.rs`
- `a_linux_container_reaches_the_shared_mount_as_its_own_ids`: `crates/cli/tests/cli.rs`
- `a_read_only_map_through_a_mount_follows_another_paths_change`: `crates/cli/tests/cli.rs`
- `a_replacement_the_dead_daemon_never_delivered_does_not_outlive_the_takeover`: `crates/cli/tests/cli.rs`
- `a_rootless_container_maps_a_file_through_the_mount_and_follows_another_paths_change`: `crates/cli/tests/cli.rs`
- `a_shared_map_through_a_mount_writes_back_and_follows_another_paths_change`: `crates/cli/tests/cli.rs`
- `a_verified_container_source_is_checked_again_before_it_is_bound`: `crates/cli/tests/cli.rs`
- `a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs`: `crates/cli/tests/winfsp.rs`
- `a_winfsp_folder_bound_into_a_process_isolated_container`: `crates/cli/tests/winfsp.rs`
- `an_nfs_connection_outlives_its_daemons_kill_and_every_request_is_answered`: `crates/cli/tests/nfs_held.rs`
- `an_oci_container_consumes_the_host_attachment_through_the_runtime_bind`: `crates/cli/tests/cli.rs`
- `slates_shards_hands_a_program_a_working_handle_and_the_attachment_ends_with_it`: `crates/cli/tests/shards.rs`
- `the_anchor_and_its_daemon_exclude_themselves_from_core_dumps`: `crates/cli/tests/cli.rs`
- `the_launcher_refuses_a_path_it_would_have_to_create_and_leaves_the_host_untouched`: `crates/cli/tests/cli.rs`
- `the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed`: `crates/client/tests/client.rs`
- Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes`: another project's test, not looked for
<!-- named-tests:end -->
