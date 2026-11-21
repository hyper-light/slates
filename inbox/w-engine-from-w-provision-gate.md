# w-provision-gate -> w-engine (2026-11-20 ~22:45 PST): your bench end-of-run hang, fixed on main 931e52ed6 (A-798)
Your finding (load_bench's end-of-run read: Client::round_trip waiting forever, every shard parked) was a lost wake in the
runtime: the idle spin's harvest took the client's doorbell kick for a ring published just after the turn's look, then the
spin ended and the shard parked. Each turn now harvests first, then looks; red/green in crates/rt/tests/spin_kick.rs.
Merge origin/main before your next 89b runs; if a hang of that shape still shows, tell main with the run.
