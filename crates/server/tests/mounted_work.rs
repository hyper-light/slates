//! Ten ordinary editors, their journals submitted, the Green granted to disk (A-774; GAP-A9-14; §4.16 "Role", §4.15):
//! the mounted-editor workflow end to end, through the daemon. One green holds eight files. Ten works are made over
//! it, and each is mounted like any volume over the daemon's own NFS transport, where an editor saves the way real
//! editors do: the new content written whole to a temporary file beside the original, then renamed over it. Each
//! work is then submitted as it is, with no declaration by the editor: the mount's journal is the work's changeset.
//! The verdicts are the ones the edits call for: seven distinct files accepted, an eighth editor's save identical
//! to an accepted one, and two editors' saves of one line, the first accepted and the second a conflict. A grant
//! then lands the green's head into a real directory, whose bytes must equal the oracle: the base with each
//! accepted edit applied.
//!
//! Gated variant with real editor processes on a kernel mount: `mounted_work_kernel` (root; main's run queue).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
// The daemon's NFS loopback transport, on every platform that serves it (as `snapshot_landing.rs`).

use std::net::TcpStream;

use slates_client::{Client, Landing, ReadAt, Submitted, VolumeId};
use slates_ipc::protocol::Filter;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;
use common::landing::approve;
use common::nfs::{NFS_PROGRAM, call, create, lookup, mount, opaque, read_whole, status, write};
use common::target::target_dir;

/// Shape: one shard: the mounted flow is the subject, not placement.
const TEST_SHARDS: u16 = 1;
/// Shape: the green's name.
const GREEN: &str = "mounted-green";
/// Shape: the green's files: `f0` .. `f7`, each [`LINES`] lines.
const FILES: usize = 8;
/// Shape: the lines of each base file.
const LINES: usize = 5;
/// Shape: the editors, one work and one mount each.
const EDITORS: usize = 10;
/// Format: NFSv3 RENAME (RFC 1813 §3.3.14).
const NFSPROC3_RENAME: u32 = 14;

/// The base bytes of file `f{file}`: [`LINES`] numbered lines.
fn base_file(file: usize) -> Vec<u8> {
  (0..LINES)
    .map(|line| format!("file {file} line {line}\n"))
    .collect::<String>()
    .into_bytes()
}

/// `bytes` with line `line` replaced by `text`.
fn with_line(bytes: &[u8], line: usize, text: &str) -> Vec<u8> {
  let text_lines: Vec<String> = String::from_utf8(bytes.to_vec())
    .unwrap()
    .lines()
    .enumerate()
    .map(|(n, l)| {
      if n == line {
        text.to_owned()
      } else {
        l.to_owned()
      }
    })
    .collect();
  let mut out = text_lines.join("\n").into_bytes();
  out.push(b'\n');
  out
}

/// What editor `editor` does: the file it saves, the line it changes and the text it puts there.
fn edit_of(editor: usize) -> (usize, usize, String) {
  match editor {
    // Seven editors, seven files: each accepted.
    0..=6 => (editor, 2, format!("editor {editor} was here")),
    // The eighth makes editor 0's very edit: identical, accepted with nothing new.
    7 => (0, 2, "editor 0 was here".to_owned()),
    // The ninth and tenth change one line of one file: the first accepted, the second a conflict.
    _ => (7, 3, format!("editor {editor} was here")),
  }
}

/// The daemon's NFS transport mounted on `name`'s root: the stream and the root's handle.
fn mounted(daemon: &Daemon, name: &str) -> (TcpStream, Vec<u8>) {
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon
    .mount_capability(name)
    .expect("the name's owner shard answers")
    .expect("a work is served like any volume");
  let root = mount(&mut stream, &capability, 1);
  (stream, root)
}

/// NFS RENAME `from` to `to`, both in `dir`, replacing `to`: an editor's save.
fn rename(stream: &mut TcpStream, dir: &[u8], from: &str, to: &str, xid: u32) {
  let mut args = Vec::new();
  opaque(dir, &mut args);
  opaque(from.as_bytes(), &mut args);
  opaque(dir, &mut args);
  opaque(to.as_bytes(), &mut args);
  let reply = call(stream, NFS_PROGRAM, NFSPROC3_RENAME, &args, xid);
  assert_eq!(status(&reply), 0, "RENAME {from} -> {to}");
}

/// Saves `bytes` as `name` the way an editor does: the whole content written to a temporary file beside it, then
/// renamed over it. The editor first reads the file it opened, through the mount.
fn editor_save(stream: &mut TcpStream, root: &[u8], name: &str, bytes: &[u8], xid: &mut u32) {
  let mut next = || {
    *xid += 1;
    *xid
  };
  let opened = lookup(stream, root, name, next());
  let _was = read_whole(stream, &opened, next());
  let temp = format!(".{name}.swp");
  let fh = create(stream, root, &temp, next());
  write(stream, &fh, bytes, next());
  rename(stream, root, &temp, name, next());
}

fn start(tag: &str) -> (Daemon, Client, [u8; 32]) {
  let profile = common::machine_profile();
  let instance = format!("srv-mounted-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", &format!("mounted-{tag}")),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let client = connect(&instance);
  let secret = daemon.segment().issuer_secret().unwrap();
  (daemon, client, secret)
}

