//! The wire-order guard's shared half (D-15, append-only; §4.12 "Versioning a consumer pins", A-299), used by the
//! protocol's guard (`crates/ipc/tests/wire_order.rs`) and the records' (`crates/db/tests/wire_order.rs`).
//!
//! A wire enum's tag is its variant's index (`crates/wire-derive/src/lib.rs`, `expand_enum`), and its schema
//! (`Wire::SCHEMA`, `enum Name{Variant{field:Type,..},..}`) names its variants in tag order. A crate's guard reads
//! the schema of every wire enum it holds and compares the variants with its published tags (one `Enum tag Variant` a
//! line). A tag that moved (a variant inserted, removed, renamed or reordered) fails, naming the enum and the tag. An
//! appended variant fails until its change publishes it with the guard's `--ignored` writer, which refuses while any
//! tag moved. A second test fails for an enum the crate's sources derive `Wire` for that the guard does not hold.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every wire enum a guard holds, by name: its variants in tag order.
pub type Enums = BTreeMap<String, Vec<String>>;

/// An enum's name and its variants in tag order, read from its schema: the variants are the schema's parts at depth
/// zero, each a name and its fields in braces.
fn variants_of(schema: &str) -> (String, Vec<String>) {
  let body = schema.strip_prefix("enum ").expect("an enum's schema");
  let open = body.find('{').expect("an enum's schema opens its variants");
  let name = body[..open].to_owned();
  let inner = &body[open + 1..body.len() - 1];
  let mut variants = Vec::new();
  let mut depth = 0i32;
  let mut start = 0;
  for (at, character) in inner.char_indices() {
    match character {
      '{' | '(' | '<' | '[' => depth += 1,
      '}' | ')' | '>' | ']' => depth -= 1,
      ',' if depth == 0 => {
        variants.push(variant_name(&inner[start..at]));
        start = at + 1;
      }
      _ => {}
    }
  }
  if start < inner.len() {
    variants.push(variant_name(&inner[start..]));
  }
  (name, variants)
}

/// A variant's name: its part of the schema before its fields.
fn variant_name(part: &str) -> String {
  part[..part.find('{').unwrap_or(part.len())].to_owned()
}

/// Every enum among `schemas` as this build numbers it.
pub fn current(schemas: &[&str]) -> Enums {
  schemas.iter().map(|schema| variants_of(schema)).collect()
}

/// Every enum as `text` publishes it, one `Enum tag Variant` a line, each enum's tags from 0 in order.
pub fn published(text: &str) -> Enums {
  let mut enums = Enums::new();
  for line in text.lines().filter(|line| !line.trim().is_empty()) {
    let mut words = line.split_whitespace();
    let (Some(name), Some(tag), Some(variant)) = (words.next(), words.next(), words.next()) else {
      panic!("a published line is `Enum tag Variant`: {line:?}");
    };
    let variants = enums.entry(name.to_owned()).or_default();
    assert_eq!(
      tag.parse::<usize>().ok(),
      Some(variants.len()),
      "the published tags of {name} run from 0 in order: {line:?}"
    );
    variants.push(variant.to_owned());
  }
  enums
}

/// What changed in one enum from its published tags: nothing, variants appended at its end, or a published tag that
/// moved (the first, with what it was and what holds it now).
enum Change {
  None,
  Appended(Vec<String>),
  Moved {
    tag: usize,
    was: String,
    now: Option<String>,
  },
}

/// How `now` differs from `then`.
fn change_of(then: &[String], now: &[String]) -> Change {
  if let Some(tag) = (0..then.len()).find(|tag| now.get(*tag) != then.get(*tag)) {
    return Change::Moved {
      tag,
      was: then[tag].clone(),
      now: now.get(tag).cloned(),
    };
  }
  if now.len() > then.len() {
    Change::Appended(now[then.len()..].to_vec())
  } else {
    Change::None
  }
}

