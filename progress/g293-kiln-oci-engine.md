# g293-kiln-oci-engine progress (kiln UI goal 88's part 01a1f1249c5e75b5b569e52f814f3dc6: kiln's OCI/Docker engine runs each agent's container on its slates volume through the library)

POLICY_READ (g293, 2026-11-18, role: implementer, kiln + slates; slates-side A-number A-520): restart preamble
(rewritten ~22:10Z) + common-brief + focus-84-90; slates CLAUDE.md origin/main §8 (one piece, one commit, `--lib`
tests + `cargo fmt`, merge, end); kiln AGENTS.md (no tree-wide rustfmt, change only what the task asks). Shards paths
owned: none (vendor/shards untouched), so the Shards policy bootstrap does not apply to this piece.
Section split agreed with g294 (A-521, Kubernetes): kiln docs/agentic.md W9 is mine, W10 g294's.

## Piece 1: LANDED 2026-11-19 ~00:55Z
- **slates main bd1ff3ed5..b03311919** (A-520, change caa42bce9, `CONTRACT` = 44; main's A-535 had taken 43 meanwhile):
  `Instance::bind_into_container(volume, source, destination, intent)` and the `Volumes` seam's verb answer a
  `ContainerBind` (the binding's attachment + the runtime's `OciBinding` entry, re-exported with `HostMountEvidence`),
  refused typed as the daemon refuses §4.6 A-9's OCI form; the source spelled by its folder's real path (never looking
  into the mount); the simulator binds only the mounts it recorded, in the daemon's order of checks, and ends a bind
  with its mount. Files: crates/api/src/{container.rs (new), lib.rs, seam.rs, sim.rs}, crates/api/tests/api.rs,
  docs/library.md, SLATES_DESIGN (§4.6 status, §4.12 contract log 44, A-520 entry), GAPS (library entry).
- **kiln main c22e5b720..d6259c547** (W9 d642a7071): docs/agentic.md W9 (how a containerized agent gets its volume,
  Linux / macOS / Windows) + a sentence in the Status table's W row. Doc only.
- Unit tests (Mac Studio d1323898, `cargo test --locked -p slates-api --lib -- container:: sim::tests contract_history`):
  red r201 on scratch 69eda5f79 (verb stubbed as the library was): the 4 new tests failed, history passed; green r208 on
  scratch a728e2afa (the piece's code file for file): 5/5, whole `--lib` 76 passed 4 ignored; final r224 on caa42bce9
  itself: 5/5, whole `--lib` 78 passed 4 ignored. Logs on the Studio: ~/.cache/g293-a520-{red,final}.log. Studio
  clone + 5.4 GB target removed. Local worktrees removed (the superseded first copy committed on local branch
  agents/g293-a520, never pushed). Scratch remote branches left for main: agents/g293-a520-red (red evidence, never
  merge it), agents/g293-a520-piece (merged).
- Filed in run-requests.md (00:55Z): clippy + xtask for slates-api, the conformance suite + the SLATES_TEST_MOUNT
  kernel-mount bind test on macOS and Linux, Windows + Linux builds of the lib tests (the sim's paired cfg).

## Piece 2: LANDED 2026-11-19 ~01:35Z (restarted after the usage limit; scratch branch agents/g293-a520-red deleted as main asked)
- **slates main 01e4137be..c6bf19cbd** (A-520 second piece, change 9332e4b1a, `CONTRACT` = 47; main's A-524 took 46):
  `slates_api::container_engine::engine_profile(runtime)` (the handshake moved from crates/cli/src/oci_runtime.rs over an
  injected query: runtime by name, endpoint refused unless local, then `docker info`; `EngineProfile` or `EngineFault`,
  `Untested` carrying the endpoint + profile it refused) and `container::check_source` / `ContainerBind::check_source`
  (table first, then the mount id the kernel never reuses; `SourceFault` Missing/Replaced/Uncheckable/MountTable/Identity).
  The CLI's `slates oci-runtime` and `slates oci-check` call them and print what they did before; slates-bridge-oci is
  linked by slates-api on every platform (the CLI no longer links it: crates/cli/Cargo.toml + Cargo.lock).
- **kiln main 449a9288e..0ee121b1c**: agentic.md W9 names the two library calls (contract 47).
- Unit tests (Studio, `cargo test --locked -p slates-api --lib -- container_engine:: container::tests contract_history`):
  red r15 on a stubbed copy made on the Studio clone (never committed): the 5 new tests failed; green r15 10/10 and whole
  --lib 87 passed; final r23 on 9332e4b1a: 10/10, whole --lib 88 passed 4 ignored. Studio folder (4.7 GB) removed, logs
  ~/.cache/g293-a520-p2-*.log there. Scratch branch agents/g293-a520-p2-scratch deleted; merged branches
  agents/g293-a520-piece and agents/g293-a520-p2-piece remain. Local worktrees removed.
- Filed (run-requests.md 01:35Z): clippy/xtask; the CLI's bin tests and its oci tests by use (macOS, Linux); the first
  Windows build of slates-bridge-oci and the Windows lib tests; Linux lib tests; the Windows measurement for W9.

## Piece 3: LANDED 2026-11-19 ~01:50Z
- **slates main cf17cae5d..8d22d1887** (A-520 third piece, fast-forward, `CONTRACT` = 48): bridge-oci's judge speaks
  podman's own interface (`Interface`, `runtime_interface`, `PodmanFacts`, `podman_profile`) and holds a local rootless
  podman on Linux to T-4.13's rootless run (`a_rootless_container_reads_and_writes_the_volume_through_its_mount`,
  2026-10-19) with the new `IdentityRule::ContainerRootAsMountingUser` and `EveryNameServedAtOnce`; `Engine::Podman`,
  `Endpoint::Local` appended; podman as root `ProfileUntested`, remote `RemoteEngine`, labelling
  `SelinuxLabelsUntested`, every other rootless engine still `UserNamespaceUntested`; the Docker endpoint's admission
  still admits the Docker CLI alone. slates-api's handshake asks podman one `podman info` template (Go fields from
  libpod/define/info.go). CLI: the new rule's words.
- **kiln main 0ee121b1c..ef2812dce**: W9 runs agents under rootless podman as the container's root.
- Unit tests (Studio): red r47 on a stubbed copy (podman unspoken, every rootless refused): bridge-oci 3 new failed,
  api 2 new failed; green r47: bridge-oci --lib 8 passed, api focused 8/8, api --lib 90 passed 4 ignored. The landed
  code is byte-identical to r47's on the same base. Scratch branch deleted; Studio folder (3.5 GB) removed, logs
  ~/.cache/g293-a520-p3-*.log there; worktrees removed.
- Filed (run-requests.md 01:50Z): clippy/xtask; bridge-oci's runtime integration test; the CLI bin tests; THE LIVE
  PODMAN CHECK (the evidence test, `slates oci-runtime podman`, other ids refused, hard links through podman).

## Piece 4 (kiln): LANDED 2026-11-19 ~02:55Z
- **kiln main 5ee60ecac..d7f76c1dc** (change 8251b05ac): vendor/slates at de1031f1d (`vendor.py check slates`: 1149
  files, 1 recorded change, 0 differences; Cargo.lock: slates-api depends on slates-base); `workspace::slates::CONTRACT`
  48 with 43-48 mapped, no wildcard (`MountMissing::PrivateLoopback` -> kiln's can't-mount refusal in Slates' words;
  `MountFault::Busy` built with its holders in both tests); new `workspace::container` (W9's first runnable step):
  `start(made, plan)` = Slates' bind -> `engine_mount` (Slates' entry word for word as each engine's `--mount` says it;
  `nosuid`/`nodev` held by `--security-opt=no-new-privileges` and `--cap-drop=MKNOD`) -> `check_source` just before
  the engine binds -> the engine's `run --detach`; identity per profile (`--user 0:0` rootless podman; rootful Linux
  refused `NeedsSharedMount`); a refused start lets its bind go; `end(running, made)` = container rm -> bind detach ->
  W3's `remove` (no patience: main's W3 now asks the mount to go once). agentic.md W9 updated.
