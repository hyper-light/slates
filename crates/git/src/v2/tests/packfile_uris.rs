use proptest::prelude::*;

use super::{Choice, UriPack, choose};
use crate::object_name::{ObjectFormat, ObjectId};
use crate::v2::fetch::{self, Argument as F, Event};
use crate::v2::request::{self, Arguments, Request};
use crate::v2::tests::room;
use crate::v2::{Line, Refusal};
use crate::v2_rest_fixtures::{self as rest, id};
use crate::{Admission, PastAdmission, to_u64};

/// The fetch arguments of a request stock git answered.
fn arguments(stream: &[u8]) -> Vec<Line<F<'_>>> {
  match request::decode(stream, &mut room()).unwrap() {
    Request::Command(request) => match request.arguments {
      Arguments::Fetch(arguments) => arguments,
      other => panic!("not fetch: {other:?}"),
    },
    Request::End => panic!("no command"),
  }
}

/// The pack the script's server names by URI, its hash and its URI as `uri-pack.txt` records them.
fn served_pack() -> (ObjectId, &'static [u8]) {
  let text = std::str::from_utf8(rest::URI_PACK_TXT).unwrap().trim_end();
  let (hash, uri) = text.split_once(' ').unwrap();
  (id(ObjectFormat::Sha1, hash), uri.as_bytes())
}

/// A pack offered for `first..=last`, its URI `uri`, self-contained or not, named by `hash`.
fn pack(first: usize, last: usize, self_contained: bool, uri: &'static [u8]) -> UriPack<'static> {
  UriPack {
    first,
    last,
    self_contained,
    hash: ObjectId::from_bytes(ObjectFormat::Sha1, &[u8::try_from(first).unwrap(); 20]).unwrap(),
    uri,
  }
}

/// The choice for `arguments` over `units` and `offered`, with room to spare.
fn chosen(arguments: &[Line<F<'_>>], units: usize, offered: &[UriPack<'_>]) -> Choice {
  choose(arguments, units, offered, &mut room()).unwrap()
}

/// A request's arguments: `sideband-all` where `sideband_all`, `packfile-uris <protocols>` where there are protocols,
/// `thin-pack` where `thin`.
fn asking(
  protocols: Option<&'static [u8]>,
  sideband_all: bool,
  thin: bool,
) -> Vec<Line<F<'static>>> {
  let mut arguments = vec![Line::lf(F::OfsDelta)];
  if sideband_all {
    arguments.push(Line::lf(F::SidebandAll));
  }
  if thin {
    arguments.push(Line::lf(F::ThinPack));
  }
  if let Some(list) = protocols {
    arguments.push(Line::lf(F::PackfileUris(list)));
  }
  arguments
}

/// A-624, golden vectors: do choose, for each fetch stock git 2.50.1 answered from a server naming the larger blob's
/// pack by URI, over the run git's answer splits (the rest inline, then the blob's pack), with that pack offered; and
/// read git's answer. Expect: git's decision every time, the pack by URI to a client taking `https`, or `ftp` and
/// `https`, under `sideband-all`, and nothing by URI to one taking only `http` or not under `sideband-all`; and the
/// section's lines the ones git wrote.
#[test]
fn every_choice_stock_git_made_is_made_here_and_named_as_git_named_it() {
  let (hash, uri) = served_pack();
  let offered = [UriPack {
    first: 1,
    last: 1,
    self_contained: true,
    hash,
    uri,
  }];
  for (name, request, response, multiplexed, by_uri) in [
    (
      "https",
      rest::URIS_HTTPS_REQUEST,
      rest::URIS_HTTPS_RESPONSE,
      true,
      true,
    ),
    (
      "ftp,https",
      rest::URIS_FTP_HTTPS_REQUEST,
      rest::URIS_FTP_HTTPS_RESPONSE,
      true,
      true,
    ),
    (
      "http",
      rest::URIS_HTTP_REQUEST,
      rest::URIS_HTTP_RESPONSE,
      true,
      false,
    ),
    (
      "no sideband-all",
      rest::URIS_NO_SIDEBAND_ALL_REQUEST,
      rest::URIS_NO_SIDEBAND_ALL_RESPONSE,
      false,
      false,
    ),
  ] {
    let choice = chosen(&arguments(request), 2, &offered);
    let gits: Vec<Event<'_>> =
      fetch::decode_response(response, ObjectFormat::Sha1, multiplexed, &mut room())
        .unwrap()
        .into_iter()
        .filter(|event| matches!(event, Event::PackfileUri(_)))
        .collect();
    let expected = if by_uri {
      Choice {
        inline: 1,
        by_uri: vec![0],
      }
    } else {
      Choice {
        inline: 2,
        by_uri: Vec::new(),
      }
    };
    let ours: Vec<Event<'_>> = choice.uris(&offered).collect();
    assert_eq!((&choice, ours), (&expected, gits), "{name}");
  }
}

