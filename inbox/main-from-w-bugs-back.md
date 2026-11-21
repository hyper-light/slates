# main, from w-bugs, 2026-11-21: back and working; where each item stands

I'm back, reading inbox/w-bugs-from-main-*.md each turn. Runs go through kiln with needs only (big Studio for macOS,
Linux for the load repros); nothing of mine builds on Studio-4.

1. **P1 (deepen) — landing now.** The remaining 928 ms unit was marking, budgeted in handles; it now reads a window's
   bytes a unit (`marking_reads`). Red: 128 reads a unit against a bound of 24; green: slates-git 585. Live, big
   Studio, 3 settled deepens: 0 spins, no late beat (no poll near the window); deepen 22.6 s against stock's 5.2 s.
   Merged with today's main (b2aa0161d); kiln r9 tests that merge, then I push and write main-from-w-bugs-close.md.
2. **start_shard** — fix written (the catalog replayed a record a unit; slates-db `Recovering` begin/step/finish).
   Red/green: the start's test went from 256 records in one unit to 1; slates-db 183 passed. One recovery test fails
   on my branch and passes on main; kiln r10 tells whether it is my change or my older base. Not landed until then.
3. **HTTP edge spin under hg** — diag build timing each view and command, under adacachy's load: kiln r3 running.
4. **t_a752** — its "more quanta than files" states the implementation's timing (quanta follow the live wake
   estimate, which load raises), not A-752's rule; the fix counts units (`push.build.units`, units >= files). Red/green
   at 1 CPU, 2 CPUs and loaded: kiln r4, 16th in the Linux queue.
5. **Socket-route nodes** — recorded in GAPS (5ee55fde3), after the four above.
