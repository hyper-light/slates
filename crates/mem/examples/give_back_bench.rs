//! What a write costs on pages given back to the OS and cleared (A-105 on Windows, VFS plan step 12), against one on
//! pages never used and one on pages kept resident: the record behind "a write after reuse costs the same beyond the
//! spread".
//!
//! `cargo run --release -p slates-mem --example give_back_bench`
//!
//! Each round writes a fresh private region, whose record starts empty, then maps a fresh shared region, as a daemon's
//! content extent is, and in turn writes every byte of it as 64 KiB chunks: first on never-used pages (the allocation
//! clears each block, since the region's record starts whole), then after freeing and purging (the pages left and come
//! back), then after freeing without a purge (the pages stayed). The rounds interleave the four, the times are per MiB,
//! and every round is printed with the best of each.
//!
//! On a platform whose discard takes no page, the second row measures the same thing as the third.

use std::time::Instant;

use slates_mem::{ChunkArena, Extent, Region, SharedObject, Words};

/// Shape: the region's length, far above the caches, so a write row is its page cost and not one cache's.
const REGION: usize = 256 << 20;
/// Shape: one content chunk, the block a file's bytes are written in, sixteen 4 KiB pages.
const CHUNK: usize = 64 << 10;
/// Shape: the interleaved rounds whose best is reported.
const ROUNDS: usize = 11;
/// Shape: the fresh object's name differs per round and per process, since Windows keeps a name while it is held.
const NAME: &str = "slates-mem-give-back-bench";

/// Allocates and writes every chunk: the microseconds per MiB and the blocks.
fn write_chunks(arena: &mut ChunkArena) -> Result<(f64, Vec<Extent>), slates_mem::MemError> {
  let began = Instant::now();
  let mut chunks = Vec::with_capacity(REGION / CHUNK);
  for _ in 0..REGION / CHUNK {
    let chunk = arena.alloc(CHUNK)?;
    if let Some(bytes) = arena.bytes_mut(chunk) {
      bytes.fill(0xAB);
    }
    chunks.push(chunk);
  }
  let per_mib = began.elapsed().as_secs_f64() * 1e6 / (REGION >> 20) as f64;
  Ok((per_mib, chunks))
}

/// The one test of each row against the others, in turn.
fn main() -> Result<(), slates_mem::MemError> {
  let page = usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap_or(4096);
  let (mut untouched, mut fresh, mut reused, mut kept) =
    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
  for round in 0..ROUNDS {
    // The reference for what clearing a never-used block adds: a private region's record starts empty.
    let mut private = ChunkArena::new(page);
    private.add_region(Region::map(REGION, page, false)?)?;
    untouched.push(write_chunks(&mut private)?.0);
    drop(private);
    let object = SharedObject::create(
      &format!("{NAME}-{}-{round}", std::process::id()),
      REGION,
      Words::new(),
    )?;
    // SAFETY: this bench holds the only mapping of the object, and it declares no words.
    let exclusive = unsafe { slates_mem::ExclusiveObject::new(object) };
    let mut arena = ChunkArena::new(page).discarding_from(page);
    arena.add_region(Region::shared(exclusive, page))?;
    let (micros, chunks) = write_chunks(&mut arena)?;
    fresh.push(micros);
    for chunk in chunks {
      arena.free(chunk)?;
    }
    while arena.purge(usize::MAX).more {}
    let (micros, chunks) = write_chunks(&mut arena)?;
    reused.push(micros);
    for chunk in chunks {
      arena.free(chunk)?;
    }
    let (micros, _chunks) = write_chunks(&mut arena)?;
    kept.push(micros);
  }
  let best = |row: &[f64]| row.iter().copied().fold(f64::INFINITY, f64::min);
  for (name, row) in [
    ("never used, nothing to clear", &untouched),
    ("never used, cleared on allocation", &fresh),
    ("given back, cleared on allocation", &reused),
    ("kept resident, zeroed on free", &kept),
  ] {
    let rounds: Vec<String> = row.iter().map(|micros| format!("{micros:.1}")).collect();
    println!(
      "{name}: best {:.1} us/MiB of {ROUNDS}: {}",
      best(row),
      rounds.join(" ")
    );
  }
  Ok(())
}
