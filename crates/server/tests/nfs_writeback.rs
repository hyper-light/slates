//! An NFS v3 client's write-back against another path's newer change, adversarially (§4.6 "NFS v3"; A-192, the A/B Ada
//! asked for on 2026-10-31 between sync-point coherence and refusing a stale write-back).
//!
//! Each case drives the daemon's real NFS export with a hand-rolled ONC RPC client and plays the client the way a cache
//! does: it READs a page, and later WRITEs that page back whole, its own store with the rest of the page as it fetched
//! it, which is what Apple's client sends for a page a program dirtied through a shared map (measured 3 of 3 on CI's
//! macOS runner, diagnostics run 40821794698). Another path writes in between: the SDK, or a second NFS connection. The
//! invariants:
//! - **never silent:** the file never ends holding the client's stale copy of a byte another path changed after the
//!   client fetched it, and a write-back that did not land is answered with an error;
//! - **never refused for nothing:** a write-back no other path overtook, one over a page the client fetched again, a
//!   write the client made without reading, and one whose stale bytes equal the file's are all applied;
//! - **never broken by hostile input:** offsets at the end of the range, empty writes, and writes past the end are answered
//!   and the export goes on serving.
//!
//! Every case prints an `NFS-AB` line with what happened, so one run of each arm is its scorecard.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::net::TcpStream;

use slates_client::{Client, Intent, VolumeId};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{connect, scratch};
use common::nfs::{create, mount, read_whole, write_at_status};

/// Format: the page a client writes back whole: Apple silicon's 16 KiB, the page of the measured case.
const PAGE: usize = 16 * 1024;
/// Format: the file every case works on.
const FILE: &str = "f";
/// Format: the mode the SDK writes the file with.
const FILE_MODE: u32 = 0o644;
/// Format: `NFS3_OK`.
const OK: u32 = 0;

/// A started, bootstrapped daemon of `shards` shards, and its instance name.
fn daemon(tag: &str, shards: u16) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(shards));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// One case's world: a daemon, an SDK client with a write attachment on one volume, and that volume's mount path.
struct World {
  daemon: Daemon,
  sdk: Client,
  volume: VolumeId,
  attachment: u64,
  path: String,
  xid: u32,
}

impl World {
  /// A daemon of `shards` shards with volume `name`, written by nothing yet.
  fn new(tag: &str, shards: u16) -> World {
    let (daemon, instance) = daemon(tag, shards);
    let mut sdk = connect(&instance);
    let volume = sdk.create(&scratch(tag)).unwrap();
    let attachment = sdk.attach(volume, None, Intent::Write).unwrap().attachment;
    let path = daemon
      .mount_capability(tag)
      .expect("the name's owner shard answers")
      .expect("a volume by that name is provisioned there");
    World {
      daemon,
      sdk,
      volume,
      attachment,
      path,
      xid: 1,
    }
  }

  /// A fresh xid.
  fn xid(&mut self) -> u32 {
    self.xid = self.xid.wrapping_add(1);
    self.xid
  }

  /// A new NFS connection, mounted: the stream and the root's handle.
  fn connection(&mut self) -> (TcpStream, Vec<u8>) {
    let port = self.daemon.nfs_port().expect("the daemon serves NFS");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let xid = self.xid();
    let root = mount(&mut stream, &self.path, xid);
    (stream, root)
  }

  /// The file's bytes now, read by a connection that keeps nothing.
  fn bytes(&mut self, fh: &[u8]) -> Vec<u8> {
    let (mut stream, _) = self.connection();
    let xid = self.xid();
    read_whole(&mut stream, fh, xid)
  }

  /// The SDK's whole-file write of `bytes`: another path.
  fn sdk_writes(&mut self, bytes: &[u8]) {
    self
      .sdk
      .fs_write((self.volume, self.attachment), FILE, bytes, FILE_MODE)
      .unwrap();
  }

  /// A daemon counter, summed over its shards.
  fn counter(&self, name: &str) -> u64 {
    self
      .daemon
      .refusals_on_every_shard()
      .unwrap()
      .get(name)
      .copied()
      .unwrap_or(0)
  }
}

/// READ of `count` bytes at `offset`: the bytes, or the status answered instead.
fn read_at(
  stream: &mut TcpStream,
  fh: &[u8],
  (offset, count): (u64, u32),
  xid: u32,
) -> Result<Vec<u8>, u32> {
  let mut args = Vec::new();
  common::nfs::opaque(fh, &mut args);
  args.extend_from_slice(&offset.to_be_bytes());
  args.extend_from_slice(&count.to_be_bytes());
  let reply = common::nfs::call(stream, common::nfs::NFS_PROGRAM, 6, &args, xid);
  let answered = common::nfs::status(&reply);
  if answered != OK {
    return Err(answered);
  }
  let mut off = 4;
  let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84;
  }
  off += 8;
  Ok(common::nfs::read_opaque(&reply, off).0)
}

