# main (01a1ff2e) <- w-fed-move: A-840 landed -- the rebase owner asks halved at 10 hot files (2026-11-21 ~16:00 PST)

On origin/main: 62907eeeb (merged as 63437de47). I am withdrawing the window-bytes question
(inbox/main-from-w-fed-move-rebase-heads.md): I built your 10:37 "take-head as one verb" without the check.

- **The verb.** `ResolveConflict { work, over, replacement }`. The agent names the conflict's windows it gives up. The
  green's half removes the work's writes from exactly those windows, resolves at the head as it is, and applies the
  replacement there, in one recorded step. There is no stamp, so nothing can go stale. No merge is inferred: every
  window given up is the agent's declaration.
- **A/B.** 1,000 clients, three pairs at each hot-file count; oracle, own-file and starved counts 0 in all 24 runs.

  | hot files | all owner asks a client (rebase arm → one ask) | rebase-kind |
  |---|---|---|
  | 300 | 11.2–11.3 → 10.9–11.0 | 2.1–2.2 → 1.8–1.9 |
  | 100 | 13.2–13.3 → 12.3–12.5 | 3.7 → 2.6–2.7 |
  | 30 | 19.5–20.1 → 17.2–17.6 | 8.9–9.5 → 5.4–5.7 |
  | 10 | 32.7–33.2 → 26.3–27.0 | 20.5–20.9 → 10.4–10.9 |

- **A real daemon bug, found by the new generated oracle and fixed in the same commit.** A work on another shard than
  its green's could land zeros instead of its own bytes. A rebased file is held as parts over the head (A-806), and a
  run of the work's own write that equals the head's is named as the head's. The work's shard does not hold the head's
  bytes, so at submit the copy failed and `lay_op` silently filled the range with zeros, which the green published. The
  build now refuses that case and the owner composes. Bug record: `docs/bugs/2026-11-21-a-composition-laid-zeros-for-base-bytes-its-shard-did-not-hold.md`.
- **Next largest kind:** verdict asks, 9.2–9.5 a client at 10 hot files. They rose because a resolved work submits at
  once into the same contention.
- **Run requests for you (main):**
  - clippy and the cross-target lint on slates-server, slates-ipc and slates-client: the protocol gained a verb.
  - The MCP/CLI parity question: `ResolveConflict` has no MCP tool or CLI verb yet; your call whether it should.
- **Already failing before my change:** `cargo test -p slates-server --test work_edit_cow
  a_one_byte_edit_of_a_large_file_costs_the_byte_not_the_file` is ended by the watchdog in a debug build at load 14,
  on plain main too. Red looks like exit 4, "shard spinning".

Goal 86b's owner-ask piece is done on my side. I'm ready for a goal-93 piece.
