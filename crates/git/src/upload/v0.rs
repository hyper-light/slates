//! Protocol v0 and v1, served (A-640 part 3; §4.17.2 "Pieces (git)" piece 14; A-604's codecs in `crate::v0`): the ref
//! advertisement, and a fetch's request answered statelessly, as `git upload-pack --stateless-rpc` answers each `POST`
//! of git's smart HTTP.
//!
//! **Sources** (read 2026-11-19, git v2.50.1): `upload-pack.c` (`send_ref`, `format_symref_info`, `upload_pack`,
//! `receive_needs`, `get_common_commits`, `got_oid`, `do_got_oid`, `ok_to_give_up`, `create_pack_file`,
//! `send_client_data`), `sideband.c` (`send_sideband`) and `pkt-line.h` (`DEFAULT_PACKET_MAX`) [C];
//! `gitprotocol-pack` and `gitprotocol-capabilities` [B].
//!
//! **The advertisement.** `version 1` first under v1. `HEAD` first where its branch has a commit, then each ref in name
//! order, an annotated tag's peeled line after it; a repository with no ref sends `capabilities^{}` in their place. The
//! capabilities ride on the first line, in `send_ref`'s order: `multi_ack thin-pack side-band side-band-64k ofs-delta
//! shallow deepen-since deepen-not deepen-relative no-progress include-tag multi_ack_detailed allow-tip-sha1-in-want
//! allow-reachable-sha1-in-want no-done`, then `symref=HEAD:<branch>` where `HEAD`'s branch has a commit, `filter`,
//! `object-format` and `agent`. The two `allow-*-in-want` tell the client a want need not be a tip, which holds here:
//! as under protocol v2, a want may be any object the repository holds.
//!
//! **A request** is read whole: the wants, the first carrying the client's capabilities (git reads them on any want,
//! and so does this), then any `shallow`, `deepen`, `deepen-since`, `deepen-not` and `filter`, then a flush; then the
//! haves, ended by `done` or a flush, the request's end read as a flush as git reads one. A capability the server did
//! not advertise is refused, as the document asks ("Server MUST diagnose and abort if capabilities it does not
//! understand were sent" [gitprotocol-capabilities]), where git 2.50.1 passes it; so is a line out of its place, or
//! bytes after the request's end.
//!
//! **The answer**, as `upload_pack` writes it statelessly. A request with no want is answered with nothing. A request
//! that deepens is answered first with the shallow-update and its flush (`receive_needs`). Then the acknowledgments,
//! have by have (`get_common_commits`):
//! - a have the repository lacks: once the server is ready, `ACK <it> continue` under `multi_ack`, `ACK <it> ready`
//!   under `multi_ack_detailed`;
//! - a held have: `ACK <id> common` under `multi_ack_detailed`, `ACK <id> continue` under `multi_ack`, and, with
//!   neither, `ACK <id>` while git counts one have (`have_obj`: a commit counts unless an earlier held commit names it
//!   as a parent or it was named already; any other object counts each time it is named).
//!
//! At `done`: `ACK <last held>` under either multi-ack when a have counts, `NAK` when none does, then the pack. At a
//! flush: `ACK <last held> ready` under `multi_ack_detailed` when a have was held, none was lacked and the server is
//! ready; `NAK` when no have counts or under either multi-ack; and, when the client asked for `no-done` and `ready` was
//! said, `ACK <last held>` and the pack. Otherwise the answer ends there, and the client asks again. Ready is A-615's,
//! with the held haves so far as the common objects, as git's `ok_to_give_up` reads its `THEY_HAVE` marks (a held
//! commit and its parents). It is asked again only when a held have was added since it was last asked, since lacked
//! haves change nothing it reads, and each asking keeps what it builds within the request's room and then lets it go.
//!
//! **The pack** is A-615's choice for the wants and every have, since `create_pack_file` takes them all. It is framed
//! on side-band-64k, on side-band (1000-byte packets, `DEFAULT_PACKET_MAX`) or on neither, as the client chose, with a
//! flush after it on either band. A `sparse:oid` filter's blob is read once the pack is due, as git's `pack-objects`
//! reads it (A-640 part 4); one it dies of is answered as git answers it: after the acknowledgments, its words on band
//! 2 and git's band-3 words on either band, and nothing with neither.
//!
//! **Stateless only.** Over SSH a v0 client speaks in rounds on one stream, and the server keeps what each round taught
//! it; that is owed (GAPS). git's client speaks protocol v2 over SSH since 2.26, and v2 is stateless by design.

