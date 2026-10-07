# The macOS NFS client panicked the kernel when its server was killed under load

Date: 2026-10-06.
Area: `crates/server/src/nfs.rs` (the loopback NFS connection), `crates/anchor/src/held.rs` (the anchor's hold),
`crates/rt/src/tcp.rs` (the stream).
Conditions: 11 (adversarial recovery), 2 and 6 (macOS mounts); design §4.6, A-113.

## Description

An adversarial test on this Mac (M5 Max, Darwin 25.4.0, xnu-12377.101.15) mounted a 2 GiB volume over the loopback
NFSv3 mount `slates mount` makes. A Python writer created and fsynced files of 100 B to 300 KB for 20 s, deleting old
ones, while the daemon was killed with `SIGKILL` 10 times at random 0.6–1.6 s intervals; the anchor restarted it each
time.

- Twice the machine panicked and rebooted (`/Library/Logs/DiagnosticReports/panic-full-2026-10-06-194707.0002.panic`,
  `panic-full-2026-10-06-200003.0002.panic`): "Kernel tag check fault" — the memory-tagging hardware caught a kernel
  use-after-free. Both stopped at the same unslid instruction (`0xfffffe0006f02ca8`), inside
  `com.apple.filesystems.nfs`. The first panicked in the writer's process (`Python`), the second in `launchd`.
- In a run that did not panic, every kill cost the writer a flat 1.0 s stall (1,003–1,015 ms over six kills; four kills
  in quick succession, 4,006 ms). An earlier run stalled 10,015 ms once.
- The same run returned two fsync-acknowledged 300 KB files empty after the kills. That is a separate, open
  durability question (GAPS, 2026-10-06), not this record.

## Root cause

The defect is in Apple's NFS client: a user-space server's death, however abrupt, must not panic the kernel. The
faulting function was not symbolicated (no kernel debug kit here), so which client path frees the object is not known
from the log.

What slates controls is the trigger. The anchor held only the NFS *listener* across a daemon restart (§4.6). Each
accepted connection belonged to the daemon alone, so its death closed every connection at once, with requests in
flight. The kernel client then tore down its connection state, timed out, retransmitted and reconnected — once per
kill, ten times in 12 s. The flat 1.0 s stall per kill is that reconnect.

A daemon restart is a designed event (§2.6 supervision, A-61 for FUSE). So the bug on slates' side is that a restart
was visible to the kernel client as a dropped connection at all.

## Impact

Any macOS host with a slates mount whose daemon died — a crash, memory pressure, an upgrade, `kill` — exposed the whole
machine to a panic, and every mount stalled about a second per restart. Linux FUSE mounts were not affected: their
devices have been held across restarts since A-61.

## Edits (A-113)

- **The anchor holds every accepted loopback connection** (`crates/anchor/src/held.rs`: hold-connection and
  release-connection messages, `ENV_CONNECTIONS`). The hold channel, Linux-only before, is now Unix: a sequenced-packet
  socketpair on Linux, a datagram socketpair on macOS (no sequenced-packet Unix sockets there). The anchor polls the
  channel, so a hold is taken as it arrives, and raises its descriptor limit (it held devices at a shell's soft limit
  before, a sibling of this bug).
- **A request leaves the kernel only once its reply is in it** (`crates/server/src/nfs.rs`): records are peeked
  (`MSG_PEEK`), served, their replies sent, and only then consumed. A daemon that dies leaves every unanswered request
  queued for its successor, which answers it on the same socket.
- **Replies leave in whole records**: each send is a run of whole records no longer than the largest record, with
  `SO_SNDLOWAT` at that size, so XNU's `sosend` takes a send whole or not at all. A death can at most repeat a whole
  reply, which the client drops by its xid.
- **Exact bounds**: `MAX_MESSAGE` is derived (`MAX_TRANSFER + COMPOUND_HEADER_BYTES`, was a placeholder 2 MiB), each
  connection's buffers hold two records, and READDIR budgets are capped at the transfer ceiling (a sibling: a client
  asking 4 GiB got a page as large as the directory).
- **A connection that ends is shut down** before its hold is released (the anchor's copy would keep it open), and every
  path where a living daemon gives a connection up releases it.
- A peer that closes with an unfinished record queued is seen through the TCP state (`TcpStream::peer_closed`): a
  peeking reader never sees the close as a zero-length read. The hostile suite found this the first time it ran: 192
  serve tasks were left behind.
- Linux does not hold connections yet: its kernel may take part of a send, so a successor could resume mid-record. It
  keeps today's behaviour, recorded in GAPS.

## Tests

- `crates/cli/tests/nfs_held.rs` (T-4.12): one userspace NFS connection under the real anchor, six `SIGKILL`s each
  inside a burst of 12 pipelined FILE_SYNC WRITEs of 4 KiB to 256 KiB. The connection never closed; all 72 requests
  were answered `NFS3_OK`; the READ after each kill was answered by a new daemon; the file matched the model byte for
  byte. Two of six kills landed with requests still unsent, so successors answered requests the dead daemon never read.
  Longest kill-to-answer 123–126 ms over three runs, against 1,003–1,015 ms per kill through the reconnect.
- `crates/anchor/src/held.rs` units: a connection hold and release cross the channel in order with the very
  descriptor; their own bound; the handoff round-trips; malformed handoffs refused.
- `crates/server/tests/nfs_hostile.rs` passes (it caught the peer-close case).
- Not yet run: the kernel client under kills with this fix. That repeats the experiment that panicked the machine
  twice, so it waits for Ada.
