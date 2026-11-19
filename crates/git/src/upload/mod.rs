//! upload-pack, the server's half of a fetch (A-640; §4.17.2 "A fetch", "The wire" and "Pieces (git)" piece 7). One
//! entry point, [`serve`], answers each exchange a client has with a repository A-614's store holds, and the [`Answer`]
//! it makes is written out a slice at a time ([`Answer::write`]), so a shard serves a clone of any size without holding
//! it and without holding its state across a wait.
//!
//! **Sources** (read 2026-11-19, git v2.50.1). The documents [B]: `gitprotocol-v2.adoc` ("Capability Advertisement",
//! "Command Request", "ls-refs", "fetch", "object-info"). The code [C]: `serve.c` (`protocol_v2_advertise_capabilities`,
//! `process_request`, `parse_command`, `receive_client_capability`), `ls-refs.c` (`ls_refs`, `send_ref`,
//! `send_possibly_unborn_head`), `upload-pack.c` (`upload_pack_advertise`, `upload_pack_v2`, `process_args`,
//! `parse_want`, `parse_want_ref`, `process_shallow`, `process_deepen_not`, `send_acks`, `process_haves_and_send_acks`,
//! `send_shallow_info`, `send_wanted_ref_info`, `create_pack_file`), `refs.c` (`expand_ref`, `ref_rev_parse_rules`) and
//! `pkt-line.c` (`packet_writer_error`).
//!
//! **One exchange a call.** [`Asked::Advertise`] is answered with the capability advertisement (§4.17.2 "The wire":
//! `agent`, `ls-refs=unborn`, `fetch=shallow wait-for-done filter ref-in-want sideband-all packfile-uris`,
//! `server-option`, `object-format`, `session-id` and `object-info`). [`Asked::Request`] is answered as
//! `git upload-pack --stateless-rpc` answers one `POST`, and as an SSH session answers each request it reads whole: a
//! request is held to the advertisement first, as `serve.c` holds it, so a command or a capability not advertised
//! (`bundle-uri` until the repository offers a bundle list, `promisor-remote`) and an object format other than the
//! repository's are refused; then `ls-refs`, `fetch` or `object-info` answers it. A flush alone ends a session, and is
//! answered with nothing ([`Answer::ends_session`]). Protocol v0 and v1 (A-640 part 3) are answered as git answers them
//! over smart HTTP: the ref advertisement, and each request whole and statelessly, its acknowledgments as
//! `get_common_commits` writes them and its pack on the band the client chose (the `v0` module).
//!
//! **One position of the store.** Everything but the pack is decided and encoded while [`serve`] runs, in one turn over
//! the repository: `ls-refs` reads the refs at one position (§4.17.2 "A fetch" item 1), and `fetch` maps its arguments
//! onto A-615's negotiation over A-640 part 1's graph and encodes its sections. The pack is written as the answer is
//! stepped, by a [`PackSource`] (A-639 writes it from the store's fragments), whose bytes the answer frames on band 1, a
//! packet at a time. What a pack names, objects by position and runs of versions, never changes once written, so a push
//! that lands between two steps leaves the pack whole.
//!
//! **Refusals.** A request the server refuses is answered as git's server answers one, with `ERR <words>`, or with its
//! words on band 3 when the request asked for `sideband-all` (`packet_writer_error`); git's own words are kept where a
//! client reads them (`not our ref`, `unknown ref`, `invalid command`). A pack that cannot be finished ends with git's
//! band-3 words for it (`create_pack_file`'s "aborting due to possible repository corruption on the remote side."). The
//! answer keeps the typed [`Refusal`] for the caller to count ([`Answer::refusal`]).
//!
//! **Stricter than git, by the document's words.** A `want-ref` sent twice is refused ("It is a protocol error to send
//! want-ref for the same ref more than once" [B: gitprotocol-v2 "fetch"]); git 2.50.1 answers it. And the sections
//! after the acknowledgments come in the document's order, `shallow-info` before `wanted-refs`, which git's own client
//! requires; git 2.50.1's server wrote them the other way round (A-582 part 2).
//!
//! **Bounds.** The request's [`Admission`] bounds everything [`serve`] keeps: the decode (A-582), the negotiation
//! (A-615), the lists the arguments map to, the bytes encoded before the pack and the two packet buffers a pack is
//! framed through, each charged before it is made. A step writes at most the room its caller gives it and asks the
//! pack's source for at most one packet's data at a time, so a step's work is bounded by its room and one packet.
//!
//! **`sparse:oid`** (A-640 part 4, every protocol version): the filter's blob is resolved and read ([`sparse`]) and the
//! negotiation walks by its patterns. A blob that cannot be resolved or read is answered as git answers it when the
//! answer carries a pack: its `pack-objects` dies as it begins, so the client meets git's words on band 2 and git's
//! band-3 words, or, under protocol v0 with no band, the pack missing.
//!
//! **Owed** (GAPS): a stateful protocol v0 session, an SSH client's (git speaks v2 over SSH since 2.26);
//! `packfile-uris` answered from the store's URI packs and `bundle-uri` from its bundle list, whose choice A-624 built
//! and which the store keeps neither of yet; progress on band 2; hidden refs, which §4.17.2's "Access" makes policy.

