# The svnsync use row compared two random mirror UUIDs, so it could never pass (2026-11-23)

## Description
Main's round (B), `a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve` with `SLATES_SVN_USE_VERBS=svnsync`
over `http` (the big Studio, after 414925522 served `replay-report`), failed with one difference: the two mirrors'
`svnadmin dump`s differed in the `UUID:` line alone (stock `3a47c9d9-…`, slates `b4f84b7f-…`).

## Root cause
The harness. `mirror_dump_of` (`crates/cli/tests/svn_interop.rs`) made each leg's mirror with `svnadmin create`, which
gives a repository a random UUID, and stock `svnsync` never sets a mirror's UUID: Subversion 1.15.1
`subversion/svnsync/svnsync.c` `do_initialize` records the source's UUID only in the `svn:sync-from-uuid` revision
property, and says "the RA layer doesn't have a way to set a UUID". So the two mirrors' dumps differ on every run,
whatever the server. `ci/interop/svn-matrix.sh` already gives its mirror a fixed `MIRROR_UUID` with `svnadmin setuuid`;
the Rust row did not.

Ruled out first, on the MacBook (a debug slates at main 65cdf08b0, `crates/svn/fixtures/dump/full.dump` imported):
the import keeps the dump's UUID, and the HTTP edge reports it (the OPTIONS `SVN-Repository-UUID` header and
PROPFIND's `lp2:repository-uuid` both `7bf7a5ef-cabf-0310-b7d4-93df341afa7e`, the dump's).

## Impact
The svnsync use row failed on every route. The HTTP edge and the import were not wrong. Nothing in slates changed.

## Edits
- `crates/cli/tests/svn_interop.rs`: `MIRROR_UUID`; `mirror_dump_of` gives each mirror that UUID before its `init`,
  as `svn-matrix.sh` does, so the `UUID:` line is still compared and is the same on both legs.

## Evidence
- Before: main's (B), one difference, the `UUID:` line.
- After: owed to main's rerun of (B) (no stock Subversion on the MacBook). The test builds and skips loudly here.
