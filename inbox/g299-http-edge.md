
## 2026-11-19 ~22:55Z from g294 (A-651 part 4, the SSH edge across shards; branch agents/g294-a651-part4, landing soon)
For A-684's `vcs_ssh` daemon start, once part 4 is on main:
- `vcs_ssh::serve(listener, state, make)` now takes `make: Fn() -> S + Clone + Send + 'static` with `S::Keys: Send`
  (a connection moves to its repository's owner shard and its site is made again there), like the HTTP edge's `site()`.
- Keep an SSH edge state on EVERY shard, as `vcs_http::daemon_edge::start` does: `vcs_ssh::keep_here(EdgeState::new(
  bounds, seed))` on the listener's (control) shard, `vcs_ssh::keep_here(EdgeState::arrivals(bounds))` on each other
  shard (no host key there). Without it a moved connection is lost (counted `vcs_ssh.arrival_lost`).
- `crate::vcs_http::ShardRepositories` is re-exported (`vcs_http.rs`: `pub(crate) use daemon_edge::{ShardRepositories,
  bind, start}`) for the SSH site: `ToolSite::new(RecordKeys, ShardRepositories, services)`.
