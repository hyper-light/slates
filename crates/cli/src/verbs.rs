//! The client verbs: one connection, one request, the reply printed in a stable plain form.

use slates_client::{
  Client, ClientError, CreateSpec, DaemonReport, Deadlines, Rebased, StatusReport, Submitted,
  VolumeSummary,
};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_server::daemon::LIVENESS_BUDGET_NS;

use crate::Failure;
use crate::args::{ClientRequest, ProfileOptions, Verb};
use crate::format::volume_id_text;

fn failure_of(e: ClientError, instance: &str) -> Failure {
  match e {
    ClientError::Refused(refusal) => Failure::Refused(format!("{refusal:?}")),
    ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. })
    | ClientError::DaemonGone { .. } => Failure::Unavailable(instance.to_owned()),
    other => Failure::Failed(other.to_string()),
  }
}

fn connect(instance: &str) -> Result<Client, Failure> {
  let deadlines = Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS).get();
  Client::connect(instance, deadlines).map_err(|e| failure_of(e, instance))
}

/// Runs one client verb.
pub(crate) fn run(request: &ClientRequest) -> Result<(), Failure> {
  // `unmount` is a pure OS operation (`umount`); it needs no daemon, so it runs before connecting —
  // a stale mount is unmountable even after its daemon has gone.
  if let Verb::Unmount { path } = &request.verb {
    crate::mount::unmount(path)?;
    println!("unmounted: {path}");
    return Ok(());
  }
  let mut client = connect(&request.instance)?;
  // `mount` reads the volume's name and the daemon's NFS port through the client, then runs `mount_nfs`
  // to mount it over the loopback NFS bridge (§4.6) — no privilege, no kernel extension, no Apple
  // entitlement. Its failure is a `Failure` (a mount refusal, not a client error), so it is handled
  // here rather than in [`serve`].
  if let Verb::Mount { volume, path } = &request.verb {
    let report = client
      .status(*volume)
      .map_err(|e| failure_of(e, &request.instance))?;
    let mounted = crate::mount::establish(&report, path)?;
    println!("mounted: {mounted}");
    return Ok(());
  }
  let outcome = serve(&mut client, &request.verb);
  outcome.map_err(|e| failure_of(e, &request.instance))
}

/// Serves the MCP tools (§4.12): the merge/volume/attach/base/land tools mapped onto the client SDK,
/// over stdio by default or loopback Streamable HTTP when a port is given. MCP is the tool-plane edge
/// only — the fleet's claims plane is a separate owned protocol (hecate's two-plane UDP), never MCP.
/// The protocol and dispatch live in `slates-mcp`; this is the transport the CLI owns.
pub(crate) fn mcp(options: &crate::args::McpOptions) -> Result<(), Failure> {
  let client = connect(&options.instance)?;
  let server = slates_mcp::McpServer::new(client);
  match options.http {
    Some(port) => serve_mcp_http(server, port),
    None => serve_mcp_stdio(server),
  }
}

/// Serves MCP over a loopback Streamable HTTP port (§4.12): the local edge, HTTP/1.1 over loopback.
fn serve_mcp_http(server: slates_mcp::McpServer, port: u16) -> Result<(), Failure> {
  let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
    .map_err(|e| Failure::Failed(format!("mcp http bind: {e}")))?;
  slates_mcp::serve(server, &listener).map_err(|e| Failure::Failed(format!("mcp http: {e}")))
}

