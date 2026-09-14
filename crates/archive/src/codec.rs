//! The compress-or-not cost model (§4.11 "Cost model", D-17): the per-chunk decision between raw,
//! LZ4 and a zstd level, made from **measured** inputs — the boot profile's codec throughputs and
//! ratios — never from a fixed "save 12.5%" rule (D-17 "Lost: … fixed 'save 12.5%' rules"). This
//! module is the pure decision: it owns no clock, no codec state and no I/O, so it is unit-tested with
//! synthetic profile points on every host and the archiver applies it ([`crate::Archive::chunk_with`]).
//!
//! The rule, in the design's words: *per chunk: zero-detect → Btrfs-style sampled statistics → LZ4
//! probe with early exit → predicted savings per level → choose the encoding maximizing
//! `bytes_saved × value_of_byte(pressure) − (t_compress + E[reads] × t_decompress) × value_of_cpu(load)`,
//! subject to the format floor (savings must exceed the chunk's metadata overhead).* Each element
//! traces to a precedent recorded in `research/compression-archive-dedup.md` §2.3: the sampler is
//! Btrfs's production heuristic (`fs/btrfs/compression.c`, constants reproduced with their names);
//! the probe-then-decide step is OpenZFS's zstd early abort ("LZ4 alone gets you a lot of the way";
//! measured 3 min 40 s → 48.6 s on incompressible data at under 0.3 % ratio cost) and Borg's `auto`
//! mode; the level prediction from the LZ4 size is the regression the research names, with Silesia's
//! prior (zstd −1 output ≈ 0.73 × LZ4's) until a profile point supplies the machine's own ratio.
//!
//! Integer arithmetic throughout: every rate is nanoseconds per byte scaled by [`RATE_SCALE`], every
//! value is parts per thousand, so two hosts with the same profile decide identically and the archive
//! bytes stay deterministic under one policy (the manifest identity is the BLAKE3 of the *raw* bytes and
//! never depends on the decision at all).

/// Format: Btrfs `SAMPLING_READ_SIZE` — the sampler reads this many bytes at each sampling point
/// (`fs/btrfs/compression.c`).
const SAMPLING_READ_SIZE: usize = 16;
/// Format: Btrfs `SAMPLING_INTERVAL` — one sample every this many bytes, so at most a sixteenth of
/// the chunk is examined (`fs/btrfs/compression.c`).
const SAMPLING_INTERVAL: usize = 256;
/// Format: Btrfs `BYTE_SET_THRESHOLD` — fewer distinct byte values than this in the sample means
/// "text, etc": compressible (`fs/btrfs/compression.c`, verdict 2).
const BYTE_SET_THRESHOLD: usize = 64;
/// Format: Btrfs `BYTE_CORE_SET_LOW` — a core set (the values covering 90 % of the sample) at or
/// below this is compressible (verdict 3).
const BYTE_CORE_SET_LOW: usize = 64;
/// Format: Btrfs `BYTE_CORE_SET_HIGH` — a core set at or above this is incompressible (verdict 0).
const BYTE_CORE_SET_HIGH: usize = 200;
/// Format: Btrfs's core set covers this share of the sample, in percent (`byte_core_set_size`).
const CORE_SET_COVERAGE_PERCENT: usize = 90;
/// Format: Btrfs `ENTROPY_LVL_ACEPTABLE` (sic) — sample entropy at or below this percentage of eight
/// bits is compressible (verdict 4).
const ENTROPY_LVL_ACCEPTABLE: u64 = 65;
/// Format: Btrfs `ENTROPY_LVL_HIGH` — below this, compress speculatively (verdict 5); at or above,
/// do not (verdict 0).
const ENTROPY_LVL_HIGH: u64 = 80;
/// Format: the Shannon entropy is reported as a percentage of a byte's eight bits.
const BITS_PER_BYTE: u64 = 8;
/// Format: parts per hundred — the scale of the sampler's percentages (Btrfs reports its entropy
/// levels and core-set coverage in percent).
const PERCENT: u64 = 100;
/// Format: [`PERCENT`] as a `usize`, for the sample-length arithmetic.
const PERCENT_LEN: usize = 100;
/// Format: the entropy sum is accumulated in fixed point with this many fractional bits, so the
/// integer `log2` approximation (Btrfs's `ilog2(n^4)` idea, here an exact `ilog2` over a scaled ratio)
/// keeps four bits of the fraction before the final percentage — enough to place a sample on either
/// side of the 65/80 thresholds without a float.
const ENTROPY_FRACTION_BITS: u32 = 4;
/// Format: parts per thousand, the scale of every value and ratio here.
pub const PERMILLE: u64 = 1000;
/// Format: a rate is nanoseconds per byte scaled by this, so sub-nanosecond per-byte costs (a codec at
/// several GB/s) keep three decimal places in integers.
pub const RATE_SCALE: u64 = 1000;
/// Format: the size of one chunk record's fixed fields in the archive — identity (32), raw and stored
/// lengths (8 + 8), encoding and level (1 + 1), dictionary (32): the metadata a compressed chunk must
/// out-save (§4.11 "the format floor"). Anchored to [`crate::format::Chunk`]'s wire layout.
const CHUNK_METADATA_BYTES: u64 = 32 + 8 + 8 + 1 + 1 + 32;
/// Derived: the Silesia prior for a zstd level's output as a share of LZ4's, in permille — the research's
/// starting regression before the machine's own ratio points replace it (`research/compression-archive-dedup.md`
/// §2.3 item 3: "zstd -1 output is about 0.73x LZ4's, 34.64% vs 47.60% of original").
const ZSTD_OVER_LZ4_PRIOR_PERMILLE: u64 = 730;
/// Format: the neutral value of a byte and of a CPU-nanosecond — [`PERMILLE`], one whole — the scaling
/// under which a byte saved is worth exactly the profile's cost of saving it. The live pressure and
/// load signals (`value_of_byte(memory pressure)`, `value_of_cpu(current load)`) raise or lower these;
/// until they are wired the model decides on the measured throughputs and ratios alone, which is the
/// design's rule at neutral values.
pub const NEUTRAL_PERMILLE: u64 = PERMILLE;

