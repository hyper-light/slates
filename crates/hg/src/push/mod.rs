//! A push's `unbundle`, part 1 (A-641; §4.17.3 "Push" steps 1, 4 and 7, piece 5): what a client's bundle2 asks, read
//! part by part as 7.2's part handlers read it ([`read`]); the race checks it carries, judged against the served view
//! the push read ([`check`]); and the reply 7.2's server writes for each outcome ([`reply`]). The works a push makes of
//! its changesets (§4.17.1 "A push is a work"), their landing on each green's own log at the positions the push read
//! (A-642), and the policy and the hooks at their points are the piece's next parts.
//!
//! **Sources** (read 2026-11-19 in the wheel of Mercurial 7.2 that uv installs, CPython 3.12, on the Mac Studio; each
//! file's SHA-256 in `docs/wip/research/mercurial-server.md` §20) \[C\]: `bundle2.py` (`_gethandler`, `_processpart`,
//! `processbundle`), `bundle2_part_handlers.py` (each handler, the parameters it names and the race checks' words),
//! `wireprotov1server.py` (`unbundle`: the reply to each refusal), `exchange.py` (`check_heads`, `unbundle`) and
//! `changegroup.py` (`apply`: the change in heads a reply counts).
//!
//! **A part and its handler.** 7.2 routes a part by its type in lower case to the handler registered for it. A
//! mandatory part (an upper-case letter in its type) that no handler takes is refused as unsupported content naming
//! its type in lower case, and so is a mandatory parameter its handler does not name, the unknown names sorted
//! (`BundleUnknownFeatureError`, answered `error:unsupportedcontent`); an advisory part or parameter nobody knows is
//! passed over. [`read`] takes the parts a push carries: `replycaps`, the four checks, `changegroup`, `phase-heads`,
//! `bookmarks`, `pushkey`, `obsmarkers`, `pushvars` and `output`, each payload decoded by piece 1's codecs (A-514,
//! A-545) and kept in stream order, since 7.2 handles the parts, and answers them, in that order.
//!
//! **Stricter than 7.2, each on purpose.** A part 7.2 handles only in a pull or in a reply (`listkeys`, `hgtagsfnodes`,
//! `cache:rev-branch-cache`, `stream2`, `remote-changegroup`, the `reply:` and `error:` parts) has no handler in a push
//! here, so a mandatory one is refused as unsupported and an advisory one passed over; 7.2 would act on it, and no
//! stock client sends one in a push. A changegroup part is taken once, and in versions `01` to `03` only (§4.17.3 "The
//! wire surface"). A payload its codec refuses, a number parameter that is no number, and a `targetphase` other than
//! public or draft are refused by kind, where 7.2's handlers fail with an exception and its server with them. A
//! mandatory part inside another (an interruption) is refused as unsupported: 7.2 runs it outside the push's
//! operation, and no stock client interrupts a push.
//!
//! **Bounds (§4.9).** Everything kept is charged to the request's [`Admission`] before it is made: the list of parts,
//! each decoded vector, each name and value copied, and a payload that arrived in several chunks, joined. A payload in
//! one chunk is decoded where it lies.
//!
//! **Golden vectors.** `fixtures/push/` holds what stock 7.2 sent and answered over SSH (made by
//! `make-push-fixtures.sh`, its command in its header, checked by its `SHA256SUMS`): a bookmark moved, a draft
//! published, a plain push, three races made by sending a recorded session again after another client moved a head, a
//! bookmark or a phase, a new head refused by 7.2's own `reject_new_heads`, and an unknown mandatory part and parameter.
//! Each request is read as 7.2's listing shows it, each race found in 7.2's words, and each reply written as 7.2 wrote
//! it.

pub mod check;
pub mod reply;

use std::borrow::Cow;

use crate::bundle2::{Bundle2, Part};
use crate::changegroup::Version;
use crate::phases::{Phase, PhaseHead};
use crate::read::to_u64;
use crate::{Admission, Node, PastAdmission, bookmarks, heads, phases, pushkey};

/// Format: the parameters 7.2's `changegroup` handler names (`@parthandler(b'changegroup', ...)`).
const CHANGEGROUP_PARAMS: [&[u8]; 6] = [
  b"version",
  b"nbchanges",
  b"exp-sidedata",
  b"exp-wanted-sidedata",
  b"treemanifest",
  b"targetphase",
];

/// Format: the parameters 7.2's `pushkey` handler names, the four [`pushkey::PARAMETERS`].
const PUSHKEY_PARAMS: [&[u8]; 4] = pushkey::PARAMETERS;

