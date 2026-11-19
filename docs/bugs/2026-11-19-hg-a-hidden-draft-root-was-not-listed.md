# A hidden draft root was not listed to Mercurial peers (2026-11-19, A-691)

## Description

Found by g303 reading `crates/hg/src/serve/pull.rs` against 7.2's `phases.py`; reproduced by use and fixed by g083 (UI
goal 93). `listkeys phases` answered the served view's draft roots, computed over the view: its draft changesets none
of whose parents is draft. Stock 7.2 answers the draft roots its repository stores, read from the unfiltered changelog,
so a draft root an amend hid is listed though the served view leaves it out. For a repository whose draft root was
amended under evolution, stock 7.2 answered both the hidden draft root and its successor; slates answered the
successor alone.

## The rule, from 7.2's source

Read from Mercurial 7.2 from PyPI through uv on the Mac Studio (`mercurial/__version__.py`: `7.2`), on 2026-11-19:

- `phases.py` lines 1049-1058 (`listphases`): `cl = repo.unfiltered().changelog`; for each root in
  `repo._phasecache._phaseroots[draft]`, `if repo._phasecache.phase(repo, root) <= draft: keys[hex(to_node(root))] =
  value`. The roots are the stored ones, mapped to nodes through the unfiltered changelog, so a hidden root is listed;
  one whose phase a root of a higher phase raised is not.
- `phases.py` lines 195-229 (`_readroots`): the stored roots are read over `repo.unfiltered()`, and one whose node the
  changelog lacks is dropped (`removing unknown node ... from ... phase boundary`).
- `pushkey.py` lines 27-32: the `phases` namespace lists through `phases.listphases`, which `listkeys phases` answers.

## Root cause

A-619 wrote `listkeys phases` over the served view, since the view was all a `Repository` answered: the roots were
computed from its changesets, and A-619's oracle computed them the same way, over generated histories that hide
nothing, so the two always agreed. The register's stored roots, which the view reads (A-680 part 1), never reached the
answer.

## Impact

Every peer of a repository holding a hidden draft root (any draft root amended, rebased or pruned under evolution) was
answered fewer draft roots than 7.2 answers. A client ignores a root it does not know, so what a stock client did was
not observed to change; the answer itself differed from 7.2's bytes, which the design holds every answer to, and was
measured (below).

## Reproduction, by use (Mac Studio, macOS arm64, stock Mercurial 7.2 from PyPI through uv, 2026-11-19)

The repository: `crates/hg/fixtures/state/make-state-fixtures.sh . amended-root` (a public root; a draft root on it
amended under evolution, so a marker hides the old changeset; another draft on the root made secret afterwards),
rebuilt from the committed fixture files:

```
$ hg log --hidden -T '{rev} {node|short} {phase} {desc}\n'
3 d6b2f99ba770 secret draft root made secret
2 d57c1eb0d689 draft draft root, amended
1 4226264b8244 draft draft root
0 c3daaa7ea5f5 public public root
$ cat .hg/store/phaseroots
1 4226264b8244d7ff9affa7425d3c5b59b0227b9f
1 d57c1eb0d689f44baffe2277d99f381122fc8ac0
2 d6b2f99ba770dd408c76e4d893463582b6ee71de
$ hg debugpushkey <repository> phases
4226264b8244d7ff9affa7425d3c5b59b0227b9f	1
d57c1eb0d689f44baffe2277d99f381122fc8ac0	1
$ hg serve ... -a 127.0.0.1 -p 0          # bound to 127.0.0.1:58762
$ hg debugpushkey http://127.0.0.1:58762/ phases
4226264b8244d7ff9affa7425d3c5b59b0227b9f	1
d57c1eb0d689f44baffe2277d99f381122fc8ac0	1
```

7.2 had taken the secret one out of the draft roots when it was made secret, so this repository holds no stored draft
root whose phase a higher root raised; the view's generated model covers that case (below).

slates, before the fix (kiln r411 over c396a7391, `cargo test --locked -p slates-hg --lib serve::`, 53 passed and 1
failed): the view of the same repository answered `listkeys phases` with `d57c1eb0…` alone.

## Edits

- `crates/hg/src/serve/mod.rs`: `Repository::draft_roots`, the stored draft roots still draft, read past the served
  filter (7.2's `listphases`).
- `crates/hg/src/serve/indexed.rs`: `still_draft` takes the register's draft roots whose phase the forward walk found
  draft, before the filter keeps what it serves; a root whose node the changelog lacks was dropped before; the module
  doc's "Draft roots".
- `crates/hg/src/serve/pull.rs`: `listkeys phases` answers `Repository::draft_roots`, sorted and each once.
- The test repositories state their own (`serve/tests.rs`' `History`, `serve/getbundle_tests.rs`' `Served`,
  `push/tests.rs`' and `push/classify_tests.rs`' views, and in `crates/server` the hg push tests' view, the HTTP
  tests' empty store and the SSH tests' probed repository): each hides nothing, so its roots are its draft changesets
  with no draft parent.
- `crates/hg/fixtures/state`: `make-state-fixtures.sh` and `make-served-fixtures.sh` make and record `amended-root`;
  its files and stock 7.2's served answers; `SHA256SUMS`. `crates/hg/src/tests/fixtures.rs` lists them.
- `crates/hg/src/serve/indexed_tests.rs`: the golden `a_hidden_draft_root_is_listed_as_7_2_lists_it`; the view's model
  states `listphases` literally (`listed_draft_roots`, over `phases`, factored out of `model`) and the property test
  holds the view's draft roots to it; `a_hidden_draft_root_is_listed_and_one_under_a_secret_root_is_not`.
