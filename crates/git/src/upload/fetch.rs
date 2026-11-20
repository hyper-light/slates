//! `fetch`, served (A-640 part 2; §4.17.2 "A fetch" items 2 and 3; `upload-pack.c` `upload_pack_v2`, `process_args`,
//! `parse_want`, `parse_want_ref`, `process_deepen_not`, `send_acks`, `process_haves_and_send_acks`,
//! `send_shallow_info` and `send_wanted_ref_info`): a request's arguments mapped onto A-615's negotiation over A-640 part
//! 1's graph, and the answer's sections written in the document's order, its pack begun.
//!
//! **The arguments.** Each `want` and `have` is passed on as named, each `want-ref` looked up among the refs (its id a
//! want, and its line in `wanted-refs`), each `deepen-not` resolved by git's patterns, and the `filter` parsed, a
//! `sparse:oid` filter's blob resolved and read (A-640 part 4, the `sparse` module). A `deepen` with `deepen-relative`
//! counts from the client's shallow commits; `deepen-relative` alone deepens nothing, as git's `send_shallow_list`
//! reads it. `thin-pack` and `ofs-delta` go to the pack's source, `sideband-all` multiplexes the whole answer, and
//! `include-tag` goes to the negotiation. `no-progress` asks for what is never sent (no progress is written yet), and
//! `packfile-uris` for what the store does not keep (no URI packs yet), so both are read and change nothing but the
//! answer to a pack git's `pack-objects` dies of, whose packfile section's header git leaves out under `sideband-all`
//! with `packfile-uris`. A later `deepen`, `deepen-since` or `filter` replaces an earlier one, as in git.
//!
//! **The sections** (gitprotocol-v2 "fetch" `output`). No want and no `wait-for-done`: nothing at all, as git answers.
//! Not ready: the acknowledgments (`NAK`, or an `ACK` for each common have) and the flush; the client asks again. Ready,
//! or `done`: the acknowledgments with `ready` and a delimiter (none after `done`), `shallow-info` where the client is
//! shallow or deepens (its `shallow` and `unshallow` lines without an LF, as git writes them), `wanted-refs` where
//! refs were wanted by name, then the packfile section's header; the pack follows as the answer is stepped, or, when
//! the request's `sparse:oid` filter's blob was not resolved or read, git's words for it (the `answer` module).

use super::answer::{Answer, Framing, Said};
use super::sparse;
use super::{PackPlan, PackSource, Refusal, Refused, Served, Sink, charged, refs};
use crate::Admission;
use crate::negotiation::{self, Acknowledgments, Deepen, Filter, Outcome, ShallowInfo};
use crate::object_name::{ObjectFormat, ObjectId};
use crate::store::StoreGraph;
use crate::v2::fetch::{Argument, Event, Response, Section, WantedRef};
use crate::v2::{Ending, Line};

/// What a `fetch` asks for that is a choice, not a list.
#[derive(Clone, Copy, Debug, Default)]
struct Flags {
  /// `done`.
  done: bool,
  /// `wait-for-done`.
  wait_for_done: bool,
  /// `include-tag`.
  include_tag: bool,
  /// `thin-pack`.
  thin: bool,
  /// `ofs-delta`.
  ofs_delta: bool,
  /// `deepen-relative`.
  relative: bool,
  /// `sideband-all`.
  multiplexed: bool,
  /// `packfile-uris`: the protocols the client takes a pack's URI by.
  uris: bool,
}

impl Flags {
  /// These choices with `flag`'s; an argument that is no choice changes nothing.
  fn with(self, flag: Argument<'_>) -> Flags {
    match flag {
      Argument::Done => Flags { done: true, ..self },
      Argument::WaitForDone => Flags {
        wait_for_done: true,
        ..self
      },
      Argument::IncludeTag => Flags {
        include_tag: true,
        ..self
      },
      Argument::ThinPack => Flags { thin: true, ..self },
      Argument::OfsDelta => Flags {
        ofs_delta: true,
        ..self
      },
      Argument::DeepenRelative => Flags {
        relative: true,
        ..self
      },
      Argument::SidebandAll => Flags {
        multiplexed: true,
        ..self
      },
      Argument::PackfileUris(_) => Flags { uris: true, ..self },
      _ => self,
    }
  }
}

