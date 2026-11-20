//! A-640 part 4, fourth slice: a `sparse:oid` filter's name read as git reads it, with what git writes to its error
//! stream meanwhile, through upload-pack's entry point over the fixture repository, held to stock git 2.50.1's own
//! answers (`fixtures/fetch-sparse-{warn,abbrev,describe,ambiguous}-*`): the ref a capture added served beside the
//! fixture's, and the eight objects whose names begin alike made from the bytes the recorder writes.

use std::collections::BTreeSet;

use super::{MAIN_REF, answer_of, command, naming};
use crate::http::Version;
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::pktline::{self, Packet};
use crate::served::{self, REFS, TAG, THIRD, id};
use crate::store::{Abbrev, NamedRef};
use crate::unpack::Object;
use crate::upload::{Asked, Refusal};

/// A golden vector of `make-fixtures.sh`'s fourth-slice captures.
macro_rules! golden {
  ($name:literal) => {
    (
      $name,
      include_bytes!(concat!(
        "../../../fixtures/fetch-sparse-",
        $name,
        ".request.bin"
      ))
      .as_slice(),
      include_bytes!(concat!(
        "../../../fixtures/fetch-sparse-",
        $name,
        ".response.bin"
      ))
      .as_slice(),
    )
  };
}

/// Format: the root tree of `main`'s head, which the alike commits name (`fixtures/objects.txt`).
const ROOT: &str = "0351da7889aaa5309979d9a4e742f54ac1750e44";

/// Format: the identity and date the recorder writes into the alike commits and tags, before a zone.
const IDENT: &str = "test <test@example.com> 1112911993";

/// Format: the first eight digits of each alike object's name, as the recorder checks them, in the order it makes
/// them: the two blobs, their tree, its commit, the two commits, the two tags.
const ALIKE_BEGINS: [&str; 8] = [
  "5ad7d2ca", "5ad7d2ca", "3af7e09f", "c6d52c1b", "67fd0f47", "67fdbfcb", "2188e00e", "21887657",
];

/// Format: the greens the alike objects' refs name, from the first the fixture leaves free (`served`'s are 1 to 3):
/// `collide-blobs`, `collide-136`, `collide-151`, `collide-48`, `collide-106`; then the three refs a capture adds,
/// each naming `main`'s head.
const ALIKE_GREENS: [u64; 5] = [4, 5, 6, 7, 8];

/// Format: the greens of the refs the warning captures add: `refs/tags/main`, `refs/heads/4cdb` and the branch named
/// by `main`'s head's full name.
const ADDED_GREENS: [u64; 3] = [9, 10, 11];

/// Format: the ref each warning capture adds, by name and green.
const ADDED: [NamedRef<'static>; 3] = [
  NamedRef {
    name: b"refs/tags/main",
    green: ADDED_GREENS[0],
  },
  NamedRef {
    name: b"refs/heads/4cdb",
    green: ADDED_GREENS[1],
  },
  NamedRef {
    name: b"refs/heads/d0f175d3390380c935e9b56e0cebb715995d838d",
    green: ADDED_GREENS[2],
  },
];

/// An object of `kind` whose content is `content`, named by its bytes, as a pack's reader would have read it whole.
fn object(kind: Kind, content: Vec<u8>) -> Object {
  Object {
    offset: 0,
    stored: 0,
    kind,
    id: name_of(ObjectFormat::Sha1, kind, &content).unwrap(),
    content,
    chain: None,
  }
}

