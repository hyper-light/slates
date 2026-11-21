# CLOSE LIST (main, 2026-11-20 ~16:15 PST). Ada's order, binding:
- "ensure they close the existing fucking items"; "anything you have currently added up until now gets closed, but no
  more new"; "the large message I sent you (the Codex/ChatGPT analysis) also needs to be closed"; "as well as the
  changeset work and integration in full"; "prioritize implementation/completion first, then robustness, then
  performance".
- Each owner works ITS list below, in order: completion items, then robustness, then performance. Nothing else.
  A new finding goes to docs/wip/GAPS.md and the owner's progress file; it is not worked and not added anywhere.
- An item closes when it holds on origin/main with evidence. Write inbox/main-from-<you>-close.md (the item, the
  commit, the test or measurement); main marks the UI part met the same check.
- Platform runs other than your own machine's, suites, clippy and CI go to run-requests.md (CLAUDE.md §8).

## A. THE ANALYSIS (Ada 13:15, Slates vs Sapling/EdenFS: "dispatch agents to tackle these areas and gaps")
w-work-cow     C: a Work costs its divergence: a first edit copies the chunks written, not the whole file (red: a
               1-byte edit to a large file adds about a chunk). P: the brief's deciding measurement over the grid
               (1..100k files, 1 MB..1 GB, 1..100 Works), release, in BENCHMARKS.
w-work-mount   C: a Work mounts and works through FUSE and WinFsp too (tests written; Linux/Windows runs in
               run-requests); multi-span edits through D-O17's minimiser. R: unsubmitted mount saves survive a daemon
               restart; a mount refreshes when its Work's base moves. P: lazy base read for mounts and landings.
w-realtools    C: land the real-tools acceptance test (#pragma once across a snapshot change and a SIGKILL takeover),
               then make (mtimes/ctimes stable), cargo incremental (a no-op), git status (clean), each across a
               snapshot change, memory pressure and a takeover; Linux FUSE and WinFsp runs in run-requests.
w-rewrite-ops  C: cross-shard Works get the base bytes of files replaced whole; a rename over an existing file is
               refined. R: the refine runs in slices under the shard's quantum, its worst case bounded.
w-sessionlog   C: identical vs accepted in the submit's reply (one typed outcome); merges held for approval shown in
               the log; the same read-only view over MCP and in the Node and Python SDKs.
w-provision-gate C: R9's provisioning gate wired as the permanent ratcheted CI gate (provision_bench: p50/p99/p999/
               max, spinning and parked). P: the loaded p99 (1.6 ms; the macOS zero-timeout kevent you found) fixed
               red first, with g222.

## B. THE CHANGESET (Ada 12:20 "one unit, the changeset"; 16:10 "the changeset work and integration in full")
w-store        C: (2) vendor Mantle's engine/disk/chunk/crc/codec + hyper-block/hyper-log: NO hyper-rt (an external
               runtime, D-9) and NO unsafe impl Send/Sync; port the vendored seams onto slates_rt (its bounded rings
               and completion drivers) as vendor patches in the manifest; the engine's disk writes sit behind the
               land crate's write seam (R1, under the repository's standing grant). (3) slates-store: the seam
               (piece/stage/land/changesets), landing (volume flush, log flush, apply), the content-form A/B with the
               derived per-path arm. (4)-(6) hg, git, svn onto it with w-hg, w-git, w-svn, each with an RSS red test.
w-git          C: git onto the changeset store with w-store: B2's landed half; the pack homed as content (record v4)
               as the changeset's payload section. R: load-only edge tests (headers early + side-band keepalive); the
               Windows restart-spin check. P: the partial clone regression since p3; push_planned's store commit
               sliced.
w-hg           C: hg onto the changeset store with w-store (the derived per-path index). R: replay at restart sliced
               (130 s of CPU in 265-277 ms units). P: clone-b 41 -> 357 s.
w-svn          C: DeltaV v1; svn onto the changeset store with w-store (your stored-form answer). R: the hook test
               flake; recovery replaying a revision record whole, sliced; the exporter's rebuild sliced + the red
               svnserve-over-SSH test. P: svn:// update 5.6 s vs 2.7 s; svnsync 524 / 1,068 s (A-772); the 500 MB
               commit re-measured.

