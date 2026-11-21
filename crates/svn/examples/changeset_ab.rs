//! How a landed Subversion history is held, measured: svn's arm of the changeset A/B (Ada's ruling of 2026-11-20,
//! ~13:35 PST: changesets the one stored unit, per-file deltas derived; main's A/B order), the same shape as
//! `slates-hg`'s `changeset_ab` so both tools give one table. Both arms hold exactly the deltas the records computed at
//! import (S4 c2: each text an svndiff of version 2 against its node's skip-delta predecessor), so neither diffs here;
//! they differ in what the unit is:
//!
//! - **A, per-file**: each path's texts together, in revision order (FSFS's and a revlog's shape).
//! - **B, changesets**: one log in revision order, each revision's texts together, in its changed paths' order.
//!
//! Beside `changeset_ab`'s columns it times the import itself (the dump streamed in, every text deltified), counts the
//! skip-delta chains (the chain bound is svn's checkpoint spacing: a read applies at most that many deltas), and states
//! the history's whole-text bytes, so RSS reads against the data (Ada's sizing rule).
//!
//! For each arm and form (raw, and in `SEGMENT`-byte segments compressed with LZ4 and zstd at levels 1, 3 and 9): bytes
//! held, an old text's read latency (rebuilt through its base chain, every segment it touches decoded through a bounded
//! cache, the text checked against its identity and length), and a checkout's file section streamed (every text path by
//! path through the same cache). Every number is printed; the load is the caller's to record.
//!
//! Command: `cargo run --release -p slates-svn --example changeset_ab -- <dump> <A|B> <samples>`. One arm a process, so
//! its peak RSS (`/usr/bin/time -l`) is its own.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss,
  clippy::cast_sign_loss,
  clippy::print_stdout
)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read as _;
use std::time::Instant;

use slates_svn::dump::{Admission, Uuid};
use slates_svn::index::ContentIdentity;
use slates_svn::migration::{Importer, Importing, Repository, Room};
use slates_svn::svndiff;

/// Shape: a compressed segment's bytes before compression (as `changeset_ab` in slates-hg).
const SEGMENT: usize = 64 * 1024;
/// Shape: the decoded segments a reader keeps (as `changeset_ab` in slates-hg).
const CACHED_SEGMENTS: usize = 64;
/// Shape: a room no bench history comes near.
const ROOMY: Room = Room {
  entries: 1 << 26,
  bytes: 1 << 36,
};
/// Shape: an admission past the largest text of the git/git history.
const ADMISSION: Admission = Admission {
  buffer_bytes: 1 << 28,
};
/// Shape: the dump bytes read from the file at once.
const READ_PIECE: usize = 8 << 20;

/// One stored text: the text its delta is against (`None` for a first version), where its delta lies in the arm's log,
/// and its whole text's identity and length.
#[derive(Clone, Copy)]
struct Stored {
  base: Option<usize>,
  at: usize,
  len: usize,
  text: ContentIdentity,
  text_len: usize,
}

/// The log in compressed segments.
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

/// The dump at `path` imported, streamed `READ_PIECE` bytes at a time, each record committed as it is made.
fn imported(path: &str) -> Repository {
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-0000000064a5").unwrap();
  let mut repository = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOMY).unwrap();
  let mut importer = Importer::new(ADMISSION);
  let mut file = std::fs::File::open(path).unwrap();
  let mut buffer: Vec<u8> = Vec::new();
  let mut ended = false;
  let mut fill = |buffer: &mut Vec<u8>, ended: &mut bool| {
    let start = buffer.len();
    buffer.resize(start + READ_PIECE, 0);
    let read = file.read(&mut buffer[start..]).unwrap();
    buffer.truncate(start + read);
    *ended = read == 0;
  };
  fill(&mut buffer, &mut ended);
  loop {
    match importer.import(&mut repository, &buffer, ended).unwrap() {
      Importing::Done => return repository,
      Importing::NeedMore { consumed, .. } => {
        buffer.drain(..consumed);
        fill(&mut buffer, &mut ended);
      }
      Importing::Commit { consumed, record } => {
        buffer.drain(..consumed);
        repository.commit_record(*record).unwrap();
      }
    }
  }
}

