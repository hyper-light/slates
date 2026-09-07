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
}

fn connect(instance: &str) -> Client {
  let deadlines = Deadlines {
    reply_ns: 5_000_000_000,
    reconnect_ns: 5_000_000_000,
  };
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
  assert_eq!(status["shards"], 2);
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
  let config = DaemonConfig::derive(&profile, &instance).with_shards(2);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: "slates-seg-mcp".to_owned(),
    },
  )
  .unwrap();
  let mut server = McpServer::new(connect(&instance));

  assert_protocol(&mut server);
  assert_merge_loop(&mut server);
  assert_volume_lifecycle(&mut server);
  assert_malformed(&mut server);

  daemon.stop();
}
