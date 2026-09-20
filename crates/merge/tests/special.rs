//! A-26 / §4.16: IPC namespace metadata is versioned without creating a file-content path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use slates_merge::engine::{Green, Increment, Outcome, Rebased};
use slates_merge::increment::{Base, VolumeOp, compose_volume};
use slates_merge::ops_doc::{DocDecodeError, OpKind};
use slates_merge::origin::Origin;
use slates_merge::special::{SpecialKind, SpecialNode};

fn node(kind: SpecialKind) -> SpecialNode {
  SpecialNode {
    kind,
    uid: 123,
    gid: 456,
    atime: -1,
    mtime: 2,
    ctime: 3,
    btime: 0,
  }
}

fn make(path: &str, kind: SpecialKind) -> VolumeOp {
  VolumeOp::Mknod {
    path: path.to_owned(),
    node: node(kind),
    mode: 0o640,
  }
}

fn increment(base: &Base, version: u64, journal: &[VolumeOp]) -> Increment {
  let doc = compose_volume(base, journal).expect("valid IPC journal");
  let mut post = Vec::new();
  for op in &doc.ops {
    if op.kind != OpKind::Mknod {
      continue;
    }
    let path = doc.paths.path(op.path).unwrap();
    let metadata = journal
      .iter()
      .rev()
      .find_map(|entry| match entry {
        VolumeOp::Mknod {
          path: declared,
          node,
          ..
        } if declared == path => Some(*node),
        _ => None,
      })
      .unwrap()
      .encode();
    let start = usize::try_from(op.src).unwrap();
    post.resize(start + metadata.len(), 0);
    post[start..].copy_from_slice(&metadata);
  }
  let mut identity = blake3::Hasher::new();
  identity.update(&doc.encode());
  identity.update(&post);
  identity.update(&version.to_le_bytes());
  Increment {
    id: *identity.finalize().as_bytes(),
    base: version,
    doc,
    post_state: post,
    evidence: vec![],
  }
}

/// AC-6.8 / AC-6.13: capture all IPC metadata, replay it, and distinguish every kind and owner.
#[test]
fn an_ipc_origin_preserves_metadata_and_changes_identity_with_kind_or_owner() {
  let origin = Origin {
    specials: vec![("pipe".into(), node(SpecialKind::Fifo))],
    ..Origin::default()
  };
  let decoded = Origin::decode(&origin.encode()).unwrap();
  let green = Green::with_origin(&decoded);
  assert_eq!(green.special("pipe"), Some(node(SpecialKind::Fifo)));
  assert_eq!(green.content("pipe"), None);
  assert_eq!(green.current_base().specials, origin.specials);
  for replacement in [
    node(SpecialKind::Socket),
    SpecialNode {
      uid: 789,
      ..node(SpecialKind::Fifo)
    },
  ] {
    let different = Origin {
      specials: vec![("pipe".into(), replacement)],
      ..Origin::default()
    };
    assert_ne!(origin.identity(), different.identity());
    assert_ne!(
      green.head_identity(),
      Green::with_origin(&different).head_identity()
    );
  }
}

/// AC-6.8 / A-26: changing an IPC inode through its primary name updates and invalidates aliases.
#[test]
fn ipc_aliases_share_metadata_and_are_invalidated_with_their_inode() {
  let origin = Origin {
    specials: vec![("pipe".into(), node(SpecialKind::Fifo))],
    hardlinks: vec![("alias".into(), "pipe".into())],
    modes: vec![("pipe".into(), 0o640)],
    ..Origin::default()
  };
  let mut green = Green::with_origin(&origin);
  assert_eq!(green.mode("alias"), Some(0o640));
  let inc = increment(
    &green.current_base(),
    0,
    &[VolumeOp::SetMode {
      path: "pipe".into(),
      mode: 0o600,
    }],
  );
  assert_eq!(green.submit(&inc), Outcome::Accepted { version: 1 });
  assert_eq!(green.mode("alias"), Some(0o600));
  assert_eq!(green.changed_between(0, 1), vec!["alias", "pipe"]);
}

