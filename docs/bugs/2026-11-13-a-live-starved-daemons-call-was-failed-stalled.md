# A live, starved daemon's call was failed Stalled (2026-11-13, §4.7, A-56, A-232)

## Description

A client call waits one reply deadline, the liveness budget's wall second (`Deadlines::derive`), and a call the
daemon has not answered by then fails `ClientError::Stalled` unless the daemon is gone. This holds in all three of the
client's waits: the async driver's overdue tick (`crates/client/src/driver.rs`), the synchronous `round_trip` and the
ring-full wait in `send` (`crates/client/src/client.rs`). On a busy machine, the normal case, a daemon whose threads
wait for a CPU is alive. The daemon's own watchdog says so (A-228's rule: a starved shard is not a stuck one), but its
calls were failed `Stalled` all the same.

Found 2026-11-07 (GAPS, the starved-machine entry: `bootstrap root` refused "no reply within 1000000000 ns" in 5 of 15
runs at 8 spinners a CPU) and in Stack B1's gate (`presentation_fairness.rs`'s granted probe, `Stalled { after_ns:
1000000000 }` with its shard runnable in 6,581 of 6,582 samples; `83d64eb4` on `agents/reland-a207-9`,
`docs/bugs/2026-11-08-a-landings-plan-held-its-shard.md`).

## Base

`agents/clients-a232` is cut from origin's main `29ef4521d9711fd380b7fde45f63e83bac3166c7` (2026-11-09), by Ada's
decision of 2026-11-13: liveness's starved-is-not-stuck sha (after their B3), the base A-232's goal named, had not
been sent, and the branch moves onto it when it is. `git merge-base agents/clients-a232 origin/main` is `29ef4521`.

- r4 is on main: `agents/clients-r4`'s five commits landed rebased as `agents/clients-r4-c`, under the same subjects
  (`f0095051`, `4b74e610`, `1db9c9e4`, `1dd26f0f`, `573dddb9` on main; `git cherry` finds three of the five
  patch-identical). The branch ref `agents/clients-r4` still names the commits before that rebase, which is why it reads
  as five commits off main.
- `agents/clients-asynccore` (`9ee048b1`, async_core's waits judged by the daemon's own watchdog) had not landed. Its
  parent is main `29ef4521`, so the branch takes it as it is.

## Red (on main `29ef4521`)

The probe is diagnostics only and never on main: `crates/client/tests/a232_red.rs` in the tree of `diag-a232-red-4`
(`e553e306`). It runs an in-process daemon of two shards and a client in one process. Unstarved, it measures its
profile, starts the daemon and makes one volume, then says `A232-READY` and is starved. For 45 s it then calls `list`
and the volume's `status` in turn, with the product's deadlines (a 1 s reply deadline). It names every `Stalled` by
the daemon's own watchdog (`Daemon::watchdog_turns`, looks and withheld beats) before and after the call. A `Stalled`
with no beat withheld is a call failed against a daemon its own watchdog judged alive.

**Linux CI, `diag-a232-red-4` (`.github/workflows/diag-a232.yml` and `.github/diag-a232-red.sh` in its tree).** The
probe's whole process sits in a cgroup of `cpu.weight` 1. Beside it are 32 spinners, eight a CPU on 4 CPUs, each in a
sibling cgroup of its own at the default weight 100, as systemd units are. Each run kept 51 to 72 ms of CPU in the
starved cgroup.

| Runner | Runs red | Calls failed `Stalled`, no beat withheld | Those calls' time | `Stalled` with a beat withheld | Results |
|---|---|---|---|---|---|
| ubuntu-latest (x86_64) | 10 of 10 | 25 (1 to 3 a run) | 2.256 to 8.929 s | 0 | `diag-results/a232-red-4-ubuntu-latest` (`53e51a9a`) |
| ubuntu-24.04-arm | 10 of 10 | 19 (1 to 3 a run) | 2.152 to 4.786 s | 0 | `diag-results/a232-red-4-ubuntu-24.04-arm` (`0fadc8a3`) |

