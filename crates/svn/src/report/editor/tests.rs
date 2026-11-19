//! The editor writer's tests (A-616, part 2; §4.17.4 "5. The wire").
//!
//! The oracle is `svnserve` itself: every drive stock Subversion 1.15.0's `svnserve` wrote in the
//! recorded sessions (`crates/svn/fixtures/report/`, each file's SHA-256 in `SHA256SUMS`) is read
//! back into calls, its deltas decoded into windows, and written again. The writer must write the
//! same calls, and every item byte for byte, a chunk of a delta `svnserve` stored LZ4-compressed
//! aside, whose windows must decode alike. Beside them: absent entries to a client that did not
//! announce them, svndiff version 0 to a client that does not accept version 2, its chunks as
//! `svndiff.c` writes them (a window of new data alone in two, the stream's header in the first),
//! every call out of the editor's order refused with nothing written, the bound on nodes open, and
//! hostile window encodings.

use super::*;
use crate::report::drive::Facts;
use crate::report::recorded::{
  SESSIONS, Written, drive_items, items, read_drive, stock_capabilities, value_of,
};

/// The most nodes the tests' writers hold open: past the fixtures' deepest path.
const MOST_OPEN: usize = 64;

/// The drives the recordings hold, which the replay test must meet: the 22 stock checkouts,
/// updates, switches, `status -u` and diff, and eight hand-made sessions' (an unknown report
/// command, a missing path, a revision past the youngest, an absent target, the early answer, the
/// refused edit, and two asking for copy sources).
const RECORDED_DRIVES: usize = 30;

/// The calls a read drive makes, borrowing its windows. A node's facts are not on the wire, and
/// the writer takes none of them.
fn calls(written: &[Written]) -> Vec<Call<'_>> {
  let facts = Facts {
    repository_path: "",
    created_revision: 0,
    sha1: None,
  };
  let mut calls = Vec::new();
  for call in written {
    match call {
      Written::TargetRevision(revision) => calls.push(Call::TargetRevision(*revision)),
      Written::OpenRoot(base) => calls.push(Call::OpenRoot {
        base: *base,
        node: facts,
      }),
      Written::Open(kind, path, base) => calls.push(Call::Open {
        kind: *kind,
        path,
        base: *base,
        node: facts,
      }),
      Written::Add(kind, path, copy_from) => calls.push(Call::Add {
        kind: *kind,
        path,
        copy_from: copy_from
          .as_ref()
          .map(|(source, revision)| (source.as_str(), *revision)),
        node: facts,
      }),
      other => calls.extend(leaf_calls(other)),
    }
  }
  calls
}

/// The calls of a read call that opens no node.
fn leaf_calls(written: &Written) -> Vec<Call<'_>> {
  match written {
    Written::Delete(path, revision) => vec![Call::Delete {
      path,
      revision: *revision,
    }],
    Written::Absent(kind, path) => vec![Call::Absent { kind: *kind, path }],
    Written::ChangeProperty(name, value) => vec![Call::ChangeProperty {
      name,
      value: value.as_deref(),
    }],
    Written::Delta(base_md5, _, windows) => {
      let mut delta = vec![Call::ApplyTextDelta {
        base_md5: *base_md5,
      }];
      delta.extend(windows.iter().map(Call::Window));
      delta.push(Call::TextDeltaEnd);
      delta
    }
    Written::CloseFile(md5) => vec![Call::CloseFile { md5: *md5 }],
    Written::CloseDirectory => vec![Call::CloseDirectory],
    Written::CloseEdit => vec![Call::CloseEdit],
    Written::AbortEdit => vec![Call::AbortEdit],
    opening => panic!("{opening:?} opens a node"),
  }
}

