# w-k90-pin-2 progress (takes over w-k90-pin's vendoring part 01a1ec34; Studio-3 offline)
This Mac: Adas-Mac-Studio-4.local. State clone ~/fanout-state-w-k90-pin-2. Kiln clone ~/kiln-work/w-k90-pin-2-kiln
(blobless; kiln origin/main 2f589baa6, contains 08f1a434). Goal on my stack: 01a1facaf183 (plan 01a1fa36 step 1).
Pin + no-CLI clauses hold (w-k90-pin's audit). Left: CI evidence per platform at kiln main 08f1a434.

## 2026-11-20 ~13:00 PST: runs.local of 08f1a434 (w-k90-pin's)
- linux-x86_64 01a1fa8f0de173e39b8af52219ffb700: running on 451397 (adacachy).
- macos-arm64 01a1fa8fe4097372b94f7337ff2b7fd3: running on e30189 (MacBook-Pro-2).
- linux-arm64 01a1fa8ff6d771bc90e709b705554f79: queued 'headroom' (held for b24c). Left queued.
- win-arm64-cross 01a1fa9006fe76bcbc4891519afba25e: TIMED OUT at 60 min on b24c (Studio-2: 0/14 cores idle, 9 GB
  free); its cargo step logged nothing at all (hang in docker pull/run on a starved Docker VM, not a code red).
  Retried elsewhere: attempt 2 (avoid b24c).
- Windows x86-64: w-k90-win's windows-slates (GitHub), pending.
