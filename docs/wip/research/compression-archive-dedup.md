# Compression, deduplication, hashing, and the archive format for slates

> **Current contract, A-9 (2026-09-05).** §4.2/§4.9 require bounded charged transfer/decompression and verification before publication; §4.13 scopes identity lookup and sharing to authorized consumers.
> See [the contract review](hecate-contract-review.md) and [the unified design](../SLATES_DESIGN.md).
> The rest of this file is dated research evidence; conflicting recommendations are superseded.

Status: research note (work in progress, written incrementally). Date: 2026-09-03.
Scope: an in-memory, copy-on-write, hermetic VFS ("slates") that must archive/compress volumes into RAM
and export them as byte streams, clone, and replicate across machines. No disk writes by us.

Evidence tiers used below: [A] peer-reviewed paper / thesis; [B] standard, textbook, official manual;
[C] widely deployed implementation, its design doc or source; [D] blog / benchmark (hardware stated).

## 1. Questions answered

1. Which compression algorithms, at which settings, and with which Rust crates, fit an allocation-aware,
   cross-platform in-memory VFS; what are the measured speed/ratio curves; can contexts be pre-allocated?
   Answer: zstd (RFC 8878) as the only ratio-bearing codec, LZ4 as the probe/hot-path codec, no Brotli, no xz.
   zstd and LZ4 both offer allocation-free operation from caller-owned workspaces; Rust bindings reach it only
   through the C library (`zstd-sys` with the `experimental` feature); pure-Rust `lz4_flex` is allocation-free
   for LZ4; pure-Rust `ruzstd` is a viable decoder-only fallback where a C toolchain is unavailable.
2. Do zstd dictionaries pay off for many small similar files, how are they trained, how much does training
   cost, and how should dictionaries be identified and versioned? Answer: yes, 2-5x better ratio on small
   records is documented by the zstd authors; train with FastCover from the volume's own data, address the
   dictionary by its BLAKE3 hash, embed it in the archive, and never mutate it in place.
3. How should the system decide whether to compress a chunk at all? Answer: a cheap sampled-statistics
   pre-filter (Btrfs's heuristic, reproduced here from source) followed by an LZ4 probe (OpenZFS early abort,
   Borg "auto"), with the acceptance threshold and the codec/level chosen by a cost model whose inputs
   (throughput per codec/level, memory bandwidth, free memory) are measured at boot and re-measured online,
   instead of a fixed "save 12.5%" rule.
4. Which deduplication granularity, and how should chunk size be chosen? Answer: whole-file dedup by content
   hash first (it captures roughly three quarters of the block-level gain on desktop file systems), FastCDC
   content-defined chunking only for files above a size threshold that is derived from the measured file-size
   distribution and the measured marginal dedup gain on the volume itself.
5. Which hash for content addressing? Answer: BLAKE3, fixed in the format (not chosen at boot), 256-bit,
   with the Bao tree mode for verified streaming; SHA-256 is the runner-up (hardware SHA extensions make it
   competitive only on some CPUs); xxh3/XXH64 are for in-memory tables and per-frame checksums only.
6. What archive format? Answer: a single-pass streamable, content-addressed chunk store with a Merkle
   manifest, fixed-layout little-endian headers, per-chunk BLAKE3 identity, zstd frames with the standard
   seekable-format seek table, embedded dictionaries, a trailer with an index for random access, and a
   versioned schema with explicit read/ignore/reject rules; the same container is the replication wire format.
7. What policy for archived volumes held in RAM? Answer: compress once at archive time with a level chosen
   by the cost model, keep the manifest uncompressed, decompress lazily per chunk on re-attach, and expect
   3-5x on source trees (Silesia's source-code member and the LBFS/FastCDC source-tree measurements).

## 2. Findings by sub-question

### 2.1 Algorithms and measured trade-offs

Terms. "Ratio" below is uncompressed/compressed unless a table says "size %" (compressed as a percentage
of original). "Window" is the maximum back-reference distance; it bounds decoder memory. "Level" is a codec's
speed/ratio knob. "Static context" is a codec working state carved from caller memory rather than malloc.

Zstandard format facts that constrain the design.
- A zstd frame starts with the little-endian magic 0xFD2FB528; the header optionally carries the original
  content size, a window descriptor, a dictionary ID, and a 32-bit content checksum that is the low four
  bytes of XXH64(seed 0) of the decoded data. [B: RFC 8878 sections 3.1.1.1, 3.1.1.1.4]
- "Block_Maximum_Size is the smallest of: Window_Size [or] 128 KB", applying to both compressed and
  decompressed block size. Every zstd frame is therefore already a sequence of at most 128 KiB blocks.
  [B: RFC 8878 section 3.1.1.2.4]
- Window size: "The minimum Window_Size is 1 KB"; the maximum is 3.75 TB; "it's recommended for decoders
  to support values of Window_Size up to 8 MB". Any archive we emit with a window over 8 MiB is not portable
  to a conforming-but-minimal decoder. [B: RFC 8878 section 3.1.1.1.2]
- Skippable frames use magic 0x184D2A50..0x184D2A5F; the seekable format uses one of them (0x184D2A5E)
  to append a seek table. [B: RFC 8878 section 3.1.2] [C: zstd contrib/seekable_format spec]
- Dictionary format: magic 0xEC30A437, a non-zero 4-byte Dictionary_ID, entropy tables (Huffman literals,
  FSE offsets, match lengths, literal lengths), three repeat offsets, then raw content. [B: RFC 8878 section 5]
- Default parameters are chosen by source-size class. For inputs of at most 16 KiB the default windowLog is
  14 (16 KiB) at every level; at most 128 KiB it is 17; at most 256 KiB it is 18; larger inputs use 19 at level
  1 rising to 23 at level 19 and 27 at level 22. The strategy escalates fast -> dfast -> greedy -> lazy2 ->
  btlazy2 -> btopt -> btultra2. Consequence: compressing a chunk of at most 128 KiB never needs more than a
  128 KiB window regardless of level, so decoder memory per chunk is bounded by the chunk size.
  [C: facebook/zstd lib/compress/clevels.h]

Allocation-free zstd (the property slates needs).
- `ZSTD_estimateCCtxSize(level)` returns a budget "for any compression level up to the selected one" but
  "does not include space for [the] window buffer, so estimation is only guaranteed for single-shot
  compressions"; `ZSTD_estimateCStreamSize(level)` is the streaming worst case; `ZSTD_estimateDCtxSize()`
  and `ZSTD_estimateDStreamSize(windowSize)` are the decoder equivalents; `ZSTD_estimateCDictSize(dictSize,
  level)` sizes a digested dictionary. [B: zstd manual, Memory estimation]
- `ZSTD_initStaticCCtx(workspace, size)` / `ZSTD_initStaticDCtx` build a context inside a caller buffer that
  "must be 8-bytes aligned" and "must outlive [the] object"; "zstd will never resize nor malloc() when using a
  static buffer". Limitations: static contexts are "incompatible with internal dictionary creation,
  multi-threading, or legacy support". `ZSTD_initStaticCDict` exists for dictionaries. [B: zstd manual,
  Static allocation]
