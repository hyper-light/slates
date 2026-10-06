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
  assert_eq!(
    reply["result"]["isError"], false,
    "tool {name} failed: {reply}"
  );
  reply["result"]["structuredContent"].clone()
}

/// The `_meta` a modern (2026-07-28) request carries: its protocol version and capabilities.
fn modern_meta(version: &str) -> Value {
  json!({
    "io.modelcontextprotocol/protocolVersion": version,
    "io.modelcontextprotocol/clientInfo": { "name": "slates-test", "version": "0" },
    "io.modelcontextprotocol/clientCapabilities": {},
  })
}

/// MCP 2026-07-28 (stateless, §4.12, D-19): do discover the server with a modern request; expect a complete result
/// naming 2026-07-28 among its supported versions, its tools capability, guidance for the model, a cache hint and
/// the server's identity in `_meta`. List the tools the same way; expect a complete, cacheable result. Ask in an
/// unknown version; expect `-32022` naming the supported versions and the one requested. A legacy client's
/// `initialize` (no `_meta`) is still answered (dual era).
fn assert_modern_protocol(server: &mut McpServer) {
  let discover = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "d", "method": "server/discover",
      "params": { "_meta": modern_meta("2026-07-28") },
    }))
    .unwrap();
  let result = &discover["result"];
  assert_eq!(result["resultType"], "complete", "{discover}");
  assert!(
    result["supportedVersions"]
      .as_array()
      .unwrap()
      .contains(&json!("2026-07-28"))
  );
  assert!(result["capabilities"]["tools"].is_object());
  assert!(
    result["instructions"]
      .as_str()
      .is_some_and(|text| !text.is_empty())
  );
  assert!(result["ttlMs"].is_u64());
  assert_eq!(result["cacheScope"], "public");
  assert_eq!(
    result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
    "slates"
  );
  assert_modern_list_and_versions(server);
}

/// The rest of [`assert_modern_protocol`]: a modern `tools/list`, an unsupported version, and a legacy client.
fn assert_modern_list_and_versions(server: &mut McpServer) {
  let list = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "l", "method": "tools/list",
      "params": { "_meta": modern_meta("2026-07-28") },
    }))
    .unwrap();
  assert_eq!(list["result"]["resultType"], "complete");
  assert!(list["result"]["ttlMs"].is_u64());
  assert_eq!(list["result"]["cacheScope"], "public");
  let unsupported = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "u", "method": "tools/list",
      "params": { "_meta": modern_meta("1900-01-01") },
    }))
    .unwrap();
  assert_eq!(unsupported["error"]["code"], -32022, "{unsupported}");
  assert_eq!(unsupported["error"]["data"]["requested"], "1900-01-01");
  assert!(
    unsupported["error"]["data"]["supported"]
      .as_array()
      .unwrap()
      .contains(&json!("2026-07-28"))
  );
  let legacy = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": 0, "method": "initialize",
      "params": { "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "old", "version": "0" } },
    }))
    .unwrap();
  assert_eq!(
    legacy["result"]["protocolVersion"], "2025-11-25",
    "a legacy client keeps its version"
  );
  // `ping` is a legacy-era method (removed in 2026-07-28): a legacy client's is answered, a modern request's is not.
  let ping = server
    .handle(&json!({ "jsonrpc": "2.0", "id": "p", "method": "ping" }))
    .unwrap();
  assert_eq!(ping["result"], json!({}), "{ping}");
  let modern_ping = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "mp", "method": "ping",
      "params": { "_meta": modern_meta("2026-07-28") },
    }))
    .unwrap();
  assert_eq!(modern_ping["error"]["code"], -32601, "{modern_ping}");
  assert_modern_meta_is_required(server);
}

/// MCP 2026-07-28 (SEP-2575): do send modern requests (`clientCapabilities` present) whose `_meta` lacks its protocol
/// version, and one with a version but no `clientCapabilities`; expect each refused `-32602`. Send `initialize` as a
/// modern request; expect `-32601` (the method is removed in the modern era).
fn assert_modern_meta_is_required(server: &mut McpServer) {
  let no_version = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "nv", "method": "tools/list",
      "params": { "_meta": { "io.modelcontextprotocol/clientCapabilities": {} } },
    }))
    .unwrap();
  assert_eq!(no_version["error"]["code"], -32602, "{no_version}");
  let no_capabilities = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "nc", "method": "tools/list",
      "params": { "_meta": { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } },
    }))
    .unwrap();
  assert_eq!(
    no_capabilities["error"]["code"], -32602,
    "{no_capabilities}"
  );
  let modern_initialize = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": "mi", "method": "initialize",
      "params": { "_meta": modern_meta("2026-07-28") },
    }))
    .unwrap();
  assert_eq!(
    modern_initialize["error"]["code"], -32601,
    "{modern_initialize}"
  );
}

