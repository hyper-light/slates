//! The editor drive's tests (A-627): one drive of every call written as `update.c` writes it in send-all mode and
//! outside it, and as `replay.c` writes it, byte for byte against their format strings; a streamed delta decoded back
//! and applied, however its windows were split; each order a drive may not take refused by kind; and hostile values
//! escaped or refused.

use super::*;
use crate::dav::xml;
use crate::dav::{Bounds, SVN};
use crate::svndiff::{Instruction, apply_all, decode_all};

/// Room no drive here comes near.
const ROOM: usize = 1 << 16;

/// `hello\n`'s MD5 and SHA-1, as Subversion writes them.
const HELLO_MD5: &str = "b1946ac92492d2347c6235b4d2611184";

/// `hello\n`'s SHA-1.
const HELLO_SHA1: &str = "f572d396fae9206628714fb2ce00f72e94f2258f";

/// A base text's MD5, as a delta names it.
const BASE_MD5: &str = "00112233445566778899aabbccddeeff";

/// A digest from its hex.
fn digest<const N: usize>(hex: &str) -> [u8; N] {
  let mut out = [0u8; N];
  for (at, byte) in out.iter_mut().enumerate() {
    *byte = u8::from_str_radix(hex.get(at * 2..at * 2 + 2).unwrap(), 16).unwrap();
  }
  out
}

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

/// A window of `count` new bytes `byte`.
fn filled(byte: u8, count: u64) -> Window {
  Window {
    source_offset: 0,
    source_len: 0,
    target_len: count,
    instructions: vec![Instruction::New { length: count }],
    new_data: vec![byte; usize::try_from(count).unwrap()],
  }
}

/// A node's facts.
fn facts(repository_path: &str, created_revision: Revision, sha1: Option<&str>) -> Facts<'_> {
  Facts {
    repository_path,
    created_revision,
    sha1: sha1.map(digest),
  }
}

/// The facts a replay drive gives, which its answer never reads.
const NO_FACTS: Facts<'static> = Facts {
  repository_path: "/",
  created_revision: 0,
  sha1: None,
};

/// An update-report's answer under `/repo` with `!svn`, by version resources unless `v2`.
fn update(send_all: bool, inline_props: bool, v2: bool) -> Drive {
  let uris = Uris {
    root_path: String::from("/repo"),
    special_uri: String::from("!svn"),
    v2,
  };
  let update = Update {
    send_all,
    inline_props,
    targeted: false,
    uris,
  };
  Drive::update(update, Version::Zero, ROOM).unwrap()
}

/// `text` decoded from base64 (RFC 4648 §4), for reading a delta back.
fn unbase64(text: &str) -> Vec<u8> {
  let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut bits = 0u32;
  let mut held = 0u32;
  let mut out = Vec::new();
  for character in text.bytes().filter(|character| *character != b'=') {
    let value = alphabet.iter().position(|known| *known == character).unwrap();
    bits = (bits << 6) | u32::try_from(value).unwrap();
    held += 6;
    if held >= 8 {
      held -= 8;
      out.push(u8::try_from((bits >> held) & 0xff).unwrap());
    }
  }
  out
}

/// The text between `start` and the next `end` in `written`.
fn between<'w>(written: &'w str, start: &str, end: &str) -> &'w str {
  let from = written.find(start).unwrap() + start.len();
  let rest = written.get(from..).unwrap();
  rest.get(..rest.find(end).unwrap()).unwrap()
}

/// An update drive of every call in send-all mode.
fn update_drive() -> String {
  let mut drive = update(true, true, false);
  drive.target_revision(5).unwrap();
  drive
    .open_root(Some(4), &facts("/trunk", 5, None))
    .unwrap();
  drive
    .change_property("svn:entry:committed-rev", Some(b"5"))
    .unwrap();
  let added = facts("/trunk/a b.txt", 5, Some(HELLO_SHA1));
  drive
    .add(NodeKind::File, "trunk/a b.txt", None, &added)
    .unwrap();
  drive
    .change_property("svn:eol-style", Some(b"native"))
    .unwrap();
  drive.apply_textdelta(None).unwrap();
  drive.window(&hello()).unwrap();
  drive.textdelta_end().unwrap();
  drive.close_file(digest(HELLO_MD5)).unwrap();
  let copied = facts("/trunk/d", 5, None);
  drive
    .add(NodeKind::Directory, "trunk/d", Some(("/trunk/c", 3)), &copied)
    .unwrap();
  drive.close_directory().unwrap();
  update_drive_rest(drive)
}

