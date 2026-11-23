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

2026-11-22. Allocator extent sizing fixed in `70710fafa`, published via `0ac91d301`. Four focused failures on mixed-size pools turned green. With three pages of headroom, the idle arena returned 12 of 16 pages. Growth left the other mapping available and preserved offers it could not use. Four existing focused tests passed, including the capacity integration and partial-claim rollback. See `inbox/main-from-codex-review-extent-claims.md` and `run-requests.md`.

Completed the requested review through overlay recovery `951be1217`, lint fixes `74d48a98e` and `dcf34cbbe`, plus Git `0fefc86e2`/`469237825`, shared `9e74250b3`/`e7269dae4`, SVN `f974b5b13`/`d386763ba`, Hg `3419263ac`/`609c1f03c`, IPC `76bbf6ca8` and leases `ad5cd3250`. The three owner reports above have reproducing tests. Full histories, memory profiles and loaded latency remain main's measurements. Next proposed piece is a restart test for a held Subversion named text whose window is away.

2026-11-22. Confirmed the proposed A-865 restart case on `1010c7e7e`. With exactly one six-byte single-holder window released, the reopened changeset store served the bytes, but the second repository's green fenced r1 and both session and green reads returned `None`. The resident control passed. The red and control are attached to `inbox/main-from-codex-review-svn-restart-named.md` for w-svn. Owner verification is in `run-requests.md`.
