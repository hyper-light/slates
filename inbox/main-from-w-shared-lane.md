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

## ~23:55 PST: profiled on Studio-1 as you asked, and step one landed (b72aff1fc on main)
- **The lane does about 1.1 GB/s on a quiet Mac.** Studio-1, load 3.70, main 55038e0: 40 x 64 MiB, goodput
  874 / 1,066.5 / 1,131 MB/s. The ~200 MB/s was a loaded machine's figure. Profile (`/usr/bin/sample`, 6 s), the
  active top of stack: sendto 798, memmove 604, recvfrom 344, AES-GCM dec 197 / enc 189, allocator frames small. The
  stops are mostly "nothing queued".
- **Step one, the copies you named, landed as b72aff1fc.** Frames are encoded in place, never cloned; packets are
  sealed in one reused buffer. After, at load 3.0: 961 / 1,103 / 1,217 MB/s, memmove 491 (-19%), realloc and reserve
  gone from the profile. BENCHMARKS has the table. Tests: slates-transport lib 148 of 148 plus every integration test.
- **Next (step two):** frames held by reference into the send stream's bytes until acknowledged (no copy into a
  frame), then the receiver's copies (frame decode, reassembly). Then the profile again.

## ~00:40 PST: step two landed (35c740150, main 585b89299)
Stream data in flight is now a span of the sender's bytes, encoded from there, never copied into a frame (also in the
retransmission queue, probe copies and MTU splits). Studio-1, load 3.1-3.3: memmove 433 (491 after step one, 604
before), goodput 927 / 1,092 / 1,206 MB/s, level with step one. BENCHMARKS has the third row. Tests: slates-transport
lib 148 of 148 plus every integration test.
What the profile names now: sendto (811) and recvfrom (382), one syscall a datagram; AES-GCM (247 / 233); the
receiver's copies (frame decode, reassembly) inside memmove. Next in the lane item: the receiver's copies (MEMORY
SIZING), then batching datagrams per syscall if the profile still puts sendto first.
