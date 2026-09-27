//! The NFSv4.1/4.2 front end driven as a client drives it (A-35): `COMPOUND`s encoded on the wire,
//! served through `serve_compound` over a real `Export` of a scratch volume, the replies decoded and
//! checked against RFC 8881's rules — the session handshake, exactly-once replies from the slot cache,
//! open state and its refusals, and hostile compounds.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_nfs::Export;
use slates_bridge_nfs::v4::Nfsstat4;
use slates_bridge_nfs::v4::backend::serve_compound;
use slates_bridge_nfs::v4::compound::{COMPOUND_HEADER_BYTES, MIN_OPERATION_BYTES, Server, op};
use slates_bridge_nfs::v4::session::Limits;
use slates_bridge_nfs::v4::types::{Bitmap, ChannelAttrs, Stateid};
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const VOLUME: VolumeId = VolumeId { bytes: [0x11; 16] };
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
/// `FATTR4_SIZE`, `FATTR4_MODE`, `FATTR4_FILEID` (RFC 7863).
const ATTR_SIZE: u32 = 4;
const ATTR_MODE: u32 = 33;
const ATTR_FILEID: u32 = 20;
/// `OPEN4_SHARE_ACCESS_*` / `OPEN4_SHARE_DENY_*`.
const READ: u32 = 1;
const WRITE: u32 = 2;
const BOTH: u32 = 3;
const DENY_NONE: u32 = 0;
/// `createmode4`.
const UNCHECKED: u32 = 0;
const GUARDED: u32 = 1;
/// `FILE_SYNC4`.
const FILE_SYNC: u32 = 2;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn root_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VOLUME,
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn export<'a>(bridge: &'a mut VolumeBridge<'_>, uid: u32) -> Export<'a> {
  Export::new(
    bridge,
    VOLUME,
    Principal::Uid { uid },
    Rights {
      read: true,
      write: true,
    },
  )
  .unwrap()
}

thread_local! {
  /// The clock the tests serve compounds at, in nanoseconds; a test that measures a lease moves it.
  static NOW_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A server with the standalone offer, `max_clients` clients and a `lease_ns` lease.
fn server_with(max_clients: usize, lease_ns: u64) -> Server {
  let size = slates_bridge_nfs::procedures::MAX_TRANSFER + COMPOUND_HEADER_BYTES;
  Server::new(
    7,
    Limits {
      max_clients,
      max_sessions_per_client: 4,
      offer: ChannelAttrs {
        header_pad: 0,
        max_request: size,
        max_response: size,
        max_response_cached: size,
        max_operations: size / MIN_OPERATION_BYTES,
        max_requests: 4,
      },
      lease_ns,
    },
    [9; 8],
    64,
  )
}

/// One `COMPOUND`'s decoded reply: the overall status and each operation's (opnum, status, body).
struct Reply {
  status: u32,
  results: Vec<(u32, u32)>,
  bytes: Vec<u8>,
}

impl Reply {
  /// The results after the frame, owned (for a reply whose frame is not kept).
  fn walk_owned(&self) -> Vec<u8> {
    self.walk().rest().to_vec()
  }

  /// The body reader positioned after the frame, for walking the results in order.
  fn walk(&self) -> XdrReader<'_> {
    let mut reader = XdrReader::new(&self.bytes);
    reader.u32().unwrap();
    reader.opaque(1024).unwrap();
    reader.u32().unwrap();
    reader
  }
}

/// Decodes the frame and the (opnum, status) of each result; the bodies are walked by the test.
fn decode_frame(bytes: Vec<u8>) -> Reply {
  let mut reader = XdrReader::new(&bytes);
  let status = reader.u32().unwrap();
  reader.opaque(1024).unwrap();
  let count = reader.u32().unwrap();
  // Only the statuses are taken here: a result body's length depends on its operation, so the last
  // result's opnum and status are recovered from the tail of an error reply, and success replies are
  // walked by the caller.
  let mut results = Vec::new();
  if count > 0 && status != Nfsstat4::Ok.wire() {
    let tail = &bytes[bytes.len() - 8..];
    let mut tail = XdrReader::new(tail);
    results.push((tail.u32().unwrap(), tail.u32().unwrap()));
  }
  Reply {
    status,
    results,
    bytes,
  }
}

