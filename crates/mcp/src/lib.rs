//! The MCP server (§4.12, Phase 6 task 8): the tools a coding agent calls over the Model Context
//! Protocol, mapped onto the Rust client SDK. This crate is the protocol and the dispatch — a pure
//! function from one JSON-RPC message to its reply — so it is driven directly in tests against a live
//! daemon; the `slates mcp` command wraps it in the stdio transport (the I/O boundary the CLI owns).
//!
//! The surface here is the merge tools (§4.16): create a green, clone it into a work, declare edits,
//! submit, rebase, and read the version chain — the whole loop an agent needs to merge into a shared
//! volume. It grows toward the full §4.12 tool set (volume, attach, fs, base, status) in later slices.
//!
//! **No grant, ever (R10).** No tool here creates a landing grant; the grant is a human-only act on
//! the CLI or a confirmation surface. The server has no grant verb to refuse — it simply does not
//! offer one, and any `slates.land` tool added later returns `GrantRequired`, never a grant.
//!
//! The wire is JSON-RPC 2.0. A request with an `id` gets a reply; a notification (no `id`) gets none.
//! A tool call returns `structuredContent` (the machine-readable result) alongside a text `content`
//! block (the same, rendered), the shape MCP clients expect. Volume ids cross the wire as lowercase
//! hex, opaque to the agent and echoed back on every result.

use serde_json::{Value, json};
use slates_client::{
  Client, ClientError, CreateSpec, DaemonReport, NamePolicy, Rebased, SizeClass, SnapshotId,
  StatusReport, Submitted, VolumeId, VolumeSummary,
};

/// The MCP protocol version this server speaks (the dated revision it targets, §4.12).
const PROTOCOL_VERSION: &str = "2026-07-28";
/// The server's name, reported in `initialize`.
const SERVER_NAME: &str = "slates";

/// JSON-RPC error codes (the standard set plus two in the implementation-defined server range for a
/// typed refusal from the daemon and an unreachable daemon). These are the JSON-RPC 2.0 wire values,
/// not tunables.
mod code {
  /// Format: JSON-RPC 2.0 "method not found".
  pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
  /// Format: JSON-RPC 2.0 "invalid params".
  pub(crate) const INVALID_PARAMS: i64 = -32602;
  /// Format: JSON-RPC 2.0 server-error range — the daemon refused (its typed message carried through).
  pub(crate) const REFUSED: i64 = -32000;
  /// Format: JSON-RPC 2.0 server-error range — the daemon was unreachable.
  pub(crate) const UNAVAILABLE: i64 = -32001;
  /// Format: JSON-RPC 2.0 "parse error" (the bytes were not valid JSON).
  pub(crate) const PARSE_ERROR: i64 = -32700;
}

/// Format: the radix of a volume id's hex text.
const HEX_RADIX: u32 = 16;

/// A JSON-RPC "parse error" reply (against a null id), for the transport to send when a line of input
/// is not valid JSON. The protocol crate owns the code so the transport carries no wire constant.
pub fn parse_error_reply() -> Value {
  error(&Value::Null, code::PARSE_ERROR, "parse error")
}

/// An MCP server bound to one daemon connection. The connection is the agent's session; the MCP
/// protocol above it is stateless per request (§4.12), so each `handle` is independent.
pub struct McpServer {
  client: Client,
}

impl McpServer {
  /// Wraps a connected client.
  pub fn new(client: Client) -> McpServer {
    McpServer { client }
  }

