# To w-hg from w-interop: why slates' hg stream clone sends twice stock's data (2026-11-25)

main 01a20182 moved this to me off your queue while you build the content-store rule. At 15b4f1061 slates' stream sent
8,454 files and 144 MB where stock `hg serve` sent 4,284 files and 74.1 MB from the same repository. The fix is
yours, since the stream code is.

**How.** `crates/cli/bench/hg_stream_compare.sh` (b2888af84) fills a fresh stock `hg serve` and a fresh slates
repository by pushing the same source over HTTP, as `hg_scale.sh` does. It then stream-clones each
(`hg clone -U --stream --debug`) and compares the clones' stores, the streams' file lists and protocols, and
`hg debugrevlog`'s chunk kinds. Run here: Mercurial's own history cut at r400 (401 changesets, 104 store files), debug
slates at 8a03eb239, stock hg 7.2.4 (Homebrew), on the MacBook.

**Result, the same 2x as the 57k repository:** stock streamed 102 files and 470 KB; slates 195 files and 813 KB. No
file was sent twice by either. Two classes double, and one more thing differs:

1. **Uncompressed revlog chunks, the bytes.** Every chunk slates stores is `u` (uncompressed): changelog 401 of 401,
   manifest 401 of 401, `mercurial/hg.py`'s filelog 119 of 129, its chunk bytes 100% `u`. Stock compresses with
   zstd: chunk byte `0x28` on 98% of the changelog's chunks and most of the manifest's and the filelog's, raw deltas
   (`0x00`) for the rest, and `u` only on 8 tiny changelog revisions. The revisions and the delta chains are the same
   (changelog 0 deltas on both; manifest 385 of 401 deltas on both), so the bytes differ by compression alone:
   - changelog 148,189 against 103,912 bytes (1.43x; average full revision 369 against 259);
   - manifest 68,435 against 52,547 chunk bytes (1.30x);
   - `hg.py`'s filelog 94,904 against 40,086 (2.37x);
   - all filelogs 564,930 against 269,833 (2.09x).
2. **Split revlogs, the file count.** Slates writes every filelog and the manifest as an index `.i` and a separate
   data `.d`: 95 `.d` files, where stock keeps these small revlogs inline (data inside the `.i`, as hg does below its
   inline limit). So slates sends two files per revlog, and its `fncache` lists both (4,999 bytes against 2,514).
3. **The protocol.** Stock served stream v2 (a bundle2 `stream2` part). For slates, the client fell back to the legacy
   `stream_out` command (stream v1), likely because slates doesn't advertise `stream2`. v1 sends revlogs as stored, so
   it doesn't cause the 2x, but it's a difference from stock.

Smaller: `requires` is 59 bytes against stock's 83, so slates' clone may lack a requirement stock's has (perhaps
`revlog-compression-zstd`). `hg debugrequires` on both clones would say.

**What would close it:** write revlog chunks compressed (zstd where the client's requirements allow, else zlib, as hg
does), keep small revlogs inline, and offer stream v2. Rerun `hg_stream_compare.sh` to check: 102 files and about 470
KB at r400 against stock's.

Full record: `~/.cache/w-interop-hgstream/logs/record.txt` on the MacBook (both store listings, both debug stream
logs).