/// Format: what 7.2 puts before a `pushvars` key, which it upper-cases (`bundle2getvars`).
const USERVAR: &[u8] = b"USERVAR_";

/// Which handler takes a part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handler {
  /// `replycaps`.
  ReplyCaps,
  /// `check:heads`.
  CheckHeads,
  /// `check:updated-heads`.
  CheckUpdatedHeads,
  /// `check:phases`.
  CheckPhases,
  /// `check:bookmarks`.
  CheckBookmarks,
  /// `changegroup`.
  Changegroup,
  /// `phase-heads`.
  PhaseHeads,
  /// `bookmarks`.
  Bookmarks,
  /// `pushkey`.
  Pushkey,
  /// `obsmarkers`.
  Obsmarkers,
  /// `pushvars`.
  Pushvars,
  /// `output`.
  Output,
}

/// Format: the part types a push's handlers take, as 7.2 registers them (lower case), with the parameters each handler
/// names (`@parthandler`, `bundle2_part_handlers.py`); every other type has no handler in a push (module docs).
const HANDLERS: [(&[u8], &[&[u8]], Handler); 12] = [
  (b"replycaps", &[], Handler::ReplyCaps),
  (b"check:heads", &[], Handler::CheckHeads),
  (b"check:updated-heads", &[], Handler::CheckUpdatedHeads),
  (b"check:phases", &[], Handler::CheckPhases),
  (b"check:bookmarks", &[], Handler::CheckBookmarks),
  (b"changegroup", &CHANGEGROUP_PARAMS, Handler::Changegroup),
  (b"phase-heads", &[], Handler::PhaseHeads),
  (b"bookmarks", &[], Handler::Bookmarks),
  (b"pushkey", &PUSHKEY_PARAMS, Handler::Pushkey),
  (b"obsmarkers", &[], Handler::Obsmarkers),
  (b"pushvars", &[], Handler::Pushvars),
  (b"output", &[], Handler::Output),
];

/// A bookmark as a push names it: its name, and its node or none for "no bookmark" (a deletion, or a check that it is
/// missing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookmarkAt {
  /// The name, as sent.
  pub name: Vec<u8>,
  /// The node, or none.
  pub node: Option<Node>,
}

/// A `changegroup` part: its version, its parameters and its payload, which the push's verification decodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangegroupPart {
  /// The changegroup's version (`version`, `01` when absent).
  pub version: Version,
  /// The changesets the client says it holds (`nbchanges`), which 7.2 reads only for its progress.
  pub changesets: Option<u64>,
  /// The phase the client asks its changesets to take (`targetphase`): public or draft.
  pub target_phase: Option<Phase>,
  /// Whether the client says it carries tree manifests (`treemanifest`).
  pub tree_manifests: bool,
  /// The payload: the changegroup's bytes.
  pub payload: Vec<u8>,
}

/// A `pushkey` part's request, as sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushkeyAsk {
  /// The namespace.
  pub namespace: Vec<u8>,
  /// The key.
  pub key: Vec<u8>,
  /// The value the client saw.
  pub old: Vec<u8>,
  /// The value it asks for.
  pub new: Vec<u8>,
}

/// What a part a handler took asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asks {
  /// `replycaps`: a reply is wanted; the client's bundle2 capability blob, as sent.
  ReplyCaps(Vec<u8>),
  /// `check:heads`: the repository's heads are exactly these.
  CheckHeads(Vec<Node>),
  /// `check:updated-heads`: each of these is still the head of a branch.
  CheckUpdatedHeads(Vec<Node>),
  /// `check:phases`: each node is still in its phase.
  CheckPhases(Vec<PhaseHead>),
  /// `check:bookmarks`: each bookmark still names its node, or is still missing.
  CheckBookmarks(Vec<BookmarkAt>),
  /// `changegroup`: the changesets, manifests and file revisions.
  Changegroup(ChangegroupPart),
  /// `phase-heads`: each head and its ancestors move to its phase.
  PhaseHeads(Vec<PhaseHead>),
  /// `bookmarks`: each bookmark set to its node, or deleted.
  Bookmarks(Vec<BookmarkAt>),
  /// `pushkey`: one key moved from the value the client saw to another.
  Pushkey(PushkeyAsk),
  /// `obsmarkers`: the markers' bytes, as sent (decoded with the markers, piece 7).
  Obsmarkers(Vec<u8>),
  /// `pushvars`: each variable as the push's hooks see it (`USERVAR_` and the key upper-cased) and its value.
  Pushvars(Vec<(Vec<u8>, Vec<u8>)>),
  /// `output`: words a client printed into its bundle, which 7.2 prints on the server; a push keeps nothing of them.
  Output,
}

