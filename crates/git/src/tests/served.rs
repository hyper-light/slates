//! A-640: the repository stock git 2.50.1 answered the golden vectors from (`fixtures/make-fixtures.sh`), loaded into
//! A-614's store from its own pack (`fixtures/ofs.pack`, all twelve objects), its refs as greens and `main`'s history as
//! three fast-forward versions, one a commit, so the service answers from the repository git answered from.
//!
//! `main` is `first`, `second`, `third` (`d0f175d3`); `side` is `first` (`de204ecf`); the annotated tag `v1`
//! (`01b733a3`) names `second` (`46959f8a`); `HEAD` names `main`.

use std::convert::Infallible;

use slates_vcs::ContentStore;

use crate::Admission;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::store::{Entries, Incoming, NamedRef, Push, Repository};
use crate::unpack::{self, NoIndex, Object};

/// The pack of all twelve objects, as `git pack-objects --delta-base-offset` wrote it.
pub(crate) const OFS_PACK: &[u8] = include_bytes!("../../fixtures/ofs.pack");

/// `main`'s head, the third commit.
pub(crate) const THIRD: &str = "d0f175d3390380c935e9b56e0cebb715995d838d";
/// The second commit, which the tag names.
pub(crate) const SECOND: &str = "46959f8a540a8b77be110b36029d84b840a9faff";
/// The first commit, `side`'s head.
pub(crate) const FIRST: &str = "de204ecf4cbcbf9dec27d422e67f087c136709db";
/// The annotated tag `v1`.
pub(crate) const TAG: &str = "01b733a3e55963dbd3b64253d4ff5ff68d763b89";

/// `main`'s green.
pub(crate) const MAIN: u64 = 1;
/// `side`'s green.
pub(crate) const SIDE: u64 = 2;
/// `v1`'s green.
pub(crate) const V1: u64 = 3;

/// The refs, each naming its green.
pub(crate) const REFS: [NamedRef<'static>; 3] = [
  NamedRef {
    name: b"refs/heads/main",
    green: MAIN,
  },
  NamedRef {
    name: b"refs/heads/side",
    green: SIDE,
  },
  NamedRef {
    name: b"refs/tags/v1",
    green: V1,
  },
];

/// The SHA-1 name `hex` writes.
pub(crate) fn id(hex: &str) -> ObjectId {
  ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap()
}

/// The repository, its content, and the objects its pack holds.
pub(crate) struct Served {
  pub(crate) repository: Repository,
  pub(crate) store: ContentStore,
  pub(crate) objects: Vec<Object>,
}

/// A pack's objects handed to a push in pack order, each named as the reader named it.
struct Source<'o> {
  objects: &'o [Object],
  at: usize,
}

impl Entries for Source<'_> {
  type Refused = Infallible;

  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Infallible>> {
    let object = self.objects.get(self.at)?;
    self.at += 1;
    Some(Ok(Incoming {
      kind: object.kind,
      claimed: Some(object.id),
      content: &object.content,
      depth: object.chain.map_or(0, |chain| chain.depth),
    }))
  }
}

/// Commits `objects` and moves each green in `moves` to its head, as one version each.
pub(crate) fn commit(
  repository: &mut Repository,
  store: &mut ContentStore,
  objects: &[Object],
  moves: &[(u64, &str)],
) {
  let mut push = Push::new(ObjectFormat::Sha1);
  push
    .admit_all(store, &repository.naming(), &mut Source { objects, at: 0 })
    .unwrap();
  repository.seal(store, &mut push).unwrap();
  let admitted: Vec<_> = moves
    .iter()
    .map(|(green, head)| {
      repository
        .admit(store, &push, *green, Some(id(head)))
        .unwrap()
    })
    .collect();
  repository.commit(store, &mut push, &admitted).unwrap();
}

/// The repository stock git served the golden vectors from.
pub(crate) fn served() -> Served {
  let unpacked = unpack::read_whole(
    OFS_PACK,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let mut store = ContentStore::new(1 << 26, 1 << 10).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  commit(
    &mut repository,
    &mut store,
    &unpacked.objects,
    &[(MAIN, FIRST), (SIDE, FIRST), (V1, TAG)],
  );
  commit(&mut repository, &mut store, &[], &[(MAIN, SECOND)]);
  commit(&mut repository, &mut store, &[], &[(MAIN, THIRD)]);
  Served {
    repository,
    store,
    objects: unpacked.objects,
  }
}
