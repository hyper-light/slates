
// ---------------------------------------------------------------------------------------------------------------------
// The measurer's harness for g230's landing (A-323, A-324), appended on a diagnostics branch only and never committed
// to main. Each test skips loudly unless SLATES_TEST_MEASURE=1; run in release. A fleet test node has one shard, so
// the owner shard is the control shard (shard 0) and its longest step is the pulse every test here reads.
// ---------------------------------------------------------------------------------------------------------------------

/// Format: the owner's count of owner-shard turns of an envelope wrap (`fleet::wrap_slice`, A-323).
const MEASURE_WRAP_SLICES: &str = "fleet.seal.wrap_slices";
/// Format: the owner's count of owner-shard turns of a returned holder's walk (`live_ship::reached_slice`, A-324).
const MEASURE_REACHED_SLICES: &str = "fleet.live.reached_slices";

/// Measurer: whether this run measures; prints the skip otherwise.
fn measure_enabled(what: &str) -> bool {
  if std::env::var_os("SLATES_TEST_MEASURE").is_some() {
    return true;
  }
  eprintln!("skipping {what}: set SLATES_TEST_MEASURE=1 (release)");
  false
}

/// Measurer: a number from the environment, else `default`.
fn measure_setting(name: &str, default: u64) -> u64 {
  std::env::var(name)
    .ok()
    .and_then(|value| value.parse::<u64>().ok())
    .unwrap_or(default)
}

/// Measurer: the host's load averages as the OS reports them.
fn measure_load() -> String {
  if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
    return text.trim().to_owned();
  }
  std::process::Command::new("sysctl")
    .args(["-n", "vm.loadavg"])
    .output()
    .ok()
    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    .filter(|text| !text.is_empty())
    .unwrap_or_else(|| "not reported".to_owned())
}

/// Measurer: each daemon's shards' longest single steps so far (since boot), nanoseconds.
fn measure_steps(daemons: &[Daemon]) -> Vec<Vec<(u16, u64)>> {
  daemons
    .iter()
    .map(|daemon| {
      daemon
        .shard_pulses()
        .iter()
        .map(|pulse| (pulse.shard, pulse.longest_step_ns))
        .collect()
    })
    .collect()
}

/// Measurer: prints each daemon's longest step before and after a span, and whether it rose in the span (a rise is a
/// step the span took; no rise means no step of the span was longer than the maximum before it).
fn measure_print_steps(label: &str, before: &[Vec<(u16, u64)>], after: &[Vec<(u16, u64)>]) {
  let names = ["a", "b", "c"];
  for (index, now) in after.iter().enumerate() {
    let was = before.get(index).cloned().unwrap_or_default();
    let max_before = was.iter().map(|(_, ns)| *ns).max().unwrap_or(0);
    let max_after = now.iter().map(|(_, ns)| *ns).max().unwrap_or(0);
    let name = names.get(index).copied().unwrap_or("?");
    eprintln!(
      "measure-steps {label} {name}: {} shard(s), longest step before {:.3} ms, after {:.3} ms, rose in the span: {}",
      now.len(),
      max_before as f64 / 1e6,
      max_after as f64 / 1e6,
      max_after > max_before
    );
  }
}

/// Measurer: the `fleet.*` counters of `daemon`'s control shard whose names hold `part`.
fn measure_counters(daemon: &Daemon, part: &str) -> std::collections::BTreeMap<String, u64> {
  daemon
    .fleet_refusals()
    .map(|map| {
      map
        .iter()
        .filter(|(name, _)| name.contains(part))
        .map(|(name, count)| ((*name).to_owned(), *count))
        .collect()
    })
    .unwrap_or_default()
}

/// Measurer: the counters that moved between two readings, as `name: before -> after`.
fn measure_moved(
  before: &std::collections::BTreeMap<String, u64>,
  after: &std::collections::BTreeMap<String, u64>,
) -> String {
  after
    .iter()
    .filter(|(name, count)| before.get(*name).copied().unwrap_or(0) != **count)
    .map(|(name, count)| format!("{name}: {} -> {count}", before.get(name).copied().unwrap_or(0)))
    .collect::<Vec<_>>()
    .join(", ")
}

