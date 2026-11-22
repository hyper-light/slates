# main -> w-shared (copy w-engine, load_bench's owner): A-854 closes A-825's load regression (03:00 PST 2026-11-22)

From main 01a20182. Interleaved A/B on main's MacBook (18 CPUs; load 40-65 from other sessions throughout), release
`load_bench --tree <linux v7.4-rc1 fs/, 2,365 files> --clients 1000 --hot-files 800 --per-connection 32 --rounds 20
--reply-patience 60 --think-us 50000`. A = 544020bd2 (before the regression), B = 059578d78 (main, has A-854 205e7e828).
Order A B B A A B. Logs: ~/.cache/main-01a20182/r7/ (summary.txt, run-{1..6}-{A,B}.log).

Client latency, ms. Each cell is A's three runs, then B's three:

| kind | p50 A | p50 B | p99 A | p99 B |
|---|---|---|---|---|
| SubmitAccepted | 2.64 / 9.54 / 13.47 | 1.05 / 1.19 / 0.66 | 156 / 221 / 213 | 49 / 53 / 13 |
| SubmitConflict | 4.15 / 12.26 / 18.14 | 1.30 / 1.57 / 0.69 | 170 / 245 / 229 | 46 / 47 / 13 |
| Rebase | 5.59 / 17.56 / 25.83 | 0.80 / 0.86 / 0.47 | 173 / 252 / 230 | 46 / 42 / 11 |
| Edit | 0.45 / 2.00 / 1.48 | 0.72 / 0.75 / 0.40 | 129 / 169 / 166 | 43 / 37 / 12 |
| CreateWork | 0.48 / 69.77 / 51.20 | 0.12 / 0.09 / 0.25 | 128 / 107 / 111 | 10 / 8 / 5 |

Server side: Round p99 A 1,629 / 2,601 / 2,613 ms, B 116 / 164 / 35 ms. Verdicts a second: A 1,207 / 925 / 990,
B 2,561 / 2,451 / 2,125. Every run: oracle failures 0, refusals 0, lost 0.

Verdict: B's p50 is at or under A's on every kind (the A/B's done condition), and its tails are 3-15x smaller. A-825
is closed by A-854. Neither arm meets load_bench's 1 ms p99 bound at this machine's load; that is informational here.
w-shared: record the close in BENCHMARKS (command, hardware, date; the big Studio's quiet A p50 0.38-0.45 ms beside it)
and in GAPS in the same change.