In every run the daemon logged its shards starved, never wedged or spinning, for example: "shard 1 is starved: no step
and no CPU progress for 5564 ms, past the 1000 ms window, while its thread waits for a CPU; the watchdog goes on
beating". `daemon_gone()` answered false after every such call.

**This Mac** (M5 Max, 18 CPUs, macOS 26.4.1). The same probe, built on `29ef4521`, ran at nice 19 beside 36 spinners
(two a CPU), 3 runs. Run 3 was red: one call failed `Stalled` after 1.334 s, with the watchdog's looks and withheld
beats at (42, 0) before and (46, 0) after, at load 198. Runs 1 and 2 answered every call: 2,942 and 11,697 calls. Run 1's
probe still made a volume a turn, and 1,381 of its `list`s were refused "reply too large for the bulk area".

**Lanes that did not answer, kept on record.**
- `diag-a232-red-1` (`af8bad9c`): two spinners a CPU in one weight-100 cgroup. The probe kept 1/101 of the machine,
  about 1.8 s of CPU a 53 s run. Calls took about 225 ms and none stalled: 0 of 10 on both runners.
- `diag-a232-red-2` (`08162d19`): the probe was starved from its first instruction, and its machine profile was refused
  before any daemon existed. It got about 0.1 s of CPU in 114 s, 10 of 10 runs on x86_64; arm64 never published.
- `diag-a232-red-3` (`ffe21f35`): the script moved `timeout`'s pid rather than the probe's, so nothing was starved:
  0 ms in the starved cgroup, every call answered, 0 of 10 on both runners.

## Root cause

The reply deadline is a wall clock judged by the client alone. It measures how the machine schedules the daemon's
threads, not whether the daemon is working. The daemon already tells starved from stuck (A-228), and its supervisor
ends a stuck one (the anchor's lapse), but the client did not wait on that judgment. A-56's rule says so in as many
words: "an overdue call fails `Stalled` while the daemon lives", and "a stopped one stays a stall".

## Fix

Owed, in A-232's plan (plan `01a1d6ca960575d0a36250c5f0a22500` in kiln, steps 2 to 6). A call waits while its daemon
lives. A daemon judged stuck is ended by its supervisor: the anchor, or for a daemon run alone or in-process its own
watchdog, on the same verdict, window and starved-is-not-stuck rule. The client then sees `DaemonGone`. The green, the
mutations and the bound (t0 + 2·W + B + L + kill) are recorded here when they are measured.

### Step 2: a lone daemon ends itself (`agents/clients-a232`, `6aaded0c`)

**Step 2, built 2026-11-13.** A daemon no other process can end (`SegmentSource::Create`, and `Handoff`, an anchor in
its own process) ends its own process on its watchdog's stuck verdict (`crates/server/src/watchdog.rs`, `decide` and
the end; A-232's amendment). In `crates/client/tests/lone_daemon.rs` a lone daemon whose client's shard was wedged
exited 1.412 s after the wedge, within `W + B + L` (1.465 s, `L` the verdict's lateness its end line names, at load
100). The outstanding call ended `DaemonGone`. With the self-end removed, on a copy of the tree, the daemon was still
running at the test's 10 s bound and the call still pending. Starved, it was ended in 0 of 10 runs at nice 19 beside
two spinners a CPU on this Mac and 0 of 10 at `cpu.weight` 1 beside default-weight spinner units on Linux
(adacachy2), each run judged starved by the watchdog.

### Step 3: the client's one rule (`agents/clients-a232-s3`, on `3c17e017`)

Every wait a call makes now keeps one rule: each reply deadline that passes asks the daemon's liveness; gone, the
client recovers or reconnects and fails `DaemonGone` past the reconnect budget, as before; alive, the deadline re-arms
and the call goes on waiting. The three waits:

- the async driver's tick (`crates/client/src/driver.rs`): an overdue call, sent or queued, is re-armed while the
  daemon lives; `submit_patient` and the `patient` flag are gone, and both SDKs' `land` submits as every verb does;
- the synchronous `round_trip` (`crates/client/src/client.rs`): the `defers_reply` arm and the `Stalled` arm are gone;
- `send`'s wait for a slot on a full ring: it spins for the first reply deadline as before, then asks the liveness at
  each deadline and, while the daemon lives, checks for a slot paced as the reconnect paces (`next_pause_ns` over the
  reply deadline: doubling from the wake estimate, at most a tenth of it), so it costs no core beside a starved daemon.

`ClientError::Stalled` had no other producer and is removed, with `defers_reply` and `Client::defers`, which nothing
consults once the waits share the rule. `load_bench` waits for a live daemon's reply too (its stall dump still names a
worker whose replies stop) and no longer retries on `Stalled`; `recovery.rs`'s `read_answered` reads once.