/// The rest of [`update_drive`]: an opened file with a binary property, a removal and a two-window delta against a
/// named base, then a deletion with no revision, an absent file, and the close.
fn update_drive_rest(mut drive: Drive) -> String {
  drive
    .open(NodeKind::File, "trunk/g.txt", Some(3), &facts("/trunk/g.txt", 4, None))
    .unwrap();
  drive.change_property("bin", Some(&[0xff])).unwrap();
  drive.change_property("old", None).unwrap();
  drive.apply_textdelta(Some(digest(BASE_MD5))).unwrap();
  drive.window(&filled(b'x', 5)).unwrap();
  drive.window(&filled(b'y', 7)).unwrap();
  drive.textdelta_end().unwrap();
  drive.close_file([0; 16]).unwrap();
  drive.delete_entry("trunk/gone", None).unwrap();
  drive.absent(NodeKind::File, "trunk/secret").unwrap();
  drive.close_directory().unwrap();
  drive.close_edit().unwrap();
  String::from_utf8(drive.finish().unwrap()).unwrap()
}

/// A-627 (`reports/update.c`, send-all mode): do write a drive of every call; expect `update.c`'s bytes: the header,
/// each entry by its basename with the checked-in href `dav_svn__build_uri` builds from its facts (the path
/// URI-encoded), the added directory's bc-url with its copy source, the added file's SHA-1, the binary property in
/// base64 with line breaks, both deltas in base64 with none, the text's MD5 (none for a zero digest), and the
/// deletion's missing revision as -1.
#[test]
fn an_update_drive_is_written_as_update_c_writes_it() {
  let two_windows = crate::dav::listing::delta(&[filled(b'x', 5), filled(b'y', 7)], Version::Zero).unwrap();
  let expected = [
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
    "<S:update-report xmlns:S=\"svn:\" xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:D=\"DAV:\" ",
    "send-all=\"true\" inline-props=\"true\">\n",
    "<S:target-revision rev=\"5\"/>\n",
    "<S:open-directory rev=\"4\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/5/trunk</D:href></D:checked-in>\n",
    "<S:set-prop name=\"svn:entry:committed-rev\">5</S:set-prop>\n",
    "<S:add-file name=\"a b.txt\" sha1-checksum=\"f572d396fae9206628714fb2ce00f72e94f2258f\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/5/trunk/a%20b.txt</D:href></D:checked-in>\n",
    "<S:set-prop name=\"svn:eol-style\">native</S:set-prop>\n",
    "<S:txdelta>U1ZOAAAABgEGhmhlbGxvCg==</S:txdelta>",
    "<S:prop><V:md5-checksum>b1946ac92492d2347c6235b4d2611184</V:md5-checksum></S:prop></S:add-file>\n",
    "<S:add-directory name=\"d\" bc-url=\"/repo/!svn/bc/5/trunk/d\" copyfrom-path=\"/trunk/c\" copyfrom-rev=\"3\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/5/trunk/d</D:href></D:checked-in>\n",
    "</S:add-directory>\n",
    "<S:open-file name=\"g.txt\" rev=\"3\">\n",
    "<D:checked-in><D:href>/repo/!svn/ver/4/trunk/g.txt</D:href></D:checked-in>\n",
    "<S:set-prop name=\"bin\" encoding=\"base64\">\n/w==\n</S:set-prop>\n",
    "<S:remove-prop name=\"old\"/>\n",
    &format!("<S:txdelta base-checksum=\"{BASE_MD5}\">{two_windows}</S:txdelta>"),
    "</S:open-file>\n",
    "<S:delete-entry name=\"gone\" rev=\"-1\"/>\n",
    "<S:absent-file name=\"secret\"/>\n",
    "</S:open-directory>\n",
    "</S:update-report>\n",
  ]
  .concat();
  assert_eq!(update_drive(), expected);
}