/// Expects the next result to be `opnum` succeeding.
fn expect_ok(reader: &mut XdrReader<'_>, opnum: u32) {
  assert_eq!(reader.u32().unwrap(), opnum, "result order");
  assert_eq!(
    reader.u32().unwrap(),
    Nfsstat4::Ok.wire(),
    "op {opnum} status"
  );
}

/// A test client: builds compounds, tracks its session's slot 0 sequence.
struct Client {
  owner: Vec<u8>,
  clientid: u64,
  sessionid: [u8; 16],
  sequence: u32,
}

impl Client {
  fn call(service: &mut Export<'_>, server: &mut Server, args: &[u8]) -> Reply {
    decode_frame(
      serve_compound(service, server, 0, NOW_NS.with(std::cell::Cell::get), args)
        .expect("the compound completes in one poll"),
    )
  }

  /// EXCHANGE_ID then CREATE_SESSION for `owner`.
  fn connect(service: &mut Export<'_>, server: &mut Server, owner: &[u8]) -> Client {
    let mut args = frame(1, 1);
    args.u32(op::EXCHANGE_ID);
    args.fixed(&[1; 8]);
    args.opaque(owner);
    args.u32(0); // flags
    args.u32(0); // SP4_NONE
    args.u32(0); // no implementation id
    let reply = Client::call(service, server, args.as_slice());
    assert_eq!(reply.status, Nfsstat4::Ok.wire(), "EXCHANGE_ID");
    let mut body = reply.walk();
    expect_ok(&mut body, op::EXCHANGE_ID);
    let clientid = body.u64().unwrap();
    let sequenceid = body.u32().unwrap();

    let mut args = frame(1, 1);
    args.u32(op::CREATE_SESSION);
    args.u64(clientid);
    args.u32(sequenceid);
    args.u32(0);
    let asked = ChannelAttrs {
      header_pad: 0,
      max_request: 1 << 20,
      max_response: 1 << 20,
      max_response_cached: 1 << 20,
      max_operations: 16,
      max_requests: 8,
    };
    asked.encode(&mut args);
    asked.encode(&mut args);
    args.u32(0x4000_0000); // callback program
    args.u32(1);
    args.u32(0); // AUTH_NONE
    let reply = Client::call(service, server, args.as_slice());
    assert_eq!(reply.status, Nfsstat4::Ok.wire(), "CREATE_SESSION");
    let mut body = reply.walk();
    expect_ok(&mut body, op::CREATE_SESSION);
    let mut sessionid = [0u8; 16];
    sessionid.copy_from_slice(body.fixed(16).unwrap());
    Client {
      owner: owner.to_vec(),
      clientid,
      sessionid,
      sequence: 0,
    }
  }

  /// A compound of `count` operations after a SEQUENCE on slot 0 with the next sequence id.
  fn sequenced(&mut self, count: u32) -> XdrWriter {
    self.sequence += 1;
    self.sequenced_at(self.sequence, count)
  }

  /// A compound whose SEQUENCE carries `sequence` on slot 0 (a retry reuses the last one).
  fn sequenced_at(&self, sequence: u32, count: u32) -> XdrWriter {
    let mut args = frame(1, count + 1);
    args.u32(op::SEQUENCE);
    args.fixed(&self.sessionid);
    args.u32(sequence);
    args.u32(0); // slot
    args.u32(0); // highest slot
    args.bool(true);
    args
  }

