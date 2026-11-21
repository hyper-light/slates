# To main from w-k90-pin-2 (took over w-k90-pin; 2026-11-20 ~14:55 PST)
1. LANDED kiln c0f576eb3 (is-ancestor confirmed, base c0f576eb3): GitHub `slates` job timeout 240 + Linux disk-room step.
   Run 44807509237 (a82ce7ede): windows-11-arm built kiln and ran the library's Slates tests green (52 min), then was
   stopped at 120 min; Linux lanes lost the runner (exit 143).
2. Green so far: GitHub windows-11-arm (clippy, build) 136537837066 and windows-latest (clippy, build) 136537837057.
   kiln lanes at 08f1a434: macos-arm64 ENOSPC on MacBook-Pro-2 -> retried, running on d13238; win-arm64-cross timed out
   silently on Studio-2 (0 idle cores) -> retried, queued; linux-x86_64 waits capacity on adacachy; linux-arm64 queued.
3. For you: adalundhe-MacBook-Pro has 80 MB disk free; macos/ubuntu-arm clippy red on vendor/slates' deprecated
   fetch_update under -D warnings (lint keeper); w-k90-win's windows-slates red (inbox note sent).

## ~16:10 PST
4. LANDED kiln 5264caaa3: build job's Report shows error blocks first (Windows (clippy, build) went red on 63da2c592 with
   exit 101; the annotation held only vendor/slates warnings, so the real error is unknown until the next run).
5. kiln's own CI legs for 08f1a434 are wedged ~4 h: legs 01a1fa8f0de8 (linux-x86_64, 'running' on 451397) and
   01a1fae6520e (macos-arm64, 'running' on d13238, routing 'held' cec37f with room) have every step pending, no
   checkout. Scheduler state looks stuck; a person may need to look. I'm leaving them queued per kiln's rules and
   watching GitHub's run on 5264caaa3 for evidence meanwhile.

## ~19:00 PST
6. GREEN macOS arm64: GitHub run 44841298533 job 136653837819 (kiln 4aa0ee778, vendor/slates at the pin): build,
   lib Slates tests, vendored + slates_roles all success.
7. LANDED e763e394f (Linux lanes: oomd off, 16 GiB swap, -j2) and eb7194f00 (slates job annotates failed tests).
   windows-11-arm: build + lib Slates tests green, vendored/slates_roles red (exit 101; repro r145 queued on jupiter).
8. Every clippy leg is red on crates/kiln/src/workspace/container.rs:301 `crate::rt::sleep` (disallowed_methods),
   w-k90-containers' A-520 piece 5. Not mine; for the lint keeper / w-k90-containers.

## ~01:15 PST 11-21
9. GREEN Linux x86-64: GitHub run 44851246181 job 136703754443 (kiln 4f5e58c1b). macOS arm64 green as before.
10. LANDED 482bd01f8 (Linux arm64 links with the toolchain's lld: GNU ld failed kiln's link after a 90-min build) and
    60c0b27c5 (vendored test reads Cargo.lock as LF: windows-11-arm failed 2/11 on a CRLF checkout; local 11/11 LF+CRLF).
    Next started GitHub main run should give Linux arm64 + Windows arm64. Windows x86-64 waits on w-k90-win.

## ~04:25 PST 11-21: VENDORING PART DONE (goal 01a1facaf183 marked met)
Pin: vendor/slates at 8ce3d61523e1 (vendor/MANIFEST.toml; the pin moved from 38ed3dfd since 08f1a434, so every run
below is at 8ce3d615). Green per platform (GitHub ci.yml):
- macOS arm64: run 44851246181 job 136703754428 (kiln 4f5e58c1b)
- Linux x86-64: run 44851246181 job 136703754443 (kiln 4f5e58c1b)
- Linux arm64: run 44887857088 job 136804151244 (kiln a0d114abb)
- Windows x86-64: run 44887857088 job 136804151233 (w-k90-win's windows-slates; kiln a0d114abb)
- Windows arm64: run 44887857088 job 136804151286 (kiln a0d114abb)
No slates CLI call (git grep over a0d114abb). Landed: c0f576eb3, 5264caaa3, e763e394f, eb7194f00, f500759d2, 482bd01f8,
60c0b27c5. Leftovers for you: kiln's own CI lanes (ci.runs.local 01a1fa8f0de1/01a1fa8fe409/01a1fa8ff6d7/01a1fa9006fe)
are wedged and need a person's cancel; every clippy leg is red on container.rs:301 (w-k90-a520's).

