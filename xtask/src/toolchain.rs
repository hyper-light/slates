//! `cargo xtask toolchain`: every place that picks a Rust toolchain picks the one `rust-toolchain.toml` pins
//! (`docs/bugs/2026-10-27-rust-1-98-0-miscompiles-boxed-iterator-chains.md`).
//!
//! The toolchain file is the only hand-edited toolchain in the tree. Everything else that selects one must name
//! exactly its channel: a container image (`rust:X.Y.Z`, or a variant of it such as `rust:X.Y.Z-alpine`), a build's
//! `RUSTUP_TOOLCHAIN=`, a `cargo +X.Y.Z` or `rustc +X.Y.Z`, and a workflow's `toolchain:` or `rust-toolchain:` input. A
//! floating tag (`rust:1.98`, `rust:alpine`) is drift too, since it takes whichever release a machine has cached.
//! A named nightly is allowed: Miri and ThreadSanitizer need one, and the nightly run follows it on purpose
//! (`.github/workflows/ci.yml`, its schedule).
//!
//! Why a check: the move to 1.98.1 (7318e744), made because 1.98.0 miscompiles boxed iterator chains, changed every
//! setup step and missed seven image and build pins. A sweep found them the same day. This check runs that sweep on
//! every `cargo xtask check`.
//!
//! What it reads: every file git tracks (`git ls-files`, so no build output), except prose, which records what ran on
//! a date: `docs/`, Markdown, and comment lines (`#`, `//`). `vendor/` holds other projects' trees, which slates does
//! not build with their own pins. This module's own file is skipped too, since its tests spell drift on purpose.

use std::path::Path;
use std::process::Command;

use crate::Failure;

/// Format: the toolchain file, the one hand-edited pin.
const TOOLCHAIN_FILE: &str = "rust-toolchain.toml";

/// Format: the directories this check does not read: prose records, and other projects' vendored trees.
const SKIPPED_PREFIXES: [&str; 2] = ["docs/", "vendor/"];
/// Format: the file kinds this check does not read: Markdown is prose, which records what ran on a date.
const SKIPPED_SUFFIXES: [&str; 1] = [".md"];
/// Format: this module's own file, whose tests spell drift on purpose.
const THIS_FILE: &str = "xtask/src/toolchain.rs";

/// Format: the spellings that pick a toolchain by what follows them on the line.
const VALUE_MARKERS: [&str; 3] = ["RUSTUP_TOOLCHAIN=", "cargo +", "rustc +"];
/// Format: the workflow inputs that pick a toolchain, as a line's first key.
const INPUT_KEYS: [&str; 2] = ["toolchain:", "rust-toolchain:"];
/// Format: an image reference to the Rust image, matched where no name character comes before it.
const IMAGE: &str = "rust:";
/// Format: a named nightly, the one toolchain allowed besides the pinned channel.
const NIGHTLY: &str = "nightly";
/// Format: rustup's channel names (rustup's "Toolchain specification"), each of which a workflow input may name.
const RUSTUP_CHANNELS: [&str; 3] = ["stable", "beta", NIGHTLY];

/// `cargo xtask toolchain`: refuses, naming each file and line, where a pin names anything but the channel.
pub(crate) fn run(root: &Path) -> Result<(), Failure> {
  let channel = channel_of(&std::fs::read_to_string(root.join(TOOLCHAIN_FILE))?)?;
  let listed = Command::new("git")
    .arg("-C")
    .arg(root)
    .args(["ls-files", "-z"])
    .output()?;
  if !listed.status.success() {
    return Err(Failure(format!(
      "toolchain: git ls-files failed: {}",
      String::from_utf8_lossy(&listed.stderr).trim()
    )));
  }
  let mut drift = Vec::new();
  for name in listed.stdout.split(|byte| *byte == 0) {
    let name = String::from_utf8_lossy(name);
    if name.is_empty() || is_skipped(&name) {
      continue;
    }
    // A file that is not text (an image, an archive) names no toolchain.
    let Ok(text) = std::fs::read_to_string(root.join(name.as_ref())) else {
      continue;
    };
    drift.extend(drift_in(&name, &text, &channel));
  }
  if drift.is_empty() {
    println!("toolchain: every pin names {channel}");
    return Ok(());
  }
  Err(Failure(format!(
    "toolchain: {} pin(s) name something other than {TOOLCHAIN_FILE}'s {channel}:\n  {}",
    drift.len(),
    drift.join("\n  ")
  )))
}

/// The channel the toolchain file pins (`[toolchain] channel`).
fn channel_of(text: &str) -> Result<String, Failure> {
  let parsed: toml::Table = toml::from_str(text)
    .map_err(|e| Failure(format!("toolchain: {TOOLCHAIN_FILE} does not parse: {e}")))?;
  parsed
    .get("toolchain")
    .and_then(|table| table.get("channel"))
    .and_then(toml::Value::as_str)
    .map(str::to_owned)
    .ok_or_else(|| {
      Failure(format!(
        "toolchain: {TOOLCHAIN_FILE} names no [toolchain] channel"
      ))
    })
}