/// The protocol handshake: initialize reports the version and identity, tools/list offers the merge
/// tools and no grant tool (R10), and a notification (no id) gets no reply.
fn assert_protocol(server: &mut McpServer) {
  let init = server
    .handle(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" }))
    .unwrap();
  assert_eq!(
    init["result"]["protocolVersion"], "2025-11-25",
    "a legacy initialize naming no version is answered in the newest legacy revision"
  );
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
  assert_eq!(
    reply["result"]["isError"], true,
    "a refusal is a tool-execution error the model sees: {reply}"
  );
  reply["result"]["structuredContent"]["error"]["message"]
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
  assert_residency_names_what_slates_protects(&call(
    server,
    "slates.volume.stat",
    json!({ "volume": volume }),
  ));

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

/// Shape: a file larger than one request (a request rides one 4 KiB bulk chunk), so its write is staged first.
const STAGED_FILE_BYTES: usize = 300 * 1024;

/// §4.12 `slates.fs` (condition 13): do, over MCP under a write attachment of a plain volume, make a directory, write a
/// file and read it back, rewrite it shorter, write a file past one request, move a file, and remove a file and the
/// emptied directory; expect each read to give exactly what was last written (a rewrite truncates, the large file is
/// whole), the moved file only at its new name, and the removed names gone. Then do the adversarial calls: a `..`
/// path, a write into a directory that does not exist, a write over a directory, a write under a read-only
/// attachment and under another volume's attachment, and a mode past the permission bits; expect each refused typed,
/// with nothing changed.
fn assert_files_change_through_mcp(server: &mut McpServer) {
  let create = |server: &mut McpServer, name: &str| {
    call(
      server,
      "slates.volume.create",
      json!({ "name": name, "bounded": 8u64 << 20 }),
    )["volume"]
      .as_str()
      .unwrap()
      .to_owned()
  };
  let volume = create(server, "files");
  let attach = |server: &mut McpServer, volume: &str, write: bool| {
    call(
      server,
      "slates.attach.attach",
      json!({ "volume": volume, "write": write }),
    )["attachment"]
      .as_u64()
      .unwrap()
  };
  let writer = attach(server, &volume, true);
  let at = |extra: Value| {
    let mut args = json!({ "volume": volume, "attachment": writer });
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
      args.extend(extra.clone());
    }
    args
  };
  let read = |server: &mut McpServer, path: &str| {
    call(
      server,
      "slates.fs.read",
      json!({ "volume": volume, "path": path }),
    )["text"]
      .as_str()
      .unwrap()
      .to_owned()
  };
  call(server, "slates.fs.mkdir", at(json!({ "path": "src" })));
  let program = "fn main() { println!(\"hi\"); }\n";
  let written = call(
    server,
    "slates.fs.write",
    at(json!({ "path": "src/main.rs", "text": program })),
  );
  assert_eq!(written["size"], program.len() as u64, "{written}");
  assert_eq!(read(server, "src/main.rs"), program);
  call(
    server,
    "slates.fs.write",
    at(json!({ "path": "src/main.rs", "text": "fn main() {}\n" })),
  );
  assert_eq!(
    read(server, "src/main.rs"),
    "fn main() {}\n",
    "a rewrite truncates"
  );
  let large: String = b"abcdefghijklmnopqrstuvwxyz"
    .iter()
    .cycle()
    .take(STAGED_FILE_BYTES)
    .map(|byte| char::from(*byte))
    .collect();
  let staged = call(
    server,
    "slates.fs.write",
    at(json!({ "path": "big.txt", "text": large })),
  );
  assert_eq!(staged["size"], STAGED_FILE_BYTES as u64);
  assert_eq!(read(server, "big.txt"), large, "the staged file is whole");
  call(
    server,
    "slates.fs.move",
    at(json!({ "from": "src/main.rs", "to": "src/lib.rs" })),
  );
  assert_eq!(read(server, "src/lib.rs"), "fn main() {}\n");
  assert!(
    call_refused(
      server,
      "slates.fs.read",
      json!({ "volume": volume, "path": "src/main.rs" })
    )
    .contains("NotFound")
  );
  call(
    server,
    "slates.fs.remove",
    at(json!({ "path": "src/lib.rs" })),
  );
  call(server, "slates.fs.remove", at(json!({ "path": "src" })));
  let listed = call(server, "slates.fs.list", json!({ "volume": volume }));
  let names: Vec<&str> = listed["entries"]
    .as_array()
    .unwrap()
    .iter()
    .filter_map(|entry| entry["name"].as_str())
    .collect();
  assert_eq!(names, ["big.txt"], "only the large file remains: {listed}");
  refused_file_calls_change_nothing(server, (&volume, writer));
}

/// The adversarial half of [`assert_files_change_through_mcp`]: each refused call typed, and the volume unchanged.
fn refused_file_calls_change_nothing(server: &mut McpServer, (volume, writer): (&str, u64)) {
  let at = |extra: Value| {
    let mut args = json!({ "volume": volume, "attachment": writer });
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
      args.extend(extra.clone());
    }
    args
  };
  let attach = |server: &mut McpServer, volume: &str, write: bool| {
    call(
      server,
      "slates.attach.attach",
      json!({ "volume": volume, "write": write }),
    )["attachment"]
      .as_u64()
      .unwrap()
  };
  assert!(
    !call_refused(
      server,
      "slates.fs.write",
      at(json!({ "path": "../escape", "text": "x" }))
    )
    .is_empty()
  );
  assert!(
    call_refused(
      server,
      "slates.fs.write",
      at(json!({ "path": "no/such/dir.txt", "text": "x" }))
    )
    .contains("NotFound")
  );
  call(server, "slates.fs.mkdir", at(json!({ "path": "d" })));
  assert!(
    !call_refused(
      server,
      "slates.fs.write",
      at(json!({ "path": "d", "text": "x" }))
    )
    .is_empty()
  );
  let reader = attach(server, volume, false);
  assert!(
    call_refused(
      server,
      "slates.fs.write",
      json!({ "volume": volume, "attachment": reader, "path": "r.txt", "text": "x" })
    )
    .contains("Forbidden"),
    "a read-only attachment writes nothing"
  );
  let other = call(
    server,
    "slates.volume.create",
    json!({ "name": "files-other", "bounded": 1u64 << 20 }),
  )["volume"]
    .as_str()
    .unwrap()
    .to_owned();
  let other_writer = attach(server, &other, true);
  // Refused `Forbidden` when the other volume's record is on this volume's partition, `NotFound` when it is not.
  let refusal = call_refused(
    server,
    "slates.fs.write",
    json!({ "volume": volume, "attachment": other_writer, "path": "o.txt", "text": "x" }),
  );
  assert!(
    refusal.contains("Forbidden") || refusal.contains("NotFound"),
    "another volume's attachment writes nothing here: {refusal}"
  );
  let bad_mode = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": 9, "method": "tools/call",
      "params": { "name": "slates.fs.mkdir", "arguments": at(json!({ "path": "m", "mode": 0o100755 })) },
    }))
    .unwrap();
  assert!(bad_mode.to_string().contains("mode"), "{bad_mode}");
  let listed = call(server, "slates.fs.list", json!({ "volume": volume }));
  let mut names: Vec<&str> = listed["entries"]
    .as_array()
    .unwrap()
    .iter()
    .filter_map(|entry| entry["name"].as_str())
    .collect();
  names.sort_unstable();
  assert_eq!(
    names,
    ["big.txt", "d"],
    "the refused calls changed nothing: {listed}"
  );
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
    reply["result"]["isError"], true,
    "an unopenable target is a tool-execution error: {reply}"
  );
  assert_eq!(
    reply["result"]["structuredContent"]["error"]["code"], -32000,
    "a typed refusal: {reply}"
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
    unknown["error"]["code"], -32602,
    "an unknown tool is a protocol error, invalid params (MCP server/tools)"
  );

  let bad_id = server
    .handle(&json!({
      "jsonrpc": "2.0", "id": 2, "method": "tools/call",
      "params": { "name": "slates.merge.versions", "arguments": { "green": "not-a-volume-id" } },
    }))
    .unwrap();
  assert_eq!(
    bad_id["result"]["isError"], true,
    "a bad argument is a tool-execution error the model can correct (SEP-1303): {bad_id}"
  );
  assert_eq!(
    bad_id["result"]["structuredContent"]["error"]["code"],
    -32602
  );

  let unknown_method = server
    .handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "no/such" }))
    .unwrap();
  assert_eq!(unknown_method["error"]["code"], -32601);
}

