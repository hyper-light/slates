//! The MCP server (§4.12, Phase 6 task 8): the tools a coding agent calls over the Model Context
//! Protocol, mapped onto the Rust client SDK. This crate is the protocol and the dispatch — a pure
//! function from one JSON-RPC message to its reply — so it is driven directly in tests against a live
//! daemon; the `slates mcp` command wraps it in the stdio transport (the I/O boundary the CLI owns).
//!
//! The surface covers the merge loop (§4.16: create a green, clone a work, edit content, declare
//! namespace operations, submit, rebase, read the chain), the volume lifecycle (create, list, stat,
//! snapshot, clone, resize, destroy), attach and
//! detach, the base operations (read_base, digest, rewitness, pin), `slates.status`, and `slates.land`
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

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use serde_json::{Value, json};
use slates_client::{
  AttachRequest, AttachTransport, Attachment, AttachmentCapability, CauseRecord, ChokepointReport,
  Client, ClientError, Conformance, CreateSpec, DaemonReport, DeleteWhileOpen, Established, Filter,
  GreenBase, GroupReport, HostAnswer, Intent, KernelCache, Landing, LandingDegradation,
  LandingOutcome, LandingSummary, NamePolicy, OciBinding, ReadAt, ReadWritePolicy, Rebased,
  Residency, ShardReport, Signal, SizeClass, SnapshotId, SpanRecord, StatusReport, Submitted,
  TargetPathConstraint, TelemetryReport, TransportReport, UnsupportedReason, VolumeId,
  VolumeSummary, WorkOp,
};

// The loopback HTTP edge runs on a slates shard's TCP, which the runtime offers on macOS and Linux (the
// Windows runtime's sockets are UDP only); Windows serves MCP over stdio.
#[cfg(not(windows))]
pub mod http;

pub mod query;
mod skills;

#[cfg(not(windows))]
pub use http::serve;

/// Shape: the largest MCP message accepted on either transport — an HTTP body or a stdio line — a bound on
/// a hostile or mistaken sender so a message never drives an unbounded allocation (AUD-29-24's stdio
/// half). 64 MiB is far above any real JSON-RPC message and far below memory pressure.
pub const MAX_MESSAGE_BYTES: u64 = 64 << 20;

/// What one bounded stdio read produced ([`read_stdio_line`]).
#[derive(Debug, PartialEq, Eq)]
pub enum StdioLine {
  /// One message line, its newline removed.
  Message(Vec<u8>),
  /// A line past [`MAX_MESSAGE_BYTES`], consumed through its newline and discarded unread.
  TooLong,
  /// The end of input.
  End,
}

/// Reads one newline-terminated message from `reader`, keeping at most [`MAX_MESSAGE_BYTES`] of it: a
/// longer line is consumed to its end and answered [`StdioLine::TooLong`], so one oversized message costs
/// the bound, never its length, and the next line is read whole. A final line without a newline is a
/// message.
pub fn read_stdio_line(reader: &mut impl std::io::BufRead) -> std::io::Result<StdioLine> {
  let cap = usize::try_from(MAX_MESSAGE_BYTES).unwrap_or(usize::MAX);
  let mut line: Vec<u8> = Vec::new();
  let mut too_long = false;
  let mut read_any = false;
  loop {
    let available = reader.fill_buf()?;
    if available.is_empty() {
      return Ok(match (read_any, too_long) {
        (false, _) => StdioLine::End,
        (true, true) => StdioLine::TooLong,
        (true, false) => StdioLine::Message(line),
      });
    }
    read_any = true;
    let (chunk, ended) = match available.iter().position(|byte| *byte == b'\n') {
      Some(at) => (available.get(..at).unwrap_or_default(), Some(at)),
      None => (available, None),
    };
    if !too_long && line.len().saturating_add(chunk.len()) <= cap {
      line.extend_from_slice(chunk);
    } else {
      too_long = true;
      line = Vec::new();
    }
    let consumed = ended.map_or(available.len(), |at| at.saturating_add(1));
    reader.consume(consumed);
    if ended.is_some() {
      return Ok(if too_long {
        StdioLine::TooLong
      } else {
        StdioLine::Message(line)
      });
    }
  }
}

/// Format: the modern (stateless) MCP revisions this server speaks (§4.12, D-19): a request carrying one in
/// `_meta["io.modelcontextprotocol/protocolVersion"]` is served in it; any other is refused
/// `UnsupportedProtocolVersion` naming these.
const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// Format: the legacy (`initialize`-handshake) revisions a dual-era server also serves (MCP 2026-07-28 versioning:
/// "a dual-era server MAY serve both eras concurrently on the same endpoint", for at least the twelve-month
/// deprecation window), newest first: an `initialize` naming one is answered in it, any other in the first.
const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
/// The server's name, reported in `initialize` and in every modern result's `_meta`.
const SERVER_NAME: &str = "slates";
/// Format: the `_meta` key a modern request names its protocol version under.
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// Format: the `_meta` key a modern result names the server under.
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// Format: how long a client may cache a list or discovery result, in milliseconds: zero, "immediately stale"
/// (MCP 2026-07-28 `CacheableResult`). A daemon can be replaced by a newer binary under the same endpoint, so no
/// longer freshness is promised; the deterministic order of every list still lets a client's prompt cache hit.
const CACHE_TTL_MS: u64 = 0;
/// The guidance `server/discover` gives a model about this server (MCP 2026-07-28 `DiscoverResult.instructions`):
/// what the tools do together, not what each does.
const INSTRUCTIONS: &str = "slates gives each agent its own copy-on-write volume, kept in RAM: create or clone a volume, attach it (a mount path, or read and write through slates.fs), work in it, and bring work together through the merge loop (slates.merge.*). Nothing reaches the host's disk until a human grants a landing; slates.land.materialize only plans one and answers GrantRequired. Volume ids are opaque hex: pass back exactly what a tool returned. A refusal is typed in structuredContent.error; call slates.help for the workflow.";

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
  /// Format: JSON-RPC 2.0 "Invalid Request" (the message is not a valid request object, here an empty batch).
  pub(crate) const INVALID_REQUEST: i64 = -32600;
  /// Format: MCP 2026-07-28 `UNSUPPORTED_PROTOCOL_VERSION`: a modern request named a version this server does not
  /// speak.
  pub(crate) const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
  /// Format: MCP 2026-07-28 `HEADER_MISMATCH`: an HTTP request's MCP headers disagree with its body, or a required
  /// one is missing.
  #[cfg(not(windows))]
  pub(crate) const HEADER_MISMATCH: i64 = -32020;
}

/// Format: the radix of a volume id's hex text.
const HEX_RADIX: u32 = 16;

#[cfg(not(windows))]
/// Format: the `_meta` key a modern request names its protocol version under, for the HTTP edge's header check.
pub(crate) const META_PROTOCOL_VERSION_KEY: &str = META_PROTOCOL_VERSION;
#[cfg(not(windows))]
/// Format: `UnsupportedProtocolVersion`, answered `400` on HTTP.
pub(crate) const UNSUPPORTED_PROTOCOL_VERSION_CODE: i64 = code::UNSUPPORTED_PROTOCOL_VERSION;
#[cfg(not(windows))]
/// Format: JSON-RPC "method not found", answered `404` on HTTP.
pub(crate) const METHOD_NOT_FOUND_CODE: i64 = code::METHOD_NOT_FOUND;

#[cfg(not(windows))]
/// Whether `version` is a revision this server speaks, in either era.
pub(crate) fn speaks(version: &str) -> bool {
  MODERN_VERSIONS.contains(&version) || LEGACY_VERSIONS.contains(&version)
}

#[cfg(not(windows))]
/// The `HeaderMismatch` error (MCP 2026-07-28, `-32020`) for request `id`: its HTTP headers disagree with its body,
/// or a required one is missing, as `mismatch` says.
pub(crate) fn header_mismatch_reply(id: &Value, mismatch: &str) -> Value {
  error(id, code::HEADER_MISMATCH, mismatch)
}

/// A JSON-RPC "parse error" reply (against a null id), for the transport to send when a line of input
/// is not valid JSON. The protocol crate owns the code so the transport carries no wire constant.
pub fn parse_error_reply() -> Value {
  error(&Value::Null, code::PARSE_ERROR, "parse error")
}

/// An MCP server bound to one daemon connection. The connection is the agent's session; the MCP
/// protocol above it is stateless per request (§4.12), so each `handle` is independent.
pub struct McpServer {
  client: Client,
  /// Whether this transport can carry a subscription's stream: stdio can (every message shares the channel); the
  /// HTTP edge answers one JSON body per request and does not stream yet.
  streams: bool,
  /// The open `subscriptions/listen` requests, by their JSON-RPC id (MCP 2026-07-28 patterns/subscriptions).
  subscriptions: Vec<Value>,
}

/// Format: the `_meta` key every message of a subscription carries: the JSON-RPC id of its `subscriptions/listen`.
const META_SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
/// Format: the list-change notification types a `subscriptions/listen` filter names (MCP 2026-07-28).
const LIST_FILTERS: [&str; 3] = [
  "toolsListChanged",
  "promptsListChanged",
  "resourcesListChanged",
];

/// Derived: the subscriptions one connection may hold open at once: one per notification source this server has
/// (each list it publishes, and each skill document), past which a further one can only repeat an open one. Each
/// holds only its id, so the set stays bounded by this.
pub(crate) fn max_subscriptions() -> usize {
  LIST_FILTERS.len().saturating_add(skills::SKILLS.len())
}

