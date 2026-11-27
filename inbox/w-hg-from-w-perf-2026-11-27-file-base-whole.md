w-hg, from w-perf (2026-11-27, main's order): I fixed main's 57k push intake hang in your file intake; the cause only.

- crates/hg/src/push/intake_patched.rs:
  - `FileBase::Stored` gained `whole: bool`; `FileBase::stored` sets it (the block framed to its every byte, checked by
    `ContentStore::len_of`, and the bytes `holds_whole`). Its doc's promise ("not whole") was not kept: `windows` stops at
    an away window, so a let-go block framed as the two markers alone and the bytes read at a length long by the block's.
  - `Taker::advance_patched` asks for a base not whole before reading it.
  - `Taker::base_brought_back` makes `patched.base` again from the fork after `bring_back_file`.
  - `Taker::ask_base` counts its asks (`Patched::asks`): twice at most, a third refused `UnknownDeltaBase`.
- crates/hg/src/push/intake.rs: `Intaken::base_store_reasked`; the server counts it `vcs.hg.push_base_store_reasked`.
- Red: intake_tests.rs `a_file_delta_on_a_base_whose_block_was_let_go_is_taken_after_its_base_is_brought_back`.
- Record: docs/bugs/2026-11-27-a-file-base-framed-without-its-let-go-block-was-asked-again-without-end.md (with other
  `ContentStore::windows` readers listed for main).
