//! Golden tests of the objects (A-588): every revision one repository's history stored through stock Mercurial 7.2
//! (`fixtures/objects`): each node computed from its parents and text, each changeset, manifest and file revision read
//! as 7.2 reads it, and each written back to its stored bytes.

use crate::fixtures::{hex, objects::file};
use crate::manifest::Flag;
use crate::{Admission, Node, changeset, filerev, manifest, node};

/// One revision of the index 7.2's storage wrote: its log, its name in that log, its number, node, parents and file.
struct Stored {
  log: String,
  name: String,
  rev: String,
  node: Node,
  p1: Node,
  p2: Node,
  file: String,
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

/// Every revision `index.txt` lists.
fn stored() -> Vec<Stored> {
  let index = core::str::from_utf8(file("index.txt")).unwrap();
  index
    .lines()
    .map(|line| {
      let fields: Vec<&str> = line.split(' ').collect();
      Stored {
        log: fields[0].to_string(),
        name: fields[1].to_string(),
        rev: fields[2].to_string(),
        node: to_node(fields[3]),
        p1: to_node(fields[4]),
        p2: to_node(fields[5]),
        file: fields[6].to_string(),
      }
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

/// The stored revisions of one log.
fn of_log(log: &str) -> Vec<Stored> {
  stored()
    .into_iter()
    .filter(|revision| revision.log == log)
    .collect()
}

/// A changeset's extras as 7.2 reads them: its own, with `branch` `default` where it names no branch, sorted by key.
fn extras(read: &changeset::Changeset<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
  let mut buffer = Vec::new();
  let mut found: Vec<(Vec<u8>, Vec<u8>)> = read
    .extras(&mut Admission::new(1 << 20), &mut buffer)
    .unwrap()
    .into_iter()
    .map(|(key, value)| (key.to_vec(), value.to_vec()))
    .collect();
  if !found.iter().any(|(key, _)| key == b"branch") {
    found.push((b"branch".to_vec(), b"default".to_vec()));
  }
  found.sort();
  found
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

/// A-588 (golden): do compute the node of every revision 7.2 stored, from its parents and its text (a file revision's
/// metadata block included). Expect each node 7.2 gave it: eleven changesets, nine manifests and ten file revisions.
#[test]
fn every_revision_hashes_to_its_node() {
  let all = stored();
  assert_eq!(all.len(), 30);
  for revision in &all {
    let text = file(&revision.file);
    assert_eq!(
      node::check(&revision.node, &revision.p1, &revision.p2, text),
      Ok(()),
      "{} {} {}",
      revision.log,
      revision.name,
      revision.rev
    );
  }
}

/// A-588 (golden): do read every changeset 7.2 stored. Expect the manifest, user, time, timezone, description, files
/// and extras 7.2 reads from each: a branch's empty file list, the timezone of `-3600`, the description of several
/// lines, the closed head's `close`, the user beyond ASCII, and the extras holding every escaped byte.
#[test]
fn every_changeset_reads_as_mercurial_reads_it() {
  let changesets = of_log("changeset");
  for (revision, fields) in changesets.iter().zip(expected("changeset")) {
    let read = changeset::decode(file(&revision.file)).unwrap();
    assert_eq!(
      read.manifest_node(),
      Ok(to_node(&fields[2])),
      "{}",
      revision.rev
    );
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
    let wanted: Vec<Vec<u8>> = fields[7]
      .split(',')
      .filter(|name| *name != "-")
      .map(unhex)
      .collect();
    assert_eq!(files, wanted);
    assert_eq!(
      extras(&read),
      expected_extras(&fields[8]),
      "{}",
      revision.rev
    );
  }
}

/// A-588 (golden): do write every changeset 7.2 stored back from what was read of it. Expect its stored bytes.
#[test]
fn every_changeset_writes_back_to_its_bytes() {
  for revision in of_log("changeset") {
    let text = file(&revision.file);
    let mut out = Vec::new();
    changeset::encode(&changeset::decode(text).unwrap(), &mut out).unwrap();
    assert_eq!(out, text, "{}", revision.rev);
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
/// back. Expect the paths, file nodes and flags (`x`, `l`) 7.2 reads, in order, and the stored bytes.
#[test]
fn every_manifest_reads_as_mercurial_reads_it() {
  let manifests = of_log("manifest");
  let entries = expected("entry");
  for fields in expected("changeset") {
    let stored = manifests
      .iter()
      .find(|revision| hex(&revision.node) == fields[2])
      .unwrap();
    let text = file(&stored.file);
    let read: Vec<manifest::Entry<'_>> = manifest::decode(text).unwrap().collect();
    let found: Vec<(Vec<u8>, Node, String)> = read
      .iter()
      .map(|entry| {
        (
          entry.path.to_vec(),
          entry.node,
          flag_letter(entry.flag).to_string(),
        )
      })
      .collect();
    let wanted: Vec<(Vec<u8>, Node, String)> = entries
      .iter()
      .filter(|entry| entry[1] == fields[1])
      .map(|entry| (unhex(&entry[2]), to_node(&entry[3]), entry[4].clone()))
      .collect();
    assert_eq!(found, wanted, "changeset {}", fields[1]);
    let mut out = Vec::new();
    manifest::encode(&read, &mut out).unwrap();
    assert_eq!(out, text);
  }
}

/// A-588 (golden): do split every file revision 7.2 stored and pack it again. Expect the copy 7.2 reads (`c` copied
/// from `a` at `a`'s node), the empty block 7.2 wrote before bytes that begin as one does, and the stored bytes.
#[test]
fn every_file_revision_keeps_its_metadata_apart() {
  let files = of_log("file");
  for fields in expected("filerev") {
    let path = unhex(&fields[1]);
    let stored = files
      .iter()
      .find(|revision| revision.name.as_bytes() == path && revision.rev == fields[2])
      .unwrap();
    let text = file(&stored.file);
    let read = filerev::split(text).unwrap();
    let copy = read
      .copy()
      .unwrap()
      .map(|copy| (copy.path.to_vec(), copy.node));
    let wanted = (fields[3] != "-").then(|| (unhex(&fields[3]), to_node(&fields[4])));
    assert_eq!(copy, wanted, "{}", stored.name);
    let entries: Vec<(&[u8], &[u8])> = read.entries().collect();
    let mut out = Vec::new();
    filerev::pack(&entries, read.bytes, &mut out).unwrap();
    assert_eq!(out, text, "{}", stored.name);
  }
  let meta = filerev::split(file("file-meta-0.txt")).unwrap();
  assert_eq!(meta.metadata, Some(&[][..]));
  assert!(meta.bytes.starts_with(filerev::MARKER));
}
