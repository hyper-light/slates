# A readiness wait took any wake of its task for its own

**Status:** fixed 2026-10-22 (A-149, approved by Ada in plan mode). The root cause was shown by a syscall trace and by
three tests that failed before the fix.

**Severity:** a daemon serving two FUSE mounts of one volume used a whole CPU core, forever, after one write through
either mount. That costs a laptop's battery, and on a server a core taken from the guests and builds beside it. It
happens wherever two mounts share a volume: the coherence tests, a guest's share beside a host mount, an agent with
two views.

The same cause made a deadline raced against a bare readiness wait unreportable: `futures::within(ns,
readable(fd))` could never answer that its deadline passed.

**Found:** 2026-10-22 on the box (Linux 7.2.9, x86_64). While measuring the truncate-under-DAX hang, the daemon
showed 129 to 200 clock ticks per 2 s when it should have been idle.

## Reproduction and evidence

The daemon was given zero, one and two FUSE mounts of one volume, with a 32 KiB file written through the first
mount. Its threads' CPU was then read over 4 s, 8 s after the last request:

| mounts | `slates-shard-0` | `slates-shard-1` |
|---|---|---|
| 0 | 1 tick | — |
| 1 | 1 tick | 1 tick |
| 2 | 1 tick | **397 ticks** (of 400), state `R` |

With two mounts and no write, the daemon stayed idle.

`perf trace -s` of the spinning thread over 2 s:

| call | calls | errors |
|---|---|---|
| `epoll_ctl` | 369 947 | 184 976 |
| `clock_gettime` | 184 965 | 0 |
| `getrusage` | 184 974 | 0 |
| `read` | 92 510 | 92 510 |
| `epoll_pwait` | 92 529 | 0 |

The individual calls repeat one round about every 21 µs:
1. `epoll_pwait` returns one event at once.
2. The shard's step attribution reads its clocks.
3. `read` of the second mount's `/dev/fuse` returns `EAGAIN`.
4. `epoll_ctl` re-arms the device (`ADD` refused `EEXIST`, then `MOD`), and then re-arms the second mount's change
   eventfd the same way.

At the same moment:
- the change eventfd's count read 1 (`/proc/<pid>/fdinfo`);
- epoll held its one-shot registration disarmed (`events: 40000000`), so it was the descriptor that fired;
- the device's registration was still armed;
- no `read` of the eventfd appears anywhere in the trace, so nothing ever drained it.

## Root cause

`slates_rt::readiness::Ready::poll` (`crates/rt/src/readiness.rs`) returns ready on any poll after it registered
("the driver woke us"). A driver's completion carries only the waking task's word, so the shard cannot tell which
of a task's registrations fired.

The FUSE serve loop (`crates/server/src/fuse.rs` `wait_for_work`) races its device's readiness, polled first,
against its change signal's. A change made through the other mount fires the signal, which wakes the task. The
device's future then answers ready, so the loop takes a request turn, reads nothing (`EAGAIN`) and goes back to
waiting. Only a change turn drains the signal, and the loop never takes one. So the count stays 1, the re-armed
one-shot registration fires again at once, and the loop repeats forever.

**Sibling, same cause:** `futures::within` polls its work before its deadline, so the deadline's own wake is taken
for readiness, and `within(ns, readable(fd))` answers `Ok(Some(..))` when its deadline passes. In production:
- The three callers that race a socket *read* against a deadline (`crates/mcp/src/http.rs` keep-alive,
  `crates/server/src/member_task.rs`, `crates/server/src/dns.rs`) still time out. The read's own loop retries the
  syscall, re-registers, and the deadline is then seen. Each deadline costs them one wasted syscall and one
  registration.
- `crates/server/src/fuse.rs:360` races a bare readiness wait, and checks the deadline itself on the next turn.

## Tests that fail before the fix

- `crates/rt/tests/readiness.rs` `a_task_racing_two_descriptors_learns_which_one_became_readable` fails with
  `left: Quiet`, `right: Signalled` (macOS kqueue, and Linux epoll on the box).
- `crates/rt/tests/readiness.rs` `a_deadline_on_a_readiness_wait_that_never_comes_is_reported_as_the_deadline`
  fails: readiness is reported at the deadline (macOS and Linux).
- `crates/server/tests/fuse_idle.rs` `a_write_through_one_of_two_fuse_mounts_leaves_the_daemon_idle_afterwards`
  fails with "the daemon's shards used 86 of 100 clock ticks in the 1s after the mounts had served the write" (the
  box).

## The fix (A-149)

