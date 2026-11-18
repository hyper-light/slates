# winfsp-bench -> g258-step-continued-ada: the Windows landing writer (goal 6773b0e6910f8d6ca8a7bee2d544a0f2)

State as of 2026-11-17. It was written here because this session had no kiln agent tools, and `SendMessage` could not
reach `g258-step-continued-ada`.

## Branches (hyper-light/slates)

- **`agents/winfsp-bench-land-4811191a` at `7824e22d`.** The Windows writer (A-185) on main, with main merged in up to
  B9 (`1ed67337`, `9ba16222`, `84d1f59a`, `1c49c33f`), as merge commits and never rebased.
  - Gate run `42708861224` is green on four runners: x86_64 and arm64 Windows from pwsh, Ubuntu and macOS.
  - The supervisor said they would merge `7824e22d` into B11 (`agents/land-b11-wip`, on main `db9c514e`) and re-gate it.
  - It was not on main as of 2026-11-16.
- **`agents/winfsp-bench-ownmount-typed` at `e99829c2`, on `db9c514e`.** The Unix own-mount landing test asserts
  `LandingTarget` and no `TargetUnavailable`. It was red-checked on Linux and macOS:
  - run `42712764148`: passes as written;
  - run `42712785924`: with `OwnMount` reported as `Unavailable`, fails;
  - run `42712805607`: main's old assertion with the same break still passes.
  It is ready to land.

## Done

- The port of `agents/windows-land-wip`.
- A-227's own-drive check, run in `walk_dir` before any open: red (the daemon wedged) and green, on both Windows runners.
- The owner check: the token's user SID or its default owner, tested and red-checked both ways.
- `set_mode` keeps the `ARCHIVE` bit: a landing over a placed file had been refused as a conflict.
- Release's `5f910dd3` server lib step.
- `slates-mcp`'s tests on both Windows lanes, and `slates-land` linted on both.
- `land_bench.rs`'s four Windows lint errors.
- Follow-up `dumps.rs`: main's `b72f8c34` had already done it, so no branch.

## Left

1. After the land branch is on main:
   - Remove the stale GAPS line saying `dumps.rs`'s unit test cannot build on Windows. It is the first item under
     "Reported, not changed (2026-11-09)".
   - Delete these branches, as the supervisor authorized once the land branch is on main:
     - `agents/winfsp-bench-server-lib`, `agents/winfsp-bench-land` and `agents/winfsp-bench-land-ecb86b4f`;
     - `diag-winfsp-bench-land-12` to `-17` and their `diag-winfsp-bench-land-results-*` branches;
     - `diag-winfsp-bench-ownunix-1` to `-3` and `diag-winfsp-bench-ownunix-results-*`.
2. Reported in GAPS, not changed:
   - A-262 shows every reparse point as a link, so a cloud placeholder or a deduplicated file reads as one. Not
     measured on a host holding one.
   - `landing_fairness` was slow on Ubuntu. The comparison with main was inconclusive: 2 runs each.
   - The line "was not imaged, skipped: EIO" appears during a landing on Windows and macOS, though not on Linux.
3. GAPS entry owned by winfsp-bench: the timer wheel commits its whole bound on Windows. That is why the server lib and
   client tests run one at a time on Windows.

## Notes and logs

- Bug records, on the land branch, in `docs/bugs/`:
  - `2026-11-06-an-elevated-token-owned-nothing-it-made.md`
  - `2026-11-09-the-windows-writer-walked-its-own-drive-before-a-227-looked.md`
  - `2026-11-09-the-windows-writer-cleared-the-archive-bit-ntfs-sets-again.md`
- Diagnostics workflows on jupiter, in this directory: `diag-legs.yml` (four runners' legs) and `diag-ownunix.yml`.
  Results publish to `diag-winfsp-bench-*-results-*` branches.
- Worktrees, in this directory: `wt-land3` (the land branch) and `wt-own` (the own-mount branch). Target directory:
  `C:\Users\corph\.cache\slates-agents-target-winfsp-bench`.
- Gotchas:
  - Run Windows tests from PowerShell, never Git Bash, which sets the token's default owner to the user.
  - jupiter has no WinFsp, so live mount tests run on CI only.
  - A child `powershell` 5.1 under CI's pwsh 7 needs `PSModulePath` cleared.
  - GitHub's unauthenticated API is limited to 60 requests an hour for jupiter's address, so poll result branches with
    `git ls-remote` instead.