/// AC-6.2 / A-26: equal mode bits cannot make a stale IPC chmod apply to a replacement file.
#[test]
fn an_ipc_metadata_change_conflicts_after_the_path_changes_kind() {
  let origin = Origin {
    specials: vec![("pipe".into(), node(SpecialKind::Fifo))],
    modes: vec![("pipe".into(), 0o640)],
    ..Origin::default()
  };
  let mut green = Green::with_origin(&origin);
  let pending = increment(
    &green.current_base(),
    0,
    &[VolumeOp::SetMode {
      path: "pipe".into(),
      mode: 0o600,
    }],
  );
  let remove = increment(
    &green.current_base(),
    0,
    &[VolumeOp::Unlink {
      path: "pipe".into(),
    }],
  );
  assert_eq!(green.submit(&remove), Outcome::Accepted { version: 1 });
  let replacement = increment(
    &green.current_base(),
    1,
    &[
      VolumeOp::Create {
        path: "pipe".into(),
      },
      VolumeOp::SetMode {
        path: "pipe".into(),
        mode: 0o600,
      },
    ],
  );
  assert_eq!(green.submit(&replacement), Outcome::Accepted { version: 2 });
  let identity = green.head_identity();
  assert!(matches!(green.submit(&pending), Outcome::Conflict { .. }));
  assert_eq!(green.head_identity(), identity);
}

/// AC-6.2 / A-26: stale xattr sets and removals also conflict after an IPC name changes kind.
#[test]
fn ipc_xattr_changes_cannot_follow_a_name_into_a_replacement_file() {
  for removing in [false, true] {
    let origin = Origin {
      specials: vec![("pipe".into(), node(SpecialKind::Fifo))],
      xattrs: vec![("pipe".into(), "user.key".into(), b"old".to_vec())],
      ..Origin::default()
    };
    let mut green = Green::with_origin(&origin);
    let mut pending = common::Build::new();
    if removing {
      pending.removexattr("pipe", "user.key");
    } else {
      pending.setxattr("pipe", "user.key", b"new");
    }
    let removed = increment(
      &green.current_base(),
      0,
      &[VolumeOp::Unlink {
        path: "pipe".into(),
      }],
    );
    assert_eq!(green.submit(&removed), Outcome::Accepted { version: 1 });
    assert_eq!(green.xattr("pipe", "user.key"), None);
    let mut replacement = common::Build::new();
    replacement.create("pipe", b"");
    if !removing {
      replacement.setxattr("pipe", "user.key", b"new");
    }
    assert_eq!(
      green.submit(&replacement.at(2, 1)),
      Outcome::Accepted { version: 2 }
    );
    assert!(matches!(
      green.submit(&pending.at(3, 0)),
      Outcome::Conflict { .. }
    ));
  }
}

/// AC-6.3 / AC-6.13: an owner and a holder replay exactly the same FIFO/socket creations.
#[test]
fn declared_ipc_creations_replay_without_file_contents() {
  let mut owner = Green::new();
  let mut holder = Green::new();
  let journal = [
    make("socket", SpecialKind::Socket),
    make("pipe", SpecialKind::Fifo),
  ];
  let inc = increment(&Base::default(), 0, &journal);
  let replay = Increment::decode(&inc.encode()).unwrap();
  assert_eq!(owner.submit(&inc), Outcome::Accepted { version: 1 });
  assert_eq!(holder.submit(&replay), Outcome::Accepted { version: 1 });
  assert_eq!(owner.head_identity(), holder.head_identity());
  for (path, kind) in [("pipe", SpecialKind::Fifo), ("socket", SpecialKind::Socket)] {
    assert_eq!(owner.special(path), Some(node(kind)));
    assert_eq!(owner.mode(path), Some(0o640));
    assert_eq!(owner.content(path), None);
  }
  let reverse = increment(
    &Base::default(),
    0,
    &[journal[1].clone(), journal[0].clone()],
  );
  assert_eq!(inc.encode(), reverse.encode());
}

/// AC-6.2: incompatible concurrent kinds conflict; equal metadata converges without losing kind.
#[test]
fn ipc_creation_conflicts_with_files_directories_and_other_ipc_kinds() {
  for collision in [
    vec![VolumeOp::Create {
      path: "pipe".into(),
    }],
    vec![VolumeOp::Mkdir {
      path: "pipe".into(),
    }],
    vec![make("pipe", SpecialKind::Socket)],
  ] {
    let mut green = Green::new();
    let pipe = increment(&Base::default(), 0, &[make("pipe", SpecialKind::Fifo)]);
    assert_eq!(green.submit(&pipe), Outcome::Accepted { version: 1 });
    let identity = green.head_identity();
    let other = increment(&Base::default(), 0, &collision);
    assert!(matches!(green.submit(&other), Outcome::Conflict { .. }));
    assert_eq!(green.head_identity(), identity);
    assert_eq!(green.submit(&pipe), Outcome::Accepted { version: 1 });
  }
}

