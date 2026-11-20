//! A-640 part 4, third slice: a `sparse:oid` filter's `<blob-ish>` resolved as git's `get_oid_with_context` resolves it
//! in a repository with no work tree, through upload-pack's entry point over the fixture repository (A-582's fixtures,
//! loaded into A-614's store); and the lines of a `pack-objects` that dies relayed as git's server relays them.

use super::{MAIN_REF, WholeObjects, WholePack, answer_v2, command, naming, pkt, serving};
use crate::Admission;
use crate::served::{self, REFS, TAG, THIRD, id};
use crate::upload::answer::Said;
use crate::upload::{Answer, Refusal, Written};

/// Format: the root tree of `main`'s head (`fixtures/objects.txt`).
const ROOT: &str = "0351da7889aaa5309979d9a4e742f54ac1750e44";

/// Format: the tree `dir`, the same under `main` and under the tag's commit.
const DIR: &str = "cc01dbca1db1ab97354bc849d5631a785fcb68ab";

/// Format: the blob `dir/file`, `deep`: a pattern that matches no path.
const DEEP: &str = "4cdb2265d30204be5463b38174b2e8e717982405";

/// Format: an object the repository does not hold.
const UNHELD: &str = "3333333333333333333333333333333333333333";

/// Format: git's band-3 words for a pack that could not be finished, after their band byte.
const ABORT: &[u8] = b"\x03aborting due to possible repository corruption on the remote side.";

/// Format: the most bytes git's server relays in one band-2 packet (`create_pack_file`'s `progress[128]`).
const RELAYED: usize = 128;

/// Format: each `<blob-ish>` naming the blob `dir/file` that this server resolves: `main`'s path; the branch's full
/// name and a shorter one; `HEAD` and `@`; the tag `v1`, peeled through its commit; the commit's and the tree's own
/// names; the blob's own name in either case; and `git describe`'s name for it, whose ref names no ref.
const SERVED: [&str; 11] = [
  "main:dir/file",
  "refs/heads/main:dir/file",
  "heads/main:dir/file",
  "HEAD:dir/file",
  "@:dir/file",
  "v1:dir/file",
  "d0f175d3390380c935e9b56e0cebb715995d838d:dir/file",
  "cc01dbca1db1ab97354bc849d5631a785fcb68ab:file",
  "4cdb2265d30204be5463b38174b2e8e717982405",
  "4CDB2265D30204BE5463B38174B2E8E717982405",
  "v1-1-g4cdb2265",
];

/// Format: each `<blob-ish>` naming an object that is no blob the store holds, and the object: a commit, by a branch
/// alone; the root tree, by an empty path; the tree `dir`, with and without a trailing `/`; the annotated tag, by its
/// name alone; and a full name the store does not hold.
const UNPARSED: [(&str, &str); 6] = [
  ("main", THIRD),
  ("main:", ROOT),
  ("main:dir", DIR),
  ("main:dir/", DIR),
  ("v1", TAG),
  (UNHELD, UNHELD),
];

/// Format: each `<blob-ish>` that names nothing, as git finds nothing for it: a ref that is not there, and one before a
/// relative path, which is then never read; a path that is not there, by `side`'s tree too; an empty component, first
/// or within; a path through a file, and through a blob named as the revision; a trailing `/` after a file; an index
/// path, with a stage and without, and `:/` alone, as a repository with no work tree holds no index; a full name the
/// store does not hold before a path; a name like `git describe`'s whose ref part is no `<ref>-<count>`; and the blob's
/// abbreviated name before a path, which walks no tree.
const NOTHING: [&str; 17] = [
  "nosuch:dir/file",
  "nosuch:./dir/file",
  "main:nosuch",
  "side:dir/file",
  "main:/dir/file",
  "main:dir//file",
  "main:/",
  "main:text/x",
  "4cdb2265d30204be5463b38174b2e8e717982405:x",
  "main:dir/file/",
  ":dir/file",
  ":0:dir/file",
  ":/",
  "3333333333333333333333333333333333333333:dir/file",
  "nosuch",
  "nosuch-gabcd",
  "4cdb2265:x",
];

/// Format: each `<blob-ish>` whose path is relative, after a revision that resolves (a full name the store does not
/// hold among them) or after a `:`, with a stage and without.
const RELATIVE: [&str; 5] = [
  "main:./dir/file",
  "main:../dir/file",
  "3333333333333333333333333333333333333333:./dir/file",
  ":./dir/file",
  ":0:../dir/file",
];

