# Edits and reads past one bulk chunk were refused

Date: 2026-10-05. Scope: the typed client channel (§4.3, §4.12), so MCP `slates.merge.edit` and `slates.fs.read`,
the SDKs' `edit` and `read`, and the CLI verbs over them.

## Symptom

A new MCP test wrote a 16 KiB file into a work and read it back. The edit failed:

```
channel: payload of 16443 bytes exceeds the slot's 4096
```

Any edit inserting more than about 4 KB, and any read of a file larger than about 4 KB, failed the same way. The
mounts were not affected; the typed channel that agents and SDKs use was.

## Root cause

The design says "large payloads (archive streams, big reads through the SDK) travel through a per-client bulk
region referenced by offset from a slot" (§4.3). The implementation divides each client's bulk region into one
fixed chunk per slot and direction (`config::BULK_CHUNK_BYTES`, 4 KiB; `protocol::chunk`), and `protocol::pack`
refuses a framed message larger than its chunk (`PayloadTooLarge`). `Edit` carried its whole insert and
`ReadBytes` its whole file, and nothing paged either. The status report had already met the limit (2026-09-20) and
was paged then (`DaemonStatusPage`); edits and reads were not.

## Impact

Agents over MCP and SDK callers could not write or read a typical source file through the typed channel. The
failure was typed, never silent, but it made `slates.merge.edit` and `slates.fs.read` unusable beyond small files.

## Fix

- Failing test first: `assert_a_large_file_reads_whole` (`crates/mcp/tests/mcp.rs`) edits a 16 KiB file into a
  work through MCP and reads it back whole.
- Reads are paged: `ReadRange {volume, path, at, offset, max}` answers `ReadPage {bytes, total, stamp}`, each page
  sized to one reply chunk. The client's `read` loops over it, and refuses `ChangedWhileRead` when the stamp moves
  between pages. The stamp is the green's version for a green, the work's new `revision` counter for a work, and
  the inode's change counter for a plain volume. `assert_a_page_stamp_moves_with_the_file` checks that it moves on
  an edit and holds otherwise. Pages borrow the green's history (`Green::content_ref_at`) and read only the
  requested range of a plain volume, so a paged read costs its own bytes, not the file's per page.
- Large edits stay one edit: the client stages the insert on the work's owner (`StageBegin`, then `StagePut` in
  pages, each exactly what a request holds past the put's fixed framing), then sends `EditStaged`, one splice and
  one journal operation. Splitting the edit into several would leave half of it applied if a later page failed.
  Staging buffers (`crates/server/src/staging.rs`) are charged to the owner's metadata budget, whole or not at all.
  They belong to the principal that began them, expire one lease after their last use (released by the reaper or
  the next staging call), and are not durable. Unit tests cover order, idempotent retries, ownership, the budget
  and expiry.

## Siblings

- The SDKs' async verbs (`read_begin`/`read_poll`, and an async edit) still send one message. A large one is
  refused, typed, and never truncated. The async page loop is owed (GAPS).
- Work volumes have no capacity accounting: `edit` grows a work's content map and journal without a charge or a
  limit (works are created `Dynamic { max: 0 }`, and their content lives outside the charged store). That is an
  unbounded-growth defect of its own, owed next (GAPS).
