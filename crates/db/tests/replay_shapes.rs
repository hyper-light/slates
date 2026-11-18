#![allow(dead_code, unused_imports)]
// Scratch measurement (g251, not for main): the commit stand-in against a real replay, per record shape.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
include!("snapshot_cadence_helpers.rs");

fn cost(seg: &mut AnchorSegment) -> u64 {
  let mut v: Vec<u64> = (0..7).map(|_| recovery_cost_ns(|| recover(seg, 0, caps(), 0).unwrap().1).1).collect();
  v.sort_unstable();
  v[3]
}

fn shape(name: &str, mut write: impl FnMut(&mut slates_db::replay::Db, &mut AnchorSegment, VolumeId, u64)) {
  let mut seg = segment(BUDGET_LOG_BYTES);
  let mut db = recover(&mut seg, 0, caps(), 0).unwrap().0;
  db.set_recovery_budget(u64::MAX);
  let record = green();
  let id = record.id;
  db.mutate(&mut seg, &Op::VolumeCreated { record }, 0).unwrap();
  let base = cost(&mut seg);
  let before = db.stats();
  let mut n = 0u64;
  while db.stats().tail.replay_ns - before.tail.replay_ns < 20_000_000 {
    n += 1;
    write(&mut db, &mut seg, id, n);
  }
  let after = db.stats();
  let committed = after.tail.replay_ns - before.tail.replay_ns;
  let replay = cost(&mut seg).saturating_sub(base);
  println!("SHAPE {name}: {n} records, {} bytes; commits measured {committed} ns; replay {replay} ns past {base} ns; replay/commits {:.2}",
    after.tail.bytes - before.tail.bytes, replay as f64 / committed as f64);
}

#[test]
fn replay_against_commits_by_shape() {
  for _ in 0..2 {
    shape("small single-op", |db, seg, id, n| { account(db, seg, id, n); });
    shape("64 KiB increment+fold", |db, seg, id, n| { advance_and_fold(db, seg, id, n); });
    shape("64-op transaction", |db, seg, id, n| {
      db.begin();
      for k in 0..64 { db.mutate(seg, &Op::VolumeAccounted { id, referenced_bytes: n * 64 + k, unique_bytes: k }, 0).unwrap(); }
      db.commit(seg).unwrap();
    });
    shape("4 KiB increment+fold", |db, seg, id, n| {
      let increment = vec![u8::try_from(n % 251).unwrap(); 4096];
      db.mutate(seg, &Op::GreenAdvanced { green: id, increment }, 0).unwrap();
      let version = db.partition().green_head(id);
      db.mutate(seg, &Op::GreenCheckpointed { green: id, version, checkpoint: vec![0u8; 8] }, 0).unwrap();
    });
  }
}
