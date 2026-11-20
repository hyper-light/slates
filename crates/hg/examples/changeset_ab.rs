//! How a landed Mercurial history is held, measured (Ada's ruling of 2026-11-20, ~13:35 PST: "store it as changesets
//! ... so we aren't introducing a second underlying unit"; main's A/B order). Both arms hold exactly the deltas Mercurial
//! itself computed (each file revision's revlog chunk against its recorded base, the base's text by identity where the
//! chunk is whole), so neither diffs at the server; they differ in what the unit is:
//!
//! - **A, per-file**: each file's revisions in its own log, in revision order (a revlog's shape).
//! - **B, changesets**: one log in changeset order, each changeset's file revisions together (grouped by the changeset
//!   that introduced them, their link), a file's revisions found through its path history (the changesets touching it).
//!
//! Every version's full text is rebuilt once and its identity (plain BLAKE3, its length) recorded, as both arms keep
//! it in metadata. The log is held raw and, as it would land, in segments compressed with LZ4 and with zstd at levels
//! 1, 3 and 9, each segment `SEGMENT` bytes. For each arm and form: build time, bytes held, the process's peak RSS, an
//! old version's read latency (rebuild through its base chain, every segment it touches decoded through a bounded
//! cache, its text checked against the identity), and a clone's file section streamed (every file revision in
//! changegroup order, file by file, through the same cache).
//!
//! Command: `cargo run --release -p slates-hg --example changeset_ab -- <repository> <arm A|B> <samples>`. One arm a
//! process, so its peak RSS is its own. Prints every number; the load is the caller's to record.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::print_stdout,
  clippy::print_stderr
)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use slates_hg::Admission;
use slates_hg::revlog::Revlog;

/// Shape: a compressed segment's bytes before compression (a landed log is written and read in such pieces).
const SEGMENT: usize = 64 * 1024;
/// Shape: the decoded segments a reader keeps (a bounded cache, as a server's would be).
const CACHED_SEGMENTS: usize = 64;
/// Shape: the room each revlog read is admitted (the whole history fits).
const ROOM: u64 = 1 << 34;

/// One stored file revision: the revision its delta rests on (`None` for a whole text), where its bytes lie in the arm's
/// log, and its whole text's identity and length.
#[derive(Clone, Copy)]
struct Stored {
  base: Option<usize>,
  at: usize,
  len: usize,
  text: [u8; 32],
  text_len: usize,
}

/// A file's revisions as both arms number them: each revision's index in the arm's `revisions`.
struct File {
  revisions: Vec<usize>,
}

/// An arm's log: its bytes, every stored revision, and each file's revisions in revision order.
struct Arm {
  log: Vec<u8>,
  revisions: Vec<Stored>,
  files: BTreeMap<Vec<u8>, File>,
}

/// The log in compressed segments: each segment's compressed bytes, and the decoder.
struct Segments {
  codec: Codec,
  frames: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
enum Codec {
  Raw,
  Lz4,
  Zstd(i32),
}

impl Segments {
  fn of(log: &[u8], codec: Codec) -> Segments {
    let frames = log
      .chunks(SEGMENT)
      .map(|piece| match codec {
        Codec::Raw => piece.to_vec(),
        Codec::Lz4 => lz4_flex::block::compress_prepend_size(piece),
        Codec::Zstd(level) => zstd::bulk::compress(piece, level).unwrap(),
      })
      .collect();
    Segments { codec, frames }
  }

  fn bytes(&self) -> usize {
    self.frames.iter().map(Vec::len).sum()
  }

  fn decode(&self, at: usize) -> Vec<u8> {
    let frame = &self.frames[at];
    match self.codec {
      Codec::Raw => frame.clone(),
      Codec::Lz4 => lz4_flex::block::decompress_size_prepended(frame).unwrap(),
      Codec::Zstd(_) => zstd::bulk::decompress(frame, SEGMENT).unwrap(),
    }
  }
}

/// A bounded cache of decoded segments, oldest out first; counts its decodes.
struct Cache<'s> {
  segments: &'s Segments,
  held: HashMap<usize, Vec<u8>>,
  order: VecDeque<usize>,
  decodes: u64,
}

