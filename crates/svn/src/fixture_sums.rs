//! The recorded fixtures (`crates/svn/fixtures`), each directory's `SHA256SUMS` held to the bytes beside it: a sum that
//! no longer matches its file, a sum whose file is gone and a file no sum names each fail, so a fixture edited or added
//! by hand without its sum made again is caught here (g297's finding of 2026-11-19: `reads/sessions.txt` gained a line
//! in bdc73e368 and kept its old sum, and no test read the sums but for the names they list).

use std::collections::BTreeSet;
use std::path::Path;

/// The fixtures' directory.
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

/// Each fixture directory and the files in it its recorder reads and never writes, which its `SHA256SUMS` leaves out:
/// the recorder itself, and the history `report`'s recorder builds its repository from (its `inputs`).
const RECORDER_INPUTS: [(&str, &[&str]); 7] = [
  ("commit", &["make-fixtures.py"]),
  ("dump", &["make-fixtures.sh"]),
  ("fold", &["make-fold.py"]),
  ("handshake", &["make-fixtures.py"]),
  ("hooks", &["make-fixtures.py"]),
  ("reads", &["make-fixtures.py"]),
  ("report", &["make-fixtures.py", "history.txt"]),
];

/// `bytes`' SHA-256 in lowercase hexadecimal, as `shasum -a 256` writes it.
fn sha256_hex(bytes: &[u8]) -> String {
  aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// Every file beneath `directory`, named by its path from `root` with `/` between its parts, as `SHA256SUMS` names one.
fn files_beneath(root: &Path, directory: &Path) -> Vec<String> {
  let mut files = Vec::new();
  for entry in std::fs::read_dir(directory).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      files.extend(files_beneath(root, &path));
    } else {
      let relative = path.strip_prefix(root).unwrap();
      let parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
      files.push(parts.join("/"));
    }
  }
  files
}

/// What does not hold in the fixture directory `name`: a file its sums name that is gone or holds other bytes, and a
/// file no sum names that is neither the sums nor one of its recorder's `inputs`.
fn problems_of(name: &str, inputs: &[&str]) -> Vec<String> {
  let directory = Path::new(FIXTURES).join(name);
  let sums = std::fs::read_to_string(directory.join("SHA256SUMS")).unwrap();
  let mut named = BTreeSet::new();
  let mut problems = Vec::new();
  for line in sums.lines() {
    let (sum, file) = line.split_once("  ").unwrap();
    named.insert(file.to_owned());
    match std::fs::read(directory.join(file)) {
      Ok(bytes) if sha256_hex(&bytes) == sum => {}
      Ok(_) => problems.push(format!("{name}/{file}: its bytes are not its sum's")),
      Err(error) => problems.push(format!(
        "{name}/{file}: named by a sum, not readable: {error}"
      )),
    }
  }
  for file in files_beneath(&directory, &directory) {
    if file != "SHA256SUMS" && !inputs.contains(&file.as_str()) && !named.contains(&file) {
      problems.push(format!("{name}/{file}: no sum names it"));
    }
  }
  problems
}

/// A-613 part 2 (the fixtures' sums): read every fixture directory; expect each to be one a recorder's inputs are named
/// for, each file its `SHA256SUMS` names to hold the bytes of that SHA-256, and every other file but the sums and the
/// recorder's inputs to be named by a sum.
#[test]
fn every_fixture_file_holds_its_recorded_sum() {
  let mut directories: Vec<String> = std::fs::read_dir(FIXTURES)
    .unwrap()
    .map(|entry| entry.unwrap().path())
    .filter(|path| path.is_dir())
    .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
    .collect();
  directories.sort();
  let known: Vec<String> = RECORDER_INPUTS
    .iter()
    .map(|(name, _)| (*name).to_owned())
    .collect();
  assert_eq!(directories, known, "the fixture directories");
  let problems: Vec<String> = RECORDER_INPUTS
    .iter()
    .flat_map(|(name, inputs)| problems_of(name, inputs))
    .collect();
  assert!(problems.is_empty(), "{}", problems.join("\n"));
}