/// A part a push's handler took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Taken {
  /// The part's id, which a reply names (`in-reply-to`).
  pub id: u32,
  /// Whether the part was mandatory: a mandatory `pushkey` that does not move refuses the push.
  pub mandatory: bool,
  /// What it asks.
  pub asks: Asks,
}

/// What one `unbundle` asks: the parts its handlers took, in stream order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unbundle {
  /// The parts, in stream order.
  pub parts: Vec<Taken>,
}

impl Unbundle {
  /// The client's bundle2 capabilities if it asked for a reply (`replycaps`); 7.2 writes reply parts only then.
  pub fn reply_caps(&self) -> Option<&[u8]> {
    self.parts.iter().find_map(|taken| match &taken.asks {
      Asks::ReplyCaps(caps) => Some(caps.as_slice()),
      _ => None,
    })
  }

  /// The changegroup part and its id, if the push carries one.
  pub fn changegroup(&self) -> Option<(u32, &ChangegroupPart)> {
    self.parts.iter().find_map(|taken| match &taken.asks {
      Asks::Changegroup(part) => Some((taken.id, part)),
      _ => None,
    })
  }
}

/// A payload a part's codec refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
  /// A `check:heads` or `check:updated-heads` payload.
  Heads(heads::Refusal),
  /// A `check:phases` or `phase-heads` payload.
  Phases(phases::Refusal),
  /// A `check:bookmarks` or `bookmarks` payload.
  Bookmarks(bookmarks::Refusal),
  /// A `pushkey` part's parameters.
  Pushkey(pushkey::Refusal),
}

/// Why a push's bundle was refused before any of it was judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A mandatory part no handler of a push takes, or a mandatory part with a mandatory parameter its handler does not
  /// name: 7.2's `BundleUnknownFeatureError`, answered `error:unsupportedcontent`.
  Unsupported {
    /// The part's type, in lower case, as 7.2 names it.
    parttype: Vec<u8>,
    /// The parameters its handler does not name, sorted; empty for a part no handler takes.
    params: Vec<Vec<u8>>,
  },
  /// A part's payload or parameters its codec refused.
  Payload {
    /// The part's id.
    id: u32,
    /// What its codec found.
    fault: Fault,
  },
  /// A second `changegroup` part: a push carries one.
  SecondChangegroup {
    /// The second part's id.
    id: u32,
  },
  /// A changegroup version 7.2 does not define, or one a push does not take (`04`, `05`).
  ChangegroupVersion {
    /// The part's id.
    id: u32,
    /// The version named.
    version: Vec<u8>,
  },
  /// A `nbchanges` or `targetphase` parameter that is no number its field holds.
  Parameter {
    /// The part's id.
    id: u32,
    /// The parameter.
    key: &'static [u8],
  },
  /// A `targetphase` other than public or draft: a secret changeset never reaches the server (§4.17.3 decision 3).
  TargetPhase {
    /// The part's id.
    id: u32,
    /// The phase's number.
    number: i32,
  },
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Reads what a push's bundle2 asks (module docs): every part its handlers take, in stream order, each decoded; a
/// mandatory part or parameter no handler takes refused as 7.2 refuses it, and an advisory one passed over.
pub fn read(bundle: &Bundle2<'_>, admission: &mut Admission) -> Result<Unbundle, Refusal> {
  if let Some(nested) = bundle
    .parts
    .iter()
    .find(|part| part.depth > 0 && part.is_mandatory())
  {
    return Err(unsupported(nested, Vec::new()));
  }
  admission.charge_items::<Taken>(bundle.top_level().count())?;
  let mut parts = Vec::with_capacity(bundle.top_level().count());
  for (index, part) in bundle.top_level() {
    let Some(handler) = handler_of(part)? else {
      continue;
    };
    let asks = asks(handler, bundle, index, part, admission)?;
    if let Asks::Changegroup(_) = asks
      && parts
        .iter()
        .any(|taken: &Taken| matches!(taken.asks, Asks::Changegroup(_)))
    {
      return Err(Refusal::SecondChangegroup { id: part.id });
    }
    parts.push(Taken {
      id: part.id,
      mandatory: part.is_mandatory(),
      asks,
    });
  }
  Ok(Unbundle { parts })
}

