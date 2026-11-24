//! A module's docs in one place (`cargo xtask structural`): no `mod` declaration carries `///` docs when its module
//! opens with `//!` docs.
//!
//! rustdoc joins a module's outer docs, the `///` lines on its declaration, to its inner docs, the `//!` lines that
//! open the module, as one text, and resolves each part's links in its own scope: the outer part's from the module
//! that declares it, the inner part's from the module itself. So a link that resolves in the module's file names
//! another item, or none, on its declaration, and the module's docs sit in two files, each read alone. On 2026-11-23
//! eleven modules had both (w-wall's sweep, then this check's first run): none had a broken link yet, and one had
//! another module's docs. d0087a8b4 put `slates-ipc`'s `mod mach_claim;` between `mod owner_name;` and its docs, so
//! the rendered docs of `mach_claim`, whose work is Mach messages, said of it that "no message crosses it".
//!
//! **What is read.** Every `.rs` file under `crates/` and `xtask/` ([`check`]). A `mod` declaration, of any visibility,
//! out of line or inline, carries outer docs when `///` lines or a `#[doc]` attribute reach it past its other
//! attributes, blank lines and plain comments; a line that begins inside a string literal is text
//! ([`documented_modules`]). Its module's lines are the file the compiler reads for it ([`candidates`]), or for an
//! inline module the lines after its declaration. They open with inner docs when the first of them that is not blank, a
//! plain comment or part of an inner attribute is `//!`, `/*!` or `#![doc` ([`opens_with_inner_docs`]).
//!
//! **What it does not read.** An out-of-line declaration inside an inline module is looked for as if its file had made
//! it, where the compiler looks in the inline module's directory, and a `/** */` doc is not read: no source has either
//! (2026-11-23).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{
  Failure, Metadata, StringState, code_only, code_only_carrying, rust_sources, without_visibility,
};

/// A `mod` declaration that carries outer docs.
#[derive(Debug, PartialEq, Eq)]
struct Declaration {
  /// Its line, counted from 1.
  line: usize,
  /// The module's name.
  name: String,
  /// Where the module's lines are.
  body: Body,
}

/// Where a declared module's lines are.
#[derive(Debug, PartialEq, Eq)]
enum Body {
  /// In a file, named by the declaration's `#[path = "..."]` when it carries one.
  File { path: Option<String> },
  /// After the declaration, from this index of its file's lines.
  Inline { from: usize },
}

/// A module whose docs are in two places.
#[derive(Debug, PartialEq, Eq)]
struct Joined {
  /// Its declaration's line, counted from 1.
  line: usize,
  /// The module's name.
  name: String,
  /// The file whose opening `//!` docs rustdoc joins to the declaration's; `None` for an inline module.
  module_file: Option<PathBuf>,
}

/// Refuses each `mod` declaration under `crates/` and `xtask/` that carries outer docs while its module opens with
/// inner docs, naming its file, its line and where the inner docs are. A file declares its modules beside itself when
/// it is a `mod.rs` or the root file of a target `metadata` names.
pub(super) fn check(
  root: &Path,
  metadata: &Metadata,
  violations: &mut Vec<String>,
) -> Result<(), Failure> {
  let crate_roots: BTreeSet<&Path> = metadata
    .packages
    .iter()
    .flat_map(|package| {
      package
        .targets
        .iter()
        .map(|target| target.src_path.as_path())
    })
    .collect();
  let mut files = Vec::new();
  rust_sources(&root.join("crates"), &mut files)?;
  rust_sources(&root.join("xtask"), &mut files)?;
  let read = |path: &Path| std::fs::read_to_string(path);
  for file in &files {
    let source = std::fs::read_to_string(file)?;
    let beside_itself =
      crate_roots.contains(file.as_path()) || file.file_name().is_some_and(|name| name == "mod.rs");
    for joined in docs_in_two_places(file, &source, beside_itself, &read)? {
      let inner = joined.module_file.map_or_else(
        || "after its declaration".to_owned(),
        |module_file| format!("at the top of {}", relative(root, &module_file)),
      );
      violations.push(format!(
        "{}:{}: `mod {}` carries `///` docs and its module opens with `//!` docs ({inner}); rustdoc joins the two and \
         resolves the `///` part's links from the declaring module, so keep the module's docs in its `//!` docs",
        relative(root, file),
        joined.line,
        joined.name
      ));
    }
  }
  Ok(())
}

