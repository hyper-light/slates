# A thin pack in a shallow exchange sent blobs the client held (2026-11-19, A-615, found by A-640 part 3)

## Description

Found by A-640 part 3's golden test on 2026-11-19. Served stock git 2.50.1's own `git fetch --deepen=1` session
(`fixtures/v0/deepen-v0`): the client is shallow at `main`'s tip and sends it as a have, asking `deepen 1` with
`deepen-relative`, `thin-pack` and `done`. Our pack held `main`'s second commit, its tree, and two blobs that git's pack
left out. The client already held those blobs, in its tip's tree.

## Root cause

A-615's object choice (`negotiation/objects.rs`, `left_out`) left out only the trees of edge commits, the had parents of
commits sent. That is what `pack-objects --objects-edge` leaves out. But git's server runs `pack-objects --thin
--shallow` for a thin, shallow fetch, and that walks with `--objects-edge-aggressive` (`builtin/pack-objects.c`
v2.50.1). Then `mark_edges_uninteresting` (`revision.c`) leaves out the tree of every uninteresting commit on its command
line:

- each have `do_got_oid` lists in `have_obj` (a commit only when no earlier have named it or named it as a parent);
- each commit `send_unshallow` adds to `extra_edge_obj`.

The tip here is a child of the commit sent, so it is never an edge, and its tree was not left out. The first fix left
out the trees of the commits a cut makes whole in every exchange. It failed git's stateless `deepen 2` without
`thin-pack` (`fixtures/v0/unshallow`), where git sends those blobs. So the aggressive rule holds only under `--thin` with
`--shallow`, which is how git's server passes them (`create_pack_file`: `--thin` for `thin-pack`, `--shallow` while
`shallow_nr` is not zero).

## Impact

Every thin fetch in a shallow exchange sent objects the client already held: a client shallow or made shallow, a have
beyond the edges. That covers git's `fetch --deepen`, `--depth` on a shallow clone and `--shallow-since`, all thin by
git's default. The packs were correct, only larger than git's. A-615's oracle stated the same rule as its code, so it
certified the gap. An oracle must state the design's rule, here git's, not the implementation's (CLAUDE.md's gotcha for
oracle tests).

## Edits

- `crates/git/src/negotiation/mod.rs`: `Request` gains `thin`.
- `crates/git/src/negotiation/objects.rs`: `left_out_aggressively`, applied when the request is thin and the cut has
  grafts, and `listed_haves`, git's `have_obj` order.
- `crates/git/src/negotiation/tests/mod.rs`: the oracle's `oracle_aggressive` and `oracle_listed_haves`; the generator
  draws `thin` (plan flag 64).
- `crates/git/src/upload/fetch.rs`, `upload/v0.rs` and `tests/store_graph.rs`: the request's `thin` taken from the
  client's `thin-pack`.
- Tests:
  - `every_one_round_session_is_answered_as_gits_server_answered` (`upload/tests/v0.rs`, deepen-v0) fails without the
    rule.
  - `every_stateless_exchange_is_answered_with_gits_lines_and_objects` (unshallow) fails with the rule applied always.
  - A-615's oracle (`every_answer_is_the_oracles`) fails both ways.

## Siblings swept

- **v2 `fetch` without a shallow line or a deepening** (git's `fetch-haves`, thin with a have): git passes no
  `--shallow`, so only edges. The rule leaves the edges alone there, and git's golden still holds.
- **Bitmaps.** git's server with reachability bitmaps chooses objects another way. slates follows the walk without
  bitmaps, as A-615 states, so nothing changes here.
- **The one-round path** carries runs of D(k) and no edge rule: it is taken only where the client is not shallow and
  asks no deepening (A-615), so the aggressive rule never meets it.