/// JSON-RPC 2.0 §6 batches (MCP 2025-03-26, a revision this server serves, requires them). Do: send a batch of a
/// request, an unknown method and a notification; an empty batch; a batch of notifications only. Expect: an array of
/// two replies, each under its own id (the request's result, the unknown method's `-32601`); one Invalid Request
/// (`-32600`) against a null id; no reply. Before 2026-10-06 any array was taken for a notification (it has no `id`)
/// and answered with nothing, so a client waiting on its batch waited for good (found by an adversarial stdio session).
fn assert_batches(server: &mut McpServer) {
  let replies = server
    .handle(&json!([
      { "jsonrpc": "2.0", "id": 101, "method": "tools/list" },
      { "jsonrpc": "2.0", "id": 102, "method": "no/such" },
      { "jsonrpc": "2.0", "method": "notifications/initialized" },
    ]))
    .expect("a batch with requests is answered");
  let replies = replies
    .as_array()
    .expect("a batch is answered with an array");
  assert_eq!(
    replies.len(),
    2,
    "one reply per request, none for the notification: {replies:?}"
  );
  let by_id = |id: u64| replies.iter().find(|reply| reply["id"] == id).cloned();
  assert!(
    by_id(101).is_some_and(|reply| reply["result"]["tools"].is_array()),
    "the request's own result: {replies:?}"
  );
  assert_eq!(
    by_id(102).map(|reply| reply["error"]["code"].clone()),
    Some(json!(-32601))
  );
  let nested = server
    .handle(&json!([[{ "jsonrpc": "2.0", "id": 103, "method": "tools/list" }]]))
    .expect("a nested batch is answered");
  assert_eq!(
    nested[0]["error"]["code"], -32600,
    "a batch inside a batch is refused, not served: {nested}"
  );
  let empty = server
    .handle(&json!([]))
    .expect("an empty batch is answered");
  assert_eq!(empty["error"]["code"], -32600, "{empty}");
  assert!(empty["id"].is_null(), "{empty}");
  assert!(
    server
      .handle(&json!([{ "jsonrpc": "2.0", "method": "notifications/initialized" }]))
      .is_none(),
    "a batch of notifications is answered with nothing"
  );
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
  assert_batches(&mut server);
  assert_modern_protocol(&mut server);
  assert_skills_over_mcp(&mut server);
  assert_a_large_file_reads_whole(&mut server);
  assert_a_page_stamp_moves_with_the_file(&instance);
  assert_directories_list_across_pages(&mut server);
  assert_codemode_answers_one_query(&mut server);
  assert_subscriptions(&instance);
  assert_merge_loop(&mut server);
  assert_volume_lifecycle(&mut server);
  assert_files_change_through_mcp(&mut server);
  assert_attach_base(&mut server);
  assert_land(&mut server);
  assert_malformed(&mut server);
  assert_http_transport(&profile, &instance);

  daemon.stop();
}

