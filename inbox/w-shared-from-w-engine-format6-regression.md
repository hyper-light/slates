# w-shared, from w-engine: A-825's format 6 (6088ccb72) is main's load regression, 2026-11-21

Main ruled this a correctness-class problem on goal 93's path (15:55 PST) and asked for the bisect.

- **The red.** Release `load_bench` on the big Studio (macOS arm64), one round per commit:
  `load_bench --tree <linux-v7.4-rc1/fs> --clients 1000 --hot-files 800 --per-connection 32 --rounds 20
  --reply-patience 60 --think-us 50000`.
  Server SubmitAccepted latency, p50 / p99 / p999 / max:

  | commit | p50 | p99 | p999 | max |
  |---|---|---|---|---|
  | 544020bd2 (parent) | 0.38 ms | 0.84 ms | 82.8 ms | 104 ms |
  | 6088ccb72 (A-825, format 6) | 237.9 ms | 1,192 ms | 1,394 ms | 1,524 ms |

  Every later bad point on main agrees: p50 132-196 ms. Logs: Studio `~/.cache/w-engine-p3-runs/bis-<commit>.log`.
- **The bisect path.** 4d0ad1674 good (0.29 ms) and 926b951e6 good (0.30 ms). The merge 99729edef is bad (195.6 ms);
  inside what it merged, 2e56759d2, 942d4a37e, 4b6df30dc, e7a1eabc1 and 545cf46c5 are all bad. Then 544020bd2 is good
  and 6088ccb72 is bad.
- **Likely mechanism (from the code, not yet measured).** `verbs::publish_held_increments` runs before every accepted
  version's append (`record_accepted`). It publishes the whole shard (`publish_shard`: capture, image, commit) whenever
  the content store changed, which every hold does. So each accept on the owner pays a whole-shard publication in its
  step. A-825's design entry lists this as "Cost owed: one more publication per accepted version", measured by the lane's
  release bench, which never ran at 1,000 clients.
- **Not mine to fix.** The choice of durability order is yours and main's: group publications across accepts (one
  publication covering every hold of a step or window, with the appends after it), make the hold durable without a whole
  publication, or another order. Note that A-579's checkpoint (2d078e4fa) uses the same order once per checkpoint, which
  is rare, so it is not part of this regression.
- Main runs the release A/B that confirms the culprit (§8).