  /// Handles one JSON-RPC message, returning its reply — or `None` for a notification (a message
  /// with no `id`, such as `notifications/initialized`), which JSON-RPC answers with nothing.
  pub fn handle(&mut self, request: &Value) -> Option<Value> {
    // A notification carries no `id` field and expects no reply; `?` returns nothing for it.
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
      "initialize" => Ok(initialize_result()),
      "tools/list" => Ok(json!({ "tools": tool_list() })),
      "tools/call" => self.call_tool(&params),
      other => Err(McpError {
        code: code::METHOD_NOT_FOUND,
        message: format!("unknown method: {other}"),
      }),
    };
    Some(match result {
      Ok(value) => reply(&id, value),
      Err(e) => error(&id, e.code, &e.message),
    })
  }

  /// Dispatches a `tools/call`: the tool name selects the merge operation, its `arguments` object
  /// carries the parameters. The result is a `tools/call` result envelope (text plus structured).
  fn call_tool(&mut self, params: &Value) -> Result<Value, McpError> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let structured = match name {
      "slates.help" => Ok(json!({ "text": HELP })),
      "slates.merge.create_green" => self.create_green(&args),
      "slates.merge.create_work" => self.create_work(&args),
      "slates.merge.edit" => self.edit(&args),
      "slates.merge.submit" => self.submit(&args),
      "slates.merge.rebase" => self.rebase(&args),
      "slates.merge.versions" => self.versions(&args),
      "slates.merge.changed_since" => self.changed_since(&args),
      "slates.volume.create" => self.create_volume(&args),
      "slates.volume.list" => self.list_volumes(),
      "slates.volume.stat" => self.stat_volume(&args),
      "slates.volume.snapshot" => self.snapshot_volume(&args),
      "slates.volume.clone" => self.clone_volume(&args),
      "slates.volume.resize" => self.resize_volume(&args),
      "slates.volume.destroy" => self.destroy_volume(&args),
      "slates.status" => self.status(),
      other => {
        return Err(McpError {
          code: code::METHOD_NOT_FOUND,
          message: format!("unknown tool: {other}"),
        });
      }
    }?;
    Ok(tool_result(&structured))
  }

  fn create_green(&mut self, args: &Value) -> Result<Value, McpError> {
    let name = string_arg(args, "name")?;
    let require_evidence = args
      .get("require_evidence")
      .and_then(Value::as_bool)
      .unwrap_or(false);
    let green = self
      .client
      .create_green(&name, require_evidence)
      .map_err(refusal)?;
    Ok(json!({ "green": id_hex(green) }))
  }

  fn create_work(&mut self, args: &Value) -> Result<Value, McpError> {
    let green = volume_arg(args, "green")?;
    let name = string_arg(args, "name")?;
    let (work, base) = self.client.create_work(green, &name).map_err(refusal)?;
    Ok(json!({ "work": id_hex(work), "base": base }))
  }

  fn edit(&mut self, args: &Value) -> Result<Value, McpError> {
    let work = volume_arg(args, "work")?;
    let path = string_arg(args, "path")?;
    let at = u64_arg(args, "at")?;
    let delete_len = args.get("delete_len").and_then(Value::as_u64).unwrap_or(0);
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    self
      .client
      .edit(work, &path, at, delete_len, text.as_bytes())
      .map_err(refusal)?;
    Ok(json!({ "edited": true }))
  }

  fn submit(&mut self, args: &Value) -> Result<Value, McpError> {
    let work = volume_arg(args, "work")?;
    Ok(match self.client.submit(work).map_err(refusal)? {
      Submitted::Accepted(version) => json!({ "accepted": true, "version": version }),
      Submitted::Conflict(windows) => {
        json!({ "accepted": false, "conflicts": windows_json(&windows) })
      }
    })
  }

  fn rebase(&mut self, args: &Value) -> Result<Value, McpError> {
    let work = volume_arg(args, "work")?;
    Ok(match self.client.rebase(work).map_err(refusal)? {
      Rebased::Rebased(version) => json!({ "rebased": true, "version": version }),
      Rebased::Conflict(windows) => {
        json!({ "rebased": false, "conflicts": windows_json(&windows) })
      }
    })
  }

  fn versions(&mut self, args: &Value) -> Result<Value, McpError> {
    let green = volume_arg(args, "green")?;
    let head = self.client.versions(green).map_err(refusal)?;
    Ok(json!({ "head": head }))
  }

  fn changed_since(&mut self, args: &Value) -> Result<Value, McpError> {
    let green = volume_arg(args, "green")?;
    let version = u64_arg(args, "version")?;
    let paths = self.client.changed_since(green, version).map_err(refusal)?;
    Ok(json!({ "paths": paths }))
  }

  fn create_volume(&mut self, args: &Value) -> Result<Value, McpError> {
    let spec = CreateSpec {
      name: string_arg(args, "name")?,
      size: size_arg(args),
      names: if args.get("fold").and_then(Value::as_bool).unwrap_or(false) {
        NamePolicy::Fold
      } else {
        NamePolicy::Exact
      },
      require_locked: false,
      base: args.get("base").and_then(Value::as_str).map(str::to_owned),
    };
    let volume = self.client.create(&spec).map_err(refusal)?;
    Ok(json!({ "volume": id_hex(volume) }))
  }

  fn list_volumes(&mut self) -> Result<Value, McpError> {
    let volumes = self.client.list().map_err(refusal)?;
    Ok(json!({ "volumes": volumes.iter().map(summary_json).collect::<Vec<_>>() }))
  }

  fn stat_volume(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let report = self.client.status(volume).map_err(refusal)?;
    Ok(status_json(&report))
  }

  fn snapshot_volume(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let snapshot = self.client.snapshot(volume).map_err(refusal)?;
    Ok(json!({ "snapshot": snapshot.value }))
  }

  fn clone_volume(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let snapshot = SnapshotId {
      value: u64_arg(args, "snapshot")?,
    };
    let name = string_arg(args, "name")?;
    let clone = self
      .client
      .clone_snapshot(volume, snapshot, &name)
      .map_err(refusal)?;
    Ok(json!({ "volume": id_hex(clone) }))
  }

  fn resize_volume(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    self
      .client
      .resize(volume, size_arg(args))
      .map_err(refusal)?;
    Ok(json!({ "resized": true }))
  }

  fn destroy_volume(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    self.client.destroy(volume).map_err(refusal)?;
    Ok(json!({ "destroyed": true }))
  }

  fn status(&mut self) -> Result<Value, McpError> {
    let report = self.client.daemon_status().map_err(refusal)?;
    Ok(daemon_json(&report))
  }
}