/// The acknowledgement a `subscriptions/listen` request `id` opens its stream with (MCP 2026-07-28 patterns/subscriptions):
/// the filters asked for that this server honours. The list filters are honoured whole, and of the resource
/// subscriptions only the skill URIs it publishes; an unsupported type is left out, as the specification asks. None of
/// them changes while the server runs, so after this the stream carries nothing until it ends. One rule for stdio and
/// the HTTP edge.
pub(crate) fn listen_acknowledgement(id: &Value, params: &Value) -> Value {
  let asked = params.get("notifications").cloned().unwrap_or(Value::Null);
  let mut honoured = serde_json::Map::new();
  for filter in LIST_FILTERS {
    if asked.get(filter) == Some(&Value::Bool(true)) {
      honoured.insert(filter.to_owned(), Value::Bool(true));
    }
  }
  let resources: Vec<Value> = asked
    .get("resourceSubscriptions")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter(|uri| uri.as_str().and_then(skills::by_uri).is_some())
    .cloned()
    .collect();
  if !resources.is_empty() {
    honoured.insert("resourceSubscriptions".to_owned(), Value::Array(resources));
  }
  json!({
    "jsonrpc": "2.0",
    "method": "notifications/subscriptions/acknowledged",
    "params": {
      "_meta": { META_SUBSCRIPTION_ID: id },
      "notifications": honoured,
    },
  })
}

/// The refusal of a `subscriptions/listen` past the bound: `open` streams are open already, one per notification
/// source ([`max_subscriptions`]), so a further one could only repeat one of them.
pub(crate) fn subscriptions_full(open: usize) -> (i64, String) {
  (
    code::INVALID_PARAMS,
    format!("{open} subscriptions are open, one per notification source; cancel one first"),
  )
}

impl McpServer {
  /// Wraps a connected client, for a transport that answers one message per request (the HTTP edge).
  pub fn new(client: Client) -> McpServer {
    McpServer {
      client,
      streams: false,
      subscriptions: Vec::new(),
    }
  }

  /// Wraps a connected client for a streaming transport (stdio), where `subscriptions/listen` is served.
  pub fn streaming(client: Client) -> McpServer {
    McpServer {
      streams: true,
      ..McpServer::new(client)
    }
  }

  /// Ends every open subscription gracefully (MCP 2026-07-28 "Graceful Closure"): one completion result per
  /// `subscriptions/listen`, correlated by its id, for the transport to send before it closes.
  pub fn close_subscriptions(&mut self) -> Vec<Value> {
    self
      .subscriptions
      .drain(..)
      .map(|id| {
        let mut result = complete(json!({}));
        if let Some(meta) = result.get_mut("_meta").and_then(Value::as_object_mut) {
          meta.insert(META_SUBSCRIPTION_ID.to_owned(), id.clone());
        }
        reply(&id, result)
      })
      .collect()
  }

  /// `subscriptions/listen`: the acknowledgement, the subscription's first message. The server honours the list
  /// filters and the skill URIs it publishes; none of them changes while the server runs (the lists are fixed, the
  /// skills are compiled in), so after the acknowledgement the stream carries nothing until it ends. A URI the server
  /// does not publish is left out of the acknowledged filter, as the specification asks for an unsupported type.
  fn listen(&mut self, id: &Value, params: &Value) -> Result<Value, McpError> {
    if !self.streams {
      return Err(McpError {
        code: code::METHOD_NOT_FOUND,
        message: "subscriptions/listen is served over stdio and the HTTP edge, not this transport"
          .to_owned(),
      });
    }
    if self.subscriptions.len() >= max_subscriptions() {
      let (code, message) = subscriptions_full(self.subscriptions.len());
      return Err(McpError { code, message });
    }
    self.subscriptions.push(id.clone());
    Ok(listen_acknowledgement(id, params))
  }

  /// `notifications/cancelled` (MCP 2026-07-28 cancellation): a client ends its subscription on stdio by naming the
  /// listen request; the server sends nothing for it after.
  fn cancelled(&mut self, params: &Value) {
    if let Some(request) = params.get("requestId") {
      self.subscriptions.retain(|id| id != request);
    }
  }

  /// Handles one JSON-RPC message, returning its reply — or `None` for a notification (a message
  /// with no `id`, such as `notifications/initialized`), which JSON-RPC answers with nothing.
  pub fn handle(&mut self, request: &Value) -> Option<Value> {
    self.handle_with_header(request, None)
  }

