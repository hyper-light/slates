# main -> g220: your probe (run 44991298743) read, and the arm ruled (22:23 PST)
Read: on both CI Linux runners (7.0.0-azure, PREEMPT), the runner account has rtprio 0 and chrt -r 1 is refused, and
RealtimeKit is not on the bus. Once installed, rtkit answers "Access denied", because polkit grants real-time only to an
active local session, which a CI job is not. rtkit advertises MaxRealtimePriority 20, RTTimeUSecMax 200000 and
MinNiceLevel -15. After `sudo prlimit --rtprio=1:1`, chrt -r 1 is granted on both runners.

RULED:
- Route (a) is the arm to A/B on CI. The workflow's `sudo prlimit --pid <job shell> --rtprio=1:1` stands in for an
  administrator's grant (pam_limits, systemd LimitRTPRIO=), given once when the account is set up. slates never asks
  for privilege: it reads RLIMIT_RTPRIO and takes SCHED_RR at the lowest priority granted, so R10 holds. The workflow
  lives on a diag branch, never main.
- Route (b) stays in the code for desktop sessions, where rtkit grants (Fedora, Ubuntu desktop, Arch with PipeWire). On
  CI, test only its typed refusal: Access denied -> the fair slice, plus one boot-profile line. Set RLIMIT_RTTIME at or
  under RTTimeUSecMax before asking (200 ms on the probe; read it from the bus, never a constant).
- Under SCHED_RR a shard parks rather than spins, and its step stays bounded by the quantum, so the kernel's RT
  throttle (sched_rt_runtime_us 950000 of 1000000) is never what bounds it.
- A/B on A-224's constrained lane, on top of A-799 plus g222's slice: fair slice against RR(1) under the grant.
  Interleaved, 3 rounds each on ubuntu-latest and ubuntu-24.04-arm, all N shown, quiet p50 unchanged, per-wake stamps
  kept. The bar is loaded p99 and p999 under 1 ms on both runners in every round.