/// Serves MCP over stdio: one JSON-RPC message per line in, its reply per line out, until end of
/// input. A line that is not valid JSON gets a JSON-RPC parse error, so a malformed message never
/// stops the server.
fn serve_mcp_stdio(mut server: slates_mcp::McpServer) -> Result<(), Failure> {
  use std::io::{BufRead, Write};
  let stdin = std::io::stdin();
  let mut input = stdin.lock();
  let mut out = std::io::stdout().lock();
  let mut line = String::new();
  loop {
    line.clear();
    let read = input
      .read_line(&mut line)
      .map_err(|e| Failure::Failed(e.to_string()))?;
    if read == 0 {
      return Ok(()); // end of input
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
      continue;
    }
    let reply = match serde_json::from_str::<serde_json::Value>(trimmed) {
      Ok(request) => server.handle(&request),
      // Not valid JSON: the protocol crate builds the JSON-RPC parse-error reply.
      Err(_) => Some(slates_mcp::parse_error_reply()),
    };
    if let Some(reply) = reply {
      writeln!(out, "{reply}").map_err(|e| Failure::Failed(e.to_string()))?;
      out.flush().map_err(|e| Failure::Failed(e.to_string()))?;
    }
  }
}

/// The merge verbs (§4.16), split out to keep [`serve`] under the cognitive-complexity bound.
fn serve_merge(client: &mut Client, verb: &Verb) -> Result<(), ClientError> {
  match verb {
    Verb::Green { name, evidence } => {
      let id = client.create_green(name, *evidence)?;
      println!("id: {}", volume_id_text(id));
    }
    Verb::Versions { green } => {
      println!("head: {}", client.versions(*green)?);
    }
    Verb::ChangedSince { green, version } => {
      for path in client.changed_since(*green, *version)? {
        println!("{path}");
      }
    }
    Verb::Work { green, name } => {
      let (id, base) = client.create_work(*green, name)?;
      println!("id: {}", volume_id_text(id));
      println!("base: {base}");
    }
    Verb::Edit {
      work,
      path,
      at,
      delete_len,
      bytes,
    } => {
      client.edit(*work, path, *at, *delete_len, bytes)?;
      println!("edited");
    }
    Verb::Submit { work } => match client.submit(*work)? {
      Submitted::Accepted(version) => println!("accepted: {version}"),
      Submitted::Conflict(windows) => print_windows(&windows),
    },
    Verb::Rebase { work } => match client.rebase(*work)? {
      Rebased::Rebased(version) => println!("rebased: {version}"),
      Rebased::Conflict(windows) => print_windows(&windows),
    },
    // Only the merge verbs above reach here.
    _ => {}
  }
  Ok(())
}

/// Prints merge conflict windows, one per line: the path, the byte range, and the conflict class.
fn print_windows(windows: &[slates_ipc::protocol::MergeWindow]) {
  println!("conflict:");
  for window in windows {
    println!(
      "  {} [{}..{}] class {}",
      window.path,
      window.at,
      window.at + window.len,
      window.class
    );
  }
}

