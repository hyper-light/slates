//! The FSFS view (A-645 route A, A-738 P1) against stock Subversion: the repository the golden dump makes, laid out a
//! revision at a time and written as FSFS's files into a scratch directory, is read by stock `svnadmin` and `svnlook`
//! as the repository the dump was taken from. `svnadmin verify` passes it, `svnadmin dump` (whole and with deltas)
//! gives back `full.dump` and `deltas.dump` byte for byte, and `svnlook changed --copy-info` gives back `changed.txt`,
//! the stock tools' own accounts recorded when the vectors were made (`fixtures/dump/make-fixtures.sh`, Subversion
//! 1.15.0). Hosts without stock Subversion skip that leg loudly; the layout's own laws run everywhere.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

use std::process::Command;

use slates_svn::dump::{Admission, Uuid};
use slates_svn::fsfs::{self, Advance, RevisionFile, View};
use slates_svn::index::RevisionRecord;
use slates_svn::migration::{Importer, Importing, Repository, Room};

/// `svnadmin dump -q` of the vectors' repository.
const FULL: &[u8] = include_bytes!("../fixtures/dump/full.dump");

/// `svnadmin dump -q --deltas` of the same repository.
const DELTAS: &[u8] = include_bytes!("../fixtures/dump/deltas.dump");

/// `svnlook changed --copy-info` of every revision but 0, each after an `r<N>` line.
const CHANGED: &[u8] = include_bytes!("../fixtures/dump/changed.txt");

/// A room no test history comes near.
/// Shape: 2^20 index entries and 2^30 bytes, past the vectors.
const ROOMY: Room = Room {
  entries: 1 << 20,
  bytes: 1 << 30,
};

/// An admission past every line and window of the vectors.
/// Shape: 2^20 bytes, past svndiff's largest window.
const ADMISSION: Admission = Admission {
  buffer_bytes: 1 << 20,
};

/// The most bytes one read of a revision file asks for: small, so a read crosses runs.
/// Shape: under the vectors' smallest text past a few bytes.
const READ_BYTES: usize = 7;

/// Revision 0's file as `svnadmin create` writes it for format 6: an empty root and its empty entries list.
/// Format: `fs_fs.c` `write_revision_zero` before format 7.
const REVISION_ZERO: &[u8] = b"PLAIN\nEND\nENDREP\nid: 0.0.r0/17\ntype: dir\ncount: 0\n\
text: 0 0 4 4 2d2977d1c96f487abe4a1e202dd03b4e\ncpath: /\n\n\n17 107\n";

/// The repository `dump` makes.
fn imported(dump: &[u8]) -> Repository {
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-000000000738").unwrap();
  let mut repository = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOMY).unwrap();
  let mut importer = Importer::new(ADMISSION);
  let mut rest = dump;
  loop {
    match importer.import(&mut repository, rest, true).unwrap() {
      Importing::Done => return repository,
      Importing::NeedMore { .. } => panic!("the whole dump was given"),
      Importing::Commit { consumed, record } => {
        rest = &rest[consumed..];
        repository.commit_record(*record).unwrap();
      }
    }
  }
}

/// `repository`'s view laid out `budget` node-revisions a call, and the calls it took.
fn laid_out(repository: &Repository, budget: usize) -> (View, usize) {
  let mut view = View::new();
  let mut calls = 0;
  loop {
    calls += 1;
    if view
      .advance(repository.index(), repository, budget)
      .unwrap()
      == Advance::Done
    {
      return (view, calls);
    }
  }
}

/// Every byte of `file`, read [`READ_BYTES`] at a time.
fn read_whole(file: &RevisionFile, repository: &Repository) -> Vec<u8> {
  let mut bytes = Vec::new();
  loop {
    let offset = bytes.len() as u64;
    if file
      .read(repository, offset, READ_BYTES, &mut bytes)
      .unwrap()
      == 0
    {
      break;
    }
  }
  assert_eq!(bytes.len() as u64, file.len());
  bytes
}

/// Revision `revision`'s file of `view`, whole.
fn revision_bytes(view: &View, repository: &Repository, revision: u64) -> Vec<u8> {
  let file = view
    .revision_file(repository.index(), repository, revision)
    .unwrap();
  read_whole(&file, repository)
}

