//! The server's choice of what a fetch sends by URI (A-624; §4.17.2 "A fetch", item 3: "With `packfile-uris` or
//! `bundle-uri` the server may name the base pack and runs of fragments as static, content-addressed URIs that any
//! cache can hold"; gitprotocol-v2 "fetch", `packfile-uris` \[B\]; `upload-pack.c` `process_args`, `pack-objects.c`
//! `want_object_in_pack_mtime` (the server), `fetch-pack.c` `get_pack` and `do_fetch_pack_v2` (the client) \[C\]).
//!
//! A fetch sends a run of units in the order a pack carries them: the base pack where the fetch is a clone, then each
//! version's fragment, oldest first. The store may hold runs of those units as static packs, each named by a URI
//! ([`UriPack`]). The choice is which of them the client downloads by URI; the inline pack carries the rest, and the
//! response's `packfile-uris` section names the chosen packs in order ([`Choice::uris`]).
//!
//! **What git's client allows** (`fetch-pack.c`). It indexes the inline pack first, and only then downloads each URI's
//! pack, in the order the section lists them, indexing each with the inline pack's own arguments. With `thin-pack`
//! those arguments include `--fix-thin`, which completes a delta's base from what the repository already holds;
//! without it every pack must hold its own bases. So:
//! - the inline pack may depend only on what the client had: its units come first, a prefix of the run;
//! - each URI's pack may depend on the client's objects, the inline pack and the URI packs before it, so the chosen
//!   packs tile the rest of the run exactly, in order;
//! - without `thin-pack`, only a self-contained pack may go by URI.
//!
//! **What git's server allows** (`upload-pack.c`, `pack-objects.c`). A URI goes to a client that takes its protocol:
//! the URI begins with one of the protocols the request lists, comma-separated, then `:`. And only under
//! `sideband-all`: without it `upload-pack.c` clears the list. The golden vectors hold stock git to both rules.
//!
//! **The choice.** The longest suffix of the run that eligible packs tile exactly, in the fewest packs among those that
//! do; the inline pack takes the rest. It is found from the run's end back over the packs by their first unit (dynamic
//! programming), in time and room in proportion to the packs offered, whatever the run's length; the room is charged
//! to the caller's admission first.

use super::Refusal;
use super::fetch::{Argument, Event, PackfileUri};
use super::line::Line;
use crate::object_name::ObjectId;
use crate::{Admission, to_u64};

/// Format: the byte between the protocols a `packfile-uris` argument lists (`upload-pack.c`: `string_list_split(...,
/// ',', -1)`).
const PROTOCOL_SEPARATOR: u8 = b',';

/// Format: the byte after a URI's protocol (`pack-objects.c`: `skip_prefix(ex->uri, protocol, &p) && *p == ':'`).
const SCHEME_END: u8 = b':';

/// A static pack the store names by a URI, holding a run of a fetch's units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UriPack<'a> {
  /// The first unit it holds: 0 for the first unit the fetch sends.
  pub first: usize,
  /// The last unit it holds; a pack with `last` before `first`, or past the fetch's units, is not offered to it.
  pub last: usize,
  /// Whether it holds every delta's base itself, so a client that did not ask for `thin-pack` can index it.
  pub self_contained: bool,
  /// Its hash, as `git index-pack` prints it, in the repository's format.
  pub hash: ObjectId,
  /// Where a client downloads it.
  pub uri: &'a [u8],
}

/// What a fetch sends by URI, and what inline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
  /// How many units the inline pack carries, the first ones: units `0..inline`.
  pub inline: usize,
  /// The packs the client downloads by URI, as indices into the offered packs, in the order the section names them.
  pub by_uri: Vec<usize>,
}

impl Choice {
  /// The `packfile-uris` section's lines for the chosen packs, in order, each ended by an LF as git's server ends it
  /// (`upload-pack.c`'s `"\1%s\n"`); none when nothing goes by URI, and then the section is left out.
  pub fn uris<'a>(&'a self, offered: &'a [UriPack<'a>]) -> impl Iterator<Item = Event<'a>> + 'a {
    self.by_uri.iter().filter_map(move |index| {
      offered.get(*index).map(|pack| {
        Event::PackfileUri(Line::lf(PackfileUri {
          hash: pack.hash,
          uri: pack.uri,
        }))
      })
    })
  }
}

/// What a request's arguments allow: the protocols it takes URIs of, and whether a URI's pack may be thin.
#[derive(Clone, Copy, Debug, Default)]
struct Taken<'a> {
  /// The `packfile-uris` list, when the request has one and `sideband-all` with it.
  protocols: Option<&'a [u8]>,
  /// Whether the request asked for `thin-pack`.
  thin: bool,
}

