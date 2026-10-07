# The thirteen conditions: where each stands, and the evidence

This maps the project's terminating conditions (Ada's standing goal, 2026-10-04) to the evidence in the tree:
- the README's capability table, rendered from `xtask/src/capabilities.rs`; `cargo xtask check` refuses it if a cited
  test disappears or is ignored;
- the dated records in `GAPS.md` and `BENCHMARKS.md`, cited by their headings;
- the tests themselves.

A status is a reviewed judgement:
- **works**: proven as the condition states;
- **limited**: proven within named limits;
- **failing**: measured not to meet the condition.

Every limit names what is owed. Updated whenever a condition's evidence changes (CLAUDE.md §6: the record changes
in the same commit as the work). Last reviewed 2026-10-07 at `89956659`.

| # | Condition | Status |
|---|---|---|
| 1 | One binary and CLI, laptop to global, with records proving it | limited |
| 2 | Volumes provisioned and managed; OCI, CRI, Kubernetes, microVMs | limited |
| 3 | No write reaches disk except a granted landing | works |
| 4 | Escapes battle-tested adversarially | limited |
| 5 | Fast reads of the local filesystem, accurate diff and change tracking | works |
| 6 | Cross-OS and cross-architecture | limited |
| 7 | Fast remote pulls under heavy contention and bad networks | failing on independent delay jitter; works otherwise |
| 8 | Post-quantum protected transfer | works for the fleet transport; the export's TLS group unverified |
| 9 | Post-quantum encryption at rest and in transit | works |
| 10 | Multi-cluster replication under horrible networks | limited |
| 11 | No panics; failures recovered, proven adversarially | works |
| 12 | Far-above-industry p99s under non-ideal patterns, against Tectonic | limited |
| 13 | A full MCP server with codemode and skills, for every agent | limited |

## 1. One binary, laptop to global

- **Evidence.** `slates` is one binary: the CLI, the anchor, the daemon and the MCP server (`crates/cli`).
  - Laptop and fleet run one code path: no mode switches (R8). The commit rule is proven identical at f = 0 and a
    simulated f = 1 (`crates/db/src/register.rs`, AC-2.5).
  - A fleet deploys from one manifest across real processes, including the owner's SIGKILL
    (`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death`).
  - On real Linux pods: `BENCHMARKS.md` "The KIND lane on the streamed-checkpoint build (2026-10-07)" — formation,
    placement at f + 1, a SIGKILL takeover served in 3.5 s, a fresh-identity rejoin in 3.7 s, and 0 leader changes
    under WAN, WAN-with-loss and 350 ms profiles.
- **Owed.**
  - A multi-region deployment on real separate networks: the KIND lane emulates WAN on one host.
  - The transport's replacement by a conformed QUIC (AUD-29-50–54, capability row "Fleet").

## 2. Volumes; OCI, CRI, Kubernetes, microVMs