/// A-738 P1: an empty repository's view is the revision 0 `svnadmin create` writes, byte for byte.
#[test]
fn revision_zero_is_the_file_svnadmin_create_writes() {
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-000000000738").unwrap();
  let repository = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOMY).unwrap();
  let (view, _) = laid_out(&repository, 1);
  assert_eq!(revision_bytes(&view, &repository, 0), REVISION_ZERO);
  assert_eq!(
    view.revision_file(repository.index(), &repository, 1),
    Err(fsfs::ViewRefusal::NotLaidOut { revision: 1 })
  );
}

/// A-738 P1: laid out one node-revision a call, the layout takes a call a revision and makes the files a single call
/// makes; in every file the trailer names the root's header and the changed paths, and each header's ID names its own
/// revision and offset.
#[test]
fn a_budget_slices_the_layout_by_revision_and_every_header_names_its_own_place() {
  let repository = imported(FULL);
  let youngest = repository.youngest();
  let (sliced, calls) = laid_out(&repository, 1);
  let (whole, one) = laid_out(&repository, usize::MAX);
  assert_eq!(one, 1);
  assert_eq!(calls as u64, youngest + 1, "a revision a call");
  for revision in 0..=youngest {
    let bytes = revision_bytes(&sliced, &repository, revision);
    assert_eq!(
      bytes,
      revision_bytes(&whole, &repository, revision),
      "r{revision}"
    );
    assert_eq!(sliced.revision_length(revision), Some(bytes.len() as u64));
    assert_the_trailer_names_the_root_and_the_changes(&bytes, revision);
    assert_every_header_names_its_own_place(&bytes, revision);
  }
}

/// Revision `revision`'s file `bytes`: its trailer names the root's header (an ID naming this revision and offset, its
/// `cpath` the root) and where the changed paths end.
fn assert_the_trailer_names_the_root_and_the_changes(bytes: &[u8], revision: u64) {
  let text = String::from_utf8_lossy(bytes);
  let body = text.strip_suffix('\n').unwrap();
  let (_, trailer) = body.rsplit_once('\n').unwrap();
  let (root, changes) = trailer.split_once(' ').unwrap();
  let root: usize = root.parse().unwrap();
  let changes: usize = changes.parse().unwrap();
  let root_header = &bytes[root..];
  assert!(
    root_header.starts_with(b"id: "),
    "r{revision}: the root's header at {root}"
  );
  let root_line =
    String::from_utf8_lossy(root_header.split(|byte| *byte == b'\n').next().unwrap()).into_owned();
  assert!(
    root_line.ends_with(&format!(".r{revision}/{root}")),
    "r{revision}: {root_line}"
  );
  assert!(root_header.windows(8).any(|window| window == b"cpath: /"));
  assert!(
    changes <= bytes.len() && bytes[..changes].ends_with(b"\n\n"),
    "r{revision}"
  );
}

/// Every node-revision header in revision `revision`'s file `bytes` whose ID names this revision names its own offset;
/// there is at least one.
fn assert_every_header_names_its_own_place(bytes: &[u8], revision: u64) {
  let mut headers = 0;
  let mut start = 0;
  while let Some(found) = find(&bytes[start..], b"\nid: ") {
    let at = start + found + 1;
    let line = String::from_utf8_lossy(bytes[at..].split(|byte| *byte == b'\n').next().unwrap())
      .into_owned();
    if line.contains(&format!(".r{revision}/")) && !line.ends_with(&format!("/{at}")) {
      panic!("r{revision}: {line} lies at {at}");
    }
    headers += 1;
    start = at;
  }
  assert!(headers >= 1, "r{revision}: no header");
}

/// Where `needle` first lies in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack
    .windows(needle.len())
    .position(|window| window == needle)
}

/// Whether this host runs stock `svnadmin` and `svnlook`.
fn stock_tools() -> bool {
  ["svnadmin", "svnlook"].iter().all(|tool| {
    Command::new(tool)
      .arg("--version")
      .output()
      .is_ok_and(|output| output.status.success())
  })
}

/// A scratch directory in the build output (`CARGO_TARGET_TMPDIR`), named with the process id, removed when dropped
/// (A-50: a test's host files live in the build output, never `/tmp`).
struct Scratch {
  path: String,
}