The tests, each driving the real client against a real daemon. A held daemon is a one-shard daemon whose shard
`Daemon::wedge_control_shard` holds off its CPU (its thread asleep) for half the watchdog's window, with the client's
reply deadline a fifth of the hold: the call is overdue five times over while the watchdog, which calls a shard stuck
only after a whole window without movement, judges the daemon alive, and each test asserts it withheld no beat.

- `driver.rs`, `a_held_daemons_calls_are_waited_for_and_answered_and_a_cancel_releases_at_once`: twice the outstanding
  bound submitted (a ring sent, a ring queued), one sent and one queued call cancelled after two deadlines (released at
  once, never reported), every other call answered, the loop's longest step within a tenth of the reply deadline unless
  preempted. The restart, death and overflow test and the ready-once test are unchanged.
- `waits.rs` (new): a round trip and a send on a full ring (proved full: a call begun after an abandon is refused
  `RingFull`) to the held daemon, both answered; and both to a daemon `SIGKILL`ed in a child process (this binary
  re-invoked), both `DaemonGone` past the reconnect budget (the control, green before and after).

Red on `3c17e017` (macOS arm64, 2026-11-13, load 40): the driver test's calls all `Stalled { after_ns: 100000000 }` at
the first deadline; the round trip `Stalled` after 102 ms; the full-ring send `Stalled` after 100 ms; the killed-daemon
control passed. Green after the change: driver 3 of 3, waits 3 of 3; the whole client suite (18 binaries) and
`landing_fairness`'s landing test and `recovery.rs`'s three `read_answered` tests pass, load 30 to 47. Mutations, each
on a copy of the tree, each restoring a live-daemon failure at one wait: the driver tick turned the driver test red
(`ChannelLost` at the first deadline), `round_trip` the round-trip test (`RingFull` after 102 ms), `send` the full-ring
test (`RingFull` after 112 ms). Logs: `~/.cache/slates-agents-scratch/a232/evidence/s3/` (`red-base.log`, `green.log`,
`mutations.log`, `client-suite.log`, `server-tests.log`). Gates: `cargo fmt --all`, `cargo clippy --workspace
--all-targets -- -D warnings` and with `-p slates-sdk-node -p slates-sdk-python`, `cargo xtask check`, and CI's
`cargo doc --workspace --no-deps --document-private-items`, all clean on macOS arm64.

Owed by later steps: the SDK suites' two stopped-group cases each still assert `Stalled`, and with this rule they
wait instead (step 4); the design text, GAPS and both SDKs' docs still state the old rule (step 6).

### Step 5: the rule and its bound, live (`agents/clients-a232-s5`, on step 4 with step 2's `6aaded0c` merged)

Three end-to-end tests, one per shape, each against real processes:

- **Stuck, supervised** (`crates/cli/src/liveness_tests.rs`,
  `an_anchored_daemons_wedged_shard_is_ended_within_the_liveness_bound_and_its_call_answered_after`). The test binary is
  the anchor. A child runs `anchor::supervise`, which is the `slates anchor` path after its arguments: `anchor::run` now
  builds the `slates daemon` arguments and calls it. It supervises this binary re-invoked as the daemon
  (`daemon::start` then `daemon::serve`, split from `daemon::run` unchanged; `SegmentSource::FromEnv`, so supervised).
  The first daemon wedges the shard that serves the parent's client in one poll, on a read of a pipe nobody writes.
  `t0` is the host's monotonic clock read just before that read, which every process on the host shares. The test
  asserts four things: the watchdog's wedged line says it withholds the beat; the anchor's kill line arrives within
  `t0 + 2·W + B + L`; the client's liveness answers gone `kill` after that line; and the call is then answered by the
  daemon the anchor restarts, after one reconnect, within the reply deadline and the reconnect budget.