use std::collections::BTreeSet;

use super::answer::{Answer, Framing};
use super::{AGENT, PackPlan, PackSource, Refusal, Served, charged, refs, sparse};
use crate::http::Version;
use crate::negotiation::{self, Deepen, Filter, Graph, ShallowInfo};
use crate::object_name::{ObjectFormat, ObjectId};
use crate::pktline::{self, Packet};
use crate::sideband::MAX_BAND_DATA;
use crate::store::StoreGraph;
use crate::v0::{
  self as codec, AckStatus, AdvertisedRef, Capabilities, Capability, ClientLine, ServerLine,
  ShallowUpdate,
};
use crate::{Admission, to_u64};

/// Format: the capabilities this server advertises that are a word alone, in `send_ref`'s order.
const FLAGS: [&[u8]; 15] = [
  MULTI_ACK,
  b"thin-pack",
  SIDE_BAND,
  SIDE_BAND_64K,
  b"ofs-delta",
  b"shallow",
  b"deepen-since",
  b"deepen-not",
  b"deepen-relative",
  b"no-progress",
  b"include-tag",
  MULTI_ACK_DETAILED,
  b"allow-tip-sha1-in-want",
  b"allow-reachable-sha1-in-want",
  b"no-done",
];

/// Format: the capabilities with a value a client may send back: its filter's (a word alone), its object format's
/// and its agent's (`fetch-pack.c` `find_common`).
const VALUED: [&[u8]; 3] = [FILTER, OBJECT_FORMAT, AGENT_NAME];

/// Format: `multi_ack` [gitprotocol-capabilities].
const MULTI_ACK: &[u8] = b"multi_ack";

/// Format: `multi_ack_detailed` [gitprotocol-capabilities].
const MULTI_ACK_DETAILED: &[u8] = b"multi_ack_detailed";

/// Format: `side-band`, packets of 1000 bytes [gitprotocol-capabilities].
const SIDE_BAND: &[u8] = b"side-band";

/// Format: `side-band-64k`, packets of 65520 bytes [gitprotocol-capabilities].
const SIDE_BAND_64K: &[u8] = b"side-band-64k";

/// Format: `no-done`: after `ACK ready`, the pack follows without the client's `done` (`upload-pack.c`).
const NO_DONE: &[u8] = b"no-done";

/// Format: `symref`, whose value is `HEAD:<branch>` (`format_symref_info`).
const SYMREF: &[u8] = b"symref";

/// Format: what `symref`'s value begins with, `HEAD` and a colon.
const HEAD_TO: &[u8] = b"HEAD:";

/// Format: `HEAD`'s name.
const HEAD: &[u8] = b"HEAD";

/// Format: `filter`: the server takes a partial clone's filter [gitprotocol-capabilities].
const FILTER: &[u8] = b"filter";

/// Format: `object-format`, naming the repository's hash [gitprotocol-capabilities].
const OBJECT_FORMAT: &[u8] = b"object-format";

/// Format: `agent`, naming the side's program [gitprotocol-capabilities].
const AGENT_NAME: &[u8] = b"agent";

/// Format: the most bytes a side-band packet takes, its length digits and band byte included (`pkt-line.h`
/// `DEFAULT_PACKET_MAX`; gitprotocol-capabilities "side-band": "up to 1000 bytes of data in each packet").
const SIDE_BAND_PACKET: usize = 1000;

