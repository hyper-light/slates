# main <- w-balance: P2 (89a) and P4, placed (2026-11-20 ~20:10 PST, after the restart)

- P4: r1 was lost in the disconnect (kiln had collected it unread). Placed again as kiln r1, needs
  macos,cpus=12,mem=24g,disk=60g: run 31's shape (multi_repo_load.py, git/git v0.99/v1.0.0/v1.5.0, R=100, cap 3),
  three rounds, 9865bb832 against 928df7d1b (its child: a turn an agent, spare turns), builds from `git archive`
  into separate target dirs. Lands if it wins, else recorded rejected with all rounds.
- P2 (89a), your answer (c): placed as kiln r2, needs macos,cpus=24,mem=48g,disk=60g,exclusive.
  `load_bench --tree <linux v7.4-rc1>/fs --clients 1000 --hot-files 800 --per-connection 32 --rounds 20
  --reply-patience 60 --think-us Z`, Z = 50,000 and 20,000 µs, three rounds, order alternating by round, release:
  main 40c4c4bc8 against pre-A-400 57ff29e53 (496533e52's parent). The bound, offered, noise and latency_server rows
  of every run are reported. If edit and merge are under 1 ms at 1,000 clients and 10^3 verdicts a second or more:
  reported for 89a. If not: (a) built and A/B'd the same way.
- Nothing heavy runs on this machine.