/// Writes `calls` through a fresh writer for a client with `capabilities`.
fn write_all(calls: &[Call<'_>], capabilities: &ClientCapabilities) -> Vec<u8> {
  let mut writer = EditorWriter::new(capabilities, MOST_OPEN);
  let mut out = Vec::new();
  for call in calls {
    writer.write(call, &mut out).unwrap();
  }
  out
}

/// A-616 part 2 (`editorp.c` and `marshal.c`'s editor commands): read every recorded drive back
/// into calls and write them again for a stock 1.15 client; expect the same calls, the same
/// windows, and every item `svnserve`'s byte for byte, a chunk of a delta it stored LZ4-compressed
/// aside; and every one of the recordings' drives met.
#[test]
fn every_recorded_drive_is_written_again_as_svnserve_wrote_it() {
  let mut drives = 0;
  for session in SESSIONS {
    let theirs = drive_items(session.server);
    if theirs.is_empty() {
      continue;
    }
    drives += 1;
    let read = read_drive(&theirs);
    let written = write_all(&calls(&read.calls), &stock_capabilities());
    let ours = items(&written);
    assert_eq!(
      read_drive(&ours).calls,
      read.calls,
      "{}: the calls",
      session.name
    );
    assert_eq!(ours.len(), theirs.len(), "{}: the items", session.name);
    for ((ours, theirs), compressed) in ours.iter().zip(&theirs).zip(&read.compressed) {
      if !compressed {
        assert_eq!(
          String::from_utf8_lossy(ours),
          String::from_utf8_lossy(theirs),
          "{}",
          session.name
        );
      }
    }
  }
  assert_eq!(drives, RECORDED_DRIVES);
}

/// A-616 part 2 (`ra_svn_absent_dir` and `ra_svn_absent_file` skip a client without
/// `absent-entries`): write a drive holding an absent directory and an absent file for a client
/// with the capability and one without; expect both commands, then neither, the rest alike.
#[test]
fn absent_entries_go_only_to_a_client_that_announced_them() {
  let facts = Facts {
    repository_path: "/trunk",
    created_revision: 1,
    sha1: None,
  };
  let drive = [
    Call::TargetRevision(1),
    Call::OpenRoot {
      base: Some(1),
      node: facts,
    },
    Call::Absent {
      kind: NodeKind::Directory,
      path: "secret",
    },
    Call::Absent {
      kind: NodeKind::File,
      path: "hidden.txt",
    },
    Call::CloseDirectory,
    Call::CloseEdit,
  ];
  let mut without = stock_capabilities();
  without.absent_entries = false;
  let common = "( target-rev ( 1 ) ) ( open-root ( ( 1 ) 2:d0 ) ) ";
  let end = "( close-dir ( 2:d0 ) ) ( close-edit ( ) ) ";
  let absent = "( absent-dir ( 6:secret 2:d0 ) ) ( absent-file ( 10:hidden.txt 2:d0 ) ) ";
  assert_eq!(
    String::from_utf8(write_all(&drive, &stock_capabilities())).unwrap(),
    format!("{common}{absent}{end}")
  );
  assert_eq!(
    String::from_utf8(write_all(&drive, &without)).unwrap(),
    format!("{common}{end}")
  );
}

/// A-616 part 2 (`svn_ra_svn__svndiff_version`: version 2 only to a client that accepts it, never
/// version 1): write a recorded checkout's drive for a client that announced `svndiff1` but not
/// `accepts-svndiff2`; expect every delta's stream in version 0 and its windows alike.
#[test]
fn a_client_that_does_not_accept_svndiff2_is_sent_version_0() {
  let checkout = SESSIONS
    .iter()
    .find(|session| session.name == "checkout-head.1");
  let read = read_drive(&drive_items(checkout.unwrap().server));
  let mut older = stock_capabilities();
  older.accepts_svndiff2 = false;
  let written = write_all(&calls(&read.calls), &older);
  let again = read_drive(&items(&written)).calls;
  let mut deltas = 0;
  for (ours, theirs) in again.iter().zip(&read.calls) {
    match (ours, theirs) {
      (Written::Delta(base, 0, windows), Written::Delta(their_base, 2, their_windows)) => {
        assert_eq!((base, windows), (their_base, their_windows));
        deltas += 1;
      }
      _ => assert_eq!(ours, theirs),
    }
  }
  assert_eq!(again.len(), read.calls.len());
  assert!(deltas > 0, "the checkout holds deltas");
}

/// Every `textdelta-chunk`'s bytes in `written`, in order.
fn chunks_of(written: &[u8]) -> Vec<Vec<u8>> {
  items(written)
    .into_iter()
    .filter_map(|item| {
      let Value::List(parts) = value_of(item) else {
        return None;
      };
      let [Value::Word("textdelta-chunk"), Value::List(params)] = parts.as_slice() else {
        return None;
      };
      let [_, Value::String(bytes)] = params.as_slice() else {
        return None;
      };
      Some(bytes.to_vec())
    })
    .collect()
}

/// A-616 part 2, its sibling bug of 2026-11-19 (`svndiff.c` at `d434dc14` and 1.15.0 alike:
/// `window_handler` 259-261 hands a version-0 window holding no source copy and one instruction to
/// `send_simple_insertion_window` 89-151, which writes the stream's header, when it is not out yet,
/// with the window's integers and its instruction as one chunk, then its new data as another; every
/// other window has the header written first as a chunk of its own, 263-270, then its integers, its
/// instructions and its new data, each a chunk when it is not empty, 288-300): write, for a client
/// that does not accept svndiff2, an added file's two windows of new data; an opened file's window
/// copying its base, then one of new data past the base's end; an empty added file; and an added
/// file whose one window copies no source but holds two instructions, a byte of new data and a copy
/// of the target repeating it; expect `SVN\0` with the first window's integers and instruction as
/// one chunk, its new data, the second window's integers and instruction, its new data; then
/// `SVN\0` alone, the copying window's integers and instructions, the last window's integers and
/// instruction and its new data; then `SVN\0` alone; then `SVN\0` alone, the two-instruction
/// window's integers, its instructions and its new data; and for a stock client, `SVN\x02` alone
/// first.
#[test]
fn a_version_0_window_of_new_data_alone_carries_the_streams_header_in_its_first_chunk() {
  let new_data = three_bytes();
  let copy = Window {
    source_offset: 0,
    source_len: 3,
    target_len: 3,
    instructions: vec![crate::svndiff::Instruction::Source {
      offset: 0,
      length: 3,
    }],
    new_data: Vec::new(),
  };
  let past_the_base = Window {
    source_offset: 3,
    ..three_bytes()
  };
  let repeated = Window {
    source_offset: 0,
    source_len: 0,
    target_len: 4,
    instructions: vec![
      crate::svndiff::Instruction::New { length: 1 },
      crate::svndiff::Instruction::Target {
        offset: 0,
        length: 3,
      },
    ],
    new_data: b"a".to_vec(),
  };
  let md5 = [0u8; 16];
  let added = |path| Call::Add {
    kind: NodeKind::File,
    path,
    copy_from: None,
    node: TEST_FACTS,
  };
  let drive = [
    Call::TargetRevision(1),
    Call::OpenRoot {
      base: Some(1),
      node: TEST_FACTS,
    },
    added("a.txt"),
    Call::ApplyTextDelta { base_md5: None },
    Call::Window(&new_data),
    Call::Window(&new_data),
    Call::TextDeltaEnd,
    Call::CloseFile { md5 },
    Call::Open {
      kind: NodeKind::File,
      path: "b.txt",
      base: Some(1),
      node: TEST_FACTS,
    },
    Call::ApplyTextDelta {
      base_md5: Some(md5),
    },
    Call::Window(&copy),
    Call::Window(&past_the_base),
    Call::TextDeltaEnd,
    Call::CloseFile { md5 },
    added("c.txt"),
    Call::ApplyTextDelta { base_md5: None },
    Call::TextDeltaEnd,
    Call::CloseFile { md5 },
    added("d.txt"),
    Call::ApplyTextDelta { base_md5: None },
    Call::Window(&repeated),
    Call::TextDeltaEnd,
    Call::CloseFile { md5 },
    Call::CloseDirectory,
    Call::CloseEdit,
  ];
  let mut older = stock_capabilities();
  older.accepts_svndiff2 = false;
  let expected: [&[u8]; 14] = [
    b"SVN\0\x00\x00\x03\x01\x03\x83",
    b"abc",
    b"\x00\x00\x03\x01\x03\x83",
    b"abc",
    b"SVN\0",
    b"\x00\x03\x03\x02\x00",
    b"\x03\x00",
    b"\x03\x00\x03\x01\x03\x83",
    b"abc",
    b"SVN\0",
    b"SVN\0",
    b"\x00\x00\x04\x03\x01",
    b"\x81\x43\x00",
    b"a",
  ];
  assert_eq!(chunks_of(&write_all(&drive, &older)), expected);
  let stock = chunks_of(&write_all(&drive, &stock_capabilities()));
  assert_eq!(
    stock.first().map(Vec::as_slice),
    Some(b"SVN\x02".as_slice())
  );
}

/// A call and whether the writer must take it, in a sequence a test writes.
type Step<'a> = (Call<'a>, bool);

