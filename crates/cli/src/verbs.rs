//! The client verbs: one connection, one request, the reply printed in a stable plain form.

use slates_client::{
  Client, ClientError, CreateSpec, DaemonReport, Deadlines, StatusReport, VolumeSummary,
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
  let mut client = connect(&request.instance)?;
  let outcome = serve(&mut client, &request.verb);
  outcome.map_err(|e| failure_of(e, &request.instance))
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
    Verb::Snapshot { volume } => {
      println!("snapshot: {}", client.snapshot(*volume)?.value);
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
  }
  Ok(())
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
