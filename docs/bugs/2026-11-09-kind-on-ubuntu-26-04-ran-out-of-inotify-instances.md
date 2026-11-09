# KIND on Ubuntu 26.04 ran out of inotify instances, so kube-proxy crash-looped and the fleet never formed

**Date:** 2026-11-09. **Lane:** the `kind:` job of `.github/workflows/ci.yml` (`cargo xtask kind all`;
`docs/wip/kind-lane.md`).

## Description

After `ubuntu-latest` moved to Ubuntu 26.04 (image `ubuntu26 20261101.186.1`, kernel 7.0.0-1019-azure, Docker
29.4.2), the KIND lane failed at formation: `peers_probed=0` on every pod, `configuration_version 0` on two of
three, and `slates council` refused `ConsensusRecoveryUnavailable`. The same lane on main, pinned to `ubuntu-24.04`
(image `ubuntu24 20261101.360.1`, Docker 28.0.4), passed (diag-release-10), so the cause was the image, not slates.
A plain kind cluster showed equal pod-to-pod TCP and DNS on both images (diag-release-11 and -12), which ruled out
kindnet itself.

## Root cause

diag-release-13 kept the lane's cluster and looked: no slates pod restarted, and no AppArmor denial touched the
cluster. But kube-proxy was `Error` with 5 restarts on 4 of the 5 workers. A pod on a node with no kube-proxy cannot
reach a Service's address, so slates-1 and slates-2 (on two of those workers) never heard from the cluster's DNS
(`no answer within the timeout after 2 attempt(s); last asked 10.96.0.10:53`). Neither could resolve its peers, so no
peer was ever probed.

diag-release-14 ran plain kind with the lane's `deploy/kind/cluster.yaml` (one control plane, five workers), no
slates, three legs:

| leg | `fs.inotify.max_user_instances` / `max_user_watches` | kube-proxy after 2 min |
|---|---|---|
| ubuntu-24.04 | 1280 / 655360 (the image's) | 6 of 6 running, 0 restarts |
| ubuntu-26.04 | 128 / 129238 (the image's; 128 is the kernel's default) | 4 of 6 `Error`, exit 1, 4 restarts each (4 of 5 workers) |
| ubuntu-26.04, raised | 512 / 524288 | 6 of 6 running, 0 restarts |

kind's nodes are privileged containers whose processes all run as the host's root, so every node's kubelet,
containerd, systemd and kube-proxy draw on root's one budget of inotify instances. 26.04's image no longer raises it
above the kernel's 128, and the last nodes up find it spent. kind's Known Issues page lists this failure ("Pod errors
due to too many open files") with the limits the raise leg used. kube-proxy's own last words were gone with its
container (`unable to retrieve container logs`), so the cause rests on the A/B: only the limit differed between the
two 26.04 legs.

## Impact

The KIND lane on main failed on every run since `ubuntu-latest` became 26.04. slates itself was unaffected. A
Linux host that keeps the kernel's 128 and runs `cargo xtask kind all` meets the same failure.

## Edits

- `.github/workflows/ci.yml`, `kind:` job: a step before the lane raises `fs.inotify.max_user_instances` to at
  least 512 and `fs.inotify.max_user_watches` to at least 524288 (kind's documented values; it never lowers a
  higher limit). The image stays `ubuntu-latest`.
- `docs/wip/kind-lane.md`, "The CI lane": the limits and why; also corrects that paragraph, which still named
  `helm/kind-action` after the job moved to a pinned, retried download (2026-11-08).
- `docs/wip/GAPS.md`: a top blockquote.