use slates_vcs::ContentStore;

use crate::http::Version;
use crate::negotiation::{self, Filter, Run};
use crate::object_name::{CollisionAttack, ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::store::{GraphFault, NamedRef, Repository, StoreGraph, StoreRefused};
use crate::v2::request::{self, Arguments, Command, CommandRequest};
use crate::v2::{self, Line, object_info};
use crate::{Admission, PastAdmission};

mod advertise;
mod answer;
mod fetch;
mod fragments;
mod refs;
mod revision;
mod sparse;
mod v0;

pub use answer::{Answer, Written};
pub use fragments::{Fragments, Planned, Streamed};

/// Format: the name this server gives itself in `agent`, under every protocol version, its crate's version after it;
/// receive-pack advertises the same (A-654).
pub const AGENT: &str = concat!("slates/", env!("CARGO_PKG_VERSION"));

/// Format: git's band-3 words for a pack that could not be finished (`upload-pack.c` `create_pack_file`'s
/// `abort_msg`).
const ABORT: &[u8] = b"aborting due to possible repository corruption on the remote side.";

/// A repository as upload-pack serves it. It is lent afresh for each step of an answer, so a shard's state is never held
/// across a wait.
#[derive(Clone, Copy)]
pub struct Served<'r> {
  /// The repository: its index, versions and greens (A-614).
  pub repository: &'r Repository,
  /// The content its objects are kept in.
  pub store: &'r ContentStore,
  /// The refs a client may see, each naming its green, in name order and each once: `ls-refs` answers in this order,
  /// and a `want-ref` or a `deepen-not` is looked up among them.
  pub refs: &'r [NamedRef<'r>],
  /// The branch `HEAD` names, the repository's default branch (§4.17.2 "The repository in git's terms" item 6): a ref's
  /// full name, born or not.
  pub head: &'r [u8],
}

/// What a client asks of upload-pack in one exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked<'b> {
  /// The advertisement: `GET <repo>/info/refs?service=git-upload-pack`, or an SSH session's first answer.
  Advertise,
  /// One command request, whole: a `POST <repo>/git-upload-pack`'s body, or what [`request::whole_len`] measured of an
  /// SSH session's input.
  Request(&'b [u8]),
}

/// What a fetch's pack carries, as the negotiation chose it and the client asked for it.
#[derive(Clone, Copy, Debug)]
pub struct PackPlan<'p> {
  /// Runs of a green's versions the pack carries whole, from their fragments, in the negotiation's order.
  pub runs: &'p [Run<u64>],
  /// Objects the pack carries one by one, by position, each once, in position order.
  pub objects: &'p [u64],
  /// The filter the runs' objects are held to, one that decides each object by its own row
  /// ([`Filter::is_per_object`]); the single objects were chosen under it already.
  pub filter: Option<&'p Filter>,
  /// `thin-pack`: a delta may name a base the client holds.
  pub thin: bool,
  /// `ofs-delta`: the client reads a delta whose base is named by its offset.
  pub ofs_delta: bool,
}

