//! The landing manifest's golden vectors (A-469): the canonical encoding a grant binds (§4.15 steps 1 and 3), pinned
//! byte for byte for each layout the encoder writes — an overlay's (judged by witness, `VERSION` 3) and a captured
//! clone's (judged by content, `VERSION_CONTENT` 5) — over fixed manifests holding every action, Windows bits set and
//! unset. The published bytes and identities live in `manifest_vectors.txt`, one line per header, entry and identity, so a
//! change shows as the lines it moves. A layout change fails `the_manifest_encodings_are_their_golden_vectors` until it is
//! republished deliberately (`--ignored publish_the_manifest_vectors`), as the doc-truth tests' writers do; a normal run
//! never touches the tree. The encoding is only ever produced and hashed (a grant names its BLAKE3; a presentation keeps
//! the manifest itself), so no decoder exists to feed hostile bytes to; what stands in for one here is that every field a
//! landing acts on moves the identity.

use slates_vfs::inode::{Fingerprint, Kind, WindowsAttributes, Witness};

use crate::manifest::{Action, FileMeta, Judged, LandingEntry, Manifest, OverlayIdentity};

/// Format: the published vectors.
const PUBLISHED: &str = include_str!("manifest_vectors.txt");

/// Format: the bytes of a manifest's header: its magic (4), its layout (2) and its entry count (4).
const HEADER_BYTES: usize = 10;

/// Format: the published file's preamble, which the writer writes and the test compares too.
const PREAMBLE: &str = "\
# The landing manifest's golden vectors (A-469): the canonical encoding a grant binds, one for each layout the encoder
# writes, over the fixed manifests of crates/land/src/manifest_vectors.rs. A line is `<layout> <part> <hex>`: the
# header (magic, layout, entry count), each entry by its path in landing order, then the manifest's BLAKE3 identity,
# the hash a grant names. Republish deliberately after a layout change:
#   cargo test -p slates-land --lib -- --ignored publish_the_manifest_vectors
";

/// Shape: a fixed device number, every byte distinct, so a field moved within the encoding shows.
const DEV: u64 = 0x0807_0605_0403_0201;
/// Shape: a fixed modification time.
const MTIME_NS: i64 = 1_700_000_000_000_000_001;
/// Shape: a fixed change time.
const CTIME_NS: i64 = 1_700_000_000_000_000_002;
/// Shape: a fixed birth time (not in the encoding: a witness's fingerprint binds its device, inode, size, times and mode).
const BIRTH_NS: i64 = 1_600_000_000_000_000_003;
/// Shape: the volume's modification time of what it writes.
const OVERLAY_MTIME_NS: i64 = 1_700_000_000_000_000_010;
/// Shape: a regular file's mode, read-write for its owner.
const FILE_MODE: u32 = 0o100_644;
/// Shape: the same file made executable.
const EXECUTABLE_MODE: u32 = 0o100_755;
/// Shape: a directory's mode.
const DIR_MODE: u32 = 0o040_755;

fn fingerprint(ino: u64, mode: u32, windows: u16) -> Fingerprint {
  Fingerprint {
    dev: DEV,
    ino,
    size: 3,
    mtime_ns: MTIME_NS,
    ctime_ns: CTIME_NS,
    mode,
    uid: 501,
    gid: 20,
    birth_ns: BIRTH_NS,
    windows,
  }
}

/// An overlay's witness: the disk's fingerprint and the BLAKE3 of its bytes (`identity`, one byte repeated).
fn witness(ino: u64, mode: u32, windows: u16, identity: u8) -> Witness {
  Witness {
    fingerprint: fingerprint(ino, mode, windows),
    identity: [identity; 32],
    witnessed_at: 11,
    racy: false,
  }
}

/// A captured clone's witness: what the capture held, its bytes' identity and its metadata (A-291, A-466).
fn content_witness(identity: u8, meta: FileMeta) -> Witness {
  Witness {
    fingerprint: Fingerprint {
      mode: meta.permissions,
      windows: meta.windows,
      ..Fingerprint::default()
    },
    identity: [identity; 32],
    witnessed_at: 0,
    racy: false,
  }
}

fn overlay(hash: u8, size: u64, mode: u32) -> OverlayIdentity {
  OverlayIdentity {
    hash: [hash; 32],
    size,
    mode,
    mtime_ns: OVERLAY_MTIME_NS,
  }
}

