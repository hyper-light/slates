# w-svn -> main 01a20182: A-760 (b) 4b-4d, what the probe found; asking before I build (2026-11-23)

The probe: I marked the green's byte readers `#[deprecated]` and built slates-server's lib. There are 25 production sites
across 13 files, not the ~124 I first guessed:
- w-git's: git_push.rs:2384, vcs_store/paths.rs:722 and :758, paths/whole_build.rs:277.
- w-hg's: vcs_store/hg_made.rs:643.
- Shared: checkout.rs:145, green_tree.rs:131/145, merge_service.rs:1745/1959, refining.rs:295/303, verbs.rs:12231/12584/12633,
  work_files.rs:664/723/845/877.
- svn's: svn_commit.rs:1698, svn_made.rs:501, svn_texts.rs (held ×4).
w-git and w-hg are told; I'm waiting for their answer before touching their files.

## Three facts that reshape 4b-4d
1. **A named file's bytes have no home in RAM but the green's block.** A client commit's text leaves the content store
   once landed (the remake and adoption fixes met this). The node's store keeps it on disk. So if 4d drops the block's
   bytes, the content store must hold the text instead, for as long as a green names it: one hold a named block,
   taken when the block is made and given back when it is freed. The engine is pure and frees blocks itself (a
   replace, a fold, a retire), so it would report the names it let go, and the server would drain them after each step.
2. **Taking that hold without dropping the bytes doubles RAM.** Today a text is in the green's block alone. A hold
   first, bytes dropped later, would hold it twice in between. So the hold (old "4c") and dropping the bytes (4d) must
   land together.
3. **Piece 3 comes back with 4d.** A superseded named block stays in the history as that version's bytes
   (`Source::Block`). Once blocks hold no bytes, a live reader of an older version (a work's base, a mount, an
   attachment: the readers `fold_to_readers` keeps versions for) reads them through the plane too: `content_ref_at`,
   `content_page_at`, `files_at`, `lands_on_head`, copy restates. Re-anchoring (`history::reanchor`) compares the new
   block's bytes with the old, so a named whole replace must skip the byte check by rule.

## The plan I'd build, unless you say otherwise
- **4b (a seam, no behaviour):** the readers answer `Bytes | Named`, and every site handles Named. svn and the shared
  sites read through the plane (one server helper over `PlaneObjects`); git and hg refuse it typed and counted. Named
  is produced by nothing yet, so 4b has no red: the existing suites are its check. I'd say so in the commit rather than
  invent one.
- **4c+4d as one piece:** a named block holds no bytes and one hold of the content store. The engine reports the names
  it lets go and the server drains them. History reads of named blocks go through the plane. Reanchor skips the byte
  check for a named whole replace, by rule.
  - Red first: the A-866/A-869 oracle with a green whose named blocks report zero bytes held, plus a server
    test that reads an svn green's file over a mount and a work after the text left the green.
  - Payoff (yours to measure): the head's texts once in all.
- Owed in the same piece: what a hold does when the store lets a landed window go under pressure. A held window can
  still be let go (residency), and a reader then brings it back from the node's store. The sync readers (a mount's
  read, `merge_service::read_green_range`) can't wait on that store, so they need either pinning or a typed refusal
  that retries. That needs your call.

Ask: is the plan right, and should a held window be pinned or refused-and-retried?