- Tests (Studio): red r129 on stubs (six w9 tests failed), green r129 8/8 and workspace:: 103 passed; final r305 on
  8251b05ac: all w9/w1/w2/w3 pass, workspace:: 100 passed + 2 load-flaky follow tests (passed alone right after;
  reported). Studio folder (19 GB) removed, logs ~/.cache/g293-kiln-p4-*.log there; kiln branches deleted; worktrees
  removed. One compile miss found by the final run and fixed before landing: main's W3 `remove` lost its patience
  argument (main's change), so the container's end calls `remove(made)`.

## What's left (each a piece, in order)
1. (done: piece 2, above.)
2. (done: piece 3, above. Next for it: turn the live podman check's results into a cli.rs test once main has run it.)
3. (done: piece 4.)
4. NEXT: kiln: a runner's `container` engine (wire `workspace::container` in) (harness::engine beside host/microvm: facts, placement, drain) and
   `sandbox::container` implementing the seam's `Boundary::Container` over the machine's engine (runner::docker::find):
   profile first, workspace mounted (shared only for a rootful Linux engine), bind via the library, `--mount`
   (never -v), container at the mount's own path, `--user` per the identity rule, end: container, bind, mount.
5. kiln, macOS: leave the NFS client's `.nfs.*` silly-rename names out of landings and change requests; git
   `core.createObject=rename` in the container; refuse a runner state outside Docker Desktop's shares.
6. Docker's missing `nosuid`/`nodev` words: measure (a setuid file and a device node made through one container's
   bind, used through another's), then hold by the container's own rules if the bind lets them through.
7. Windows: measure whether Docker Desktop's WSL 2 9P server reaches a WinFsp drive the daemon mounted in the
   person's logon session (jupiter, or a Windows runner with Docker Desktop); then Slates' OCI form on Windows by the
   daemon's own record of its WinFsp mount (A-520), else the network export route.
8. The goal's proof (main's): a test that starts an agent in a container through kiln on Linux, macOS and Windows.

## Reported, not changed
- §4.12's contract log has no lines for A-535 (43) or A-524 (46): the log goes 42, 44, 45, 47.
- Slates' CLI resolves `--oci-source` whole (`std::fs::canonicalize`, crates/cli/src/verbs.rs), which looks into the
  mount; the library now spells it by its folder (A-520).
- Main's A-535 raised slates-api CONTRACT to 43 without a line in §4.12's contract log (the log goes 42 then A-520's 44).
- slates-server warnings at main bf21a05b4: unused `slates_rt::TaskId` import (server/src/deadlines.rs:16), unused
  `CONTROL_BROKEN` (server/src/reap.rs:35).

## PARKED 2026-11-19: A-520 piece 5 (kiln: a runner's `container` engine), moved to goal 92 (A-591) by main
- State: NO CODE. Worktree ~/slates-work/g293-kiln-p5, branch agents/g293-oci-engine-p5 at kiln main d7f76c1dc with
  zero commits and a clean tree; nothing to push (removed, so no empty scratch branch exists). Resume from a fresh
  worktree at kiln's newest main.
- Scoping read (all at kiln d7f76c1dc):
  - `kiln_ci::facts::Engine{Host, Microvm}` (serde lowercase, TS "RunnerEngine", `as_str`), `Isolation{.., engine}`;
    `RunnerFacts` derives Default + `#[serde(default)]`, so a new `container: Option<ContainerFacts>` field is additive.
  - `harness/engine.rs`: `Asked{engine, version}`, `capable(facts)` (from `isolation.available`), `held(asked,&can)`,
    `overlay`, `asked_of` ("host"|"microvm"), `set` (409 when not capable, kv `runner_engine/<name>`), `split`.
  - `harness/lineage.rs` ~1205-1270: `isolation_on`, `wanted`, `engine_at`, `on_engine` (exhaustive Engine matches with
    per-arm messages), `isolation_for` ("none"|"microvm").
  - Other exhaustive matches on Engine (86 uses): harness/acp/catalog.rs `sessions_on_host`;
    agentdefs/experiments/mod.rs (~329, 345, 485-491, 857, 958); agentdefs/mod.rs; agentdefs/uses.rs;
    agentfiles/prebuild.rs:243.
  - Launch: harness/session.rs ~5248-5320 (`None if isolated => sandbox::isolated::launch`, else `kind.launch` then
    `proc::spawn_duplex_in_pool`); `Kind::launch` (kinds.rs) builds host-path args (program path, MCP config with a host
    socket, plugin dirs), so a container session needs its own launch: the image's program on PATH, no kiln MCP until a
    lane reaches containers (as microVMs), credentials by name (`--env K`), `<cli> exec -i -w <workdir> <container>`
    into a container `workspace::container::start` ran with an idle keeper; `workspace::release` -> `container::end`.
  - Probe: runner/probe.rs ~385 (`f.workspaces = Some(workspace::slates::facts().await)`); runner/docker.rs `find()`.
- Proposed split (not decided by main): piece 5a = facts (`RunnerFacts.container` with typed `ContainerMissing{Engine,
  SharedMount, DockerVersion, Profile}`), probe (find + `engine_profile` + Docker >= 25 as the interface floor of
  `bind-recursive=disabled`), held/capable/overlay/asked_of/set (409 typed), placement and drain, the exhaustive
  matches; piece 5b = the launch through `workspace::container::{start, end}` with end order container, bind, mount.

# GOAL 92 (51cef9f7): A-591, git's pack index v2 and a pack's header and trailer (relaunch 2026-11-19 ~03:30Z)

POLICY_READ (relaunch): restart preamble (THE RULE), slates CLAUDE.md origin/main §8, design §4.17.2 in full; role
implementer; Shards paths: none (bootstrap n/a). Readback of THE RULE sent to main first. A-520 piece 5 (kiln container
engine) re-checked: no kiln worktree or branch for it exists anywhere (~/slates-work, ~/Projects/kiln*), so it has no
work to save; it stays PARKED with NO CODE, as recorded above.

## LANDED 2026-11-19 04:08Z and 04:15Z
- **main 9bbedc2d8..ad2751556** (fast-forward):
  - da3eacd0f: code, tests and golden vectors;
  - 1d769d649: the lock and the docs;
  - c0944906e and 4c164752a: merges of A-582 part 3, A-602 and A-607; conflicts only at documents' ends, both sides
    kept, main's first;
  - ad2751556: pack points at g258's entry::PACK_HEADER_LEN (part 3 landed first, as main ruled); Idx::offset_of added
    for A-608's id-to-offset trait.
- **main 553d3d155..42e2f50ef**: test-only fix found by reading the tests against clippy.toml before ending. Three
  u64/usize-to-u32 casts in tests trip cast_possible_truncation, and the golden test had reached cognitive complexity 11
  (threshold 10).
- What is in it:
  - crates/git/src/pack.rs: `read_header` streams the header; `check` checks a whole pack, trailer first; `Writer`
    writes the header at version 2 and the trailer as bytes pass; `Checksummed`, pub(crate), is shared with idx.
  - crates/git/src/pack/idx.rs: `Idx::decode` (own checksum first; fanout, ids, size, large table, header offsets),
    `find`, `offset_of`, `entries`, `check_pack` (count, trailer, offsets, objects back to back, CRC32 per span,
    admission charged first), and `write`/`written_len` (git 2.50.1 byte for byte; writer refuses what the reader
    refuses), plus pub(crate) `write_above` for git's forced off32_limit golden vector.
  - Tests: tests/pack.rs (10) and tests/pack_idx.rs (22, two of them proptests: a model and an oracle pack).
  - Fixtures: ofs-large.idx (`git index-pack --index-version=2,0x200`), sha256.pack/.idx and the listings, with
    make-index-fixtures.sh.
  - Dependency: `crc32fast` 1.5.5, approved by main as flate2's own CRC crate. The lock has its entry alone; cargo's own
    rewrite also moved 5 packages from windows-sys 0.60.2 to 0.61.2, which was not carried.
  - Docs: §4.17.2 status; the A-591 amendment entry, with every git string and source checked against v2.50.1;
    the GAPS §4.17 line.
- Unit tests (Mac Studio M3 Ultra, debug; kiln returned no run ids, so times are given):
  - red 03:52Z: 0 of 32 passed with the eight entry points stubbed on the Studio's clone (never committed);
  - green 03:52Z (da3eacd0f): slates-git --lib 54 of 54;
  - 03:57Z (1d769d649, --locked): 54 of 54;
  - 04:04Z (ad2751556, --locked): 76 of 76;
  - 04:11Z (main 553d3d155 plus the fix, hashed equal to 42e2f50ef's file): 76 of 76;
  - fmt clean every time. Logs are on the Studio in ~/.cache/g293-a591-logs (red, green, final, merged, lints-fix,
    stub.py). The Studio clone and target are removed. Local worktrees g293-a591 and g293-a591-test-lints are removed;
    local scratch is removed. Branches agents/g293-a591-git-idx and agents/g293-a591-test-lints are merged and left.
- Filed: run-requests.md "2026-11-19 g293 main 42e2f50ef":
  - clippy and xtask;
  - cargo doc and xwin doc;
  - pack:: on Linux x86_64, Windows x86_64 and Linux aarch64, for the CRC's runtime paths.

## Left (each a piece; main assigns)
1. The pack reader (piece 3; g258's A-608): entries, inflation, deltas, the trailer checked as the stream arrives, and
   an index's ids checked against its pack's objects. pack::read_header, object_name::Hasher and Idx::offset_of are its
   inputs.
2. A sliced decode and check for a large pack (`Idx::decode` and `check_pack` run whole), with piece 15's import of a
   repository on disk, which also remakes version 1 indexes.
3. A vector that reaches a checksum's collision refusal: no published collision begins with `PACK` or `\377tOc`.

## Reported, not changed
- lib.rs's crate doc still says "Protocol v2's messages, deltas, pack indexes and packs are A-582's next parts". It
  cites `gitprotocol-common.txt`, `gitprotocol-v2.txt` and `gitprotocol-capabilities.txt`, which are `.adoc` at
  v2.50.1 (each .txt returns 404, checked 2026-11-19). slates-git's Cargo.toml description names only pkt-lines and
  side-band-64k.
- g258's tests/pack_fixtures.rs (private constants, SHA-1 listings) and A-591's test modules both read the
  verify-pack listings; one helper could serve both.

# GOAL 93 (51cef9f7): A-610, git piece 12's codecs: Git LFS on the server side (2026-11-19 ~04:05Z to 05:10Z)

POLICY_READ: unchanged from A-591 (THE RULE, CLAUDE.md §8); role implementer; Shards paths none. Main had written the
A-610 row and moved the registry's next free to A-612. Module choice (main left it mine): slates-git's `lfs`; it moves
to a crate both servers use when §4.17.3 takes Mercurial's `lfs`.

## LANDED 2026-11-19 ~05:08Z: main b24a8c826..00a568db8
- Branch agents/g293-a610-lfs, in order:
  - 5e91f9af6: the recorder, responder.py and make-lfs-exchange.sh;
  - e14d7ef74: code, tests and fixtures;
  - a7d48c7d0: compile fixes found by the Studio's first build;
  - 95457db5f and 77a36c9d7: two test expectations corrected to git-lfs's own behaviour (a comment-only block holds no
    example; a pointer without a marker word is no pointer);
  - fb499833c: docs with evidence, and the refusal table as an array;
  - d33525123: merge with main (four both-sides-added conflicts);
  - 00a568db8: merge with main's four svn-only commits.
- What is in it:
  - `lfs::json`: RFC 8259 validated in one pass as a loop, nesting's bit-stack charged to the admission; a cursor
    borrowing strings and numbers; integers refused by kind; a compact writer.
  - `lfs::batch`: requests and verify callbacks read; answers written with headers checked.
  - `lfs::locks`: create, verify and unlock bodies; the GET query as Go writes it; the answers.
  - `lfs::pointer`: read as git-lfs's DecodePointerFromBlob, canonical told apart; written as its Encoded.
  - In lfs.rs: Oid (lower case only), Timestamp (RFC 3339 to the second, years 0000 to 9999, Hinnant's calendar),
    ErrorBody, and the routes.
  - serde_json is a dev-dependency only, the oracle.
- Golden vectors:
  - git-lfs v3.8.1 (tag 36e29152, commit a662a6dbf) docs verbatim, with a fetch script that checks SHA-256s;
  - its pointer_test.go transcribed;
  - the stock git-lfs 3.8.1 client's 24 exchanges against responder.py on the Studio, 2026-11-19 04:27Z (Homebrew
    git-lfs installed there, approved). The file was brought back by pasting the run's output and checked against the
    Studio's SHA-256 (e06b5e56...), because the Studio has no GitHub credentials to push it.
- Unit tests (Studio, debug):
  - red 04:57Z: 0 of 50 passed with the stubs;
  - green 04:59Z on 77a36c9d7: 126 of 126;
  - green 05:02Z on fb499833c: 126 of 126;
  - landed tree d33525123 at 05:05Z: 269 of 269;
  - fmt clean every time.
  The first build failed to compile, four errors fixed in a7d48c7d0; the first green had two wrong test expectations,
  fixed. Logs are on the Studio in ~/.cache/g293-a610-logs; the clone and target are removed; the local worktree and
  scratch are removed.
- Filed: run-requests.md "2026-11-19 g293 main 00a568db8": clippy, xtask, doc and xwin doc; Linux and Windows lib
  runs; and, once routes exist, the stock client end to end against slates.

## Left of A-610 (each a piece; main assigns)
1. Serving LFS: the routes and media types in the smart HTTP server; authorization (read for download, write for
   upload and locks, force for another's lock); the object store (BLAKE3 content, SHA-256 index row, upload checked);
   lock records per repository with cursors in id order.
2. Expiries learned from the deployment's own token lifetimes (§4.17.1's rule).
3. The stock client end to end against slates' own server (main's run, Command 4 of the run request).
4. Mercurial's `lfs` (§4.17.3), moving the module to a shared crate.

## Reported, not changed
- slates-git's lib.rs crate doc and Cargo.toml description are older than most modules (main said it fixes the doc in
  its lint pass).

# GOAL 93 (51cef9f7): A-629 part 1, serving Git LFS, slates-git's half (2026-11-19 ~05:15Z to 06:10Z)

POLICY_READ: unchanged from A-591 (THE RULE, CLAUDE.md §8, ~/.local/state/slates-fanout/lint-habits.md read in full);
role implementer; Shards paths none. Main's lint pass d7ed2c71f was merged first. Main's rulings: an action's expiry is
the asking credential's own (`Context::credential_expiry`, A-625 supplies it), none for Basic, no learner; and A-642
(53d2ebd03): the locks live in the repository's lock log, guarded then applied, not as StateChange kinds on A-603's
entry (g261 rebuilds that); readback sent to main.

## LANDED 2026-11-19 06:05Z: main 88a4235af..c3de50ef5
- Branch agents/g293-a629-lfs-serve (worktree from ~/slates-work/g085-windows-unit-tests, now removed), in order:
  - 52825c396: code and tests;
  - cb761ddbf: the route table's `/a%zz` row left to slates-http, whose reader refuses it as a bad target first;
  - 9a6111cec and 5fdae64d4: docs (§4.17.2's "Large files" and status, GAPS, the amendment entry);
  - e7c78498e, f472f62b5, c3de50ef5: merges with main (two conflicts at the design's end, main's entries first).
- What is in it:
  - `lfs::objects`: LfsNaming (SHA-256 by aws-lc-rs), LfsObjects (an index of its own over slates-vcs's quarantine;
    a mismatch kept nowhere, the store unchanged; past the budget typed).
  - `lfs::serve`: `route` (batch, locks, the basic transfers at this server's own hrefs; slash/NUL escapes and
    non-UTF-8 paths 400, empty/./.. segments 404, 405 with Allow; Content-Type 415; Accept by RFC 9110's precedence
    and zero weights, empty elements skipped, 406), `base_url`, `Context`, `Need` and the `Access`, `Objects` and
    `Locks` seams, `Server::answer`. 401 with LFS-Authenticate when no credentials, else 403. Every answer charged
    before it is made and allocated at its measured length; a page borrows the log's records and is cut by the
    limit or where the room would not hold the next lock and the cursor after it; 413 rather than an empty page
    with a cursor; an unknown cursor or a query limit that is no number 422; a full log 507.
  - `lfs::locks`: `listed_len`, `cursor_len`.
- Tests: tests/lfs_serve.rs (17: the stock client's 24 recorded exchanges replayed through the server; route and
  media tables; policy, 401, expiry, integrity, unlock; pages by limit and room; 413/422/507; a proptest against a
  model of the paging rule) and tests/lfs_objects.rs (5).
- Unit tests (Mac Studio, debug): red 05:55Z, 22 of 22 new failing with 9 entry points stubbed on the clone (never
  committed), 271 others passing; green 05:58Z on cb761ddbf 293 of 293; landed tree c3de50ef5 06:04Z 360 passed, 2
  ignored (main's); fmt clean each time. The first "green" (05:55Z) reused the red build through a shared target
  directory: cargo hashes a workspace member by its path from the workspace root and judges it fresh by the dep-info's
  absolute paths, which named the red copy's unchanged files. Red and green now use target directories of their own,
  and the green log must say `Compiling slates-git`. Logs and stub.py are on the Studio in ~/.cache/g293-a629/logs;
  the clones and targets are removed (2.3 GB); local scratch removed.
- Filed: run-requests.md "2026-11-19 g293 main c3de50ef5": clippy and xtask, doc and xwin doc, Linux and Windows lib
  runs, the stock client end to end later; and my older lint debt (284 lines over 100 columns in A-610's files, 127
  in A-591's).
- amendments.tsv: A-629's row says part 1 landed (backup amendments.tsv.bak-g293-a629).

## Left of A-629 (each a piece; main assigns)
1. Part 2, the server's: the lock log in slates-db (guarded then applied, ids naming log places, A-642); `Access` over
   vcs_policy's Rights and the owner rule; each repository's LfsObjects beside the shared store; the call A-625's edge
   makes (head, body, Context with the credential's expiry).
2. A streaming put in slates-vcs, so an upload is admitted window by window rather than whole.
3. The stock client end to end against slates (main's run).
4. Mercurial's `lfs` (§4.17.3), moving the module to a crate both servers use.

## Reported, not changed
- My A-591 and A-610 files predate the lint habits: 411 lines over 100 columns (counted on main 2026-11-19).

# GOAL 93 (51cef9f7): A-629 part 2 (2026-11-19 ~06:12Z on)

POLICY_READ: unchanged (THE RULE; lint habits incl. the shared-target trap). Main's brief: the slates-db lock log, Access
over vcs_policy + owner rule, the call g299's edge makes (send its signature on landing); rewrap my over-width lines in
files I touch.

## 2a: NOT LANDED (superseded)
- Branch agents/g293-a629p2-lfs-locks at 6296d8b28: a separate slates-db lock log (LfsLocks, Op 98/99), green (db --lib
  143/143, red 7/7), but g261's A-635 (bf9c71d14) landed the same record as StateLog::Locks + StateChange::LockTaken/
  LockReleased while I worked. Two mechanisms would be layering; told main; not landed.
- Found: the MacBook's disk hit 163 MiB free at 06:41Z; a merge died mid-write (41 files + 290 new written, one
  truncated). Restored the worktree from blobs verified equal to origin/main's (no checkout/reset). Reported to main.
- Found: deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request fails on main
  ef3fd18a0 on the Studio, 3 of 3 alone and in whole runs (deadlines.rs:830). Reported.

## 2b-i: LANDED 07:05Z, main a0840090b..9294c4819
- 440f9d646 code+tests, docs commit, merges 19f235655, b9b677e00, 9294c4819. slates-git only.
- Hrefs carry the batch's ref (?refspec=), transfers judged on it; Locks reads owned records; LocksRefused {Full 507,
  Failed 500}; pages charge each record's text.
- Studio: red 2 failing (transfer_allowed stubbed); green 387; final on b9b677e00 443 passed 3 ignored; fmt clean.

## 2b-ii: next (slates-server)
- crates/server/src/lfs.rs: Access over vcs_policy::admit (Need -> Right; no ref -> the default branch; UnlockOthers ->
  Rewrite; no principal -> refused); Locks over Repositories' locks log through RepositoryLogs::append (owner-only
  until push's policy check lands: told main); lock bytes = Wire {position, owner Principal, locked_at}; id =
  "{position}.{hex(path)}"; slates-db Repositories::lock_on/locks_from accessors; the entry answer(state, objects,
  call). The edge's Answer::Serve fixes 200 at the head: g299 needs a decision that renders the status after the body.

## 2b-ii: LANDED 07:17Z, main 145a12542..9ef49984b
- 24c2fc933 code+tests, 9d7108e76 compile fixes (Clock trait in scope; hooks_env from Default), docs, merges.
- crates/server/src/lfs.rs: answer(state, objects, &LfsCall) -> Answer; LfsAccess (vcs_policy::admit per need/ref;
  default branch for no ref; UnlockOthers = Rewrite; no principal refused); LogLocks (A-635 locks log through
  RepositoryLogs::append; HeldLock bytes; id "{position}.{hex(path)}"); crates/db Repositories::lock_on/locks_from;
  slates-git re-exports Admitted, ContentStore.
- Studio: red 4/4 (answer, allows stubbed); final on 9ef49984b: server 635 passed + main's deadlines failure, git 443,
  db 142; fmt clean. Studio clones/targets removed (logs kept in ~/.cache/g293-a629p2{,b}/logs); local worktree and
  scratch removed; branches agents/g293-a629p2-lfs-locks (2a, unlanded) and -p2b-lfs-serve left.
- Owed (told main): the edge's decision whose status follows the body (g299); the durable object store on the owner
  shard (A-581); locks by non-owners once push's policy check lands (A-635's owner rule); principals' names (§4.13).

# GOAL 93: A-665 assigned 07:2xZ, NOT STARTED -- context spent; main relaunches g293 fresh
- Main's brief (verbatim essentials): git piece 10 part 1, the read-only bare-repository view a git hook runs in, as a
  pure function in slates-git: given A-614's store, a version and a push's quarantine, answer a tree of paths to
  contents a volume serves lazily -- objects/pack/*.pack and .idx v2 from the store's fragments and base pack;
  packed-refs from the greens' heads; HEAD as a symref; config with the object format; the quarantine as git's
  tmp_objdir presents it (GIT_QUARANTINE_PATH, GIT_OBJECT_DIRECTORY, GIT_ALTERNATE_OBJECT_DIRECTORIES). Nothing
  materialized until read (A-645's scale: a hook reads a little of a huge repository). Unit tests against fixtures
  recorded from stock git (git init --bare + index-pack layout). File main's integration run (stock git over a mounted
  view). Part 2 (presenting it to hook_driver on the owner shard) after g258's A-662. Also owed after A-662:
  lfs::serve::Objects over the repository's store (durable LFS objects).
- Registered A-665 in amendments.tsv (backup .bak-g293-a665); next free A-666.
- Starting points for the fresh run: A-591's pack/idx writer (crates/git/src/pack/idx.rs: `write`, `written_len`;
  pack header/trailer), A-614's store (crates/git/src/store/: fragments, base pack), A-638's quarantine path (receive),
  A-622 part 3's hook driver (crates/server/src/hook_driver.rs) for what a hook's environment needs. Record fixtures on
  the Studio with stock git 2.50.1 (as A-591's make-index-fixtures.sh did).

# GOAL 93: A-665 (relaunched fresh 2026-11-18 ~23:20 PST / 11-19 ~07:20Z)
POLICY_READ (g293 fresh, role: implementer, slates-git; A-665): restart preamble (2026-11-18 rewrite) in full; slates
CLAUDE.md origin/main a1932c3ee §8 (one piece; `cargo test -p <crate> --lib` + `cargo fmt` only; merge = fetch, merge
origin/main, read diff --stat, conflict-marker grep, `git push origin HEAD:main` alone; END = progress, run-requests,
report). Read §4.17.2 "The repository in git's terms", "Hooks, in git's terms", Pieces (git) 10; A-591, A-614 (1, 2),
A-622 part 3, A-638, A-645, A-648. Readback sent to main.
- Main's Studio rule (11-19): CARGO_BUILD_JOBS=4 on every cargo; never two cargo runs of mine at once; delete each
  red/green target as soon as its result is read; a run that "did not answer"/killed: wait a minute, rerun once,
  counted neither red nor green.
- Main (11-19): Studio OFFLINE after OOM. No builds anywhere else; write tests + code + docs, commit to branch; probe
  d1323898 with one `kiln run` (sync false, `true`) every ~5 min; when nothing more can be done, progress + one line.
- Worktree ~/slates-work/g293-a665 (branch agents/g293-a665-hook-view from origin/main a1932c3ee, made from
  ~/slates-work/g085-windows-unit-tests); scratch ~/slates-work/g293-scratch (design.md copy).
- 11-19 ~08:10Z: Studio still offline (probed ~every 5 min). Written, UNBUILT, committed on the branch (not pushed):
  bb4d16e05 fixtures (crates/git/fixtures/view + make-view-fixtures.sh, recorded provisionally on this MacBook with
  the same stock git 2.50.1 Apple Git-155; every inherited GIT_* cleared first -- a first run had captured the
  harness's GIT_CONFIG_KEY_* (private hosts/credential helpers): deleted, never committed); 3c4a0ae81 code+tests:
  crates/git/src/store/hook_view.rs + hook_view/{units,seal,stored,text,environment}.rs; one-line additions in
  store/{mod,positions,repository}.rs and pack/idx.rs (len_of, is_large); tests src/tests/store/hook_view.rs and
  hook_view_stored.rs (+ `mod hook_view;` in tests/store/mod.rs).
- Findings to report: stock receive-pack 2.50.1 sets GIT_PUSH_OPTION_COUNT=0 on a push that negotiated no push
  options (cmd_receive_pack always passes &push_options, receive-pack.c 2592-2620; fixtures hook.loose/pack.*);
  crates/server/src/vcs_hooks.rs push_option_env(None) unsets it (follows githooks.adoc, which says "not set").
- Next: docs (§4.17.2 status, Hooks paragraph, piece 10, A-665 entry, GAPS); when the Studio answers: push branch,
  clone there, red (stub patch via heredoc), green, fmt; re-run make-view-fixtures.sh there and cmp.
- 11-19 ~08:45Z: branch agents/g293-a665-hook-view PUSHED at 3c823ddad (fixtures bb4d16e05, code 3c4a0ae81, docs
  9c0085e6f, test fixes ddfd5ce41/e4fa0b877, ignored main-run test 3c823ddad). Docs written (§4.17.2 status, Hooks
  paragraph, piece 10, A-665 entry at the end of SLATES_DESIGN.md, GAPS §4.17 line after A-639) -- the entry's run
  results still to fill. Red patch: ~/slates-work/g293-scratch/red.patch (stubs HookView::open -> empty view,
  Unit::fill, stored::fill, Adler32::update). UNBUILT: Studio offline every probe since 07:25Z.
- Studio plan: clone branch to ~/.cache/g293-a665/{red,green}, own CARGO_TARGET_DIR each, CARGO_BUILD_JOBS=4;
  red = git apply red.patch, `cargo test --locked -p slates-git --lib -- hook_view` (all new fail); green = same
  filter then whole `--lib`; `cargo fmt -p slates-git` diff back; re-run make-view-fixtures.sh into the clone and
  `git status` (byte-identical check); delete targets after reading each log.
- 11-19 ~08:50Z, MAIN: stop Studio probes; arm no watcher; end the turn. Main messages me when the Studio answers;
  that message is the signal to run (plan above). Background timer buhcb6qi8 stopped.
- CLAIMED for me (bug-claims.md), AFTER A-665 part 1 lands, failing test first: vcs_hooks::push_option_env(None) must
  set GIT_PUSH_OPTION_COUNT=0 as stock receive-pack 2.50.1 does, compared against fixtures/view/hook.*.txt; sweep the
  other hook variables for the same unset-versus-empty difference.

## A-665 part 1: LANDED 2026-11-19 ~11:40Z, main 2ca3f94fb..91584c74e, then 91584c74e..556a4e050 (a doc link)
- Built on jupiter (Windows x86_64, main's order; Studio offline): red 13 of 15 at a719726 (stubs: HookView::open
  empty, Unit::fill, stored::fill, Adler32::update); the 2 that passed were vacuous -> made to read (a whole stream
  its full length; the view its five packs and every file) -> red 2 of 2 at ce13693; green at ce13693: 15 of 15 +
  1 ignored, slates-git --lib 462 passed 4 ignored (= main's baseline 447/3 at 018d32ba2 + 15/1). rustfmt layout
  from that run applied. Clones/targets on jupiter removed (logs ~/.cache/g293-a665-green*.log there).
- Merges: two conflicts in SLATES_DESIGN.md (both sides appending entries at the file's end), resolved keeping both,
  main's first. NOTE: BSD grep misses `=======` with BRE `\|`; use `grep -E '^(<<<<<<< |=======$|>>>>>>> )'`.
- Filed (run-requests.md, "2026-11-19 g293 main 556a4e050"): clippy, xtask + doc gate, the macOS unit run and the
  fixtures re-made on the Studio, stock git over the view via the ignored `a_view_for_stock_git`.
- Found, not mine: slates-merge `source_of` never used (crates/merge/src/engine/copy.rs:535) in the jupiter build.
- Owed (A-665): part 2, the view presented to hook_driver on the owner shard after g258's A-662 (main says when);
  seals as derived data; a seal at encoding; packed-refs by ranges; the cover as derived data + merged fragments.
- Branch agents/g293-a665-hook-view left on GitHub (merged). Worktree ~/slates-work/g293-a665 to remove.

## NEXT (main, 11-19): GIT_PUSH_OPTION_COUNT fix (bug-claims.md), failing test first: vcs_hooks::push_option_env(None)
must set GIT_PUSH_OPTION_COUNT=0 as stock receive-pack 2.50.1 does (fixtures/view/hook.loose.*); sweep the other
hook variables for the same unset-versus-empty difference. Build on jupiter (clones with -c http.version=HTTP/1.1
-c http.sslBackend=openssl -c core.autocrlf=false).
- GIT_PUSH_OPTION_COUNT fix, branch agents/g293-push-option-count (worktree ~/slates-work/g293-push-option-count):
  red commit 967e9e4bd (tests), fix 90fdf1708 (vcs_hooks push_option_env sets the count, 0 with none; EnvValue::Unset
  + hook_driver's env_remove removed; bug note docs/bugs/2026-11-19-a-git-hook-without-push-options-was-handed-no-count.md;
  A-597/A-665/GAPS corrected; sweep in GAPS). jupiter red (12:50Z): vcs_hooks 31 passed, the 2 new git tests FAILED.
  Green building on jupiter (slow: shared box, cold slates-server). The kiln call aborted at 1800 s of silence --
  keep wait_s <= 1500 and read logs at ~/.cache/g293-poc-*.log there. fmt not yet taken (re-run cargo fmt after).
- Sweep findings for main (Mercurial 7.2, owed): _exthook always sets HGPLAIN (1 by default; configitems.toml 1563)
  and shellenviron sets HG; HG_TXNID/HG_TXNNAME missing from slates' changegroup env.

## GIT_PUSH_OPTION_COUNT fix: LANDED 2026-11-19 ~13:35Z, main f6ec36df2..d54bbfe56
- vcs_hooks::push_option_env sets GIT_PUSH_OPTION_COUNT always (0 with none negotiated), as stock receive-pack 2.50.1;
  EnvValue::Unset + hook_driver's env_remove removed; bug note docs/bugs/2026-11-19-a-git-hook-without-push-options-
  was-handed-no-count.md; A-597 and A-665 entries corrected; GAPS line with the sweep.
- jupiter (Windows x86_64): red (code as at 967e9e4bd) 31 passed + the 2 new git tests FAILED; green at 90fdf1708
  vcs_hooks 33 of 33; rustfmt layout from a fresh jupiter clone applied (285c815a2). bug-claims.md line marked
  [LANDED g293 d54bbfe56] (backup .bak-g293-poc).
- Filed (run-requests "2026-11-19 g293 main d54bbfe56"): hook_driver's Unix-only test on macOS/Linux, clippy, xtask.
- Owed (reported, not claimed): Mercurial 7.2 hooks' HGPLAIN/HG always set by stock, unset under slates;
  HG_TXNID/HG_TXNNAME missing from slates' changegroup hooks.
- Still owed from A-665: part 2 after g258's A-662 (main says when); the macOS run and Studio fixture re-record (filed).
- Worktree ~/slates-work/g293-push-option-count to remove; branches agents/g293-a665-hook-view and
  agents/g293-push-option-count on GitHub (both merged).

# GOAL 93: A-665 part 2 (relaunched fresh 2026-11-19 ~19:20Z; brief: hand the view to the hook driver, GIT_DIR=. wired)
POLICY_READ (g293 fresh, role implementer, slates-server + slates-git): relaunch-2026-11-19.md, restart preamble,
slates CLAUDE.md origin/main §8 (focused unit + integration tests + fmt on the Studio, one commit + the merge, END),
lint-habits.md. Shards paths: none (bootstrap n/a).
- Scope decided (no mount exists for a computed tree): part 2 = `crates/server/src/git_hooks.rs` (+ tests): GitHookView
  open over vcs_stores (refs as ReadRef{name, green, version} -> store versions; NotInStore), tables charged to the
  shard budget, seal_turn sliced (step quantum; seal lent HookView::seal_len), node/list/read for a presenter,
  presented_at(root) -> GIT_DIR=. + quarantine vars, close gives all back; run(site, input) = configured hook + view env
  via hook_driver, ViewForAnotherPoint/NotGits refused. slates-git: HookView::seal_len + test. vcs_store: SealedPush::push,
  VcsStores::content_mut, within_budget pub(crate), tests' push helpers pub(crate). Part 3 (presenting at a path) owed.
- Worktrees: ~/slates-work/g293-a665p2 (branch agents/g293-a665p2-hook-view, wip commits, pushed d968db87b);
  ~/slates-work/g293-a665p2-land (agents/g293-a665p2-land = squash onto 22855c29e + merge of origin/main, scratch,
  pushed 77b880972). Final: fresh worktree from origin/main, squash land, fill @@RUNS placeholders (design status,
  A-665 part 2 entry "Runs.", GAPS), real message, push HEAD:main.
- Studio runs (macOS arm64, Mac Studio d1323898, CARGO_TARGET_DIR ~/.cache/g293-target, clone ~/.cache/g293-a665p2):
  r186 (a348e2e4a): slates-git hook_view 16 passed 1 ignored; server compile errors (mine: within() answer shape,
  a local shadowing `repository`). r200 (4067ea279): server git_hooks + vcs_store:: 17 of 17. r204: --nocapture main
  test 1 passed, no SKIP (stock git 2.50.1 Apple Git-155 ran); RED with 4 stubs (env filtered, rposition any, sees
  always, seal_len without sums): git 1 failed (seal_len test) 15 passed; server 3 of 3 new failed; restored clean;
  fmt diff applied locally (sha256 b54b61b5...). r12 (kiln restarted; land tree 77b880972): running.
- Findings for main: a git hook inherits the daemon's GIT_* repository variables (git's local_repo_env; connect.c clears
  them for receive-pack, from memory) - update/post-receive set none, so they would point git away from the view.

# GOAL 93: A-665 part 2, relaunched fresh again 2026-11-19 ~21:20Z (predecessor stopped mid-merge ~21:14Z)
POLICY_READ (g293 fresh, role implementer; slates-server + slates-git): relaunch-2026-11-19.md, restart preamble,
CLAUDE.md origin/main §8 (focused unit + integration tests + fmt on the Studio; merge; END), lint-habits.md.
- Found: landing commit 763acdc61 (real message) + merge 8af067214 (origin/main d015143ac) on local branch
  agents/g293-a665p2-commit, NOT pushed; main had moved to 3612d1ef9 (A-626 part 4, touches slates-server listing/
  verbs/work_home/work_log), so r39's green no longer stands. PUSHED agents/g293-a665p2-commit at 8af067214 (21:25Z).
- Worktrees: g293-a665p2 (husk: vendor/ only), g293-a665p2-merge2 (interrupted `worktree add`, no admin dir, untouched
  checkout: nothing to keep), -final/-land (scratch, pushed). Integration tests running a git hook: none exist
  (crates/*/tests); focused = git --lib hook_view; server --lib git_hooks vcs_hooks hook_driver vcs_store::.
- Plan: merge origin/main in g293-a665p2-commit (main's design-log entries first, mine last), push branch, Studio run
  on the merged tree, fmt, push HEAD:main.
- 21:30-22:00Z: main moved 3612d1ef9 -> b5231f0ff -> 0577ab0bd (A-654 part 2: git_push driver; added
  SealedPush::quarantine == my SealedPush::push -> kept main's, git_hooks now calls quarantine(); module docs, lib.rs,
  design, GAPS name `git_push` as the owed caller) -> 34d10acb5 (A-636 part 2a, slates-db ids/logs).
- Studio runs (macOS arm64, Mac Studio, git 2.50.1 Apple Git-155, rustc 1.98.1; target ~/.cache/g293-target):
  r82 on a637b8db4 (merge 3612d1ef9): git hook_view 16+1 ignored; server git_hooks 4, vcs_hooks 34, hook_driver 11,
  vcs_store 18 = 67/67; no SKIP; fmt clean. r111 on 068d00c19 (merge 0577ab0bd): same + git_push 3 = 70/70; 16+1; fmt.
  r161 on 564c4447f (merge 34d10acb5, branch agents/g293-a665p2-land4): running.
- Real commits so far (unpushed to main): 7b797f174 (merge 0577ab0bd over 8af067214, real message citing r82/r111;
  docs cite r111). Final = worktree g293-a665p2-land5 from 7b797f174, merge origin/main, restore design log from
  564c4447f, verify tree == tested, real message citing r161, push HEAD:main.

# GOAL 93: A-665 part 2, relaunched fresh 2026-11-19 ~22:10Z (predecessor interrupted after 42ff0c9ff; brief: "serve
# the hooks' read-only view, so a push's hooks run rather than answering 'hook not run'; finish it as the progress file
# describes")
POLICY_READ (g293 fresh, role implementer; slates-server + slates-git): relaunch-2026-11-19.md (incl. Ada ~22:45Z
"completeness first"; findings by reading -> main in one line), restart preamble, CLAUDE.md §8, lint-habits.md.
Shards paths none.
- Found: land5 at 42ff0c9ff (pushed) = landing commit 763acdc61 + merges through main 3b0b95379; Studio run m7
  (~/.cache/g293-a665p2-m7-logs there, 22:13Z) on it: server 70/70, git 16+1 ignored, fmt clean. Main had moved to
  b94ae6e90 (A-678, A-653 cont. (4): vcs_store.rs + slates-git store) one second before 42ff0c9ff.
- cbfc774ad merge b94ae6e90 (design log conflict, main first) -> kiln r246 (macOS arm64, Mac Studio, 22:21-22:28Z):
  server `git_hooks vcs_hooks hook_driver vcs_store:: git_push::` 70/70, no slates warning; skip-check 1/1 no SKIP;
  git `hook_view` 16 passed 1 ignored; fmt clean.
- 6cec55f17 merge 17f4f7d6f (A-651 part 3: ssh/credentials/vcs_ssh only; run stands, message says so) -- NOT pushed to
  main: main moved again (e25799e8c, A-655 part 2b: vcs_store.rs +255, conflict at within_budget's doc).
- 5649db814 merge e25799e8c (vcs_store.rs conflict: main's svn fns kept, then my doc + pub(crate)); pushed to
  agents/g293-a665p2-land5; kiln r281 running on it.
- r281 (macOS arm64, Mac Studio, 22:30-22:41Z) on 5649db814: server 70/70, no slates warning; skip-check 1/1 no SKIP;
  git hook_view 16 passed 1 ignored; fmt clean. Main had moved to 770ae8826 (A-680 part 1: hg crate, vcs_http/hg.rs;
  126eb8a83 cli tests): nothing the hooks call, so 99068c566 merged it with "the run on 5649db814 stands".

## A-665 part 2: LANDED 2026-11-19 ~22:45Z, main 770ae8826..99068c566
- 763acdc61 (code, tests, docs) + merges 8af067214, 7b797f174, 509aab4e2, 42ff0c9ff, cbfc774ad, 6cec55f17,
  5649db814, 99068c566; every message real. Diff vs main before the push: the piece's 9 files alone (1562+, 29-).
- amendments.tsv A-665 row updated (backup .bak-g293-a665p2); run-requests.md "2026-11-19 g293 main 99068c566"
  (backup .bak-g293-a665p2): clippy, xtask, doc gates, Linux x86_64/aarch64 focused runs, Windows hook_view + vcs_store::.
- Cleanup: Studio clones m8/m9 and ~/.cache/g293-target removed (logs kept: ~/.cache/g293-a665p2-m{3..9}-logs);
  local worktrees g293-a665p2{,-commit,-final,-land,-land3,-land4,-land5,-merge2} removed (all clean and pushed);
  branches agents/g293-a665p2-* left on GitHub (scratch; main may delete). ~/slates-work/g293-scratch kept (messages).

## Left for "a push's hooks run rather than answering `hook not run`" (each a piece; main assigns)
1. Part 3: present the view at a path a hook process opens (a read-only attachment the mount edges serve from
   GitHookView::{node, list, read}; on Linux inside the launcher's namespace, §4.6). Without it `run` has no root.
2. git_push's calls (A-654): replace `Declining::hook` (HOOK_NOT_RUN) with open -> seal_turn (sliced) -> present ->
   git_hooks::run -> close at pre-receive (quarantined), update (per ref, migrated) and post-receive (no push);
   git's words for each outcome (`pre-receive hook declined`, `hook declined`).
3. Seals kept as derived data across views (a pack's name hashes what it is made from).
4. Reported earlier (predecessor): a hook inherits the daemon's GIT_* repository variables (git's local_repo_env).

# GOAL 93: A-665 part 3 (main 2026-11-19 ~22:50Z): a push runs its hooks instead of answering "hook not run"
POLICY_READ: unchanged (relaunch incl. "completeness first", restart preamble, CLAUDE.md origin/main §8, lint habits).
- Worktree ~/slates-work/g293-a665p3, branch agents/g293-a665p3 from origin/main 550153532.
- Finding (by reading; scope): the mount edges serve only volumes through bridge-core's one VolumeBridge ("a second,
  parallel operation layer is banned"), so a view shown at a path needs a volume whose slot host (vfs HostFs, as S3
  bases do, A-555) reads the repository's content store, which lives in ShardState beside the volumes; plus a mount
  path (the root mount, D-1; SLATES_ROOT, "the daemon publishing its root mount" owed in GAPS). svn and hg hooks have
  no production view either (svn_hooks/hg_push run_hooks called from tests alone).
- Split decided: 3a (this piece) = git_push runs pre-receive / update / post-receive through a presenter seam
  (tests: scratch-directory presenter; production: none yet -> "hook not run" kept, typed), git's words
  (`pre-receive hook declined`; update via slates-git protected::hook_declined; accepted hooks' and post-receive's
  words on band 2; the report's final flush after post-receive), the edge's step loop (Sealing -> yield, Hook -> run).
  3b (next) = the production presenter: a read-only volume over the view (HostFs over GitHookView) under the root
  mount, plus the HTTP edge's client-gone future and the hooks' learned-time table.
- slates-git additions planned: protected::{PRE_RECEIVE_HOOK_DECLINED, pre_receive_declined}; report::{write_hook_words,
  write_report_held, write_report_end} (write_report = held + end); Session::decide (answer = decide + write).
- 23:00-23:40Z: code written (slates-git report split + Session::decide + protected words + 2 tests; server git_push
  stepwise answer_step/hook_ran, Answering, Unpresented, HookVerdicts, git_push/hooks.rs (unix), git_hooks PresentView,
  HTTP edge step loop; docs: design status + Hooks + piece 10 + A-665 part 3 entry, GAPS; @@RUNS placeholders to fill).
  Scratch branch agents/g293-a665p3-scratch (wip commits 841d8f7b4, 0456291ce; land via fresh worktree + cherry-pick -n).
- r388 (Studio, 841d8f7b4): slates-git receive::/protected_fixtures 49/49; server compiled clean but the watchdog ended
  the test process (exit 4): the first hook tests ran whole pushes inside audit_on_shard, blocking the shard >1 s.
  Fixed: tests drive the push as a detached task on audit_daemon()'s shard (with_state per step, hooks awaited), as the
  HTTP edge does. r424 running (green + fmt diff). Then: red (sed `if hooked {` -> `if hooked && false {`), land.
- r424 green 52/52 (fmt diff applied locally); r430 on e610a6ec1: red 0/3 (hook path stubbed `if hooked && false`),
  green server 52/52 + git 49/49, fmt clean, no slates warnings.
- Landing: worktree ~/slates-work/g293-a665p3-land, branch agents/g293-a665p3-land: 86a4a0fb9 (the one commit,
  real message, docs cite r430) + e331fc829 (merge origin/main 2c3fa2bcc; design log conflict, main first). The
  MacBook's disk hit 1.0 GiB free mid-merge (ort failed, 31 untracked leftovers of main's files): removed the pushed
  scratch worktree g293-a665p3 (157 MB) and the leftovers (each checked present on origin/main; zsh's `path` variable
  clobbers PATH in loops -- use another name), merged again. Pushed; r443 running on e331fc829. Then push HEAD:main.

## A-665 part 3 (3a): LANDED 2026-11-20 ~00:02Z, main 8c7965bf4..c8be7c738
- 86a4a0fb9 (the piece; docs cite r430) + merges e331fc829, 58c3215bc, 8cd2cc3aa, 3070dfc85, ab9dc12a7, c8be7c738
  (each: design log end, main first; diff vs main the piece's 14 files alone, 1757+/103-; no wip commit landed).
- Runs (macOS arm64, Mac Studio, stock git 2.50.1, rustc 1.98.1): r430 red 0/3 (hook path stubbed), green server
  `git_push:: git_hooks:: vcs_http::` 52/52 + slates-git `receive:: protected_fixtures` 49/49, fmt clean; merged trees
  r443 60/60, r457 61/61 (after A-694's append change), r468 67/67 (after A-629 part 2c's git site) + 49/49, fmt clean;
  the last merge (A-688, vcs_ssh svn only) stood on r468.
- Filed: run-requests.md "2026-11-19 g293 main c8be7c738" (clippy, xtask/doc gates, Linux both archs, Windows);
  amendments.tsv A-665 row (backup .bak-g293-a665p3). Studio clones + ~/.cache/g293-target removed (logs
  ~/.cache/g293-a665p3-logs{,2..6}); local worktrees removed. Scratch branch agents/g293-a665p3-scratch (3 wip
  commits, never landed) and agents/g293-a665p3-land (merged) left on GitHub for main.
- Left (so a push really runs its hooks): 3b, the daemon's presenter: a read-only attachment showing GitHookView at a
  path (a volume whose base-plane HostFs reads the repository's content store, under the root mount, D-1; Linux in the
  launcher's namespace); the HTTP edge's client-gone future (body_end passes pending()); the hooks' learned times
  (bound None); the SSH channel's call; seals as derived data. Until 3b, Unpresented => `hook not run`, as before.
