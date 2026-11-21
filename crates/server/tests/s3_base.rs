//! A volume over an S3 base, made through the wire (§4.15 "An S3 base", A-555): `CreateOverS3` names the base and hands
//! the credential its fetches are signed with; the daemon makes the overlay over the base's objects, starts the base's
//! fetch driver on the volume's owner shard, and a read of a base file is answered from the store once its fetches
//! land. The store here is a loopback S3 of the test's own, answering `ListObjectsV2` and ranged `GetObject` as S3's API
//! reference states them, over plain HTTP, which the client allows to loopback alone.
// Test harness code: an unwrap here is a failed test, which is what it should be; the store is a thread of the test's
// own on a loopback socket (std::net), as the test of S3's client serves its store.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice,
  clippy::unwrap_in_result,
  clippy::disallowed_methods,
  clippy::disallowed_types
)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use slates_client::{Client, CreateSpec, NamePolicy, SizeClass, VolumeId};
use slates_ipc::protocol::{ReadAt, S3BaseSpec, S3Credential};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;

/// Shape: one shard, the volume's owner.
const TEST_SHARDS: u16 = 1;
/// Shape: the overlay's growth bound, room for the base's file many times over.
const OVERLAY_BYTES: u64 = 1 << 26;
/// Shape: the bucket and the prefix the base reads.
const BUCKET: &str = "bucket";
/// Shape: the prefix, ending in `/`.
const PREFIX: &str = "base/";
/// Shape: the one object under the prefix, by its name in the volume.
const FILE: &str = "hello.txt";
/// Shape: its bytes.
const BYTES: &[u8] = b"hello from an S3 base\n";
/// Shape: its ETag, quotes and all, as S3 states one.
const ETAG: &str = "\"5d41402abc4b2a76b9719d911017c592\"";
/// Shape: how long a read may take to be answered while its fetches land: far past a loopback store's round trips,
/// short enough that a fetch never started fails the test rather than hanging it.
const READ_WITHIN: Duration = Duration::from_secs(30);
/// Shape: the pause between a read answered "not yet" and the next.
const RETRY_PAUSE: Duration = Duration::from_millis(20);

fn daemon(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-s3-base-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-s3-base-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The test's store: one object, `PREFIX` + `FILE`, served on a loopback listener; its port.
fn store() -> u16 {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let port = listener.local_addr().unwrap().port();
  std::thread::spawn(move || {
    for stream in listener.incoming() {
      let Ok(stream) = stream else { return };
      std::thread::spawn(move || serve(stream));
    }
  });
  port
}

/// Serves one connection: each request head read whole, answered by [`answer`], until the client closes it.
fn serve(mut stream: TcpStream) {
  let mut buffer = Vec::new();
  let mut chunk = [0u8; 4096];
  loop {
    while let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
      let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
      buffer.drain(..end + 4);
      if stream.write_all(&answer(&head)).is_err() {
        return;
      }
    }
    match stream.read(&mut chunk) {
      Ok(0) | Err(_) => return,
      Ok(n) => buffer.extend_from_slice(&chunk[..n]),
    }
  }
}

/// The answer to one request head: a delimited listing of the prefix, or a range of the object.
fn answer(head: &str) -> Vec<u8> {
  let target = head.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("");
  if target.contains("list-type=2") {
    let key = format!("{PREFIX}{FILE}");
    let body = format!(
      "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult \
       xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{BUCKET}</Name><Prefix>{PREFIX}</Prefix>\
       <KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys><Delimiter>/</Delimiter><EncodingType>url</EncodingType>\
       <IsTruncated>false</IsTruncated><Contents><Key>{key}</Key><LastModified>2026-11-20T00:00:00.000Z</LastModified>\
       <ETag>&quot;5d41402abc4b2a76b9719d911017c592&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>\
       </Contents></ListBucketResult>",
      BYTES.len()
    );
    return response("200 OK", &[("Content-Type", "application/xml".to_owned())], body.as_bytes());
  }
  if target == format!("/{BUCKET}/{PREFIX}{FILE}") {
    let range = head
      .lines()
      .find_map(|line| line.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_owned))
      .unwrap_or_default();
    let (first, last) = range.split_once('-').unwrap_or(("0", ""));
    let first: usize = first.trim().parse().unwrap_or(0);
    let last: usize = last.trim().parse().unwrap_or(BYTES.len() - 1).min(BYTES.len() - 1);
    let bytes = &BYTES[first..=last];
    return response(
      "206 Partial Content",
      &[
        ("Content-Range", format!("bytes {first}-{last}/{}", BYTES.len())),
        ("ETag", ETAG.to_owned()),
        ("Last-Modified", "Fri, 20 Nov 2026 00:00:00 GMT".to_owned()),
        ("Content-Type", "application/octet-stream".to_owned()),
      ],
      bytes,
    );
  }
  response("404 Not Found", &[], b"")
}

