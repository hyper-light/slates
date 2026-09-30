//! Removal under outsider interference over the simulated host (§4.15 step 6; D-26; AUD-29-04): an entry
//! the landing removes or displaces is removed only as the object it moved to one of its own hidden names
//! and checked there, so an outsider's replacement, made at any moment, is never removed in the witnessed
//! object's place, and a removal the outsider made stale ends in a conflict. The rules are
//! `common::removal`'s; `tests/os_removal.rs` holds a real directory to the same ones.
//!
//! Until 2026-09-29 a delete opened and checked the file, closed it, then unlinked the name, so a
//! replacement in between was removed; a symlink or other entry was unlinked unchecked; a directory removal
//! and a directory rename checked the directory, then moved whatever held the name; and the exchange
//! fallback renamed the new file over whatever held the name after its check
//! (docs/bugs/2026-09-29-a-landing-removal-could-remove-an-outsiders-replacement.md).
//!
//! Each history lands one entry that removes, replaces or renames a base entry at `/doomed` while an outsider
//! replaces a name with a file of its own. A reference run counts the landing's host calls; one fresh history
//! per call then arms the replacement just before that call, and one per pair of calls arms two.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_land::engine::{
  LandingRefusal, LandingReport, LandingRequest, LandingTarget, Unobserved,
};
use slates_land::grant::{GrantScope, Surface};
use slates_vfs::base::BaseConfig;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::{Interference, SimHost};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::removal::{
  Fired, LATER_OUTSIDER, OUTSIDER, REMOVALS, Removal, Seen, assert_reference, judge,
};
use common::{
  LARGE, Session, Setup, TERM_NS, config, mkdir, rename, request, rm_r, store, unlink, write_file,
};

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    config(),
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

fn root_target(host: &mut SimHost) -> LandingTarget {
  LandingTarget {
    dir: host.root(),
    key: "/".into(),
  }
}

/// A disk holding the base entry at `/doomed` and an overlay over it that removes, replaces or renames it.
fn history(removal: Removal) -> (SimHost, Store, Volume) {
  let mut host = SimHost::new();
  host.set_exchange_supported(removal.exchange());
  match removal {
    Removal::File | Removal::Replace { .. } => host.replace_file("/doomed", b"base bytes"),
    Removal::Symlink => host.symlink("/doomed", "somewhere"),
    Removal::Directory | Removal::Clear { .. } | Removal::Rename | Removal::RenameOnto => {
      host.mkdir("/doomed");
      host.replace_file("/doomed/inner", b"base inner");
    }
  }
  let mut store = store();
  let mut vol = overlay(&mut host, &mut store);
  let (v, h, s) = (&mut vol, &mut host, &mut store);
  match removal {
    Removal::File | Removal::Symlink => unlink(v, h, s, "/doomed"),
    Removal::Directory => rm_r(v, h, s, "/doomed"),
    Removal::Replace { .. } => write_file(v, h, s, "/doomed", b"the landing's bytes"),
    Removal::Clear { .. } => {
      rm_r(v, h, s, "/doomed");
      mkdir(v, h, s, "/doomed");
    }
    Removal::Rename | Removal::RenameOnto => rename(v, h, s, "/doomed", "/moved"),
  }
  (host, store, vol)
}

/// One landing of a history.
struct Run {
  /// The host after.
  host: SimHost,
  /// The landing's result.
  result: Result<LandingReport, LandingRefusal>,
  /// The host calls the granted landing made.
  calls: u64,
  /// The inode of the witnessed base entry at `/doomed`.
  witnessed: u64,
}

impl Run {
  /// What the landing left, for the rules.
  fn seen(&self) -> Seen<'_> {
    Seen {
      all_fired: self.host.interfered(),
      fired: self
        .host
        .interferences()
        .iter()
        .map(|i| Fired {
          created: i.created,
          displaced: i.displaced,
        })
        .collect(),
      inodes: self
        .host
        .paths()
        .into_iter()
        .filter_map(|(path, _)| self.host.fingerprint(&path).map(|fp| (fp.ino, path)))
        .collect(),
      witnessed: self.witnessed,
      result: &self.result,
    }
  }
}