/// Derived: the most bytes of a pack one side-band packet carries: [`SIDE_BAND_PACKET`] less what a band-1 packet
/// spends besides its data (a side-band-64k packet's [`pktline::MAX_LEN`] less its [`MAX_BAND_DATA`]: the four length
/// digits and the band byte), so 995, `send_sideband`'s `packet_max - 5`.
const SIDE_BAND_DATA: usize =
  SIDE_BAND_PACKET.saturating_sub(pktline::MAX_LEN.saturating_sub(MAX_BAND_DATA));

/// Shape: the acknowledgment lines an answer can end with after its haves': `ACK ready`, `NAK` and `ACK`.
const CLOSING_LINES: usize = 3;

/// Which multi-ack the client chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MultiAck {
  /// Neither.
  None,
  /// `multi_ack`.
  Plain,
  /// `multi_ack_detailed`.
  Detailed,
}

/// What the client chose among the capabilities.
#[derive(Clone, Copy, Debug)]
struct Chosen {
  /// Its multi-ack.
  multi_ack: MultiAck,
  /// `no-done`.
  no_done: bool,
  /// How its pack is framed.
  framing: Framing,
  /// `thin-pack`.
  thin: bool,
  /// `ofs-delta`.
  ofs_delta: bool,
  /// `include-tag`.
  include_tag: bool,
  /// `deepen-relative`.
  relative: bool,
}

/// A request, read whole.
#[derive(Debug, Default)]
struct Asked<'a> {
  /// The objects wanted, in the order sent.
  wants: Vec<ObjectId>,
  /// The capability lists the wants carried.
  lists: Vec<Capabilities<'a>>,
  /// The client's shallow commits.
  shallow: Vec<ObjectId>,
  /// `deepen <depth>`.
  depth: Option<u32>,
  /// `deepen-since <time>`.
  since: Option<u64>,
  /// Each `deepen-not`'s name, as sent.
  not: Vec<&'a [u8]>,
  /// `filter <spec>`, as sent.
  filter: Option<&'a [u8]>,
  /// The haves, in the order sent.
  haves: Vec<ObjectId>,
  /// Whether the haves ended with `done`.
  done: bool,
}

impl Asked<'_> {
  /// Whether the client chose the capability `name`, on any of its wants.
  fn chose(&self, name: &[u8]) -> bool {
    self.lists.iter().any(|list| list.get(name).is_some())
  }

  /// What the client chose.
  fn chosen(&self) -> Chosen {
    let multi_ack = if self.chose(MULTI_ACK_DETAILED) {
      MultiAck::Detailed
    } else if self.chose(MULTI_ACK) {
      MultiAck::Plain
    } else {
      MultiAck::None
    };
    let framing = if self.chose(SIDE_BAND_64K) {
      Framing::Band {
        data: MAX_BAND_DATA,
      }
    } else if self.chose(SIDE_BAND) {
      Framing::Band {
        data: SIDE_BAND_DATA,
      }
    } else {
      Framing::Raw
    };
    Chosen {
      multi_ack,
      no_done: self.chose(NO_DONE),
      framing,
      thin: self.chose(b"thin-pack"),
      ofs_delta: self.chose(b"ofs-delta"),
      include_tag: self.chose(b"include-tag"),
      relative: self.chose(b"deepen-relative"),
    }
  }
}

/// The advertisement of the repository `served` holds at protocol `version` (`send_ref`): its refs and capabilities
/// charged to `admission` before they are made.
pub(super) fn advertisement(
  served: &Served<'_>,
  version: Version,
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  refs::check_order(served.refs)?;
  let graph = StoreGraph::new(served.repository, served.store, served.refs);
  let advertised = advertised_refs(&graph, served, admission)?;
  let symref = [HEAD_TO, served.head].concat();
  let born = advertised.first().is_some_and(|first| first.name == HEAD);
  let format = served.repository.format().name().as_bytes();
  let list = capability_list(born.then_some(symref.as_slice()), format, admission)?;
  let advertisement = codec::Advertisement {
    version: match version {
      Version::V1 => codec::Version::V1,
      Version::V0 | Version::V2 => codec::Version::V0,
    },
    capabilities: Capabilities::listed(&list),
    refs: advertised,
    shallows: Vec::new(),
  };
  let (bytes, ()) = charged(admission, |out| {
    advertisement.write(out).map_err(Refusal::Write)
  })?;
  match graph.fault() {
    Some(fault) => Err(Refusal::Store(fault)),
    None => Ok(bytes),
  }
}

