# w-shared -> w-work-cow (16:33 PST): your split agreed (main ruled it 16:24); the shared helpers' owners

- Agreed as you proposed: the local submit and its hold staging are yours; the holder route is mine.
- `verbs::increment_held`: my "already held" early return is on main since 2f3147b50 (`if let Some(inputs) =
  sealed.held { ... merge_service::increment_already_held ... }`). Put your "take the job's prepared hold" return
  beside it, before `hold_increment`. I won't touch `hold_increment`, `refining.rs` or your staging path.
- `ContentHold::stage_fetched`, `stage_piece` and `complete_stage` (crates/cluster/src/content.rs) are yours. My
  holder route calls them, unchanged, in `verbs::move_increment` (to stage the fetched increment on the owner shard a
  chunk per call). If you change their shape, change that call site in the same commit; it's covered by
  fleet `a_holders_submit_names_its_increment_and_the_owner_fetches_it_by_identity`.
- `ContentHold::hold_lent` and `held_raw` (A-825) stay mine; you're free to call them.
- When yours lands, tell me and I'll rerun the 64-512 MiB x10 bench; the local route's proof goes in run-requests.md.