/// The eight objects whose names begin alike, from the bytes `make-fixtures.sh` writes (its "Objects whose names begin
/// alike"): two blobs, their tree and its commit; two commits by an author east of UTC with a two-line subject; two
/// tags of `main`'s head.
fn alike() -> Vec<Object> {
  let blob_a = object(Kind::Blob, b"collide 26350\n".to_vec());
  let blob_b = object(Kind::Blob, b"collide 62386\n".to_vec());
  let entries = [
    b"100644 a\0".as_slice(),
    blob_a.id.as_bytes(),
    b"100644 b\0",
    blob_b.id.as_bytes(),
  ];
  let pair = object(Kind::Tree, entries.concat());
  let collisions = format!(
    "tree {}\nparent {THIRD}\nauthor {IDENT} +0000\ncommitter {IDENT} +0000\n\ncollisions\n",
    pair.id
  );
  let mut objects = vec![
    blob_a,
    blob_b,
    pair,
    object(Kind::Commit, collisions.into_bytes()),
  ];
  for n in [136, 151] {
    let commit = format!(
      "tree {ROOT}\nparent {THIRD}\nauthor {IDENT} +0530\ncommitter {IDENT} +0000\n\ncollide {n}\nsecond line  \n\nbody\n"
    );
    objects.push(object(Kind::Commit, commit.into_bytes()));
  }
  for n in [48, 106] {
    let tag = format!(
      "object {THIRD}\ntype commit\ntag collide-{n}\ntagger {IDENT} +0530\n\ncollide {n}\n"
    );
    objects.push(object(Kind::Tag, tag.into_bytes()));
  }
  objects
}

/// The fixture repository with the alike objects, each ref of theirs and each ref a capture adds a green of its own.
fn served_alike() -> served::Served {
  let mut fixture = served::served();
  let objects = alike();
  let heads: Vec<String> = [3, 4, 5, 6, 7]
    .iter()
    .map(|at| objects[*at].id.to_string())
    .collect();
  let moves: Vec<(u64, &str)> = ALIKE_GREENS
    .iter()
    .copied()
    .zip(heads.iter().map(String::as_str))
    .chain(ADDED_GREENS.iter().map(|green| (*green, THIRD)))
    .collect();
  served::commit(
    &mut fixture.repository,
    &mut fixture.store,
    &objects,
    &moves,
  );
  fixture.objects.extend(objects);
  fixture
}

/// The fixture's refs and `added`, in name order.
fn refs_with(added: Option<NamedRef<'static>>) -> Vec<NamedRef<'static>> {
  let mut refs: Vec<NamedRef<'static>> = REFS.iter().copied().chain(added).collect();
  refs.sort_by(|earlier, later| earlier.name.cmp(later.name));
  refs
}

/// A v2 response's packets but its pack's data, each whole, and the names of the objects its pack holds, a thin pack's
/// bases from `objects`.
fn parts<'r>(response: &'r [u8], objects: &[Object]) -> (Vec<&'r [u8]>, BTreeSet<ObjectId>) {
  let mut others = Vec::new();
  let mut pack = Vec::new();
  let mut in_pack = false;
  let mut at = 0;
  while at < response.len() {
    let (packet, took) = pktline::read(&response[at..], at as u64).unwrap().unwrap();
    let len = match packet {
      Packet::Data(_) => took,
      _ => pktline::HEADER_LEN,
    };
    match packet {
      Packet::Data([1, data @ ..]) if in_pack => pack.extend_from_slice(data),
      Packet::Data(data) => {
        in_pack |= data == b"packfile\n";
        others.push(&response[at..at + len]);
      }
      _ => others.push(&response[at..at + len]),
    }
    at += took;
  }
  let names = if pack.is_empty() {
    BTreeSet::new()
  } else {
    served::pack_names(&pack, objects)
  };
  (others, names)
}

/// A golden vector served: its name, git's request and git's answer, and why the answer is refused, if it is.
type Golden<'g> = (&'g str, &'g [u8], &'g [u8], Option<Refusal>);

