//! `slates oci-runtime RUNTIME`: the consuming runtime's bounded handshake (§4.6 A-9; AUD-29-67). A container
//! harness runs it immediately before its runtime binds a verified source, beside `slates oci-check`: the
//! source check says the path is still the mount verified, and this says whether the runtime that will bind
//! it is a profile a container workload has run through.
//!
//! The handshake is the library's (`slates_api::container_engine::engine_profile`, moved there from this module on
//! 2026-11-19 by A-520, so the command line and an embedding host judge an engine through one implementation): it asks
//! the runtime's engine, through the runtime's own CLI, for the facts that decide where a bind source resolves and with
//! which ids a container writes, each query bounded in time and in the size of its answer, refused before the engine is
//! asked when the endpoint is not local, and refused typed when the engine does not answer, answers too much, or answers
//! garbage. This module prints its answer: the profile, then the evidence it holds, or the refusal.

use slates_api::container_engine::{
  EngineFault, HardLinkRule, IdentityRule, RuntimeProfile, engine_profile,
};

use crate::Failure;

/// A profile's identity rule on the CLI's output.
fn identity_text(rule: IdentityRule) -> &'static str {
  match rule {
    IdentityRule::HostUserThroughShare => {
      "host_user_through_share (container ids are not forwarded; the attachment's capability is the authority)"
    }
    IdentityRule::ContainerIdsAsHostIds => {
      "container_ids_as_host_ids (no remapping; the kernel checks each id's permission bits, container root bypasses them)"
    }
  }
}

/// A profile's hard-link rule on the CLI's output.
fn hard_link_text(rule: HardLinkRule) -> &'static str {
  match rule {
    HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved => {
      "other_names_stale_after_the_first_is_removed (until the guest revalidates; git: core.createObject=rename)"
    }
    HardLinkRule::EveryNameServedAtOnce => "every_name_served_at_once",
  }
}

/// The profile line: what the engine's statement established, printed before it is judged held or refused.
fn print_profile(endpoint: &str, profile: &RuntimeProfile) {
  slates_machine::out_line!(
    "profile: engine={:?} version={} endpoint={endpoint} rootless={} userns={}",
    profile.operating_system,
    profile.server_version,
    profile.rootless,
    profile.user_namespace_remap
  );
}

/// `oci-runtime RUNTIME`: the profile and the evidence it holds (exit 0), or the typed refusal (exit 1). A profile
/// established and then refused is printed before its refusal, as an operator reading why needs it.
pub(crate) fn oci_runtime(runtime: &str) -> Result<(), Failure> {
  match engine_profile(runtime) {
    Ok(judged) => {
      print_profile(&judged.endpoint, &judged.profile);
      slates_machine::out_line!(
        "evidence: {} ({})",
        judged.tested.test,
        judged.tested.description
      );
      slates_machine::out_line!("identity: {}", identity_text(judged.tested.identity));
      slates_machine::out_line!("hard_links: {}", hard_link_text(judged.tested.hard_links));
      Ok(())
    }
    Err(EngineFault::Untested {
      endpoint,
      profile,
      refusal,
    }) => {
      print_profile(&endpoint, &profile);
      Err(Failure::Refused(refusal.to_string()))
    }
    Err(fault) => Err(Failure::Refused(fault.to_string())),
  }
}