A registration hands the driver an **interest word** instead of the task's. It carries the owning shard in the shard
field, the slot field's top 15 bits marked, then the direction and the descriptor. The shard's table maps the
descriptor and direction to the task waiting, its own token, and whether it fired. A readiness future answers ready
only when its own record fired. Every path that turns a word into a wake decodes interest words the same way: both
completion harvests, a same-shard wake (the simulation's fabric wakes through the registry), and a word from another
shard.

Rejected: probing the descriptor with a zero-timeout `poll(2)` on each wake. It attributes as exactly, at one syscall
per wake on every socket and device; the interest word costs a map lookup.

## What leaned on the old behaviour

Every readiness consumer and every stored waker was swept, to find code that relied on a readiness future answering on
another source's wake. One did: the virtio-fs device loop (`crates/bridge-virtiofs/src/serve.rs` `serve_round`).
- A revoke (`request_revoke`: a guest device's `detach` or its volume's destroy) and a drain kick (`kick`: a
  `snapshot` recall of writable DAX windows) set a flag and wake the waker the loop recorded.
- That wake interrupted the loop's doorbell wait only because the doorbell's future answered on any wake.
- With the runtime fix alone, a loop parked on an idle guest's doorbell kept serving a detached device until the guest
  kicked again. `a_revoke_request_ends_a_loop_parked_on_an_idle_guests_doorbell` was red at that point (the loop had
  not ended when the test's 10 s wait expired). The existing revoke test passed only because its revoke landed between
  passes.
- The wait now races the doorbell's readiness against the shard's requests explicitly (`doorbell_or_request`).

The others wait on their own state:
- the transport's receive loop and `recv_within`;
- the three socket reads raced against deadlines;
- the rendezvous accept loop;
- the landing's barrier wait (`futures::idle`);
- the cross-shard, callback and inbox waiters.

## Exact edits

- `crates/mem/src/handle.rs`: `Encoded::interest`, `Encoded::as_interest`, and `MAX_TASK_SLOT`; `pack` refuses the
  marked slots.
- `crates/rt/src/interest.rs` (new): `Interests` (`register`, `fire`, `take`, `forget`, `unclaimed`), `Direction`, and
  `Taken`.
- `crates/rt/src/shard.rs`:
  - `ShardContext.interests`, kept apart from `inner`;
  - `register_interest`, `register_readable` (its public form), `take_interest`, `forget_interest`, `deliver` and
    `with_interests`;
  - both completion harvests and `handle_wake` deliver;
  - `Counters::interests_unclaimed`;
  - `ShardSeed::register` refuses a task arena larger than a wake word can name.
- `crates/rt/src/registry.rs`: a same-shard `wake` delivers.
- `crates/rt/src/readiness.rs`: `Ready` holds its token, answers only on `Fired`, and forgets its record on drop.
- `crates/rt/src/error.rs`: `RtError::InterestsExhausted`.
- `crates/bridge-virtiofs/src/serve.rs`: `doorbell_or_request`, `request_pending` and `Woke`.
- The tests:
  - `crates/rt/tests/readiness.rs` (three);
  - `crates/server/tests/fuse_idle.rs`;
  - `crates/bridge-virtiofs/tests/serve.rs` `a_revoke_request_ends_a_loop_parked_on_an_idle_guests_doorbell`;
  - the unit tests of the table and the layout.
- `.github/workflows/ci.yml`: the runtime's Miri step runs `--test readiness` (the simulated race; the OS-driver tests
  are ignored under Miri).
- Docs: `SLATES_DESIGN.md` §4.3 and A-149, GAPS, CLAUDE.md.

## Verification

| | before | after |
|---|---|---|
| the two OS-driver races, macOS kqueue and Linux epoll | `left: Quiet`, `right: Signalled`; readiness at the deadline | green |
| all three races with a registration counted fired on any wake | — | red again |
| `fuse_idle` (the box) | 86 of 100 ticks in 1 s | green |
| the virtio-fs revoke test, runtime fix alone | the loop not ended within 10 s | green with the explicit race |
| the two-mount daemon after a write (the box) | 397 of 400 ticks over 4 s | the busier shard 2 ticks in all over a 6 s trace with no request |

- **Miri:** clean on the `mem` library (88), the `rt` library (34), `differential`, `ownership`, `timers` and the
  simulated race.
- **macOS:**
  - the `rt` and `mem` suites (232);
  - the virtio-fs suite (76);
  - workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and fmt;
  - the arm64 Windows cross-lint of `wire`, `mem`, `rt` and `ipc`.
- **The box (Linux 7.2.9):**
  - the `rt` and `mem` suites (239);
  - virtio-fs (93);
  - the server's `nfs_mount` (39), `nfs_hostile` (17), `fuse_mount` (3), `fuse_idle`, `virtiofs` (18) and
    `attach_forms` (9);
  - the client (23), `coherence_mount` (7), the CLI suite (51) and the guest tests (3);
  - workspace clippy `-D warnings`.

## Observed, not this bug

After the fix, one two-mount run showed 81 ticks over 4 s on the busier shard. The cause was requests, not a loop:
- the second mount's kernel sent two `LOOKUP`s about a second apart, answered `ENOENT`;
- the shard spun out its idle window after them (A-42) and parked;
- their sizes match `.Trash` and `.Trash-1000`, the probes of the box's KDE Plasma session. That is inferred from the
  sizes, not read from the requests.

The idle window's length after a lone request is owed a measurement (GAPS).
