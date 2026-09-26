# A renamed file's sidecar arrived through its old name, and a real `._` file appeared

Date: 2026-09-26. Contracts: §4.6 "Extended attributes over NFSv3" (A-33), AC-4.2 (workloads).
Found by the first local workload run with the AppleDouble view in place: git, rsync and vim still
saw `._` entries (`refs/heads/._master`, `._note.txt~`), while cargo, python, rg and sqlite passed.

## Symptom

`mv c d` on a file with attributes left a real 4,096-byte `._d` in the directory, and `ls` listed it.

## Root cause (from a request trace)

Temporary logging of every NFS procedure with its file handle and name showed that one `mv c d`
issued:

| Step | Call | What the server did |
|---|---|---|
| 1 | `RENAME c → d` | moved the inode, its attributes with it |
| 2 | `LOOKUP ._c` | `NOENT`: `c` no longer exists |
| 3 | `CREATE ._c` | no owner `c`, so a **real** file |
| 4 | `WRITE`, `COMMIT` to it | doubleagentd wrote `com.apple.provenance` |
| 5 | `SETATTR` on `d` | xnu's mtime touch on the owner |
| 6 | `LOOKUP ._d`, then `RENAME ._c → ._d` | the real file became `._d` |

After the main rename, xnu stamps the moved file through its **old** vnode name (the identity
update comes later in `vn_rename`, `bsd/vfs/kpi_vfs.c` at `xnu-12377.121.6`). It then renames
`._old` onto `._new` to carry the sidecar along. On a filesystem that stores `._` files natively,
`._old` already existed and simply moved.

A first fix merged a real orphan into the owner on that rename. It removed the entry but broke the
client's cache: the client then held `._d` as the deleted orphan, and `xattr -l d` read nothing
until the attribute cache expired.

## Fix

- **A departure per directory** (`Volume::departed`): a rename records the vacated name and the moved
  inode. A lookup or create of `._old` while `old` is absent resolves to that inode's view, so the
  gap write merges into the real attributes. The trailing `RENAME ._old → ._new` names the same view
  on both sides, so it is completed as a no-op and the departure is forgotten.
- **Bounded:** one departure per directory, replaced by the next rename there, removed when the
  directory is reclaimed. It is not recovered: a restart mid-rename loses only a window the client
  has not been told about.
- **The orphan merge stays** as the fallback when two renames overlap in one directory.

## Evidence after the fix

- Live on macOS 26.4.1: `user.q` and `com.apple.provenance` survive `mv c d` and `mv d sub/e`, and
  `find -name '._*'` is empty after a git commit.
- `cargo xtask conformance run --suite workloads`: git, cargo, npm, python, rg, rsync, sqlite and vim
  are all identical.
- The npm difference in the same run was the harness: npm redacts UUID-shaped path segments as `***`
  and the scratch directory's name was a UUID. `normalize_output` now matches that spelling too,
  with a test.

## Edits

- `crates/vfs/src/volume.rs` (departures, and their removal on reclaim)
- `crates/bridge-core/src/volume_bridge/view.rs` (`view_owner` resolves departures, `view_rename`)
- `crates/bridge-core/src/lib.rs` (`appledouble_rename`)
- `crates/bridge-nfs/src/procedures.rs` (RENAME routing)
- `crates/conformance/src/workload.rs` (npm redaction)