/// The refs advertised: `HEAD` where its branch has a commit, then each ref that names an object, in name order, an
/// annotated tag with what it peels to.
fn advertised_refs<'s>(
  graph: &StoreGraph<'_>,
  served: &Served<'s>,
  admission: &mut Admission,
) -> Result<Vec<AdvertisedRef<'s>>, Refusal> {
  let count = served.refs.len().saturating_add(1);
  admission
    .charge_items::<AdvertisedRef<'s>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut advertised = Vec::with_capacity(count);
  let head = refs::named(graph, served.refs, served.head).map(|node| (HEAD, node));
  let each = served
    .refs
    .iter()
    .filter_map(|named| graph.head(named.green).map(|node| (named.name, node)));
  for (name, node) in head.into_iter().chain(each) {
    advertised.push(AdvertisedRef {
      id: graph.id(node),
      name,
      peeled: refs::peeled(graph, node),
    });
  }
  Ok(advertised)
}

/// The capabilities advertised, in `send_ref`'s order: every flag, `symref` where `HEAD`'s branch has a commit,
/// `filter`, the repository's `format` and the agent.
fn capability_list<'a>(
  symref: Option<&'a [u8]>,
  format: &'a [u8],
  admission: &mut Admission,
) -> Result<Vec<Capability<'a>>, Refusal> {
  let count = FLAGS.len().saturating_add(VALUED.len()).saturating_add(1);
  admission
    .charge_items::<Capability<'a>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut list = Vec::with_capacity(count);
  list.extend(FLAGS.iter().map(|name| Capability { name, value: None }));
  list.extend(symref.map(|value| Capability {
    name: SYMREF,
    value: Some(value),
  }));
  list.extend([
    Capability {
      name: FILTER,
      value: None,
    },
    Capability {
      name: OBJECT_FORMAT,
      value: Some(format),
    },
    Capability {
      name: AGENT_NAME,
      value: Some(AGENT.as_bytes()),
    },
  ]);
  Ok(list)
}

/// Where a request's line stands: among its wants, or among its haves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
  /// Before the wants' flush.
  Wants,
  /// After it.
  Haves,
}

/// Walks the request `bytes` hold, its object names in `format`, handing each line to `each`: whether its haves ended
/// with `done`. Refused for bytes that are no pkt-lines, a line no client sends, a line out of its part, a request with
/// no flush after its wants, and bytes after its end.
fn walk<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  mut each: impl FnMut(ClientLine<'a>),
) -> Result<bool, Refusal> {
  let mut at = 0usize;
  let mut part = Part::Wants;
  loop {
    let rest = bytes.get(at..).unwrap_or_default();
    let read = pktline::read(rest, to_u64(at)).map_err(|_| Refusal::Request { at: to_u64(at) })?;
    let Some((packet, took)) = read else {
      return match part {
        Part::Wants => Err(Refusal::Request { at: to_u64(at) }),
        Part::Haves => Ok(false),
      };
    };
    let next = at.saturating_add(took);
    match (part, packet) {
      (Part::Wants, Packet::Flush) => part = Part::Haves,
      (Part::Haves, Packet::Flush) => return ended(bytes, next, false),
      (_, Packet::Data(data)) => {
        let line = ClientLine::read(data, format).map_err(|refusal| Refusal::Line {
          at: to_u64(at),
          refusal,
        })?;
        if part == Part::Haves && line == ClientLine::Done {
          return ended(bytes, next, true);
        }
        if !belongs(part, line) {
          return Err(Refusal::Request { at: to_u64(at) });
        }
        each(line);
      }
      (_, _) => return Err(Refusal::Request { at: to_u64(at) }),
    }
    at = next;
  }
}

