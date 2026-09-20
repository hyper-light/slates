# Telemetry drain test omitted the spans produced by draining

Date: 2026-09-20. Design: §4.14, AC-0.11. Scope: test convergence bound; no runtime change.

The full disposable Linux workspace run failed the daemon lifecycle test at `the drain
converges`. This was the telemetry scenario, not volume destruction. The original run did
not log its ring size or batch quota; its exact interleaving was not captured.

Two diagnostic reruns (one isolated, one with all ten daemon tests) passed with 512 held
spans and a reply quota of 50. Each drain emitted two new spans (`shard.op`, `log.append`),
so batches reduced the backlog by 48, not 50. A local-owner request may additionally emit
`ring.request`. The test already allowed these three spans in its final count, but its
round bound was `held / quota + 2`, ignoring their cumulative cost. Increasing a ring or
reducing a valid reply quota invalidates that bound.

A deterministic wire regression runs the same scenario at the normal quota and at one
more than the three possible drain-emitted spans. At quota 4 with 256 held spans, the old
assertion failed at round 67 with 118 still queued, while every batch reduced the backlog
by two. This proves the bound is wrong without claiming the original run's uncaptured
configuration was identical. Command (ordinary user, bounded to 120 seconds):

```sh
cargo test --offline -p slates-server --test daemon \
  the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals -- --nocapture
```

Environment: approved disposable Debian 13 / Linux 6.12.76-linuxkit / Rust 1.98.0 container,
four CPUs, 4 GiB, io_uring driver. Red: 4.03 seconds; log
`/private/tmp/slates-telemetry-drain-small-red.log`. Diagnostic logs:
`/private/tmp/slates-telemetry-drain.log`, `...-concurrent.log`.

Fix: derive net progress as `quota - maximum drain-emitted spans`, require it positive,
and bound subsequent rounds by `ceil(first.remaining / net_progress)`. Every reply must
also reduce the reported backlog by at least that amount (clamped at zero). Keep the
existing no-loss and total-span checks, using the same named emission bound. Temporary
per-batch diagnostics are removed. This changes no production quota or timeout.

Green: the same command passes in **5.58 seconds**. The normal quota drained 256 held
spans in six batches; quota four drained 512 held spans in 255 batches (1,020 returned
spans including the drains' own emissions). Log: `/private/tmp/slates-telemetry-drain-green.log`.