/// One measured codec point the policy chooses among: its level and its measured cost and ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecRate {
  /// The codec level (LZ4 has one, reported as 0; zstd levels as measured).
  pub level: i32,
  /// Measured compression cost in nanoseconds per input byte, scaled by [`RATE_SCALE`].
  pub compress_ns_per_byte: u64,
  /// Measured decompression cost in nanoseconds per output byte, scaled by [`RATE_SCALE`].
  pub decompress_ns_per_byte: u64,
  /// Measured compressed size as parts per thousand of the input, over the profile's corpus.
  pub ratio_permille: u64,
}

impl CodecRate {
  /// A rate from measured throughputs (bytes per second) and a ratio, the profile's units.
  pub fn from_throughput(
    level: i32,
    compress_bytes_per_second: u64,
    decompress_bytes_per_second: u64,
    ratio_permille: u64,
  ) -> CodecRate {
    CodecRate {
      level,
      compress_ns_per_byte: ns_per_byte_scaled(compress_bytes_per_second),
      decompress_ns_per_byte: ns_per_byte_scaled(decompress_bytes_per_second),
      ratio_permille,
    }
  }
}

/// Format: nanoseconds per second, for the throughput → cost conversion.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Nanoseconds per byte scaled by [`RATE_SCALE`] from a throughput in bytes per second; a zero
/// throughput (never measured) is the largest cost, so an unmeasured codec is never chosen.
fn ns_per_byte_scaled(bytes_per_second: u64) -> u64 {
  if bytes_per_second == 0 {
    return u64::MAX;
  }
  u64::try_from(
    (u128::from(NANOS_PER_SECOND) * u128::from(RATE_SCALE)) / u128::from(bytes_per_second),
  )
  .unwrap_or(u64::MAX)
}

/// The per-chunk decision policy: the measured codec points and the value constants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecPolicy {
  /// The LZ4 point, when measured — the probe codec and the fastest candidate.
  pub lz4: Option<CodecRate>,
  /// The zstd points, one per candidate level, when measured.
  pub zstd: Vec<CodecRate>,
  /// The **neutral worth of one byte** in scaled nanoseconds ([`RATE_SCALE`]): the exchange rate between
  /// the objective's two sides, bytes saved and CPU spent. Derived from the profile's measured memcpy
  /// bandwidth — at neutral pressure a byte of RAM is worth the CPU it takes to move a byte through
  /// memory, so a codec pays when the bytes it saves would have cost more to move than they cost to
  /// compress and decompress. `value_of_byte` scales this; without it the two sides would compare a
  /// byte to a nanosecond at par, a dimensionless coincidence, not a rule.
  pub byte_ns_scaled: u64,
  /// `value_of_byte(pressure)` in permille of neutral: what a byte of RAM saved is worth.
  pub value_of_byte_permille: u64,
  /// `value_of_cpu(load)` in permille of neutral: what a nanosecond of CPU spent is worth.
  pub value_of_cpu_permille: u64,
  /// `E[reads]`: how many times a chunk stored under this policy is expected to be decompressed.
  pub expected_reads: u64,
}

