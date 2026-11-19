//! A-644: a protected branch's questions of the store (`store::protect`), by use. How a move changes its ref, judged
//! against the ref's own head, with no walk where a generation number decides; which commits a move brings, the first
//! merge commit among them, and the first no trusted key signed, with why; and an oracle over generated histories that
//! holds the change and the merge commit to plain reachability.

use proptest::prelude::*;
use slates_vcs::ContentStore;

use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tree::Mode;
use crate::protected_fixtures::{KeyRefused, Keys, base64_encode};
use crate::store::{Change, Push, Repository, StoreCounters, StoreRefused, Unsigned};

use super::{Made, blob, commit, made, push_and_move, sealed, store, tree};

/// Shape: the green the tests protect.
const MAIN: u64 = 1;
/// Shape: a second green.
const SIDE: u64 = 2;
/// Shape: a green no move has made.
const NEW: u64 = 9;

/// Shape: the date every test commit is authored and committed at.
const SECONDS: u64 = 1_112_911_993;

/// A commit of one file holding `text` over `parents` at `seconds`, signed as `signature` says (the header and the
/// armour), with its blob and tree before it: the commit is the last.
fn on(
  format: ObjectFormat,
  text: &str,
  parents: &[ObjectId],
  signature: Option<(&str, &[u8])>,
) -> Vec<Made> {
  let blob = blob(format, text.as_bytes());
  let tree = tree(format, &[(Mode::File, b"file", blob.id)]);
  let unsigned = commit(format, tree.id, parents, SECONDS);
  let commit = match signature {
    None => unsigned,
    Some((header, armour)) => signed(format, &unsigned.content, header, armour),
  };
  vec![blob, tree, commit]
}

/// `content`, a commit, with a header `name` holding `armour` folded as git folds it, after its committer.
fn signed(format: ObjectFormat, content: &[u8], name: &str, armour: &[u8]) -> Made {
  let text = String::from_utf8(content.to_vec()).unwrap();
  let (headers, message) = text.split_once("\n\n").unwrap();
  let folded = String::from_utf8(armour.to_vec())
    .unwrap()
    .trim_end()
    .replace('\n', "\n ");
  let content = format!("{headers}\n{name} {folded}\n\n{message}");
  made(format, Kind::Commit, content.into_bytes())
}

/// The id of the commit `objects` end with.
fn last(objects: &[Made]) -> ObjectId {
  objects.last().unwrap().id
}

/// An SSH public key in wire form, `ssh-ed25519` of 32 bytes of `fill`.
fn key(fill: u8) -> Vec<u8> {
  let mut blob = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
  blob.extend_from_slice(&[fill; 32]);
  blob
}

/// An SSH string: its length, then its bytes (RFC 4251 §5).
fn ssh_string(bytes: &[u8]) -> Vec<u8> {
  let mut out = u32::try_from(bytes.len()).unwrap().to_be_bytes().to_vec();
  out.extend_from_slice(bytes);
  out
}

/// The armour of an SSH signature blob made by `key` under git's namespace, its signature bytes made up: what the
/// tests' verifier reads, a signer, and nothing it checks besides.
fn ssh_armour(key: &[u8]) -> Vec<u8> {
  let mut blob = b"SSHSIG\0\0\0\x01".to_vec();
  for part in [key, b"git", b"", b"sha512", b"made up"] {
    blob.extend_from_slice(&ssh_string(part));
  }
  let mut armour = b"-----BEGIN SSH SIGNATURE-----\n".to_vec();
  for line in base64_encode(&blob).chunks(70) {
    armour.extend_from_slice(line);
    armour.push(b'\n');
  }
  armour.extend_from_slice(b"-----END SSH SIGNATURE-----\n");
  armour
}

/// The tests' verifier, trusting the key of `1`s alone.
fn keys() -> Keys {
  Keys::trusting(vec![key(1)])
}

/// A repository whose `MAIN` holds `objects`' last commit, everything in the index.
fn holding(format: ObjectFormat, objects: Vec<Made>) -> (Repository, ContentStore) {
  let mut repository = Repository::new(format);
  let mut store = store();
  let head = last(&objects);
  push_and_move(&mut repository, &mut store, objects, &[(MAIN, Some(head))]).unwrap();
  (repository, store)
}