## ~04:50 PST 11-21: R2 ROOT CAUSE FOUND (the runner links dropping) -- needs a person's change on adacachy
- adacachy's enp9s0 holds TWO addresses: 192.168.1.77/24 PRIMARY (static, valid forever, not in NetworkManager's
  profile, which is DHCP-only: added by hand or by something with `ip addr add`) and 192.168.1.90/24 secondary (DHCP).
  kiln advertises quic_advertise = 192.168.1.90:7071 and binds 0.0.0.0:7071; `ip route get 192.168.1.83` -> src .77.
- So every UDP answer to a LAN runner leaves FROM .77. Proof: kiln's own UDP probe (tunnel.rs version_probe) sent from
  Studio-4 to 192.168.1.90:7071 is answered 5/5, every answer from 192.168.1.77:7071.
- Effect: a runner's direct QUIC to .90 never accepts the answers (a QUIC server may not change address) -> "UDP to the
  server gets no answer" -> HTTP tunnel; the 5-minute check back to UDP requires the answer FROM .90, so it never
  switches back. Every LAN Mac is now on the tunnel (one TCP connection), and these Macs reach adacachy over 2.4 GHz
  Wi-Fi (Studio-4: en1, ch 9, -69 dBm, its Ethernet en0 unplugged): each Wi-Fi blip ("Host is down", "Connection reset",
  "Broken pipe" in the runners' logs) breaks the tunnel's TCP, the QUIC link ends ~30-90 s later = "agent disconnected
  reason=closed". Not load (adacachy's UDP socket 7071: 0 drops; ARP table 109/1024), not upgrades (1 drop of ~20 lines up).
- Operational fix (a person's, on adacachy; I make no system changes): drop the stray .77 (`sudo ip addr del
  192.168.1.77/24 dev enp9s0`) unless something needs it, or make .90 the source for the LAN (`sudo ip route replace
  192.168.1.0/24 dev enp9s0 src 192.168.1.90`). Runners then take UDP again within 5 min (their probe). Plugging the
  Studios' Ethernet in would also end the Wi-Fi blips.
- kiln fix (mine, next, red first): the QUIC listener answers from the address it was reached at (IP_PKTINFO), so a
  multi-homed server can't do this again.

## ~09:30 PST 11-21: R2 IP_PKTINFO fix built, green so far; waiting on Linux for the kiln end-to-end red/green
- Where: kiln-rt (kiln's own hyper-rt: vendor/hyper-raft/crates/hyper-rt stays upstream's and nothing kiln builds uses
  it): Arrival::to (IP_PKTINFO/IPV6_RECVPKTINFO), Batched::send_segments_from (IP_PKTINFO src; Linux sendmmsg beside
  UDP_SEGMENT, macOS sendmsg). Vendored hyper-raft: hyper-transport Endpoint::handle_datagram_to (local_ip in place of
  None), driver passes arrival.to and sends from transmit.src_ip -- recorded as vendor/patches/hyper-raft/0006 + rows in
  UPSTREAM.md and kiln-rt ORIGIN.md. Windows runs only client endpoints (src_ip None): unchanged.
- Green: Linux (container on this Mac) kiln-rt reached_at_a_second_address_it_answers_from_that_address -- the kernel
  answers a 127.0.0.2 datagram from 127.0.0.1 (the fault) and from 127.0.0.2 with the fix; udp_batched 23/23; clippy
  -D warnings clean (Linux + macOS). macOS: new test 1/1, udp_batched 18/18 x10, kiln lib+tests compile.
- Waiting: r19 on adacachy (the only Linux runner; "too few idle CPUs"): kiln tunnel test RED with the fix reversed,
  GREEN with it, + vendor.py check (this Mac has Python 3.9). Then land, then measure the runners' drops.
- Finding for GAPS: kiln's topo.drops did not count Studio-4's 15:34:40Z drop; I measure drops from runners' logs.

## ~09:45 PST 11-21: Studio-4 disk
- ~/.cache/w-k90-pin-2-target.noindex was deleted whole at ~09:20 (Studio-4 was at 6.2 GB free); it is 160 MB now
  (debug only, kiln-rt's clippy). Studio-4 is at 24 GB free: the rest isn't mine. I stopped my queued macOS e2e run (r18)
  rather than rebuild kiln's lib tests here. I land the Linux piece as soon as r19's red/green passes; the target goes
  whole once it lands.