/// `bytes` as an RPC count.
fn count(bytes: usize) -> u32 {
  u32::try_from(bytes).unwrap()
}

/// `bytes` as a file offset.
fn offset(bytes: usize) -> u64 {
  u64::try_from(bytes).unwrap()
}

/// A page of `fill`.
fn page(fill: u8) -> Vec<u8> {
  vec![fill; PAGE]
}

/// The client's write-back of a page it fetched as `fetched`, its program having stored `Z` at the page's first byte.
fn written_back(fetched: &[u8]) -> Vec<u8> {
  let mut bytes = fetched.to_vec();
  bytes[0] = b'Z';
  bytes
}

/// Whether any byte of `bytes` from `from` on is `stale`: the client's old copy where another path wrote.
fn holds_stale(bytes: &[u8], from: usize, stale: u8) -> bool {
  bytes.get(from..).is_some_and(|rest| rest.contains(&stale))
}

/// The never-silent invariant over a case's outcome: no stale byte survives where another path wrote, and a write-back
/// whose store did not land was answered with an error.
fn never_silent(case: &str, status: u32, bytes: &[u8], (stale, from): (u8, usize)) {
  let stale_kept = holds_stale(bytes, from, stale);
  let store_landed = bytes.first() == Some(&b'Z');
  eprintln!(
    "NFS-AB {case}: write-back answered {status}, store landed {store_landed}, stale bytes kept {stale_kept}, length {}",
    bytes.len()
  );
  assert!(
    !stale_kept,
    "{case}: the client's stale copy was put back over another path's newer write (answered {status})"
  );
  assert!(
    store_landed || status != OK,
    "{case}: the write-back did not land and was answered OK"
  );
}

/// A-192, never silent. Do: a client READs a page of `A`, the SDK replaces the file with `C`, and the client writes its
/// page back with its store (`Z` and the stale `A`s). Expect: the file never ends holding the stale `A`s, and the client
/// is told its write-back did not land.
#[test]
fn a_stale_page_never_puts_its_old_bytes_back_over_the_sdks_write() {
  let mut world = World::new("wb-sdk", 1);
  let (mut client, root) = world.connection();
  let xid = world.xid();
  let fh = create(&mut client, &root, FILE, xid);
  let xid = world.xid();
  assert_eq!(write_at_status(&mut client, &fh, 0, &page(b'A'), xid), OK);
  let xid = world.xid();
  let fetched = read_at(&mut client, &fh, (0, count(PAGE)), xid).unwrap();
  world.sdk_writes(&page(b'C'));
  let xid = world.xid();
  let status = write_at_status(&mut client, &fh, 0, &written_back(&fetched), xid);
  let bytes = world.bytes(&fh);
  never_silent("sdk-replace", status, &bytes, (b'A', 1));
  eprintln!(
    "NFS-AB sdk-replace counters: conflicts {}",
    world.counter("nfs.writeback_conflicts")
  );
}

/// A-192, never silent past a cut. Do: a client READs two pages, the SDK cuts the file to nothing, and the client writes
/// its second page back. Expect: the cut bytes never come back, and the client is told.
#[test]
fn a_stale_page_never_brings_back_bytes_another_path_cut() {
  let mut world = World::new("wb-cut", 1);
  let (mut client, root) = world.connection();
  let xid = world.xid();
  let fh = create(&mut client, &root, FILE, xid);
  let mut two = page(b'A');
  two.extend_from_slice(&page(b'A'));
  let xid = world.xid();
  assert_eq!(write_at_status(&mut client, &fh, 0, &two, xid), OK);
  let xid = world.xid();
  let fetched = read_at(&mut client, &fh, (offset(PAGE), count(PAGE)), xid).unwrap();
  world.sdk_writes(&[]);
  let xid = world.xid();
  let status = write_at_status(&mut client, &fh, offset(PAGE), &written_back(&fetched), xid);
  let bytes = world.bytes(&fh);
  eprintln!(
    "NFS-AB cut: write-back answered {status}, length after {} (the cut left 0)",
    bytes.len()
  );
  assert!(
    bytes.is_empty() || status != OK,
    "cut: a stale page brought the cut file back to {} bytes, answered OK",
    bytes.len()
  );
  assert!(
    !holds_stale(&bytes, 0, b'A'),
    "cut: the cut bytes came back"
  );
}