- **Evidence.** The capability rows:
  - lifecycle (works);
  - mount on macOS and Linux (works) and Windows (limited);
  - "Containers: an OCI runtime binds a volume" (limited: Docker Desktop and Docker Engine);
  - "Kubernetes: kubelet mounts a volume as an NFS PersistentVolume" (limited: kernel-TLS nodes, a CI-only kubelet
    leg);
  - "VM guests over virtio-fs" (limited: QEMU's vhost-user-fs, nine roster workloads identical in a live guest).

  `BENCHMARKS.md` "Linux FUSE mount: small creates, and a containerd/runc binding (2026-10-05)".
- **Owed.**
  - Firecracker and Cloud Hypervisor guests (untested).
  - Rootless engines and other runtimes (refused typed until a workload runs, AUD-29-67).
  - A CRI-level integration beyond the kubelet NFS PersistentVolume (no CSI driver: a node plugin needs privileged
    mount propagation, which R10 forbids).
  - The kubelet leg cannot run on Docker Desktop: its kernel has no `CONFIG_TLS`, so it skips loudly here.

## 3. No disk escape except a granted landing

- **Evidence.**
  - The lint wall denies `std::fs` and file-creating syscalls outside the landing crate, and a structural test
    walks the dependency graph (CLAUDE.md §1, R1).
  - `GAPS.md` "no write escapes to disk while the daemon is killed mid-install (conditions 3, 4, 11)": real pip and
    npm installs with 15–19 daemon SIGKILLs, and `find -newer` showing only the test's and npm's own files.
    Re-run 2026-10-07 on this day's build: 11 and 8 kills, 3,606 RECORD hashes intact, `npm ls` clean, the same
    escape set.
  - `GAPS.md` "an ended FUSE mount no longer exposes the disk beneath it" and "a strict volume locks only its own
    content".
  - The landing engine's crash-at-every-instruction oracle (`crates/land/tests/oracle.rs`).
- **Owed.** Nothing named.

## 4. Escapes battle-tested

- **Evidence.**
  - `GAPS.md` "an adversarial container run — containment held".
  - "Kubernetes pods do not get the PersistentVolume's nosuid".
  - "an ended FUSE mount no longer exposes the disk beneath it".
  - The landing's escape tests (`crates/land/tests/os_escape.rs`), symlinks out of the volume resolving only for
    their owner (A-107), and hostile NFS (`crates/server/tests/nfs_hostile.rs`).
  - A generated battery: `GAPS.md` "a generated escape battery through a shared FUSE mount" (recorded command
    `docs/wip/bench/escape/run.sh`). 1,500 seeded hostile steps as the owner and another user produced no
    violation: 192 of 192 out-of-volume links refused to the other user, with positive controls, 0 daemon disk
    writes, and the outside tree's hash unchanged.
  - Over Linux's NFS client (`run-nfs.sh`, mounted `nosuid,nodev` as kubelet does): no disk write, the outside
    tree unchanged, no device, no setuid elevation, but **out-of-volume links followed by another user on 46 of 116**
    (the client caches symlink targets per inode, so A-107's per-caller rule holds only until a link's first
    resolution; `GAPS.md` 2026-10-07).
- **Owed.**
  - A-107 on NFS: a per-identity export, or links the client must re-ask about.
  - The battery over virtio-fs, and against a hostile user holding `CAP_SYS_ADMIN`.

## 5. Fast local reads, accurate diff and change tracking

- **Evidence.**
  - `BENCHMARKS.md` "An overlay of a real tree: read, change, plan (2026-10-05)".
  - "Real workloads through Linux's own NFS client, beside tmpfs".
  - `GAPS.md` "Linux FUSE: the pip leftovers check".
  - The base overlay's tests (`crates/vfs/tests/base.rs`), with drift, witnesses and digests against a simulated
    host with outsider edits.
  - 2026-10-07: a drift re-check the host refuses is reported unverified, never as a match.
- **Owed.** Nothing named.

## 6. Cross-OS and cross-architecture

- **Evidence.**
  - macOS on arm64 natively (this machine).
  - Linux on aarch64 (Docker lanes; the Linux clippy gate on every commit).
  - `BENCHMARKS.md` "x86_64 Linux under emulation (2026-10-05)".
  - Windows: the daemon builds and runs its lifecycle in CI, and the WinFsp mount is proven live on the
    `windows-latest` runner (capability row, limited).
- **Owed.**
  - Native x86_64 performance numbers (emulation proves behaviour, not speed).
  - Windows beyond create, write, read, list and delete.

## 7. Remote pulls under contention and bad networks

- **Evidence.** `BENCHMARKS.md` "Remote pulls of 64 MiB …" and "Remote pulls under horrific networks"
  (`fetch_bench`, real endpoints over the simulated network). These complete:
  - delay, 1% and 5% loss;
  - a 1/20-rate holder, and a silent holder;
  - 8 readers, and 8 readers at 10% loss (199.6 of 300 Mbit/s).
- **Failing.** Independent delay jitter of ±40–60 ms on 200–250 ms paths. The two reordering rows do not finish
  64 MiB in 600 s, and 8 MiB takes 68–104 s on a 10 Mbit/s link. Copa reads non-congestive jitter as queue (the
  starvation result, Arun, Alizadeh, Balakrishnan, SIGCOMM 2022). Three fixes were measured and rejected; the
  records are in `BENCHMARKS.md`.
- **Owed.** A delay signal that separates jitter from queueing without a long memory of the path's rate, clearing
  `congestion_bench`'s 56 scenarios with no regression and the jitter rows at 8 MiB.

## 8. Post-quantum protected transfer

- **Evidence.**
  - The fleet transport's TLS 1.3 key exchange prefers `SecP384r1MLKEM1024` (ML-KEM-1024 hybrid,
    `crates/transport/src/kx.rs`).
  - `GAPS.md` "post-quantum key exchange by default".
- **Owed.** The Kubernetes export's RPC-with-TLS handshake runs in the node's `tlshd` (GnuTLS); its key-exchange
  group is the node's, not slates', and is not yet verified post-quantum.

## 9. Post-quantum encryption at rest and in transit