/// A `fetch` request, read: the lists the negotiation reads, and the choices the answer is written with.
#[derive(Debug, Default)]
struct Asked<'a> {
  /// The objects wanted: each `want`'s, then each `want-ref`'s, in the order sent.
  wants: Vec<ObjectId>,
  /// The objects the client has.
  haves: Vec<ObjectId>,
  /// The client's shallow commits.
  shallow: Vec<ObjectId>,
  /// What each `deepen-not` names.
  not: Vec<ObjectId>,
  /// The refs wanted by name, and what each names, in the order sent.
  wanted: Vec<WantedRef<'a>>,
  /// `deepen <depth>`.
  depth: Option<u32>,
  /// `deepen-since <time>`.
  since: Option<u64>,
  /// `filter <spec>`, parsed.
  filter: Option<Filter>,
  /// The choices.
  flags: Flags,
}

impl Asked<'_> {
  /// How the request deepens a shallow history (`send_shallow_list`): by depth, or by a cut at a time or at what refs
  /// reach; `deepen-relative` alone deepens nothing.
  fn deepen(&self) -> Option<Deepen<'_>> {
    match self.depth {
      Some(depth) => Some(Deepen::Depth {
        depth,
        relative: self.flags.relative,
      }),
      None if self.since.is_some() || !self.not.is_empty() => Some(Deepen::Cut {
        since: self.since,
        not: &self.not,
      }),
      None => None,
    }
  }
}

/// Answers a `fetch` request's `arguments` (`upload_pack_v2`): its pack, when it carries one, from `packs`; refused on
/// band 3 when it asked for `sideband-all`.
pub(super) fn answer<P: PackSource>(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  arguments: &[Line<Argument<'_>>],
  packs: &mut P,
  admission: &mut Admission,
  turns: bool,
) -> Result<Answer<P::Pack>, Refused> {
  let multiplexed = arguments
    .iter()
    .any(|line| line.value == Argument::SidebandAll);
  let refused = |refusal: Refusal| Refused {
    refusal,
    multiplexed,
  };
  let asked = read(graph, served, arguments, admission).map_err(refused)?;
  let sparse = sparse::read(graph, served, asked.filter.as_ref(), admission).map_err(refused)?;
  let request = negotiation::Request {
    wants: &asked.wants,
    haves: &asked.haves,
    done: asked.flags.done,
    wait_for_done: asked.flags.wait_for_done,
    shallow: &asked.shallow,
    deepen: asked.deepen(),
    filter: asked.filter.as_ref(),
    sparse: sparse.patterns.as_ref(),
    include_tag: asked.flags.include_tag,
    thin: asked.flags.thin,
  };
  let answer = if turns {
    match negotiation::begin(graph, &request, admission)
      .map_err(|refusal| refused(Refusal::Negotiation(refusal)))?
    {
      negotiation::Begun::Answered(outcome) => {
        written(served, &asked, outcome, packs, sparse, admission)
      }
      negotiation::Begun::Selecting(negotiating) => {
        choosing(served, &asked, *negotiating, sparse, admission)
      }
    }
    .map_err(refused)?
  } else {
    let outcome = negotiation::negotiate(graph, &request, admission)
      .map_err(|refusal| refused(Refusal::Negotiation(refusal)))?;
    written(served, &asked, outcome, packs, sparse, admission).map_err(refused)?
  };
  match graph.fault() {
    Some(fault) => Err(refused(Refusal::Store(fault))),
    None => Ok(answer),
  }
}

/// Reads `arguments` (`process_args`): every list charged to `admission` before it is made, each `want-ref` and
/// `deepen-not` looked up among the refs, a `want-ref` sent twice refused.
fn read<'a>(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  arguments: &[Line<Argument<'a>>],
  admission: &mut Admission,
) -> Result<Asked<'a>, Refusal> {
  let mut asked = with_room(arguments, admission)?;
  for line in arguments {
    take(&mut asked, line.value, graph, served)?;
  }
  once_each(&asked.wanted, admission)?;
  Ok(asked)
}