  /// OPEN `name` in the current directory for this client's owner (or `owner`), creating it with
  /// `create` = (createmode, mode, size) when given.
  fn open_args(
    &self,
    args: &mut XdrWriter,
    owner: &[u8],
    name: &str,
    access: u32,
    deny: u32,
    create: Option<(u32, Option<u32>, Option<u64>)>,
  ) {
    args.u32(op::OPEN);
    args.u32(0);
    args.u32(access);
    args.u32(deny);
    args.u64(self.clientid);
    args.opaque(owner);
    match create {
      Some((mode_kind, mode, size)) => {
        args.u32(1); // OPEN4_CREATE
        args.u32(mode_kind);
        let mut bits = Vec::new();
        let mut values = XdrWriter::new();
        if let Some(size) = size {
          bits.push(ATTR_SIZE);
          values.u64(size);
        }
        if let Some(mode) = mode {
          bits.push(ATTR_MODE);
          values.u32(mode);
        }
        Bitmap::of(&bits).encode(args);
        args.opaque(values.as_slice());
      }
      None => args.u32(0), // OPEN4_NOCREATE
    }
    args.u32(0); // CLAIM_NULL
    args.opaque(name.as_bytes());
  }
}

/// A COMPOUND's frame: an empty tag, the minor version and the operation count.
fn frame(minor: u32, count: u32) -> XdrWriter {
  let mut args = XdrWriter::new();
  args.opaque(b"");
  args.u32(minor);
  args.u32(count);
  args
}

/// Skips a SEQUENCE result body.
fn skip_sequence(body: &mut XdrReader<'_>) {
  expect_ok(body, op::SEQUENCE);
  body.fixed(16 + 5 * 4).unwrap();
}

/// Reads an OPEN result body: the state id; the change info, flags, attrset and delegation skipped.
fn open_result(body: &mut XdrReader<'_>) -> (Stateid, Bitmap) {
  expect_ok(body, op::OPEN);
  let stateid = Stateid::decode(body).unwrap();
  body.fixed(4 + 8 + 8 + 4).unwrap();
  let attrset = Bitmap::decode(body).unwrap();
  body.u32().unwrap();
  (stateid, attrset)
}

/// Opens (creating if absent, unchecked) `name` at the root for `owner`: the state id and file handle.
fn open_at_root(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  owner: &[u8],
  name: &str,
  access: u32,
  deny: u32,
) -> Result<(Stateid, Vec<u8>), u32> {
  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  client.open_args(
    &mut args,
    owner,
    name,
    access,
    deny,
    Some((UNCHECKED, Some(0o644), None)),
  );
  args.u32(op::GETFH);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return Err(reply.status);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (stateid, _) = open_result(&mut body);
  expect_ok(&mut body, op::GETFH);
  Ok((stateid, body.opaque(128).unwrap().to_vec()))
}

/// READ `count` bytes at 0 of `fh` under `stateid`: the data, or the refusal status.
fn read(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
  fh: &[u8],
  stateid: Stateid,
) -> Result<Vec<u8>, u32> {
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(fh);
  args.u32(op::READ);
  stateid.encode(&mut args);
  args.u64(0);
  args.u32(4096);
  let reply = Client::call(service, server, args.as_slice());
  if reply.status != Nfsstat4::Ok.wire() {
    return Err(reply.status);
  }
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::READ);
  let _eof = body.bool().unwrap();
  Ok(body.opaque(4096).unwrap().to_vec())
}

/// READDIR of the root asking size, mode and file id: each entry's (name, size), checking the reserved
/// cookies and the attributes returned, and that the listing is complete.
fn list_root(
  client: &mut Client,
  service: &mut Export<'_>,
  server: &mut Server,
) -> Vec<(String, u64)> {
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::READDIR);
  args.u64(0);
  args.fixed(&[0; 8]);
  args.u32(4096);
  args.u32(8192);
  Bitmap::of(&[ATTR_SIZE, ATTR_MODE, ATTR_FILEID]).encode(&mut args);
  let reply = Client::call(service, server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "READDIR");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  expect_ok(&mut body, op::READDIR);
  body.fixed(8).unwrap();
  let mut names = Vec::new();
  while body.bool().unwrap() {
    let cookie = body.u64().unwrap();
    assert!(
      cookie > 2,
      "cookies 0, 1 and 2 are reserved (RFC 8881 §18.23.3)"
    );
    let name = body.string(255).unwrap().to_owned();
    let bitmap = Bitmap::decode(&mut body).unwrap();
    assert!(bitmap.has(ATTR_SIZE) && bitmap.has(ATTR_MODE));
    let values = body.opaque(1024).unwrap();
    let mut values = XdrReader::new(values);
    let size = values.u64().unwrap();
    names.push((name, size));
  }
  assert!(body.bool().unwrap(), "the listing is complete");
  names
}

