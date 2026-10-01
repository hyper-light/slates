//! The MCP server's tests (§4.12, Phase 6 task 8): the tools driven through MCP tool calls against a
//! live in-process daemon — the protocol handshake, the whole merge loop (with a concurrent
//! conflict), the volume lifecycle, and typed JSON-RPC errors — asserting on the results an agent
//! would receive. The scenarios share one daemon in one serial test so their spinning shards do not
//! contend (the daemon-per-test contention the client and server tests also avoid).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use slates_client::{Client, Deadlines};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_mcp::McpServer;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(5),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures")
}

fn connect(instance: &str) -> Client {
  // The product's own deadlines, never a hand-picked reply clock
  // (`docs/bugs/2026-09-28-the-client-tests-judged-a-live-daemon-by-a-shorter-clock.md`).
  let deadlines = Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines) {
      Ok(client) => return client,
      Err(slates_client::ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < Duration::from_secs(5) =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

/// Calls a tool and returns its `structuredContent`, failing on a JSON-RPC error.
fn call(server: &mut McpServer, name: &str, arguments: Value) -> Value {
  let request = json!({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": { "name": name, "arguments": arguments },
  });
  let reply = server.handle(&request).expect("a call gets a reply");
  assert!(reply.get("error").is_none(), "tool {name} errored: {reply}");
  reply["result"]["structuredContent"].clone()
}

/// The protocol handshake: initialize reports the version and identity, tools/list offers the merge
/// tools and no grant tool (R10), and a notification (no id) gets no reply.
fn assert_protocol(server: &mut McpServer) {
  let init = server
    .handle(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" }))
    .unwrap();
  assert_eq!(init["result"]["protocolVersion"], "2026-07-28");
  assert_eq!(init["result"]["serverInfo"]["name"], "slates");

  let list = server
    .handle(&json!({ "jsonrpc": "2.0", "id": 0, "method": "tools/list" }))
    .unwrap();
  let names: Vec<String> = list["result"]["tools"]
    .as_array()
    .unwrap()
    .iter()
    .map(|t| t["name"].as_str().unwrap().to_owned())
    .collect();
  assert!(names.contains(&"slates.merge.submit".to_owned()));
  assert!(names.contains(&"slates.volume.create".to_owned()));
  assert!(names.contains(&"slates.land.materialize".to_owned()));
  assert!(
    !names.iter().any(|n| n.contains("grant")),
    "no grant tool is offered: {names:?}"
  );

  assert!(
    server
      .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
      .is_none(),
    "a notification gets no reply"
  );
}

/// The merge loop over MCP: create a green and two works (both based on version 0), edit, submit,
/// read the chain, and see the second work conflict on the same file rather than clobbering the first.
fn assert_merge_loop(server: &mut McpServer) {
  let green = call(server, "slates.merge.create_green", json!({ "name": "g" }))["green"]
    .as_str()
    .unwrap()
    .to_owned();
  let new_work = |server: &mut McpServer, name: &str| -> String {
    let work = call(
      server,
      "slates.merge.create_work",
      json!({ "green": green, "name": name }),
    );
    assert_eq!(work["base"], 0, "a work over a fresh green is based on 0");
    work["work"].as_str().unwrap().to_owned()
  };
  let work_a = new_work(server, "a");
  let work_b = new_work(server, "b");
  call(
    server,
    "slates.merge.edit",
    json!({ "work": work_a, "path": "f", "at": 0, "text": "hello" }),
  );
  call(
    server,
    "slates.merge.edit",
    json!({ "work": work_b, "path": "f", "at": 0, "text": "world" }),
  );

  let submitted = call(server, "slates.merge.submit", json!({ "work": work_a }));
  assert_eq!(submitted, json!({ "accepted": true, "version": 1 }));
  assert_eq!(
    call(server, "slates.merge.versions", json!({ "green": green }))["head"],
    1
  );
  assert_eq!(
    call(
      server,
      "slates.merge.changed_since",
      json!({ "green": green, "version": 0 }),
    )["paths"],
    json!(["f"])
  );

  let conflict = call(server, "slates.merge.submit", json!({ "work": work_b }));
  assert_eq!(conflict["accepted"], false, "the second submit conflicts");
  assert!(
    conflict["conflicts"]
      .as_array()
      .unwrap()
      .iter()
      .any(|w| w["path"] == "f"),
    "the conflict names the file: {conflict}"
  );

  assert_declare_namespace_ops(server, &green);
}

/// Namespace operations over MCP (`slates.merge.declare`): a work builds a directory tree with a
/// rename, a mode change, a symlink, a hard link and an xattr, then submits cleanly — the counterpart
/// to `edit`'s content splice. `edit` creates a file on write, so these have real targets.
fn assert_declare_namespace_ops(server: &mut McpServer, green: &str) {
  let work = call(
    server,
    "slates.merge.create_work",
    json!({ "green": green, "name": "ns" }),
  )["work"]
    .as_str()
    .unwrap()
    .to_owned();
  call(
    server,
    "slates.merge.edit",
    json!({ "work": work, "path": "/keep.txt", "at": 0, "text": "keep" }),
  );
  for op in [
    json!({ "kind": "mkdir", "path": "/d" }),
    json!({ "kind": "rename", "from": "/keep.txt", "to": "/d/g.txt" }),
    json!({ "kind": "set_mode", "path": "/d/g.txt", "mode": 384 }),
    json!({ "kind": "symlink", "path": "/d/link", "target": "g.txt" }),
    json!({ "kind": "link", "path": "/d/hard.txt", "target": "/d/g.txt" }),
    json!({ "kind": "set_xattr", "path": "/d/g.txt", "name": "user.slates", "value": "1" }),
  ] {
    assert_eq!(
      call(
        server,
        "slates.merge.declare",
        json!({ "work": work, "op": op.clone() }),
      ),
      json!({ "declared": true }),
      "declare {op}"
    );
  }
  let submitted = call(server, "slates.merge.submit", json!({ "work": work }));
  assert_eq!(
    submitted["accepted"], true,
    "the namespace ops submit cleanly: {submitted}"
  );
  assert_attach_advance_read(server, green);
}

/// Calls a tool expecting a JSON-RPC error, returning its message.
fn call_refused(server: &mut McpServer, name: &str, arguments: Value) -> String {
  let request = json!({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": { "name": name, "arguments": arguments },
  });
  let reply = server.handle(&request).expect("a call gets a reply");
  reply["error"]["message"]
    .as_str()
    .unwrap_or_else(|| panic!("tool {name} was not refused: {reply}"))
    .to_owned()
}

/// A green reader over MCP (§4.16 "Attachments and versions"; AC-6.13/T-6.15): `slates.attach.attach`
/// pins the head version; a further work lands a new version; `slates.fs.read` at the attachment still
/// serves the pinned view (a file born after the pin is refused typed) while a read at the head sees
/// the new file; `slates.merge.advance` re-pins and names the invalidated path; then the read serves it.
fn assert_attach_advance_read(server: &mut McpServer, green: &str) {
  let (attachment, head_after) = assert_attach_pins_then_a_version_lands(server, green);
  assert_reads_move_only_by_advance(server, green, attachment, head_after);
}

/// `slates.attach.attach` pins the green's head; a further work lands the next version. Returns the
/// attachment and the new head.
fn assert_attach_pins_then_a_version_lands(server: &mut McpServer, green: &str) -> (u64, u64) {
  let attached = call(server, "slates.attach.attach", json!({ "volume": green }));
  let attachment = attached["attachment"].as_u64().unwrap();
  let pinned = attached["version"].as_u64().unwrap();
  let head_before = call(server, "slates.merge.versions", json!({ "green": green }))["head"]
    .as_u64()
    .unwrap();
  assert_eq!(pinned, head_before, "the attachment pins the head");

  let work = call(
    server,
    "slates.merge.create_work",
    json!({ "green": green, "name": "later" }),
  )["work"]
    .as_str()
    .unwrap()
    .to_owned();
  call(
    server,
    "slates.merge.edit",
    json!({ "work": work, "path": "/later.txt", "at": 0, "text": "after the pin" }),
  );
  let submitted = call(server, "slates.merge.submit", json!({ "work": work }));
  assert_eq!(submitted["accepted"], true, "{submitted}");
  let head_after = submitted["version"].as_u64().unwrap();
  assert_eq!(head_after, pinned + 1);
  (attachment, head_after)
}

/// The attached read serves the pinned view (the later file refused typed) while the head serves it;
/// `slates.merge.advance` re-pins naming the invalidated path; then the attached read serves it too.
fn assert_reads_move_only_by_advance(
  server: &mut McpServer,
  green: &str,
  attachment: u64,
  head_after: u64,
) {
  let refused = call_refused(
    server,
    "slates.fs.read",
    json!({ "volume": green, "path": "/later.txt", "attachment": attachment }),
  );
  assert!(
    refused.contains("NotFound"),
    "the pinned view lacks the later file: {refused}"
  );
  let at_head = call(
    server,
    "slates.fs.read",
    json!({ "volume": green, "path": "/later.txt" }),
  );
  assert_eq!(at_head["text"], "after the pin");
  assert_eq!(at_head["len"], 13);

  let advanced = call(
    server,
    "slates.merge.advance",
    json!({ "attachment": attachment }),
  );
  assert_eq!(advanced["version"], head_after);
  assert_eq!(advanced["invalidated"], json!(["later.txt"]));
  let at_pin = call(
    server,
    "slates.fs.read",
    json!({ "volume": green, "path": "later.txt", "attachment": attachment }),
  );
  assert_eq!(at_pin["text"], "after the pin");
  assert_eq!(
    call(
      server,
      "slates.attach.detach",
      json!({ "attachment": attachment })
    ),
    json!({ "detached": true })
  );
}

/// The volume lifecycle over MCP: create, list, stat, snapshot, clone, resize, destroy, and status.
fn assert_volume_lifecycle(server: &mut McpServer) {
  let volume = call(
    server,
    "slates.volume.create",
    json!({ "name": "v", "bounded": 1 << 20 }),
  )["volume"]
    .as_str()
    .unwrap()
    .to_owned();

  let listed = call(server, "slates.volume.list", json!({}));
  assert!(
    listed["volumes"]
      .as_array()
      .unwrap()
      .iter()
      .any(|v| v["name"] == "v"),
    "the volume is listed: {listed}"
  );
  assert_eq!(
    call(server, "slates.volume.stat", json!({ "volume": volume }))["name"],
    "v"
  );

  let snapshot = call(
    server,
    "slates.volume.snapshot",
    json!({ "volume": volume }),
  )["snapshot"]
    .as_u64()
    .unwrap();
  let clone = call(
    server,
    "slates.volume.clone",
    json!({ "volume": volume, "snapshot": snapshot, "name": "v-clone" }),
  );
  assert_ne!(clone["volume"], Value::Null);

  assert_eq!(
    call(
      server,
      "slates.volume.resize",
      json!({ "volume": volume, "bounded": 2u64 << 20 }),
    ),
    json!({ "resized": true })
  );
  assert_eq!(
    call(server, "slates.volume.destroy", json!({ "volume": volume })),
    json!({ "destroyed": true })
  );

  let status = call(server, "slates.status", json!({}));
  assert_eq!(status["pid"], std::process::id());
  assert_status_exports_the_registries(&status);
}

/// `slates.status` — the same `daemon_json` the CLI's `status --json` prints — carries every shard's
/// block with the health signals and the telemetry drain (§4.14; §4.12 parity: the JSON form is the
/// text form's definition, not a subset). Expect: two shard blocks; a measured `catalog.volumes` as a
/// number with its absence meaning; the nine chokepoints in roster order; `shard.op` fresh on some
/// shard after the lifecycle verbs; `archive.chunk` typed absent (`unknown`), never reported, with no
/// producer on this host; and a span carrying the request, trace, span and cause identities distinctly.
fn assert_status_exports_the_registries(status: &Value) {
  let shards = status["shards"]
    .as_array()
    .expect("every shard's block, not a bare count");
  assert_eq!(shards.len(), 2);
  let signals = shards[0]["signals"].as_array().expect("the health signals");
  assert!(
    signals.iter().any(|s| s["name"] == "catalog.volumes"
      && s["value"].is_u64()
      && s["absence"] == "degraded"
      && s["freshness_ns"].is_u64()),
    "catalog.volumes is a measured number with its absence meaning: {signals:?}"
  );
  assert_chokepoint_registry(shards);
  assert_span_identities(shards);
  for shard in shards {
    let drain = &shard["telemetry"];
    assert!(drain["shed_before"].is_u64() && drain["dropped_total"].is_u64());
    assert!(drain["remaining"].is_u64() && drain["missing_links"].is_u64());
    assert!(drain["window_ns"].is_u64() && drain["horizon_ns"].is_u64());
  }
}

/// A shard block's telemetry registry entries.
fn chokepoints_of(shard: &Value) -> Vec<Value> {
  shard["telemetry"]["chokepoints"]
    .as_array()
    .expect("the telemetry drain's registry")
    .clone()
}

/// The nine chokepoints in roster order; `archive.chunk` typed absent, never reported, with no
/// producer here; `shard.op` fresh on some shard after the lifecycle verbs.
fn assert_chokepoint_registry(shards: &[Value]) {
  let names: Vec<String> = chokepoints_of(&shards[0])
    .iter()
    .map(|c| c["name"].as_str().unwrap().to_owned())
    .collect();
  assert_eq!(
    names,
    [
      "bridge.request",
      "ring.request",
      "shard.op",
      "log.append",
      "ship.record",
      "consensus.step",
      "archive.chunk",
      "land.entry",
      "merge.verdict",
    ],
    "the registry, in roster order"
  );
  let archive = chokepoints_of(&shards[0])
    .into_iter()
    .find(|c| c["name"] == "archive.chunk")
    .unwrap();
  assert_eq!(archive["fresh"], false, "nothing produces it here");
  assert!(
    archive["latest_age_ns"].is_null(),
    "never reported: no age, not a zero"
  );
  assert_eq!(archive["absence"], "unknown");
  assert_eq!(archive["expected"], false);
  assert!(
    shards.iter().any(|shard| chokepoints_of(shard)
      .iter()
      .any(|c| c["name"] == "shard.op" && c["fresh"] == true)),
    "the lifecycle verbs left a fresh shard.op on some shard: {shards:?}"
  );
}

/// A drained span carries the request, a 32-hex trace, the span id and a typed cause.
fn assert_span_identities(shards: &[Value]) {
  let span = shards
    .iter()
    .flat_map(|shard| shard["telemetry"]["spans"].as_array().unwrap().clone())
    .next()
    .expect("a drained span");
  assert!(span["request"]["client"].is_u64() && span["request"]["sequence"].is_u64());
  assert_eq!(
    span["trace"].as_str().unwrap().len(),
    32,
    "a 128-bit trace as hex"
  );
  assert!(span["span"].is_u64());
  assert!(
    ["root", "span", "missing"].contains(&span["cause"]["kind"].as_str().unwrap()),
    "a typed cause: {span}"
  );
}

/// Attach and the base operations over MCP: over an overlay of a real, readable directory (so the
/// base operations have a base), attach for reading and detach, then rewitness and pin the base.
fn assert_attach_base(server: &mut McpServer) {
  // An overlay over this crate's own directory (read only, R1): a small, always-present base, with a
  // bounded size so pin has budget to lock its entries.
  let volume = call(
    server,
    "slates.volume.create",
    json!({ "name": "ab", "base": env!("CARGO_MANIFEST_DIR"), "bounded": 16u64 << 20 }),
  )["volume"]
    .as_str()
    .unwrap()
    .to_owned();

  let attached = call(server, "slates.attach.attach", json!({ "volume": volume }));
  let attachment = attached["attachment"].as_u64().unwrap();
  assert_eq!(
    call(
      server,
      "slates.attach.detach",
      json!({ "attachment": attachment })
    ),
    json!({ "detached": true })
  );

  assert!(
    call(server, "slates.base.rewitness", json!({ "volume": volume }))["rewitnessed"].is_array()
  );
  // §4.15 the clean-file digest: the crate's own manifest is an untouched base file, so its digest
  // is exported — a 32-byte BLAKE3 as hex and the file's length — and twice identically.
  let digest = call(
    server,
    "slates.base.digest",
    json!({ "volume": volume, "path": "/Cargo.toml" }),
  );
  let identity = digest["identity"].as_str().unwrap();
  assert_eq!(identity.len(), 64, "a BLAKE3 as hex: {identity}");
  assert!(identity.bytes().all(|b| b.is_ascii_hexdigit()));
  assert!(digest["size"].as_u64().unwrap() > 0);
  assert_eq!(digest["path"], "/Cargo.toml");
  assert_eq!(
    call(
      server,
      "slates.base.digest",
      json!({ "volume": volume, "path": "/Cargo.toml" }),
    ),
    digest,
    "two exports of unchanged content are identical"
  );
  assert!(call(server, "slates.base.pin", json!({ "volume": volume }))["pinned"].is_number());
}

/// A landing planned over MCP: to a target that cannot be opened, the tool surfaces the daemon's
/// typed refusal as a JSON-RPC error (the full plan-grant-execute flow needs a real target and the
/// control channel, exercised in the server's Linux landing lane). No grant is ever created (R10).
fn assert_land(server: &mut McpServer) {
  let volume = call(
    server,
    "slates.volume.create",
    json!({ "name": "land-v", "bounded": 1 << 20 }),
  )["volume"]
    .as_str()
    .unwrap()
    .to_owned();
  let reply = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": 9, "method": "tools/call",
      "params": {
        "name": "slates.land.materialize",
        "arguments": { "volume": volume, "target": "/nonexistent/slates/mcp/target" },
      },
    }))
    .unwrap();
  assert_eq!(
    reply["error"]["code"], -32000,
    "an unopenable target is a typed refusal: {reply}"
  );
}