/// The facts every test call carries.
const TEST_FACTS: Facts<'static> = Facts {
  repository_path: "/trunk",
  created_revision: 1,
  sha1: None,
};

/// A window of three new bytes.
fn three_bytes() -> Window {
  Window {
    source_offset: 0,
    source_len: 0,
    target_len: 3,
    instructions: vec![crate::svndiff::Instruction::New { length: 3 }],
    new_data: b"abc".to_vec(),
  }
}

/// Writes `steps` through a writer for a stock client, each expected taken or refused out of
/// order; a refused step must append nothing and leave the writer as a writer that never saw it.
fn expect_steps(name: &str, steps: &[Step<'_>]) {
  let mut writer = EditorWriter::new(&stock_capabilities(), MOST_OPEN);
  let mut clean = EditorWriter::new(&stock_capabilities(), MOST_OPEN);
  let (mut out, mut clean_out) = (Vec::new(), Vec::new());
  for (index, (call, taken)) in steps.iter().enumerate() {
    let before = out.len();
    let result = writer.write(call, &mut out);
    if *taken {
      assert_eq!(result, Ok(()), "{name}: step {index}");
      clean.write(call, &mut clean_out).unwrap();
    } else {
      assert_eq!(
        result,
        Err(EditorRefused::OutOfOrder),
        "{name}: step {index}"
      );
      assert_eq!(out.len(), before, "{name}: step {index} appended");
    }
  }
  assert_eq!(out, clean_out, "{name}: a refusal changed the writer");
}

/// The calls that open a root and a file inside it.
fn root_and_file<'a>() -> Vec<Step<'a>> {
  vec![
    (Call::TargetRevision(1), true),
    (
      Call::OpenRoot {
        base: Some(1),
        node: TEST_FACTS,
      },
      true,
    ),
    (
      Call::Add {
        kind: NodeKind::File,
        path: "a.txt",
        copy_from: None,
        node: TEST_FACTS,
      },
      true,
    ),
  ]
}

/// A-616 part 2 (the editor's order, `svn_delta.h`): write sequences that break it, each followed
/// by calls that keep it; expect each breaking call refused out of order with nothing written and
/// the writer as if it never came.
#[test]
fn a_call_out_of_the_editors_order_is_refused_and_changes_nothing() {
  let window = three_bytes();
  let md5 = [0u8; 16];
  let mut inside_a_file = root_and_file();
  inside_a_file.extend([
    (Call::CloseDirectory, false),
    (
      Call::Open {
        kind: NodeKind::File,
        path: "b.txt",
        base: Some(1),
        node: TEST_FACTS,
      },
      false,
    ),
    (Call::Window(&window), false),
    (Call::TextDeltaEnd, false),
    (Call::CloseEdit, false),
    (Call::ApplyTextDelta { base_md5: None }, true),
    (
      Call::ChangeProperty {
        name: "color",
        value: Some(b"red"),
      },
      false,
    ),
    (Call::CloseFile { md5 }, false),
    (Call::Window(&window), true),
    (Call::TextDeltaEnd, true),
    (Call::TextDeltaEnd, false),
    (Call::CloseFile { md5 }, true),
    (Call::CloseFile { md5 }, false),
    (Call::CloseDirectory, true),
    (Call::CloseDirectory, false),
    (Call::CloseEdit, true),
    (Call::CloseEdit, false),
    (Call::AbortEdit, true),
    (Call::AbortEdit, false),
    (Call::TargetRevision(1), false),
  ]);
  expect_steps("inside a file", &inside_a_file);
  expect_steps(
    "before the root",
    &[
      (Call::Window(&window), false),
      (Call::CloseDirectory, false),
      (Call::TargetRevision(2), true),
      (Call::TargetRevision(2), false),
      (
        Call::OpenRoot {
          base: None,
          node: TEST_FACTS,
        },
        true,
      ),
      (
        Call::OpenRoot {
          base: None,
          node: TEST_FACTS,
        },
        false,
      ),
      (Call::AbortEdit, true),
      (Call::CloseEdit, false),
    ],
  );
}

/// A-616 part 2 (the writer's bound on nodes open): open directories one inside another past the
/// bound; expect the one past it refused, nothing written, and the writer still taking a close.
#[test]
fn a_node_past_the_bound_on_nodes_open_is_refused() {
  fn open(path: &str) -> Call<'_> {
    Call::Add {
      kind: NodeKind::Directory,
      path,
      copy_from: None,
      node: TEST_FACTS,
    }
  }
  let mut writer = EditorWriter::new(&stock_capabilities(), 2);
  let mut out = Vec::new();
  let root = Call::OpenRoot {
    base: Some(1),
    node: TEST_FACTS,
  };
  writer.write(&root, &mut out).unwrap();
  writer.write(&open("a"), &mut out).unwrap();
  let before = out.len();
  assert_eq!(
    writer.write(&open("a/b"), &mut out),
    Err(EditorRefused::TooDeep)
  );
  assert_eq!(out.len(), before);
  writer.write(&Call::CloseDirectory, &mut out).unwrap();
  writer.write(&open("c"), &mut out).unwrap();
  assert!(
    String::from_utf8(out)
      .unwrap()
      .ends_with("( add-dir ( 1:c 2:d0 2:d2 ( ) ) ) ")
  );
}

