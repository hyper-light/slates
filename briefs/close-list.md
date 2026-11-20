# CLOSE LIST (main, 2026-11-20 ~16:05 PST, Ada's order: close the existing UI items; nothing else)
Each line: UI goal / part -> owner(s): the items that close it, in order. A part is marked met when every item holds on
origin/main with evidence (commit + test or measurement). See briefs/common.md "CLOSE THE OPEN ITEMS".

84a Loaded p99/p999 of a full edit and merge under 1 ms on Linux, macOS, Windows -> g222 (lead: cause + fix, the three
    platforms' numbers), w-provision-gate (macOS: the zero-timeout kevent handing the core away), w-engine (89b's
    pacing only where it cuts this p99).
84b macOS quiet-Mac run: no 17-31 ms late wake of a kicked, parked shard -> w-provision-gate (the quietest Mac kiln
    gives: the big Studio or adalundhe-macbook-pro; record the load). g223's Studio keeps dropping off kiln.
85a A live write adds < 50 us median, < 1 ms p999 under load -> g228.
85b Chaos across real processes, every register invariant holding -> w-forming: (1) the readmission test's flake,
    (2) a given-up green reopened like A-775's volumes, (3) the chaos run itself (latency + loss, replicas offline,
    volume failure, client disconnect), recorded. g227 lands its catch-up A/B winner here.
85c A live_chaos flow kills a writing client mid-write; the next owner keeps or drops the write whole -> w-fed-faults,
    after 91.
86a 1,000 clients under 1 ms (loadtest, then W1) -> w-balance with 89a; w-work-mount runs the loadtest.
86b 100,000 clients, one primary + two replicas, editing the Linux kernel tree, under 1 ms, conflicts reported
    correctly -> w-fed-move (lead), w-sessionlog (conflict reporting).
87  v0.1.0 published for macOS arm64, Windows, Linux amd64/arm64 -> Ada's go (outward-facing).
89a The 1,000-client A/B: a green's works off its owner shard, edit and merge under 1 ms at 1,000+ clients ->
    w-balance (lead), w-work-cow.
89b A-579: the server's own p99 under load found and cut (9-75 ms at 1,000 clients) -> w-engine (the pacing
    primitive ruled 15:35 is its lever).
90a kiln vendors slates, builds it in kiln's workspace and CI on every platform, calls the library API ->
    w-k90-vendor (vendor + the export_engine re-vendor), w-k90-pin-2 (every platform's lane green; the runner-link
    flap it was given unblocks the rest).
90b OCI/Docker engine on slates volumes -> w-k90-containers; w-k90-a520 owns the session-end fix (r49).
90c Kubernetes engine on a KIND cluster -> w-k90-close.
90d microVM engine on Linux KVM (HVF and WHP done) -> w-k90-kvm, w-k90-microvm (drop the kiln take_down/notice bugs).
90e kiln's slates_e2e bare_metal tests on Windows -> w-k90-win.
90f SlatesVolumes keeps the resync end on KVM -> w-k90-kvm, w-k90-microvm.
90g A-520 piece 5, the container engine on workspace::container -> w-k90-a520.
91  The multi-runner test with injected faults -> w-fed-faults.
92a The Shards-owed items done with Shards' side -> w-k90-whp.
92b A-555 parts 2-3 (parking on a pending S3 base) -> w-k90-whp.
93 shared  The shared pieces of 4.17.1 -> w-shared (three sleep-and-look waits, exit 4 at 64 MiB, inputs held at B, the
    lane's ~200 MB/s, the content-plane-vs-lane A/B, A-697 f, A-658 3c); w-perf (deltify job, svn storage A/B vs FSFS,
    c3); w-engine (A-760 confirmed whole with evidence, A-752's two literals derived); w-balance (v3 at f+1 with the
    takeover test: send the evidence, e73283fd6 + 08fba4007).
93 git  -> w-git (partial clone regression since p3; push_planned: pack homed as content v4, store commit sliced; B2's
    landed half with w-store), w-realtools (load-only edge tests: headers early + side-band keepalive; the Windows
    restart-spin check), w-interop (r110 rerun), w-k90-vendor (the encode spin at git_encode.rs:345), w-bugs (deepen's
    217 s: read_objects).
93 hg   -> w-hg (replay at restart sliced), w-perf (the import's 5.73 GB peak), w-sizing (7.1 GB resident after an
    84 MB push: extents; g226's give-back numbers feed it).
93 svn  -> w-svn (update 5.6 s vs 2.7 s; the exporter's rebuild sliced + red svnserve-over-SSH test; the hook test
    flake; recovery replays a revision record whole; the 500 MB commit re-measured; DeltaV v1), w-perf (deltify job),
    w-interop (HTTP v2 rows).
93 interop -> w-interop (lead), w-hg (clone-b 357 s), w-sizing + w-hg + w-store (peak RSS 145x), w-reaper (svn
    revprop rows A-645 (a); the issuer route's EINTR fix for the Linux counts), w-svn (svnsync times), svn on Windows
    (needs Ada's go for an svn client on jupiter).
93 perf An order of magnitude faster than competing servers, measured and recorded -> w-rewrite-ops (the comparison:
    each server's client operations against stock git/hg/svn servers, release, best of N, all shown, in BENCHMARKS),
    w-perf after its items.
Keeper: w-lint (the wall, run-requests). Machine fix: w-k90-pin-2 (cec37f60's kiln link closing every 1-5 min).