impl<'s> Cache<'s> {
  fn new(segments: &'s Segments) -> Cache<'s> {
    Cache {
      segments,
      held: HashMap::new(),
      order: VecDeque::new(),
      decodes: 0,
    }
  }

  /// `len` bytes of the log from `at`, decoding each segment they lie in that the cache lacks.
  fn read(&mut self, at: usize, len: usize, out: &mut Vec<u8>) {
    out.clear();
    let mut pos = at;
    let end = at + len;
    while pos < end {
      let segment = pos / SEGMENT;
      if !self.held.contains_key(&segment) {
        if self.order.len() == CACHED_SEGMENTS
          && let Some(old) = self.order.pop_front()
        {
          self.held.remove(&old);
        }
        self.held.insert(segment, self.segments.decode(segment));
        self.order.push_back(segment);
        self.decodes += 1;
      }
      let bytes = &self.held[&segment];
      let from = pos - segment * SEGMENT;
      let to = (end - segment * SEGMENT).min(bytes.len());
      out.extend_from_slice(&bytes[from..to]);
      pos = segment * SEGMENT + to;
    }
  }
}

/// The stock repository's revlogs, read once from disk and leaked for the process's life (an example's source).
struct Source {
  dotencode: bool,
  root: std::path::PathBuf,
}

impl Source {
  fn revlog(&self, store_path: &[u8]) -> Option<Revlog<'static>> {
    let encoded = slates_hg::store::encode_path(store_path, self.dotencode).ok()?;
    let index_path = self
      .root
      .join(".hg/store")
      .join(String::from_utf8_lossy(&encoded).as_ref());
    let index: &'static [u8] = Box::leak(std::fs::read(&index_path).ok()?.into_boxed_slice());
    let mut data_path = index_path.clone();
    data_path.set_extension("d");
    let data: &'static [u8] = Box::leak(
      std::fs::read(&data_path)
        .unwrap_or_default()
        .into_boxed_slice(),
    );
    Revlog::open(index, data, &mut Admission::new(ROOM)).ok()
  }
}

/// Peak resident bytes of this process so far (`ps`'s RSS is the current; macOS reports `ru_maxrss` in bytes).
fn peak_rss_mib() -> f64 {
  let output = std::process::Command::new("ps")
    .args(["-o", "rss=", "-p", &std::process::id().to_string()])
    .output()
    .unwrap();
  String::from_utf8_lossy(&output.stdout)
    .trim()
    .parse::<f64>()
    .unwrap_or(0.0)
    / 1024.0
}

/// The `per_mille`th of `values` (rounded to the nearest rank, as `((n - 1) * p).round()`).
fn percentile(values: &mut [f64], per_mille: usize) -> f64 {
  values.sort_by(f64::total_cmp);
  values[(values.len().saturating_sub(1) * per_mille + 500) / 1000]
}

/// One file revision as read from the source: its path's index, revision, link, delta base, chunk and whole text's
/// identity and length.
struct Read {
  path: usize,
  rev: u32,
  link: u32,
  base: Option<u32>,
  chunk: Vec<u8>,
  text: [u8; 32],
  text_len: usize,
}

/// Every file revision of every one of `paths` read once from `source`: its chunk (Mercurial's delta against its base,
/// or whole), its base, its link, its whole text's identity.
fn read_every_revision(source: &Source, paths: &[Vec<u8>]) -> Vec<Read> {
  let mut read: Vec<Read> = Vec::new();
  for (path_index, path) in paths.iter().enumerate() {
    let store_path = [b"data/".as_slice(), path, b".i"].concat();
    let Some(revlog) = source.revlog(&store_path) else {
      continue;
    };
    for rev in 0..revlog.len() {
      let entry = revlog.entry(rev).unwrap();
      let chunk = revlog.chunk(rev, &mut Admission::new(ROOM)).unwrap();
      let text = revlog.text(rev, &mut Admission::new(ROOM)).unwrap();
      read.push(Read {
        path: path_index,
        rev,
        link: entry.link,
        base: (entry.base != rev).then_some(entry.base),
        chunk,
        text: *blake3::hash(&text).as_bytes(),
        text_len: text.len(),
      });
    }
  }
  read
}

/// Revision `at`'s whole text, rebuilt through `cache` from its chain's whole base and each delta after it.
fn rebuilt(arm: &Arm, cache: &mut Cache<'_>, at: usize, bytes: &mut Vec<u8>) -> Vec<u8> {
  let mut chain = vec![at];
  while let Some(base) = arm.revisions[*chain.last().unwrap()].base {
    chain.push(base);
  }
  let whole = arm.revisions[chain.pop().unwrap()];
  cache.read(whole.at, whole.len, bytes);
  let mut text = bytes.clone();
  while let Some(link) = chain.pop() {
    let stored = arm.revisions[link];
    cache.read(stored.at, stored.len, bytes);
    text = slates_hg::delta::apply(&text, bytes, &mut Admission::new(ROOM)).unwrap();
  }
  text
}