fn serve(client: &mut Client, verb: &Verb) -> Result<(), ClientError> {
  match verb {
    Verb::Create {
      name,
      size,
      names,
      require_locked,
      base,
    } => {
      let id = client.create(&CreateSpec {
        name: name.clone(),
        size: *size,
        names: *names,
        require_locked: *require_locked,
        base: base.clone(),
      })?;
      println!("id: {}", volume_id_text(id));
      println!("path: (none until a bridge exists)");
    }
    Verb::List => {
      for volume in client.list()? {
        println!("{}", summary_line(&volume));
      }
    }
    Verb::DaemonStatus => {
      print!("{}", daemon_status_text(&client.daemon_status()?));
    }
    Verb::Status { volume, drift } => {
      let report = client.status(*volume)?;
      if *drift {
        for path in &report.drifted {
          println!("{path}");
        }
      } else {
        print!("{}", status_text(&report));
      }
    }
    // `mount`/`unmount` run `mount_nfs`/`umount` (CLI/OS operations whose failure is a `Failure`, not a
    // `ClientError`), so [`run`] handles them before this dispatch; they never reach here.
    Verb::Mount { .. } | Verb::Unmount { .. } => {}
    Verb::Snapshot { volume } => {
      println!("snapshot: {}", client.snapshot(*volume)?.value);
    }
    Verb::DestroySnapshot { volume, snapshot } => {
      client.destroy_snapshot(*volume, *snapshot)?;
      println!("snapshot destroyed");
    }
    Verb::Green { .. }
    | Verb::Versions { .. }
    | Verb::ChangedSince { .. }
    | Verb::Work { .. }
    | Verb::Edit { .. }
    | Verb::Submit { .. }
    | Verb::Rebase { .. } => serve_merge(client, verb)?,
    Verb::Placed {
      volume,
      snapshot,
      scope,
    } => {
      let (placed, mirror_age_ns) = client.await_placed(*volume, *snapshot, *scope)?;
      println!("placed: {placed}");
      println!("mirror_age_ns: {}", option_text(mirror_age_ns));
    }
    Verb::Clone {
      volume,
      snapshot,
      name,
    } => {
      let id = client.clone_snapshot(*volume, *snapshot, name)?;
      println!("id: {}", volume_id_text(id));
    }
    Verb::Resize { volume, size } => {
      client.resize(*volume, *size)?;
      println!("ok");
    }
    Verb::Destroy { volume } => {
      client.destroy(*volume)?;
      println!("ok");
    }
    Verb::Attach {
      volume,
      snapshot,
      intent,
    } => {
      let attached = client.attach(*volume, *snapshot, *intent)?;
      println!("attachment: {}", attached.attachment);
      println!("lease_epoch: {}", option_text(attached.lease_epoch));
      println!(
        "path: {}",
        attached
          .path
          .unwrap_or_else(|| "(none until a bridge exists)".to_owned())
      );
    }
    Verb::Detach { attachment } => {
      client.detach(*attachment)?;
      println!("ok");
    }
    Verb::ReadBase { volume, path } => {
      use std::io::Write;
      let bytes = client.read_base(*volume, path)?;
      let mut out = std::io::stdout().lock();
      // A closed pipe is the reader's choice, not a failure of the verb.
      let _ = out.write_all(&bytes);
      let _ = out.flush();
    }
    Verb::Rewitness { volume, paths } => {
      for path in client.rewitness(*volume, paths.clone())? {
        println!("{path}");
      }
    }
    Verb::Pin { volume, paths } => {
      println!("pinned: {}", client.pin(*volume, paths.clone())?);
    }
    Verb::Land {
      volume,
      snapshot,
      target,
      filter,
      grant,
    } => print_landing(client.land(*volume, *snapshot, target, filter.clone(), *grant)?),
    Verb::Grants => {
      for grant in client.grants()? {
        println!(
          "{} {} {} {:?} {}",
          grant.id,
          volume_id_text(grant.volume),
          grant.target,
          grant.scope,
          grant.state
        );
      }
    }
    Verb::Audit { since } => {
      for record in client.audit(*since)? {
        println!(
          "{} {} {} grant={:?} landing={:?} outcome={:?}",
          record.seq, record.at_ns, record.kind, record.grant, record.landing, record.outcome
        );
      }
    }
  }
  Ok(())
}

/// Prints a landing's outcome, or the grant it needs.
fn print_landing(landing: slates_client::Landing) {
  match landing {
    slates_client::Landing::Landed(outcome) => {
      println!("landing: {}", outcome.landing);
      println!("state: {}", outcome.state);
      println!("written: {}", outcome.written);
      println!("skipped: {}", outcome.skipped);
      println!("conflicts: {}", outcome.conflicts);
      println!("failed: {}", outcome.failed);
      println!("bytes_written: {}", outcome.bytes_written);
    }
    slates_client::Landing::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    } => {
      println!("landing: {landing}");
      println!("manifest: {}", hex32(&manifest));
      for action in &summary.by_action {
        println!("action {}: {}", action.action, action.count);
      }
      println!("bytes: {}", summary.bytes);
      println!("filtered_out: {}", summary.filtered_out);
      for path in &conflicts {
        println!("conflict: {path}");
      }
      println!("grant with: slates grant {landing}");
    }
  }
}