/// A client opens a new file, writes it, reads it back, closes it, and finds it in a directory listing
/// with the size it wrote — the walk a Linux `vers=4.1` mount makes for `echo > f; cat f; ls -l`.
/// A-35; RFC 8881 §18.16, §18.32, §18.22, §18.2, §18.23.
#[test]
fn a_client_opens_writes_reads_closes_and_lists_a_file() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");

  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  let owner = client.owner.clone();
  client.open_args(
    &mut args,
    &owner,
    "notes.txt",
    BOTH,
    DENY_NONE,
    Some((GUARDED, Some(0o640), None)),
  );
  args.u32(op::GETFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (stateid, attrset) = open_result(&mut body);
  assert!(attrset.has(ATTR_MODE), "the create reports the mode it set");
  expect_ok(&mut body, op::GETFH);
  let fh = body.opaque(128).unwrap().to_vec();

  let payload = b"written through NFSv4.1";
  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::WRITE);
  stateid.encode(&mut args);
  args.u64(0);
  args.u32(FILE_SYNC);
  args.opaque(payload);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "WRITE");
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTFH);
  expect_ok(&mut body, op::WRITE);
  assert_eq!(
    body.u32().unwrap() as usize,
    payload.len(),
    "every byte written"
  );

  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, stateid).unwrap(),
    payload
  );

  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::CLOSE);
  args.u32(0);
  stateid.encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire(), "CLOSE");
  assert_eq!(server.open_count(), 0, "the close released the open");

  let names = list_root(&mut client, &mut service, &mut server);
  assert_eq!(
    names,
    vec![("notes.txt".to_owned(), payload.len() as u64)],
    "no dot entries, the size written"
  );
}

/// A request retried on its slot with the same sequence id is answered with the kept reply, byte for
/// byte, and not run again: a retried CREATE does not become `NFS4ERR_EXIST`, while a new request that
/// creates the same name does. A-35; RFC 8881 §2.10.6.1.
#[test]
fn a_retried_request_gets_the_kept_reply_and_is_not_run_again() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");

  let mkdir = |args: &mut XdrWriter| {
    args.u32(op::PUTROOTFH);
    args.u32(op::CREATE);
    args.u32(2); // NF4DIR
    args.opaque(b"build");
    Bitmap::default().encode(args);
    args.opaque(b"");
  };
  let mut args = client.sequenced(2);
  mkdir(&mut args);
  let first = serve_compound(
    &mut service,
    &mut server,
    0,
    NOW_NS.with(std::cell::Cell::get),
    args.as_slice(),
  )
  .unwrap();
  assert_eq!(
    decode_frame(first.clone()).status,
    Nfsstat4::Ok.wire(),
    "the first CREATE"
  );

  let mut retry = client.sequenced_at(client.sequence, 2);
  mkdir(&mut retry);
  let replayed = serve_compound(
    &mut service,
    &mut server,
    0,
    NOW_NS.with(std::cell::Cell::get),
    retry.as_slice(),
  )
  .unwrap();
  assert_eq!(replayed, first, "the retry is answered from the slot cache");

  let mut again = client.sequenced(2);
  mkdir(&mut again);
  let reply = Client::call(&mut service, &mut server, again.as_slice());
  assert_eq!(
    reply.status,
    Nfsstat4::Exist.wire(),
    "a new request runs, and the name exists"
  );
  assert_eq!(reply.results, vec![(op::CREATE, Nfsstat4::Exist.wire())]);
}

