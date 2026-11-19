//! A-640 part 2's tests: upload-pack's service held to stock git 2.50.1's own answers, from the repository git answered
//! from (A-582's fixtures, loaded into A-614's store by `crate::served`), with a pack source of whole objects standing
//! in for A-639's.

mod advertise;
mod clone;
mod fetch;
mod fragments;
mod ls_refs;
mod refused;
mod steps;
mod v0;

use std::collections::BTreeSet;
use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::{Answer, Asked, PackPlan, PackRefused, PackSource, Served, Written, serve};
use crate::Admission;
use crate::entry::{self, Representation};
use crate::http::Version;
use crate::negotiation::Graph;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::pack;
use crate::pktline::{self, Packet};
use crate::served::{self, REFS};
use crate::store::{NamedRef, StoreGraph};
use crate::unpack::Object;
use crate::v2::fetch::{Event, decode_response};

/// The served repository's default branch.
pub(super) const MAIN_REF: &[u8] = b"refs/heads/main";

/// What a pack of whole objects keeps while it is written: its bytes, and how many are written.
pub(super) type WholePack = (Vec<u8>, usize);

/// The fixture repository as upload-pack serves it, with `refs` and `head`.
pub(super) fn serving<'r>(
  fixture: &'r served::Served,
  refs: &'r [NamedRef<'r>],
  head: &'r [u8],
) -> Served<'r> {
  Served {
    repository: &fixture.repository,
    store: &fixture.store,
    refs,
    head,
  }
}

/// A pack of whole objects, the tests' stand-in for A-639: every object the plan's runs brought that its filter admits,
/// and every single object, each written whole and deflated, in position order.
pub(super) struct WholeObjects<'o> {
  /// Every object's bytes, by name.
  pub(super) objects: &'o [Object],
  /// Packs begun.
  pub(super) begun: u64,
}

impl<'o> WholeObjects<'o> {
  /// A source of `objects`' bytes.
  pub(super) fn new(objects: &'o [Object]) -> Self {
    Self { objects, begun: 0 }
  }
}

impl PackSource for WholeObjects<'_> {
  type Pack = WholePack;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    _: &mut Admission,
  ) -> Result<WholePack, PackRefused> {
    self.begun += 1;
    let graph = StoreGraph::new(served.repository, served.store, served.refs);
    Ok((
      whole_pack(&graph, &positions(&graph, plan), self.objects),
      0,
    ))
  }

  fn write(
    &mut self,
    _: &Served<'_>,
    pack: &mut WholePack,
    _: &mut Admission,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    let (bytes, at) = pack;
    let rest = bytes.get(*at..).unwrap_or_default();
    let chunk = rest.get(..room).unwrap_or(rest);
    out.extend_from_slice(chunk);
    *at += chunk.len();
    Ok(*at < bytes.len())
  }
}

/// Every object `plan` puts in the pack, by position: each run's versions' objects its filter admits, and each single
/// object.
pub(super) fn positions(graph: &StoreGraph<'_>, plan: &PackPlan<'_>) -> BTreeSet<u64> {
  let mut positions: BTreeSet<u64> = plan.objects.iter().copied().collect();
  for run in plan.runs {
    for version in run.after + 1..=run.through {
      positions.extend(graph.brought(run.green, version).filter(|node| {
        plan
          .filter
          .is_none_or(|filter| filter.admits(graph.kind(*node), graph.size(*node), 0))
      }));
    }
  }
  positions
}

/// A pack of the objects at `positions`, each whole and deflated, their bytes from `objects`.
pub(super) fn whole_pack(
  graph: &StoreGraph<'_>,
  positions: &BTreeSet<u64>,
  objects: &[Object],
) -> Vec<u8> {
  let entries: Vec<(Kind, &[u8])> = positions
    .iter()
    .map(|node| {
      let id = graph.id(*node);
      let object = objects.iter().find(|object| object.id == id).unwrap();
      (object.kind, object.content.as_slice())
    })
    .collect();
  pack_of(&entries)
}