- **Stuck, unsupervised** (`crates/client/tests/lone_daemon.rs`, step 2's child-role daemon,
  `SegmentSource::Create`). `t0` is read the same way. The watchdog's end line must arrive within `t0 + W + B + L`. The
  call ends `DaemonGone` past the reconnect budget.
- **Starved, supervised** (`an_anchored_daemon_starved_beside_spinners_fails_no_call_and_is_never_killed`, heavy, run
  only with `SLATES_A232_STARVE=1`). The anchor and its daemon are held off their CPUs: on Linux CI they are moved into
  a cgroup of `cpu.weight` 1 beside eight default-weight spinners a CPU, each spinner in its own cgroup; elsewhere they
  run at nice 19 beside two spinners a CPU. For 45 windows the test calls `list`, `status` and an async `list` in turn.
  No call may fail, the anchor may kill nothing, there may be no reconnect, and the run must have starved something.

**The bound, by name.** `W` is the liveness budget and `B` the beat. The verdict comes within `W + B + L` of a wedge at
`t0`, `L` the verdict's own lateness (step 2's rule: measured from the look before the shard last moved, which came
before `t0`; the watchdog's start counts as that look when there was no other). A shard wedged at `t0` is first seen
still at the next look `a1`, a beat and that look's lateness after the look before it, and the beat is withheld from
the first look a whole window after `a1`. So an anchored daemon's last beat comes within `W + B + late(a1)`. The anchor kills once the beat is older
than `W` plus its own lateness p99, at a look its own lateness makes late too, and its kill line logs that age. So the
supervised bound is `t0 + 2·W + B + L + kill`, with `L` the watchdog's first-look lateness plus the anchor's logged age
past the window (its p99 and its overshoot). `kill` is the measured time from the kill line to the client's liveness
answering gone. The watchdog's withhold line names both of its terms: "the verdict came N us past a window and a beat
after the watchdog's last look before the shard last moved; the look that last saw it move came M us past a beat after
the one before it".

**Where the plan's words were short, measured.**
- *Supervised.* With `L` the anchor's p99 alone, as the plan worded it, the bound failed on this Mac in 5 of 5 runs, by
  0.3 to 98 ms, at load 15 to 60. The watchdog's 100 ms looks came back about 245 ms apart under kiln's launchd timer
  policy, and its first look after the wedge was 124 to 150 ms late (`macos-timed-waits-stretched`). With the
  watchdog's term the same runs passed 10 of 10, 50 to 230 ms inside the bound. On Linux CI the watchdog's own lateness
  is microseconds, and the two readings of `L` agree to within it.
- *Unsupervised.* Read as the client's call ending `DaemonGone`, the plan's `W + B + L` cannot hold: the call ends past
  the reconnect budget, `reconnect_ns = RECOVERY_BUDGET_NS + W`, after the exit. "DaemonGone time" is therefore the
  instant the client's liveness first answers gone. On Linux the test waits on the daemon's control socket and reads
  it as it closes (`Client::daemon_watch`); elsewhere it polls at each step of its loop, a hundredth of a beat. The
  verdict-to-gone time `end` is measured and named as the supervised case's `kill` is. The test asserts the verdict's
  line by `t0 + W + B + L` and reports gone against both `W + B + L + end` and `W + B + L`.
- *Supervised, the call.* An anchor restarts the daemon it kills, so the outstanding call is resent under its id to
  the successor and answered. It does not end `DaemonGone`. The test asserts it is answered after one reconnect.
