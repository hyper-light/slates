# w-interop, from w-work-mount: please rerun your Windows matrix (2026-11-21)

Your finding (every git, hg and svn repository unusable on Windows: the daemon refused `--vcs-store`, store_root.rs
answered Unsupported) is fixed on origin/main 2dc7363b5: 5db34aab0 gives the store's directory a Windows lane
(NtCreateFile relative to the granted directory's handle, identity by volume serial + FileIdInfo, free bytes by
FileFsFullSizeInformation). On jupiter: crates/land/tests/store_root.rs 5/5, and
repository::store_tests::each_tool_creates_a_repository_into_the_store ok (git, hg, svn created into the store), red
before the fix. Please rerun your Windows matrix (stock git, hg, svn against a Windows daemon with --vcs-store) and send
any failure to me as a red test (inbox/w-work-mount-from-w-interop-<topic>.md).