/// A-627 (`send_vsn_url`, `dav_svn__build_uri`): do open a root by HTTPv2 at the server's root, and a targeted root;
/// expect the revision-root href with no root path before `!svn`, and no href for the targeted anchor.
#[test]
fn hrefs_name_revision_roots_by_httpv2_and_a_targeted_anchor_has_none() {
  let uris = Uris {
    root_path: String::from("/"),
    special_uri: String::from("!svn"),
    v2: true,
  };
  let open = |targeted: bool| {
    let update = Update {
      send_all: true,
      inline_props: false,
      targeted,
      uris: uris.clone(),
    };
    let mut drive = Drive::update(update, Version::Zero, ROOM).unwrap();
    drive
      .open_root(Some(2), &facts("/trunk/x y", 2, None))
      .unwrap();
    drive.close_directory().unwrap();
    String::from_utf8(drive.finish().unwrap()).unwrap()
  };
  assert!(open(false).contains("<S:open-directory rev=\"2\">\n<D:checked-in><D:href>/!svn/rvr/2/trunk/x%20y</D:href>"));
  assert!(open(true).contains("<S:open-directory rev=\"2\">\n</S:open-directory>\n"));
}

/// A replay drive of every call.
fn replay_drive() -> String {
  let mut drive = Drive::replay(Version::Zero, ROOM);
  drive.target_revision(7).unwrap();
  drive.open_root(None, &NO_FACTS).unwrap();
  drive
    .open(NodeKind::Directory, "trunk", None, &NO_FACTS)
    .unwrap();
  drive
    .change_property("svn:mergeinfo", Some(b"/b:3"))
    .unwrap();
  let source = Some(("/trunk/old.txt", 6));
  drive
    .add(NodeKind::File, "trunk/new <&>.txt", source, &NO_FACTS)
    .unwrap();
  drive.change_property("svn:eol-style", None).unwrap();
  drive.apply_textdelta(Some(digest(BASE_MD5))).unwrap();
  drive.window(&hello()).unwrap();
  drive.textdelta_end().unwrap();
  drive.close_file(digest(HELLO_MD5)).unwrap();
  drive.delete_entry("trunk/gone", None).unwrap();
  drive.absent(NodeKind::File, "trunk/secret").unwrap();
  drive
    .add(NodeKind::Directory, "trunk/d", None, &NO_FACTS)
    .unwrap();
  drive.close_directory().unwrap();
  drive.close_directory().unwrap();
  drive.close_directory().unwrap();
  String::from_utf8(drive.finish().unwrap()).unwrap()
}

/// A-627 (`reports/replay.c`): do write a replay drive of every call; expect `replay.c`'s bytes: its header at the
/// first call, each call an empty element with its whole path (quoted), the base revisions Subversion's path driver
/// passes as -1, properties in base64 with line breaks or `del`, the delta in `S:apply-textdelta` with its base
/// checksum, the close with the text's MD5, nothing for the absent file, and every close its own element.
#[test]
fn a_replay_drive_is_written_as_replay_c_writes_it() {
  let expected = [
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
    "<S:editor-report xmlns:S=\"svn:\">\n",
    "<S:target-revision rev=\"7\"/>\n",
    "<S:open-root rev=\"-1\"/>\n",
    "<S:open-directory name=\"trunk\" rev=\"-1\"/>\n",
    "<S:change-dir-prop name=\"svn:mergeinfo\">L2I6Mw==\n</S:change-dir-prop>\n",
    "<S:add-file name=\"trunk/new &lt;&amp;&gt;.txt\" copyfrom-path=\"/trunk/old.txt\" copyfrom-rev=\"6\"/>\n",
    "<S:change-file-prop name=\"svn:eol-style\" del=\"true\"/>\n",
    &format!("<S:apply-textdelta checksum=\"{BASE_MD5}\">U1ZOAAAABgEGhmhlbGxvCg==</S:apply-textdelta>\n"),
    &format!("<S:close-file checksum=\"{HELLO_MD5}\"/>\n"),
    "<S:delete-entry name=\"trunk/gone\" rev=\"-1\"/>\n",
    "<S:add-directory name=\"trunk/d\"/>\n",
    "<S:close-directory/>\n",
    "<S:close-directory/>\n",
    "<S:close-directory/>\n",
    "</S:editor-report>\n",
  ]
  .concat();
  assert_eq!(replay_drive(), expected);
}