/// Outside a session only the session operations are served, each alone; a minor version this server
/// does not speak is refused; an unknown operation number inside a session is `OP_ILLEGAL`, reported
/// under `OP_ILLEGAL`. A-35; RFC 8881 §15.1, §16.2.3, §18.46.3.
#[test]
fn outside_a_session_only_the_session_operations_are_served() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();

  let mut args = frame(1, 1);
  args.u32(op::PUTROOTFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::OpNotInSession.wire());

  let mut args = frame(1, 2);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"host-a");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  args.u32(op::PUTROOTFH);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::NotOnlyOp.wire());

  for minor in [0, 3] {
    let mut args = frame(minor, 0);
    args.u32(op::PUTROOTFH);
    let reply = Client::call(&mut service, &mut server, args.as_slice());
    assert_eq!(
      reply.status,
      Nfsstat4::MinorVersMismatch.wire(),
      "minor {minor}"
    );
  }

  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let mut args = client.sequenced(1);
  args.u32(9999);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::OpIllegal.wire());
  assert_eq!(
    reply.results,
    vec![(op::ILLEGAL, Nfsstat4::OpIllegal.wire())]
  );
}

/// An open-owner's second open of a file shares its state id, advanced; after the close, that state id
/// serves nothing, and an earlier seqid of a live open is `NFS4ERR_OLD_STATEID`. A-35; RFC 8881
/// §9.1.4.1, §8.2.2.
#[test]
fn a_reopen_by_the_same_owner_shares_one_state_id_and_close_releases_it() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();

  let (first, fh) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "log",
    READ,
    DENY_NONE,
  )
  .unwrap();
  let (second, _) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "log",
    BOTH,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(second.other, first.other, "one state id per owner and file");
  assert_eq!(second.seqid, first.seqid + 1, "the upgrade advances it");
  assert_eq!(server.open_count(), 1);

  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, first),
    Err(Nfsstat4::OldStateid.wire())
  );
  assert!(read(&mut client, &mut service, &mut server, &fh, second).is_ok());

  let mut args = client.sequenced(2);
  args.u32(op::PUTFH);
  args.opaque(&fh);
  args.u32(op::CLOSE);
  args.u32(0);
  second.encode(&mut args);
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Ok.wire()
  );
  assert_eq!(server.open_count(), 0);
  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh, second),
    Err(Nfsstat4::BadStateid.wire())
  );
}

/// A state id serves only the file it opened: presenting one file's open for another is
/// `NFS4ERR_BAD_STATEID`. A-35; RFC 8881 §8.2.4.
#[test]
fn a_state_id_serves_only_its_own_file() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();
  let (for_a, _) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "a",
    READ,
    DENY_NONE,
  )
  .unwrap();
  let (_, fh_b) = open_at_root(
    &mut client,
    &mut service,
    &mut server,
    &owner,
    "b",
    READ,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(
    read(&mut client, &mut service, &mut server, &fh_b, for_a),
    Err(Nfsstat4::BadStateid.wire())
  );
}

/// Opening an existing file checks the caller's permission for the access asked: a read-only file of
/// another owner opens for reading and is `NFS4ERR_ACCESS` for writing. A-35; RFC 8881 §18.16.3.
#[test]
fn an_open_of_an_existing_file_checks_the_callers_permission() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  bridge
    .create(ObjectId::new(root, 0), &cx, "config", 0o644, 0)
    .unwrap();
  let mut service = export(&mut bridge, 1000);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();

  let open = |client: &mut Client, service: &mut Export<'_>, server: &mut Server, access| {
    let mut args = client.sequenced(2);
    args.u32(op::PUTROOTFH);
    client.open_args(&mut args, &owner, "config", access, DENY_NONE, None);
    Client::call(service, server, args.as_slice()).status
  };
  assert_eq!(
    open(&mut client, &mut service, &mut server, READ),
    Nfsstat4::Ok.wire()
  );
  assert_eq!(
    open(&mut client, &mut service, &mut server, WRITE),
    Nfsstat4::Access.wire()
  );
}

