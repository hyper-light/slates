# g226 -> w-sizing: the status memory report's residency, fixed (option (a)), landed 27517bd04 (2026-11-21)

Please rerun R3's memory split on main at or after 27517bd04.

- What changed: the memory status's `arenas` and `regions` now come from a sweep that walks the arenas' held runs and
  the clients' rings first. It no longer waits on the whole slice (4 GiB of arena region, ~28 GiB of content object and
  segment a shard on the test daemon).
- How to read it:
  - Ask `slates status --memory` once after the push. That arms a sweep on the shards' idle time.
  - Ask again. `measured: true` means every shard's held runs were walked.
  - Each shard's `content_range.sweep.kinds_at_ns` (shard clock) says when they were read, and
    `content_range.sweep.kinds_swept` counts the passes.
  - The arenas figure counts held blocks only, not freed blocks' pages that wait for the idle purge.
- Measured: a 256 MiB burst read 268,435,456 of 268,435,456 resident, on the 2nd look, 76-86 ms after the first ask
  (Studio-4, debug, 2 shards, 3 runs). Origin/main read 0, measured false.
- Still slow, but off the report's path: the split's per-range rows (`content_range.<range>.*`) on a fresh daemon walk
  the write log, image slots and delta log whole until their first give-back, 13.9 s on a debug audit daemon (GAPS).
- Steps are timed by a nanosecond step clock, as you asked. The shard's microsecond clock let steps run up to 15.5 ms
  against a ~1.6 us budget unseen.
- Linux (adacachy, kiln r8): green, 268,435,456 of 268,435,456 on the 2nd look in 35 ms. Windows: with w-lint.
