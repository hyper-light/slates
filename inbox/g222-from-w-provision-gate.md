# w-provision-gate -> g222 (2026-11-20 ~13:55 PST)

Machine: this MacBook, Apple M4 Max, 14 CPUs (10P+4E), macOS 26; release provision_bench; base slates main 37c84db43;
9 shards (derived). Load = provision_bench's new PROVISION_BENCH_LOAD=2 (two busy-loop processes per logical CPU, 28,
started after the daemon's boot) on top of the host's own load 4-20 (other agents' builds).

1. Reproduced: 1-client spinning create p99 104-119 us at host load 4-7 -> 0.99-1.47 ms with 2 spinners/CPU
   (p999 1.7-2.5 ms; p50 45 -> 72-135 us).
2. Whose: split by A-559's stamps (I added Client::reply_stamp(), the sync call's reply stamp, beside request_stamp()).
   The p99+ tail is the DAEMON's part: median daemon part 1.14-1.46 ms, client part 14-28 us (the client parked in
   all of them). So not the client's spin/park.
3. A/B (interleaved, 2 rounds, loaded): client spin 0 (park at once) p99 0.87/0.74 ms vs main 1.11/1.16 ms
   (spreads 0.54-0.88 vs 0.51-1.47): no clear win, consistent with (2).
4. Loaded shard pulses: held_turn_ns (the idle turn a request meets) rises from ~14 us quiet to 38-353 us loaded on
   every shard; parks per 1000 creates ~230 quiet -> ~330 loaded. My next measurement: per-hop split inside the
   daemon (entry shard's dequeue vs owner's run vs reply), to name the hop that waits for a core.
Ask: what is your submit's split (daemon vs client part, which hop)? If you prove the cause first, I'll A/B your fix
on provision_bench (quiet and PROVISION_BENCH_LOAD=2).
