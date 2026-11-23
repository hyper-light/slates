# codex-review start, 2026-11-22

I am the reviewer-fixer on `agents/codex-review`. My starting checkout is `a5aa2d57663894208ec6316b6cba93e68b03c4ae`. I read both root rule files in full, including CLAUDE.md §8. These are the Git blob revisions at that checkout.

| File | Revision | Sections read |
| --- | --- | --- |
| AGENTS.md | `04b53ed49733f00f141e1d9164d97ad54fceb8cb` | All |
| CLAUDE.md | `eb798d78e3118c4b07c9e0760da61cdeb146c021` | All |
| docs/wip/SLATES_DESIGN.md | `498989b4b7d46c633a361c3a42f13a0f2a4a370f` | §4.17.1 object ownership and fetches, §4.17.2 pack rows and fetches, §4.17.5 A-851, A-767 C5 read-through, A-863 and A-864 |
| docs/wip/GAPS.md | `1c485b1c22689cd44fe08009128f101991ba4c8f` | Lines 83–113 and 2342–2346, including the hold at `c9bb36dc5` |

The first three leads follow. The run measurements below are main’s.

1. Can an idle give-back, a restart, or destruction of another holder release a landed window that an active reader or the region still needs? Trace `646de255c`, `54369b008` and `6f6e87d82` through the hold at `c9bb36dc5`. Exercise the surviving reader after the handoff.
2. Which Git read paths can return a false missing object once a landed window goes? Start with tag peeling and negotiation, then check push validation against an older object. Main’s Studio r52 saw 1 of 8 stock verbs pass. Use an ordinary landed push and compare its replies with and without resident bytes.
3. Can a fetch turn repeat without advancing its cursor after a window stop? Check `f37113d45` and `06aa0e2ce` first among the A-864 changes. Studio r69 stopped at 183,511 bytes with zero parts copied. A failing small stream test goes to w-git through main.

I will send owner-area failures here with the focused test, source line and command. For goal 84a, Submit is first among the unowned verbs. I will notify main before taking it.
