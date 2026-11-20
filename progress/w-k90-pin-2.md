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

## ~13:40 PST
- macos-arm64 01a1fa8fe409 attempt 1 FAILED on e30189 (MacBook-Pro-2): ENOSPC while compiling kiln lib (infra, not
  code). Retried elsewhere: attempt 2 (avoid e30189). Didn't add `disk:` to resources: kiln's debug target is tens of
  GB and only ~2 Macs have 40 GB free now, so a disk request could park the lane.
- win-arm64-cross attempt 2 queued 'headroom'; only candidate 756947 (adalundhe-MacBook-Pro) has 80 MB disk free
  (for main: that machine's disk is full).
- GitHub run 44807509237 (a82ce7ede, contains 08f1a434): ubuntu-latest + ubuntu-24.04-arm `slates` Build killed
  (exit 143); windows-slates (w-k90-win) red in slates_e2e + lib steps -> inbox/w-k90-win-from-w-k90-pin-2.md.
  macos-latest / windows-11-arm slates still running then. api.github.com rate-limited (60/h shared).

## ~14:50 PST
- GitHub run 44807509237 (a82ce7ede, contains 08f1a434), per job:
  windows-11-arm `slates` 136537837042: build 62 min OK, `--lib workspace::slates` OK (52 min), integration step stopped
  by timeout-minutes 120. macos-latest `slates` 136537836996: build 98 min, stopped in lib tests. Ubuntu slates both
  lost the runner ~23 min into the build (exit 143). windows-11-arm (clippy, build) 136537837066 GREEN; windows-latest
  (clippy, build) 136537837057 GREEN. macos-latest/ubuntu-arm clippy red (deprecated fetch_update in vendor/slates under
  -D warnings; for main's lint keeper). linux (clippy, tests) lost its runner.
- LANDED kiln c0f576eb3 (merge-base confirmed; base c0f576eb3): GitHub `slates` job timeout 240, "Room on disk (Linux)"
  step (rm the image's dotnet/android/ghc/CodeQL). Validated (ci.workflows.validate: 0 diagnostics).
- kiln lanes: linux-x86_64 leg 01a1fa8f0de8 'running' but waits 'capacity' on 451397 (steps pending ~2.5 h);
  macos-arm64 attempt 2 running on d13238; linux-arm64 + win-arm64-cross queued 'headroom'.
- Worktree ~/kiln-work/w-k90-pin-2-land (landed; remove when done).

## ~16:10 PST
- LANDED kiln 5264caaa3 (merge-base confirmed; base 5264caaa3): build job's Report annotates `error` blocks first
  (run 44825381107, 63da2c592: windows-latest + windows-11-arm (clippy, build) red exit 101, annotation full of
  vendor/slates fetch_update warnings, error unseen). Validated 0 diagnostics.
- GitHub: run on c0f576eb3 (44830441910) pending behind 44825381107 (63da2c592, old 120-min slates timeout); the next
  started main run carries both fixes.
- kiln lanes WEDGED ~4 h: linux-x86_64 leg 01a1fa8f0de8 and macos-arm64 attempt-2 leg 01a1fae6520e both 'running' with
  every step pending (no checkout); mac leg's routing says held for cec37f (room=true, room_at long past) yet never
  starts. linux-arm64 + win-arm64-cross queued 'headroom'. Told main (scheduler, needs a person's look).
