# g298-svn-dav: progress (2026-11-19)

Goal: UI goal 93 (kiln 51cef9f7), git/Mercurial/Subversion servers; my piece A-627's rest (§4.17.4 piece 13's second
part), taken over from g250. Role: implementer (Claude subagent of main). THE RULE readback sent to main.
Policy read: restart-preamble-2026-11-18.md, slates CLAUDE.md §8 (origin/main), §4.17.4, lint-habits.md.

## Landed on main
- 1af538442 (merge of 494d62425 and my branch agents/g298-svn-dav; work commits afaf24109, fc3df7293, 65078aed2,
  c48845669, cc1d10464, and merges 585458a4c, 2b4ccaaef, 1af538442's parent chain):
  - `crates/svn/src/dav/drive.rs`: one `Drive` writes the update-report answer (update.c, send-all mode AND skelta
    mode) and the replay answer (replay.c) from A-616's `report::drive::Call` (`Drive::call` is the match). Hrefs and
    bc-url built from `Facts` (dav_svn__build_uri, svn_path_uri_encode); deltas streamed through `Base64Stream`;
    out-of-order calls refused `DavRefusal::DriveOrder`; aborted drive `DavRefusal::Aborted`.
  - `crates/svn/src/dav/locking.rs`: `lock_request`/`lock_answer`, `unlock_request`/`unlock_answer`, `lockdiscovery`,
    `MethodRefusal` (each pre-guard refusal at mod_dav's status), `HttpAnswer`; lockinfo read as
    dav_lock_parse_lockinfo; Timeout as dav_get_timeout (atol, whole seconds, DAVMinTimeout floor).
  - `dav/xml.rs`: `Document`/`parse_document` (namespaces in APR order), `Base64Stream`; `dav/response.rs`
    `Body::left`; `dav.rs` three refusal variants.
  - Fixtures: stock svn 1.15.0's LOCK/UNLOCK/PROPFIND heads + lock-steal.xml (recorded on the Studio 2026-11-19).
  - Docs: design §4.17.4 status + piece 13 + amendment "A-627, completed"; GAPS §4.17.4; research notes §1 + §3.
- Runs (Mac Studio, debug): slates-svn --lib 351/351 (65078aed2), 357/357 (585458a4c), 390/390 (2b4ccaaef); fmt clean.
  Red: six mutations, 7 of 351 failing; the namespace-order mutation needed a stronger test (two declarations on one
  element), which then failed alone.
- Hygiene: scratch ref wip-g298-a627-1 deleted; Studio clones and target deleted. Scratch evidence kept in
  ~/.local/state/slates-fanout/g298-scratch (capture/, svn-src-extra/ with hashes in the notes).

## Findings for main (not fixed here)
1. A-616's `Facts::created_revision` is documented as `svn_fs_node_created_rev`; update.c's hrefs and bc-url use
   `dav_svn__get_safe_cr`, which differs for a node beneath a copy not changed since (created rev predates the path).
   The drive (g234) should give the safe CR. Also g234's Facts doc says bc-url uses the target revision: update.c uses
   the safe CR.
2. `dav/listing.rs` `delta()` (file-revs) writes svndiff2 sections stored, never LZ4; Subversion compresses a section
   when shorter (svndiff.c 196-233). The drive now uses `Compression::Lz4`. Not changed in listing.
3. The landed update drive could not write rev -1 for a deletion (reporter.c passes it) and wrote send-all content
   outside send-all mode: both fixed here.

## Left (main starts each)
- The update-report's resource walk (S:resource-walk, pre-1.2 clients).
- Facts' revision as dav_svn__get_safe_cr (A-616's drive).
- Run requests filed 2026-11-19T07:20Z: clippy/xtask/doc, cross-target check, a stock-client run against the
  answers, main's live mod_dav_svn capture.
