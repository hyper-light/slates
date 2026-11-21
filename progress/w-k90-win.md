# w-k90-win progress (kiln goal 90, Windows parts)

## 2026-11-20 ~09:30 PST start (Adas-Mac-Studio-2, b24c; pushes over SSH)
- State folder: ~/fanout-state-w-k90-win-s (shallow https clone, push over ssh). The full SSH clone at
  ~/fanout-state-w-k90-win timed out half-made (only .git): for main to delete.
- Also for main to delete: ~/kiln-work/w-k90-win-kiln (blobless kiln clone whose checkout failed; promisor fetch broken).
- kiln worktree: ~/kiln-work/w-k90-win (branch agents/w-k90-win, from ~/Projects/kiln, fetched over https), base ed8991d29.
- Found: the bare_metal drive's Windows code already landed (b7720ebca, cfg(any(unix, windows)), "built by nothing
  here"). Part 1 is now: build it on Windows, run it (needs WinFsp + Python 3), fix what breaks.
- kiln GitHub CI on ed8991d29 (run 44758164143): windows-latest / windows-11-arm Clippy fail (windows-latest exit 101 =
  compile error); every lane's clippy fails too. Windows lanes only clippy+build; no Windows test lane runs kiln's tests.
- Plan: (a) jupiter: cargo build + `cargo test --no-run` of kiln (AWS_LC_SYS_PREBUILT_NASM=1, own target off C: if a
  drive has more room); fix Windows compile errors; run the no-mount tests (slates_e2e checks, workspace libs).
  (b) GitHub CI: a Windows test lane installing WinFsp (as slates' CI does) running slates_e2e bare_metal with
  SLATES_E2E=1, plus kiln's Windows-relevant tests.

## 2026-11-20 ~10:15 PST: jupiter blocked on disk; Windows lane landed in GitHub CI
- jupiter: ONE drive, C: 4.9 GB free / 919.4 GB used (Get-PSDrive); no Python 3, no WinFsp. No kiln build possible
  there until space is freed (not mine to delete). Reported in summary to main.
- kiln's own (.kiln) CI has no Windows lane; GitHub's ci.yml is kiln's CI Windows lane. kiln has no GitHub provider
  linked in kiln (ci.providers: kiln only); GitHub logs need a sign-in (no gh/token here) -> results via annotations.
- LANDED kiln main 842cc2c2f (is-ancestor confirmed, base 6965eaf63): ci.yml job `windows (Slates tests)` (WinFsp 2.1
  pinned + MountUseMountmgrFromFSD, Python, `SLATES_E2E=1 cargo test -p kiln --test slates_e2e`, `--lib -- workspace::`,
  annotations) + `build` matrix reports clippy/build errors as annotations; docs/agentic.md W line.
- WATCH: GitHub run 44782069370 (842cc2c2f). Next: read its annotations; fix Windows compile/test failures in kiln.

## 2026-11-20 ~10:00 PST: WinFsp installed on jupiter (Ada's word via main, "Yes, install WinFsp on jupiter")
- Installed WinFsp **2.1.25156** (winfsp-2.1.25156.msi, release v2.1), NOT the latest v2.2 (2026-11-19): Slates'
  receiver accepts only the 2.1 DLL (vendor/slates/crates/bridge-winfsp/src/dll_version.rs; Slates CI pins it, A-206).
  SHA-256 073a70e0...db9f7a = GitHub's published digest = Slates' pin; Authenticode Valid (CN=NAVIMATICS LLC).
  msiexec /qn exit 0 from kiln run r8 (runner already elevated: Jupiter\corph, session 0) -> NO UAC prompt.
  winfsp-x64.dll FileVersion 2.1.25156.ddca7bd; WinFsp.Launcher RUNNING; set MountUseMountmgrFromFSD=1 (as Slates CI).
  My files: C:\Users\corph\w-k90-win\ (msi, install log, script).
- Verify-by-use BLOCKED on disk: C: 5 GB free; no slates daemon binary on jupiter (only other agents' test exes); a
  slates-server + kiln build won't fit. Asked main for space.
- GitHub: run 44782069370 cancelled by newer pushes; run 44784299374 (0a17f46e5) carries `windows (Slates tests)`
  job 136462100187 and windows-latest build 136462100146 -- watching.

## 2026-11-20 ~11:10 PST: kiln builds + clippy clean on Windows -- LANDED da441f112
- jupiter C: freed by main (r865): 309 GB free. My ONE target: C:\Users\corph\w-k90-win\target; clone ...\w-k90-win\kiln.
- r12 (jupiter, 83a03cd1): kiln lib did NOT compile on Windows: workspace/slates.rs:1231 `unmount_busy` (cfg(not(windows))
  in Slates) from fa86489f5. GitHub job 136462100146 (run 44784299374, 0a17f46e5) also showed clone_on_copy + redundant
  closure (annotation capped at 4096 chars).
- Fix: let_go_busy cfg'd (Windows keeps the volume, MountStays + context), clippy fixes/allows. r18 (jupiter, tree
  8462b0b5 = fix on 715063153): `cargo clippy -p kiln --all-targets --locked -- -D warnings` exit 0.
- LANDED kiln main da441f112 (is-ancestor confirmed; main was 715063153).
- Running r19 (jupiter): kiln.exe build + `KILN_TEST_KILN=... cargo test -p kiln --lib -- workspace::bare_metal_tests`.
- jupiter has NO Python 3: slates_e2e bare_metal drives skip without it (NO_PYTHON, even with SLATES_E2E=1). Python is
  an install -> asked main for Ada's word. GitHub windows-latest has Python (job `windows (Slates tests)`).
- w-k90-pin added `slates` job (715063153) on ubuntu/macos/windows-11-arm; mine (`windows (Slates tests)`) is windows-latest.

## 2026-11-20 ~12:30 PST: by-use on jupiter -- RED in Slates' channel on Windows (before any mount)
- r19 (jupiter, kiln main da441f112): kiln.exe built (12m21s); `KILN_TEST_KILN=...\kiln.exe cargo test -p kiln --lib --
  workspace::bare_metal_tests` (lib tests built 29m) FAILED: "Slates' anchor ended before its instance answered: the host
  refused: the channel's shared memory refused: shared layout refused at 0 for 64 bytes: TouchesWord".
- r24 RUST_BACKTRACE=1: from kiln::workspace::slates::up_or_started <- ensure_up <- make_as <- workspace::prepare, i.e. the
  first attach to a fresh instance (kilntest<pid>), no mount yet. Anchor log: daemon started, 2 shards, served normally.
- Reading: "host refused" = the daemon's refusal relayed at rendezvous; 64 bytes at 0 matches slates-ipc
  ClientRegion::create's fixed-header write (region.rs:303, FIXED_HEADER_BYTES=64) -> on Windows something declares a
  word in [0,64) or the check misfires. Not found by reading (REGION_WORDS start at 64; Layout::touches looks right).
- Slates CI gives no Windows evidence (every main run cancelled by newer pushes).
- NEXT: r25 on jupiter: slates main clone C:\Users\corph\w-k90-win\slates, `cargo test -p slates-ipc --lib -- region
  endpoint` (same one target). Red there -> Slates bug: fix in slates (failing test first), then w-k90-pin re-pins.
- kiln server dropping runs often (r13, r22, r25/r26 lost).
- GitHub run 44784299374: windows-latest clippy job 136462100146 failed (fixed by da441f112); slates_e2e step failed
  (expected: kiln lib didn't compile on Windows then).

## 2026-11-20 ~14:15 PST: WinFsp mount by use GREEN on jupiter; Windows runner MCP gap found and being built
- r29 (jupiter, kiln main 64412a67 incl. cc1ecb625's A-717 carry, WinFsp 2.1.25156): `KILN_TEST_KILN=...\kiln.exe
  cargo test -p kiln --lib -- workspace::bare_metal_tests` -> 1 passed (w2_every_harness_and_agent_works_on_a_volume_
  reviews_drifts_and_lands_under_a_grant), 37.7 s. A Slates volume mounted through WinFsp on jupiter, by use.
- GitHub run 44807509237 (a82ce7ede, windows-latest): windows-latest clippy PASSED; bare_metal_tests ok there too;
  slates_e2e bare_metal drives FAILED at landing: "LAND_FAIL no kiln MCP server was given to this harness" (mount at Z:,
  edits, change request all PROVEN). Cause: kiln gives a Windows runner's process harness no kiln MCP (reach socket is
  Unix-only; docs/reach.md §9 "Not yet"). Also some other workspace:: lib test failed there (annotation truncated).
- Building: reach::loopback (kiln, branch agents/w-k90-win-mcp1 abe073d89): runner listens on 127.0.0.1:0, serves each
  connection as the session's lane (reach::mcp::serve_lane) after a per-session token hello; `kiln reach mcp` on
  Windows pipes stdio over it (KILN_REACH_ADDR/KILN_REACH_TOKEN); Claude's --mcp-config is a user-only file in
  %TEMP%\kiln-mcp. Mac: 4/4 new tests green. r31 (jupiter): Windows clippy + reach::loopback tests.
- Python on jupiter still needed for the e2e drive there (inbox/main-from-w-k90-win.md).

## 2026-11-20 ~14:50 PST: Python on jupiter (Ada's word via main, ~11:45 PST)
- Installed CPython **3.15.2** (current stable; 3.16 is alpha), python.org python-3.15.2-amd64.exe, SHA-256 06e225a2...
  = python.org's published; Authenticode Valid (CN=Python Software Foundation). r36: /quiet InstallAllUsers=0
  TargetDir=C:\Users\corph\bin PrependPath=0 Include_launcher=0, exit 0, NO UAC. python3.exe = copy of python.exe.
  C:\Users\corph\bin is already on every kiln run's PATH there (no runner restart): `python`/`python3` -> 3.15.2.
  Told w-k90-whp (inbox/w-k90-whp-from-w-k90-win.md). My files: C:\Users\corph\w-k90-win\python-*.
- Found: a per-user Python 3.13.17 installed 2026-11-19 23:25 (someone's), registry PATH only (runner started before it).

## 2026-11-20 ~17:00 PST: Windows runner MCP (reach::loopback) iterating on jupiter
- r37 (jupiter, tree c812123f = main c0f576eb3 + loopback): reach::loopback 4/4 green on Windows; Windows clippy RED
  only at main's new workspace/container.rs:299 rt::sleep (2f589baa6) -> allowed as kind (c) in my branch.
  slates_e2e bare_metal (SLATES_E2E=1, Python 3.15.2): drives reach landing; LAND_FAIL changed from "no kiln MCP server
  was given" to "no answer to initialize": `kiln reach mcp` bailed on Windows in reach/cli.rs ("reach needs a Unix
  machine for now") -> fixed (3a1fde52d, branch agents/w-k90-win-mcp3).
- r42 (jupiter): e2e bare_metal then clippy on mcp3. Then land (one commit: loopback + cli + allows + ci report + docs).

## 2026-11-20 ~16:30 PST: CLOSE ORDER -- my list: C1 goal 90e (slates_e2e bare_metal on Windows)
- LANDED kiln main 4af73427b (base 648c57184, is-ancestor confirmed): reach::loopback (Windows runner harnesses get kiln's
  MCP over 127.0.0.1 by per-session token), kiln reach mcp runs on Windows, container.rs:299 wait allowed (kind c),
  Windows lane Report failures-first, docs reach.md §9 / agents.md. Tests: reach::loopback 4/4 macOS + Windows (r37).
- Trimmed: deleted ~/.cache/w-k90-win-local-target.noindex (main's ask, Studio-2 disk).
- r2 (jupiter, main 4af73427b+): SLATES_E2E=1 slates_e2e bare_metal, then Windows clippy. Watching. Also GitHub ci.yml
  run on 4af73427b's descendants: `windows (Slates tests)` job.
- For main to delete: ~/fanout-state-w-k90-win (half clone), ~/kiln-work/w-k90-win-kiln (broken blobless clone).

## 2026-11-20 ~20:20 PST: C1 CLOSED (inbox/main-from-w-k90-win-close.md)
- jupiter r3 at main eb7194f00: slates_e2e bare_metal 2/2 passed (93 s), WinFsp 2.1.25156, Python 3.15.2.
- GitHub run 44851246181 job 136703754349 (4f5e58c1b): slates_e2e 25/25 incl. both bare_metal drives.
- mod bare_metal is cfg(any(unix, windows)).
- Follow-up in flight: workspace::tests w2/w7 '/'-joined expectations fail on Windows (98/100) -> fb3460e9e, jupiter r4.

## 2026-11-20 ~21:45 PST: C1 met in kiln (goals_done 01a1fa369a1c76f99a79a44a1fcf1f98); follow-up state
- Finding (not C1): kiln workspace::tests w2_/w7_ hard-code Unix paths, fail on Windows (CI job 136703754349 98/100).
  Fix on branch agents/w-k90-win-wst2 9ebc11e5d (host-spelled paths). jupiter r5 (first version): w2 passes on Windows,
  w7 failed because '/Users/..' isn't absolute on Windows (rehomed correctly skips it) -> 9ebc11e5d gives w7 drive-letter
  paths there. Its jupiter run (r7) was lost to the kiln server dropping; not yet green on Windows, not landed.
  Next: re-run `cargo test -p kiln --lib -- workspace::tests::` on jupiter at wst2, then land as its own commit.

## 2026-11-20 ~21:45 PST: w2/w7 test follow-up LANDED f16212799 (main 492af26f9)
- macOS arm64 Studio-2 local: workspace::tests:: 11/11. Windows run lost to kiln server; GitHub run 44872271307
  (492af26f9) pending -> watching its `windows (Slates tests)` job; send w-k90-pin-2 (01a1facadebb) the green run id.
- Re-created ~/.cache/w-k90-win-local-target.noindex for this test (Studio-2 117 GB free).
