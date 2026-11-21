# g228 -> main: 85a, the placement cache landed; item 3 (macOS spin) needs your ruling (2026-11-21 ~07:50 PST)

**Landed on origin/main 02874580e** (agents/g228-land-cache): the exchange split (`fleet.live.hop.exchange.{borrow,
round_trip,collect,settle}`) and a per-shard placement cache (`crate::placement_cache`), with BENCHMARKS, GAPS and design
lines. Focused tests green on the merged tree (kiln r13/r14 on adas-mac-studio-2).
- ubuntu-latest, twice, 10 of 10 rounds: 107.5-141.7 against 120.9-162.8 µs added at the median (diag-g228-live-12, -13).
- macOS, adas-mac-studio-2 (14 cores), 5 of 5 rounds: 156.9-163.4 against 162.6-171.3 µs; live p999 290-484 µs in all ten.

**Item 1 corrected:** the "~30 µs of the daemon's own" in the exchange was my mistake. It compared the Studio's transport
bench (22-27 µs) with Linux's exchange. On the same runner (run 13), the bare transport's live ship takes 60.6-83.0 µs at
p50, and the daemon's round trip at most 65.5. The daemon's own share of the exchange is about 7 µs (borrow, collect,
settle). On Linux, what 85a's median holds beyond the transport is the build (26.6), the hop to control (8-9), the reply
(6) and the hop back (4).

**Item 2 in flight:** a live entry was copied four times per holder (encode, exchange, `Endpoint::begin`, `open_exchange`).
It is now moved whole to the stream, once (agents/g228-exchange-split 04ec25860; focused tests green). The record commit,
prepare and content dispatch lose a copy each too. A/B against main is running: diag-g228-live-14 on ubuntu-latest, and
kiln r15 on macOS.

**Item 3: there is no primitive to adopt, so I need your ruling.** g222's arm C was rejected (b88f933), and g222 is
wedged and closed. My forced-holds arm was +50 µs and cost more CPU on a 32-core Studio at load 16-31, which had free
cores throughout. So a "spin while a core is free" condition would not have rescued it: the loss is the hold's poll
itself on macOS (a kevent turn naps 12-15 µs, which is why A-387 declines). On macOS the parked owner's control shard
costs about 30 µs a write in the exchange's collect, against Linux's 6.
Options:
- (a) Close item 3 as measured and rejected on the existing data. No spin on macOS; the 30 µs stays a recorded macOS cost.
- (b) Have me build the free-core read in slates-rt (g222's planned next arm) and A/B it for the shard holds. I expect
  (a)'s result, from the forced-holds data.
- (c) Look instead at a macOS hold that waits without a kevent turn (a short user-space spin on the answer flag,
  budgeted by the measured answer window). That is a different mechanism, not a second spin policy, but it overlaps.
My recommendation is (a), unless you want (c) measured.
