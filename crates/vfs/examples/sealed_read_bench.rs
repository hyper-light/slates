//! A-99 piece 4: the read chokepoint's cost with chunks sealed in the arena, against the same chunks in the clear.
//! 64 MiB of full chunks are sealed into one store under hyper-seal's `VersionKey` (AES-256-GCM, the server's
//! cipher) and into another with no cipher; then random 4 KiB reads at granule-aligned offsets, and a whole-file
//! read, through `ChunkStore::read_extent_into`. Reported: the 4 KiB read's p50, p99 and p999, and the whole read's
//! throughput, and the seal's cost per chunk, for both. The budget it is judged against: a 4 KiB open at most about 1 µs p99 (A-92, seal.md §1).
//!
//! `cargo run --release -p slates-vfs --example sealed_read_bench`

use std::time::Instant;

use hyper_seal::Secret32;
use hyper_seal::stream::VersionKey;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::content::{ChunkCipher, ChunkStore, Extent, KeyIdentity, Tag};
use slates_vfs::error::VfsError;
use slates_vfs::ids::Epoch;

/// Shape: the base page and granule.
const PAGE: usize = 4096;
/// Shape: the content the bench holds.
const CONTENT: usize = 64 << 20;
/// Shape: random 4 KiB reads measured per store.
const READS: usize = 200_000;
/// Shape: the reads one sample's percentile is taken over.
const PERMILLE: usize = 1000;
/// Shape: keys held at once: one secret and the version key it makes, for one sealed store at a time.
const KEYS_HELD: usize = 4;

/// The server's cipher shape over one key: hyper-seal's `VersionKey`.
struct Cipher(VersionKey);

impl ChunkCipher for Cipher {
  fn seal(
    &self,
    _: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
  ) -> Result<Tag, VfsError> {
    self
      .0
      .seal(version, index, last, segment)
      .map_err(|_| VfsError::Integrity)
  }
  fn open(
    &self,
    _: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
    tag: &Tag,
  ) -> Result<(), VfsError> {
    self
      .0
      .open(version, index, last, segment, tag)
      .map_err(|_| VfsError::Integrity)
  }
  fn identity(&self, _: u32) -> Option<KeyIdentity> {
    Some(KeyIdentity::default())
  }
  fn reference(&mut self, _: &KeyIdentity) -> Result<u32, VfsError> {
    Ok(0)
  }
  fn key_for_volume(&mut self, _: [u8; 16]) -> Result<u32, VfsError> {
    Ok(0)
  }
}

/// A store holding `CONTENT` bytes in full chunks, sealed when `sealed`; its extents, and the nanoseconds the seals
/// took (each `ChunkStore::seal` timed alone, summed).
fn filled(sealed: bool) -> Result<(ChunkStore, Vec<Extent>, u128), Box<dyn std::error::Error>> {
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(Region::map(CONTENT * 2, PAGE, false)?)?;
  let mut store = ChunkStore::new(arena, PAGE, CONTENT / PAGE);
  if sealed {
    let secret = Secret32::from_bytes(&[7u8; 32])?;
    store.set_cipher(Box::new(Cipher(VersionKey::new(&secret, [1u8; 16])?)));
  }
  let chunk = store.chunk_bytes();
  let plain: Vec<u8> = (0..chunk)
    .map(|at| u8::try_from(at % 251))
    .collect::<Result<_, _>>()?;
  let mut extents = Vec::new();
  let mut sealing = 0u128;
  for index in 0..CONTENT / chunk {
    let off = u64::try_from(index.checked_mul(chunk).ok_or(VfsError::Invalid)?)?;
    let mut open = store.open(off, chunk, Epoch(0), false)?;
    store.write_open(&mut open, 0, &plain)?;
    let started = Instant::now();
    let extent = store.seal(open, sealed.then_some(0))?;
    sealing += started.elapsed().as_nanos();
    if let Some(extent) = extent {
      extents.push(extent);
    }
  }
  Ok((store, extents, sealing))
}

fn percentile(sorted: &[u64], permille: usize) -> u64 {
  let at = (sorted.len().saturating_sub(1) * permille) / PERMILLE;
  sorted.get(at).copied().unwrap_or(0)
}

fn measure(label: &str, sealed: bool) -> Result<(), Box<dyn std::error::Error>> {
  let (store, extents, sealing) = filled(sealed)?;
  let chunk = store.chunk_bytes();
  let mut out = vec![0u8; PAGE];
  let mut state = 0x2545_F491_4F6C_DD1D_u64;
  let mut samples = Vec::with_capacity(READS);
  for _ in 0..READS {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    let count = u64::try_from(extents.len())?;
    let pages = u64::try_from(chunk / PAGE)?;
    let extent = extents
      .get(usize::try_from(
        state.checked_rem(count).ok_or(VfsError::Invalid)?,
      )?)
      .ok_or(VfsError::Invalid)?;
    let page = (state >> 32).checked_rem(pages).ok_or(VfsError::Invalid)?;
    let off = extent.off + page * u64::try_from(PAGE)?;
    let started = Instant::now();
    store.read_extent_into(extent, off, &mut out)?;
    samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    std::hint::black_box(&out);
  }
  samples.sort_unstable();
  let mut whole = vec![0u8; chunk];
  let started = Instant::now();
  for extent in &extents {
    store.read_extent_into(extent, extent.off, &mut whole)?;
    std::hint::black_box(&whole);
  }
  let seconds = started.elapsed().as_secs_f64();
  println!(
    "{label}: 4 KiB read p50 {} ns, p99 {} ns, p999 {} ns; whole {} MiB at {:.2} GB/s; seal {} ns a chunk ({} seals refused)",
    percentile(&samples, 500),
    percentile(&samples, 990),
    percentile(&samples, 999),
    CONTENT >> 20,
    CONTENT as f64 / seconds / 1e9,
    sealing / u128::try_from(extents.len().max(1))?,
    store.seal_refusals()
  );
  Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  // Keys live in hyper-seal's locked region, made once before the first key, as the daemon does at boot.
  hyper_seal::lock_keys(KEYS_HELD)?;
  for round in 1..=3 {
    println!("round {round}");
    measure("clear ", false)?;
    measure("sealed", true)?;
  }
  Ok(())
}
