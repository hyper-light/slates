//! A-161 by use (§4.6, the Shards route): a client attaches a volume in the Shards form and receives the stream end the
//! daemon serves the attachment on, as a Shards spawner would before handing it to its VMM. On Linux the end comes
//! over the client's control socket, sent just before the attach's reply; on macOS the client claims it on the
//! daemon's socket route with the secret the reply carries. The test then speaks FUSE through it in Shards' framing
//! (a little-endian length, then the request; one reply frame back) and checks every change through slates' own reads
//! and writes. The daemon has two shards and the test attaches a volume on each, so one attach is served on the
//! client's own shard and the other on another shard, whose end comes home with the forwarded reply (Linux) or is
//! taken from the owner shard by the route's exchange (macOS). On Windows, which has no route yet, the attach is
//! refused by kind, the reason the daemon's own transport report gives.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result,
  clippy::panic_in_result_fn
)]

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass, VolumeId,
};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so a client's shard and a volume's owner may differ.
const TEST_SHARDS: u16 = 2;
/// Shape: a volume's bounded quota.
const QUOTA: u64 = 4 << 20;
/// Shape: how long a client retries the rendezvous while its daemon starts, and how long a stream's reply or an
/// attachment's end may take.
const WAIT: Duration = Duration::from_secs(20);

/// The machine profile, measured once per process.
fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}

