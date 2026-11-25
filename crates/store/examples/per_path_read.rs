//! The derived per-path read form, measured on Mercurial's own history (A-645 on A-767; main's order of 2026-11-20 18:30:
//! "an old file revision read without rebuilding any changeset it isn't in, within the per-file arm's 91 µs").
//!
//! The history is landed into a store as changesets: one a changelog revision, holding every file revision that
//! revision introduced (its link), each its revlog chunk as Mercurial stored it (a whole, or its `mpatch` hunks against
//! its recorded delta base, the edit form A-767 keeps), the chunks one payload section and a `Texts` section naming each
//! text's identity, length, base and place. No diff is made: the deltas are Mercurial's own, as both arms of
//! `slates-hg`'s `changeset_ab` hold them. Then `samples` file revisions, drawn as `changeset_ab` draws them (xorshift,
//! seed 0x2026_1120), are each read whole on a cold cache (`Store::text_at`), checked against their identity, timed, and
//! counted: the changesets each read opened, which must be its own chain's alone.
//!
//! Command: `cargo run --release -p slates-store --example per_path_read -- <hg repository> <store dir> <samples>`. The
//! store directory must be empty or absent; it is written under a grant made here for it.

#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation,
  clippy::cast_precision_loss,
  clippy::cast_sign_loss,
  clippy::print_stdout,
  clippy::disallowed_methods
)]

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use slates_hg::Admission;
use slates_hg::revlog::Revlog;
use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::staging::Staging;
use slates_store::store::{Contents, Sizing, Store};
use slates_store::texts::{self, Place, TextCache, TextEntry};

/// Shape: the room each revlog read is admitted (the whole history fits).
const ROOM: u64 = 1 << 34;
/// Shape: the memory the store's engine is lent (its rows: two a file revision).
const ENGINE_MEMORY: usize = 256 << 20;
/// Shape: the disk the store may grow to.
const DISK: u64 = 1 << 34;
/// Shape: a cold read's cache: room for one chain's texts, as a reader keeps for one read.
const CACHE_BYTES: u64 = 64 << 20;

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

fn percentile(values: &mut [f64], p: f64) -> f64 {
  values.sort_by(f64::total_cmp);
  values[((values.len() as f64 - 1.0) * p).round() as usize]
}

/// One file revision as read from its revlog: its path, link, chunk, base (by its revision), and text's identity.
struct Read {
  path: usize,
  rev: u32,
  link: u32,
  base: Option<u32>,
  chunk: Vec<u8>,
  text: [u8; 32],
  len: u64,
}

/// The file revisions of `read` by the changeset (link) that made them, in link order.
fn grouped_by_link(read: &[Read]) -> BTreeMap<u32, Vec<usize>> {
  let mut by_link: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
  for (at, r) in read.iter().enumerate() {
    by_link.entry(r.link).or_default().push(at);
  }
  by_link
}

/// Every tracked path of the repository at `root`, read from its store's fncache (each `data/<path>.i`).
fn tracked_paths(root: &std::path::Path) -> Vec<Vec<u8>> {
  let fncache = std::fs::read(root.join(".hg/store/fncache")).unwrap();
  fncache
    .split(|byte| *byte == b'\n')
    .filter_map(|line| {
      line
        .strip_prefix(b"data/")?
        .strip_suffix(b".i")
        .map(<[u8]>::to_vec)
    })
    .collect()
}

/// Every file revision of every one of `paths` read once from `source`: its chunk, link, base, and its whole text's
/// identity and length.
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
        len: text.len() as u64,
      });
    }
  }
  read
}