impl CodecPolicy {
  /// The policy that stores everything raw: no codec measured — a profile without the codec probe,
  /// or a build without the codecs. The same decision path with an empty candidate set (R8); the
  /// byte's worth is the rate scale itself (one scaled nanosecond), never consulted with no candidate.
  pub fn raw_only() -> CodecPolicy {
    CodecPolicy {
      lz4: None,
      zstd: Vec::new(),
      byte_ns_scaled: RATE_SCALE,
      value_of_byte_permille: NEUTRAL_PERMILLE,
      value_of_cpu_permille: NEUTRAL_PERMILLE,
      expected_reads: 1,
    }
  }

  /// The neutral worth of a byte from a measured memcpy bandwidth (bytes per second): the scaled
  /// nanoseconds moving one byte costs. A zero bandwidth (unmeasured) leaves the byte worth one scaled
  /// nanosecond, the floor.
  pub fn byte_worth_from_memcpy(bytes_per_second: u64) -> u64 {
    if bytes_per_second == 0 {
      return RATE_SCALE;
    }
    ns_per_byte_scaled(bytes_per_second).max(1)
  }

  /// Whether any codec is available to choose.
  pub fn has_codecs(&self) -> bool {
    self.lz4.is_some() || !self.zstd.is_empty()
  }
}

/// What the sampler concluded about a chunk before any codec ran (Btrfs's verdict classes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sampled {
  /// All zero: a hole, stored raw and deduplicated by identity — never compressed.
  Zero,
  /// Surely compressible (a repeated pattern, few distinct bytes, a small core set, low entropy).
  Compressible,
  /// Uncertain (entropy between the acceptable and high levels): probe before deciding.
  Uncertain,
  /// Surely incompressible (a large core set or high entropy): stored raw, no probe.
  Incompressible,
}

/// The chunk encoding the policy chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// Store the raw bytes.
  Raw,
  /// LZ4-compress.
  Lz4,
  /// zstd-compress at this level.
  Zstd(i32),
}

/// Btrfs's sampled statistics over `bytes` (`btrfs_compress_heuristic`, `research` §2.3): 16 bytes
/// every 256, then in order — a repeated half, the distinct-byte count, the 90 % core set, the Shannon
/// entropy — each verdict as the kernel's. Pure and bounded: at most a sixteenth of the chunk is read.
pub fn sample(bytes: &[u8]) -> Sampled {
  if bytes.iter().all(|byte| *byte == 0) {
    return Sampled::Zero;
  }
  let sampled = collect_sample(bytes);
  if sampled.is_empty() {
    return Sampled::Uncertain;
  }
  if repeated_halves(&sampled) {
    return Sampled::Compressible;
  }
  let mut histogram = [0usize; 256];
  for byte in &sampled {
    histogram[usize::from(*byte)] += 1;
  }
  let distinct = histogram.iter().filter(|count| **count > 0).count();
  if distinct < BYTE_SET_THRESHOLD {
    return Sampled::Compressible;
  }
  let core = core_set_size(&histogram, sampled.len());
  if core <= BYTE_CORE_SET_LOW {
    return Sampled::Compressible;
  }
  if core >= BYTE_CORE_SET_HIGH {
    return Sampled::Incompressible;
  }
  let entropy = entropy_percent(&histogram, sampled.len());
  if entropy <= ENTROPY_LVL_ACCEPTABLE {
    Sampled::Compressible
  } else if entropy < ENTROPY_LVL_HIGH {
    Sampled::Uncertain
  } else {
    Sampled::Incompressible
  }
}

