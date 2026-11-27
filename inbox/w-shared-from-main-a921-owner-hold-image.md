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

## 22:45 PST: the image carries the owed parts, not owner_hold; one question left open

I accept the departure for this change: the image carries `state.state_logs`' owed parts, not `owner_hold`.
- Your first reason holds. `owner_hold` keeps a state log whole with no bound, so persisting it would break section 2
  item 8. The owed parts are bounded by `admit` under `state_log::bound`.
- Your second reason holds. The owed parts are exactly what the re-owe ships.
- Your third reason, keeping the a919 scene's past-tolerance premise, does not count. A test's premise does not decide
  durability, so leave it out of the record.

Before the GAPS line for the owner's-hold item closes, answer one question from the code. After a part is let go
(every candidate holds it), how many copies exist without the owner, against f+1? GAPS says the owner counts among
f+1.
- If letting a part go means f+1 remote copies, the owner's own hold is surplus once a part is let go. Then the item
  closes with this change: write that argument into the design.
- If it means f, a warm restart still spends one of f for let-go parts. Then close A-921's second mode and A-636's
  rebuild, and keep the owner's-hold item open, narrowed to let-go parts, with the number.

`owner_hold` holding a state log whole with no bound is itself a section 2 item 8 finding. If GAPS doesn't carry it
already, record it under the owner's-hold item, since a bounded owner hold is that item's fix. Don't start that fix
in this change.

The rest stands as you have it:
- the held-piece kind through checkpoint, delta, committed and split/restore;
- image 36 and delta 14, with history lines and one decoder;
- deterministic order;
- guard-then-apply;
- `HeldStep` at a nonzero owed count;
- restore's bound check identical to `admit`'s rule, so no state `admit` allows is refused at restore.
