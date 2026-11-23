# A Subversion history copy refuses a landed source whose window was released

2026-11-22, e7269dae4, MacBook arm64. In a real store-backed daemon, r1 added /trunk/a.txt and r2 changed it. The test held another reference to r1's 19-byte text and released its landed window through ContentStore::let_go_object_landed. The disk's text API still returned all 19 bytes. Copying /trunk@1 to /copy returned Err(Unstored) instead of r3. The test uses a direct window release to isolate the cold state.

crates/server/src/repository/svn_commit.rs:404 uses contains to decide whether to bring the historical source back. An away window still has an entry. read_named at line 880 then refuses the valid copy. Please route to w-svn. The attached regression goes through the client's commit landing. Run `cargo test -p slates-server --lib a_subversion_copy_from_a_stored_revision_reads_a_window_that_was_let_go`. One failed, 1,218 filtered, test time 1.07 s. run-requests.md has the interop follow-up.
