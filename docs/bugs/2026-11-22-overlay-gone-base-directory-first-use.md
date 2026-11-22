# An overlay's gone base directory refused itself at its first use after a recovery, and a use at the descriptor limit recorded a live one gone (2026-11-22)

## Description
`crates/server/tests/recovery.rs`, `an_overlay_whose_base_directory_was_removed_serves_its_own_files_after_a_restart`,
fails on origin/main (w-svn's find, at main 7ced987e6 too; GAPS names it as not A-865's). After the restart, the test's
first `LOOKUP src` answers status 5 (`NFS3ERR_IO`, `tests/common/nfs.rs:238`). `src` is the overlay's base directory
that was removed while the daemon was down. It holds the overlay's own `agent.rs`, so it must still resolve; only its
untouched base entries (`lib.rs`, `mod.rs`) should refuse.

## Root cause
A backtrace at the refusal: `Export::lookup_result` → `VolumeBridge::attr_of` → `Overlay::observe` → `load_listing`
→ `reacquire`, which finds `src` gone, records it in `plane.unavailable` and returns `BaseUnavailable`.

`Overlay::observe` (`crates/vfs/src/base.rs`) loads a merged directory's listing for its link count, except for a
directory recovery could not reacquire. For that one it keeps the count it was recovered with, because the directory
is the volume's own node and answers for itself (§4.8's failure matrix, A-153). It tested this by
`plane.unavailable.contains_key(&no)`.

Before A-833 (6e3dec78d, 2026-11-21), a recovery reopened every recorded directory eagerly, so a gone `src` was in
`unavailable` before any request came. Since A-833 a recovery reopens only the base root, and each other directory at
its first use (`reacquire`). So the first `observe` of `src` was the call that discovered it gone. That call's
`BaseUnavailable` failed the whole LOOKUP, where every later call would have answered from the node. A directory's
answer depended on whether recovery found it gone eagerly or lazily.

The sibling, from main's ruling of the same day: `reacquire` recorded a directory unavailable on any refusal,
`EMFILE`/`ENFILE` included. Under the lazy reopen, a directory's first use at the descriptor limit (A-837's subject)
would record a live directory gone for good, and refuse its untouched entries until the volume was recovered again.

## Impact
- An overlay recovered over a base whose directory was removed: the first NFS LOOKUP (or `getattr`) of that directory
  answered `NFS3ERR_IO` (`BaseUnavailable` through the client) instead of the directory. A client that gave up on it
  never reached the overlay's own files in it. A retry would have answered.
- At the descriptor limit: a live base directory's untouched entries were refused `BaseUnavailable` for the rest of the
  volume's life in that daemon. No data was lost; the overlay's own entries kept serving.

## Edits
- `crates/vfs/src/base.rs`, `Overlay::observe`: when `load_listing` refuses `BaseUnavailable` and the directory is now
  recorded unavailable (this call found it gone), the directory answers from its node, as the eager path did. Any other
  refusal still propagates. `Overlay::base_dir_unavailable` names the test both places use.
- `crates/vfs/src/base.rs`, `Overlay::reacquire`: a refusal that passes (`passes`: descriptors or Windows handles
  spent, by `slates_machine::descriptors::is_exhaustion_errno` / `is_exhaustion_win32`) refuses this use with the
  host's code and keeps the listing, so the next use reopens the directory. It is never recorded unavailable.
- `crates/vfs/tests/base.rs`: `a_gone_base_directorys_first_use_after_recovery_answers_from_its_node` (the first use
  after recovery is a `stat` of the gone directory) and `a_base_directorys_first_use_at_the_descriptor_limit_never_records_it_gone`
  (the host armed to refuse `/src`'s next open `EMFILE`, through `SimHost::fail`: the first read refused with the host's
  code, the second reads the base's bytes). `listed_src_with_an_overlay_file` builds both.

## Evidence
- Before, the MacBook, main 27cae9bf7: the recovery test fails alone, `LOOKUP src: status 5`. Against the unfixed
  `base.rs`, both new vfs tests fail: the stat refused, and the second read refused `BaseUnavailable(Some(Errno(24)))`
  (the live directory recorded gone).
- After: the recovery test passes alone; both vfs tests pass; `slates-vfs` `--test base` 58/58, `--test base_identity`
  9/9, `--test recover` 50/50, `--lib base` 10/10; `slates-server --test recovery` as recorded in the landing.
