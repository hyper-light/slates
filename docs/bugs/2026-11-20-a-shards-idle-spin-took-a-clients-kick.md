# A shard's idle spin took a client's kick and parked with the client's ring unseen (A-798, 2026-11-20)

**Seen by use.** A bench's client waited forever in `Client::round_trip` with every shard parked:
- provision_bench, at the first create of a run, with the idle window forced to 5 µs (1 hang in 2 runs; earlier the
  same day 1 in 1);
- w-engine's load_bench end-of-run read, twice on the big Studio (inbox main-from-w-engine-89b).

**The lost wake.** A client publishes its request into its ring, then rings; the doorbell thread (macOS, Windows) or
the client itself (Linux's eventfd) kicks the shard. The kick is sticky (an `EVFILT_USER` trigger, an eventfd count)
until a wait takes it. The idle spin (`Shard::spin_for_work`) took kicks too: each turn looked at the inbound rings and
the pollers (the client rings' readiness) and then harvested the driver without blocking. A harvest reports the
completions it found, never a kick it took (kqueue filters the user event; epoll drains the eventfd). So a ring
published between the turn's look and its harvest lost its kick to that harvest. If the spin's window ended on that
turn, the shard parked: `park`'s re-check reads only the inbound shard rings, and the driver wait had no kick left.
The runtime's own park protocol (loom-checked) was not at fault: the kick was sent, and a harvest swallowed it.

**The fix.** Each turn harvests first, then looks. A kick is sent after its message is published, so a message whose kick
the harvest took is visible to the looks after it; a message published after the looks has its kick still pending, and
the park takes it.

**Red first.** `crates/rt/tests/spin_kick.rs`: the shard's poller, on a chosen ask during the idle spin, answers what it
saw, then has another thread publish a ring and kick the shard, and holds the turn past the window. Looking before
harvesting: 3 of 3 runs never served the ring, the shard parked until the 20 s backstop (spin hits 0, poller wakes 1).
Harvesting first: 3 of 3 served in 20-40 ms. Focused tests green on macOS arm64 (this Mac, Studio-4): slates-rt pollers,
idle_spin, readiness, tcp, spin_kick and the shard unit tests; slates-server closed_loop and idle_window.