- Consequence: slates can pre-size one CCtx workspace per worker for the highest level it will ever use and
  one DCtx workspace per worker for its maximum window (the chunk size), carve both from its arenas at boot,
  and never call malloc on the compression path. Multithreaded zstd (`ZSTD_c_nbWorkers`) is unusable with
  static contexts, so parallelism must come from compressing different chunks on different workers, which is
  the natural shape here anyway. [B: zstd manual, ZSTD_c_nbWorkers; static allocation note]
- Streaming and framing: `ZSTD_getFrameContentSize` returns the decoded size when the frame carries it;
  "decompressed size is always present when compression is completed using single-pass functions";
  `ZSTD_decompressBound` gives an upper bound across frames; `ZSTD_compressBound(srcSize)` bounds output.
  [B: zstd manual]
- Long-distance matching (`--long`, `ZSTD_c_enableLongDistanceMatching`) "increases default windowLog to
  128 MB"; `--long` "defaults to 27" and a decompressor with a smaller default limit must be told the window.
  Per-chunk compression makes LDM irrelevant; cross-chunk redundancy is handled by dedup and dictionaries.
  [B: zstd manual, ZSTD_c_enableLongDistanceMatching] [C: zstd.1 man page]
- Negative levels: "The negative compression levels, specified with --fast=#, offer faster compression and
  decompression speed at the cost of compression ratio." [C: facebook/zstd README]
- Dictionary reference semantics: `ZSTD_CCtx_refCDict` parameters "supersede any compression parameter
  previously set within CCtx"; a `refPrefix` "is only used once" and the buffer "must outlive compression".
  [B: zstd manual]
- Decoder safety: `ZSTD_d_windowLogMax` limits the window the streaming decoder will accept "to protect the
  host from unreasonable memory requirements"; slates should set it to its chunk-size log so a hostile
  archive cannot request a multi-gigabyte window. [B: zstd manual, ZSTD_d_windowLogMax]