/// Why a pack's source could not go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackRefused {
  /// The store could not read an object the pack carries.
  Store(StoreRefused),
  /// What the pack would keep is past the request's admission.
  PastAdmission(PastAdmission),
  /// The pack's trailer met a SHA-1 collision attack.
  Collision(CollisionAttack),
}

/// Where a fetch's pack comes from: A-639 writes it from the store's fragments and kept compressed objects
/// ([`Fragments`]). A source may write many packs at once; each keeps its own state in [`PackSource::Pack`].
pub trait PackSource {
  /// One pack being written: what the source keeps between steps. It borrows nothing of the store, which is lent again
  /// for each step.
  type Pack;

  /// Begins the pack `plan` names, keeping within `admission` what it must hold until the pack's end.
  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<Self::Pack, PackRefused>;

  /// Writes `pack`'s next bytes to `out`, at most `room` of them, doing at most a turn's work toward them: whether any
  /// are left after them. A call may work and write nothing yet; the answer then asks again at its next step, so each
  /// call must bring the pack nearer its end. What a turn's work holds only for a while (A-653 continued (4): a delta
  /// search over an object past a window) is charged to the request's `admission` as it begins and given back as it
  /// ends, within the same request.
  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut Self::Pack,
    admission: &mut Admission,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused>;
}

/// Why upload-pack refused an exchange. Each is answered to the client in words, git's where it has them
/// ([`Refusal::words`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The request is not one protocol v2 reads (A-582's refusal), or an answer could not be written.
  Wire(v2::Refusal),
  /// A protocol v0 or v1 request the pack protocol does not read: bytes that are no pkt-lines, a line out of its
  /// place, a request that ends before its wants' flush, or bytes after its end.
  Request {
    /// Where the packet at fault begins, or where the bytes ended.
    at: u64,
  },
  /// A protocol v0 or v1 line that is no client line (A-604's refusal).
  Line {
    /// Where its packet begins.
    at: u64,
    /// Why.
    refusal: crate::v0::LineRefusal,
  },
  /// A protocol v0 or v1 answer's line the protocol cannot carry (A-604's refusal).
  Write(crate::v0::WriteRefusal),
  /// A command the server does not advertise (`parse_command`: "invalid command").
  Command(Command),
  /// A capability the server does not advertise (`receive_client_capability`: "unknown capability").
  Capability,
  /// The request's object format is not the repository's, or one git does not know (`process_request`: "mismatched
  /// object format").
  ObjectFormat {
    /// The repository's.
    server: ObjectFormat,
    /// The request's, `None` for a name git does not know.
    client: Option<ObjectFormat>,
  },
  /// A `want-ref` naming no ref the client may see, or one with no commit (`parse_want_ref`: "unknown ref").
  UnknownRef {
    /// The name, as the client sent it.
    name: Vec<u8>,
  },
  /// A `want-ref` sent twice.
  WantRefTwice {
    /// The name.
    name: Vec<u8>,
  },
  /// A `deepen-not` that names no ref by any of git's patterns (`process_deepen_not`: "deepen-not is not a ref").
  DeepenNotRef {
    /// What the client sent.
    rev: Vec<u8>,
  },
  /// A `deepen-not` that names two refs or more (`process_deepen_not`: "ambiguous deepen-not").
  DeepenNotAmbiguous {
    /// What the client sent.
    rev: Vec<u8>,
  },
  /// A `filter` spec that is not one git reads.
  Filter(negotiation::filter::Refusal),
  /// A `sparse:oid` filter's `<blob-ish>` that names nothing: a ref or a path the repository does not hold, an object
  /// it holds no tree of, or an index path, which a served repository holds none of (A-640 part 4; git's
  /// `filter_sparse_oid__init`: "unable to access sparse blob in '%s'").
  SparseBlob {
    /// The `<blob-ish>`, as the client sent it.
    name: Vec<u8>,
  },
  /// A `sparse:oid` filter's `<blob-ish>` in a form of git's revision syntax this server does not resolve yet: a parent
  /// or an ancestor (`~`, `^`), a peel or a reflog (`^{...}`, `@{...}`), or a search of commit messages (`:/`).
  /// Answered with git's words for a name that names nothing, and why.
  SparseForm {
    /// The `<blob-ish>`, as the client sent it.
    name: Vec<u8>,
  },
  /// A `sparse:oid` filter's `<blob-ish>` whose path is relative (`./`, `../`), which needs a work tree a served
  /// repository has none of (`resolve_relative_path`: "relative path syntax can't be used outside working tree").
  SparseRelative,
  /// A `sparse:oid` filter's `<blob-ish>` naming an object that is no blob the store holds ("unable to parse sparse
  /// filter data in %s").
  SparseData {
    /// What it names.
    id: ObjectId,
  },
  /// A `sparse:oid` filter's blob past git's limit, its final newline counted (`add_patterns_from_blob_to_list`: git
  /// warns "ignoring excessively large pattern blob: %s", then refuses as for [`Refusal::SparseData`]).
  SparseTooLarge {
    /// The blob.
    id: ObjectId,
  },
  /// The negotiation refused (A-615).
  Negotiation(negotiation::Refusal),
  /// A read the store could not make while the answer was made (A-640 part 1).
  Store(GraphFault),
  /// The pack's source refused.
  Pack(PackRefused),
  /// What the answer would keep is past the request's admission.
  PastAdmission(PastAdmission),
  /// The served refs are not in name order, each once, as [`Served::refs`] must be: where the first out of place is.
  Refs {
    /// Its index.
    at: usize,
  },
}

