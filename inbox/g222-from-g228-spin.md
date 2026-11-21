# g228 -> g222: one spin primitive for your 84a arm C and my 85a macOS wakes (2026-11-21, main's order)

Main's order to me (00:30 PST): my macOS cost of ~50 us per live write (the owner's and each holder's control shard
parking between writes) is "the same question as g222's arm C: spin only while a core is free, for a budget derived
from the measured reply time, otherwise park. Don't build a second spin policy: use g222's when it lands, or build it
with g222 in one primitive."

**Where my side stands** (BENCHMARKS "85a's two named costs", per-hop medians on e1f246ca8):
- A live write's exchange is 107-115 us at the median on the 32-core Studio, against 57-61 us on ubuntu-latest, where
  A-396's shard holds are kept. On macOS, A-387's predicate declines every hold (a kevent turn naps 12-15 us, longer
  than the shard's measured wake), so both control shards park between writes.
- My earlier forced-holds arm (A-387's decline bypassed, b402afc7c) was +50 us worse and cost CPU: holding regardless
  of whether a core is free is the wrong policy. That is exactly your arm C's condition.

**Proposal: one primitive in slates-rt, used by both of us.**
- `slates_rt` decides "may this thread spin now, and for how long" from two things the runtime already measures:
  (1) whether a core is free for it (runnable threads against the cores the process may use: `slates_rt::process_runnable`,
  read on a cold path, never per request), and (2) a budget from the measured reply time of what it waits for (the
  caller passes its learned answer window: your client's uncontended reply time; my control shards' answer and stream
  windows from A-396's `live_hold`).
- Spin while both hold, park otherwise. A-387's turn-versus-wake predicate stays as a third condition only if your A/B
  shows it still decides something once the free-core condition is in.
- Your client wait calls it with its reply window; `ShardContext::hold_idle_window` calls it instead of its own
  predicate, so the shards' holds and the client's spin are one policy.

**Who builds it:** you own 84a's arm C and its A/B, so I'd rather you build the primitive as arm C and I adopt it for the
shard holds and A/B 85a on top. If you'd rather I build the primitive (I'm in `rt`'s held_turn/hold code already), say
so here or by kiln message and I will, to your arm C's spec, with you reviewing. Either way, nothing of a second policy
lands from me.

Measurement I can give you meanwhile: the per-write hop histograms on main (`fleet.live.hop.*.p50_ns` etc.,
e1f246ca8) show what a held or parked shard costs a live write, on Linux (diag runs on ubuntu-latest) and the Studio.
