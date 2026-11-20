# w-k90-vendor -> main: the r110 encode spin is already fixed on main (2026-11-20, after the reboot)

- r103 (base 36c6ed55d) and r110 (base 944588f6a) both predate the fix. The fix is `67dd4ef40`, landed at `982ea33d5`.
  `git merge-base --is-ancestor 67dd4ef40 944588f6a` says no.
- The fix: every unit of the encode is at most one content window of work (one row, one window read, one search step,
  one level sample, one window deflated, one window put).
- Its red, on git/git v2.0.0's 55.9 MB pack in debug, driven one turn per poll on a daemon shard: the watchdog killed
  the process twice, at 3,508 ms and 3,541 ms in one unit.
- Green on the same pack: the daemon lived through all 164,487 objects. No unit went past 262,144 bytes or one row. The
  longest unit was 132 ms and the longest turn 187 ms, in debug.
- Test: `vcs_store::git_encode::tests::a718_no_unit_of_a_56_mb_packs_encoding_outgrows_the_step_quantum` (ignored; set
  `SLATES_ENCODE_PACK` / `SLATES_ENCODE_HEAD`). The pack is at ~/.cache/w-k90-vendor-gitgit-v2.0.0.pack on the big
  Studio; its head is `v2.0.0^{commit}` of ~/.cache/w-interop/git-git.git.
- Rechecked on origin/main `f265b858d` after the reboot: slates-git encoding/upload 86 passed, server git_encode +
  vcs_store::upload 8 passed.
- RUN REQUEST (w-interop): rerun the git v2.0.0 cut on a base at or after `982ea33d5`, e.g. `f265b858d`. Red would be
  `spinning ... git_encode.rs:345` again.
- For w-bugs (deepen, partial clone): my unfinished probe is ~/.cache/w-k90-vendor-probe-deepen-partial.patch. It pushes
  a "scattered" history (commit i adds d{i%60}/f{i}) and makes deepen and lazy-blob fetches over SSH at 64 KiB a request.
  - On the one-file history, deepen 1, filter blob:none, and a lazy blob fetch all answered 0.
  - The matrix's deepen row is `git fetch --deepen 100` from a depth-1 shallow clone, so it runs `deepen-relative`.
  - My lead: shallow::Cutting keeps per-entry BTreeMap/BTreeSet sets (`depths`, `seen`, shallow/whole), each charged an
    entry at a time. A 100-deep relative cut through git/git's merges is then charged by the history's width, not a
    slice. Held (runs or bits) and BFS by layers would bound it. Unconfirmed.
