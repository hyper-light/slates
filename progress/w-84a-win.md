# w-84a-win (goal 84a: every single run's p99 and p999 under 1 ms on Windows bare metal)

## 2026-11-24 ~06:30 PST
- A-897 LANDED fb50fbd61. edit_merge_bench prints a floor line (the OS's own wake of a parked thread), a cpu line per
  form, every run's p99/p999 beside w-perf's verdict, and a slow line per sample over 1 ms. The slow line splits on one
  clock into the client's send, the seat's pickup, the service and the client's take. The measured-and-rejected record
  (BENCHMARKS "84a on Windows bare metal: whose time a slow sample is"; GAPS 84a; design log A-897):
  - 143 invocations. No scheduling change on slates' side holds every run on jupiter: shard tail window, client spin,
    seat hold, holds off, and the client at ABOVE_NORMAL, HIGHEST or TIME_CRITICAL or fixed to a CPU.
  - Main failed 1/96 loaded and 15/114 quiet runs.
  - Of 239 slow samples, the client thread held 177 (send 97, take 80) and the seat's pickup 29.
  - Inferred cause: hypervisor- or DPC-class preemption. The bar on jupiter is Ada's to restate.
- A-901 LANDED 0b641260f (merged at 1bbc2317c). An observation (Telemetry, DaemonStatus, DaemonStatusNext) keeps a
  closed loop's clock as an acknowledgement does and holds no shard. Red first: closed_loop.rs
  an_observation_holds_no_shard_while_a_closed_loop_still_does, 390 holds before, 0 after. A-326's test 2/2.
- Running (kiln r918, jupiter): A-901's A/B. A = 866374d85, B = 1bbc2317c. edit_merge_bench loaded and quiet ABBA:
  shard CPU and every run's p99/p999. Then provision_bench ABBA: every row's p99, the ac-2.1 spinning p99 the red.
  Main runs the macOS provision half on the big Studio.
- Left: report both A/Bs to main with numbers; record A-901's measured CPU in BENCHMARKS and close GAPS 84a's owed line.
- jupiter C: was at 8.6 GB free. I removed only my own target dirs (prototype, test); main cleans its own.
