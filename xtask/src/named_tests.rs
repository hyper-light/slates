//! Doc truth for the tables that judge each platform by its tests (CLAUDE.md §4, "doc-truth tests"; step 1 of the
//! VFS and DAX plan, 2026-10-31).
//!
//! `docs/wip/platforms.md` judges every function on every platform, and the DAX matrix in `docs/wip/virtiofs.md` §8
//! (A-164) every route in every environment. A cell that says "built" or "proven" names the lane, the test or the run
//! behind it. Nothing compiles a document, so a test renamed or removed left its cells claiming a proof the tree no
//! longer had, and nothing said so. This check reads every table row of both documents (§8 alone of `virtiofs.md`),
//! takes each test and each path of this tree a cell names, and looks for it: a test as a function that a source under
//! `crates/` or `xtask/` defines, a path as a file or a directory. One the tree lacks is refused with its document, its
//! line and its name, by `cargo xtask structural` and by this module's test, so the push that orphans a cell fails CI.
//!
//! Each document carries a generated list, between markers, of the tests its tables name and the files defining them,
//! with the other projects' tests and paths it cites. The test holds each list equal to the tree. The `--ignored`
//! writer `regenerate_the_named_tests` rewrites the lists, and refuses while any named test or path is missing: a
//! missing test is its cell's to fix, not the list's.
//!
//! **What a named test is.** A backticked identifier of lowercase words joined by underscores, at least
//! [`TEST_NAME_MIN_WORDS`] of them, alone or as the last part of a `::` path. slates names a test as a sentence ("do X,
//! expect Y", CLAUDE.md §4); the other identifiers the tables cite are names of functions and options
//! (`get_shared_memory_region`, `nfs_revalidate_mapping`, `map_read`).
//!
//! **What is another project's.** A test is another project's when its owner's possessive comes right before it: the
//! owner's name, an apostrophe, a space, then the backticked name. The owners are [`OTHER_PROJECTS`]. A path is
//! another project's when it lies under a crate directory this tree does not have, as Shards' `crates/vmm` does. Both
//! are listed and not looked for, so a misspelled crate or a misattributed test shows in the list a reviewer reads.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{Failure, rust_sources};

/// Shape: the fewest words a named test has. Over both tables on 2026-10-31, the backticked identifiers that are not
/// tests had at most four words (`get_shared_memory_region`) and the shortest test name had nine
/// (`a_winfsp_folder_bound_into_a_process_isolated_container`); five sits in the gap with room on either side.
const TEST_NAME_MIN_WORDS: usize = 5;

/// Format: the projects whose tests the tables cite by name, each with its possessive right before the name. Shards
/// is the microVM manager whose guests prove the DAX matrix's Shards rows.
const OTHER_PROJECTS: [&str; 1] = ["Shards"];

/// Format: the marker a document's generated list starts after.
const LIST_BEGIN: &str = "<!-- named-tests:begin -->\n";

/// Format: the marker a document's generated list ends before. The indentation in front of it is the list's own,
/// since §8's list sits inside a numbered item.
const LIST_END: &str = "<!-- named-tests:end -->";

/// A document whose tables name tests, and the part of it they are in.
struct Document {
  /// The document's path from the workspace root.
  path: &'static str,
  /// The heading the part starts at; `None` is the document's start.
  from: Option<&'static str>,
  /// The heading the part ends before; `None` is the document's end.
  to: Option<&'static str>,
  /// The indentation of the generated list's lines.
  indent: &'static str,
}

/// Format: the documents this check reads, and where in each.
const DOCUMENTS: [Document; 2] = [
  Document {
    path: "docs/wip/platforms.md",
    from: None,
    to: None,
    indent: "",
  },
  Document {
    path: "docs/wip/virtiofs.md",
    from: Some("## 8."),
    to: Some("## 9."),
    indent: "   ",
  },
];

/// What a table cell names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Named {
  /// A test of this tree, by its function's name.
  Test(String),
  /// Another project's test: its owner and its name.
  OthersTest { owner: &'static str, name: String },
  /// A path in this tree.
  Path(String),
  /// A path under a crate directory this tree does not have.
  OthersPath(String),
}

/// One thing a table row names, with the row's line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Citation {
  /// The row's line in its document, counted from 1.
  line: usize,
  /// What the row names.
  named: Named,
}

