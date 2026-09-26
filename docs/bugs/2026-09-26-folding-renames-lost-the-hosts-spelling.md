# Folding renames lost the host's spelling; the differential compared one of several refusals

Date: 2026-09-26. Contracts: AC-1.2 (the differential suite), EQUIVALENCE §3–§4, T-1.2, POSIX
`rename(2)` and XSH 2.3 ("Error Numbers"). Found by CI run 36261369758 (`de11563`, Linux gates,
`PROPTEST_CASES=2000` on tmpfs) and by running the same suite on this Mac's APFS.

## Symptom

1. CI shrank a history to `mkdir b; create C; link b/C → C; rename b/C → b`: the volume said
   `EISDIR`, the Linux tmpfs host `ENOTEMPTY`.
2. On APFS (a folding host) the suite then found:
   - two more errno orderings (see the table below);
   - two listing differences: `rename A → a` left `A` in the volume where APFS stores `a`, and
     `rename d → a` over an existing `A` left `a` in the volume where APFS keeps `A`.

## Root cause

- **The errno cases are the suite's.** Each step met more than one refusal POSIX names, and POSIX lets
  an implementation report any one of them. The suite compared errnos one to one (with two fixed
  pairs), so a host that detects in another order failed it:

  | History | Refusals that apply | Volume | Host |
  |---|---|---|---|
  | file onto a non-empty directory | `EISDIR`, `ENOTEMPTY` | `EISDIR` | tmpfs `ENOTEMPTY` |
  | missing source into a path through a symlink | `ENOENT`, `ENOTDIR` | `ENOTDIR` | APFS `ENOENT` |
  | directory into itself onto a file there | `EINVAL`, `ENOTDIR` | `EINVAL` | APFS `ENOTDIR` |

- **The listing cases are the volume's.** Its rename returned early whenever the two names folded
  equal ("the same entry"), and on replacing an entry it stored the requested spelling. APFS keeps names
  as spelled: a case-only rename respells the entry, and a rename onto another spelling of an existing
  entry keeps that entry's spelling. EQUIVALENCE §4 makes a folding volume match its folding host, so
  `mv readme README` did nothing on a slates volume that APFS would have renamed.

## Fix

- **The suite** (`crates/vfs/tests/differential.rs`, `rename_refusals`):
  - From the state before each rename, it lists every refusal POSIX names: `ENOENT`, `ENOTDIR` (a
    path component, or a directory onto a file), `EISDIR`, `ENOTEMPTY`/`EEXIST` and `EINVAL`. Names
    compare under the run's policy.
  - When two or more distinct refusals apply, both sides may report any of them. Otherwise errnos
    compare exactly as before.
  - EQUIVALENCE §3 records the rule and the three measured orderings.
- **The volume** (`crates/vfs/src/volume.rs`, `Volume::rename`):
  - Another spelling of the same entry respells it: the same inode under the new name, the rename's
    usual record and directory times.
  - Only identical bytes are a no-op.
  - A rename replacing a different entry stores the replaced entry's spelling (`stored_name`, one
    O(log n) lookup, only when a target exists).
  - Under the exact policy both rules reduce to the old behaviour.
- **The oracle model** (`crates/vfs/tests/model.rs`) states the same rules, so model and volume agree.
- EQUIVALENCE §4 records both spelling rules.

## Tests

- `a_file_renamed_onto_a_non_empty_directory_may_report_either_refusal`: CI's shrunk history, replayed
  by name.
- `a_case_only_rename_respells_the_entry_on_a_folding_volume`: failed before the fix (`["readme"]`,
  expected `["README"]`).
- `a_rename_onto_another_spelling_keeps_the_replaced_entrys_spelling`.
- The vfs suite passes, including the model's proptest oracle. The differential on APFS passed 10 runs
  of 1,000 histories (it failed within one run before). The Linux tmpfs lane is proven by the next CI
  run; Docker was unavailable here.

## Edits

- `crates/vfs/src/volume.rs`, `crates/vfs/tests/{differential,model}.rs`, `docs/wip/EQUIVALENCE.md`,
  `docs/wip/TBD_FIXES.md`.
