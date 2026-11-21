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

## 2026-11-20 ~23:20 PST (machine clock; my earlier "00:40" stamps ran ahead): step three landed (c3db76bcd on main)
The receiver's copies: packets opened in place, frames applied from the opened packet (stream bytes lent), stream
bytes delivered once, messages taken whole by move. Studio-1, load 2.3-2.6: goodput 966 / 1,308 / 1,382 MB/s (median
+20% over step two's 1,092), 64 MiB p50 ~51 ms (~61 before). Tests: slates-transport lib 148 of 148 plus every
integration test; slates-cluster lib 248 of 248 and --test live 4 of 4.
Lane so far: ~1.07 GB/s -> ~1.31 GB/s median on a quiet Mac, with every per-packet copy main named removed (and the
receiver's). The profile's top is now sendto / recvfrom, one syscall a datagram. The next lever is batching datagrams
per syscall (sendmsg_x/recvmsg_x on macOS, sendmmsg/recvmmsg or GSO/GRO on Linux). Shall I take that under the lane
item, or close the lane item here (the ~200 MB/s question answered: it was load, and the copies are gone)?

## ~00:35 PST: the lane measured in place, after exit 4's fix
A-697's release bench on Studio-1 (main 5ab1c23b5's parent tree ffb2a65): a holder's forward of its increment takes
428 ms at 64 MiB, 849 at 128, 2,737 at 256, 3,047 at 320: about 105-150 MB/s, while the transport alone does
1.3 GB/s (bulk row). So the lane's in-place throughput is bounded above the transport: the forward lane's own steps
(its chunking and its session borrow per turn, `forward_lane.rs`), and the control shard's beat runs 0.5-1.4 s late
during the larger ones. I'd take this as the lane item's remainder, ahead of datagram batching. Your call.

## ~01:05 PST: the lane in place refuses 320 MiB holder submits about half the time
Studio-1, main 5ab1c23, load 2.2-2.9, two runs of the 320 MiB bench without a profiler: run 1 passed (holder forward
3,114 ms), run 2 failed: `holder 335544320 run 0: Refused { refusal: Overloaded { shard: 0 } }` (fleet.rs:22692).
A third run under `sample` failed the same way. The forward's ask outlives its parked verb's bound and is answered
`Overloaded`, unrecorded, as designed, so a retry re-forwards the 320 MiB and meets the same bound. At 256 MiB and below
it passed (r9: 2,737 ms at 256).

Where the 3.1 s goes, so far: the owner's own steps are ~0.9 s (judge 20, append 300, place 600 ms, from the local
run's phases), so the transfer is ~2.2 s, about 150 MB/s, against the transport's 1.3 GB/s on the bulk row. The
`sample` (8 s, both nodes in one process): the holder's control shard in `resolve_and_forward -> forward_lane::
drive_or_queued -> Endpoint::drive -> flush -> sendto` (~377 samples); the owner's control shard in `serve_peer_records
-> serve_forward -> run_forwarded -> submit_computed -> judge_submission` (~500) and the record plane (~320); across all
threads blake3 1,072 and memmove 824 at the top of the stack. The serving side waits on `next_request()`, not a tick,
so I'm not claiming the poll-interval hypothesis without a measurement.

Proposed next, under the lane item (P): time the forward's phases on both ends (request bytes sent, last byte
received, owner's step, reply) to find the ~2 s, then fix the step that owns it. The bound that refuses at 320 MiB
should stay a bound (CLAUDE.md §2 item 8); a faster lane is the fix. Unless you'd rather I stop at the profile.
