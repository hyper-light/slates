//! Golden tests of the objects (A-588; AC-HG.2): every revision one repository's history stored through stock
//! Mercurial 7.2 (`fixtures/objects`), each log listed by 7.2's `debugindex --debug` and each text written by its
//! `debugdata`: each node computed from its parents and text, each changeset, manifest and file revision read as 7.2
//! reads it, and each written back to its stored bytes.

use crate::fixtures::{
  hex,
  objects::{file, names},
};
use crate::manifest::Flag;
use crate::{Admission, Node, changeset, filerev, manifest, node};

/// A log's listing file and the prefix of its revisions' text files.
const CHANGELOG: (&str, &str) = ("index-changelog.txt", "changeset");

/// The manifest log's.
const MANIFEST_LOG: (&str, &str) = ("index-manifest.txt", "manifest");

/// The prefix of a file log's listing.
const FILE_LOG_PREFIX: &str = "index-file-";

/// One revision as 7.2's `debugindex --debug` listed it, with the text its `debugdata` wrote.
struct Stored {
  /// Its number in its log.
  rev: usize,
  /// The changeset that introduced it (`linkrev`).
  link: usize,
  /// Its node.
  node: Node,
  /// Its first parent's node.
  p1: Node,
  /// Its second parent's node.
  p2: Node,
  /// Its text's length as 7.2's index records it.
  full_size: usize,
  /// Its index flags.
  flags: u16,
  /// Its text.
  text: &'static [u8],
}

/// Bytes from hexadecimal, `-` for none.
fn unhex(text: &str) -> Vec<u8> {
  if text == "-" {
    return Vec::new();
  }
  let (pairs, _) = text.as_bytes().as_chunks::<2>();
  pairs
    .iter()
    .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect()
}

/// A node from its hexadecimal.
fn to_node(text: &str) -> Node {
  Node::try_from(unhex(text).as_slice()).unwrap()
}

/// Every revision of the log `listing` names, its texts in the files `<prefix>-<rev>.txt`: each row's columns found by
/// the header's names, every row as wide as the header.
fn log(listing: &str, prefix: &str) -> Vec<Stored> {
  let text = core::str::from_utf8(file(listing)).unwrap();
  let mut lines = text.lines();
  let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
  let column = |name: &str| header.iter().position(|known| *known == name).unwrap();
  let (rev, link, node, p1, p2, size, flags) = (
    column("rev"),
    column("linkrev"),
    column("nodeid"),
    column("p1-nodeid"),
    column("p2-nodeid"),
    column("full-size"),
    column("flags"),
  );
  lines
    .map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      assert_eq!(fields.len(), header.len(), "{listing}: {line}");
      Stored {
        rev: fields[rev].parse().unwrap(),
        link: fields[link].parse().unwrap(),
        node: to_node(fields[node]),
        p1: to_node(fields[p1]),
        p2: to_node(fields[p2]),
        full_size: fields[size].parse().unwrap(),
        flags: fields[flags].parse().unwrap(),
        text: file(&format!("{prefix}-{}.txt", fields[rev])),
      }
    })
    .collect()
}

/// Every file log: each path and its revisions.
fn file_logs() -> Vec<(String, Vec<Stored>)> {
  names()
    .filter_map(|name| name.strip_prefix(FILE_LOG_PREFIX)?.strip_suffix(".txt"))
    .map(|path| {
      let listing = format!("{FILE_LOG_PREFIX}{path}.txt");
      (path.to_string(), log(&listing, &format!("file-{path}")))
    })
    .collect()
}

/// The lines of `expected.txt` of one kind, split at spaces.
fn expected(kind: &str) -> Vec<Vec<String>> {
  let text = core::str::from_utf8(file("expected.txt")).unwrap();
  text
    .lines()
    .map(|line| line.split(' ').map(str::to_string).collect::<Vec<_>>())
    .filter(|fields| fields[0] == kind)
    .collect()
}

