# A version-0 delta sent its header as a chunk of its own (2026-11-19, A-616)

## Description

Found by g297 on 2026-11-19, reading 1.15.0's `svndiff.c`, and claimed for g234 by main. The reporter's editor writer
(`crates/svn/src/report/editor.rs`) sent every svndiff stream's four-byte header, `SVN\0` for version 0, as a
`textdelta-chunk` of its own, then each window as up to three chunks: its integers, its instructions and its new data.
`svnserve` writes a version-0 window of new data alone in two chunks, the header leading the first when it is not out
yet. Every text a client older than 1.4 checked out or updated from slates, or any client announcing neither `svndiff1`
nor `accepts-svndiff2`, came as a different series of chunks from `svnserve`'s. A stock client decodes either, since the
svndiff stream is the same bytes in order; but a reply must be `svnserve`'s byte for byte (§4.17.1's client-fidelity
rule).

## Root cause

The writer was written from `window_handler` alone (`svndiff.c` 250-303 at `d434dc14`, 1.15.0 alike), which writes the
header on its first call and then a window's three sections, each when it is not empty. It missed `window_handler`'s
first test (259-261): a version-0 window holding no source copy and one instruction goes to
`send_simple_insertion_window` (89-151) instead, which builds the header, when it is not out yet, the window's integers
and its one instruction in one buffer and writes it as one chunk, then its new data. Every text `svnserve` sends whole,
an added file's or one whose base has run out, is such a window.

The recorded drives did not show it: every recorded `svnserve` reply was version 2 (stock 1.15 clients accept
svndiff2, and `window_handler` writes version 2's header alone), and the hand-made test of version 0 compared windows
decoded, not chunks.

## Impact

Version 0 alone: a client announcing neither svndiff version (Subversion before 1.4, and any client that leaves both
out) got each new-data window as three or four chunks where `svnserve` sends two. The bytes of the svndiff stream were
the same, so stock decoders, which read the stream whatever its chunks, applied the same texts. Version-2 replies, every
stock 1.15 client's, were `svnserve`'s already.

## Edits

- `crates/svn/src/report/editor.rs`: `window` writes a window `simple_insertion` names (version 0, one instruction, new
  data) as `send_simple_insertion_window` does, the header put in front of the encoded window in the writer's buffer so
  that the header, the integers and the instruction are one chunk, then the new data; every other window as before. The
  module documentation states both shapes.
- `crates/svn/fixtures/report/make-fixtures.py`: two hand-made sessions from a client announcing neither svndiff
  version, `svndiff0-checkout` (`/trunk` at r6 from nothing: every text new data, `big.txt` four windows) and
  `svndiff0-update` (from r5: `dir/b.txt`'s and `big.txt`'s windows copy from their bases), recorded against stock
  `svnserve` 1.15.0 once the Mac Studio is back.
- Test: `a_version_0_window_of_new_data_alone_carries_the_streams_header_in_its_first_chunk` (`report/editor/tests.rs`),
  from `svndiff.c` itself: an added file's two new-data windows, an opened file's copying window then one past its
  base, an empty file. By its reading it fails without the change, whose first chunk is `SVN\0` alone; the run, red
  then green, is owed with the Mac Studio, which was offline when this was written. Owed with the recording: the two
  sessions added to the recorded ones and `every_recorded_drive_is_written_again_as_svnserve_wrote_it` writing each
  drive with its own client's capabilities, so their chunks are held to `svnserve`'s byte for byte.

## Sweep

Every other writer of an svndiff stream was read for the same shape on 2026-11-19. Only the reporter's editor writes
`textdelta-chunk`s; the dump's writer (`dump/write.rs`), the HTTP listing's and drive's (`dav/listing.rs`,
`dav/drive.rs`) and the migration's export (`migration/export.rs`) each write one continuous stream, base64 or raw,
where the boundaries between writes never show. The commit's editor (`commit/edit.rs`, `commit/drive.rs`) writes no
deltas; it gathers a client's chunks and decodes every whole part of what it holds (`textdelta_chunk`, and the
reading of a delta's pending bytes), so an old client's header sent with its first window in one chunk is read alike.
