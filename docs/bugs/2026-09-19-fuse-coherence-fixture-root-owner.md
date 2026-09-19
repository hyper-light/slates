# The mounted FUSE fixture belonged to root

## Failure

On 2026-09-19, Ubuntu job [105969405845](https://github.com/hyper-light/slates/actions/runs/35470114220/job/105969405845)
at `e882683` failed `coherence_mount` before the coherence scenario started:
the mounting user could not create `f` (`Permission denied`).

The saved pre-fix binary reproduces the identical failure locally in 0.01 s:

```sh
timeout 20s setpriv --reuid=nobody --regid=nogroup --clear-groups \
  target-linux/coherence-before --nocapture
```

Environment: disposable Linux 6.12.76-linuxkit arm64 container, `/dev/fuse`, container-only
`CAP_SYS_ADMIN`, fuse3 3.17.2. The test itself runs without root as uid 65534.
The earlier root-run proof did not test ordinary-user access and missed this defect.

## Cause and fix

`Volume::create` births a root-owned inode. Production provisioning stamps that
inode with the creating user's uid and primary gid (`server::verbs::stamp_root_owner`).
The mounted fixture omitted that step. FUSE's `default_permissions` correctly
refused a non-owner's write to the root-owned directory.

`tests/common::volume_for_owner` now stamps the fixture exactly as provisioning
does. `coherence_mount` supplies the process uid and gid. The sibling `oci_container`
fixture had the same omission and now uses the same shared provisioner. Its full
Docker bind scenario was not run by this repair. The mount's permission
checks remain enabled. The serving thread is scoped, so failed assertions unmount
and join it during unwinding as well. `mount_owner` reads the attributes through the bridge for
a deliberately non-root principal on every host, including root-run containers;
before the fix it reported `(0, 0)` instead of `(1234, 2345)`.

## Validation and local reproduction

The new ownership test passes. The rebuilt mounted regression, under the same
unprivileged identity and kernel as the red run, passes in 0.13 s. It exercises
file creation, warm attribute caching, a change through another attachment, and
retry after a refused invalidation gather.

```sh
cargo test -p slates-bridge-fuse --test mount_owner
cargo test -p slates-bridge-fuse --test coherence_mount --no-run
# Run the executable printed by --no-run as an ordinary user, with a bounded supervisor:
timeout 20s setpriv --reuid=nobody --regid=nogroup --clear-groups \
  target-linux/debug/deps/coherence_mount-<build-id> --nocapture
```

An unavailable helper/device is a printed skip, not mounted validation. Review the
output for a real run. The test's scratch is `/dev/shm`, removed after unmount.