/// AC-6.8: metadata changes and removal preserve old versions and precise invalidation paths.
#[test]
fn ipc_metadata_and_removal_preserve_pinned_versions_and_balanced_retention() {
  let mut green = Green::new();
  let created = increment(&Base::default(), 0, &[make("pipe", SpecialKind::Fifo)]);
  assert_eq!(green.submit(&created), Outcome::Accepted { version: 1 });
  let first = green.current_base();
  let chmod = increment(
    &first,
    1,
    &[VolumeOp::SetMode {
      path: "pipe".into(),
      mode: 0o600,
    }],
  );
  assert_eq!(green.submit(&chmod), Outcome::Accepted { version: 2 });
  let stale_unlink = increment(
    &first,
    1,
    &[VolumeOp::Unlink {
      path: "pipe".into(),
    }],
  );
  assert!(matches!(
    green.submit(&stale_unlink),
    Outcome::Conflict { .. }
  ));
  let unlink = increment(
    &green.current_base(),
    2,
    &[VolumeOp::Unlink {
      path: "pipe".into(),
    }],
  );
  assert_eq!(green.submit(&unlink), Outcome::Accepted { version: 3 });
  assert_eq!(
    (green.special("pipe"), green.mode("pipe")),
    (None, None),
    "an unlinked IPC inode has no live metadata"
  );
  assert_eq!(green.base_at(1), first);
  assert_eq!(green.changed_between(0, 3), vec!["pipe"]);
  assert_eq!(
    green.retained_bytes().history,
    green.history_bytes_recounted()
  );
  fold_removed_ipc_history(&mut green);
}

fn fold_removed_ipc_history(green: &mut Green) {
  green.fold_history_before(3);
  assert_eq!(
    green.retained_bytes().history,
    green.history_bytes_recounted()
  );
  assert!(green.base_at(3).specials.is_empty());
}

/// AC-6.8 / AUD-16: a one-byte edit and a payload-free unlink retain old values; reserve those
/// values before applying either operation, so a small request cannot escape memory admission.
#[test]
fn short_edits_and_unlinks_reserve_the_values_they_supersede() {
  let origin = Origin {
    files: vec![("big".into(), vec![b'.'; 2048])],
    specials: vec![("pipe".into(), node(SpecialKind::Fifo))],
    ..Origin::default()
  };
  let mut green = Green::with_origin(&origin);
  let mut build = common::Build::new();
  build.overwrite("big", 0, b"x");
  let edit = build.with_id([1; 32], 0);
  assert_reserved_history_covers_apply(&mut green, &edit);
  let unlink = increment(
    &green.current_base(),
    1,
    &[VolumeOp::Unlink {
      path: "pipe".into(),
    }],
  );
  assert_reserved_history_covers_apply(&mut green, &unlink);
}

fn assert_reserved_history_covers_apply(green: &mut Green, inc: &Increment) {
  let reserved = green.history_reservation(inc);
  let before = green.retained_bytes().history;
  assert!(matches!(green.submit(inc), Outcome::Accepted { .. }));
  let added = green.retained_bytes().history - before;
  assert!(
    added > inc.post_state.len(),
    "the incoming payload does not bound retention"
  );
  assert_eq!(reserved, added);
  assert_eq!(
    green.history_reservation(inc),
    0,
    "an accepted retry retains nothing new"
  );
}

/// AC-6.2: composing create/unlink cancels; content operations on either IPC kind refuse.
#[test]
fn ipc_creation_cancels_and_content_never_composes_on_an_ipc_name() {
  let canceled = compose_volume(
    &Base::default(),
    &[
      make("pipe", SpecialKind::Fifo),
      VolumeOp::Unlink {
        path: "pipe".into(),
      },
    ],
  )
  .unwrap();
  assert!(canceled.ops.is_empty());
  let base = Base {
    specials: vec![("pipe".into(), node(SpecialKind::Fifo))],
    ..Base::default()
  };
  for op in [
    VolumeOp::Create {
      path: "pipe".into(),
    },
    VolumeOp::Overwrite {
      path: "pipe".into(),
      at: 0,
      len: 1,
    },
    VolumeOp::Truncate {
      path: "pipe".into(),
      len: 0,
    },
  ] {
    assert!(compose_volume(&base, &[op]).is_err());
  }
}

