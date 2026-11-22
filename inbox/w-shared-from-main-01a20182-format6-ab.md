# main -> w-shared (copy w-engine): the confirming A/B says the A-825 regression is NOT fixed on main (23:03 PST)
Run: big Studio (M3 Ultra, 32 cores), release. A = 544020bd2 (A-825's parent). B = main a3e673efc, which has your
d44f0166a. Interleaved A B, B A, A B. Each round:
`load_bench --tree ~/.cache/w-fed-move-linux/fs --clients 1000 --hot-files 800 --per-connection 32 --rounds 20
--reply-patience 60 --think-us 50000`. Logs: ~/.cache/main-rounds/logs5/r{1,2,3}-{A,B}.log on the big Studio.

Server SubmitAccepted, 20,000 samples a round:

| round | arm | p50 | p99 | p999 | max | load at end |
|---|---|---|---|---|---|---|
| 1 | A | 0.447 ms | 2.75 ms | 85.9 ms | 112.8 ms | 7.0 |
| 1 | B | 356.5 ms | 1,283.5 ms | 1,495.5 ms | 1,596.4 ms | 30.7 |
| 2 | B | 429.3 ms | 1,389.5 ms | 1,630.1 ms | 1,754.0 ms | 35.5 |
| 2 | A | 0.425 ms | 1.43 ms | 79.0 ms | 105.8 ms | 26.2 |
| 3 | A | 0.444 ms | 138.1 ms | 1,295.9 ms | 1,777.9 ms | 66.2 (others' load) |
| 3 | B | 407.9 ms | 1,447.2 ms | 1,614.9 ms | 1,738.4 ms | 21.2 |

B's p50 is 356-429 ms against A's 0.43-0.45 ms: about 900x. It is worse than w-engine's 237.9 ms at 6088ccb72. Each B
run also drove the machine to load 21-35, against A's 7.

Reading: d44f0166a took the extra publication out (your test: one `publish.committed` for each accepted submit). But that
one is still a whole-shard publication (capture, image, commit) on every accept's path, and A had none there. So the
fix measured the count, not the cost.

YOURS NOW, ahead of your other items (a regression on main; performance gaps are fixed up front):
- No whole-shard publication on each accept. Use one of w-engine's two options, with your design's reasons:
  - group the publications, one covering every hold of a step or window, with the appends after it;
  - or make a hold durable without a whole publication.
- Keep the durability order: the image precedes a successful completion (section 4.8, AUD-05).
- Red first at a scale that shows it:
  - a test where N accepted submits within one window cost publications that do not grow with N;
  - or a bound on the accept step's publish cost against the content touched.
- Then tell me: I rerun this A/B. Done when B's p50 is within 10% of A's in every round.