/// A changeset's extras as 7.2 reads them: its own, the later of a key given twice, with `branch` `default` where it
/// names no branch, sorted by key.
fn extras(read: &changeset::Changeset<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
  let mut buffer = Vec::new();
  let mut found = std::collections::BTreeMap::new();
  found.insert(b"branch".to_vec(), b"default".to_vec());
  for (key, value) in read
    .extras(&mut Admission::new(1 << 20), &mut buffer)
    .unwrap()
  {
    found.insert(key.to_vec(), value.to_vec());
  }
  found.into_iter().collect()
}

/// The extras `expected.txt` lists for a changeset: `key:value` pairs in hexadecimal.
fn expected_extras(field: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
  field
    .split(',')
    .filter(|pair| *pair != "-")
    .map(|pair| {
      let (key, value) = pair.split_once(':').unwrap();
      (
        unhex(key),
        if value.is_empty() {
          Vec::new()
        } else {
          unhex(value)
        },
      )
    })
    .collect()
}

/// A-588 (golden; AC-HG.2): do compute the node of every revision 7.2 stored, from the parents its `debugindex` lists
/// and the text its `debugdata` wrote (a file revision's metadata block included). Expect each node 7.2 gave it, each
/// text as long as 7.2's index says, no flag set, and every log present: eleven changesets, nine manifests, and ten
/// file revisions in eight file logs.
#[test]
fn every_revision_hashes_to_its_node() {
  let mut logs = vec![
    ("changelog".to_string(), log(CHANGELOG.0, CHANGELOG.1)),
    ("manifest".to_string(), log(MANIFEST_LOG.0, MANIFEST_LOG.1)),
  ];
  let files = file_logs();
  assert_eq!(files.len(), 8);
  logs.extend(files);
  let counts: Vec<usize> = logs.iter().map(|(_, revisions)| revisions.len()).collect();
  assert_eq!(counts[..2], [11, 9]);
  assert_eq!(counts[2..].iter().sum::<usize>(), 10);
  for (name, revisions) in &logs {
    for (position, stored) in revisions.iter().enumerate() {
      assert_eq!(stored.rev, position, "{name}");
      assert_eq!(stored.text.len(), stored.full_size, "{name} {}", stored.rev);
      assert_eq!(stored.flags, 0, "{name} {}", stored.rev);
      assert_eq!(
        node::check(&stored.node, &stored.p1, &stored.p2, stored.text),
        Ok(()),
        "{name} {}",
        stored.rev
      );
    }
  }
}

/// A-588 (golden): do read every changeset 7.2 stored. Expect the manifest, user, time, timezone, description, files
/// and extras 7.2 reads from each: a branch's empty file list, the timezone of `-3600`, the description of several
/// lines, the closed head's `close`, the user beyond ASCII, and the extras holding every escaped byte; and each
/// changeset its own link.
#[test]
fn every_changeset_reads_as_mercurial_reads_it() {
  let changesets = log(CHANGELOG.0, CHANGELOG.1);
  let wanted = expected("changeset");
  assert_eq!(changesets.len(), wanted.len());
  for (stored, fields) in changesets.iter().zip(wanted) {
    assert_eq!((stored.link, stored.rev.to_string()), (stored.rev, fields[1].clone()));
    let read = changeset::decode(stored.text).unwrap();
    assert_eq!(read.manifest_node(), Ok(to_node(&fields[2])), "{}", stored.rev);
    assert_eq!(
      (read.user, read.time, read.timezone),
      (
        unhex(&fields[3]).as_slice(),
        fields[4].as_bytes(),
        fields[5].as_bytes()
      )
    );
    assert_eq!(read.description, unhex(&fields[6]).as_slice());
    let files: Vec<Vec<u8>> = read.file_names().map(<[u8]>::to_vec).collect();
    let touched: Vec<Vec<u8>> = fields[7]
      .split(',')
      .filter(|name| *name != "-")
      .map(unhex)
      .collect();
    assert_eq!(files, touched);
    assert_eq!(extras(&read), expected_extras(&fields[8]), "{}", stored.rev);
  }
}

/// A-588 (golden): do write every changeset 7.2 stored back from what was read of it. Expect its stored bytes, at the
/// length `encoded_len` gave.
#[test]
fn every_changeset_writes_back_to_its_bytes() {
  for stored in log(CHANGELOG.0, CHANGELOG.1) {
    let read = changeset::decode(stored.text).unwrap();
    let mut out = Vec::new();
    changeset::encode(&read, &mut out).unwrap();
    assert_eq!(out, stored.text, "{}", stored.rev);
    assert_eq!(changeset::encoded_len(&read), Ok(u64::try_from(stored.full_size).unwrap()));
  }
}