impl Refusal {
  /// What the client is told, in git's words where it has them.
  pub fn words(&self) -> Vec<u8> {
    match self {
      Refusal::Wire(_) => said(&[b"upload-pack: the request is not one protocol v2 reads"]),
      Refusal::Request { .. } | Refusal::Line { .. } => {
        said(&[b"upload-pack: the request is not one the pack protocol reads"])
      }
      Refusal::Write(_) => said(&[b"upload-pack: an answer's line could not be written"]),
      Refusal::Command(command) => said(&[b"invalid command '", command.name(), b"'"]),
      Refusal::Capability => said(&[b"upload-pack: unknown capability"]),
      Refusal::ObjectFormat { server, client } => said(&[
        b"mismatched object format: server ",
        server.name().as_bytes(),
        b"; client ",
        client.map_or(b"unknown".as_slice(), |format| format.name().as_bytes()),
      ]),
      Refusal::UnknownRef { name } => said(&[b"unknown ref ", name]),
      Refusal::WantRefTwice { name } => said(&[b"upload-pack: want-ref sent twice: ", name]),
      Refusal::DeepenNotRef { rev } => said(&[
        b"git upload-pack: deepen-not is not a ref: deepen-not ",
        rev,
      ]),
      Refusal::DeepenNotAmbiguous { rev } => {
        said(&[b"git upload-pack: ambiguous deepen-not: deepen-not ", rev])
      }
      Refusal::Filter(_) => said(&[b"upload-pack: invalid filter-spec"]),
      Refusal::SparseBlob { name } => said(&[SPARSE_BLOB, name, b"'"]),
      Refusal::SparseForm { name } => said(&[SPARSE_BLOB, name, b"': ", SPARSE_FORMS]),
      Refusal::SparseRelative => {
        said(&[b"relative path syntax can't be used outside working tree"])
      }
      Refusal::SparseData { id } | Refusal::SparseTooLarge { id } => {
        said(&[b"unable to parse sparse filter data in ", &hex(id)])
      }
      Refusal::Negotiation(refusal) => negotiation_words(refusal),
      Refusal::Store(_) | Refusal::Pack(_) => said(&[ABORT]),
      Refusal::PastAdmission(_) => said(&[PAST_ADMISSION]),
      Refusal::Refs { .. } => said(&[b"upload-pack: the server's refs are out of order"]),
    }
  }

