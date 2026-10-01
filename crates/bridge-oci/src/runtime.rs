//! The consuming runtime's handshake, judged (§4.6 A-9 "Capabilities differ by host, kernel, runtime and VMM
//! and must be reported"; Appendix C "OCI namespace handoff ... must report [its] own tested semantics";
//! AUD-29-67).
//!
//! What it is. The daemon exports a host mount and verifies a bind source against the kernel's table; it does
//! not run the container and cannot know which runtime will. Whether a runtime consumes that source as the
//! tests saw is a fact of the runtime's **profile**: which engine answers, where it resolves a bind source,
//! and whether it remaps the ids a container writes with. Docker resolves a bind source on the host its
//! daemon runs on, and Docker Desktop shares host paths into its VM through its own file sharing ("Bind
//! mounts", docs.docker.com/engine/storage/bind-mounts, tier B). So the harness, which holds the runtime,
//! asks the runtime's engine for its profile immediately before it binds (`slates oci-check`), and this
//! module judges the answer.
//!
//! What is judged, in the order that consults least:
//! - The runtime: the handshake speaks the Docker engine's interface through its CLI. Any other runtime is
//!   refused `RuntimeUnsupported` before anything is asked: a runtime's name certifies nothing.
//! - The endpoint the CLI would reach, before the engine is asked: a local socket or named pipe is this
//!   host's engine. A TCP or SSH endpoint is refused `RemoteEngine`, loopback included, since a forwarded
//!   port can put the engine, and so the bind source's resolution, on any host. An endpoint in no known
//!   scheme is refused `EndpointUnrecognized`.
//! - The engine's own statement (`docker info`): its kind, its version, and its security options. A rootless
//!   engine or one remapping users through a user namespace is refused `UserNamespaceUntested`.
//! - The profile against the evidence: only a profile a container workload has run through by use holds
//!   evidence. Today that is Docker Desktop on macOS over its local socket (T-4.13, `crates/cli/tests/cli.rs`).
//!   Every other profile is refused `ProfileUntested`, naming the engine and host.
//!
//! The engine's version is reported, not gated: the evidence names the profile, and the handshake tells the
//! harness which version it is about to trust.

/// The host the harness runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
  /// macOS.
  MacOs,
  /// Linux.
  Linux,
  /// Windows.
  Windows,
  /// Any other.
  Other,
}

impl Host {
  /// This build's host.
  pub const fn this() -> Self {
    if cfg!(target_os = "macos") {
      Self::MacOs
    } else if cfg!(target_os = "linux") {
      Self::Linux
    } else if cfg!(windows) {
      Self::Windows
    } else {
      Self::Other
    }
  }

  /// The host's name in a refusal.
  pub const fn name(self) -> &'static str {
    match self {
      Self::MacOs => "macos",
      Self::Linux => "linux",
      Self::Windows => "windows",
      Self::Other => "other",
    }
  }
}

/// Format: the runtime CLI whose engine interface the handshake speaks, as its executable's name (with the
/// Windows suffix).
const DOCKER: [&str; 2] = ["docker", "docker.exe"];

/// Format: Docker's name for its Desktop engine in `docker info`'s `OperatingSystem`.
const DOCKER_DESKTOP: &str = "Docker Desktop";

/// Format: the security option `docker info` lists for a rootless engine (`name=rootless`).
const ROOTLESS: &str = "rootless";
/// Format: the security option `docker info` lists for user-namespace remapping (`name=userns`).
const USER_NAMESPACE: &str = "userns";

/// Format: the test that ran a container workload through the tested profile.
const TESTED_BY: &str = "T-4.13";

/// A local endpoint the runtime's engine answers on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
  /// A Unix domain socket (`unix://PATH`).
  UnixSocket(String),
  /// A Windows named pipe (`npipe://NAME`).
  NamedPipe(String),
}

/// What an engine states about itself (`docker info`'s `OperatingSystem`, `ServerVersion`,
/// `SecurityOptions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineFacts {
  /// The engine's operating system as it names it (`Docker Desktop` for Desktop's VM; the distribution
  /// for a host's own engine).
  pub operating_system: String,
  /// The engine's version.
  pub server_version: String,
  /// The engine's security options, each `name=NAME[,key=value]...`.
  pub security_options: Vec<String>,
}

/// The engine's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
  /// Docker Desktop: the engine runs in Desktop's VM, which shares host paths in through its file sharing.
  DockerDesktop,
  /// A Docker Engine on its own host.
  DockerEngine,
}

/// The consuming runtime's profile: what the handshake established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeProfile {
  /// The local endpoint the engine answered on.
  pub endpoint: Endpoint,
  /// The engine's kind.
  pub engine: Engine,
  /// The engine's operating system as it named it.
  pub operating_system: String,
  /// The engine's version.
  pub server_version: String,
  /// Whether the engine runs rootless.
  pub rootless: bool,
  /// Whether the engine remaps users through a user namespace.
  pub user_namespace_remap: bool,
}