/// Shape: the skill a subscription test names (one the server publishes).
const SUBSCRIBED_SKILL: &str = "skill://slates/working-in-slates-volumes/SKILL.md";
/// Shape: the protocol revision the subscription test speaks (the one that defines `subscriptions/listen`).
const LISTEN_VERSION: &str = "2026-07-28";

/// A modern `subscriptions/listen` request with JSON-RPC id `id` and filter `notifications`.
fn listen(id: u64, notifications: Value) -> Value {
  json!({
    "jsonrpc": "2.0",
    "id": id,
    "method": "subscriptions/listen",
    "params": { "_meta": modern_meta(LISTEN_VERSION), "notifications": notifications },
  })
}

/// MCP 2026-07-28 subscriptions (condition 13). Do: on stdio's streaming server, listen for the tools list and two
/// resources (a published skill, an unpublished URI); call a tool; open a second subscription and cancel it with
/// `notifications/cancelled`; end the server's subscriptions; then listen on the HTTP edge's server, and open more
/// subscriptions than the server has notification sources. Expect: the acknowledgement as the first message,
/// carrying the listen's id as its subscription id and only what the server honours (the unpublished URI left
/// out); tools still answered while it is open; nothing for the cancelled one; the graceful close answering only the
/// open subscription with a complete result under its id; listen refused on the non-streaming transport; and the
/// subscription past the bound refused.
fn assert_subscriptions(instance: &str) {
  let mut server = McpServer::streaming(connect(instance));
  let ack = server
    .handle(&listen(
      7,
      json!({ "toolsListChanged": true, "resourceSubscriptions": [SUBSCRIBED_SKILL, "volume://none/x"] }),
    ))
    .expect("an acknowledgement");
  assert_eq!(
    ack["method"], "notifications/subscriptions/acknowledged",
    "{ack}"
  );
  assert!(
    ack.get("id").is_none(),
    "the acknowledgement is a notification, not the reply: {ack}"
  );
  assert_eq!(
    ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
    7
  );
  assert_eq!(
    ack["params"]["notifications"],
    json!({ "toolsListChanged": true, "resourceSubscriptions": [SUBSCRIBED_SKILL] })
  );
  call(&mut server, "slates.volume.list", json!({}));
  server.handle(&listen(8, json!({ "promptsListChanged": true })));
  let cancelled = server.handle(&json!({
    "jsonrpc": "2.0",
    "method": "notifications/cancelled",
    "params": { "requestId": 8, "reason": "done" },
  }));
  assert!(cancelled.is_none(), "a notification is not answered");
  let closed = server.close_subscriptions();
  assert_eq!(
    closed.len(),
    1,
    "only the open subscription ends: {closed:?}"
  );
  assert_eq!(closed[0]["id"], 7);
  assert_eq!(closed[0]["result"]["resultType"], "complete");
  assert_eq!(
    closed[0]["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
    7
  );
  assert_listen_bounds(instance, &mut server);
}

/// The second half of [`assert_subscriptions`]: listen refused on the non-streaming transport, and past the bound.
fn assert_listen_bounds(instance: &str, server: &mut McpServer) {
  let mut edge = McpServer::new(connect(instance));
  let refused = edge
    .handle(&listen(9, json!({ "toolsListChanged": true })))
    .unwrap();
  assert_eq!(refused["error"]["code"], -32601, "{refused}");
  let mut opened = 0;
  let mut refusal = None;
  for id in 10..100 {
    let answer = server
      .handle(&listen(id, json!({ "toolsListChanged": true })))
      .unwrap();
    if answer.get("error").is_some() {
      refusal = Some(answer);
      break;
    }
    opened += 1;
  }
  let refusal = refusal.expect("a bound refuses past the notification sources");
  assert_eq!(refusal["error"]["code"], -32602, "{refusal}");
  assert!(opened > 0, "subscriptions open up to the bound");
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

/// A POST declaring and sending a body one byte past [`slates_mcp::http::MAX_BODY`], whole, before reading (as an
/// ordinary client does); the read's error kind, if any, and the response read.
fn send_past_the_bound(
  port: u16,
  ours: &str,
  token: &str,
) -> (Result<usize, std::io::ErrorKind>, String) {
  use std::io::{Read, Write};
  let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
  let declared = slates_mcp::http::MAX_BODY.saturating_add(1);
  let _ = stream.write_all(
    format!(
      "{}\r\nContent-Length: {declared}\r\n\r\n",
      mcp_head(port, ours, None, Some(token))
    )
    .as_bytes(),
  );
  let chunk = vec![b'A'; 1 << 20];
  let mut sent: u64 = 0;
  while sent < declared {
    let take = usize::try_from((declared - sent).min(1 << 20)).unwrap();
    if stream.write_all(&chunk[..take]).is_err() {
      break;
    }
    sent += take as u64;
  }
  let mut response = String::new();
  let read = stream.read_to_string(&mut response);
  (read.map_err(|e| e.kind()), response)
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

  // A body past the bound, sent whole as an ordinary client sends it before reading: the client reads `413`. The edge
  // refuses on the head and closes; a client that keeps writing may see its send fail, and still reads the refusal
  // (2026-10-06: Python's `http.client` raised on the send and never read, which is the client's choice).
  let oversized = send_past_the_bound(port, &ours, &token);
  assert!(
    oversized.1.starts_with("HTTP/1.1 413"),
    "a client that sent a body past the bound reads 413, not a reset: {:?}",
    (oversized.0, &oversized.1[..oversized.1.len().min(80)])
  );

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
    valid.contains("\"protocolVersion\":\"2025-11-25\""),
    "a legacy initialize is answered in the newest legacy revision: {valid}"
  );

  assert_modern_http(port, &ours, &token);
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

/// AUD-29-77: every transport's residency says what slates protects — the daemon's own RAM, locked and kept out
/// of dumps — and what else the bytes reach beyond it, which slates does not protect: nothing for a record
/// form, the kernel's page cache for a host mount, the runtime's VM for a container, the guest's memory (the
/// VMM's, its page cache and the device's reply buffers within it) for a guest. A protected export is never reported as a protected workload.
fn assert_residency_names_what_slates_protects(stat: &Value) {
  for capability in stat["transports"]["capabilities"].as_array().unwrap() {
    let residency = &capability["residency"];
    assert_eq!(residency["protected"], "daemon_ram", "{capability}");
    let beyond: Vec<&str> = residency["beyond_protection"]
      .as_array()
      .unwrap()
      .iter()
      .map(|v| v.as_str().unwrap())
      .collect();
    let expected: &[&str] = match residency["kind"].as_str().unwrap() {
      "daemon_ram" => &[],
      "daemon_ram_and_kernel_cache" => &["host_kernel_cache"],
      "daemon_ram_kernel_cache_and_runtime_vm" => &["host_kernel_cache", "runtime_vm"],
      "daemon_ram_and_guest_page_cache" => &["guest_memory"],
      other => panic!("an unknown residency {other}"),
    };
    assert_eq!(beyond, expected, "{capability}");
  }
}

/// One modern request over the HTTP edge with the given MCP headers: the raw response.
fn modern_exchange(port: u16, host: &str, token: &str, headers: &str, body: &Value) -> String {
  let head = format!(
    "{}{headers}",
    mcp_head(port, host, Some(&format!("http://{host}")), Some(token))
  );
  exchange(port, &head, &body.to_string())
}

/// A modern request body: `method` with `params` and the 2026-07-28 `_meta` naming `version`.
fn modern_body(method: &str, mut params: Value, version: &str) -> Value {
  params["_meta"] = modern_meta(version);
  json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
}

/// MCP Streamable HTTP 2026-07-28 ("Protocol Version Header", "Standard Request Headers", "Server Validation"): do
/// send modern requests over the HTTP edge; expect one whose headers match its body served `200`; a mismatched
/// `Mcp-Method`, a missing `Mcp-Name` on `tools/call`, and a missing `MCP-Protocol-Version` each refused `400` with
/// `HeaderMismatch` (`-32020`); a Base64-sentinel `Mcp-Name` decoded and served; an unsupported version refused
/// `400` with `-32022`; and an unknown method answered `404` with `-32601`.
fn assert_modern_http(port: u16, host: &str, token: &str) {
  let v = "2026-07-28";
  let list = modern_body("tools/list", json!({}), v);
  let served = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list",
    &list,
  );
  assert!(served.starts_with("HTTP/1.1 200"), "{served}");
  assert!(served.contains("\"resultType\":\"complete\""), "{served}");
  let wrong_method = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call",
    &list,
  );
  assert!(wrong_method.starts_with("HTTP/1.1 400"), "{wrong_method}");
  assert!(wrong_method.contains("-32020"), "{wrong_method}");
  let no_version = modern_exchange(port, host, token, "\r\nMcp-Method: tools/list", &list);
  assert!(
    no_version.starts_with("HTTP/1.1 400") && no_version.contains("-32020"),
    "{no_version}"
  );
  let call = modern_body(
    "tools/call",
    json!({ "name": "slates.status", "arguments": {} }),
    v,
  );
  let unnamed = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call",
    &call,
  );
  assert!(
    unnamed.starts_with("HTTP/1.1 400") && unnamed.contains("-32020"),
    "{unnamed}"
  );
  let sentinel = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: =?base64?c2xhdGVzLnN0YXR1cw==?=",
    &call,
  );
  assert!(
    sentinel.starts_with("HTTP/1.1 200"),
    "a sentinel name is decoded: {sentinel}"
  );
  let old = modern_body("tools/list", json!({}), "1900-01-01");
  let unsupported = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 1900-01-01\r\nMcp-Method: tools/list",
    &old,
  );
  assert!(
    unsupported.starts_with("HTTP/1.1 400") && unsupported.contains("-32022"),
    "{unsupported}"
  );
  assert_modern_http_refusals(port, host, token);
}

