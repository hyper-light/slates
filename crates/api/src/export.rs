//! A volume exported to one client over the node's NFS export (§4.6 "Kubernetes publication without privilege",
//! AUD-29-75; A-115; A-521): the bound host mount's attachment as a verb of the consumer contract (A-299). A host whose
//! work runs where the volume's daemon does not — a pod on another Kubernetes node above all (kiln's Kubernetes engine,
//! its `docs/agentic.md` W10) — asks for the volume's export to one client, a Unix identity, and gets back the
//! export's attachment and the path its capability authorizes, `/<name>@<attachment>.<token>`
//! ([`crate::mount::export_path`]), which the client's mount names: a PersistentVolume's `nfs.path`, which kubelet
//! mounts over RPC-with-TLS. Nothing is mounted here and nothing is listened on: the daemon's NFS listeners serve every
//! capability it issued, the loopback one and, on a fleet node with an operator authority to check its clients against
//! (Unix), the network export, over RPC-with-TLS alone, on the probe plane's address (`crates/server/src/deploy.rs`),
//! which the deployment's own plan names (a fleet manifest's base port; Slates' KIND proof's `base_port`).
//!
//! **One capability per client.** Each call makes an attachment of its own, with a token of its own (§4.13; AUD-01),
//! bound to the client's uid (A-115): that uid's calls are served, and every other caller, the superuser's included, is
//! shown attributes and an `ACCESS` that grants nothing, so what one client's kernel cached for its uid (a link's target
//! above all) is never served to another user. A host gives each client its own export, never one path to share.
//!
//! **Ended by its detach.** [`Instance::detach`] of the export's attachment ends its capability: its path reaches
//! nothing after, whatever a client still holds; the volume's destroy ends it too. Its scope and view are the host
//! mount's (§4.6 item 2): the live head, beneath `subtree` when one is named.
//!
//! **Refused, typed, before any effect:** a volume that does not exist (`NotFound`), and what the daemon refuses of the
//! form, typed as the contract types it (a `subtree` that names nothing or no directory, a volume another node owns,
//! the caller's rights, and for a writable export the volume's write lease while another principal holds it, D-16:
//! one caller may hold many). An answer without its capability breaks the protocol, since every bound host mount's
//! attach carries one, so it is a protocol fault, never a path made up.
//!
//! **Its path is a secret.** The token in it is a bearer capability on the network export's path, so an [`Exported`]'s
//! `Debug` shows the path without it, as a mount's refusal names its mount point and never its capability.

use std::fmt;

use slates_ipc::protocol::{Intent, VolumeId};

use crate::error::{Error, Missing, ProtocolFault};
use crate::instance::Instance;

/// Format: the verb an answer that carried no capability names, as a protocol fault.
const VERB: &str = "attach (an export)";

/// How a volume is exported to its client ([`Instance::export`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
  /// Read-only: the client's changes are refused.
  pub read_only: bool,
  /// Only this directory of the volume (`/src`, from the volume's root): the export's root is it, and `..` there is
  /// itself, on every request.
  pub subtree: Option<String>,
}

/// A volume exported to one client ([`Instance::export`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Exported {
  /// The export's own attachment: [`Instance::detach`] ends it, and its capability with it.
  pub attachment: u64,
  /// The path its capability authorizes, `/<name>@<attachment>.<token>`, which the client's mount names.
  pub path: String,
}

/// Shows the path without its token, a bearer secret (see the module's notes).
impl fmt::Debug for Exported {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let named = self
      .path
      .rsplit_once('.')
      .map_or("", |(named, _token)| named);
    f.debug_struct("Exported")
      .field("attachment", &self.attachment)
      .field("path", &format_args!("{named}.<token>"))
      .finish()
  }
}

impl Instance {
  /// Exports `volume` to one client, the Unix identity `uid` (§4.6 "Kubernetes publication without privilege"; A-115;
  /// A-521): an attachment of its own whose capability serves `uid` alone, and the path the client's mount names (see
  /// the module's notes). Read-only, or beneath a directory, as `options` asks. Refused typed before any effect.
  pub fn export(
    &mut self,
    volume: VolumeId,
    uid: u32,
    options: &ExportOptions,
  ) -> Result<Exported, Error> {
    // The raw status: an export needs only the volume's name, so a base the status could not read now does not stop it.
    let report = self
      .client
      .status(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    let intent = if options.read_only {
      Intent::Read
    } else {
      Intent::Write
    };
    let attachment = self
      .client
      .attach_bound_mount(volume, intent, uid, options.subtree.as_deref())
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    exported(&report.name, attachment.attachment, attachment.token)
  }
}

/// The export of the volume named `name` through `attachment`, whose capability is `token`: the path that capability
/// authorizes; a protocol fault where the daemon answered no token, which every bound host mount's attach carries.
pub(crate) fn exported(
  name: &str,
  attachment: u64,
  token: Option<[u8; 16]>,
) -> Result<Exported, Error> {
  let token = token.ok_or(Error::Protocol(ProtocolFault::UnexpectedReply {
    verb: VERB,
  }))?;
  Ok(Exported {
    attachment,
    path: crate::mount::export_path(name, (attachment, token)),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: an export's attachment as the daemon numbers it.
  const ATTACHMENT: u64 = 0xabc;
  /// Shape: a capability's token, one byte repeated, so its hex is known.
  const TOKEN: [u8; 16] = [0x5a; 16];

  /// §4.13 (AUD-01), A-521. Do: build the export of volume `work` from an attachment the daemon answered with its token,
  /// and from one it answered without. Expect the first the path that token authorizes, exactly what the daemon's MOUNT
  /// service parses, and shown by `Debug` without its token; the second a protocol fault naming the export, never a path.
  #[test]
  fn an_export_is_the_path_its_capability_authorizes_and_never_shows_its_token() {
    let answered = exported("work", ATTACHMENT, Some(TOKEN)).unwrap();
    let token_hex = "5a".repeat(TOKEN.len());
    assert_eq!(
      answered,
      Exported {
        attachment: ATTACHMENT,
        path: format!("/work@abc.{token_hex}"),
      }
    );
    let shown = format!("{answered:?}");
    assert!(
      !shown.contains(&token_hex) && shown.contains("/work@abc.<token>"),
      "{shown}"
    );
    assert_eq!(
      exported("work", ATTACHMENT, None),
      Err(Error::Protocol(ProtocolFault::UnexpectedReply {
        verb: VERB
      }))
    );
  }
}