/// This process's resident set now, in MiB (`ps`; the peak is the caller's `/usr/bin/time -l`).
fn rss_mib() -> f64 {
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

fn percentile(values: &mut [f64], p: f64) -> f64 {
  values.sort_by(f64::total_cmp);
  values[((values.len() as f64 - 1.0) * p).round() as usize]
}

/// The text a chain of deltas makes, read from the arm's log through `cache`: the first applied against nothing.
fn rebuilt(
  revisions: &[Stored],
  at: usize,
  cache: &mut Cache<'_>,
  bytes: &mut Vec<u8>,
) -> (Vec<u8>, usize) {
  let mut chain = vec![at];
  while let Some(base) = revisions[*chain.last().unwrap()].base {
    chain.push(base);
  }
  let depth = chain.len();
  let mut text = Vec::new();
  while let Some(link) = chain.pop() {
    let stored = revisions[link];
    cache.read(stored.at, stored.len, bytes);
    let (_, windows) = svndiff::decode_all(bytes).unwrap();
    text = svndiff::apply_all(&text, &windows).unwrap();
  }
  (text, depth)
}

/// A text as the row that first names it: its path's index, its revision, identity, delta base and delta, and its
/// whole length.
struct TextRead {
  path: usize,
  revision: u64,
  identity: ContentIdentity,
  base: Option<ContentIdentity>,
  delta: Vec<u8>,
  text_len: usize,
}

/// Every text of `repository` once, in revision order, as the rows that first name it: the paths, the texts, and each
/// text's place among them by identity.
fn read_every_text(
  repository: &Repository,
) -> (Vec<String>, Vec<TextRead>, HashMap<ContentIdentity, usize>) {
  let youngest = repository.youngest();
  let mut paths: Vec<String> = Vec::new();
  let mut path_index: HashMap<String, usize> = HashMap::new();
  let mut read: Vec<TextRead> = Vec::new();
  let mut position: HashMap<ContentIdentity, usize> = HashMap::new();
  for revision in 1..=youngest {
    for change in repository.index().changes(revision).unwrap() {
      let Some(text) = change.text else { continue };
      if position.contains_key(&text.identity) {
        continue;
      }
      let (base, delta) = repository
        .text_delta(&text.identity)
        .expect("every text a row names is kept as a delta");
      let path = *path_index.entry(change.path.clone()).or_insert_with(|| {
        paths.push(change.path.clone());
        paths.len() - 1
      });
      position.insert(text.identity, read.len());
      read.push(TextRead {
        path,
        revision,
        identity: text.identity,
        base,
        delta: delta.to_vec(),
        text_len: usize::try_from(text.size).unwrap(),
      });
    }
  }
  (paths, read, position)
}

fn main() {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let dump = &arguments[0];
  let arm_name = arguments[1].clone();
  let samples: usize = arguments[2].parse().unwrap();
  let dump_bytes = std::fs::metadata(dump).unwrap().len();

  let started = Instant::now();
  let repository = imported(dump);
  let import_s = started.elapsed().as_secs_f64();
  let youngest = repository.youngest();
  println!(
    "arm {arm_name}: dump {:.1} MiB, {youngest} revisions imported in {import_s:.1} s ({} texts made whole, deltas {:.1} MiB); rss {:.0} MiB",
    dump_bytes as f64 / 1048576.0,
    repository.texts_made_whole(),
    repository.text_bytes() as f64 / 1048576.0,
    rss_mib()
  );

  let started = Instant::now();
  let (paths, read, position) = read_every_text(&repository);
  let text_bytes: usize = read.iter().map(|r| r.text_len).sum();
  println!(
    "history: {} paths, {} texts, {:.1} MiB as whole texts, {:.1} MiB of deltas, read out in {:.1} s",
    paths.len(),
    read.len(),
    text_bytes as f64 / 1048576.0,
    read.iter().map(|r| r.delta.len()).sum::<usize>() as f64 / 1048576.0,
    started.elapsed().as_secs_f64()
  );

  // The arm's order: A path by path (each path's texts in revision order), B as read (revision, then its rows).
  let started = Instant::now();
  let mut order: Vec<usize> = (0..read.len()).collect();
  if arm_name == "A" {
    order.sort_by_key(|at| (read[*at].path, read[*at].revision));
  }
  let mut log: Vec<u8> = Vec::new();
  let mut revisions = vec![
    Stored {
      base: None,
      at: 0,
      len: 0,
      text: [0; 32],
      text_len: 0
    };
    read.len()
  ];
  for at in &order {
    let r = &read[*at];
    revisions[*at] = Stored {
      base: r.base.map(|base| position[&base]),
      at: log.len(),
      len: r.delta.len(),
      text: r.identity,
      text_len: r.text_len,
    };
    log.extend_from_slice(&r.delta);
  }
  let mut files: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
  for (at, r) in read.iter().enumerate() {
    files.entry(r.path).or_default().push(at);
  }
  let build_s = started.elapsed().as_secs_f64();
  drop(read);
  drop(repository);

  // The chains: how many deltas a read of each text applies (svn's checkpoint spacing is this bound).
  let mut depths: Vec<f64> = (0..revisions.len())
    .map(|at| {
      let mut depth = 1usize;
      let mut link = at;
      while let Some(base) = revisions[link].base {
        depth += 1;
        link = base;
      }
      depth as f64
    })
    .collect();
  println!(
    "arm {arm_name} built in {build_s:.2} s: log {:.1} MiB raw, {} texts; chain p50 {:.0} p99 {:.0} max {:.0}; rss {:.0} MiB ({:.2}x the whole texts, {:.2}x the dump)",
    log.len() as f64 / 1048576.0,
    revisions.len(),
    percentile(&mut depths.clone(), 0.5),
    percentile(&mut depths.clone(), 0.99),
    percentile(&mut depths, 1.0),
    rss_mib(),
    rss_mib() * 1048576.0 / text_bytes as f64,
    rss_mib() * 1048576.0 / dump_bytes as f64
  );

  let mut rng = 0x2026_1120_u64;
  let mut next = move || {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    rng
  };
  let sample: Vec<usize> = (0..samples)
    .map(|_| (next() % revisions.len() as u64) as usize)
    .collect();
  for codec in [
    Codec::Raw,
    Codec::Lz4,
    Codec::Zstd(1),
    Codec::Zstd(3),
    Codec::Zstd(9),
  ] {
    let started = Instant::now();
    let segments = Segments::of(&log, codec);
    let encode_s = started.elapsed().as_secs_f64();
    // Old reads: each sampled text rebuilt through its base chain, a fresh cache each read (a cold read).
    let mut micros = Vec::with_capacity(samples);
    let mut decodes = Vec::with_capacity(samples);
    let mut bytes = Vec::new();
    for at in &sample {
      let started = Instant::now();
      let mut cache = Cache::new(&segments);
      let (text, _) = rebuilt(&revisions, *at, &mut cache, &mut bytes);
      micros.push(started.elapsed().as_secs_f64() * 1e6);
      decodes.push(cache.decodes as f64);
      let wanted = revisions[*at];
      assert_eq!(
        (*blake3::hash(&text).as_bytes(), text.len()),
        (wanted.text, wanted.text_len),
        "text {at}"
      );
    }
    // A checkout's file section: every text path by path, in revision order, through one bounded cache.
    let started = Instant::now();
    let mut cache = Cache::new(&segments);
    let mut sent = 0usize;
    for texts in files.values() {
      for at in texts {
        let stored = revisions[*at];
        cache.read(stored.at, stored.len, &mut bytes);
        sent += bytes.len();
      }
    }
    let stream_s = started.elapsed().as_secs_f64();
    println!(
      "arm {arm_name} {codec:?}: held {:.1} MiB (encoded in {encode_s:.2} s); old read µs p50 {:.0} p99 {:.0} max {:.0}, \
       segments decoded p50 {:.0} p99 {:.0}; file section {:.1} MiB in {stream_s:.2} s, {} segment decodes; rss {:.0} MiB",
      segments.bytes() as f64 / 1048576.0,
      percentile(&mut micros.clone(), 0.5),
      percentile(&mut micros.clone(), 0.99),
      percentile(&mut micros, 1.0),
      percentile(&mut decodes.clone(), 0.5),
      percentile(&mut decodes, 0.99),
      sent as f64 / 1048576.0,
      cache.decodes,
      rss_mib()
    );
  }
}