/// The modules `source`, the text of `file`, declares with outer docs while their own docs open with inner docs.
/// `beside_itself` says whether `file` declares its modules beside itself; `read` reads a file.
fn docs_in_two_places(
  file: &Path,
  source: &str,
  beside_itself: bool,
  read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Vec<Joined>, Failure> {
  let lines: Vec<&str> = source.lines().collect();
  let mut joined = Vec::new();
  for Declaration { line, name, body } in documented_modules(&lines) {
    let module_file = match body {
      Body::Inline { from } => {
        let after = lines.get(from..).unwrap_or_default();
        opens_with_inner_docs(after.iter().copied()).then_some(None)
      }
      Body::File { path } => {
        let found = first_found(
          candidates(file, &name, path.as_deref(), beside_itself),
          read,
        )?;
        found
          .filter(|(_, text)| opens_with_inner_docs(text.lines()))
          .map(|(module_file, _)| Some(module_file))
      }
    };
    if let Some(module_file) = module_file {
      joined.push(Joined {
        line,
        name,
        module_file,
      });
    }
  }
  Ok(joined)
}

/// Each `mod` declaration among `lines` that carries outer docs: `///` lines or a `#[doc]` attribute that reach it past
/// its other attributes (a `#[path]` among them noted), blank lines and plain comments. A line that begins inside a
/// string literal is text, and ends what came before it.
fn documented_modules(lines: &[&str]) -> Vec<Declaration> {
  let mut found = Vec::new();
  let mut documented = false;
  let mut path = None;
  let mut attribute_depth: usize = 0;
  let mut string = StringState::Code;
  for (index, line) in lines.iter().enumerate() {
    let begins_in_text = string != StringState::Code;
    let code = code_only_carrying(line, &mut string);
    let trimmed = line.trim();
    if begins_in_text {
      (documented, path, attribute_depth) = (false, None, 0);
    } else if attribute_depth > 0 {
      attribute_depth = depth_after(attribute_depth, &code);
    } else if is_outer_doc(trimmed) {
      documented = true;
    } else if trimmed.starts_with("#[") {
      documented |= trimmed.starts_with("#[doc");
      if let Some(rest) = trimmed.strip_prefix("#[path") {
        path = rest.split('"').nth(1).map(str::to_owned);
      }
      attribute_depth = depth_after(0, &code);
    } else if !(trimmed.is_empty() || trimmed.starts_with("//")) {
      let declared = declared_module(&code, index, path.take()).filter(|_| documented);
      if let Some((name, body)) = declared {
        found.push(Declaration {
          line: index.saturating_add(1),
          name,
          body,
        });
      }
      (documented, path) = (false, None);
    }
  }
  found
}

/// The module `code` declares, if it is a `mod` declaration, and where its lines are: in a file for `mod name;` (named
/// by `path`, the `#[path]` it carries, if any), after the line at `index` for `mod name {`.
fn declared_module(code: &str, index: usize, path: Option<String>) -> Option<(String, Body)> {
  let rest = without_visibility(code.trim_start())
    .strip_prefix("mod ")?
    .trim_start();
  let name_end = rest
    .find(|character: char| !(character.is_alphanumeric() || character == '_'))
    .unwrap_or(rest.len());
  let (name, after) = rest.split_at_checked(name_end)?;
  if name.is_empty() {
    return None;
  }
  let body = match after.trim_start().chars().next()? {
    ';' => Body::File { path },
    '{' => Body::Inline {
      from: index.checked_add(1)?,
    },
    _ => return None,
  };
  Some((name.to_owned(), body))
}

/// Whether `lines` open with inner docs: whether the first of them that is not blank, a plain comment or part of an
/// inner attribute is `//!`, `/*!` or `#![doc`.
fn opens_with_inner_docs<'a>(lines: impl IntoIterator<Item = &'a str>) -> bool {
  let mut attribute_depth: usize = 0;
  for line in lines {
    let trimmed = line.trim();
    if attribute_depth > 0 {
      attribute_depth = depth_after(attribute_depth, &code_only(line));
    } else if trimmed.starts_with("//!")
      || trimmed.starts_with("/*!")
      || trimmed.starts_with("#![doc")
    {
      return true;
    } else if trimmed.starts_with("#![") {
      attribute_depth = depth_after(0, &code_only(line));
    } else if !(trimmed.is_empty() || (trimmed.starts_with("//") && !is_outer_doc(trimmed))) {
      return false;
    }
  }
  false
}

