//! The update-report drive's tests (A-627): a drive of every call `update.c` writes in send-all mode, byte for byte
//! against its format strings, read back as well-formed XML; and the header when neither mode is on.

use super::*;
use crate::dav::xml;
use crate::dav::{Bounds, SVN};
use crate::svndiff::Instruction;

/// Room no drive here comes near.
const ROOM: usize = 1 << 16;

/// One window making `hello\n` from nothing.
fn hello() -> Window {
  Window {
    source_offset: 0,
    source_len: 0,
    target_len: 6,
    instructions: vec![Instruction::New { length: 6 }],
    new_data: b"hello\n".to_vec(),
  }
}

/// A checkout-and-update drive with every call: the anchor, an added file with its text, an added directory, an opened
/// file with a binary property and a removal, a deletion, and an absent file.
fn drive() -> Vec<u8> {
  let mut drive = Drive::new(true, true, ROOM).unwrap();
  drive.target_revision(5).unwrap();
  drive.open_root(4, Some("/repo/!svn/ver/5/trunk")).unwrap();
  drive.set_prop("svn:entry:committed-rev", b"5").unwrap();
  let file = Added {
    sha1: Some("f572d396fae9206628714fb2ce00f72e94f2258f"),
    ..Added::default()
  };
  drive
    .add(
      NodeKind::File,
      "f.txt",
      file,
      "/repo/!svn/ver/5/trunk/f.txt",
    )
    .unwrap();
  drive.set_prop("svn:eol-style", b"native").unwrap();
  drive.txdelta(None, &[hello()], Version::Zero).unwrap();
  drive
    .close_file(true, Some("b1946ac92492d2347c6235b4d2611184"))
    .unwrap();
  let directory = Added {
    bc_url: Some("/repo/!svn/bc/5/trunk/d"),
    ..Added::default()
  };
  drive
    .add(
      NodeKind::Directory,
      "d",
      directory,
      "/repo/!svn/ver/5/trunk/d",
    )
    .unwrap();
  drive.close_directory(true).unwrap();
  drive
    .open(NodeKind::File, "g.txt", 3, "/repo/!svn/ver/4/trunk/g.txt")
    .unwrap();
  drive.set_prop("bin", &[0xff]).unwrap();
  drive.remove_prop("old").unwrap();
  drive.close_file(false, None).unwrap();
  drive.delete_entry("gone", 4).unwrap();
  drive.absent(NodeKind::File, "secret").unwrap();
  drive.close_directory(false).unwrap();
  drive.finish().unwrap()
}

/// A-627 (`reports/update.c`, send-all mode): do write a drive of every call; expect `update.c`'s bytes: the header with
/// both modes, each element and its checked-in href, the binary property in base64 with line breaks, the delta in base64
/// with none, and the text's MD5 before the file's close.
#[test]
fn a_drive_is_written_as_update_c_writes_it() {
  let expected = concat!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
    "<S:update-report xmlns:S=\"svn:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:D=\"DAV:\" ",
    "send-all=\"true\" inline-props=\"true\">\n",
    "<S:target-revision rev=\"5\"/>\n",
    "<S:open-directory rev=\"4\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/5/trunk</D:href></D:checked-in>\n",
    "<S:set-prop name=\"svn:entry:committed-rev\">5</S:set-prop>\n",
    "<S:add-file name=\"f.txt\" sha1-checksum=\"f572d396fae9206628714fb2ce00f72e94f2258f\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/5/trunk/f.txt</D:href></D:checked-in>\n",
    "<S:set-prop name=\"svn:eol-style\">native</S:set-prop>\n",
    "<S:txdelta>U1ZOAAAABgEGhmhlbGxvCg==</S:txdelta>",
    "<S:prop><V:md5-checksum>b1946ac92492d2347c6235b4d2611184</V:md5-checksum></S:prop></S:add-file>\n",
    "<S:add-directory name=\"d\" bc-url=\"/repo/!svn/bc/5/trunk/d\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/5/trunk/d</D:href></D:checked-in>\n",
    "</S:add-directory>\n",
    "<S:open-file name=\"g.txt\" rev=\"3\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/4/trunk/g.txt</D:href></D:checked-in>\n",
    "<S:set-prop name=\"bin\" encoding=\"base64\">\n/w==\n</S:set-prop>\n",
    "<S:remove-prop name=\"old\"/>\n",
    "</S:open-file>\n",
    "<S:delete-entry name=\"gone\" rev=\"4\"/>\n",
    "<S:absent-file name=\"secret\"/>\n",
    "</S:open-directory>\n",
    "</S:update-report>\n",
  );
  assert_eq!(String::from_utf8(drive()).unwrap(), expected);
}

/// A-627 (a client parses the drive): do read the drive back with the reader; expect it well formed, rooted at
/// `svn:update-report`.
#[test]
fn a_drive_reads_back_as_well_formed_xml() {
  let bounds = Bounds {
    body_bytes: ROOM,
    depth: 16,
    name_bytes: 64,
    attributes: 8,
    elements: 256,
  };
  let root = xml::parse(&drive(), &bounds).unwrap();
  assert_eq!(
    (root.namespace.as_str(), root.name.as_str()),
    (SVN, "update-report")
  );
}

/// A-627 (`maybe_start_update_report`): do start a drive with neither mode, a copied directory and a copied file; expect
/// the two empty `%s` leaving their spaces, and each copy source after the other attributes.
#[test]
fn the_header_without_modes_keeps_its_spaces_and_a_copy_names_its_source() {
  let mut drive = Drive::new(false, false, ROOM).unwrap();
  let copied = Added {
    copy_from: Some(("/trunk/d", 3)),
    ..Added::default()
  };
  drive
    .add(NodeKind::Directory, "copy", copied, "/repo/!svn/ver/5/copy")
    .unwrap();
  let written = String::from_utf8(drive.finish().unwrap()).unwrap();
  assert!(written.contains("xmlns:D=\"DAV:\"  >\n"), "{written}");
  assert!(
    written
      .contains("<S:add-directory name=\"copy\" copyfrom-path=\"/trunk/d\" copyfrom-rev=\"3\">\n"),
    "{written}"
  );
}