/// A refusal from the dispatch, rendered as a JSON-RPC error.
struct McpError {
  code: i64,
  message: String,
}

/// Maps a client error to a JSON-RPC error: a typed daemon refusal keeps its message; an unreachable
/// daemon is its own code so an agent can distinguish "refused" from "gone".
fn refusal(e: ClientError) -> McpError {
  match e {
    ClientError::Refused(refusal) => McpError {
      code: code::REFUSED,
      message: format!("{refusal:?}"),
    },
    ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. })
    | ClientError::DaemonGone { .. } => McpError {
      code: code::UNAVAILABLE,
      message: "the slates daemon is unavailable".to_owned(),
    },
    other => McpError {
      code: code::REFUSED,
      message: other.to_string(),
    },
  }
}

/// The `initialize` result: the protocol version, the server's identity, and its capabilities (tools).
fn initialize_result() -> Value {
  json!({
    "protocolVersion": PROTOCOL_VERSION,
    "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
    "capabilities": { "tools": {} },
  })
}

/// One tool descriptor for `tools/list`: its name, one-line description, and the JSON schema of its
/// arguments (so a client validates a call before making it).
fn tool(name: &str, description: &str, properties: Value, required: Value) -> Value {
  json!({
    "name": name,
    "description": description,
    "inputSchema": {
      "type": "object",
      "properties": properties,
      "required": required,
    },
  })
}

