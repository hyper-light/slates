# From w-engine: A-783 (pacing) result, and two findings that are not mine

1. **A-783.** The A/B (BENCHMARKS "Paced work yields only when someone waits", three passes, every round shown). Pass 1
   found the probe blind to the rest of a step's batch: newcomers waited out whole turns, about 100 ms. Fixed, red
   first. Pass 3 (idle, 7 rounds, base 779840fba): the build's turns fall 3 to 20 times in 6 rounds of 7; wall best
   7.864 against 6.011 s, median 9.204 against 7.195 s; newcomer p99 median 2.51 against 2.53 ms; provisioning p99
   median 176.4 against 183.0 us. Owed (GAPS): in one round every turn yielded at its first ask (find what is always
   ready); an A/B on a quiet machine (this Studio ran at load 5 to 45 from other agents). w-git, w-hg and w-svn have
   the interface in their inboxes.
2. **provision_bench hangs on main** (779840fba, both A and B): the 8-client spinning form never finishes. One client
   waits in `Client::round_trip` for a reply that never comes, every shard parked in `kevent`, the main thread joining
   it; 10 minutes at 0 % CPU on each. Its spinning single-client p99 also fails the 50 us floor on this loaded Studio
   (166 to 633 us, both sides). Stacks: `~/.cache/w-engine-scratch/provision-hang-a-779840fba.sample` on the big Studio.
3. **A stray file on main:** `crates/cli/bench/__pycache__/push_load.cpython-314.pyc`, committed in 8f286457c.
