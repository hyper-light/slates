# w-shared -> main: the lane's ~200 MB/s, the receive-buffer hypothesis refuted, a profile needed (2026-11-20 ~21:30 PST)

Base: origin/main e4cfbd38d. Instrument: replication_bench's bulk row (3a9e5bf21). macOS arm64, this MacBook, release,
load average 22: not a recorded number. The pattern is what matters.

- **Hypothesis refuted:** the UDP receive buffer is not the bound. Over six 64 MiB transfers (about 5 s),
  `netstat -s -p udp` "dropped due to full socket buffers" rose by 8; over an idle 20 s it rose by 32 (other
  processes). Wrong, as I had stated it in progress and the close note.
- **What the row shows:** 64 MiB in 330 to 546 ms (123 to 205 MB/s), about 7,300 packets at the 9,209-byte path MTU.
  That is 45 to 75 µs per packet, an order above a send syscall plus AEAD of 9 KB. Stops a run: budget about 7,300;
  window 2,100 to 6,600; pacer 440 to 900; credit 0; empty 33 to 5,165. The sender alternates between window-limited
  and having nothing queued, so the cost is per-packet work on one side or the other, not the network.
- **Suspects from reading, unmeasured:** per packet, `Connection::poll_transmit` builds two `Vec<Frame>`s and clones
  every reliable frame (each stream frame's 9 KB) into the sent packet; `record_sent` keeps the originals; and
  `protect_packet` allocates the datagram. The receiver takes one `recv` per datagram and makes an acknowledgement as
  it goes. Not one of these alone is 45 µs.

**Run request (yours under the policy: profiling):** a CPU profile of both ends of
`REPLICATION_BENCH_BULK_MIB=64 target/release/examples/replication_bench 20` on a quiet Mac (samply or Instruments
Time Profiler), split into the sender's shard and the holder's. I'll fix what it names, with BENCHMARKS before and
after tables, under MEMORY SIZING (no copy per packet). Until then I'm on the robustness items' verification (r11
family three running; r9 exit 4 queued).
