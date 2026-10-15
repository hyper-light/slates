# A back channel being probed was reported down, and every Linux client started a storm of probes

**Found:** 2026-10-09, profiling the daemon during reads through Linux's NFSv4.2 client (`perf`: `clock_gettime` at 5.9%
of samples, under `probe_back_channel` and its sleeps). **Status: fixed** (same day).

## Description

The daemon's callback counters after six seconds of reads on one mount:

| counter | count |
|---|---|
| `nfs4.callback.down` | 7,210 |
| `nfs4.callback.timeout` | 4,596 |
| `nfs4.callback.delayed` | 8,127 |
| `nfs4.callback.resynced` | 3,258 |
| `nfs4.callback.waited_for_slot` | 556,803 |
| `nfs4.callback.up` | 16 |

4,596 timeouts of a one-second liveness budget in six seconds means thousands of probes were in flight at once, each
retrying the one callback slot every poll interval.

## Root cause

Two changes, each correct alone, made a loop:

- `bind_back_channel` (`crates/server/src/nfs.rs`) marked an unproven back channel `Down` while its first probe ran,
  so that a second compound would not start another probe.
- A-124 (2026-10-08) made a `Down` back channel visible to the client: every `SEQUENCE` reply carries
  `SEQ4_STATUS_CB_PATH_DOWN_SESSION` and `SEQ4_STATUS_CB_PATH_DOWN`, and `BIND_CONN_TO_SESSION` makes it unproven again
  so it is re-probed.

So the loop ran:
1. every probe told the client its back channel was down;
2. the Linux client rebinds on either flag (`nfs41_handle_cb_path_down`);
3. the rebind made the channel unproven;
4. the next compound started another probe beside the one still running.

The probes contended for slot 0, and the client answered them `NFS4ERR_DELAY` and misordered, so they failed, which
fed the loop.

## Impact

- CPU on every Linux NFSv4 mount: the probes' clock reads and slot retries were about 3.3% of the daemon's samples
  during reads, plus the probes and rebinds themselves on the wire.
- Delegations: a channel that is never `Up` gets no delegation (RFC 8881 §10.2), and recalls ride the same flapping
  channel. Docker and Kubernetes mounts use this client, so condition 2.

## Exact edits

- `crates/bridge-nfs/src/v4/session.rs`:
  - `CallbackState::Probing` is appended: a probe in flight, neither up nor down for `callback_path_down`.
  - `Sessions::claim_probe` moves `Unproven` to `Probing` and says whether this caller starts the probe.
  - `rebind_back_channel` leaves `Probing` as it is, so the probe in flight decides.
- `crates/server/src/nfs.rs` `bind_back_channel`:
  - it starts a probe only when it claims one, counted as `nfs4.callback.probes`;
  - if the spawn is refused, the channel is marked down.
- Test: `a_back_channel_being_probed_is_not_reported_down_and_is_probed_once` (`crates/server/tests/nfs_mount.rs`).
  It rebinds a proven back channel, holds the probe that follows unanswered, sends more compounds, then rebinds again.
  No reply may report the channel down, exactly one probe may start, and the channel must be up once the probe is
  answered. It failed before the fix: the compound that started the probe carried flags `0x201`.
