# The conformance lanes judged each run over the last run's scratch, which the CI cache restored

## Description

- **What failed.** CI run 36681945355 (commit `76308bd`) failed both conformance lanes, macOS and Linux:
  - the git and rsync workloads differed, with the **host** reference exiting 1 and the mount exiting 0;
  - hermeticity failed its verdict with 0 outside writes.
- **Last green.** The run for `2de3276` passed both lanes.
- **Unrelated code.** Nothing either suite exercises changed between the two commits.

## Root cause

- **Where the scratch lives.** Since `d6e8153` (A-50) the scratch is `target/conformance-scratch`, and
  `Swatinem/rust-cache` caches `target/`. The `2de3276` run saved key `v0-rust-conformance-Linux-x64-b3412203-e20e8097`,
  and the `76308bd` run restored it with a full match.
- **The broken promise.** `Scratch::subdir` promised "a fresh subdirectory" but used `create_dir_all`, which
  reuses a directory that already exists. So every suite ran inside the last run's tree:
  - git's `ln -s` met the last run's `link` (`ln: link: File exists`), so the script exited 1;
  - rsync's `dst/` held the last run's copy;
  - the landing target held the last run's landing. The record shows `conformance-4610/…` beside this
    run's `conformance-4530/…`, so the landed tree differed from the mounted one.

## Impact

- **Only the harness was wrong.** No slates behaviour was at fault. Every run after a cached one failed
  its verdict, and the same thing happened to any developer rerunning with `--scratch` over a kept tree.

## Fix

- **Two accessors.** `Scratch::fresh(name)` empties the directory before a suite uses it, and a removal
  that fails is a refusal. `Scratch::shared(name)` is kept only for the tool builds (`tools`), whose
  reuse cannot change a verdict.
- **Where `fresh` is used.** The workloads host tree, the hermeticity landing target, the fsx logs and
  the pjdfstest output.
- **Test.** `a_host_reference_run_over_a_kept_scratch_starts_from_an_empty_tree` runs the real git workload
  twice over one scratch. It was red under reuse (`ln: link: File exists`, exit 1) and is green with
  `fresh`, judged Identical by the suite's own `compare`.

## Sibling sweep

- **Every scratch subdirectory is covered.** `tools` stays shared; the other four are fresh.
- **Root-level files start fresh anyway:**
  - `trace.log` is written fresh by the tracer (`strace -o`);
  - `trace-judgement.txt` and `fsstress-output.txt` are overwritten;
  - the anchor's log is named by the instance.