/// Whether `line` may stand in `part`: a want, `shallow`, a deepening or `filter` before the flush, a have after it.
fn belongs(part: Part, line: ClientLine<'_>) -> bool {
  match line {
    ClientLine::Have(_) => part == Part::Haves,
    ClientLine::Want { .. }
    | ClientLine::Shallow(_)
    | ClientLine::Deepen(_)
    | ClientLine::DeepenSince(_)
    | ClientLine::DeepenNot(_)
    | ClientLine::Filter(_) => part == Part::Wants,
    ClientLine::Done | ClientLine::Error(_) => false,
  }
}

/// The request's end at `end`: `done` when `done` says so; refused for bytes after it.
fn ended(bytes: &[u8], end: usize, done: bool) -> Result<bool, Refusal> {
  if end < bytes.len() {
    return Err(Refusal::Request { at: to_u64(end) });
  }
  Ok(done)
}

/// What a request's walk counted, to charge its lists before they are made.
#[derive(Clone, Copy, Debug, Default)]
struct Counted {
  /// Wants.
  wants: usize,
  /// Capability lists.
  lists: usize,
  /// Shallow commits.
  shallow: usize,
  /// `deepen-not` names.
  not: usize,
  /// Haves.
  haves: usize,
}

/// Reads the request `bytes` hold whole, its object names in `format`: its lists charged to `admission` before they
/// are made, from a first walk that counts them.
fn read<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  admission: &mut Admission,
) -> Result<Asked<'a>, Refusal> {
  let mut counted = Counted::default();
  walk(bytes, format, |line| count(&mut counted, line))?;
  let ids = counted
    .wants
    .saturating_add(counted.shallow)
    .saturating_add(counted.haves);
  admission
    .charge_items::<ObjectId>(ids)
    .and_then(|()| admission.charge_items::<Capabilities<'a>>(counted.lists))
    .and_then(|()| admission.charge_items::<&[u8]>(counted.not))
    .map_err(Refusal::PastAdmission)?;
  let mut asked = Asked {
    wants: Vec::with_capacity(counted.wants),
    lists: Vec::with_capacity(counted.lists),
    shallow: Vec::with_capacity(counted.shallow),
    not: Vec::with_capacity(counted.not),
    haves: Vec::with_capacity(counted.haves),
    ..Asked::default()
  };
  let done = walk(bytes, format, |line| take(&mut asked, line))?;
  asked.done = done;
  Ok(asked)
}

/// Counts `line` into `counted`.
fn count(counted: &mut Counted, line: ClientLine<'_>) {
  let slot = match line {
    ClientLine::Want {
      capabilities: Some(_),
      ..
    } => {
      counted.lists = counted.lists.saturating_add(1);
      &mut counted.wants
    }
    ClientLine::Want { .. } => &mut counted.wants,
    ClientLine::Shallow(_) => &mut counted.shallow,
    ClientLine::DeepenNot(_) => &mut counted.not,
    ClientLine::Have(_) => &mut counted.haves,
    _ => return,
  };
  *slot = slot.saturating_add(1);
}

/// Takes `line` into `asked`; a later `deepen`, `deepen-since` or `filter` replaces an earlier one, as in git.
fn take<'a>(asked: &mut Asked<'a>, line: ClientLine<'a>) {
  match line {
    ClientLine::Want { id, capabilities } => {
      asked.wants.push(id);
      asked.lists.extend(capabilities);
    }
    ClientLine::Have(id) => asked.haves.push(id),
    ClientLine::Shallow(id) => asked.shallow.push(id),
    ClientLine::Deepen(depth) => asked.depth = Some(depth),
    ClientLine::DeepenSince(time) => asked.since = Some(time),
    ClientLine::DeepenNot(name) => asked.not.push(name),
    ClientLine::Filter(spec) => asked.filter = Some(spec),
    ClientLine::Done | ClientLine::Error(_) => {}
  }
}

