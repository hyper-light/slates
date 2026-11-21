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
  let bytes: Vec<u8> = (0..TEXT)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let mut work = named_create("big", object);
  let mut green = Green::new();
  assert_eq!(
    green.submit(&work),
    Outcome::ContentUnread { identity: object }
  );
  let steps = read_in_steps(&mut work, &plane, SLICE);
  assert_eq!(steps, (TEXT as u64).div_ceil(SLICE));
  assert!(
    plane.largest.get() <= usize::try_from(SLICE).unwrap(),
    "{}",
    plane.largest.get()
  );
  assert!(work.unread().is_none());
  let encoded = work.encode();
  assert!(encoded.len() < 4096, "{} bytes encoded", encoded.len());
  assert_eq!(green.submit(&work), Outcome::Accepted { version: 1 });
  assert_eq!(green.content("big"), Some(bytes.as_slice()));
  round_trips_by_name(&work, &encoded, object);
}

/// `encoded`, `work`'s encoding, decodes to its names alone: no post-state, the run of `object` unread again.
fn round_trips_by_name(work: &Increment, encoded: &[u8], object: Identity) {
  let decoded = Increment::decode(encoded).unwrap();
  assert_eq!(decoded.named, work.named);
  assert!(decoded.post_state.is_empty());
  assert_eq!(decoded.unread().map(|run| run.identity), Some(object));
}

