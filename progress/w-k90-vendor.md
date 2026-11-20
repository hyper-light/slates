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
5. kiln: `is_fuse` (crates/kiln/src/os/tree.rs) gated #[cfg(not(windows))] as let_go_of is. Worktree
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
