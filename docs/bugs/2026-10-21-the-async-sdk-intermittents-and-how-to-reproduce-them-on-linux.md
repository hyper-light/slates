# The async SDK's two intermittents, and how to reproduce them on Linux

**Status:** open. One measurement flaw fixed (`967fd387`); neither intermittent root-caused.
**Why this exists:** three hypotheses were tested and refuted, and a Linux reproduction harness now
exists. Without a record, the next person repeats all of it.

## The two failures

They are different, and conflating them wasted time.

| | symptom | where seen | rate |
|---|---|---|---|
| **A, the held loop** | `the loop was never held: worst lateness 0.298 s … bound 0.1 s plus … noise 0.001 s` | ubuntu CI, runs 39538566246 and 39539155335 | 2 of 10 |
| **B, the unstalled call** | `AssertionError: SlatesError not raised` | ubuntu CI 2026-10-02 and 2026-10-06; reproduced locally 2026-10-21 | 1 of 10 |

Both are in
`crates/sdk-python/tests/test_sdk_async.py::test_every_async_call_ends_across_restart_silence_cancellation_and_death`
(AUD-29-19/20).

## What was fixed

**A was partly a measurement flaw.** The bound read `ticker.worst` *after* `noise.stop()`, with the
ticker still running. `noise.stop()` blocks: `communicate()` closes the sampler child's stdin, reads to
EOF and waits for a whole Python interpreter to exit. That block landed in `ticker.worst`, and the
sampler had already stopped, so the bound's noise term could not account for it. Measured on an idle
machine, 5 trials: `communicate()` blocked 4.6 ms → lateness 3.7 ms; 10.9 → 10.6; 10.9 → 10.5. The
lateness tracks the block almost exactly while the sampler reports a flat ~8 ms.

Fixed in `967fd387` by reading the ticker before tearing the sampler down. **This is not claimed to
account for the full 0.298 s**; on an idle machine it is worth 3–11 ms.

## Three hypotheses, each refuted by measurement

Recorded so nobody re-walks them. Each looked right from the code.

1. **"`Client::send` reconnects inline, and `reconnect()` sleeps in `park_timeout` while the GIL is
   held."** The call chain is real — `begin_spin` → `driver.submit` → `Client::begin` → `send()` →
   `reconnect()`, which does `std::thread::park_timeout` in a retry loop, and the Python SDK calls
   `allow_threads` nowhere. **Refuted:** driven directly with the daemon killed for good, the call
   burned the full 2 s reconnect budget and ended `DaemonGone` after 3086 ms, and the loop's worst
   lateness was **2 ms**. The wait is driven asynchronously; there is no inline blocking reconnect.

2. **"Phase 1 — a daemon killed under outstanding calls, then restarted — holds the loop."** Driven
   directly, with a real reconnect (`client.reconnects() == 1`): worst lateness **5 ms**.

3. **"B is a race between `killpg(SIGSTOP)` returning and the daemon actually stopping."** Plausible:
   the test calls `client.list()` with no wait after the signal. **Refuted by A/B:** arm A (call
   straight after `killpg`, as the test does) stalled 12/12; arm B (poll `/proc/<pid>/stat` until the
   state is `T`, then call) stalled 12/12. Even trials that read state `R` at the call stalled
   correctly.

Also ruled out while reading, so not worth re-checking: `poll_reply_word` is non-blocking (buffer
check plus `try_take`), and `recv_mapping` — the only Linux-specific path in the client's poll — uses
`RecvFlags::DONTWAIT`.

## The clue that is left

**Phase 2 in isolation never fails (24/24 across both A/B arms), but the full test fails ~1 in 10.**
So B depends on state left by the preceding phases, not on phase 2 itself. Phase 1 submits
`outstanding_limit * 2` calls, has some refused at once, kills the daemon under the rest and gathers
them across the anchor's restart. A leftover buffered reply is the obvious next suspect — the client
buffers replies for other requests (`Client::buffer`) — but that is a suspicion with no log behind it,
and three suspicions have already died here.

An instrumented copy of the test prints the state at the phase-2 assertion. On twelve passing runs it
read, every time: `reconnects=1`, one daemon, state `T`, and `Stalled { after_ns: 1000000000 }`. The
failing shape has not yet been captured.

## The reproduction harness

This is the part worth keeping. Neither failure reproduces on macOS (phase 1 and the full test both
pass repeatedly); both are Linux. A native arm64 Linux container reproduces B at ~1 in 10 and costs
about a minute once warm.

```
mkdir -p /tmp/slates-lintarget
docker run --rm --memory=8g --cpus=6 \
  -v "$PWD":/src:ro -w /src \
  -v "$HOME/.cargo/registry":/usr/local/cargo/registry \
  -v /tmp/slates-lintarget:/tmp/lintarget \
  -e CARGO_TARGET_DIR=/tmp/lintarget -e CARGO_BUILD_JOBS=4 \
  rust:1.98 bash -c '
    apt-get -qq update >/dev/null && apt-get -qq install -y python3-venv >/dev/null
    cargo build --locked --bin slates
    python3 -m venv /tmp/venv
    /tmp/venv/bin/pip -q install "maturin>=1.5,<2"
    /tmp/venv/bin/maturin build -m crates/sdk-python/Cargo.toml --out /tmp/dist -i /tmp/venv/bin/python
    /tmp/venv/bin/pip -q install /tmp/dist/slates-*.whl
    cd crates/sdk-python/tests
    for i in $(seq 1 10); do
      SLATES_DAEMON=/tmp/lintarget/debug/slates \
        /tmp/venv/bin/python -m unittest test_sdk_async.SlatesAsyncEveryCallEnds
    done'
```

Notes that cost time to learn:

- The repo is mounted **read-only** and the target directory redirected to a host scratch path. Two
  agents were editing this checkout at the time; a container writing into it would have made that
  worse, and a persisted target directory makes the second run take 45 s instead of minutes.
- The `rust:1.98` image has `python3` but no `ensurepip`, so `python3 -m venv` fails until
  `python3-venv` is installed.
- **The Docker VM has network again** (checked 2026-10-21). A standing note said it did not, which is
  why this was not attempted sooner.
- To change the test without touching the shared tree, copy it to a scratch directory and mount that
  read-only beside the repo.

## Owed

- Capture the failing shape of B with the instrumentation, then root-cause it.
- Establish whether anything of A survives the measurement fix. The ticker now reports which of the
  four scenario phases held it, which the earlier failures could not say.