/// Reads `work`'s unread run from `plane` `slice` bytes a step until it is read whole: the steps taken.
fn read_in_steps(work: &mut Increment, plane: &Plane, slice: u64) -> u64 {
  let mut steps = 1u64;
  while !work.read_step(plane, slice).unwrap() {
    steps += 1;
  }
  steps
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

/// A-760 part 2 (Ada's sizing rule: a landing holds a large text once, beside the plane). Do: name a 4 MiB text a work
/// creates, read it, judge it with `judge_named` and commit the verdict. Expect: accepted as version 1, the green's file
/// the text's bytes, made of the very buffer the run was read into (the same allocation: no copy, the work left holding
/// none), and a verdict that never built the file (the work's own buffer is the only one made).
#[test]
fn a_named_texts_buffer_becomes_the_greens_file_with_no_copy() {
  const TEXT: usize = 4 << 20;
  let bytes: Vec<u8> = (0..TEXT)
    .map(|at| u8::try_from(at % 241).unwrap())
    .collect();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let mut work = named_create("big", object);
  while !work.read_step(&plane, 1 << 20).unwrap() {}
  let buffer = work.post().runs[0].as_ptr();
  let mut green = Green::new();
  let judged = match green.judge_named(&mut work) {
    super::Judgment::Accepted(judged) => judged,
    other => panic!("not accepted: {other:?}"),
  };
  assert!(
    work.post().runs.iter().all(Vec::is_empty),
    "the work kept its bytes"
  );
  assert_eq!(
    green.commit_judged(judged).map(|(version, _)| version),
    Ok(1)
  );
  let file = green.content("big").unwrap();
  assert_eq!(file, bytes.as_slice());
  assert_eq!(file.as_ptr(), buffer, "the file is not the run's buffer");
}

/// A-760 part 2. Do: create two files from one named run (the same text twice) and commit. Expect: both files the text's
/// bytes: a run two files make is copied for them, never moved out from under the other.
#[test]
fn a_run_two_files_make_is_copied_never_moved_twice() {
  let bytes = b"the same text".to_vec();
  let mut plane = Plane::default();
  let object = plane.put(bytes.clone());
  let journal = vec![
    VolumeOp::Create {
      path: "a".to_owned(),
    },
    VolumeOp::Extend {
      path: "a".to_owned(),
      at: 0,
      len: object.len,
    },
    VolumeOp::Create {
      path: "b".to_owned(),
    },
    VolumeOp::Extend {
      path: "b".to_owned(),
      at: 0,
      len: object.len,
    },
  ];
  let (mut doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
  // Both files' runs read from the one named run: each adding op's source is its offset.
  for op in &mut doc.ops {
    op.src = 0;
  }
  let mut work = Increment {
    id: [3; 32],
    base: 0,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  work.push_named(NamedRun {
    identity: object,
    offset: 0,
    len: object.len,
  });
  while !work.read_step(&plane, 4).unwrap() {}
  let mut green = Green::new();
  let super::Judgment::Accepted(judged) = green.judge_named(&mut work) else {
    panic!("not accepted");
  };
  green.commit_judged(judged).unwrap();
  assert_eq!(green.content("a"), Some(bytes.as_slice()));
  assert_eq!(green.content("b"), Some(bytes.as_slice()));
}

/// A-760 part 2's bug, found 2026-11-20 closing A-760 (C1). Do: a work whose file `a` is the whole of named run 0 (its
/// buffer moved into the file at the commit) and whose file `b` is the first five bytes of run 1, addressed past run 0
/// in the post-state's one address space; judge it with `judge_named` and commit. Expect: `a` run 0's bytes and `b` run
/// 1's first five. Before the fix the commit moved run 0's buffer out before making `b`, so every later run's bytes moved
/// down by run 0's length and `b` was read from the wrong place.
#[test]
fn a_run_moved_into_its_file_leaves_later_runs_where_they_were() {
  let first = b"the first text, moved whole".to_vec();
  let second = b"second text, of which five bytes".to_vec();
  let mut plane = Plane::default();
  let first_object = plane.put(first.clone());
  let second_object = plane.put(second.clone());
  let journal = vec![
    VolumeOp::Create {
      path: "a".to_owned(),
    },
    VolumeOp::Extend {
      path: "a".to_owned(),
      at: 0,
      len: first_object.len,
    },
    VolumeOp::Create {
      path: "b".to_owned(),
    },
    VolumeOp::Extend {
      path: "b".to_owned(),
      at: 0,
      len: 5,
    },
  ];
  let (doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
  let mut work = Increment {
    id: [5; 32],
    base: 0,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  assert_eq!(
    work.push_named(NamedRun {
      identity: first_object,
      offset: 0,
      len: first_object.len,
    }),
    Some(0)
  );
  assert_eq!(
    work.push_named(NamedRun {
      identity: second_object,
      offset: 0,
      len: second_object.len,
    }),
    Some(first_object.len)
  );
  while !work.read_step(&plane, 8).unwrap() {}
  let mut green = Green::new();
  let super::Judgment::Accepted(judged) = green.judge_named(&mut work) else {
    panic!("not accepted");
  };
  green.commit_judged(judged).unwrap();
  assert_eq!(green.content("a"), Some(first.as_slice()));
  assert_eq!(green.content("b"), second.get(..5));
}

/// A-760's last piece (C1, 2026-11-20: "a partial splice of a run copies at the commit"). Do: a green holding a 1 MiB
/// file; a work that overwrites 5 bytes in its middle from named run 0 and creates a file of the whole of named run 1;
/// judge it with `judge_named`, make its files with `make_step` a 64 KiB slice a step, then commit. Expect: no step
/// copies more than its slice, and the steps together copy the spliced file once (1 MiB); the commit makes no file
/// (`mapped_files_made` unmoved, `files_made_in_steps` one more); the spliced file is the base with run 0's five bytes
/// in its middle, the created one run 1's bytes in run 1's own buffer.
#[test]
fn a_splice_into_a_large_file_is_made_a_slice_a_step_before_its_commit() {
  const FILE: usize = 1 << 20;
  const SLICE: u64 = 64 << 10;
  let base: Vec<u8> = (0..FILE)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let mut green = Green::new();
  let (doc, _) = compose_volume_planned(
    &Base::of_files(Vec::new()),
    &[
      VolumeOp::Create {
        path: "big".to_owned(),
      },
      VolumeOp::Extend {
        path: "big".to_owned(),
        at: 0,
        len: FILE as u64,
      },
    ],
  )
  .unwrap();
  let seed = Increment {
    id: [1; 32],
    base: 0,
    doc,
    post_state: base.clone(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  let super::Judgment::Accepted(seeded) = green.judge_named(&mut seed.clone()) else {
    panic!("the seed was not accepted");
  };
  green.commit_judged(seeded).unwrap();

  let whole = b"a whole text, moved".to_vec();
  let splice = b"FIVE!".to_vec();
  let mut plane = Plane::default();
  let whole_object = plane.put(whole.clone());
  let splice_object = plane.put(splice.clone());
  let middle = (FILE / 2) as u64;
  let (doc, _) = compose_volume_planned(
    &Base::of_files(vec![("big".to_owned(), FILE as u64)]),
    &[
      VolumeOp::Create {
        path: "new".to_owned(),
      },
      VolumeOp::Extend {
        path: "new".to_owned(),
        at: 0,
        len: whole_object.len,
      },
      VolumeOp::Overwrite {
        path: "big".to_owned(),
        at: middle,
        len: 5,
      },
    ],
  )
  .unwrap();
  let mut work = Increment {
    id: [2; 32],
    base: 1,
    doc,
    post_state: Vec::new(),
    evidence: Vec::new(),
    named: Default::default(),
  };
  // The composed post-state holds `big`'s five bytes first, then `new`'s (its ops' sources): the runs in that order.
  assert_eq!(
    work.push_named(NamedRun {
      identity: splice_object,
      offset: 0,
      len: splice_object.len,
    }),
    Some(0)
  );
  assert_eq!(
    work.push_named(NamedRun {
      identity: whole_object,
      offset: 0,
      len: whole_object.len,
    }),
    Some(splice_object.len)
  );
  while !work.read_step(&plane, 8).unwrap() {}
  let whole_buffer = work.post().runs[1].as_ptr();
  let super::Judgment::Accepted(mut judged) = green.judge_named(&mut work) else {
    panic!("not accepted");
  };
  let (made_before, stepped_before) = (green.mapped_files_made(), green.files_made_in_steps());
  let mut copied = 0u64;
  let mut steps = 0u64;
  loop {
    let made = green.make_step(&mut judged, SLICE).unwrap();
    assert!(made.copied <= SLICE, "a step copied {} bytes", made.copied);
    copied += made.copied;
    steps += 1;
    if made.done {
      break;
    }
  }
  assert_eq!(copied, FILE as u64, "the spliced file copied once");
  assert!(steps >= FILE as u64 / SLICE, "{steps} steps");
  assert_eq!(green.commit_judged(judged).map(|(v, _)| v), Ok(2));
  assert_eq!(
    green.mapped_files_made(),
    made_before,
    "the commit made a file"
  );
  assert_eq!(green.files_made_in_steps(), stepped_before + 1);
  let mut expected = base;
  expected[FILE / 2..FILE / 2 + 5].copy_from_slice(&splice);
  assert!(
    green.content("big") == Some(expected.as_slice()),
    "the spliced file differs"
  );
  let created = green.content("new").unwrap();
  assert_eq!(created, whole.as_slice());
  assert_eq!(created.as_ptr(), whole_buffer, "the whole run was copied");
}