/// Refused when a capability the client chose was not advertised, or its object format is not the repository's
/// `format`.
fn check_chosen(asked: &Asked<'_>, format: ObjectFormat) -> Result<(), Refusal> {
  let advertised = |name: &[u8]| FLAGS.contains(&name) || VALUED.contains(&name);
  for list in &asked.lists {
    if list.iter().any(|capability| !advertised(capability.name)) {
      return Err(Refusal::Capability);
    }
    match list.object_format() {
      Ok(client) if client == format => {}
      client => {
        return Err(Refusal::ObjectFormat {
          server: format,
          client: client.ok(),
        });
      }
    }
  }
  Ok(())
}

/// Answers the v0 or v1 request `bytes` hold whole (`upload_pack`, stateless), its pack, when one is due, from `packs`.
pub(super) fn answer<P: PackSource>(
  served: &Served<'_>,
  bytes: &[u8],
  packs: &mut P,
  admission: &mut Admission,
) -> Result<Answer<P::Pack>, Refusal> {
  refs::check_order(served.refs)?;
  let format = served.repository.format();
  let asked = read(bytes, format, admission)?;
  check_chosen(&asked, format)?;
  if asked.wants.is_empty() {
    return Ok(Answer::whole(Vec::new()));
  }
  let graph = StoreGraph::new(served.repository, served.store, served.refs);
  if let Some(id) = asked.wants.iter().find(|id| graph.node(id).is_none()) {
    return Err(Refusal::Negotiation(negotiation::Refusal::NotOurs {
      id: *id,
    }));
  }
  let not = asked
    .not
    .iter()
    .map(|rev| refs::deepen_not(&graph, served, rev))
    .collect::<Result<Vec<ObjectId>, Refusal>>()?;
  let filter = asked
    .filter
    .map(Filter::parse)
    .transpose()
    .map_err(Refusal::Filter)?;
  let chosen = asked.chosen();
  let request = negotiation::Request {
    wants: &asked.wants,
    haves: &asked.haves,
    done: true,
    wait_for_done: false,
    shallow: &asked.shallow,
    deepen: deepen(&asked, &not, chosen.relative),
    filter: filter.as_ref(),
    sparse: None,
    include_tag: chosen.include_tag,
    thin: chosen.thin,
  };
  let answered = answered(&graph, served, &request, &asked, chosen, packs, admission)?;
  match graph.fault() {
    Some(fault) => Err(Refusal::Store(fault)),
    None => Ok(answered),
  }
}

/// How the request deepens (`receive_needs`): by depth, from the client's shallow commits under `deepen-relative`; or
/// by a cut at a time or at what the `deepen-not` refs, resolved to `not`, reach.
fn deepen<'n>(asked: &Asked<'_>, not: &'n [ObjectId], relative: bool) -> Option<Deepen<'n>> {
  match asked.depth {
    Some(depth) => Some(Deepen::Depth { depth, relative }),
    None if asked.since.is_some() || !not.is_empty() => Some(Deepen::Cut {
      since: asked.since,
      not,
    }),
    None => None,
  }
}

/// The answer: the shallow-update where the request deepens, the acknowledgments, and the pack when one is due.
fn answered<P: PackSource>(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  request: &negotiation::Request<'_>,
  asked: &Asked<'_>,
  chosen: Chosen,
  packs: &mut P,
  admission: &mut Admission,
) -> Result<Answer<P::Pack>, Refusal> {
  let update = negotiation::shallow_update(graph, request, admission)
    .map_err(Refusal::Negotiation)?
    .map(|ShallowInfo { shallow, unshallow }| ShallowUpdate { shallow, unshallow });
  let (lines, due) = acknowledgments(graph, asked, chosen, admission)?;
  let (head, ()) = charged(admission, |out| {
    if let Some(update) = &update {
      update.write(out).map_err(Refusal::Write)?;
    }
    lines
      .iter()
      .try_for_each(|line| line.write(out).map_err(Refusal::Write))
  })?;
  if !due {
    return Ok(Answer::whole(head));
  }
  with_pack(graph, served, request, head, chosen, packs, admission)
}