/// An empty request whose lists have room for what `arguments` name, charged to `admission` first.
fn with_room<'a>(
  arguments: &[Line<Argument<'a>>],
  admission: &mut Admission,
) -> Result<Asked<'a>, Refusal> {
  let count =
    |kind: fn(&Argument<'_>) -> bool| arguments.iter().filter(|line| kind(&line.value)).count();
  let haves = count(|argument| matches!(argument, Argument::Have(_)));
  let shallow = count(|argument| matches!(argument, Argument::Shallow(_)));
  let not = count(|argument| matches!(argument, Argument::DeepenNot(_)));
  let wanted = count(|argument| matches!(argument, Argument::WantRef(_)));
  let wants = count(|argument| matches!(argument, Argument::Want(_))).saturating_add(wanted);
  let ids = wants
    .saturating_add(haves)
    .saturating_add(shallow)
    .saturating_add(not);
  admission
    .charge_items::<ObjectId>(ids)
    .and_then(|()| admission.charge_items::<WantedRef<'a>>(wanted))
    .map_err(Refusal::PastAdmission)?;
  Ok(Asked {
    wants: Vec::with_capacity(wants),
    haves: Vec::with_capacity(haves),
    shallow: Vec::with_capacity(shallow),
    not: Vec::with_capacity(not),
    wanted: Vec::with_capacity(wanted),
    ..Asked::default()
  })
}

/// Takes one argument into `asked`.
fn take<'a>(
  asked: &mut Asked<'a>,
  argument: Argument<'a>,
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
) -> Result<(), Refusal> {
  match argument {
    Argument::Want(id) => asked.wants.push(id),
    Argument::WantRef(name) => {
      let id = refs::wanted(graph, served, name)?;
      asked.wants.push(id);
      asked.wanted.push(WantedRef { id, name });
    }
    Argument::Have(id) => asked.haves.push(id),
    Argument::Shallow(id) => asked.shallow.push(id),
    Argument::Deepen(depth) => asked.depth = Some(depth),
    Argument::DeepenSince(time) => asked.since = Some(time),
    Argument::DeepenNot(rev) => asked.not.push(refs::deepen_not(graph, served, rev)?),
    Argument::Filter(spec) => asked.filter = Some(Filter::parse(spec).map_err(Refusal::Filter)?),
    flag => asked.flags = asked.flags.with(flag),
  }
  Ok(())
}

/// Refused when a ref is wanted by name twice ("It is a protocol error to send want-ref for the same ref more than
/// once"), the names sorted in a list charged to `admission` first.
fn once_each(wanted: &[WantedRef<'_>], admission: &mut Admission) -> Result<(), Refusal> {
  admission
    .charge_items::<&[u8]>(wanted.len())
    .map_err(Refusal::PastAdmission)?;
  let mut names: Vec<&[u8]> = wanted.iter().map(|wanted| wanted.name).collect();
  names.sort_unstable();
  match names.windows(2).find_map(|pair| match pair {
    [earlier, later] if earlier == later => Some(*later),
    _ => None,
  }) {
    Some(name) => Err(Refusal::WantRefTwice {
      name: name.to_vec(),
    }),
    None => Ok(()),
  }
}

/// Where the packfile section's header goes (`upload_pack_v2`, `relay_pack_data`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
  /// Before `pack-objects` begins, so before what it writes to its error stream.
  First,
  /// Once the pack begins, after what `pack-objects` wrote before it: under `sideband-all` with `packfile-uris`.
  AtThePack,
  /// Never: under `sideband-all` with `packfile-uris`, a pack that never begins.
  Never,
}

