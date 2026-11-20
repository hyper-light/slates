# w-k90-vendor -> main: A-745 items 3 and 4 are on main (2026-11-20 ~13:00 PST)

Please mark both complete in the UI. With them, none of A-745's git items is owed (GAPS says so).

- Item 3, protocol v0's fetch in turns. LANDED `1625570cf` via `bd4d7989b`, base `5c4c68ef1`.
  - Before: `serve_in_turns` sent v0 and v1 requests through the whole path, all in one call.
  - Now `upload::v0::answer_in_turns` makes the answer in steps: its acknowledgments a have per unit, each `ready`
    search a parent per unit (`negotiation::Ready`), and the negotiation's cut and choice. The head goes out once those
    are known.
  - Every v0 test's exchange is now answered both whole and in turns, with the same bytes, refusal and counters.
  - New test: a v0 answer has decided nothing when made, and a pack's answer takes more than one step.
  - slates-git 573; server vcs and git_push 335.
- Item 4, scattered plans bounded. LANDED `7ffddbdf0` via `3e95e057e`, base `0ee6c7d55`.
  - `Positions` now keeps whichever of three forms is cheapest: runs (16 B each), a sorted list (8 B a position), or a
    bit table (8 B per 64 positions).
  - The negotiation's bit table goes to the plan as is. It used to be converted back into one run per position.
  - Encoding still writes runs, so kept content is unchanged.
  - 3,000 positions every third: 1,128 B, where runs cost 48,000 B.
  - New oracle covers every operation and form.
  - slates-git 575; server 335.
- Ada's allocation rule: there is no allocation per object or per pkt-line. The v0 acknowledgment lines live in one
  buffer reserved up front; set operations build one buffer each.

Two findings from building the A/B, not fixed, for routing:
1. `Repository::kept_fragments` counts `store.len_of(root)`. For a part longer than one window, the root is the list
   naming its windows, a few hundred bytes. So A-724's relief under memory pressure under-counts large fragments by
   about 40x.
2. Fragment size: git/git cut at v1.0.0 encodes to 40.7 MB of fragments (`encode_bench`, debug) against git's own
   3.4 MB pack for the same 64 MB of objects.
   - The likely cause: a first version has no previous tree to name bases from.
   - The learned delta window starts at one candidate, in an order sorted by size.
   - So an import finds few deltas. Every clone of an imported repository then sends about 12 times git's bytes.

The release A/B on git/git is running now (kiln run r14, no `on`). Earlier attempts: r5 and r6 hit a runner that never
answered the sync; r12 was interrupted. Results go into BENCHMARKS.md.
