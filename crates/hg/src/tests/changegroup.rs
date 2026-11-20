//! Tests of `changegroup` (A-514): the changegroups stock Mercurial 7.2 wrote as versions `01` to `04`, decoded as
//! 7.2 lists them, every text rebuilt from its deltas, and each encoded back to its bytes; hostile changegroups
//! refused by kind; every structure held to the admission.

use core::ops::Range;

use super::{
  Changegroup, DeltaInfo, EncodeRefusal, FLAG_FULL_TEXT, FLAG_SIDEDATA, Refusal, Revision, Version,
};
use crate::bundle2::Bundle2;
use crate::fixtures::{self, Row};
use crate::{Admission, NULL_NODE, Node, bundle1, delta};
use proptest::prelude::*;

fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// The changegroup payload of a golden bundle2 stream, whole, with the version its part names.
fn payload(stream: &[u8]) -> (Version, Vec<u8>) {
  let decoded = Bundle2::decode(stream, &mut room()).unwrap();
  let (index, part) = decoded
    .top_level()
    .find(|(_, part)| part.is(b"changegroup"))
    .unwrap();
  let version = Version::from_part_param(part.param(b"version")).unwrap();
  (version, decoded.payload_bytes(index, &mut room()).unwrap())
}

/// Every golden changegroup: its version and its bytes.
fn golden() -> Vec<(Version, Vec<u8>)> {
  let mut out = vec![(
    Version::V01,
    bundle1::changegroup(fixtures::CG01).unwrap().to_vec(),
  )];
  for stream in [fixtures::CG02, fixtures::CG03, fixtures::CG04] {
    out.push(payload(stream));
  }
  out
}

/// A group's revisions as 7.2's listing writes them.
fn rows(cg: &Changegroup<'_>, range: &Range<usize>) -> Vec<Row> {
  cg.group(range)
    .iter()
    .map(|revision| Row {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      link: revision.link,
      base: revision.base,
      len: revision.data.len(),
    })
    .collect()
}

/// Every group of a changegroup as 7.2's listing names them: the changelog, the manifest, then each file.
fn listed(cg: &Changegroup<'_>) -> Vec<(String, Vec<Row>)> {
  let mut out = vec![
    ("changelog".to_string(), rows(cg, &cg.changelog)),
    ("manifest".to_string(), rows(cg, &cg.manifests)),
  ];
  for file in &cg.files {
    out.push((
      String::from_utf8(file.name.to_vec()).unwrap(),
      rows(cg, &file.revisions),
    ));
  }
  out
}

/// Every revision's text in a group, rebuilt: a full text as it came, a delta applied to the empty text when its base
/// is null and otherwise to the text of the earlier revision of the group it names.
fn texts(cg: &Changegroup<'_>, range: &Range<usize>) -> Vec<(Node, Vec<u8>)> {
  let mut out: Vec<(Node, Vec<u8>)> = Vec::new();
  for revision in cg.group(range) {
    let text = if revision.is_full_text() {
      revision.data.to_vec()
    } else {
      let base: &[u8] = if revision.base == NULL_NODE {
        b""
      } else {
        &out
          .iter()
          .find(|(node, _)| *node == revision.base)
          .expect("its base comes earlier in its group")
          .1
      };
      delta::apply(base, revision.data, &mut room()).unwrap()
    };
    out.push((revision.node, text));
  }
  out
}

/// A one-revision changegroup of `version`, encoded, for the hostile tests to break.
fn small(version: Version) -> Vec<u8> {
  let mut data = Vec::new();
  delta::encode_full_text(b"text\n", &mut data).unwrap();
  let revision = Revision {
    node: [1; 20],
    p1: NULL_NODE,
    p2: NULL_NODE,
    base: NULL_NODE,
    link: [1; 20],
    flags: 0,
    info: (version == Version::V04).then_some(DeltaInfo {
      snapshot_level: -2,
      raw_size: 5,
      compression: 0,
      stored_base: NULL_NODE,
      stored_snapshot_level: -2,
    }),
    protocol_flags: 0,
    data: &data,
    sidedata: None,
  };
  let cg = Changegroup {
    version,
    revisions: vec![revision],
    changelog: 0..1,
    manifests: 1..1,
    trees: Vec::new(),
    files: Vec::new(),
  };
  let mut out = Vec::new();
  cg.encode(&mut out).unwrap();
  out
}

