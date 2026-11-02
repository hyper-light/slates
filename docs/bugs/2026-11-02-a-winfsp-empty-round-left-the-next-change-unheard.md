# A WinFsp empty notification round left the next change unheard

Observed 2026-11-02, A-198 Step 9.4.

## Description and failing test

`a_winfsp_view_follows_its_file_after_an_sdk_rename` opens and faults a mapped page, renames its file through the SDK,
then rewrites the same inode through the new name. The old view must follow while an ordinary open of the removed
name still fails. The unchanged test fails in [41480565373](https://github.com/hyper-light/slates/actions/runs/41480565373)
on both Windows architectures, with zero purge outcomes and echoes. Callback-by-callback tracing had made the case
pass, so those passing traced runs were insufficient.

The quieter stage trace [41482946787](https://github.com/hyper-light/slates/actions/runs/41482946787), commit
`01923954cfa6137dba286f9972a85f1cc3e2d7ae`, confirms the missing subscription. On arm64, the rename's parent-inode
changes are gathered at cursor 4 with `names=1 records=0`. Every round and purge counter is still zero at the failed
view deadline. Only the test's later ordinary pathname lookup wakes the mount: the file's truncate/write changes
are then gathered from cursor 5 with `names=1 records=2`, and notification and purge finally start. No mount failure
or refused notification precedes the stale view.

Command: `cargo test -p slates-cli --test winfsp -- --nocapture --test-threads=1`, with
`SLATES_TEST_CLI=1 WINFSP_TEST_MOUNT=1`; the diagnostic adds stage logs but changes no delivery decision.

## Root cause

`Volume::wait_for_change` registers a waker only when its observed generation is current. A mount turn that sees a
change returns ready without registering. The WinFsp task gathered that change, advanced its seen generation, then
parked with `futures::idle`. When the round had no records, neither a command outcome nor a new kernel request was
guaranteed to wake it. The following SDK write therefore changed the journal without waking the mount. This is why
a rename of a viewed file exposed it: the directory changed, but Windows held only the file open.

## Exact correction and sibling sweep

`crates/server/src/winfsp.rs::turn_with` registers the waker again at the generation it just delivered, before it can
park. This happens on the same owner shard, so no mutation can run between reading the generation and registering.
A missing volume ends the mount rather than parking. No polling interval, retry loop or alternate path is added.

The other production callers were checked with `rg -n 'wait_for_change\(' crates/server/src`. FUSE registers in
`wait_for_work` after its turn; NFS writeback registers in its next keeper poll. They do not share this ordering.
The separate private-intent event regression remains necessary for callback-free channel progress; it did not fix
this missing volume subscription. The unchanged live and takeover rename cases and twenty-kill history supply the positive verification below.

## Verification

Diagnostics [41484542209](https://github.com/hyper-light/slates/actions/runs/41484542209), commit
`a7e5bce5c4ec70c9773a434887cc66c9b865a90e`, pass without operation traces on both Windows architectures. Each
runner passes all nine live CLI tests and loses zero drives in twenty kills; every iteration requires the original
handle's counted read and the original mapped view following the successor, with four restored reads in the final
daemon. The live renamed view records one completed purge/echo; the renamed takeover records two. Neither revives
the removed public name, and no purge is refused or kept. All six separate foreign process-rights requests are
denied, and the independent second daemon is refused while its original owner still serves.

The same run passes all 54 bridge tests, one separately enabled real-driver captured-read oracle (one counted
settlement, 0.15 s on x86_64 and 0.19 s on arm64), all 32 active anchor tests, native clippy and private Windows
documentation. The CLI histories take 41.51 s on x86_64 and 21.76 s on arm64. Mac formatting, workspace clippy,
structural/literal/unsafe checks and private workspace documentation pass in 92.15 s on the same code.