/// A pack of `entries`, each object whole and deflated, in order.
pub(super) fn pack_of(entries: &[(Kind, &[u8])]) -> Vec<u8> {
  let mut bytes = Vec::new();
  let count = u32::try_from(entries.len()).unwrap();
  let mut writer = pack::Writer::new(ObjectFormat::Sha1, count, &mut bytes);
  let mut at = entry::PACK_HEADER_LEN;
  for (kind, content) in entries {
    let header = entry::Header {
      representation: Representation::Whole(*kind),
      size: u64::try_from(content.len()).unwrap(),
    };
    let mut written = Vec::new();
    entry::write(header, at, &mut written).unwrap();
    let mut deflate = ZlibEncoder::new(Vec::new(), Compression::default());
    deflate.write_all(content).unwrap();
    written.extend(deflate.finish().unwrap());
    writer.put(&written);
    at += u64::try_from(written.len()).unwrap();
  }
  writer.finish().unwrap();
  bytes
}

/// What `asked` is answered with by the fixture repository, served with `refs` and `head` at `version`, written in one
/// step, and the answer.
pub(super) fn answer_of(
  fixture: &served::Served,
  refs: &[NamedRef<'_>],
  head: &[u8],
  version: Version,
  asked: Asked<'_>,
) -> (Vec<u8>, Answer<WholePack>) {
  let served = serving(fixture, refs, head);
  let mut source = WholeObjects::new(&fixture.objects);
  let mut admission = Admission::new(u64::MAX);
  let mut answer = serve(&served, version, asked, &mut source, &mut admission);
  let mut bytes = Vec::new();
  assert_eq!(
    answer.write(&served, &mut source, &mut admission, &mut bytes, usize::MAX),
    Written::End
  );
  (bytes, answer)
}

/// What the v2 request `request` is answered with by the fixture repository as A-582's fixtures name it, and the
/// answer.
pub(super) fn answer_v2(fixture: &served::Served, request: &[u8]) -> (Vec<u8>, Answer<WholePack>) {
  answer_of(
    fixture,
    &REFS,
    MAIN_REF,
    Version::V2,
    Asked::Request(request),
  )
}

/// A data packet of `text`.
pub(super) fn pkt(text: &[u8]) -> Vec<u8> {
  let mut bytes = Vec::new();
  pktline::write(Packet::Data(text), &mut bytes).unwrap();
  bytes
}

/// A v2 command request: `command=<command>`, `object-format=sha1`, the delimiter, each argument on a line ending with
/// an LF, the flush.
pub(super) fn command(command: &str, arguments: &[&[u8]]) -> Vec<u8> {
  let mut bytes = pkt(format!("command={command}\n").as_bytes());
  bytes.extend(pkt(b"object-format=sha1\n"));
  bytes.extend(b"0001");
  for argument in arguments {
    bytes.extend(pkt(&[*argument, b"\n"].concat()));
  }
  bytes.extend(b"0000");
  bytes
}

/// `prefix`, a space, and the id `hex` writes: an argument naming an object.
pub(super) fn naming(prefix: &str, hex: &str) -> Vec<u8> {
  format!("{prefix} {hex}").into_bytes()
}

/// The events of a `fetch` response but its pack's data and its progress, and the names of the objects its pack
/// holds, a thin pack's bases taken from `objects`.
pub(super) fn read_fetch<'b>(
  response: &'b [u8],
  multiplexed: bool,
  objects: &[Object],
) -> (Vec<Event<'b>>, BTreeSet<ObjectId>) {
  let events = decode_response(
    response,
    ObjectFormat::Sha1,
    multiplexed,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let pack: Vec<u8> = events
    .iter()
    .filter_map(|event| match event {
      Event::Pack(bytes) => Some(*bytes),
      _ => None,
    })
    .flatten()
    .copied()
    .collect();
  let others = events
    .into_iter()
    .filter(|event| !matches!(event, Event::Pack(_) | Event::Progress(_)))
    .collect();
  let names = if pack.is_empty() {
    BTreeSet::new()
  } else {
    served::pack_names(&pack, objects)
  };
  (others, names)
}