/// A-627 (`svn_repos_replay2`'s revision 0, `end_report`; a replay with no delta asked): do replay revision 0, end a
/// replay with no call, and send a file with deltas off (`svn_repos_replay2` hands the handler only its end, and marks
/// a property change on a directory with no name); expect the target revision alone, the end tag alone, the svndiff
/// header alone in base64, and an unnamed removal.
#[test]
fn a_replay_of_nothing_writes_what_replay_c_writes_for_it() {
  let mut zero = Drive::replay(Version::Zero, ROOM);
  zero.target_revision(0).unwrap();
  let zero = String::from_utf8(zero.finish().unwrap()).unwrap();
  let expected = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:editor-report xmlns:S=\"svn:\">\n\
    <S:target-revision rev=\"0\"/>\n</S:editor-report>\n";
  assert_eq!(zero, expected);
  let empty = Drive::replay(Version::Zero, ROOM).finish().unwrap();
  assert_eq!(empty, b"</S:editor-report>\n");
  let mut no_deltas = Drive::replay(Version::Zero, ROOM);
  no_deltas.open_root(None, &NO_FACTS).unwrap();
  no_deltas.change_property("", None).unwrap();
  no_deltas
    .add(NodeKind::File, "f", None, &NO_FACTS)
    .unwrap();
  no_deltas.apply_textdelta(None).unwrap();
  no_deltas.textdelta_end().unwrap();
  no_deltas.close_file([0; 16]).unwrap();
  no_deltas.close_directory().unwrap();
  let written = String::from_utf8(no_deltas.finish().unwrap()).unwrap();
  assert!(written.contains("<S:change-dir-prop name=\"\" del=\"true\"/>\n"), "{written}");
  assert!(written.contains("<S:apply-textdelta>U1ZOAA==</S:apply-textdelta>\n<S:close-file/>\n"), "{written}");
}

/// A delta's base64 as one replay answer writes it, its windows given one at a time.
fn replay_delta(windows: &[Window], version: Version) -> String {
  let mut drive = Drive::replay(version, ROOM);
  drive.open_root(None, &NO_FACTS).unwrap();
  drive.add(NodeKind::File, "f", None, &NO_FACTS).unwrap();
  drive.apply_textdelta(None).unwrap();
  for window in windows {
    drive.window(window).unwrap();
  }
  drive.textdelta_end().unwrap();
  drive.close_file([0; 16]).unwrap();
  drive.close_directory().unwrap();
  let written = String::from_utf8(drive.finish().unwrap()).unwrap();
  between(&written, "<S:apply-textdelta>", "</S:apply-textdelta>").to_owned()
}

/// A-627 (`svn_txdelta_to_svndiff3` into `svn_base64_encode2`'s stream): do stream deltas of one to four windows of
/// lengths that leave every remainder of a base64 group between them, in both versions, and decode each back; expect
/// the windows given, applied to the text they make, and in version 0 the same base64 as the whole delta encoded at
/// once.
#[test]
fn a_streamed_delta_decodes_to_its_windows_however_they_were_split() {
  let windows = [filled(b'a', 1), filled(b'b', 2), filled(b'c', 300), filled(b'd', 4)];
  for count in 1..=windows.len() {
    let given = windows.get(..count).unwrap();
    decodes_back(given, Version::Zero);
    decodes_back(given, Version::Two);
    let whole = crate::dav::listing::delta(given, Version::Zero).unwrap();
    assert_eq!(replay_delta(given, Version::Zero), whole);
  }
}

/// Checks that `given`, streamed in `version`, decodes to itself and applies to the text it makes.
fn decodes_back(given: &[Window], version: Version) {
  let text = replay_delta(given, version);
  let (decoded_version, decoded) = decode_all(&unbase64(&text)).unwrap();
  assert_eq!((decoded_version, decoded.as_slice()), (version, given));
  let target: Vec<u8> = given.iter().flat_map(|window| window.new_data.clone()).collect();
  assert_eq!(apply_all(b"", &decoded).unwrap(), target);
}

