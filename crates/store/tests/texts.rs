//! The derived per-path read form (A-645 on A-767; main's order of 2026-11-20 18:30, the interface agreed with w-hg and
//! w-store): an old file revision read from its own chain's changesets alone, before and after a recovery. The
//! directories are real ones in the build output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1).
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::indexing_slicing,
  clippy::cast_possible_truncation
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::staging::Staging;
use slates_store::store::{Contents, Sizing, Store};
use slates_store::texts::{self, Place, TextCache, TextEntry};

fn scratch(name: &str) -> PathBuf {
  let dir =
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("texts-{name}-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  dir.canonicalize().unwrap()
}

fn open(dir: &Path) -> Store {
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(dir).unwrap(),
    issued_ns: 0,
  };
  let root = StoreRoot::open(dir, &grant).unwrap();
  Store::open(
    root,
    Sizing {
      memory: 8 << 20,
      disk: 1 << 30,
      extent_bytes: 128 << 10,
      fanout: 8,
    },
  )
  .unwrap()
}

/// A hunk replacing `start..end` of the base with `data` (Mercurial's `mpatch` form).
fn hunk(start: u32, end: u32, data: &[u8]) -> Vec<u8> {
  [
    start.to_be_bytes().as_slice(),
    &end.to_be_bytes(),
    &(data.len() as u32).to_be_bytes(),
    data,
  ]
  .concat()
}

/// The repository's history, kept beside the store as the oracle: every version of each path, and each version's
/// chain of changesets (the sequences its read must read, and no other).
struct History {
  texts: BTreeMap<(String, u64), Vec<u8>>,
  chains: BTreeMap<[u8; 32], BTreeSet<u64>>,
}

/// Lands `changesets` changesets of repository 1, the `n`th editing path `n % 3` (a whole at its first version, then
/// ten bytes replaced a version), each with a record, a payload of its edit or whole, and its `Texts` section.
/// Each path's current text: its bytes, its identity, and the sequences of the changesets its chain spans.
type CurrentTexts<'p> = BTreeMap<&'p str, (Vec<u8>, [u8; 32], BTreeSet<u64>)>;

fn land_history(store: &mut Store, changesets: u64) -> History {
  let paths = ["a.txt", "b.txt", "c.txt"];
  let staging = Staging::new(1 << 20);
  let mut current: CurrentTexts<'_> = BTreeMap::new();
  let mut history = History {
    texts: BTreeMap::new(),
    chains: BTreeMap::new(),
  };
  for sequence in 1..=changesets {
    let path = paths[(sequence % 3) as usize];
    let (payload, text, base, chain) = match current.get(path) {
      None => {
        let text: Vec<u8> = (0..2_000u32)
          .map(|at| (at % 251) as u8 ^ path.as_bytes()[0])
          .collect();
        (text.clone(), text, None, BTreeSet::from([sequence]))
      }
      Some((before, identity, chain)) => {
        let at = ((sequence * 37) % 1_990) as u32;
        let data = [sequence as u8; 10];
        let mut text = before.clone();
        text[at as usize..at as usize + 10].copy_from_slice(&data);
        let mut chain = chain.clone();
        chain.insert(sequence);
        (hunk(at, at + 10, &data), text, Some(*identity), chain)
      }
    };
    let identity = *blake3::hash(&text).as_bytes();
    let entry = TextEntry {
      path: path.as_bytes().to_vec(),
      tool_id: Vec::new(),
      text: identity,
      len: text.len() as u64,
      base,
      place: Place {
        payload: *blake3::hash(&payload).as_bytes(),
        offset: 0,
        len: payload.len() as u64,
      },
    };
    let section = texts::encode(&[entry]);
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
          (SectionKind::Record, Contents::Bytes(b"heads")),
          (SectionKind::Payload, Contents::Bytes(&payload)),
          (SectionKind::Texts, Contents::Bytes(&section)),
        ],
        &staging,
      )
      .unwrap();
    history
      .texts
      .insert((path.to_owned(), sequence), text.clone());
    history.chains.insert(identity, chain.clone());
    current.insert(path, (text, identity, chain));
  }
  history
}