/// The green with its eight base files, seeded through a work of its own (declared edits, the SDK path); its id.
fn seeded_green(client: &mut Client) -> VolumeId {
  let green = client.create_green(GREEN, false).unwrap();
  let (seed, _) = client.create_work(green, "mounted-seed").unwrap();
  for file in 0..FILES {
    client
      .edit(seed, &format!("f{file}"), 0, 0, &base_file(file))
      .unwrap();
  }
  assert!(
    matches!(client.submit(seed).unwrap(), Submitted::Accepted(_)),
    "the seed is accepted"
  );
  green
}

/// Each of [`EDITORS`] reads the green's base through its own work's mount and saves its edit as an editor does.
fn every_editor_saves(daemon: &Daemon) {
  for editor in 0..EDITORS {
    let (file, line, text) = edit_of(editor);
    let name = format!("f{file}");
    let (mut stream, root) = mounted(daemon, &format!("mounted-work-{editor}"));
    let opened = lookup(&mut stream, &root, &name, 2);
    assert_eq!(
      read_whole(&mut stream, &opened, 3),
      base_file(file),
      "editor {editor} reads the green's base through its mount"
    );
    let mut xid = 10;
    editor_save(
      &mut stream,
      &root,
      &name,
      &with_line(&base_file(file), line, &text),
      &mut xid,
    );
  }
}

/// Editor `editor`'s verdict as the flow expects it: 0-6 and 8 accepted, their edits then in the green (true); 7 (0's
/// edit again) accepted as identical; 9 a conflict on the line 8 changed.
fn verdict_as_expected(editor: usize, verdict: &Submitted) -> bool {
  match editor {
    0..=6 | 8 => {
      assert!(
        matches!(verdict, Submitted::Accepted(_)),
        "editor {editor}: {verdict:?}"
      );
      true
    }
    7 => {
      assert!(
        matches!(verdict, Submitted::Accepted(_)),
        "editor 7's identical save: {verdict:?}"
      );
      false
    }
    _ => {
      assert!(
        matches!(verdict, Submitted::Conflict(_)),
        "editor 9 changed the line editor 8 changed: {verdict:?}"
      );
      false
    }
  }
}

/// Each of `works` submitted in turn, with no declaration: editors 0-6 and 8 accepted, 7 (0's edit again) accepted as
/// identical, 9 a conflict on 8's line; the green's head then the oracle, which is returned.
fn submitted_in_turn(client: &mut Client, green: VolumeId, works: &[VolumeId]) -> Vec<Vec<u8>> {
  let mut oracle: Vec<Vec<u8>> = (0..FILES).map(base_file).collect();
  for (editor, &work) in works.iter().enumerate() {
    let verdict = client.submit(work).unwrap();
    if verdict_as_expected(editor, &verdict) {
      let (file, line, text) = edit_of(editor);
      oracle[file] = with_line(&oracle[file], line, &text);
    }
  }
  for (file, expected) in oracle.iter().enumerate() {
    assert_eq!(
      &client
        .read(green, &format!("f{file}"), ReadAt::Head)
        .unwrap(),
      expected,
      "the green's head of f{file}"
    );
  }
  oracle
}

/// What landed in `path`: every file the oracle's, and nothing else (no editor's temporary).
fn landed_exactly(path: &str, oracle: &[Vec<u8>]) {
  for (file, expected) in oracle.iter().enumerate() {
    let landed = std::fs::read(format!("{}/f{file}", path)).unwrap();
    assert_eq!(&landed, expected, "the landed f{file} is the oracle's");
  }
  let mut names: Vec<String> = std::fs::read_dir(path)
    .unwrap()
    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
    .collect();
  names.sort();
  let expected: Vec<String> = (0..FILES).map(|file| format!("f{file}")).collect();
  assert_eq!(
    names, expected,
    "exactly the green's files landed, no editor's temporary"
  );
}

/// A-774 (GAP-A9-14). Do: make a green of eight files and ten works over it; mount each work over NFS and save one
/// edit through it as an editor does (temporary file, rename over); submit each work in turn, with no declaration;
/// grant and land the green's head into a fresh directory. Expect: editors 0-6 accepted, editor 7 (editor 0's edit
/// again) accepted as identical with the head's bytes unchanged, editor 8 accepted and editor 9 a conflict on the
/// line editor 8 changed; the landing done; every landed file equal to the oracle, and nothing else landed.
#[test]
fn ten_editors_on_mounted_works_submit_and_a_grant_lands_the_green() {
  let (daemon, mut client, secret) = start("ten");
  let green = seeded_green(&mut client);
  let works: Vec<VolumeId> = (0..EDITORS)
    .map(|editor| {
      client
        .create_work(green, &format!("mounted-work-{editor}"))
        .unwrap()
        .0
    })
    .collect();

  // Every editor saves through its own mount before any submits, as ten concurrent editors would.
  every_editor_saves(&daemon);

  let oracle = submitted_in_turn(&mut client, green, &works);

  // The human's grant (R10): presented, approved under the issuer's proof, then landed.
  let target = target_dir();
  let grant = approve(&mut client, &secret, green, None, &target.path);
  match client.land(green, None, &target.path, Filter::default(), Some(grant)) {
    Ok(Landing::Landed(outcome)) => assert_eq!(outcome.state, "done"),
    other => panic!("the granted landing of the green: {other:?}"),
  }
  landed_exactly(&target.path, &oracle);
  drop(client);
  daemon.stop();
}
