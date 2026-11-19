//! Golden tests of the wire framings (A-569): every SSH session and HTTP exchange stock Mercurial 7.2 made in
//! `fixtures/wire` (a clone, a pull, a push, `debugwireargs` through each way of carrying arguments, a bundle1 client
//! refused), read by this crate's codecs as 7.2 read them, every request and answer written again to its bytes.

use crate::bundle2::Bundle2;
use crate::fixtures::wire::file;
use crate::wire::{Arguments, Spec};
use crate::{Admission, batch, capabilities, http, pushkey, ssh};

/// 7.2's argument specs for the commands the captured sessions send (`wireprotov1server.py`).
const SPECS: [(&[u8], &[u8]); 9] = [
  (b"hello", b""),
  (b"between", b"pairs"),
  (b"protocaps", b"caps"),
  (b"batch", b"cmds *"),
  (b"getbundle", b"*"),
  (b"debugwireargs", b"one two *"),
  (b"listkeys", b"namespace"),
  (b"branchmap", b""),
  (b"unbundle", b"heads"),
];

/// The arguments `debugwireargs` was given in every session: `one`, `two`, then `--three` and `--four`.
const THREE: &[u8] = b"a b=c&d+e%f;g,h:i";

/// See [`THREE`]: `--four`, a word of UTF-8.
const FOUR: &[u8] = b"caf\xc3\xa9";

fn room() -> Admission {
  Admission::new(1 << 20)
}

fn spec(command: &[u8]) -> Spec<'static> {
  Spec(
    SPECS
      .iter()
      .find(|(name, _)| *name == command)
      .map(|(_, spec)| *spec)
      .unwrap(),
  )
}

/// One request a client sent: its command, its arguments, its bytes without `unbundle`'s input, and that input.
struct Sent<'a> {
  command: &'a [u8],
  arguments: Arguments<'a>,
  bytes: &'a [u8],
  input: Vec<u8>,
}

/// `unbundle`'s chunked input at the front of `rest`, joined, and the bytes it took.
fn chunks(rest: &[u8]) -> (Vec<u8>, usize) {
  let (mut input, mut at) = (Vec::new(), 0);
  while let ssh::Read::Done(Some(data), used) = ssh::chunk(&rest[at..], &room()).unwrap() {
    input.extend_from_slice(data);
    at += used;
  }
  assert_eq!(
    ssh::chunk(&rest[at..], &room()),
    Ok(ssh::Read::Done(None, 2))
  );
  (input, at + 2)
}

/// Every request of a client's capture, read with the SSH codec.
fn requests(capture: &[u8]) -> Vec<Sent<'_>> {
  let longest = SPECS.iter().map(|(name, _)| name.len()).max().unwrap();
  let (mut sent, mut at) = (Vec::new(), 0);
  while at < capture.len() {
    let rest = &capture[at..];
    let Ok(ssh::Read::Done(command, used)) = ssh::command(rest, longest) else {
      panic!("a command line at {at}")
    };
    let Ok(ssh::Read::Done(arguments, read)) =
      ssh::arguments(&rest[used..], spec(command), &mut room())
    else {
      panic!("{} arguments at {at}", String::from_utf8_lossy(command))
    };
    let (input, taken) = if command == b"unbundle" {
      chunks(&rest[used + read..])
    } else {
      (Vec::new(), 0)
    };
    sent.push(Sent {
      command,
      arguments,
      bytes: &rest[..used + read],
      input,
    });
    at += used + read + taken;
  }
  sent
}

/// A request written again as 7.2's client writes it.
fn written(sent: &Sent<'_>) -> Vec<u8> {
  let spec = spec(sent.command);
  let named: Vec<(&[u8], &[u8])> = spec
    .named()
    .zip(sent.arguments.named.iter().copied())
    .collect();
  let dictionary = spec
    .takes_dictionary()
    .then_some(sent.arguments.dictionary.as_slice());
  let mut out = Vec::new();
  ssh::encode_request(sent.command, &named, dictionary, &mut out);
  out
}