/// Reads the version of `path` that changeset `sequence` kept, on a cold cache: its bytes, and the changesets read.
fn read(
  store: &mut Store,
  history: &History,
  path: &str,
  sequence: u64,
) -> (Vec<u8>, BTreeSet<u64>) {
  let text = &history.texts[&(path.to_owned(), sequence)];
  let identity = *blake3::hash(text).as_bytes();
  let staging = Staging::new(0);
  let mut cache = TextCache::new(1 << 20);
  let bytes = store
    .text_at(&staging, 1, &identity, 0, &mut cache)
    .unwrap()
    .unwrap()
    .to_vec();
  (bytes, cache.changesets)
}

/// A-645 on A-767, main's red test (2026-11-20 18:30): "an old file revision read without rebuilding any changeset it
/// isn't in". Do: land 39 changesets, each editing one of three paths; read `a.txt`'s first, an early and its latest
/// versions on a cold cache, and the path's history; reopen the store (its rows recovered from the log) and read again.
/// Expect: each version's bytes, the changesets read exactly the version's chain (none that touched another path, none
/// after it), the history in sequence order, and the same after the reopen.
#[test]
fn an_old_file_revision_is_read_from_its_own_chain_alone() {
  let dir = scratch("chain");
  let mut store = open(&dir);
  let history = land_history(&mut store, 39);
  let a_versions: Vec<u64> = (1..=39).filter(|sequence| sequence % 3 == 0).collect();
  let check = |store: &mut Store| {
    for sequence in [a_versions[0], a_versions[3], *a_versions.last().unwrap()] {
      let (bytes, read) = read(store, &history, "a.txt", sequence);
      let text = &history.texts[&("a.txt".to_owned(), sequence)];
      assert_eq!(&bytes, text, "a.txt at {sequence}");
      let chain = &history.chains[blake3::hash(text).as_bytes()];
      assert_eq!(
        &read, chain,
        "a.txt at {sequence}: only its chain's changesets read"
      );
    }
    let listed: Vec<u64> = store
      .path_texts(1, b"a.txt", 0, 100)
      .unwrap()
      .into_iter()
      .map(|(sequence, _)| sequence)
      .collect();
    assert_eq!(listed, a_versions);
  };
  check(&mut store);
  drop(store);
  let mut store = open(&dir);
  check(&mut store);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A text that comes back (A, then B, then A again, as a file reverted is): its `T` row is the first changeset's that
/// kept it and is never replaced, so no chain loops. Do: land A whole, B as an edit of A, A again as an edit of B; read
/// each on a cold cache. Expect: each version's bytes, A read from its first changeset alone, and the path's history
/// naming all three.
#[test]
fn a_text_that_comes_back_keeps_its_first_row_and_no_chain_loops() {
  let dir = scratch("revert");
  let mut store = open(&dir);
  let staging = Staging::new(0);
  let a: Vec<u8> = b"the first text".to_vec();
  let b: Vec<u8> = b"the other text".to_vec();
  let (id_a, id_b) = (*blake3::hash(&a).as_bytes(), *blake3::hash(&b).as_bytes());
  let edit_to_b = hunk(4, 9, b"other");
  let edit_to_a = hunk(4, 9, b"first");
  for (sequence, (payload, text, base)) in (1u64..).zip([
    (a.clone(), id_a, None),
    (edit_to_b, id_b, Some(id_a)),
    (edit_to_a, id_a, Some(id_b)),
  ]) {
    let entry = TextEntry {
      path: b"f".to_vec(),
      tool_id: Vec::new(),
      text,
      len: 14,
      base,
      place: Place {
        payload: *blake3::hash(&payload).as_bytes(),
        offset: 0,
        len: payload.len() as u64,
      },
    };
    let section = texts::encode(&[entry]);
    store
      .land(
        Header {
          tool: Tool::Git,
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
  for (identity, text, chain) in [
    (id_a, &a, BTreeSet::from([1])),
    (id_b, &b, BTreeSet::from([1, 2])),
  ] {
    let mut cache = TextCache::new(0);
    let read = store
      .text_at(&staging, 1, &identity, 0, &mut cache)
      .unwrap()
      .map(<[u8]>::to_vec);
    assert_eq!(read.as_ref(), Some(text));
    assert_eq!(cache.changesets, chain);
  }
  let history: Vec<u64> = store
    .path_texts(1, b"f", 0, 10)
    .unwrap()
    .into_iter()
    .map(|(sequence, _)| sequence)
    .collect();
  assert_eq!(history, [1, 2, 3]);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A push lands as one changeset holding several versions of one path (each changeset of the push its own version):
/// the path's history names every one, in the order the section lists them. Do: land one changeset whose `Texts`
/// section holds three versions of `f` (a whole, then two edits); list the path's history and read each version. Expect:
/// three entries, all at the changeset's sequence, in order, each read back.
#[test]
fn a_changeset_holding_several_versions_of_a_path_lists_them_all() {
  let dir = scratch("several");
  let mut store = open(&dir);
  let staging = Staging::new(0);
  let first = b"version one of f".to_vec();
  let second = b"version two of f".to_vec();
  let third = b"version 333 of f".to_vec();
  let (edit_two, edit_three) = (hunk(8, 11, b"two"), hunk(8, 11, b"333"));
  let payload = [first.as_slice(), &edit_two, &edit_three].concat();
  let payload_id = *blake3::hash(&payload).as_bytes();
  let ids: Vec<[u8; 32]> = [&first, &second, &third]
    .iter()
    .map(|text| *blake3::hash(text).as_bytes())
    .collect();
  let place = |offset: usize, len: usize| Place {
    payload: payload_id,
    offset: offset as u64,
    len: len as u64,
  };
  let entries = vec![
    TextEntry {
      path: b"f".to_vec(),
      tool_id: b"n1".to_vec(),
      text: ids[0],
      len: 16,
      base: None,
      place: place(0, first.len()),
    },
    TextEntry {
      path: b"f".to_vec(),
      tool_id: b"n2".to_vec(),
      text: ids[1],
      len: 16,
      base: Some(ids[0]),
      place: place(first.len(), edit_two.len()),
    },
    TextEntry {
      path: b"f".to_vec(),
      tool_id: b"n3".to_vec(),
      text: ids[2],
      len: 16,
      base: Some(ids[1]),
      place: place(first.len() + edit_two.len(), edit_three.len()),
    },
  ];
  let section = texts::encode(&entries);
  store
    .land(
      Header {
        tool: Tool::Mercurial,
        repository: 1,
        sequence: 1,
        judged_over: 0,
        grant: 1,
        work: 1,
        time_ns: 1,
        sections: 0,
      },
      &[
        (SectionKind::Payload, Contents::Bytes(&payload)),
        (SectionKind::Texts, Contents::Bytes(&section)),
      ],
      &staging,
    )
    .unwrap();
  let history = store.path_texts(1, b"f", 0, 10).unwrap();
  assert_eq!(history, vec![(1, ids[0]), (1, ids[1]), (1, ids[2])]);
  // Each version found by its tool's id (Mercurial's node), with its place: what a stream sends as its chunk.
  let (by_tool, row) = store.text_by_tool(1, b"n2").unwrap().unwrap();
  assert_eq!((by_tool, row.base), (ids[1], Some(ids[0])));
  assert_eq!(store.place_bytes(&staging, &row.place).unwrap(), edit_two);
  assert_eq!(store.text_by_tool(1, b"n9").unwrap(), None);
  for (identity, text) in ids.iter().zip([&first, &second, &third]) {
    let mut cache = TextCache::new(0);
    let read = store
      .text_at(&staging, 1, identity, 0, &mut cache)
      .unwrap()
      .map(<[u8]>::to_vec);
    assert_eq!(read.as_ref(), Some(text));
  }
  let _ = std::fs::remove_dir_all(&dir);
}
