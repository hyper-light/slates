//! `cargo xtask unsafe`: the unsafe budget. Counts `unsafe` blocks, functions and impls in each
//! shipped crate's sources (comments, doc comments and test trees excluded) and fails when a
//! crate exceeds the budget recorded in `unsafe-budget.toml`; `--tighten` lowers budgets to the
//! current counts, changing only those numbers (the file's comments are the ceilings' history).
//! Budgets only go down; a raise is a deliberate edit with a reason in the file.

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
  let file: Budget =
    toml::from_str(&text).map_err(|e| Failure(format!("unsafe-budget.toml: {e}")))?;
  let mut lowered = BTreeMap::new();
  let mut recorded = BTreeMap::new();
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
        lowered.insert(package.name.clone(), count);
        tightened += 1;
        "tightened"
      }
      Some(_) => "ok",
      None => {
        if tighten {
          recorded.insert(package.name.clone(), count);
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
    let rewritten = tightened_text(&text, &lowered, &recorded)?;
    // xtask is a development tool rewriting a tracked file in the repository.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(&path, rewritten)?;
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

/// Format: the header of the table the ceilings live in.
const BUDGET_TABLE: &str = "[budget]";

/// The ceiling a line of the budget table sets: the crate's name (a TOML key, bare or quoted), and the byte range of
/// its decimal digits within the line. `None` for a comment, a blank line, or anything that is not `key = digits`.
fn entry_of(line: &str) -> Option<(&str, std::ops::Range<usize>)> {
  if line.trim_start().starts_with('#') {
    return None;
  }
  let (key, rest) = line.split_once('=')?;
  let name = key.trim().trim_matches('"');
  let digits_at = key.len() + 1 + (rest.len() - rest.trim_start().len());
  let digits = line
    .get(digits_at..)?
    .bytes()
    .take_while(u8::is_ascii_digit)
    .count();
  (digits > 0).then_some((name, digits_at..digits_at + digits))
}

/// The file's new text: `text` byte for byte, except the digits of each ceiling in `lowered` and one new line at the
/// end of the budget table for each crate in `recorded`. The file's comments are the ceilings' history (each raise
/// names its site and reason), so nothing else may change; until 2026-11-08 this re-serialized the table and kept only
/// the leading comments. A name with no line in the table is refused, and the result must parse back to exactly the
/// intended ceilings, so a line this reader misjudges fails here and not as a silently wrong budget.
fn tightened_text(
  text: &str,
  lowered: &BTreeMap<String, usize>,
  recorded: &BTreeMap<String, usize>,
) -> Result<String, Failure> {
  let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
  let mut remaining: Vec<&String> = lowered.keys().collect();
  let mut in_budget = false;
  let mut table_end = None;
  for (at, line) in lines.iter_mut().enumerate() {
    let trimmed = line.trim();
    if trimmed.starts_with('[') {
      if in_budget {
        table_end = Some(at);
        break;
      }
      in_budget = trimmed == BUDGET_TABLE;
      continue;
    }
    if !in_budget {
      continue;
    }
    let Some((name, digits)) = entry_of(line) else {
      continue;
    };
    if let Some(ceiling) = lowered.get(name) {
      remaining.retain(|wanted| wanted.as_str() != name);
      line.replace_range(digits, &ceiling.to_string());
    }
  }
  if let Some(name) = remaining.first() {
    return Err(Failure(format!(
      "unsafe-budget.toml: `{name}` has no line in the {BUDGET_TABLE} table to lower"
    )));
  }
  if !recorded.is_empty() {
    insert_recorded(&mut lines, table_end, recorded);
  }
  let rewritten = lines.concat();
  verify_ceilings(text, &rewritten, lowered, recorded)?;
  Ok(rewritten)
}

/// Adds one line per recorded crate at the end of the budget table: before the next table, ahead of the blank lines
/// that separate it, or at the end of the file, opening the table if the file has none.
fn insert_recorded(
  lines: &mut Vec<String>,
  table_end: Option<usize>,
  recorded: &BTreeMap<String, usize>,
) {
  if let Some(last) = lines.last_mut()
    && !last.ends_with('\n')
  {
    last.push('\n');
  }
  let has_table = lines.iter().any(|line| line.trim() == BUDGET_TABLE);
  let at = table_end.unwrap_or(lines.len());
  let mut added: Vec<String> = Vec::new();
  if !has_table {
    added.push(format!("{BUDGET_TABLE}\n"));
  }
  added.extend(
    recorded
      .iter()
      .map(|(name, count)| format!("{name} = {count}\n")),
  );
  // Before the blank line that ends the table's last block, so the new lines stay inside the table's text and the
  // separation before the next table is kept.
  let at = match table_end {
    Some(_) if at > 0 && lines.get(at - 1).is_some_and(|line| line.trim().is_empty()) => at - 1,
    _ => at,
  };
  lines.splice(at..at, added);
}

/// Refuses a rewrite that does not parse back to the old ceilings with exactly `lowered` and `recorded` applied.
fn verify_ceilings(
  before: &str,
  after: &str,
  lowered: &BTreeMap<String, usize>,
  recorded: &BTreeMap<String, usize>,
) -> Result<(), Failure> {
  let parse = |text: &str| -> Result<Budget, Failure> {
    toml::from_str(text).map_err(|e| Failure(format!("unsafe-budget.toml: {e}")))
  };
  let mut expected = parse(before)?.budget;
  expected.extend(lowered.iter().map(|(name, count)| (name.clone(), *count)));
  expected.extend(recorded.iter().map(|(name, count)| (name.clone(), *count)));
  if parse(after)?.budget == expected {
    Ok(())
  } else {
    Err(Failure(
      "unsafe-budget.toml: the rewritten file does not parse to the intended ceilings; nothing was written".to_owned(),
    ))
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
      count += keyword_occurrences(&code, "unsafe");
    }
  }
  Ok(count)
}

/// Counts occurrences of `word` as a whole identifier token, so a substring inside a longer
/// identifier (for example `unsafe` inside `unshare_unsafe` or `unsafe_op_in_unsafe_fn`) does
/// not count. A token boundary is a non-identifier character (or the string's edge).
fn keyword_occurrences(code: &str, word: &str) -> usize {
  let bytes = code.as_bytes();
  let mut count = 0;
  let mut from = 0;
  while let Some(rel) = code[from..].find(word) {
    let start = from + rel;
    let end = start + word.len();
    let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
    let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
    if before_ok && after_ok {
      count += 1;
    }
    from = end;
  }
  count
}

/// Whether a byte can be part of a Rust identifier (so a token boundary is where it is not).
fn is_ident_byte(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A budget file with the shape of the real one: a header, comment blocks between the entries, a blank line, a
  /// comment that reads like an entry, a trailing comment, a quoted key, and a later table.
  const FILE: &str = "# The unsafe budget.\n#\n# alpha = 9 is what this comment says, and it is not an entry.\n\n[budget]\n# alpha holds five sites.\nalpha = 5\n\n# beta's history:\n#   raised 6 -> 7 on 2026-09-14.\n\"beta\" = 7 # a trailing note\ngamma   =   2\n\n[other]\nalpha = 100\n";

  fn map(entries: &[(&str, usize)]) -> BTreeMap<String, usize> {
    entries
      .iter()
      .map(|(name, count)| ((*name).to_owned(), *count))
      .collect()
  }

  /// Do tighten a budget file with comments, lowering two ceilings; expect those two numbers changed and every other
  /// byte the same: the header, the comments between entries (one reads like an entry), the blank lines, a trailing
  /// comment, the spacing around `=`, and a later table with a same-named key.
  #[test]
  fn tightening_changes_only_the_lowered_numbers() {
    let tightened = tightened_text(FILE, &map(&[("alpha", 3), ("beta", 6)]), &map(&[]));
    let expected = FILE
      .replace("\nalpha = 5\n", "\nalpha = 3\n")
      .replace("\"beta\" = 7 #", "\"beta\" = 6 #");
    assert_eq!(tightened.ok().as_deref(), Some(expected.as_str()));
  }

  /// Do tighten the repository's own file, lowering one ceiling; expect exactly one line of its 500-odd to differ,
  /// and that line to be the ceiling's. Before 2026-11-08 `--tighten` kept only the leading comments and dropped the
  /// rest, about 290 lines of the ceilings' history.
  #[test]
  fn tightening_the_real_file_changes_only_the_one_line() {
    let Ok(root) = crate::workspace_root() else {
      panic!("no workspace root")
    };
    let Ok(text) = std::fs::read_to_string(root.join("unsafe-budget.toml")) else {
      panic!("no budget file")
    };
    let budget: Budget = toml::from_str(&text).unwrap_or_default();
    let Some((name, ceiling)) = budget.budget.iter().find(|(_, ceiling)| **ceiling > 0) else {
      panic!("no ceiling above zero")
    };
    let lowered = map(&[(name.as_str(), ceiling - 1)]);
    let Ok(tightened) = tightened_text(&text, &lowered, &map(&[])) else {
      panic!("the file was refused")
    };
    let before: Vec<&str> = text.split_inclusive('\n').collect();
    let after: Vec<&str> = tightened.split_inclusive('\n').collect();
    assert_eq!(before.len(), after.len());
    let changed: Vec<(&&str, &&str)> = before.iter().zip(&after).filter(|(a, b)| a != b).collect();
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(*changed[0].1, format!("{name} = {}\n", ceiling - 1));
  }

  /// Do record a crate with no ceiling; expect its line added at the end of the budget table, before the next table,
  /// and every existing byte kept.
  #[test]
  fn a_recorded_crate_joins_the_end_of_the_table() {
    let tightened = tightened_text(FILE, &map(&[]), &map(&[("delta", 4)]));
    let expected = FILE.replace("gamma   =   2\n\n", "gamma   =   2\ndelta = 4\n\n");
    assert_eq!(tightened.ok().as_deref(), Some(expected.as_str()));
  }

  /// Do lower a crate no line names, or one named only in a comment or in another table; expect a refusal naming it,
  /// never a silent no-op that leaves the old ceiling standing.
  #[test]
  fn a_lowered_crate_with_no_line_is_refused() {
    for name in ["absent", "other_only"] {
      let file = format!("{FILE}other_only = 3\n");
      match tightened_text(&file, &map(&[(name, 1)]), &map(&[])) {
        Ok(text) => panic!("{name} was accepted: {text}"),
        Err(Failure(message)) => assert!(message.contains(name), "{message}"),
      }
    }
  }
}
