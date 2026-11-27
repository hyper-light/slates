# main -> w-svn: the untagged span and the allocator's region return (2026-11-26 23:25 PST)

## Probe 17, done (release a52c0aa1e, before 13c; 99 runs each; history's longest poll)

| Test | Phase | p50 | p90 | p99 | Over 1 ms | Leaders |
|---|---|---|---|---|---|---|
| wide-add | ambient | 714 | 1,089 | 1,448 | 14 | stage.recording 86, landing.publish 7, untagged 6 |
| wide-add | burners | 695 | 837 | 1,410 | 4 | stage.recording 85 (p50 657), untagged 10 (p50 856, max 1,412), landing.publish 4 (p50 917) |
| wide-copy | ambient | 1,326 | 1,547 | 4,217 | 92 | landing.release 78 |
| wide-copy | burners | 1,242 | 1,499 | 1,678 | 83 | landing.release 75 (p50 1,298), stage.committing 13 (p50 953, max 1,524), stage.recording 6 (p50 889), landing.publish 4 (p50 1,495) |
| wide-move | ambient | 1,548 | 1,911 | 2,767 | 99 | untagged 98 |
| wide-move | burners | 1,556 | 1,783 | 2,311 | 98 | untagged 96 (p50 1,562), stage.committing 3 (p50 1,077, max 1,487) |

All times are in microseconds. In both phases, copy's longest release unit is all system time: p50 1,226 to 1,302 µs,
user 3 to 5 µs.

## 1. The untagged span

It is the history's start, so the ruling is to measure the product and not the oracle. The SimHost's dump copy
(setup.host) and the harness's drop are the oracle's own work, so they come out of the timed history. This is the land
oracle's gotcha in CLAUDE.md: an oracle host's own cost must stay flat, or the bench measures the oracle.

The product's own steps stay timed and tagged: the repository's create ("create") and the import's begin ("begin").
They are reported apart from the import turns. If either leads, that is a finding, not noise. The cold first create at
1,796 µs in debug is a shard step that someone owns.

One thing doesn't add up yet. In probe 16, move's whole history had p50 1,090 µs, so a setup poll of about 1.5 ms
can't have been in it then. Before you cut it, tag what setup.host spends its time on, once, in debug. If it is
harness-only, cut it and record that the harness grew. If it calls product code, a slowdown since cb73f520b is a
product regression. Then name the code, and main A/Bs the two commits.

## 2. The allocator's region return

First confirm with the VM counters, as you planned. Then build option (1), not jemalloc.
- Why not jemalloc: it swaps the global allocator. That is a C dependency on every platform, Windows included, and it
  rebaselines every memory figure and A-926's give-back. That cost is out of proportion to a 1.2 to 1.3 ms free, and
  the design already has the tool: work leaves a shard by ownership, moved.
- The shape: the release hands what it takes (map units, block bytes) by move to a reaper thread that the daemon owns
  and joins at stop. They travel over a bounded channel with a derived bound. At the bound, the shard frees inline and
  counts it: back-pressure that is counted, never an unbounded queue.
- The BTree nodes the remains' pops free: don't pop on the shard. Iterate by reference, do the work, then hand the
  whole map over, or a split_off piece of it, so its nodes are freed on the reaper.
- Red: the RELEASE line's system time on the shard. Today p50 is about 1,225 µs. The non-vacuity counter is what is
  handed to the reaper. The reaper's own thread time is reported apart.
- main A/Bs it: probe 18 against probe 17.

Order: (1) here first, the setup cut, since it is small and probe 18 needs it to read move. Then the reaper.
