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
  let by_tools = store
    .texts_by_tools(1, b"f", &[b"n2".to_vec(), b"n9".to_vec()])
    .unwrap();
  let (by_tool, row) = by_tools[0].unwrap();
  assert_eq!((by_tool, row.base), (ids[1], Some(ids[0])));
  assert_eq!(store.place_bytes(&staging, &row.place).unwrap(), edit_two);
  assert_eq!(by_tools[1], None);
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

/// Goal 93 perf (r25 on 5d7e05b81, hg-src: a stream's store thread read each chunk with its own payload lookup and its
/// own `pread`, about 230,000 a stream; a later stream, the log out of the page cache, took 10.7 s against stock's
/// 0.47 s). A log's revisions lie in order in their push's payload, apart only by each chunk's header, so a batch of
/// their places is read in one read. Do: land a payload of three versions of `f` each after a short header, and a
/// fourth after a gap longer than the three together; ask for the four places in one batch. Expect each place's bytes
/// as read alone, and two log reads: the three near ones together, the far one alone.
#[test]
fn a_batch_of_places_in_order_in_one_payload_is_read_in_one_read() {
  let dir = scratch("coalesced");
  let mut store = open(&dir);
  let staging = Staging::new(0);
  let header = b"chunkhdr".to_vec();
  let versions: Vec<Vec<u8>> = (0..4)
    .map(|at| format!("version {at} of f, sixteen+").into_bytes())
    .collect();
  let gap = vec![b'.'; versions.iter().map(Vec::len).sum::<usize>() * 2];
  let mut payload = Vec::new();
  let mut places = Vec::new();
  for (at, version) in versions.iter().enumerate() {
    payload.extend_from_slice(if at == 3 { &gap } else { &header });
    places.push((payload.len(), version.len()));
    payload.extend_from_slice(version);
  }
  let payload_id = *blake3::hash(&payload).as_bytes();
  let places: Vec<Place> = places
    .iter()
    .map(|(offset, len)| Place {
      payload: payload_id,
      offset: *offset as u64,
      len: *len as u64,
    })
    .collect();
  let entries: Vec<TextEntry> = versions
    .iter()
    .zip(&places)
    .enumerate()
    .map(|(at, (version, place))| TextEntry {
      path: b"f".to_vec(),
      tool_id: format!("n{at}").into_bytes(),
      text: *blake3::hash(version).as_bytes(),
      len: version.len() as u64,
      base: None,
      place: *place,
    })
    .collect();
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
  let before = store.place_reads();
  let read = store.places_bytes(&staging, &places).unwrap();
  assert_eq!(read, versions);
  assert_eq!(store.place_reads() - before, 2, "log reads for the batch");
  let _ = std::fs::remove_dir_all(&dir);
}