/// AC-HG.1 (golden), A-514: do decode the changegroups 7.2 wrote as versions 01 and 02. Expect every group, and every
/// revision's node, parents, link, delta base and delta length, exactly as 7.2's own `debugbundle --all` lists them;
/// version 01's bases resolved as 7.2 resolves them (the previous revision of the group, or p1 for its first).
#[test]
fn versions_01_and_02_decode_as_mercurial_lists_them() {
  let cg01 = bundle1::changegroup(fixtures::CG01).unwrap();
  let decoded = Changegroup::decode(cg01, Version::V01, &mut room()).unwrap();
  assert_eq!(listed(&decoded), fixtures::listing(fixtures::CG01_LISTING));
  let (version, bytes) = payload(fixtures::CG02);
  assert_eq!(version, Version::V02);
  let decoded = Changegroup::decode(&bytes, version, &mut room()).unwrap();
  assert_eq!(listed(&decoded), fixtures::listing(fixtures::CG02_LISTING));
  assert!(decoded.trees.is_empty());
}

/// AC-HG.1 (golden), A-514: do decode the changegroup 7.2 wrote as version 03. Expect its changelog and manifest as
/// 7.2 lists them (its listing stops there), its files the same revisions as version 02's, and an empty tree list
/// read.
#[test]
fn version_03_decodes_as_mercurial_lists_it() {
  let (_, bytes02) = payload(fixtures::CG02);
  let cg02 = Changegroup::decode(&bytes02, Version::V02, &mut room()).unwrap();
  let (version, bytes03) = payload(fixtures::CG03);
  assert_eq!(version, Version::V03);
  let cg03 = Changegroup::decode(&bytes03, version, &mut room()).unwrap();
  let listing = fixtures::listing(fixtures::CG03_LISTING);
  assert_eq!(listing.len(), 2);
  assert_eq!(listed(&cg03)[..2], listing[..]);
  assert_eq!(listed(&cg03)[2..], listed(&cg02)[2..]);
  assert!(cg03.trees.is_empty());
}

/// AC-HG.1 (golden), A-514: do decode the changegroup 7.2 wrote as version 04. Expect its changesets in the order
/// 7.2's plain listing names them, every revision's node, parents and link equal to version 03's, and every one
/// carrying delta information, some of them full texts.
#[test]
fn version_04_decodes_as_mercurial_lists_it() {
  let (_, bytes03) = payload(fixtures::CG03);
  let cg03 = Changegroup::decode(&bytes03, Version::V03, &mut room()).unwrap();
  let (version, bytes04) = payload(fixtures::CG04);
  assert_eq!(version, Version::V04);
  let cg04 = Changegroup::decode(&bytes04, version, &mut room()).unwrap();
  let nodes: Vec<Node> = cg04
    .group(&cg04.changelog)
    .iter()
    .map(|revision| revision.node)
    .collect();
  assert_eq!(nodes, fixtures::plain_nodes(fixtures::CG04_LISTING));
  let identity = |revision: &Revision<'_>| (revision.node, revision.p1, revision.p2, revision.link);
  assert_eq!(
    cg04.revisions.iter().map(identity).collect::<Vec<_>>(),
    cg03.revisions.iter().map(identity).collect::<Vec<_>>()
  );
  assert!(
    cg04
      .revisions
      .iter()
      .all(|revision| revision.info.is_some())
  );
  assert!(cg04.revisions.iter().any(Revision::is_full_text));
}