- *A shard's first look.* A wedge 2.8 ms after a daemon began serving was first seen 187 ms later while the line named
  0 us of lateness (macOS): the first look had no look before it. Step 2 found the same and begins each shard's watch
  at the watchdog's own start (`ShardWatch::begun_at`), which this branch takes from it (`d8df0952`).
- *`t0` read before a write.* The role had printed its held line from inside the poll, then blocked. On a loaded Mac
  the shard lost its CPU after the write and blocked 51 to 55 ms after the time it named, in 2 of 3 runs. `t0` is now
  the last read before the block, and another thread prints it.

**The anchor killed a starved daemon (found by this step, fixed in it).** Round 1 on Linux CI (`diag-a232-starved-1`
on `bd84c944`) answered every call to a live daemon, but in 3 of its 20 starved runs (x86_64 run 6, arm64 runs 6 and 7)
the anchor killed the starved daemon: "the daemon's heartbeat lapsed (3495 ms old, past 2518 ms: the 1000 ms window
plus this anchor's lateness p99 of 1518 ms over 16 waits); killing it". The restarted daemon, starved too, did not
answer within the client's 2 s reconnect budget, so 11, 5 and 7 calls ended `DaemonGone`. At `cpu.weight` 1 the
watchdog thread is held off its CPU with the rest of the daemon. Its beat then lapses past the window and the anchor's
own lateness p99, and the anchor could not tell that from a withheld beat. Mutation (a)'s run 3, which changes the
client alone, met the same kill.

The fix (`a7c479b3`). The watchdog marks every look in the segment, beating or not: `SUP_LOOKED`, the supervision
block's word at offset 216, zeroed at each start. On a lapse the anchor decides by `judge_lapse`:
- Looks within the lapse bound: the watchdog's verdict, killed as before.
- No look within it: the daemon's process is judged by the start's rule, in its own CPU time, from when the looks
  stopped. Starved or alive, it is spared and looked at every beat. Wedged (no CPU progress, never runnable, as a
  stopped process) or spinning for a window, it is killed, by a line of its own.

A daemon stopped whole (SIGSTOP) is still killed that way, 1.88 to 1.93 s after the stop (macOS 5 of 5): the lapse,
then a window of the process's own CPU time.

**adacachy2 is not a place to judge a wedge from `t0`.** It pauses whole processes for about 1.16 s at a time. A plain
Python probe, one thread blocked on a pipe read and a 100 ms sleeper, saw 2 gaps of 1.164 and 1.165 s in 20 s, and the
blocked thread was charged 8.6 to 11.7 us of CPU across each. Its run cgroup's freezer bits stayed 0 throughout. Such a
pause wakes the wedged shard, so the watchdog rightly sees it move after `t0`: 35 of 35 supervised runs there failed in
one stretch, the shard last seen moving 1.4 to 5.2 s after `t0`. Its Linux runs therefore went to CI.

**Linux CI, the green lanes** (`.github/workflows/diag-a232-step5.yml` and `.github/diag-a232-step5.sh` in each lane's
tree; `ubuntu-latest` x86_64 and `ubuntu-24.04-arm`, 4 CPUs each; results on `diag-results/<lane>-<os>`, read with git).
The stuck shapes ran on an unloaded runner, 10 runs each. The starved shape then ran 10 runs with the anchor and its
daemon in a cgroup of `cpu.weight` 1 beside 32 spinners in 32 sibling cgroups of weight 100 (load 23 to 37); each run
kept 39 to 60 ms of CPU in the starved cgroup over its 45 windows.