/// Measurer: NFS WRITE `data` at `offset` (FILE_SYNC), a retry-later answer retried as a client does: the status.
fn measure_write_at(stream: &mut TcpStream, file: &[u8], offset: u64, data: &[u8], xid: u32) -> u32 {
  let mut args = Vec::new();
  common::nfs::opaque(file, &mut args);
  args.extend_from_slice(&offset.to_be_bytes());
  args.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
  args.extend_from_slice(&2u32.to_be_bytes());
  common::nfs::opaque(data, &mut args);
  common::nfs::status(&common::nfs::served_past_retry_later(
    stream, 7, &args, xid, "WRITE",
  ))
}

/// Measurer: fills `block` with bytes no other (file, block) pair repeats (splitmix64 over the pair), so no chunk of
/// the measured volume is shared with another.
fn measure_fill(block: &mut [u8], file: u64, index: u64) {
  let mut state = (file << 40) ^ index ^ 0x9e37_79b9_7f4a_7c15;
  for word in block.chunks_mut(8) {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    let bytes = z.to_le_bytes();
    let length = word.len();
    word.copy_from_slice(&bytes[..length]);
  }
}

/// Measurer (A-323 at scale, g230's landing): a volume of `SLATES_MEASURE_SEAL_MIB` MiB (default 1,024) of distinct
/// bytes, written over the owner's NFS port in files of `SLATES_MEASURE_SEAL_FILE_MIB` MiB (default 16), sealed by a
/// snapshot at the shard's own quantum (nothing injected). Prints the seal's wall time from the snapshot to placed,
/// the wrap's turns, each daemon's longest step before and after the seal, the `seal` counters that moved, the load.
/// Where the owner refuses the volume's size for its budget, the size is halved until admitted, and said.
#[test]
fn measure_a_large_volume_seals_in_turns() {
  /// Shape: each NFS write's bytes.
  const BLOCK: usize = 64 << 10;
  if !measure_enabled("the large-seal measurement") {
    return;
  }
  let mut total_mib = measure_setting("SLATES_MEASURE_SEAL_MIB", 1024);
  let file_mib = measure_setting("SLATES_MEASURE_SEAL_FILE_MIB", 16).max(1);
  let deadline = Duration::from_secs(measure_setting("SLATES_MEASURE_DEADLINE_SECS", 900));
  let _serial = serialize_fleet_tests();
  let load_start = measure_load();
  let formed_at = Instant::now();
  let (_serve_lease, daemons, _hosts) = formed_mesh(&["a", "b", "c"]);
  let formed_in = formed_at.elapsed();
  let mut client = Client::connect(daemons[0].instance());
  let mut name = String::new();
  let mut id = None;
  while id.is_none() && total_mib >= 16 {
    name = format!("measured-seal-{total_mib}");
    let limit = (total_mib + total_mib / 2 + 64) << 20;
    let created = client.call(&RequestBody::Create {
      name: name.clone(),
      size: SizeClass::Bounded { limit },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    });
    match created {
      ReplyBody::Created { id: made } => id = Some(made),
      other => {
        eprintln!("measure-seal: a volume bounded at {limit} bytes for {total_mib} MiB refused: {other:?}");
        total_mib /= 2;
      }
    }
  }
  let Some(id) = id else {
    eprintln!("measure-seal: no volume of 16 MiB or more was admitted");
    return;
  };
  let port = daemons[0].nfs_port().expect("the owner serves NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the NFS port");
  stream.set_nodelay(true).ok();
  let root = mount(&mut stream, &capability_path(&daemons[0], &name), 1);
  let files = total_mib.div_ceil(file_mib);
  let blocks_per_file = (file_mib << 20) / u64::try_from(BLOCK).unwrap();
  let mut block = vec![0u8; BLOCK];
  let mut xid = 2u32;
  let mut refused_writes = 0u64;
  let write_began = Instant::now();
  for file in 0..files {
    xid = xid.wrapping_add(1);
    let handle = create(&mut stream, &root, &format!("f{file}"), xid);
    for index in 0..blocks_per_file {
      measure_fill(&mut block, file, index);
      xid = xid.wrapping_add(1);
      let offset = index * u64::try_from(BLOCK).unwrap();
      if measure_write_at(&mut stream, &handle, offset, &block, xid) != 0 {
        refused_writes += 1;
      }
    }
  }
  let written_in = write_began.elapsed();
  let counters_before = measure_counters(&daemons[0], "seal");
  let before = measure_steps(&daemons);
  let turns_before = refusal_count(&daemons[0], MEASURE_WRAP_SLICES).unwrap_or(0);
  let load_seal = measure_load();
  let began = Instant::now();
  let sealed = client.call(&RequestBody::Snapshot { volume: id });
  let snapshot = match sealed {
    ReplyBody::Snapshotted { id, .. } => Some(id),
    other => {
      eprintln!("measure-seal: the snapshot was refused: {other:?}");
      None
    }
  };
  let snapshot_answered_in = began.elapsed();
  let mut placed = false;
  let mut polls = 0u64;
  if let Some(snapshot) = snapshot {
    while began.elapsed() < deadline {
      polls += 1;
      if matches!(
        client.call(&RequestBody::AwaitPlaced {
          volume: id,
          snapshot: Some(snapshot),
          scope: Scope::Region,
        }),
        ReplyBody::Placed { placed: true, .. }
      ) {
        placed = true;
        break;
      }
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_millis(20));
    }
  }
  let sealed_in = began.elapsed();
  let after = measure_steps(&daemons);
  let turns = refusal_count(&daemons[0], MEASURE_WRAP_SLICES)
    .unwrap_or(0)
    .saturating_sub(turns_before);
  let counters_after = measure_counters(&daemons[0], "seal");
  let load_end = measure_load();
  drop(stream);
  for daemon in daemons {
    daemon.stop();
  }
  eprintln!(
    "measure-seal: {total_mib} MiB in {files} files of {file_mib} MiB, {refused_writes} writes refused, written in {:.2} \
     s; formed in {:.2} s; the snapshot answered in {:.3} s; placed {placed} {:.3} s after the snapshot ({polls} polls); \
     wrap turns {turns}; load at start [{load_start}], at the seal [{load_seal}], at the end [{load_end}]",
    written_in.as_secs_f64(),
    formed_in.as_secs_f64(),
    snapshot_answered_in.as_secs_f64(),
    sealed_in.as_secs_f64(),
  );
  measure_print_steps("seal", &before, &after);
  eprintln!(
    "measure-seal counters on a that moved: {}",
    measure_moved(&counters_before, &counters_after)
  );
}

