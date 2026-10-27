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

Each cell says one of:
- **built**: the code exists and a test or CI lane runs it;
- **built, unrun**: the code exists, but no test runs it on that platform;
- **missing**: no code for that platform;
- **impossible**: with its source.

Platforms: mac = macOS arm64; lx = Linux x86_64; la = Linux arm64; wx = Windows x86_64; wa = Windows arm64.

## Foundational gaps

Each of these blocks several functions below, so it comes first.

1. **TCP in the runtime on Windows: closed 2026-10-26 (A-182).** The Windows runtime had UDP (AFD) and named pipes
   (IOCP, A-179) but no TCP (`#[cfg(not(windows))] pub mod tcp`), which took the MCP HTTP edge, the NFS network export
   and every TCP service off Windows. `tcp` now rides the socket seam on every platform. `crates/rt/tests/tcp.rs`
   passes 10 of 10 on jupiter (Windows x86_64), 9 of 9 on Linux x86_64 and macOS, and runs in the three Windows CI
   lanes. The services on it are the next rows to build.
2. **Handing sockets and handles to a restarted daemon on Windows.** The anchor holds the fleet's serve sockets, the
   NFS listener and the FUSE devices across a daemon restart by descriptor inheritance (`crates/cli/src/anchor.rs`,
   all `#[cfg(unix)]`). On Windows none is held: a fleet node's ports are free between a stop and a restart, and a
   WinFsp mount lives in the daemon's process and dies with it. Windows hands sockets on with `WSADuplicateSocketW`
   (or an inherited handle list) and handles with `DuplicateHandle`.

## Functions