  /// [`McpServer::handle`] for a request that arrived with the HTTP `MCP-Protocol-Version` header `header`, which
  /// also says which era the request is in.
  pub fn handle_with_header(&mut self, request: &Value, header: Option<&str>) -> Option<Value> {
    // A batch (JSON-RPC 2.0 §6; MCP 2025-03-26, a revision this server serves, requires it): each member is handled
    // as a message of its own and the replies are returned together; notifications add none, so a batch of only
    // notifications is answered with nothing, and an empty batch is one Invalid Request against a null id. Its size
    // is bounded by the transport's cap on a message. Before 2026-10-06 an array, having no `id`, was taken for a
    // notification and answered with nothing, and a client waited on its batch for good.
    if let Some(batch) = request.as_array() {
      if batch.is_empty() {
        return Some(error(&Value::Null, code::INVALID_REQUEST, "an empty batch"));
      }
      let replies: Vec<Value> = batch
        .iter()
        .filter_map(|member| {
          if member.is_array() {
            // A batch inside a batch is no request (§6 nests nothing): that member alone is refused.
            return Some(error(
              &Value::Null,
              code::INVALID_REQUEST,
              "a batch inside a batch",
            ));
          }
          self.handle_with_header(member, header)
        })
        .collect();
      return (!replies.is_empty()).then_some(Value::Array(replies));
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    if method == "notifications/cancelled" {
      self.cancelled(&params);
    }
    // A notification carries no `id` field and expects no reply; `?` returns nothing for it.
    let id = request.get("id").cloned()?;
    // The era is the request's own (MCP 2026-07-28 versioning); nothing is remembered between requests.
    let modern = match era(&params, method, header) {
      Ok(modern) => modern,
      Err(refusal) => return Some(refusal.reply(&id)),
    };
    let result = match method {
      "initialize" => Ok(initialize_result(&params)),
      // A legacy-era method (2026-07-28 removed it): a legacy client's liveness check is answered empty.
      "ping" if !modern => Ok(json!({})),
      "server/discover" => Ok(discover_result()),
      // The acknowledgement is the subscription's first message, not a reply: the reply ends the subscription.
      "subscriptions/listen" if modern => {
        return Some(
          self
            .listen(&id, &params)
            .unwrap_or_else(|e| error(&id, e.code, &e.message)),
        );
      }
      "tools/list" => Ok(json!({
        "tools": tool_list(),
        "ttlMs": CACHE_TTL_MS,
        "cacheScope": "public",
      })),
      "tools/call" => self.call_tool(&params),
      "resources/list" => Ok(resources_list()),
      "resources/templates/list" => Ok(resource_templates_list()),
      "resources/read" => self.resource_read(&params),
      "prompts/list" => Ok(prompts_list()),
      "prompts/get" => prompt_get(&params),
      other => Err(McpError {
        code: code::METHOD_NOT_FOUND,
        message: format!("unknown method: {other}"),
      }),
    };
    Some(match result {
      Ok(value) if modern => reply(&id, complete(value)),
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
      "slates.help" => help(&args),
      "slates.merge.create_green" => self.create_green(&args),
      "slates.merge.create_work" => self.create_work(&args),
      "slates.merge.edit" => self.edit(&args),
      "slates.merge.declare" => self.declare(&args),
      "slates.merge.submit" => self.submit(&args),
      "slates.merge.rebase" => self.rebase(&args),
      "slates.merge.versions" => self.versions(&args),
      "slates.merge.changed_since" => self.changed_since(&args),
      "slates.merge.advance" => self.advance(&args),
      "slates.fs.read" => self.read(&args),
      "slates.fs.list" => self.list(&args),
      "slates.fs.write" => self.fs_write(&args),
      "slates.fs.remove" => self.fs_remove(&args),
      "slates.fs.move" => self.fs_move(&args),
      "slates.fs.mkdir" => self.fs_mkdir(&args),
      "slates.query" => self.query(&args),
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
      "slates.base.digest" => self.digest(&args),
      "slates.base.rewitness" => self.rewitness(&args),
      "slates.base.pin" => self.pin(&args),
      "slates.land.materialize" => self.land_materialize(&args),
      "slates.status" => self.status(),
      other => {
        // An unknown tool is a protocol error (MCP server/tools: `-32602`), not a tool-execution one.
        return Err(McpError {
          code: code::INVALID_PARAMS,
          message: format!("unknown tool: {other}"),
        });
      }
    };
    // Anything that went wrong once the tool was found (an argument it could not use, a typed refusal from the
    // daemon, an unreachable daemon) is a tool-execution error the model sees and can act on (SEP-1303), never a
    // JSON-RPC error; its typed code stays in `structuredContent.error`.
    Ok(match structured {
      Ok(structured) => tool_result(&structured),
      Err(failure) => tool_error(&failure),
    })
  }

  /// Creates a green from scratch, or over a complete immutable base when `base_volume` and
  /// `base_snapshot` name a snapshot (§4.16; refused `ConsistentBaseUnavailable` for one still served
  /// from a host directory).
  fn create_green(&mut self, args: &Value) -> Result<Value, McpError> {
    let name = string_arg(args, "name")?;
    let require_evidence = args
      .get("require_evidence")
      .and_then(Value::as_bool)
      .unwrap_or(false);
    let green = match args.get("base_volume") {
      None => self.client.create_green(&name, require_evidence),
      Some(_) => {
        let base = GreenBase {
          volume: volume_arg(args, "base_volume")?,
          snapshot: SnapshotId {
            value: u64_arg(args, "base_snapshot")?,
          },
        };
        self.client.create_green_over(&name, require_evidence, base)
      }
    }
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

  /// Submits a work's increment, carrying the `evidence` references (hexadecimal BLAKE3 identities,
  /// opaque to slates) when given (§4.16; a green that requires evidence refuses without any).
  fn submit(&mut self, args: &Value) -> Result<Value, McpError> {
    let work = volume_arg(args, "work")?;
    let evidence = string_list(args, "evidence")
      .iter()
      .map(|text| hex_identity(text))
      .collect::<Result<Vec<[u8; 32]>, McpError>>()?;
    Ok(
      match self
        .client
        .submit_with_evidence(work, &evidence)
        .map_err(refusal)?
      {
        Submitted::Accepted(version) => json!({ "accepted": true, "version": version }),
        Submitted::Conflict(windows) => {
          json!({ "accepted": false, "conflicts": windows_json(&windows) })
        }
      },
    )
  }

  /// Re-pins a green attachment to `version`, or to the head (§4.16 "Attachments and versions"): the
  /// version now pinned and the paths the move invalidated.
  fn advance(&mut self, args: &Value) -> Result<Value, McpError> {
    let attachment = u64_arg(args, "attachment")?;
    let version = args.get("version").and_then(Value::as_u64);
    let advanced = self.client.advance(attachment, version).map_err(refusal)?;
    Ok(json!({ "version": advanced.version, "invalidated": advanced.invalidated }))
  }

  /// Reads a file at a view (§4.12 `slates.fs.read`): a green's head, its `version`, or the version
  /// the `attachment` pins; a work's or plain volume's live tree. The exact byte length and a lossy
  /// UTF-8 text, as `read_base` reports.
  fn read(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let path = string_arg(args, "path")?;
    let at = view_arg(args);
    let bytes = self.client.read(volume, &path, at).map_err(refusal)?;
    Ok(json!({
      "path": path,
      "len": bytes.len(),
      "text": String::from_utf8_lossy(&bytes),
    }))
  }

  /// `resources/read`: the skill document a `skill://` URI names, or the file a `volume://` URI names at its volume's
  /// head, read under this connection's rights (as `slates.fs.read` is) and given as text when its bytes are UTF-8,
  /// else as a base64 `blob` (MCP 2026-07-28 resource contents). A volume's file is never cached by the client (its
  /// head moves) and is private to the reader. `-32602` for a URI that names nothing (an unknown resource is invalid
  /// params, and an empty `contents` never stands for one).
  fn resource_read(&mut self, params: &Value) -> Result<Value, McpError> {
    let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
    let not_found = || McpError {
      code: code::INVALID_PARAMS,
      message: format!("Resource not found: {uri}"),
    };
    if let Some(skill) = skills::by_uri(uri) {
      return Ok(json!({
        "contents": [{ "uri": skill.uri(), "mimeType": skills::MIME_TYPE, "text": skill.body }],
        "ttlMs": CACHE_TTL_MS,
        "cacheScope": "public",
      }));
    }
    let (volume, path) = uri
      .strip_prefix(VOLUME_SCHEME)
      .and_then(|rest| rest.split_once('/'))
      .and_then(|(id, path)| Some((id_from_hex(id)?, percent_decode(path)?)))
      .filter(|(_, path)| !path.is_empty())
      .ok_or_else(not_found)?;
    let bytes = match self.client.read(volume, &path, ReadAt::Head) {
      Ok(bytes) => bytes,
      Err(ClientError::Refused(slates_ipc::protocol::Refusal::NotFound)) => return Err(not_found()),
      Err(e) => return Err(refusal(e)),
    };
    let content = match String::from_utf8(bytes) {
      Ok(text) => json!({ "uri": uri, "mimeType": "text/plain", "text": text }),
      Err(raw) => {
        json!({ "uri": uri, "mimeType": "application/octet-stream", "blob": base64(raw.as_bytes()) })
      }
    };
    Ok(json!({ "contents": [content], "ttlMs": 0, "cacheScope": "private" }))
  }

  fn fs_write(&mut self, args: &Value) -> Result<Value, McpError> {
    let target = (volume_arg(args, "volume")?, u64_arg(args, "attachment")?);
    let path = string_arg(args, "path")?;
    let text = string_arg(args, "text")?;
    let mode = mode_arg(args, FILE_MODE)?;
    let size = self
      .client
      .fs_write(target, &path, text.as_bytes(), mode)
      .map_err(refusal)?;
    Ok(json!({ "path": path, "size": size }))
  }

  fn fs_remove(&mut self, args: &Value) -> Result<Value, McpError> {
    let target = (volume_arg(args, "volume")?, u64_arg(args, "attachment")?);
    let path = string_arg(args, "path")?;
    self.client.fs_remove(target, &path).map_err(refusal)?;
    Ok(json!({ "path": path, "removed": true }))
  }

  fn fs_move(&mut self, args: &Value) -> Result<Value, McpError> {
    let target = (volume_arg(args, "volume")?, u64_arg(args, "attachment")?);
    let from = string_arg(args, "from")?;
    let to = string_arg(args, "to")?;
    let size = self.client.fs_rename(target, &from, &to).map_err(refusal)?;
    Ok(json!({ "from": from, "to": to, "size": size }))
  }

  fn fs_mkdir(&mut self, args: &Value) -> Result<Value, McpError> {
    let target = (volume_arg(args, "volume")?, u64_arg(args, "attachment")?);
    let path = string_arg(args, "path")?;
    let mode = mode_arg(args, DIR_MODE)?;
    self.client.fs_mkdir(target, &path, mode).map_err(refusal)?;
    Ok(json!({ "path": path, "made": true }))
  }

  fn list(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let path = args
      .get("path")
      .and_then(Value::as_str)
      .unwrap_or("")
      .to_owned();
    let at = view_arg(args);
    let entries = self.client.list_dir(volume, &path, at).map_err(refusal)?;
    let listed: Vec<Value> = entries
      .iter()
      .map(|entry| {
        json!({
          "name": entry.name,
          "kind": match entry.kind {
            slates_client::EntryKind::File => "file",
            slates_client::EntryKind::Dir => "dir",
            slates_client::EntryKind::Symlink => "symlink",
            slates_client::EntryKind::Other => "other",
          },
          "size": entry.size,
        })
      })
      .collect();
    Ok(json!({ "path": path, "entries": listed }))
  }

  fn query(&mut self, args: &Value) -> Result<Value, McpError> {
    let text = string_arg(args, "text")?;
    let answer =
      query::run(&mut ClientSource(&mut self.client), &text).map_err(|failure| McpError {
        code: match failure {
          query::QueryError::Refused(_) => code::REFUSED,
          _ => code::INVALID_PARAMS,
        },
        message: failure.to_string(),
      })?;
    let rows: Vec<Value> = answer
      .rows
      .iter()
      .map(|row| Value::Array(row.iter().map(query::Cell::to_json).collect()))
      .collect();
    Ok(json!({
      "columns": answer.columns,
      "rows": rows,
      "matched": answer.matched,
      "visited": answer.visited,
      "bytes_read": answer.bytes_read,
    }))
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
    let form = attach_form(args)?;
    let attached = self
      .client
      .attach_with(volume, snapshot, intent, form)
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

  /// `slates.base.digest` (§4.15): a clean base file's verified content digest — the BLAKE3 as
  /// 64 hex digits and the length digested; a diverged entry is the typed `DigestNotClean`
  /// refusal, so an agent reads and hashes those bytes itself rather than trust a stale digest.
  fn digest(&mut self, args: &Value) -> Result<Value, McpError> {
    let volume = volume_arg(args, "volume")?;
    let path = string_arg(args, "path")?;
    let digest = self.client.digest(volume, &path).map_err(refusal)?;
    let identity: String = digest.identity.iter().map(|b| format!("{b:02x}")).collect();
    Ok(json!({
      "path": path,
      "identity": identity,
      "size": digest.size,
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
    let (report, telemetry) = gather_daemon_status(&mut self.client).map_err(refusal)?;
    Ok(daemon_json(&report, &telemetry))
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

/// The server's identity, as both eras report it.
fn server_info() -> Value {
  json!({ "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") })
}

/// The legacy `initialize` result: the protocol version (the client's own when it is a legacy revision this server
/// speaks, else the newest legacy one), the server's identity, and its capabilities (tools).
fn initialize_result(params: &Value) -> Value {
  let asked = params.get("protocolVersion").and_then(Value::as_str);
  let version = asked
    .filter(|asked| LEGACY_VERSIONS.contains(asked))
    .or_else(|| LEGACY_VERSIONS.first().copied())
    .unwrap_or_default();
  json!({
    "protocolVersion": version,
    "serverInfo": server_info(),
    "capabilities": capabilities(),
    "instructions": INSTRUCTIONS,
  })
}

/// What this server offers, as both eras declare it: tools, resources (the skills) and prompts (the skills again).
fn capabilities() -> Value {
  json!({ "tools": {}, "resources": {}, "prompts": {} })
}

/// The `server/discover` result (MCP 2026-07-28): the modern versions this server speaks, its capabilities, and
/// guidance for the model; cacheable as every discovery result is.
fn discover_result() -> Value {
  json!({
    "supportedVersions": MODERN_VERSIONS,
    "capabilities": capabilities(),
    "instructions": INSTRUCTIONS,
    "ttlMs": CACHE_TTL_MS,
    "cacheScope": "public",
  })
}

/// A modern result: `resultType` "complete" (every result of this server is; it never asks for input mid-call) and
/// the server's identity in `_meta`.
fn complete(mut result: Value) -> Value {
  if let Some(object) = result.as_object_mut() {
    object.insert("resultType".to_owned(), json!("complete"));
    object.insert(
      "_meta".to_owned(),
      json!({ META_SERVER_INFO: server_info() }),
    );
  }
  result
}

/// Format: the `_meta` key a modern request names its capabilities under.
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// Why a request was refused before it was served: a JSON-RPC error, with its `data` when it carries one.
pub(crate) struct EraRefusal {
  /// The JSON-RPC error code.
  pub(crate) code: i64,
  message: String,
  data: Option<Value>,
}

impl EraRefusal {
  /// The refusal as the reply to request `id`.
  pub(crate) fn reply(&self, id: &Value) -> Value {
    let mut error = json!({ "code": self.code, "message": self.message });
    if let (Some(data), Some(object)) = (&self.data, error.as_object_mut()) {
      object.insert("data".to_owned(), data.clone());
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
  }
}

/// The era of a request with `params` and `method`, arrived with HTTP header `header` (MCP 2026-07-28 versioning,
/// SEP-2575): modern when its `_meta` names a protocol version or client capabilities (only a modern client sends
/// them; a legacy client's `_meta` may carry a `progressToken` and nothing more), or when the HTTP header names a
/// revision that is not a legacy one; legacy otherwise, as a dual-era server serves an `initialize` client. A modern
/// request must name a version this server speaks (`-32022` with the supported list otherwise) and its client
/// capabilities (`-32602`, as for a missing version); `initialize`, removed from the modern era, is `-32601`.
pub(crate) fn era(params: &Value, method: &str, header: Option<&str>) -> Result<bool, EraRefusal> {
  let meta = params.get("_meta");
  let version = meta.and_then(|meta| meta.get(META_PROTOCOL_VERSION));
  let capabilities = meta.and_then(|meta| meta.get(META_CLIENT_CAPABILITIES));
  let modern_header = header.is_some_and(|header| !LEGACY_VERSIONS.contains(&header));
  if version.is_none() && capabilities.is_none() && !modern_header {
    return Ok(false);
  }
  if method == "initialize" {
    return Err(EraRefusal {
      code: code::METHOD_NOT_FOUND,
      message:
        "initialize is not a method of the modern era (MCP 2026-07-28); call server/discover"
          .to_owned(),
      data: None,
    });
  }
  let Some(version) = version else {
    return Err(EraRefusal {
      code: code::INVALID_PARAMS,
      message: format!("a modern request names its version in _meta[\"{META_PROTOCOL_VERSION}\"]"),
      data: None,
    });
  };
  let version = version.as_str().unwrap_or("");
  if !MODERN_VERSIONS.contains(&version) {
    return Err(EraRefusal {
      code: code::UNSUPPORTED_PROTOCOL_VERSION,
      message: "Unsupported protocol version".to_owned(),
      data: Some(json!({ "supported": MODERN_VERSIONS, "requested": version })),
    });
  }
  if capabilities.is_none() {
    return Err(EraRefusal {
      code: code::INVALID_PARAMS,
      message: format!(
        "a modern request names its capabilities in _meta[\"{META_CLIENT_CAPABILITIES}\"]"
      ),
      data: None,
    });
  }
  Ok(true)
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
      "The slates workflow in brief and the names of its skills; with `skill`, that skill's full instructions.",
      json!({ "skill": string }),
      json!([]),
    ),
    tool(
      "slates.merge.create_green",
      "Create a green volume (the shared target agents merge into): from scratch, or over a complete \
       immutable base — `base_volume`'s `base_snapshot` (pin the whole base first).",
      json!({ "name": string, "require_evidence": { "type": "boolean" }, "base_volume": string, "base_snapshot": integer }),
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
      "Submit a work's increment to its green: accepted at a new version, or the conflict windows. \
       `evidence` lists opaque hexadecimal identities a green may require.",
      json!({ "work": string, "evidence": { "type": "array", "items": string } }),
      json!(["work"]),
    ),
    tool(
      "slates.merge.advance",
      "Re-pin a green attachment to `version` (or the head): the version pinned and the paths \
       invalidated. An attachment's view never moves otherwise.",
      json!({ "attachment": integer, "version": integer }),
      json!(["attachment"]),
    ),
    tool(
      "slates.fs.read",
      "Read a file's bytes (text and exact length) from a volume: a green's head, its `version`, or \
       the version an `attachment` pins; a work's or plain volume's live tree.",
      json!({ "volume": string, "path": string, "version": integer, "attachment": integer }),
      json!(["volume", "path"]),
    ),
    tool(
      "slates.fs.list",
      "List a directory's entries (name, kind file/dir/symlink/other, a file's size) in a volume: a green's head, \
       its `version`, or the version an `attachment` pins; a work's or plain volume's live tree. `path` defaults to \
       the root.",
      json!({ "volume": string, "path": string, "version": integer, "attachment": integer }),
      json!(["volume"]),
    ),
    tool(
      "slates.fs.write",
      "Write a file in a plain volume under your write attachment (slates.attach.attach with write): the file's \
       whole new content as `text`; created (with `mode`, by default rw-r--r--) when absent, its directory existing. The \
       file's new size.",
      json!({ "volume": string, "attachment": integer, "path": string, "text": string, "mode": integer }),
      json!(["volume", "attachment", "path", "text"]),
    ),
    tool(
      "slates.fs.remove",
      "Remove a file, symbolic link or empty directory in a plain volume, under your write attachment.",
      json!({ "volume": string, "attachment": integer, "path": string }),
      json!(["volume", "attachment", "path"]),
    ),
    tool(
      "slates.fs.move",
      "Rename `from` to `to` in a plain volume under your write attachment, replacing what `to` names.",
      json!({ "volume": string, "attachment": integer, "from": string, "to": string }),
      json!(["volume", "attachment", "from", "to"]),
    ),
    tool(
      "slates.fs.mkdir",
      "Make a directory (with `mode`, by default rwxr-xr-x) in a plain volume under your write attachment.",
      json!({ "volume": string, "attachment": integer, "path": string, "mode": integer }),
      json!(["volume", "attachment", "path"]),
    ),
    tool(
      "slates.query",
      "Codemode: one read-only query in place of many calls; only its answer comes back. \
       FROM volumes() | files(\"VOL\"[, version = N][, under = \"dir\"]) | lines(\"VOL\" ...) | changed(\"VOL\", since = N) \
       [WHERE cond] [SELECT cols] [ORDER BY col [DESC], ...] [LIMIT n]. Columns: volumes id,name,referenced,unique; \
       files path,name,ext,dir,kind,size,content; lines path,line,text; changed path. Conditions: = != < <= > >= \
       CONTAINS, STARTS WITH, ENDS WITH, GLOB (** spans directories), AND, OR, NOT. VOL is a volume id or name. \
       Work ceilings refuse by name; narrow with WHERE, under =, LIMIT.",
      json!({ "text": string }),
      json!(["text"]),
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
      "Attach to a volume for reading (or writing, taking its lease); the attachment id, lease, what was established and the transport's capability; a green attachment pins the green's head `version`, moved only by slates.merge.advance. With oci_source (the host mount point, from `slates mount`) and oci_destination (a path inside the container), a container bind: the verified source and the runtime `mounts` entry to hand your OCI runtime (read-only unless write).",
      json!({ "volume": string, "snapshot": integer, "write": { "type": "boolean" }, "oci_source": string, "oci_destination": string }),
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
      "slates.base.digest",
      "A clean base file's verified content digest: its BLAKE3 as hex and its length; refused `DigestNotClean` for an entry the volume changed.",
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
      "The daemon's status: generation, restarts, its place in the fleet, every shard's counters and health signals (each with what its absence means), and each shard's telemetry drain — the chokepoint spans since the last drain with their request, trace, span and cause identities, the loss markers, and every chokepoint's freshness.",
      json!({}),
      json!([]),
    ),
  ]
}

/// The `slates.help` text.
const HELP: &str = "slates merge over MCP: create_green -> create_work -> edit -> submit. \
On a conflict, read the green's bytes for each window (slates.fs.read at the version), rewrite \
your edit, then rebase and submit again. versions and changed_since read the chain. A reader \
attaches to a green (slates.attach.attach) and its view is pinned to that version until \
slates.merge.advance moves it. No tool creates a landing grant (a grant is a human-only act on \
the CLI).";

/// `slates.help`: with `skill`, that skill's document; without, the workflow in brief and the skills' names.
fn help(args: &Value) -> Result<Value, McpError> {
  match args.get("skill").and_then(Value::as_str) {
    Some(name) => skills::by_name(name)
      .map(|skill| json!({ "skill": skill.name, "text": skill.body }))
      .ok_or_else(|| McpError {
        code: code::INVALID_PARAMS,
        message: format!("no skill named {name}"),
      }),
    None => Ok(json!({
      "text": HELP,
      "skills": skills::SKILLS.iter().map(|skill| skill.name).collect::<Vec<_>>(),
    })),
  }
}

/// `resources/list`: one resource per skill (MCP 2026-07-28 server/resources), in a fixed order.
fn resources_list() -> Value {
  let resources: Vec<Value> = skills::SKILLS
    .iter()
    .map(|skill| {
      json!({
        "uri": skill.uri(),
        "name": skill.name,
        "description": skill.description(),
        "mimeType": skills::MIME_TYPE,
        "size": skill.body.len(),
      })
    })
    .collect();
  json!({ "resources": resources, "ttlMs": CACHE_TTL_MS, "cacheScope": "public" })
}

/// Format: the scheme of a volume file's resource URI, `volume://<volume id>/<path>` (the id as 32 lowercase hex
/// characters, the path percent-encoded as RFC 3986 allows).
const VOLUME_SCHEME: &str = "volume://";

/// `resources/templates/list`: a skill document by its name, and a volume's file by its volume and path.
fn resource_templates_list() -> Value {
  json!({
    "resourceTemplates": [{
      "uriTemplate": skills::URI_TEMPLATE,
      "name": "slates skill",
      "description": "A slates skill document (Agent Skills format), by its name.",
      "mimeType": skills::MIME_TYPE,
    }, {
      "uriTemplate": "volume://{volume}/{+path}",
      "name": "slates volume file",
      "description": "A file of a slates volume at its head, by the volume's id and the file's path, read under this \
        connection's rights.",
    }],
    "ttlMs": CACHE_TTL_MS,
    "cacheScope": "public",
  })
}

/// `prompts/list`: one prompt per skill, taking no arguments.
fn prompts_list() -> Value {
  let prompts: Vec<Value> = skills::SKILLS
    .iter()
    .map(|skill| json!({ "name": skill.name, "description": skill.description(), "arguments": [] }))
    .collect();
  json!({ "prompts": prompts, "ttlMs": CACHE_TTL_MS, "cacheScope": "public" })
}

/// `prompts/get`: the named skill's document as one user message; `-32602` for an unknown prompt.
fn prompt_get(params: &Value) -> Result<Value, McpError> {
  let name = params.get("name").and_then(Value::as_str).unwrap_or("");
  let skill = skills::by_name(name).ok_or_else(|| McpError {
    code: code::INVALID_PARAMS,
    message: format!("unknown prompt: {name}"),
  })?;
  Ok(json!({
    "description": skill.description(),
    "messages": [{ "role": "user", "content": { "type": "text", "text": skill.body } }],
  }))
}

/// A JSON-RPC success reply.
fn reply(id: &Value, result: Value) -> Value {
  json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC error reply.
pub(crate) fn error(id: &Value, code: i64, message: &str) -> Value {
  json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A `tools/call` result: the structured result, plus a text block carrying the same JSON (the shape
/// MCP clients render). `isError` stays false: a tool that reached the daemon and got an answer
/// succeeded even when that answer is a conflict.
fn tool_result(structured: &Value) -> Value {
  json!({
    "content": [ { "type": "text", "text": structured.to_string() } ],
    "structuredContent": structured,
    "isError": false,
  })
}

/// A failed `tools/call` (SEP-1303): `isError` true, the failure's message as text for the model, and its typed
/// code and message in `structuredContent.error` (`-32602` an argument the tool could not use, `-32000` a typed
/// refusal from the daemon, `-32001` an unreachable daemon).
fn tool_error(failure: &McpError) -> Value {
  json!({
    "content": [ { "type": "text", "text": failure.message } ],
    "structuredContent": { "error": { "code": failure.code, "message": failure.message } },
    "isError": true,
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
    "mounts": r
      .mounts
      .iter()
      .map(|mount| serde_json::json!({ "attachment": mount.attachment, "path": mount.path }))
      .collect::<Vec<_>>(),
    "mounts_elided": r.mounts_elided,
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
    "transports": transport_report_json(&r.transports),
  })
}

/// A transport's name on both surfaces (§4.6 A-9; one vocabulary for the CLI text and the JSON).
pub fn transport_name(transport: AttachTransport) -> &'static str {
  match transport {
    AttachTransport::Root => "root",
    AttachTransport::NfsLoopback => "nfs_loopback",
    AttachTransport::Fuse => "fuse",
    AttachTransport::Fskit => "fskit",
    AttachTransport::WinFsp => "winfsp",
    AttachTransport::Oci => "oci",
    AttachTransport::VirtioFsInProcess => "virtiofs_in_process",
    AttachTransport::VirtioFsInheritedDescriptor => "virtiofs_inherited_descriptor",
  }
}

/// A refusal reason's name on both surfaces.
pub fn unsupported_reason_name(reason: UnsupportedReason) -> &'static str {
  match reason {
    UnsupportedReason::HostPlatform => "host_platform",
    UnsupportedReason::ListenerNotBound => "listener_not_bound",
    UnsupportedReason::MountNeedsPrivilege => "mount_needs_privilege",
    UnsupportedReason::BridgeNotWired => "bridge_not_wired",
    UnsupportedReason::HostMountRequired => "host_mount_required",
    UnsupportedReason::SnapshotNotPresentedByHostMount => "snapshot_not_presented_by_host_mount",
    UnsupportedReason::SeamNotOnWire => "seam_not_on_wire",
    UnsupportedReason::BindingNotBuilt => "binding_not_built",
    UnsupportedReason::DaxNotEstablished => "dax_not_established",
    UnsupportedReason::NotificationQueueNotOffered => "notification_queue_not_offered",
    UnsupportedReason::FuseUnavailable => "fuse_unavailable",
    UnsupportedReason::ContainerWorkloadUnproven => "container_workload_unproven",
    UnsupportedReason::AllowOtherNotGranted => "allow_other_not_granted",
    UnsupportedReason::MountNotShared => "mount_not_shared",
  }
}

/// A target-path constraint's name on both surfaces.
pub fn target_path_name(target: TargetPathConstraint) -> &'static str {
  match target {
    TargetPathConstraint::RootMount => "root_mount",
    TargetPathConstraint::UserOwnedExistingDirectory => "user_owned_existing_directory",
    TargetPathConstraint::ContainerDestination => "container_destination",
    TargetPathConstraint::DriveLetter => "drive_letter",
    TargetPathConstraint::GuestTag => "guest_tag",
  }
}

/// A read/write policy's name on both surfaces.
pub fn read_write_name(policy: ReadWritePolicy) -> &'static str {
  match policy {
    ReadWritePolicy::ReadOnly => "read_only",
    ReadWritePolicy::ReadWrite => "read_write",
  }
}

/// A residency boundary's name on both surfaces.
pub fn residency_name(residency: Residency) -> &'static str {
  match residency {
    Residency::DaemonRam => "daemon_ram",
    Residency::DaemonRamAndKernelCache => "daemon_ram_and_kernel_cache",
    Residency::DaemonRamKernelCacheAndRuntimeVm => "daemon_ram_kernel_cache_and_runtime_vm",
    Residency::DaemonRamAndGuestPageCache { .. } => "daemon_ram_and_guest_page_cache",
  }
}

/// Format: what slates protects on every transport — the daemon's own RAM, locked and kept out of dumps (R1,
/// §4.2). Nothing beyond it is slates' to protect.
const PROTECTED: &str = "daemon_ram";

/// Where a transport's bytes reach beyond what slates protects (AUD-29-77): caches and memory of others —
/// the host kernel's page cache, the container runtime's VM, the guest's memory — which may be swapped,
/// dumped or snapshotted by their owners. A protected export is not a protected workload. A guest's memory is
/// the VMM's: its page cache and the very buffers the device copies replies into live there, and the device
/// maps it (vhost-user) or is handed it (in-process) without locking any of it — measured: the live guest's
/// mapping in the daemon held resident pages with 0 kB locked (`a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user`).
pub fn beyond_protection(residency: Residency) -> &'static [&'static str] {
  match residency {
    Residency::DaemonRam => &[],
    Residency::DaemonRamAndKernelCache => &["host_kernel_cache"],
    Residency::DaemonRamKernelCacheAndRuntimeVm => &["host_kernel_cache", "runtime_vm"],
    Residency::DaemonRamAndGuestPageCache { .. } => &["guest_memory"],
  }
}

/// A residency boundary as text: its name, for a guest whether DAX is mapped, and what lies beyond what
/// slates protects.
pub fn residency_text(residency: Residency) -> String {
  let name = match residency {
    Residency::DaemonRamAndGuestPageCache { dax_mapped } => {
      format!("{}(dax_mapped={dax_mapped})", residency_name(residency))
    }
    other => residency_name(other).to_owned(),
  };
  let beyond = beyond_protection(residency);
  if beyond.is_empty() {
    format!("{name} protected={PROTECTED}")
  } else {
    format!("{name} protected={PROTECTED} beyond={}", beyond.join(","))
  }
}

/// A residency boundary as JSON: `{ "kind", "protected", "beyond_protection" }`, and for a guest the DAX fact.
fn residency_json(residency: Residency) -> Value {
  let mut value = json!({
    "kind": residency_name(residency),
    "protected": PROTECTED,
    "beyond_protection": beyond_protection(residency),
  });
  if let Residency::DaemonRamAndGuestPageCache { dax_mapped } = residency
    && let Some(object) = value.as_object_mut()
  {
    object.insert("dax_mapped".to_owned(), json!(dax_mapped));
  }
  value
}

/// A conformance evidence class's name on both surfaces.
pub fn conformance_name(conformance: Conformance) -> &'static str {
  match conformance {
    Conformance::None => "none",
    Conformance::VerbLifecycleTest => "verb_lifecycle_test",
    Conformance::LiveKernelMountTest => "live_kernel_mount_test",
    Conformance::VerifiedSourceExport => "verified_source_export",
    Conformance::SimulatedGuestDriver => "simulated_guest_driver",
    Conformance::LiveGuestWorkloads => "live_guest_workloads",
  }
}

/// A delete-while-open rule's name on both surfaces.
pub fn delete_while_open_name(rule: DeleteWhileOpen) -> &'static str {
  match rule {
    DeleteWhileOpen::NoKernelClient => "no_kernel_client",
    DeleteWhileOpen::Unlinked => "unlinked",
    DeleteWhileOpen::SillyRenamed => "silly_renamed",
  }
}

/// A kernel cache posture as text: its kind, and for a negotiated one what was negotiated.
pub fn kernel_cache_text(cache: KernelCache) -> String {
  match cache {
    KernelCache::NotEstablished => "not_established".to_owned(),
    KernelCache::ClientTimeouts => "client_timeouts".to_owned(),
    KernelCache::Negotiated {
      writeback,
      explicit_invalidation,
    } => format!("negotiated(writeback={writeback},explicit_invalidation={explicit_invalidation})"),
    KernelCache::InheritedFromHostMount => "inherited_from_host_mount".to_owned(),
  }
}

/// A kernel cache posture as JSON: `{ "kind" }`, with the negotiated flags when negotiated.
fn kernel_cache_json(cache: KernelCache) -> Value {
  match cache {
    KernelCache::NotEstablished => json!({ "kind": "not_established" }),
    KernelCache::ClientTimeouts => json!({ "kind": "client_timeouts" }),
    KernelCache::Negotiated {
      writeback,
      explicit_invalidation,
    } => json!({
      "kind": "negotiated",
      "writeback": writeback,
      "explicit_invalidation": explicit_invalidation,
    }),
    KernelCache::InheritedFromHostMount => json!({ "kind": "inherited_from_host_mount" }),
  }
}

/// One transport's capability as JSON: the six facts of §4.6 A-9 and the refusal reason when not
/// supported. Public so the CLI's `--json` emits the same schema as the MCP surface (§4.12 parity).
pub fn capability_json(c: &AttachmentCapability) -> Value {
  json!({
    "transport": transport_name(c.transport),
    "supported": c.supported,
    "unsupported_reason": c.unsupported_reason.map(unsupported_reason_name),
    "target_path": target_path_name(c.target_path),
    "read_write": read_write_name(c.read_write),
    "sharing": {
      "one_owning_shard": c.sharing.one_owning_shard,
      "server_open_state": c.sharing.server_open_state,
      "cache": kernel_cache_json(c.sharing.cache),
      "delete_while_open": delete_while_open_name(c.sharing.delete_while_open),
    },
    "residency": residency_json(c.residency),
    "conformance": conformance_name(c.conformance),
  })
}

/// The host's transport report as JSON (§4.6 A-9). Public for the CLI's `--json status`.
pub fn transport_report_json(r: &TransportReport) -> Value {
  json!({
    "os": r.os,
    "kernel": r.kernel,
    "capabilities": r.capabilities.iter().map(capability_json).collect::<Vec<_>>(),
  })
}

/// The runtime-specification `mounts` entry as JSON — exactly what the harness hands its OCI runtime
/// (`destination`, `type`, `source`, `options`). Public so the CLI prints the same entry (§4.12).
pub fn oci_mount_json(binding: &OciBinding) -> Value {
  json!({
    "destination": binding.destination,
    "type": binding.mount_type,
    "source": binding.source,
    "options": binding.options,
  })
}

/// A container binding as JSON: the verified source, the destination, the policy, the mount table's
/// evidence, and the runtime entry.
pub fn oci_binding_json(binding: &OciBinding) -> Value {
  json!({
    "source": binding.source,
    "destination": binding.destination,
    "read_only": binding.read_only,
    "evidence": {
      "fstype": binding.evidence.fstype,
      "mount_source": binding.evidence.mount_source,
      "names_volume": binding.evidence.names_volume,
      "mount_id": binding.evidence.mount_id,
      "mount_device": binding.evidence.mount_device,
    },
    "mount": oci_mount_json(binding),
  })
}

/// What an attach established, as JSON: its form, and for a container bind the binding.
pub fn established_json(established: &Established) -> Value {
  match established {
    Established::Record => json!({ "form": "record" }),
    Established::OciBind { binding } => {
      json!({ "form": "oci_bind", "binding": oci_binding_json(binding) })
    }
  }
}

/// A landing's summary as JSON: entries per action, bytes to write, and entries the filter excluded.
/// Public so the CLI's `--json land` emits the same schema as the MCP surface (§4.12 schema parity).
pub fn landing_summary_json(s: &LandingSummary) -> Value {
  json!({
    "by_action": s.by_action.iter().map(|a| json!({ "action": a.action, "count": a.count })).collect::<Vec<_>>(),
    "bytes": s.bytes,
    "filtered_out": s.filtered_out,
  })
}

/// A finished landing's outcome as JSON. Public so the CLI's `--json land` emits the same schema as
/// the MCP surface (§4.12 schema parity — one definition, two surfaces).
pub fn outcome_json(o: &LandingOutcome) -> Value {
  json!({
    "landing": o.landing,
    "state": o.state,
    "written": o.written,
    "skipped": o.skipped,
    "conflicts": o.conflicts,
    "failed": o.failed,
    "bytes_written": o.bytes_written,
    "held": o.held,
    "durability": {
      "data_synced": o.durability.data_synced,
      "dirs_synced": o.durability.dirs_synced,
      "media": o.durability.media,
      "media_requested": o.durability.media_requested,
      "dirs": o.durability.dirs,
    },
    "degraded": o.degraded.iter().map(degradation_json).collect::<Vec<_>>(),
    "ramp_depth": o.ramp_depth,
  })
}

/// A Degraded cell a landing met, as JSON: its `kind` and its facts (§4.15 failure matrix; AUD-29-05).
/// Public so the CLI prints the same schema.
pub fn degradation_json(d: &LandingDegradation) -> Value {
  match d {
    LandingDegradation::NoExchange { widest_window_ns } => {
      json!({"kind": "no_exchange", "widest_window_ns": widest_window_ns})
    }
    LandingDegradation::BarriersOnly => json!({"kind": "barriers_only"}),
    LandingDegradation::Crashed { errno } => json!({"kind": "crashed", "errno": errno}),
    LandingDegradation::Unsynced { dir, answer } => {
      json!({"kind": "unsynced", "dir": dir, "answer": answer_json(answer)})
    }
    LandingDegradation::MediaUnsynced { answer } => {
      json!({"kind": "media_unsynced", "answer": answer_json(answer)})
    }
    LandingDegradation::Leftover { path, answer } => {
      json!({"kind": "leftover", "path": path, "answer": answer_json(answer)})
    }
    LandingDegradation::Unswept { dir, answer } => {
      json!({"kind": "unswept", "dir": dir, "answer": answer_json(answer)})
    }
    LandingDegradation::Kept { path, kept } => json!({"kind": "kept", "path": path, "kept": kept}),
  }
}

/// A host's answer as JSON: a typed refusal's name, or `{"errno": n}`.
fn answer_json(answer: &HostAnswer) -> Value {
  match answer {
    HostAnswer::NotFound => json!("not_found"),
    HostAnswer::NotDirectory => json!("not_directory"),
    HostAnswer::NotFile => json!("not_file"),
    HostAnswer::StaleHandle => json!("stale_handle"),
    HostAnswer::Errno { errno } => json!({"errno": errno}),
  }
}

/// An attachment as JSON: its id (for detach), the lease epoch for a write attachment, the path
/// (none until a bridge exists), what was established, and the transport's capability report (§4.6
/// A-9). Public so the CLI's `--json attach` emits the same schema as the MCP surface (§4.12 schema
/// parity — one definition, two surfaces).
pub fn attachment_json(a: &Attachment) -> Value {
  json!({
    "attachment": a.attachment,
    "lease_epoch": a.lease_epoch,
    "path": a.path,
    "version": a.version,
    "established": established_json(&a.established),
    "capability": capability_json(&a.capability),
  })
}

/// Format: a BLAKE3 identity is 32 bytes, given as 64 hexadecimal characters (two per byte).
const IDENTITY_HEX_CHARS: usize = 64;

/// A 64-character hexadecimal identity (an evidence reference) as its bytes, or a typed argument error.
fn hex_identity(text: &str) -> Result<[u8; 32], McpError> {
  let bad = || McpError {
    code: code::INVALID_PARAMS,
    message: format!("evidence is not a 64-character hexadecimal identity: {text}"),
  };
  if text.len() != IDENTITY_HEX_CHARS || !text.is_ascii() {
    return Err(bad());
  }
  let mut out = [0u8; 32];
  for (byte, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
    let hex = std::str::from_utf8(pair).map_err(|_| bad())?;
    *byte = u8::from_str_radix(hex, HEX_RADIX).map_err(|_| bad())?;
  }
  Ok(out)
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

/// The attach tool's form: the record form, or — with both `oci_source` and `oci_destination` — a
/// container bind whose source is resolved to the real path the kernel's mount table records (a read,
/// never a write; the mcp crate may read host paths for its transport, and this is a `canonicalize`).
/// One of the two alone is an invalid-params error naming the other.
fn attach_form(args: &Value) -> Result<AttachRequest, McpError> {
  let source = args.get("oci_source").and_then(Value::as_str);
  let destination = args.get("oci_destination").and_then(Value::as_str);
  match (source, destination) {
    (Some(source), Some(destination)) => {
      let source = std::fs::canonicalize(source)
        .map_err(|e| McpError {
          code: code::INVALID_PARAMS,
          message: format!("oci_source {source}: {e}"),
        })?
        .to_string_lossy()
        .into_owned();
      Ok(AttachRequest::Oci {
        source,
        destination: destination.to_owned(),
      })
    }
    (None, None) => Ok(AttachRequest::Root),
    (Some(_), None) => Err(McpError {
      code: code::INVALID_PARAMS,
      message: "oci_source needs oci_destination (the container path)".to_owned(),
    }),
    (None, Some(_)) => Err(McpError {
      code: code::INVALID_PARAMS,
      message: "oci_destination needs oci_source (the host mount point)".to_owned(),
    }),
  }
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

/// The daemon's status with every shard's telemetry ring drained (§4.14): the one gather both
/// operator surfaces use — `slates status` and the MCP `slates.status` — so the CLI and MCP read one
/// definition (§4.12 parity). Each shard's drain is bounded to one reply; what it left behind is in its
/// `remaining` and the next status reads it.
pub fn gather_daemon_status(
  client: &mut Client,
) -> Result<(DaemonReport, Vec<TelemetryReport>), ClientError> {
  let report = client.daemon_status()?;
  let mut telemetry = Vec::with_capacity(report.shards.len());
  for shard in &report.shards {
    telemetry.push(client.telemetry(shard.partition)?);
  }
  Ok((report, telemetry))
}

/// The daemon's status as JSON: the daemon's counters, every shard's block (its counters, refusals,
/// health signals with what their absence means, and its telemetry drain), and its place in the fleet.
/// Public so the CLI's `--json` emits the same schema as the MCP surface (§4.12 schema parity — the
/// text form's per-shard lines and this are one definition). A shard whose drain was not gathered
/// carries `"telemetry": null`.
pub fn daemon_json(r: &DaemonReport, telemetry: &[TelemetryReport]) -> Value {
  json!({
    "pid": r.pid,
    "generation": r.generation,
    "restarts": r.restarts,
    "heartbeat_age_ns": r.heartbeat_age_ns,
    "clients_reaped": r.clients_reaped,
    "clients_refused": r.clients_refused,
    "seal": {
      "state": r.seal.state,
      "root_id": r.seal.root_id.iter().map(|b| format!("{b:02x}")).collect::<String>(),
      "recipient_id": r.seal.recipient_id.iter().map(|b| format!("{b:02x}")).collect::<String>(),
      "key_slots": r.seal.key_slots,
      "keys_held": r.seal.keys_held,
    },
    "shards": r.shards.iter().map(|shard| {
      let drain = telemetry.iter().find(|batch| batch.partition == shard.partition);
      shard_json(shard, drain)
    }).collect::<Vec<_>>(),
    "fleet": {
      "host": r.fleet.host,
      "f": r.fleet.f,
      "host_epoch": r.fleet.host_epoch,
      "members": r.fleet.members,
      "peers_probed": r.fleet.peers_probed,
      "unknown_id": r.fleet.unknown_id,
      "inbox_full": r.fleet.inbox_full,
      "sessions_refused": r.fleet.sessions_refused,
      "replaced": r.fleet.replaced,
      "held_records": r.fleet.held_records,
      "takeovers_pending": r.fleet.takeovers_pending,
      "configuration_version": r.fleet.configuration_version,
      "takeover": takeover_json(&r.fleet.takeover),
      "council": group_json(&r.fleet.council),
      "root": group_json(&r.fleet.root),
      "detector": r.fleet.detector.iter().map(|peer| json!({
        "peer": peer.peer,
        "configured": peer.configured,
        "suspicions": peer.suspicions,
        "suspicion_allowance_milli": peer.suspicion_allowance_milli,
        "condemnations": peer.condemnations,
        "condemnation_allowance_milli": peer.condemnation_allowance_milli,
        "judged_by": peer.judged_by,
        "expected_ns": peer.expected_ns,
        "margin_ns": peer.margin_ns,
        "samples": peer.samples,
        "mistake_milli": peer.mistake_milli,
      })).collect::<Vec<_>>(),
      "detector_granularity_ns": r.fleet.detector_granularity_ns,
      "sessions": r.fleet.sessions.iter().map(|session| json!({
        "peer": session.peer,
        "lent": session.lent,
        "congestion_window": session.congestion_window,
        "smoothed_rtt_ns": session.smoothed_rtt_ns,
        "pto_ns": session.pto_ns,
        "spurious_losses": session.spurious_losses,
        "persistent_collapses": session.persistent_collapses,
        "bytes_consumed": session.bytes_consumed,
        "path_mtu": session.path_mtu,
      })).collect::<Vec<_>>(),
    },
  })
}

/// The takeover block of the fleet status as JSON (§4.8): this node's settled and current neighbourhood
/// versions and every retirement kept — the same fields the text form's `fleet_settled_generation`,
/// `fleet_neighbourhood_generation` and `fleet_retirement` lines print.
fn takeover_json(t: &slates_client::TakeoverReport) -> Value {
  json!({
    "settled_generation": t.settled_generation,
    "neighbourhood_generation": t.neighbourhood_generation,
    "retirements": t.retirements.iter().map(|r| json!({
      "host": r.host,
      "version": r.version,
      "survivors": r.survivors,
      "confirmed": r.confirmed,
      "unconfirmed": r.unconfirmed,
    })).collect::<Vec<_>>(),
    "members": t.members,
  })
}

/// A consensus group's block of the fleet status as JSON (§4.8): whether this node leads it, the election
/// timing it derived, and its election state — the same fields the text form's `fleet_<group>_*` lines print.
fn group_json(g: &GroupReport) -> Value {
  json!({
    "leads": g.leads,
    "base_periods": g.base_periods,
    "span_periods": g.span_periods,
    "rtt_tail_ns": g.rtt_tail_ns,
    "rtt_spread_ns": g.rtt_spread_ns,
    "samples": g.samples,
    "term": g.term,
    "priority_ns": g.priority_ns,
    "priority_spread_ns": g.priority_spread_ns,
    "rank": g.rank,
    "leader_lease": g.leader_lease,
    "pre_elections": g.pre_elections,
    "elections": g.elections,
    "pre_votes_granted": g.pre_votes_granted,
    "pre_votes_refused": g.pre_votes_refused,
    "refused_role": g.refused_role,
    "refused_leased": g.refused_leased,
    "refused_term": g.refused_term,
    "refused_log": g.refused_log,
    "voters": g.voters,
    "joint": g.joint,
  })
}

/// One shard's block of the daemon's status as JSON (§4.14): the fields the text form prints per
/// shard, its health signals, and its telemetry drain when gathered.
fn shard_json(s: &ShardReport, telemetry: Option<&TelemetryReport>) -> Value {
  json!({
    "partition": s.partition,
    "clients": s.clients,
    "volumes": s.volumes,
    "served": s.served,
    "refusals": s.refusals.iter().map(|r| json!({ "kind": r.kind, "count": r.count })).collect::<Vec<_>>(),
    "replayed_records": s.replayed_records,
    "replay_ns": s.replay_ns,
    "torn_tail": s.torn_tail,
    "mapped_bytes": s.mapped_bytes,
    "locked_bytes": s.locked_bytes,
    "reserve_bytes": s.reserve_bytes,
    "committed_bytes": s.committed_bytes,
    "version_slots": s.version_slots,
    "committed_versions": s.committed_versions,
    "signals": s.signals.iter().map(signal_json).collect::<Vec<_>>(),
    "spans_held": s.spans_held,
    "spans_dropped": s.spans_dropped,
    "peers_probed": s.peers_probed,
    "tasks_refused": s.tasks_refused,
    "landings_awaiting": s.landings_awaiting,
    "landings_awaiting_bound": s.landings_awaiting_bound,
    "landings_in_flight": s.landings_in_flight,
    "target_leases": s.target_leases,
    "nfs_calls": s.nfs_calls,
    "telemetry": telemetry.map(telemetry_json),
  })
}

/// A health signal as JSON (§4.14, A-9): the measured value, or `null` with `absence` saying what the
/// gap means — a measured zero is `0`, never conflated with an absent value.
pub fn signal_json(s: &Signal) -> Value {
  json!({
    "name": s.name,
    "value": s.value,
    "absence": s.absence.name(),
    "freshness_ns": s.freshness_ns,
  })
}

/// A shard's telemetry drain as JSON (§4.14): the batch's window, horizon and loss markers, the
/// registry in roster order with each chokepoint's freshness, and the spans with their three identities
/// distinct — the request as `{client, sequence}`, the trace as 32 hex characters, the span id, and the
/// cause as `{kind, span}`.
pub fn telemetry_json(t: &TelemetryReport) -> Value {
  json!({
    "partition": t.partition,
    "now_ns": t.now_ns,
    "window_ns": t.window_ns,
    "horizon_ns": t.horizon_ns,
    "shed_before": t.shed_before,
    "dropped_total": t.dropped_total,
    "remaining": t.remaining,
    "missing_links": t.missing_links,
    "chokepoints": t.chokepoints.iter().map(chokepoint_json).collect::<Vec<_>>(),
    "spans": t.spans.iter().map(|span| span_json(span, &t.chokepoints)).collect::<Vec<_>>(),
  })
}

/// One chokepoint's freshness entry as JSON (§4.14): `fresh` is the typed judgement; `latest_age_ns`
/// is `null` when nothing reported it in the batch, and a last sighting's age when one did but is past
/// the horizon — stated as an age, never as a live value.
fn chokepoint_json(c: &ChokepointReport) -> Value {
  json!({
    "name": c.name,
    "dimension": c.dimension,
    "spans": c.spans,
    "latest_age_ns": c.latest_age_ns,
    "fresh": c.fresh,
    "absence": c.absence.name(),
    "producer": c.producer,
    "expected": c.expected,
  })
}

/// One span as JSON, its chokepoint named through the batch's registry entries.
fn span_json(s: &SpanRecord, chokepoints: &[ChokepointReport]) -> Value {
  let point = chokepoints
    .get(usize::try_from(s.point).unwrap_or(usize::MAX))
    .map_or("", |entry| entry.name.as_str());
  let cause = match s.cause {
    CauseRecord::Root => json!({ "kind": "root", "span": Value::Null }),
    CauseRecord::Span { id } => json!({ "kind": "span", "span": id }),
    CauseRecord::Missing => json!({ "kind": "missing", "span": Value::Null }),
  };
  json!({
    "point": point,
    "label": s.label,
    "request": { "client": s.request_client, "sequence": s.request_sequence },
    "trace": format!("{:016x}{:016x}", s.trace_high, s.trace_low),
    "span": s.span,
    "cause": cause,
    "start_ns": s.start_ns,
    "end_ns": s.end_ns,
  })
}

/// The client's verbs as a query's source (`query::Source`).
struct ClientSource<'a>(&'a mut Client);

impl query::Source for ClientSource<'_> {
  fn volumes(&mut self) -> Result<Vec<(VolumeId, String, u64, u64)>, String> {
    self
      .0
      .list()
      .map(|volumes| {
        volumes
          .into_iter()
          .map(|volume| {
            (
              volume.id,
              volume.name,
              volume.referenced_bytes,
              volume.unique_bytes,
            )
          })
          .collect()
      })
      .map_err(|e| e.to_string())
  }

  fn list(
    &mut self,
    volume: VolumeId,
    path: &str,
    at: ReadAt,
  ) -> Result<Vec<(String, slates_client::EntryKind, u64)>, String> {
    self
      .0
      .list_dir(volume, path, at)
      .map(|entries| {
        entries
          .into_iter()
          .map(|e| (e.name, e.kind, e.size))
          .collect()
      })
      .map_err(|e| e.to_string())
  }

  fn read(&mut self, volume: VolumeId, path: &str, at: ReadAt) -> Result<Vec<u8>, String> {
    self.0.read(volume, path, at).map_err(|e| e.to_string())
  }

  fn changed(&mut self, green: VolumeId, since: u64) -> Result<Vec<String>, String> {
    self
      .0
      .changed_since(green, since)
      .map_err(|e| e.to_string())
  }
}

/// The view a read or listing names: a green's `version`, the version an `attachment` pins, or the head.
fn view_arg(args: &Value) -> ReadAt {
  match (
    args.get("version").and_then(Value::as_u64),
    args.get("attachment").and_then(Value::as_u64),
  ) {
    (Some(version), _) => ReadAt::Version { version },
    (None, Some(attachment)) => ReadAt::Attachment { attachment },
    (None, None) => ReadAt::Head,
  }
}

/// Format: the permission bits `slates.fs.write` gives a file it creates when the call names none (a umask of 022's).
const FILE_MODE: u32 = 0o644;
/// Format: the permission bits `slates.fs.mkdir` gives a directory when the call names none (a umask of 022's).
const DIR_MODE: u32 = 0o755;
/// Format: the permission bits a mode argument may carry (the file type and set-id bits are not the caller's to set
/// here).
const MODE_BITS: u64 = 0o1777;

/// The optional `mode` argument: `default` when absent; a value past the permission and sticky bits is a bad argument.
fn mode_arg(args: &Value, default: u32) -> Result<u32, McpError> {
  match args.get("mode") {
    None | Some(Value::Null) => Ok(default),
    Some(value) => value
      .as_u64()
      .filter(|mode| mode & !MODE_BITS == 0)
      .and_then(|mode| u32::try_from(mode).ok())
      .ok_or_else(|| McpError {
        code: code::INVALID_PARAMS,
        message: "mode: permission bits, at most 0o1777".to_owned(),
      }),
  }
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

/// The bytes of a percent-encoded URI path (RFC 3986 §2.1), as UTF-8; `None` for a malformed escape or bytes that are
/// not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
  let mut bytes = Vec::with_capacity(text.len());
  let mut rest = text.as_bytes();
  while let Some((&first, tail)) = rest.split_first() {
    if first == b'%' {
      let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
      bytes.push(u8::from_str_radix(hex, PERCENT_RADIX).ok()?);
      rest = tail.get(2..)?;
    } else {
      bytes.push(first);
      rest = tail;
    }
  }
  String::from_utf8(bytes).ok()
}

/// Format: a percent escape's two digits are hexadecimal (RFC 3986 §2.1).
const PERCENT_RADIX: u32 = 16;
/// Format: base64 encodes each group of three bytes (RFC 4648 §4)...
const BASE64_GROUP_BYTES: usize = 3;
/// Format: ...as four characters, padded with `=` (RFC 4648 §4).
const BASE64_GROUP_CHARS: usize = 4;
/// Format: one base64 character carries six bits (RFC 4648 §4).
const BASE64_SEXTET: u32 = 0x3f;
/// Format: the base64 alphabet (RFC 4648 §4).
const BASE64_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `bytes` in base64 with padding (RFC 4648 §4), as MCP resource blobs carry them.
fn base64(bytes: &[u8]) -> String {
  let sextet = |value: u32| {
    char::from(
      BASE64_ALPHABET
        .get(usize::try_from(value & BASE64_SEXTET).unwrap_or(0))
        .copied()
        .unwrap_or(b'A'),
    )
  };
  let mut out = String::with_capacity(
    bytes
      .len()
      .div_ceil(BASE64_GROUP_BYTES)
      .saturating_mul(BASE64_GROUP_CHARS),
  );
  for group in bytes.chunks(BASE64_GROUP_BYTES) {
    let byte = |at: usize| group.get(at).copied().unwrap_or(0);
    let word = u32::from_be_bytes([0, byte(0), byte(1), byte(2)]);
    out.push(sextet(word >> 18));
    out.push(sextet(word >> 12));
    out.push(if group.len() > 1 {
      sextet(word >> 6)
    } else {
      '='
    });
    out.push(if group.len() > 2 { sextet(word) } else { '=' });
  }
  out
}

/// A volume id as 32 lowercase hex characters.
fn id_hex(id: VolumeId) -> String {
  id.bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a volume id from 32 hex characters, or `None` when it is malformed.
fn id_from_hex(text: &str) -> Option<VolumeId> {
  let mut bytes = [0u8; 16];
  if text.len() != bytes.len().saturating_mul(2) {
    return None;
  }
  for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
    *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, HEX_RADIX).ok()?;
  }
  Some(VolumeId { bytes })
}

#[cfg(test)]
mod stdio_tests {
  use super::*;

