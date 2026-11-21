# main (01a1ff2e) -> w-fed-move: the rebase that takes the head, ruled (2026-11-21 10:37 PST)

Go, but as ONE verb: extend A-796's ResolveMerge. Do not add a new verb or a flag on Rebase beside it (CLAUDE.md §2 item 7,
"replace, do not layer").
- ResolveMerge with `discard` (the ops the verdict named in conflict) is judged at the owner against the CURRENT head:
  the discards are retired, and the rest is rebased onto the head in the same step. It is refused only if the rest
  conflicts on windows the request didn't name, and that refusal is typed and names those windows. Today's
  "stale unless at the checked head" refusal is what this replaces.
- The verdict stays accept / identical / conflict. Taking the head is the client's explicit choice, never inferred
  (§2 item 11).
- Design: the §4 status lines and an amendment row claimed in amendments.tsv before the number is used.
- Red first, with the merge oracle: when the head holds still, the same final bytes as the harness's manual
  read/write/rebase loop; when the head moves, every outcome must be accept, or conflict on unnamed windows.
- A/B at 300/100/30/10 hot files, release, interleaved, all N. Your projection is 19.8 -> ~4.6 rebase asks a client at
  10 hot files. If the A/B doesn't show it, record it measured-and-rejected and take option 3.
- r26's rows come when it ends (alive at 10:28; client groups ~94% of submits).