/// The answer once its pack is due: `head`, then the pack, or, when the request's `sparse:oid` filter's blob was not
/// resolved or read, git's answer to a `pack-objects` that dies as it begins (A-640 part 4). The blob is read here, as
/// git's `pack-objects` reads it once the pack is due.
fn with_pack<P: PackSource>(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  request: &negotiation::Request<'_>,
  head: Vec<u8>,
  chosen: Chosen,
  packs: &mut P,
  admission: &mut Admission,
) -> Result<Answer<P::Pack>, Refusal> {
  let sparse = sparse::read(graph, served, request.filter, admission)?;
  let request = negotiation::Request {
    sparse: sparse.patterns.as_ref(),
    ..*request
  };
  let outcome = negotiation::negotiate(graph, &request, admission).map_err(Refusal::Negotiation)?;
  let negotiation::Answer::Pack(pack) = outcome.answer else {
    return Ok(Answer::whole(head).counted(outcome.counters));
  };
  if let Some(refusal) = sparse.dies {
    return Answer::died(head, refusal, chosen.framing.is_banded(), admission)
      .map(|answer| answer.counted(outcome.counters));
  }
  let plan = PackPlan {
    runs: &pack.runs,
    objects: &pack.objects,
    filter: request.filter,
    thin: chosen.thin,
    ofs_delta: chosen.ofs_delta,
  };
  let source = packs
    .begin(served, &plan, admission)
    .map_err(Refusal::Pack)?;
  Answer::streaming(head, chosen.framing, 0, source, admission)
    .map(|answer| answer.counted(outcome.counters))
}

/// What the haves have taught the server so far, have by have (`get_common_commits`, `do_got_oid`).
#[derive(Debug, Default)]
struct Learned {
  /// The commits git marks `THEY_HAVE`: each held commit have, and its parents.
  marked: BTreeSet<u64>,
  /// How many haves git counts (`have_obj`).
  counted: usize,
  /// Every held have so far, each once: the common objects `ready` reads.
  common: Vec<ObjectId>,
  /// The held haves so far, to keep `common` to one of each.
  seen: BTreeSet<ObjectId>,
  /// The last held have (`last_hex`).
  last: Option<ObjectId>,
  /// Whether a held have came since the last flush (`got_common`).
  got_common: bool,
  /// Whether a lacked have came (`got_other`).
  got_other: bool,
  /// Whether `ready` was said (`sent_ready`).
  sent_ready: bool,
  /// How many common objects `ready` was last asked about, and what it answered.
  asked: Option<(usize, bool)>,
}

impl Learned {
  /// Whether the server is ready with the held haves so far, asked again only when one was added since; what it builds
  /// is kept within `admission`'s room and let go after.
  fn ready(
    &mut self,
    graph: &StoreGraph<'_>,
    wants: &[ObjectId],
    admission: &Admission,
  ) -> Result<bool, Refusal> {
    if let Some((common, ready)) = self.asked
      && common == self.common.len()
    {
      return Ok(ready);
    }
    let mut room = *admission;
    let ready =
      negotiation::ready(graph, wants, &self.common, &mut room).map_err(Refusal::Negotiation)?;
    self.asked = Some((self.common.len(), ready));
    Ok(ready)
  }

  /// Learns the held have at `node` (`do_got_oid`): a commit counts unless already marked, and marks itself and its
  /// parents; any other object counts each time.
  fn hold(&mut self, graph: &StoreGraph<'_>, id: ObjectId, node: u64) {
    let counts = if graph.commit(node).is_some() {
      let new = self.marked.insert(node);
      self.marked.extend(graph.parents(node));
      new
    } else {
      true
    };
    if counts {
      self.counted = self.counted.saturating_add(1);
    }
    if self.seen.insert(id) {
      self.common.push(id);
    }
    self.got_common = true;
    self.last = Some(id);
  }
}