impl<'a> Taken<'a> {
  /// What `arguments` allow.
  fn of(arguments: &[Line<Argument<'a>>]) -> Taken<'a> {
    let mut protocols = None;
    let mut sideband_all = false;
    let mut thin = false;
    for line in arguments {
      match line.value {
        Argument::PackfileUris(list) => protocols = Some(list),
        Argument::SidebandAll => sideband_all = true,
        Argument::ThinPack => thin = true,
        _ => {}
      }
    }
    Taken {
      protocols: protocols.filter(|_| sideband_all),
      thin,
    }
  }

  /// Whether `pack` may go by URI to the client within a run of `units`.
  fn allows(&self, pack: &UriPack<'_>, units: usize) -> bool {
    let fits = pack.first <= pack.last && pack.last < units;
    let indexable = self.thin || pack.self_contained;
    fits && indexable && self.protocols.is_some_and(|list| takes(list, pack.uri))
  }
}

/// Whether a client listing `protocols` takes `uri`: it begins with one of them and then `:`, as `pack-objects.c`
/// tests it.
fn takes(protocols: &[u8], uri: &[u8]) -> bool {
  protocols
    .split(|byte| *byte == PROTOCOL_SEPARATOR)
    .any(|protocol| {
      uri
        .strip_prefix(protocol)
        .is_some_and(|rest| rest.first() == Some(&SCHEME_END))
    })
}

/// The best tiling found from a unit to the run's end: how many packs it takes, and the first of them.
#[derive(Clone, Copy, Debug)]
struct Best {
  /// The unit it starts at.
  from: usize,
  /// The packs it takes.
  packs: usize,
  /// The first pack, as an index into the offered packs.
  pack: usize,
}

/// Chooses what a fetch whose request has `arguments` sends by URI, of a run of `units` and the packs `offered`.
/// Refused, nothing chosen, when the work would take more room than `admission` has left: room for each offered pack
/// twice, charged first.
pub fn choose(
  arguments: &[Line<Argument<'_>>],
  units: usize,
  offered: &[UriPack<'_>],
  admission: &mut Admission,
) -> Result<Choice, Refusal> {
  let taken = Taken::of(arguments);
  let room = core::mem::size_of::<usize>()
    .saturating_add(core::mem::size_of::<Best>())
    .saturating_add(core::mem::size_of::<usize>())
    .saturating_mul(offered.len());
  admission
    .charge(to_u64(room))
    .map_err(Refusal::PastAdmission)?;
  let mut eligible: Vec<usize> = (0..offered.len())
    .filter(|index| {
      offered
        .get(*index)
        .is_some_and(|pack| taken.allows(pack, units))
    })
    .collect();
  eligible.sort_unstable_by_key(|index| {
    let first = offered.get(*index).map_or(0, |pack| pack.first);
    (core::cmp::Reverse(first), *index)
  });
  let table = tile(&eligible, offered, units);
  Ok(follow(&table, offered, units))
}

/// The best tiling from each unit an eligible pack starts at to the run's end, found from the end back: the table, its
/// units falling, each entry the fewest packs (the earliest offered among equals).
fn tile(eligible: &[usize], offered: &[UriPack<'_>], units: usize) -> Vec<Best> {
  let mut table: Vec<Best> = Vec::with_capacity(eligible.len());
  for index in eligible {
    let Some(pack) = offered.get(*index) else {
      continue;
    };
    let next = pack.last.saturating_add(1);
    let rest = if next == units {
      Some(0)
    } else {
      found(&table, next).map(|best| best.packs)
    };
    let Some(rest) = rest else {
      continue;
    };
    let candidate = Best {
      from: pack.first,
      packs: rest.saturating_add(1),
      pack: *index,
    };
    keep(&mut table, candidate);
  }
  table
}

/// The table's entry for `unit`, if one starts there: the table is ordered by unit, falling.
fn found(table: &[Best], unit: usize) -> Option<Best> {
  table
    .binary_search_by(|best| unit.cmp(&best.from))
    .ok()
    .and_then(|at| table.get(at).copied())
}

/// Puts `candidate` in the table, or keeps the entry already there when it takes fewer packs, or as many and was
/// offered earlier. Candidates come with their units falling, so a new unit goes last.
fn keep(table: &mut Vec<Best>, candidate: Best) {
  match table.last_mut() {
    Some(last) if last.from == candidate.from => {
      let better = (candidate.packs, candidate.pack) < (last.packs, last.pack);
      if better {
        *last = candidate;
      }
    }
    _ => table.push(candidate),
  }
}

/// The choice the table makes: the inline pack up to the first unit a tiling starts at (the table's last entry, its
/// smallest), then that tiling's packs in order.
fn follow(table: &[Best], offered: &[UriPack<'_>], units: usize) -> Choice {
  let Some(start) = table.last() else {
    return Choice {
      inline: units,
      by_uri: Vec::new(),
    };
  };
  let mut by_uri = Vec::with_capacity(start.packs);
  let mut at = Some(*start);
  while let Some(best) = at {
    by_uri.push(best.pack);
    let next = offered
      .get(best.pack)
      .map_or(units, |pack| pack.last.saturating_add(1));
    at = if next < units {
      found(table, next)
    } else {
      None
    };
  }
  Choice {
    inline: start.from,
    by_uri,
  }
}

#[cfg(test)]
#[path = "tests/packfile_uris.rs"]
mod tests;
