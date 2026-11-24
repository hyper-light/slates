//! The memory status's gauges (main 01a20182's ruling of 2026-11-22 03:46, the 787 MiB a landed git push kept): what each
//! holder of a shard's memory holds, by the daemon's own accounting, so a resident set is read against what holds it,
//! never guessed from the OS's map. One mechanism for every measurement (w-svn's import uses the same rows).
//!
//! **Rows.** Each `mem.gauge.<holder>.<measure>`, reported only in a status that asks for memory and only where non-zero:
//! - `vcs_content.*`: the repositories' content store, its entries by where their bytes are (windows the window storage
//!   keeps, windows let go, windows, lists, tables and derived nodes in its own memory), the objects it names, and its
//!   budget's use and charges (`VcsStores::memory_rows`).
//! - `homes.*`: what the shard's homes hold beside the content store (`vcs_store::homes::held`): its region holds, the
//!   windows of the objects they name (each a landed window's pin), its landed records and their bytes as the image
//!   keeps them.
//! - `git.*`: over the git repositories, each one's read scratch's room, the objects it read lately, its maps' entries,
//!   its charge on the content store, and its greens' map heads.
//! - `greens.*`: over the shard's greens, the merge engine's retained bytes ([`slates_merge::engine::RetainedBytes`]):
//!   current files, history, change index, rejected results.
//! - `held.*`: the shard's content hold's charge and objects (inputs, checkpoints, replicas), and its chains' entries and
//!   bytes.
//! - `content.*`: the shard's chunk arena over the content object: the bytes its regions map, their free bytes, the
//!   freed bytes zeroed in place an idle purge would give back, the frees held for a committed image (A-64), and the
//!   regions' pages in RAM (`ChunkArena::residency`). Beside them the counter `content.purge_kept_short_bytes`: free
//!   runs a purge left resident for being shorter than the discard size. A content object resident past what the arena
//!   holds is told apart by them: frees not yet purged (an idle pass not yet run), short runs kept, or frees deferred.
//! - `process.*` (the control shard alone, the process's figures once): the system allocator's live bytes and the bytes
//!   it ever allocated (its churn), and the
//!   mapped blocks live and their resident bytes (`slates_mem::large::heap`). A resident set past the live bytes and
//!   the mapped blocks' resident pages is what the system allocator kept of freed blocks. On Linux with glibc (absent
//!   elsewhere), glibc's own figures beside them (`slates_mem::heap::malloc`, summed over every arena, as glibc 2.39's
//!   `malloc.c` reads them): `malloc_free_bytes`, the free bytes it keeps in its arenas (`fordblks`, its fastbins'
//!   `fsmblks` already among them, so not added again), measuring that kept memory where elsewhere it is the remainder;
//!   `malloc_mapped_bytes`, its blocks mapped each on its own (`hblkhd`); `malloc_arena_bytes`, what its arenas hold from
//!   the OS (`arena`).

use crate::state::ShardState;
use slates_ipc::protocol::RefusalCount;

/// The gauges' rows for this shard (module docs), none unless `memory`.
pub(crate) fn rows(state: &ShardState, memory: bool) -> Vec<RefusalCount> {
  if !memory {
    return Vec::new();
  }
  let mut rows = state.vcs_stores.memory_rows();
  rows.extend(green_rows(state));
  rows.extend(held_rows(state));
  rows.extend(content_rows(state));
  if crate::verbs::is_control_shard(state) {
    let heap = slates_mem::large::heap();
    rows.extend([
      ("mem.gauge.process.system_live_bytes", heap.system_live),
      (
        "mem.gauge.process.system_allocated_bytes",
        heap.system_allocated,
      ),
      ("mem.gauge.process.mapped_blocks", heap.mapped_blocks),
      (
        "mem.gauge.process.mapped_resident_bytes",
        heap.mapped_resident,
      ),
      ("mem.gauge.process.mapped_untracked", heap.untracked),
    ]);
    #[cfg(all(target_os = "linux", target_env = "gnu", not(miri)))]
    {
      let malloc = slates_mem::heap::malloc();
      rows.extend([
        ("mem.gauge.process.malloc_free_bytes", malloc.free),
        ("mem.gauge.process.malloc_mapped_bytes", malloc.mapped),
        ("mem.gauge.process.malloc_arena_bytes", malloc.arena),
      ]);
    }
  }
  rows
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(kind, count)| RefusalCount {
      kind: kind.to_owned(),
      count,
    })
    .collect()
}