  /// AUD-29-24 (the stdio half): do: read a message line, a line one byte past the message bound, then
  /// another message and a final line without a newline; expect the first and last messages whole, the
  /// oversized line reported `TooLong` without being kept, and the line after it read whole.
  #[test]
  fn an_oversized_stdio_line_is_discarded_and_the_next_read_whole() {
    let cap = usize::try_from(MAX_MESSAGE_BYTES).unwrap();
    let mut input = b"{\"a\":1}\n".to_vec();
    input.extend(std::iter::repeat_n(b'x', cap + 1));
    input.extend_from_slice(b"\n{\"b\":2}\n{\"c\":3}");
    let mut reader = std::io::BufReader::new(input.as_slice());
    let lines: Vec<StdioLine> =
      std::iter::from_fn(|| match read_stdio_line(&mut reader).unwrap() {
        StdioLine::End => None,
        line => Some(line),
      })
      .collect();
    assert_eq!(
      lines,
      vec![
        StdioLine::Message(b"{\"a\":1}".to_vec()),
        StdioLine::TooLong,
        StdioLine::Message(b"{\"b\":2}".to_vec()),
        StdioLine::Message(b"{\"c\":3}".to_vec()),
      ]
    );
  }
}

#[cfg(test)]
mod resource_encoding_tests {
  use super::*;

