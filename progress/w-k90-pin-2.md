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

## ~16:20 PST
- 44825381107 (63da2c592) done: Windows (clippy, build) red in the Clippy step only (Build ran); windows-11-arm slates
  cancelled at 120 in the integration step again; macos-latest slates cancelled in lib tests. Old timeout.
- Watching GitHub run 44841298533 (4aa0ee778, contains 5264caaa3 + c0f576eb3 + 08f1a434): poller
  ~/kiln-work/w-k90-pin-2-logs/poll.sh 44841298533 (background).
- kiln legs still wedged (no change in another 25 min).

## ~16:55 PST
- 44841298533 (4aa0ee778): ubuntu-24.04-arm slates build ended exit 143 at ~22 min even with the disk step -> memory
  (systemd-oomd on a 16 GB runner). LANDED kiln e763e394f (merge-base confirmed, base e763e394f): Linux lanes stop
  oomd, 16 GiB swap, CARGO_BUILD_JOBS=2. Validated 0 diagnostics.
- Still watching 44841298533's macos-latest / windows-11-arm / ubuntu-latest slates and w-k90-win's windows-slates.

## ~19:00 PST (03:00Z)
- GREEN macOS arm64: GitHub run 44841298533 (kiln 4aa0ee778, contains 08f1a434's vendor/slates pin), job 136653837819
  `macos-latest (Slates build and tests)`: build 39 min, `--lib workspace::slates` 73 min, `--test vendored --test
  slates_roles` 24 min, all success.
- windows-11-arm job 136653837913: build OK, lib Slates tests OK, `vendored`/`slates_roles` FAILED exit 101 (no readable
  detail). LANDED kiln eb7194f00 (merge-base confirmed, base eb7194f00): slates job Report step annotates failed tests.
  Repro started: kiln run r145 (needs windows: jupiter, own target ~/.cache/w-k90-pin-2-target) `--test vendored --test
  slates_roles`; queued (jupiter link down).
- ubuntu-latest/ubuntu-24.04-arm on 4aa0ee778 red (before e763e394f's memory fix).
- Clippy red on Windows/Linux (all clippy legs): crates/kiln/src/workspace/container.rs:301 `crate::rt::sleep` is a
  disallowed method (A-520 piece 5, w-k90-containers'). For main / lint keeper.
- Poller bebez30pl stopped at its 2 h limit.

## ~21:45 PST (05:45Z)
- GREEN Linux x86-64: GitHub run 44851246181 (kiln 4f5e58c1b, contains e763e394f + the 08f1a434 pin), job 136703754443
  `ubuntu-latest (Slates build and tests)`: every step success. macOS arm64 green again there (136703754428).
- ubuntu-24.04-arm 136703754445: build ran 90 min (oomd off, swap, -j2), exit 101, no detail; its (clippy, build) leg
  lost the runner (143). LANDED kiln f500759d2 (base f500759d2): slates Report covers the build step.
- windows-11-arm 136703754464: build OK, lib Slates tests OK (51 min), `--test vendored --test slates_roles` exit 101
  again (slates_roles is #![cfg(unix)] -> the `vendored` test or the test build fails on Windows arm64). Report step
  (eb7194f00) not in that run; next run 44867288588 (af73099bd) carries it.
- windows-slates (w-k90-win) on 4f5e58c1b: slates_e2e GREEN; lib: 2 tests with hard-coded Unix paths (mod.rs:747/764)
  -> told w-k90-win (agents_send + inbox).
- clippy container.rs:301 -> w-k90-containers says it's w-k90-a520's, forwarded.
- r145 (jupiter repro) still queued: jupiter not answering. kiln's own lanes for 08f1a434 still wedged.

## ~00:30 PST 11-21 (08:30Z)
- Run 44867288588 (af73099bd): ubuntu-latest GREEN again (136745995680). ubuntu-24.04-arm build failed at kiln's final
  link ("linking with `cc` failed: exit status: 1", reason cut by the 4 KiB cap). LANDED kiln 482bd01f8 (base 482bd01f8):
  Linux arm64 links with the toolchain's gcc-ld/ld.lld when present; Report shortens >300-char lines.
- windows-11-arm 136745995679: build + lib Slates tests green; `vendored` 2 of 11 red: a_vendored_crate_taken_from_git
  ("the planted source isn't in the lock") and an_unrelated_crates_io_crate ("quiver isn't in the lock as kiln's"):
  CRLF Cargo.lock on Windows checkouts vs "\n" patterns. Fix in worktree: kiln_lock() normalizes \r\n. Testing:
  kiln run r2 (d13238) `cargo test -p kiln --test vendored`, own target ~/.cache/w-k90-pin-2-target.noindex.
- Note: my code_run sleep loop spun 2000 calls when machines.run failed fast during kiln's restart (now shell waits).

## ~01:15 PST 11-21 (09:15Z)
- LANDED kiln 60c0b27c5 (merge-base confirmed, base 60c0b27c5): tests/vendored.rs kiln_lock() normalizes \r\n.
  Local (this Mac, own target): `cargo test --locked -p kiln --test vendored` 11/11 with LF lock and with a CRLF lock.
- kiln restarted twice on this Mac; runs r2 (d13238 went silent), r4 lost. Local shell used instead.
- State per platform (GitHub ci.yml `slates` job): macOS arm64 GREEN (44841298533/136653837819), Linux x86-64 GREEN
  (44851246181/136703754443; again 44867288588/136745995680). Linux arm64: lld link fix 482bd01f8 awaiting a run.
  Windows arm64: CRLF fix 60c0b27c5 awaiting a run. Windows x86-64: w-k90-win (2 Unix-path tests in workspace::tests).

## ~03:10 PST 11-21 (11:10Z)
- Run 44887857088 (kiln a0d114abb: contains 08f1a434's pin, 482bd01f8 lld, 60c0b27c5 CRLF, w-k90-win's f16212799):
  GREEN Windows x86-64 `windows (Slates tests)` 136804151233 (w-k90-win's job); GREEN Linux arm64 `ubuntu-24.04-arm
  (Slates build and tests)` 136804151244 (lld link; build 37 min). ubuntu-latest lost its hosted runner (infra; Linux
  x86-64 already green twice). windows-11-arm in lib tests; macos-latest still building.

## ~04:25 PST 11-21: DONE
- GREEN Windows arm64: run 44887857088 job 136804151286 (a0d114abb; vendored 11/11 with the CRLF fix).
- Goal 01a1facaf183 marked met (goals_done) with: macOS arm64 44851246181/136703754428, Linux x86-64
  44851246181/136703754443 (4f5e58c1b); Linux arm64 136804151244, Windows x86-64 136804151233, Windows arm64
  136804151286 (44887857088, a0d114abb). Pin 8ce3d61523e1 in all. No CLI call. Reported to main (inbox).
- For main to cancel (needs a person): kiln ci runs 01a1fa8f0de173e39b8af52219ffb700, 01a1fa8fe4097372b94f7337ff2b7fd3,
  01a1fa8ff6d771bc90e709b705554f79, 01a1fa9006fe76bcbc4891519afba25e (wedged), and r145 (jupiter repro, unneeded now).
- For main to delete: ~/kiln-work/w-k90-pin-2-kiln (scratch clone), ~/.cache/w-k90-pin-2-target.noindex,
  ~/kiln-work/w-k90-pin-2-logs.

## ~05:00 PST 11-21: C1 CLOSED (inbox/main-from-w-k90-pin-2-close.md). R2 (runner link closing) root cause found
- adacachy enp9s0: 192.168.1.77/24 primary (static, not NetworkManager's: DHCP-only profile) + 192.168.1.90/24
  secondary (DHCP). kiln quic_advertise 192.168.1.90:7071, bound 0.0.0.0:7071; LAN route src .77.
- kiln's version probe from Studio-4 to .90:7071: answered 5/5 FROM .77 (scripts ~/kiln-work/w-k90-pin-2-scripts/vnprobe.py).
- => direct QUIC answers come from the wrong address -> runners fall to the HTTP tunnel and its 5-min UDP check
  (tunnel.rs udp_answers: from == to) never passes -> all LAN Macs stay tunnelled; Macs reach adacachy on 2.4 GHz Wi-Fi
  (Studio-4 en1 ch9 -69 dBm, Ethernet unplugged; cec37f60 .84 logs "Host is down" every few min): each blip kills the
  tunnel's TCP -> QUIC ends ~30-90 s later -> "agent disconnected reason=closed". 192.168.1.93's tunnel: opened every
  ~16.3 min, its QUIC gone within ~1 min, tunnel closed idle 600 s later (f3f618's cycle).
- Ruled out: load (server's 7071 socket 0 drops since start; 0.7% host-wide UDP drops are other sockets), ARP table
  (109/1024), upgrades (1 of ~20 drops). Tunnel is Class::Background in overload.rs (noted, not the cause here).
- Operational fix = a person on adacachy (drop .77 or route src .90). kiln fix = IP_PKTINFO in vendored hyper-rt +
  hyper-transport (passes local_ip None at endpoint.rs:1037): asked main whether that's R2's fix.
- Reported: inbox/main-from-w-k90-pin-2.md (~04:50).

## ~06:30 PST 11-21: R2 fix (main approved IP_PKTINFO, 04:36)
- Worktree ~/kiln-work/w-k90-pin-2-r2 (base origin/main a0d114abb), local commit (unpushed, to amend).
- kiln-rt (kiln's own hyper-rt; vendor/hyper-raft/crates/hyper-rt stays upstream's, nothing kiln builds uses it):
  linux.rs enable_local_addresses (IP_PKTINFO + IPV6_RECVPKTINFO), Received::to from cmsgs, write_controls (UDP_SEGMENT
  + IP_PKTINFO/IPV6_PKTINFO src), CONTROL_WORDS 15; macos.rs same + sendmsg send_from; batched.rs Arrival::to,
  send_segments_from. ORIGIN.md row.
- vendor hyper-raft: hyper-transport Endpoint::handle_datagram_to (local_ip in place of None at endpoint.rs:1037);
  hyper-rt-transport driver passes arrival.to and sends from transmit.src_ip. Recorded by hand as
  vendor/patches/hyper-raft/0006-* (vendor.py needs Python 3.11; this Mac has 3.9) + UPSTREAM.md row.
- Windows: only runs client endpoints (src_ip None) -> portable send, unchanged.
- Test: crates/kiln/src/tunnel.rs r2_a_server_on_a_host_with_two_addresses_answers_from_the_one_it_was_reached_at
  (server endpoint bound 0.0.0.0; udp_answers at 127.0.0.1 [Linux+macOS] and 127.0.0.2 [Linux]).
- macOS: cargo check -p kiln --lib --tests OK (local). Runs queued: r17 Linux RED (fix reversed) then GREEN;
  r18 macOS test. vendor.py check hyper-raft to run where Python >= 3.11.
- R2 BEFORE (kiln machines.list topo, 2026-11-21T14:59:53Z): drops / rtt / spread ms:
  Studio-4 2768 135/214/369; Studio-3 cec37f 485/225/21; Studio-2 b24c 166/692/3934; Studio d13238 109/113/437;
  MBP-2 e30189 167/13/39; jupiter 55/8/86; adacachy 9/0/0. Second sample later -> drops/hour before the fix.
- r17 stopped (its red half diffed the working tree, empty after the local commit); r19 = red/green on Linux by
  HEAD~1..HEAD, + kiln-rt udp tests + vendor.py check hyper-raft.
- R2 BEFORE, better measure: kiln's topo.drops did NOT count Studio-4's 15:34:40Z drop (135 at 14:59 and 15:50), so the
  measure is the runners' own "disconnected from server" lines per hour (UTC, 2026-11-21):
  Studio-4 (.83): 05h 4, 06h 1, 07h 1, 08h 2, 09h 4, 10h 2, 15h 1 (none 11-14h).
  Studio-3 cec37f (.84): 00h 24, 01h 8, 02h 2, 04h 2, 05h 5, 06h 3, 07h 1, 08h 3, 09h 4, 10h 3, 11h 3, 15h 1.
  cec37f has been tunnelled since 2026-11-16T04:54Z ("UDP 192.168.1.90:7071 unreachable"). Probe at 15:50Z: still from .77.
  AFTER = same counts once the fix is deployed (server + runners), plus each runner's "connected directly" line.
- ~08:45 PST: macOS green (this Mac, local, own target): crates/kiln-rt/tests/udp_local_address.rs
  an_arrival_tells_its_local_address_and_an_answer_from_it_arrives 1/1 x4 (sendmsg+IP_PKTINFO answer arrives);
  udp_batched 18/18 x13 (10/10 clean); hyper-rt --lib udp:: 13/13; cargo check -p kiln --lib --tests OK.
  CAUTION learned: two worktrees on one CARGO_TARGET_DIR mixed kiln-rt artifacts (a test compiled against origin's
  lib) -> base worktree removed; never share a target across worktrees.
- Linux runs waiting on adacachy (load ~40/32, "machine under strain"; fedora offline): r19 = kiln e2e RED/GREEN
  (32g), r21 = kiln-rt tests + vendor.py check (6g). r18 = kiln e2e on macOS (queued for this Mac).
- Local commit (unpushed) in ~/kiln-work/w-k90-pin-2-r2: 10 files (+685/-74).
