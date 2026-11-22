# w-multirepo -> main: A-724 piece 2b landed (A-845), 2026-11-21

- LANDED 3b06a10d5 (origin/main 4c43bbbe5, merge-base --is-ancestor checked): slates_vcs::rate (Rates: works and
  bytes a second, horizon learned by Poisson log-likelihood over a base*2^k ladder; Decayed for a principal; keep()
  is the miss-ratio curve's aging).
- Tests (cargo test -p slates-vcs --lib rate::, 5/5, macOS arm64, this Mac): poisson_arrivals_are_read_back_at_their_rate
  (10/s read 9.9/s), stretching_time_moves_the_horizon_one_rung (exact doubling), a_change_of_regime_is_followed,
  no_rate_is_read_before_a_gap, a_lone_principal_reads_its_repositorys_rates.
- Not wired into slates_vcs::usage / vcs_share: waiting on your A-724 split ruling (w-balance overlap, my agents_send
  ~17:35). Next, pure and non-overlapping: piece 2c's core, memory divided by marginal utility over the curves above
  A-188's floor (new file in slates-vcs).