/// AC-6.8: a clean rebase restates an IPC creation and keeps its metadata and mode intact.
#[test]
fn an_ipc_creation_rebases_without_becoming_an_empty_file() {
  let mut green = Green::new();
  let pending = increment(&Base::default(), 0, &[make("pipe", SpecialKind::Fifo)]);
  let directory = increment(&Base::default(), 0, &[VolumeOp::Mkdir { path: "d".into() }]);
  green.submit(&directory);
  let Rebased::Rebased {
    version,
    files,
    journal,
  } = green.rebase(&pending)
  else {
    panic!("disjoint work rebases");
  };
  assert!(files.is_empty());
  let rebased = increment(&green.current_base(), version, &journal);
  assert_eq!(green.submit(&rebased), Outcome::Accepted { version: 2 });
  assert_eq!(green.special("pipe"), Some(node(SpecialKind::Fifo)));
  assert_eq!(green.mode("pipe"), Some(0o640));
}

/// AC-6.13: corrupted IPC metadata is refused before any holder can apply it.
#[test]
fn hostile_ipc_metadata_refuses_unknown_kinds_and_every_truncation() {
  let bytes = node(SpecialKind::Socket).encode();
  for cut in 0..bytes.len() {
    assert!(SpecialNode::decode(&bytes[..cut]).is_err());
  }
  let mut invalid = bytes.clone();
  invalid[0] = u8::MAX;
  assert_eq!(SpecialNode::decode(&invalid), Err(DocDecodeError::BadKind));
  let mut trailing = bytes;
  trailing.push(0);
  assert_eq!(
    SpecialNode::decode(&trailing),
    Err(DocDecodeError::TrailingBytes)
  );
  let mut inc = increment(&Base::default(), 0, &[make("pipe", SpecialKind::Fifo)]);
  inc.post_state[0] = u8::MAX;
  assert_eq!(
    Increment::decode(&inc.encode()),
    Err(DocDecodeError::BadKind)
  );
  assert!(matches!(
    Green::new().submit(&inc),
    Outcome::Conflict { .. }
  ));
}

/// AC-6.13: a malformed IPC path reference cannot create an unnamed inode during holder replay.
#[test]
fn an_ipc_creation_with_an_invalid_path_reference_is_refused() {
  let mut bad_path = increment(&Base::default(), 0, &[make("pipe", SpecialKind::Fifo)]);
  bad_path
    .doc
    .ops
    .iter_mut()
    .find(|op| op.kind == OpKind::Mknod)
    .unwrap()
    .path = u16::MAX;
  assert_eq!(
    Increment::decode(&bad_path.encode()),
    Err(DocDecodeError::BadPath)
  );
  assert!(matches!(
    Green::new().submit(&bad_path),
    Outcome::Conflict { .. }
  ));
}

/// AC-6.13: the fixed IPC metadata encoding is byte-for-byte portable, including negative time.
#[test]
fn ipc_metadata_matches_its_canonical_golden_vector() {
  let hex: String = node(SpecialKind::Socket)
    .encode()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  assert_eq!(
    hex,
    "017b000000c8010000ffffffffffffffff020000000000000003000000000000000000000000000000"
  );
}

/// AC-6.13: adding sorted IPC names cannot redirect a symlink through a stale path-table index.
#[test]
fn ipc_names_preserve_other_namespace_references_when_paths_are_sorted() {
  let origin = Origin {
    specials: vec![("a".into(), node(SpecialKind::Fifo))],
    ..Origin::default()
  };
  let mut green = Green::with_origin(&origin);
  let journal = [
    VolumeOp::Unlink { path: "a".into() },
    VolumeOp::Symlink {
      path: "z".into(),
      target: "target".into(),
    },
  ];
  let inc = increment(&green.current_base(), 0, &journal);
  assert_eq!(green.submit(&inc), Outcome::Accepted { version: 1 });
  assert_eq!(green.symlink("z"), Some("target"));
}
