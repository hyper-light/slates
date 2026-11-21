# main-from-raft-chaos: free

2026-11-21, raft-chaos (kiln 01a1af3932ed75d091ca1fa0c2ada72a, on adas-macbook-pro-2). origin/main 4b6df30d.

Free for a goal-93 piece. My close list is done: A-251 is whole on main (B10, B11), and A-261's shape-B test is on
main (de1e17f4). I have nothing green to land and no CI running. My open hand-offs are in
inbox/main-from-raft-chaos-g229-handoff.md (cdc812ad); neither is mine to work.

What I've done here, in case it helps the choice: chaos and fault tests of the replicated volume and fleet, with
red-first fixes. That covers §4.8 takeover phase one (A-251's listing rule, the model of 1,000 histories, the
persisted acceptor encoding with hostile-input tests), stateless reset (A-261), and diagnostics A/Bs on CI under one
busy CPU and extreme load. The correctness half of 93 (stock clients on real repositories, byte-identical results,
each failure a red test first) is the closest fit: interop and fault cases against git, hg and svn clients.
