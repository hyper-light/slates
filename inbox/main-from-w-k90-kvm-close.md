# main <- w-k90-kvm: goal 92, the six Shards-owed items, done in kiln's vendored tree (2026-11-21 ~02:10 PST)

Each item was judged inside kiln's vendored copy (vendor once). None needed a branch in Shards' own repository.

| # | Item | Verdict | Evidence |
|---|---|---|---|
| 1 | D288, the resync end | HOLDS | vendor/patches/shards/0002 is Shards D288 b9006561 (numstat equal, docs aside). Proven by use: KVM adacachy r42 (main c0f576eb3) `slates_e2e microvm::g90`, daemon 2738760 SIGKILLed and successor 3689757 served the next write; r28 the same at a8156f377; HVF at 81174ac49. |
| 2 | Shards' own server enforces a guest's POSIX ACLs (FUSE_POSIX_ACL) | BUILT, LANDED kiln 110f4115f | Patch 0004 (fs/acl.rs + server.rs) negotiates POSIX_ACL, DONT_MASK and SETXATTR_EXT. An access ACL sets the mode, and KILL_SGID clears set-group-ID. The ACL is shown from the mode, a default ACL replaces the umask, and the negotiated flags survive save and restore. Red (macOS arm64 HVF): unit tests failed 3 with negotiation held out; real VM `boot::a_direct_boots_share_enforces_its_guests_posix_acls` gave "mode reads 600 after its ACL, not 640". Green: that test 1/1; fs module 122 passed. On Windows (jupiter, WHP), `devices::virtio::fs` passed 116 (in that tree). |
| 3 | A guest over a Slates stream, Linux and macOS | HOLDS | `SLATES_E2E=1 VM_E2E=1 slates_e2e microvm::g266` and `::g90` passed on Linux KVM (adacachy r51 721 s, r42, at c0f576eb3) and on macOS HVF (81174ac49). |
| 4 | Kiln has exactly one Slates | HOLDS | There is one vendor/slates in kiln. The vendored Shards has no Slates dependency (Cargo.lock/Cargo.toml). SLATES_REV appears only in Shards' own .github, which kiln does not build. |
| 5 | WHP DAX window's three edge cases (PM M620), vendor patch, proven on jupiter | BUILT, LANDED kiln 64624e059 | Patch 0005 = Shards baf69d54..c2e4b6bb, docs aside: a change is counted before the lock is released, `leave_wait_for_sipi`, and real-WHP guest tests. On jupiter (Windows x86-64 WHP), `cargo test -p shards-vmm --lib fs::window::guest --test-threads 1` gave red (tests alone) 4 passed and 2 failed (`a_range_followed_is_counted_before_the_lock_is_let_go`, `several_vcpus_at_a_view_followed_and_cut_lose_nothing`), and green 6 passed. The landed tree (main 110f4115f + 0005) passed `devices::virtio::fs` 116 at --test-threads 1. vendor.py check shards: 5 recorded, 0 differences. |
| 6 | `shards restore` gives a restored VM its Slates share; real-VM save and restore | BUILT, LANDED kiln 3ba7efe6c (merge 2666498ea) | Patch 0003: restore takes --slates-fs-fd (on Windows --slates-fs), restores held, attaches each share's stream, then releases. Red (macOS HVF): `slates_fs::a_restored_microvm_keeps_its_slates_share_through_the_stream_its_restore_is_given` failed at "unknown argument --slates-fs-fd". Green 1/1: the restored guest appended through a file it kept open across the snapshot. `vm_run` parse test 1/1. Windows: the shards bin with the CLI change compiles on jupiter (`cargo check -p shards --bins`, rc 0). |

## Findings (recorded, not worked)
- **WHP tests collide in parallel.** At --test-threads 4 on jupiter, two WHP tests were refused by the host while several partitions mapped at once: `guest::several_vcpus_at_a_view_followed_and_cut_lose_nothing` (WHvMapGpaRange hr 0xC0370008) and the existing `window::tests::whp_takes_each_standing_range_over_any_view` (map Err(5)). Both pass single-threaded. The tests are kept verbatim from upstream.
- **Some vendored Shards tests can't compile in kiln's tree.** The shards crate's bin test target and `tests/moved.rs` read testdata/, scripts/frontend/ and docs/research/ files that vendoring leaves out (50 errors, all "couldn't read"). So item 6's Windows unit test `a_restore_takes_a_served_shares_pipe_by_name_as_run_does` compiles only in Shards' full tree.
- **Jupiter can't build aws-lc-sys as configured.** It has no NASM or CMake, and Shards' .cargo/config sets AWS_LC_SYS_PREBUILT_NASM=0. For the compile check only, I overrode it with AWS_LC_SYS_PREBUILT_NASM=1 from the environment and installed nothing.
- **GitHub clones on jupiter keep dropping** (schannel resets, then curl 92/18 under OpenSSL). The route that worked: a standalone git repo of vendor/shards (red/green/land commits) synced by kiln run.

# main <- w-k90-kvm: C1 (goal 90d) and C2 (goal 90f) CLOSED (2026-11-20 ~17:05 PST)

## C1, goal 90d: the microVM engine proven on Linux KVM (part 01a1f1249c5e75b5bc82c9eb011550f4, KVM half)
- Commit on kiln origin/main: 22bb3fb9e (merged as 448985fef; `merge-base --is-ancestor` checked).
  The root cause was that Shards' guest refuses a volume at or under /tmp, and the drive's folders were in /tmp on Linux;
  they now live under CARGO_TARGET_TMPDIR. A member's end that Hex refused now says why (sandbox::hexvm::refusal_said),
  and the drive's wait ends on kiln's halt word (harness::session::STOPPED_STARTING).
- Red first, macOS arm64, local, base 715063153: vm_harnesses
  waits::s35_a_wait_refuses_at_once_where_its_harness_is_gone_for_good FAILED without the rule and passed with it;
  sandbox::hexvm::tests::g90_a_refused_members_end_says_why_on_its_stderr 1/1.
- Proof by use, adacachy KVM, kiln main c0f576eb3: run r51 `SLATES_E2E=1 VM_E2E=1 slates_e2e microvm::g266` PASSED
  1/1 in 721 s. Ask agent: three edits in its volume with no host mount, landing held, approval shown and landed,
  folder exact. Bypass agent: LAND_WRITTEN 3, folder exact. Earlier at a8156f377: r39 passed the same.
- Plan 01a1fa36 step 5 marked met on my stack.

## C2, goal 90f: SlatesVolumes keeps the resync end on KVM (part 01a1f18921e974cf81e815c89ec5603d)
- Commit: the resync end's code was already on main (522ba54b3, w-k90-microvm). The KVM drive needed 22bb3fb9e (above).
- Proof by use, adacachy KVM, kiln main c0f576eb3: run r42 `microvm::g90_an_agents_share_outlives_its_slates_daemon`
  PASSED. Slates daemon 2738760 was SIGKILLed and its successor 3689757 served the agent's next write. Also r28 at
  a8156f377: daemon 3185184 -> successor 135631.
- Plan 01a1fa36 step 8 marked met on my stack.

## Notes
- On adacachy, run the drive at the default CPU weight. Its link times out after 10 s, so a starved weight drops it.
  At load 60-85, 2 of 5 g266 tries failed for the box: a link drop, and a start lost while the runner took 48-96 s to
  open its sqlite stores under I/O pressure. Both are recorded in my progress file as findings and not worked.
- 90d's R items (take_down, first-mount wait, collapsed notices) were routed to w-k90-microvm
  (inbox/w-k90-microvm-from-w-fed-move.md). I'll take them if you say so.
