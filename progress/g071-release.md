# g071 -- progress (rewritten 2026-11-18 ~23:55Z from my own context after the ~/.cache wipe)
Agent: Claude subagent g071 of the slates supervisor (main), implementer. Base clone: ~/slates-work/g071-release.
THE RULE (final): ONE piece -> failing unit test first (red once) + fix + design/GAPS -> `cargo test -p <crate> --lib
<filter>` + `cargo fmt` + `cargo clippy --locked -p <crate> --all-targets -- -D warnings` + `cargo xtask check` ->
fetch, merge origin/main, `git diff --stat origin/main HEAD`, `git push origin HEAD:main` -> END. Builds on the Mac
Studio d1323898 (sync:false, GitHub clone of a pushed branch, own CARGO_TARGET_DIR under ~/.cache there).

## NOW: UI goal 85 (kiln 17ad60b6), piece A-490 = bug 5 (g267's finding in K3)
A Windows runner cannot join a slates fleet: kiln always trusts its CA as the fleet root, and slates' fleet manifest
reader then needs /etc/resolv.conf, which Windows lacks. Fix by a paired #[cfg] with one seam; failing unit test first
on the pure part; `cargo xwin check` clean plus macOS and Linux checks.
- [x] LANDED on main 2026-11-19 ~00:00Z: 22f9dc88c (a8ad21db9 fix + 963a93999 rustfmt reflow + 66283c8fc evidence +
  merge 22f9dc88c). Mac Studio, debug: red once on a copy where no adapter counts (failed at its first assertion); green
  `cargo test -p slates-api --lib role::fleet` 11/11 (before and after the last merge); fmt clean; `cargo xwin check -p
  slates-api --all-targets` + `cargo xwin clippy -p slates-api --lib --no-deps -D warnings` clean (x86_64-pc-windows-msvc);
  `cargo xtask unsafe` slates-api 9/9; clippy slates-server clean, slates-api only main's crates/api/tests/api.rs:1324
  and :1630; xtask structural ok, literals 45 findings none mine. Linux check not run (no Linux runner had room).
  Filed (run-requests 00:05Z): Windows native lib tests + clippy, Nano Server start (new iphlpapi import), a live
  Windows start on a manifest with a root, the Linux check. Test branch deleted; amendments.tsv A-490 landed.
  Worktree ~/slates-work/g071-a490 kept (branch agents/g071-a490-windows-manifest at 22f9dc88c).
