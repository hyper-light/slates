# A submit resent after its green's takeover was never answered its version

Found 2026-11-08 by goal 4's work on A-252. A-252 made a submit waiting for its version's commit a durable state of the
owner's partition, so a resend after the owner's restart joins the wait or is answered its version, never run again.
After a takeover the resend was refused `OwnerUnreachable` without end. Nothing ran twice, but the client never learned
its version. §4.16's failure matrix promises "Masked after takeover on a candidate that holds the ledger", as do T-8.11
and §4.8's walk-through ("its retries return their original completion records" from the new owner).

## Cause

- The completion window lived on the owner's partition, so it died with the owner. RIFL (§3): "If the object migrates,
  then the completion record must move with it." For a merge the object is the green, which a successor takes over. The
  submit's work is not taken over: a work's head is never placed.
- The resend routes by its work's id (`RequestBody::volume`, `crates/ipc/src/protocol.rs`). After the owner's death no
  member owns the work, so every peer's location round truthfully answered "not owner". fixtures' first red test
  counted 5,244 rounds, every one `not_owner`. The green's successor was found: a read of the green from the other
  survivor reached it in 3.1 s (`a_greens_successor_is_found_by_a_survivor_after_its_owner_died`).

## Failing tests

- `a_submit_whose_answer_was_lost_with_its_owner_is_answered_its_version_by_the_greens_successor`. Its version committed
  at the quorum, then its owner died with the answer unread. On main `1ed67337` the resend was refused
  `OwnerUnreachable { region: 0 }`, every setup step passing.
- `a_submit_resent_after_its_owner_was_taken_over_is_answered_its_version` (loadtest-scale's). It was refused
  `OwnerUnreachable` before `d28e4021`.

## Exact edits

loadtest-scale built the route (`agents/loadtest-scale-a255-2`), and fixtures finished it (`agents/fixtures-a255`).

1. **The completion moves with the green.** A chain entry carries the submit's completion key, `(origin, client,
   sequence)`, inside its digest (`slates_merge::chain_entry` version 2). So does the merge record that holders replicate
   (`MergeRecordValue::completion`). A green's successor records each adopted completion in its own window, the newest
   `completion_bound` per client (`record_taken_over_completions`). The anchor layout goes to 7.
2. **The route.**
   - A work ships its catalog register, which names its green (`CatalogValue::work_of`).
   - The adopter of a work's catalog keeps it placed (`fleet::owed_adopted_work_catalogs`).
   - A request for a work no member owns goes, unchanged, to its green's owner (`verbs::locate_work_green`). The route
     comes from the work's held catalog record, or from a location round that carries the green from the catalog's
     holders (`owner_location::locate_work_of`).
   - The green's owner answers only the completion under the request's own key, and refuses anything else `NotFound`.
     A copied key is refused earlier, at its origin's door (A-SEAT, `refuse_foreign_request`).
3. **The gate.** A version's merge record ships only once its submitting work's catalog is committed
   (`PendingMergeRecord::work`, `fleet::catalog_committed`). Without it a holder could hold the completion with no route
   to it. The record plane ships that catalog itself: at `create_work` (`works_owing_catalog`), and on demand when a
   record waits (`MergeWork::ShipWorkCatalog`).
4. **The record's format.** A merge record carries the magic "GMRC" and format version 1. Another version is refused
   typed (`RecordRefused::Version`), with no second decoder.

## Verification

- adas-mac-studio.local-2, idle, 2026-11-09: the A-255, commit and forged-id tests pass, nine in 29.1 s.
- The five-node test (`a_survivor_holding_no_copy_of_the_works_catalog_learns_its_green_from_a_location_round`) passes
  in 12.4 s. It is red with the round's work lookup out (`OwnerUnreachable`).
- The gate's test is red with the gate out: version 1's record was placed while its work's catalog was withheld.
- The forged-id test is red with A-SEAT's door out.
- The gate's cost, as a submit made right after its work's creation (A-235's 20 timed submits, median):

  | Arm | Medians |
  |---|---|
  | Neither the catalog nor the gate | 1.25, 1.29 ms |
  | The catalog shipped only by the period | 109.2, 109.2 ms |
  | Shipped on demand | 1.61, 1.87 ms |
  | Shipped at creation | 1.59, 1.64 ms |

  What is left, about 0.35 ms, is the catalog's own commit, serialized on the coordinator before the record's.
