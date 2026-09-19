//! The client verbs: one connection, one request, the reply printed in a stable plain form.

use std::ffi::{OsStr, OsString};

use slates_client::{
  AttachRequest, AuditEntry, ChokepointReport, Client, ClientError, CreateSpec, DaemonReport,
  Deadlines, Digest, Established, GrantScope, GrantSummary, GreenBase, Intent, Landing, Principal,
  Rebased, Rights, Scope, SnapshotId, StatusReport, Submitted, TelemetryReport, VolumeId,
  VolumeSummary,
};
use slates_db::replay::RECOVERY_BUDGET_NS;
use slates_ipc::delivery::{Delivery, Output};
use slates_ipc::rendezvous::ENV_ENDPOINT;
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::landing::{enroll_proof, revoke_proof};

use crate::Failure;
use crate::args::{ClientRequest, ProfileOptions, RunRequest, SharePrincipal, Verb};
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

fn emit_recovery_plan(
  client: &mut Client,
  root: bool,
  target: Option<[u8; 32]>,
  json: bool,
) -> Result<(), Failure> {
  use slates_ipc::protocol::{ReplyBody, RequestBody};
  let reply = client
    .call(&RequestBody::RecoveryPlan { root, target })
    .map_err(|error| failure_of(error, "recovery-plan"))?;
  let ReplyBody::RecoveryPlan { plan } = reply else {
    return Err(Failure::Refused(format!(
      "recovery-plan refused: {reply:?}"
    )));
  };
  if json {
    println!(
      "{}",
      serde_json::json!({
        "member": plan.member, "previous_group": hex32(&plan.previous), "plan": hex32(&plan.digest),
        "committed": plan.committed, "last_log": plan.last_log, "version": plan.version,
        "voters": plan.voters, "target": plan.target.as_ref().map(hex32),
        "fencing_required": true, "data_loss_possible": true,
      })
    );
  } else {
    println!(
      "plan {}\nmember {}\nprevious group {}\ncommitted position {}\nlast log position {}\nconfiguration version {}\nknown former voters {:?}",
      hex32(&plan.digest),
      plan.member,
      hex32(&plan.previous),
      plan.committed,
      plan.last_log,
      plan.version,
      plan.voters
    );
    if let Some(target) = plan.target {
      println!("join group {}", hex32(&target));
    }
    println!(
      "Compare the retained copies before choosing one. Fence the entire former group before recovery; unreachable copies may contain newer committed state. Confirm only after accepting that possible loss."
    );
  }
  Ok(())
}