/// A directory the volume makes: the planner's overlay identity for one (no bytes, its mode and mtime).
fn made_directory() -> OverlayIdentity {
  overlay(0, 0, DIR_MODE)
}

fn bits(windows: u16, kind: Kind) -> WindowsAttributes {
  WindowsAttributes::kept(u32::from(windows), kind)
}

fn entry(
  path: &str,
  action: Action,
  witnessed: Option<Witness>,
  overlay: Option<OverlayIdentity>,
  judged: Judged,
  windows: WindowsAttributes,
) -> LandingEntry {
  LandingEntry {
    path: path.into(),
    action,
    witnessed,
    overlay,
    judged,
    windows,
  }
}

/// The fixed entries of each layout's golden manifest, in an order of no meaning (the manifest's own is the landing
/// order): every action a manifest so judged holds, the Windows bits set on some and unset on others.
fn golden_entries(basis: Judged) -> Vec<LandingEntry> {
  /// Format: `FILE_ATTRIBUTE_READONLY`.
  const READONLY: u16 = 0x1;
  /// Format: `FILE_ATTRIBUTE_HIDDEN`.
  const HIDDEN: u16 = 0x2;
  /// Format: `FILE_ATTRIBUTE_DIRECTORY`, a link's directory kind (A-275).
  const LINK_DIRECTORY: u16 = 0x10;
  /// Format: `FILE_ATTRIBUTE_ARCHIVE`.
  const ARCHIVE: u16 = 0x20;
  let none = WindowsAttributes::default();
  let executable = FileMeta {
    permissions: 0o755,
    windows: HIDDEN | ARCHIVE,
  };
  let mut entries = vec![
    entry(
      "/src/new.rs",
      Action::Create,
      None,
      Some(overlay(0x11, 5, FILE_MODE)),
      basis,
      bits(HIDDEN | ARCHIVE, Kind::File),
    ),
    entry(
      "/made",
      Action::Mkdir,
      None,
      Some(made_directory()),
      basis,
      bits(READONLY | HIDDEN, Kind::Dir),
    ),
    entry(
      "/link",
      Action::Symlink {
        target: "src/lib.rs".into(),
      },
      None,
      None,
      basis,
      bits(LINK_DIRECTORY | HIDDEN, Kind::Symlink),
    ),
  ];
  match basis {
    Judged::Witness => {
      let was = FileMeta {
        permissions: 0o644,
        windows: 0,
      };
      entries.extend([
        entry(
          "/src/lib.rs",
          Action::Replace,
          Some(witness(2, FILE_MODE, ARCHIVE, 0x22)),
          Some(overlay(0x33, 7, FILE_MODE)),
          basis,
          none,
        ),
        entry(
          "/run.sh",
          Action::Chmod {
            from: was,
            to: executable,
          },
          Some(witness(5, FILE_MODE, 0, 0x55)),
          Some(overlay(0x55, 9, EXECUTABLE_MODE)),
          basis,
          bits(HIDDEN | ARCHIVE, Kind::File),
        ),
        entry(
          "/src/gone.rs",
          Action::Delete,
          Some(witness(3, FILE_MODE, 0, 0x44)),
          None,
          basis,
          none,
        ),
        entry(
          "/old",
          Action::Rmdir,
          Some(witness(4, DIR_MODE, 0, 0)),
          None,
          basis,
          none,
        ),
        entry(
          "/moved",
          Action::Rename {
            from: "/origin".into(),
          },
          Some(witness(6, DIR_MODE, 0, 0)),
          None,
          basis,
          none,
        ),
        entry(
          "/cleared",
          Action::Clear,
          Some(witness(10, DIR_MODE, 0, 0)),
          Some(made_directory()),
          basis,
          none,
        ),
      ]);
    }
    // A captured clone's manifest makes no clear and no rename (A-291): a moved directory is its paths removed and made.
    Judged::Content => {
      let was = FileMeta {
        permissions: 0o644,
        windows: ARCHIVE,
      };
      entries.extend([
        entry(
          "/src/lib.rs",
          Action::Replace,
          Some(content_witness(0x22, was)),
          Some(overlay(0x33, 7, FILE_MODE)),
          basis,
          bits(ARCHIVE, Kind::File),
        ),
        entry(
          "/run.sh",
          Action::Chmod {
            from: was,
            to: executable,
          },
          Some(content_witness(0x55, was)),
          Some(overlay(0x55, 9, EXECUTABLE_MODE)),
          basis,
          bits(HIDDEN | ARCHIVE, Kind::File),
        ),
        entry(
          "/src/gone.rs",
          Action::Delete,
          Some(content_witness(0x44, FileMeta::default())),
          None,
          basis,
          none,
        ),
        entry(
          "/old",
          Action::Rmdir,
          Some(content_witness(0, FileMeta::default())),
          None,
          basis,
          none,
        ),
      ]);
    }
  }
  entries
}

