//! A-92 piece 3b's cost: wrapping a snapshot's archive in its envelope (every chunk sealed under the volume's lineage
//! key and named under its naming key) and opening it again, against the plain archive's own encode and decode.
//! `cargo run --release -p slates-cluster --example envelope_bench`; prints best-of-`RUNS` with every run shown.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::print_stdout,
  clippy::disallowed_methods
)]

use std::time::Instant;

use hyper_seal::keys::WrappingKey;
use hyper_seal::name::Namer;
use slates_archive::{Archive, Entry, Extent, Node, NodeMeta};

/// Shape: the snapshot's chunks and their size: 64 MiB in 64 KiB chunks, a working set a small volume seals.
const CHUNKS: usize = 1024;
/// Shape: one chunk's bytes.
const CHUNK_BYTES: usize = 64 * 1024;
/// Shape: the segment the volume seals in (its base page).
const SEGMENT: u32 = 4096;
/// Shape: timed runs per measurement; the best is the figure, all are printed.
const RUNS: usize = 5;

fn snapshot() -> Archive {
  let chunks: Vec<_> = (0..CHUNKS)
    .map(|index| {
      let seed = u8::try_from(index % 251).unwrap();
      let mut bytes: Vec<u8> = (0..CHUNK_BYTES)
        .map(|at| u8::try_from(at % 253).unwrap() ^ seed)
        .collect();
      // Every chunk distinct, so the envelope seals all of them.
      bytes[..8].copy_from_slice(&u64::try_from(index).unwrap().to_le_bytes());
      Archive::raw_chunk(bytes)
    })
    .collect();
  let entries = chunks
    .iter()
    .enumerate()
    .map(|(index, chunk)| Entry {
      name: format!("file{index:05}"),
      meta: NodeMeta {
        mode: 0o100_644,
        size: chunk.raw_len,
        nlink: 1,
        ..NodeMeta::default()
      },
      node: Node::File(vec![Extent {
        offset: 0,
        len: chunk.raw_len,
        chunk: chunk.identity,
        chunk_offset: 0,
      }]),
    })
    .collect();
  Archive {
    base_page_size: SEGMENT,
    chunk_min: 4096,
    chunk_max: u32::try_from(CHUNK_BYTES).unwrap(),
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    root_meta: NodeMeta::default(),
    manifest: Node::Directory(entries),
    chunks,
  }
}

fn timed(label: &str, bytes: usize, mut work: impl FnMut()) {
  let runs: Vec<f64> = (0..RUNS)
    .map(|_| {
      let start = Instant::now();
      work();
      start.elapsed().as_secs_f64()
    })
    .collect();
  let best = runs.iter().copied().fold(f64::MAX, f64::min);
  let shown: Vec<String> = runs.iter().map(|run| format!("{:.1}", run * 1e3)).collect();
  println!(
    "{label}: best {:.1} ms ({:.2} GB/s); runs ms [{}]",
    best * 1e3,
    bytes as f64 / best / 1e9,
    shown.join(", ")
  );
}

fn main() {
  hyper_seal::lock_keys(16).unwrap();
  let tenant = WrappingKey::generate(1).unwrap();
  let (naming, _) = tenant.make_child().unwrap();
  let namer = Namer::new(&naming).unwrap();
  let lineage = WrappingKey::generate(1).unwrap();
  let archive = snapshot();
  let bytes = CHUNKS * CHUNK_BYTES;
  timed("plain encode", bytes, || {
    std::hint::black_box(archive.encode());
  });
  let encoded = archive.encode();
  timed("plain decode", bytes, || {
    std::hint::black_box(Archive::decode(&encoded).unwrap());
  });
  timed("envelope wrap", bytes, || {
    std::hint::black_box(
      slates_cluster::envelope::wrap(&archive, &lineage, &namer, SEGMENT).unwrap(),
    );
  });
  let envelope = slates_cluster::envelope::wrap(&archive, &lineage, &namer, SEGMENT).unwrap();
  timed("envelope open", bytes, || {
    std::hint::black_box(slates_cluster::envelope::open(&envelope, &lineage, &namer).unwrap());
  });
  println!(
    "envelope: {} chunks, {} bytes encoded against the plain archive's {}",
    envelope.chunks.len(),
    envelope.encode().len(),
    encoded.len()
  );
}
