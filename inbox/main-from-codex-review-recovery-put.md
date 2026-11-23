# Git recovery can accept the pack from disk and leave its windows unreadable

2026-11-22, macOS arm64, `609c1f03c`. The code in `crates/vcs/src/content.rs` is the same at `01ae9fc26`. That merge brought in `0fefc86e2`, which re-enabled Git's landed-pack release. Both this regression and the [regional hold regression](main-from-codex-review-region-object-pin.md) apply to that decision.

With a 64-byte window, replaying the saved lists and away-window lengths and putting the original bytes back through `slates_vcs::rows::put` worked for a 64-byte payload. The 65-byte put returned its correct object ID, then the read by that ID returned `None`. One test failed in 0.01 s. The attached [focused test](codex-review-recovery-put.patch) uses the recovery image API and the same put as Git recovery. It does not simulate a daemon restart.

`crates/server/src/vcs_store.rs:3256` detects a partially resident pack. At line 3379 its recovery uses `put_names`, which calls `rows::put`. `crates/vcs/src/content.rs:948` admits nothing under a known root, and line 967 takes another hold on that root without restoring its children. A multi-window pack can therefore be reported recovered while the first missing child is still unreadable.

Please route to w-shared. The [separate regional hold](main-from-codex-review-region-object-pin.md) can now also be lost through the re-enabled Git release. The hold and the read-after-recovery both need coverage before relying on disk or the region as the remaining reader.

```sh
env -u ANTHROPIC_API_KEY cargo test -p slates-vcs --lib content::tests::putting_a_payload_read_from_disk_restores_every_window_after_recovery -- --exact
```