  /// What git's `pack-objects` warns of before it dies of this refusal, in git's words: a `sparse:oid` filter's blob
  /// past git's limit (`add_patterns_from_blob_to_list`); `None` for every other refusal.
  pub(super) fn warning(&self) -> Option<Vec<u8>> {
    if let Refusal::SparseTooLarge { id } = self {
      Some(said(&[
        b"ignoring excessively large pattern blob: ",
        &hex(id),
      ]))
    } else {
      None
    }
  }
}

/// Format: git's words for a `sparse:oid` filter's `<blob-ish>` that names nothing, before the name and its closing
/// quote (`list-objects-filter.c` `filter_sparse_oid__init`).
const SPARSE_BLOB: &[u8] = b"unable to access sparse blob in '";

/// Format: why a `<blob-ish>` of a form this server does not resolve is answered as naming nothing: what it resolves.
const SPARSE_FORMS: &[u8] = b"this server resolves an object's full or abbreviated name, git describe's name or a \
  ref, alone or with :<path>";

/// Format: the words for a request past its admission.
const PAST_ADMISSION: &[u8] = b"upload-pack: the request needs more room than the server admits";

/// The words for a negotiation's refusal, git's where it has them (`parse_want`, `process_shallow`,
/// `deepen_by_rev_list`, `process_deepen`).
fn negotiation_words(refusal: &negotiation::Refusal) -> Vec<u8> {
  match refusal {
    negotiation::Refusal::NotOurs { id } => said(&[b"upload-pack: not our ref ", &hex(id)]),
    negotiation::Refusal::ShallowNotCommit { id } => said(&[b"invalid shallow object ", &hex(id)]),
    negotiation::Refusal::DeepenNotHeld { id } => said(&[
      b"upload-pack: deepen-not names an object not held: ",
      &hex(id),
    ]),
    negotiation::Refusal::NothingSelected => said(&[b"no commits selected for shallow requests"]),
    negotiation::Refusal::ZeroDepth => said(&[b"Invalid deepen: 0"]),
    negotiation::Refusal::Filter(_) => said(&[b"upload-pack: filter not served"]),
    negotiation::Refusal::PastAdmission(_) => said(&[PAST_ADMISSION]),
  }
}

/// The words `parts` make, in order.
fn said(parts: &[&[u8]]) -> Vec<u8> {
  parts.concat()
}

/// `id` in lower-case hexadecimal.
fn hex(id: &ObjectId) -> Vec<u8> {
  let mut bytes = Vec::with_capacity(id.format().hex_len());
  id.write_hex(&mut bytes);
  bytes
}

/// A refusal, and whether its answer is multiplexed (`sideband-all`), which puts its words on band 3.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Refused {
  /// Why.
  refusal: Refusal,
  /// Whether the request asked for `sideband-all`.
  multiplexed: bool,
}

impl Refused {
  /// `refusal`, answered outside a multiplexed response.
  fn plain(refusal: Refusal) -> Refused {
    Refused {
      refusal,
      multiplexed: false,
    }
  }
}

/// Answers one exchange, A-640's entry point: what `asked` asks of the repository `served` holds, at protocol `version`,
/// with the pack it carries (if any) from `packs`, everything it keeps until its end within `admission`. A refused
/// exchange is answered too, as git's server answers one, and [`Answer::refusal`] says why.
pub fn serve<P: PackSource>(
  served: &Served<'_>,
  version: Version,
  asked: Asked<'_>,
  packs: &mut P,
  admission: &mut Admission,
) -> Answer<P::Pack> {
  let made = match (version, asked) {
    (Version::V2, Asked::Advertise) => advertise::answer(served.repository.format(), admission)
      .map(Answer::whole)
      .map_err(Refused::plain),
    (Version::V2, Asked::Request(bytes)) => request(served, bytes, packs, admission),
    (Version::V0 | Version::V1, Asked::Advertise) => v0::advertisement(served, version, admission)
      .map(Answer::whole)
      .map_err(Refused::plain),
    (Version::V0 | Version::V1, Asked::Request(bytes)) => {
      v0::answer(served, bytes, packs, admission).map_err(Refused::plain)
    }
  };
  made.unwrap_or_else(Answer::refused)
}