/// Malformed calls are typed JSON-RPC errors, not panics: an unknown tool, a bad volume id, and an
/// unknown method.
fn assert_malformed(server: &mut McpServer) {
  let unknown = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": 1, "method": "tools/call",
      "params": { "name": "slates.merge.nope", "arguments": {} },
    }))
    .unwrap();
  assert_eq!(
    unknown["error"]["code"], -32601,
    "unknown tool: method not found"
  );

  let bad_id = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": 2, "method": "tools/call",
      "params": { "name": "slates.merge.versions", "arguments": { "green": "not-a-volume-id" } },
    }))
    .unwrap();
  assert_eq!(
    bad_id["error"]["code"], -32602,
    "bad volume id: invalid params"
  );

  let unknown_method = server
    .handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "no/such" }))
    .unwrap();
  assert_eq!(unknown_method["error"]["code"], -32601);
}

/// The whole MCP surface over one daemon: the handshake, the merge loop, the volume lifecycle, and
/// typed errors. One serial daemon so the scenarios' spinning shards do not contend.
#[test]
fn the_mcp_surface_serves_the_tools() {
  let profile = profile();
  let instance = format!("mcp-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-mcp-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut server = McpServer::new(connect(&instance));

  assert_protocol(&mut server);
  assert_merge_loop(&mut server);
  assert_volume_lifecycle(&mut server);
  assert_attach_base(&mut server);
  assert_land(&mut server);
  assert_malformed(&mut server);
  assert_http_transport(&profile, &instance);

  daemon.stop();
}

/// Shape: the edge's bearer token in this test (the command mints one from the platform's secure random).
const TEST_BEARER: [u8; slates_mcp::http::BEARER_BYTES] = [0x5c; slates_mcp::http::BEARER_BYTES];

/// Starts the HTTP edge for `instance` on its own shard thread, as `slates mcp --http` runs it (the runtime
/// and connection bound derived for one shard of this machine, the client's reply deadline), and returns
/// the port it listens on. The thread serves until the process ends.
fn start_http_edge(profile: &MachineProfile, instance: &str) -> u16 {
  use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
  let config = DaemonConfig::derive(profile, instance, Some(1));
  let server = McpServer::new(connect(instance));
  let (port_tx, port_rx) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    let runtime = slates_rt::runtime::LocalRuntime::new(&config.runtime).unwrap();
    let backlog = i32::try_from(config.clients_per_shard).unwrap();
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), backlog).unwrap();
    let port = listener.local_addr().unwrap().port();
    let edge = slates_mcp::http::HttpEdge {
      port,
      bearer: TEST_BEARER,
      deadline_ns: Deadlines::derive(
        slates_server::daemon::LIVENESS_BUDGET_NS,
        slates_db::replay::RECOVERY_BUDGET_NS,
      )
      .get()
      .reply_ns,
      connections: config.clients_per_shard,
    };
    port_tx.send(port).unwrap();
    runtime
      .spawn(async move {
        let _ = slates_mcp::serve(server, listener, edge).await;
      })
      .unwrap();
    // Serves until the process ends: the edge's own loop, not an idle check (a waiting accept is not idle).
    runtime.context().run();
  });
  port_rx.recv().unwrap()
}

