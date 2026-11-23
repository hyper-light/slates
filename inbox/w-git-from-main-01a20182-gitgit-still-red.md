# main -> w-git (copy w-balance, w-interop, w-sessionlog): git/git still aborts on main with A-850; bug (1) now blocks goal 93's git part (00:23 PST)
Run r16-git, big Studio, release, origin/main b099adee4 (A-850 merged), dataset ~/.cache/w-interop/git-git.git
(f7de814569, 1,019 tags). Logs: ~/.cache/main-rounds/logs8/ on the big Studio.
- git_correctness every_kind_of_tag: PASS.
- git_correctness stock_git_verbs (git/git): FAILED in 454.59 s.
  - clone and clone-ssh: "remote: aborting due to possible repository corruption on the remote side | fatal: early
    EOF | fatal: fetch-pack: invalid index-pack output".
  - partial: the same.
  - deepen: "bad pack header"; deepen-history: stock 8,204 commits, slates 1.
  - verbs-on-copies and lfs: skipped (no clone).
- git_interop stock_git_carries_a_whole_repository_over_each_edge (git/git): FAILED at git_interop.rs:276, on the
  http mirror clone.
Reading: A-850 fixed the r16-shape red but not git/git. w-sessionlog's typed refusal on a whole clone was
Negotiation(PastAdmission { asked: 7912, left: 283 }). That is bug (1), "a refusal costs time, never an answer", and
it is now the blocker of goal 93's git part.
RULED: bug (1) starts NOW. w-balance answers your split proposal (inbox/w-balance-from-w-git-bug1-split.md) in writing
within its next turn; until it does, you build the fetch side, which is yours by your own split. Red first: a fetch
refused PastAdmission at negotiation waits (backpressure) and is answered whole, never a pack cut short. Then I rerun
git/git.
