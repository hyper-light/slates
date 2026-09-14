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
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom cargo test -p slates-mem -p slates-rt --lib --release loom -- --nocapture --test-threads=1
```

| Model (file, test) | Threads, shape | What every explored interleaving must keep | Bounded (2 preemptions) | Exhaustive (`LOOM_MAX_PREEMPTIONS=255`) |
|---|---|---|---|---|
| SPSC ring — `crates/mem/src/ring.rs`, `every_interleaving_of_one_producer_and_one_consumer_is_fifo_without_loss` (T-0.3) | 1 producer, 1 consumer; capacity 2; 3 words (a full ring, a retried refusal, one wrap) | FIFO; every word exactly once; nothing beyond the words pushed; the ring empty at the end; a full-ring refusal hands the word back and the retry lands it; the refusal path ran in some interleaving (non-vacuity counter) | 157 interleavings | 6,096 (equal to the unbounded run at c80b6f9 before the bound was applied in code) |
| MPSC ring, contention — `crates/mem/src/mpsc.rs`, `every_interleaving_of_two_contending_producers_keeps_each_order_and_loses_nothing` (T-0.3, "two producers") | 2 producers × 2 words, 1 consumer; capacity 4 (every word fits, isolating the claim race) | every word exactly once; each producer's order kept; nothing beyond the words pushed | 3,865 interleavings | did not finish inside a 600 s box (which is what the bound is for) |
| MPSC ring, lapping — `crates/mem/src/mpsc.rs`, `every_interleaving_of_a_producer_lapping_the_ring_is_fifo_without_loss` (T-0.3) | 1 producer × 3 words, 1 consumer; capacity 2 (a full ring, a retried refusal, the second lap of the sequence numbers) | FIFO; exactly once; the refusal retried; the refusal path ran in some interleaving | 26 interleavings | 192 |
| Handle core — `crates/mem/src/slab.rs`, `a_handle_returning_after_its_slot_was_reused_is_a_typed_miss_never_the_new_occupant` (AC-0.7 "handle cores"; T-0.1 under loom) | the owning shard and a peer; a one-slot slab; an SPSC ring out, an MPSC ring back | a request naming a handle either reads the occupant the handle was issued for or is refused `StaleHandle` with that handle's index and generation; it never reads the slot's new occupant; both outcomes reached in some interleaving | 6 interleavings | — (the two orders are the whole space) |
| Kick-if-parked — `crates/rt/src/parking.rs`, `a_word_published_while_the_shard_parks_is_never_lost` (AC-0.7; the CLAUDE.md "kick a shard only when it is parked" pattern) | 1 sender, 1 parking shard; the shard's MPSC ring; loom's `Notify` as the driver's kick (sticky and spurious-capable, like an eventfd count or an `EVFILT_USER` trigger) | the shard always receives the word: it saw it before waiting or was kicked out of its wait; a lost wake is a shard blocked with nothing runnable, which loom reports as a deadlock; kicks, skipped kicks and waits each reached in some interleaving | 27 interleavings | — |

Wall time for the five models under the bound: 0.12 s (mem, four models) + 0.00 s (rt) after the
build; the build under `--cfg loom` is ≈ 14 s cold.

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
and `Acquire` only and no re-check of the client rings after the announcement — reported, not fixed.

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

## 4. Owed

- **TSan nightly** (the design's Part 6 row): needs the nightly toolchain's `-Zsanitizer=thread`
  on a Linux target (`RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -Zbuild-std --target
  x86_64-unknown-linux-gnu -p slates-mem -p slates-rt`); not added, a lane for a Linux host to own.
- **The ipc request-direction parking sibling** (§1 above; the bug record's sibling sweep):
  a failing by-use test first ("a poller that becomes ready between the loop's last look and the
  park is woken without a driver wait"), then the fences and the extended pending check.
- **shuttle over two transport endpoints** (§3).
- **A loom model of the ipc rings' park/wake** is out of reach as written: the rings live in a
  shared-memory region behind std atomics over mapped bytes, which loom cannot instrument; the
  protocol's safety rests on the kernel's word compare, argued in the bug record.
