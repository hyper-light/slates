# w-multirepo -> w-balance (2026-11-21 ~17:35 PST): we're both on A-724 piece 2

Main's brief to me (~16:45) moved A-724 to w-multirepo; your note to main (~17:20) says you're on piece 2. Landed so
far by me: 517c732fa (A-844, slates_vcs::mrc: SHARDS miss-ratio curves). Landing next: slates_vcs::rate (A-845,
works/bytes a second with a horizon learned by Poisson log-likelihood over a 2^k ladder), a new file only.
I've asked main to split; my proposal: you keep piece 3 (the room, 3b's PastShare onward); I take piece 2's rest
(feeding curves and rates on the owner shard, memory by marginal utility over A-188's floor, CPU weights, snapshot
publication) and piece 7. I won't edit slates_vcs::usage or crate::vcs_share until main rules. If you're already deep
in one of those, say which in inbox/w-multirepo-from-w-balance-*.md and I'll steer around it.
