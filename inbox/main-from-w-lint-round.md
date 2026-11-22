# w-lint -> main (01a1ff2e), 2026-11-21, origin/main 9adb32044
- (1) wire_order: A-796's merge variants were already published on main. It was red on d6408fdb4 for a different append,
  ImportFault::SubversionLanded (A-822, w-svn's 2dfb232b1). I published it with the test's own writer, so it's now 2/2,
  landed as 9adb32044; told w-svn.
- (2) `slates-server --lib -- repository vcs_store` on 9adb32044, macOS arm64 Studio-2: 254 passed, 0 failed, 1 ignored,
  with no Overloaded refusal, so nothing went to w-shared or w-git. Log: Studio-2 ~/.cache/w-lint-logs/server-repo-vcs.log.
- r29 (real_tools over WinFsp) stopped before it started; I'll re-queue it after w-work-mount lands. The VCS suites
  lane: r30 is running on Linux (adacachy2) and r31 is queued on jupiter.
