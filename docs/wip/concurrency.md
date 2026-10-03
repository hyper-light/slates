# Concurrency proofs: loom on the cores, shuttle on the schedules (record, 2026-09-13)

The design's Part 6 "Concurrency" row: *loom (cores), shuttle (schedules), TSan, Miri — all explored
interleavings keep invariants; no data race; no UB — CI (loom, Miri), nightly (shuttle, TSan)*. This
file records what each model checks, the bounds it explores under, and the numbers, with the command
that produced each number. AC-0.7 ("loom passes on the ring and handle cores; Miri passes on `mem`,
`rt`, `wire` unit tests"), T-0.3, T-1.6 and T-6.7 are the identifiers.

Machine and load for every number below: Apple M5 Max (18 cores), macOS, `cargo 1.98.0`, load
average 6–7 with two other agents building concurrently. loom runs are `--release` (loom is too slow
otherwise); shuttle runs are the default profile. All commands run from the workspace root with
`CARGO_TARGET_DIR=target/loom` or `target/shuttle` so the `--cfg` builds do not evict the normal
build cache.

## 1. loom (CI, `miri-and-loom` lane)

### Bounds, one definition

`crates/mem/src/loom_bounds.rs` (compiled only under `--cfg loom`) holds the bounds every loom model
checks through, and `explore(name, model)` prints the interleavings a model explored (or the one a
failing model reached), so a run's record does not depend on loom's own logging:

| Bound | Value | Derivation |
|---|---|---|
| `PREEMPTION_BOUND` | 2 | Iterative context bounding [A: Musuvathi & Qadeer, PLDI 2007]: every bug in their corpus surfaced within two preemptions; loom's documentation recommends two or three; the parking protocol's store-buffering race needs one preemption per side, so two is the smallest bound that finds it. |
| `BRANCH_CAP` | 2 × 112 = 224 | Twice the longest honest execution measured across the models by bisecting `LOOM_MAX_BRANCHES`: the two contending producers pass at 112 and fail at 108; the one-producer ring passes at 82 and fails at 80; the lapping producer passes at 86 and fails at 84; the handle core passes at 50 and fails at 40; the parking protocol passes at 60 and fails at 40. An execution past the cap is a livelock (a spin loop whose condition never comes), which loom turns into a failure instead of a hang. |

loom's own variables stay its debugging interface: `LOOM_MAX_PREEMPTIONS` and `LOOM_MAX_BRANCHES`
override the constants for an investigation; `LOOM_MAX_PREEMPTIONS=255` (loom's `u8` widest, which no
execution here reaches) is the exhaustive run. CI sets no variable.

### The models and their numbers

Command for all of them (the CI step):

```
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom cargo test -p slates-mem -p slates-rt -p slates-ipc --lib --release loom -- --nocapture --test-threads=1
```

| Model (file, test) | Threads, shape | What every explored interleaving must keep | Bounded (2 preemptions) | Exhaustive (`LOOM_MAX_PREEMPTIONS=255`) |
|---|---|---|---|---|
| SPSC ring — `crates/mem/src/ring.rs`, `every_interleaving_of_one_producer_and_one_consumer_is_fifo_without_loss` (T-0.3) | 1 producer, 1 consumer; capacity 2; 3 words (a full ring, a retried refusal, one wrap) | FIFO; every word exactly once; nothing beyond the words pushed; the ring empty at the end; a full-ring refusal hands the word back and the retry lands it; the refusal path ran in some interleaving (non-vacuity counter) | 157 interleavings | 6,096 (equal to the unbounded run at c80b6f9 before the bound was applied in code) |
| MPSC ring, contention — `crates/mem/src/mpsc.rs`, `every_interleaving_of_two_contending_producers_keeps_each_order_and_loses_nothing` (T-0.3, "two producers") | 2 producers × 2 words, 1 consumer; capacity 4 (every word fits, isolating the claim race) | every word exactly once; each producer's order kept; nothing beyond the words pushed | 3,865 interleavings | did not finish inside a 600 s box (which is what the bound is for) |
| MPSC ring, lapping — `crates/mem/src/mpsc.rs`, `every_interleaving_of_a_producer_lapping_the_ring_is_fifo_without_loss` (T-0.3) | 1 producer × 3 words, 1 consumer; capacity 2 (a full ring, a retried refusal, the second lap of the sequence numbers) | FIFO; exactly once; the refusal retried; the refusal path ran in some interleaving | 26 interleavings | 192 |
| Handle core — `crates/mem/src/slab.rs`, `a_handle_returning_after_its_slot_was_reused_is_a_typed_miss_never_the_new_occupant` (AC-0.7 "handle cores"; T-0.1 under loom) | the owning shard and a peer; a one-slot slab; an SPSC ring out, an MPSC ring back | a request naming a handle either reads the occupant the handle was issued for or is refused `StaleHandle` with that handle's index and generation; it never reads the slot's new occupant; both outcomes reached in some interleaving | 6 interleavings | — (the two orders are the whole space) |
| Kick-if-parked — `crates/rt/src/parking.rs`, `a_word_published_while_the_shard_parks_is_never_lost` (AC-0.7; the CLAUDE.md "kick a shard only when it is parked" pattern) | 1 sender, 1 parking shard; the shard's MPSC ring; loom's `Notify` as the driver's kick (sticky and spurious-capable, like an eventfd count or an `EVFILT_USER` trigger) | the shard always receives the word: it saw it before waiting or was kicked out of its wait; a lost wake is a shard blocked with nothing runnable, which loom reports as a deadlock; kicks, skipped kicks and waits each reached in some interleaving | 27 interleavings | 116 (measured 2026-09-25) |
| Kick-if-parked, timed — `crates/rt/src/parking.rs`, `a_timed_park_never_loses_the_word_and_reads_only_its_own_stamp` (§4.3, A-31: the online wake estimate's kick stamp) | as above, the shard timing its wakes (`Parking::time_wakes`): the sender stamps the host clock on the first kick of a park, the shard takes the stamp after its wait | the shard always receives the word (the stamp is a `Relaxed` measurement word outside the protocol); a park that waited never reads a stale stamp (a sender stamps only after publishing, so a stamp always finds its word published and a later re-check skips the wait); a wake measured in some interleaving. One word cannot reach a stamp left from an earlier park: moving the announcement's time after the announcement leaves the model passing (checked 2026-09-25), so that ordering is held by the `Woken` unit tests | 22 interleavings | 83 (measured 2026-09-25) |
| Control flag — `crates/rt/src/parking.rs`, `a_control_message_published_while_the_shard_drains_is_never_lost` (AC-0.7; the control path of `registry::send_control_to` and `shard::drain_control`; `docs/bugs/2026-09-28-a-cleared-control-flag-stranded-a-shutdown.md`) | 2 senders, 1 shard that takes the `ControlFlag`, drains its channel (an MPSC ring standing in for it) and parks unless the flag is pending; loom's `Notify` as the kick | both messages are drained: a publication the shard's take absorbs is ordered before the drain (one release sequence of RMWs), and one that lands after stays set; a lost message is a shard parked with nothing runnable, which loom reports as a deadlock; the shard waited in some interleaving. Both halves were needed: a store publish deadlocked at interleaving 1, a load-then-store take at interleaving 207 | 5,204 interleavings | did not finish inside a 600 s box (measured 2026-09-28; the longest honest execution passes a branch cap of 106 and fails 105, under the 112 the cap is derived from) |
| Doorbell — `crates/ipc/src/doorbell.rs`, `a_request_published_while_the_shard_goes_idle_is_never_lost` (§4.7 "Wake strategy"; AC-0.7) | 1 client, 1 serving shard; the command ring's depth, the shard's idle announcement (`daemon_parked`) and loom's `Notify` as the doorbell (sticky, like the Linux kick eventfd), both sides through the same fence functions the client and the serve loop call | the shard always serves the request: it saw it on its re-check or was rung out of its idle; a lost wake is reported as a deadlock; rings, skipped rings and idles each reached in some interleaving. Its witness, `the_unfenced_protocol_loses_a_wake` (`should_panic`), keeps the pre-2026-09-28 orderings and must deadlock (it does, at interleaving 1) | 47 interleavings | 151 (measured 2026-09-28; the longest honest execution passes a branch cap of 26 and fails 24) |

Wall time for the five models under the bound: 0.12 s (mem, four models) + 0.00 s (rt) after the
build; the build under `--cfg loom` is ≈ 14 s cold. The timed parking model (2026-09-25) adds 0.00 s
(Apple M5 Max: mem 0.10 s, rt 0.00 s for the two parking models).

The unbounded numbers of the two original models at c80b6f9 (the CI command of that commit,
`RUSTFLAGS="--cfg loom" cargo test -p slates-mem --lib --release loom`): 6,096 (one producer) and
3,186 (two producers × one word); with `LOOM_MAX_PREEMPTIONS=2`: 157 and 475 (0.02 s).

### Measured and rejected

- **Two producers × two words on a two-slot ring** (both producers spinning on a full ring while the
  consumer drains): loom explores the two spinners' voluntary yields into executions past any honest
  branch cap — it failed at loom's default 1,000 and at every cap down to 40 with "Model exceeded
  maximum number of branches … e.g. spin locks" (2026-09-13). Replaced by the contention model
  (capacity 4, no full ring) plus the lapping model (one spinner), which together cover the claim race
  and the full-ring wait.
- **The handle model with the peer spinning for the publication** never reached the live outcome (19
  interleavings): a thread spinning in `yield_now` is re-run only when the active thread yields.
  Publishing before the peer starts reached 5 interleavings and still never the live outcome, because
  loom's partial-order reduction backtracks only a thread's *last* conflicting access (the drain after
  the reuse). One explicit `yield_now` after the spawn makes "the peer finishes first" the initial
  schedule, and loom's exploration of the older value at the shard's acquire load gives the other
  order: 6 interleavings, both outcomes.

### The defect loom found

`docs/bugs/2026-09-13-parked-shard-loses-a-foreign-wake.md`: on the protocol as it stood (a `SeqCst`
store and load on the parked flag; the ring's `Release` publication outside that order), loom reported
`deadlock; threads = [(Id(0), Blocked), (Id(1), Terminated)]` at interleaving 1. x86 realizes the
ordering through its store buffer (a `Release` store and a `SeqCst` load are both plain `mov`); arm64
does not. Fixed by a `SeqCst` fence between the write and the read on both sides (the C++20 fence–fence
rule), in one seam both the senders and the shard drive. Fenced: 27 interleavings pass. Sibling sweep
in the record: the ipc reply direction is safe by the kernel's word compare in `futex_wait`; the ipc
request direction (`ClientEnd::send` → the daemon's `mark_parked`) has the same shape with `Release`
and `Acquire` only and no re-check of the client rings after the announcement — reported then, **fixed
2026-09-28** (`docs/bugs/2026-09-28-a-client-request-waited-for-a-timer-after-a-lost-doorbell.md`): the
fences live in `slates-ipc` `doorbell.rs`, the serve loop announces, fences and re-checks its rings
(`verbs::announce_idle`), and the doorbell model below proves it.

### Miri (unchanged commands)

`cargo +nightly miri test -p slates-mem -p slates-wire --lib` and
`MIRIFLAGS="-Zmiri-ignore-leaks" cargo +nightly miri test -p slates-rt --lib --test differential`
(the `miri-and-loom` lane). The loom-only modules (`loom_bounds`, the `cfg(loom)` test modules) are
not compiled under Miri; the shipped parking seam runs under Miri through rt's unit tests, and Miri
models `fence(SeqCst)`.

## 2. shuttle (nightly cadence, `shuttle-nightly` job)

shuttle 0.9.3 is a dev-dependency under `[target.'cfg(shuttle)'.dev-dependencies]` of `slates-vfs`
and `slates-merge` (its transitive dev-only additions to `Cargo.lock`: `shuttle-engine`,
`shuttle-schedulers`, `shuttle-std`, `bitvec`, `rand` 0.8 and their crates); nothing shipped links it.
Both tests model the architecture rather than a lock: one owner per store or green (D-7), the agents
as threads whose requests and replies are moves over bounded shuttle channels, shuttle drawing the
thread schedule and the agents' randomness (`shuttle::rand`), so a failing schedule replays from the
seed shuttle prints. Every bound is a documented constant in the test file: agents, rounds, files,
the schedule budget, the seed, the per-thread stack (2 MiB, what `cargo test` gives a thread), the
channel bound.

Command (the nightly step):

```
RUSTFLAGS="--cfg shuttle" CARGO_TARGET_DIR=target/shuttle cargo test -p slates-vfs -p slates-merge --test shuttle_clones --test shuttle_green -- --nocapture
```

| Test | Shape | Oracle (reused, not new) | Result (2026-09-13) |
|---|---|---|---|
| T-1.6 — `crates/vfs/tests/shuttle_clones.rs`, `two_agents_interleaving_clone_and_write_under_random_schedules_stay_independent` | 2 agents, each: clone, then 1–20 write-and-create steps at the *same* offsets and names as the other agent, then a read-back; the store owner serves in arrival order | `tests/clones.rs`'s record: each clone's expected bytes and names; the snapshot and the origin still the base | 200 schedules (seed `0xc10e5eed`) in 0.09 s |
| T-6.7 — `crates/merge/tests/shuttle_green.rs`, `sixteen_agents_submitting_with_random_overlaps_are_linearized_and_decided_as_the_oracle_says` | 16 agents × 3 rounds over 3 files of 5 blocks, block tags 0..=3 drawn by shuttle; the green's owner serves in arrival order and logs every submission | the block reference of `tests/engine.rs`, moved to `tests/common/mod.rs` and shared: linearizability (the log replayed on a fresh green reproduces every outcome, head, bytes and counter), the per-block conflict rule, the final bytes, and the fast-path counter under the design's rule `last_changed[path] <= base` | 200 schedules (seed `0x5eed6ee4`) in 0.26 s: 48 submissions per schedule; 2,606 accepts, 584 identical accepts, 6,994 conflicts, 1,934 fast paths |

The budgets are the case counts the serial forms run (`tests/clones.rs`: 200 proptest cases; the
engine's generated histories: proptest's default 256), and the measured mix shows every outcome class
reached thousands of times inside them.

## 3. The transport's "loom on the state machine" (`docs/wip/fleet-transport.md`)

The sans-io `Connection` is single-threaded by construction: it holds no atomic, no channel and no
thread; every packet in and out passes through one `&mut self` call on its owning shard. loom
explores interleavings of *shared-memory* operations, so there is nothing for it to explore there —
a loom model of the state machine would be a serial test with extra steps. What loom *can* check on
the transport is the same thing it checks here: the rings and the parking protocol the shard's
datagram demux rides (this record). What the state machine needs instead is schedule exploration of
*two* endpoints' message orders — loss, reorder and duplication — which is what the reordering
oracle in `crates/transport/src/connection.rs` (`any_streams_survive_loss_and_reorder`, two
connections pumped over a deterministic lossy, reordering channel) and the live session tests in
`crates/transport/tests/session.rs` already do, and would grow under shuttle the way T-6.7 does
(two endpoint threads with a shuttle-scheduled fabric between them). The owed item should be
restated as that, not as loom.

## 4. TSan (nightly cadence, `tsan-nightly` job; 2026-10-03, AUD-29-32)

Ada authorized the lane and the workflow's nightly schedule on 2026-10-03. `cargo xtask tsan`
(`xtask/src/tsan.rs`) runs on the nightly toolchain with `-Zsanitizer=thread` and the standard library
rebuilt instrumented (`-Zbuild-std`, host target). The job runs on every push to main and on the daily
schedule (03:17 UTC).

1. **The canary first.** `crates/mem/tests/race_canary.rs` races two threads' writes to one word, one after
   the other in time with nothing ordering them (the second waits on a relaxed flag), both threads alive
   across both writes: reported 100 of 100 runs, and 50 of 50 at two CPUs (2026-10-03). Two earlier versions
   missed it, once in three CI runs and once in twenty runs here. The test is `#[ignore]`d and deliberately undefined behaviour. The task
   requires ThreadSanitizer's `data race` report from it and a non-zero exit. Run uninstrumented, the same
   test passes with exit 0 and no report, so a toolchain or flag change that drops the instrumentation fails
   the lane instead of passing it vacuously.
2. **The suites.** `--lib --tests` of `slates-mem`, `slates-rt`, `slates-ipc` and `slates-client`, with
   `--no-fail-fast`. A report makes the test binary exit 66, so any race fails the task. Doctests are not
   run: two compile-fail doctests in `slates-rt` fail to compile under `-Zbuild-std` for another reason,
   and no doctest runs threads.

Measured 2026-10-03 on an aarch64 Linux container (18 cores, Docker's default seccomp, so the runtime
serves through epoll there; the GitHub runner uses io_uring), with nightly 2026-10-02:

| Crate | Tests run instrumented | Reports |
|---|---|---|
| `slates-mem` | 68 | 0 |
| `slates-rt` | 100 | 0 |
| `slates-ipc` | 38 | 0 |
| `slates-client` | 14 | 0 |

Two tests are skipped. Each is an assertion that the instrumentation itself falsifies, and each still runs
in every other lane:

- `region::tests::locking_a_small_region_is_reported_by_the_os_within_one_page`. ThreadSanitizer ignores
  `mlock` (its runtime prints `ThreadSanitizer ignores mlock/mlockall/munlock/munlockall` at verbosity 1),
  so the locked delta reads 0. The test passes uninstrumented in the same container.
- `driver::tests::a_zero_timeout_wait_delivers_what_is_ready_and_never_sleeps`. It asserts that 256
  zero-timeout waits never switch the thread out. Running the whole library, it failed 3 times in 12
  instrumented runs and 0 times in 10 plain runs. Run alone, it passed 20 of 20 either way. The likely
  cause is the instrumented runtime's internal locking under the parallel tests; that is a hypothesis, and
  no log has shown it.

## 5. Owed

- **TSan over the server and the fleet**: the lane covers the crates whose tests drive real threads at the
  core. The server's and the fleet's suites are long, and the fleet suite must run alone, so adding them is
  a measured extension of this lane, not yet made.
- **shuttle over two transport endpoints** (§3).
- **A loom model of the ipc reply direction's park/wake** is out of reach as written: the rings live
  in a shared-memory region behind std atomics over mapped bytes, which loom cannot instrument, and that
  direction's safety rests on the kernel's word compare, argued in the 2026-09-13 bug record. The
  request direction's doorbell is modeled at the protocol level (`doorbell.rs`, §1), because its safety
  rests on the two fences alone, which live in functions both the real code and the model call.
