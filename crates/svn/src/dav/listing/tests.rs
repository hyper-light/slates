//! `list-report` and `file-revs-report`'s tests (A-627): the bodies stock svn 1.15.0 sent for `svn ls -v` and `svn
//! blame` (recorded on the Mac Studio, 2026-11-19), every cut of each, and each answer byte for byte as `list.c` and
//! `file-revs.c` write it.

use super::*;
use crate::svndiff::Instruction;

/// Bounds no captured body comes near.
const ROOMY: Bounds = Bounds {
  body_bytes: 1 << 16,
  depth: 16,
  name_bytes: 128,
  attributes: 16,
  elements: 1_000,
};

/// `svn ls -v URL`'s list-report.
const LIST: &[u8] = include_bytes!("../fixtures/list-report.xml");

/// `svn blame URL`'s file-revs-report.
const FILE_REVS: &[u8] = include_bytes!("../fixtures/file-revs-report.xml");

/// A-627 (`list.c`): do decode `svn ls -v`'s list-report; expect the target's path at revision 5, depth immediates,
/// and the kind, size, created revision, date and author asked for, not whether there are properties.
#[test]
fn the_captured_list_report_asks_for_the_fields_svn_ls_shows() {
  let expected = ListReport {
    path: String::new(),
    revision: Some(5),
    depth: SvnDepth::Immediates,
    patterns: None,
    fields: DirentFields {
      kind: true,
      size: true,
      has_props: false,
      created_rev: true,
      time: true,
      last_author: true,
    },
  };
  assert_eq!(list_report(LIST, &ROOMY), Ok(expected));
}

/// A-627 (`file-revs.c`): do decode `svn blame`'s file-revs-report; expect revisions 0 to 5 of the target itself.
#[test]
fn the_captured_file_revs_report_names_its_range() {
  let expected = FileRevsReport {
    path: String::new(),
    start: Some(0),
    end: Some(5),
    include_merged_revisions: false,
  };
  assert_eq!(file_revs_report(FILE_REVS, &ROOMY), Ok(expected));
}

/// A-627 (a body cut anywhere): do decode every proper prefix of both captured bodies; expect each refused.
#[test]
fn every_cut_of_both_captured_reports_is_refused() {
  let lists = (1..LIST.len())
    .filter(|&cut| list_report(&LIST[..cut], &ROOMY).is_ok())
    .count();
  let revs = (1..FILE_REVS.len())
    .filter(|&cut| file_revs_report(&FILE_REVS[..cut], &ROOMY).is_ok())
    .count();
  assert_eq!((lists, revs), (0, 0));
}

/// A-627 (`list.c` `list_receiver`): do answer an entry with every field and one with its kind alone; expect each
/// attribute in `list.c`'s order and the author as an element.
#[test]
fn a_list_answer_is_written_as_list_c_writes_it() {
  let items = [
    ListItem {
      path: "f.txt".to_owned(),
      kind: "file",
      size: Some(6),
      has_props: Some(false),
      created_rev: Some(5),
      date: Some(AprTime(1_700_000_000_000_000)),
      author: Some("alice".to_owned()),
    },
    ListItem {
      path: "d".to_owned(),
      kind: "dir",
      size: None,
      has_props: None,
      created_rev: None,
      date: None,
      author: None,
    },
  ];
  let written = String::from_utf8(list(&items, 1 << 16).unwrap()).unwrap();
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:list-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
    <S:item node-kind=\"file\" size=\"6\" has-props=\"false\" created-rev=\"5\" \
    date=\"2023-11-14T22:13:20.000000Z\">f.txt<D:creator-displayname>alice</D:creator-displayname></S:item>\n\
    <S:item node-kind=\"dir\">d</S:item>\n</S:list-report>\n";
  assert_eq!(written, expected);
}

/// A-627 (`file-revs.c` `file_rev_handler`, `send_prop`): do answer one revision with a revision property, a property
/// set, one XML cannot carry and one removed, and a text of one window; expect `file-revs.c`'s elements, the binary
/// value in base64 with its line break, and the delta's svndiff in base64 with none.
#[test]
fn a_file_revs_answer_carries_its_properties_and_its_delta_in_flat_base64() {
  let window = Window {
    source_offset: 0,
    source_len: 0,
    target_len: 6,
    instructions: vec![Instruction::New { length: 6 }],
    new_data: b"hello\n".to_vec(),
  };
  let file_rev = FileRev {
    path: "/trunk/f.txt".to_owned(),
    revision: 5,
    rev_props: vec![("svn:author".to_owned(), b"alice".to_vec())],
    props: vec![
      ("svn:eol-style".to_owned(), Some(b"native".to_vec())),
      ("bin".to_owned(), Some(vec![0xff])),
      ("gone".to_owned(), None),
    ],
    merged: false,
    delta: Some(vec![window]),
  };
  let written = String::from_utf8(file_revs(&[file_rev], Version::Zero, 1 << 16).unwrap()).unwrap();
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:file-revs-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
    <S:file-rev path=\"/trunk/f.txt\" rev=\"5\">\n<S:rev-prop name=\"svn:author\">alice</S:rev-prop>\n\
    <S:set-prop name=\"svn:eol-style\">native</S:set-prop>\n<S:set-prop name=\"bin\" encoding=\"base64\">/w==\n\
    </S:set-prop>\n<S:remove-prop name=\"gone\"/>\n<S:txdelta>U1ZOAAAABgEGhmhlbGxvCg==</S:txdelta></S:file-rev>\n\
    </S:file-revs-report>\n";
  assert_eq!(written, expected);
}