fn golden_manifest(basis: Judged) -> Manifest {
  Manifest::from_judged(golden_entries(basis), 0, basis)
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The vectors' text as the encoder writes them now: for each layout, its header, each entry in landing order, and the
/// manifest's identity. An entry's bytes are where the encoding of the entries before it ends and that of the entries up
/// to it ends: the header is the same length whatever it counts, and each entry encodes on its own.
fn vector_text() -> String {
  let mut text = String::from(PREAMBLE);
  for basis in [Judged::Witness, Judged::Content] {
    let manifest = golden_manifest(basis);
    let whole = Manifest::encode(&manifest.entries, basis);
    let layout = u16::from_le_bytes([whole[4], whole[5]]);
    text.push_str(&format!(
      "{layout} header {}\n",
      hex(&whole[..HEADER_BYTES])
    ));
    let mut at = HEADER_BYTES;
    for (index, entry) in manifest.entries.iter().enumerate() {
      let next = Manifest::encode(&manifest.entries[..=index], basis).len();
      text.push_str(&format!(
        "{layout} {} {}\n",
        entry.path,
        hex(&whole[at..next])
      ));
      at = next;
    }
    assert_eq!(
      at,
      whole.len(),
      "every byte belongs to the header or an entry"
    );
    text.push_str(&format!("{layout} identity {}\n", hex(&manifest.hash)));
  }
  text
}

/// A-469. Do: encode each layout's golden manifest and compare its header, its entries and its identity with the
/// published vectors, line by line. Expect every line equal: a change to the encoding a grant binds fails here, naming
/// the first line it moved, until it is republished deliberately.
#[test]
fn the_manifest_encodings_are_their_golden_vectors() {
  let now = vector_text();
  for (number, (published, encoded)) in PUBLISHED.lines().zip(now.lines()).enumerate() {
    assert_eq!(
      published,
      encoded,
      "line {} of manifest_vectors.txt moved; if the layout changed on purpose, republish: cargo test -p slates-land --lib -- --ignored publish_the_manifest_vectors",
      number + 1
    );
  }
  assert_eq!(
    PUBLISHED.lines().count(),
    now.lines().count(),
    "the vectors hold another number of lines than the encoder writes"
  );
}

/// The deliberate writer: rewrites `manifest_vectors.txt` from the encoder. Ignored, so a normal run never touches the
/// tree; run it with `--ignored` after a layout change that is meant, and review the lines it moves.
#[test]
#[ignore = "rewrites crates/land/src/manifest_vectors.txt from the encoder; run deliberately with --ignored"]
fn publish_the_manifest_vectors() {
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/manifest_vectors.txt");
  // The design's `--ignored` writer rewriting a tracked file in the repository (CLAUDE.md §4 "Doc-truth tests");
  // shipped code never reaches this call.
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, vector_text()).unwrap();
}

/// A-469 (determinism). Do: build each layout's golden manifest twice, the second time from its entries in reverse order,
/// and encode each twice; then encode one manifest's entries under the other layout. Expect the same bytes and identity
/// every time, since the landing order is the manifest's own whatever order its entries were planned in; and the two
/// layouts never the same bytes, so no grant of one binds a manifest of the other.
#[test]
fn a_manifest_encodes_to_the_same_bytes_whatever_order_it_was_planned_in() {
  for basis in [Judged::Witness, Judged::Content] {
    let first = golden_manifest(basis);
    let mut reversed = golden_entries(basis);
    reversed.reverse();
    let second = Manifest::from_judged(reversed, 0, basis);
    assert_eq!(
      Manifest::encode(&first.entries, basis),
      Manifest::encode(&first.entries, basis)
    );
    assert_eq!(
      Manifest::encode(&first.entries, basis),
      Manifest::encode(&second.entries, basis)
    );
    assert_eq!(first.hash, second.hash, "{basis:?}");
  }
  let entries = golden_manifest(Judged::Content).entries;
  assert_ne!(
    Manifest::encode(&entries, Judged::Witness),
    Manifest::encode(&entries, Judged::Content)
  );
}