impl Scratch {
  /// A fresh scratch directory named `name`.
  fn new(name: &str) -> Scratch {
    let path = format!(
      "{}/{name}-{}",
      env!("CARGO_TARGET_TMPDIR"),
      std::process::id()
    );
    let made = Command::new("mkdir").args(["-p", &path]).output().unwrap();
    assert!(made.status.success(), "mkdir -p {path}: {made:?}");
    Scratch { path }
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = Command::new("rm").args(["-rf", &self.path]).output();
  }
}

/// Writes `repository`'s view as FSFS's files beneath `root`.
#[allow(clippy::disallowed_methods)]
fn materialize(view: &View, repository: &Repository, root: &str) {
  let youngest = repository.youngest();
  for directory in fsfs::directories(youngest) {
    std::fs::create_dir_all(format!("{root}/{directory}")).unwrap();
  }
  for (path, bytes) in fsfs::fixed_files(repository.uuid().as_bytes(), youngest) {
    std::fs::write(format!("{root}/{path}"), bytes).unwrap();
  }
  for revision in 0..=youngest {
    std::fs::write(
      format!("{root}/{}", fsfs::revision_file_path(revision)),
      revision_bytes(view, repository, revision),
    )
    .unwrap();
    std::fs::write(
      format!("{root}/{}", fsfs::revision_properties_path(revision)),
      fsfs::revision_properties_file(repository, revision).unwrap(),
    )
    .unwrap();
  }
}