/// The refusals of [`assert_modern_http`]: a header and body naming different versions (`-32020`), a modern request
/// missing its `_meta` (`-32602`, 400), `initialize` as a modern request (`-32601`, 404), and an unknown method (404).
fn assert_modern_http_refusals(port: u16, host: &str, token: &str) {
  let v = "2026-07-28";
  // A header and body naming different versions disagree before either is judged supported (SEP-2575: -32020).
  let mismatched = modern_body("tools/list", json!({}), "1900-01-01");
  let disagreed = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list",
    &mismatched,
  );
  assert!(
    disagreed.starts_with("HTTP/1.1 400") && disagreed.contains("-32020"),
    "{disagreed}"
  );
  let bare = json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list", "params": {} });
  let missing_meta = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/list",
    &bare,
  );
  assert!(
    missing_meta.starts_with("HTTP/1.1 400") && missing_meta.contains("-32602"),
    "{missing_meta}"
  );
  let initialize = json!({ "jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {} });
  let removed = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: initialize",
    &initialize,
  );
  assert!(
    removed.starts_with("HTTP/1.1 404") && removed.contains("-32601"),
    "{removed}"
  );
  let unknown = modern_body("no/such", json!({}), v);
  let missing = modern_exchange(
    port,
    host,
    token,
    "\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: no/such",
    &unknown,
  );
  assert!(
    missing.starts_with("HTTP/1.1 404") && missing.contains("-32601"),
    "{missing}"
  );
}

