//! A-760's tests: an increment whose content is named by identity, read in bounded steps, then judged and committed.

use std::cell::Cell;
use std::collections::BTreeMap;

use super::{Green, Increment, Outcome};
use crate::increment::{Base, VolumeOp, compose_volume_planned};
use crate::named::{Identity, NamedRun, ObjectMissing, Objects};

/// A content plane as a map of objects by identity, counting its reads and the largest.
#[derive(Default)]
struct Plane {
  objects: BTreeMap<[u8; 32], Vec<u8>>,
  reads: Cell<u64>,
  largest: Cell<usize>,
}

impl Plane {
  /// Holds `bytes`; their identity.
  fn put(&mut self, bytes: Vec<u8>) -> Identity {
    let identity = Identity {
      hash: *blake3::hash(&bytes).as_bytes(),
      len: bytes.len() as u64,
    };
    self.objects.insert(identity.hash, bytes);
    identity
  }
}

impl Objects for Plane {
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing> {
    self.reads.set(self.reads.get() + 1);
    self.largest.set(self.largest.get().max(into.len()));
    let missing = ObjectMissing {
      identity: *identity,
    };
    let bytes = self.objects.get(&identity.hash).ok_or(missing)?;
    let at = usize::try_from(at).map_err(|_| missing)?;
    let from = bytes.get(at..at + into.len()).ok_or(missing)?;
    into.copy_from_slice(from);
    Ok(())
  }
}

/// A work creating `path` whose bytes are the whole of `object`, named rather than carried.
fn named_create(path: &str, object: Identity) -> Increment {
  let journal = vec![
    VolumeOp::Create {
      path: path.to_owned(),
    },
    VolumeOp::Extend {
      path: path.to_owned(),
      at: 0,
      len: object.len,
    },
  ];
  let (doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
  let mut work = Increment {
    id: [7; 32],
    base: 0,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  let src = work.push_named(NamedRun {
    identity: object,
    offset: 0,
    len: object.len,
  });
  assert_eq!(src, Some(0), "the run is the post-state's first bytes");
  work
}

/// A-760 (main's ruling ~09:10: a large text's landing applied in bounded steps). Do: name a 4 MiB text a work creates;
/// submit it unread; read its run 64 KiB a step from the plane; submit it again; encode and decode it. Expect: refused
/// `ContentUnread` before any verdict while unread; read in 64 steps, no read past 64 KiB; then accepted, the green's
/// file the text's bytes; its encoding the names alone (shorter than the text), decoded with its run unread again.
#[test]
fn a_named_text_is_read_a_slice_a_step_then_lands_whole() {
  const TEXT: usize = 4 << 20;
  const SLICE: u64 = 64 << 10;
  let bytes: Vec<u8> = (0..TEXT).map(|at| (at % 251) as u8).collect();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let mut work = named_create("big", object);
  let mut green = Green::new();
  assert_eq!(
    green.submit(&work),
    Outcome::ContentUnread { identity: object }
  );
  let mut steps = 0u64;
  while !work.read_step(&plane, SLICE).unwrap() {
    steps += 1;
  }
  steps += 1;
  assert_eq!(steps, (TEXT as u64).div_ceil(SLICE));
  assert!(
    plane.largest.get() <= SLICE as usize,
    "{}",
    plane.largest.get()
  );
  assert!(work.unread().is_none());
  let encoded = work.encode();
  assert!(encoded.len() < 4096, "{} bytes encoded", encoded.len());
  assert_eq!(green.submit(&work), Outcome::Accepted { version: 1 });
  assert_eq!(green.content("big"), Some(bytes.as_slice()));
  let decoded = Increment::decode(&encoded).unwrap();
  assert_eq!(decoded.named, work.named);
  assert!(decoded.post_state.is_empty());
  assert_eq!(decoded.unread().map(|run| run.identity), Some(object));
}

/// A-760's hostile input. Do: decode a named work's encoding with its run count raised past the bytes left, and with a
/// run reaching past its object's length. Expect: each refused `Truncated`, nothing allocated for the wild count.
#[test]
fn a_named_runs_table_past_its_bytes_is_refused() {
  let mut plane = Plane::default();
  let object = plane.put(vec![1; 10]);
  let work = named_create("f", object);
  let encoded = work.encode();
  let table = encoded.len() - crate::named::RUN_BYTES - 8;
  let mut wild = encoded.clone();
  wild[table..table + 8].copy_from_slice(&u64::MAX.to_le_bytes());
  assert_eq!(
    Increment::decode(&wild),
    Err(crate::ops_doc::DocDecodeError::Truncated)
  );
  let mut past = encoded;
  let run_len = past.len() - 8;
  past[run_len..].copy_from_slice(&11u64.to_le_bytes());
  assert_eq!(
    Increment::decode(&past),
    Err(crate::ops_doc::DocDecodeError::Truncated)
  );
}

/// A-760. Do: ask a step of a work whose object the plane does not hold. Expect: `ObjectMissing` naming it, nothing
/// appended to the post-state.
#[test]
fn a_named_run_the_plane_lacks_is_refused_typed() {
  let plane = Plane::default();
  let object = Identity {
    hash: [9; 32],
    len: 100,
  };
  let mut work = named_create("f", object);
  assert_eq!(
    work.read_step(&plane, 64),
    Err(ObjectMissing { identity: object })
  );
  assert!(work.post_state.is_empty());
}