/// The evidence a judged profile holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestedProfile {
  /// The test that ran a container workload through this profile.
  pub test: &'static str,
  /// The profile, in words.
  pub description: &'static str,
}

/// Why a runtime profile is not one a container workload has run through, typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileRefusal {
  /// The handshake does not speak this runtime's engine interface.
  RuntimeUnsupported {
    /// The runtime as the harness named it.
    runtime: String,
  },
  /// The endpoint is in no scheme the handshake knows.
  EndpointUnrecognized {
    /// The endpoint.
    endpoint: String,
  },
  /// The engine is reached over a network: a bind source would resolve on the engine's host.
  RemoteEngine {
    /// The endpoint.
    endpoint: String,
  },
  /// The engine runs rootless or remaps users through a user namespace; the tested profile ran neither.
  UserNamespaceUntested,
  /// No container workload has run through this engine on this host.
  ProfileUntested {
    /// The engine's operating system as it named it.
    engine: String,
    /// The host.
    host: &'static str,
  },
}

impl std::fmt::Display for ProfileRefusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::RuntimeUnsupported { runtime } => write!(
        f,
        "RuntimeUnsupported: the handshake speaks the Docker engine's interface, not {runtime:?}'s"
      ),
      Self::EndpointUnrecognized { endpoint } => {
        write!(f, "EndpointUnrecognized: {endpoint:?}")
      }
      Self::RemoteEngine { endpoint } => write!(
        f,
        "RemoteEngine: {endpoint} is reached over a network, so a bind source would resolve on the engine's host"
      ),
      Self::UserNamespaceUntested => f.write_str(
        "UserNamespaceUntested: the engine runs rootless or remaps users; no workload has run so",
      ),
      Self::ProfileUntested { engine, host } => write!(
        f,
        "ProfileUntested: no container workload has run through {engine:?} on {host}"
      ),
    }
  }
}

/// Admits the runtime by its executable's name, before it is run: the handshake speaks only the Docker CLI.
pub fn admit_runtime(runtime: &str) -> Result<(), ProfileRefusal> {
  let name = runtime.rsplit(['/', '\\']).next().unwrap_or("");
  if DOCKER.contains(&name) {
    Ok(())
  } else {
    Err(ProfileRefusal::RuntimeUnsupported {
      runtime: runtime.to_owned(),
    })
  }
}

/// Admits the runtime and the endpoint its CLI would reach, before the engine is asked: the local endpoint,
/// or the typed refusal.
pub fn admit_endpoint(runtime: &str, endpoint: &str) -> Result<Endpoint, ProfileRefusal> {
  admit_runtime(runtime)?;
  let unrecognized = || ProfileRefusal::EndpointUnrecognized {
    endpoint: endpoint.to_owned(),
  };
  let (scheme, address) = endpoint.split_once("://").ok_or_else(unrecognized)?;
  match scheme {
    _ if address.is_empty() => Err(unrecognized()),
    "unix" => Ok(Endpoint::UnixSocket(address.to_owned())),
    "npipe" => Ok(Endpoint::NamedPipe(address.to_owned())),
    "tcp" | "ssh" => Err(ProfileRefusal::RemoteEngine {
      endpoint: endpoint.to_owned(),
    }),
    _ => Err(unrecognized()),
  }
}

/// Whether a `name=NAME[,...]` security option names `wanted`.
fn names(option: &str, wanted: &str) -> bool {
  option
    .split(',')
    .any(|field| field.split_once('=') == Some(("name", wanted)))
}

/// The profile the engine's statement and its admitted endpoint establish.
pub fn runtime_profile(endpoint: Endpoint, facts: &EngineFacts) -> RuntimeProfile {
  let engine = if facts.operating_system == DOCKER_DESKTOP {
    Engine::DockerDesktop
  } else {
    Engine::DockerEngine
  };
  RuntimeProfile {
    endpoint,
    engine,
    operating_system: facts.operating_system.clone(),
    server_version: facts.server_version.clone(),
    rootless: facts.security_options.iter().any(|o| names(o, ROOTLESS)),
    user_namespace_remap: facts
      .security_options
      .iter()
      .any(|o| names(o, USER_NAMESPACE)),
  }
}

/// The evidence `profile` holds on `host`, or why it holds none.
pub fn judge(profile: &RuntimeProfile, host: Host) -> Result<TestedProfile, ProfileRefusal> {
  if profile.rootless || profile.user_namespace_remap {
    return Err(ProfileRefusal::UserNamespaceUntested);
  }
  match (profile.engine, &profile.endpoint, host) {
    (Engine::DockerDesktop, Endpoint::UnixSocket(_), Host::MacOs) => Ok(TestedProfile {
      test: TESTED_BY,
      description: "Docker Desktop on macOS over its local socket, binding the host mount through Desktop's file sharing",
    }),
    _ => Err(ProfileRefusal::ProfileUntested {
      engine: profile.operating_system.clone(),
      host: host.name(),
    }),
  }
}