/// The string answers at the front of a server's capture, `count` of them, and the bytes they took.
fn answers(capture: &[u8], count: usize) -> (Vec<&[u8]>, usize) {
  let (mut found, mut at) = (Vec::new(), 0);
  for _ in 0..count {
    let Ok(ssh::Read::Done(value, used)) = ssh::string(&capture[at..], &room()) else {
      panic!("a string answer at {at}")
    };
    found.push(value);
    at += used;
  }
  (found, at)
}

/// Where the bundle2 stream at the front of `bytes` ends: its frames walked as 7.2's reader walks them.
fn stream_end(bytes: &[u8]) -> usize {
  let length = |at: usize| i32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
  let mut at = 8 + usize::try_from(length(4)).unwrap();
  loop {
    let header = usize::try_from(length(at)).unwrap();
    at += 4 + header;
    if header == 0 {
      return at;
    }
    loop {
      let chunk = usize::try_from(length(at)).unwrap();
      at += 4 + chunk;
      if chunk == 0 {
        break;
      }
    }
  }
}

/// A-569 (golden): do read the client's side of `debugwireargs` over SSH. Expect the handshake (`hello`, `between`
/// with forty zeros twice, `protocaps` with the engines it decodes), then `debugwireargs` twice with its named
/// arguments and its dictionary, and every request written again to its bytes.
#[test]
fn an_ssh_clients_requests_read_as_mercurial_wrote_them() {
  let capture = file("ssh-wireargs.client");
  let sent = requests(capture);
  let commands: Vec<&[u8]> = sent.iter().map(|sent| sent.command).collect();
  assert_eq!(
    commands,
    [
      &b"hello"[..],
      b"between",
      b"protocaps",
      b"debugwireargs",
      b"debugwireargs"
    ]
  );
  let zeros = [b'0'; 40];
  assert_eq!(
    sent[1].arguments.named,
    [[&zeros[..], b"-", &zeros[..]].concat().as_slice()]
  );
  assert_eq!(
    sent[2].arguments.named,
    [&b"comp=zstd,zlib,none,bzip2 partial-pull"[..]]
  );
  let expected = Arguments {
    named: vec![&b"un"[..], &b"deux"[..]],
    dictionary: vec![(&b"three"[..], THREE), (&b"four"[..], FOUR)],
  };
  assert_eq!(sent[3].arguments, expected);
  assert!(sent.iter().all(|sent| written(sent) == sent.bytes));
  assert_eq!(
    sent.iter().map(|sent| sent.bytes.len()).sum::<usize>(),
    capture.len()
  );
}

/// A-569 (golden): do read the server's answers to that session. Expect `hello`'s capabilities line (read and written
/// again to its bytes), `between`'s empty line, `protocaps`' `OK`, `debugwireargs`' echo of its arguments twice, and
/// every answer framed again by `ssh::respond` into the server's bytes.
#[test]
fn an_ssh_servers_answers_read_and_frame_as_mercurial_framed_them() {
  let capture = file("ssh-wireargs.server");
  let (found, used) = answers(capture, 5);
  assert_eq!(used, capture.len());
  let caps = capabilities::decode_hello(found[0]).unwrap();
  let mut hello = Vec::new();
  capabilities::encode_hello(&caps_line(caps), &mut hello);
  assert_eq!(hello, found[0]);
  assert_eq!((found[1], found[2]), (&b"\n"[..], &b"OK"[..]));
  let echo = file("ssh-wireargs.answer");
  assert_eq!(found[3], &echo[..echo.len() - 1]);
  let mut framed = Vec::new();
  for value in &found {
    ssh::respond(value, &mut framed);
  }
  assert_eq!(framed, capture);
}

/// A capabilities line written again from what was read of it.
fn caps_line(caps: capabilities::Capabilities<'_>) -> Vec<u8> {
  let read: Vec<(&[u8], Option<&[u8]>)> = caps.iter().collect();
  let mut line = Vec::new();
  capabilities::encode(&read, &mut line).unwrap();
  line
}

