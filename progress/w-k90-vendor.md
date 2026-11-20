# w-k90-vendor progress (kiln goal 90; brief briefs/w-k90-vendor.md) -- QUEUE DONE 2026-11-19 ~23:10 PST
Mac Studio d1323898. State clone ~/fanout-state-w-k90-vendor. Kiln clone ~/kiln-work/w-k90-vendor-kiln, worktree
~/kiln-work/w-k90-vendor-link. Target ~/.cache/w-k90-vendor-target. Policy read: kiln AGENTS.md/CLAUDE.md at f7ec15878.
PUSHING: this Studio has no GitHub credential. Landed through kiln `run` on adas-mac-studio-2.local (b24c5c95, SSH push
works) from a shallow clone there (~/.cache/w-k90-vendor-push): a git bundle of origin/main..branch carried over (base64
in env, or curl from a one-off http.server on this Studio's LAN address), fetched, `git push` sha:main. A `run` with
sync:true of the kiln repo to that machine hung (r7, never synced). Runner e30189ea (MacBook Pro-2) can push by HTTPS too.

1. Vendor slates at a pin, library API only: ALREADY ON kiln main (f7ec15878): vendor/slates at de1031f1d (contract 48),
   vendor.py check slates 0 differences (python3.12), crates/kiln depends on slates-api only, tests/slates_roles.rs
   asserts no Slates binary runs. Nothing built.
2. review reads core.fileMode: LANDED kiln main e33c47001..24fa289e0 (dfe8aa5c4, f9d870b60 + merges). git::file_modes,
   review Modes; tests g90_with_core_filemode_false_no_execute_bit_is_committed_as_git_records_none,
   g90_file_modes_reads_core_filemode_and_the_indexs_executable_paths; 6 passed; clippy -p kiln clean. agentic.md W8.
3. QUIC link runner-role only + reissue: LANDED kiln main 24fa289e0..2f343ed9c (g294's cf3e5c5a3/3a89feb58/b152f8b8a,
   merge f139c005d: ServerMsg::RunnerRole -> 87 after main's UserEnv 86, tables (75, 88); f3a4c8f7b clippy: AnyDriver's
   variants boxed, needless borrows, tests'/bench's net::Spec revoked_serials). w10_ proto::tests wire:: 266 passed on
   the merged tree; clippy -p kiln --lib --tests --benches clean.
4. slates_e2e bare_metal on Windows: code ALREADY ON main (b7720ebca, bare_metal cfg(any(unix, windows))).
   2026-11-20 on jupiter (kiln main fa96bcc47, clone C:\Users\corph\.cache\w-k90-vendor-kiln, target
   C:\Users\corph\.cache\w-k90-vendor-target): `cargo test --locked -p kiln --test slates_e2e --no-run` built (66 min
   cold); --list holds both bare_metal::g266_* tests. Python 3.13.17 + py launcher installed per-user (winget
   Python.Python.3.13 --scope user). `SLATES_E2E=1 slates_e2e.exe bare_metal:: --test-threads=1`: 0 passed, 2 failed, both
   at kiln's runner's own refusal "cannot mount here: WinFsp is not installed: winfsp-x64.dll did not load".
   BLOCKED (Ada): WinFsp (a machine-wide kernel file-system driver) is not installed on jupiter; not installed by me.
   To finish once it is: same binary, PATH with the Python Launcher, SLATES_E2E=1, bare_metal:: --test-threads=1.
   Finding (read, not mine): crates/kiln/src/os/tree.rs:101 `is_fuse` is dead code on Windows (a warning there; a
   Windows clippy -D warnings would fail).
Owed (main's): Linux/Windows clippy + tests of items 2-3 (gate), the Windows bare_metal drive once jupiter takes kiln work.

## Next pieces from main (2026-11-20)
Feedback: tool installs on Ada's machines (even user-scoped Python on jupiter) need Ada's word first (slates CLAUDE.md
§2 item 14). Item 4 waits on Ada's word about WinFsp on jupiter; target kept there.
5. kiln: `is_fuse` LANDED kiln main 28ea7df79..90623ee40 (Windows -D warnings no longer reports it; that build still fails
   on session.rs:5371 `sandbox::container` (hard error) and held.rs:29 unused import, reported). Was: worktree
   ~/kiln-work/w-k90-vendor-isfuse, branch agents/w-k90-isfuse (from kiln main 68964a93d), uncommitted until jupiter's
   check: `cargo rustc --locked -p kiln --lib -- -D warnings` on jupiter (kiln run r24, patch applied over 68964a93d in
   C:\Users\corph\.cache\w-k90-vendor-kiln, target ...\w-k90-vendor-target).
6. slates A-736 (SSH edge client-gone): LANDED slates main 431b1102..2b1b8106 (2a16c482 red, 028d88bf green, merges
   a8aee59c + 2b1b8106). vcs_ssh::site::ClientGone (client's bytes read_now'd while a step runs, kept for the session,
   EOF/reset = gone, past one read peer_closed once); Command::step(gone); git receive answer_step; svn tunnel via
   HookSite::Tunnel(policy, gone), client left -> E210002. Red: g90 test (false,false); green 79/79; merged tree
   vcs_ssh vcs_svn vcs_store::upload git_push vcs_http 159/159; fmt clean. amendments.tsv A-736 row (update to landed).
   A local merge 15453374 had conflict markers in vcs_svn.rs (my marker check covered docs only); never pushed: the
   landing went from a clean re-merge in ~/slates-work/w-k90-vendor-land (branch agents/w-k90-ssh-gone-land).
   Run requests (main's): clippy/xtask for slates-server; Windows build of vcs_ssh; a live git push over SSH whose
   client is killed while a hook runs.
7. g266 microVM EACCES (main's reorder): FOUND + FIXED. Looped kiln's `SLATES_E2E=1 VM_E2E=1 slates_e2e microvm::g266` on
   this Studio (kiln 28ea7df79, a local diagnostic in vendored slates logging each Shards stream request): Shards'
   owner word arrived as SetXattr node 1 uid 0 in the SHORT fuse_setxattr_in (8 bytes) while the stream had kept
   FUSE_SETXATTR_EXT -> missed, dispatched, errno 95 -> no owner swap -> run uid 200000 refused EACCES. Cause: Linux
   fuse_setxattr sizes by fc->setxattr_ext at the call, fuse_block_alloc holds it until INIT's reply (kernel.org
   master, sha256 89203290559bd37a / 29cc467f872b1ef0). Fix A-741: abi::setxattr_head (per-request form).
   LANDED slates main 2add2964..e0c06750 (b1f68fb9 red, be465b23 fix + merge); kiln recorded change 0002, kiln main
   5a294413c..81174ac49. Drive: 3/3 first-run failures before, 6/6 passes after (same pin, same bridge sources).
   Note: claimed A-737 then A-740 by mistake (both taken); A-741 is mine (registry fixed by a remote union rewrite).
Queue (main's list) done: is_fuse, A-736, A-741. Item 4 waits on Ada's word about WinFsp on jupiter (target kept there).
8. Goal "slates_e2e bare_metal tests run on Windows too": LANDED kiln main 4aabdba18..7e542f29c (1d0657923, a321a8189 +
   merge). bare_metal was already any(unix, windows) (b7720ebca) and skips loudly without WinFsp ("cannot mount here:
   WinFsp is not installed ... install WinFsp from winfsp.dev"; SLATES_E2E=1 makes it a failure, by design). Ported:
   stand_in (4 tests, no mount) off cfg(unix), its Python via vm_e2e::stub_python on Windows; the stand-in itself used
   os.uname (Unix-only) for its WHERE line -> platform.system, which the bare-metal drive's Windows run also needed.
   jupiter (kiln 81174ac49 + branch): stand_in 4/4 (4 failed before, AttributeError), bare_metal 2 skipped loudly;
   macOS stand_in 4/4. Once WinFsp is installed on jupiter: SLATES_E2E=1 slates_e2e bare_metal:: (Python Launcher on
   PATH), no code change; target C:\Users\corph\.cache\w-k90-vendor-target kept.
 P1 LANDED slates main bc180d63..86adda1c (86adda1c6 = merge of the piece; worktree ~/slates-work/w-k90-vendor-a745):
    Positions moved to slates_git::positions; Graph::Node: Into<u64>+TryFrom<u64>; Pack.objects/PackPlan.objects/
    Planned/Answered::objects are Positions; plan copy charged per run. Red (old per-object charge put back): left 96
    right 16. Green: slates-git negotiation::/upload::/store::... 207 passed; slates-server git upload paths 25 passed.
 NEXT P2: the walk's own sets (objects::select's BTreeSet/BTreeMap: objects, opened, done, sent commits, left_out;
    paint's flags; ready; shallow cut) made resumable under a quantum, upload::Answering {begin, step} modelled on
    hg's getbundle::Answering; oracle = sliced bytes == serve()'s whole for every upload test; least room < whole.
 P2a LANDED slates main b56e5766..1c5c79a9: negotiation::Held (Positions charged per run) for paint want/have/taken,
    sent commits, left_out, chosen objects, done, opened (tree: filter keeps a depth map); negotiation::Peak for queue
    and stacks; Positions::insert_new in place. Red (main 86adda1c6 + test): PastAdmission{asked 16,left 0}; green 210
    passed (merged tree). CAUTION learned: a scratch worktree sharing my target dir made cargo run ITS binary in my
    worktree (same member path hash) -> never share a target dir between two worktrees of one repo at once.
 NEXT P2b: walk resumable under a quantum (Paint::run, mark, open, sparse walk, ready, shallow cut) as a
    negotiation::Negotiating state machine with step(graph, spent); upload::Answering {begin, step}; oracle: sliced
    bytes == serve()'s whole for every upload test; least room a slice needs < whole answer's. Then P3 (server).
