# A stale-copy reclaim dropped content a successor still fetched (2026-11-18)

Contracts: §4.8 "Promotion and takeover" (a successor "adopts the newest records, and serves"), §4.10 "fetches … by
identity from a recorded holder", AUD-29-43 (a holder keeps content for a head it backs, and only that), A-272 and
A-309 (a holder lets a copy go only on evidence that its object was adopted), R4.

## Description

A holder of a departed owner's volume, asked in the takeover's round but outside the successor's placement of the
volume, let its copy go whole, record and content, as soon as it installed the successor's confirmation of its share
(`takeover::stale_after_confirmation`), or the takeover's completion record (`Lineage::Settled` with the holder among
those that confirmed). The confirmation certifies the volume's head register adopted and placed under the successor,
nothing more (`takeover::share_done`); the successor's materialization then fetches the content the adopted head names,
from the holders that head names and only from them (`fleet::fetch_into_hold`, `HeadValue::holders`). A seal places its
content on `f + 1` holders, the owner and one more. So at `f = 1`, when that one more was this holder and the successor
held none of the content, the successor could fetch from nobody once this holder had dropped it: the volume waited for
its content and was given up (`NotRecovered { cause: TakeoverIncomplete { waited_on: "content" } }`). One failure, the
owner's, lost the volume.

It takes a region whose neighbourhoods are narrower than the region (at `f = 1`, scatter 3 in four members or more), so
that the successor's placement can leave the holder out; a successor that is not among the content's holders; and the
holder installing the confirmation before the successor's fetch has finished. The confirmation is one council commit
away; a large volume's fetch takes periods (it resumes from what it staged). Not seen in a test or a run: the fleet
tests run three members, whose neighbourhoods are the whole region, so no holder is ever outside a successor's
placement. Found by reading, while checking the rule A-309 owed for the copies of unasked holders (below).

## Root cause

`takeover::resolve_held_objects` judged a copy stale by the register's lineage alone, and `takeover::reclaim` dropped
the content with the record (`ContentHold::forget_object`). Two things that are true of the record are not true of the
content: the successor's confirmation says the record was adopted, not that the content was fetched; and the adopted
record is the newest one the round learned, whose head may name this holder among the content's holders. AUD-29-43's
retention rule already states the right test for the content: a holder keeps content for a head it backs, which the
adopted head is. The reclaim was the one path that bypassed that rule.

## Impact

A taken-over volume lost after a single failure, within the tolerance the region promises, whenever the confirmation
reached the holder before the successor's fetch finished. Greens are exposed the same way: a green's successor fetches a
version's inputs from the survivors that reported the adopted version (`fleet::fetch_green_inputs`), and a reclaim
dropped a reporter's inputs with its record.

## Exact edits

- `crates/server/src/takeover.rs`: `let_go_stale` decides every stale copy: it runs the retention rule on the object
  first (`content_retention::retain_object`), then keeps the copy whole when content is still held for it (content its
  newest record names it for, or content placed ahead of its records), counted `fleet.takeover.kept_holding_content`,
  and reclaims it otherwise. Both reclaim sites call it (a confirmed successor's stale copy, and a line settled at a
  completion record this holder confirmed). `reclaim` is reached only through it.
- `crates/server/src/daemon.rs` (tests): `hold_a_head_naming_this_holder` (a seven-member `f = 1` region; the audit
  node holds a departed member's volume head naming the member and itself as the content's holders, and that content;
  the volume's successor places it away from the audit node);
  `a_holder_outside_the_successors_placement_keeps_the_content_its_head_names` (the confirmation, then the completed
  takeover) and `a_lagging_holder_keeps_the_content_its_head_names_when_the_takeover_completes` (a holder that installs
  only the completion record).
- `docs/wip/SLATES_DESIGN.md` (A-335, the §4.8 status, A-309's rule), `docs/wip/GAPS.md`.

## Evidence

- Red and green on the Mac Studio (d1323898, 32 cores, load 38 to 42; kiln run r1474, 2026-11-18): the tree of main
  1e8f4a49e with the tests and the fix passes the four holder tests (the two new ones and A-309's
  `a_holder_outside_the_successors_placement_lets_its_copy_go_when_the_takeover_completes` and
  `a_holder_the_takeover_never_asked_keeps_its_copy`). With `crates/server/src/takeover.rs` taken from main and the tests
  kept, both new tests fail: `a_holder_outside_the_successors_placement_keeps_the_content_its_head_names` at "the
  successor's confirmation leaves the content its head names, which the successor may still fetch" (the copy and its
  content reclaimed at the successor's confirmation), and
  `a_lagging_holder_keeps_the_content_its_head_names_when_the_takeover_completes` at "the copy and the content its head
  names stay" (reclaimed at the completion record); A-309's two pass on both trees.
- `cargo clippy --locked -p slates-server --all-targets -- -D warnings`: clean. The crate's suite: below, on the tree as
  merged with main.

## Siblings, swept

- **The rule A-309 owed for unasked copies** (GAPS, `fleet.takeover.kept_unasked`: "a holder an owner's neighbourhood
  settled away from lets its copies go at that settle, on the owner's report that every record it owns is placed on the
  new neighbourhood"). Built as stated it would drop content the same way: the settle report covers the registers
  (`fleet::records_placed_on_current_neighbourhood`), while content stays on the cohort it was placed on and the head
  goes on naming its holders (§4.8 "Content stays on its current cohort: the head names its holders"). Not built as
  stated; the safe part (register copies whose content no head names) is g250's next piece.
- **A-276's open healer item.** A named holder still a member but outside the object's candidates can be neither
  restored nor verified by the healer (`fleet::heal_decision`, `fleet.heal.named_outside_candidates`), which leaves it
  named and relies on it keeping the copy. This fix keeps such a holder's copy through a takeover too.
- **The release this fix owes.** A copy kept for its content stays until a newer record reaches its holder and the
  retention rule releases the content. A holder outside the successor's placement receives none, so it keeps it for
  good: a leak bounded by what it holds, never a loss. Owed (GAPS, g250): a successor confirms only once its adopted
  volumes' content is re-placed under its own placement, a settle certifies content as well as records, and a holder
  outside then lets go.
- **Other paths that free held content** (A-272's sweep, re-read for content): a tombstone (`tombstone::on_held`,
  `live_ship::let_go`) is a durably accepted destroy, positive evidence that nothing serves the volume again; the
  retention rule's own releases are by events on the holder's records; the green replica a reclaim drops
  (`merge.replicas`) is rebuilt by the successor from its own records, never fetched. None drops content a head may
  still name.