/// A-627 (`upd_change_xxx_prop`, `upd_apply_textdelta`, `upd_close_file`, `close_helper` outside send-all mode): do
/// drive an addition with a property and a text, a copied addition with a removal and a value, and an opened file with
/// a property and a text, properties not inline; expect no delta at all, the addition's value unsent, the copied
/// addition's removal written at its close, the opened file's property sent and its text fetched with its base and
/// SHA-1, and every MD5 sent.
#[test]
fn outside_send_all_mode_texts_are_fetched_and_an_addition_s_properties_wait() {
  let mut drive = update(false, false, false);
  drive.target_revision(5).unwrap();
  drive.open_root(Some(4), &facts("/trunk", 5, None)).unwrap();
  let added = facts("/trunk/n.txt", 5, Some(HELLO_SHA1));
  drive.add(NodeKind::File, "trunk/n.txt", None, &added).unwrap();
  drive.change_property("svn:eol-style", Some(b"native")).unwrap();
  drive.apply_textdelta(None).unwrap();
  drive.window(&hello()).unwrap();
  drive.textdelta_end().unwrap();
  drive.close_file(digest(HELLO_MD5)).unwrap();
  let copied = facts("/trunk/c", 5, None);
  drive.add(NodeKind::Directory, "trunk/c", Some(("/trunk/d", 3)), &copied).unwrap();
  drive.change_property("gone", None).unwrap();
  drive.change_property("kept", Some(b"v")).unwrap();
  drive.close_directory().unwrap();
  outside_send_all_rest(drive);
}

/// The opened file of [`outside_send_all_mode_texts_are_fetched_and_an_addition_s_properties_wait`], and its checks.
fn outside_send_all_rest(mut drive: Drive) {
  let opened = facts("/trunk/g.txt", 4, Some(HELLO_SHA1));
  drive.open(NodeKind::File, "trunk/g.txt", Some(3), &opened).unwrap();
  drive.change_property("p", Some(b"v")).unwrap();
  drive.apply_textdelta(Some(digest(BASE_MD5))).unwrap();
  drive.window(&hello()).unwrap();
  drive.textdelta_end().unwrap();
  drive.close_file(digest(HELLO_MD5)).unwrap();
  drive.close_directory().unwrap();
  let written = String::from_utf8(drive.finish().unwrap()).unwrap();
  let md5 = format!("<S:prop><V:md5-checksum>{HELLO_MD5}</V:md5-checksum></S:prop>");
  let expected_pieces = [
    "xmlns:D=\"DAV:\"  >\n".to_owned(),
    format!("<D:checked-in><D:href>/repo/!svn/ver/5/trunk/n.txt</D:href></D:checked-in>\n{md5}</S:add-file>\n"),
    "</D:checked-in>\n<S:remove-prop name=\"gone\"/>\n</S:add-directory>\n".to_owned(),
    "<S:set-prop name=\"p\">v</S:set-prop>\n".to_owned(),
    format!("<S:fetch-file base-checksum=\"{BASE_MD5}\" sha1-checksum=\"{HELLO_SHA1}\"/>\n{md5}</S:open-file>\n"),
  ];
  for piece in &expected_pieces {
    assert!(written.contains(piece.as_str()), "{piece} in {written}");
  }
  assert!(!written.contains("txdelta") && !written.contains("native") && !written.contains("kept"), "{written}");
}

/// What `setup` meets on a fresh replay drive.
fn refused(setup: impl FnOnce(&mut Drive) -> Result<(), DavRefusal>) -> Result<(), DavRefusal> {
  let mut drive = Drive::replay(Version::Zero, ROOM);
  setup(&mut drive)
}