/// The letter `expected.txt` writes for a flag.
fn flag_letter(flag: Flag) -> &'static str {
  match flag {
    Flag::Regular => "-",
    Flag::Executable => "x",
    Flag::Link => "l",
  }
}

/// A-588 (golden): do read every changeset's manifest 7.2 stored, found by the node the changeset names, and write it
/// back. Expect the paths, file nodes and flags (`x`, `l`) 7.2 reads, in order, each file node one its path's log
/// holds, and the stored bytes.
#[test]
fn every_manifest_reads_as_mercurial_reads_it() {
  let manifests = log(MANIFEST_LOG.0, MANIFEST_LOG.1);
  let files = file_logs();
  let entries = expected("entry");
  for fields in expected("changeset") {
    let stored = manifests
      .iter()
      .find(|revision| hex(&revision.node) == fields[2])
      .unwrap();
    let read: Vec<manifest::Entry<'_>> = manifest::decode(stored.text).unwrap().collect();
    let found: Vec<(Vec<u8>, Node, String)> = read
      .iter()
      .map(|entry| (entry.path.to_vec(), entry.node, flag_letter(entry.flag).to_string()))
      .collect();
    let wanted: Vec<(Vec<u8>, Node, String)> = entries
      .iter()
      .filter(|entry| entry[1] == fields[1])
      .map(|entry| (unhex(&entry[2]), to_node(&entry[3]), entry[4].clone()))
      .collect();
    assert_eq!(found, wanted, "changeset {}", fields[1]);
    for entry in &read {
      let (_, revisions) = files
        .iter()
        .find(|(path, _)| path.as_bytes() == entry.path)
        .unwrap();
      assert!(revisions.iter().any(|revision| revision.node == entry.node));
    }
    let mut out = Vec::new();
    manifest::encode(&read, &mut out).unwrap();
    assert_eq!(out, stored.text);
    assert_eq!(manifest::encoded_len(&read), Ok(u64::try_from(stored.full_size).unwrap()));
  }
}

/// A-588 (golden): do split every file revision 7.2 stored, read its copy with the first parent 7.2's index lists,
/// write the split back, pack its entries again, and order its parents as 7.2 stores them. Expect the copy 7.2's
/// `renamed` reads (`c` copied from `a` at `a`'s second node, and nothing else), the empty block 7.2 wrote before bytes
/// that begin as one does, the stored bytes from both writers, and the parents as 7.2's index lists them.
#[test]
fn every_file_revision_keeps_its_metadata_apart() {
  let files = file_logs();
  let wanted = expected("filerev");
  assert_eq!(wanted.len(), 10);
  for fields in wanted {
    let path = unhex(&fields[1]);
    let (_, revisions) = files
      .iter()
      .find(|(name, _)| name.as_bytes() == path)
      .unwrap();
    let stored = &revisions[fields[2].parse::<usize>().unwrap()];
    let read = filerev::split(stored.text).unwrap();
    let copy = read
      .copy(&stored.p1)
      .unwrap()
      .map(|copy| (copy.path.to_vec(), copy.node));
    let copied = (fields[3] != "-").then(|| (unhex(&fields[3]), to_node(&fields[4])));
    assert_eq!(copy, copied, "{}", fields[1]);
    let mut again = Vec::new();
    read.encode(&mut again).unwrap();
    assert_eq!(again, stored.text);
    let entries: Vec<(&[u8], &[u8])> = read.entries().collect();
    let mut packed = Vec::new();
    filerev::pack(&entries, read.bytes, &mut packed).unwrap();
    assert_eq!(packed, stored.text);
    assert_eq!(read.parents_as_stored(stored.p1, stored.p2), (stored.p1, stored.p2));
  }
  let meta = filerev::split(file("file-meta-0.txt")).unwrap();
  assert_eq!(meta.metadata, Some(&[][..]));
  assert!(meta.bytes.starts_with(filerev::MARKER));
}
