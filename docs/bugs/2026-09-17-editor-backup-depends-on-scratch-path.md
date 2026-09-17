# Editor workload silently skips backups in temporary paths

Date: 2026-09-17. Design: Part 6 real workloads, AC-3.2 / T-3.3.

## Evidence and cause

Ubuntu conformance job [105312670403](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670403)
reported eight identical workloads and one editor difference at 18:20:13 UTC:
the host output contained `note.txt~`; the mounted output ended before that entry.
The actual roster script is the failing real-save reproduction. Host scratch lived beneath
`/home/runner/work/_temp`; the mount helper chooses a system temporary directory.

The script enabled `backup` and `writebackup`, but left Vim's `backupskip` default intact.
That option overrides backup creation when the edited file matches a temporary-directory
pattern. The two roots therefore selected different editor behavior despite identical flags.
The installed Vim 9.1 reference, `/usr/share/vim/vim91/doc/options.txt`, lines 1274–1297,
documents the Unix, macOS and environment-derived temporary patterns and their effect.
This is a workload configuration defect; excluding backup files from comparison would hide it.

## Edits

- Clear `backupskip` explicitly in the editor roster command.
- Read the backup into the compared output. A missing backup now fails the script even when
  both host and mounted paths match a temporary pattern.
- Add a real Vim regression that runs the unchanged roster entry in an ordinary path and a
  path matching its child process's TMPDIR. Assert exact edited and backup bytes. The fixture
  cleans up its own process-named directory under `SLATES_TEST_RAMDIR`.
- Keep the reviewed manifest exclusions and expected-failure lists unchanged.

## Validation and remaining execution

The red real-save comparison is the recorded CI run above; it was inspected before this edit.
The new regression compiles. Its local save execution requires RAM-backed scratch, which the
host has not been authorized to create. Available cached Rust Linux images have no Vim; no
tool was installed. The fixture skips loudly without the supplied RAM directory or Vim.

Once an existing RAM directory is supplied or the requested temporary RAM volume is authorized:

```
SLATES_TEST_RAMDIR=<RAM directory> cargo test -p xtask \
  conformance::workloads::tests::an_editor_save_preserves_its_backup_even_inside_tmpdir \
  -- --exact --nocapture
```

No successful local real-save run or full native conformance rerun is claimed by this change.