/// A-192, never silent between two clients. Do: client one READs a page, client two writes the page with `C`, and client
/// one writes its stale page back. Expect: client two's write survives, and client one is told.
#[test]
fn one_clients_stale_page_never_undoes_another_clients_write() {
  let mut world = World::new("wb-two", 1);
  let (mut one, root) = world.connection();
  let (mut two, _) = world.connection();
  let xid = world.xid();
  let fh = create(&mut one, &root, FILE, xid);
  let xid = world.xid();
  assert_eq!(write_at_status(&mut one, &fh, 0, &page(b'A'), xid), OK);
  let xid = world.xid();
  let fetched = read_at(&mut one, &fh, (0, count(PAGE)), xid).unwrap();
  let xid = world.xid();
  assert_eq!(write_at_status(&mut two, &fh, 0, &page(b'C'), xid), OK);
  let xid = world.xid();
  let status = write_at_status(&mut one, &fh, 0, &written_back(&fetched), xid);
  let bytes = world.bytes(&fh);
  never_silent("two-clients", status, &bytes, (b'A', 1));
}

/// A-192, never silent on a volume another shard owns: the connection moves to the volume's shard at its first call
/// there, and what the server keeps of its cache must go with it. Do: on a daemon of two shards, run the SDK case on four
/// volumes, so that one living on the shard the listener does not is all but certain. Expect: never silent on any.
#[test]
fn a_stale_page_is_caught_on_a_volume_of_either_shard() {
  for tag in ["wb-shard-a", "wb-shard-b", "wb-shard-c", "wb-shard-d"] {
    let mut world = World::new(tag, 2);
    let (mut client, root) = world.connection();
    let xid = world.xid();
    let fh = create(&mut client, &root, FILE, xid);
    let xid = world.xid();
    assert_eq!(write_at_status(&mut client, &fh, 0, &page(b'A'), xid), OK);
    let xid = world.xid();
    let fetched = read_at(&mut client, &fh, (0, count(PAGE)), xid).unwrap();
    world.sdk_writes(&page(b'C'));
    let xid = world.xid();
    let status = write_at_status(&mut client, &fh, 0, &written_back(&fetched), xid);
    let bytes = world.bytes(&fh);
    never_silent(&format!("two-shards {tag}"), status, &bytes, (b'A', 1));
  }
}

/// A-192, never refused for nothing. Do, each on its own file: a write-back no other path overtook; a write-back of page
/// one after another client wrote page three; a write-back after the client fetched the page again past another path's
/// change; a write the client makes without having read; and a write-back whose stale bytes another client rewrote with
/// the same values. Expect: each applied, answered OK, its store in the file.
#[test]
fn write_backs_no_other_path_overtook_are_applied() {
  let mut world = World::new("wb-fair", 1);
  let (mut one, root) = world.connection();
  let (mut two, _) = world.connection();
  let cases = [
    ("alone", alone(&mut world, &mut one, &root)),
    ("beside", beside(&mut world, (&mut one, &mut two), &root)),
    ("again", again(&mut world, (&mut one, &mut two), &root)),
    ("blind", blind(&mut world, (&mut one, &mut two), &root)),
    ("same", same(&mut world, (&mut one, &mut two), &root)),
  ];
  for (case, (status, fh)) in cases {
    let bytes = world.bytes(&fh);
    eprintln!(
      "NFS-AB fair {case}: answered {status}, store landed {}",
      bytes.first() == Some(&b'Z')
    );
    assert_eq!(
      status, OK,
      "{case}: a write-back nobody overtook was refused"
    );
    assert_eq!(bytes.first(), Some(&b'Z'), "{case}: the store landed");
  }
}

/// A file `name` of `pages` pages of `A`, written and then read whole by `client`: its handle and the bytes fetched.
fn fetched_file(
  world: &mut World,
  client: &mut TcpStream,
  root: &[u8],
  (name, pages): (&str, usize),
) -> (Vec<u8>, Vec<u8>) {
  let xid = world.xid();
  let fh = create(client, root, name, xid);
  let xid = world.xid();
  assert_eq!(
    write_at_status(client, &fh, 0, &vec![b'A'; pages * PAGE], xid),
    OK
  );
  let xid = world.xid();
  let fetched = read_at(client, &fh, (0, count(pages * PAGE)), xid).unwrap();
  (fh, fetched)
}

/// The fair case nobody else wrote: the write-back's status and the file.
fn alone(world: &mut World, one: &mut TcpStream, root: &[u8]) -> (u32, Vec<u8>) {
  let (fh, fetched) = fetched_file(world, one, root, ("alone", 1));
  let xid = world.xid();
  (
    write_at_status(one, &fh, 0, &written_back(&fetched), xid),
    fh,
  )
}

