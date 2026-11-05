# Patches for Shards' owner

Shards is another project; slates never pushes to it. This folder is where its patches go, named here by the VFS plan's
step 14 because slates' docs named no folder in the tree (part 2's series lived in a scratch directory, GAPS). Apply with
`git am docs/wip/shards-patches/*.patch` on Shards' `gpu` at `e39a9218`.

## The WHP DAX window's three edge cases (2026-11-04)

`0001` adds six tests that run a real `Window` under real vCPUs on a real WHP partition
(`crates/vmm/src/devices/virtio/fs/window_whp_guest.rs`); two of them fail until `0002`, the fix. `0003` is the record, PM M421.

| edge case | what the window's design says | found on jupiter and windows-2025 |
|---|---|---|
| a view shorter than its range | a share's own file: a view as long as its bytes, zeros after. A server's section: reaches past every range, so a shorter one is refused | holds at 8 places a file can end. A section ending short is refused `EIO`, and the range takes a view after |
| 64 KiB granules | a view begins at a multiple of the allocation granularity; a split falls at any page | holds: views from all 32 of a range's granules, each way; 6 offsets between granules refused `EINVAL`, with what was mapped left in place |
| several vCPUs during a replace | a fault in a mapped range is a replace's and runs again once | a section's replace holds. **A followed file's did not: fixed by `0002`** |

The bug: `Window::reconcile` and `Window::evict` counted a change after letting the lock go. A vCPU whose fault waited for
that lock could read the old count, and a second fault at one address at one count is taken for the guest's own, so a live
range's load read a bus's `0xff` and its store was lost. It reaches a share's own file only: a slates section is never
followed.

Red, without `0002`, each of 3 runs: 2, 36 and 38 accesses fell through at 2 vCPUs in 6,400 followings, and 3, 9 and 12
changes of 20,000 were seen with the count not moved. Green, with it, 3 of 3 runs at 2, 4 and 8 vCPUs: none fell through,
7,347 to 13,640 faults ran again. The section storm was red-checked both ways, by never asking the window (40,657 accesses
fell) and by publishing the epoch before the replace (757,450,579 stale loads). All six tests passed 10 of 10 on jupiter
(AMD Ryzen AI Max+ 395, Windows 10.0.26200):

    cargo +1.98.1 test -p shards-vmm --lib --profile test-release -- --nocapture --test-threads=1 window::

On CI's windows-2025 (AMD EPYC 7763, 4 logical, Windows 10.0.26100; slates' `diag-shards-whp-4`, run 41981896876,
2026-11-05): without `0002`, 5 of 5 runs red, 407, 604, 308, 416 and 614 accesses fell through at 2 vCPUs. With it, all 16
window tests in each of 5 runs: none fell, none stale, every paced store found, 46,259 to 47,708 faults run again at 4 vCPUs.

## Seen beside them, not changed, for the owner to judge

- `crates/vmm/src/devices/virtio/fs/window.rs`, `Window::memory_fault`: the Unix window also counts a followed change after
  `drop(table)` (read 2026-11-04; no test run on it).
- `Window::map` on WHP forgets what was mapped before it places: a mapping refused `EIO` leaves the old one gone. A guest
  whose upgrade of a read-only range is refused keeps a page-table entry to a range with nothing there. Read, not measured.
