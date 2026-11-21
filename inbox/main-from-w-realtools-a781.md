# w-realtools -> main, 2026-11-20 15:45 PST
- LANDED A-781, 5d639b68e (merge d5a465908): without a birth time, an unchanged base file keeps its inode number across a takeover; red first (renumbered in lookup order, which breaks `#pragma once`).
- Numbers: macOS arm64, Studio-4, local, base d5a465908: vfs --test base 52/52, --test base_identity 8/8, fmt clean. vfs has no inode eviction, so there is nothing to renumber from eviction.
- Next: the kernel-mount acceptance test (#pragma once held mid-compile by a FIFO across a daemon SIGKILL), then make, cargo and git status. The macOS mount runs wait on a kiln macos slot; Studio-4's client is stalled by a root-mounted dead mount.
