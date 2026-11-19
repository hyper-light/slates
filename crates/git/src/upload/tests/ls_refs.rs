//! `ls-refs`, served (A-640 part 2; §4.17.2 "A fetch" item 1).

use proptest::prelude::*;

use super::{answer_of, answer_v2, command};
use crate::Admission;
use crate::fixtures::{LS_REFS_REQUEST, LS_REFS_RESPONSE};
use crate::http::Version;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::served::{self, FIRST, REFS, SECOND, TAG, THIRD, id};
use crate::store::NamedRef;
use crate::upload::Asked;
use crate::v2::ls_refs::{self, Target};

/// One answer line, as compared: what it names (`None` for `unborn`), its name, its symref's target and its peeled id.
type Listed = (Option<ObjectId>, Vec<u8>, Option<Vec<u8>>, Option<ObjectId>);

/// The lines of the `ls-refs` answer `bytes` hold.
fn listed(bytes: &[u8]) -> Vec<Listed> {
  ls_refs::decode_answer(bytes, ObjectFormat::Sha1, &mut Admission::new(u64::MAX))
    .unwrap()
    .into_iter()
    .map(|line| {
      let target = match line.value.target {
        Target::Id(id) => Some(id),
        Target::Unborn => None,
      };
      (
        target,
        line.value.name.to_vec(),
        line.value.attributes.symref_target().map(<[u8]>::to_vec),
        line.value.attributes.peeled(),
      )
    })
    .collect()
}

/// A-640 part 2 (git's `ls-refs` exchange: `peel`, `symrefs`, `unborn`, `ref-prefix HEAD`, `ref-prefix refs/`): do send
/// the request stock git's client sent. Expect: the answer stock git 2.50.1 wrote, byte for byte.
#[test]
fn ls_refs_answers_byte_for_byte_as_stock_git_answered() {
  let fixture = served::served();
  let (bytes, answer) = answer_v2(&fixture, LS_REFS_REQUEST);
  assert_eq!(answer.refusal(), None);
  assert_eq!(bytes, LS_REFS_RESPONSE);
}

/// A-640 part 2 (`send_possibly_unborn_head`): do serve the repository with `HEAD` on a branch with no commit, asking
/// with `unborn` and `symrefs`, then with `symrefs` alone. Expect: `unborn HEAD symref-target:refs/heads/none` first,
/// then the refs; and without `unborn`, no `HEAD` line at all.
#[test]
fn an_unborn_head_is_listed_only_when_asked_for_with_its_target() {
  let fixture = served::served();
  let unborn = command("ls-refs", &[b"symrefs", b"unborn"]);
  let (bytes, _) = answer_of(
    &fixture,
    &REFS,
    b"refs/heads/none",
    Version::V2,
    Asked::Request(&unborn),
  );
  let lines = listed(&bytes);
  assert_eq!(
    lines.first(),
    Some(&(
      None,
      b"HEAD".to_vec(),
      Some(b"refs/heads/none".to_vec()),
      None
    ))
  );
  assert_eq!(lines.len(), 4);
  let symrefs = command("ls-refs", &[b"symrefs"]);
  let (bytes, _) = answer_of(
    &fixture,
    &REFS,
    b"refs/heads/none",
    Version::V2,
    Asked::Request(&symrefs),
  );
  let names: Vec<Vec<u8>> = listed(&bytes).into_iter().map(|line| line.1).collect();
  assert_eq!(
    names,
    [
      b"refs/heads/main".to_vec(),
      b"refs/heads/side".to_vec(),
      b"refs/tags/v1".to_vec()
    ]
  );
}

/// A-640 part 2 (each prefix a run of the sorted refs): do ask with prefixes that overlap and repeat, and with none.
/// Expect: each ref a prefix takes once, in name order, `HEAD` only where a prefix takes it; with no prefix, `HEAD` and
/// every ref, the tag peeled.
#[test]
fn overlapping_prefixes_list_each_ref_once_in_name_order() {
  let fixture = served::served();
  let overlapping = command(
    "ls-refs",
    &[
      b"ref-prefix refs/heads/s",
      b"ref-prefix refs/",
      b"ref-prefix refs/heads/",
      b"ref-prefix refs/",
    ],
  );
  let (bytes, _) = answer_v2(&fixture, &overlapping);
  let names: Vec<Vec<u8>> = listed(&bytes).into_iter().map(|line| line.1).collect();
  assert_eq!(
    names,
    [
      b"refs/heads/main".to_vec(),
      b"refs/heads/side".to_vec(),
      b"refs/tags/v1".to_vec()
    ]
  );
  let everything = command("ls-refs", &[b"peel"]);
  let (bytes, _) = answer_v2(&fixture, &everything);
  assert_eq!(
    listed(&bytes),
    [
      (Some(id(THIRD)), b"HEAD".to_vec(), None, None),
      (Some(id(THIRD)), b"refs/heads/main".to_vec(), None, None),
      (Some(id(FIRST)), b"refs/heads/side".to_vec(), None, None),
      (
        Some(id(TAG)),
        b"refs/tags/v1".to_vec(),
        None,
        Some(id(SECOND))
      ),
    ]
  );
}