/// Sends `method` with `params` (a legacy-era request) and returns its `result`, failing on an error.
fn rpc(server: &mut McpServer, method: &str, params: Value) -> Value {
  let reply = server
    .handle(&json!({ "jsonrpc": "2.0", "id": 11, "method": method, "params": params }))
    .unwrap();
  assert!(reply.get("error").is_none(), "{method} errored: {reply}");
  reply["result"].clone()
}

/// Checks `document` as the open Agent Skills specification (agentskills.io) requires a `SKILL.md` to be, for the
/// skill named `name`: frontmatter whose `name` is that name (1–64 lowercase letters, digits and single inner
/// hyphens, no reserved word) and whose `description` is 1–1,024 characters, and a body under 500 lines.
fn assert_agent_skill(name: &str, document: &str) {
  let yaml = document
    .strip_prefix("---\n")
    .and_then(|rest| rest.split_once("\n---\n"))
    .map(|(yaml, _)| yaml)
    .unwrap_or_else(|| panic!("{name}: no frontmatter"));
  let field = |key: &str| {
    yaml
      .lines()
      .find_map(|line| line.strip_prefix(&format!("{key}:")))
      .map(str::trim)
      .unwrap_or_else(|| panic!("{name}: no {key}"))
  };
  assert_eq!(field("name"), name, "the frontmatter names its directory");
  assert!((1..=64).contains(&name.len()), "{name}: name length");
  assert!(
    name
      .bytes()
      .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
      && !name.starts_with('-')
      && !name.ends_with('-')
      && !name.contains("--"),
    "{name}: name characters"
  );
  assert!(
    !name.contains("anthropic") && !name.contains("claude"),
    "{name}: a reserved word"
  );
  let description = field("description");
  assert!(
    (1..=1024).contains(&description.chars().count()),
    "{name}: description length {}",
    description.chars().count()
  );
  assert!(document.lines().count() < 500, "{name}: under 500 lines");
}

/// Skills over MCP (§4.12, D-19): do list the resources; expect one `skill://slates/<name>/SKILL.md` per skill, each
/// `text/markdown` with its own description. Read each; expect a document valid under the Agent Skills
/// specification for that name, whose description the listing showed. Expect the URI template, one prompt per skill
/// returning the same document as a user message, `slates.help` returning it by name, and an unknown resource,
/// prompt and skill refused (`-32602`; the help tool's as a tool-execution error).
fn assert_skills_over_mcp(server: &mut McpServer) {
  let listed = rpc(server, "resources/list", json!({}));
  let resources = listed["resources"].as_array().unwrap().clone();
  assert!(resources.len() >= 3, "{listed}");
  let mut names = Vec::new();
  for resource in &resources {
    let uri = resource["uri"].as_str().unwrap();
    let name = uri
      .strip_prefix("skill://slates/")
      .and_then(|rest| rest.strip_suffix("/SKILL.md"))
      .unwrap_or_else(|| panic!("a skill URI: {uri}"));
    assert_eq!(resource["mimeType"], "text/markdown");
    let read = rpc(server, "resources/read", json!({ "uri": uri }));
    let document = read["contents"][0]["text"].as_str().unwrap();
    assert_agent_skill(name, document);
    assert!(
      document.contains(resource["description"].as_str().unwrap()),
      "{name}: the listing shows the skill's own description"
    );
    let prompt = rpc(server, "prompts/get", json!({ "name": name }));
    assert_eq!(prompt["messages"][0]["role"], "user");
    assert_eq!(prompt["messages"][0]["content"]["text"], document);
    let help = call(server, "slates.help", json!({ "skill": name }));
    assert_eq!(help["text"], document);
    names.push(name.to_owned());
  }
  assert_skill_listings_and_refusals(server, &names);
}

