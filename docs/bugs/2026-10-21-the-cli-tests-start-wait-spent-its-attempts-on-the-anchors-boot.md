# The CLI tests' start wait spent its attempts on the anchor's own boot

**Status:** fixed (this change). Test harness only; no product defect.
**Design:** A-114 (the anchor holds the client rendezvous), A-119 (the anchor never gives up on its daemon), R5.
**Found:** 2026-10-21, verifying the FUSE change-delivery fix
(`2026-10-21-a-fuse-mount-hears-of-another-paths-change-only-on-its-next-request.md`) on a loaded Mac.

## Failure and evidence

On an 18-core Mac carrying other sessions' builds (load 25–110), CLI tests failed inside `start_anchor` with
"the daemon came up within 60 attempts (last exit 3)": **12 of 15 starts** on 2026-10-21. In most of the failed
runs the anchor had printed nothing at all — not even its greeting — when the budget ran out (7.7 s). In one, the
daemon was answering when the budget ran out ("last exit 0"): the streak of 10 was still being counted.

Measured beside them, with an anchor started exactly as the fixture starts it and `volume list` polled every 20 ms
as the fixture polls, each attempt's start compared with the anchor's greeting and the daemon's serving line:

| load (1 min) | anchor greeting after spawn | attempts refused before it | refused after the serving line |
|---|---|---|---|
| 53–57 | 2.5 s | 21–22 | — (not recorded) |
| 99–110 | 4.6–5.4 s | 46–51 | — (not recorded) |
| 67–78 | 3.6–4.4 s | 30–39 | **0 of 39** |

At load 67–78, 1–2 attempts were refused between the greeting and the serving line in each of the four rounds,
and none after it.

## Root cause

Until the daemon publishes its rendezvous object, a client is refused at once: `open_bootstrap` answers "the
rendezvous object is not there" before the anchor holds it, and "the daemon has not yet published its rendezvous
object" after. Once it is published, a client of a live anchor waits out a daemon restart instead (A-114). So
refused attempts were fast and numerous exactly while the anchor and its daemon booted.

`START_ATTEMPTS` counted attempts from the spawn, on the reasoning (2026-10-18, `c0ab2345`) that "a contended host
makes each attempt slower, not more numerous". That holds only once the daemon serves. Before it, the budget
measured the boot in units of fast refusals, which is wall time in disguise, and a loaded host's boot is longer
than 60 of them.

## Impact

Any test that starts through `start_anchor`, `start_anchor_with_environment` or `start_anchor_with_issuer_surface`
— the CLI suite, which the gates run on ubuntu and macOS — could fail falsely on a loaded host. A failure there read
as "the daemon never came up", which points away from the harness.

## Fix

`await_serving` (`crates/cli/tests/cli.rs`) reads the anchor's piped stderr until the daemon's serving line, and,
when asked on a platform that prints one, until the anchor's issuer surface too — in whichever order they arrive,
since two processes print them. It echoes every line, so the anchor's output still lands on the test's stderr.
Only then does the same attempt count run (`await_stable_daemon`). The wait ends the test instead of waiting on
when the anchor ends first (its stderr closes) or reports its daemon in a crash loop. The anchor never gives up on a
daemon (A-119), so a daemon that cannot serve shows up as that line rather than as an exit.

`read_issuer_surface`'s paired `cfg` functions became one `cfg!` constant, `PRINTS_ISSUER_SURFACE`. It keeps the
2026-10-20 lesson: an order assumed between two log lines broke this file twice.

## Verification

- macOS, load 19–73: 16 of 16 starts succeeded — six runs of the generated-sequence mount test, then five runs of
  both mount-route tests, every run passing once the test's own writer was fixed (below).
- Linux (arm64 container, 6 CPUs, 8 GB, `--device /dev/fuse`, fuse3): the whole CLI suite at two threads, seven
  runs. Six passed 42 of 42. The first failed one test, `a_destroyed_volumes_mount_refuses_writes_and_never_lets_them_reach_the_disk_beneath`,
  in its start: **the anchor's stderr closed before its daemon served**, so the anchor had exited during its boot.
  Its own lines were lost to the run's output filter, the kernel log shows no out-of-memory kill, and six further
  runs (252 test starts) and three of that test alone did not reproduce it. Under the old wait the same exit would
  have read "the daemon came up within 60 attempts". It is recorded here unexplained rather than given a cause. The
  wait now prints the anchor's own lines above that panic, so a recurrence carries its cause.

## Siblings (checked, not this flaw)

- `start_anchored_fleet_node`, the export fixture's `start_anchor` and the codec-derivation test start anchors
  without this wait; their callers wait their own way, by status or by the line they need.
- The same runs exposed a second defect, in the test rather than the harness: the mount-route fixture replaced its
  file through `slates write` under an attachment a `slates attach` process had made. An attachment belongs to its
  client and is removed once that client is reaped (`crates/server/src/reap.rs`; §4.7, D-16), so the replacements
  outlived it, refused `NotFound` at change 3 to 12 in five of six macOS runs. The fixture now holds an SDK client
  for its life. How long a CLI-made attachment should live is one of the open `slates run` lifetime questions for
  Ada, not decided here.
