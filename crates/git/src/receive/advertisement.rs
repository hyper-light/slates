//! receive-pack's ref advertisement (A-620 part 1; gitprotocol-pack "Reference Discovery"; git's
//! `show_ref` and `write_head_info`, v2.50.1): A-604's protocol v0 advertisement
//! ([`crate::v0::Advertisement`]), written with the capabilities receive-pack advertises in
//! `show_ref`'s order and no peeled lines, which receive-pack never sends.

use super::Refusal;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::Out;
use crate::v0::{AdvertisedRef, Advertisement, Capabilities, Capability, Version};

/// Format: the capabilities receive-pack always advertises, in `show_ref`'s order.
const ALWAYS: [&[u8]; 5] = [
  b"report-status",
  b"report-status-v2",
  b"delete-refs",
  b"side-band-64k",
  b"quiet",
];
/// Format: `atomic`, when advertised.
const ATOMIC: &[u8] = b"atomic";
/// Format: `ofs-delta`, when advertised.
const OFS_DELTA: &[u8] = b"ofs-delta";
/// Format: `push-options`, when advertised.
const PUSH_OPTIONS: &[u8] = b"push-options";
/// Format: `session-id`, before the session's id.
const SESSION_ID: &[u8] = b"session-id";
/// Format: `object-format`, before the repository's format.
const OBJECT_FORMAT: &[u8] = b"object-format";
/// Format: `agent`, before the server's agent.
const AGENT: &[u8] = b"agent";

/// What receive-pack advertises: its repository's format and the capabilities a deployment chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advertised<'a> {
  /// The repository's object format.
  pub format: ObjectFormat,
  /// `atomic` (git: `receive.advertiseAtomic`, on by default).
  pub atomic: bool,
  /// `ofs-delta` (git: `repack.useDeltaBaseOffset`, on by default).
  pub ofs_delta: bool,
  /// `push-options` (git: `receive.advertisePushOptions`).
  pub push_options: bool,
  /// `session-id=`, when the deployment tells clients its session (git: `transfer.advertiseSID`).
  pub session_id: Option<&'a [u8]>,
  /// `agent=`.
  pub agent: &'a [u8],
}

/// Writes receive-pack's advertisement of `refs` (each an id and a name, in the order to advertise
/// them: git's is by name) as git writes it, through A-604's writer: refused, nothing written, for
/// a part that writer refuses.
pub fn write_advertisement(
  refs: &[(ObjectId, &[u8])],
  advertised: &Advertised<'_>,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let format = advertised.format.name().as_bytes();
  let mut list: Vec<Capability<'_>> = ALWAYS
    .iter()
    .map(|name| Capability { name, value: None })
    .collect();
  for (on, name) in [
    (advertised.atomic, ATOMIC),
    (advertised.ofs_delta, OFS_DELTA),
    (advertised.push_options, PUSH_OPTIONS),
  ] {
    if on {
      list.push(Capability { name, value: None });
    }
  }
  let valued = [
    (SESSION_ID, advertised.session_id),
    (OBJECT_FORMAT, Some(format)),
    (AGENT, Some(advertised.agent)),
  ];
  for (name, value) in valued {
    if value.is_some() {
      list.push(Capability { name, value });
    }
  }
  let advertisement = Advertisement {
    version: Version::V0,
    capabilities: Capabilities::listed(&list),
    refs: refs
      .iter()
      .map(|(id, name)| AdvertisedRef {
        id: *id,
        name,
        peeled: None,
      })
      .collect(),
    shallows: Vec::new(),
  };
  advertisement.write(out).map_err(Refusal::Advertisement)
}