/// Answers the v2 command request `bytes` hold whole.
fn request<P: PackSource>(
  served: &Served<'_>,
  bytes: &[u8],
  packs: &mut P,
  admission: &mut Admission,
) -> Result<Answer<P::Pack>, Refused> {
  refs::check_order(served.refs).map_err(Refused::plain)?;
  let command = match request::decode(bytes, admission) {
    Ok(request::Request::End) => return Ok(Answer::end()),
    Ok(request::Request::Command(command)) => command,
    Err(refusal) => return Err(Refused::plain(Refusal::Wire(refusal))),
  };
  advertise::check(&command, served.repository.format()).map_err(Refused::plain)?;
  let graph = StoreGraph::new(served.repository, served.store, served.refs);
  command_answer(served, &graph, &command, packs, admission)
}

/// Answers `command`, held to the advertisement already.
fn command_answer<P: PackSource>(
  served: &Served<'_>,
  graph: &StoreGraph<'_>,
  command: &CommandRequest<'_>,
  packs: &mut P,
  admission: &mut Admission,
) -> Result<Answer<P::Pack>, Refused> {
  match &command.arguments {
    Arguments::LsRefs(arguments) => refs::ls_refs(graph, served, arguments, admission)
      .map(Answer::whole)
      .map_err(Refused::plain),
    Arguments::Fetch(arguments) => fetch::answer(graph, served, arguments, packs, admission),
    Arguments::ObjectInfo(arguments) => object_info_answer(served, arguments, admission)
      .map(Answer::whole)
      .map_err(Refused::plain),
    Arguments::BundleUri => Err(Refused::plain(Refusal::Command(Command::BundleUri))),
  }
}

/// The `object-info` answer to `arguments`, each size from the store's rows (A-624's `Stored`).
fn object_info_answer(
  served: &Served<'_>,
  arguments: &[Line<object_info::Argument>],
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let sizes = object_info::Stored {
    repository: served.repository,
    content: served.store,
  };
  let format = served.repository.format();
  charged(admission, |out| {
    object_info::answer(arguments, format, &sizes, out).map_err(Refusal::Wire)
  })
  .map(|(bytes, ())| bytes)
}

/// Where an answer's bytes go while it is made: a count first, then the bytes, so they are charged before they are made.
enum Sink<'v> {
  /// Counting.
  Measure(Measure),
  /// Writing.
  Bytes(&'v mut Vec<u8>),
}

impl Out for Sink<'_> {
  fn put(&mut self, bytes: &[u8]) {
    match self {
      Sink::Measure(measure) => measure.put(bytes),
      Sink::Bytes(written) => written.put(bytes),
    }
  }
}

/// The bytes `write` writes, charged to `admission` before they are made: written once to count them, then again into
/// a buffer of that size. Answers them, and what `write` answered the second time.
fn charged<T>(
  admission: &mut Admission,
  mut write: impl FnMut(&mut Sink<'_>) -> Result<T, Refusal>,
) -> Result<(Vec<u8>, T), Refusal> {
  let mut counting = Sink::Measure(Measure::default());
  write(&mut counting)?;
  let len = match counting {
    Sink::Measure(Measure(len)) => len,
    Sink::Bytes(_) => 0,
  };
  admission.charge(len).map_err(Refusal::PastAdmission)?;
  let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
  let answered = write(&mut Sink::Bytes(&mut bytes))?;
  Ok((bytes, answered))
}

#[cfg(test)]
mod tests;
