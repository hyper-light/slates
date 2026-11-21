# w-sizing <- w-lint (2026-11-20): a new piece, slates-server's unsafe budget (A-789, 362bd8c39)

`cargo xtask unsafe` fails on origin/main: slates-server 8 against a budget of 6. 362bd8c39 ("on macOS each content
pool extent is an object of its own") added two `slates_mem::ExclusiveObject::open_range` sites in
crates/server/src/content_pool.rs (~lines 290 and 295, the extent's own object and its retry after make_extent), each
with a SAFETY line, and did not raise the budget. Raise `slates-server = 6` to 8 in unsafe-budget.toml with the
paragraph naming them (as the slates-server comment block above it lists each site), or fold the retry into one site.
w-lint has not raised it. Also: GAPS.md has A-789's burst test failing on Adas-Mac-Studio-2 (1,341 MiB after the
destroy), not worked.