/// A-627 (the drive's order): do make each call a drive's order does not allow where it comes: a second root, an entry
/// with no root, a property with no node, a delta on a directory or twice on a file, a window or an end with no
/// delta, a file closed as a directory, a close with its delta open, and the edit closed with a node open; expect each
/// refused by its call.
#[test]
fn each_call_out_of_the_drive_s_order_is_refused_by_kind() {
  let order = |call| Err(DavRefusal::DriveOrder { call });
  let file = |drive: &mut Drive| -> Result<(), DavRefusal> {
    drive.open_root(None, &NO_FACTS)?;
    drive.add(NodeKind::File, "f", None, &NO_FACTS)
  };
  let cases: [(Result<(), DavRefusal>, &str); 10] = [
    (refused(|d| { d.open_root(None, &NO_FACTS)?; d.open_root(None, &NO_FACTS) }), "open-root"),
    (refused(|d| d.add(NodeKind::File, "f", None, &NO_FACTS)), "add"),
    (refused(|d| d.change_property("p", None)), "change-property"),
    (refused(|d| { d.open_root(None, &NO_FACTS)?; d.apply_textdelta(None) }), "apply-textdelta"),
    (refused(|d| { file(d)?; d.apply_textdelta(None)?; d.textdelta_end()?; d.apply_textdelta(None) }), "apply-textdelta"),
    (refused(|d| { file(d)?; d.window(&hello()) }), "window"),
    (refused(|d| { file(d)?; d.textdelta_end() }), "textdelta-end"),
    (refused(|d| { file(d)?; d.close_directory() }), "close-directory"),
    (refused(|d| { file(d)?; d.apply_textdelta(None)?; d.close_file([0; 16]) }), "close-file"),
    (refused(|d| { file(d)?; d.close_edit() }), "close-edit"),
  ];
  for (result, call) in cases {
    assert_eq!(result, order(call));
  }
}

/// A-627 (an aborted or unfinished drive): do abort a drive mid-way, and end one with a node open; expect the aborted
/// answer refused whole and the unfinished one refused at its end, so no partial answer is ever sent.
#[test]
fn an_aborted_or_unfinished_drive_sends_nothing() {
  let mut aborted = Drive::replay(Version::Zero, ROOM);
  aborted.open_root(None, &NO_FACTS).unwrap();
  aborted.abort_edit();
  assert_eq!(aborted.finish(), Err(DavRefusal::Aborted));
  let mut open = Drive::replay(Version::Zero, ROOM);
  open.open_root(None, &NO_FACTS).unwrap();
  assert_eq!(open.finish(), Err(DavRefusal::DriveOrder { call: "finish" }));
}

/// A-627 (hostile values): do open update answers under root paths and special URIs a URL cannot carry as they are,
/// and write a drive past its room, a kept removal among it; expect each refused by kind, the room's refusal before the
/// removal is kept.
#[test]
fn unwritable_uri_spaces_and_answers_past_their_room_are_refused() {
  let open = |root: &str, special: &str| {
    let uris = Uris {
      root_path: root.to_owned(),
      special_uri: special.to_owned(),
      v2: false,
    };
    let update = Update {
      send_all: false,
      inline_props: false,
      targeted: false,
      uris,
    };
    Drive::update(update, Version::Zero, ROOM).map(|_| ())
  };
  for (root, special) in [("/repo/", "!svn"), ("repo", "!svn"), ("/re po", "!svn"), ("/a/../b", "!svn"), ("/repo", "!s/vn"), ("/repo", "")] {
    assert!(matches!(open(root, special), Err(DavRefusal::Unwritable { .. })), "{root} {special}");
  }
  assert_eq!(open("/", "!svn"), Ok(()));
  let mut small = Drive::replay(Version::Zero, 100);
  small.open_root(None, &NO_FACTS).unwrap();
  assert!(matches!(small.open(NodeKind::Directory, &"d".repeat(64), None, &NO_FACTS), Err(DavRefusal::OutputRoom { .. })));
}

/// A-627 (`close_helper`'s kept removals, held against the room): do keep a copied addition's removals outside
/// send-all mode until they would outgrow the room; expect the removal past it refused before it is kept.
#[test]
fn kept_removals_are_held_against_the_room() {
  let uris = Uris {
    root_path: String::from("/repo"),
    special_uri: String::from("!svn"),
    v2: false,
  };
  let update = Update {
    send_all: false,
    inline_props: false,
    targeted: true,
    uris,
  };
  let mut drive = Drive::update(update, Version::Zero, 700).unwrap();
  drive.open_root(Some(1), &facts("/", 1, None)).unwrap();
  drive.add(NodeKind::Directory, "c", Some(("/d", 1)), &facts("/c", 1, None)).unwrap();
  let mut kept = 0;
  let refusal = loop {
    match drive.change_property(&format!("property-{kept:04}"), None) {
      Ok(()) => kept += 1,
      Err(refusal) => break refusal,
    }
  };
  assert!(matches!(refusal, DavRefusal::OutputRoom { .. }) && kept > 0, "{kept} {refusal:?}");
}