/// Measurer: one live-shipped volume of the return measurement, the `index`th, bounded at `limit` bytes: created,
/// live-shipped, written once at durability and its attachment let go; the refusal said where one came.
fn measure_live_volume(client: &mut Client, index: usize, limit: u64) -> Result<VolumeId, String> {
  let created = client.call(&RequestBody::Create {
    name: format!("measured-live-{index}"),
    size: SizeClass::Bounded { limit },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  });
  let ReplyBody::Created { id } = created else {
    return Err(format!("create: {created:?}"));
  };
  let attached = client.call(&RequestBody::Attach {
    volume: id,
    snapshot: None,
    intent: Intent::Write,
    form: AttachRequest::Root,
  });
  let ReplyBody::Attached { attachment, .. } = attached else {
    return Err(format!("attach: {attached:?}"));
  };
  let shipped = client.call(&RequestBody::LiveShip {
    volume: id,
    live_shipped: true,
  });
  if !matches!(shipped, ReplyBody::LiveShippingSet { live_shipped: true }) {
    return Err(format!("live-ship: {shipped:?}"));
  }
  let written = client.call(&RequestBody::FsWrite {
    volume: id,
    attachment,
    path: "/measured".to_owned(),
    bytes: vec![u8::try_from(index % 251).unwrap(); 64],
    mode: 0o644,
  });
  if !matches!(written, ReplyBody::FsDone { .. }) {
    return Err(format!("write: {written:?}"));
  }
  let detached = client.call(&RequestBody::Detach { attachment });
  if !matches!(detached, ReplyBody::Detached) {
    return Err(format!("detach: {detached:?}"));
  }
  Ok(id)
}

