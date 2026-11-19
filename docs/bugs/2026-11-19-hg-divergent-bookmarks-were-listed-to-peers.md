# Divergent bookmarks were listed to Mercurial peers (2026-11-19, A-690)

## Description

Found by g303 reading `crates/hg/src/serve/pull.rs` against 7.2's `bookmarks.py`; reproduced by use and fixed by g083
(UI goal 93). `serve::pull::bookmark_entries` listed every bookmark naming a changeset the served view holds, divergent
names among them, and both `listkeys bookmarks` and a `getbundle`'s `bookmarks` part read it. Stock 7.2 leaves a
divergent name out of both. For a repository where `feature` moved apart in a clone and the clone was pulled back, so
7.2 kept the clone's as `feature@1`, stock 7.2's `hg serve` answered `@` and `feature` and a clone over it held those two;
slates' view of the same repository answered `feature@1` too.

## The rule, from 7.2's source

Read from Mercurial 7.2 from PyPI through uv on the Mac Studio (`mercurial/__version__.py`: `7.2`), on 2026-11-19:

- `bookmarks.py` lines 471-472 (`isdivergent`): `return b'@' in b and not b.endswith(b'@')`. A name with an `@` before
  its end is divergent; `@` alone, and any name ending with `@`, is not.
- `bookmarks.py` lines 475-484 (`listbinbookmarks`): `if hasnode(v) and not isdivergent(k): yield k, v`, commented
  "don't expose local divergent bookmarks"; lines 487-491 (`listbookmarks`) list the same as hexadecimal.
- `pushkey.py` lines 27-32: the `bookmarks` namespace lists through `bookmarks.listbookmarks`, so `listkeys bookmarks`
  leaves divergent names out.
- `exchange.py` lines 2638-2649 (`_getbundlebookmarkpart`): the `bookmarks` part is
  `binaryencode(repo, listbinbookmarks(repo))`, so a clone or a pull is never sent one either.
- `bookmarks.py` lines 592-614 (`_diverge`): how a pull names the remote's moved-apart bookmark, `name@pathalias` where
  the pulled path has an alias and `name@number` otherwise.

## Root cause

A-619 wrote the listing as "every bookmark naming a changeset the served view holds" and held it to an oracle that
stated the same rule (`crates/hg/src/serve/tests.rs`, `oracle_listkeys` and the `bookmarks` part's expectation) over
generated histories whose bookmark names never held an `@`, so the oracle could not see the rule it left out: the
CLAUDE.md gotcha about a model that states the implementation's rule. The push side already had the rule
(`push/classify.rs`'s `divergent`, refusing a divergent name as 7.2's `handlebookmark` does); the pull side did not
share it.

## Impact

Every peer of a repository holding a divergent bookmark was shown it: `listkeys bookmarks` answered it and the
`bookmarks` part of a clone or a pull carried it, where 7.2 shows neither. A repository gets one by a pull (an import of
a stock repository brings its divergent bookmarks, A-675). What a stock client did with the extra name was not measured,
since no daemon route serves Mercurial yet (A-680 part 2); the answers themselves were (below).

## Reproduction, by use (Mac Studio, macOS arm64, stock Mercurial 7.2 from PyPI through uv, 2026-11-19)

The repository: `crates/hg/fixtures/state/make-state-fixtures.sh . divergent` (a root bookmarked `feature`; the
repository cloned, a changeset on `feature` in the clone and another here; the clone pulled back, which printed
"divergent bookmark feature stored as feature@1"; `@` on the root), rebuilt from the committed fixture files and served:

```
$ hg bookmarks
   @                         0:227d4fea737b
   feature                   1:e532f4e59b1a
   feature@1                 2:bd211ad51c55
$ hg serve ... -a 127.0.0.1 -p 0          # bound to 127.0.0.1:54846
$ hg debugpushkey http://127.0.0.1:54846/ bookmarks
@	227d4fea737b6ade41f90d5edc5cd84f8a935b25
feature	e532f4e59b1abec9d29cae641034b40d2eeb03de
$ hg clone http://127.0.0.1:54846/ clone && hg -R clone bookmarks
 * @                         0:227d4fea737b
   feature                   1:e532f4e59b1a
```

slates, before the fix (kiln r340 over ab23dd3e0, `cargo test --locked -p slates-hg --lib serve::`, 52 passed and 1
failed): the view of the same repository answered `listkeys bookmarks` with
`@ 227d4fea…`, `feature e532f4e5…` and `feature@1 bd211ad5…`, where 7.2 answers the first two.

## Edits

- `crates/hg/src/bookmarks.rs`: `divergent` (7.2's `isdivergent`) and its byte moved here from `push/classify.rs`,
  crate-wide, with the module doc's "Divergent names".
- `crates/hg/src/push/classify.rs`: shares it (no change in behaviour).
- `crates/hg/src/serve/pull.rs`: `bookmark_entries` leaves a divergent name out, so `listkeys bookmarks` and the
  `bookmarks` part do; the module doc and the function's doc state 7.2's rule.
- `crates/hg/src/serve/mod.rs`: `Repository::bookmarks`' doc names the rule.
- `crates/hg/src/serve/tests.rs`: the generator names every third bookmark with an `@` inside it and every third with
  an `@` at its end; `oracle_listkeys` and the `bookmarks` part's expectation state `isdivergent` as its source does.
- `crates/hg/fixtures/state`: `make-state-fixtures.sh` and `make-served-fixtures.sh` take repository names and make
  `divergent`; its files and stock 7.2's served answers; `SHA256SUMS`. `crates/hg/src/tests/fixtures.rs` lists them.
- `crates/hg/src/serve/indexed_tests.rs`: `a_divergent_bookmark_is_served_as_7_2_serves_it`, the golden.