/// The tools this server offers (§4.16 merge surface). No grant tool exists (R10).
fn tool_list() -> Vec<Value> {
  let string = json!({ "type": "string" });
  let integer = json!({ "type": "integer", "minimum": 0 });
  vec![
    tool(
      "slates.help",
      "The slates MCP tools and how to use them.",
      json!({}),
      json!([]),
    ),
    tool(
      "slates.merge.create_green",
      "Create a green volume (the shared target agents merge into).",
      json!({ "name": string, "require_evidence": { "type": "boolean" } }),
      json!(["name"]),
    ),
    tool(
      "slates.merge.create_work",
      "Clone a green into a work volume to edit, based on the green's current head.",
      json!({ "green": string, "name": string }),
      json!(["green", "name"]),
    ),
    tool(
      "slates.merge.edit",
      "Declare a content splice on a work: at `at`, remove `delete_len` bytes, insert `text`.",
      json!({ "work": string, "path": string, "at": integer, "delete_len": integer, "text": string }),
      json!(["work", "path", "at"]),
    ),
    tool(
      "slates.merge.submit",
      "Submit a work's increment to its green: accepted at a new version, or the conflict windows.",
      json!({ "work": string }),
      json!(["work"]),
    ),
    tool(
      "slates.merge.rebase",
      "Rebase a work onto its green's head (the corrective path); the green is unchanged.",
      json!({ "work": string }),
      json!(["work"]),
    ),
    tool(
      "slates.merge.versions",
      "The green's current head version.",
      json!({ "green": string }),
      json!(["green"]),
    ),
    tool(
      "slates.merge.changed_since",
      "The files that changed on the green strictly after `version`.",
      json!({ "green": string, "version": integer }),
      json!(["green", "version"]),
    ),
    tool(
      "slates.volume.create",
      "Create a volume: `bounded` (a byte limit) or `dynamic` (a max), optional `base` host path.",
      json!({ "name": string, "bounded": integer, "dynamic": integer, "fold": { "type": "boolean" }, "base": string }),
      json!(["name"]),
    ),
    tool(
      "slates.volume.list",
      "Every volume, with its referenced and unique bytes.",
      json!({}),
      json!([]),
    ),
    tool(
      "slates.volume.stat",
      "A volume's status: bytes, lease, attachments, head, snapshots, drift, placement.",
      json!({ "volume": string }),
      json!(["volume"]),
    ),
    tool(
      "slates.volume.snapshot",
      "Take a snapshot of a volume; the snapshot id.",
      json!({ "volume": string }),
      json!(["volume"]),
    ),
    tool(
      "slates.volume.clone",
      "Clone a volume's snapshot into a new volume.",
      json!({ "volume": string, "snapshot": integer, "name": string }),
      json!(["volume", "snapshot", "name"]),
    ),
    tool(
      "slates.volume.resize",
      "Resize a volume: `bounded` (a byte limit) or `dynamic` (a max).",
      json!({ "volume": string, "bounded": integer, "dynamic": integer }),
      json!(["volume"]),
    ),
    tool(
      "slates.volume.destroy",
      "Destroy a volume (its clones survive).",
      json!({ "volume": string }),
      json!(["volume"]),
    ),
    tool(
      "slates.status",
      "The daemon's status: generation, restarts, shard count.",
      json!({}),
      json!([]),
    ),
  ]
}

/// The `slates.help` text.
const HELP: &str = "slates merge over MCP: create_green -> create_work -> edit -> submit. \
On a conflict, read the green's bytes for each window, rewrite your edit, then rebase and submit \
again. versions and changed_since read the chain. No tool creates a landing grant (a grant is a \
human-only act on the CLI).";