/// Runs stock `tool` with `arguments` and answers its standard output, failing on a refusal.
fn stock(tool: &str, arguments: &[&str]) -> Vec<u8> {
  let output = Command::new(tool).args(arguments).output().unwrap();
  assert!(
    output.status.success(),
    "{tool} {arguments:?}: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  output.stdout
}

/// A-738 P1: stock `svnadmin` and `svnlook` read the view of the golden dump's repository as the repository the dump
/// was taken from: it verifies, it dumps back to both recorded dumps byte for byte, and every revision's changed paths
/// are the recorded account.
#[test]
fn stock_svnadmin_and_svnlook_read_the_view_as_the_repository_the_dump_was_taken_from() {
  if !stock_tools() {
    eprintln!(
      "SKIPPED: stock svnadmin and svnlook are not on this host, so the view's stock reading is not checked"
    );
    return;
  }
  let repository = imported(FULL);
  let (view, _) = laid_out(&repository, 1);
  let scratch = Scratch::new("fsfs-view");
  let root = format!("{}/repository", scratch.path);
  materialize(&view, &repository, &root);
  stock("svnadmin", &["verify", "-q", &root]);
  let dumped = stock("svnadmin", &["dump", "-q", &root]);
  assert!(
    dumped == FULL,
    "svnadmin dump of the view:\n{}",
    String::from_utf8_lossy(&dumped)
  );
  let deltas = stock("svnadmin", &["dump", "-q", "--deltas", &root]);
  assert!(
    deltas == DELTAS,
    "svnadmin dump --deltas of the view differs from deltas.dump"
  );
  let mut changed = Vec::new();
  for revision in 1..=repository.youngest() {
    changed.extend_from_slice(format!("r{revision}\n").as_bytes());
    changed.extend(stock(
      "svnlook",
      &["changed", "--copy-info", "-r", &revision.to_string(), &root],
    ));
  }
  assert!(
    changed == CHANGED,
    "svnlook changed of the view:\n{}",
    String::from_utf8_lossy(&changed)
  );
}

/// The repository of `FULL`'s revisions before `revision`: the dump cut where that revision's record begins.
fn imported_before(revision: u64) -> Repository {
  let marker = format!("Revision-number: {revision}\n");
  let at = find(FULL, marker.as_bytes()).unwrap();
  imported(&FULL[..at])
}

/// The transaction each revision of `FULL` was, made again as a trial on the revision before it in a repository cut
/// there: its name, its files, and the repository and view it was made in.
struct Retried {
  name: String,
  files: fsfs::TransactionFiles,
  before: Repository,
  view: View,
}

/// Revision `revision`'s work of `full`, tried on the revision before it.
fn retried(full: &Repository, revision: u64) -> Retried {
  let before = imported_before(revision);
  let (view, _) = laid_out(&before, usize::MAX);
  let name = full
    .index()
    .transaction(revision)
    .unwrap()
    .unwrap()
    .to_owned();
  let work = RevisionRecord {
    transaction: name.clone(),
    changes: full.index().changes(revision).unwrap().to_vec(),
  };
  let trial = before.index().trial(revision - 1, &work).unwrap();
  let properties = full.revision_properties(revision).unwrap();
  let files = fsfs::transaction_files(
    &view,
    before.index(),
    &trial,
    &work.changes,
    &name,
    properties
      .iter()
      .map(|(name, value)| (name.as_slice(), value.as_slice()))
      .collect(),
    full,
  )
  .unwrap();
  Retried {
    name,
    files,
    before,
    view,
  }
}

/// A-738 part 2: every revision's work, tried as a transaction on the revision before it, makes a directory whose
/// root is `node.0.0` and whose changed paths are the revision's, each naming a transaction node or a committed one.
#[test]
fn each_revisions_work_makes_a_transaction_directory_rooted_at_node_0_0() {
  let full = imported(FULL);
  for revision in 1..=full.youngest() {
    let retried = retried(&full, revision);
    let directory = fsfs::transaction_path(&retried.name);
    let root = retried
      .files
      .files
      .iter()
      .find(|(path, _)| *path == format!("{directory}/node.0.0"))
      .unwrap_or_else(|| panic!("r{revision}: no root"));
    let root = String::from_utf8_lossy(&root.1).into_owned();
    assert!(
      root.starts_with(&format!("id: 0.0.t{}\n", retried.name)),
      "r{revision}: {root}"
    );
    let changes = retried
      .files
      .files
      .iter()
      .find(|(path, _)| *path == format!("{directory}/changes"))
      .unwrap();
    let lines = String::from_utf8_lossy(&changes.1)
      .lines()
      .filter(|line| line.contains(" false ") || line.contains(" true "))
      .count();
    assert_eq!(
      lines,
      full.index().changes(revision).unwrap().len(),
      "r{revision}"
    );
  }
}

/// Writes a retried transaction's directory and prototype file beneath `root`, its repository written there first.
#[allow(clippy::disallowed_methods)]
fn materialize_transaction(retried: &Retried, full: &Repository, root: &str) {
  materialize(&retried.view, &retried.before, root);
  for directory in &retried.files.directories {
    std::fs::create_dir_all(format!("{root}/{directory}")).unwrap();
  }
  for (path, bytes) in &retried.files.files {
    std::fs::write(format!("{root}/{path}"), bytes).unwrap();
  }
  std::fs::write(
    format!("{root}/{}", retried.files.prototype_path),
    read_whole(&retried.files.prototype, full),
  )
  .unwrap();
}

/// A-738 part 2: stock `svnlook -t` reads each revision's work, tried as a transaction on the revision before it, as
/// `svnlook -r` reads the revision itself: its changed paths with their copies, its author, date and log, its tree, its
/// changed directories and its diff.
#[test]
fn stock_svnlook_reads_each_revisions_work_as_a_transaction_as_it_reads_the_revision() {
  if !stock_tools() {
    eprintln!(
      "SKIPPED: stock svnlook is not on this host, so the transaction view's stock reading is not checked"
    );
    return;
  }
  let full = imported(FULL);
  let (view, _) = laid_out(&full, usize::MAX);
  let scratch = Scratch::new("fsfs-transaction");
  let committed = format!("{}/committed", scratch.path);
  materialize(&view, &full, &committed);
  for revision in 1..=full.youngest() {
    let retried = retried(&full, revision);
    let root = format!("{}/before-{revision}", scratch.path);
    materialize_transaction(&retried, &full, &root);
    let number = revision.to_string();
    for command in [
      &["changed", "--copy-info"][..],
      &["info"],
      &["tree"],
      &["dirs-changed"],
      &["diff"],
    ] {
      let mut asked = command.to_vec();
      asked.extend(["-t", &retried.name, &root]);
      let transaction = String::from_utf8_lossy(&stock("svnlook", &asked)).into_owned();
      let mut asked = command.to_vec();
      asked.extend(["-r", &number, &committed]);
      let committed = String::from_utf8_lossy(&stock("svnlook", &asked)).into_owned();
      let transaction = transaction.replace(
        &format!("(txn {})", retried.name),
        &format!("(rev {revision})"),
      );
      assert_eq!(transaction, committed, "r{revision}: svnlook {command:?}");
    }
  }
}