/// Goal 93 perf (r27 on e0b677380, hg-src: a stream asked the store for every revision's row by node, about 230,000
/// lookups a stream, 1.35 s of its 3.7 s waiting on them). A log's tool id rows share a key prefix, so its versions are
/// read in one scan. Do: land one changeset holding three versions of `f` and one of `g`; ask for `f`'s three by their
/// tool ids, in an order of their own, with one no changeset named. Expect each version's identity in the order asked,
/// none for the stranger, `g`'s never among them, and one engine read for the four asked (`Store::tool_reads`).
#[test]
fn a_logs_versions_by_tool_id_are_read_in_one_scan() {
  let dir = scratch("by-tools");
  let mut store = open(&dir);
  let staging = Staging::new(0);
  let texts: Vec<(&[u8], &[u8], Vec<u8>)> = vec![
    (b"f", b"n1", b"one of f".to_vec()),
    (b"f", b"n2", b"two of f".to_vec()),
    (b"f", b"n3", b"three of f".to_vec()),
    (b"g", b"m1", b"one of g".to_vec()),
  ];
  let payload: Vec<u8> = texts.iter().flat_map(|(_, _, text)| text.clone()).collect();
  let payload_id = *blake3::hash(&payload).as_bytes();
  let mut offset = 0_u64;
  let entries: Vec<TextEntry> = texts
    .iter()
    .map(|(path, tool_id, text)| {
      let entry = TextEntry {
        path: path.to_vec(),
        tool_id: tool_id.to_vec(),
        text: *blake3::hash(text).as_bytes(),
        len: text.len() as u64,
        base: None,
        place: Place {
          payload: payload_id,
          offset,
          len: text.len() as u64,
        },
      };
      offset += text.len() as u64;
      entry
    })
    .collect();
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
  let before = store.tool_reads();
  let asked = [
    b"n3".to_vec(),
    b"n9".to_vec(),
    b"n1".to_vec(),
    b"n2".to_vec(),
  ];
  let found: Vec<Option<[u8; 32]>> = store
    .texts_by_tools(1, b"f", &asked)
    .unwrap()
    .into_iter()
    .map(|found| found.map(|(identity, _)| identity))
    .collect();
  let identity = |at: usize| Some(*blake3::hash(&texts[at].2).as_bytes());
  assert_eq!(found, vec![identity(2), None, identity(0), identity(1)]);
  assert_eq!(
    store.tool_reads() - before,
    1,
    "engine reads for the four asked"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// Lands repository 1's changeset `sequence` with a record, the one `payload`, and a `Texts` section naming `entries`,
/// each placed whole in the payload.
fn land_texts(store: &mut Store, sequence: u64, payload: &[u8], entries: &[(&str, &[u8], &[u8])]) {
  let staging = Staging::new(1 << 20);
  let entries: Vec<TextEntry> = entries
    .iter()
    .map(|(path, tool_id, text)| TextEntry {
      path: path.as_bytes().to_vec(),
      tool_id: tool_id.to_vec(),
      text: *blake3::hash(text).as_bytes(),
      len: text.len() as u64,
      base: None,
      place: Place {
        payload: *blake3::hash(payload).as_bytes(),
        offset: 0,
        len: payload.len() as u64,
      },
    })
    .collect();
  let section = texts::encode(&entries);
  store
    .land(
      Header {
        tool: Tool::Mercurial,
        repository: 1,
        sequence,
        judged_over: sequence - 1,
        grant: 1,
        work: 0,
        time_ns: sequence,
        sections: 0,
      },
      &[
        (SectionKind::Record, Contents::Bytes(b"segment")),
        (SectionKind::Payload, Contents::Bytes(payload)),
        (SectionKind::Texts, Contents::Bytes(&section)),
      ],
      &staging,
    )
    .unwrap();
}

/// Main 01a20182's ruling of 2026-11-22 12:00 (an abandoned push's run: its rows forgotten at its abandonment, counted
/// and bounded, never another's). Do: land changeset 1 keeping texts `x` at `p` (tool id `n1`) and `y` at `p` (`n2`),
/// then changeset 2 keeping `x` again, a copy at `q` (`m1`), as a named push that brought the same text after it;
/// forget changeset 1; reopen the store (its recovery). Expect the rows forgotten counted; changeset 1 gone from the
/// log's rows and its payload no longer read; `x`'s text row moved to changeset 2, as its own place; `y`'s text row
/// and `p`'s versions gone; `q`'s version intact; all of it so after the recovery; and forgetting it again forgets
/// nothing. Red before: no way to forget a changeset's rows.
#[test]
fn a_forgotten_changesets_rows_go_and_a_text_another_kept_moves_to_it() {
  let dir = scratch("forget");
  let (x, y) = (
    b"the text both kept\n".as_slice(),
    b"the text only the forgotten kept\n".as_slice(),
  );
  let (first, second) = (
    b"the forgotten changeset's payload".as_slice(),
    b"the named changeset's payload".as_slice(),
  );
  let id = |bytes: &[u8]| *blake3::hash(bytes).as_bytes();
  let look = |store: &mut Store| {
    let staging = Staging::new(0);
    (
      store
        .changesets(1, 0, 10)
        .unwrap()
        .iter()
        .map(|row| row.sequence)
        .collect::<Vec<_>>(),
      store
        .piece(&staging, &id(first), first.len() as u64, 0)
        .unwrap()
        .is_some(),
      store
        .text_row(1, &id(x))
        .unwrap()
        .map(|row| (row.sequence, row.place.payload)),
      store.text_row(1, &id(y)).unwrap(),
      store
        .texts_by_tools(1, b"p", &[b"n1".to_vec(), b"n2".to_vec()])
        .unwrap(),
      store
        .texts_by_tools(1, b"q", &[b"m1".to_vec()])
        .unwrap()
        .into_iter()
        .map(|found| found.map(|(text, row)| (text, row.sequence)))
        .collect::<Vec<_>>(),
    )
  };
  let forgotten = {
    let mut store = open(&dir);
    land_texts(&mut store, 1, first, &[("p", b"n1", x), ("p", b"n2", y)]);
    land_texts(&mut store, 2, second, &[("q", b"m1", x)]);
    assert_eq!(
      store.text_row(1, &id(x)).unwrap().map(|row| row.sequence),
      Some(1)
    );
    let forgotten = store.forget(1, 1).unwrap();
    assert_eq!(store.forget(1, 1).unwrap(), 0, "nothing left to forget");
    forgotten
  };
  let mut store = open(&dir);
  let seen = look(&mut store);
  assert!(forgotten >= 6, "{forgotten} rows forgotten or moved");
  assert_eq!(seen.0, vec![2], "changeset 1's row forgotten");
  assert!(!seen.1, "its payload no longer read");
  assert_eq!(
    seen.2,
    Some((2, id(second))),
    "x's text row moved to changeset 2"
  );
  assert_eq!(seen.3, None, "y's text row forgotten");
  assert_eq!(seen.4, vec![None, None], "p's versions forgotten");
  assert_eq!(seen.5, vec![Some((id(x), 2))], "q's version intact");
}

/// Regression (Wave 6F): asking for one or a few tool ids in a large log must use O(asked) point
/// lookups instead of scanning the entire path prefix a few rows per scan. Do: land 400 versions
/// of `f`, then ask for one late tool id and for three scattered tool ids (including one absent).
/// Expect: exact matching identities and `tool_reads()` bounded by the asked count, not the log's
/// length.
#[test]
fn sparse_tool_id_lookups_do_not_scan_the_whole_log() {
  let dir = scratch("sparse-by-tools");
  let mut store = open(&dir);
  let staging = Staging::new(0);
  let count = 400_u32;
  let mut payload = Vec::new();
  let mut entries = Vec::with_capacity(count as usize);
  let mut ids = Vec::with_capacity(count as usize);
  for i in 0..count {
    let text = format!("revision payload {i:04}\n").into_bytes();
    let offset = payload.len() as u64;
    payload.extend_from_slice(&text);
    let mut tool_id = [0_u8; 20];
    tool_id[..4].copy_from_slice(&i.to_be_bytes());
    let text_id = *blake3::hash(&text).as_bytes();
    ids.push((tool_id.to_vec(), text_id));
    entries.push(TextEntry {
      path: b"f".to_vec(),
      tool_id: tool_id.to_vec(),
      text: text_id,
      len: text.len() as u64,
      base: None,
      place: Place {
        payload: [0; 32],
        offset,
        len: text.len() as u64,
      },
    });
  }
  let payload_id = *blake3::hash(&payload).as_bytes();
  for entry in &mut entries {
    entry.place.payload = payload_id;
  }
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

  let before_one = store.tool_reads();
  let target = &ids[350];
  let got_one = store
    .texts_by_tools(1, b"f", core::slice::from_ref(&target.0))
    .unwrap();
  assert_eq!(got_one.len(), 1);
  assert_eq!(got_one[0].map(|(id, _)| id), Some(target.1));
  let reads_one = store.tool_reads() - before_one;
  assert_eq!(
    reads_one, 1,
    "single tool id lookup must take 1 engine read, took {reads_one}"
  );

  let before_three = store.tool_reads();
  let missing = vec![0xFF; 20];
  let asked = vec![
    ids[380].0.clone(),
    missing,
    ids[10].0.clone(),
    ids[200].0.clone(),
  ];
  let got_three = store.texts_by_tools(1, b"f", &asked).unwrap();
  let got_ids: Vec<Option<[u8; 32]>> = got_three
    .into_iter()
    .map(|row| row.map(|(id, _)| id))
    .collect();
  assert_eq!(
    got_ids,
    vec![Some(ids[380].1), None, Some(ids[10].1), Some(ids[200].1)]
  );
  let reads_three = store.tool_reads() - before_three;
  assert!(
    reads_three <= asked.len() as u64,
    "sparse 4-id lookup must take at most {} engine reads, took {reads_three}",
    asked.len()
  );
  let _ = std::fs::remove_dir_all(&dir);
}
