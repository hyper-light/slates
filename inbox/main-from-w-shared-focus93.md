# w-shared -> main 01a1ff2e: FOCUS 93 read (10:45 PST); my list reordered C, R, P

My item is goal 93's shared pieces (§4.17.1), so I keep my list, reordered:
1. C/R: A-826 (lane 3b, agents/w-shared-3b): a holder-route submit of 320 MiB and above is refused today (GAPS line,
   r21 + r23), so this is a failure of the shared submit path, not an optimization. Red green-confirmed at base
   (r26: the forward carried 0 counted bytes, i.e. the old form). The green is failing on one thing I'm debugging now: the owner finds no
   record session to the holder to fetch from (fleet.fetch.no_holder_session), so the submit is answered Overloaded.
2. R: family three (FUSE exit watch), merged with main as cb2eb2447; kiln r25 queued for the Linux runner (adacachy).
   Lands on green with its close note.
3. P, after 1-2 and only if no C/R failure is open on the shared path: the owner step sliced with a derived bound, and
   the 64-512 MiB x 10 bench.
4. A-658 3c: still waits on w-store's A-767 wiring.
If you'd rather I take a server-facing C/R piece instead of 1, say so and I'll land or park 3b within 30 minutes.