  /// RFC 4648 §10's test vectors for base64 with padding, which a `volume://` resource's blob uses. Do: encode each.
  /// Expect: the RFC's output, byte for byte.
  #[test]
  fn base64_matches_the_rfc_vectors() {
    for (input, expected) in [
      ("", ""),
      ("f", "Zg=="),
      ("fo", "Zm8="),
      ("foo", "Zm9v"),
      ("foob", "Zm9vYg=="),
      ("fooba", "Zm9vYmE="),
      ("foobar", "Zm9vYmFy"),
    ] {
      assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
    }
    assert_eq!(base64(&[0xff, 0x00, 0xfe]), "/wD+");
  }

  /// A `volume://` path's percent-decoding (RFC 3986 §2.1). Do: decode plain, escaped, multi-byte UTF-8 and malformed
  /// paths. Expect: the decoded path, and `None` for a truncated escape, a non-hex escape, or bytes that are not UTF-8.
  #[test]
  fn volume_paths_percent_decode_and_malformed_ones_are_refused() {
    assert_eq!(percent_decode("src/lib.rs").as_deref(), Some("src/lib.rs"));
    assert_eq!(
      percent_decode("src%2Flib.rs").as_deref(),
      Some("src/lib.rs")
    );
    assert_eq!(percent_decode("caf%C3%A9").as_deref(), Some("café"));
    assert_eq!(percent_decode("a%2"), None);
    assert_eq!(percent_decode("a%zz"), None);
    assert_eq!(percent_decode("%ff%fe"), None);
  }
}