/// Format: each `<blob-ish>` of a form git reads that this server does not resolve yet: an ancestor, a parent, a
/// numbered parent, a peel, a reflog entry, a branch checked out before, an upstream, and a search of commit messages.
const FORMS: [&str; 8] = [
  "main~1:dir/file",
  "main^:dir/file",
  "main^2:dir/file",
  "main^{tree}:dir/file",
  "main@{1}:dir/file",
  "@{-1}:dir/file",
  "main@{upstream}",
  ":/third",
];

/// The answer to a clone of `main` under `sparse:oid=<name>`, and why it was refused, if it was.
fn cloned(fixture: &served::Served, name: &[u8]) -> (Vec<u8>, Option<Refusal>) {
  let filter = [b"filter sparse:oid=".as_slice(), name].concat();
  let request = command("fetch", &[&naming("want", THIRD), &filter, b"done"]);
  let (bytes, answer) = answer_v2(fixture, &request);
  (bytes, answer.refusal().cloned())
}

/// The band-2 packets git's server relays `line` in: one a read of at most `RELAYED` bytes.
fn relayed(line: &[u8]) -> Vec<u8> {
  line
    .chunks(RELAYED)
    .flat_map(|read| pkt(&[b"\x02".as_slice(), read].concat()))
    .collect()
}

/// A-640 part 4 (`get_oid_with_context_1`, `get_oid_basic`, `repo_dwim_ref`, `interpret_empty_at`,
/// `read_object_with_reference`, `get_tree_entry`): do clone `main` under `sparse:oid=` each form of `<blob-ish>` this
/// server resolves that names the blob `dir/file`. Expect: each answered, not refused, with the same bytes as by
/// `main:dir/file`, whose answer stock git's is held to (`fetch-sparse-main-dir-file`).
#[test]
fn every_form_of_a_blob_ish_this_server_resolves_serves_the_same_clone() {
  let fixture = served::served();
  let (expected, _) = cloned(&fixture, b"main:dir/file");
  for name in SERVED {
    let (bytes, refusal) = cloned(&fixture, name.as_bytes());
    assert_eq!((refusal, bytes == expected), (None, true), "{name}");
  }
}

/// A-640 part 4 (`do_read_blob`: no blob, no patterns; "unable to parse sparse filter data in %s"): do clone `main`
/// under `sparse:oid=` each `<blob-ish>` naming a commit, a tree, the tag or an object not held. Expect: each refused
/// as git's `pack-objects` dies of it, naming that object.
#[test]
fn a_blob_ish_that_names_no_blob_the_store_holds_is_refused_naming_its_object() {
  let fixture = served::served();
  for (name, object) in UNPARSED {
    let (_, refusal) = cloned(&fixture, name.as_bytes());
    assert_eq!(
      refusal,
      Some(Refusal::SparseData { id: id(object) }),
      "{name}"
    );
  }
}

/// A-640 part 4 (`get_oid_with_context_1` finds nothing: "unable to access sparse blob in '%s'"; `find_tree_entry`): do
/// clone `main` under `sparse:oid=` each `<blob-ish>` that names nothing in a repository with no work tree. Expect:
/// each refused as naming nothing, with the name as sent.
#[test]
fn a_blob_ish_that_names_nothing_is_refused_with_its_name() {
  let fixture = served::served();
  for name in NOTHING {
    let (_, refusal) = cloned(&fixture, name.as_bytes());
    let expected = Refusal::SparseBlob {
      name: name.as_bytes().to_vec(),
    };
    assert_eq!(refusal, Some(expected), "{name}");
  }
}

/// A-640 part 4 (`resolve_relative_path` with no work tree): do clone `main` under `sparse:oid=` each `<blob-ish>`
/// whose path is relative. Expect: each refused as relative.
#[test]
fn a_relative_path_is_refused_as_git_refuses_it_with_no_work_tree() {
  let fixture = served::served();
  for name in RELATIVE {
    let (_, refusal) = cloned(&fixture, name.as_bytes());
    assert_eq!(refusal, Some(Refusal::SparseRelative), "{name}");
  }
}

/// A-640 part 4 (`get_oid_1`, `peel_onion`, `get_oid_basic`'s reflog, `get_oid_oneline`): do clone `main` under
/// `sparse:oid=` each `<blob-ish>` of a form git reads that this server does not resolve yet. Expect: each refused by
/// that kind, with the name as sent.
#[test]
fn a_form_this_server_does_not_resolve_is_refused_by_its_kind() {
  let fixture = served::served();
  for name in FORMS {
    let (_, refusal) = cloned(&fixture, name.as_bytes());
    let expected = Refusal::SparseForm {
      name: name.as_bytes().to_vec(),
    };
    assert_eq!(refusal, Some(expected), "{name}");
  }
}

