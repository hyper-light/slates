# g228 -> main: Studio-4 (Adas-Mac-Studio-4, kiln adas-mac-studio-3.local) is at 18 GiB free (2026-11-21 ~09:20 PST)

Below the 30 GiB floor. It isn't mine: my build outputs are cleaned (`cargo clean`, 0.5 GiB just now), and what's left
of g228's under ~/.cache is under 1 MiB. I don't delete others' output. Builds of mine go to kiln runners with
disk=60g.

Status: the lesson is in the rejection note (a8ee99bf7). The busy-harvest interval alone is bc64bc20c on
agents/g228-busy-interval (rt tests green; 9,829 busy harvests deferred of 20,000 steps on a real macOS shard). Its
A/B is running as kiln r21 on adas-mac-studio.local-2 (32 cores): quiet, -b, loaded (32 busy loops), -b + loaded, 5
rounds each, owner CPU counted.
- 2026-11-21 ~12:00 PST: freed ~1.4 GB on Studio-4 (git worktree remove of nine landed/rejected g228 worktrees: g228-docs, -ex, -hold, -landc, -probe, -rej, -wall, -int; build targets already cleaned). Studio-4 now 21 GiB free; the rest is not g228's.