/// Whether a trimmed line is an outer doc comment: three slashes, where four or more are a plain comment.
fn is_outer_doc(trimmed: &str) -> bool {
  trimmed.starts_with("///") && !trimmed.starts_with("////")
}

/// The depth of an attribute's open brackets after `code`, from `depth` before it: the attribute ends at zero.
fn depth_after(depth: usize, code: &str) -> usize {
  depth
    .saturating_add(code.matches('[').count())
    .saturating_sub(code.matches(']').count())
}

/// The files the compiler reads for the module `name` that `file` declares out of line: the `#[path]` the declaration
/// carries (`path`), from `file`'s directory; else `<dir>/<name>.rs`, then `<dir>/<name>/mod.rs`, where `<dir>` is
/// `file`'s directory when `file` declares beside itself (a crate root or a `mod.rs`), and `file`'s path without its
/// extension otherwise.
fn candidates(file: &Path, name: &str, path: Option<&str>, beside_itself: bool) -> Vec<PathBuf> {
  let Some(directory) = file.parent() else {
    return Vec::new();
  };
  if let Some(path) = path {
    return vec![directory.join(path)];
  }
  let base = if beside_itself {
    directory.to_path_buf()
  } else {
    file.with_extension("")
  };
  vec![
    base.join(format!("{name}.rs")),
    base.join(name).join("mod.rs"),
  ]
}

