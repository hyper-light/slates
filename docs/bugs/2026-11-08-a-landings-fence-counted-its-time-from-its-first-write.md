# A landing's fence counted its time from its first write

**Found** 2026-11-07, re-landing the lease keepalive (the sibling named in
`docs/bugs/2026-11-07-a-landings-lease-renewal-was-refused-while-it-held.md`); the supervisor ranked it first as the
safety side.

**Red** (`crates/land/tests/lease.rs`, `a_holder_paused_before_its_writes_begin_writes_nothing_after_its_term`).
A sliced landing of two replacements runs under a 50 ms lease. It is stepped one unit (before its writes), pauses
100 ms, then is stepped to its end. On main `ecb86b4f` both entries were `Written`, 3 of 3 runs. The fence's "now" was
2 to 255 µs past the request's clock after the 100 ms pause, so a lease that had ended 50 ms earlier still held.

**Root cause.** `Landing::fence` reads the landing's clock as `request.now_ns` plus the time since
`WriteProgress::started`. That instant was the write phase's own start (`WriteProgress::start()`, at the end of the
validation). Everything between the request's clock read and the first write went uncounted:
- the plan (`begin_landing`, O(files): 489 ms for 1,000 files on a loaded Mac);
- the sweep and the validation;
- a shard descheduled among them.
So a landing paused there wrote past its lease's term, and past its grant's. That is the unsafe side of AUD-29-03 ("a
holder paused past its term never writes on after another may hold the target"). The server's callers also read the
request's clock before opening the target, outside even that count.

**Fix.**
- The run takes the instant its request's clock stands for as `begin_landing` starts (`LandingRun::began`). The write
  phase counts from it, so every fence and every `land.entry` span include the plan, the sweep and the validation.
- The server reads the request's clock as it calls the engine (`run_landing`, `begin_granted`): after the target is
  opened, not before.

**Impact.** A landing paused or slowed before its first write, by longer than its lease's (or grant's) remaining term,
wrote entries another holder may already have held. Nothing writes later now than before the fix: the fence can only
end a lease or grant sooner.