/// The sample: [`SAMPLING_READ_SIZE`] bytes at every [`SAMPLING_INTERVAL`] offset.
fn collect_sample(bytes: &[u8]) -> Vec<u8> {
  let mut sampled =
    Vec::with_capacity(bytes.len() / SAMPLING_INTERVAL * SAMPLING_READ_SIZE + SAMPLING_READ_SIZE);
  let mut offset = 0;
  while offset < bytes.len() {
    let end = offset.saturating_add(SAMPLING_READ_SIZE).min(bytes.len());
    sampled.extend_from_slice(&bytes[offset..end]);
    offset = offset.saturating_add(SAMPLING_INTERVAL);
  }
  sampled
}

/// Btrfs `sample_repeated_patterns`: the first half of the sample equals the second half.
fn repeated_halves(sampled: &[u8]) -> bool {
  let half = sampled.len() / 2;
  half > 0 && sampled[..half] == sampled[half..half * 2]
}

/// Btrfs `byte_core_set_size`: how many of the most frequent byte values cover
/// [`CORE_SET_COVERAGE_PERCENT`] of the sample (the kernel radix-sorts the buckets; a sort of 256
/// counts is the same set).
fn core_set_size(histogram: &[usize; 256], sample_len: usize) -> usize {
  let mut counts: Vec<usize> = histogram
    .iter()
    .copied()
    .filter(|count| *count > 0)
    .collect();
  counts.sort_unstable_by(|a, b| b.cmp(a));
  let target = sample_len * CORE_SET_COVERAGE_PERCENT / PERCENT_LEN;
  let mut covered = 0;
  for (index, count) in counts.iter().enumerate() {
    covered += count;
    if covered >= target {
      return index + 1;
    }
  }
  counts.len()
}

/// Btrfs `shannon_entropy`, as a percentage of eight bits: `Σ p·log2(1/p)` over the histogram, in
/// fixed point with [`ENTROPY_FRACTION_BITS`] fractional bits (`ilog2` over the scaled ratio stands in
/// for the kernel's `ilog2(n^4)` approximation — both integer, both monotone in `p`).
fn entropy_percent(histogram: &[usize; 256], sample_len: usize) -> u64 {
  if sample_len == 0 {
    return 0;
  }
  let len = u64::try_from(sample_len).unwrap_or(u64::MAX);
  let mut sum_scaled = 0u64;
  for count in histogram.iter().copied().filter(|count| *count > 0) {
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    // log2(len / count) in fixed point: ilog2 of (len << fraction) / count.
    let ratio = (len << ENTROPY_FRACTION_BITS) / count;
    let log2_scaled =
      u64::from(ratio.max(1).ilog2()).saturating_sub(u64::from(ENTROPY_FRACTION_BITS));
    // Weighted by p = count / len, kept scaled by len to stay in integers.
    sum_scaled = sum_scaled.saturating_add(count.saturating_mul(log2_scaled));
  }
  // sum_scaled / len is the entropy in bits; as a percentage of eight bits.
  sum_scaled.saturating_mul(PERCENT) / (len.saturating_mul(BITS_PER_BYTE))
}

/// The LZ4 probe with early exit (OpenZFS early abort; Borg `auto`): compresses `bytes` and reports
/// the compressed size, or `None` when the output would not beat `accept_below` — the caller's
/// acceptance size, so an incompressible chunk costs one fast pass and no further codec work.
pub fn lz4_probe(bytes: &[u8], accept_below: u64) -> Option<(u64, Vec<u8>)> {
  let compressed = lz4_flex::block::compress(bytes);
  let size = u64::try_from(compressed.len()).unwrap_or(u64::MAX);
  (size < accept_below).then_some((size, compressed))
}

/// The predicted stored size of `bytes` under a zstd `rate`, from the LZ4 probe's size (`research`
/// §2.3 item 3): the machine's own ratio point against the LZ4 point when both were measured, else the
/// Silesia prior. Predicted, never below one byte.
fn predicted_zstd_size(policy: &CodecPolicy, rate: &CodecRate, lz4_size: u64) -> u64 {
  let share_permille = match policy.lz4 {
    Some(lz4) if lz4.ratio_permille > 0 => {
      rate.ratio_permille.saturating_mul(PERMILLE) / lz4.ratio_permille
    }
    _ => ZSTD_OVER_LZ4_PRIOR_PERMILLE,
  };
  (lz4_size.saturating_mul(share_permille) / PERMILLE).max(1)
}