Measured speed/ratio curves (Silesia corpus; single thread; numbers are per the cited benchmark's hardware).
- lzbench 2.0.1, AMD EPYC 9554 at 3.10 GHz, gcc 14.2.0, Ubuntu 24.04.1, options `-eALL -t8,8 -o1c4`:
  memcpy 16332 / 16362 MB/s; zstd 1.5.6 -1 422 / 1347 MB/s, 34.64% of original; -2 344 / 1246, 32.79%;
  -5 125 / 1197, 29.74%; -8 62.9 / 1319, 28.32%; -11 34.4 / 1332, 27.49%; -15 8.36 / 1369, 26.97%;
  -18 3.79 / 1169, 25.16%; -22 2.08 / 1073, 24.69%; lz4 1.10.0 577 / 3716, 47.60%; lz4hc -1 262 / 3221,
  42.06%; lz4fast accel 3 657 / 3744, 50.52%; accel 17 1002 / 4166, 62.15%; brotli 1.1.0 -0 341 / 352,
  37.01%; -2 140 / 413, 32.12%; -5 37.1 / 451, 28.10%; -8 12.3 / 477, 26.96%; -11 0.58 / 389, 23.78%;
  xz 5.6.3 -0 23.6 / 98.2, 29.53%; -3 7.52 / 122, 26.30%; -6 2.97 / 127, 23.21%; -9 2.57 / 123, 23.00%;
  zlib 1.3.1 -1 93.0 / 323, 36.45%; -6 25.3 / 344, 32.19%; -9 10.3 / 348, 31.92%; libdeflate 1.23 -1
  207 / 860, 34.68%; -6 84.3 / 912, 31.85%; -12 5.14 / 919, 30.52%; snappy 1.2.1 401 / 1077, 47.85%;
  lzo1x 2.10 -1 513 / 696, 47.45%; bzip2 -1 14.8 / 46.6, 28.54%. [D: inikep/lzbench README]
- facebook/zstd README, Core i7-9700K at 4.9 GHz, Ubuntu 24.04, gcc 14.2.0, lzbench, Silesia:
  zstd 1.5.7 -1 ratio 2.896, 510 / 1550 MB/s; --fast=1 2.439, 545 / 1850; --fast=4 2.146, 665 / 2050;
  lz4 1.10.0 2.101, 675 / 3850; brotli 1.1.0 -1 2.883, 290 / 425; zlib 1.3.1 -1 2.743, 105 / 390;
  snappy 1.2.1 2.089, 520 / 1500. [D: facebook/zstd README]
- lz4/lz4 README, Core i7-9700K at 4.9 GHz, GCC 8.2.0, lzbench, Silesia: memcpy 13700 MB/s; LZ4 1.9.0
  ratio 2.101, 780 / 4970 MB/s; LZ4 HC -9 2.721, 41 / 4900; zstd 1.4.0 -1 2.883, 515 / 1380; zlib -1
  2.730, 100 / 415; zlib -6 3.099, 36 / 445. "All versions feature the same decompression speed" across LZ4
  levels. [D: lz4/lz4 README]
- Reading of the curves. (a) zstd decompression is flat at roughly 1.1-1.4 GB/s across all levels on the
  EPYC and 1.5 GB/s on the i7, so a higher level costs only at archive time. (b) LZ4 decompresses 2.7-3.7x
  faster than zstd -1 but stores 35-40% more bytes (47.6% vs 34.6% of original on Silesia). (c) Brotli at
  any level decompresses about 3x slower than zstd and at level 11 compresses at 0.58 MB/s; its ratio
  advantage over zstd -22 is 0.9 points (23.78% vs 24.69%). (d) xz decompresses at about 100-127 MB/s,
  roughly 10x slower than zstd, and compresses at 2.6-24 MB/s; its best ratio (23.00%) beats zstd -22 by
  1.7 points. On a re-attach path that must decompress on demand, xz and Brotli lose on the only axis that
  matters there. [D: same lzbench table]
- Real-world corroboration for dropping xz: Arch Linux moved package compression from xz to zstd citing
  "~0.8% increase in package size" for "~1300% speedup" in decompression (announcement dated 2020-01-04).
  [D: Arch Linux news, "Now using Zstandard instead of xz for package compression"]

LZ4 as the probe and hot-path codec.
- `LZ4_compress_fast(acceleration)`: "The larger the acceleration value, the faster the algorithm, but also
  the lesser the compression"; value 1 equals the default. [C: lz4/lz4 lib/lz4.h]
- Allocation-free: "Use LZ4_sizeofState() to know how much memory must be allocated, and allocate it on
  8-bytes boundaries"; `LZ4_compress_fast_extState` then compresses with that caller-owned state. The default
  hash table is `LZ4_MEMORY_USAGE` 14, "for 16KB, which nicely fits into most L1 caches". Decompression
  (`LZ4_decompress_safe`) needs no state at all and "never writes outside 'dst' buffer, nor read outside
  'source' buffer". `LZ4_compressBound(isize) = isize + isize/255 + 16`. [C: lz4/lz4 lib/lz4.h]
- `LZ4_decompress_safe_partial` decodes only up to a target output size, which lets a probe stop early and
  lets readers materialize a prefix of a chunk. [C: lz4/lz4 lib/lz4.h]

Brotli: what it is for and why not here.
- Brotli's window is (1 << WBITS) - 16 bytes for WBITS in 10..24 (about 1 KiB to 16 MiB); it embeds a static
  dictionary of 122,784 bytes (13,504 words, 121 transforms each), and each meta-block decodes to at most
  16 MiB; it is streamable with bounded intermediate storage; the RFC ties it to WOFF 2.0 fonts.
  [B: RFC 7932 sections 1.1, 1.2, 2, 8, 9.1]
- The built-in dictionary is English/HTML-flavoured and fixed; slates gets the same effect, tuned to the
  actual volume, from zstd dictionaries (section 2.2), with 3x faster decompression (lzbench above).

Rust crate support.
- `zstd` / `zstd-safe` / `zstd-sys` (gyscos/zstd-rs) wrap the C library; `zstd-sys` compiles it with the
  `cc` crate from `lib/common`, `lib/compress`, `lib/decompress`, plus `contrib/seekable_format` (feature
  `seekable`), `lib/dictBuilder` (`zdict_builder`), `lib/legacy` (`legacy`). Feature-to-define map:
  `experimental` -> `ZSTD_STATIC_LINKING_ONLY`, `ZDICT_STATIC_LINKING_ONLY`; `zstdmt` -> `ZSTD_MULTITHREAD`;
  `no_asm` -> `ZSTD_DISABLE_ASM`; `thin` -> `HUF_FORCE_DECOMPRESS_X1`, `ZSTD_NO_INLINE`,
  `ZSTD_STRIP_ERROR_STRINGS`, `DYNAMIC_BMI2=0`; bindgen is run over `zstd.h` (and `zdict.h`,
  `zstd_seekable.h` when enabled). [C: gyscos/zstd-rs zstd-safe/zstd-sys/build.rs]
- The static-allocation and estimate functions live in the `ZSTD_STATIC_LINKING_ONLY` section of `zstd.h`,
  so in Rust they are reachable only with `zstd-sys` feature `experimental`; the docs.rs listing of
  `zstd-safe` (57% documented) shows `CCtx`, `DCtx`, `CDict`, `DDict`, `CParameter`, `DParameter::WindowLogMax`
  but no static-workspace constructor, so slates should call `ZSTD_initStaticCCtx`/`DCtx` through `zstd-sys`
  directly and own the safety wrapper. Must-verify: re-check the current `zstd-safe` source for a workspace
  constructor before writing the wrapper. [C: docs.rs zstd-safe; docs.rs zstd-sys features]
  [B: zstd manual, static allocation section]
- `ruzstd` (KillingSpark/zstd-rs, pure Rust): "complete implementation of a Zstandard decompressor";
  encoder offers Uncompressed, Fastest (about level 1), Default (about 3), Better (about 7), Best (about 11)
  and "does not yet reach the speed, ratio or configurability of the original zstd library"; decoder measured
  about 3.5x slower than C on highly compressible data and about 1.4x slower on incompressible data;
  dictionary decompression supported; dictionary generation within 0.2% of the reference with `dict_builder`.
  [C: KillingSpark/zstd-rs README]
- `lz4_flex` (pure Rust): features `safe-encode`, `safe-decode` (default), `frame`, `std`; "no_std support is
  currently only for the block format"; `compress_into`, `decompress_into`, `compress_into_with_table` take
  caller buffers, and the hash table lives on the stack (8-16 KiB) or is caller-provided. Benchmarks (AMD
  Ryzen 7 5900HX, rustc 1.69.0): 66 KB JSON compress 1615 MB/s unsafe / 1272 safe vs 1469 for the C lz4
  1.9.3 binding, decompress 5973 (unchecked) / 5512 / 4540 safe vs 5313 C; 10 MB dickens compress 347 / 259
  vs 324 C, decompress 3168 / 2338 vs 2759 C. So pure-Rust LZ4 is at parity with C for our purposes.
  [D: PSeitz/lz4_flex README]
- `blake3` crate: features `std`, `rayon`, `mmap`, `neon` (required on ARMv7, default on AArch64),
  `wasm32_simd`, `zeroize`, `serde`, `pure`, `no_avx512`/`no_avx2`/`no_sse41`/`no_sse2`; `no_std`
  supported; `CHUNK_LEN` 1024, `BLOCK_LEN` 64, `OUT_LEN` 32; runtime CPU feature detection on x86.
  [C: docs.rs blake3]
- Cross-compilation implication: every C-backed crate (`zstd-sys`, `lz4-sys`) needs a working C toolchain for
  each of the nine targets (three OSes x arm64/x64, plus i686 Windows); `zstd-sys` also offers `pkg-config`
  linking to a system libzstd. Pure-Rust `lz4_flex` and `ruzstd` need nothing. Recommendation in section 4:
  build the C zstd everywhere it links (it is the reference implementation of the format) and keep `ruzstd`
  behind a feature as a decode-only fallback for targets where the C build is unavailable; never let the
  pure-Rust encoder produce archives (its frames are still valid RFC 8878, but ratio/speed differ).
  [C: zstd-sys build.rs] [C: ruzstd README]

Multithreading. zstd's own `nbWorkers` mode is excluded by static contexts (above). The right parallelism
for slates is chunk-level: each worker owns a static CCtx/DCtx pair and compresses whole chunks; the archive
format (section 2.6) records chunk boundaries so decode is embarrassingly parallel. [B: zstd manual]
[C: zstd seekable format spec]

### 2.2 Dictionary compression for many small similar files

Terms. A "dictionary" here is a zstd dictionary: a blob of representative content plus pre-built entropy
tables that both encoder and decoder load before a frame; it supplies match candidates that a small input
cannot supply for itself. "COVER" and "FastCover" are zstd's dictionary trainers. A "k-mer"/"d-mer" is a
substring of length k/d.

- Format: a zstd dictionary is `0xEC30A437` magic, a non-zero 4-byte Dictionary_ID, the four entropy tables,
  three repeat offsets, then content; a frame that used it records the Dictionary_ID in its header. The ID
  is a 32-bit tag, not a content hash, so slates must map IDs to real dictionary identities itself.
  [B: RFC 8878 sections 3.1.1.1.3, 5]
- Measured gain on small records (zstd authors): 1,000 GitHub user JSON records (about 850 KB) compressed
  2.8x without a dictionary and 6.9x with a trained 65,599-byte dictionary; "small data compression can
  range anywhere from 2x to 5x better than compression without dictionaries". The benchmark machine was an
  Intel E5-2678 v3 at 2.5 GHz, CentOS 7. [D: engineering.fb.com, "Smaller and faster data compression with
  Zstandard", 2016]
- The zstd CLI documents that dictionary compression "greatly improves efficiency on small files and
  messages" and that training "requires a lot of samples (> 100), and weight typically 100x the target
  dictionary size". Trainers: `--train-fastcover[=k#,d=#,f=#,steps=#,split=#,accel=#]` (default),
  `--train-cover[=k#,d=#,steps=#,split=#,shrink]`, `--train-legacy[=selectivity=#]`; `--maxdict=#` and
  `--dictID=#` set size and ID. [C: facebook/zstd programs/zstd.1.md]
- COVER algorithm (the basis of zstd's trainers): dictionary construction is cast as a string-covering
  problem over k-mers; a reservoir sampler finds the most frequent k-mers; the collection is split into
  "epochs" and in each epoch the segment whose k-mers achieve the highest coverage score (an l_p norm of
  k-mer frequencies, with already-covered k-mers zeroed) is appended to the dictionary; "Compared with the
  best existing pruning method, CARE, our scheme has a similar construction time, but achieves better
  compression effectiveness. Over several multi-gigabyte document collections, there are relative gains of up
  to 27%." With k = 16 the authors "can distinguish well between high and low-frequency k-mers"; the
  evaluation data sets include GOV2 and CC web crawls and KERNEL, "all (332) linux kernel versions", noted
  as "highly repetitive". Large-scale runs used an Intel Xeon E5640 with memory-mapped input.
  [A: Liao, Petri, Moffat, Wirth, WWW 2016]
- FastCover is the same idea with an approximate frequency table (parameter `f` is its log2 size) and an
  `accel` knob; it is zstd's default trainer since it replaced COVER in the CLI. [C: zstd.1.md]
  (must-verify exact speed/quality figures from `lib/zdict.h` / release notes; requested, see section 5)
- A pure-Rust trainer exists: ruzstd's `dict_builder` produces dictionaries "within 0.2% of the official
  implementation" on its test samples, raw-content only. [C: KillingSpark/zstd-rs README]
- Digested dictionaries can be static: `ZSTD_estimateCDictSize` + `ZSTD_initStaticCDict` place a CDict in
  caller memory; a `ZSTD_CCtx_refCDict` reference then overrides compression parameters for following
  frames. [B: zstd manual]
- Why this matters for slates specifically: with per-chunk compression (section 2.6) every chunk is an
  independent frame, and with 4 KiB median file sizes (section 2.4) most files are single small chunks;
  without a dictionary each such frame starts cold. The zstd measurements above are exactly this regime.
- What the data decides. Train from the volume's own bytes (a sample of chunks, stratified over paths, as
  COVER's epochs stratify over the collection); accept a candidate dictionary only if the measured
  compressed size of a held-out sample with the candidate beats the measured size with the incumbent by
  more than the amortized training cost (training time at the measured trainer throughput, converted with
  the same CPU-value constant as section 2.3). Content classes are discovered, not declared: compress a
  chunk sample with each existing dictionary and assign the class by best measured size; a file extension
  is only a hint for the sampler. [A: WWW 2016 (stratification rationale)] [D: fb.com 2016 (gain regime)]
- Lifecycle rules (design, justified by the format facts above): the identity of a dictionary is the BLAKE3
  hash of its bytes; the 4-byte RFC Dictionary_ID written in frames is the low 32 bits of that hash forced
  non-zero, used only as a cheap consistency check; the manifest stores the full hash per chunk; the
  dictionary is stored as an ordinary content-addressed chunk inside every archive that references it, so
  archives are self-contained and remain decodable forever; retraining produces a new dictionary with a new
  hash and never rewrites old chunks; a dictionary is garbage-collected when no live chunk references it.
  [B: RFC 8878 section 5] [C: casync/restic content-addressed stores, section 2.6]

### 2.3 Deciding whether (and how hard) to compress: "the data decides"

Terms. "Entropy" below is the sample Shannon entropy of byte values, expressed as a percentage of 8 bits.
"Core set" is the smallest set of byte values covering 90% of a sample. "Probe" is a fast trial
compression whose output size predicts the real codec's output size.

Btrfs heuristic (production Linux code; constants reproduced from source).
- Sampling: 16 bytes every 256 bytes (`SAMPLING_READ_SIZE 16`, `SAMPLING_INTERVAL 256`) over at most one
  128 KiB extent (`BTRFS_MAX_UNCOMPRESSED`), so at most 8 KiB (1/16 of the data) is examined.
  [C: linux fs/btrfs/compression.c, heuristic_collect_sample]
- Check order and verdicts: (1) `sample_repeated_patterns`: if the first half of the sample equals the
  second half, compress (ret 1); (2) byte histogram; `byte_set_size < BYTE_SET_THRESHOLD (64)` distinct
  values -> compress (ret 2, "text, etc"); (3) `byte_core_set_size`: radix-sort the 256 buckets, count how
  many top values cover 90% of the sample; `<= BYTE_CORE_SET_LOW (64)` -> compress (ret 3);
  `>= BYTE_CORE_SET_HIGH (200)` -> do not compress (ret 0); (4) `shannon_entropy` with an integer
  `ilog2(n^4)` approximation: `<= ENTROPY_LVL_ACEPTABLE (65)` -> compress (ret 4); `< ENTROPY_LVL_HIGH (80)`
  -> compress speculatively (ret 5); otherwise do not compress (ret 0). [C: fs/btrfs/compression.c,
  btrfs_compress_heuristic]
- Documented behaviour: "The tests performed based on the following: data sampling, long repeated pattern
  detection, byte frequency, Shannon entropy." Data are "split into smaller chunks (128KiB) before
  compression"; after a real compression attempt fails the inode gets a sticky NOCOMPRESS flag "not from
  heuristics alone"; zstd levels 1-15 plus negative levels since kernel 6.15. [C: btrfs.readthedocs.io,
  Compression]
- Cost: one pass over 8 KiB, a 256-entry histogram, a 256-entry radix sort and 256 integer logs; against
  LZ4 at 577-780 MB/s on Silesia (section 2.1) the full-extent LZ4 probe costs about 170-220 µs per 128 KiB
  while the heuristic touches 1/16 of the bytes once. (Derived from the cited throughputs; the absolute
  microseconds are a must-measure item.) [D: lzbench; lz4 README]

OpenZFS zstd early abort (production, 2.2.0+).
- Module parameters and defaults: `zstd_earlyabort_pass = 1` ("Enable early abort attempts when using
  zstd"), `zstd_abort_size = 128 * 1024` ("Minimal size of block to attempt early abort"),
  `zstd_cutoff_level = ZIO_ZSTD_LEVEL_3`. Procedure when level >= 3 and the block is >= 128 KiB: try LZ4;
  if LZ4 shrinks the block below the target, run the requested zstd level; otherwise try zstd level 1; if
  that also fails, store uncompressed. Source comment: "LZ4 alone gets you a lot of the way, but on highly
  compressible data, it was losing up to 8.5% of the compressed savings versus no early abort".
  [C: openzfs/zfs module/zstd/zfs_zstd.c]
- Measured effect (PR author, Ryzen 5900X, incompressible firmware data, 1 MiB recordsize, zstd-12):
  3 min 40 s -> 48.6 s; space cost about 76 MB on 42 GB (about 0.2%); on a Raspberry Pi 4 the same kind of
  workload went from 2 hours to 15 minutes with under 0.3% ratio penalty. [D: openzfs/zfs PR 13244]
- Release notes: "ZSTD early abort (#13244) - When using the zstd compression algorithm, data that can not
  be compressed is detected quickly, avoiding wasted work." [C: zfs-2.2.0 release notes]
- ZFS's fixed acceptance rule: "There is 12.5% default compression threshold in addition to sector
  rounding." [C: zfsprops(7)]
- Borg's per-chunk rule: "auto,C[,L]: Use a built-in heuristic to decide per chunk whether to compress or
  not. The heuristic tries with lz4 whether the data is compressible. For incompressible data, it will not
  use compression (uses "none"). For compressible data, it uses the given C[,L] compression."
  [C: borg help compression]
- restic stores a blob uncompressed when compression does not help; the blob type byte distinguishes
  compressed from plain data/tree blobs (format v2). [C: restic design document]
- zstd's own adaptive precedent: `--adapt[=min=#,max=#]` "dynamically adjusts compression levels based on
  I/O conditions". [C: zstd.1.md]

Why measurement beats a fixed "save at least 12.5%" threshold, and where the fixed rule still applies.
- The 12.5% rule exists because ZFS stores in physical sectors: a saving smaller than the rounding
  granularity is worth nothing. In RAM there is no sector; the value of a saved byte is a function of
  memory pressure (near zero with free memory, dominant near exhaustion), and the cost of saving it is
  measured CPU time now plus expected decompression time later. [C: zfsprops(7), "in addition to sector
  rounding"] [D: lzbench throughputs]
- Throughput varies by more than an order of magnitude across the targets slates supports: zstd -1
  compresses at 422 MB/s on an EPYC 9554 and 510 MB/s on an i7-9700K (section 2.1); BLAKE3 runs at 5.8 GB/s
  with AVX-512 on an i3-1005G1 and 380 MB/s with NEON on a Raspberry Pi 4 (section 2.5). A constant that
  is right on one machine is wrong by 10x on another. [D: lzbench; openzfs PR 12918]
- The Btrfs constants (65/80 entropy, 64/200 core set) and the ZFS "cutoff level 3" are priors chosen by
  their authors; OpenZFS's own comment quantifies that a probe-only policy lost "up to 8.5%" of savings on
  compressible data, which is exactly the kind of number a calibrated model should learn per volume rather
  than accept globally. [C: zfs_zstd.c]
- Where a fixed rule is right: a compressed chunk must save at least its own metadata overhead (the
  compressed-frame header and a manifest entry, both known constants of the format), otherwise it is a net
  loss regardless of pressure. That floor is derived from the format, not chosen.

Calibrated cost model (what slates should do; each element traced to a precedent).
1. At boot, benchmark in place: memcpy bandwidth at the chunk size; BLAKE3 throughput; LZ4 (accel 1) and
   zstd at a candidate level set (negative, 1, 3, 6, 9, 12, 19) compress and decompress throughput on (a) a
   sample of chunks from any attached volume, (b) synthetic incompressible and all-zero buffers. Record
   MB/s and achieved size per codec/level. Precedents: OpenZFS benchmarks every checksum implementation at
   module load and selects "fastest" (`zfs_blake3_impl`, `zfs_fletcher_4_impl`); PR 12918: "The fastest
   available implementation" is "determined through benchmarking conducted during module initialization".
   [C: zfs(4) man page; openzfs PR 12918]
2. Keep the model live: update per-codec throughput and the LZ4-size -> zstd-size regression with an
   exponentially weighted average of observed per-chunk timings and sizes, because boot measurements are
   perturbed by turbo, thermal state and co-tenants. Precedent: zstd `--adapt`. [C: zstd.1.md]
3. Per chunk: (0) detect all-zero (hole) by scan; (1) run the Btrfs sample heuristic, whose verdicts 0-5
   partition chunks into "surely incompressible", "surely compressible" and "uncertain"; (2) LZ4-probe the
   uncertain and compressible chunks with a caller-owned state and stop early when the output exceeds the
   acceptance size (`LZ4_compress_fast_extState`, `dstCapacity` bound); (3) predict each zstd level's size
   from the LZ4 size using the learned regression (initial prior from Silesia: zstd -1 output is about
   0.73x LZ4's, 34.64% vs 47.60% of original); (4) choose the level (or none, or LZ4 only) maximizing
   bytes_saved * value_of_byte(memory pressure) - (t_compress + E[reads] * t_decompress) *
   value_of_cpu(current load), where E[reads] is high for attached (hot) volumes and about one per re-attach
   for archived volumes. [C: fs/btrfs/compression.c] [C: lz4.h] [C: zfs_zstd.c] [D: lzbench]
4. Learn the priors away: record, for chunks in each heuristic verdict class, the realized zstd saving;
   move the verdict thresholds to the measured boundary where compression stops paying under the current
   value constants. This converts Btrfs's hand-picked 65/80/64/200 into per-volume measured quantities.


### 2.4 Deduplication and chunk-size choice

- Whole-file dedup captures about three quarters of the block-level savings on live desktop file systems and 87% on backups [A: Meyer & Bolosky FAST'11 abstract, §4]. Data Domain's design shows the index-locality problem of block dedup and its remedies (Summary Vector Bloom filter, stream-informed layout, locality-preserved caching) [A: Zhu et al. FAST'08]. LBFS introduced content-defined chunking with a 48-byte Rabin window, 2 KiB minimum, 8 KiB expected, 64 KiB maximum [A: Muthitacharoen et al. SOSP'01 §3.1]; FastCDC is about 10x faster than Rabin-based CDC and 3x faster than Gear/AE-based CDC with normalized chunking that narrows the size distribution [A: Xia et al. ATC'16]; the survey by Xia et al. covers the design space [A: Xia et al. Proc. IEEE 2016]; delta compression against similar chunks is a further step used in backup systems [A: Shilane et al. FAST'12].
- Policy: identity (BLAKE3 of the whole sealed object) first, which makes clone/snapshot/archive dedup free; page-multiple fixed chunks for large files (copy-cost-optimal, `cow-data-structures.md` §2.3); FastCDC only for the class of files whose measured size exceeds a threshold and whose observed dedup gain (bytes saved per byte hashed, tracked per volume) exceeds the measured hashing cost; the FastCDC parameters (min/avg/max) start from the paper's 8 KiB regime and are re-derived from the measured file-size distribution of the volume.

### 2.5 Hashing

- BLAKE3 is a 256-bit tree hash over 1 KiB leaves (`CHUNK_LEN` 1024, `BLOCK_LEN` 64, `OUT_LEN` 32) with SIMD across leaves, incremental and parallel by construction, and verifiable streaming through Bao [C: BLAKE3 spec and crate]. Throughput varies more than an order of magnitude across the target matrix (AVX-512 desktops versus NEON on small ARM parts) [D: openzfs PR 12918], so the boot profile measures it. SHA-256 with hardware extensions (ARMv8 SHA2, x86 SHA-NI) is competitive on some parts and is what REAPI uses [C: REAPI]; xxh3 is for non-cryptographic in-memory tables only.
- Collision resistance matters because agents (and any content they import) share one content store; a 256-bit cryptographic hash is required; BLAKE3 is fixed in the format for stability (an archive written on an AVX-512 machine must verify on a NEON one), so "benchmark and pick at boot" applies to *whether and when* to hash, not to *which* hash.

### 2.6 Archive format (field list)

Archive = one streamable byte sequence; the same container is the replication and clone-from-archive format.

1. Header (fixed layout, little-endian, 8-byte aligned): magic; format major/minor; header length; flags (compressed manifest, dictionaries present, seek table present); base page size used for chunking; chunk-size parameters; BLAKE3 of the manifest; total chunk count; total byte counts (raw, stored); creation timestamp (informational); volume identity and snapshot identity; name-equivalence policy id and Unicode version.
2. Dictionary section: zero or more zstd dictionaries, each an ordinary chunk (BLAKE3 identity, length, bytes), referenced by identity from chunk records; the RFC 8878 4-byte Dictionary_ID written into frames is the low 32 bits of the identity forced non-zero and used only as a consistency check.
3. Chunk records, in manifest order (streamable single pass): BLAKE3 identity (32 B); raw length; stored length; encoding (raw | lz4 | zstd) and level; dictionary identity or zero; optional per-record content checksum (the zstd frame carries its own XXH64); payload as a zstd frame (with content size) or raw bytes; page-multiple alignment of payload starts for zero-copy re-attach where the payload is raw.
4. Manifest (uncompressed by default so listing is O(1) per entry without decompression; optionally compressed as a whole when large): a canonical, sorted, Merkle-hashed tree: per directory node its entries (name bytes, kind, inode number, mode, times, size, nlink, xattr flags) and child identities; per file the extent list (offset, length, chunk identity, chunk offset) with holes as zero extents; the tree root identity equals the header's manifest hash.
5. Seek table: a zstd seekable-format skippable frame (magic 0x184D2A5E) listing frame offsets so any chunk can be located without scanning [C: zstd contrib/seekable_format].
6. Trailer: index of section offsets; BLAKE3 over the whole archive; the header repeated for tail-first readers.
Rules: readers reject unknown majors and unknown required flags and ignore unknown optional sections (read/ignore/reject per field is declared in the schema); every chunk verifies against its identity before use; a truncated archive is detected by the trailer; resumable transfer is by chunk identity (a receiver reports which identities it already holds, as TRANSFER's "missing set" does [C: survey-hecate.md §3.7]).

### 2.7 Space/time policy for archived volumes held in RAM

Archive once at archive time with the level chosen by the calibrated cost model under the current pressure; keep the manifest uncompressed; on re-attach decompress lazily per chunk on first read (the attach is metadata-only); expected ratios for source trees are 3-5x (Silesia's source-like members and LBFS/FastCDC source-tree measurements) with dictionaries lifting small-file classes further [D: lzbench; A: Muthitacharoen01; A: Xia16].

## 3. Measured numbers table

| Value | Conditions | Source (tier) |
|---|---|---|
| zstd -1 422/1347 MB/s, 34.64%; -3 ~29-32%; -19 8.4/1369, 26.97%; lz4 577/3716 MB/s, 47.60%; brotli -11 0.58 MB/s; xz -6 2.97/127 MB/s | EPYC 9554, lzbench, Silesia | lzbench README (D) |
| zstd -1 2.896 ratio, 510/1550 MB/s; lz4 2.101, 675/3850 | i7-9700K | facebook/zstd README (D) |
| zstd dictionary on 1,000 small JSON records: 2.8x → 6.9x | E5-2678 v3 | Facebook engineering 2016 (D) |
| Btrfs heuristic samples 16 B every 256 B over ≤128 KiB; thresholds 64/200 core set, 65/80 entropy | Linux source | fs/btrfs/compression.c (C) |
| OpenZFS early abort: 3m40s → 48.6s on incompressible data, ~0.2% space cost | Ryzen 5900X | openzfs PR 13244 (D) |
| Whole-file dedup ≈ 3/4 of block-level (live), 87% (backup) | 857 desktops | Meyer & Bolosky FAST'11 (A) |
| FastCDC ~10x Rabin CDC, ~3x Gear/AE | i7-4770 | Xia et al. ATC'16 (A) |
| BLAKE3 5.8 GB/s (AVX-512, i3-1005G1) vs 380 MB/s (NEON, RPi 4) | OpenZFS module benchmarks | openzfs PR 12918 (D) |

## 4. Recommendation for slates

Algorithms: zstd (C library through `zstd-sys` with static contexts carved from arenas; `ruzstd` decode-only fallback behind a feature) as the only ratio-bearing codec; LZ4 (`lz4_flex`, allocation-free) as probe and hot-path codec; no Brotli, no xz. Dictionaries: trained per content class from the volume's own data with FastCover, identified by BLAKE3, embedded in archives, never mutated, accepted only when a held-out sample proves the gain over the amortized training cost. Compress-or-not: all-zero detection, the Btrfs sample heuristic, an LZ4 probe with early exit, a learned LZ4-size-to-zstd-size regression, and a cost model calibrated at boot and updated online whose value-of-a-byte rises with measured memory pressure; the only fixed rule is the format-derived floor (a compressed chunk must save more than its own metadata). Chunking and dedup: whole-object identity first, page-multiple fixed chunks for large files, FastCDC for the measured large-file class only. Hash: BLAKE3 fixed in the format. Archive: the field list in §2.6. Runner-ups: Brotli (3x slower decode), xz (10x slower decode), CDC everywhere (CPU for little gain per Meyer & Bolosky), SHA-256 (slower without hardware extensions; not fixed-cost across the matrix).

## 5. Risks, unknowns, must-measure

- Per-platform codec and hash throughput; the LZ4-to-zstd regression per volume; dictionary training cost on the target machine classes; whether `zstd-safe` exposes static contexts (verify; else call `zstd-sys` directly); the C toolchain for `zstd-sys` on i686 and Windows ARM64.

## 6. Bibliography

- RFC 8878 (Zstandard); RFC 7932 (Brotli); RFC 1951 (DEFLATE). https://www.rfc-editor.org/
- facebook/zstd: manual, lib/compress/clevels.h, contrib/seekable_format, programs/zstd.1.md, README. https://github.com/facebook/zstd
- lz4/lz4: lib/lz4.h, README. https://github.com/lz4/lz4
- inikep/lzbench README. https://github.com/inikep/lzbench
- gyscos/zstd-rs; KillingSpark/zstd-rs (ruzstd); PSeitz/lz4_flex; BLAKE3-team/BLAKE3 and BLAKE3-specs. https://github.com/
- Y. Liao, M. Petri, A. Moffat, A. Wirth. Effective Construction of Relative Lempel-Ziv Dictionaries. WWW 2016. https://dl.acm.org/doi/10.1145/2872427.2883042
- Linux fs/btrfs/compression.c; btrfs.readthedocs.io Compression; OpenZFS module/zstd/zfs_zstd.c, PR 13244, PR 12918, zfsprops(7), zfs(4).
- Borg compression help; restic design document; casync; OSTree; IPFS CAR/UnixFS; Bazel REAPI.
- D. Meyer, W. Bolosky. FAST 2011; B. Zhu, K. Li, H. Patterson. FAST 2008; A. Muthitacharoen et al. SOSP 2001; W. Xia et al. ATC 2016; W. Xia et al. Proc. IEEE 2016; P. Shilane et al. FAST 2012.
- Arch Linux news 2020-01-04 (zstd packages).
