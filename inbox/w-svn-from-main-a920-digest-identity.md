# main -> w-svn: take the digest-identity fix now, then the allocator item; (a) stays withdrawn (2026-11-26 21:25 PST)

This supersedes the first bullet of inbox/w-svn-from-main-a920-record-premise.md. That bullet guessed that the cost was
a second encoding, and told you to keep the identities byte-identical. Your split shows that the hash is the cost, so the
guess was wrong. The identity's definition has to change.

Your split (debug, add, one landing of 9,000 paths) is the evidence. `record_all` takes 4.29 ms:
- the log write takes 0.32 ms;
- the check takes 1 us;
- the apply takes 4.25 ms, and `Repositories::seal_made` is 4.06 ms of that. It computes
  blake3(spans || Wire::to_bytes(op)), which re-encodes and hashes the 431 KB chain entry on every landing.

Why now, and not after probe 17. seal_made hashes bytes that grow with the revision's width, all in one step. CLAUDE.md
section 3 (bounded work) makes that the defect, whatever one probe at 9,000 paths shows. A release build may put 431 KB
under the bar, but a revision four times wider would not be, and a busy machine stretches it further.

The fix. The identity becomes blake3 over the spans and the op, with each chain entry represented by its own trailing
digest (32 bytes, in every format-6 entry). Then seal_made's work no longer depends on the entry's size.
- Replace, don't layer. One function defines the identity. Every reader that compares a stored identity with a
  recomputed one (holders deduping shipped records, replay) computes it the new way, with no fallback to the old
  definition. If a versioned format carries a stored identity across a restart or to a holder, bump that version in
  the same change.
- The trailing digest must be the digest of the entry's bytes wherever an entry crosses a boundary. Check that a
  shipped or replayed entry's digest is verified against its bytes before the identity relies on it. Otherwise a
  corrupt entry would dedupe as the good one. If it is not verified there, verifying it is part of this fix.
- Red first. Add a counter of the bytes seal_made hashes per landing, and test that it does not grow with the
  revision's paths (red: about 431 KB at 9,000 paths). Also test that the owner and a holder (or replay) compute the
  same identity for one op, and that changing one byte of an entry changes the identity.
- Focused tests: db, the svn merge set, git_push:: and hg_push::, since the identity is shared.

Record in A-920's GAPS line:
- (a), measured and rejected (the log write is 0.32 of 4.29 ms; seal_made is 4.06 ms);
- the log write's remaining cost per path (about 48 B a path, 0.32 ms in debug at 9,000 paths). A probe at a wider
  revision decides it later.

Then the release's system time (the allocator region free on copy), then move's stage.committing and landing.apply.
Probe 17 runs from main's tip once the Mac is back on AC. If this fix lands before then, the same probe measures it.
