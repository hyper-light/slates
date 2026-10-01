//! The consuming runtime's handshake judged (§4.6 A-9 "Capabilities differ by host, kernel, runtime and VMM";
//! AUD-29-67): which runtime profiles hold by-use evidence, and the typed refusal of every other. Pure, so
//! every profile is judged on every host from the facts an engine states.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_oci::runtime::{
  EngineFacts, Host, ProfileRefusal, admit_endpoint, judge, runtime_profile,
};

/// Format: Docker Desktop's answer as this machine's engine gave it (29.3.1, 2026-10-01).
fn desktop() -> EngineFacts {
  EngineFacts {
    operating_system: "Docker Desktop".to_owned(),
    server_version: "29.3.1".to_owned(),
    security_options: vec![
      "name=seccomp,profile=builtin".to_owned(),
      "name=cgroupns".to_owned(),
    ],
  }
}

/// Format: a Linux host's own Docker Engine, as `docker info` names the distribution.
fn linux_engine() -> EngineFacts {
  EngineFacts {
    operating_system: "Ubuntu 24.04.3 LTS".to_owned(),
    ..desktop()
  }
}

/// Format: the local endpoints the engines answer on (Docker Desktop's user socket; a Linux daemon's).
const DESKTOP_SOCKET: &str = "unix:///Users/someone/.docker/run/docker.sock";
const LINUX_SOCKET: &str = "unix:///var/run/docker.sock";

/// AUD-29-67. Do: judge Docker Desktop on macOS over its local socket. Expect: the profile T-4.13 ran, named
/// with the engine's own version; the same engine on Linux and a Linux host's Docker Engine are refused
/// `ProfileUntested`, naming the engine and host, since no container workload has run through them.
#[test]
fn only_the_profile_a_container_workload_ran_through_holds_evidence() {
  let endpoint = admit_endpoint("docker", DESKTOP_SOCKET).unwrap();
  let profile = runtime_profile(endpoint.clone(), &desktop());
  let tested = judge(&profile, Host::MacOs).unwrap();
  assert_eq!(tested.test, "T-4.13");
  assert_eq!(profile.server_version, "29.3.1");
  assert!(matches!(
    judge(&profile, Host::Linux),
    Err(ProfileRefusal::ProfileUntested { .. })
  ));
  let linux = runtime_profile(
    admit_endpoint("docker", LINUX_SOCKET).unwrap(),
    &linux_engine(),
  );
  assert!(matches!(
    judge(&linux, Host::Linux),
    Err(ProfileRefusal::ProfileUntested { engine, .. }) if engine == "Ubuntu 24.04.3 LTS"
  ));
  assert!(matches!(
    judge(&linux, Host::MacOs),
    Err(ProfileRefusal::ProfileUntested { .. })
  ));
}

/// AUD-29-67. Do: admit endpoints a remote engine answers on (TCP, including loopback, and SSH), an endpoint
/// in no scheme the handshake knows, and runtimes whose engine interface the handshake does not speak.
/// Expect: each refused typed before any engine is asked — `RemoteEngine` (a bind source resolves on the
/// engine's host, which a forwarded port can put anywhere), `EndpointUnrecognized`, `RuntimeUnsupported`;
/// a Windows named pipe and the runtime given by its path are admitted.
#[test]
fn a_remote_or_unknown_endpoint_and_an_unspoken_runtime_are_refused_before_the_engine_is_asked() {
  for remote in [
    "tcp://127.0.0.1:2375",
    "tcp://[::1]:2376",
    "ssh://builder@build-host",
  ] {
    assert_eq!(refusal("docker", remote), "RemoteEngine", "{remote}");
  }
  for unknown in ["", "fd://", "/var/run/docker.sock", "unix:"] {
    assert_eq!(
      refusal("docker", unknown),
      "EndpointUnrecognized",
      "{unknown:?}"
    );
  }
  for runtime in ["podman", "nerdctl", "runc", "", "/usr/local/bin/"] {
    assert_eq!(
      refusal(runtime, DESKTOP_SOCKET),
      "RuntimeUnsupported",
      "{runtime:?}"
    );
  }
  assert_eq!(
    refusal("docker", "npipe:////./pipe/docker_engine"),
    "admitted"
  );
  assert_eq!(refusal("/usr/local/bin/docker", DESKTOP_SOCKET), "admitted");
}

/// The refusal `admit_endpoint` answers, by name, or `admitted`.
fn refusal(runtime: &str, endpoint: &str) -> &'static str {
  match admit_endpoint(runtime, endpoint) {
    Ok(_) => "admitted",
    Err(ProfileRefusal::RemoteEngine { .. }) => "RemoteEngine",
    Err(ProfileRefusal::EndpointUnrecognized { .. }) => "EndpointUnrecognized",
    Err(ProfileRefusal::RuntimeUnsupported { .. }) => "RuntimeUnsupported",
    Err(_) => "another refusal",
  }
}

/// AUD-29-67. Do: judge Docker Desktop on macOS whose engine runs rootless, or remaps users through a user
/// namespace. Expect: `UserNamespaceUntested` either way — the tested profile ran neither, and the ids a
/// container writes with differ under both.
#[test]
fn a_user_namespace_the_workload_never_ran_under_is_refused() {
  for option in ["name=rootless", "name=userns"] {
    let mut facts = desktop();
    facts.security_options.push(option.to_owned());
    let profile = runtime_profile(admit_endpoint("docker", DESKTOP_SOCKET).unwrap(), &facts);
    assert_eq!(
      judge(&profile, Host::MacOs),
      Err(ProfileRefusal::UserNamespaceUntested),
      "{option}"
    );
  }
}