/// One change to one field of an entry, which a landing acts on.
type Change = fn(&mut LandingEntry);

/// The changes to a witness's fingerprint that an overlay's manifest binds (its device, inode, size, times and mode)
/// and a captured clone's does not hold: its witness is what the capture held, its bytes' identity and its metadata.
fn fingerprint_changes() -> [(&'static str, Change); 5] {
  [
    ("witness device", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.fingerprint.dev ^= 1;
      }
    }),
    ("witness inode", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.fingerprint.ino ^= 1;
      }
    }),
    ("witness size", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.fingerprint.size ^= 1;
      }
    }),
    ("witness mtime", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.fingerprint.mtime_ns ^= 1;
      }
    }),
    ("witness ctime", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.fingerprint.ctime_ns ^= 1;
      }
    }),
  ]
}

/// The changes every manifest binds: a path, the Windows bits, a witness's mode (a captured clone's: the mode the
/// capture held) and identity, the volume's identity, size, mode and mtime, a target or an origin, and a metadata
/// entry's either side.
fn common_changes() -> [(&'static str, Change); 11] {
  [
    ("path", |e| e.path = format!("{}x", e.path).into()),
    ("windows bits", |e| {
      e.windows = WindowsAttributes::kept(u32::from(e.windows.bits() ^ 0x4), Kind::Dir);
    }),
    ("witness mode", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.fingerprint.mode ^= 0o100;
      }
    }),
    ("witness identity", |e| {
      if let Some(w) = e.witnessed.as_mut() {
        w.identity[31] ^= 1;
      }
    }),
    ("volume's identity", |e| {
      if let Some(o) = e.overlay.as_mut() {
        o.hash[0] ^= 1;
      }
    }),
    ("volume's size", |e| {
      if let Some(o) = e.overlay.as_mut() {
        o.size ^= 1;
      }
    }),
    ("volume's mode", |e| {
      if let Some(o) = e.overlay.as_mut() {
        o.mode ^= 0o100;
      }
    }),
    ("volume's mtime", |e| {
      if let Some(o) = e.overlay.as_mut() {
        o.mtime_ns ^= 1;
      }
    }),
    ("symlink target or rename origin", |e| match &mut e.action {
      Action::Symlink { target } => *target = format!("{target}x").into(),
      Action::Rename { from } => *from = format!("{from}x").into(),
      _ => {}
    }),
    ("metadata entry's base", |e| {
      if let Action::Chmod { from, .. } = &mut e.action {
        from.windows ^= 0x4;
      }
    }),
    ("metadata entry's target", |e| {
      if let Action::Chmod { to, .. } = &mut e.action {
        to.permissions ^= 0o1;
      }
    }),
  ]
}

/// A-469 (the bytes bind every field a landing acts on, in place of a decoder's hostile-input tests: nothing decodes
/// these bytes). Do: change one field of one entry of each layout's golden manifest at a time — a path, a target, an
/// origin, the Windows bits, a witness's mode and identity (and an overlay's witness's device, inode, size and times),
/// the volume's identity, size, mode or mtime, a metadata entry's either side — and rebuild the manifest. Expect its
/// identity to move every time: a grant names no manifest but its own. An overlay's witness's owner, birth time and
/// Windows bits are judged (its verdict compares whole fingerprints) and not bound: owed (GAPS, A-469).
#[test]
fn every_field_a_landing_acts_on_moves_the_identity() {
  for basis in [Judged::Witness, Judged::Content] {
    let golden = golden_manifest(basis);
    let mut changes: Vec<(&str, Change)> = common_changes().to_vec();
    if basis == Judged::Witness {
      changes.extend(fingerprint_changes());
    }
    for (name, change) in changes {
      for at in 0..golden.entries.len() {
        let mut entries = golden.entries.clone();
        let before = entries[at].clone();
        change(&mut entries[at]);
        if entries[at] == before {
          // The entry holds no such field (a create has no witness).
          continue;
        }
        let changed = Manifest::from_judged(entries, 0, basis);
        assert_ne!(
          changed.hash, golden.hash,
          "{basis:?}: changing the {name} of {} left the identity a grant names",
          before.path
        );
      }
    }
  }
}