/// A-627 (a client parses the drive): do read both answers back with the reader; expect each well formed, rooted at
/// `svn:update-report` and `svn:editor-report`.
#[test]
fn both_answers_read_back_as_well_formed_xml() {
  let bounds = Bounds {
    body_bytes: ROOM,
    depth: 16,
    name_bytes: 64,
    attributes: 8,
    elements: 256,
  };
  let update = xml::parse(update_drive().as_bytes(), &bounds).unwrap();
  let replay = xml::parse(replay_drive().as_bytes(), &bounds).unwrap();
  assert_eq!(
    [
      (update.namespace.as_str(), update.name.as_str()),
      (replay.namespace.as_str(), replay.name.as_str())
    ],
    [(SVN, "update-report"), (SVN, "editor-report")]
  );
}

/// The calls of [`replay_drive`] as A-616's drive yields them, closed with `CloseEdit`.
fn replay_calls(hello: &Window) -> Vec<Call<'_>> {
  vec![
    Call::TargetRevision(7),
    Call::OpenRoot { base: None, node: NO_FACTS },
    Call::Open { kind: NodeKind::Directory, path: "trunk", base: None, node: NO_FACTS },
    Call::ChangeProperty { name: "svn:mergeinfo", value: Some(b"/b:3".as_slice()) },
    Call::Add {
      kind: NodeKind::File,
      path: "trunk/new <&>.txt",
      copy_from: Some(("/trunk/old.txt", 6)),
      node: NO_FACTS,
    },
    Call::ChangeProperty { name: "svn:eol-style", value: None },
    Call::ApplyTextDelta { base_md5: Some(digest(BASE_MD5)) },
    Call::Window(hello),
    Call::TextDeltaEnd,
    Call::CloseFile { md5: digest(HELLO_MD5) },
    Call::Delete { path: "trunk/gone", revision: None },
    Call::Absent { kind: NodeKind::File, path: "trunk/secret" },
    Call::Add { kind: NodeKind::Directory, path: "trunk/d", copy_from: None, node: NO_FACTS },
    Call::CloseDirectory,
    Call::CloseDirectory,
    Call::CloseDirectory,
    Call::CloseEdit,
  ]
}

/// A-627 with A-616 (`report::drive::Call`, one drive for every protocol): do write the replay drive's calls through
/// `Drive::call` into a replay answer and into an update answer, and abort a third drive part way; expect the replay
/// answer equal to the one its methods write, the update answer naming each entry by its basename in `update.c`'s
/// elements, and the aborted answer refused whole.
#[test]
fn one_drive_of_calls_feeds_both_answers() {
  let hello = hello();
  let mut replay = Drive::replay(Version::Zero, ROOM);
  for call in replay_calls(&hello) {
    replay.call(call).unwrap();
  }
  assert_eq!(String::from_utf8(replay.finish().unwrap()).unwrap(), replay_drive());
  let mut update = update(true, true, false);
  for call in replay_calls(&hello) {
    update.call(call).unwrap();
  }
  let written = String::from_utf8(update.finish().unwrap()).unwrap();
  let expected_pieces = [
    "<S:open-directory name=\"trunk\" rev=\"-1\">\n",
    "<S:add-file name=\"new &lt;&amp;&gt;.txt\" copyfrom-path=\"/trunk/old.txt\" copyfrom-rev=\"6\">\n",
    "<S:delete-entry name=\"gone\" rev=\"-1\"/>\n<S:absent-file name=\"secret\"/>\n",
    "</S:open-directory>\n</S:open-directory>\n</S:update-report>\n",
  ];
  for piece in expected_pieces {
    assert!(written.contains(piece), "{piece} in {written}");
  }
  let mut aborted = Drive::replay(Version::Zero, ROOM);
  for call in replay_calls(&hello).into_iter().take(4).chain([Call::AbortEdit]) {
    aborted.call(call).unwrap();
  }
  assert_eq!(aborted.finish(), Err(DavRefusal::Aborted));
}
