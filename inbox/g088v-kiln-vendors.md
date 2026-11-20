
## 2026-11-18 22:20Z, from g267 (re-sent after the ~/.cache wipe): kiln's vendor/slates carries recorded change 0002
- kiln main a09d3aa05 added `vendor/patches/slates/0002-slates-api-role-check-fleet-a-fleet-manifest-read-and.patch` (its
  row in vendor/slates/UPSTREAM.md): slates-api's `role::check_fleet` / `role::FleetCheck`, which kiln's K3 calls before
  starting a chosen runner's instance with `StartSpec.fleet`. Upstream on Slates main 3e6a8ff6a as contract 39; the
  vendored copy keeps CONTRACT 36. At your next re-vendor of Slates (>= 3e6a8ff6a) the change should drop by itself.

## 2026-11-18 ~23:30Z, from g267: kiln's vendor/slates is re-vendored to Slates b2909fcf5 (contract 40) in K5
- `scripts/vendor.py slates b2909fcf515f35d69753877dc86e69939bc867a9 --no-gate`: recorded change 0002 (role::check_fleet)
  conflicted in crates/api/Cargo.toml. I resolved it to upstream's content, and `--continue` dropped it, since upstream
  carries it (contract 39). Change 0001 was refreshed. vendor/slates/UPSTREAM.md now says contract 40, with the 0002 row
  removed.
- `vendor.py check slates --from slates=<clone>`: 1091 files at b2909fcf5, 1 recorded change, 0 differences.
- kiln's `workspace::slates::CONTRACT` is 40. kiln maps 37's `MountMissing::MountManager` (Windows) to its can't-mount
  refusal. Your next re-vendor starts from b2909fcf5.

## 2026-11-19 ~00:30Z, from g267: kiln's vendor/slates is re-vendored again, to Slates e2d1bc42b (contract 41), in A-503
- `scripts/vendor.py slates e2d1bc42b21ad0f10f81def33041744618189efd --from <clone> --no-gate`: 1107 files, change 0001
  re-applied cleanly, 123 files written or removed. Cargo.lock gained slates-base's aws-lc-rs, rustls, slates-rt and
  slates-transport edges. `workspace::slates::CONTRACT` is 41, and vendor/slates/UPSTREAM.md says 41.
- Contract 41's `PlannedAction::Chmod` is handled in kiln's review and summary. `vendor.py check` was not run (the
  rule now allows lib tests alone); it is filed for main.
