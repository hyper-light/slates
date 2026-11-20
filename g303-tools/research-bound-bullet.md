- **The room a compressed answer is charged** (`compression::bound`, charged before the answer is compressed). Each
  engine's own documented worst case: libbzip2's "1% larger than the uncompressed data, plus six hundred extra bytes"
  (`BZ2_bzBuffToBuffCompress`; libbz2-rs-sys is its port), libzstd's `ZSTD_compressBound`, and for zlib the bound of
  the backend the workspace links (A-607): zlib-rs 0.6.8's `compress_bound`, zlib-ng's `compressBound`, `len +
  ceil(len / 8) + 3 + 6`, a byte more below 9 bytes and one more for none. Stock zlib's `compressBound` (`len + (len >>
  12) + (len >> 14) + (len >> 25) + 13`, `compress.c`) is no bound there: it counts stored blocks, which stock zlib
  falls back to on bytes that do not compress, while zlib-rs's level 1 is zlib-ng's `deflate_quick`
  (`src/deflate/algorithm/quick.rs`), which writes every literal in fixed codes, nine bits for the literals 144 to 255
  (RFC 1951 §3.2.6), and never falls back. {{RUN-BOUND}}
