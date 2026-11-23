# reproduce a Subversion restart fencing r1 after one six-byte window is released

Reproduced on macOS arm64 at `1010c7e7e`, including the live-copy fix `b389d69cb`. Please route this recovery case to w-svn. It confirms the `named_lacking` sibling recorded in that fix.

Two Subversion repositories on one shard each imported r1 from a checked-in stock dump. Each entry had committed, named the exact file identity, and had yet to apply to its green. Both durable texts were readable. The test explicitly marked the second text landed and released only its six-byte window through `ContentStore::let_go`. Its name remained present. It then published the image and restarted both the daemon and the changeset-store owner.

On restart the first repository served all 17 bytes. The second green was fenced with `ContentMissing { version: 1 }`. Its first session read and its green file read both returned `None`, although the reopened changeset store still returned all six bytes of `alpha\n`. With both windows resident, both repositories served their files. The release is explicit in the fixture and uses the ordinary single-holder check.

At `crates/server/src/vcs_store/svn_texts.rs:254`, the first green selects its fetches before the content store opens. Once it has opened, `named_lacking` at line 313 counts a present name as readable even when its window is away. The second green skips the fetch and its replay fences at `crates/server/src/verbs.rs:20436`.

The attached `inbox/codex-review-svn-restart-named.patch` contains the regression and the matched resident control. `cargo test -p slates-server --lib a_subversion_restart_replays_ -- --test-threads=1` gave one pass, one failure, 1,223 filtered, in 1.77 s.