- **Evidence.**
  - `GAPS.md` "volumes sealed at rest (A-92)" and "sealed content no longer leaves plaintext in RAM".
  - `BENCHMARKS.md` "A-99 sealed read" and "The idle sweep on real installs through Docker".
  - Idle content is sealed with AES-256-GCM. A 256-bit symmetric key keeps 128-bit strength against Grover's
    algorithm, the margin NIST and CNSA 2.0 give for post-quantum symmetric protection.
  - 2026-10-07: a seal refused partway can no longer leave ciphertext marked plaintext, and a write log's clear
    scrubs its plaintext whatever its header writes answer.
  - In transit: condition 8.
- **Owed.** Nothing named.

## 10. Multi-cluster replication under horrible networks

- **Evidence.**
  - The capability row "Fleet: replication, takeover, multi-process deployment" (limited).
  - The fleet suite (`crates/server/tests/fleet.rs`, 71 tests), including cross-region lookups to a copyset
    successor.
  - The KIND lane's WAN, WAN-with-loss and 350 ms profiles.
  - `fetch_bench`'s content pulls (condition 7).
  - 2026-10-07: two regions on two Docker networks, joined only by a router shaping 100 ms ± 40 ms one way and
    3 % loss (`docs/wip/bench/multiregion/run.sh`). Both regions formed and bootstrapped. The run found and fixed an
    owner lease that lapsed whenever far members stretched the probe round (`GAPS.md` 2026-10-07).
- **Owed.**
  - **Cross-region mirroring and promotion are built and proven in-process** (§4.10; `docs/wip/mirroring.md`): a
    volume awaited in the mirror is read byte-identical in the mirror region after its home region is lost and
    promoted. Still owed: the loss-window report, `NotPlaced { mirror }` at a deadline, and the two-network run
    under a shaped router.
  - Cross-region reads are refused `HomedElsewhere` on the two networks (location rounds `unavailable`).
  - The detector's far-link false deaths are fixed in the vendored copy (2026-10-07); the upstream change is owed,
    and the two-network run still has to show it on the real topology.
  - Condition 7's jitter design, which replication's content pulls share.
  - The copyset-successor fleet test fails intermittently under a loaded full suite (4 of 9 untraced runs, passing
    alone). It is instrumented (`fleet.owner_location.no_session`); the hypothesis is a location round that skipped
    a successor it held no session to.

## 11. No panics; recovery proven adversarially

- **Evidence.**
  - Panics are denied in every crate by the lint wall: `unwrap`, `expect`, indexing, slicing and overflowing
    arithmetic (`GAPS.md` "the no-panic sweep's indexing half is closed and the lints are denied
    workspace-wide").
  - SIGKILLs under real workloads (`BENCHMARKS.md` "The daemon SIGKILLed in the middle of real workloads", "SIGKILL
    under write load on a Linux FUSE mount").
  - Crash at every landing instruction (`crates/land/tests/oracle.rs`).
  - Recovery oracles: `crates/vfs/tests/recover.rs`, including a write-accounting recount after every step; and
    `crates/db/tests/model.rs`, with drops and recoveries at random points.
  - The 2026-10-07 sweep of discarded results fixed a seal, a descriptor leak, dropped replies, a truncate and a
    scrub, and made every remaining refusal counted in status.
- **Owed.** Nothing named. The goal's "batteries" are the suites above; a generated fault-injection campaign across
  a live fleet (the deterministic simulation's nemesis library over the seed budget, CLAUDE.md §4) is the
  strengthening.

## 12. Tail latency against Tectonic

- **Evidence.** `BENCHMARKS.md` "Tail latency under a hot-directory storm, against Tectonic's published tails",
  re-run 2026-10-07:
  - create p99 0.35 ms at one writer and 3.37 ms at sixteen in one directory;
  - reads 80 µs p99;
  - the daemon's own operation p99 about 10 µs.

  Against Tectonic's published 150–200 ms write and about 100 ms read tails (FAST'21), with the media difference
  stated.
- **Owed.**
  - A broader p99 matrix: mixed read-write, large files, a loaded fleet, remote pulls.
  - Condition 7's jitter tail.

## 13. MCP with codemode and skills

- **Evidence.**
  - The capability row "MCP server for agents" (works).
  - `GAPS.md` "MCP speaks 2026-07-28, dual era (A-81)".
  - `BENCHMARKS.md` "Codemode against list-and-read on a real agent task".
  - The skills served over MCP (`crates/mcp/src/skills.rs`, `skills/`).
  - `crates/mcp/tests/mcp.rs`: version negotiation, tools, refusals, batches, subscriptions, and the HTTP transport
    with its authorization.
- **Owed.**
  - Validation by an independent checker (the official MCP inspector needs an npm install, awaiting Ada's
    authorization).
  - Recorded runs from each named agent (Codex, Claude, Cursor, Pi) against a live daemon.
