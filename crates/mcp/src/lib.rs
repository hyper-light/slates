//! The MCP server (§4.12, Phase 6 task 8): the tools a coding agent calls over the Model Context
//! Protocol, mapped onto the Rust client SDK. This crate is the protocol and the dispatch — a pure
//! function from one JSON-RPC message to its reply — so it is driven directly in tests against a live
//! daemon; the `slates mcp` command wraps it in the stdio transport (the I/O boundary the CLI owns).
//!
//! The surface covers the merge loop (§4.16: create a green, clone a work, edit content, declare
//! namespace operations, submit, rebase, read the chain), the volume lifecycle (create, list, stat,
//! snapshot, clone, resize, destroy), attach and
//! detach, the base operations (read_base, rewitness, pin), `slates.status`, and `slates.land`
//! (materialize, which returns `GrantRequired`). Owed toward the full §4.12: `slates.fs` (the mount
//! path), Streamable HTTP, resources and prompts.
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
  Attachment, Client, ClientError, CreateSpec, DaemonReport, Filter, Intent, Landing,
  LandingOutcome, LandingSummary, NamePolicy, Rebased, SizeClass, SnapshotId, StatusReport,
  Submitted, VolumeId, VolumeSummary, WorkOp,
};

pub mod http;

pub use http::serve;

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
      "slates.merge.declare" => self.declare(&args),
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
      "slates.attach.attach" => self.attach(&args),
      "slates.attach.detach" => self.detach(&args),
      "slates.base.read_base" => self.read_base(&args),
      "slates.base.rewitness" => self.rewitness(&args),
      "slates.base.pin" => self.pin(&args),
      "slates.land.materialize" => self.land_materialize(&args),
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

  /// Declares a namespace operation on a work volume (§4.16) — the counterpart to [`Self::edit`]'s
  /// content splice. `op.kind` selects the operation; `work_op_from` reads its fields. A mounted work
  /// journals the same operations from its filesystem calls.
  fn declare(&mut self, args: &Value) -> Result<Value, McpError> {
    let work = volume_arg(args, "work")?;
    let op = args.get("op").ok_or_else(|| McpError {
      code: code::INVALID_PARAMS,
      message: "missing object argument: op".to_owned(),
    })?;
    self
      .client
      .declare(work, work_op_from(op)?)
      .map_err(refusal)?;
    Ok(json!({ "declared": true }))
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

  fn attach(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let snapshot = args
      .get("snapshot")
      .and_then(Value::as_u64)
      .map(|value| SnapshotId { value });
    let intent = if args.get("write").and_then(Value::as_bool).unwrap_or(false) {
      Intent::Write
    } else {
      Intent::Read
    };
    let attached = self
      .client
      .attach(volume, snapshot, intent)
      .map_err(refusal)?;
    Ok(attachment_json(&attached))
  }

  fn detach(&mut self, args: &Value) -> Result<Value, McpError> {
    let attachment = u64_arg(args, "attachment")?;
    self.client.detach(attachment).map_err(refusal)?;
    Ok(json!({ "detached": true }))
  }

  fn read_base(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let path = string_arg(args, "path")?;
    let bytes = self.client.read_base(volume, &path).map_err(refusal)?;
    // The exact byte length is reported; the text is a lossy UTF-8 rendering (a base64 field for
    // binary bytes is owed), so an agent always knows the true size even when the text is lossy.
    Ok(json!({
      "path": path,
      "len": bytes.len(),
      "text": String::from_utf8_lossy(&bytes),
    }))
  }

  fn rewitness(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let paths = self
      .client
      .rewitness(volume, paths_opt(args))
      .map_err(refusal)?;
    Ok(json!({ "rewitnessed": paths }))
  }

  fn pin(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let pinned = self.client.pin(volume, paths_opt(args)).map_err(refusal)?;
    Ok(json!({ "pinned": pinned }))
  }

  fn status(&mut self) -> Result<Value, McpError> {
    let report = self.client.daemon_status().map_err(refusal)?;
    Ok(daemon_json(&report))
  }

  /// Plans a landing onto a host directory (§4.15). MCP never passes a grant (R10), so the reply is
  /// `GrantRequired` — the manifest and summary a human reviews, and the `slates grant` command that
  /// authorizes it — or `Landed` when there is nothing diverged to write. The grant itself is a
  /// human-only act on the CLI; this tool cannot make one.
  fn land_materialize(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let target = string_arg(args, "target")?;
    let snapshot = args
      .get("snapshot")
      .and_then(Value::as_u64)
      .map(|value| SnapshotId { value });
    let filter = Filter {
      include: string_list(args, "include"),
      exclude: string_list(args, "exclude"),
    };
    match self
      .client
      .land(volume, snapshot, &target, filter, None)
      .map_err(refusal)?
    {
      Landing::GrantRequired {
        landing,
        manifest,
        summary,
        conflicts,
      } => Ok(json!({
        "grant_required": true,
        "landing": landing,
        "manifest": hex32(&manifest),
        "summary": landing_summary_json(&summary),
        "conflicts": conflicts,
        "grant_with": format!("slates grant {landing}"),
      })),
      Landing::Landed(outcome) => Ok(json!({
        "grant_required": false,
        "outcome": outcome_json(&outcome),
      })),
    }
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
      "slates.merge.declare",
      "Declare a namespace op on a work (the counterpart to edit). `op.kind` is one of unlink{path}, \
       rename{from,to}, mkdir{path}, rmdir{path}, set_mode{path,mode}, symlink{path,target}, \
       link{path,target}, set_xattr{path,name,value}, remove_xattr{path,name}.",
      json!({ "work": string, "op": { "type": "object", "properties": { "kind": string, "path": string, "from": string, "to": string, "target": string, "name": string, "value": string, "mode": integer }, "required": ["kind"] } }),
      json!(["work", "op"]),
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
      "slates.attach.attach",
      "Attach to a volume for reading (or writing, taking its lease); the attachment id and lease.",
      json!({ "volume": string, "snapshot": integer, "write": { "type": "boolean" } }),
      json!(["volume"]),
    ),
    tool(
      "slates.attach.detach",
      "Detach an attachment, releasing its lease.",
      json!({ "attachment": integer }),
      json!(["attachment"]),
    ),
    tool(
      "slates.base.read_base",
      "Read a file's bytes from a volume's base (the text form and the exact length).",
      json!({ "volume": string, "path": string }),
      json!(["volume", "path"]),
    ),
    tool(
      "slates.base.rewitness",
      "Re-witness a volume's base entries (given `paths`, or all); the paths whose base drifted.",
      json!({ "volume": string, "paths": { "type": "array", "items": string } }),
      json!(["volume"]),
    ),
    tool(
      "slates.base.pin",
      "Pin a volume's base entries (given `paths`, or all) into memory; the count pinned.",
      json!({ "volume": string, "paths": { "type": "array", "items": string } }),
      json!(["volume"]),
    ),
    tool(
      "slates.land.materialize",
      "Plan a landing of a volume's diverged entries onto a host directory. Returns the manifest and \
       the `slates grant` command a human runs to authorize it — this tool never grants (R10).",
      json!({
        "volume": string,
        "target": string,
        "snapshot": integer,
        "include": { "type": "array", "items": string },
        "exclude": { "type": "array", "items": string },
      }),
      json!(["volume", "target"]),
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
/// Merge conflict windows as JSON. Public so the CLI's `--json` submit/rebase emit the same schema as
/// the MCP surface (§4.12 schema parity).
pub fn windows_json(windows: &[slates_ipc::protocol::MergeWindow]) -> Value {
  Value::Array(
    windows
      .iter()
      .map(|w| json!({ "path": w.path, "at": w.at, "len": w.len, "class": w.class }))
      .collect(),
  )
}

/// Reads a `WorkOp` from an `op` object (§4.16): `kind` names the operation, the rest are its fields.
/// The `WorkOp` enum stays inside the server — its wire form is this plain JSON object.
fn work_op_from(op: &Value) -> Result<WorkOp, McpError> {
  let kind = string_arg(op, "kind")?;
  let path = |op: &Value| string_arg(op, "path");
  let work_op = match kind.as_str() {
    "unlink" => WorkOp::Unlink { path: path(op)? },
    "rename" => WorkOp::Rename {
      from: string_arg(op, "from")?,
      to: string_arg(op, "to")?,
    },
    "mkdir" => WorkOp::Mkdir { path: path(op)? },
    "rmdir" => WorkOp::Rmdir { path: path(op)? },
    "set_mode" => WorkOp::SetMode {
      path: path(op)?,
      mode: u32::try_from(u64_arg(op, "mode")?).map_err(|_| McpError {
        code: code::INVALID_PARAMS,
        message: "mode is out of range".to_owned(),
      })?,
    },
    "symlink" => WorkOp::Symlink {
      path: path(op)?,
      target: string_arg(op, "target")?,
    },
    "link" => WorkOp::Link {
      path: path(op)?,
      target: string_arg(op, "target")?,
    },
    "set_xattr" => WorkOp::SetXattr {
      path: path(op)?,
      name: string_arg(op, "name")?,
      value: string_arg(op, "value")?.into_bytes(),
    },
    "remove_xattr" => WorkOp::RemoveXattr {
      path: path(op)?,
      name: string_arg(op, "name")?,
    },
    other => {
      return Err(McpError {
        code: code::INVALID_PARAMS,
        message: format!("unknown work op kind: {other}"),
      });
    }
  };
  Ok(work_op)
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

/// A volume summary as JSON. Public so the CLI's `--json` emits the same schema as the MCP surface
/// (§4.12 schema parity — one definition, two surfaces).
pub fn summary_json(v: &VolumeSummary) -> Value {
  json!({
    "id": id_hex(v.id),
    "name": v.name,
    "referenced_bytes": v.referenced_bytes,
    "unique_bytes": v.unique_bytes,
    "overlay": v.overlay,
  })
}

/// A volume status report as JSON. Public so the CLI's `--json` emits the same schema as the MCP
/// surface (§4.12 schema parity — one definition, two surfaces).
pub fn status_json(r: &StatusReport) -> Value {
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
    "nfs_port": r.nfs_port,
    "placed": {
      "region": r.placed.region,
      "host_epoch": r.placed.host_epoch,
      "mirror_age_ns": r.placed.mirror_age_ns,
    },
  })
}

/// A landing's summary as JSON: entries per action, bytes to write, and entries the filter excluded.
fn landing_summary_json(s: &LandingSummary) -> Value {
  json!({
    "by_action": s.by_action.iter().map(|a| json!({ "action": a.action, "count": a.count })).collect::<Vec<_>>(),
    "bytes": s.bytes,
    "filtered_out": s.filtered_out,
  })
}

/// A finished landing's outcome as JSON.
fn outcome_json(o: &LandingOutcome) -> Value {
  json!({
    "landing": o.landing,
    "state": o.state,
    "written": o.written,
    "skipped": o.skipped,
    "conflicts": o.conflicts,
    "failed": o.failed,
    "bytes_written": o.bytes_written,
  })
}

/// An attachment as JSON: its id (for detach), the lease epoch for a write attachment, and the path
/// (none until a bridge exists).
fn attachment_json(a: &Attachment) -> Value {
  json!({
    "attachment": a.attachment,
    "lease_epoch": a.lease_epoch,
    "path": a.path,
  })
}

/// The optional `paths` list of a base operation: `Some` of the given paths, or `None` (all paths)
/// when the argument is absent.
fn paths_opt(args: &Value) -> Option<Vec<String>> {
  args.get("paths").and_then(Value::as_array).map(|items| {
    items
      .iter()
      .filter_map(|v| v.as_str().map(str::to_owned))
      .collect()
  })
}

/// A required-or-empty list-of-strings argument (e.g. a landing filter's includes).
fn string_list(args: &Value, key: &str) -> Vec<String> {
  args
    .get(key)
    .and_then(Value::as_array)
    .map(|items| {
      items
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
    })
    .unwrap_or_default()
}

/// Format: a 32-byte hash as 64 lowercase hex characters.
fn hex32(bytes: &[u8; 32]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The daemon's status as JSON (the top-level counters and the shard count). Public so the CLI's
/// `--json` emits the same schema as the MCP surface (§4.12 schema parity).
pub fn daemon_json(r: &DaemonReport) -> Value {
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
