# The long push's test allocated beside the heap readers of its binary (2026-11-23, goal 93)

Found by use: `crates/hg/tests/push_heap.rs`'s `a_cached_file_base_is_read_where_it_lies` failed in 4 of 6 runs of its
binary on adacachy2 at main e836e65eb, each time with "a revision allocated 330710 B, a copy of its 163840 B file's
text". It is a defect of the test binary, not of the intake: the same test passed alone, and the whole binary passed 4
of 4 tests on one test thread.

## Description

The binary installs the slates allocator as its global allocator, and its heap tests read the process's figures before
and after their work. Two tests reading at once would read each other's allocations, so each reader holds `ALONE`, a
lock of the binary, while it runs. `a_long_pushs_fork_keeps_its_runs_within_a_logarithm_of_its_rows` (c6fab004e, the
index seal's reland, 2026-11-23) takes a history of `LONG_EDITS` edits through the intake and reads no heap figure, so
it was written without the lock. Run on its own thread beside `a_cached_file_base_is_read_where_it_lies`, its
allocations were counted as that test's revision.

## Measured

On adacachy2 (x86_64, 32 cores, shared), the debug build, `cargo test -p slates-hg --test push_heap`, test threads as
the harness chooses:

| Binary | Runs | Failed |
|---|---|---|
| e836e65eb as landed | 6 | 4 (`a_cached_file_base_is_read_where_it_lies` each time) |
| the cached base's test alone | 1 | 0 |
| one test thread (`--test-threads=1`) | 1 | 0 of 4 tests |
| with the long push's test holding `ALONE` | 6 | 0 |

The runs had the long push's bound in integers, as main's 4b913b9e6 then wrote it. From e836e65eb to 0d9cc659c, where
this lands, no crate the binary links changed (`slates-hg`, `-machine`, `-mem`, `-merge`, `-pace`, `-test-seeds`,
`-vcs`) but that bound line and its twin in `crates/vcs/src/tests.rs`, nor the lockfile or the toolchain. At 0d9cc659c
itself, on the Mac (macOS arm64, 18 cores at load 60 to 90 from other sessions), the red in an APFS clone of the tree
with the lock taken out, the green in the tree:

| Binary at 0d9cc659c | Runs | Failed |
|---|---|---|
| as main has it, the long push's test without `ALONE` | 6 | 6 (`a_cached_file_base_is_read_where_it_lies` each time) |
| with the long push's test holding `ALONE` | 6 | 0 |

## Root cause

The lock's rule was stated for readers ("held by each test while it reads the heap's figures"), but the figures are
the process's, so the rule a heap reader needs is that nothing else in the binary allocates while it reads: every test
of the binary must hold the lock, the ones that read no figure too.

## Impact

CI's gates run the binary within `cargo test --workspace` (`.github/workflows/ci.yml`), the harness's test threads,
so `a_cached_file_base_is_read_where_it_lies` would turn that lane red on most runs from c6fab004e on, for no defect of
the code it measures.

## Edits

- `crates/hg/tests/push_heap.rs`: `a_long_pushs_fork_keeps_its_runs_within_a_logarithm_of_its_rows` holds `ALONE` for
  its whole body, as the other three tests do. `ALONE`'s doc states the rule for every test of the binary, and its D-8
  comment names all four holders (it named two, though three already held it).

## Siblings

Every test binary with a global allocator was read (24 files). Those whose counters are per thread
(`crates/vfs/tests/delta_cost.rs`, `crates/rt/tests/timer_allocations.rs`, `crates/mem/tests/no_alloc.rs`) cannot meet
this, nor can a binary with one test. `crates/store/tests/text_cache_heap.rs` has the same shape and no lock: five
tests, and `the_store_keeps_the_texts_it_made_off_the_system_heap` reads the process's `heap()` figures before and after
its work while the others may allocate. Measured on adacachy2 at e836e65eb: 6 of 6 runs of the binary passed (four
tests and one ignored each), so it has not been seen to fail; the shape is reported to main, not changed here.
