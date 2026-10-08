# The first large NFS write after a mount stalls about ten seconds on Linux: a zero receive window

**Found:** 2026-10-08, comparing slates against the kernel's own NFS server (`docs/wip/bench/mixed/linux_vs_nfsd.sh`).
**Status: open.** Root cause in the kernel's window not shown yet. The constraints on a fix are below; a fix needs
Ada's direction.

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

## Next steps

1. Reproduce with a plain TCP server using the same peek and `SO_RCVLOWAT` pattern, no NFS, to isolate the kernel
   behaviour.
2. Read `tcp_set_rcvlowat` and the window computation in Linux 6.12.
3. Choose a fix with Ada that keeps A-113.