/// A-514 (oracle): do rebuild every text of every golden changegroup from its deltas. Expect each file's revisions to
/// hold exactly what `make-fixtures.sh` wrote (the copy's text beginning with Mercurial's metadata block naming its
/// source), each changeset's text to begin with its manifest's node and to end with its description, the last
/// manifest to name the amended file's node, and all four versions to agree text for text.
#[test]
fn every_text_rebuilds_from_its_deltas_in_every_version() {
  let mut versions = Vec::new();
  for (version, bytes) in golden() {
    let cg = Changegroup::decode(&bytes, version, &mut room()).unwrap();
    let files: Vec<(String, Vec<Vec<u8>>)> = cg
      .files
      .iter()
      .map(|file| {
        let name = String::from_utf8(file.name.to_vec()).unwrap();
        (
          name,
          texts(&cg, &file.revisions)
            .into_iter()
            .map(|(_, text)| text)
            .collect(),
        )
      })
      .collect();
    let expected: Vec<(String, Vec<Vec<u8>>)> = fixtures::FILE_TEXTS
      .iter()
      .map(|(name, texts)| {
        (
          name.to_string(),
          texts.iter().map(|text| text.to_vec()).collect(),
        )
      })
      .collect();
    assert_eq!(files, expected, "version {version:?}");
    let manifests = texts(&cg, &cg.manifests);
    let changesets = texts(&cg, &cg.changelog);
    let descriptions = fixtures::descriptions();
    for (position, (_, text)) in changesets.iter().enumerate() {
      let manifest = format!("{}\n", fixtures::hex(&manifests[position].0));
      assert!(
        text.starts_with(manifest.as_bytes()),
        "version {version:?}, changeset {position}"
      );
      assert!(text.ends_with(descriptions[position].as_bytes()));
      assert_eq!(text.split(|byte| *byte == b'\n').nth(1), Some(&b"test"[..]));
    }
    let amended = format!(
      "dir/f\0{}\n",
      fixtures::hex(&cg.group(&cg.files[3].revisions)[1].node)
    );
    let last = &manifests.last().unwrap().1;
    assert!(
      last
        .windows(amended.len())
        .any(|line| line == amended.as_bytes())
    );
    versions.push((files, manifests, changesets));
  }
  assert!(versions.windows(2).all(|pair| pair[0] == pair[1]));
}

/// A-514 (golden): do decode each golden changegroup and encode it again. Expect its own bytes back, byte for byte, and
/// `encoded_len` their length.
#[test]
fn every_golden_changegroup_encodes_back_to_its_bytes() {
  for (version, bytes) in golden() {
    let cg = Changegroup::decode(&bytes, version, &mut room()).unwrap();
    let mut out = Vec::new();
    cg.encode(&mut out).unwrap();
    assert_eq!(out, bytes, "version {version:?}");
    assert_eq!(
      cg.encoded_len().unwrap(),
      u64::try_from(bytes.len()).unwrap()
    );
  }
}

/// AC-HG.9 (hostile input), A-514: do put every length 7.2 refuses in a chunk's length: `len = u32::MAX` (−1), −5,
/// 1, 2, 3 and 4; then `i32::MAX`, and a chunk one byte shorter than its version's header. Expect `ChunkLength` for
/// the first six, `Truncated` for the seventh, `ShortChunk` for the last.
#[test]
fn every_chunk_length_mercurial_refuses_is_refused() {
  for version in [Version::V01, Version::V02, Version::V03, Version::V04] {
    let bytes = small(version);
    for value in [-1_i32, -5, 1, 2, 3, 4] {
      let mut broken = bytes.clone();
      broken[..4].copy_from_slice(&value.to_be_bytes());
      assert_eq!(
        Changegroup::decode(&broken, version, &mut room()),
        Err(Refusal::ChunkLength { value })
      );
    }
    let mut broken = bytes.clone();
    broken[..4].copy_from_slice(&i32::MAX.to_be_bytes());
    assert!(matches!(
      Changegroup::decode(&broken, version, &mut room()),
      Err(Refusal::Truncated(_))
    ));
    let header = version.header_len();
    let mut short = i32::try_from(header + 3).unwrap().to_be_bytes().to_vec();
    short.extend(std::iter::repeat_n(0, header - 1));
    assert_eq!(
      Changegroup::decode(&short, version, &mut room()),
      Err(Refusal::ShortChunk {
        len: u64::try_from(header - 1).unwrap(),
        header: u64::try_from(header).unwrap()
      })
    );
  }
}