/// The rest of [`assert_skills_over_mcp`]: the prompt listing matches the skills `names`, the template is served,
/// and unknown resources, prompts and skills are refused.
fn assert_skill_listings_and_refusals(server: &mut McpServer, names: &[String]) {
  let prompts = rpc(server, "prompts/list", json!({}));
  let prompt_names: Vec<String> = prompts["prompts"]
    .as_array()
    .unwrap()
    .iter()
    .map(|prompt| prompt["name"].as_str().unwrap().to_owned())
    .collect();
  assert_eq!(
    prompt_names, names,
    "one prompt per skill, in the same order"
  );
  let templates = rpc(server, "resources/templates/list", json!({}));
  assert_eq!(
    templates["resourceTemplates"][0]["uriTemplate"],
    "skill://slates/{name}/SKILL.md"
  );
  for (method, params) in [
    (
      "resources/read",
      json!({ "uri": "skill://slates/nope/SKILL.md" }),
    ),
    ("resources/read", json!({ "uri": "file:///etc/passwd" })),
    ("prompts/get", json!({ "name": "nope" })),
  ] {
    let reply = server
      .handle(&json!({ "jsonrpc": "2.0", "id": 12, "method": method, "params": params }))
      .unwrap();
    assert_eq!(reply["error"]["code"], -32602, "{method} {params}: {reply}");
  }
  let unknown = call_refused(server, "slates.help", json!({ "skill": "nope" }));
  assert!(unknown.contains("nope"), "{unknown}");
}

/// Shape: a file larger than one reply's bulk chunk (4 KiB) several times over.
const LARGE_FILE_BYTES: usize = 16 * 1024;

/// §4.12 `slates.fs.read`: do write a [`LARGE_FILE_BYTES`] file into a work and read it back; expect every byte, in
/// order. A reply rides one 4 KiB bulk chunk, so a read must be paged rather than refused or lost.
fn assert_a_large_file_reads_whole(server: &mut McpServer) {
  let green = call(
    server,
    "slates.merge.create_green",
    json!({ "name": "large-g" }),
  )["green"]
    .as_str()
    .unwrap()
    .to_owned();
  let work = call(
    server,
    "slates.merge.create_work",
    json!({ "green": green, "name": "large-w" }),
  )["work"]
    .as_str()
    .unwrap()
    .to_owned();
  let text: String = (0..LARGE_FILE_BYTES)
    .map(|at| char::from(b'a' + u8::try_from(at % 26).unwrap()))
    .collect();
  call(
    server,
    "slates.merge.edit",
    json!({ "work": work, "path": "big.txt", "at": 0, "delete_len": 0, "text": text }),
  );
  let read = call(
    server,
    "slates.fs.read",
    json!({ "volume": work, "path": "big.txt" }),
  );
  assert_eq!(read["len"], LARGE_FILE_BYTES, "{}", read["len"]);
  assert_eq!(read["text"].as_str().unwrap(), text, "every byte, in order");
}

/// One `ReadRange` page of `path` in `volume` from `offset`: its bytes, total and stamp.
fn page(
  client: &mut Client,
  volume: slates_client::VolumeId,
  path: &str,
  offset: u64,
) -> (Vec<u8>, u64, u64) {
  use slates_ipc::protocol::{ReadAt, ReplyBody, RequestBody};
  match client
    .call(&RequestBody::ReadRange {
      volume,
      path: path.to_owned(),
      at: ReadAt::Head,
      offset,
      max: u64::MAX,
    })
    .unwrap()
  {
    ReplyBody::ReadPage {
      bytes,
      total,
      stamp,
    } => (bytes, total, stamp),
    other => panic!("not a page: {other:?}"),
  }
}

/// §4.12 (paged reads): do read a large work file's first page, then its second; expect the same stamp and pages
/// no larger than one reply chunk, together the file's start. Edit the file between two pages; expect the stamp to
/// move, so a reader never stitches pages of two states of the file together.
fn assert_a_page_stamp_moves_with_the_file(instance: &str) {
  let mut client = connect(instance);
  let green = client.create_green("stamp-g", false).unwrap();
  let (work, _) = client.create_work(green, "stamp-w").unwrap();
  let text = vec![b'q'; LARGE_FILE_BYTES];
  client.edit(work, "f.txt", 0, 0, &text).unwrap();
  let (first, total, stamp) = page(&mut client, work, "f.txt", 0);
  assert_eq!(total, u64::try_from(LARGE_FILE_BYTES).unwrap());
  assert!(
    !first.is_empty() && first.len() < LARGE_FILE_BYTES,
    "a page, not the file"
  );
  let (second, _, again) = page(
    &mut client,
    work,
    "f.txt",
    u64::try_from(first.len()).unwrap(),
  );
  assert_eq!(again, stamp, "an unchanged file keeps its stamp");
  assert!(!second.is_empty());
  client.edit(work, "f.txt", 0, 0, b"x").unwrap();
  let (_, _, moved) = page(
    &mut client,
    work,
    "f.txt",
    u64::try_from(first.len()).unwrap(),
  );
  assert_ne!(moved, stamp, "a change moves the stamp");
}

/// Shape: root entries enough that a listing spans several 4 KiB reply chunks.
const MANY_ENTRIES: usize = 300;

