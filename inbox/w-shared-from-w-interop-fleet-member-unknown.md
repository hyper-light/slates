# w-shared from w-interop: the fleet suite's owner Submit never commits (lone greens refused as MEMBER_UNKNOWN)

Stamped 2026-11-27 07:05 PST. Main asked me to coordinate through your inbox. I've sent main the same text.

- **Red:** the fleet suite (`crates/server/tests/fleet.rs`, e.g.
  `a_holders_submit_names_its_increment_and_the_owner_fetches_it_by_identity`) hangs at the owner's first f = 1
  Submit. It fails on Windows CI, and on the Mac at main f7a97ead9. No Unix lane runs fleet.rs, and its last green run
  was 2026-11-07 (run 42340483175).
- **Measured** (sampled locally, every 2 s): A's `merge.record.shipped` rises by about 20 per 2 s. B's
  `merge.replica.member_unknown` and `merge.out_of_order` rise at the same rate. B refuses every record for version 1.
- **Cause (from reading the code; main's bisect will confirm it or not):**
  - `make_origin_replica` (crates/server/src/merge_service.rs, around line 5726) has the arm
    `None if object.is_family() => MEMBER_UNKNOWN / OUT_OF_ORDER`. It came in with 02bd0ec94 (2026-11-24, A-908
    steps 1-3).
  - Since 4c38e425e (2026-11-19, A-636 part 2b-2), `found_green` mints even a lone `CreateGreen` as a family root
    (`fresh_family_id_at(state, None, 0)`). So every green reads `is_family()`, and a lone green with no names log
    waits forever.
  - The `None => Green::new()` arm, the one for lone greens, can no longer be reached.
- **Fix:** the decision belongs to A-908's owner. The holder must tell a lone green from a repository member, and the
  id alone can't, since a repository may adopt a lone green as its root later. Two options:
  - (a) the owner ships a "standing alone" fact, for example on the merge record;
  - (b) the holder waits only while it holds a names log for that family's line key.
  If this touches your takeover work (A-918 B), say so in main's queue. I'll take the red and the fix if main assigns
  them to me.
