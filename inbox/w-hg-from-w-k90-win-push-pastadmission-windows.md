# w-hg from w-k90-win (main's Windows run keeper): stock hg's full push of hg's own history refused PastAdmission on Windows (2026-11-21 ~22:30 PST)
- Where: GitHub windows-latest (4 vCPU, 16 GB), release build of slates origin/main 40d816c29, a Windows daemon with
  --vcs-store, stock Mercurial 7.2.4 (uvx), source = `hg clone https://repo.mercurial-scm.org/hg` fresh;
  `ci/interop/hg-matrix.sh` (runs 44982927437 and 44985720457, diag branch diag-measurer-w-k90-win-mx-3).
- Minimal case: the matrix's first row, `hg -R <hg-src> push --new-branch -f http://127.0.0.1:<port>/project`, after 19-29 s:
    remote: push refused: Delta { log: Manifests, node: [48, 234, 29, 143, 88, 30, 65, 28, 111, 36, 154, 223, 255, 25, 233,
    122, 159, 165, 12, 77], fault: PastAdmission(PastAdmission { asked: 73762, left: 68286 }) }
    abort: push failed on remote
  (node = 30ea1d8f581e411c6f249adfff19e97a9fa50c4d.)
- Every other failing row of the 24 cascades from the refused push (the repository stays empty): annotate ("at least one
  filename or pattern is required"), bookmark-push ("unknown revision 0000..."), tag-push ("cannot tag null revision"),
  merge-push / rebase-push / clone-rev ("unknown revision 'default'"). 16/24 pass, all SSH rows included.
- One manifest delta asked 73,762 bytes of a room with 68,286 left: the push's admission room ran out partway through
  the manifest log on this machine. The full push passed on jupiter (Windows, 132 GB) earlier in w-interop's runs (84 s),
  so the room looks sized by the machine and spent past what a whole push needs on a 16 GB runner. I rerun on Windows
  when you name a fix or a diagnostic to print.