/// A-569 (golden): do read the bundle2 capability blob 7.2's server advertises, through both of its quotings, and the
/// one its client sends in `getbundle`'s `bundlecaps`. Expect `HG20`, changegroups `01` to `03`, the errors it
/// reports and the rest, each blob written again to its bytes, and the client's blob the server's.
#[test]
fn the_bundle2_capability_blob_reads_and_writes_back() {
  let (found, _) = answers(file("ssh-wireargs.server"), 1);
  let caps = capabilities::decode_hello(found[0]).unwrap();
  let value = caps.get(capabilities::BUNDLE2).unwrap().unwrap();
  let (mut blob, mut buffer) = (Vec::new(), Vec::new());
  let blob = capabilities::decode_bundle2_value(value, &mut room(), &mut blob).unwrap();
  let read = capabilities::decode_bundle2(blob, &mut room(), &mut buffer).unwrap();
  let groups: [&[u8]; 3] = [b"01", b"02", b"03"];
  assert_eq!(read.get(b"changegroup"), Some(&groups[..]));
  assert_eq!(read.get(b"HG20").map(<[&[u8]]>::len), Some(0));
  let entries: Vec<(&[u8], &[&[u8]])> = read.iter().collect();
  let mut again = Vec::new();
  capabilities::encode_bundle2(&entries, &mut again);
  assert_eq!(again, blob);
  let mut quoted = Vec::new();
  capabilities::encode_bundle2_value(&again, &mut quoted);
  assert_eq!(quoted, value);
  let pull = requests(file("ssh-pull.client"));
  let bundlecaps = pull[4].arguments.dictionary[0].1;
  assert_eq!(bundlecaps, [&b"HG20,bundle2="[..], value].concat());
}

/// A-569 (golden): do read a pull over SSH. Expect discovery's `batch` (`heads` with its space and no arguments,
/// `known` with the node the client holds) written again to its bytes, `getbundle`'s dictionary in the client's
/// order, the batch's answers read back (the server's head and `1`), and the bundle2 stream after them whole.
#[test]
fn an_ssh_pull_batches_discovery_then_takes_a_stream() {
  let sent = requests(file("ssh-pull.client"));
  let cmds = sent[3].arguments.get(spec(b"batch"), b"cmds").unwrap();
  let mut buffer = Vec::new();
  let calls = batch::calls(cmds, &mut room(), &mut buffer).unwrap();
  let pairs: Vec<Vec<(&[u8], &[u8])>> = calls.iter().map(|call| call.arguments.clone()).collect();
  let encoded: Vec<(&[u8], &[(&[u8], &[u8])])> = calls
    .iter()
    .zip(&pairs)
    .map(|(call, pairs)| (call.command, pairs.as_slice()))
    .collect();
  let mut again = Vec::new();
  batch::encode_calls(&encoded, &mut again);
  assert_eq!(again, cmds);
  assert_eq!(
    (calls[0].command, calls[1].arguments[0].0),
    (&b"heads"[..], &b"nodes"[..])
  );
  let keys: Vec<&[u8]> = sent[4]
    .arguments
    .dictionary
    .iter()
    .map(|(key, _)| *key)
    .collect();
  assert_eq!(
    keys,
    [
      &b"bundlecaps"[..],
      b"common",
      b"heads",
      b"cg",
      b"phases",
      b"bookmarks",
      b"listkeys"
    ]
  );
  let server = file("ssh-pull.server");
  let (found, used) = answers(server, 4);
  let mut results = Vec::new();
  let read = batch::results(found[3], &mut room(), &mut results).unwrap();
  assert_eq!(read[1], b"1");
  assert!(Bundle2::decode(&server[used..], &mut room()).is_ok());
}

/// A-569 (golden): do read a push over SSH. Expect `unbundle`'s `heads` (`force`, hex), its input read chunk by chunk
/// into the bundle2 stream the client pushed, the server's empty string before it, the reply stream after it whole,
/// and the `listkeys phases` answer after that.
#[test]
fn an_ssh_push_sends_its_bundle_in_chunks() {
  let sent = requests(file("ssh-push.client"));
  let unbundle = sent
    .iter()
    .find(|sent| sent.command == b"unbundle")
    .unwrap();
  assert_eq!(unbundle.arguments.named, [&b"666f726365"[..]]);
  assert!(Bundle2::decode(&unbundle.input, &mut room()).is_ok());
  assert_eq!(sent.last().map(|sent| sent.command), Some(&b"listkeys"[..]));
  let server = file("ssh-push.server");
  let (found, used) = answers(server, 9);
  assert_eq!(found[8], b"");
  let end = used + stream_end(&server[used..]);
  assert!(Bundle2::decode(&server[used..end], &mut room()).is_ok());
  let (last, rest) = answers(&server[end..], 1);
  assert_eq!(end + rest, server.len());
  assert_eq!(
    pushkey::listkeys(last[0]).unwrap().collect::<Vec<_>>(),
    [(&b"publishing"[..], &b"True"[..])]
  );
}