/// What the store counted between `before` and now: walks decided without one, and walks taken.
fn walks(repository: &Repository, before: StoreCounters) -> (u64, u64) {
  let now = repository.counters();
  (
    now.decided_without_walk - before.decided_without_walk,
    now.descent_walks - before.descent_walks,
  )
}

/// A line of commits `a`, `b` on `a`, `c` on `b`, and `s` on `a`: each commit's objects, in that order.
fn line(format: ObjectFormat) -> [Vec<Made>; 4] {
  let a = on(format, "a", &[], None);
  let b = on(format, "b", &[last(&a)], None);
  let c = on(format, "c", &[last(&b)], None);
  let s = on(format, "s", &[last(&a)], None);
  [a, b, c, s]
}

/// A-644, A-642 (each ref against its own position). Do: hold `main` at `c` and `side` at `s` (a branch off `a`), then
/// ask how moves change them: `main` to a new child of `c`, to `c` itself, to `s`, to nothing; a green never made to
/// `c`; and `side` to `s` again after a delete. Expect: fast-forward, fast-forward, rewrite, delete, create, create:
/// each judged against its own green's head alone.
#[test]
fn a_move_is_judged_against_its_own_greens_head() {
  use Change::{Create, Delete, FastForward, Rewrite};
  let format = ObjectFormat::Sha1;
  let [a, b, c, s] = line(format);
  let (ids_c, ids_s) = (last(&c), last(&s));
  let mut repository = Repository::new(format);
  let mut store = store();
  let objects = [a, b, c, s].concat();
  let moves = [(MAIN, Some(ids_c)), (SIDE, Some(ids_s))];
  push_and_move(&mut repository, &mut store, objects, &moves).unwrap();
  let d = on(format, "d", &[ids_c], None);
  let push = sealed(&repository, &mut store, d.clone());
  let change = |green, to| repository.change(&store, &push, green, to).unwrap();
  let answers = [
    change(MAIN, Some(last(&d))),
    change(MAIN, Some(ids_c)),
    change(MAIN, Some(ids_s)),
    change(MAIN, None),
    change(NEW, Some(ids_c)),
  ];
  assert_eq!(answers, [FastForward, FastForward, Rewrite, Delete, Create]);
  push.abandon(&mut store).unwrap();
  push_and_move(&mut repository, &mut store, Vec::new(), &[(SIDE, None)]).unwrap();
  let empty = sealed(&repository, &mut store, Vec::new());
  let again = repository.change(&store, &empty, SIDE, Some(ids_s));
  assert_eq!(again, Ok(Create));
}

/// A-644 (§4.17.2 "A push" step 6: no walk where a generation number decides). Do: hold `main` at `c` (generation 3),
/// with `b` (2), a root `r` (1), `x` on `s` (3), `d` on `c` (4) and `y` on `x` (4) indexed, and ask how moving `main`
/// to each changes it, reading the store's counters around each question. Expect: `b`, `r` and `x` rewrites and `c`
/// a fast-forward, each decided with no walk (its generation no greater than `c`'s, or `c` itself); `d` a fast-forward
/// and `y` a rewrite, each after one walk back.
#[test]
fn a_generation_decides_a_fast_forward_without_a_walk() {
  use Change::{FastForward, Rewrite};
  let format = ObjectFormat::Sha1;
  let [a, b, c, s] = line(format);
  let (ids_b, ids_c) = (last(&b), last(&c));
  let r = on(format, "r", &[], None);
  let x = on(format, "x", &[last(&s)], None);
  let d = on(format, "d", &[ids_c], None);
  let y = on(format, "y", &[last(&x)], None);
  let targets = [ids_b, last(&r), last(&x), ids_c, last(&d), last(&y)];
  let objects = [a, b, s, r, x, d, y, c].concat();
  let (repository, mut store) = holding(format, objects);
  let push = sealed(&repository, &mut store, Vec::new());
  let answers: Vec<(Change, (u64, u64))> = targets
    .iter()
    .map(|to| {
      let before = repository.counters();
      let change = repository.change(&store, &push, MAIN, Some(*to)).unwrap();
      (change, walks(&repository, before))
    })
    .collect();
  let (decided, walked) = ((1, 0), (0, 1));
  let expected = vec![
    (Rewrite, decided),
    (Rewrite, decided),
    (Rewrite, decided),
    (FastForward, decided),
    (FastForward, walked),
    (Rewrite, walked),
  ];
  assert_eq!(answers, expected);
}