/// Format: a 32-byte hash printed as 64 hexadecimal characters.
fn hex32(bytes: &[u8; 32]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn option_text<T: std::fmt::Display>(value: Option<T>) -> String {
  value.map_or_else(|| "none".to_owned(), |v| v.to_string())
}

/// One volume per line: id, name, referenced bytes, unique bytes, overlay.
fn summary_line(volume: &VolumeSummary) -> String {
  format!(
    "{} {} referenced={} unique={} overlay={}",
    volume_id_text(volume.id),
    volume.name,
    volume.referenced_bytes,
    volume.unique_bytes,
    volume.overlay
  )
}

/// The status, one `key: value` per line.
fn status_text(report: &StatusReport) -> String {
  format!(
    "id: {}\nname: {}\nreferenced_bytes: {}\nunique_bytes: {}\nlease_epoch: {}\nattachments: {}\nhead: {}\nsnapshots: {}\nwatcher: {}\ndrifted: {}\n",
    volume_id_text(report.id),
    report.name,
    report.referenced_bytes,
    report.unique_bytes,
    option_text(report.lease_epoch),
    report.attachments,
    report.head.value,
    report.snapshots,
    report.watcher,
    report.drifted.len()
  ) + &format!(
    "placed: {}\nmirror_age_ns: {}\nhost_epoch: {}\n",
    report.placed.region,
    option_text(report.placed.mirror_age_ns),
    report.placed.host_epoch
  )
}

/// The daemon's status: the daemon's lines, then one block per shard.
fn daemon_status_text(report: &DaemonReport) -> String {
  let mut out = format!(
    "pid: {}\ngeneration: {}\nrestarts: {}\nheartbeat_age_ns: {}\nclients_reaped: {}\nclients_refused: {}\nshards: {}\n",
    report.pid,
    report.generation,
    report.restarts,
    report.heartbeat_age_ns,
    report.clients_reaped,
    report.clients_refused,
    report.shards.len()
  );
  for shard in &report.shards {
    out.push_str(&format!(
      "shard {}: clients={} volumes={} served={} replayed={} replay_ns={} torn={} reserve={} committed={}\n",
      shard.partition,
      shard.clients,
      shard.volumes,
      shard.served,
      shard.replayed_records,
      shard.replay_ns,
      shard.torn_tail,
      shard.reserve_bytes,
      shard.committed_bytes
    ));
    for refusal in &shard.refusals {
      out.push_str(&format!(
        "shard {} refused {}: {}\n",
        shard.partition, refusal.kind, refusal.count
      ));
    }
    for signal in &shard.signals {
      out.push_str(&format!(
        "shard {} {}: {} (age {} ns)\n",
        shard.partition, signal.name, signal.value, signal.freshness_ns
      ));
    }
  }
  out
}

/// `slates profile`: the machine profile, as its derived constants or as JSON.
pub(crate) fn profile(options: &ProfileOptions) -> Result<(), Failure> {
  let profile = crate::daemon::measure(options.quick);
  if options.json {
    let json = profile
      .to_json()
      .map_err(|e| Failure::Failed(e.to_string()))?;
    println!("{json}");
    return Ok(());
  }
  println!("{}", profile.facts.identity.line());
  println!("quick: {}", profile.quick);
  println!("elapsed_ns: {}", profile.elapsed_ns);
  let derived = profile.derived();
  println!("spin_before_park_ns: {}", derived.spin_before_park_ns.get());
  println!("arena_region_bytes: {}", derived.arena_region_bytes.get());
  println!("timer_tick_ns: {}", derived.timer_tick_ns.get());
  println!("task_step_budget_ns: {}", derived.task_step_budget_ns.get());
  println!(
    "copy_versus_remap_bytes: {}",
    derived.copy_versus_remap_bytes.get()
  );
  println!("ring_entries: {}", derived.ring_entries.get());
  Ok(())
}
