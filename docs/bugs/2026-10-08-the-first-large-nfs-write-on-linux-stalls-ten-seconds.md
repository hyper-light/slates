# The first large NFS write after a mount stalls about ten seconds on Linux: a zero receive window

**Found:** 2026-10-08, comparing slates against the kernel's own NFS server (`docs/wip/bench/mixed/linux_vs_nfsd.sh`).
**Status: fixed** (same day; see Fix).

## Description

On Linux 6.12 (Docker Desktop's linuxkit kernel), the first large write over a fresh NFSv4.2 mount of a slates export
takes about 10.5 s. `dd if=/dev/zero bs=1M count=64 conv=fsync` runs at 6.4 MB/s, in 5 of 5 runs. Every later write
on the same mount runs at 1.0–1.7 GB/s: 128, 256 and 512 MiB, zeros and random bytes. A one-line write before it takes
1 ms.

## Evidence

From the client (`/proc/self/mountstats`):
- WRITE: 257 calls, 267 transmissions, a cumulative queue time of 2,684 s against 4.0 s of round trips. Requests
  waited to be sent; they did not wait on the server.
- A lone SEQUENCE: 5.07 s queued, 8 ms round trip.
- The transport's connection count rose from 2 to 4 during the write.

On the wire (`tcpdump -i lo` on the slates port, headers only):
- No FIN or RST on the port during the write.
- Silences of 0.84, 1.66 and 3.26 s, each after a zero-length segment from the client, doubling. That is TCP's
  persist timer probing a zero receive window.
- The last silence (3.84 s) ends in a new SYN from the client.

On the daemon:
- The four ways `serve_connection` ends a connection are now counted (`nfs.connection.ended_by_peer`,
  `closed_mid_record`, `wait_failed`, `send_failed`), and none moved during the stall.
- `nfs.stream_refused` did not move either.

## What is known about the cause

- A connection's requests are peeked, not read: one stays in the kernel's receive queue until its reply is sent, so
  a daemon that dies loses no request (A-113).
- The daemon waits for a whole record with `SO_RCVLOWAT` (`Connection::want`).
- On Linux the receive buffer is left to autotune, and is expected to grow with the low-water mark
  (`slates_rt::tcp::reserve_buffers`'s Linux arm).
- The client's zero-window probes show the receiver advertising no window while a WRITE record (256 KiB of data plus
  its header) is incomplete. If the window Linux advertises cannot cover one record, the record can never complete
  and the daemon is never woken to read it.
- Why only the first large record on the first connection: not shown. The kernel's window derives from a measured
  scaling ratio and the buffer's autotuning, and the second connection runs at full speed.

## Constraints on a fix

- Reading the record into the daemon's memory early would end the stall but break A-113's guarantee: a request
  consumed before its reply is lost if the daemon dies.
- A fixed `SO_RCVBUF` is capped by `net.core.rmem_max` for an unprivileged daemon, and setting a buffer floor once
  refused every Linux NFS connection (memory: socket options need the Linux lane). `SO_RCVBUFFORCE` needs
  `CAP_NET_ADMIN`, which R10 forbids.
- Candidates to evaluate against the kernel's source (`tcp_set_rcvlowat`, `tcp_space_from_win`, the scaling ratio):
  - setting `SO_RCVLOWAT` in a way that grows the buffer before the first large record arrives;
  - an explicit `TCP_WINDOW_CLAMP`;
  - setting the low-water mark at the transfer ceiling from admission rather than per record.

## Impact

A fresh Linux mount's first large write pays about ten seconds. A container that writes a large file soon after its
volume is attached (an install, an image layer) sees it once per connection. Condition 12's tails on Linux include
it whenever a run starts with a large write; `mixed.py`'s large-file phase did not, because small operations came
first. Whether those small operations change the window, or only delay the first large record, is not known.

## Reproduced without NFS (same day)

`docs/wip/bench/nfs/peek_lowat_repro.py` runs the same pattern with no NFS: peek a record's marker, set `SO_RCVLOWAT`
to the whole record, peek it, consume it only after replying. The client sends 256.5 KiB records. Linux 6.12, ten
runs each, a run killed at 15 s:

| Records in flight | Low-water mark | Runs that hung |
|---|---|---|
| 4 | the whole record | 8 of 10 |
| 32 | the whole record | 9 of 10 |
| 32 | capped at half the receive buffer, then peeked again | 10 of 10 |

The runs that finished moved 2.2–3.3 GB/s. The kernel container's `net.ipv4.tcp_rmem` was 4096 131072 6291456 and
`net.core.rmem_max` 212,992. So whether the whole record ever queues depends on the kernel, and lowering the
low-water mark does not help: the record still cannot complete in the queue.

Reading, from memory and not yet checked against the source: Linux grows a connection's receive buffer and window
from what the application consumes (receive-buffer autotuning). A peek consumes nothing, so the window stays below
one record. `tcp_set_rcvlowat`'s own growth of the buffer sometimes rescues it. The NFS client recovers after about
10 s by dialing again. The plain client never does.

## Options (for Ada; each trades against A-113 or throughput)

1. Records small enough to fit the default receive buffer whole: a transfer size of about 64 KiB, so a WRITE record
   sits under `tcp_rmem`'s default with room for its successor. More RPCs per megabyte; the 1 MiB experiment showed
   the transfer size is not what bounds the write path, so the cost may be small. Measurable.
2. Consume records into the daemon's memory, bounded per connection, and give up A-113's guarantee for unanswered
   requests. A daemon that dies loses them, and the client resends after its timeout (`timeo`, 60 s on these mounts),
   using its session slot's replay cache on v4.1.
3. A fixed `SO_RCVBUF` large enough for two records: impossible unprivileged past `rmem_max` (212,992 here, smaller
   than one record), and `SO_RCVBUFFORCE` needs `CAP_NET_ADMIN`, which R10 forbids.

## Next steps

1. Reproduced without NFS (above).
2. Read `tcp_set_rcvlowat`, `tcp_grow_window` and receive-buffer autotuning in Linux 6.12, to confirm the reading.
3. Measure option 1 (a 64 KiB transfer) on the repro and on the Linux NFS bench, then choose with Ada.

## Fix (same day)

The repro first had two bugs of its own: the client counted a one-byte read as a reply, and the server peeked the next
marker under the previous record's low-water mark. With both fixed it still hung, and `ss -tmni` showed why:
- the server held 44,416 bytes of payload charged as 129,984 against a 131,072-byte buffer (`rb`), and dropped
  segments;
- the client was window-limited for its whole busy time, its persist timer backed off.

The kernel's per-segment overhead filled the default buffer with a third of a record. Autotuning never grew it,
because a peek consumes nothing.

`slates_rt::tcp::reserve_buffers` now primes the buffer on Linux at admission. It raises the low-water mark to
`CONNECTION_BUFFER_BYTES` (two records, the one served and the next arriving) and sets it back.
`tcp_set_rcvlowat` grows an unlocked socket's buffer to fit, up to `tcp_rmem[2]`, with no privilege, and requests
stay unconsumed in the kernel (A-113).

| | Repro, 4 / 32 in flight, runs hung | First 64 MiB NFS write after a mount |
|---|---|---|
| Before | 10/10 and 10/10 | 10.5 s, 6.4 MB/s, 2 reconnects (5 of 5 runs) |
| Primed to two records | 0/10 and 0/10 (3.2–3.4 GB/s) | 0.061–0.071 s, 0.95–1.1 GB/s, no reconnect (3 of 3) |

The Linux lane (non-root, io_uring allowed) passed on the change: `slates-rt` (23 test binaries) and the server's
`nfs_hostile`, `nfs_tls` and `nfs_mount`.