/// Refuses each test or path a table names that the tree lacks, with its document and line; with none missing, refuses
/// a generated list that differs from the tree.
pub(super) fn check(root: &Path, violations: &mut Vec<String>) -> Result<(), Failure> {
  let defined = defined_functions(root)?;
  for document in &DOCUMENTS {
    let text = std::fs::read_to_string(root.join(document.path))?;
    let named = citations(document, &text, &|dir| root.join(dir).is_dir());
    let absent = missing(document, &named, &defined, &|path| root.join(path).exists());
    if !absent.is_empty() {
      violations.extend(absent);
    } else if generated_list(&text) != Some(rendered_list(document, &named, &defined).as_str()) {
      violations.push(format!(
        "{}'s list of named tests differs from the tree; run `cargo test -p xtask -- --ignored regenerate_the_named_tests`",
        document.path
      ));
    }
  }
  Ok(())
}

/// Every test and path the table rows in `document`'s part of `text` name, in order. `has_crate` says whether this
/// tree has a crate directory (`crates/vfs`, `xtask`).
fn citations(document: &Document, text: &str, has_crate: &dyn Fn(&str) -> bool) -> Vec<Citation> {
  let mut found = Vec::new();
  let mut inside = document.from.is_none();
  for (index, line) in text.lines().enumerate() {
    if document.from.is_some_and(|from| line.starts_with(from)) {
      inside = true;
    }
    if document.to.is_some_and(|to| line.starts_with(to)) {
      inside = false;
    }
    if !inside || !line.trim_start().starts_with('|') {
      continue;
    }
    let line_number = index.saturating_add(1);
    found.extend(
      row_names(line, has_crate)
        .into_iter()
        .map(|named| Citation {
          line: line_number,
          named,
        }),
    );
  }
  found
}

/// What one table row names: each backticked span that is a test's name or a path under a crate directory.
fn row_names(row: &str, has_crate: &dyn Fn(&str) -> bool) -> Vec<Named> {
  let mut named = Vec::new();
  let mut before = "";
  let mut in_span = false;
  for piece in row.split('`') {
    if !in_span {
      before = piece;
    } else if let Some(name) = test_name(piece) {
      named.push(match owner_before(before) {
        Some(owner) => Named::OthersTest {
          owner,
          name: name.to_owned(),
        },
        None => Named::Test(name.to_owned()),
      });
    } else if let Some(crate_dir) = crate_of_path(piece) {
      named.push(if has_crate(crate_dir) {
        Named::Path(piece.to_owned())
      } else {
        Named::OthersPath(piece.to_owned())
      });
    }
    in_span = !in_span;
  }
  named
}

/// The test a backticked span names: an identifier of at least [`TEST_NAME_MIN_WORDS`] lowercase words joined by
/// underscores, alone or as the last part of a `::` path.
fn test_name(span: &str) -> Option<&str> {
  let identifier_parts = span.split("::").all(|part| {
    !part.is_empty()
      && part
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
  });
  let last = span.rsplit("::").next()?;
  let starts_with_a_letter = last
    .bytes()
    .next()
    .is_some_and(|byte| byte.is_ascii_lowercase());
  let words = last.split('_').filter(|word| !word.is_empty()).count();
  (identifier_parts && starts_with_a_letter && words >= TEST_NAME_MIN_WORDS).then_some(last)
}

/// The other project whose possessive ends `before`, the text up to a backticked span.
fn owner_before(before: &str) -> Option<&'static str> {
  OTHER_PROJECTS
    .into_iter()
    .find(|owner| before.ends_with(&format!("{} ", possessive(owner))))
}

/// An owner's possessive: "Shards'", or "Name's" for a name that does not end in s.
fn possessive(owner: &str) -> String {
  if owner.ends_with('s') {
    format!("{owner}'")
  } else {
    format!("{owner}'s")
  }
}

/// The crate directory a backticked path lies under, for a path in this tree's layout (`crates/<name>/…` or
/// `xtask/…`); `None` for any other span.
fn crate_of_path(span: &str) -> Option<&str> {
  if span.contains(|c: char| c.is_whitespace() || c == '*') {
    return None;
  }
  if span.starts_with("xtask/") {
    return Some("xtask");
  }
  let prefix = "crates/";
  let rest = span.strip_prefix(prefix)?;
  let name = rest.split('/').next().filter(|name| !name.is_empty())?;
  span.get(..prefix.len().checked_add(name.len())?)
}