fn main() {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let root = std::path::PathBuf::from(&arguments[0]);
  let arm_name = arguments[1].clone();
  let samples: usize = arguments[2].parse().unwrap();
  let requires = std::fs::read_to_string(root.join(".hg/requires")).unwrap_or_default();
  let source = Source {
    dotencode: requires.lines().any(|line| line == "dotencode"),
    root: root.clone(),
  };

  let changelog = source.revlog(b"00changelog.i").expect("the changelog");
  let changesets = changelog.len();
  // Every tracked path, read from the store's fncache (each `data/<path>.i`).
  let fncache = std::fs::read(root.join(".hg/store/fncache")).unwrap();
  let paths: Vec<Vec<u8>> = fncache
    .split(|byte| *byte == b'\n')
    .filter_map(|line| {
      line
        .strip_prefix(b"data/")?
        .strip_suffix(b".i")
        .map(<[u8]>::to_vec)
    })
    .collect();
  println!(
    "arm {arm_name}: {changesets} changesets, {} paths; rss before {:.0} MiB",
    paths.len(),
    peak_rss_mib()
  );

  // Every file revision read once: its chunk (Mercurial's delta against its base, or whole), its base, its link, its
  // whole text's identity. Reading the source is the same for both arms and timed apart.
  let started = Instant::now();
  let read = read_every_revision(&source, &paths);
  let read_s = started.elapsed().as_secs_f64();
  println!(
    "source read: {} file revisions, {:.1} MiB of chunks, every text rebuilt and hashed, {read_s:.1} s",
    read.len(),
    read.iter().map(|r| r.chunk.len()).sum::<usize>() as f64 / 1048576.0
  );

  // The arm's order: A file by file (as read), B by link (changeset) then file.
  let started = Instant::now();
  let mut order: Vec<usize> = (0..read.len()).collect();
  if arm_name == "B" {
    order.sort_by_key(|at| (read[*at].link, read[*at].path, read[*at].rev));
  }
  let mut arm = Arm {
    log: Vec::new(),
    revisions: vec![
      Stored {
        base: None,
        at: 0,
        len: 0,
        text: [0; 32],
        text_len: 0
      };
      read.len()
    ],
    files: BTreeMap::new(),
  };
  // Where each (path, rev) lies among `read`, so a base names its stored revision.
  let mut position: HashMap<(usize, u32), usize> = HashMap::with_capacity(read.len());
  for (at, r) in read.iter().enumerate() {
    position.insert((r.path, r.rev), at);
  }
  for at in order {
    let r = &read[at];
    let stored_at = arm.log.len();
    arm.log.extend_from_slice(&r.chunk);
    arm.revisions[at] = Stored {
      base: r.base.map(|base| position[&(r.path, base)]),
      at: stored_at,
      len: r.chunk.len(),
      text: r.text,
      text_len: r.text_len,
    };
  }
  for (at, r) in read.iter().enumerate() {
    arm
      .files
      .entry(paths[r.path].clone())
      .or_insert(File {
        revisions: Vec::new(),
      })
      .revisions
      .push(at);
  }
  let build_s = started.elapsed().as_secs_f64();
  let reads: Vec<(usize, u32)> = read.iter().map(|r| (r.path, r.rev)).collect();
  drop(read);
  println!(
    "arm {arm_name} built in {build_s:.2} s: log {:.1} MiB raw, {} revisions; rss {:.0} MiB",
    arm.log.len() as f64 / 1048576.0,
    arm.revisions.len(),
    peak_rss_mib()
  );

  let mut rng = 0x2026_1120_u64;
  let mut next = move || {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    rng
  };
  let sample: Vec<usize> = (0..samples)
    .map(|_| (next() % arm.revisions.len() as u64) as usize)
    .collect();
  for codec in [
    Codec::Raw,
    Codec::Lz4,
    Codec::Zstd(1),
    Codec::Zstd(3),
    Codec::Zstd(9),
  ] {
    let started = Instant::now();
    let segments = Segments::of(&arm.log, codec);
    let encode_s = started.elapsed().as_secs_f64();
    // Old reads: each sampled revision rebuilt through its base chain, a fresh cache each read (a cold read).
    let mut micros = Vec::with_capacity(samples);
    let mut decodes = Vec::with_capacity(samples);
    let mut bytes = Vec::new();
    for at in &sample {
      let started = Instant::now();
      let mut cache = Cache::new(&segments);
      let text = rebuilt(&arm, &mut cache, *at, &mut bytes);
      micros.push(started.elapsed().as_secs_f64() * 1e6);
      decodes.push(cache.decodes as f64);
      let wanted = arm.revisions[*at];
      assert_eq!(
        (*blake3::hash(&text).as_bytes(), text.len()),
        (wanted.text, wanted.text_len),
        "{:?}",
        reads[*at]
      );
    }
    // A clone's file section: every revision file by file, in revision order, through one bounded cache.
    let started = Instant::now();
    let mut cache = Cache::new(&segments);
    let mut sent = 0usize;
    for file in arm.files.values() {
      for at in &file.revisions {
        let stored = arm.revisions[*at];
        cache.read(stored.at, stored.len, &mut bytes);
        sent += bytes.len();
      }
    }
    let stream_s = started.elapsed().as_secs_f64();
    println!(
      "arm {arm_name} {codec:?}: held {:.1} MiB (encoded in {encode_s:.2} s); old read µs p50 {:.0} p99 {:.0} max {:.0}, \
       segments decoded p50 {:.0} p99 {:.0}; clone file section {:.1} MiB in {stream_s:.2} s, {} segment decodes; rss {:.0} MiB",
      segments.bytes() as f64 / 1048576.0,
      percentile(&mut micros.clone(), 500),
      percentile(&mut micros.clone(), 990),
      percentile(&mut micros, 1000),
      percentile(&mut decodes.clone(), 500),
      percentile(&mut decodes, 990),
      sent as f64 / 1048576.0,
      cache.decodes,
      peak_rss_mib()
    );
  }
}
