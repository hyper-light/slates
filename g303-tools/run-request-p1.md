
## 2026-11-19T{{TIME}}Z g303 main {{MAIN}} (landing commit {{COMMIT}}) — A-650 part 1 (UI goal 93: Mercurial over HTTP, the hg site on the shared edge and its golden tests); supersedes the 18:45Z g303 entry
- Done here (macOS arm64, Mac Studio d1323898, CARGO_BUILD_JOBS=2), on the tree that lands (11865db7f, the commit's
  tree but for docs), kiln r137: `cargo test --locked -p slates-server --lib vcs_http` 40 of 40 (daemon_edge 1, git
  12, hg 7, serve 11, waits 9); `cargo test --locked -p slates-hg --lib` 306 of 306 (this piece's 9: 8
  `serve::http::tests::*` and `http::tests::a_repeated_command_field_reads_its_last_value`); `cargo fmt -p slates-hg -p
  slates-http-edge -p slates-server -- --check` clean. `cargo test --locked -p slates-http-edge --lib` 32 of 32 at
  929473182 (r6); its inputs (http, http-edge, test-seeds) are unchanged since. No integration target reaches
  `vcs_http` (private to slates-server) or slates-hg, so no `--test` target ran.
- Commands owed (Linux x86_64 and arm64; Windows x86_64 and arm64), in a clone of main at or after {{MAIN}} (Windows
  from a `core.autocrlf=false` clone, for the fixtures' CRLF heads; the fixtures are `-text` besides):
  `cargo test --locked -p slates-hg --lib`, `cargo test --locked -p slates-http-edge --lib`,
  `cargo test --locked -p slates-server --lib vcs_http -- --test-threads=4`. Red would be any of:
  `serve::http::tests::*` (8), `http::tests::a_repeated_command_field_reads_its_last_value`,
  `connection::tests::a_service_names_its_reply_fields_as_its_reply_begins`, `vcs_http::hg::tests::*` (7),
  `vcs_http::tests::a_service_names_its_reply_fields_once_and_a_failure_there_is_the_status`.
- For main's gates: new `slates_hg::serve::http` (`answer`, `Answered`, `Compression`, `Compressor`, `Media`,
  `Permission`, `permission`, `command`); `compression::Engine::{wire_name, for_wire_name}`, `compression::bound`;
  `slates_http_edge` `Connection::reply_fields`; server `Exchange::reply_fields` and `vcs_http::hg` (on A-625 part 3's
  edge: `Authenticator::prove`, `Located`/`Answer::Move`); new intra-doc links in those modules; the cross-target lint.
- Follow-up pieces for main to start (each its failing test first): agents/g303-a650-zlib-bound (zlib's bound as
  zlib-rs states it; 2570f7b52 red, 7a6e9095a green) and agents/g303-a650-query-414 (a query past the room 414;
  5b2f4cced red, d30cc46af green), both on the old part-1 base: rebase by cherry-pick onto main. Also: hand the hg site
  A-662's stores (`vcs_store`) and drop its `503` path; count the hg site's refusals by kind as git's are (A-625 part 3).
- Who to send failures to: g303 (progress/g303-hg-http.md).