/// Each test or path of this tree a row names and the tree lacks, as `<document>:<line>: …`, the row's line.
fn missing(
  document: &Document,
  named: &[Citation],
  defined: &BTreeMap<String, BTreeSet<String>>,
  exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
  named
    .iter()
    .filter_map(|citation| match &citation.named {
      Named::Test(name) if !defined.contains_key(name) => Some(format!(
        "{}:{}: the row names the test `{name}`, which no source under crates/ or xtask/ defines",
        document.path, citation.line
      )),
      Named::Path(path) if !exists(path) => Some(format!(
        "{}:{}: the row names `{path}`, which is not in the tree",
        document.path, citation.line
      )),
      _ => None,
    })
    .collect()
}

/// The generated list for `document`: each test its rows name with the files defining it, then the other projects'
/// tests and paths, each once and in order; the indentation of the end marker closes it.
fn rendered_list(
  document: &Document,
  named: &[Citation],
  defined: &BTreeMap<String, BTreeSet<String>>,
) -> String {
  let mut tests = BTreeSet::new();
  let mut others_tests = BTreeSet::new();
  let mut others_paths = BTreeSet::new();
  for citation in named {
    match &citation.named {
      Named::Test(name) => {
        tests.insert(name.as_str());
      }
      Named::OthersTest { owner, name } => {
        others_tests.insert((*owner, name.as_str()));
      }
      Named::OthersPath(path) => {
        others_paths.insert(path.as_str());
      }
      Named::Path(_) => {}
    }
  }
  let indent = document.indent;
  let mut list = String::new();
  for name in tests {
    let files = defined
      .get(name)
      .map(|files| {
        files
          .iter()
          .map(|file| format!("`{file}`"))
          .collect::<Vec<_>>()
          .join(", ")
      })
      .unwrap_or_else(|| "defined nowhere".to_owned());
    list.push_str(&format!("{indent}- `{name}`: {files}\n"));
  }
  for (owner, name) in others_tests {
    list.push_str(&format!(
      "{indent}- {} `{name}`: another project's test, not looked for\n",
      possessive(owner)
    ));
  }
  for path in others_paths {
    list.push_str(&format!(
      "{indent}- `{path}`: another project's path, not looked for\n"
    ));
  }
  list.push_str(indent);
  list
}

/// A document's generated list, between its markers.
fn generated_list(text: &str) -> Option<&str> {
  let start = text.find(LIST_BEGIN)?.checked_add(LIST_BEGIN.len())?;
  let end = text.get(start..)?.find(LIST_END)?.checked_add(start)?;
  text.get(start..end)
}

/// Every function a source under `crates/` or `xtask/` defines, by name, with the files defining it (paths from the
/// workspace root, joined by `/`).
fn defined_functions(root: &Path) -> Result<BTreeMap<String, BTreeSet<String>>, Failure> {
  let mut files = Vec::new();
  rust_sources(&root.join("crates"), &mut files)?;
  rust_sources(&root.join("xtask"), &mut files)?;
  let mut defined: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
  for file in files {
    let source = std::fs::read_to_string(&file)?;
    let relative = file
      .strip_prefix(root)
      .unwrap_or(&file)
      .components()
      .map(|component| component.as_os_str().to_string_lossy())
      .collect::<Vec<_>>()
      .join("/");
    for name in function_names(&source) {
      defined
        .entry(name.to_owned())
        .or_default()
        .insert(relative.clone());
    }
  }
  Ok(defined)
}

/// The names of the functions `source` defines: each identifier after the keyword `fn` and before `(` or `<`.
fn function_names(source: &str) -> impl Iterator<Item = &str> {
  source.match_indices("fn ").filter_map(move |(at, _)| {
    let after_a_word = source
      .get(..at)?
      .chars()
      .next_back()
      .is_some_and(|c| c.is_alphanumeric() || c == '_');
    if after_a_word {
      return None;
    }
    let rest = source.get(at.checked_add("fn ".len())?..)?;
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    let name = rest.get(..end)?;
    let opens = rest.get(end..)?.trim_start().chars().next()?;
    (!name.is_empty() && (opens == '(' || opens == '<')).then_some(name)
  })
}