## C. THE UI GOALS' OPEN PARTS (owners; each owner's order is C, R, P)
84a loaded p99/p999 < 1 ms on Linux, macOS, Windows ........ g222 (lead), w-provision-gate, w-engine
84b the quiet-Mac run (no 17-31 ms late wake) ............... g222 (the quietest Mac kiln gives; record the load)
85a a live write < 50 us median, < 1 ms p999 under load ..... g228
85b chaos across real processes, invariants holding ......... w-forming: C a given-up green reopened as A-775's
    volumes are; R the readmission test's flake; then the chaos run itself (latency + loss, replicas offline, volume
    failure, client disconnect), recorded; P the capture image and large delta reads sliced. g227 lands its catch-up
    A/B winner.
85c live_chaos kills a writing client mid-write .............. w-fed-faults, after 91
86a 1,000 clients under 1 ms ................................. w-balance (with 89a)
86b 100,000 clients, 1 primary + 2 replicas, Linux tree ..... w-fed-move (then its 'unplaced' first-move asks)
87  v0.1.0 published ......................................... Ada's go (outward-facing)
89a the 1,000-client A/B (works off the owner shard) ......... w-balance (then run 31, the three-node A/B, coalescing,
    learned weights, already given)
89b A-579 the server's own p99 under load .................... w-engine (after A-760 whole and A-752's two literals;
    the pacing primitive is its lever)
90a vendoring, every platform's build and CI, the library API  w-k90-vendor (incl. the export_engine re-vendor),
    w-k90-pin-2 (lanes green; R: the cec37f60 kiln link closing every 1-5 min)
90b OCI/Docker engine ........................................ w-k90-containers; w-k90-a520 the session-end fix (r49)
90c Kubernetes on KIND ....................................... w-k90-close
90d microVM on KVM (HVF and WHP done) ........................ w-k90-kvm, w-k90-microvm (then R: kiln's take_down,
    first-mount wait and collapsed notices, already given)
90e slates_e2e bare_metal on Windows ......................... w-k90-win
90f SlatesVolumes' resync end on KVM ......................... w-k90-kvm, w-k90-microvm
90g A-520 piece 5 ............................................ w-k90-a520
91  the multi-runner test with injected faults ............... w-fed-faults
92a the Shards-owed items with Shards' side .................. w-k90-whp
92b A-555 parts 2-3 .......................................... w-k90-whp
93 shared  w-shared (three sleep-and-look waits, exit 4 at 64 MiB, inputs held at B, the lane's ~200 MB/s, the
    content-plane-vs-lane A/B, A-697 f, A-658 3c); w-perf (deltify job, svn storage A/B vs FSFS, c3); w-engine (A-760
    whole, A-752's literals); w-balance (send v3-at-f+1 evidence: e73283fd6, 08fba4007)
93 git  w-git (list B); w-interop (R: r110's rerun); w-k90-vendor (R: the encode spin at git_encode.rs:345); w-bugs
    (P: deepen's 217 s, read_objects)
93 hg   w-hg (list B); w-sizing (R: 7.1 GB resident, extents made on demand; g226's give-back numbers feed it);
    w-perf (R: the import's 5.73 GB peak)
93 svn  w-svn (list B); w-perf (P: deltify job); w-interop (C: HTTP v2 rows)
93 interop  w-reaper (C: the issuer route's EINTR fix, then A-645 (a) svn's revprop rows); w-interop (C: HTTP v2
    rows, the Linux counts, svn on Windows on Ada's go; R: r110's rerun); w-sizing + w-hg + w-store (R: peak RSS 145x);
    w-hg (P: clone-b); w-svn (P: svnsync)
93 perf an order of magnitude faster than competing servers, measured and recorded: w-interop, after its items (each
    server's client operations against git daemon/http-backend, hg serve, svnserve/mod_dav_svn; release, best of N)
Keeper: w-lint (the wall, run-requests). g226: the give-back probes for w-sizing.