| function | mac | lx | la | wx | wa |
|---|---|---|---|---|---|
| Daemon, client and the verbs over the ring (volumes, attach, read, write, versions, work and merge, status) | built | built | built, unrun except the direct-map and FUSE tests | built (the native Windows lanes drive the whole verb set) | built, unrun: the arm64 lane runs `slates-mem`, the rings and the rendezvous, no daemon |
| Landing and grants (`slates land`, `grant`, `audit`) | built | built | built, unrun | missing: `crates/land/src/os.rs` is Unix-only. Being built: `os_windows.rs`, G-shards-windows step 5 (slates-36) | missing (same) |
| Host mount | built (NFS loopback) | built (FUSE) | built (FUSE, CI's arm64 lane) | built (WinFsp, CI only: jupiter has no WinFsp) | built, unrun (G-mount) |
| A mount that outlives a daemon restart | built (the anchor holds the NFS listener; A-113) | built (the anchor holds the FUSE device; A-61) | built, unrun | missing: foundational gap 2 | missing |
| A daemon restarted from the anchor's own image after an in-place upgrade (A-145) | missing: the anchor refuses the restart, as macOS gives a process no link to its running image (`crates/cli/tests/cli.rs`, `a_daemon_killed_after_its_anchors_executable_is_replaced_restarts_from_the_anchors_own_image`) | built | built, unrun | unjudged: Windows locks a running image against replacement but lets it be renamed | unjudged |
| The network export (`slates export`, a fleet node's NFS export, Kubernetes PersistentVolumes) | built | built | built (KIND lane is x86_64; arm64 owed) | built (A-184): the NFS server's hostile-client and RPC-with-TLS suites on jupiter and in both x86_64 Windows lanes; a connection frames requests in its own inbox. Owed: a real Linux kernel client mounting it | built, unrun: the arm64 lane builds no server |
| MCP server | built (stdio, loopback HTTP) | built | built, unrun | built (A-183): stdio and the loopback HTTP edge, one code path. The MCP test passes through the HTTP edge on jupiter and stops at landing's `Unsupported`; it joins the Windows CI lanes when landing runs there | built, unrun: the arm64 lane lints five crates and runs no daemon |
| Fleet node (consensus, transport over UDP, takeover) | built | built | built, unrun | partly: the daemon's transport runs over AFD UDP, on a socket no child inherits and no other socket can bind on any address (A-183), and serves its export (A-184), but the anchor holds no serve sockets (gap 2) | partly (same) |
| `slates exec` (a program sees a volume at a path) | missing: "needs Linux user and mount namespaces" (`crates/cli/src/main.rs`) | built | built, unrun | missing | missing |
| Container bind (the OCI attachment) | built (the NFS mount bound in) | built | built (CI's arm64 lane) | built, unrun for Docker Desktop's Linux containers: the export (A-184) is the way in, through `host.docker.internal`, and no Docker Desktop on Windows has mounted it yet; impossible for process-isolated containers, measured (G-mount step 4: Docker's engine cannot see a WinFsp drive) | missing (same) |
| `slates oci-runtime` and `oci-check` (judging the engine and the bind source) | built | built | built, unrun | missing: "facilities this platform does not have" (`crates/cli/src/main.rs`) | missing |
| `slates vhost-user` (a VMM attaches slates' own virtio-fs device) | missing; QEMU 9.1's `memory-backend-shm` exists for vhost-user off Linux (to confirm from QEMU's source) | built (crosvm, the box) | built, unrun (crosvm's window is x86_64 only, but the device itself is not) | impossible (unconfirmed): no VMM on Windows offers a vhost-user front end; to confirm | impossible (unconfirmed) |
| Shards route (`slates shards`, the daemon's stream, DAX) | built | built | missing a run: G-arm64-guest | built through step 5, proven on jupiter: the daemon's pipe (A-179), `slates shards` starting its command in a kill-on-close job and naming it to the daemon as the stream's holder (A-181, f71282e1), Shards' device (its `gpu` 3b1e52a4), the DAX answer (A-180); owed: Shards' window (step 6, its patch with the owner) and a WHP guest's proof (step 7) | the ipc hand-off test runs (`stream_spawn`); the CLI test does not, as the lane builds no `slates-cli` |
| Direct map (A-135: a same-kernel process maps a file with no copy) | built (A-173) | built (A-171) | built (CI's arm64 lane) | built (A-175) | built, unrun: G-arm64-host |
| Base overlay (a volume over a host directory, read only) | built | built | built, unrun | built (`crates/base/src/windows.rs`) | built, unrun |
| Disk images (`slates image build`, `merge`) and Firecracker | image verbs: built, unrun (`crates/cli/tests/guest_image.rs` is Linux's); Firecracker: none (Linux KVM only) | built (A-160, A-165) | Firecracker pmem: owed, G-arm64-guest | image verbs: built, unrun; Firecracker: none | none |
| Python and Node SDKs | built (by-use tests in CI) | built | built, unrun | built, unrun: Windows lanes clippy them, no by-use test | built, unrun |
| Crash dumps keep no secret | built (`RLIMIT_CORE` 0) | built | built | weaker, and Ada's call (GAPS `8a-win-dumps`, OPEN): no unprivileged `RLIMIT_CORE`, so `exclude_from_dumps` is `Ok(())` (`crates/cli/src/dumps.rs`). Unprivileged steps exist for the decision: `WerSetFlags(WER_FAULT_REPORTING_FLAG_NOHEAP)`, and `WerRegisterExcludedMemoryBlock` for each region as it is made | weaker (same) |
| Hermeticity tracer (no write outside a granted target) | built (`sudo eslogger`; root and Full Disk Access, run by hand: `xtask/src/conformance/hermeticity.rs`) | built | built, unrun | missing (a branch exists: `windows-hermeticity-tracer`) | missing |
| Conformance suites | built (pjdfstest over NFS) | built (pjdfstest, fsx, fsstress over FUSE) | built, unrun | missing (WinFsp has its own suite, `winfsp-tests`) | missing |

## Order of work

1. **TCP in the Windows runtime** (gap 1): done, A-182. The MCP HTTP edge on Windows and the Windows datagram socket
   made non-inheritable and exclusive: done, A-183. The network export on Windows: done, A-184; owed there, a real
   Linux kernel client mounting it (Docker Desktop's machine or WSL2).
2. **Holding a restarted daemon's sockets and mounts on Windows** (gap 2): the fleet's serve sockets, then a WinFsp
   mount that outlives its daemon.
3. **The macOS anchor's restart from its own image** (A-145), and crash dumps on Windows once Ada decides
   `8a-win-dumps`.
4. **`slates exec` on macOS and Windows**, and `oci-runtime`/`oci-check` on Windows.
5. **Every built-but-unrun cell gets its run**: the daemon and CLI suites on Windows arm64 (its lane builds no
   `slates-cli`) and Linux arm64, the SDKs' by-use tests on Windows, and the hermeticity tracer on Windows and a
   conformance suite there.
6. **`slates vhost-user` on macOS**, once QEMU's source confirms a front end there; the Windows cells confirmed or
   built.

Owners: G-shards-windows steps 2 and 5 are slates-00's and slates-36's (Ada, 2026-10-26); the rest of this list is
slates-d7's unless Ada assigns it otherwise. Each item, as it lands, updates its row here, the gap ledger and its
design section in the same change.