/// Measurer: the position of the newest entry `holder` holds of each of `volumes`' live logs as a candidate
/// (`Daemon::fleet_live_held`, read on its control shard one volume at a time: a status report of thousands of live
/// logs is larger than a test client's status capacity).
fn measure_held(holder: &Daemon, volumes: &[VolumeId]) -> Vec<Option<u64>> {
  volumes
    .iter()
    .map(|volume| holder.fleet_live_held(ObjectId(volume.bytes)).ok().flatten())
    .collect()
}

/// Measurer (A-324 at scale, g230's landing): `SLATES_MEASURE_LIVE_VOLUMES` live-shipped volumes on a (default 2,000),
/// made by `SLATES_MEASURE_LIVE_CLIENTS` clients at once (default 16; each stops at its first refusal, said), each
/// written once at durability; b settled (it holds every log, the same positions at two reads in a row); then, as
/// `a_holders_return_is_walked_in_turns_and_it_takes_every_log` does, b's control shard held for two liveness budgets
/// while one client writes the volumes again in order, one at a time, until the hold's span has passed; then b's return
/// timed from the hold's end until b holds every volume written again one entry past where it settled. Prints the
/// return's wall time, a's reached-walk turns (none means b's link was never dropped, so no walk ran), each daemon's
/// longest step before and after the hold, the `live` counters that moved, the load. b's control shard is the one
/// held, so b's longest step after is the hold itself.
#[test]
fn measure_a_holders_return_over_thousands_of_live_volumes() {
  if !measure_enabled("the return measurement") {
    return;
  }
  let wanted = usize::try_from(measure_setting("SLATES_MEASURE_LIVE_VOLUMES", 2000)).unwrap();
  let clients = usize::try_from(measure_setting("SLATES_MEASURE_LIVE_CLIENTS", 16)).unwrap().max(1);
  let limit = measure_setting("SLATES_MEASURE_LIVE_LIMIT", 64 << 10);
  let deadline = Duration::from_secs(measure_setting("SLATES_MEASURE_DEADLINE_SECS", 900));
  let _serial = serialize_fleet_tests();
  let load_start = measure_load();
  let (_serve_lease, daemons, _hosts) = formed_mesh(&["a", "b", "c"]);
  let instance = daemons[0].instance().to_owned();
  let made_began = Instant::now();
  let made: Vec<(usize, Result<VolumeId, String>)> = std::thread::scope(|scope| {
    let handles: Vec<_> = (0..clients)
      .map(|client_index| {
        let instance = instance.clone();
        scope.spawn(move || {
          let mut client = Client::connect(&instance);
          let mut out = Vec::new();
          for index in (client_index..wanted).step_by(clients) {
            let result = measure_live_volume(&mut client, index, limit);
            let refused = result.is_err();
            out.push((index, result));
            if refused {
              break;
            }
          }
          out
        })
      })
      .collect();
    handles
      .into_iter()
      .flat_map(|handle| handle.join().unwrap_or_default())
      .collect()
  });
  let made_in = made_began.elapsed();
  let mut made = made;
  made.sort_by_key(|(index, _)| *index);
  let mut volumes: Vec<VolumeId> = Vec::with_capacity(wanted);
  let mut refusals: Vec<String> = Vec::new();
  for (index, result) in made {
    match result {
      Ok(id) => volumes.push(id),
      Err(why) => refusals.push(format!("volume {index}: {why}")),
    }
  }
  let settled_began = Instant::now();
  let mut settled = measure_held(&daemons[1], &volumes);
  let mut settle_polls = 1u64;
  loop {
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(200));
    let now = measure_held(&daemons[1], &volumes);
    settle_polls += 1;
    let stable = now == settled && now.iter().all(Option::is_some);
    settled = now;
    if stable || settled_began.elapsed() >= deadline {
      break;
    }
  }
  let settled_in = settled_began.elapsed();
  let unheld = settled.iter().filter(|held| held.is_none()).count();
  let counters_before_a = measure_counters(&daemons[0], "live");
  let counters_before_b = measure_counters(&daemons[1], "live");
  let before = measure_steps(&daemons);
  let turns_before = refusal_count(&daemons[0], MEASURE_REACHED_SLICES).unwrap_or(0);
  let load_hold = measure_load();
  let hold_ns = 2 * LIVENESS_BUDGET_NS;
  let mut client = Client::connect(daemons[0].instance());
  let hold = daemons[1]
    .starve_control_shard(hold_ns)
    .expect("b's control shard is held");
  let hold_began = Instant::now();
  let mut written: Vec<usize> = Vec::new();
  let mut refused_again = 0usize;
  for (index, volume) in volumes.iter().enumerate() {
    if hold_began.elapsed() >= Duration::from_nanos(hold_ns) {
      break;
    }
    let attached = client.call(&RequestBody::Attach {
      volume: *volume,
      snapshot: None,
      intent: Intent::Write,
      form: AttachRequest::Root,
    });
    let ReplyBody::Attached { attachment, .. } = attached else {
      refused_again += 1;
      continue;
    };
    let again = client.call(&RequestBody::FsWrite {
      volume: *volume,
      attachment,
      path: "/again".to_owned(),
      bytes: vec![0xA5; 64],
      mode: 0o644,
    });
    if matches!(again, ReplyBody::FsDone { .. }) {
      written.push(index);
    } else {
      refused_again += 1;
    }
    let _ = client.call(&RequestBody::Detach { attachment });
  }
  let writes_took = hold_began.elapsed();
  let held_ns = hold.answer();
  let hold_ended = hold_began + Duration::from_nanos(*held_ns.as_ref().unwrap_or(&hold_ns));
  let mut pending: Vec<usize> = written.clone();
  let mut polls = 0u64;
  while !pending.is_empty() && hold_ended.elapsed() < deadline {
    polls += 1;
    pending.retain(|index| {
      let want = settled[*index].map(|position| position + 1);
      daemons[1].fleet_live_held(ObjectId(volumes[*index].bytes)).ok().flatten() < want
    });
    if !pending.is_empty() {
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_millis(10));
    }
  }
  let returned_in = hold_ended.elapsed();
  let after = measure_steps(&daemons);
  let turns = refusal_count(&daemons[0], MEASURE_REACHED_SLICES)
    .unwrap_or(0)
    .saturating_sub(turns_before);
  let counters_after_a = measure_counters(&daemons[0], "live");
  let counters_after_b = measure_counters(&daemons[1], "live");
  let load_end = measure_load();
  drop(client);
  for daemon in daemons {
    daemon.stop();
  }
  eprintln!(
    "measure-return: {} of {wanted} live volumes (bounded {limit} B) made by {clients} clients in {:.2} s ({} refused: \
     {}); b settled in {:.2} s ({unheld} unheld, {settle_polls} reads); b held {held_ns:?} ns; {} written again one at \
     a time in {:.3} s ({refused_again} refused); b caught up {} {:.3} s after the hold ended ({} behind, {polls} polls); \
     a's reached-walk turns {turns}; load at start [{load_start}], at the hold [{load_hold}], at the end [{load_end}]",
    volumes.len(),
    made_in.as_secs_f64(),
    refusals.len(),
    refusals.first().cloned().unwrap_or_default(),
    settled_in.as_secs_f64(),
    written.len(),
    writes_took.as_secs_f64(),
    pending.is_empty(),
    returned_in.as_secs_f64(),
    pending.len(),
  );
  measure_print_steps("return", &before, &after);
  eprintln!(
    "measure-return counters on a that moved: {}",
    measure_moved(&counters_before_a, &counters_after_a)
  );
  eprintln!(
    "measure-return counters on b that moved: {}",
    measure_moved(&counters_before_b, &counters_after_b)
  );
}