/// One HTTP request as the capture holds it: its query, its headers and its body.
struct Raw<'a> {
  query: &'a [u8],
  headers: Vec<(&'a [u8], &'a [u8])>,
  body: &'a [u8],
}

/// Splits a captured request at its head's end, its request line at `?`, each header line at `: `.
fn raw(request: &[u8]) -> Raw<'_> {
  let end = request
    .windows(4)
    .position(|window| window == b"\r\n\r\n")
    .unwrap();
  let mut lines = request[..end]
    .split(|byte| *byte == b'\n')
    .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
  let target = lines
    .next()
    .unwrap()
    .split(|byte| *byte == b' ')
    .nth(1)
    .unwrap();
  let query = target
    .splitn(2, |byte| *byte == b'?')
    .nth(1)
    .unwrap_or_default();
  let headers = lines
    .map(|line| {
      let colon = line.iter().position(|byte| *byte == b':').unwrap();
      (&line[..colon], line[colon + 1..].trim_ascii())
    })
    .collect();
  Raw {
    query,
    headers,
    body: &request[end + 4..],
  }
}

/// A captured exchange's request, response type and response body.
fn exchange(name: &str, number: u32) -> (Vec<u8>, Vec<u8>, &'static [u8]) {
  let request = file(&format!("{name}.{number:02}.request")).to_vec();
  let head = file(&format!("{name}.{number:02}.response-head"));
  let content_type = raw(&[head, b"\r\n".as_slice()].concat())
    .headers
    .iter()
    .find(|(name, _)| name.eq_ignore_ascii_case(b"content-type"))
    .unwrap()
    .1
    .to_vec();
  (
    request,
    content_type,
    file(&format!("{name}.{number:02}.response-body")),
  )
}

/// The arguments of `debugwireargs` as a captured HTTP request carries them, read for its spec.
fn wireargs(name: &str) -> (Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>) {
  let (request, _, _) = exchange(name, 2);
  let parsed = raw(&request);
  let request = http::Request {
    query: parsed.query,
    headers: &parsed.headers,
    body: parsed.body,
  };
  let mut buffer = Vec::new();
  let fields = http::fields(&request, &mut room(), &mut buffer).unwrap();
  let arguments = fields
    .arguments(spec(b"debugwireargs"), &mut room())
    .unwrap();
  (
    fields.command().unwrap().to_vec(),
    arguments
      .dictionary
      .iter()
      .map(|(key, value)| (key.to_vec(), value.to_vec()))
      .collect(),
    arguments.named.iter().map(|value| value.to_vec()).collect(),
  )
}

/// A-569 (golden): do read `debugwireargs` as 7.2's client sent it over HTTP three ways: in `X-HgArg` headers, at the
/// body's front behind `X-HgArgs-Post`, and in the query. Expect the same command, the same named arguments and the
/// same dictionary (in 7.2's order: `four`, then `three`) all three ways, the UTF-8 value as sent.
#[test]
fn http_arguments_read_alike_in_headers_body_and_query() {
  let dictionary = vec![
    (b"four".to_vec(), FOUR.to_vec()),
    (b"three".to_vec(), THREE.to_vec()),
  ];
  for name in ["http-wireargs", "http-postargs", "http-query"] {
    let (command, read, named) = wireargs(name);
    assert_eq!(command, b"debugwireargs", "{name}");
    assert_eq!(named, [b"un".to_vec(), b"deux".to_vec()], "{name}");
    assert_eq!(read, dictionary, "{name}");
  }
}

