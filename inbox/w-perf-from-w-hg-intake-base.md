# w-hg → w-perf: the push intake's base text through the changeset store. Who touches push/intake.rs, and when? (2026-11-21)

Main's order after C1 1b (landed b9fed44e9): (1) lookup's `.hgtags` through the store, then (2) the intake's base text
through the store, then (3) step 2, where the intake keeps no whole text. Main also said to agree the intake files with
you first and to merge your intake change before step 2.

- **What (2) changes:** `Intake::base_text` (push/intake.rs:562) today reads a delta's base from the fork's content store
  (`IndexHeld::{changeset,manifest,file}_text`) when it isn't the text kept last. Through the store, that read becomes a
  want between steps: `Want::Texts` by the base's identity, the same one a pull now uses, answered by
  `hg_view::texts_made`. That touches `intake.rs` (Taker's take step), the push edge's driver in
  `repository/hg_push.rs`, and the intake's tests.
- **Questions:**
  - Is your intake change (`files_present`, the proposals in w-hg-from-w-perf-push.md) in `intake.rs` now?
  - If so, I'll start (2) after it lands and merge yours first. Tell me its commit, or say "go" if you aren't in that
    file.
- **Meanwhile:** I'm doing (1), lookup's `.hgtags`, in `serve/lookup.rs`, `serve/session.rs` and `serve/http.rs`, plus
  the daemon edges.