/// The first of `candidates` that `read` finds, with its text. One that is not there is passed over; any other failure
/// to read it is the check's.
fn first_found(
  candidates: Vec<PathBuf>,
  read: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Result<Option<(PathBuf, String)>, Failure> {
  for candidate in candidates {
    match read(&candidate) {
      Ok(text) => return Ok(Some((candidate, text))),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => return Err(error.into()),
    }
  }
  Ok(None)
}

/// `path` from the workspace root, its parts joined by `/`.
fn relative(root: &Path, path: &Path) -> String {
  path
    .strip_prefix(root)
    .unwrap_or(path)
    .components()
    .map(|component| component.as_os_str().to_string_lossy())
    .collect::<Vec<_>>()
    .join("/")
}

#[cfg(test)]
mod tests {
  use std::path::{Path, PathBuf};

  use super::{Joined, check, docs_in_two_places};
  use crate::{cargo_metadata, workspace_root};

  /// A fake crate `x`'s module files, each with its opening.
  const TREE: [(&str, &str); 9] = [
    (
      "crates/x/src/joined.rs",
      "// A plain comment first.\n#![allow(\n  dead_code,\n)]\n//! Its own docs.\n",
    ),
    ("crates/x/src/plain.rs", "//! Its own docs.\n"),
    (
      "crates/x/src/late.rs",
      "use std::fmt;\n//! Not its opening.\n",
    ),
    ("crates/x/src/after_helper.rs", "//! Its own docs.\n"),
    (
      "crates/x/src/elsewhere/named.rs",
      "#![doc = \"Its own docs.\"]\n",
    ),
    ("crates/x/src/child.rs", "//! The crate root's child.\n"),
    (
      "crates/x/src/joined/child.rs",
      "/// Its first item's docs.\nfn first() {}\n",
    ),
    ("crates/x/src/in_a_string.rs", "//! Its own docs.\n"),
    (
      "crates/x/tests/support/mod.rs",
      "\n//! The suite's support.\n",
    ),
  ];

  /// The fake crate's root: its declarations, each saying whether it is refused.
  const LIB: &str = r#"//! The crate.
/// Refused: its file opens with `//!` docs past a comment and an attribute.
mod joined;
// A plain comment.
mod plain;
/// Passed: its file's `//!` line follows an item.
mod late;
/// These docs are the function's.
fn helper() {}
mod after_helper;
/// Refused: by its path, whose `#![doc]` opens it.
#[path = "elsewhere/named.rs"]
pub(crate) mod by_path;
/// Refused: beside the crate root.
mod child;
/// Refused: inline, past a multi-line attribute.
#[cfg(any(
  unix,
  windows
))]
mod inline {
  //! Its own docs.
}
/// Passed: inline, with no `//!` docs.
mod inline_plain {
  fn first() {}
}
const TEXT: &str = "
/// Text, not docs.
mod in_a_string;
";
"#;

  /// What [`docs_in_two_places`] finds in `source`, the text of `file`, over [`TREE`].
  fn judged(file: &str, source: &str, beside_itself: bool) -> Vec<Joined> {
    let read = |path: &Path| {
      TREE
        .iter()
        .find(|(name, _)| Path::new(name) == path)
        .map(|(_, text)| (*text).to_owned())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    };
    docs_in_two_places(Path::new(file), source, beside_itself, &read).expect("the fake tree reads")
  }

  /// One refusal's expected value.
  fn joined(line: usize, name: &str, module_file: Option<&str>) -> Joined {
    Joined {
      line,
      name: name.to_owned(),
      module_file: module_file.map(PathBuf::from),
    }
  }

  /// 2026-11-23 (w-wall round 4: the eleven modules with docs in two places). Do: judge a fake crate's root, one of its
  /// module files and an integration test's root, whose `mod` declarations carry `///` docs over modules that open with
  /// `//!` docs: past a plain comment and a multi-line inner attribute, by `#[path]` to a `#![doc]`, inline past a
  /// multi-line attribute, beside the crate root, and as a `mod.rs` after a blank line; and over modules whose `//!`
  /// line follows an item or that open with outer docs; and declarations with a plain comment, after another item's
  /// docs, or inside a string. Expect: exactly the five of the first kind refused, each at its line with the file
  /// rustdoc joins (none inline), the module file's child looked for under its own name, never beside it.
  #[test]
  fn a_modules_outer_docs_are_refused_where_its_file_opens_with_inner_docs() {
    assert_eq!(
      judged("crates/x/src/lib.rs", LIB, true),
      vec![
        joined(3, "joined", Some("crates/x/src/joined.rs")),
        joined(13, "by_path", Some("crates/x/src/elsewhere/named.rs")),
        joined(15, "child", Some("crates/x/src/child.rs")),
        joined(21, "inline", None),
      ]
    );
    let module_file =
      "//! Its own docs.\n/// Passed: its child's file has no `//!` docs.\nmod child;\n";
    assert_eq!(
      judged("crates/x/src/joined.rs", module_file, false),
      Vec::new()
    );
    let suite = "/// Refused: beside the test's root, as a `mod.rs`.\nmod support;\n";
    assert_eq!(
      judged("crates/x/tests/suite.rs", suite, true),
      vec![joined(2, "support", Some("crates/x/tests/support/mod.rs"))]
    );
  }

  /// The tree keeps each module's docs in one place (CLAUDE.md §4, doc truth). Do: read every source under `crates/`
  /// and `xtask/`. Expect: no `mod` declaration carries `///` docs while its module opens with `//!` docs.
  #[test]
  fn no_module_in_the_tree_keeps_its_docs_in_two_places() {
    let root = workspace_root().expect("the workspace root");
    let metadata = cargo_metadata(&root).expect("the workspace's metadata");
    let mut violations = Vec::new();
    check(&root, &metadata, &mut violations).expect("the sources are readable");
    assert!(violations.is_empty(), "{}", violations.join("\n"));
  }
}