/// Serves each of `goldens` with `refs`, asserting every packet git's server sent but the pack's data, the objects of
/// its pack, and why the answer was refused.
fn held_to_git(fixture: &served::Served, refs: &[NamedRef<'_>], goldens: &[Golden<'_>]) {
  for (name, request, response, refusal) in goldens {
    let (bytes, answer) = answer_of(
      fixture,
      refs,
      MAIN_REF,
      Version::V2,
      Asked::Request(request),
    );
    assert_eq!(
      (parts(&bytes, &fixture.objects), answer.refusal()),
      (parts(response, &fixture.objects), refusal.as_ref()),
      "{name}: {}",
      String::from_utf8_lossy(&bytes)
    );
  }
}

/// The names of every object of `fixture`'s repository.
fn names_of(fixture: &served::Served) -> Vec<String> {
  fixture
    .objects
    .iter()
    .map(|object| object.id.to_string())
    .collect()
}

/// `name` padded past its 40 digits with zeros to 64, as git keeps a name.
fn padded(name: &str) -> String {
  format!("{name:0<64}")
}

/// The names of the objects `written` abbreviates, as the store finds them.
fn found_by(fixture: &served::Served, written: &str) -> Vec<String> {
  let abbrev = Abbrev::parse(written.as_bytes()).unwrap();
  fixture
    .repository
    .abbreviated(&fixture.store, &abbrev)
    .map(|(found, _)| found.to_string())
    .collect()
}

/// The oracle: the names among `names` that begin with `prefix` once padded, in name order.
fn beginning(names: &[String], prefix: &str) -> Vec<String> {
  let mut found: Vec<String> = names
    .iter()
    .filter(|name| padded(name).starts_with(prefix))
    .cloned()
    .collect();
  found.sort();
  found
}

/// The oracle: the fewest digits of `name`, from git's default of 7, that no other of `names` begins with.
fn unique_by_hand(names: &[String], name: &str) -> usize {
  (7..40)
    .find(|digits| {
      names
        .iter()
        .filter(|other| other.as_str() != name)
        .all(|other| other.get(..*digits) != name.get(..*digits))
    })
    .unwrap_or(40)
}

/// A-640 part 4 (`init_object_disambiguation`, `match_hash`): do look up, in the repository with the alike objects,
/// every prefix of every object's name from 4 digits to 64 (past 40 padded with zeros, as git keeps a name), in lower
/// and upper case, and each name's 40 digits with a `1` after them. Expect: the objects whose names begin with the
/// digits, in name order, exactly, for both cases; none for the `1`.
#[test]
fn every_prefix_finds_the_objects_its_digits_begin_and_no_other() {
  let fixture = served_alike();
  let names = names_of(&fixture);
  for name in &names {
    let whole = padded(name);
    for digits in 4..=64 {
      let prefix = whole.get(..digits).unwrap();
      let expected = beginning(&names, prefix);
      let found = [
        found_by(&fixture, prefix),
        found_by(&fixture, &prefix.to_uppercase()),
      ];
      assert_eq!(found, [expected.clone(), expected], "{prefix}");
    }
    assert_eq!(
      found_by(&fixture, &format!("{name}1")),
      Vec::<String>::new()
    );
  }
}

/// A-640 part 4 (`repo_find_unique_abbrev_r`): do ask each object's unique abbreviation in the repository with the
/// alike objects. Expect: git's default 7 digits (from twenty objects), and for each object the fewest digits from 7
/// that no other name begins with, the two blobs that share 8 among them at 9.
#[test]
fn each_objects_unique_abbreviation_is_the_fewest_digits_no_other_name_shares() {
  let fixture = served_alike();
  let names = names_of(&fixture);
  let least = fixture.repository.default_digits();
  let unique: Vec<usize> = fixture
    .objects
    .iter()
    .map(|object| {
      fixture
        .repository
        .unique_digits(&fixture.store, &object.id, least)
    })
    .collect();
  let expected: Vec<usize> = names
    .iter()
    .map(|name| unique_by_hand(&names, name))
    .collect();
  let blobs = names
    .iter()
    .zip(&unique)
    .filter(|(name, digits)| name.starts_with("5ad7d2ca") && **digits == 9)
    .count();
  assert_eq!((least, unique, blobs), (7, expected, 2));
}