- Not done in this piece (other changes', found on the way): clippy cognitive-complexity at crates/api/tests/api.rs
  1324 and 1630, and (Windows only) crates/server/src/shards.rs:1371 (12/10).
- Root cause: crates/api/src/role/fleet.rs `plan_from` asks `read_resolver` when an enrollment root is trusted or a
  node is named, and `read_resolver` knows only `/etc/resolv.conf`. Roots rightly need a resolver: an enrolled peer
  announces its address as a string `NodeAddress::parse` takes as a DNS name too (server discovery.rs `peer_of`).
- Fix (decided): seam `read_resolver()` paired `#[cfg(not(windows))]` (resolv.conf, as now) / `#[cfg(windows)]`
  (`role/fleet/windows_dns.rs`: GetAdaptersAddresses walk, bounded, each link checked inside the buffer) -> cfg-free
  `resolver_of_adapters` (Go's rule, net/dnsconfig_windows.go: adapters up with a gateway; IPv4 servers in order,
  each once; resolv.conf's defaults 5 s x 2 as Go takes them on Windows = KB 2834226's 10 s give-up for one server).
  Failing test first via mutation of a copy on the Studio. unsafe budget slates-api 5 -> 9 (4 sites).
- Committed a8ad21db9 on agents/g071-a490-windows-manifest (pushed as agents/g071-a490-test for the runs; delete it
  after). Runs: r35 Studio (fmt, red on a mutated copy, green role::fleet, clippy api+server, xtask check, xwin
  check+clippy; logs kept at Studio ~/.cache/g071-a490-logs/run1); r37 linux/amd64 `cargo check -p slates-api
  --all-targets` (capped on adacachy). Then: evidence commit, merge origin/main, diff --stat, push to main.

## UI goal 85, piece A-507 (g264's finding): an unmount runs no program by name -- LANDED
- [x] main 088d38779 (43166ce14 fix + 17aa331ab rustfmt/evidence + merges) and d237f56cc (unsafe budget slates-api
  9 -> 10 for the `unmount(2)` block, missed in the first landing). macOS and the BSDs call `unmount(2)` (`MNT_FORCE`
  for unmount_busy); Linux keeps `fusermount3 -u` (its mount's setuid helper; umount2(2) needs CAP_SYS_ADMIN); both
  judged by `slates_bridge_oci::mount_table` before and after (NotMounted before any call; new MountFault::Busy,
  StillMounted, MountTable); CONTRACT = 42. Mac Studio, debug: red once on a mutated copy (mapping + request tests
  failed); green `cargo test -p slates-api --lib -- mount:: contract_history` 12/12 at 43166ce14. Rule since 2026-11-18
  evening: no clippy/xtask/xwin/Linux checks by agents (main's). Filed (run-requests 00:25Z): the macOS bare-metal e2e
  with a runner PATH lacking /sbin, the SLATES_TEST_MOUNT kernel-mount test, the Linux build and lints.
  Worktree ~/slates-work/g071-a507 (branch agents/g071-a507-unmount at d237f56cc). Owed (GAPS): naming the processes
  that hold a busy mount; Linux's busy refusal typed.

## HELD until goals 84-90 are done: UI goal 82/80 "release pipeline" (kiln 6b5f1ad3) + "v0.1.0 published" (01a1af51)
Landed (all on main): A-357 Windows binary starts in Nano Server (81f77f921 via dc463dea1); CI's Nano Server step +
.github/actions/nano-server-start (3f2011cc2); A-407 a pair from two runner images refused naming both (73355c3cf
via 79834c4ff); A-411 MSVC toolset 14.44 + SDK 10.0.26100.0 pinned (8a99b919f via 8757f0303); A-413 cargo-binstall
metadata + `cargo xtask version` holds it to ASSETS (69b94cbeb + 8ca6a6440 via 205b5b1e5; xtask version::tests 13/13
+ fmt clean on the Studio, twice).
Left: (6) draft notes say the binaries are unsigned + `chmod +x`, `xattr -d com.apple.quarantine slates-macos-arm64`,
Windows `Unblock-File` (where: xtask/src/release.rs `notes()`, a const after PLATFORMS + a test resolving the assets from
ASSETS; docs/publish.md staging list + "ships unsigned" bullet, design B.2, GAPS release entry, new amendment; the
Apple/Microsoft facts are from memory: verify first). (7) post-publish record (README install from the release,
docs/publish.md, GAPS, B.2, Phase 9, §4.12) after main publishes. (8) main: finish v0.1.0 (tag at dc463dea1; release run
44347535833 attempt 4 failed 19:11Z on AC-9.5 x64 pair across two runner images; re-run a copy until both share an
image, verify, `gh release edit v0.1.0 --draft=false --latest`). (9) Ada, list below.
Run requests: re-filed 2026-11-19 00:05Z after the wipe (the v0.1.0 finish, A-411's dry run across two images, A-413's
first binstall after the publish).

## For Ada (exactly what she types; the SDK publish needs her logins)
1. pypi.org -> Your account -> Publishing -> Add a new pending publisher: project `slates`, owner `hyper-light`,
   repository `slates`, workflow `publish-python.yml`, environment `pypi`.
2. In a slates checkout: `npm login`, then
   `OUT=~/.cache/slates-npm-stubs && cargo xtask npm-reserve --out "$OUT" && for dir in "$OUT"/*/; do (cd "$dir" && npm publish --access public); done`
3. npmjs.com, each of the 8 packages (@hyper-light/slates + 7 platform packages) -> Settings -> Trusted publisher ->
   GitHub Actions: organization `hyper-light`, repository `slates`, workflow `publish-node.yml`, environment `npm`.
4. When main says the v0.1.0 binaries are published: `gh run view 44347535708 --repo hyper-light/slates --web` and
   `gh run view 44347535572 --repo hyper-light/slates --web`, "Review deployments" -> approve each (waits expire
   2026-12-18 16:28Z; then `gh run rerun <id> --failed`).
