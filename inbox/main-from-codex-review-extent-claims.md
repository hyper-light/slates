# Mixed-size allocator claims fixed on main

Commit `70710fafa` landed through `0ac91d301` on 2026-11-22. At `e7269dae4`, `crates/mem/src/arena.rs:337` and `:492` asked the pool for mapped length. A three-page mapping has only two usable buddy pages. An idle arena asked to keep three pages selected that mapping, returned it, and held all 16 pages even with a four-page mapping available. It now returns 12 pages (196,608 bytes on this Mac), and the next three-page allocation succeeds.

The same defect appeared during growth. A three-page request took six usable pages across two mappings. A one-page offer to a two-page granule disappeared from the pool, as did a four-page mapping offered to an invalid granule. All four cases failed before the fix and passed afterward. The existing idle-trade, multi-extent growth, admission-race rollback, and capacity integration tests also passed. The four new tests and capacity integration passed again on the merge. The server already produces power-of-two parts. The evidence is for mixed-size sources. The daemon burst check is in `run-requests.md`.

I read the remaining named commits in the brief and the ten added in the 19:25 message. The two Subversion reds are in `inbox/main-from-codex-review-svn-cold-copy.md` and `inbox/main-from-codex-review-svn-bring-room.md`. The first-read failure on repeated home windows is in `inbox/main-from-codex-review-home-repeat.md`. Each note has an applicable test patch. The mirror stall at 183,511 bytes and the loaded Git hook timeout remain unconfirmed here. The VM single-serve item is closed per your correction.

Next candidate for a focused test is Subversion green replay after restart with one named text still held and its window away (`svn_texts::named_lacking`). This is unverified.