/// An HTTP/1.1 response of `status` with `fields` and `body`, its length stated.
fn response(status: &str, fields: &[(&str, String)], body: &[u8]) -> Vec<u8> {
  let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
  for (name, value) in fields {
    out.push_str(&format!("{name}: {value}\r\n"));
  }
  out.push_str("\r\n");
  let mut out = out.into_bytes();
  out.extend_from_slice(body);
  out
}

/// The base the test's store at `port` holds, over plain HTTP to loopback, path-style.
fn base(port: u16) -> S3BaseSpec {
  S3BaseSpec {
    endpoint: format!("http://127.0.0.1:{port}"),
    region: "us-east-1".to_owned(),
    bucket: BUCKET.to_owned(),
    prefix: PREFIX.to_owned(),
    path_style: true,
    authorities: Vec::new(),
  }
}

/// A credential the test's store takes (it checks no signature).
fn credential() -> S3Credential {
  S3Credential {
    access_key_id: "AKIDEXAMPLE".to_owned(),
    secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
    session_token: None,
  }
}

/// `path` of `volume` read through the wire, asked again while the daemon answers that its base is still fetching,
/// within [`READ_WITHIN`]: the bytes, or the last refusal.
fn read_settled(client: &mut Client, volume: VolumeId, path: &str) -> Result<Vec<u8>, String> {
  let until = Instant::now() + READ_WITHIN;
  loop {
    match client.read(volume, path, ReadAt::Head) {
      Ok(bytes) => return Ok(bytes),
      Err(e) if Instant::now() < until && format!("{e:?}").contains("Unpublished") => std::thread::sleep(RETRY_PAUSE),
      Err(e) => return Err(format!("{e:?}")),
    }
  }
}

/// A-555 (the wire's create naming an S3 base, the credential's hand-off, the driver's start). Do: make a volume over
/// the test store's base through `CreateOverS3`, then read its one file through the wire. Expect the volume made, and
/// the file's bytes as the store holds them, answered once the base's listing and range fetches land: the daemon's
/// driver ran them against the store with the credential the create handed it.
#[test]
fn a_volume_made_over_an_s3_base_reads_its_objects_from_the_store() {
  let port = store();
  let (daemon, instance) = daemon("read");
  let mut client = connect(&instance);
  let volume = client
    .create_over_s3(
      &CreateSpec {
        name: "over-s3".into(),
        size: SizeClass::Dynamic { max: OVERLAY_BYTES },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
      &base(port),
      &credential(),
    )
    .unwrap();
  let read = read_settled(&mut client, volume, &format!("/{FILE}"));
  assert_eq!(read.as_deref(), Ok(BYTES), "the base file, as the store holds it");
  drop(client);
  daemon.stop();
}

/// A-555. Do: name a credential in `Debug` text. Expect the access key named and neither the secret nor the session
/// token, so a logged request never carries them.
#[test]
fn a_credential_s_debug_text_never_names_its_secret() {
  let mut credential = credential();
  credential.session_token = Some("FwoGZXIvYXdzEXAMPLETOKEN".to_owned());
  let text = format!("{credential:?}");
  assert!(text.contains("AKIDEXAMPLE"), "{text}");
  assert!(!text.contains("wJalrXUtnFEMI") && !text.contains("FwoGZXIvYXdz"), "{text}");
}