/// A-644 (a push's generation numbers are known only once it is sealed). Do: ask each question of a push not sealed,
/// and of one made for a SHA-256 repository. Expect: `Unsealed` and `ForeignPush`, never an answer read from a
/// generation number of zero.
#[test]
fn a_push_not_sealed_or_of_another_format_is_refused() {
  let format = ObjectFormat::Sha1;
  let [a, b, ..] = line(format);
  let to = last(&b);
  let (repository, store) = holding(format, [a, b].concat());
  for (push, refused) in [
    (Push::new(format), StoreRefused::Unsealed),
    (Push::new(ObjectFormat::Sha256), StoreRefused::ForeignPush),
  ] {
    let answers = (
      repository.change(&store, &push, MAIN, Some(to)).err(),
      repository.merge_commit(&store, &push, MAIN, to).err(),
      repository
        .unsigned_commit(&store, &push, MAIN, to, &keys())
        .err(),
    );
    assert_eq!(answers, (Some(refused), Some(refused), Some(refused)));
  }
}

/// An annotated tag named `name` of the commit `target`.
fn tag(format: ObjectFormat, target: ObjectId, name: &str) -> Made {
  let text = format!(
    "object {target}\ntype commit\ntag {name}\ntagger T <t@example.com> 1112911993 +0000\n\n{name}\n"
  );
  made(format, Kind::Tag, text.into_bytes())
}

/// A-644 (a ref holding a tag). Do: make a tag ref at a tag of `c`; ask how moving it to a tag of the merge `m` (of `c`
/// and `s`) changes it, and which merge commit a ref made at that tag brings. Expect: a rewrite, since a tag is no
/// commit; and `m`, found through the tag.
#[test]
fn a_tag_moved_is_a_rewrite_and_a_tag_brings_its_commits() {
  let format = ObjectFormat::Sha1;
  let [a, b, c, s] = line(format);
  let (ids_c, ids_s) = (last(&c), last(&s));
  let m = on(format, "m", &[ids_c, ids_s], None);
  let ids_m = last(&m);
  let (first, second) = (tag(format, ids_c, "v1"), tag(format, ids_m, "v2"));
  let (to_first, to_second) = (first.id, second.id);
  let mut repository = Repository::new(format);
  let mut store = store();
  let objects = [a, b, c, s, m, vec![first, second]].concat();
  push_and_move(
    &mut repository,
    &mut store,
    objects,
    &[(SIDE, Some(to_first))],
  )
  .unwrap();
  let push = sealed(&repository, &mut store, Vec::new());
  let answers = (
    repository.change(&store, &push, SIDE, Some(to_second)),
    repository.merge_commit(&store, &push, NEW, to_second),
  );
  assert_eq!(answers, (Ok(Change::Rewrite), Ok(Some(ids_m))));
}

/// A-644 (linear history: no merge commit among the commits a move brings, D(k)'s). Do: hold `main` at `c`; push the
/// merge `m` of `c` and `s` and `d` on `m`, and ask which merge commit moving `main` to `m`, to `d` and to `s` brings;
/// then move `main` to `m` and ask again of `d`, and of a green never made at `d`. Expect: `m`, `m`, none (`s` brings
/// itself alone); then none, since `main` reaches `m`; and `m` for the create, which brings its whole history.
#[test]
fn the_first_merge_commit_a_move_brings_is_named() {
  let format = ObjectFormat::Sha1;
  let [a, b, c, s] = line(format);
  let (ids_c, ids_s) = (last(&c), last(&s));
  let (mut repository, mut store) = holding(format, [a, b, s, c].concat());
  let m = on(format, "m", &[ids_c, ids_s], None);
  let d = on(format, "d", &[last(&m)], None);
  let (ids_m, ids_d) = (last(&m), last(&d));
  let push = sealed(&repository, &mut store, [m.clone(), d.clone()].concat());
  let merge = |push: &Push, green, to| repository.merge_commit(&store, push, green, to).unwrap();
  let before = [
    merge(&push, MAIN, ids_m),
    merge(&push, MAIN, ids_d),
    merge(&push, MAIN, ids_s),
  ];
  assert_eq!(before, [Some(ids_m), Some(ids_m), None]);
  push.abandon(&mut store).unwrap();
  push_and_move(
    &mut repository,
    &mut store,
    [m, d].concat(),
    &[(MAIN, Some(ids_m))],
  )
  .unwrap();
  let empty = sealed(&repository, &mut store, Vec::new());
  let after = (
    repository.merge_commit(&store, &empty, MAIN, ids_d),
    repository.merge_commit(&store, &empty, NEW, ids_d),
  );
  assert_eq!(after, (Ok(None), Ok(Some(ids_m))));
}