/// A-640 part 4 (the death's answer, as `fetch-sparse-*` holds git's): do clone `main` under `sparse:oid=` a relative
/// path and a form this server does not resolve. Expect: the packfile section's header, then the `fatal:` line of each
/// on band 2 in reads of 128 bytes, git's for the first and git's for a name that names nothing with what this server
/// resolves after it for the second, then git's band-3 words, and no flush.
#[test]
fn a_relative_path_and_a_form_not_resolved_are_answered_in_their_words() {
  let fixture = served::served();
  let cases = [
    (
      "main:./dir/file",
      "relative path syntax can't be used outside working tree".to_owned(),
    ),
    (
      "main~1:dir/file",
      "unable to access sparse blob in 'main~1:dir/file': this server resolves an object's full or abbreviated \
       name, git describe's name or a ref, alone or with :<path>"
        .to_owned(),
    ),
  ];
  for (name, words) in cases {
    let (bytes, _) = cloned(&fixture, name.as_bytes());
    let line = format!("fatal: {words}\n");
    let expected = [pkt(b"packfile\n"), relayed(line.as_bytes()), pkt(ABORT)].concat();
    assert_eq!(
      bytes,
      expected,
      "{name}: {}",
      String::from_utf8_lossy(&bytes)
    );
  }
}

/// What `Answer::died` answers once written whole, with `head` before it and with a band or none, from `fixture`'s
/// repository, and why it was refused.
fn died(fixture: &served::Served, refusal: Refusal, banded: bool) -> (Vec<u8>, Option<Refusal>) {
  let served = serving(fixture, &REFS, MAIN_REF);
  let mut source = WholeObjects::new(&fixture.objects);
  let mut admission = Admission::new(u64::MAX);
  let mut answer: Answer<WholePack> = Answer::died(
    b"head".to_vec(),
    Said::default(),
    refusal,
    banded,
    &mut admission,
  )
  .unwrap();
  let mut bytes = Vec::new();
  assert_eq!(
    answer.write(&served, &mut source, &mut admission, &mut bytes, usize::MAX),
    Written::End
  );
  (bytes, answer.refusal().cloned())
}

/// A-640 part 4 (`add_patterns_from_blob_to_list` warns of a blob past git's limit before `pack-objects` dies, and
/// `create_pack_file` relays each write it reads): do answer a pack whose filter's blob is past git's limit, with a
/// band and with none. Expect: with a band, the warning's line and then the `fatal:` line, each in its own packet, then
/// git's band-3 words; with none, what came before the pack alone; refused with why either way.
#[test]
fn a_blob_past_gits_limit_is_warned_of_then_refused_as_git_does() {
  let fixture = served::served();
  let large = Refusal::SparseTooLarge { id: id(DEEP) };
  let warning = format!("warning: ignoring excessively large pattern blob: {DEEP}\n");
  let fatal = format!("fatal: unable to parse sparse filter data in {DEEP}\n");
  let expected = [
    b"head".to_vec(),
    relayed(warning.as_bytes()),
    relayed(fatal.as_bytes()),
    pkt(ABORT),
  ]
  .concat();
  assert_eq!(
    [
      died(&fixture, large.clone(), true),
      died(&fixture, large.clone(), false)
    ],
    [
      (expected, Some(large.clone())),
      (b"head".to_vec(), Some(large))
    ]
  );
}

/// A-640 part 4 (`vfreportf` cuts a line to 4096 bytes, its newline last; `create_pack_file` relays a read of 128
/// bytes a packet): do answer a pack whose `<blob-ish>`, 5000 bytes long, names nothing. Expect: its `fatal:` line cut
/// to 4096 bytes, the name's closing quote cut with it, in 32 full packets, the newline last; then git's band-3 words.
#[test]
fn a_line_past_gits_report_is_cut_to_it_and_relayed_in_full_reads() {
  let fixture = served::served();
  let name = vec![b'x'; 5000];
  let (bytes, _) = died(&fixture, Refusal::SparseBlob { name: name.clone() }, true);
  let words = [
    b"fatal: unable to access sparse blob in '".as_slice(),
    &name,
  ]
  .concat();
  let line = [words.get(..4095).unwrap(), b"\n"].concat();
  let expected = [b"head".to_vec(), relayed(&line), pkt(ABORT)].concat();
  assert_eq!(
    (line.len(), line.len() / RELAYED, bytes),
    (4096, 32, expected)
  );
}
