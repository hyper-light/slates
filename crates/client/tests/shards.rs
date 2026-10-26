//! A-161 by use (§4.6, the Shards route): a client attaches a volume in the Shards form and receives the stream end the
//! daemon serves the attachment on, as a Shards spawner would before handing it to its VMM. On Linux the end comes
//! over the client's control socket, sent just before the attach's reply; on macOS the client claims it on the
//! daemon's socket route with the secret the reply carries. The test then speaks FUSE through it in Shards' framing
//! (a little-endian length, then the request; one reply frame back) and checks every change through slates' own reads
//! and writes. The daemon has two shards and the test attaches a volume on each, so one attach is served on the
//! client's own shard and the other on another shard, whose end comes home with the forwarded reply (Linux) or is
//! taken from the owner shard by the route's exchange (macOS). On Windows the daemon duplicates the client end of the
//! attachment's named pipe into this process and the reply names its value (A-179); the same history, hostile frames
//! and endings run over it, and a second process is refused the pipe by name.
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

/// Linux, macOS and Windows: the end comes by the platform's route, and FUSE runs through it.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod delivered {
  use std::io::{ErrorKind, Read, Write};
  #[cfg(unix)]
  use std::os::unix::net::UnixStream;

  use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
  use slates_bridge_fuse::reply::EntryOut;
  #[cfg(windows)]
  use std::process::Command;

  #[cfg(windows)]
  use slates_client::AttachTransport;
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

  /// The end the client received, as Shards' device reads and writes it: a Unix socket with the test's read bound.
  #[cfg(unix)]
  type Wire = UnixStream;

  /// The end the client received on Windows: the client end of the attachment's pipe, opened for overlapped I/O, so
  /// each read and write waits on an `OVERLAPPED` of its own (`slates_ipc::stream::read_waiting`, `write_waiting`).
  #[cfg(windows)]
  struct Wire(slates_client::StreamEnd);

  #[cfg(windows)]
  impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
      use std::os::windows::io::AsHandle;
      slates_ipc::stream::read_waiting(self.0.as_handle(), buf).map_err(std::io::Error::other)
    }
  }

  #[cfg(windows)]
  impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      use std::os::windows::io::AsHandle;
      slates_ipc::stream::write_waiting(self.0.as_handle(), buf).map_err(std::io::Error::other)
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  /// The received end as a [`Wire`].
  #[cfg(unix)]
  fn wire(end: slates_client::StreamEnd) -> Wire {
    let stream = UnixStream::from(end);
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
  }

  /// The received end as a [`Wire`].
  #[cfg(windows)]
  fn wire(end: slates_client::StreamEnd) -> Wire {
    Wire(end)
  }

  /// Shards' fs device, as seen from the end the client received.
  struct Guest {
    stream: Wire,
    unique: u64,
  }

  impl Guest {
    fn over(shards: ShardsStream) -> Guest {
      let mut guest = Guest {
        stream: wire(shards.stream),
        unique: 0,
      };
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

    /// Writes raw bytes on the stream, as a hostile device would.
    fn send_raw(&mut self, bytes: &[u8]) {
      self.stream.write_all(bytes).unwrap();
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

  /// Waits until the daemon has counted `kind` at least `at_least` times on any shard: the count, after at most
  /// [`WAIT`].
  fn wait_for_count(daemon: &Daemon, kind: &str, at_least: u64) -> u64 {
    let started = Instant::now();
    loop {
      let counted = daemon
        .refusals_on_every_shard()
        .unwrap()
        .get(kind)
        .copied()
        .unwrap_or(0);
      if counted >= at_least || started.elapsed() > WAIT {
        return counted;
      }
      std::thread::yield_now();
    }
  }

  /// Format: the length word's bit that marks Shards' own control frames (`shards_fs::CONTROL_FRAME`).
  const CONTROL: u32 = 1 << 31;

  /// A-161, A-179, hostile input over the platform's stream (a socket pair on Unix, a named pipe on Windows): a
  /// length of `u32::MAX`, a length past the largest frame, a frame cut off inside its length word and one cut off
  /// inside its body by the peer's close. Do: send each on a stream of its own, beside a bystander's. Expect each
  /// refused before anything is allocated for it (the bound is checked on the length word alone, `split_frame`),
  /// counted by its kind, its stream ended and its attachment removed, so the volume's attachment count comes back to
  /// the bystander's one and then to zero; the bystander served throughout, and the daemon never panics.
  #[test]
  fn hostile_frames_end_only_their_own_stream_and_leave_no_attachment() {
    let (daemon, mut client) = start("hostile");
    let id = volume(&mut client, "hostile");
    let mut bystander = Guest::over(client.attach_shards(id, Intent::Write, None).unwrap());
    for (word, refused) in [(u32::MAX, 1), (!CONTROL, 2)] {
      let mut hostile = Guest::over(client.attach_shards(id, Intent::Write, None).unwrap());
      hostile.send_raw(&word.to_le_bytes());
      assert!(
        hostile.ended(),
        "a refused length ends its stream: {word:#x}"
      );
      assert_eq!(
        wait_for_count(&daemon, "shards.frame_refused", refused),
        refused
      );
    }
    let mut cut = 64u32.to_le_bytes().to_vec();
    cut.extend_from_slice(&[0u8; 10]);
    for (part, truncated) in [(&cut[..2], 1), (&cut[..], 2)] {
      let mut hostile = Guest::over(client.attach_shards(id, Intent::Write, None).unwrap());
      hostile.send_raw(part);
      drop(hostile);
      assert_eq!(
        wait_for_count(&daemon, "shards.frame_truncated", truncated),
        truncated
      );
    }
    wait_for_attachments(&mut client, id, 1);
    assert_eq!(bystander.lookup(ROOT, "nothing"), Err(ENOENT));
    bystander.write_file("still-served.txt", FROM_GUEST);
    drop(bystander);
    wait_for_attachments(&mut client, id, 0);
    let refusals = daemon.refusals_on_every_shard().unwrap();
    for kind in [
      "shards.sweep_refused",
      "shards.record_end_refused",
      "shards.end_refused",
    ] {
      assert_eq!(refusals.get(kind), None, "{kind}: {refusals:?}");
    }
    drop(client);
    drop(daemon);
  }

  /// Whether `attachment`'s record is on `volume`'s owner partition, asked of that shard in this process.
  fn recorded(daemon: &Daemon, volume: VolumeId, attachment: u64) -> bool {
    let partition = usize::from(slates_server::verbs::owner_of(volume));
    let shard = daemon.shards().get(partition).copied();
    daemon
      .observation(
        shard,
        slates_server::daemon::OBSERVE_BUDGET_NS,
        move |state: &mut slates_server::state::ShardState| {
          state.db.partition().attachment(attachment).is_some()
        },
      )
      .unwrap()
      .wait()
      .unwrap()
  }

  /// A-161, A-179: the volume's destroy ends its stream. Do: attach a volume in the Shards form, speak through the end,
  /// then destroy the volume. Expect the end to read the end of the stream and then the attachment's record gone from
  /// its partition (asked of the owner shard: the end is seen at once, and the record goes at the serve task's next
  /// turn), with no sweep or record refusal counted.
  #[test]
  fn a_destroy_ends_the_stream_and_removes_its_record() {
    let (daemon, mut client) = start("destroy");
    let id = volume(&mut client, "destroy");
    let shards = client.attach_shards(id, Intent::Write, None).unwrap();
    let attachment = shards.attachment.attachment;
    let mut guest = Guest::over(shards);
    guest.write_file("doomed.txt", FROM_GUEST);
    client.destroy(id).unwrap();
    assert!(guest.ended(), "the destroy ends the stream");
    let started = Instant::now();
    while recorded(&daemon, id, attachment) {
      assert!(
        started.elapsed() < WAIT,
        "the destroyed volume's stream kept its record"
      );
      std::thread::yield_now();
    }
    let refusals = daemon.refusals_on_every_shard().unwrap();
    for kind in [
      "shards.sweep_refused",
      "shards.record_end_refused",
      "shards.end_refused",
    ] {
      assert_eq!(refusals.get(kind), None, "{kind}: {refusals:?}");
    }
    drop(client);
    drop(daemon);
  }

  /// The account a client of this machine's rendezvous is: its uid on Unix; zero on Windows, where the rendezvous names
  /// every client's uid zero and judges the user by its token instead.
  #[cfg(unix)]
  fn account() -> u32 {
    rustix::process::getuid().as_raw()
  }

  /// See the Unix arm.
  #[cfg(windows)]
  fn account() -> u32 {
    0
  }

  /// A-161, A-179, AUD-29-84: a revocation ends its consumer's stream. Do: enroll a consumer, share the account's
  /// volume with it for writing, bind a channel to it and attach the volume in the Shards form, then revoke the
  /// consumer from the account. Expect the end to read the end of the stream and the attachment gone from the volume's
  /// count, as every other attachment a revoked consumer held.
  #[test]
  fn a_revocation_ends_its_consumers_stream() {
    use slates_server::landing::{enroll_proof, revoke_proof};
    let (daemon, mut owner) = start("revoke");
    let secret = daemon.segment().issuer_secret().unwrap();
    let account = account();
    let (consumer, capability) = owner
      .enroll(account, enroll_proof(&secret, account))
      .unwrap();
    let id = volume(&mut owner, "revoked");
    owner
      .share(
        id,
        slates_client::Principal::Consumer { account, consumer },
        slates_client::Rights {
          read: true,
          write: true,
          admin: false,
        },
      )
      .unwrap();
    let instance = format!("shards-revoke-{}", std::process::id());
    let deadlines =
      Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get();
    let mut bound = Client::connect(&instance, deadlines).unwrap();
    bound.attest(consumer, &capability).unwrap();
    let shards = bound.attach_shards(id, Intent::Write, None).unwrap();
    let mut guest = Guest::over(shards);
    assert_eq!(guest.lookup(ROOT, "nothing"), Err(ENOENT));
    owner
      .revoke(consumer, revoke_proof(&secret, consumer))
      .unwrap();
    assert!(guest.ended(), "the revocation ends the consumer's stream");
    wait_for_attachments(&mut owner, id, 0);
    let refusals = daemon.refusals_on_every_shard().unwrap();
    for kind in [
      "shards.sweep_refused",
      "shards.record_end_refused",
      "shards.end_refused",
    ] {
      assert_eq!(refusals.get(kind), None, "{kind}: {refusals:?}");
    }
    drop(bound);
    drop(owner);
    drop(daemon);
  }

  /// Shape: the ends the in-flight test receives, as many as macOS's route test claims under its churn
  /// (`crates/server/tests/shards.rs`, `CLAIMS_UNDER_CHURN`), where 16 to 40 of 48 arrived flushed before its fix.
  #[cfg(target_os = "linux")]
  const ENDS_UNDER_CHURN: usize = 48;

  /// A-161 step 2 (Linux), the counterpart of macOS's `every_end_handed_over_arrives_live_while_sockets_churn`
  /// (`docs/bugs/2026-10-24-a-stream-end-arrived-flushed-by-the-kernel-in-flight.md`). The daemon closes its copy of an
  /// end right after sending it over the control socket, so until the client's receive returns, the message holds the
  /// end's only reference: the window in which XNU's collector flushed an end. Do: attach the Shards form many times
  /// while another thread makes and drops unreachable cycles of descriptors in flight, each drop a run of Linux's
  /// collector (`unix_gc`, which a released socket with descriptors in flight starts). Expect every end live: its
  /// receive buffer not the zero a flush leaves, and `INIT` answered through it; and the churn to have run throughout.
  #[cfg(target_os = "linux")]
  #[test]
  fn every_end_delivered_arrives_live_while_the_kernel_collects_descriptors_in_flight() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let (daemon, mut client) = start("churn");
    let volume = volume(&mut client, "churn");
    let stop = AtomicBool::new(false);
    let cycles = AtomicUsize::new(0);
    let (dead, during) = std::thread::scope(|scope| {
      scope.spawn(|| {
        while !stop.load(Ordering::Relaxed) {
          drop_an_unreachable_cycle();
          cycles.fetch_add(1, Ordering::Relaxed);
        }
      });
      // The churn stops however the attaches end, a failed assertion included: the scope waits for its threads.
      let _stop = StopOnDrop(&stop);
      let before = cycles.load(Ordering::Relaxed);
      let mut dead = 0usize;
      for _ in 0..ENDS_UNDER_CHURN {
        let shards = client.attach_shards(volume, Intent::Write, None).unwrap();
        if rustix::net::sockopt::socket_recv_buffer_size(&shards.stream).unwrap() == 0 {
          dead += 1;
          continue;
        }
        drop(Guest::over(shards));
      }
      (dead, cycles.load(Ordering::Relaxed) - before)
    });
    eprintln!(
      "{ENDS_UNDER_CHURN} ends delivered, {dead} flushed, {during} unreachable cycles dropped meanwhile"
    );
    assert_eq!(dead, 0, "{dead} of {ENDS_UNDER_CHURN} ends arrived flushed");
    assert!(
      during >= ENDS_UNDER_CHURN,
      "the collector was started only {during} times while {ENDS_UNDER_CHURN} ends were in flight"
    );
    drop(client);
    drop(daemon);
  }

  /// Sets its flag when dropped: the churn's stop, on the attaches' every exit.
  #[cfg(target_os = "linux")]
  struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

  #[cfg(target_os = "linux")]
  impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
      self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
  }

  /// One socket pair's end sent over the pair itself and never received, then both closed: a cycle only the message
  /// still holds, which Linux's collector of descriptors in flight finds and frees when the release starts it.
  #[cfg(target_os = "linux")]
  fn drop_an_unreachable_cycle() {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    use std::os::fd::AsFd;
    let (near, far) = UnixStream::pair().unwrap();
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let carried = [far.as_fd()];
    assert!(control.push(SendAncillaryMessage::ScmRights(&carried)));
    rustix::net::sendmsg(
      &near,
      &[std::io::IoSlice::new(b"x")],
      &mut control,
      SendFlags::empty(),
    )
    .unwrap();
    drop(near);
    drop(far);
  }

  /// Format: the variable that makes a re-invoked test binary the opener, naming the pipe it opens.
  #[cfg(windows)]
  const ROLE_PIPE: &str = "SLATES_SHARDS_TEST_OPEN_PIPE";
  /// Format: the tag the opener prints before the code its open was refused with (libtest prints its own banner and
  /// test name on stdout first, so the parent looks for the tag anywhere in a line).
  #[cfg(windows)]
  const OPENED_TAG: &str = "SHARDS-PIPE-OPEN";
  /// Format: `ERROR_PIPE_BUSY`, what an open of a connected one-instance pipe answers.
  #[cfg(windows)]
  const ERROR_PIPE_BUSY: u32 = 231;

  /// The opener's role, run only when re-invoked with [`ROLE_PIPE`] set: opens the named pipe for reading and writing
  /// as any client would, and prints the code the open was refused with (0 for an open that succeeded).
  #[cfg(windows)]
  #[test]
  #[ignore = "a role the parent test re-invokes this binary for"]
  fn opener() {
    use windows_sys::Win32::Foundation::{
      CloseHandle, GENERIC_READ, GENERIC_WRITE, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
      CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    };
    let Some(name) = std::env::var_os(ROLE_PIPE) else {
      return;
    };
    let wide: Vec<u16> = name
      .to_string_lossy()
      .encode_utf16()
      .chain(std::iter::once(0))
      .collect();
    // SAFETY: a NUL-terminated name; a handle made is closed below.
    let handle = unsafe {
      CreateFileW(
        wide.as_ptr(),
        GENERIC_READ | GENERIC_WRITE,
        0,
        std::ptr::null(),
        OPEN_EXISTING,
        FILE_FLAG_OVERLAPPED,
        std::ptr::null_mut(),
      )
    };
    let code = if handle == INVALID_HANDLE_VALUE {
      // SAFETY: reads this thread's last error.
      unsafe { GetLastError() }
    } else {
      // SAFETY: the handle just opened, closed once.
      unsafe { CloseHandle(handle) };
      0
    };
    println!("{OPENED_TAG} {code}");
  }

  /// A-179, by use. Do: attach a volume in the Shards form and take the handle the daemon duplicated into this
  /// process; read the transport report; then have a second process open the handle's pipe by its name, and speak
  /// FUSE over the handle after. Expect the report to offer the Shards transport, the handle to name a Shards stream's
  /// pipe, the second process refused `ERROR_PIPE_BUSY`, and the stream to go on serving its own client.
  #[cfg(windows)]
  #[test]
  fn a_second_process_is_refused_an_attached_streams_pipe_by_name() {
    let (daemon, mut client) = start("pipe-busy");
    let id = volume(&mut client, "pipe-busy");
    let report = client.status(id).unwrap();
    let shards_fs = report
      .transports
      .capabilities
      .iter()
      .find(|capability| capability.transport == AttachTransport::ShardsFs)
      .expect("the report names the Shards transport");
    assert!(shards_fs.supported, "{shards_fs:?}");
    let shards = client.attach_shards(id, Intent::Write, None).unwrap();
    let name = {
      use std::os::windows::io::AsHandle;
      slates_ipc::stream::name_of(shards.stream.as_handle()).unwrap()
    };
    assert!(name.starts_with(r"\slates-shards-"), "{name}");
    let full = format!(r"\\.\pipe{name}");
    let output = Command::new(std::env::current_exe().unwrap())
      .args(["--ignored", "--exact", "delivered::opener", "--nocapture"])
      .env(ROLE_PIPE, &full)
      .output()
      .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let code: u32 = stdout
      .lines()
      .find_map(|line| {
        line
          .split_once(OPENED_TAG)
          .map(|(_, code)| code.trim().to_owned())
      })
      .unwrap_or_else(|| panic!("the opener printed no code: {stdout}"))
      .parse()
      .unwrap();
    assert_eq!(code, ERROR_PIPE_BUSY, "{stdout}");
    let attachment = shards.attachment.attachment;
    let mut guest = Guest::over(shards);
    assert_eq!(
      guest.lookup(ROOT, "nothing"),
      Err(ENOENT),
      "the stream serves its own client after the refused open"
    );
    client.detach(attachment).unwrap();
    drop(guest);
    drop(client);
    drop(daemon);
  }
}
