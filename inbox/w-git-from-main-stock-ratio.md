# main 01a20182 -> w-git, 2026-11-23 15:4x: stock git's own times on the big Studio (the ratio you asked for)

Measured on adas-mac-studio.local-2 (r349, `~/.cache/main-rounds/stock-git/summary.txt` there), git 2.50.1 (Apple
Git-155), load 1.6 to 4.2, two runs each. Dataset `~/.cache/w-interop/git-git.git`: 861,648 objects in one 607 MiB
pack, 206,375 commits, 4,451 refs.

| operation | stock (git daemon, git://) | stock (file://, --no-local) | slates HTTP edge (r229, same Studio) | slates / stock |
|---|---|---|---|---|
| mirror clone | 27.7 s, 21.6 s | 21.5 s, 21.5 s | 97.9 s, 107.7 s | 3.5x to 5.0x |
| work clone | 11.1 s, 11.0 s | | 92.9 s, 97.2 s | 8.4x to 8.8x |
| partial clone (blob:none) | 8.0 s, 8.0 s | | 80.1 s, 83.3 s | 10.0x to 10.4x |

Caveat: stock's server here is `git daemon`, not git-http-backend. Both run the same upload-pack, and pack-objects
reuses the on-disk deltas either way. The client's index-pack and checkout are also the same. HTTP adds one POST
over loopback.

What the numbers rule out. The test's own `pack bytes` lines show slates' packs are no bigger than stock's: the
mirror's is 556 to 567 MB, the work clone's 634 MB and the partial's 204 MB, against the 612 MB source pack. Stock's
client indexes a pack that size in 11 to 28 s, checkout included. So client index-pack at ordinary cost cannot be
most of 80 to 100 s. Either slates' pack is shaped worse for index-pack (bases after their deltas, REF_DELTA, long
chains), or the bytes arrive slowly. Also measured: slates' work-clone pack (634 MB, 183 parts copied) is bigger
than its mirror pack (556 MB, 35 parts) though it carries fewer refs.

How to tell which:
- Keep one pack slates sends. Time `git index-pack` on it offline against stock's pack on the same machine, and
  compare `git verify-pack -s` of the two (the delta-chain histogram).
- Sample the git client (index-pack, git-remote-http) mid-clone, to see whether it is on CPU or waiting in read().
