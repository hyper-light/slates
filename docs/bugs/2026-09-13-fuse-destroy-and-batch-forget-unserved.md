# 2026-09-13 — The FUSE codec answered `FUSE_DESTROY` with `ENOSYS` and never swept; `FUSE_BATCH_FORGET` was not an opcode it served

## Description

Building the virtio-fs device's high-priority queue (virtio 1.2 §5.11.6.2: `FUSE_FORGET`,
`FUSE_BATCH_FORGET` and `FUSE_INTERRUPT` ride it) against `slates_bridge_fuse::bridge::dispatch`
showed two gaps in the codec every transport shares:

1. `FUSE_DESTROY` (opcode 38) was in the `Opcode` set but fell to the dispatch's
   `_ => ENOSYS` arm. The kernel ignores the reply to DESTROY, so nothing was visibly wrong at a
   `/dev/fuse` unmount — but the attachment's references were never swept
   (`Bridge::sweep_attachment`, §4.6: called "once when an attachment ends — a FUSE unmount ...
   since FUSE does not guarantee a `FORGET` per outstanding reference"), so every inode the kernel
   still referenced at unmount stayed pinned in the volume until the volume's own teardown.
2. `FUSE_BATCH_FORGET` (opcode 42) was not in the `Opcode` set at all, so the batched forget the
   kernel sends over `/dev/fuse` (and a virtio-fs guest may send on the hiprio queue) was answered
   `ENOSYS` — a 16-byte reply to a request that has no reply — and none of its references were
   dropped.

Failing-first: `crates/bridge-fuse/tests/dispatch.rs`
`batch_forget_drops_every_listed_reference_with_no_reply` ("BATCH_FORGET has no reply (was
ENOSYS: 16 bytes)": left 16, right 0) and `destroy_sweeps_the_attachment_and_replies_success`
("DESTROY succeeds (not ENOSYS)": left 4294967258 = `-ENOSYS`, right 0). Run:
`cargo test -p slates-bridge-fuse --test dispatch` → 2 failed of 8, then 8 passed after the fix.

## Root cause

The dispatch grew opcode by opcode as the Phase 3 driver needed them; DESTROY was named (the
enum) but its arm was never written, and BATCH_FORGET was never named. The codec's tests drove
FORGET (single) and had a `swept` counter on the mock that no test asserted.

## Impact

- Every FUSE mount (`/dev/fuse`) leaked its outstanding lookup references at unmount until the
  volume was torn down: memory held past the mount's life; an unlinked-but-referenced file's
  content not reclaimed at unmount.
- A kernel that batches forgets (the `/dev/fuse` path does, `fuse_dev_do_read` with
  `FUSE_BATCH_FORGET`, minor ≥ 16 — which slates negotiates at 7.31) had *none* of those forgets
  applied: references accumulated for the mount's life, the same leak, larger.
- The virtio-fs device inherits both fixes through the shared dispatch.

## Exact edits

- `crates/bridge-fuse/src/abi.rs`: `Opcode::BatchForget = 42` (Format: `FUSE_BATCH_FORGET`) and
  its entry in `ALL`.
- `crates/bridge-fuse/src/bridge.rs`: `Opcode::BatchForget => serve_batch_forget(...)` — parses
  `fuse_batch_forget_in { count, dummy }` then `count × fuse_forget_one { nodeid, nlookup }`,
  applying only the complete entries the body holds (an overclaimed count never reads past the
  message), no reply; `Opcode::Destroy => serve_destroy(...)` — `bridge.sweep_attachment(cx)`
  and an empty success reply.
- `crates/bridge-fuse/tests/dispatch.rs`: the two tests above (the overclaimed-count case
  included).

Commit `d42d5ff`.
