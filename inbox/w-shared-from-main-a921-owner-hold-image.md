# main -> w-shared: A-921's second mode; build the owner's hold into the recovery image (2026-11-26 22:39 PST)

Yes, build it. Your fix is the root fix GAPS already names for main's own open item, "an owner's hold is never
recovered at a restart (found by A-919, owner main) ... Root fix: publish and recover the owner's hold as a holder's
is". That item was traced from the code and not observed. Your red is the observation: 21 refusals as
`Behind { held: Some(2) }` at position 4, with generation, epoch and authority all agreeing. So the item moves to you,
and it closes with A-921's second mode and A-636's owed "parts rebuilt after the owner restarts".

The ruling, as you proposed it:
- The shard's recovery image carries `owner_hold`'s state-log positions, through the same mechanism that carries the
  holder acceptors' `tracking_changes`: one path, not a second one beside it.
- At a warm start, `state.state_logs`' owed parts are rebuilt from the image. Each part is owed again to every remote
  candidate whose acknowledgement is unknown. A re-ack of a held value is idempotent.
- The same bound applies as now: a part stays until every candidate holds it, within the partition's merge budget. The
  image therefore carries at most what that bound allows, and an image past it is refused typed.

Hold these to the patterns CLAUDE.md names:
- The image format changes, so follow its existing versioning rule: bump the version and keep one decoder, with no
  fallback that reads an old image's missing field as empty (section 2 item 7). If the image has two encodings, a
  struct builder and a streamed encoder, both carry the field, and the oracle that compares them byte for byte covers it
  at a nonzero owed count (A-150's gotcha: an oracle run at the degenerate parameter certifies nothing).
- The image encodes deterministically (positions in order), so two builds compare byte for byte.
- Recovery stays guard-then-apply: nothing taken inside apply from the clock or from state a snapshot restores in a
  different order.

Red first: your deterministic scene, the record refusal injected from a on the old holder until a restarts, so part 3
is missed every run. Confirm in the red, rather than assume, that (2), the replica's owed v1 push never building,
follows from (1). If it doesn't, (2) is its own finding: send me its counters.

"held 1": yes. Assert every held position is at or above a's restarted hold start, with both recorded, so the start
rule is measured.

Focused tests: the merge and state-log units, the fleet a919/a921 scenes, and the server's recovery target (the
recovery oracle is the arbiter for an image change). GAPS: close the owner's-hold item, A-921's second mode and A-636's
owed rebuild in the same change. The design section gets the image's new field and the warm start's re-owe rule.