/// A share that denies writing refuses another owner's open for writing (`NFS4ERR_SHARE_DENIED`) and
/// admits its open for reading. A-35; RFC 8881 §9.7.
#[test]
fn a_conflicting_share_is_denied() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  open_at_root(
    &mut client,
    &mut service,
    &mut server,
    b"first",
    "shared",
    READ,
    WRITE,
  )
  .unwrap();
  assert_eq!(
    open_at_root(
      &mut client,
      &mut service,
      &mut server,
      b"second",
      "shared",
      WRITE,
      DENY_NONE
    )
    .err(),
    Some(Nfsstat4::ShareDenied.wire())
  );
  assert!(
    open_at_root(
      &mut client,
      &mut service,
      &mut server,
      b"second",
      "shared",
      READ,
      DENY_NONE
    )
    .is_ok()
  );
}

/// An `UNCHECKED` open of a file that exists opens it and applies only the size asked (the truncate of
/// `O_TRUNC`), leaving its mode. A-35; RFC 8881 §18.16.3.
#[test]
fn an_unchecked_open_of_an_existing_file_truncates_it_and_keeps_its_mode() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let cx = root_cx();
  let root = bridge.root(&cx).unwrap();
  let file = bridge
    .create(ObjectId::new(root, 0), &cx, "out", 0o600, 0)
    .unwrap();
  bridge
    .write(
      ObjectId::new(file.0.ino, file.0.generation),
      &cx,
      0,
      b"old contents",
    )
    .unwrap();
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();
  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let owner = client.owner.clone();

  let mut args = client.sequenced(3);
  args.u32(op::PUTROOTFH);
  client.open_args(
    &mut args,
    &owner,
    "out",
    WRITE,
    DENY_NONE,
    Some((UNCHECKED, Some(0o777), Some(0))),
  );
  args.u32(op::GETATTR);
  Bitmap::of(&[ATTR_SIZE, ATTR_MODE]).encode(&mut args);
  let reply = Client::call(&mut service, &mut server, args.as_slice());
  assert_eq!(reply.status, Nfsstat4::Ok.wire());
  let mut body = reply.walk();
  skip_sequence(&mut body);
  expect_ok(&mut body, op::PUTROOTFH);
  let (_, attrset) = open_result(&mut body);
  assert!(
    attrset.has(ATTR_SIZE) && !attrset.has(ATTR_MODE),
    "only the size was set"
  );
  expect_ok(&mut body, op::GETATTR);
  Bitmap::decode(&mut body).unwrap();
  let values = body.opaque(64).unwrap();
  let mut values = XdrReader::new(values);
  assert_eq!(values.u64().unwrap(), 0, "truncated");
  assert_eq!(values.u32().unwrap() & 0o7777, 0o600, "the mode kept");
}

/// A full client table makes room from the clients whose lease has lapsed, and their opens go with
/// them; while every client's lease is live, a new client is `NFS4ERR_RESOURCE`. A-35; RFC 8881
/// §8.3, §18.35.4.
#[test]
fn a_lapsed_client_makes_room_and_its_opens_go_with_it() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);

  let mut live = server_with(1, u64::MAX);
  Client::connect(&mut service, &mut live, b"host-a");
  let mut args = frame(1, 1);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"host-b");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  assert_eq!(
    Client::call(&mut service, &mut live, args.as_slice()).status,
    Nfsstat4::Resource.wire()
  );

  const LEASE_NS: u64 = 1_000;
  let mut lapsing = server_with(1, LEASE_NS);
  let mut first = Client::connect(&mut service, &mut lapsing, b"host-a");
  let owner = first.owner.clone();
  open_at_root(
    &mut first,
    &mut service,
    &mut lapsing,
    &owner,
    "held",
    READ,
    DENY_NONE,
  )
  .unwrap();
  assert_eq!(lapsing.open_count(), 1);
  // At exactly one lease since its last request the client still holds its place.
  NOW_NS.with(|now| now.set(now.get() + LEASE_NS));
  let mut refused = frame(1, 1);
  refused.u32(op::EXCHANGE_ID);
  refused.fixed(&[1; 8]);
  refused.opaque(b"host-b");
  refused.u32(0);
  refused.u32(0);
  refused.u32(0);
  assert_eq!(
    Client::call(&mut service, &mut lapsing, refused.as_slice()).status,
    Nfsstat4::Resource.wire(),
    "a lease is not lapsed until it has passed"
  );
  NOW_NS.with(|now| now.set(now.get() + 1));
  Client::connect(&mut service, &mut lapsing, b"host-b");
  assert_eq!(
    lapsing.sessions.client_count(),
    1,
    "the lapsed client made room"
  );
  assert_eq!(lapsing.open_count(), 0, "and its open went with it");
}

