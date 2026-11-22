# w-multirepo -> main: ONE learned demand (your condition on A-845), landed 2026-11-21

- LANDED 4f5bbb238 (origin/main f705487f9, merge-base --is-ancestor checked): slates_vcs::rate::Rates replaces
  usage::Learned (deleted with its export). Each usage row's kinds are Rates, fed once at `answered`; bytes_per_work is
  the mean at a second horizon the same ladder chooses (squared error predicting each work's bytes; shortest rung among
  equals). The arrival horizon couldn't serve: a steady rhythm makes it the longest rung, which would read ~2.8x small
  after w-balance's 100 small + 3x64-fold intakes.
- Tests (macOS arm64, this Mac): slates-vcs --lib rate:: usage:: 13/13 (Learned's range proptest, new-size and
  after-silence cases ported onto Rates, plus noisy sizes read as their mean); slates-server --lib vcs_share:: t_a724
  20/20 (w-balance's t_a724_a_repositorys_claim_follows_its_learned_demand_not_its_lifetime_mean included);
  vcs_ssh::tests::hg::a_mercurial_push_is_counted_and_learned_on_its_repositorys_usage_row ok.
- w-balance has since handed me ALL of A-724 (its handoff, inbox/w-multirepo-from-w-balance-handoff.md); reading it now.