fn emit_recovery(
  client: &mut Client,
  root: bool,
  target: Option<[u8; 32]>,
  plan: [u8; 32],
  json: bool,
) -> Result<(), Failure> {
  use slates_ipc::protocol::{ReplyBody, RequestBody};
  let proof = match crate::recovery_key::load()? {
    Some(key) => key.proof(&plan),
    None => slates_server::recovery_proof(&issuer_secret()?, &plan),
  };
  let reply = client
    .call(&RequestBody::Recover {
      root,
      target,
      plan,
      proof,
    })
    .map_err(|error| failure_of(error, "recover"))?;
  let ReplyBody::RecoveryStarted { group, joining } = reply else {
    return Err(Failure::Refused(format!("recovery refused: {reply:?}")));
  };
  if json {
    println!(
      "{}",
      serde_json::json!({"group": hex32(&group), "joining": joining})
    );
  } else {
    println!(
      "{} {}",
      if joining {
        "joining recovery group"
      } else {
        "created recovery group"
      },
      hex32(&group)
    );
  }
  Ok(())
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
  // `grant`, `enroll` and `revoke` prove the human's authority from the anchor segment this command runs
  // under, then ask the daemon; their refusals are the anchor's (`Failure`), not the client's, so they
  // are served here.
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
  if let Verb::RecoveryPlan { root, target } = request.verb {
    return emit_recovery_plan(&mut client, root, target, request.json);
  }
  if let Verb::Recover { root, target, plan } = request.verb {
    return emit_recovery(&mut client, root, target, plan, request.json);
  }
  if let Verb::Bootstrap { root } = request.verb {
    let member = client
      .daemon_status()
      .map_err(|error| failure_of(error, "bootstrap status"))?
      .fleet
      .host;
    let reply = client
      .call(&slates_ipc::protocol::RequestBody::Bootstrap { root, member })
      .map_err(|error| failure_of(error, "bootstrap"))?;
    return match reply {
      slates_ipc::protocol::ReplyBody::Acknowledged => {
        println!(
          "{}",
          if request.json {
            "{\"bootstrapped\":true}"
          } else {
            "bootstrapped"
          }
        );
        Ok(())
      }
      _ => Err(Failure::Refused(format!("bootstrap refused: {reply:?}"))),
    };
  }
  if let Verb::Enroll { account } = &request.verb {
    return emit_enroll(&mut client, *account, request.json);
  }
  if let Verb::Revoke { consumer } = &request.verb {
    return emit_revoke(&mut client, *consumer, request.json);
  }
  // `mount` reads the volume's name and the daemon's NFS port through the client, **attaches** for the
  // host mount — the access-list-checked `attach` returns the mount capability the loopback edge
  // authorizes every request's file handle against (§4.13; AUD-01: a supplied uid and loopback
  // reachability are not authority, the capability is) — then runs `mount_nfs` to mount it over the
  // loopback NFS bridge (§4.6) with that capability in the export path: no privilege, no kernel
  // extension, no Apple entitlement. Its failure is a `Failure` (a mount refusal, not a client error),
  // so it is handled here rather than in [`serve`].
  if let Verb::Mount {
    volume,
    path,
    read_only,
  } = &request.verb
  {
    let report = client
      .status(*volume)
      .map_err(|e| failure_of(e, &request.instance))?;
    // The mount's own attachment (§4.6, §4.13): it outlives this process and a daemon restart, and
    // ends with the kernel's `UMNT` when the mount is removed. A write mount takes the write lease
    // (D-16); `--read-only` takes none and gets a read-only capability.
    let intent = if *read_only {
      Intent::Read
    } else {
      Intent::Write
    };
    let attachment = client
      .attach_mount(*volume, intent)
      .map_err(|e| failure_of(e, &request.instance))?;
    let Some(token) = attachment.token else {
      return Err(Failure::Refused(
        "the daemon issued no mount capability for this attachment; the volume cannot be mounted"
          .to_owned(),
      ));
    };
    let mounted =
      crate::mount::establish(&report, (attachment.attachment, token), path, *read_only)?;
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
/// `green NAME [--base VOLUME --snapshot N]`: the new green's id, as text or a JSON `{ "id" }` under
/// `--json`.
fn merge_green(
  client: &mut Client,
  name: &str,
  evidence: bool,
  base: Option<GreenBase>,
  json: bool,
) -> Result<(), ClientError> {
  let id = match base {
    None => client.create_green(name, evidence)?,
    Some(base) => client.create_green_over(name, evidence, base)?,
  };
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

/// `advance ATTACHMENT [VERSION]`: the version now pinned and the invalidated paths, as text lines or
/// a JSON `{ "version", "invalidated" }`.
fn merge_advance(
  client: &mut Client,
  attachment: u64,
  version: Option<u64>,
  json: bool,
) -> Result<(), ClientError> {
  let advanced = client.advance(attachment, version)?;
  if json {
    println!(
      "{}",
      serde_json::json!({ "version": advanced.version, "invalidated": advanced.invalidated })
    );
  } else {
    println!("version: {}", advanced.version);
    for path in advanced.invalidated {
      println!("{path}");
    }
  }
  Ok(())
}

fn serve_merge(client: &mut Client, verb: &Verb, json: bool) -> Result<(), ClientError> {
  match verb {
    Verb::Green {
      name,
      evidence,
      base,
    } => merge_green(client, name, *evidence, *base, json)?,
    Verb::Advance {
      attachment,
      version,
    } => merge_advance(client, *attachment, *version, json)?,
    Verb::Read { volume, path, at } => {
      use std::io::Write;
      // Raw bytes, like `base read`: a file's content streams unwrapped, `--json` or not.
      let bytes = client.read(*volume, path, *at)?;
      let mut out = std::io::stdout().lock();
      let _ = out.write_all(&bytes);
      let _ = out.flush();
    }
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
    Verb::Submit { work, evidence } => {
      let outcome = client.submit_with_evidence(*work, evidence)?;
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
    slates_mcp::residency_text(c.residency),
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
    | Verb::Rebase { .. }
    | Verb::Advance { .. }
    | Verb::Read { .. } => serve_merge(client, verb, json)?,
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
    Verb::Share {
      volume,
      principal,
      rights,
    } => emit_share(client, *volume, principal, *rights, json)?,
    // Served in `run`, before this: their authority comes from the anchor, not the client.
    Verb::Bootstrap { .. }
    | Verb::RecoveryPlan { .. }
    | Verb::Recover { .. }
    | Verb::Grant { .. }
    | Verb::Enroll { .. }
    | Verb::Revoke { .. } => {}
  }
  Ok(())
}

/// This process's host account — the account an enrollment defaults to and a `consumer:N` principal
/// is under: the uid.
#[cfg(unix)]
fn current_account() -> u32 {
  rustix::process::getuid().as_raw()
}

/// On Windows the rendezvous authenticates by the section's DACL and reports the peer's account as
/// zero (`slates_ipc::rendezvous`), so the same account is named here.
#[cfg(not(unix))]
fn current_account() -> u32 {
  0
}

/// `enroll`: the human surface enrolls a consumer under the account (§4.13 "Principals"), proving
/// issuer authority as `grant` does. The capability is shown once, here, and never again: a harness
/// delivers it to the workload on an inherited descriptor (`slates run` does both in one step).
fn emit_enroll(client: &mut Client, account: Option<u32>, json: bool) -> Result<(), Failure> {
  let secret = issuer_secret()?;
  let account = account.unwrap_or_else(current_account);
  let (consumer, capability) = client
    .enroll(account, enroll_proof(&secret, account))
    .map_err(|e| failure_of(e, "enroll"))?;
  if json {
    println!(
      "{}",
      serde_json::json!({ "consumer": consumer, "account": account, "capability": hex32(&capability) })
    );
  } else {
    println!("consumer: {consumer}");
    println!("account: {account}");
    println!("capability: {}", hex32(&capability));
  }
  Ok(())
}

/// `revoke CONSUMER`: the human surface revokes an enrollment; every later verb from a channel bound
/// to it refuses `ConsumerRevoked`.
fn emit_revoke(client: &mut Client, consumer: u64, json: bool) -> Result<(), Failure> {
  let secret = issuer_secret()?;
  client
    .revoke(consumer, revoke_proof(&secret, consumer))
    .map_err(|e| failure_of(e, "revoke"))?;
  emit_ok("revoked", json);
  Ok(())
}

/// `share ID PRINCIPAL [--read] [--write] [--admin]`: a principal's rights on a volume (§4.13
/// "Access lists"); a `consumer:N` principal without an account is under this process's.
fn emit_share(
  client: &mut Client,
  volume: VolumeId,
  principal: &SharePrincipal,
  rights: Rights,
  json: bool,
) -> Result<(), ClientError> {
  let principal = match principal {
    SharePrincipal::Uid(uid) => Principal::Uid { uid: *uid },
    SharePrincipal::Consumer { account, consumer } => Principal::Consumer {
      account: account.unwrap_or_else(current_account),
      consumer: *consumer,
    },
  };
  client.share(volume, principal, rights)?;
  emit_ok("shared", json);
  Ok(())
}

/// Shape: the bytes of a captured workload output the harness verb holds when a caller captures it
/// (the tests; `run` itself hands the workload the terminal): a few tagged lines, far under this.
const WORKLOAD_OUTPUT_CAP: usize = 1 << 20;

/// What `run` spawns: the command, the instance its client will find, whether the enrollment
/// outlives it, and where its output goes.
struct Workload {
  program: OsString,
  args: Vec<OsString>,
  instance: String,
  keep: bool,
  output: Output,
}

/// What a spawned workload came to: the consumer it ran as, its exit, its captured output.
struct Ran {
  consumer: u64,
  exit: Option<i32>,
  output: Vec<u8>,
}

/// The harness verb's core (§4.13 "the harness owns process isolation and capability delivery"):
/// enroll a consumer under `account` with the issuer's proof, deliver its capability to the workload
/// on an inherited descriptor — the capability is never printed, never in an argument, never in the
/// environment — announce the consumer, spawn the workload with `SLATES_ENDPOINT` naming the
/// instance, wait for it, and revoke the consumer unless the enrollment is kept. Volumes the workload
/// created stay owned by the consumer; a kept enrollment is the human's to `revoke` later.
fn spawn_as_ephemeral_consumer(
  client: &mut Client,
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  account: u32,
  workload: &Workload,
  extra_environment: &[(&OsStr, &OsStr)],
  announce: impl FnOnce(u64),
) -> Result<Ran, Failure> {
  let (consumer, capability) = client
    .enroll(account, enroll_proof(secret, account))
    .map_err(|e| failure_of(e, "run"))?;
  let delivery = Delivery::prepare(consumer, &capability)
    .map_err(|e| Failure::Failed(format!("run: preparing the delivery: {e}")))?;
  announce(consumer);
  let endpoint = OsString::from(&workload.instance);
  let mut environment: Vec<(&OsStr, &OsStr)> =
    vec![(OsStr::new(ENV_ENDPOINT), endpoint.as_os_str())];
  environment.extend_from_slice(extra_environment);
  let mut child = delivery
    .spawn(
      &workload.program,
      &workload.args,
      &environment,
      workload.output,
    )
    .map_err(|e| {
      Failure::Failed(format!(
        "run: spawning {}: {e}",
        workload.program.to_string_lossy()
      ))
    })?;
  let (exit, output) = child
    .wait_with_output(WORKLOAD_OUTPUT_CAP)
    .map_err(|e| Failure::Failed(format!("run: waiting for the workload: {e}")))?;
  if !workload.keep {
    client
      .revoke(consumer, revoke_proof(secret, consumer))
      .map_err(|e| failure_of(e, "run"))?;
  }
  Ok(Ran {
    consumer,
    exit,
    output,
  })
}

/// Prints the consumer a workload runs as, before the workload starts, so a human can `share`
/// volumes with it while it runs: a JSON object or a `consumer:` line, flushed ahead of the
/// workload's own output on the same stream.
fn announce_consumer(consumer: u64, json: bool) {
  use std::io::Write;
  if json {
    println!("{}", serde_json::json!({ "consumer": consumer }));
  } else {
    println!("consumer: {consumer}");
  }
  let _ = std::io::stdout().flush();
}

/// `run [--keep] -- CMD [ARG ...]`: the harness verb of §4.13. The human's authority comes from the
/// anchor segment this command runs under (as `grant` does); the command runs as a consumer enrolled
/// for its lifetime, with the capability delivered on an inherited descriptor and nowhere else; its
/// standard streams are this command's; its exit code is this command's.
pub(crate) fn run_consumer(request: &RunRequest) -> Result<(), Failure> {
  let secret = issuer_secret()?;
  let mut client = connect(&request.instance)?;
  let (program, args) = request
    .command
    .split_first()
    .ok_or_else(|| Failure::Failed("run needs a command".to_owned()))?;
  let workload = Workload {
    program: OsString::from(program),
    args: args.iter().map(OsString::from).collect(),
    instance: request.instance.clone(),
    keep: request.keep,
    output: Output::Inherit,
  };
  let ran = spawn_as_ephemeral_consumer(
    &mut client,
    &secret,
    current_account(),
    &workload,
    &[],
    |consumer| announce_consumer(consumer, request.json),
  )?;
  // A captured output would be the caller's to hand on; the terminal was the workload's, so this is
  // empty here and printed as is.
  let _ = std::io::Write::write_all(&mut std::io::stdout(), &ran.output);
  match ran.exit {
    Some(0) => Ok(()),
    Some(code) => Err(Failure::ChildExited { code }),
    None => Err(Failure::Failed(format!(
      "run: the command (consumer {}) was ended by a signal",
      ran.consumer
    ))),
  }
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

/// The daemon's issuer secret, read from the anchor segment this command runs under — the authority of
/// `grant`, `enroll`, `revoke` and `run`; refused typed when no anchor is in the environment (the
/// command was not run by the anchor's user from the anchor's session: the anchor prints the
/// variables to export on macOS and Windows; on Linux only its children hold the descriptor) or the
/// daemon has not published one yet (an all-zero secret is no authority).
fn issuer_secret() -> Result<[u8; slates_anchor::layout::ISSUER_SECRET_BYTES], Failure> {
  if std::env::var_os(slates_anchor::segment::ENV_HANDOFF).is_none() {
    return Err(Failure::Failed(
      "no anchor in this environment — recover, grant, enroll, revoke and run are the anchor user's surface; \
       run them from the session that started the daemon, with the variables `slates anchor` printed \
       exported (the MCP server and the SDKs cannot issue grants or enrollments)"
        .to_owned(),
    ));
  }
  let identity = slates_machine::facts::Facts::query().identity;
  let segment = slates_anchor::AnchorSegment::attach_from_env(&identity)
    .map_err(|e| Failure::Failed(format!("attaching the anchor: {e}")))?;
  let secret = segment
    .issuer_secret()
    .map_err(|e| Failure::Failed(format!("reading the issuer secret: {e}")))?;
  if secret.iter().all(|byte| *byte == 0) {
    return Err(Failure::Failed(
      "the daemon has published no issuer secret yet".to_owned(),
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

/// A consensus group's lines of the status (§4.8): `fleet_<group>_leads`, the derived election timing in
/// coordinator periods, the measured tail and spread it came from, and the samples behind it.
fn group_text(group: &str, report: &slates_client::GroupReport) -> String {
  format!(
    "fleet_{group}_leads: {}\nfleet_{group}_base_periods: {}\nfleet_{group}_span_periods: {}\nfleet_{group}_rtt_tail_ns: {}\nfleet_{group}_rtt_spread_ns: {}\nfleet_{group}_samples: {}\n",
    report.leads,
    report.base_periods,
    report.span_periods,
    report.rtt_tail_ns,
    report.rtt_spread_ns,
    report.samples
  )
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
  out.push_str(&group_text("council", &report.fleet.council));
  out.push_str(&group_text("root", &report.fleet.root));
  for shard in &report.shards {
    out.push_str(&format!(
      "shard {}: clients={} volumes={} served={} replayed={} replay_ns={} torn={} mapped={} reserve={} committed={} retained={} retained_versions={} metadata={} committed_metadata={} tasks_refused={}\n",
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
      shard.committed_metadata,
      shard.tasks_refused
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
mod harness_tests {
  //! The harness verb's core against an in-process daemon, with this test binary re-invoked as the
  //! workload (the cross-process fixture pattern of `crates/anchor/tests/anchor.rs`).
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

  use std::ffi::{OsStr, OsString};
  use std::time::{Duration, Instant};

  use slates_client::{
    Client, ClientError, CreateSpec, Deadlines, NamePolicy, Refusal, SizeClass, VolumeId,
  };
  use slates_ipc::delivery::Output;
  use slates_server::{Daemon, DaemonConfig, SegmentSource};

  use super::{Workload, current_account, hex32, spawn_as_ephemeral_consumer};
  use crate::format::{parse_volume_id, volume_id_text};

  /// Format: the environment variable that turns this test binary into the workload the core spawns.
  const WORKLOAD_ROLE: &str = "SLATES_CLI_TEST_WORKLOAD";
  /// Shape: how long a client retries the rendezvous while the daemon starts.
  const START_WAIT: Duration = Duration::from_secs(5);
  /// Shape: the reply deadline of the test clients (nanoseconds): a fifth of a second.
  const REPLY_NS: u64 = 200_000_000;
  /// Shape: the reconnect budget of the test clients (nanoseconds): five seconds.
  const RECONNECT_NS: u64 = 5_000_000_000;
  /// Shape: shards per test daemon: two, so the consumer's record and the attesting channel differ.
  const TEST_SHARDS: u16 = 2;

  fn deadlines() -> Deadlines {
    Deadlines {
      reply_ns: REPLY_NS,
      reconnect_ns: RECONNECT_NS,
    }
  }

  fn connect_retrying(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match Client::connect(instance, deadlines()) {
        Ok(client) => return client,
        Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
          if started.elapsed() < START_WAIT =>
        {
          std::hint::spin_loop();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  fn unhex32(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
      *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap();
    }
    out
  }

  fn line_after<'a>(text: &'a str, tag: &str) -> &'a str {
    text
      .lines()
      .find_map(|line| line.strip_prefix(tag))
      .unwrap_or_else(|| panic!("no `{tag}` line in:\n{text}"))
  }

  /// The workload: `Client::connect` binds it to the consumer delivered on its inherited descriptor;
  /// it prints its consumer, the capability (to the captured pipe the parent reads — the parent
  /// proves the revocation with it) and a volume it made. Ignored so `cargo test` never runs it in
  /// process; the parent runs it with `--ignored --exact`.
  #[test]
  #[ignore = "the workload child; run by run_spawns_the_workload_... with --ignored"]
  fn consumer_workload_child() {
    // The role variable carries the name of the volume to make (names are unique per daemon, so
    // each run makes its own).
    let Ok(name) = std::env::var(WORKLOAD_ROLE) else {
      return;
    };
    let instance = slates_ipc::instance_from_env();
    let mut client = Client::connect(&instance, deadlines()).unwrap();
    let consumer = client.consumer().expect("spawned as a consumer");
    let taken = slates_ipc::delivery::delivered().unwrap();
    let volume = client
      .create(&CreateSpec {
        name,
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    println!("consumer: {consumer}");
    println!("capability: {}", hex32(&taken.capability));
    println!("volume: {}", volume_id_text(volume));
    std::process::exit(0);
  }

  fn workload(instance: &str, keep: bool) -> Workload {
    Workload {
      program: std::env::current_exe().unwrap().into_os_string(),
      args: [
        "--ignored",
        "--exact",
        "verbs::harness_tests::consumer_workload_child",
        "--nocapture",
      ]
      .into_iter()
      .map(OsString::from)
      .collect(),
      instance: instance.to_owned(),
      keep,
      output: Output::Captured,
    }
  }

  /// Runs the core once and returns the consumer it announced, the workload's report, and the volume
  /// the workload made, after checking its exit and that it ran as the announced consumer.
  fn run_once(
    client: &mut Client,
    secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
    instance: &str,
    keep: bool,
  ) -> (u64, [u8; 32], VolumeId) {
    let mut announced = None;
    let ran = spawn_as_ephemeral_consumer(
      client,
      secret,
      current_account(),
      &workload(instance, keep),
      &[(
        OsStr::new(WORKLOAD_ROLE),
        OsStr::new(if keep { "kept" } else { "mine" }),
      )],
      |consumer| announced = Some(consumer),
    )
    .unwrap();
    let text = String::from_utf8_lossy(&ran.output).into_owned();
    assert_eq!(ran.exit, Some(0), "the workload:\n{text}");
    assert_eq!(announced, Some(ran.consumer), "announced before the spawn");
    assert_eq!(
      line_after(&text, "consumer: ").parse::<u64>().unwrap(),
      ran.consumer,
      "the workload ran as the consumer the core enrolled"
    );
    let capability = unhex32(line_after(&text, "capability: "));
    let volume = parse_volume_id(line_after(&text, "volume: ")).unwrap();
    (ran.consumer, capability, volume)
  }

  /// AC-2.13 / T-2.15 through the harness verb's core (§4.12 CLI, §4.13 "the harness owns process
  /// isolation and capability delivery"): the core enrolls a consumer, delivers its capability to the
  /// workload — this binary re-invoked — on the one inherited descriptor, announces it, waits, and
  /// revokes it: the workload ran as that consumer (its own report), the volume it made is the
  /// consumer's (the account is refused `Forbidden` on it), and the enrollment is gone afterwards (a
  /// fresh client attesting with the workload's capability is refused `ConsumerRevoked`); with the
  /// enrollment kept, the same attest binds.
  #[test]
  fn run_spawns_the_workload_as_an_ephemeral_consumer_and_revokes_it_after() {
    let profile = crate::daemon::measure(true);
    let instance = format!("cli-run-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance).with_shards(TEST_SHARDS);
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: "slates-seg-cli-run".to_owned(),
      },
    )
    .unwrap();
    daemon
      .bootstrap(true)
      .expect("the fixture explicitly creates its local consensus group");
    let secret = daemon.segment().issuer_secret().unwrap();
    let mut human = connect_retrying(&instance);

    let (consumer, capability, volume) = run_once(&mut human, &secret, &instance, false);
    assert!(
      matches!(
        human.status(volume),
        Err(ClientError::Refused(Refusal::Forbidden { .. }))
      ),
      "the workload's volume is the consumer's, not the account's"
    );
    let mut later = connect_retrying(&instance);
    assert!(
      matches!(
        later.attest(consumer, &capability),
        Err(ClientError::Refused(Refusal::ConsumerRevoked))
      ),
      "the ephemeral enrollment is revoked once the workload ends"
    );

    let (kept, capability, _) = run_once(&mut human, &secret, &instance, true);
    assert_ne!(kept, consumer, "a fresh enrollment per run");
    later
      .attest(kept, &capability)
      .expect("a kept enrollment outlives the workload");
    assert_eq!(later.consumer(), Some(kept));
    daemon.stop();
  }
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
