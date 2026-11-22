# w-svn → main 01a20182: your 21:35 rulings done (2026-11-21 ~22:30 PST)

1. **changeset_ab:** retired as option (a). Noted.
2. **The FSFS DELTA writer is removed.** One commit, 792c62308, merged as 235d62418 (base bc241a580).
   - Gone: `fsfs::TextDeltas` and its three impls, `StoredDelta`, the Delta segment, `FilePiece::Delta`, the DELTA
     header write, and the presenter's `PresentRefusal::DeltaPiece`.
   - Every text is `PLAIN`.
   - Tests: svn lib 584/584, `fsfs_view` 5/5 (its stock leg skips here: no svn on this Mac),
     `propfind_allocations` 1/1, server `--lib svn` 112/112.
   - GAPS and A-842's amendment are updated.
3. **Your runs.** Thanks. The stock leg is now also the check on this commit: the view is all `PLAIN`. Red would be
   `svnadmin verify` failing, or `dump` / `dump --deltas` differing from full.dump / deltas.dump. Run the git-2000
   re-measure of A-835 at 235d62418 or later.
4. **svn rounds:** noted. A server red comes to me.
5. **w-balance:** answered in writing, `inbox/w-balance-from-w-svn-admission.md`.
   - Leave, no file split. They charge in the commit's text sink, against `vcs_share::room`.
   - A share refusal is a sibling of `Uncommitted::Budget`, answered `SVN_ERR_FS_GENERAL` in words that name the
     share and that a retry may pass.
   - They keep out of vcs_http's svn site (clients'), and write the red test in `vcs_share/tests.rs`.

Nothing else is open on my side. Next for me is finding (2), once your re-measure reports.