/// A-640 part 4 (`init_object_disambiguation` refuses fewer than 4 digits, more than 64, and anything but hexadecimal
/// digits): do read names of 3 and 65 digits, and of 4 with a byte that is no digit. Expect: none is an abbreviation.
#[test]
fn a_name_too_short_too_long_or_not_hexadecimal_is_no_abbreviation() {
  let refused: [&[u8]; 4] = [b"4cd", &[b'0'; 65], b"4cdg", b"4cd\xc3"];
  assert!(refused.iter().all(|name| Abbrev::parse(name).is_none()));
}

/// A-640 part 4 (`get_short_oid`, `get_describe_name`; git's `fetch-sparse-abbrev-*` and `fetch-sparse-describe`): do
/// clone `main` under `sparse:oid=` the blob `dir/file` abbreviated to 4 digits and to 8 upper case, padded past its
/// 40 digits with a `0` and with a `1`, main's head abbreviated before a path, the root tree abbreviated, and `git
/// describe`'s name for the blob, whose ref names no ref. Expect: each answered as git answered it: the blob's names
/// served as `main:dir/file` is, the `1` naming nothing, the tree no blob, and nothing else said.
#[test]
fn abbreviated_and_described_names_are_read_as_git_reads_them() {
  let fixture = served_alike();
  let goldens = [
    golden!("abbrev-blob"),
    golden!("abbrev-upper"),
    golden!("abbrev-padded"),
    golden!("abbrev-rev"),
    golden!("describe"),
  ];
  let served: Vec<Golden<'_>> = goldens
    .iter()
    .map(|(name, request, response)| (*name, *request, *response, None))
    .collect();
  let unpadded = "4cdb2265d30204be5463b38174b2e8e7179824051";
  let (tree, unheld) = (golden!("abbrev-tree"), golden!("abbrev-unpadded"));
  let refused = [
    (
      tree.0,
      tree.1,
      tree.2,
      Some(Refusal::SparseData { id: id(ROOT) }),
    ),
    (
      unheld.0,
      unheld.1,
      unheld.2,
      Some(Refusal::SparseBlob {
        name: unpadded.as_bytes().to_vec(),
      }),
    ),
  ];
  held_to_git(&fixture, &REFS, &served);
  held_to_git(&fixture, &REFS, &refused);
}

/// A-640 part 4 (`get_oid_basic`'s warning, `expand_ref`; git's `fetch-sparse-warn-*`): do clone `main` under
/// `sparse:oid=main:dir/file` with a tag `main` beside the branch, `4cdb` with a branch `4cdb` (which also abbreviates
/// the blob `dir/file`'s name alone), and main's head's full name before `:dir/file` with a branch of that name.
/// Expect: each answered as git answered it: its warning on band 2 after the packfile section's header, and the advice
/// of refs named like an object after the last; then the pack of `main:dir/file`'s clone for the first and last, and
/// for the second, the branch's commit taken, git's words for an object that is no blob, read apart from the warning.
#[test]
fn a_name_that_names_a_ref_and_something_else_is_warned_of_as_git_warns() {
  let fixture = served_alike();
  let cases = [
    (golden!("warn-two-refs"), ADDED[0], None),
    (
      golden!("warn-ref-and-object"),
      ADDED[1],
      Some(Refusal::SparseData { id: id(THIRD) }),
    ),
    (golden!("warn-full-name"), ADDED[2], None),
  ];
  for ((name, request, response), added, refusal) in cases {
    held_to_git(
      &fixture,
      &refs_with(Some(added)),
      &[(name, request, response, refusal)],
    );
  }
}