/// The handler a part goes to: none for an advisory part no handler takes, or one with a mandatory parameter its
/// handler does not name; a mandatory such part refused as 7.2's `_gethandler` refuses it.
fn handler_of(part: &Part<'_>) -> Result<Option<Handler>, Refusal> {
  let Some((_, known, handler)) = HANDLERS.iter().find(|(name, _, _)| part.is(name)) else {
    return if part.is_mandatory() {
      Err(unsupported(part, Vec::new()))
    } else {
      Ok(None)
    };
  };
  let mut unknown: Vec<Vec<u8>> = part
    .mandatory()
    .iter()
    .filter(|param| !known.contains(&param.key))
    .map(|param| param.key.to_vec())
    .collect();
  if unknown.is_empty() {
    return Ok(Some(*handler));
  }
  if !part.is_mandatory() {
    return Ok(None);
  }
  unknown.sort();
  Err(unsupported(part, unknown))
}

/// The refusal of `part` as unsupported content, its type in lower case.
fn unsupported(part: &Part<'_>, params: Vec<Vec<u8>>) -> Refusal {
  Refusal::Unsupported {
    parttype: part.kind.to_ascii_lowercase(),
    params,
  }
}

/// What part `index`, which `handler` takes, asks.
fn asks(
  handler: Handler,
  bundle: &Bundle2<'_>,
  index: usize,
  part: &Part<'_>,
  admission: &mut Admission,
) -> Result<Asks, Refusal> {
  let id = part.id;
  Ok(match handler {
    Handler::ReplyCaps => Asks::ReplyCaps(owned(&whole(bundle, index, admission)?, admission)?),
    Handler::CheckHeads => {
      Asks::CheckHeads(nodes(&whole(bundle, index, admission)?, id, admission)?)
    }
    Handler::CheckUpdatedHeads => {
      Asks::CheckUpdatedHeads(nodes(&whole(bundle, index, admission)?, id, admission)?)
    }
    Handler::CheckPhases => Asks::CheckPhases(phase_heads(
      &whole(bundle, index, admission)?,
      id,
      admission,
    )?),
    Handler::CheckBookmarks => Asks::CheckBookmarks(bookmarks_of(
      &whole(bundle, index, admission)?,
      id,
      admission,
    )?),
    Handler::Changegroup => Asks::Changegroup(changegroup_of(bundle, index, part, admission)?),
    Handler::PhaseHeads => Asks::PhaseHeads(phase_heads(
      &whole(bundle, index, admission)?,
      id,
      admission,
    )?),
    Handler::Bookmarks => Asks::Bookmarks(bookmarks_of(
      &whole(bundle, index, admission)?,
      id,
      admission,
    )?),
    Handler::Pushkey => Asks::Pushkey(pushkey_of(part, admission)?),
    Handler::Obsmarkers => Asks::Obsmarkers(owned(&whole(bundle, index, admission)?, admission)?),
    Handler::Pushvars => Asks::Pushvars(pushvars_of(part, admission)?),
    Handler::Output => Asks::Output,
  })
}

/// Part `index`'s payload whole: borrowed where it arrived in one chunk, joined (charged first) where in several.
fn whole<'a>(
  bundle: &Bundle2<'a>,
  index: usize,
  admission: &mut Admission,
) -> Result<Cow<'a, [u8]>, Refusal> {
  let mut chunks = bundle.payload(index);
  match (chunks.next(), chunks.next()) {
    (None, _) => Ok(Cow::Borrowed(&[])),
    (Some(only), None) => Ok(Cow::Borrowed(only)),
    (Some(_), Some(_)) => Ok(Cow::Owned(bundle.payload_bytes(index, admission)?)),
  }
}

/// `bytes` copied, charged first.
fn owned(bytes: &[u8], admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  admission.charge(to_u64(bytes.len()))?;
  Ok(bytes.to_vec())
}

/// The nodes a heads payload names.
fn nodes(payload: &[u8], id: u32, admission: &mut Admission) -> Result<Vec<Node>, Refusal> {
  let heads = heads::decode(payload).map_err(|fault| Refusal::Payload {
    id,
    fault: Fault::Heads(fault),
  })?;
  admission.charge_items::<Node>(heads.len())?;
  Ok(heads.collect())
}

/// The entries a phases payload holds.
fn phase_heads(
  payload: &[u8],
  id: u32,
  admission: &mut Admission,
) -> Result<Vec<PhaseHead>, Refusal> {
  let entries = phases::decode(payload).map_err(|fault| Refusal::Payload {
    id,
    fault: Fault::Phases(fault),
  })?;
  admission.charge_items::<PhaseHead>(entries.clone().count())?;
  Ok(entries.collect())
}