/// A daemon in this process named `tag`, with its consensus group made, and a client of it.
fn start(tag: &str) -> (Daemon, Client) {
  let profile = profile();
  let instance = format!("shards-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-shards-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let deadlines =
    Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get();
  let started = Instant::now();
  let client = loop {
    match Client::connect(&instance, deadlines) {
      Ok(client) => break client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  };
  (daemon, client)
}

/// A scratch volume named `name`.
fn volume(client: &mut Client, name: &str) -> VolumeId {
  client
    .create(&CreateSpec {
      name: name.to_owned(),
      size: SizeClass::Bounded { limit: QUOTA },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap()
}

/// Linux and macOS: the end comes by the platform's route, and FUSE runs through it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod delivered {
  use std::io::{ErrorKind, Read, Write};
  use std::os::unix::net::UnixStream;

  use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
  use slates_bridge_fuse::reply::EntryOut;
  use slates_client::{ReadAt, ShardsStream};

  use super::*;

  /// Shape: the names tried before two volumes land on different owner shards (names route by hash).
  const MOST_NAMES: usize = 64;

  /// Format: Linux `ENOENT`.
  const ENOENT: i32 = 2;
  /// Format: `O_RDWR` and `O_RDONLY`, an open's access modes.
  const O_RDWR: u32 = 2;
  const O_RDONLY: u32 = 0;
  /// Format: the FUSE node id of the attachment's root.
  const ROOT: u64 = 1;
  /// Format: the FUSE protocol a guest kernel offers (7.31).
  const MAJOR: u32 = 7;
  const MINOR: u32 = 31;
  /// The bytes the guest writes and slates reads back, and the reverse.
  const FROM_GUEST: &[u8] =
    b"written through the stream end the client received, read by slates' own read\n";
  const FROM_SLATES: &[u8] = b"written by slates' own write, read through the stream end\n";

  /// Shards' fs device, as seen from the end the client received.
  struct Guest {
    stream: UnixStream,
    unique: u64,
  }

  impl Guest {
    fn over(shards: ShardsStream) -> Guest {
      let stream = UnixStream::from(shards.stream);
      stream.set_read_timeout(Some(WAIT)).unwrap();
      let mut guest = Guest { stream, unique: 0 };
      let mut init = Vec::new();
      for word in [MAJOR, MINOR, 128 << 10, 0] {
        init.extend_from_slice(&word.to_le_bytes());
      }
      assert_eq!(errno(&guest.ask(Opcode::Init, 0, &init)), 0, "INIT");
      guest
    }

    fn ask(&mut self, opcode: Opcode, nodeid: u64, body: &[u8]) -> Vec<u8> {
      self.unique += 1;
      let total = IN_HEADER_LEN + body.len();
      let mut request = vec![0u8; total];
      request[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
      request[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
      request[8..16].copy_from_slice(&self.unique.to_le_bytes());
      request[16..24].copy_from_slice(&nodeid.to_le_bytes());
      request[IN_HEADER_LEN..].copy_from_slice(body);
      self
        .stream
        .write_all(&u32::try_from(total).unwrap().to_le_bytes())
        .unwrap();
      self.stream.write_all(&request).unwrap();
      let mut word = [0u8; 4];
      self.stream.read_exact(&mut word).unwrap();
      let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(word)).unwrap()];
      self.stream.read_exact(&mut reply).unwrap();
      reply
    }

    fn lookup(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
      let reply = self.ask(Opcode::Lookup, parent, &named(name));
      match errno(&reply) {
        0 => Ok(u64_at(&reply, OUT_HEADER_LEN)),
        error => Err(error),
      }
    }

    /// Creates `name`, writes `bytes` and closes it, every reply a success.
    fn write_file(&mut self, name: &str, bytes: &[u8]) {
      let mut body = Vec::new();
      body.extend_from_slice(&O_RDWR.to_le_bytes());
      body.extend_from_slice(&0o644u32.to_le_bytes());
      body.extend_from_slice(&[0u8; 8]);
      body.extend_from_slice(&named(name));
      let created = self.ask(Opcode::Create, ROOT, &body);
      assert_eq!(errno(&created), 0, "CREATE {name}");
      let node = u64_at(&created, OUT_HEADER_LEN);
      let fh = u64_at(&created, OUT_HEADER_LEN + EntryOut::LEN);
      let mut write = Vec::new();
      write.extend_from_slice(&fh.to_le_bytes());
      write.extend_from_slice(&0u64.to_le_bytes());
      write.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
      write.extend_from_slice(&[0u8; 20]);
      write.extend_from_slice(bytes);
      assert_eq!(errno(&self.ask(Opcode::Write, node, &write)), 0, "WRITE");
      self.close(node, fh);
    }

    /// Opens `name` for reading and reads it whole.
    fn read_file(&mut self, name: &str) -> Vec<u8> {
      let node = self.lookup(ROOT, name).unwrap();
      let mut open = O_RDONLY.to_le_bytes().to_vec();
      open.extend_from_slice(&0u32.to_le_bytes());
      let opened = self.ask(Opcode::Open, node, &open);
      assert_eq!(errno(&opened), 0, "OPEN {name}");
      let fh = u64_at(&opened, OUT_HEADER_LEN);
      let mut read = Vec::new();
      read.extend_from_slice(&fh.to_le_bytes());
      read.extend_from_slice(&0u64.to_le_bytes());
      read.extend_from_slice(&4096u32.to_le_bytes());
      read.extend_from_slice(&[0u8; 20]);
      let reply = self.ask(Opcode::Read, node, &read);
      assert_eq!(errno(&reply), 0, "READ {name}");
      self.close(node, fh);
      reply[OUT_HEADER_LEN..].to_vec()
    }

    fn close(&mut self, node: u64, fh: u64) {
      let mut body = fh.to_le_bytes().to_vec();
      body.extend_from_slice(&[0u8; 16]);
      assert_eq!(errno(&self.ask(Opcode::Flush, node, &body)), 0, "FLUSH");
      assert_eq!(errno(&self.ask(Opcode::Release, node, &body)), 0, "RELEASE");
    }

    fn rename(&mut self, from: &str, to: &str) -> i32 {
      let mut body = ROOT.to_le_bytes().to_vec();
      body.extend_from_slice(&named(from));
      body.extend_from_slice(&named(to));
      errno(&self.ask(Opcode::Rename, ROOT, &body))
    }

    fn unlink(&mut self, name: &str) -> i32 {
      errno(&self.ask(Opcode::Unlink, ROOT, &named(name)))
    }

    /// Whether the daemon has ended the stream (its next read is the end).
    fn ended(&mut self) -> bool {
      let mut byte = [0u8; 1];
      match self.stream.read(&mut byte) {
        Ok(0) => true,
        Err(e) if e.kind() == ErrorKind::ConnectionReset => true,
        _ => false,
      }
    }
  }

  fn named(name: &str) -> Vec<u8> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    bytes
  }

  fn errno(reply: &[u8]) -> i32 {
    -i32::from_le_bytes(reply[4..8].try_into().unwrap())
  }

  fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
  }

  /// Two volumes on different owner shards, so one is served where the client is and the other elsewhere.
  fn on_both_shards(client: &mut Client) -> [VolumeId; 2] {
    let first = volume(client, "shards-0");
    let owner = slates_server::verbs::owner_of(first);
    for at in 1..MOST_NAMES {
      let next = volume(client, &format!("shards-{at}"));
      if slates_server::verbs::owner_of(next) != owner {
        return [first, next];
      }
    }
    panic!("{MOST_NAMES} names all landed on shard {owner}");
  }

  /// The guest writes a file slates reads back, reads a file slates wrote, renames and unlinks, each change checked
  /// through slates' own verbs.
  fn a_history_through(client: &mut Client, volume: VolumeId, guest: &mut Guest) {
    guest.write_file("from-guest.txt", FROM_GUEST);
    assert_eq!(
      client
        .read(volume, "/from-guest.txt", ReadAt::Head)
        .unwrap(),
      FROM_GUEST,
      "slates reads what the guest wrote through the stream"
    );
    let own = client
      .attach(volume, None, Intent::Write)
      .unwrap()
      .attachment;
    client
      .fs_write((volume, own), "/from-slates.txt", FROM_SLATES, 0o644)
      .unwrap();
    assert_eq!(
      guest.read_file("from-slates.txt"),
      FROM_SLATES,
      "the guest reads through the stream what slates wrote"
    );
    client.detach(own).unwrap();
    assert_eq!(guest.rename("from-guest.txt", "renamed.txt"), 0);
    assert!(
      client
        .read(volume, "/from-guest.txt", ReadAt::Head)
        .is_err()
    );
    assert_eq!(
      client.read(volume, "/renamed.txt", ReadAt::Head).unwrap(),
      FROM_GUEST
    );
    assert_eq!(guest.unlink("renamed.txt"), 0);
    assert!(client.read(volume, "/renamed.txt", ReadAt::Head).is_err());
    assert_eq!(guest.lookup(ROOT, "renamed.txt"), Err(ENOENT));
  }

  /// A-161, step 2, by use: on each of two volumes, one served on the client's shard and one on the other, the client
  /// attaches the Shards form and receives the end over its control socket; a FUSE history through it is what slates'
  /// own reads see, and the reverse; closing the end ends the attachment.
  #[test]
  fn a_client_receives_its_stream_end_and_drives_a_history_through_it_on_either_shard() {
    let (daemon, mut client) = start("history");
    for volume in on_both_shards(&mut client) {
      let shards = client.attach_shards(volume, Intent::Write, None).unwrap();
      let attachment = shards.attachment.attachment;
      assert!(shards.attachment.lease_epoch.is_some());
      assert!(shards.attachment.capability.supported);
      let mut guest = Guest::over(shards);
      a_history_through(&mut client, volume, &mut guest);
      drop(guest);
      // Closing the end ends the attachment as a recorded operation: the status counts it gone, and a detach then
      // finds nothing.
      wait_for_attachments(&mut client, volume, 0);
      assert!(matches!(
        client.detach(attachment),
        Err(ClientError::Refused(slates_client::Refusal::NotFound))
      ));
    }
    drop(client);
    drop(daemon);
  }

  /// Waits until `volume`'s status counts `count` attachments.
  fn wait_for_attachments(client: &mut Client, volume: VolumeId, count: u32) {
    let started = Instant::now();
    while client.status(volume).unwrap().attachments != count {
      assert!(
        started.elapsed() < WAIT,
        "the attachments never came to {count}"
      );
      std::thread::yield_now();
    }
  }

  /// A-161, step 2: a stream end reaches only the attachment it was made for. Do: attach two scoped streams of one
  /// volume, each to its own directory, the second for reading, before reading either end. Expect each end to show
  /// only its own directory's file, and the read-only end to refuse a write that the writable end makes.
  #[test]
  fn a_stream_end_reaches_only_its_own_attachment() {
    let (daemon, mut client) = start("own");
    let volume = volume(&mut client, "own");
    let own = client
      .attach(volume, None, Intent::Write)
      .unwrap()
      .attachment;
    for (directory, file) in [("/a", "/a/only-a.txt"), ("/b", "/b/only-b.txt")] {
      client.fs_mkdir((volume, own), directory, 0o755).unwrap();
      client
        .fs_write((volume, own), file, file.as_bytes(), 0o644)
        .unwrap();
    }
    client.detach(own).unwrap();
    let a = client
      .attach_shards(volume, Intent::Write, Some("/a"))
      .unwrap();
    let b = client
      .attach_shards(volume, Intent::Read, Some("/b"))
      .unwrap();
    assert_ne!(a.attachment.attachment, b.attachment.attachment);
    let (mut a, mut b) = (Guest::over(a), Guest::over(b));
    assert_eq!(a.read_file("only-a.txt"), b"/a/only-a.txt");
    assert_eq!(a.lookup(ROOT, "only-b.txt"), Err(ENOENT));
    assert_eq!(b.read_file("only-b.txt"), b"/b/only-b.txt");
    assert_eq!(b.lookup(ROOT, "only-a.txt"), Err(ENOENT));
    a.write_file("made-by-a.txt", FROM_GUEST);
    assert_ne!(
      b.unlink("only-b.txt"),
      0,
      "the read-only end changes nothing"
    );
    assert_eq!(
      client
        .read(volume, "/a/made-by-a.txt", ReadAt::Head)
        .unwrap(),
      FROM_GUEST
    );
    assert_eq!(
      client.read(volume, "/b/only-b.txt", ReadAt::Head).unwrap(),
      b"/b/only-b.txt"
    );
    drop(a);
    drop(b);
    drop(client);
    drop(daemon);
  }

  /// A detach of a stream's attachment by the client that made it ends the stream: the end it handed out reads the
  /// end.
  #[test]
  fn a_detach_ends_the_stream_the_client_handed_out() {
    let (daemon, mut client) = start("detach");
    let volume = volume(&mut client, "detach");
    let shards = client.attach_shards(volume, Intent::Write, None).unwrap();
    let attachment = shards.attachment.attachment;
    let mut guest = Guest::over(shards);
    assert_eq!(guest.lookup(ROOT, "nothing"), Err(ENOENT));
    client.detach(attachment).unwrap();
    assert!(guest.ended(), "a detach ends the stream");
    drop(client);
    drop(daemon);
  }
}

/// Windows: no route hands a client a handle yet, so the daemon refuses the form by kind.
#[cfg(windows)]
mod refused {
  use slates_client::{AttachTransport, Refusal, UnsupportedReason};

  use super::*;

  /// A-161 (Shards part 1 step 4: "Windows compiles and refuses the form by kind"). Do: attach a volume in the Shards
  /// form, for writing and for reading, whole and as a directory, and read the volume's transport report. Expect every
  /// attach refused `AttachmentUnsupported` for the Shards transport with `DescriptorRouteNotBuilt`, the reason the
  /// report gives for the transport, and no attachment made.
  #[test]
  fn the_shards_form_is_refused_by_kind_where_no_route_hands_a_handle() {
    let (_daemon, mut client) = start("refused");
    let id = volume(&mut client, "refused");
    let report = client.status(id).unwrap();
    let shards_fs = report
      .transports
      .capabilities
      .iter()
      .find(|capability| capability.transport == AttachTransport::ShardsFs)
      .expect("the report names the Shards transport");
    assert!(!shards_fs.supported);
    assert_eq!(
      shards_fs.unsupported_reason,
      Some(UnsupportedReason::DescriptorRouteNotBuilt)
    );
    for (intent, subtree) in [
      (Intent::Write, None),
      (Intent::Read, None),
      (Intent::Write, Some("dir")),
    ] {
      match client.attach_shards(id, intent, subtree) {
        Err(ClientError::Refused(Refusal::AttachmentUnsupported { transport, reason })) => {
          assert_eq!(transport, AttachTransport::ShardsFs);
          assert_eq!(reason, UnsupportedReason::DescriptorRouteNotBuilt);
        }
        other => panic!("{intent:?} {subtree:?}: {other:?}"),
      }
    }
    assert_eq!(
      client.status(id).unwrap().attachments,
      0,
      "a refused attach makes no attachment"
    );
  }
}