/// A-624 (`fetch-pack.c` indexes a URI's pack with the inline pack's own arguments, `--fix-thin` only under
/// `thin-pack`): do choose for a client taking `https` under `sideband-all`, a thin pack offered, with and without
/// `thin-pack`; then a self-contained one without. Expect: the thin pack by URI only to the client that asked for
/// `thin-pack`; the self-contained one to either.
#[test]
fn without_thin_pack_only_a_self_contained_pack_goes_by_uri() {
  let thin = [pack(1, 1, false, b"https://example.invalid/thin.pack")];
  let whole = [pack(1, 1, true, b"https://example.invalid/whole.pack")];
  let by_uri = Choice {
    inline: 1,
    by_uri: vec![0],
  };
  let inline = Choice {
    inline: 2,
    by_uri: Vec::new(),
  };
  assert_eq!(
    (
      chosen(&asking(Some(b"https"), true, false), 2, &thin),
      chosen(&asking(Some(b"https"), true, true), 2, &thin),
      chosen(&asking(Some(b"https"), true, false), 2, &whole),
    ),
    (inline, by_uri.clone(), by_uri)
  );
}

/// A-624 (§4.17.2: the base pack and runs of fragments by URI; `fetch-pack.c`: the inline pack is indexed before any
/// URI's): do choose over a clone's run of four units, the base pack and three fragments, with offers of each shape.
/// Expect: the base pack alone sends nothing by URI, since the fragments after it would have to go inline after it; the
/// base pack and a run to the end send everything by URI, in order; a run at the end sends the run, the rest inline; of
/// several tilings the fewest packs; a run short of the end, a pack past the run and a URI of a protocol not taken send
/// nothing by URI.
#[test]
fn the_inline_pack_is_a_prefix_and_the_uris_tile_the_rest() {
  let https = |first, last| pack(first, last, false, b"https://example.invalid/run.pack");
  let arguments = asking(Some(b"https"), true, true);
  let choice = |offered: &[UriPack<'_>]| chosen(&arguments, 4, offered);
  let nothing = Choice {
    inline: 4,
    by_uri: Vec::new(),
  };
  assert_eq!(
    (
      choice(&[https(0, 0)]),
      choice(&[https(1, 3), https(0, 0)]),
      choice(&[https(2, 3)]),
      choice(&[https(1, 1), https(2, 2), https(3, 3), https(1, 3)]),
      choice(&[https(1, 2)]),
      choice(&[https(2, 5)]),
      choice(&[pack(2, 3, true, b"http://example.invalid/run.pack")]),
    ),
    (
      nothing.clone(),
      Choice {
        inline: 0,
        by_uri: vec![1, 0]
      },
      Choice {
        inline: 2,
        by_uri: vec![0]
      },
      Choice {
        inline: 1,
        by_uri: vec![3]
      },
      nothing.clone(),
      nothing.clone(),
      nothing,
    )
  );
}

/// A-624, bounds: do choose over three offered packs with room one byte short of the work's, then with exactly it.
/// Expect: refused `PastAdmission` with the admission untouched, then chosen with it spent.
#[test]
fn a_choice_past_its_admission_is_refused_before_it_is_made() {
  let offered = [
    pack(0, 0, true, b"https://example.invalid/a.pack"),
    pack(1, 1, true, b"https://example.invalid/b.pack"),
    pack(2, 2, true, b"https://example.invalid/c.pack"),
  ];
  let room = to_u64(3 * (2 * core::mem::size_of::<usize>() + core::mem::size_of::<[usize; 3]>()));
  let arguments = asking(Some(b"https"), true, false);
  let (mut short, mut exact) = (Admission::new(room - 1), Admission::new(room));
  assert_eq!(
    (
      choose(&arguments, 3, &offered, &mut short),
      short.left(),
      choose(&arguments, 3, &offered, &mut exact).map(|choice| choice.inline),
      exact.left(),
    ),
    (
      Err(Refusal::PastAdmission(PastAdmission {
        asked: room,
        left: room - 1
      })),
      room - 1,
      Ok(0),
      0,
    )
  );
}

/// The URIs a generated offer may have: one of each protocol the generated requests may take, and one of none.
const URIS: [&[u8]; 4] = [
  b"https://example.invalid/p.pack",
  b"http://example.invalid/p.pack",
  b"ftp://example.invalid/p.pack",
  b"httpsx://example.invalid/p.pack",
];

/// The protocol lists a generated request may take.
const PROTOCOLS: [&[u8]; 4] = [b"https", b"http", b"ftp,https", b"https,"];

/// What the choice must be, found by trying every set of the eligible packs: the smallest unit a set tiles from to the
/// run's end exactly, and the fewest packs that do; the run's length and no pack when none does.
fn oracle(
  units: usize,
  offered: &[UriPack<'_>],
  eligible: impl Fn(&UriPack<'_>) -> bool,
) -> (usize, usize) {
  let mut best = (units, 0);
  for set in 1u32..(1 << offered.len()) {
    let mut chosen: Vec<&UriPack<'_>> = offered
      .iter()
      .enumerate()
      .filter(|(index, _)| set & (1 << index) != 0)
      .map(|(_, pack)| pack)
      .collect();
    if !chosen.iter().all(|pack| eligible(pack)) {
      continue;
    }
    chosen.sort_by_key(|pack| pack.first);
    let tiles = chosen
      .windows(2)
      .all(|pair| pair[0].last + 1 == pair[1].first)
      && chosen.last().is_some_and(|pack| pack.last + 1 == units);
    let candidate = (chosen[0].first, chosen.len());
    if tiles && candidate < best {
      best = candidate;
    }
  }
  best
}

/// An offered pack's parts: its first unit, its length, whether it is self-contained, and its URI's index.
fn any_pack() -> impl Strategy<Value = (usize, usize, bool, usize)> {
  (0usize..7, 0usize..4, any::<bool>(), 0..URIS.len())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-624: do choose over any run of up to seven units, any packs offered (any runs, self-contained or not, any of
  /// four protocols), for any request (any protocol list, `sideband-all` or not, `thin-pack` or not), and try every
  /// set of packs. Expect: the choice the oracle finds, the smallest inline prefix and the fewest packs; and the packs
  /// chosen each eligible, tiling the run from the inline prefix to its end, in order.
  #[test]
  fn the_choice_is_the_longest_tiled_suffix_in_the_fewest_packs(
    units in 0usize..8,
    parts in proptest::collection::vec(any_pack(), 0..7),
    protocols in proptest::option::of(0..PROTOCOLS.len()),
    sideband_all in any::<bool>(),
    thin in any::<bool>(),
  ) {
    let offered: Vec<UriPack<'_>> = parts
      .iter()
      .map(|(first, len, self_contained, uri)| pack(*first, first + len, *self_contained, URIS[*uri]))
      .collect();
    let list = protocols.map(|index| PROTOCOLS[index]);
    let arguments = asking(list, sideband_all, thin);
    let choice = chosen(&arguments, units, &offered);
    let eligible = |pack: &UriPack<'_>| {
      let scheme = |protocol: &[u8]| pack.uri.strip_prefix(protocol).is_some_and(|rest| rest.first() == Some(&b':'));
      pack.last < units
        && (thin || pack.self_contained)
        && sideband_all
        && list.is_some_and(|list| list.split(|byte| *byte == b',').any(scheme))
    };
    let mut at = choice.inline;
    for index in &choice.by_uri {
      let pack = offered[*index];
      prop_assert!(eligible(&pack) && pack.first == at, "{:?} {:?}", choice, offered);
      at = pack.last + 1;
    }
    prop_assert_eq!(at, units, "{:?}", choice);
    prop_assert_eq!((choice.inline, choice.by_uri.len()), oracle(units, &offered, eligible));
  }
}