/// Whether the check leaves `name` unread (the module doc says why).
fn is_skipped(name: &str) -> bool {
  name == THIS_FILE
    || SKIPPED_PREFIXES
      .iter()
      .any(|prefix| name.starts_with(prefix))
    || SKIPPED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Each pin in `text` (the file `name`) that names something other than `channel`, as `name:line: ...`.
fn drift_in(name: &str, text: &str, channel: &str) -> Vec<String> {
  let mut drift = Vec::new();
  for (index, line) in text.lines().enumerate() {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with("//") {
      continue;
    }
    for named in pins_in(trimmed) {
      if !names_the_channel(&named, channel) {
        drift.push(format!(
          "{name}:{}: names `{named}`",
          index.saturating_add(1)
        ));
      }
    }
  }
  drift
}

/// Every toolchain the (uncommented) line picks, as written.
fn pins_in(line: &str) -> Vec<String> {
  let mut pins = Vec::new();
  for marker in VALUE_MARKERS {
    for (at, _) in line.match_indices(marker) {
      if let Some(rest) = line.get(at.saturating_add(marker.len())..) {
        pins.push(token(rest));
      }
    }
  }
  for (at, _) in line.match_indices(IMAGE) {
    let before = line.get(..at).and_then(|head| head.chars().next_back());
    if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
      continue;
    }
    if let Some(rest) = line.get(at.saturating_add(IMAGE.len())..) {
      pins.push(token(rest));
    }
  }
  let key_line = line.trim_start_matches("- ").trim_start();
  for key in INPUT_KEYS {
    if let Some(rest) = key_line.strip_prefix(key) {
      let value = token(rest.trim_start());
      if is_a_rustup_toolchain(&value) {
        pins.push(value);
      }
    }
  }
  pins
}

/// Whether a workflow input's `value` names a rustup toolchain (a release, or a channel by name) rather than
/// something else that shares the key: kiln's `toolchain: rust` names a job's toolchain family, and its jobs run
/// `rustup toolchain install`, which reads the toolchain file.
fn is_a_rustup_toolchain(value: &str) -> bool {
  value.starts_with(|c: char| c.is_ascii_digit())
    || RUSTUP_CHANNELS
      .iter()
      .any(|channel| value.starts_with(channel))
}

/// The toolchain token at the start of `rest`: up to whitespace, a quote, a backtick or a YAML comment.
fn token(rest: &str) -> String {
  rest
    .chars()
    .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '`' | '#' | ',' | ')'))
    .collect()
}

/// Whether `named` picks the pinned `channel`: exactly it, an image variant of it (`1.98.1-alpine`), or a nightly.
fn names_the_channel(named: &str, channel: &str) -> bool {
  named == channel
    || named
      .strip_prefix(channel)
      .is_some_and(|variant| variant.starts_with('-'))
    || named.starts_with(NIGHTLY)
}

#[cfg(test)]
mod tests {
  use super::*;

  const CHANNEL: &str = "1.98.1";

  /// Do: judge lines that name the pinned channel in every spelling the tree uses. Expect: no drift.
  #[test]
  fn every_spelling_of_the_pinned_channel_passes() {
    let text = "FROM rust:1.98.1 AS build\n\
                image: docker.io/library/rust:1.98.1-alpine\n\
                    RUSTUP_TOOLCHAIN=1.98.1 cargo build --release\n\
                          toolchain: 1.98.1 # keep in sync with rust-toolchain.toml\n\
                          rust-toolchain: 1.98.1\n\
                run: cargo +nightly miri test -p slates-mem\n\
                const IMAGE: &str = \"rust:1.98.1\";\n";
    assert_eq!(drift_in("f", text, CHANNEL), Vec::<String>::new());
  }

  /// Do: judge each stale or floating pin 7318e744 missed. Expect: each is refused with its file and line.
  #[test]
  fn a_stale_or_floating_pin_is_refused_with_its_line() {
    let text = "FROM rust:1.98.0\n\
                FROM rust:1.98 AS build\n\
                    RUSTUP_TOOLCHAIN=1.98.0 cargo build\n\
                docker run --rm -v \"$PWD\":/work rust:1.98-alpine sh -c\n\
                const IMAGE: &str = \"rust:1.98.0\";\n\
                          toolchain: stable\n\
                run: cargo +1.97.0 build\n";
    assert_eq!(
      drift_in("Dockerfile", text, CHANNEL),
      vec![
        "Dockerfile:1: names `1.98.0`",
        "Dockerfile:2: names `1.98`",
        "Dockerfile:3: names `1.98.0`",
        "Dockerfile:4: names `1.98-alpine`",
        "Dockerfile:5: names `1.98.0`",
        "Dockerfile:6: names `stable`",
        "Dockerfile:7: names `1.97.0`",
      ]
    );
  }

  /// Do: judge comment lines, names that merely end in "rust:", and kiln's toolchain family. Expect: none is read as
  /// a pin.
  #[test]
  fn comments_and_other_names_are_not_pins() {
    let text = "# `docker run rust:1.98.0` resolved to a cached image\n\
                  // rust:1.98.0 image, on 2026-10-03\n\
                let trust: u8 = 0;\n\
                mistrust:1.0\n\
                    toolchain: rust\n";
    assert_eq!(drift_in("f", text, CHANNEL), Vec::<String>::new());
  }

  /// Do: read the channel of a toolchain file shaped like the tree's. Expect: its channel; a file without one is
  /// refused.
  #[test]
  fn the_channel_is_read_from_the_toolchain_file() {
    let file = "# a comment\n[toolchain]\nchannel = \"1.98.1\"\ncomponents = [\"rustfmt\"]\n";
    assert_eq!(channel_of(file).ok().as_deref(), Some(CHANNEL));
    assert!(channel_of("[toolchain]\n").is_err());
  }

  /// Do: run the check over this repository. Expect: every pin names the toolchain file's channel.
  #[test]
  fn this_tree_names_one_toolchain() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    assert!(run(&root).is_ok(), "{:?}", run(&root).err());
  }
}