/// The answer to `outcome`: its sections encoded, what git's `pack-objects` wrote to its error stream as it read a
/// `sparse:oid` filter's name relayed on band 2 (A-640 part 4), and its pack begun from `packs`; or, when the filter's
/// blob dies unread, the pack answered as git answers it. git writes the packfile section's header before
/// `pack-objects` begins, but under `sideband-all` with `packfile-uris` (`upload_pack_v2`) at the pack's start, after
/// what `pack-objects` wrote before it, and never for a pack that never begins.
fn written<P: PackSource>(
  served: &Served<'_>,
  asked: &Asked<'_>,
  outcome: Outcome<u64, u64>,
  packs: &mut P,
  sparse: sparse::Read,
  admission: &mut Admission,
) -> Result<Answer<P::Pack>, Refusal> {
  let format = served.repository.format();
  let multiplexed = asked.flags.multiplexed;
  match outcome.answer {
    negotiation::Answer::Nothing => Ok(Answer::whole(Vec::new()).counted(outcome.counters)),
    negotiation::Answer::Acknowledgments(acknowledgments) => {
      let (bytes, _) = charged(admission, |out| {
        let mut writer = Writer::new(format, multiplexed, out);
        writer.acknowledgments(&acknowledgments)?;
        writer.event(&Event::Flush)?;
        Ok(writer.finish())
      })?;
      Ok(Answer::whole(bytes).counted(outcome.counters))
    }
    negotiation::Answer::Pack(pack) => {
      let at_the_pack = multiplexed && asked.flags.uris;
      if let Some(refusal) = sparse.dies {
        let header = if at_the_pack {
          Header::Never
        } else {
          Header::First
        };
        let (bytes, _) = charged(admission, |out| {
          before_pack(
            (pack.acknowledgments.as_ref(), pack.shallow_info.as_ref()),
            &asked.wanted,
            format,
            multiplexed,
            header,
            &Said::default(),
            out,
          )
        })?;
        return Answer::died(bytes, sparse.said, refusal, true, admission)
          .map(|answer| answer.counted(outcome.counters));
      }
      let plan = PackPlan {
        runs: &pack.runs,
        objects: &pack.objects,
        filter: asked.filter.as_ref(),
        thin: asked.flags.thin,
        ofs_delta: asked.flags.ofs_delta,
      };
      let source = packs
        .begin(served, &plan, admission)
        .map_err(Refusal::Pack)?;
      let header = if at_the_pack {
        Header::AtThePack
      } else {
        Header::First
      };
      let (bytes, (response, at)) = charged(admission, |out| {
        let said = &sparse.said;
        let sections = (pack.acknowledgments.as_ref(), pack.shallow_info.as_ref());
        before_pack(
          sections,
          &asked.wanted,
          format,
          multiplexed,
          header,
          said,
          out,
        )
      })?;
      Answer::streaming(bytes, Framing::V2(response), at, source, admission)
        .map(|answer| answer.counted(outcome.counters))
    }
  }
}

/// The answer to a fetch whose pack's objects are still being chosen (A-745): the sections before the pack encoded now,
/// as [`written`] encodes them for a pack (they rest on what the request bounds decided, never on the objects), and the
/// choice left to the answer's steps, whose last begins the pack. A `sparse:oid` filter whose blob dies unread is
/// answered now as [`written`] answers it, since no pack follows.
fn choosing<K>(
  served: &Served<'_>,
  asked: &Asked<'_>,
  negotiating: negotiation::Negotiating<u64, u64>,
  sparse: sparse::Read,
  admission: &mut Admission,
) -> Result<Answer<K>, Refusal> {
  let format = served.repository.format();
  let multiplexed = asked.flags.multiplexed;
  let at_the_pack = multiplexed && asked.flags.uris;
  let sections = (negotiating.acknowledgments(), negotiating.shallow_info());
  if let Some(refusal) = sparse.dies {
    let header = if at_the_pack {
      Header::Never
    } else {
      Header::First
    };
    let (bytes, _) = charged(admission, |out| {
      before_pack(
        sections,
        &asked.wanted,
        format,
        multiplexed,
        header,
        &Said::default(),
        out,
      )
    })?;
    return Answer::died(bytes, sparse.said, refusal, true, admission);
  }
  let header = if at_the_pack {
    Header::AtThePack
  } else {
    Header::First
  };
  let (bytes, (response, at)) = charged(admission, |out| {
    before_pack(
      sections,
      &asked.wanted,
      format,
      multiplexed,
      header,
      &sparse.said,
      out,
    )
  })?;
  Answer::choosing(
    bytes,
    super::answer::Choosing::new(
      negotiating,
      asked.filter.clone(),
      asked.flags.thin,
      asked.flags.ofs_delta,
      Framing::V2(response),
      at,
    ),
    admission,
  )
}

