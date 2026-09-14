//! The client verbs: one connection, one request, the reply printed in a stable plain form.

use slates_client::{
  AttachRequest, AuditEntry, ChokepointReport, Client, ClientError, CreateSpec, DaemonReport,
  Deadlines, Digest, Established, GrantScope, GrantSummary, Intent, Landing, Rebased, Scope,
  SnapshotId, StatusReport, Submitted, TelemetryReport, VolumeId, VolumeSummary,
};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_server::daemon::LIVENESS_BUDGET_NS;

use crate::Failure;
use crate::args::{ClientRequest, ProfileOptions, Verb};
use crate::format::volume_id_text;

fn failure_of(e: ClientError, instance: &str) -> Failure {
  match e {
    ClientError::Refused(refusal) => Failure::Refused(format!("{refusal:?}")),
    // Both are "no daemon" to the caller (exit 3), but the cause is kept: a rendezvous that is not
    // there, a claim the daemon never answered, or a daemon that stopped answering are different
    // things for an operator to chase.
    ClientError::Ipc(e @ slates_ipc::IpcError::DaemonUnavailable { .. }) => Failure::Unavailable {
      instance: instance.to_owned(),
      cause: e.to_string(),
    },
    e @ ClientError::DaemonGone { .. } => Failure::Unavailable {
      instance: instance.to_owned(),
      cause: e.to_string(),
    },
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
  // `grant` proves the human's authority from the anchor segment this command runs under, then asks the
  // daemon to issue; its refusals are the anchor's (`Failure`), not the client's, so it is served here.
  if let Verb::Grant {
    landing,
    manifest,
    scope,
    term_ns,
  } = &request.verb
  {
    return emit_grant(
      &mut client,
      *landing,
      *manifest,
      *scope,
      *term_ns,
      request.json,
    );
  }
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
  // A container bind names its host mount point by the real path the kernel records (`mount_nfs`
  // resolves symlinks; `mktemp -d` on macOS hands out a symlinked `/var/folders` path), so the source
  // is resolved here, in the client — a read, never a write — before the daemon compares it with its
  // mount table. A path that does not exist is the command's failure, not a client error.
  if let Verb::Attach {
    volume,
    snapshot,
    intent,
    form: AttachRequest::Oci {
      source,
      destination,
    },
  } = &request.verb
  {
    let source = std::fs::canonicalize(source)
      .map_err(|e| Failure::Failed(format!("--oci-source {source}: {e}")))?
      .to_string_lossy()
      .into_owned();
    let form = AttachRequest::Oci {
      source,
      destination: destination.clone(),
    };
    return emit_attach(&mut client, *volume, *snapshot, *intent, form, request.json)
      .map_err(|e| failure_of(e, &request.instance));
  }
  let outcome = serve(&mut client, &request.verb, request.json);
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
/// `green NAME`: the new green's id, as text or a JSON `{ "id" }` under `--json`.
fn merge_green(
  client: &mut Client,
  name: &str,
  evidence: bool,
  json: bool,
) -> Result<(), ClientError> {
  let id = client.create_green(name, evidence)?;
  if json {
    println!("{}", serde_json::json!({ "id": volume_id_text(id) }));
  } else {
    println!("id: {}", volume_id_text(id));
  }
  Ok(())
}

/// `work GREEN NAME`: the work's id and the green base version, as text lines or a JSON object.
fn merge_work(
  client: &mut Client,
  green: slates_client::VolumeId,
  name: &str,
  json: bool,
) -> Result<(), ClientError> {
  let (id, base) = client.create_work(green, name)?;
  if json {
    println!(
      "{}",
      serde_json::json!({ "id": volume_id_text(id), "base": base })
    );
  } else {
    println!("id: {}", volume_id_text(id));
    println!("base: {base}");
  }
  Ok(())
}

/// `edit WORK PATH AT DELETE TEXT`: acknowledged as text `edited` or a JSON `{ "edited": true }`.
fn merge_edit(
  client: &mut Client,
  work: slates_client::VolumeId,
  path: &str,
  at: u64,
  delete_len: u64,
  bytes: &[u8],
  json: bool,
) -> Result<(), ClientError> {
  client.edit(work, path, at, delete_len, bytes)?;
  if json {
    println!("{}", serde_json::json!({ "edited": true }));
  } else {
    println!("edited");
  }
  Ok(())
}

fn serve_merge(client: &mut Client, verb: &Verb, json: bool) -> Result<(), ClientError> {
  match verb {
    Verb::Green { name, evidence } => merge_green(client, name, *evidence, json)?,
    Verb::Versions { green } => {
      let head = client.versions(*green)?;
      if json {
        println!("{}", serde_json::json!({ "head": head }));
      } else {
        println!("head: {head}");
      }
    }
    Verb::ChangedSince { green, version } => {
      let paths = client.changed_since(*green, *version)?;
      if json {
        println!("{}", serde_json::json!({ "paths": paths }));
      } else {
        for path in paths {
          println!("{path}");
        }
      }
    }
    Verb::Work { green, name } => merge_work(client, *green, name, json)?,
    Verb::Edit {
      work,
      path,
      at,
      delete_len,
      bytes,
    } => merge_edit(client, *work, path, *at, *delete_len, bytes, json)?,
    Verb::Submit { work } => {
      let outcome = client.submit(*work)?;
      if json {
        println!("{}", submit_json(&outcome));
      } else {
        match outcome {
          Submitted::Accepted(version) => println!("accepted: {version}"),
          Submitted::Conflict(windows) => print_windows(&windows),
        }
      }
    }
    Verb::Rebase { work } => {
      let outcome = client.rebase(*work)?;
      if json {
        println!("{}", rebase_json(&outcome));
      } else {
        match outcome {
          Rebased::Rebased(version) => println!("rebased: {version}"),
          Rebased::Conflict(windows) => print_windows(&windows),
        }
      }
    }
    // Only the merge verbs above reach here.
    _ => {}
  }
  Ok(())
}

/// A submit outcome as JSON (the MCP schema — `slates_mcp::windows_json` for the conflicts): `accepted`
/// with the new green `version`, or the `conflicts` windows to rebase against.
fn submit_json(outcome: &Submitted) -> serde_json::Value {
  match outcome {
    Submitted::Accepted(version) => serde_json::json!({ "accepted": true, "version": version }),
    Submitted::Conflict(windows) => {
      serde_json::json!({ "accepted": false, "conflicts": slates_mcp::windows_json(windows) })
    }
  }
}

/// A rebase outcome as JSON (the MCP schema): `rebased` with the head `version`, or the `conflicts`.
fn rebase_json(outcome: &Rebased) -> serde_json::Value {
  match outcome {
    Rebased::Rebased(version) => serde_json::json!({ "rebased": true, "version": version }),
    Rebased::Conflict(windows) => {
      serde_json::json!({ "rebased": false, "conflicts": slates_mcp::windows_json(windows) })
    }
  }
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

/// `volume list`: every volume, one per line, or a JSON array (the MCP `summary_json` schema).
fn emit_list(client: &mut Client, json: bool) -> Result<(), ClientError> {
  let volumes = client.list()?;
  if json {
    let array = serde_json::Value::Array(volumes.iter().map(slates_mcp::summary_json).collect());
    println!("{array}");
  } else {
    for volume in &volumes {
      println!("{}", summary_line(volume));
    }
  }
  Ok(())
}

/// `status` (no volume): the daemon's status as text, or JSON (the MCP `daemon_json` schema), with
/// every shard's telemetry ring drained (§4.14) — the same gather the MCP `slates.status` makes.
fn emit_daemon_status(client: &mut Client, json: bool) -> Result<(), ClientError> {
  let (report, telemetry) = slates_mcp::gather_daemon_status(client)?;
  if json {
    println!("{}", slates_mcp::daemon_json(&report, &telemetry));
  } else {
    print!("{}", daemon_status_text(&report, &telemetry));
  }
  Ok(())
}

/// `status ID` / `volume stat ID`: the report as text, its drifted paths (`--drift`), or the whole
/// report as JSON (the MCP `status_json` schema; `--drift` narrows only the text form).
fn emit_status(
  client: &mut Client,
  volume: slates_client::VolumeId,
  drift: bool,
  json: bool,
) -> Result<(), ClientError> {
  let report = client.status(volume)?;
  if json {
    println!("{}", slates_mcp::status_json(&report));
  } else if drift {
    for path in &report.drifted {
      println!("{path}");
    }
  } else {
    print!("{}", status_text(&report));
  }
  Ok(())
}

/// `volume create`: the new volume's id, as text (the id and its bridge path) or a JSON `{ "id" }`.
/// The id key matches `green`/`work`/`clone`, so a script reads `.id` from every creating verb.
fn emit_create(client: &mut Client, spec: &CreateSpec, json: bool) -> Result<(), ClientError> {
  let id = client.create(spec)?;
  if json {
    println!("{}", serde_json::json!({ "id": volume_id_text(id) }));
  } else {
    println!("id: {}", volume_id_text(id));
    println!("path: (none until a bridge exists)");
  }
  Ok(())
}

/// `volume snapshot`: the snapshot's sequence number, as text or a JSON `{ "snapshot" }` (the MCP
/// `slates.volume.snapshot` schema).
fn emit_snapshot(client: &mut Client, volume: VolumeId, json: bool) -> Result<(), ClientError> {
  let snapshot = client.snapshot(volume)?;
  if json {
    println!("{}", serde_json::json!({ "snapshot": snapshot.value }));
  } else {
    println!("snapshot: {}", snapshot.value);
  }
  Ok(())
}

/// `volume clone`: the clone's id, as text or a JSON `{ "id" }` (the id key `create`/`green`/`work`
/// all use, so a script reads `.id` from every creating verb).
fn emit_clone(
  client: &mut Client,
  volume: VolumeId,
  snapshot: SnapshotId,
  name: &str,
  json: bool,
) -> Result<(), ClientError> {
  let id = client.clone_snapshot(volume, snapshot, name)?;
  if json {
    println!("{}", serde_json::json!({ "id": volume_id_text(id) }));
  } else {
    println!("id: {}", volume_id_text(id));
  }
  Ok(())
}

/// `volume placed`: whether the scope is durable and the mirror's age, as two text lines or a JSON
/// `{ "placed", "mirror_age_ns" }` (a null `mirror_age_ns` when there is no mirror).
fn emit_placed(
  client: &mut Client,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  scope: Scope,
  json: bool,
) -> Result<(), ClientError> {
  let (placed, mirror_age_ns) = client.await_placed(volume, snapshot, scope)?;
  if json {
    println!(
      "{}",
      serde_json::json!({ "placed": placed, "mirror_age_ns": mirror_age_ns })
    );
  } else {
    println!("placed: {placed}");
    println!("mirror_age_ns: {}", option_text(mirror_age_ns));
  }
  Ok(())
}

/// `attach`: the attachment id, lease epoch, path, what was established (for a container bind, the
/// verified host mount, the policy, the evidence and the runtime's `mounts` entry) and the transport's
/// capability, as text lines or JSON (the MCP `slates_mcp::attachment_json` schema — one definition
/// for both surfaces, §4.12 parity).
fn emit_attach(
  client: &mut Client,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  intent: Intent,
  form: AttachRequest,
  json: bool,
) -> Result<(), ClientError> {
  let attached = client.attach_with(volume, snapshot, intent, form)?;
  if json {
    println!("{}", slates_mcp::attachment_json(&attached));
  } else {
    println!("attachment: {}", attached.attachment);
    println!("lease_epoch: {}", option_text(attached.lease_epoch));
    println!(
      "path: {}",
      attached
        .path
        .unwrap_or_else(|| "(none until a bridge exists)".to_owned())
    );
    print!("{}", established_text(&attached.established));
    print!("{}", capability_text(&attached.capability));
  }
  Ok(())
}

/// What an attach established, as `key: value` lines; a container bind shows its verified source,
/// destination, policy, the mount table's evidence, and the runtime's entry as one JSON line.
fn established_text(established: &Established) -> String {
  match established {
    Established::Record => "established: record\n".to_owned(),
    Established::OciBind { binding } => format!(
      "established: oci_bind\noci_source: {}\noci_destination: {}\noci_read_only: {}\noci_evidence: fstype={} source={} names_volume={}\noci_mount: {}\n",
      binding.source,
      binding.destination,
      binding.read_only,
      binding.evidence.fstype,
      binding.evidence.mount_source,
      binding.evidence.names_volume,
      slates_mcp::oci_mount_json(binding)
    ),
  }
}

/// One transport's six facts on one `transport:` line (§4.6 A-9), in the vocabulary the JSON uses.
fn capability_text(c: &slates_client::AttachmentCapability) -> String {
  format!(
    "transport: {} supported={} reason={} target={} read_write={} cache={} open_state={} delete_while_open={} residency={} conformance={}\n",
    slates_mcp::transport_name(c.transport),
    c.supported,
    c.unsupported_reason
      .map_or("none", slates_mcp::unsupported_reason_name),
    slates_mcp::target_path_name(c.target_path),
    slates_mcp::read_write_name(c.read_write),
    slates_mcp::kernel_cache_text(c.sharing.cache),
    c.sharing.server_open_state,
    slates_mcp::delete_while_open_name(c.sharing.delete_while_open),
    slates_mcp::residency_name(c.residency),
    slates_mcp::conformance_name(c.conformance),
  )
}

/// `base rewitness`: the paths whose base drifted, one per text line or a JSON `{ "paths" }` (the key
/// `changed-since` uses, so a script reads `.paths` from both).
fn emit_rewitness(
  client: &mut Client,
  volume: VolumeId,
  paths: Option<Vec<String>>,
  json: bool,
) -> Result<(), ClientError> {
  let rewitnessed = client.rewitness(volume, paths)?;
  if json {
    println!("{}", serde_json::json!({ "paths": rewitnessed }));
  } else {
    for path in rewitnessed {
      println!("{path}");
    }
  }
  Ok(())
}

/// `base pin`: the count of base entries pinned, as text or a JSON `{ "pinned" }` (the MCP
/// `slates.base.pin` schema).
fn emit_pin(
  client: &mut Client,
  volume: VolumeId,
  paths: Option<Vec<String>>,
  json: bool,
) -> Result<(), ClientError> {
  let pinned = client.pin(volume, paths)?;
  if json {
    println!("{}", serde_json::json!({ "pinned": pinned }));
  } else {
    println!("pinned: {pinned}");
  }
  Ok(())
}

/// `grants`: every grant, one per text line or a JSON array of [`grant_json`] objects (the shape a
/// script iterates, matching `volume list`'s array of objects).
fn emit_grants(client: &mut Client, json: bool) -> Result<(), ClientError> {
  let grants = client.grants()?;
  if json {
    let array = serde_json::Value::Array(grants.iter().map(grant_json).collect());
    println!("{array}");
  } else {
    for grant in &grants {
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
  Ok(())
}

/// One grant as JSON: its id, volume, target, the manifest hash it binds, its scope and its state.
fn grant_json(grant: &GrantSummary) -> serde_json::Value {
  serde_json::json!({
    "id": grant.id,
    "volume": volume_id_text(grant.volume),
    "target": grant.target,
    "manifest": hex32(&grant.manifest),
    "scope": format!("{:?}", grant.scope).to_lowercase(),
    "state": grant.state,
  })
}

/// `audit`: every record, one per text line or a JSON array of [`audit_json`] objects.
fn emit_audit(client: &mut Client, since: u64, json: bool) -> Result<(), ClientError> {
  let records = client.audit(since)?;
  if json {
    let array = serde_json::Value::Array(records.iter().map(audit_json).collect());
    println!("{array}");
  } else {
    for record in &records {
      println!(
        "{} {} {} grant={:?} landing={:?} outcome={:?}",
        record.seq, record.at_ns, record.kind, record.grant, record.landing, record.outcome
      );
    }
  }
  Ok(())
}

/// One audit record as JSON: its sequence, monotonic time, kind, the grant/landing/manifest it binds
/// (null when unbound), and the terminal outcome (null until a landing finishes).
fn audit_json(record: &AuditEntry) -> serde_json::Value {
  serde_json::json!({
    "seq": record.seq,
    "at_ns": record.at_ns,
    "kind": record.kind,
    "grant": record.grant,
    "landing": record.landing,
    "manifest": record.manifest.as_ref().map(hex32),
    "outcome": record.outcome,
  })
}

/// A landing's result, as the text form ([`print_landing`]) or JSON (the MCP `slates.land.materialize`
/// schema — `grant_required` false with the `outcome`, or true with the manifest, summary, conflicts,
/// and the `slates grant` command a human runs).
fn emit_landing(landing: Landing, json: bool) {
  if json {
    println!("{}", landing_json(&landing));
  } else {
    print_landing(landing);
  }
}

/// A landing as JSON, reusing the MCP landing serializers so the two surfaces share one schema.
fn landing_json(landing: &Landing) -> serde_json::Value {
  match landing {
    Landing::Landed(outcome) => serde_json::json!({
      "grant_required": false,
      "outcome": slates_mcp::outcome_json(outcome),
    }),
    Landing::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    } => serde_json::json!({
      "grant_required": true,
      "landing": landing,
      "manifest": hex32(manifest),
      "summary": slates_mcp::landing_summary_json(summary),
      "conflicts": conflicts,
      "grant_with": format!("slates grant {landing}"),
    }),
  }
}

/// Acknowledges an outcome-only verb: a JSON `{ "ok": true }` under `--json`, else the text `message`.
/// One shape for every verb whose success is a bare acknowledgement (resize, destroy, detach, destroy
/// a snapshot), so a script tests `.ok` uniformly.
fn emit_ok(message: &str, json: bool) {
  if json {
    println!("{}", serde_json::json!({ "ok": true }));
  } else {
    println!("{message}");
  }
}

fn serve(client: &mut Client, verb: &Verb, json: bool) -> Result<(), ClientError> {
  match verb {
    Verb::Create {
      name,
      size,
      names,
      require_locked,
      base,
    } => {
      let spec = CreateSpec {
        name: name.clone(),
        size: *size,
        names: *names,
        require_locked: *require_locked,
        base: base.clone(),
      };
      emit_create(client, &spec, json)?;
    }
    Verb::List => emit_list(client, json)?,
    Verb::DaemonStatus => emit_daemon_status(client, json)?,
    Verb::PromoteRegion { region } => {
      client.promote_region(*region)?;
      emit_ok("region promoted", json);
    }
    Verb::Status { volume, drift } => emit_status(client, *volume, *drift, json)?,
    // `mount`/`unmount` run `mount_nfs`/`umount` (CLI/OS operations whose failure is a `Failure`, not a
    // `ClientError`), so [`run`] handles them before this dispatch; they never reach here.
    Verb::Mount { .. } | Verb::Unmount { .. } => {}
    Verb::Snapshot { volume } => emit_snapshot(client, *volume, json)?,
    Verb::DestroySnapshot { volume, snapshot } => {
      client.destroy_snapshot(*volume, *snapshot)?;
      emit_ok("snapshot destroyed", json);
    }
    Verb::Green { .. }
    | Verb::Versions { .. }
    | Verb::ChangedSince { .. }
    | Verb::Work { .. }
    | Verb::Edit { .. }
    | Verb::Submit { .. }
    | Verb::Rebase { .. } => serve_merge(client, verb, json)?,
    Verb::Placed {
      volume,
      snapshot,
      scope,
    } => emit_placed(client, *volume, *snapshot, *scope, json)?,
    Verb::Clone {
      volume,
      snapshot,
      name,
    } => emit_clone(client, *volume, *snapshot, name, json)?,
    Verb::Resize { volume, size } => {
      client.resize(*volume, *size)?;
      emit_ok("ok", json);
    }
    Verb::Destroy { volume } => {
      client.destroy(*volume)?;
      emit_ok("ok", json);
    }
    Verb::Attach {
      volume,
      snapshot,
      intent,
      form,
    } => emit_attach(client, *volume, *snapshot, *intent, form.clone(), json)?,
    Verb::Detach { attachment } => {
      client.detach(*attachment)?;
      emit_ok("ok", json);
    }
    Verb::ReadBase { volume, path } => {
      use std::io::Write;
      let bytes = client.read_base(*volume, path)?;
      let mut out = std::io::stdout().lock();
      // A closed pipe is the reader's choice, not a failure of the verb. `read-base` writes raw bytes
      // (a file's content), so `--json` does not wrap them — a script redirects the stream to a file.
      let _ = out.write_all(&bytes);
      let _ = out.flush();
    }
    Verb::Digest { volume, path } => {
      let digest = client.digest(*volume, path)?;
      if json {
        println!("{}", digest_json(path, &digest));
      } else {
        println!("identity: {}", hex32(&digest.identity));
        println!("size: {}", digest.size);
      }
    }
    Verb::Rewitness { volume, paths } => emit_rewitness(client, *volume, paths.clone(), json)?,
    Verb::Pin { volume, paths } => emit_pin(client, *volume, paths.clone(), json)?,
    Verb::Land {
      volume,
      snapshot,
      target,
      filter,
      grant,
    } => emit_landing(
      client.land(*volume, *snapshot, target, filter.clone(), *grant)?,
      json,
    ),
    Verb::Grants => emit_grants(client, json)?,
    Verb::Audit { since } => emit_audit(client, *since, json)?,
    // Served in `run`, before this: its authority comes from the anchor, not the client.
    Verb::Grant { .. } => {}
  }
  Ok(())
}

/// `grant`: the human surface (§4.13 "Grants"). The command runs as the user who started the anchor and
/// attaches the anchor segment from its environment — the handoff only the supervisor's children and its
/// user's shell inherit — reads the issuer secret the daemon minted at start, proves the exact landing
/// under it, and asks the daemon to issue. With no anchor in the environment there is no authority to
/// prove, and the command refuses before asking: an agent driving the ring, the MCP server or an SDK
/// never inherits the handoff, which is what makes this the human's surface and not theirs.
fn emit_grant(
  client: &mut Client,
  landing: u64,
  manifest: [u8; 32],
  scope: GrantScope,
  term_ns: u64,
  json: bool,
) -> Result<(), Failure> {
  let secret = issuer_secret()?;
  let proof = slates_server::landing::grant_proof(&secret, landing, &manifest, scope, term_ns);
  let grant = client
    .grant(landing, manifest, scope, term_ns, proof)
    .map_err(|e| failure_of(e, "grant"))?;
  if json {
    println!("{}", serde_json::json!({ "grant": grant }));
  } else {
    println!("grant: {grant}");
  }
  Ok(())
}

/// The daemon's grant-issuer secret, read from the anchor segment this command runs under; refused
/// typed when no anchor is in the environment (the command was not run by the anchor's user from the
/// anchor's session) or the daemon has not published one yet (an all-zero secret is no authority).
fn issuer_secret() -> Result<[u8; slates_anchor::layout::ISSUER_SECRET_BYTES], Failure> {
  if std::env::var_os(slates_anchor::segment::ENV_HANDOFF).is_none() {
    return Err(Failure::Failed(
      "grant: no anchor in this environment — `slates grant` is the anchor user's surface; run it from \
       the session that started the daemon (the MCP server and the SDKs cannot issue grants)"
        .to_owned(),
    ));
  }
  let identity = slates_machine::facts::Facts::query().identity;
  let segment = slates_anchor::AnchorSegment::attach_from_env(&identity)
    .map_err(|e| Failure::Failed(format!("grant: attaching the anchor: {e}")))?;
  let secret = segment
    .issuer_secret()
    .map_err(|e| Failure::Failed(format!("grant: reading the issuer secret: {e}")))?;
  if secret.iter().all(|byte| *byte == 0) {
    return Err(Failure::Failed(
      "grant: the daemon has published no issuer secret yet".to_owned(),
    ));
  }
  Ok(secret)
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

/// A clean file's digest as JSON (the MCP `slates.base.digest` schema): the path asked for, the
/// BLAKE3 as 64 hex digits, and the length digested — the same bytes for the same content, so a
/// script can compare two exports textually.
fn digest_json(path: &str, digest: &Digest) -> serde_json::Value {
  serde_json::json!({
    "path": path,
    "identity": hex32(&digest.identity),
    "size": digest.size,
  })
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
  ) + &transports_text(&report.transports)
}

/// The host's transport report as text (§4.6 A-9): the host facts, then one line per transport.
fn transports_text(report: &slates_client::TransportReport) -> String {
  let mut text = format!(
    "os: {}\nkernel: {}\noci_runtime: {}\n",
    report.os,
    report.kernel.as_deref().unwrap_or("absent/not_stated"),
    slates_mcp::oci_runtime_text(&report.oci_runtime)
  );
  for capability in &report.capabilities {
    text.push_str(&capability_text(capability));
  }
  text
}

/// A health signal's value as text: the measured number, or `absent/<meaning>` when the signal has no
/// value (§4.14 A-9) — never a bare `0` a reader could mistake for a measured zero.
fn signal_value(signal: &slates_client::Signal) -> String {
  match signal.value {
    Some(value) => value.to_string(),
    None => format!("absent/{:?}", signal.absence).to_lowercase(),
  }
}

/// A chokepoint's activity as text (§4.14): `spans=N latest_age_ns=A` when its newest span is within
/// the horizon; otherwise `absent/<meaning>` — with the last sighting's age when there was one, stated
/// as an age, never as a live value — and whether any producer of it runs on this host.
fn chokepoint_value(point: &ChokepointReport) -> String {
  let producer = if point.expected {
    format!("producer {} expected here", point.producer)
  } else {
    format!("no producer here ({})", point.producer)
  };
  match (point.fresh, point.latest_age_ns) {
    (true, Some(age)) => format!("spans={} latest_age_ns={age}", point.spans),
    (_, Some(age)) => format!(
      "absent/{} (last seen {age} ns ago; {producer})",
      point.absence.name()
    ),
    (_, None) => format!("absent/{} (never; {producer})", point.absence.name()),
  }
}

/// One shard's telemetry drain as text (§4.14): the batch line (its window, horizon, loss markers and
/// remainder), then one line per chokepoint in roster order.
fn telemetry_text(batch: &TelemetryReport) -> String {
  let mut out = format!(
    "shard {} drain: spans={} window_ns={} horizon_ns={} shed_before={} dropped_total={} remaining={} missing_links={}\n",
    batch.partition,
    batch.spans.len(),
    batch.window_ns,
    batch.horizon_ns,
    batch.shed_before,
    batch.dropped_total,
    batch.remaining,
    batch.missing_links
  );
  for point in &batch.chokepoints {
    out.push_str(&format!(
      "shard {} span {}: {}\n",
      batch.partition,
      point.name,
      chokepoint_value(point)
    ));
  }
  out
}

/// The daemon's status: the daemon's lines, its place in the fleet (a laptop: `f` 0, itself the one
/// member, no peers probed), then one block per shard with its health signals and its telemetry drain.
fn daemon_status_text(report: &DaemonReport, telemetry: &[TelemetryReport]) -> String {
  let members: Vec<String> = report.fleet.members.iter().map(u64::to_string).collect();
  let mut out = format!(
    "pid: {}\ngeneration: {}\nrestarts: {}\nheartbeat_age_ns: {}\nclients_reaped: {}\nclients_refused: {}\nshards: {}\nfleet_host: {}\nfleet_f: {}\nfleet_host_epoch: {}\nfleet_members: {}\nfleet_peers_probed: {}\nfleet_unknown_id: {}\nfleet_inbox_full: {}\nfleet_sessions_refused: {}\nfleet_replaced: {}\n",
    report.pid,
    report.generation,
    report.restarts,
    report.heartbeat_age_ns,
    report.clients_reaped,
    report.clients_refused,
    report.shards.len(),
    report.fleet.host,
    report.fleet.f,
    report.fleet.host_epoch,
    members.join(" "),
    report.fleet.peers_probed,
    report.fleet.unknown_id,
    report.fleet.inbox_full,
    report.fleet.sessions_refused,
    report.fleet.replaced
  );
  for shard in &report.shards {
    out.push_str(&format!(
      "shard {}: clients={} volumes={} served={} replayed={} replay_ns={} torn={} mapped={} reserve={} committed={} retained={} retained_versions={} metadata={} committed_metadata={}\n",
      shard.partition,
      shard.clients,
      shard.volumes,
      shard.served,
      shard.replayed_records,
      shard.replay_ns,
      shard.torn_tail,
      shard.mapped_bytes,
      shard.reserve_bytes,
      shard.committed_bytes,
      shard.retained_bytes,
      shard.retained_versions,
      shard.metadata_bytes,
      shard.committed_metadata
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
        shard.partition,
        signal.name,
        signal_value(signal),
        signal.freshness_ns
      ));
    }
    out.push_str(&format!(
      "shard {} telemetry: spans_held={} spans_dropped={}\n",
      shard.partition, shard.spans_held, shard.spans_dropped
    ));
    if let Some(batch) = telemetry
      .iter()
      .find(|batch| batch.partition == shard.partition)
    {
      out.push_str(&telemetry_text(batch));
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

#[cfg(test)]
mod tests {
  use super::{chokepoint_value, digest_json, signal_value};
  use slates_client::{AbsenceIs, ChokepointReport, Digest, Signal};

  /// A chokepoint renders its activity when fresh, and `absent/<meaning>` when its newest span is past
  /// the horizon or it never reported — the stale age stated as a last sighting, never as a live value
  /// (§4.14 freshness, AC-0.11). Do: render a fresh, a stale and a never-reported entry. Expect:
  /// `spans=… latest_age_ns=…`, `absent/unknown (last seen … ago; …)`, and `absent/unknown (never; no
  /// producer here …)` respectively.
  #[test]
  fn a_chokepoint_renders_fresh_activity_or_typed_absence() {
    let entry =
      |spans: u64, latest_age_ns: Option<u64>, fresh: bool, expected: bool| ChokepointReport {
        name: "shard.op".to_owned(),
        dimension: "verb".to_owned(),
        spans,
        latest_age_ns,
        fresh,
        absence: AbsenceIs::Unknown,
        producer: "client verb".to_owned(),
        expected,
      };
    assert_eq!(
      chokepoint_value(&entry(3, Some(1_000), true, true)),
      "spans=3 latest_age_ns=1000"
    );
    assert_eq!(
      chokepoint_value(&entry(3, Some(20_000_000_000), false, true)),
      "absent/unknown (last seen 20000000000 ns ago; producer client verb expected here)"
    );
    assert_eq!(
      chokepoint_value(&entry(0, None, false, false)),
      "absent/unknown (never; no producer here (client verb))"
    );
  }

  /// `base digest --json` renders the MCP schema: the path, the BLAKE3 as 64 lowercase hex digits
  /// and the size; the same digest renders to the same text twice (the determinism gate at the
  /// CLI). Do: render a digest whose identity is the published BLAKE3 of the empty input. Expect:
  /// that hex string, the size, the path, and an identical second rendering.
  #[test]
  fn a_digest_renders_to_json_as_hex_identity_size_and_path() {
    /// Format: the BLAKE3 of the empty input, the published test vector for `input_len` 0.
    const EMPTY_HEX: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    let mut identity = [0u8; 32];
    for (byte, pair) in identity.iter_mut().zip(EMPTY_HEX.as_bytes().chunks(2)) {
      *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    let digest = Digest { identity, size: 0 };
    let rendered = digest_json("/src/lib.rs", &digest);
    assert_eq!(rendered["identity"], EMPTY_HEX);
    assert_eq!(rendered["size"], 0);
    assert_eq!(rendered["path"], "/src/lib.rs");
    assert_eq!(
      rendered.to_string(),
      digest_json("/src/lib.rs", &digest).to_string()
    );
  }

  /// A health signal renders its measured value, and a genuinely absent signal renders
  /// `absent/<meaning>` — never a bare `0` a reader could mistake for a measured zero (§4.14 A-9).
  /// Do: render a measured zero and an absent signal. Expect: `"0"` and `"absent/unknown"`, so a real
  /// zero and an unknown value are distinguishable in the status a human reads.
  #[test]
  fn a_signal_distinguishes_a_measured_zero_from_an_absent_value() {
    let measured_zero = Signal {
      name: "catalog.volumes".to_owned(),
      value: Some(0),
      absence: AbsenceIs::Degraded,
      freshness_ns: 0,
    };
    assert_eq!(signal_value(&measured_zero), "0");
    let absent = Signal {
      name: "mirror.age".to_owned(),
      value: None,
      absence: AbsenceIs::Unknown,
      freshness_ns: 0,
    };
    assert_eq!(signal_value(&absent), "absent/unknown");
  }
}
