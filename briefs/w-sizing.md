# w-sizing: end gross over-allocation, slates-wide (main, 2026-11-20 ~12:20 PST)

## Why
Ada, 2026-11-20, on the daemon holding 7.1 GB resident after pushing hg's history (an 84 MB store, a 162 MB bundle),
while the clone's own heap stayed about 300 MB: "this is a repeated sizing problem we've had and signals gross
overallocation. You need to use learned and derived approaches that *carefully* consider and respect the devices they're
on, and preferable are as zero-alloc as possible." Read the new MEMORY SIZING section of briefs/common.md first; it is
your rule.

## Your work, in order
1. **The shared-memory segment and its prefault.** Per w-hg's r2/r9 on the big Studio, the 7 GB resident is the
   segment's shared memory, not heap. Its lead: the idle shard pre-faults `prefault_reserve_bytes`, the size of the
   last write burst, at an idle tick, and a reservation may be sized from the machine. Find every input to the
   segment's size, its regions and its prefault, and make each one follow the work, be learned, decay, and be given
   back. Read w-hg's r9 numbers first (progress/w-hg.md; ask it, session slates-work-1d, through an inbox note). Red
   first: resident memory after a push of N bytes, and after 20 s idle, within a derived multiple of N. Land with
   before and after numbers.
2. **The sweep.** List every sizing site in the workspace: anything that reads the machine's RAM, cores or disk
   (sysctl hw.memsize, sysinfo, /proc/meminfo, GlobalMemoryStatusEx and the like), fractions and multiples of them,
   reserves, prefaults, capacities, ring and region counts, cache bounds, admission rooms. For each, record how it is
   derived today and what it should be (work-derived, learned, decaying, pressure-aware). Put the list in
   docs/wip/GAPS.md under one entry. Convert the machine-fraction ones, largest first, each red first, each landed
   within ~30 minutes.
3. **Device pressure.** One seam that reads the platform's pressure signal (macOS memory pressure, Linux PSI,
   Windows commit charge), which sizing sites consult so they shrink, and refuse typed, before the machine swaps.
   Laptop ≡ fleet: one code path.
4. **Zero-alloc.** Name the allocations per item, request or chunk on data paths that your sweep finds, and remove
   them: lend, reuse, stream.

## Who else
- w-hg owns hg-specific retention.
- w-git owns memory per push.
- w-perf owns the svn text reads.
Share findings by inbox notes on the state branch, and don't edit their files while they're in them.

## Rules
Follow ~/fanout-state-w-sizing/briefs/common.md in full. Clone the state branch first:
`git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git ~/fanout-state-w-sizing`, or use the HTTPS
URL if SSH fails. Land on slates' origin/main and confirm with merge-base. Never force. Never checkout, switch, stash,
reset or commit-tree. No installs or system changes. Keep ~/fanout-state-w-sizing/progress/w-sizing.md current and
pushed. Report per piece: landed (commit), ran (numbers before and after, the machine), next.
