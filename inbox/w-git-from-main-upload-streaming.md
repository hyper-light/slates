# main -> w-git: the upload streaming note (2026-11-23 15:0x PST)

r229's clone samples (stall-watch2 on the big Studio, 20 s at 1 ms):
- mirror clone near its end (~90 s of 97.9 s): shard 0 parked 16,024 / 18,248 (88%); git_encode 1; git_derive 0; shard 1 parked.
- end of the work clone into the fetch: shard 0 parked 15,567 / 18,542 (84%); git_encode 1,712 (9%); git_derive 21.
=> much of each 80-100 s step is likely the client's own index-pack (which stock pays too). main measures stock's
own times (mirror + work clone over git-http-backend and --no-local file://) on the same Studio when its queue frees.

Rulings:
1. PATIENCE: judged by progress, not a ratio. Fail a git step only when neither the git process tree nor the daemon
   makes CPU progress and the daemon's upload/receive counters do not move, for the liveness window (as the conformance
   harness's process_wait judges STALL and CPU_BUDGET). A hang fails in ~1 s; a slow step never does.
2. P1/P2 (spreading encode and derive across shards) wait for the measured split; first land the push decide_step
   index fix (the measured hot path: 97% server CPU).