/// The entries a bookmarks payload holds, each name copied; the entries and the names' bytes charged first.
fn bookmarks_of(
  payload: &[u8],
  id: u32,
  admission: &mut Admission,
) -> Result<Vec<BookmarkAt>, Refusal> {
  let entries = bookmarks::decode(payload).map_err(|fault| Refusal::Payload {
    id,
    fault: Fault::Bookmarks(fault),
  })?;
  let names = entries.clone().fold(0_u64, |total, entry| {
    total.saturating_add(to_u64(entry.name.len()))
  });
  admission.charge_items::<BookmarkAt>(entries.clone().count())?;
  admission.charge(names)?;
  Ok(
    entries
      .map(|entry| BookmarkAt {
        name: entry.name.to_vec(),
        node: entry.node,
      })
      .collect(),
  )
}

/// A `pushkey` part's request, copied.
fn pushkey_of(part: &Part<'_>, admission: &mut Admission) -> Result<PushkeyAsk, Refusal> {
  let request = pushkey::request(part).map_err(|fault| Refusal::Payload {
    id: part.id,
    fault: Fault::Pushkey(fault),
  })?;
  Ok(PushkeyAsk {
    namespace: owned(request.namespace, admission)?,
    key: owned(request.key, admission)?,
    old: owned(request.old, admission)?,
    new: owned(request.new, admission)?,
  })
}

/// A `pushvars` part's variables: its advisory parameters, each key upper-cased behind `USERVAR_` (`bundle2getvars`).
fn pushvars_of(
  part: &Part<'_>,
  admission: &mut Admission,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Refusal> {
  let advisory = part.advisory();
  let bytes = advisory.iter().fold(0_u64, |total, param| {
    total
      .saturating_add(to_u64(USERVAR.len()))
      .saturating_add(to_u64(param.key.len()))
      .saturating_add(to_u64(param.value.len()))
  });
  admission.charge_items::<(Vec<u8>, Vec<u8>)>(advisory.len())?;
  admission.charge(bytes)?;
  Ok(
    advisory
      .iter()
      .map(|param| {
        let key = [USERVAR, param.key.to_ascii_uppercase().as_slice()].concat();
        (key, param.value.to_vec())
      })
      .collect(),
  )
}

/// A `changegroup` part: its version (`01` to `03` taken), its numbers, and its payload, copied.
fn changegroup_of(
  bundle: &Bundle2<'_>,
  index: usize,
  part: &Part<'_>,
  admission: &mut Admission,
) -> Result<ChangegroupPart, Refusal> {
  let id = part.id;
  let named = part.param(b"version");
  let version = Version::from_part_param(named)
    .ok()
    .filter(|version| matches!(version, Version::V01 | Version::V02 | Version::V03))
    .ok_or_else(|| Refusal::ChangegroupVersion {
      id,
      version: named.unwrap_or_default().to_vec(),
    })?;
  let changesets = number(part, b"nbchanges")?;
  let target_phase = target_phase(part)?;
  let payload = match whole(bundle, index, admission)? {
    Cow::Owned(joined) => joined,
    Cow::Borrowed(only) => owned(only, admission)?,
  };
  Ok(ChangegroupPart {
    version,
    changesets,
    target_phase,
    tree_manifests: part.param(b"treemanifest").is_some(),
    payload,
  })
}

/// The decimal number parameter `key` holds, none when absent; refused when it is no number a `u64` holds.
fn number(part: &Part<'_>, key: &'static [u8]) -> Result<Option<u64>, Refusal> {
  part
    .param(key)
    .map(|value| {
      core::str::from_utf8(value)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(Refusal::Parameter { id: part.id, key })
    })
    .transpose()
}

/// The `targetphase` a changegroup part names: public or draft; any other phase refused.
fn target_phase(part: &Part<'_>) -> Result<Option<Phase>, Refusal> {
  let key: &'static [u8] = b"targetphase";
  let Some(value) = part.param(key) else {
    return Ok(None);
  };
  let number: i32 = core::str::from_utf8(value)
    .ok()
    .and_then(|text| text.parse().ok())
    .ok_or(Refusal::Parameter { id: part.id, key })?;
  match Phase::from_number(number) {
    Some(phase @ (Phase::Public | Phase::Draft)) => Ok(Some(phase)),
    _ => Err(Refusal::TargetPhase {
      id: part.id,
      number,
    }),
  }
}

#[cfg(test)]
mod tests;