/// Hostile compounds are refused with a status, never a panic: truncated arguments, a length word of
/// `u32::MAX`, more operations than the channel carries, and a name with a separator. A-35.
#[test]
fn hostile_compounds_are_refused_with_a_status() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
  let mut service = export(&mut bridge, 0);
  let mut server = Server::standalone();

  let truncated = [0u8, 0, 0];
  assert_eq!(
    Client::call(&mut service, &mut server, &truncated).status,
    Nfsstat4::Badxdr.wire()
  );

  let mut huge_tag = XdrWriter::new();
  huge_tag.u32(u32::MAX);
  assert_eq!(
    Client::call(&mut service, &mut server, huge_tag.as_slice()).status,
    Nfsstat4::Badxdr.wire()
  );

  let too_many = frame(1, u32::MAX);
  assert_eq!(
    Client::call(&mut service, &mut server, too_many.as_slice()).status,
    Nfsstat4::TooManyOps.wire()
  );

  let mut client = Client::connect(&mut service, &mut server, b"host-a");
  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.opaque(b"a/b");
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Badname.wire()
  );

  let mut args = client.sequenced(2);
  args.u32(op::PUTROOTFH);
  args.u32(op::LOOKUP);
  args.u32(u32::MAX);
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Badxdr.wire()
  );

  let mut args = client.sequenced(1);
  args.u32(op::GETFH);
  assert_eq!(
    Client::call(&mut service, &mut server, args.as_slice()).status,
    Nfsstat4::Nofilehandle.wire()
  );
}

/// Sends one ONC RPC call (AUTH_NONE) for `program` `version` over `stream`: the reply's accept status
/// and what follows it.
fn rpc(
  stream: &mut std::net::TcpStream,
  program: u32,
  version: u32,
  procedure: u32,
  args: &[u8],
) -> (u32, Vec<u8>) {
  use std::io::{Read, Write};
  let mut body = XdrWriter::new();
  for field in [1, 0, 2, program, version, procedure, 0, 0, 0, 0] {
    body.u32(field); // xid, CALL, RPC version 2, the call, AUTH_NONE credential and verifier
  }
  body.fixed(args);
  let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
  stream.write_all(&marker.to_be_bytes()).unwrap();
  stream.write_all(body.as_slice()).unwrap();
  let mut marker = [0u8; 4];
  stream.read_exact(&mut marker).unwrap();
  let mut reply = vec![0u8; (u32::from_be_bytes(marker) & 0x7fff_ffff) as usize];
  stream.read_exact(&mut reply).unwrap();
  let mut reader = XdrReader::new(&reply);
  reader.fixed(12).unwrap(); // xid, REPLY, MSG_ACCEPTED
  reader.u32().unwrap();
  reader.opaque(400).unwrap(); // the verifier
  let accept = reader.u32().unwrap();
  (accept, reader.rest().to_vec())
}

