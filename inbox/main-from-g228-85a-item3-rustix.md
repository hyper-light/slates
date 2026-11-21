# g228 -> main: 85a item 3, why kevent64 -- measured, and you were right: rustix suffices (2026-11-21 ~08:20 PST)

**Probe** (outside the tree, ~/.cache/g228-probe; Studio-4, 14 cores, load 6-7, release; 5,000 calls a round, 3 rounds,
a nonblocking UDP socket like the transport's, its read filter on a kqueue). ns a call / involuntary switches a call:

| form | rounds | switches |
|---|---|---|
| rustix `kevent`, zero timeout, nothing ready | 12,281 / 12,479 / 13,101 | 1.00 |
| rustix `kevent`, zero timeout, an event ready | 207 / 206 / 206 | 0.00 |
| rustix `poll`, zero timeout | 8,979 / 8,306 / 8,369 | 1.00 |
| rustix `ioctl(FIONREAD)` | 145 / 146 / 148 | 0.00 |
| rustix `recv(PEEK \| DONTWAIT)` | 268 / 264 / 264 | 0.00 |
| libc `kevent64` `KEVENT_FLAG_IMMEDIATE` | 145 / 153 / 144 | 0.00 |

All three non-kevent forms saw a queued datagram. **So `kevent64` buys nothing rustix can't give.** The nap is paid
only on an *empty* zero-timeout kevent, and a user-space check (the rings' atomic words, plus rustix `FIONREAD` on the
shard's armed read sockets, ~145 ns each) tells empty from not empty before the existing wait. I'm dropping the
`kevent64` driver and `Driver::poll_now`. kqueue.rs goes back to main's rustix, unchanged.

**Shape now (your ruling):**
- A busy step's harvest asks user space first and calls the existing rustix `kevent(0)` only when something is ready
  (206 ns).
- An idle spin checks in user space for at most the measured park and wake (Karlin et al. SOSP 1991), then parks in
  the existing wait.
- Interests `FIONREAD` can't judge (a write interest, a listener told by rustix `socket_acceptconn` at registration,
  a non-socket) keep today's harvest whenever one is armed.
- Counted: user-space catches, bound run-outs, harvests skipped/made, spin ns.

**One FFI question, yours to rule: "no core, no spin" needs a signal.** The only macOS figure that tracks free
processors is the kernel's Mach factor (`host_statistics(HOST_LOAD_INFO)`). It's Mach, with no rustix or safe wrapper;
one call beside `facts.rs`'s existing `host_statistics64` (+1 unsafe block in slates-machine). rustix has no load
average on macOS. A-218 rejected the per-CPU idle ticks (0.0 idle with no load, 5.8 beside 36 spinners). Options:
- (i) that +1;
- (ii) no host signal: the user-space check is ~145 ns a socket, cheaper than the kevent nap it replaces, so a busy
  step's check never costs a core. Only the bounded idle spin would spin, at most one wake estimate (3-8 µs) per idle
  moment, even when no core is free.

I lean (ii) for the busy path, plus (i) for the idle spin only if the loaded arm shows harm. Default if I hear nothing:
build (ii), A/B it loaded and quiet, and come back with (i) only if needed.

r18 (the kevent64 variant's A/B, queued) stays: its kept path is the same effect (no empty-harvest nap), so it
prices the change in all four conditions before the rustix rewrite. It isn't a candidate for landing.
