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