/// Each way a commit on `a` can be signed, and what a protected branch answers of it.
type Case = (
  Option<(&'static str, Vec<u8>)>,
  Option<Unsigned<KeyRefused>>,
);

/// The cases: the trusted key's signature, none, another key's, OpenPGP's two armours, X.509's, a made-up armour, and
/// an SSH armour that holds no signature.
fn signature_cases() -> Vec<Case> {
  let armour = |first: &str| format!("{first}\n\niQEz\n-----END-----\n").into_bytes();
  vec![
    (Some(("gpgsig", ssh_armour(&key(1)))), None),
    (None, Some(Unsigned::Missing)),
    (
      Some(("gpgsig", ssh_armour(&key(2)))),
      Some(Unsigned::Ssh(KeyRefused::Untrusted)),
    ),
    (
      Some(("gpgsig", armour("-----BEGIN PGP SIGNATURE-----"))),
      Some(Unsigned::OpenPgp),
    ),
    (
      Some(("gpgsig", armour("-----BEGIN PGP MESSAGE-----"))),
      Some(Unsigned::OpenPgp),
    ),
    (
      Some(("gpgsig", armour("-----BEGIN SIGNED MESSAGE-----"))),
      Some(Unsigned::X509),
    ),
    (
      Some(("gpgsig", armour("-----BEGIN FUTURE SIGNATURE-----"))),
      Some(Unsigned::Unknown),
    ),
    (
      Some(("gpgsig", armour("-----BEGIN SSH SIGNATURE-----"))),
      Some(Unsigned::Ssh(KeyRefused::Malformed)),
    ),
    (
      Some(("gpgsig-sha256", ssh_armour(&key(1)))),
      Some(Unsigned::Missing),
    ),
  ]
}

/// A-644 (signed commits; gpg-interface.c's `get_format_by_sig`, v2.50.1: a signature's format is its armour's first
/// line). Do: hold `main` at `a` in a SHA-1 repository and ask, of a child of `a` signed each way, which commit moving
/// `main` to it brings that no trusted key signed. Expect: none for the trusted key's SSH signature; the child, with
/// why, for each other: no signature, another key's, OpenPGP's two armours and X.509's (refused until a later piece
/// verifies them), an armour in no format git signs with, an SSH armour holding no signature (the verifier's
/// `Malformed`), and SHA-256's header in a SHA-1 repository, which is not this repository's signature.
#[test]
fn each_signature_is_told_apart_by_its_armour() {
  let format = ObjectFormat::Sha1;
  let [a, ..] = line(format);
  let base = last(&a);
  let (repository, mut store) = holding(format, a);
  for (index, (signature, expected)) in signature_cases().into_iter().enumerate() {
    let signature = signature
      .as_ref()
      .map(|(name, armour)| (*name, armour.as_slice()));
    let child = on(format, &format!("child {index}"), &[base], signature);
    let to = last(&child);
    let push = sealed(&repository, &mut store, child);
    let found = repository.unsigned_commit(&store, &push, MAIN, to, &keys());
    assert_eq!(found, Ok(expected.map(|why| (to, why))), "case {index}");
    push.abandon(&mut store).unwrap();
  }
}

/// A-644 (commit.c's `parse_buffer_signed_by_header`). Do: in a SHA-256 repository, push a child of the root signed by
/// the trusted key in `gpgsig-sha256`, which also carries a `gpgsig` header (SHA-1's); ask whether moving `main` to it
/// brings an unsigned commit. Expect: none; the verifier asked once, handed `gpgsig-sha256`'s armour and the commit
/// without either signature header, which is what git signs.
#[test]
fn the_verifier_is_handed_the_repositorys_own_signature_and_what_it_covers() {
  let format = ObjectFormat::Sha256;
  let [a, ..] = line(format);
  let base = last(&a);
  let (repository, mut store) = holding(format, a);
  let armour = ssh_armour(&key(1));
  let plain = on(format, "child", &[base], None);
  let unsigned = plain.last().unwrap().content.clone();
  let foreign = signed(
    format,
    &unsigned,
    "gpgsig",
    b"-----BEGIN PGP SIGNATURE-----\nx\n",
  );
  let both = signed(format, &foreign.content, "gpgsig-sha256", &armour);
  let to = both.id;
  let push = sealed(
    &repository,
    &mut store,
    [plain[..2].to_vec(), vec![both]].concat(),
  );
  let keys = keys();
  let found = repository.unsigned_commit(&store, &push, MAIN, to, &keys);
  assert_eq!(found, Ok(None));
  assert_eq!(keys.asked.into_inner(), vec![(armour, unsigned)]);
}

/// A-644 (each commit a move brings judged once, newest first). Do: hold `main` at `a`; push `b`, `c` and `d` in a line
/// on it, each signed by the trusted key, and ask of a move to `d`; then move `main` to `b` and ask again. Expect: no
/// commit refused either time; the verifier handed `d`, `c` and `b` in that order, then `d` and `c` alone, as many as
/// the store counted judged; and nothing `main` already reached handed again.
#[test]
fn each_commit_a_move_brings_is_judged_once_newest_first() {
  let format = ObjectFormat::Sha1;
  let [a, ..] = line(format);
  let armour = ssh_armour(&key(1));
  let signature = Some(("gpgsig", armour.as_slice()));
  let b = on(format, "b", &[last(&a)], signature);
  let c = on(format, "c", &[last(&b)], signature);
  let d = on(format, "d", &[last(&c)], signature);
  let payloads = [
    commit(format, d[1].id, &[last(&c)], SECONDS).content,
    commit(format, c[1].id, &[last(&b)], SECONDS).content,
    commit(format, b[1].id, &[last(&a)], SECONDS).content,
  ];
  let (mut repository, mut store) = holding(format, a);
  let pushed = [b.clone(), c.clone(), d.clone()].concat();
  let first = asked_of(&repository, &mut store, pushed, last(&d));
  push_and_move(
    &mut repository,
    &mut store,
    b.clone(),
    &[(MAIN, Some(last(&b)))],
  )
  .unwrap();
  let second = asked_of(&repository, &mut store, [c, d.clone()].concat(), last(&d));
  assert_eq!(first, (payloads.to_vec(), 3));
  assert_eq!(second, (payloads[..2].to_vec(), 2));
}

/// The payloads the verifier was handed, newest first, and the commits the store counted judged, for a move of `main`
/// to `to` with `objects` pushed; the push abandoned after.
fn asked_of(
  repository: &Repository,
  store: &mut ContentStore,
  objects: Vec<Made>,
  to: ObjectId,
) -> (Vec<Vec<u8>>, u64) {
  let push = sealed(repository, store, objects);
  let before = repository.counters().brought_judged;
  let keys = keys();
  let found = repository.unsigned_commit(store, &push, MAIN, to, &keys);
  assert_eq!(found, Ok(None));
  push.abandon(store).unwrap();
  let judged = repository.counters().brought_judged - before;
  let payloads = keys
    .asked
    .into_inner()
    .into_iter()
    .map(|(_, payload)| payload);
  (payloads.collect(), judged)
}

/// Shape: the most commits a generated history makes.
const MOST: usize = 12;

/// A generated history: each commit's parent count (0 to 2, no more than the commits before it) and two picks among
/// the commits before it; where the index ends; `main`'s head among the indexed; the object format.
type Generated = (Vec<(u8, u8, u8)>, usize, usize, bool);

fn generated() -> impl Strategy<Value = Generated> {
  (
    proptest::collection::vec((0u8..3, any::<u8>(), any::<u8>()), 2..MOST),
    any::<usize>(),
    any::<usize>(),
    any::<bool>(),
  )
}

/// The parents each generated commit names, by index: distinct, each earlier than it.
fn parents_of(commits: &[(u8, u8, u8)]) -> Vec<Vec<usize>> {
  commits
    .iter()
    .enumerate()
    .map(|(index, (count, one, two))| {
      let mut parents = Vec::new();
      for pick in [one, two].into_iter().take(usize::from(*count).min(index)) {
        let parent = usize::from(*pick) % index;
        if !parents.contains(&parent) {
          parents.push(parent);
        }
      }
      parents
    })
    .collect()
}

/// The model: each commit's generation number, and the commits each reaches, itself included.
fn model(parents: &[Vec<usize>]) -> (Vec<u64>, Vec<Vec<bool>>) {
  let mut generations: Vec<u64> = Vec::new();
  let mut reaches: Vec<Vec<bool>> = Vec::new();
  for (index, named) in parents.iter().enumerate() {
    let generation = named
      .iter()
      .map(|parent| generations[*parent])
      .max()
      .unwrap_or(0)
      + 1;
    let mut reached = vec![false; parents.len()];
    reached[index] = true;
    for parent in named {
      for (at, held) in reaches[*parent].iter().enumerate() {
        reached[at] |= *held;
      }
    }
    generations.push(generation);
    reaches.push(reached);
  }
  (generations, reaches)
}

/// The commits of a generated history, made in order.
fn made_history(format: ObjectFormat, parents: &[Vec<usize>]) -> Vec<Vec<Made>> {
  let mut made: Vec<Vec<Made>> = Vec::new();
  for (index, named) in parents.iter().enumerate() {
    let ids: Vec<ObjectId> = named.iter().map(|parent| last(&made[*parent])).collect();
    made.push(on(format, &format!("commit {index}"), &ids, None));
  }
  made
}

/// The oracle's answers for a move of `main` from `head` to `to`: the change, the first merge commit brought (newest
/// generation first, ties by the greater id), and whether a generation number decides the change.
fn oracle(
  (generations, reaches): &(Vec<u64>, Vec<Vec<bool>>),
  parents: &[Vec<usize>],
  ids: &[ObjectId],
  (head, to): (usize, usize),
) -> (Change, Option<ObjectId>, bool) {
  let change = if reaches[to][head] {
    Change::FastForward
  } else {
    Change::Rewrite
  };
  let merge = (0..parents.len())
    .filter(|at| reaches[to][*at] && !reaches[head][*at] && parents[*at].len() > 1)
    .max_by_key(|at| (generations[*at], ids[*at]))
    .map(|at| ids[at]);
  (change, merge, generations[to] <= generations[head])
}

/// Checks one generated history (the property's body): every commit as `main`'s next head.
fn check_history((commits, split, head, sha256): Generated) {
  let format = if sha256 {
    ObjectFormat::Sha256
  } else {
    ObjectFormat::Sha1
  };
  let parents = parents_of(&commits);
  let modelled = model(&parents);
  let made = made_history(format, &parents);
  let ids: Vec<ObjectId> = made.iter().map(|objects| last(objects)).collect();
  let indexed = 1 + split % made.len();
  let head = head % indexed;
  let mut repository = Repository::new(format);
  let mut store = store();
  let first = made[..indexed].concat();
  push_and_move(
    &mut repository,
    &mut store,
    first,
    &[(MAIN, Some(ids[head]))],
  )
  .unwrap();
  let push = sealed(&repository, &mut store, made[indexed..].concat());
  for to in 0..made.len() {
    let (change, merge, decided) = oracle(&modelled, &parents, &ids, (head, to));
    let before = repository.counters();
    let answered = repository
      .change(&store, &push, MAIN, Some(ids[to]))
      .unwrap();
    let walked = walks(&repository, before).1;
    let found = repository
      .merge_commit(&store, &push, MAIN, ids[to])
      .unwrap();
    assert_eq!((answered, found), (change, merge), "head {head}, to {to}");
    assert!(
      !decided || walked == 0,
      "a generation decided {to}, yet a walk was taken"
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(64), ..ProptestConfig::default() }))]

  /// A-644 (the oracle). Do: generate histories of up to twelve commits, each with up to two earlier parents, the first
  /// ones indexed with `main` at one of them and the rest held by a sealed push; ask how moving `main` to every commit
  /// changes it, and which merge commit it brings, in either object format. Expect: the model's answers by plain
  /// reachability every time (a fast-forward exactly when the new head reaches the old; the newest merge commit the new
  /// head reaches and the old does not), and no walk taken where a generation number decides.
  #[test]
  fn the_change_and_the_merge_commit_follow_reachability(generated in generated()) {
    check_history(generated);
  }
}