/// The net worth of storing `raw_len` bytes at `stored_len` under `rate` (the design's objective,
/// in permille × scaled-nanosecond units): `bytes_saved × value_of_byte − (t_compress + E[reads] ×
/// t_decompress) × value_of_cpu`, each byte saved worth [`CodecPolicy::byte_ns_scaled`] scaled
/// nanoseconds and `t` the codec's scaled nanoseconds over the chunk. Negative when compressing does
/// not pay; the format floor is applied by the caller.
fn worth(policy: &CodecPolicy, rate: &CodecRate, raw_len: u64, stored_len: u64) -> i128 {
  let saved = i128::from(raw_len.saturating_sub(stored_len))
    * i128::from(policy.byte_ns_scaled)
    * i128::from(policy.value_of_byte_permille);
  let compress_ns = i128::from(rate.compress_ns_per_byte) * i128::from(raw_len);
  let decompress_ns = i128::from(rate.decompress_ns_per_byte)
    * i128::from(raw_len)
    * i128::from(policy.expected_reads);
  saved - (compress_ns + decompress_ns) * i128::from(policy.value_of_cpu_permille)
}

/// The decision for one chunk (§4.11's per-chunk rule): the sampler first — a hole or a surely
/// incompressible chunk is raw with no codec work; otherwise the LZ4 probe, whose size must beat the
/// format floor (`raw − metadata`) or the chunk is raw; then the predicted zstd size per measured
/// level, and the candidate (raw, LZ4, or a zstd level) with the greatest worth wins, raw when none is
/// positive. Returns the verdict and, when it is LZ4, the probe's output so it is not compressed twice.
pub fn decide(policy: &CodecPolicy, bytes: &[u8]) -> (Verdict, Option<Vec<u8>>) {
  if !policy.has_codecs() {
    return (Verdict::Raw, None);
  }
  match sample(bytes) {
    Sampled::Zero | Sampled::Incompressible => return (Verdict::Raw, None),
    Sampled::Compressible | Sampled::Uncertain => {}
  }
  let raw_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  let floor = raw_len.saturating_sub(CHUNK_METADATA_BYTES);
  if floor == 0 {
    return (Verdict::Raw, None);
  }
  let Some((lz4_size, lz4_bytes)) = lz4_probe(bytes, floor) else {
    return (Verdict::Raw, None);
  };
  let mut best: (Verdict, i128) = (Verdict::Raw, 0);
  if let Some(lz4) = policy.lz4.as_ref() {
    let lz4_worth = worth(policy, lz4, raw_len, lz4_size);
    if lz4_worth > best.1 {
      best = (Verdict::Lz4, lz4_worth);
    }
  }
  for rate in &policy.zstd {
    let predicted = predicted_zstd_size(policy, rate, lz4_size);
    if predicted >= floor {
      continue;
    }
    let zstd_worth = worth(policy, rate, raw_len, predicted);
    if zstd_worth > best.1 {
      best = (Verdict::Zstd(rate.level), zstd_worth);
    }
  }
  match best.0 {
    Verdict::Lz4 => (Verdict::Lz4, Some(lz4_bytes)),
    verdict => (verdict, None),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A profile-shaped point set: LZ4 fast and weak, zstd levels slower and stronger — the shape the
  /// boot probe measures (research §2.1's curves), as synthetic numbers.
  fn measured() -> CodecPolicy {
    CodecPolicy {
      // A byte's neutral worth from a measured 10 GB/s memcpy: 0.1 ns per byte.
      byte_ns_scaled: CodecPolicy::byte_worth_from_memcpy(10_000_000_000),
      lz4: Some(CodecRate::from_throughput(
        0,
        700_000_000,
        3_000_000_000,
        476,
      )),
      zstd: vec![
        CodecRate::from_throughput(1, 400_000_000, 1_200_000_000, 346),
        CodecRate::from_throughput(3, 250_000_000, 1_100_000_000, 320),
        CodecRate::from_throughput(9, 60_000_000, 1_000_000_000, 300),
        CodecRate::from_throughput(19, 4_000_000, 900_000_000, 280),
      ],
      value_of_byte_permille: NEUTRAL_PERMILLE,
      value_of_cpu_permille: NEUTRAL_PERMILLE,
      expected_reads: 1,
    }
  }

  fn text(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut line = 0u32;
    while out.len() < len {
      out.extend_from_slice(format!("line {} of a structured document\n", line % 97).as_bytes());
      line += 1;
    }
    out.truncate(len);
    out
  }

  fn noise(len: usize) -> Vec<u8> {
    // A xorshift stream: incompressible to every codec here.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
      .map(|_| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 56) as u8
      })
      .collect()
  }

  /// The sampler reproduces Btrfs's verdicts by use: zeros are a hole, text is compressible, noise is
  /// incompressible — with no codec run.
  #[test]
  fn the_sampler_classifies_holes_text_and_noise() {
    assert_eq!(sample(&vec![0u8; 8192]), Sampled::Zero);
    assert_eq!(sample(&text(8192)), Sampled::Compressible);
    assert_eq!(sample(&noise(8192)), Sampled::Incompressible);
  }

  /// The design's rule, by use (§4.11 "Hot volumes stay raw unless pressure raises `value_of_byte`;
  /// archived volumes compress once"): at **neutral** — a chunk read once, a byte worth one memcpy —
  /// even text stays raw, because moving a byte is far cheaper than compressing it (with these measured
  /// points, ~50×); once `value_of_byte` is raised to what a retained byte is worth, the same text is
  /// zstd at a measured level. Noise stays raw under either, after one LZ4 probe at most; with no codec
  /// measured everything is raw.
  #[test]
  fn text_stays_raw_at_neutral_and_compresses_when_bytes_are_precious() {
    let neutral = measured();
    assert_eq!(
      decide(&neutral, &text(8192)).0,
      Verdict::Raw,
      "at neutral a chunk read once is cheaper to move than to compress"
    );
    let mut precious = measured();
    precious.value_of_byte_permille = NEUTRAL_PERMILLE * PRECIOUS_BYTES;
    let (verdict, _) = decide(&precious, &text(8192));
    assert!(
      matches!(verdict, Verdict::Zstd(_)),
      "text with bytes made precious is zstd at a measured level, got {verdict:?}"
    );
    assert_eq!(decide(&neutral, &noise(8192)).0, Verdict::Raw);
    assert_eq!(decide(&precious, &noise(8192)).0, Verdict::Raw);
    assert_eq!(
      decide(&CodecPolicy::raw_only(), &text(8192)).0,
      Verdict::Raw
    );
  }

  /// Shape: how much more than neutral a byte is worth in the "precious" arm of the tests — ten
  /// thousand memcpys. With these points a byte must be worth ~50 memcpys before any codec pays, and
  /// near ~100 LZ4's speed and zstd-1's ratio tie (measured: at exactly 100× the model picked LZ4); at
  /// ten thousand the ratio codec's extra saving dwarfs every cost, so the arm exercises the zstd branch
  /// well clear of that crossover.
  const PRECIOUS_BYTES: u64 = 10_000;

  /// The value constants steer the objective the way the design says: CPU made costly enough turns a
  /// compressible chunk raw; RAM made precious enough climbs to a stronger (slower) level.
  #[test]
  fn value_of_cpu_and_value_of_byte_move_the_decision() {
    let bytes = text(8192);
    let neutral = decide(&measured(), &bytes).0;
    let mut costly_cpu = measured();
    costly_cpu.value_of_cpu_permille = NEUTRAL_PERMILLE * 1_000_000;
    assert_eq!(
      decide(&costly_cpu, &bytes).0,
      Verdict::Raw,
      "CPU priced far above bytes: store raw"
    );
    let mut precious_ram = measured();
    precious_ram.value_of_byte_permille = NEUTRAL_PERMILLE * 1_000_000;
    let strong = decide(&precious_ram, &bytes).0;
    let level = |verdict: Verdict| match verdict {
      Verdict::Zstd(level) => level,
      _ => i32::MIN,
    };
    assert!(
      level(strong) >= level(neutral),
      "bytes priced far above CPU: at least as strong a level ({strong:?} vs {neutral:?})"
    );
    assert_eq!(
      level(strong),
      19,
      "the strongest measured level wins when bytes are all that matter"
    );
  }

  /// The format floor: a chunk too small for any saving to exceed its record's metadata stays raw
  /// even when compressible.
  #[test]
  fn a_tiny_compressible_chunk_stays_raw_under_the_format_floor() {
    let bytes = vec![0x41u8; 40];
    assert_eq!(decide(&measured(), &bytes).0, Verdict::Raw);
  }
}