| Lane, tree | Shape | x86_64 | arm64 |
|---|---|---|---|
| `diag-a232-starved-1`, `bd84c944` | stuck, supervised | 10 of 10; kill line 100.4 to 102.5 ms inside `2W + B + L` | 10 of 10; 100.4 to 101.8 ms inside |
| | stuck, unsupervised | 10 of 10; verdict line 2.8 to 4.4 ms inside `W + B + L` | 10 of 10; 2.9 to 6.2 ms inside |
| | starved | 9 of 10: run 6 killed by the anchor, 11 calls `DaemonGone` | 8 of 10: runs 6, 7 killed, 5 and 7 calls `DaemonGone` |
| `diag-a232-starved-2`, `a7c479b3` | stuck, supervised | 10 of 10; 99.8 to 102.1 ms inside; `L` 1.07 to 1.09 ms, `kill` 1.0 to 2.2 ms | 10 of 10; 90.0 to 102.2 ms inside |
| | stuck, unsupervised | 10 of 10; 2.7 to 4.6 ms inside | 10 of 10; 3.2 to 4.2 ms inside |
| | frozen (SIGSTOP) | 10 of 10; killed by its process judgment 1.90 to 2.00 s after the stop | 10 of 10 |
| | starved | 10 of 10: 108 calls, 0 failed, 0 kills, 0 reconnects; the anchor spared a silent watchdog 8 times | 10 of 10: 123 calls, 0 failed, 0 kills, 0 reconnects; spared 2 times |

Every starved run was non-vacuous: its longest call waited 4.9 to 14.6 s against the 1 s reply deadline, and the
watchdog judged a shard starved 1 to 17 times a run. No call failed `Stalled` in any run. Before step 3 every such run
failed (step 1's red, 10 of 10 on each runner).

**The unsupervised bound in the plan's words.** "DaemonGone time minus `t0` minus `(W + B + L)`" exceeds zero in 15 of
40 CI runs (round 1: 9 of 10 x86_64, 1 of 10 arm64; round 2: 0 of 10, 5 of 10), by 0.005 to 1.08 ms. The exit is the
missing term. From the watchdog's end line to the client's liveness answering gone takes `end` = 1.7 to 4.6 ms: the
log write, `exit`, and the kernel's teardown before the control socket closes. With `end` named, as the supervised
bound names `kill`, every run is inside, by the verdict line's own margin.

**Mutations, each a lane of its own on Linux CI, each turning its own shape red:**

| Lane | Mutation | Shape | x86_64 | arm64 |
|---|---|---|---|---|
| `diag-a232-starved-ma-1`, on `bd84c944` | (a) the client's `Stalled` branch restored (`crates/client/src` as of `3c17e017`, step 1's base) | starved | 10 of 10 red: 8 to 17 calls a run "no reply within 1000000000 ns", daemon alive | 10 of 10 red: 10 to 15 calls a run |
| `diag-a232-starved-mb-1`, on `bd84c944` | (b) the unsupervised self-end removed | unsupervised | 10 of 10 red: "still running 10s after its shard was held; the call: None (still pending)" | 10 of 10 red, the same |
| `diag-a232-starved-mc-1`, on `bd84c944` | (c) a starved verdict treated as stuck | starved | 4 of 10 red (runs 1, 5, 6, 8: the anchor kills the live daemon, 3 to 11 calls `DaemonGone`); in the other 6 a starved verdict never lasted until the beat lapsed | 10 of 10 red, 7 to 12 calls a run |
| `diag-a232-starved-ma-2`, on `a7c479b3` | (a), as above | starved | 10 of 10 red, 7 to 18 calls a run | 10 of 10 red, 10 to 13 calls a run |
| `diag-a232-starved-mb-2`, on `a7c479b3` | (b), as above | unsupervised | 10 of 10 red, the same words | 10 of 10 red |
| `diag-a232-starved-mc-2`, on `a7c479b3` | (c), as above | starved | 7 of 10 red: the watchdog, looking, withholds the beat on a starved shard and the anchor kills the live daemon | 9 of 10 red |
| `diag-a232-starved-md-1`, on `a7c479b3` | (d) the anchor ignores the watchdog's looks: every lapse kills, the rule before this step | starved | 1 of 10 red (run 5: killed, 13 calls `DaemonGone`) | 1 of 10 red (run 10: 11 calls) |

Mutation (d)'s rate, 2 of 20, matches round 1's before the fix, 3 of 20. The stronger evidence for the fix is the green
round 2 itself: in 5 of its 20 starved runs (x86_64 runs 3, 4, 5; arm64 runs 2, 5) the anchor met a lapsed beat with a
silent watchdog and spared the daemon, and under the rule before this step each of those lapses was a kill.
