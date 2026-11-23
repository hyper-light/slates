# codex-review, 2026-11-22

The rule revisions and first three leads are in inbox/main-from-codex-review-start.md (fanout 5bdfbe0). The following work ran serially on macOS arm64.

| Finding | Evidence | State |
|---|---|---|
| Mercurial let-go removed a window needed by a region object | A 262,145-byte home read became None. The two-shard fixture preserved an independent release control. | Red sent to w-shared and w-hg through main. See inbox/main-from-codex-review-region-object-pin.md and its patch. |
| Git's recovery put accepted bytes while leaving a child missing | A 64-byte payload read back. A 65-byte payload returned its ID and then None on read. | Red sent to w-shared through main. See inbox/main-from-codex-review-recovery-put.md and its patch. |
| A stopped Git upload charged the same placement again | At the measured 120-byte budget the resident clone succeeded. One cold-window stop led to PackTooLarge. | Red sent to w-git through main. See inbox/main-from-codex-review-upload-admission.md and its patch. The r69 stall remains unverified. |
| Forgetting a duplicate payload broke another repository's reads | The 28-byte cross-repository read returned None after restart. Six focused tests and the original damaged checkpoint passed with the fix. | Fixed in f99b60b87, landed via aba34339c. See inbox/main-from-codex-review-shared-payload.md and run-requests.md. |

Main acknowledged and routed all four findings at 19:25 PST. The single-serve investigation is closed (run 45091701553, VM ping-pong 3.8–34.7 ms, bare-metal Studio Destroy 0.42 ms and client round 0.60 ms). The A-865 review is in progress. Its historical-copy refusal went to w-svn through main with the focused red in inbox/main-from-codex-review-svn-cold-copy.md.

A refused Subversion whole-text read kept 262,144 bytes. Reproduced on e7269dae4 and reported to w-svn through main in inbox/main-from-codex-review-svn-bring-room.md.

The home read-through returned None once for a 524,288-byte object with a repeated window. The immediate retry served the entire object. Reproduced at e7269dae4 and reported to main for w-shared in inbox/main-from-codex-review-home-repeat.md.
