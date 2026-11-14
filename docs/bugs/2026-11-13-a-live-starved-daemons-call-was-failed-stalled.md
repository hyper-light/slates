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

**Step 2, built 2026-11-13.** A daemon with no anchor (`SegmentSource::Create`) ends its own process on its watchdog's
stuck verdict (`crates/server/src/watchdog.rs`, `decide` and the end; A-232's amendment). In
`crates/client/tests/lone_daemon.rs` a lone daemon whose client's shard was wedged exited 1.301 s and 1.340 s after the
wedge, within `W + 2·(B + L)`, and the outstanding call ended `DaemonGone`. With the self-end removed, on a copy of the
tree, the daemon was still running at the test's 10 s bound and the call still pending. On Linux (adacachy2) the wedged lone daemon exited 1.099 s
after the wedge. A lone daemon starved at nice 19 beside two spinners a CPU was ended in 0 of 10 runs on this Mac, each
run judged starved by the watchdog at least once.

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