/// Presents and grants the history's landing, arms each outsider edit `(n, bytes)` to replace the
/// history's interfered path just before the `n`-th host call of the granted landing, and lands.
fn run(removal: Removal, edits: &[(u64, &[u8])]) -> Run {
  let (mut host, mut store, mut vol) = history(removal);
  let witnessed = host.fingerprint("/doomed").unwrap().ino;
  let target = root_target(&mut host);
  let mut session = Session::new();
  let presented = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .present(&request(1));
  let granted = LandingRequest {
    grant: session.grants.issue(
      Surface::Cli,
      presented.manifest.hash,
      presented.binding.clone(),
      GrantScope::Once,
      1,
      TERM_NS,
    ),
    ..request(1)
  };
  for (n, bytes) in edits {
    host.interfere_at_call(
      *n,
      Interference::Replace {
        path: removal.interfered().to_owned(),
        bytes: bytes.to_vec(),
      },
    );
  }
  let before = host.calls();
  let result = Setup {
    host: &mut host,
    target: &target,
    vol: &mut vol,
    store: &mut store,
    session: &mut session,
  }
  .try_land(&granted, &mut Unobserved);
  let calls = host.calls().saturating_sub(before);
  Run {
    host,
    result,
    calls,
    witnessed,
  }
}

/// The reference run of `removal`, held to the reference rules; the landing's host calls.
fn reference(removal: Removal) -> u64 {
  let run = run(removal, &[]);
  assert_reference(removal, &run.seen());
  run.calls
}

/// AUD-29-04. Do: for a deleted file and symlink, a removed directory, a replaced file and a cleared
/// directory (each with and without the atomic exchange), and a renamed directory (the outsider at its origin
/// and at its destination), run the granted landing once to count its host calls, then once per call with
/// the outsider replacing the name with its own file just before that call. Expect: every history keeps
/// `common::removal`'s rules — the outsider's file holds its name, nothing of the landing's is left
/// unreported, a stale entry is not written, an unwritten one removed nothing, a written removal removed its
/// witnessed object — and across each kind's histories both a stale entry and a written one occurred
/// (neither rule passes vacuously).
#[test]
fn an_outsider_replacement_at_any_host_call_survives_every_removal() {
  for removal in REMOVALS {
    let calls = reference(removal);
    let (mut stale, mut written) = (0u64, 0u64);
    for n in 0..calls {
      let run = run(removal, &[(n, OUTSIDER)]);
      let verdict = judge(removal, &format!("{removal:?} at call {n}"), &run.seen());
      stale += u64::from(verdict.stale);
      written += u64::from(verdict.written);
    }
    assert!(
      written > 0 && (stale > 0 || matches!(removal, Removal::RenameOnto)),
      "{removal:?}: {stale} stale and {written} written histories of {calls}"
    );
  }
}

/// AUD-29-04, the undo paths. Do: for every kind, arm a first outsider edit before one host call and a
/// second before a later one — so a second edit can land between an exchange and the exchange back, or
/// between a move aside and the put back, which one edit never reaches. Expect: every rule holds for every
/// pair; in particular no outsider file is lost that the later edit did not itself replace, and an entry of
/// the outsider's the landing could not put back is reported kept.
#[test]
fn two_outsider_replacements_at_any_two_host_calls_lose_nothing_of_theirs() {
  for removal in REMOVALS {
    let calls = reference(removal);
    let mut judged = 0u64;
    for first in 0..calls {
      // The first edit changes the landing's path; the second may meet any call of that changed path.
      let after_first = run(removal, &[(first, OUTSIDER)]).calls;
      for second in first.saturating_add(1)..after_first {
        let run = run(removal, &[(first, OUTSIDER), (second, LATER_OUTSIDER)]);
        judge(
          removal,
          &format!("{removal:?} at calls {first} and {second}"),
          &run.seen(),
        );
        judged += 1;
      }
    }
    assert!(judged > 0, "{removal:?}: no pair judged");
  }
}