#[cfg(test)]
mod tests {
  use super::{
    Citation, DOCUMENTS, Document, LIST_BEGIN, LIST_END, Named, check, citations,
    defined_functions, function_names, missing, rendered_list,
  };
  use crate::workspace_root;
  use std::collections::{BTreeMap, BTreeSet};

  /// A document whose rows name an in-tree test, a renamed one, Shards' test, a function's name, a test in a `::` path,
  /// a test after a possessive that is not another project's, and three paths; then a test named in prose.
  const ROWS: &str = "\
# A document

| function | mac |
|---|---|
| Host mount | built: `gates` (`a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs`, `a_test_renamed_since_this_cell_was_written`) |
| Shards route | Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes`, `get_shared_memory_region`, `delivered::a_detach_ends_the_stream_the_client_handed_out`, the CLI's `a_cli_test_cited_after_a_possessive_of_this_tree` |
| Paths | `crates/cli/tests/winfsp.rs`, `crates/vmm/build.rs`, `crates/cli/tests/gone.rs`, `crates/*/src` |

Prose naming `a_test_named_in_prose_outside_any_table_row` is not read.
";

  /// The whole document, as `platforms.md` is read.
  const WHOLE: Document = Document {
    path: "rows.md",
    from: None,
    to: None,
    indent: "",
  };

  fn defined_here() -> BTreeMap<String, BTreeSet<String>> {
    [
      (
        "a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs",
        "crates/cli/tests/winfsp.rs",
      ),
      (
        "a_detach_ends_the_stream_the_client_handed_out",
        "crates/client/tests/shards.rs",
      ),
    ]
    .into_iter()
    .map(|(name, file)| (name.to_owned(), BTreeSet::from([file.to_owned()])))
    .collect()
  }

  /// Plan step 1. Do: read the rows above, where this tree has `crates/cli` and its `winfsp.rs` and defines two of the
  /// tests. Expect: each test and path is cited at its row's line, in order; Shards' test and the path under
  /// `crates/vmm` are another project's; the function's name, the glob and the prose are not cited; and exactly the
  /// renamed test, the test after "the CLI's" and the missing path are refused, each naming its document and line.
  #[test]
  fn a_row_names_its_tests_and_paths_and_another_projects_are_told_apart() {
    let has_crate = |dir: &str| dir == "crates/cli";
    let named = citations(&WHOLE, ROWS, &has_crate);
    let cited = |line, named| Citation { line, named };
    assert_eq!(
      named,
      vec![
        cited(
          5,
          Named::Test(
            "a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs".to_owned()
          )
        ),
        cited(
          5,
          Named::Test("a_test_renamed_since_this_cell_was_written".to_owned())
        ),
        cited(
          6,
          Named::OthersTest {
            owner: "Shards",
            name: "a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes"
              .to_owned(),
          }
        ),
        cited(
          6,
          Named::Test("a_detach_ends_the_stream_the_client_handed_out".to_owned())
        ),
        cited(
          6,
          Named::Test("a_cli_test_cited_after_a_possessive_of_this_tree".to_owned())
        ),
        cited(7, Named::Path("crates/cli/tests/winfsp.rs".to_owned())),
        cited(7, Named::OthersPath("crates/vmm/build.rs".to_owned())),
        cited(7, Named::Path("crates/cli/tests/gone.rs".to_owned())),
      ]
    );
    let exists = |path: &str| path == "crates/cli/tests/winfsp.rs";
    assert_eq!(
      missing(&WHOLE, &named, &defined_here(), &exists),
      vec![
        "rows.md:5: the row names the test `a_test_renamed_since_this_cell_was_written`, which no source under crates/ or xtask/ defines",
        "rows.md:6: the row names the test `a_cli_test_cited_after_a_possessive_of_this_tree`, which no source under crates/ or xtask/ defines",
        "rows.md:7: the row names `crates/cli/tests/gone.rs`, which is not in the tree",
      ]
    );
  }

  /// Plan step 1. Do: read a document whose part is §8, with rows naming missing tests before §8, inside it and after
  /// §9. Expect: only the row inside §8 is read.
  #[test]
  fn only_the_rows_of_a_documents_part_are_read() {
    let part = Document {
      path: "part.md",
      from: Some("## 8."),
      to: Some("## 9."),
      indent: "   ",
    };
    let text = "\
## 7. Before
| a | `a_row_before_the_part_names_this_missing_test` |
## 8. The part
   | b | `a_row_inside_the_part_names_this_missing_test` |
## 9. After
| c | `a_row_after_the_part_names_this_missing_test` |
";
    assert_eq!(
      citations(&part, text, &|_| true),
      vec![Citation {
        line: 4,
        named: Named::Test("a_row_inside_the_part_names_this_missing_test".to_owned()),
      }]
    );
  }

  /// Plan step 1 (the list's format, a golden vector). Do: render the list for the rows above, indented as §8's is.
  /// Expect: each test of this tree once, in order, with its files ("defined nowhere" for the two the tree lacks),
  /// then Shards' test and the other project's path, then the indentation the end marker sits at.
  #[test]
  fn the_generated_list_names_each_test_once_with_its_files_then_the_other_projects() {
    let indented = Document {
      indent: "   ",
      ..WHOLE
    };
    let named = citations(&indented, ROWS, &|dir| dir == "crates/cli");
    assert_eq!(
      rendered_list(&indented, &named, &defined_here()),
      "   - `a_cli_test_cited_after_a_possessive_of_this_tree`: defined nowhere\n\
       \x20  - `a_detach_ends_the_stream_the_client_handed_out`: `crates/client/tests/shards.rs`\n\
       \x20  - `a_test_renamed_since_this_cell_was_written`: defined nowhere\n\
       \x20  - `a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs`: `crates/cli/tests/winfsp.rs`\n\
       \x20  - Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes`: another project's test, not looked for\n\
       \x20  - `crates/vmm/build.rs`: another project's path, not looked for\n\
       \x20  "
    );
  }

  /// Plan step 1. Do: find the functions a source defines, among a call, a word ending in "fn", generic and async
  /// definitions and a test in a property block. Expect: exactly the four definitions.
  #[test]
  fn a_definition_is_a_name_after_fn_and_before_its_parameters() {
    let source = "\
fn plain() {}
pub(crate) fn generic<T>(value: T) {}
async fn awaited () {}
proptest! { fn a_property_holds(x in 0u8..) {} }
let pointer: fn(u8) = plain; selfn not_a_definition() {}
";
    assert_eq!(
      function_names(source).collect::<Vec<_>>(),
      ["plain", "generic", "awaited", "a_property_holds"]
    );
  }

  /// Plan step 1 (doc truth, CLAUDE.md §4). Do: read every table row of `docs/wip/platforms.md` and of
  /// `docs/wip/virtiofs.md` §8, and look in the tree for each test and path a cell names. Expect: the tree defines
  /// every one, and each document's generated list says where.
  #[test]
  fn every_test_the_platform_tables_name_is_in_the_tree() {
    let root = workspace_root().expect("the workspace root");
    let mut violations = Vec::new();
    check(&root, &mut violations).expect("the documents and the sources are readable");
    assert!(violations.is_empty(), "{}", violations.join("\n"));
  }

  #[test]
  #[ignore = "rewrites the generated lists of docs/wip/platforms.md and docs/wip/virtiofs.md; run deliberately with --ignored"]
  fn regenerate_the_named_tests() {
    let root = workspace_root().expect("the workspace root");
    let defined = defined_functions(&root).expect("the sources are readable");
    for document in &DOCUMENTS {
      let path = root.join(document.path);
      let text = std::fs::read_to_string(&path).expect("the document is readable");
      let named = citations(document, &text, &|dir| root.join(dir).is_dir());
      let absent = missing(document, &named, &defined, &|cited| {
        root.join(cited).exists()
      });
      assert!(
        absent.is_empty(),
        "these cells name what the tree lacks; fix the cells, the list is not theirs to fix:\n{}",
        absent.join("\n")
      );
      let start = text.find(LIST_BEGIN).expect("the begin marker") + LIST_BEGIN.len();
      let end = start + text[start..].find(LIST_END).expect("the end marker");
      let rewritten = format!(
        "{}{}{}",
        &text[..start],
        rendered_list(document, &named, &defined),
        &text[end..]
      );
      #[allow(clippy::disallowed_methods)]
      std::fs::write(&path, rewritten).expect("the document is writable");
    }
  }
}
