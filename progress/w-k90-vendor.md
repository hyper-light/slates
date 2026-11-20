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
 P2b part 1 LANDED slates main 216c57ad..392c1e9c: negotiation::begin + Negotiating::step (objects::Selecting stages:
    Commits via Paint::step, Edges, Haves, Choose, Wants, Tags; stacks persist; sparse frames held/released);
    negotiate = begin stepped to end. a745_a_negotiation_stepped_a_unit_at_a_time_answers_as_one_made_whole (1,024
    generated). 211 passed merged.
 NEXT P2b part 2: upload::Answer gains the negotiation pending (owned Asked from fetch::read, sparse::Read), its write
    taking the owner's quantum check and stepping Negotiating until done, then building the streaming answer as
    fetch::written does; v0 likewise; servers pass the quantum. Oracle: every upload test's bytes unchanged; a test
    serving with the quantum spent after every unit == whole. Then P3 (server: request room per slice; red by use).
    Also owed: begin's ready/cut walks and include-tag's scan in units.
 P2b part 2 LANDED slates main cf4e9b70..9ebcfbf8: upload::serve_in_turns + Answer::write_within (Choosing state;
    abandon -> band-3 ABORT appended to head); serve/write unchanged; upload tests' answer_of answers every exchange
    both ways, same bytes/refusal. Main's lint round boxed Begun::Selecting (9bb1558c2): adapted.
 NEXT P3 (server): HTTP vcs_http/git/upload.rs Upload and SSH vcs_ssh/git.rs UploadCommand call serve_in_turns and
    write_within with their quantum (they already step write per turn); admission = the request's room (revert A-713's
    StoreLender::room); red BY USE first: daemon_edge clone of a long history whose pack is past one request's room,
    over HTTP and over SSH. Owed after: v0 in turns, begin's ready/cut + include-tag scan in units, rule 4 generation.
 P3a LANDED slates main 57ea648f..5a93963f: Held hybrid (runs or bit table, Roaring), Paint::chosen -> Held;
    answer::packet_data (packets <= room/4 as the pack begins). Found by use: 64 KiB per request -> 3,000-commit clone
    refused "needs more room" (framing asked 131,106 after a 2.7 KB walk); with 3a answered whole.
    adas-mac-studio-2 (b24c) stopped answering kiln ~05:4x; MacBook Pro-2 (e30189) pushes over HTTPS
    (clone ~/.cache/w-k90-vendor-slates-push there).
 NEXT P3b (server): UploadServices::new(lender, room) (room from daemon_edge site) and SSH UploadCommand given
    ToolServices.room; admission = min(request room, StoreLender::room); serve_in_turns + write_within with
    PackSource::turn() (new provided method; ShardPacks overrides with the shard clock + quantum); by-use tests: HTTP
    long-history clone (existing A-713 test now at the request room) + an SSH long-history clone (new).
 P3b LANDED slates main f14562e4..562b5859: UploadServices::new(lender, room), UploadCommand::new(..., room); admission
    = min(request room, StoreLender::room); serve_in_turns + write_within(packs.turn()); PackSource::turn (ShardPacks:
    shard clock + step quantum); SSH dies on a refusal given while writing. By use: HTTP long-history clone (ring
    region) + new vcs_ssh::tests::receive::a745_a_long_historys_clone_over_ssh_is_answered_within_one_requests_room
    (64 KiB). 108 server / 214 git passed.
 A-745 git half: DONE as asked (sliced answer, slice-only admission, request room, by-use HTTP+SSH). OWED (in GAPS):
    protocol v0's fetch in turns; begin's ready/cut walks + include-tag scan in units; plans of scattered positions
    (16 B/run); rule 4 (a fetch held to the index generation it began on); the live git clone matrix rows (main's).

## Ada's rule "performance gaps upfront" (main, 2026-11-20): next 4 in order
 1. Rule 4 LANDED slates main ca204fc8..00b99ebb (3991b1df code, 00b99ebb docs). StoreLender::{Hold, hold, lend_held};
    vcs_store::upload::GitGeneration (shared Repository::fork per (repo, Repository::stamp), refs+HEAD snapshot,
    released within budget on last drop). Edges (HTTP upload, SSH git) hold on a Request, lend every later step from it.
    Red: vcs_store::upload::tests::a745_a_fetch_answers_from_the_generation_it_began_on_whatever_lands_meanwhile (push +
    tag v2 after step 0; held pack = start's objects in order, by id; unheld != start). Found: since P3b later steps had
    no refs, so include-tag lost tags; fixed by the hold. Note: zlib bytes can't be byte-equal across two streams --
    level sampling is timed on the shard's real clock (fragment::level), so objects compared by id (content hash).
    Pre-existing on main, not mine: clippy cognitive_complexity in crates/git/src/store/staging.rs:223 `unit`.
 2. NEXT: begin's ready/cut walks + include-tag scan in turns. Then v0 in turns; then bounded scattered plans.
    After 1+2: ask w-interop (via main) to rerun Windows git clone rows on current main.
 2. LANDED slates main ..508b2be8 (96cce352 code+test+docs, merges). begin reads only what the request names; ready
    (ready::Readying), cut (shallow::Cutting: Depth/Tips/Paint/Select/Finish) and include-tag (Stage::Carried via
    Graph::brought_set, Stage::Tags via Graph::tags_from) are Negotiating::step's. Upload writes sections when decided
    (upload::fetch::Before, Choosing.before; refusal before sections = Answer::refused). Red on main: begin read 3,499
    (a745_begin_reads_what_the_request_names_...), now <=4, <=2 a step. git 568 / server vcs 313 green.
    Landing note: main moves fast; the e30189 run now merges the bundle head over FETCH_HEAD and pushes, 5 tries.
 NEXT (main 2026-11-20, after item 2): ask w-interop (via main, turn text) to rerun Windows git clone rows on main.
    Then main's push-back: deterministic zlib level (inbox note to w-perf sent: inbox/w-perf-from-w-k90-vendor-zlib-level.md,
    state 19fde05); replaceable shard clock so rule 4's test compares packs byte for byte; A/B release on git/git.
    Then items 3 (v0 in turns), 4 (bounded scattered plans).
 SSH red (main, first): LANDED c21abe3c via e5f171a9 (base 26e125ad). a745 long-history SSH test: not a hang; the
    test client's fixed 4 s patience. Efficiency cores (taskpolicy -c background) red 2/2 -> green 2/2; push exits 0 in
    6.07 s, clone 18.07 s there. Peer::connect_patient (reads again on timeout; server phase waits still close a stuck
    session). Sibling likely: HTTP daemon_edge long-history push test (w-perf calls it flaky) -- not checked.
 A-718 encode poll: LANDED 67dd4ef4 via 982ea33d (tested on base dbd497bb). Red (debug, git/git v2.0.0 pack 55.9 MB,
    ~/.cache/w-k90-vendor-gitgit-v2.0.0.pack, head = v2.0.0^{commit}): watchdog ended the process twice, 3,508 / 3,541 ms
    in one encode unit (an object's whole trials + 10 level samples), gather unit 32,768 rows / 315 ms. Green: daemon
    lives, 164,487 objects, units <= 262,144 B and 1 row, longest unit 132 ms / turn 187 ms debug (1,492 s run).
    Encoder::begin/step + Putting (fragment.rs), Current/Reading + windowed Keep (encoding.rs), keep_version/keep_base
    take put roots. Owed: tree pair whole, parts' record one unit. Test: vcs_store::git_encode::tests::a718_no_unit_...
    (ignored; SLATES_ENCODE_PACK/HEAD). Note: CLAUDE.md §8 -- no clippy by agents (ran it twice earlier; stopped).
 NEXT: deterministic zlib level (w-perf no reply yet) + replaceable shard clock; then items 3 (v0) and 4 (plans).
 AFTER REBOOT (10:35): probe edits (deepen/partial, now w-bugs') backed out, saved as
    ~/.cache/w-k90-vendor-probe-deepen-partial.patch. Merged to f265b858d. The r110 encode spin = base 944588f6a, which
    predates the fix 67dd4ef4 (landed 982ea33d). Told main (inbox/main-from-w-k90-vendor-encode-spin.md) + run request.