/// A-569 (golden): do write `debugwireargs`' arguments as 7.2's client wrote them: form-encoded and sorted, split
/// into `X-HgArg` headers at 7.2's default limit, at the body's front with its length, and after `cmd` in the query.
/// Expect each request's own bytes, and the `X-HgProto` header naming `0.1`, `0.2` and the engines in its order.
#[test]
fn http_arguments_write_as_mercurials_client_wrote_them() {
  let fields: [(&[u8], &[u8]); 4] = [
    (b"four", FOUR),
    (b"one", b"un"),
    (b"three", THREE),
    (b"two", b"deux"),
  ];
  let mut form = Vec::new();
  http::encode_form(&fields, &mut form);
  let (headers, _, _) = exchange("http-wireargs", 2);
  let parsed = raw(&headers);
  let values: Vec<&[u8]> = http::header_values(&form, http::ARGUMENT_HEADER, 1024)
    .unwrap()
    .collect();
  assert_eq!(
    values,
    [parsed
      .headers
      .iter()
      .find(|(name, _)| *name == b"x-hgarg-1")
      .unwrap()
      .1]
  );
  let (post, _, _) = exchange("http-postargs", 2);
  assert_eq!(raw(&post).body, form);
  let (query, _, _) = exchange("http-query", 2);
  assert_eq!(
    raw(&query).query,
    [&b"cmd=debugwireargs&"[..], &form].concat()
  );
  let request = http::Request {
    query: parsed.query,
    headers: &parsed.headers,
    body: parsed.body,
  };
  let caps = http::protocol_caps(&request, &mut room()).unwrap();
  assert!(caps.has(b"0.1") && caps.has(b"0.2") && caps.has(b"partial-pull"));
  assert_eq!(
    caps.engines().collect::<Vec<_>>(),
    [&b"zstd"[..], b"zlib", b"none", b"bzip2"]
  );
}

/// A-569 (golden): do read the answers of a clone over HTTP. Expect `capabilities` as `0.1` (its line written again to
/// its bytes), `getbundle` as `0.2` naming `zstd` by default, and naming `none` where the server offers only that,
/// its payload then a bundle2 stream that decodes whole.
#[test]
fn http_answers_name_their_media_type_and_engine() {
  let (_, content_type, body) = exchange("http-clone", 1);
  assert_eq!(content_type, http::MEDIA_TYPE_V1);
  assert_eq!(caps_line(capabilities::decode(body)), body);
  let (_, compressed_type, compressed) = exchange("http-clone", 3);
  assert_eq!(compressed_type, http::MEDIA_TYPE_V2);
  assert_eq!(
    http::decode_v2(compressed).map(|(engine, _)| engine),
    Ok(&b"zstd"[..])
  );
  let (_, plain_type, plain) = exchange("http-none", 3);
  assert_eq!(plain_type, http::MEDIA_TYPE_V2);
  let (engine, payload) = http::decode_v2(plain).unwrap();
  assert_eq!(engine, b"none");
  assert!(Bundle2::decode(payload, &mut room()).is_ok());
}

/// A-569 (golden): do read a push over HTTP and a bundle1 client's refusal. Expect `unbundle`'s `heads` in its header
/// and its body the bundle2 stream whole; and the refusal answered `application/hg-error`, its body the message the
/// client printed.
#[test]
fn an_http_push_carries_its_bundle_and_a_refusal_its_message() {
  let (request, _, _) = exchange("http-push", 7);
  let parsed = raw(&request);
  let request = http::Request {
    query: parsed.query,
    headers: &parsed.headers,
    body: parsed.body,
  };
  let mut buffer = Vec::new();
  let fields = http::fields(&request, &mut room(), &mut buffer).unwrap();
  assert_eq!(fields.value(b"heads"), Some(&b"666f726365"[..]));
  assert!(Bundle2::decode(http::payload(&request).unwrap(), &mut room()).is_ok());
  let (_, content_type, body) = exchange("http-oob", 4);
  assert_eq!(content_type, http::MEDIA_TYPE_ERROR);
  let printed = file("http-oob.client-stderr");
  assert!(printed.ends_with(body));
}