/// The acknowledgment lines a request is answered with, have by have and at its end (`get_common_commits`), and
/// whether the pack follows them; the list charged to `admission` before it is made.
fn acknowledgments(
  graph: &StoreGraph<'_>,
  asked: &Asked<'_>,
  chosen: Chosen,
  admission: &mut Admission,
) -> Result<(Vec<ServerLine<'static>>, bool), Refusal> {
  let count = asked.haves.len().saturating_add(CLOSING_LINES);
  admission
    .charge_items::<ServerLine<'static>>(count)
    .and_then(|()| admission.charge_items::<(u64, ObjectId)>(asked.haves.len()))
    .map_err(Refusal::PastAdmission)?;
  let mut lines = Vec::with_capacity(count);
  let mut learned = Learned::default();
  for have in &asked.haves {
    let line = acknowledge(graph, asked, chosen, &mut learned, *have, admission)?;
    lines.extend(line);
  }
  let due = if asked.done {
    lines.extend(at_done(&learned, chosen));
    true
  } else {
    at_flush(graph, asked, chosen, &mut learned, &mut lines, admission)?
  };
  Ok((lines, due))
}

/// The line a have is acknowledged with, if any.
fn acknowledge(
  graph: &StoreGraph<'_>,
  asked: &Asked<'_>,
  chosen: Chosen,
  learned: &mut Learned,
  have: ObjectId,
  admission: &Admission,
) -> Result<Option<ServerLine<'static>>, Refusal> {
  let Some(node) = graph.node(&have) else {
    learned.got_other = true;
    if chosen.multi_ack == MultiAck::None || !learned.ready(graph, &asked.wants, admission)? {
      return Ok(None);
    }
    let status = if chosen.multi_ack == MultiAck::Detailed {
      learned.sent_ready = true;
      AckStatus::Ready
    } else {
      AckStatus::Continue
    };
    return Ok(Some(ServerLine::Ack { id: have, status }));
  };
  learned.hold(graph, have, node);
  let status = match chosen.multi_ack {
    MultiAck::Detailed => AckStatus::Common,
    MultiAck::Plain => AckStatus::Continue,
    MultiAck::None if learned.counted == 1 => AckStatus::Plain,
    MultiAck::None => return Ok(None),
  };
  Ok(Some(ServerLine::Ack { id: have, status }))
}

/// The line at `done`: `NAK` where no have counts; `ACK <last held>` under either multi-ack where one does; and,
/// with neither, nothing, the first held have's `ACK` having said it.
fn at_done(learned: &Learned, chosen: Chosen) -> Option<ServerLine<'static>> {
  match learned.last {
    _ if learned.counted == 0 => Some(ServerLine::Nak),
    Some(id) if chosen.multi_ack != MultiAck::None => Some(ServerLine::Ack {
      id,
      status: AckStatus::Plain,
    }),
    _ => None,
  }
}

/// The lines at a flush, pushed onto `lines`: whether the pack follows them (`no-done` after `ready`).
fn at_flush(
  graph: &StoreGraph<'_>,
  asked: &Asked<'_>,
  chosen: Chosen,
  learned: &mut Learned,
  lines: &mut Vec<ServerLine<'static>>,
  admission: &Admission,
) -> Result<bool, Refusal> {
  let detailed = chosen.multi_ack == MultiAck::Detailed;
  if detailed
    && learned.got_common
    && !learned.got_other
    && learned.ready(graph, &asked.wants, admission)?
    && let Some(id) = learned.last
  {
    learned.sent_ready = true;
    lines.push(ServerLine::Ack {
      id,
      status: AckStatus::Ready,
    });
  }
  if learned.counted == 0 || chosen.multi_ack != MultiAck::None {
    lines.push(ServerLine::Nak);
  }
  match learned.last {
    Some(id) if chosen.no_done && learned.sent_ready => {
      lines.push(ServerLine::Ack {
        id,
        status: AckStatus::Plain,
      });
      Ok(true)
    }
    _ => Ok(false),
  }
}