/// The fair case where another client wrote another page, which must be kept.
fn beside(
  world: &mut World,
  (one, two): (&mut TcpStream, &mut TcpStream),
  root: &[u8],
) -> (u32, Vec<u8>) {
  let (fh, fetched) = fetched_file(world, one, root, ("beside", 3));
  let xid = world.xid();
  assert_eq!(
    write_at_status(two, &fh, offset(2 * PAGE), &page(b'C'), xid),
    OK
  );
  let xid = world.xid();
  let status = write_at_status(one, &fh, 0, &written_back(&fetched[..PAGE]), xid);
  let bytes = world.bytes(&fh);
  assert_eq!(
    &bytes[2 * PAGE..],
    &page(b'C')[..],
    "beside: the other client's page is kept"
  );
  (status, fh)
}

/// The fair case where the client fetched the page again after another client's change.
fn again(
  world: &mut World,
  (one, two): (&mut TcpStream, &mut TcpStream),
  root: &[u8],
) -> (u32, Vec<u8>) {
  let (fh, _) = fetched_file(world, one, root, ("again", 1));
  let xid = world.xid();
  assert_eq!(write_at_status(two, &fh, 0, &page(b'C'), xid), OK);
  let xid = world.xid();
  let again = read_at(one, &fh, (0, count(PAGE)), xid).unwrap();
  let xid = world.xid();
  (write_at_status(one, &fh, 0, &written_back(&again), xid), fh)
}

/// The fair case of a write made without a read, over another client's write.
fn blind(
  world: &mut World,
  (one, two): (&mut TcpStream, &mut TcpStream),
  root: &[u8],
) -> (u32, Vec<u8>) {
  let xid = world.xid();
  let fh = create(two, root, "blind", xid);
  let xid = world.xid();
  assert_eq!(write_at_status(two, &fh, 0, &page(b'C'), xid), OK);
  let xid = world.xid();
  (
    write_at_status(one, &fh, 0, &written_back(&page(b'Y')), xid),
    fh,
  )
}

/// The fair case where another client rewrote the stale bytes with the values they had.
fn same(
  world: &mut World,
  (one, two): (&mut TcpStream, &mut TcpStream),
  root: &[u8],
) -> (u32, Vec<u8>) {
  let (fh, fetched) = fetched_file(world, one, root, ("same", 1));
  let xid = world.xid();
  assert_eq!(write_at_status(two, &fh, 100, &[b'A'; 100], xid), OK);
  let xid = world.xid();
  (
    write_at_status(one, &fh, 0, &written_back(&fetched), xid),
    fh,
  )
}

/// A-192, never broken by hostile input. Do: after a READ, WRITE at the last offsets of the range, an empty WRITE, a
/// WRITE far past the end, a READ past the end, then the SDK case. Expect: each answered (an error or OK, never a hang
/// or a crash), and the export still judging and serving after.
#[test]
fn hostile_writes_are_answered_and_the_export_goes_on() {
  let mut world = World::new("wb-hostile", 1);
  let (mut client, root) = world.connection();
  let xid = world.xid();
  let fh = create(&mut client, &root, FILE, xid);
  let xid = world.xid();
  assert_eq!(write_at_status(&mut client, &fh, 0, &page(b'A'), xid), OK);
  let xid = world.xid();
  let fetched = read_at(&mut client, &fh, (0, count(PAGE)), xid).unwrap();
  let mut answered = Vec::new();
  for (offset, len) in [
    (u64::MAX - 1, 100usize),
    (u64::MAX, 1),
    (0, 0),
    (u64::from(u32::MAX) * 4, 10),
  ] {
    let xid = world.xid();
    answered.push((
      offset,
      write_at_status(&mut client, &fh, offset, &vec![b'H'; len], xid),
    ));
  }
  let xid = world.xid();
  let past = read_at(&mut client, &fh, (u64::MAX - 10, 100), xid);
  eprintln!("NFS-AB hostile: writes answered {answered:?}, a read past the end {past:?}");
  // The export goes on: the same connection writes its fetched page back, nobody having overtaken it, and it lands.
  let xid = world.xid();
  let status = write_at_status(&mut client, &fh, 0, &written_back(&fetched), xid);
  // Only the first page is read back: the write past the end made the file sparse and many gigabytes long.
  let (mut reader, _) = world.connection();
  let xid = world.xid();
  let first = read_at(&mut reader, &fh, (0, count(PAGE)), xid).unwrap();
  assert_eq!(
    status, OK,
    "the export still applies a write-back after hostile input"
  );
  assert_eq!(
    first.first(),
    Some(&b'Z'),
    "the export still serves the file"
  );
}