fn main() {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let root = std::path::PathBuf::from(&arguments[0]);
  let store_dir = std::path::PathBuf::from(&arguments[1]);
  let samples: usize = arguments[2].parse().unwrap();
  // The changesets to land, from the first: all of them unless a fourth argument bounds them (a quick run).
  let landing: usize = arguments
    .get(3)
    .map_or(usize::MAX, |count| count.parse().unwrap());
  let requires = std::fs::read_to_string(root.join(".hg/requires")).unwrap_or_default();
  let source = Source {
    dotencode: requires.lines().any(|line| line == "dotencode"),
    root: root.clone(),
  };
  let paths = tracked_paths(&root);

  // Every file revision read once, each text rebuilt and hashed (the intake's work, timed apart).
  let started = Instant::now();
  let read = read_every_revision(&source, &paths);
  let text_of: HashMap<(usize, u32), ([u8; 32], u64)> = read
    .iter()
    .map(|r| ((r.path, r.rev), (r.text, r.len)))
    .collect();
  println!(
    "source: {} paths, {} file revisions, {:.1} MiB of chunks, read in {:.1} s",
    paths.len(),
    read.len(),
    read.iter().map(|r| r.chunk.len()).sum::<usize>() as f64 / 1_048_576.0,
    started.elapsed().as_secs_f64()
  );

  // Landed as changesets: by link, each its file revisions' chunks as one payload and their Texts entries.
  let _ = std::fs::remove_dir_all(&store_dir);
  std::fs::create_dir_all(&store_dir).unwrap();
  let store_dir = store_dir.canonicalize().unwrap();
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(&store_dir).unwrap(),
    issued_ns: 0,
  };
  let mut store = Store::open(
    StoreRoot::open(&store_dir, &grant).unwrap(),
    Sizing {
      memory: ENGINE_MEMORY,
      disk: DISK,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap();
  let by_link = grouped_by_link(&read);
  let staging = Staging::new(0);
  let started = Instant::now();
  let mut sequence_of: Vec<u64> = vec![0; read.len()];
  let mut landed: Vec<usize> = Vec::new();
  // Each text's first landing: the row the store keeps for it (a text that comes back is made as it first was).
  let mut first: HashMap<[u8; 32], usize> = HashMap::new();
  for (sequence, (_, members)) in (1u64..).zip(by_link.iter().take(landing)) {
    landed.extend_from_slice(members);
    for at in members {
      first.entry(read[*at].text).or_insert(*at);
    }
    let mut payload = Vec::new();
    let mut places = Vec::with_capacity(members.len());
    for at in members {
      places.push((payload.len() as u64, read[*at].chunk.len() as u64));
      payload.extend_from_slice(&read[*at].chunk);
    }
    let payload_id = *blake3::hash(&payload).as_bytes();
    let entries: Vec<TextEntry> = members
      .iter()
      .zip(&places)
      .map(|(at, (offset, len))| {
        let r = &read[*at];
        TextEntry {
          path: paths[r.path].clone(),
          tool_id: Vec::new(),
          text: r.text,
          len: r.len,
          base: r.base.map(|base| text_of[&(r.path, base)].0),
          place: Place {
            payload: payload_id,
            offset: *offset,
            len: *len,
          },
          form: slates_store::texts::Form::Raw,
        }
      })
      .collect();
    for at in members {
      sequence_of[*at] = sequence;
    }
    let section = texts::encode(&entries);
    store
      .land(
        Header {
          tool: Tool::Mercurial,
          repository: 1,
          sequence,
          judged_over: sequence - 1,
          grant: 1,
          work: sequence,
          time_ns: sequence,
          sections: 0,
        },
        &[
          (SectionKind::Payload, Contents::Bytes(&payload)),
          (SectionKind::Texts, Contents::Bytes(&section)),
        ],
        &staging,
      )
      .unwrap();
  }
  println!(
    "landed {} changesets in {:.1} s",
    by_link.len().min(landing),
    started.elapsed().as_secs_f64()
  );

  // Old reads: each sampled file revision made whole on a cold cache, checked, timed, its changesets counted.
  let mut rng = 0x2026_1120_u64;
  let mut next = move || {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    rng
  };
  let picks: Vec<usize> = (0..samples)
    .map(|_| landed[(next() % landed.len() as u64) as usize])
    .collect();
  for round in 1..=3 {
    let mut micros = Vec::with_capacity(samples);
    let mut opened = Vec::with_capacity(samples);
    let mut outside = 0usize;
    for at in &picks {
      let r = &read[*at];
      let mut cache = TextCache::new(CACHE_BYTES);
      let started = Instant::now();
      let bytes = store
        .text_at(&staging, 1, &r.text, 0, &mut cache)
        .unwrap()
        .expect("a landed text");
      let elapsed = started.elapsed().as_secs_f64() * 1e6;
      assert_eq!(*blake3::hash(bytes).as_bytes(), r.text);
      micros.push(elapsed);
      opened.push(cache.changesets.len() as f64);
      // The chain the store keeps for the text: from its first landing down its bases' first landings. Every changeset
      // a read opened must be one of them.
      let mut chain = std::collections::BTreeSet::new();
      let mut identity = Some(r.text);
      while let Some(text) = identity {
        let index = first[&text];
        chain.insert(sequence_of[index]);
        identity = read[index]
          .base
          .map(|base| text_of[&(read[index].path, base)].0);
      }
      if !cache.changesets.is_subset(&chain) {
        outside += 1;
      }
    }
    println!(
      "round {round}: old reads ({samples}, cold cache): µs p50 {:.0} p99 {:.0} max {:.0}; changesets opened p50 {:.0} p99 {:.0}; reads that opened a changeset outside their chain: {outside}",
      percentile(&mut micros.clone(), 0.5),
      percentile(&mut micros.clone(), 0.99),
      percentile(&mut micros, 1.0),
      percentile(&mut opened.clone(), 0.5),
      percentile(&mut opened, 0.99),
    );
  }
}