/// A-640 part 4 (`get_short_oid` unquiet, `show_ambiguous_object`, `sort_ambiguous`, `vadvise`; git's
/// `fetch-sparse-ambiguous-*`): do clone `main` under `sparse:oid=` the 4 digits two blobs' names begin with, those two
/// commits' begin with (alone, where no candidate is a blob, and before a path, where both are trees' commits), and
/// those two tags' begin with. Expect: each answered as git answered it: its `error:` line read alone, then its advice
/// listing the candidates (the blobs by 9 digits; the commits by their author dates in their zone and their two-line
/// subjects; the tags by their dates in UTC and their names) with git's words for a name that names nothing.
#[test]
fn an_ambiguous_abbreviation_lists_its_candidates_as_git_lists_them() {
  let fixture = served_alike();
  let nothing = |name: &str| {
    Some(Refusal::SparseBlob {
      name: name.as_bytes().to_vec(),
    })
  };
  let cases = [
    (golden!("ambiguous-blobs"), "5ad7"),
    (golden!("ambiguous-commits"), "67fd"),
    (golden!("ambiguous-commits-path"), "67fd:dir/file"),
    (golden!("ambiguous-tags"), "2188"),
  ];
  let goldens: Vec<Golden<'_>> = cases
    .iter()
    .map(|((name, request, response), blob_ish)| (*name, *request, *response, nothing(blob_ish)))
    .collect();
  held_to_git(&fixture, &REFS, &goldens);
}

/// A-640 part 4 (the recorder's own check of the alike objects' names): do make the eight alike objects from the
/// recorder's bytes. Expect: their names begin as the recorder checks they do, so the tests serve the objects git
/// answered from.
#[test]
fn the_alike_objects_are_the_ones_the_recorder_made() {
  let begins: Vec<String> = alike()
    .iter()
    .map(|object| object.id.to_string().get(..8).unwrap().to_owned())
    .collect();
  assert_eq!(begins, ALIKE_BEGINS);
}

/// Format: the full name of `main`'s branch, which a log's entry and a branch mark name.
const MAIN_BRANCH: &[u8] = b"refs/heads/main";

/// Why git dies of a `<blob-ish>` that names nothing: `name`, as sent.
fn naming_nothing(name: &str) -> Option<Refusal> {
  Some(Refusal::SparseBlob {
    name: name.as_bytes().to_vec(),
  })
}

/// Why git dies of a `<blob-ish>` that names the object `hex`, which is no blob.
fn naming_no_blob(hex: &str) -> Option<Refusal> {
  Some(Refusal::SparseData { id: id(hex) })
}

/// A-640 part 4, fifth slice (`get_oid_1`'s suffixes, `get_parent`, `get_nth_ancestor`, `lookup_commit_reference`;
/// git's `fetch-sparse-{ancestor,parent}*`): do clone `main` under `sparse:oid=` a parent or an ancestor before
/// `:dir/file` (of `main` by `~1`, `~`, `^`, `^0`; of `@`; of the tag `v1` by `^0`), `main^2`, `main~3` (past the
/// root), `main~2147483648` (past git's largest count), `main~2` (whose commit has no `dir/file`), a blob's ancestor,
/// and the alike commits' abbreviation's. Expect: each answered as git answered it: the paths served as
/// `main:dir/file` is, the others naming nothing, git's error that the blob is no commit before its `fatal:` line, and
/// the two commits listed for the abbreviation.
#[test]
fn parents_and_ancestors_are_read_as_git_reads_them() {
  let fixture = served_alike();
  let cases = [
    (golden!("ancestor"), None),
    (golden!("ancestor-bare"), None),
    (golden!("parent"), None),
    (golden!("parent-zero"), None),
    (golden!("ancestor-head"), None),
    (golden!("parent-tag"), None),
    (golden!("parent-second"), naming_nothing("main^2:dir/file")),
    (golden!("ancestor-past"), naming_nothing("main~3")),
    (golden!("ancestor-huge"), naming_nothing("main~2147483648")),
    (
      golden!("ancestor-pathless"),
      naming_nothing("main~2:dir/file"),
    ),
    (
      golden!("ancestor-blob"),
      naming_nothing("4cdb2265d30204be5463b38174b2e8e717982405~1"),
    ),
    (
      golden!("ancestor-ambiguous"),
      naming_nothing("67fd~1:dir/file"),
    ),
  ];
  let goldens: Vec<Golden<'_>> = cases
    .into_iter()
    .map(|((name, request, response), refusal)| (name, request, response, refusal))
    .collect();
  held_to_git(&fixture, &REFS, &goldens);
}