/// Over a real socket, version 4 of the NFS program is served beside version 3 and any other version
/// is `PROG_MISMATCH` naming 3–4; a session made on one connection carries on over the next, since the
/// listener, not the connection, holds the v4 state (RFC 8881 §2.10.3: a client reconnects and
/// continues its session). A-35.
#[test]
fn a_session_outlives_its_connection_and_other_versions_are_mismatched() {
  use std::net::{TcpListener, TcpStream};
  /// `PROG_MISMATCH` and `SUCCESS` accept statuses; the NFS program number (RFC 5531, RFC 1813).
  const SUCCESS: u32 = 0;
  const PROG_MISMATCH: u32 = 2;
  const NFS_PROGRAM: u32 = 100_003;
  const CONNECTIONS: usize = 2;
  let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
  let port = listener.local_addr().unwrap().port();
  let serving = std::thread::spawn(move || {
    let mut store = store();
    let mut vol = volume(&mut store);
    let mut bridge = VolumeBridge::new(VOLUME, &mut vol, &mut store);
    let mut service = export(&mut bridge, 0);
    let mut v4 = Server::standalone();
    for _ in 0..CONNECTIONS {
      let (mut stream, _) = listener.accept().unwrap();
      slates_bridge_nfs::serve_connection(&mut stream, &mut service, &mut v4, port);
    }
  });

  let mut first = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let (accept, range) = rpc(&mut first, NFS_PROGRAM, 2, 0, &[]);
  assert_eq!(accept, PROG_MISMATCH);
  let mut range = XdrReader::new(&range);
  assert_eq!((range.u32().unwrap(), range.u32().unwrap()), (3, 4));
  assert_eq!(
    rpc(&mut first, NFS_PROGRAM, 4, 0, &[]).0,
    SUCCESS,
    "v4 NULL"
  );
  assert_eq!(
    rpc(&mut first, NFS_PROGRAM, 3, 0, &[]).0,
    SUCCESS,
    "v3 NULL"
  );

  let mut args = frame(2, 1);
  args.u32(op::EXCHANGE_ID);
  args.fixed(&[1; 8]);
  args.opaque(b"host-a");
  args.u32(0);
  args.u32(0);
  args.u32(0);
  let (_, reply) = rpc(&mut first, NFS_PROGRAM, 4, 1, args.as_slice());
  let body = decode_frame(reply).walk_owned();
  let mut body = XdrReader::new(&body);
  expect_ok(&mut body, op::EXCHANGE_ID);
  let clientid = body.u64().unwrap();
  let sequenceid = body.u32().unwrap();
  let mut args = frame(2, 1);
  args.u32(op::CREATE_SESSION);
  args.u64(clientid);
  args.u32(sequenceid);
  args.u32(0);
  let asked = ChannelAttrs {
    max_request: 1 << 20,
    max_response: 1 << 20,
    max_response_cached: 1 << 20,
    max_operations: 16,
    max_requests: 1,
    ..ChannelAttrs::default()
  };
  asked.encode(&mut args);
  asked.encode(&mut args);
  args.u32(0);
  args.u32(0);
  let (_, reply) = rpc(&mut first, NFS_PROGRAM, 4, 1, args.as_slice());
  let body = decode_frame(reply).walk_owned();
  let mut body = XdrReader::new(&body);
  expect_ok(&mut body, op::CREATE_SESSION);
  let mut sessionid = [0u8; 16];
  sessionid.copy_from_slice(body.fixed(16).unwrap());
  drop(first);

  let mut second = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let client = Client {
    owner: b"host-a".to_vec(),
    clientid,
    sessionid,
    sequence: 0,
  };
  let mut args = client.sequenced_at(1, 1);
  args.u32(op::PUTROOTFH);
  let (accept, reply) = rpc(&mut second, NFS_PROGRAM, 4, 1, args.as_slice());
  assert_eq!(accept, SUCCESS);
  assert_eq!(
    decode_frame(reply).status,
    Nfsstat4::Ok.wire(),
    "the session made on the first connection serves the second"
  );
  drop(second);
  serving.join().unwrap();
}