/// AC-HG.9, A-514: do cut every golden changegroup at every length short of its own, and append a byte to each.
/// Expect every cut refused and the extra byte refused `TrailingBytes`, none read as a whole changegroup.
#[test]
fn a_changegroup_cut_anywhere_or_with_bytes_after_is_refused() {
  for (version, bytes) in golden() {
    for cut in 0..bytes.len() {
      assert!(
        Changegroup::decode(&bytes[..cut], version, &mut room()).is_err(),
        "{version:?} cut at {cut}"
      );
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(
      Changegroup::decode(&longer, version, &mut room()),
      Err(Refusal::TrailingBytes { bytes: 1 })
    );
  }
}

/// AC-HG.9, A-514: do flip every bit of the golden version 02 changegroup, one at a time. Expect a typed refusal, or
/// a changegroup that encodes back to exactly the flipped bytes; never a panic.
#[test]
fn a_flipped_bit_anywhere_is_refused_or_read_exactly() {
  let (version, bytes) = payload(fixtures::CG02);
  let mut refused = 0;
  for at in 0..bytes.len() {
    for bit in 0..8 {
      let mut flipped = bytes.clone();
      flipped[at] ^= 1 << bit;
      match Changegroup::decode(&flipped, version, &mut room()) {
        Ok(cg) => {
          let mut out = Vec::new();
          cg.encode(&mut out).unwrap();
          assert_eq!(out, flipped, "flip of bit {bit} at {at}");
        }
        Err(_) => refused += 1,
      }
    }
  }
  assert!(refused > 0);
}

/// AC-HG.9, A-514: do give a revision a delta mpatch cannot decode (a start past its end). Expect `Delta` naming the
/// revision and mpatch's refusal.
#[test]
fn a_delta_mpatch_cannot_decode_is_refused_with_its_revision() {
  let mut bytes = small(Version::V02);
  let start = 4 + Version::V02.header_len();
  bytes[start..start + 4].copy_from_slice(&9_i32.to_be_bytes());
  assert_eq!(
    Changegroup::decode(&bytes, Version::V02, &mut room()),
    Err(Refusal::Delta {
      node: [1; 20],
      refusal: delta::Refusal::Undecodable { at: 0 }
    })
  );
}

/// A-514: do decode a golden changegroup against what it was charged and against one byte less. Expect the first to
/// decode and the second refused `PastAdmission` before anything was built.
#[test]
fn a_changegroup_is_charged_exactly_and_refused_one_byte_short() {
  let (version, bytes) = payload(fixtures::CG02);
  let mut admission = room();
  Changegroup::decode(&bytes, version, &mut admission).unwrap();
  let charged = u64::MAX - admission.left();
  assert!(charged > 0);
  assert!(Changegroup::decode(&bytes, version, &mut Admission::new(charged)).is_ok());
  assert!(matches!(
    Changegroup::decode(&bytes, version, &mut Admission::new(charged - 1)),
    Err(Refusal::PastAdmission(_))
  ));
}

/// A-514: do name versions as bundle2's `version` parameter does, with none, and with one Mercurial does not define.
/// Expect `01` for none (7.2's default), each name its version, and `06` refused.
#[test]
fn versions_are_named_as_mercurial_names_them() {
  assert_eq!(Version::from_part_param(None), Ok(Version::V01));
  for version in [
    Version::V01,
    Version::V02,
    Version::V03,
    Version::V04,
    Version::V05,
  ] {
    assert_eq!(Version::from_part_param(Some(version.name())), Ok(version));
  }
  assert_eq!(
    Version::from_name(b"06"),
    Err(Refusal::UnknownVersion {
      name: b"06".to_vec()
    })
  );
}

/// A-514 (layout from 7.2's code, no golden vector: 7.2's bundle types offer 01 to 04): do encode a version 05
/// changegroup whose revision carries sidedata and the full-text flag, and decode it. Expect the same changegroup
/// back, the protocol flags first in the revision's header, and the sidedata in its own chunk after the revision's.
#[test]
fn version_05_carries_sidedata_in_its_own_chunk() {
  let revision = Revision {
    node: [2; 20],
    p1: NULL_NODE,
    p2: NULL_NODE,
    base: NULL_NODE,
    link: [2; 20],
    flags: 0,
    info: None,
    protocol_flags: FLAG_SIDEDATA | FLAG_FULL_TEXT,
    data: b"full text",
    sidedata: Some(b"\x00\x00"),
  };
  let cg = Changegroup {
    version: Version::V05,
    revisions: vec![revision],
    changelog: 0..1,
    manifests: 1..1,
    trees: Vec::new(),
    files: Vec::new(),
  };
  let mut bytes = Vec::new();
  cg.encode(&mut bytes).unwrap();
  assert_eq!(bytes[4], FLAG_SIDEDATA | FLAG_FULL_TEXT);
  assert_eq!(
    Changegroup::decode(&bytes, Version::V05, &mut room()),
    Ok(cg)
  );
}

/// A-514: do encode revisions whose fields their version cannot carry: a version 01 delta against other than the
/// previous revision, flags in version 02, delta information outside version 04, sidedata outside version 05.
/// Expect each refused `Unencodable`.
#[test]
fn fields_a_version_cannot_carry_are_not_encoded() {
  let base = Revision {
    node: [3; 20],
    p1: NULL_NODE,
    p2: NULL_NODE,
    base: NULL_NODE,
    link: [3; 20],
    flags: 0,
    info: None,
    protocol_flags: 0,
    data: b"",
    sidedata: None,
  };
  let info = DeltaInfo {
    snapshot_level: 0,
    raw_size: 0,
    compression: 0,
    stored_base: NULL_NODE,
    stored_snapshot_level: 0,
  };
  let cases = [
    (
      Version::V01,
      Revision {
        base: [9; 20],
        ..base
      },
    ),
    (Version::V02, Revision { flags: 1, ..base }),
    (
      Version::V03,
      Revision {
        info: Some(info),
        ..base
      },
    ),
    (Version::V04, base),
    (
      Version::V03,
      Revision {
        sidedata: Some(b"x"),
        ..base
      },
    ),
  ];
  for (version, revision) in cases {
    let cg = Changegroup {
      version,
      revisions: vec![revision],
      changelog: 0..1,
      manifests: 1..1,
      trees: Vec::new(),
      files: Vec::new(),
    };
    assert!(
      matches!(
        cg.encode(&mut Vec::new()),
        Err(EncodeRefusal::Unencodable { .. })
      ),
      "{version:?}"
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// AC-HG.9, A-514 (hostile input): do decode arbitrary bytes as each version, and the golden version 02 changegroup
  /// with arbitrary bytes written over it. Expect a typed refusal, or a changegroup no shorter than the smallest (three
  /// empty chunks, 12 bytes) and encoding back to exactly the bytes read; never a panic.
  #[test]
  fn arbitrary_bytes_never_panic(
    bytes in proptest::collection::vec(any::<u8>(), 0..512),
    at in 0..4_000_usize,
  ) {
    for version in [Version::V01, Version::V02, Version::V03, Version::V04, Version::V05] {
      let decoded = Changegroup::decode(&bytes, version, &mut Admission::new(1 << 20));
      prop_assert!(decoded.is_err() || bytes.len() >= 12);
    }
    let (version, mut golden) = payload(fixtures::CG02);
    let start = at % golden.len();
    for (offset, byte) in bytes.iter().enumerate().take(golden.len() - start) {
      golden[start + offset] = *byte;
    }
    if let Ok(cg) = Changegroup::decode(&golden, version, &mut Admission::new(1 << 20)) {
      let mut out = Vec::new();
      cg.encode(&mut out).unwrap();
      prop_assert_eq!(out, golden.clone());
    }
  }
}
