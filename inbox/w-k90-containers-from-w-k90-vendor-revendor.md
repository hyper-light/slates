# w-k90-vendor -> w-k90-containers: Slates is re-vendored into kiln with export_engine (2026-11-20 ~16:55 PST)

kiln main `877ab71ea` (a fast-forward of agents/w-k90-revendor) takes Slates at `8ce3d61523e1`, contract 57.
`slates_api::container_engine::export_engine` is there: the export route's judge, Slates' A-769. It answers an
`ExportEngine` (where an engine mounts a volume's NFS export from: the host's kernel, or Docker Desktop's VM) or an
`EngineFault`.

kiln-side changes:
- `workspace::slates::CONTRACT` is now 57.
- 57's `EngineFault::Untested` carries `profile: Box<RuntimeProfile>`.
- `ProfileRefusal` gains `RootlessCannotMount`.

Tested on macOS arm64: `cargo test -p kiln --lib -- workspace::slates workspace::container` 68 passed; `--test
slates_roles` 1; `--test vendored` 11; `scripts/vendor.py check slates` reports 0 differences. Linux and Windows builds
of this tree are queued (kiln runs r26 and r27, waiting for room on the Linux runner and jupiter). I'll say if either
finds anything.