/// A JSON-RPC success reply.
fn reply(id: &Value, result: Value) -> Value {
  json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error reply.
fn error(id: &Value, code: i64, message: &str) -> Value {
  json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A `tools/call` result: the structured result, plus a text block carrying the same JSON (the shape
/// MCP clients render). `isError` stays false — a tool that reached the daemon and got an answer
/// succeeded even when that answer is a conflict; a failure is a JSON-RPC error, not a tool result.
fn tool_result(structured: &Value) -> Value {
  json!({
    "content": [ { "type": "text", "text": structured.to_string() } ],
    "structuredContent": structured,
    "isError": false,
  })
}

/// The conflict windows as JSON: the file, the byte range, and the conflict class.
fn windows_json(windows: &[slates_ipc::protocol::MergeWindow]) -> Value {
  Value::Array(
    windows
      .iter()
      .map(|w| json!({ "path": w.path, "at": w.at, "len": w.len, "class": w.class }))
      .collect(),
  )
}

/// The size class from a `bounded` byte limit or a `dynamic` max, defaulting to an unbounded dynamic
/// volume when neither is given.
fn size_arg(args: &Value) -> SizeClass {
  if let Some(limit) = args.get("bounded").and_then(Value::as_u64) {
    SizeClass::Bounded { limit }
  } else if let Some(max) = args.get("dynamic").and_then(Value::as_u64) {
    SizeClass::Dynamic { max }
  } else {
    SizeClass::Dynamic { max: 0 }
  }
}

/// A volume summary as JSON.
fn summary_json(v: &VolumeSummary) -> Value {
  json!({
    "id": id_hex(v.id),
    "name": v.name,
    "referenced_bytes": v.referenced_bytes,
    "unique_bytes": v.unique_bytes,
    "overlay": v.overlay,
  })
}

/// A volume status report as JSON.
fn status_json(r: &StatusReport) -> Value {
  json!({
    "id": id_hex(r.id),
    "name": r.name,
    "referenced_bytes": r.referenced_bytes,
    "unique_bytes": r.unique_bytes,
    "lease_epoch": r.lease_epoch,
    "attachments": r.attachments,
    "head": r.head.value,
    "snapshots": r.snapshots,
    "watcher": r.watcher,
    "drifted": r.drifted,
    "placed": {
      "region": r.placed.region,
      "host_epoch": r.placed.host_epoch,
      "mirror_age_ns": r.placed.mirror_age_ns,
    },
  })
}

/// The daemon's status as JSON (the top-level counters and the shard count).
fn daemon_json(r: &DaemonReport) -> Value {
  json!({
    "pid": r.pid,
    "generation": r.generation,
    "restarts": r.restarts,
    "heartbeat_age_ns": r.heartbeat_age_ns,
    "clients_reaped": r.clients_reaped,
    "clients_refused": r.clients_refused,
    "shards": r.shards.len(),
  })
}

/// A required string argument, or an invalid-params error.
fn string_arg(args: &Value, key: &str) -> Result<String, McpError> {
  args
    .get(key)
    .and_then(Value::as_str)
    .map(str::to_owned)
    .ok_or_else(|| McpError {
      code: code::INVALID_PARAMS,
      message: format!("missing string argument: {key}"),
    })
}

/// A required non-negative integer argument, or an invalid-params error.
fn u64_arg(args: &Value, key: &str) -> Result<u64, McpError> {
  args
    .get(key)
    .and_then(Value::as_u64)
    .ok_or_else(|| McpError {
      code: code::INVALID_PARAMS,
      message: format!("missing integer argument: {key}"),
    })
}

/// A required volume-id argument (32 lowercase hex characters), or an invalid-params error.
fn volume_arg(args: &Value, key: &str) -> Result<VolumeId, McpError> {
  let text = string_arg(args, key)?;
  id_from_hex(&text).ok_or_else(|| McpError {
    code: code::INVALID_PARAMS,
    message: format!("argument {key} is not a volume id"),
  })
}

/// A volume id as 32 lowercase hex characters.
fn id_hex(id: VolumeId) -> String {
  id.bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a volume id from 32 hex characters, or `None` when it is malformed.
fn id_from_hex(text: &str) -> Option<VolumeId> {
  let mut bytes = [0u8; 16];
  if text.len() != bytes.len() * 2 {
    return None;
  }
  for (i, byte) in bytes.iter_mut().enumerate() {
    let pair = text.get(i * 2..i * 2 + 2)?;
    *byte = u8::from_str_radix(pair, HEX_RADIX).ok()?;
  }
  Some(VolumeId { bytes })
}