/// Every way `current` differs from `published`: the appends, and the moves.
fn differences(current: &Enums, published: &Enums) -> (Vec<String>, Vec<String>) {
  let (mut appended, mut moved) = (Vec::new(), Vec::new());
  for (name, now) in current {
    match published.get(name).map(|then| change_of(then, now)) {
      None => appended.push(format!("{name} is a wire enum not yet published")),
      Some(Change::None) => {}
      Some(Change::Appended(tail)) => appended.push(format!("{name} appended {tail:?}")),
      Some(Change::Moved { tag, was, now }) => moved.push(format!(
        "{name}'s tag {tag} was {was} and is now {}: a variant was inserted, removed, renamed or reordered",
        now.as_deref().unwrap_or("gone")
      )),
    }
  }
  moved.extend(
    published
      .keys()
      .filter(|name| !current.contains_key(*name))
      .map(|name| format!("{name} is published but no longer a wire enum this guard holds")),
  );
  (appended, moved)
}

/// The guard: `schemas` against the `published` text, for `package` (named in the writer's command). Fails for a moved
/// tag, then for an unpublished append.
pub fn assert_published(package: &str, schemas: &[&str], published_text: &str) {
  let (appended, moved) = differences(&current(schemas), &published(published_text));
  assert!(
    moved.is_empty(),
    "published wire tags moved (D-15: append only; a record read under the old tags would be misread):\n{}",
    moved.join("\n")
  );
  assert!(
    appended.is_empty(),
    "variants appended but not published; publish them with `cargo test -p {package} --test wire_order -- \
     --ignored publish_appended_variants`:\n{}",
    appended.join("\n")
  );
}

/// The `.rs` files under `dir`, at every depth.
fn sources_under(dir: &Path) -> Vec<PathBuf> {
  let mut found = Vec::new();
  for entry in std::fs::read_dir(dir).expect("the crate's sources are readable") {
    let path = entry.expect("a directory entry").path();
    if path.is_dir() {
      found.extend(sources_under(&path));
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      found.push(path);
    }
  }
  found
}

/// The enums `source` derives `Wire` for: each declared after a `#[derive(..)]` line naming `Wire`.
fn declared_in(source: &str) -> Vec<String> {
  let mut declared = Vec::new();
  let mut derives_wire = false;
  for line in source.lines().map(str::trim) {
    if line.starts_with("#[derive(") {
      derives_wire = line.contains("Wire");
      continue;
    }
    if let Some(rest) = line.strip_prefix("pub enum ")
      && derives_wire
    {
      declared.extend(
        rest
          .split(|c: char| !c.is_alphanumeric() && c != '_')
          .next()
          .map(str::to_owned),
      );
    }
    if line.starts_with("pub enum ") || line.starts_with("pub struct ") || line.starts_with("impl ")
    {
      derives_wire = false;
    }
  }
  declared
}

/// The coverage: every enum the sources under `source_dir` derive `Wire` for is among `schemas`, and only those, so a
/// new wire enum cannot go unpublished.
pub fn assert_held(source_dir: &Path, schemas: &[&str]) {
  let mut declared: Vec<String> = sources_under(source_dir)
    .iter()
    .flat_map(|path| declared_in(&std::fs::read_to_string(path).expect("a source is readable")))
    .collect();
  declared.sort();
  let mut held: Vec<String> = current(schemas).into_keys().collect();
  held.sort();
  assert_eq!(
    declared, held,
    "the wire enums the crate's sources declare, against those its guard holds (add a missing one to `schemas`)"
  );
}

/// The writer's text: `schemas` as published, after refusing while any published tag moved, so a move is never
/// published by running the writer.
pub fn publication(schemas: &[&str], published_text: &str) -> String {
  let current = current(schemas);
  let (_, moved) = differences(&current, &published(published_text));
  assert!(
    moved.is_empty(),
    "a published tag moved; undo the move, never publish it:\n{}",
    moved.join("\n")
  );
  let lines: Vec<String> = current
    .into_iter()
    .flat_map(|(name, variants)| {
      variants
        .into_iter()
        .enumerate()
        .map(move |(tag, variant)| format!("{name} {tag} {variant}"))
    })
    .collect();
  lines.join("\n") + "\n"
}