/// One HTTP exchange on a fresh connection: `head` (without the blank line) and `body`; the response.
fn exchange(port: u16, head: &str, body: &str) -> String {
  use std::io::{Read, Write};
  let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  stream
    .write_all(
      format!(
        "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
      )
      .as_bytes(),
    )
    .unwrap();
  let mut response = String::new();
  let _ = stream.read_to_string(&mut response);
  response
}

/// The head an MCP client sends this edge, with `host`, `origin` and the bearer `token` as given.
fn mcp_head(port: u16, host: &str, origin: Option<&str>, token: Option<&str>) -> String {
  let mut head = format!(
    "POST {} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json",
    slates_mcp::http::ENDPOINT_PATH
  );
  if let Some(origin) = origin {
    head.push_str(&format!("\r\nOrigin: {origin}"));
  }
  if let Some(token) = token {
    head.push_str(&format!("\r\nAuthorization: Bearer {token}"));
  }
  let _ = port;
  head
}

/// AUD-29-23 and AUD-29-24 (§4.12, §4.13; R6): the loopback HTTP edge over real sockets. Do: open hostile
/// connections and leave them open — an unterminated line past the bound, too many headers, a body that
/// never finishes, an idle kept-alive connection, and one reset mid-head — then, while they are open, send
/// a valid `initialize`; then send a volume-creating tool call from a foreign origin, under a rebound host
/// name, and with no token. Expect the oversized heads refused (`414`/`431`), the valid client answered
/// `200` while the slow and idle connections are still open, and each unauthorized call refused (`403`,
/// `421`, `401`) with no volume created.
fn assert_http_transport(profile: &MachineProfile, instance: &str) {
  use std::io::{Read, Write};
  let port = start_http_edge(profile, instance);
  let token: String = TEST_BEARER
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let ours = format!("127.0.0.1:{port}");

  let long_line = exchange(
    port,
    &format!(
      "POST /{} HTTP/1.1",
      "a".repeat(slates_mcp::http::MAX_LINE_BYTES)
    ),
    "",
  );
  assert!(long_line.starts_with("HTTP/1.1 414"), "{long_line}");
  let many: String = (0..=slates_mcp::http::MAX_FIELDS)
    .map(|at| format!("\r\nX-{at}: v"))
    .collect();
  let many_headers = exchange(
    port,
    &format!("{}{many}", mcp_head(port, &ours, None, Some(&token))),
    "{}",
  );
  assert!(many_headers.starts_with("HTTP/1.1 431"), "{many_headers}");

  // Held open while the valid client runs: a body that never finishes, an idle connection, a reset one.
  let mut slow = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  slow
    .write_all(
      format!(
        "{}\r\nContent-Length: 64\r\n\r\n{{",
        mcp_head(port, &ours, None, Some(&token))
      )
      .as_bytes(),
    )
    .unwrap();
  let idle = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let mut reset = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  reset.write_all(b"POST /mcp HTTP/1.1\r\nHo").unwrap();
  drop(reset);

  let valid = exchange(
    port,
    &mcp_head(port, &ours, Some(&format!("http://{ours}")), Some(&token)),
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
  );
  assert!(
    valid.starts_with("HTTP/1.1 200 OK"),
    "a valid client is served: {valid}"
  );
  assert!(
    valid.contains("\"protocolVersion\":\"2026-07-28\""),
    "{valid}"
  );

  assert_unauthorized_calls_have_no_effect(port, instance, &token);

  // The slow and idle connections were still open throughout; the valid client was not held behind them.
  let mut probe = [0u8; 1];
  slow.set_nonblocking(true).unwrap();
  assert!(
    matches!(slow.read(&mut probe), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
    "the slow connection is still open, unanswered"
  );
  drop(idle);
}

/// AUD-29-24: a volume-creating tool call from a foreign origin, under a rebound host name and without the
/// token is refused with its status, and no such volume exists afterwards.
fn assert_unauthorized_calls_have_no_effect(port: u16, instance: &str, token: &str) {
  let ours = format!("127.0.0.1:{port}");
  let create = |name: &str| {
    format!(
      r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"slates.volume.create","arguments":{{"name":"{name}","size":"1MiB"}}}}}}"#
    )
  };
  let refusals = [
    (
      "foreign-origin",
      mcp_head(port, &ours, Some("https://evil.example"), Some(token)),
      "403",
    ),
    (
      "rebound-host",
      mcp_head(port, &format!("rebound.example:{port}"), None, Some(token)),
      "421",
    ),
    ("unbound", mcp_head(port, &ours, None, None), "401"),
  ];
  for (name, head, status) in &refusals {
    let response = exchange(port, head, &create(name));
    assert!(
      response.starts_with(&format!("HTTP/1.1 {status}")),
      "{name}: {response}"
    );
  }
  let mut checker = McpServer::new(connect(instance));
  let listed = call(&mut checker, "slates.volume.list", json!({}));
  for (name, _, _) in &refusals {
    assert!(
      !listed.to_string().contains(name),
      "{name}: a refused call has no effect: {listed}"
    );
  }
}
