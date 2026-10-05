//! Codemode against list-and-read on a real agent task (§4.12, A-86; condition 13): which Rust files of this
//! repository's `crates/` mention `unsafe`. An in-process daemon serves an overlay volume over the tree (read-only),
//! and the MCP server is driven as an agent drives it. One path walks the tree with `slates.fs.list` and reads every
//! `.rs` file with `slates.fs.read`, filtering on the client; the other asks one `slates.query`. Both answers must be
//! the same set. Reported per path: the tool calls, the bytes of the JSON-RPC replies (what enters an agent's
//! context), and the wall time.
//!
//! `cargo run --release -p slates-mcp --example codemode_tokens`

use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use slates_client::{Client, Deadlines};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_mcp::McpServer;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the probe budget of the machine profile (the MCP tests' own).
const PROBE_BUDGET: Duration = Duration::from_millis(5);
/// Shape: the bytes per token the report divides by, a rough English and code average (a heuristic, not a tokenizer).
const BYTES_PER_TOKEN: usize = 4;
/// Shape: the daemon's shards.
const SHARDS: u16 = 2;

/// What one path cost.
#[derive(Default)]
struct Cost {
  calls: usize,
  reply_bytes: usize,
}

/// Calls tool `name`, counting the reply's bytes into `cost`; its structured content.
fn call(
  server: &mut McpServer,
  cost: &mut Cost,
  name: &str,
  arguments: Value,
) -> Result<Value, Box<dyn Error>> {
  let request = json!({
    "jsonrpc": "2.0",
    "id": cost.calls,
    "method": "tools/call",
    "params": { "name": name, "arguments": arguments },
  });
  let reply = server.handle(&request).ok_or("no reply")?;
  cost.calls += 1;
  cost.reply_bytes += serde_json::to_vec(&reply)?.len();
  let result = reply
    .get("result")
    .ok_or_else(|| format!("{name}: {reply}"))?;
  if result.get("isError") != Some(&Value::Bool(false)) {
    return Err(format!("{name} failed: {reply}").into());
  }
  Ok(
    result
      .get("structuredContent")
      .cloned()
      .unwrap_or(Value::Null),
  )
}

/// The list-and-read path: every directory listed, every `.rs` file read, the match done here.
fn list_and_read(
  server: &mut McpServer,
  volume: &str,
) -> Result<(BTreeSet<String>, Cost), Box<dyn Error>> {
  let mut cost = Cost::default();
  let mut found = BTreeSet::new();
  let mut dirs = vec![String::new()];
  while let Some(dir) = dirs.pop() {
    let listing = call(
      server,
      &mut cost,
      "slates.fs.list",
      json!({ "volume": volume, "path": dir }),
    )?;
    for entry in listing
      .get("entries")
      .and_then(Value::as_array)
      .into_iter()
      .flatten()
    {
      let name = entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
      let path = if dir.is_empty() {
        name.to_owned()
      } else {
        format!("{dir}/{name}")
      };
      match entry.get("kind").and_then(Value::as_str) {
        Some("dir") => dirs.push(path),
        Some("file") if path.ends_with(".rs") => {
          let read = call(
            server,
            &mut cost,
            "slates.fs.read",
            json!({ "volume": volume, "path": path }),
          )?;
          if read
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("unsafe"))
          {
            found.insert(path);
          }
        }
        _ => {}
      }
    }
  }
  Ok((found, cost))
}

/// The codemode path: one query.
fn codemode(
  server: &mut McpServer,
  volume: &str,
) -> Result<(BTreeSet<String>, Cost), Box<dyn Error>> {
  let mut cost = Cost::default();
  let text = format!(
    r#"FROM files("{volume}") WHERE ext = "rs" AND content CONTAINS "unsafe" SELECT path ORDER BY path"#
  );
  let answer = call(server, &mut cost, "slates.query", json!({ "text": text }))?;
  let found = answer
    .get("rows")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|row| row.get(0).and_then(Value::as_str).map(str::to_owned))
    .collect();
  Ok((found, cost))
}

fn report(label: &str, cost: &Cost, elapsed: Duration, found: usize) {
  println!(
    "{label}: {} calls, {} reply bytes (about {} tokens at {BYTES_PER_TOKEN} bytes each), {} ms, {found} files",
    cost.calls,
    cost.reply_bytes,
    cost.reply_bytes / BYTES_PER_TOKEN,
    elapsed.as_millis()
  );
}

fn main() -> Result<(), Box<dyn Error>> {
  let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
  let profile = MachineProfile::measure(ProfileOptions {
    budget_per_probe: PROBE_BUDGET,
    codecs: false,
    core_matrix: false,
  })?;
  let instance = format!("codemode-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-codemode-{}", std::process::id()),
    },
  )?;
  daemon
    .bootstrap(true)
    .map_err(|refusal| format!("bootstrap: {refusal:?}"))?;
  let deadlines = Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get();
  let mut server = McpServer::new(Client::connect(&instance, deadlines)?);
  let mut setup = Cost::default();
  let created = call(
    &mut server,
    &mut setup,
    "slates.volume.create",
    json!({ "name": "crates", "dynamic": 1u64 << 30, "base": base.canonicalize()?.display().to_string() }),
  )?;
  let volume = created
    .get("volume")
    .and_then(Value::as_str)
    .ok_or("no volume id")?
    .to_owned();
  let started = Instant::now();
  let (walked, walk_cost) = list_and_read(&mut server, &volume)?;
  let walk_time = started.elapsed();
  let started = Instant::now();
  let (queried, query_cost) = codemode(&mut server, &volume)?;
  let query_time = started.elapsed();
  report("list-and-read", &walk_cost, walk_time, walked.len());
  report("codemode", &query_cost, query_time, queried.len());
  println!(
    "same answer: {}; reply bytes {}x fewer, calls {}x fewer",
    walked == queried,
    walk_cost.reply_bytes / query_cost.reply_bytes.max(1),
    walk_cost.calls / query_cost.calls.max(1)
  );
  daemon.stop();
  if walked != queried {
    return Err(
      format!(
        "the answers differ: {:?}",
        walked.symmetric_difference(&queried).collect::<Vec<_>>()
      )
      .into(),
    );
  }
  Ok(())
}
