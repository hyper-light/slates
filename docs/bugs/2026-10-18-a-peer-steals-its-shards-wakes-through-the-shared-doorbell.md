# A peer steals its shard's wakes: the doorbell descriptor is the shard's, shared by every client

- **Date:** 2026-10-18
- **Area:** `crates/ipc/src/rendezvous.rs` (the connect hand-off: `Prepared::kick_fd`, `Doorbell::Eventfd`),
  `crates/server/src/daemon.rs` (`kick_fd_of`), `crates/rt/src/shard.rs` (`park`),
  `crates/server/src/daemon.rs` (`serve_loop`, the doorbell protocol)
- **Status:** found by attacking; **measured and recorded here, fix in progress** (the fix is structural —
  a per-client doorbell — and is described below with the kernel facts that rule the alternatives out)
- **Class:** cross-tenant denial of service through a shared capability (CWE-410, CWE-668)

## Description

At connect the daemon hands each client three descriptors over `SCM_RIGHTS`: its region, a completion
eventfd of its own, and **the shard's kick eventfd**, duplicated. The third is what a client writes to
wake a parked shard (`Doorbell::ring`). `Prepared`'s own doc says what it is: "the shard's kick
descriptor the client writes to wake a parked shard".

It is the *shard's*, so every client on that shard holds a duplicate of the same description — and an
eventfd duplicate can be **read** as well as written. A read drains the counter. So one client can
consume the wake another client just posted.

The shard then sleeps to its next timer deadline. That is not a correctness failure — the doorbell
protocol in `serve_loop` (`announce_idle`, fence, re-check) means a request published *before* the
fence is served, and `park(outcome.next_deadline_ns)` always ends — but a request published *after*
the fence depends on the kick, and the kick is what was stolen.

## The measurement

A real one-shard daemon, a victim client doing ordinary `create` calls with the shard parked between
them, and a hostile peer that connected through `connect_as` and holds what the hand-off gave it.
Three arms **interleaved round-robin**, so the machine's own load is shared by all three
(x86_64 Linux, kernel 7.2.9-2-cachyos, box load ~40, 2026-10-18):

| arm | what the hostile peer does | p50 | p90 | max |
|---|---|---|---|---|
| A | nothing | 415 µs | 615 µs | 877 µs |
| B | the identical read loop on **a private eventfd of its own** | 362 µs | 650 µs | 1,119 µs |
| C | the identical read loop on **the shard doorbell it was handed** | **9,550 µs** | 9,778 µs | 14,815 µs |

`wakes stolen 20 over 15,969,571 read attempts` — one stolen per attacked round, every round.

**Arm B is the control and it matters.** The attack spins a core, and a core spinning beside the daemon
on a loaded box is its own slowdown; B burns exactly that core on a descriptor of its own. B is
indistinguishable from idle (362 µs against 415 µs), so the 23× is the **theft**, not the CPU. An
earlier two-arm run without this control measured 390 µs → 40,153 µs (103×); the stall is however long
the shard's next timer happens to be, so its size varies and its cause does not.

## Impact

Any client of a daemon can inflate every other client's latency on its shard by one to two orders of
magnitude, continuously, with a four-line read loop and no privilege. Against a product whose gate is a
sub-50 µs provision and whose stated target is the p99 under load, that is severe.

The actor is in scope: §4.13 consumers are untrusted processes of the daemon's own uid holding exactly
this channel, and the hand-off gives this descriptor to every client alike. It is a latency attack, not
a disclosure — an eventfd carries no data — and nothing is lost or corrupted; requests are still served,
late.

**It is the second finding of the day from one root habit:** handing a peer a descriptor that is not
*its own*. The first was the content arena
(`docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md`). There the descriptor was
too wide in extent; here it is too wide in ownership.

## What the kernel allows — measured, so the alternatives are ruled out by fact

Probed on the same host (`/tmp/bellprobe.c`, `/tmp/fdprobe.c`):

```
--- the shard kick eventfd as created today (O_RDWR) ---
  write                                  GRANTED
  read (this is the theft)               GRANTED
--- a write-only reopen through /proc/self/fd ---
  reopen O_WRONLY                        Permission denied
--- a pipe as the doorbell instead ---
    peer writes                          GRANTED
    peer reads its own write end         Bad file descriptor
    peer reopens the write end O_RDONLY  GRANTED
      and reads the shard's bytes        GRANTED
```

- **A write-only eventfd handle cannot be made.** `eventfd(2)` always returns a read-write description
  and an anonymous inode cannot be re-opened through `/proc/self/fd` at all, so there is no way to hand
  out a half of it.
- **A pipe write end is not confined.** The peer cannot read its own write end, but it re-opens that
  descriptor `O_RDONLY` through `/proc/self/fd` and reads the shard's bytes. (Pipes have a real inode;
  eventfds do not. The same asymmetry defeated the read-only-duplicate idea in the arena bug.)
- **A socketpair end shared by all clients is worse, not better.** A socket cannot be re-opened through
  `/proc`, so `shutdown(SHUT_RD)` on the clients' end would stop the theft — but any one client could
  then `shutdown(SHUT_WR)` its duplicate and send EOF to the shard's end, breaking the doorbell for
  **every** client on that shard. A shared capability stays sabotageable whatever its type.

The common factor: **any descriptor shared between clients is abusable by any one of them.** The
property that fixes it is ownership, not access mode.

## The fix

**Each client's doorbell must be its own**, so that draining it, closing it or shutting it down is
self-harm and reaches no one else. The shard's kick eventfd stays what it is — the runtime's own wake
for cross-shard messages — and stops being handed out.

Two ways to give a client its own, both one registration per client on the shard's driver:

1. **A fresh eventfd per client**, created in the hand-off exactly as the completion eventfd already is
   (`rendezvous.rs` makes one per client today, so the cost is known and the shape is precedented).
2. **The client's existing control socket.** The daemon already owns one per client (`Control.socket`,
   used for the mapping hand-off and by the client for liveness) and the client already holds the other
   end, so this adds **no descriptor at all** — only a readable registration. A client that shuts its
   own socket down is reaped, which is the existing meaning of that event.

Option 2 adds nothing to the descriptor budget and reuses a channel whose per-client ownership is
already the design. Either way the wake path changes, so **R9's provisioning histogram
(`crates/client/examples/provision_bench.rs`) is the gate** before and after: the shard's poll set goes
from one kick to one registration per client, and that must be shown not to move the single-client
spinning p99 that AC-2.1 holds at 50 µs.

## Owed

- The fix above, with the provisioning A/B.
- A by-use regression test of the invariant, not of the attack: **a peer's abuse of what it is handed
  must not move another peer's latency distribution** — the three-arm probe above, committed with arm B
  as its control, so the test can never attribute a loaded machine to an attack.