/// A-640 part 4, fifth slice (`peel_onion`, `repo_peel_to_type`, `deref_tag`; git's `fetch-sparse-peel-*`): do clone
/// `main` under `sparse:oid=` peels: `main^{tree}`, `main^{commit}`, `v1^{}` and `main^{/}` before `:dir/file`;
/// `v1^{tag}`; `main^{blob}` (a commit peels to its tree, no blob); an abbreviated blob's `^{blob}`; `main^{object}`;
/// `main^{foo}`; and `main^{commit}x}`. Expect: each answered as git answered it: the paths and the blob served, the
/// tag and the commits naming no blob, git's error for the tree met before its `fatal:` line, `^{foo}` naming nothing,
/// and the words after the last `^{` read as a prefix, so `^{commit}x}` names the commit.
#[test]
fn peels_are_read_as_git_reads_them() {
  let fixture = served_alike();
  let cases = [
    (golden!("peel-tree"), None),
    (golden!("peel-commit"), None),
    (golden!("peel-deref"), None),
    (golden!("peel-search-empty"), None),
    (golden!("peel-blob"), None),
    (golden!("peel-tag"), naming_no_blob(TAG)),
    (golden!("peel-object"), naming_no_blob(THIRD)),
    (golden!("peel-trailing"), naming_no_blob(THIRD)),
    (golden!("peel-blob-mismatch"), naming_nothing("main^{blob}")),
    (golden!("peel-unknown"), naming_nothing("main^{foo}")),
  ];
  let goldens: Vec<Golden<'_>> = cases
    .into_iter()
    .map(|((name, request, response), refusal)| (name, request, response, refusal))
    .collect();
  held_to_git(&fixture, &REFS, &goldens);
}

/// A-640 part 4, fifth slice (`get_oid_basic`'s log, `read_ref_at`, `interpret_branch_mark`, `branch_get_upstream`,
/// `branch_get_push`; git's `fetch-sparse-{reflog,prior,upstream,push}*`, recorded with no log, as a bare repository
/// keeps): do clone `main` under `sparse:oid=` `main@{1}`, `@{0}` and `HEAD@{0}` before `:dir/file`; `@{1}`,
/// `@{yesterday}`, `@{garbage}` and `@{jan}`; `@{-1}` before `:dir/file` and `main@{-1}`; and the marks
/// `main@{upstream}` (alone and before `:dir/file`), `@{u}`, `nosuch@{u}`, `main@{push}` and `nosuch@{push}`. Expect:
/// each answered as git answered it: `@{0}` serving `HEAD`'s commit's path, the named logs and the branches checked
/// out before naming nothing, git's death of the empty log for a count and the dates, nothing for a word that is no
/// date, and git's deaths for the marks.
#[test]
fn a_log_entry_and_a_branch_mark_are_read_as_git_reads_them_with_no_log() {
  let fixture = served_alike();
  let empty = Some(Refusal::SparseEmptyLog {
    refname: MAIN_BRANCH.to_vec(),
  });
  let upstream = Some(Refusal::SparseNoUpstream {
    branch: b"main".to_vec(),
  });
  let nosuch = Some(Refusal::SparseNoBranch {
    branch: b"nosuch".to_vec(),
  });
  let cases = [
    (golden!("reflog-head-zero"), None),
    (
      golden!("reflog-branch"),
      naming_nothing("main@{1}:dir/file"),
    ),
    (
      golden!("reflog-named-head"),
      naming_nothing("HEAD@{0}:dir/file"),
    ),
    (golden!("reflog-head-one"), empty.clone()),
    (golden!("reflog-head-date"), empty.clone()),
    (golden!("reflog-head-month"), empty),
    (golden!("reflog-head-word"), naming_nothing("@{garbage}")),
    (golden!("prior"), naming_nothing("@{-1}:dir/file")),
    (golden!("prior-late"), naming_nothing("main@{-1}")),
    (golden!("upstream"), upstream.clone()),
    (golden!("upstream-path"), upstream.clone()),
    (golden!("upstream-short"), upstream.clone()),
    (golden!("push"), upstream),
    (golden!("upstream-nosuch"), nosuch.clone()),
    (golden!("push-nosuch"), nosuch),
  ];
  let goldens: Vec<Golden<'_>> = cases
    .into_iter()
    .map(|((name, request, response), refusal)| (name, request, response, refusal))
    .collect();
  held_to_git(&fixture, &REFS, &goldens);
}

