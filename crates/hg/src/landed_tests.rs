//! Tests of Mercurial's history on the changeset log (A-767 C4) over a double of the store: a push landed as one
//! changeset and replayed row for row, a step at a time; a changeset missing a section refused by kind.

use std::borrow::Cow;

use slates_vcs::ContentStore;

use super::{ChangesetLog, Refused, Replaying, Section, Stepped, land_push};
use crate::Admission;
use crate::bundle2::Bundle2;
use crate::fixtures::CG03;
use crate::index::HgIndex;
use crate::push::read;
use crate::push::{ChangegroupPart, intake::Intake, intake::Stepped as IntakeStepped};

/// The changeset log held in memory: each changeset's sections, its sequence its place.
#[derive(Default)]
struct Held {
  changesets: Vec<Vec<(Section, Vec<u8>)>>,
}

impl ChangesetLog for Held {
  type Refused = core::convert::Infallible;

  fn append(&mut self, sections: &[(Section, &[u8])]) -> Result<u64, Self::Refused> {
    self.changesets.push(
      sections
        .iter()
        .map(|(section, bytes)| (*section, bytes.to_vec()))
        .collect(),
    );
    Ok(u64::try_from(self.changesets.len() - 1).unwrap())
  }

  fn section(&self, sequence: u64, section: Section) -> Option<Cow<'_, [u8]>> {
    let changeset = self.changesets.get(usize::try_from(sequence).ok()?)?;
    changeset
      .iter()
      .find(|(held, _)| *held == section)
      .map(|(_, bytes)| Cow::Borrowed(bytes.as_slice()))
  }

  fn next(&self, after: Option<u64>) -> Option<u64> {
    let next = after.map_or(0, |after| after + 1);
    (usize::try_from(next).ok()? < self.changesets.len()).then_some(next)
  }
}

fn room() -> Admission {
  Admission::new(u64::MAX)
}

fn store() -> ContentStore {
  ContentStore::new(1 << 24, 1 << 12).unwrap()
}

fn stock() -> ChangegroupPart {
  let bundle = Bundle2::decode(CG03, &mut room()).unwrap();
  let unbundle = read(&bundle, &mut room()).unwrap();
  unbundle.changegroup().unwrap().1.clone()
}

/// The fork a live push's intake makes of `part` over an empty index.
fn intaken(part: &ChangegroupPart, store: &mut ContentStore) -> HgIndex {
  let mut index = HgIndex::new();
  let mut intake = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    store,
    &mut room(),
  )
  .unwrap();
  while intake.step(&part.payload, store, &mut room()).unwrap() == IntakeStepped::More {}
  let (fork, _) = intake.finish().ok().unwrap();
  index.release(store).unwrap();
  fork
}

fn rows(index: &mut HgIndex, store: &ContentStore) -> Vec<(Vec<u8>, Vec<u8>)> {
  index
    .rows()
    .iter(store)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect()
}

/// Every step of a replay of `log` into `index`: the steps taken, or the refusal.
fn replayed(
  log: &Held,
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> Result<(Replaying, u64), Refused> {
  let mut replaying = Replaying::new();
  let mut steps = 0;
  while replaying.step(log, index, store)? == Stepped::More {
    steps += 1;
  }
  Ok((replaying, steps))
}

/// A-767 C4. Do: land stock 7.2's push as one changeset (its head the `Record`, its changegroup the `Payload`), land it
/// again, and replay the log into an empty index a step at a time. Expect the `Record` a few dozen bytes and the
/// `Payload` the changegroup byte for byte; the replayed index the live intake's fork row for row; the second
/// changeset bringing nothing; and more steps than changesets (the replay is a unit a step, never a changeset whole).
#[test]
fn a_push_landed_as_a_changeset_replays_row_for_row_a_step_at_a_time() {
  let part = stock();
  let mut store = store();
  let mut expected = intaken(&part, &mut store);
  let mut log = Held::default();
  let first = land_push(&mut log, 1 << 20, (part.version, &part.payload)).unwrap();
  let second = land_push(&mut log, 1 << 20, (part.version, &part.payload)).unwrap();
  assert_eq!((first, second), (0, 1));
  assert!(log.section(0, Section::Record).unwrap().len() < 64);
  assert_eq!(
    &*log.section(0, Section::Payload).unwrap(),
    part.payload.as_slice()
  );
  let mut index = HgIndex::new();
  let (replaying, steps) = replayed(&log, &mut index, &mut store).unwrap();
  assert_eq!(rows(&mut index, &store), rows(&mut expected, &store));
  let brought: Vec<u32> = replaying
    .replayed()
    .iter()
    .map(|replayed| replayed.changesets)
    .collect();
  assert_eq!(brought.len(), 2);
  assert!(brought[0] > 0 && brought[1] == 0, "{brought:?}");
  assert!(steps > 2, "{steps} steps for two changesets");
  for index in [index, expected] {
    index.release(&mut store).unwrap();
  }
}

/// A-767 C4 (a damaged log). Do: replay a log whose one changeset has its `Record` and no `Payload`. Expect the replay
/// refused by kind, naming the changeset and the missing section, the index still empty and the store holding nothing.
#[test]
fn a_changeset_missing_its_payload_is_refused_by_kind() {
  let part = stock();
  let mut store = store();
  let mut log = Held::default();
  land_push(&mut log, 1 << 20, (part.version, &part.payload)).unwrap();
  log.changesets[0].retain(|(section, _)| *section != Section::Payload);
  let mut index = HgIndex::new();
  let refused = replayed(&log, &mut index, &mut store).err().unwrap();
  assert!(
    matches!(
      refused,
      Refused::Missing {
        sequence: 0,
        section: Section::Payload
      }
    ),
    "{refused:?}"
  );
  assert_eq!(rows(&mut index, &store), Vec::new());
  assert_eq!(store.len(), 0);
}
