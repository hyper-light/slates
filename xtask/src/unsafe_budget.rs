//! `cargo xtask unsafe`: the unsafe budget. Counts `unsafe` blocks, functions and impls in each
//! shipped crate's sources (comments, doc comments and test trees excluded) and fails when a
//! crate exceeds the budget recorded in `unsafe-budget.toml`; `--tighten` lowers budgets to the
//! current counts. Budgets only go down; a raise is a deliberate edit with a reason in the file.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Failure, cargo_metadata, code_only, is_test_tree, lines_with_test_flag, rust_sources};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Budget {
  #[serde(default)]
  budget: BTreeMap<String, usize>,
}

/// Counts and compares; `tighten` rewrites the file with the current counts where lower.
pub(crate) fn run(root: &Path, tighten: bool) -> Result<(), Failure> {
  let path = root.join("unsafe-budget.toml");
  let text = std::fs::read_to_string(&path).unwrap_or_default();
  let header: String = text
    .lines()
    .take_while(|l| l.starts_with('#'))
    .map(|l| format!("{l}\n"))
    .collect();
  let mut file: Budget =
    toml::from_str(&text).map_err(|e| Failure(format!("unsafe-budget.toml: {e}")))?;
  let metadata = cargo_metadata(root)?;
  let mut over = Vec::new();
  let mut tightened = 0usize;
  println!("{:<24} {:>6} {:>7}  verdict", "crate", "count", "budget");
  for package in metadata
    .packages
    .iter()
    .filter(|p| p.name.starts_with("slates-"))
  {
    let Some(crate_root) = Path::new(&package.manifest_path).parent() else {
      continue;
    };
    let count = count_unsafe(&crate_root.join("src"))?;
    let budget = file.budget.get(&package.name).copied();
    let verdict = match budget {
      Some(b) if count > b => {
        over.push(format!(
          "{}: {count} unsafe sites exceed the budget of {b}",
          package.name
        ));
        "OVER BUDGET"
      }
      Some(b) if tighten && count < b => {
        file.budget.insert(package.name.clone(), count);
        tightened += 1;
        "tightened"
      }
      Some(_) => "ok",
      None => {
        if tighten {
          file.budget.insert(package.name.clone(), count);
          tightened += 1;
          "recorded"
        } else {
          over.push(format!(
            "{}: {count} unsafe sites and no budget recorded",
            package.name
          ));
          "NO BUDGET"
        }
      }
    };
    println!(
      "{:<24} {:>6} {:>7}  {verdict}",
      package.name,
      count,
      budget.map_or("-".to_owned(), |b| b.to_string())
    );
  }
  if tighten {
    let body =
      toml::to_string_pretty(&file).map_err(|e| Failure(format!("unsafe-budget.toml: {e}")))?;
    // xtask is a development tool rewriting a tracked file in the repository.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(&path, format!("{header}\n{body}"))?;
    println!("unsafe: wrote {} ({tightened} tightened)", path.display());
  }
  if over.is_empty() {
    println!("unsafe: ok");
    Ok(())
  } else {
    for o in &over {
      eprintln!("unsafe: {o}");
    }
    Err(Failure(format!(
      "{} crate(s) over their unsafe budget",
      over.len()
    )))
  }
}

/// Counts `unsafe` tokens in code (not comments), outside test trees and `#[cfg(test)]` modules.
fn count_unsafe(src: &Path) -> Result<usize, Failure> {
  if !src.is_dir() {
    return Ok(0);
  }
  let mut files = Vec::new();
  rust_sources(src, &mut files)?;
  let mut count = 0;
  for file in files {
    if is_test_tree(&file) {
      continue;
    }
    let source = std::fs::read_to_string(&file)?;
    for (_, line, in_test) in lines_with_test_flag(&source) {
      if in_test {
        continue;
      }
      let code = code_only(line);
      count += code.matches("unsafe").count();
    }
  }
  Ok(count)
}