/// A-616 part 2 (`notes/svndiff`'s integers, read back from the writer's own encoding): read
/// integers at their limits and past them; expect each value and length, and none for a cut, an
/// eleventh byte or a value past 64 bits.
#[test]
fn svndiff_integers_are_read_within_the_format() {
  let cases: [(&[u8], Option<(u64, usize)>); 7] = [
    (b"\x00", Some((0, 1))),
    (b"\x7f", Some((127, 1))),
    (b"\x81\x00", Some((128, 2))),
    (
      b"\x81\xff\xff\xff\xff\xff\xff\xff\xff\x7f",
      Some((u64::MAX, 10)),
    ),
    (b"\x82\xff\xff\xff\xff\xff\xff\xff\xff\x7f", None),
    (b"\x80\x80\x80\x80\x80\x80\x80\x80\x80\x80\x00", None),
    (b"\x80", None),
  ];
  for (bytes, expected) in cases {
    assert_eq!(read_integer(bytes), expected, "{bytes:?}");
  }
}

/// A-616 part 2 (hostile encodings): split encodings whose lengths do not hold; expect each
/// refused as a corrupt window, and a whole one split into its three sections.
#[test]
fn a_window_encoding_whose_lengths_do_not_hold_is_refused() {
  let corrupt = Err(EditorRefused::Window(SvndiffError::CorruptWindowHeader));
  let encodings: [&[u8]; 4] = [
    b"",
    b"\x00\x00\x03\x01",
    b"\x00\x00\x03\x01\x03\x83",
    b"\x00\x00\x03\x01\x03\x83abcd",
  ];
  for encoded in encodings {
    assert_eq!(window_sections(encoded), corrupt, "{encoded:?}");
  }
  let whole: [&[u8]; 3] = [b"\x00\x00\x03\x01\x03", b"\x83", b"abc"];
  assert_eq!(window_sections(b"\x00\x00\x03\x01\x03\x83abc"), Ok(whole));
}
