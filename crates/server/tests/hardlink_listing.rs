//! A hard link in a green or a work as its directory listing shows it (§4.12 `slates.fs.list`, §4.16): a linked path is
//! an entry of its directory, a file of the linked file's size, at a green's head, at a named version, and in a work.
//! Before 2026-11-08 a green's and a work's listings built their entries from files, directories, symlinks and
//! specials, and never from hard links, so a linked path was missing from every listing of a green or a work.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use slates_client::{Client, VolumeId};
use slates_ipc::protocol::{DirEntry, EntryKind, ReadAt, WorkOp};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;

/// Shape: one shard, the green's owner.
const TEST_SHARDS: u16 = 1;
/// Format: the linked file's bytes; its size is what the link's entry must report.
const ORIGINAL: &[u8] = b"the bytes a hard link names";

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-hardlink-listing-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-hardlink-listing-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn entry<'a>(listing: &'a [DirEntry], name: &str) -> Option<&'a DirEntry> {
  listing.iter().find(|entry| entry.name == name)
}

/// `/alias.txt` listed as a file of the original's size, beside `/original.txt`.
fn assert_lists_the_link(listing: &[DirEntry], at: &str) {
  let original =
    entry(listing, "original.txt").unwrap_or_else(|| panic!("{at}: original.txt in {listing:?}"));
  let alias =
    entry(listing, "alias.txt").unwrap_or_else(|| panic!("{at}: alias.txt in {listing:?}"));
  assert_eq!(alias.kind, EntryKind::File, "{at}: a hard link is a file");
  assert_eq!(
    alias.size,
    ORIGINAL.len() as u64,
    "{at}: of the linked file's size"
  );
  assert_eq!(
    alias.size, original.size,
    "{at}: the same size as the original"
  );
}

/// A work that writes `/original.txt` and links `/alias.txt` to it; the work.
fn linked_work(client: &mut Client, green: VolumeId, name: &str) -> VolumeId {
  let (work, _) = client.create_work(green, name).unwrap();
  client.edit(work, "/original.txt", 0, 0, ORIGINAL).unwrap();
  client
    .declare(
      work,
      WorkOp::Link {
        path: "/alias.txt".into(),
        target: "/original.txt".into(),
      },
    )
    .unwrap();
  work
}

/// §4.12, §4.16. Do: a work writes a file and links a second name to it; list the work; submit it; list the green at
/// its head; submit a later work changing another file; list the green at the link's version and at the head, and list a
/// fresh work over the green. Expect: the linked name in every listing as a file of the original's size. Before
/// 2026-11-08 it was in none of them.
#[test]
fn a_hard_linked_path_is_listed_in_a_green_and_a_work_at_every_version() {
  let (daemon, instance) = daemon("listed");
  let mut client = connect(&instance);
  let green = client.create_green("linked-green", false).unwrap();
  let work = linked_work(&mut client, green, "linking-work");
  assert_lists_the_link(
    &client.list_dir(work, "/", ReadAt::Head).unwrap(),
    "the work",
  );
  client.submit(work).unwrap();
  let linked = client.versions(green).unwrap();
  assert_lists_the_link(
    &client.list_dir(green, "/", ReadAt::Head).unwrap(),
    "the green's head",
  );
  let (later, _) = client.create_work(green, "later-work").unwrap();
  client.edit(later, "/later.txt", 0, 0, b"later").unwrap();
  client.submit(later).unwrap();
  assert_lists_the_link(
    &client
      .list_dir(green, "/", ReadAt::Version { version: linked })
      .unwrap(),
    "the link's version",
  );
  assert_lists_the_link(
    &client.list_dir(green, "/", ReadAt::Head).unwrap(),
    "the later head",
  );
  let (fresh, _) = client.create_work(green, "fresh-work").unwrap();
  assert_lists_the_link(
    &client.list_dir(fresh, "/", ReadAt::Head).unwrap(),
    "a work over it",
  );
  drop(client);
  daemon.stop();
}