/// The shard's chunk arena over the content object (main 01a20182's ruling of 2026-11-23: arena-12 found the content
/// object 753 MiB resident against 183 MB the content store used): the regions it maps, their free bytes, the freed
/// bytes zeroed in place that an idle purge would give back, the frees held for a committed image (A-64), and the
/// regions' pages in RAM, read from the page table as the large path's `mapped_resident` is.
///
/// And what each owner holds there (goal 93, main 01a20182's order on arena-13's 373 MB no gauge named;
/// `slates_mem::Owner`): its blocks' bytes, and the bytes it asked for, whose difference is the buddy's rounding. The
/// owners' held bytes and the deferred bytes sum to the arena's allocated bytes, mapped less free, by construction.
fn content_rows(state: &ShardState) -> [(&'static str, u64); 11] {
  let arena = state.store.content.arena();
  let bytes = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
  let volume = arena.owned(slates_mem::Owner::Volume);
  let windows = arena.owned(slates_mem::Owner::VcsWindows);
  let hold = arena.owned(slates_mem::Owner::ContentHold);
  [
    ("mem.gauge.content.arena_volume_bytes", volume.held),
    ("mem.gauge.content.arena_volume_asked_bytes", volume.asked),
    ("mem.gauge.content.arena_vcs_windows_bytes", windows.held),
    (
      "mem.gauge.content.arena_vcs_windows_asked_bytes",
      windows.asked,
    ),
    ("mem.gauge.content.arena_content_hold_bytes", hold.held),
    (
      "mem.gauge.content.arena_content_hold_asked_bytes",
      hold.asked,
    ),
    (
      "mem.gauge.content.arena_mapped_bytes",
      bytes(arena.mapped_bytes()),
    ),
    (
      "mem.gauge.content.arena_free_bytes",
      bytes(arena.free_bytes()),
    ),
    (
      "mem.gauge.content.arena_resident_free_bytes",
      bytes(arena.resident_free_bytes()),
    ),
    (
      "mem.gauge.content.arena_deferred_bytes",
      bytes(arena.deferred_bytes()),
    ),
    (
      "mem.gauge.content.object_resident_bytes",
      arena.residency().resident,
    ),
  ]
}

/// The greens' retained bytes, summed over the shard's greens.
fn green_rows(state: &ShardState) -> [(&'static str, u64); 5] {
  let (mut greens, mut content, mut history, mut changes, mut rejected) =
    (0u64, 0u64, 0u64, 0u64, 0u64);
  for (_, green) in state.greens.iter() {
    let retained = green.retained_bytes();
    let add = |total: &mut u64, bytes: usize| {
      *total = total.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    };
    greens = greens.saturating_add(1);
    add(&mut content, retained.content);
    add(&mut history, retained.history);
    add(&mut changes, retained.changes);
    add(&mut rejected, retained.rejected);
  }
  [
    ("mem.gauge.greens.count", greens),
    ("mem.gauge.greens.content_bytes", content),
    ("mem.gauge.greens.history_bytes", history),
    ("mem.gauge.greens.changes_bytes", changes),
    ("mem.gauge.greens.rejected_bytes", rejected),
  ]
}

/// The content hold's charge and objects, and the chains' entries and bytes.
fn held_rows(state: &ShardState) -> Vec<(&'static str, u64)> {
  let partition = state.db.partition();
  let (mut entries, mut bytes) = (0u64, 0u64);
  for green in state.greens.keys() {
    for entry in partition.green_chain(*green) {
      entries = entries.saturating_add(1);
      bytes = bytes.saturating_add(u64::try_from(entry.len()).unwrap_or(u64::MAX));
    }
  }
  [
    (
      "mem.gauge.held.content_charged_bytes",
      state.held_content.charged_bytes(),
    ),
    (
      "mem.gauge.held.content_manifests",
      u64::try_from(state.held_content.manifest_count()).unwrap_or(u64::MAX),
    ),
    ("mem.gauge.held.chain_entries", entries),
    ("mem.gauge.held.chain_bytes", bytes),
  ]
  .to_vec()
}

#[cfg(test)]
mod tests {
  /// The memory status's gauges (main 01a20182, 2026-11-22 03:46). Do: on one shard, make a green with one file written
  /// and accepted; let its checkpoint end, once taken in turns (its hold paused before its first chunk through the
  /// submit's step, then let go) and once as the step's budget falls; then read the shard's report with and without
  /// memory. Expect, each time: with memory, the greens' rows (one green, its current file bytes), the content hold's
  /// (its checkpoint, the version folded) and the content arena's (its regions mapped and in RAM past the file's bytes,
  /// and its owners' bytes summing to what it allocated, the content hold among them); without, no gauge row at all (a
  /// plain status does no residency work). The checkpoint is held once it ends, which a checkpoint taken in turns does
  /// after the submit's step: until 2026-11-23 this read the status in that step and failed whenever the checkpoint
  /// went into turns (w-perf's probe: `merge.checkpoint.in_turns`, no manifest held; main's (2)).
  #[test]
  fn a_memory_status_names_its_holders_and_a_plain_one_none() {
    for paused in [true, false] {
      let (with, without) = status_once_checkpointed(paused);
      assert_eq!(with.get("mem.gauge.greens.count"), Some(&1), "{with:?}");
      assert!(
        with
          .get("mem.gauge.greens.content_bytes")
          .is_some_and(|bytes| *bytes >= 4096),
        "{with:?}"
      );
      assert!(
        with.contains_key("mem.gauge.held.content_manifests"),
        "{with:?}"
      );
      // The content arena's rows (main 01a20182, 2026-11-23): the file's bytes lie in a region it maps, in RAM.
      assert!(
        with
          .get("mem.gauge.content.arena_mapped_bytes")
          .is_some_and(|bytes| *bytes >= 4096),
        "{with:?}"
      );
      assert!(
        with
          .get("mem.gauge.content.object_resident_bytes")
          .is_some_and(|bytes| *bytes >= 4096),
        "{with:?}"
      );
      assert_the_arenas_owners_sum_to_its_allocated(&with);
      assert!(without.is_empty(), "{without:?}");
    }
  }

  /// The rows of the shard's report, with memory and without, by kind.
  fn gauges(
    state: &mut crate::state::ShardState,
    memory: bool,
  ) -> std::collections::BTreeMap<String, u64> {
    crate::verbs::shard_report(state, memory)
      .refusals
      .into_iter()
      .filter(|row| row.kind.starts_with("mem.gauge."))
      .map(|row| (row.kind, row.count))
      .collect()
  }

  /// Shape: how long a checkpoint taken in turns may take to end on a loaded runner.
  const CHECKPOINT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

  /// A one-shard daemon's memory status, with and without memory, once the checkpoint of a green with one file accepted
  /// has ended: its hold paused before its first chunk through the submit's step when `paused`, so the checkpoint is
  /// taken in turns.
  fn status_once_checkpointed(
    paused: bool,
  ) -> (
    std::collections::BTreeMap<String, u64>,
    std::collections::BTreeMap<String, u64>,
  ) {
    let daemon = crate::daemon::audit_daemon();
    let in_turns = crate::daemon::observe_first(&daemon, move |state| {
      state.merge.hold_pause_at = paused.then_some(0);
      accept_one_file(state);
      !state.merge.checkpointing.is_empty()
    });
    assert!(
      !paused || in_turns,
      "a paused hold puts the checkpoint in turns"
    );
    crate::daemon::observe_first(&daemon, |state| state.merge.hold_pause_at = None);
    let deadline = std::time::Instant::now() + CHECKPOINT_WAIT;
    while crate::daemon::observe_first(&daemon, |state| !state.merge.checkpointing.is_empty()) {
      assert!(
        std::time::Instant::now() < deadline,
        "the checkpoint never ended"
      );
      std::thread::yield_now();
    }
    let rows =
      crate::daemon::observe_first(&daemon, |state| (gauges(state, true), gauges(state, false)));
    daemon.stop();
    rows
  }

  /// Makes a green, writes one file of a page in a work and submits it, on `state`'s shard.
  fn accept_one_file(state: &mut crate::state::ShardState) {
    let owner = slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    };
    let slates_ipc::protocol::ReplyBody::GreenCreated { id: green } = crate::verbs::dispatch(
      state,
      1,
      &owner,
      slates_ipc::protocol::RequestBody::CreateGreen {
        name: "gauged".to_owned(),
        require_evidence: false,
        base: None,
      },
    ) else {
      panic!("no green")
    };
    let slates_ipc::protocol::ReplyBody::WorkCreated { id: work, .. } = crate::verbs::dispatch(
      state,
      1,
      &owner,
      slates_ipc::protocol::RequestBody::CreateWork {
        green,
        name: "gauged-work".to_owned(),
      },
    ) else {
      panic!("no work")
    };
    let edited = crate::verbs::dispatch(
      state,
      1,
      &owner,
      slates_ipc::protocol::RequestBody::Edit {
        work,
        path: "f".to_owned(),
        at: 0,
        delete_len: 0,
        bytes: vec![b'x'; 4096],
      },
    );
    assert!(matches!(edited, slates_ipc::protocol::ReplyBody::Edited));
    let submitted = crate::verbs::dispatch(
      state,
      1,
      &owner,
      slates_ipc::protocol::RequestBody::submit(work, Vec::new()),
    );
    assert!(
      matches!(
        submitted,
        slates_ipc::protocol::ReplyBody::Submitted {
          version: Some(1),
          ..
        }
      ),
      "{submitted:?}"
    );
  }

  /// The content arena's owners in a memory status's gauges, `with`: their bytes and the deferred frees summing to what
  /// the arena allocated, the content hold among them, and no owner asking past what it holds.
  fn assert_the_arenas_owners_sum_to_its_allocated(with: &std::collections::BTreeMap<String, u64>) {
    // Goal 93 (main 01a20182's order on arena-13's unnamed 373 MB): the arena's owners and its deferred frees sum to
    // what it has allocated, its mapped bytes less its free ones; a missing owner would be a gap. The content hold's
    // checkpoint is one owner here, so the sum is not empty.
    let row = |name: &str| with.get(name).copied().unwrap_or(0);
    let owners = row("mem.gauge.content.arena_volume_bytes")
      + row("mem.gauge.content.arena_vcs_windows_bytes")
      + row("mem.gauge.content.arena_content_hold_bytes");
    assert_eq!(
      owners + row("mem.gauge.content.arena_deferred_bytes"),
      row("mem.gauge.content.arena_mapped_bytes") - row("mem.gauge.content.arena_free_bytes"),
      "the arena's owners against its allocated bytes: {with:?}"
    );
    assert!(
      row("mem.gauge.content.arena_content_hold_bytes") > 0,
      "the content hold owns its checkpoint's blocks: {with:?}"
    );
    for owner in ["volume", "vcs_windows", "content_hold"] {
      assert!(
        row(&format!("mem.gauge.content.arena_{owner}_asked_bytes"))
          <= row(&format!("mem.gauge.content.arena_{owner}_bytes")),
        "{owner} asked past what it holds: {with:?}"
      );
    }
  }

  /// Goal 93 item 2b(b) (main 01a20182's ruling of 2026-11-22 20:20: hg's whole-history bar, about 410 MiB against
  /// 4.1 GB, is planned term by term, homes' records and region holds among the content store's terms). Do: on one
  /// shard, hold a two-window object for the region and note a landed record for a window; read the shard's report with
  /// memory. Expect: homes' rows name one region hold, its two windows, one landed record and its bytes in the image's
  /// form. Failing first: no homes row, those maps never gauged.
  #[test]
  fn a_memory_status_names_homes_records_and_region_holds() {
    let rows = crate::daemon::audit_on_shard(|state| {
      let window = state.store.content.chunk_bytes();
      let bytes: Vec<u8> = (0..window + 5)
        .map(|at| u8::try_from(at % 233).unwrap())
        .collect();
      let (object, _) = crate::vcs_store::homes::keep_for_region(
        state,
        &bytes,
        &[crate::vcs_store::homes::TEST_OWNER],
      )
      .unwrap();
      let first = state
        .vcs_stores
        .content()
        .unwrap()
        .object_layout(&object)
        .first()
        .map(|(window, _)| *window)
        .unwrap();
      crate::vcs_store::homes::note_landed(
        first,
        crate::vcs_store::homes::Landed::Hg(crate::vcs_store::homes::HgLanded {
          repository: slates_db::catalog::RepositoryId { bytes: [1; 16] },
          path: b"gauged".to_vec(),
          node: [2; 20],
        }),
      );
      super::rows(state, true)
    });
    let rows: std::collections::BTreeMap<&str, u64> = rows
      .iter()
      .filter(|row| row.kind.starts_with("mem.gauge.homes."))
      .map(|row| (row.kind.as_str(), row.count))
      .collect();
    assert_eq!(
      rows.get("mem.gauge.homes.region_holds"),
      Some(&1),
      "{rows:?}"
    );
    assert_eq!(
      rows.get("mem.gauge.homes.region_windows"),
      Some(&2),
      "{rows:?}"
    );
    assert_eq!(
      rows.get("mem.gauge.homes.landed_records"),
      Some(&1),
      "{rows:?}"
    );
    // The record's bytes as the image keeps it: the window's identity, the repository, the node and the path.
    assert_eq!(
      rows.get("mem.gauge.homes.landed_record_bytes"),
      Some(&(32 + 16 + 20 + 6)),
      "{rows:?}"
    );
  }

  /// The memory status's `process.malloc_*` gauges (main 01a20182, 2026-11-23). Do: read the control shard's gauges.
  /// Expect, where glibc's allocator serves the program, glibc's arena and free bytes among them (the main arena and its
  /// top exist once anything allocated); elsewhere no `malloc_*` row at all (they are glibc's figures alone). glibc's
  /// allocator serves a Linux build on glibc, but not under Miri, which models its own allocations, nor in the
  /// ThreadSanitizer lane (`--cfg slates_tsan`), whose runtime's allocator replaces glibc's: there glibc's figures read
  /// zero and the rows are left out (diag run 45315777446, 2026-11-24, read none).
  #[test]
  fn the_control_shards_memory_status_names_glibcs_bytes_where_glibc_runs() {
    let rows = crate::daemon::audit_on_shard(|state| super::rows(state, true));
    let malloc: std::collections::BTreeMap<&str, u64> = rows
      .iter()
      .filter(|row| row.kind.starts_with("mem.gauge.process.malloc_"))
      .map(|row| (row.kind.as_str(), row.count))
      .collect();
    if cfg!(all(
      target_os = "linux",
      target_env = "gnu",
      not(miri),
      not(slates_tsan)
    )) {
      assert!(
        malloc.contains_key("mem.gauge.process.malloc_arena_bytes"),
        "{malloc:?}"
      );
      assert!(
        malloc.contains_key("mem.gauge.process.malloc_free_bytes"),
        "{malloc:?}"
      );
    } else {
      assert!(malloc.is_empty(), "{malloc:?}");
    }
  }
}
