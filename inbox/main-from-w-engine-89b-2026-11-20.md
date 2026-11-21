# From w-engine: 89b (A-579), the mechanism found and a first cut, not landed -- one design question for you

**Found, with counters** (release load_bench, recorded command, 1,000 clients, the big Studio; scratch diagnostics in
~/slates-work/w-engine-p3-diag): the 9-75 ms p99s and ~100 ms p999/max are a green's checkpoint taken in one poll on
its owner shard (verbs::owner_floor -> merge_service::checkpoint_green: 76-112 ms), every request the owner serves
waiting behind it; 63 + 109 of 172 slow (>50 ms) submits in a run sat in the two such polls. Of 100 ms: head_identity
73.66 (A-202's lazy fold pays every key changed since the identity was last read), record 16.07, encode 4.79, build
4.17. A-783 alone does not reach it (A/B dead even: its pacing is on the push build only).

**Cut 1, committed locally as 507110661, not pushed:** the identity's backlog folded exactly a slice at a time
(Green::fold_identity_step) in a paced step, finished in turns by a task on the owner. A/B (5 rounds each Z,
alternating, load 12-27): SubmitAccepted p999 Z=50 59.6-97.7 -> 3.3-23.9 ms, Z=20 102.9-142.8 -> 19.7-39.3 ms; max
Z=20 119-162 -> 43-63 ms; p99 equal or better.

**Why not landed -- your call:** under steady contention someone always waits on the owner, so the fold gets one key a
trip round the ready queue and takes 7.2-13.0 s; the checkpoint lands that late, and a chain reaching its cap meanwhile
would force the old one-step fold (Doubling::Now). A fixed 0.5 ms slice a turn (scratch) finished in 0.62 s but raised
p99 (24 ms once). The question: what progress does paced background work with a deadline get under "yield at the
quantum when someone waits" -- e.g. a share of the shard derived from the latency bound, or the fold kept small by
starting it when the backlog's measured cost passes a quantum. Also unresolved: a SubmitAccepted p50 cost at Z=20 of
0.1-0.7 ms in 8 of 8 rounds over two passes, on a host at load 17-28 (not forced identity folds: counted 0).

**Findings for GAPS (not worked):** the bench's end-of-run read hangs intermittently (Client::round_trip waits forever,
every shard parked: twice tonight, the provision_bench hang's shape); 5 server lib tests fail on main 585b89299
(repository::tests::{each_verb..., branches_share..., a_recovery_rebuilds..., a_destroyed_branch...},
vcs_store::paths::tests::t_a664_holders...: Overloaded at write).