/// Writes the sections before a pack, then the packfile section's header and what `pack-objects` wrote to its error
/// stream before its pack (`said`, relayed on band 2 a read a packet), in the order `header` says: the response's
/// grammar after them, and how many bytes they took.
fn before_pack(
  sections: (Option<&Acknowledgments>, Option<&ShallowInfo>),
  wanted: &[WantedRef<'_>],
  format: ObjectFormat,
  multiplexed: bool,
  header: Header,
  said: &Said,
  out: &mut Sink<'_>,
) -> Result<(Response, u64), Refusal> {
  let mut writer = Writer::new(format, multiplexed, out);
  let (acknowledgments, shallow_info) = sections;
  if let Some(acknowledgments) = acknowledgments {
    writer.acknowledgments(acknowledgments)?;
    writer.event(&Event::Delim)?;
  }
  if let Some(shallow_info) = shallow_info {
    writer.shallow_info(shallow_info)?;
  }
  if !wanted.is_empty() {
    writer.wanted_refs(wanted)?;
  }
  if header == Header::First {
    writer.event(&Event::Header(Line::lf(Section::Packfile)))?;
  }
  for read in said.reads() {
    writer.event(&Event::Progress(read))?;
  }
  if header == Header::AtThePack {
    writer.event(&Event::Header(Line::lf(Section::Packfile)))?;
  }
  Ok(writer.finish())
}

/// Writes a response's events through its grammar ([`Response`]), counting the bytes.
struct Writer<'o, 's> {
  /// The grammar.
  response: Response,
  /// The bytes written so far.
  at: u64,
  /// Where they go.
  out: &'o mut Sink<'s>,
}

impl<'o, 's> Writer<'o, 's> {
  /// A writer of a response whose ids are in `format`, multiplexed throughout under `sideband-all`.
  fn new(format: ObjectFormat, multiplexed: bool, out: &'o mut Sink<'s>) -> Writer<'o, 's> {
    Writer {
      response: Response::new(format, multiplexed),
      at: 0,
      out,
    }
  }

  /// Writes `event` next.
  fn event(&mut self, event: &Event<'_>) -> Result<(), Refusal> {
    let wrote = self
      .response
      .write(event, self.at, self.out)
      .map_err(Refusal::Wire)?;
    self.at = self.at.saturating_add(wrote);
    Ok(())
  }

  /// The grammar where the writing stopped, and the bytes written.
  fn finish(self) -> (Response, u64) {
    (self.response, self.at)
  }

  /// The acknowledgments section (`send_acks`): `NAK`, or an `ACK` for each common have, then `ready` where ready.
  fn acknowledgments(&mut self, acknowledgments: &Acknowledgments) -> Result<(), Refusal> {
    self.event(&Event::Header(Line::lf(Section::Acknowledgments)))?;
    if acknowledgments.common.is_empty() {
      self.event(&Event::Nak(Ending::Lf))?;
    }
    for id in &acknowledgments.common {
      self.event(&Event::Ack(Line::lf(*id)))?;
    }
    if acknowledgments.ready {
      self.event(&Event::Ready(Ending::Lf))?;
    }
    Ok(())
  }

  /// The shallow-info section (`send_shallow_info`) and its delimiter: each `shallow`, then each `unshallow`, without an
  /// LF, as git writes them.
  fn shallow_info(&mut self, shallow_info: &ShallowInfo) -> Result<(), Refusal> {
    self.event(&Event::Header(Line::lf(Section::ShallowInfo)))?;
    for id in &shallow_info.shallow {
      self.event(&Event::Shallow(Line::bare(*id)))?;
    }
    for id in &shallow_info.unshallow {
      self.event(&Event::Unshallow(Line::bare(*id)))?;
    }
    self.event(&Event::Delim)
  }

  /// The wanted-refs section (`send_wanted_ref_info`) and its delimiter: each ref wanted by name and its id, in the order
  /// wanted.
  fn wanted_refs(&mut self, wanted: &[WantedRef<'_>]) -> Result<(), Refusal> {
    self.event(&Event::Header(Line::lf(Section::WantedRefs)))?;
    for wanted_ref in wanted {
      self.event(&Event::WantedRef(Line::lf(*wanted_ref)))?;
    }
    self.event(&Event::Delim)
  }
}