/// Why a clone of `main` under `sparse:oid=<name>` is refused, served with `head` as the default branch.
fn refused_with_head(fixture: &served::Served, name: &str, head: &[u8]) -> Option<Refusal> {
  let filter = [b"filter sparse:oid=".as_slice(), name.as_bytes()].concat();
  let request = command("fetch", &[&naming("want", THIRD), &filter, b"done"]);
  let (_, answer) = answer_of(fixture, &REFS, head, Version::V2, Asked::Request(&request));
  answer.refusal().cloned()
}

/// A-640 part 4, fifth slice (git's code, past the recordings: `approxidate_alpha` and `match_string`, `at_mark`'s
/// `strncasecmp`, `interpret_branch_mark` past a `:`, `interpret_empty_at`, `branch_get_upstream` with no branch): do
/// clone `main` under `sparse:oid=` `HEAD`'s log at `@{last}`, `@{NoW}`, `@{3}`, `@{Sept}` and `@{tea-time}`, and at
/// `@{lastly}`, `@{se}` and `@{nowhere}`; `main@{U}`, `@@{u}` and `x:y@{u}`; and `@{u}` with a tag as the default
/// branch. Expect: git's death of the empty log for the first five (a number's name, a special word in any case, a
/// digit, a month's first letters, a special word before a dash) and nothing for the next three (a word past `last`,
/// two of a month's letters, a word past `now`); no upstream configured for `main` for the marks in capitals and after
/// `@` alone, nothing for a mark after a `:`; and `HEAD` naming no branch.
#[test]
fn dates_marks_and_a_head_on_no_branch_are_read_as_gits_code_reads_them() {
  let fixture = served_alike();
  let refused = |name: &str| refused_with_head(&fixture, name, MAIN_REF);
  let empty = Some(Refusal::SparseEmptyLog {
    refname: MAIN_BRANCH.to_vec(),
  });
  let dated = ["@{last}", "@{NoW}", "@{3}", "@{Sept}", "@{tea-time}"].map(refused);
  let undated = ["@{lastly}", "@{se}", "@{nowhere}"].map(refused);
  assert_eq!(
    (dated, undated),
    (
      [(); 5].map(|()| empty.clone()),
      ["@{lastly}", "@{se}", "@{nowhere}"].map(naming_nothing)
    )
  );
  let upstream = Some(Refusal::SparseNoUpstream {
    branch: b"main".to_vec(),
  });
  assert_eq!(
    [
      refused("main@{U}"),
      refused("@@{u}"),
      refused("x:y@{u}"),
      refused_with_head(&fixture, "@{u}", b"refs/tags/v1"),
    ],
    [
      upstream.clone(),
      upstream,
      naming_nothing("x:y@{u}"),
      Some(Refusal::SparseDetached)
    ]
  );
}
