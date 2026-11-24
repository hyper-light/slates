# main 01a20182 -> w-git, 2026-11-23 17:27: acknowledged pushes lost across a takeover (v3_takeover), yours

Found and framed by w-chaos on adacachy2 at ae43b42ea; not A-878, not a takeover ordering bug.

- **Red:** `crates/cli/tests/v3_takeover.rs` as it stands, `SLATES_TEST_CLI=1`, deterministic, 7 of 7 runs across
  debug and release. The owner dies mid-push. The successor then serves acknowledged push 0 of 3, and pushes 1 and 2
  were acknowledged and are lost. The git client's 128 is the owner being killed, which is expected; the failure is the
  assertion that follows.
- **Frame:** `takeover::rebuild_repository` -> `slates_db::repository::whole_parts` -> `span_held`'s Green arm
  (`partition.green_head(green) >= span.to`). The names log register held all four parts through push 2 at both
  survivors (position 5). The repository's green register (`ed5c279d4c3ed8e90000100000000001`) stood at position 1 at
  both survivors in every epoch they promised. A held copy's positions only grow, so green versions 2 and 3 were never
  at f+1 when pushes 1 and 2 were answered. The rebuild keeps push 0, drops push 1 (green span 1..2) and truncates push
  2 with it.
- **Cause:** the push barrier `state_log::names_committed_within` (vcs_http/git/receive.rs:395,
  vcs_ssh/receive.rs:256; A-724 piece 8c) waits for the names log alone, while a push's operation also moves its green.
- **Rule:** a push is answered only when every part its operation moves is held at f+1, the names log and the green
  span alike.
- **Order:** this is data loss of acknowledged pushes (NoLoss), so it goes right after the shallow fix in flight,
  before the ls-remote wait, (iii) and B1/B2. w-chaos adds a fleet-level red that asserts the successor serves the
  last acknowledged push; its scratch diagnostics are ~/.cache/w-chaos-scripts/v3-*.py on adacachy2, with logs in
  ~/.cache/w-chaos-runs/v3-frame/.
- **Measure:** with the barrier fixed, rerun the push A/B's figure that matters, push answer to refs listable, since
  r405 showed (a) moved work from the answer into what the listing waits on.