/// The entries `slates.fs.list` answers for `path` in `volume` (extra view arguments in `view`): name, kind, size.
fn listed(
  server: &mut McpServer,
  volume: &str,
  path: &str,
  view: Value,
) -> Vec<(String, String, u64)> {
  let mut arguments = json!({ "volume": volume, "path": path });
  if let (Some(arguments), Some(view)) = (arguments.as_object_mut(), view.as_object()) {
    arguments.extend(view.clone());
  }
  call(server, "slates.fs.list", arguments)["entries"]
    .as_array()
    .unwrap()
    .iter()
    .map(|entry| {
      (
        entry["name"].as_str().unwrap().to_owned(),
        entry["kind"].as_str().unwrap().to_owned(),
        entry["size"].as_u64().unwrap(),
      )
    })
    .collect()
}

/// §4.12 `slates.fs.list`: do give a work a nested file, a declared empty directory, and [`MANY_ENTRIES`] root files;
/// expect its root listed whole across several pages (every file with its size, the nested directory implied, the
/// empty one declared), and the nested directory listed alone. Submit; expect the green's root at its new version
/// to list the same. Expect a plain volume's empty root to list nothing, and an unknown directory to be refused.
fn assert_directories_list_across_pages(server: &mut McpServer) {
  let green = call(
    server,
    "slates.merge.create_green",
    json!({ "name": "list-g" }),
  )["green"]
    .as_str()
    .unwrap()
    .to_owned();
  let work = call(
    server,
    "slates.merge.create_work",
    json!({ "green": green, "name": "list-w" }),
  )["work"]
    .as_str()
    .unwrap()
    .to_owned();
  let edit = |server: &mut McpServer, path: &str, text: &str| {
    call(
      server,
      "slates.merge.edit",
      json!({ "work": work, "path": path, "at": 0, "delete_len": 0, "text": text }),
    );
  };
  edit(server, "src/main.rs", "fn main() {}\n");
  call(
    server,
    "slates.merge.declare",
    json!({ "work": work, "op": { "kind": "mkdir", "path": "empty" } }),
  );
  for at in 0..MANY_ENTRIES {
    edit(server, &format!("file-number-{at:04}.txt"), "x");
  }
  let root = listed(server, &work, "", json!({}));
  assert_eq!(root.len(), MANY_ENTRIES + 2, "every entry, across pages");
  assert!(
    root.contains(&("src".to_owned(), "dir".to_owned(), 0)),
    "an implied directory"
  );
  assert!(
    root.contains(&("empty".to_owned(), "dir".to_owned(), 0)),
    "a declared directory"
  );
  assert!(root.contains(&("file-number-0299.txt".to_owned(), "file".to_owned(), 1)));
  assert_eq!(
    listed(server, &work, "src", json!({})),
    vec![("main.rs".to_owned(), "file".to_owned(), 13)]
  );
  let version = call(server, "slates.merge.submit", json!({ "work": work }))["version"]
    .as_u64()
    .unwrap();
  let green_root = listed(server, &green, "/", json!({ "version": version }));
  let files = |entries: &[(String, String, u64)]| {
    entries
      .iter()
      .filter(|(_, kind, _)| kind == "file")
      .cloned()
      .collect::<Vec<_>>()
  };
  assert_eq!(
    files(&green_root),
    files(&root),
    "the green lists what was submitted"
  );
  let plain = call(
    server,
    "slates.volume.create",
    json!({ "name": "list-plain", "bounded": 1 << 20 }),
  )["volume"]
    .as_str()
    .unwrap()
    .to_owned();
  assert!(
    listed(server, &plain, "", json!({})).is_empty(),
    "an empty plain root"
  );
  let missing = call_refused(
    server,
    "slates.fs.list",
    json!({ "volume": work, "path": "no/such" }),
  );
  assert!(!missing.is_empty());
}

/// §4.12 codemode (`slates.query`): do query the work `assert_directories_list_across_pages` built, by name: the
/// files whose names start `file-number-029`, ordered; expect exactly those ten paths and the walk's work reported.
/// Search its lines for `main`; expect the one line, with its number. Expect a query naming an unknown column refused
/// as a tool-execution error.
fn assert_codemode_answers_one_query(server: &mut McpServer) {
  let answer = call(
    server,
    "slates.query",
    json!({ "text": r#"FROM files("list-w") WHERE name STARTS WITH "file-number-029" SELECT path ORDER BY path"# }),
  );
  let paths: Vec<&str> = answer["rows"]
    .as_array()
    .unwrap()
    .iter()
    .map(|row| row[0].as_str().unwrap())
    .collect();
  let expected: Vec<String> = (290..300)
    .map(|at| format!("file-number-{at:04}.txt"))
    .collect();
  assert_eq!(paths, expected);
  assert!(answer["visited"].as_u64().unwrap() >= 302, "{answer}");
  let lines = call(
    server,
    "slates.query",
    json!({ "text": r#"FROM lines("list-w", under = "src") WHERE text CONTAINS "main" SELECT path, line"# }),
  );
  assert_eq!(lines["rows"], json!([["src/main.rs", 1]]));
  let refused = call_refused(
    server,
    "slates.query",
    json!({ "text": r#"FROM files("list-w") SELECT nope"# }),
  );
  assert!(refused.contains("nope"), "{refused}");
}
