# A session fell to the MTU floor on loss every size suffered

Date: 2026-11-08. Design: §4.10a (RFC 8899 §4.3 black-hole detection, §5.2 the BASE state). Found by
live-durability chasing the rolling partition flow's 2.3 s stall (A-200) in the daemon's diagnostic runs.

## Evidence

- diag-live-durability-42, x86 run 19. On loopback, the owner's session to a candidate resumed from a 2 s SIGSTOP
  went from path MTU 65,496 to 1,200 between two status dumps. Its congestion window was 11.7 KB and its smoothed
  RTT 16.6 ms. The candidate caught up partway and then stalled.
- diag-live-durability-43, x86 run 52. The owner's session to a resumed candidate ran at MTU 1,200 and an srtt of
  about 121 ms before a 21.9 MB install. The candidate never caught up, and the flow's Convergence check failed.
- On the simulated fabric, today's rule fell a 1,500-byte path's session to the floor in both cases below, and
  counted a black hole each time (`black_holes: 1`):
  - a peer silent for 2 s: three backed-off probe timeouts with nothing acknowledged;
  - an 8 ms burst lost at every size: three full-size packets declared lost in one loss pass.

## Root cause

`PathMtu::on_large_packet_lost` declared a black hole after `BLACK_HOLE_LOSSES` (3) consecutive losses of packets
above the floor, with none acknowledged between. It had two feeds:

- every non-probe packet larger than 1,200 bytes that a loss pass declared lost (`note_mtu_losses`);
- every probe timeout while such a packet was in flight (`note_mtu_timeout`).

At a confirmed size of 65 KB every data packet is above the floor. So loss that hits every size counted the same
as a path that shrank, whether it was a receiver's full buffer dropping a burst, or a stopped peer that answered
nothing at all. RFC 8899 allows data loss as black-hole evidence only when it is "excessive loss of data sent
with a specific packet size" (§4.3). It requires the PL to be robust to loss "due to other reasons (including link
transmission error, congestion)" (§3 item 4). RFC 9000 §14 never lowers the PMTU on data loss or on probe
timeouts. The rule also fell at once, without confirming the size with probes (§5.2: "If MAX_PROBES successive
PLPMTUD-sized probes fail to be acknowledged, the method enters the BASE state").

## Fix

Loss and silence only make the size suspect. Three in a row, with none acknowledged between, of above-floor packets
declared lost or probe timeouts with one in flight (`PathMtu::on_silence`) have this effect:

- The session sends at the floor (`PathMtu::current`), so packets that cross keep the acknowledgements, and so
  loss detection, running.
- It probes at the size it had.

One confirming probe acknowledged, or any above-floor packet acknowledged, and the size stands. `MAX_PROBES` of
those probes declared lost while the floor's packets cross are the black hole, and the session falls and searches
below the size that stopped.

A probe timeout counts toward no black hole, so a peer that answers nothing never makes one. Three of them in a row
still start the confirmation. The first cut of this fix made a single timeout enough: at
least 1,230 and 1,525 suspicions in 30 partition runs on x86 and arm (diag-live-durability-49), each sending at the floor
until a probe at the size came back, against the 44 and 45 falls the old rule made in the same flow
(diag-live-durability-48). After a path shrinks, nothing acknowledges a full-size packet, so the floor-size sending is
what keeps the 2026-09-28 deadlock away
(`docs/bugs/2026-09-28-a-shrunken-path-deadlocked-before-its-black-hole-was-seen.md`).

## Test

- `a_peer_silent_for_several_probe_timeouts_keeps_its_path_mtu` and `a_burst_that_loses_every_size_keeps_the_path_mtu`
  (`crates/transport/tests/session.rs`, the simulated fabric, the fleet's frame cap). Both were red on the earlier
  rule: the size fell to 1,478 and 1,459 bytes, with one black hole counted.
- `a_session_finds_its_paths_mtu_and_falls_back_when_the_path_shrinks` is green on both rules: a real shrink still
  falls.
- The unit cases in `pmtud.rs`:
  - `a_black_hole_falls_back_to_the_floor_once_probes_at_its_size_fail`;
  - `silence_makes_the_size_suspect_and_one_acknowledged_probe_restores_it`;
  - `an_acknowledged_full_size_packet_clears_a_suspect_size`.