/// Format: names a generated repository's refs are drawn from, in name order, with runs a prefix can split.
const NAMES: [&str; 10] = [
  "refs/heads/a",
  "refs/heads/a-b",
  "refs/heads/a.b",
  "refs/heads/ab",
  "refs/heads/b",
  "refs/heads/main",
  "refs/notes/x",
  "refs/tags/a",
  "refs/tags/v1",
  "refs/tags/v1.1",
];

/// Format: prefixes a generated request draws from: the empty one, ones `HEAD` begins with, ones that split runs, and
/// one that takes nothing.
const PREFIXES: [&str; 12] = [
  "",
  "H",
  "HEAD",
  "HEADS",
  "refs/",
  "refs/heads/",
  "refs/heads/a",
  "refs/heads/a.",
  "refs/t",
  "refs/tags/v1",
  "refs/heads/main",
  "x",
];

/// Format: the objects a generated ref names, by its index: three commits and an annotated tag.
const TARGETS: [&str; 4] = [FIRST, SECOND, THIRD, TAG];

/// The green a generated ref at `index` gets.
fn green(index: usize) -> u64 {
  100 + u64::try_from(index).unwrap()
}

/// What a generated ref names, and its peeled id where `peel` was asked and it names the tag.
fn named(names: &[&str], name: &str, peel: bool) -> Option<(ObjectId, Option<ObjectId>)> {
  let index = names.iter().position(|candidate| *candidate == name)?;
  let target = TARGETS[index % TARGETS.len()];
  let peeled = (peel && target == TAG).then(|| id(SECOND));
  Some((id(target), peeled))
}

/// The oracle: `HEAD`'s line where a prefix (or none) takes it, then every ref any prefix begins, in name order, each
/// as the store holds it.
fn expected(
  names: &[&str],
  prefixes: &[&str],
  head: &str,
  flags: (bool, bool, bool),
) -> Vec<Listed> {
  let (peel, symrefs, unborn) = flags;
  let takes =
    |name: &str| prefixes.is_empty() || prefixes.iter().any(|prefix| name.starts_with(prefix));
  let mut lines = Vec::new();
  if takes("HEAD") {
    let symref = symrefs.then(|| head.as_bytes().to_vec());
    match named(names, head, peel) {
      Some((target, peeled)) => lines.push((Some(target), b"HEAD".to_vec(), symref, peeled)),
      None if unborn && symrefs => lines.push((None, b"HEAD".to_vec(), symref, None)),
      None => {}
    }
  }
  for name in names.iter().filter(|name| takes(name)) {
    let (target, peeled) = named(names, name, peel).unwrap();
    lines.push((Some(target), name.as_bytes().to_vec(), None, peeled));
  }
  lines
}

/// What the store answers: a repository with `names`, each a green at its target, served with `head`.
fn answered(
  names: &[&str],
  prefixes: &[&str],
  head: &str,
  flags: (bool, bool, bool),
) -> Vec<Listed> {
  let mut fixture = served::served();
  let moves: Vec<(u64, &str)> = (0..names.len())
    .map(|index| (green(index), TARGETS[index % TARGETS.len()]))
    .collect();
  if !moves.is_empty() {
    served::commit(&mut fixture.repository, &mut fixture.store, &[], &moves);
  }
  let refs: Vec<NamedRef<'_>> = names
    .iter()
    .enumerate()
    .map(|(index, name)| NamedRef {
      name: name.as_bytes(),
      green: green(index),
    })
    .collect();
  let (peel, symrefs, unborn) = flags;
  let mut arguments: Vec<Vec<u8>> = [(peel, "peel"), (symrefs, "symrefs"), (unborn, "unborn")]
    .into_iter()
    .filter(|(asked, _)| *asked)
    .map(|(_, word)| word.as_bytes().to_vec())
    .collect();
  arguments.extend(
    prefixes
      .iter()
      .map(|prefix| format!("ref-prefix {prefix}").into_bytes()),
  );
  let arguments: Vec<&[u8]> = arguments.iter().map(Vec::as_slice).collect();
  let request = command("ls-refs", &arguments);
  let (bytes, answer) = answer_of(
    &fixture,
    &refs,
    head.as_bytes(),
    Version::V2,
    Asked::Request(&request),
  );
  assert_eq!(answer.refusal(), None);
  listed(&bytes)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-640 part 2 (the oracle: every ref a prefix begins, in name order, from a list filtered whole): do serve a
  /// generated repository's refs, with generated prefixes (repeated, overlapping, none), `HEAD` born or not, and each of
  /// `peel`, `symrefs` and `unborn` asked or not. Expect: the answer the oracle computes, line for line.
  #[test]
  fn ls_refs_lists_what_a_filter_of_every_ref_lists(
    names in proptest::sample::subsequence(NAMES.to_vec(), 0..=NAMES.len()),
    prefixes in proptest::collection::vec(proptest::sample::select(PREFIXES.to_vec()), 0..5),
    head in proptest::sample::select(vec!["refs/heads/main", "refs/heads/none"]),
    flags in any::<(bool, bool, bool)>(),
  ) {
    prop_assert_eq!(
      answered(&names, &prefixes, head, flags),
      expected(&names, &prefixes, head, flags)
    );
  }
}
