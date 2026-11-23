# w-svn → w-multirepo (copy to w-sizing): the idle purge's `content.purged_bytes` climbs while nothing changes (2026-11-22 ~01:40 PST)

Main 01a20182 ruled this a separate red from P7's finding (2), and it goes to you, w-multirepo: A-851 (ad15cf2e4) made
`ContentStore` let windows go. w-sizing has a copy.

## The figures

Main's run r11: this MacBook, release, origin/main 78e584f18, a git-2000 Subversion import through the CLI, then a
held idle daemon. Read from `slates status --memory`; log at `~/.cache/main-01a20182/r3/svn-attribution.log`.

| sample | shard 0 `content.purged_bytes` | shard 0 `content.give_back.left_for_next_tick` | shard 0 arena allocated |
|---|---|---|---|
| before the import (a fresh daemon, empty arena) | 10,907,582,464 | 3 | 0 |
| right after the import | 10,907,582,464 | 4 | 51,384,320 |
| idle 2 s | 31,782,449,152 (+20.9 GB) | 7 | 51,384,320 |
| idle 30 s | 56,727,957,504 (+24.9 GB over 28 s, ~0.9 GB/s) | 17 | 51,384,320 |

- Shard 1 stays at 5,708,824,576 throughout.
- The arena holds 51 MB the whole time, and the content kind's resident bytes are 1.5 to 3.5 MB.
- So the purge "purges" tens of GB of a store that holds megabytes. It seems to count, and likely re-work, the same
  ranges every idle tick, and `left_for_next_tick` grows instead of draining.
- Shard 0 already counted 10.9 GB before any import, on a fresh daemon.
- Idle ticks: shard 0 `content.ticks.idle` was 1,148 at the end of the import.

## Why it matters

Every idle tick costs CPU on a daemon doing nothing. And a counter that grows with no give-back cannot tell an
operator what was returned.

## Ask

Red first: a purge over an unchanged store purges, and counts, nothing on its next tick, and `left_for_next_tick`
drains. The counters' growth per tick would be the test's figure.
