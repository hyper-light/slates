//! The consuming runtime's handshake, judged (§4.6 A-9 "Capabilities differ by host, kernel, runtime and VMM
//! and must be reported"; Appendix C "OCI namespace handoff ... must report \[its\] own tested semantics";
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
//! - The runtime: the handshake speaks the Docker engine's interface through its CLI, and podman's own
//!   (A-520: `podman info`, since podman's CLI answers Docker's templates with its own fields) ([`runtime_interface`]).
//!   Any other runtime is refused `RuntimeUnsupported` before anything is asked: a runtime's name certifies nothing.
//! - The endpoint the CLI would reach, before the engine is asked: a local socket or named pipe is this
//!   host's engine. A TCP or SSH endpoint is refused `RemoteEngine`, loopback included, since a forwarded
//!   port can put the engine, and so the bind source's resolution, on any host. An endpoint in no known
//!   scheme is refused `EndpointUnrecognized`. Podman has no engine to reach on its own host: it runs the
//!   container itself ([`Endpoint::Local`]), unless its own statement says its service is remote (a
//!   `CONTAINER_HOST`, a podman machine's VM), which is refused `RemoteEngine` ([`podman_profile`]).
//! - The engine's own statement (`docker info`; `podman info`): its kind, its version, and its security options. A
//!   rootless engine or one remapping users through a user namespace is refused `UserNamespaceUntested`, but for
//!   the one rootless profile a container workload has run through: rootless podman on its own Linux host.
//! - The profile against the evidence: only a profile a container workload has run through by use holds
//!   evidence. Today those are Docker Desktop on macOS over its local socket (T-4.13, `crates/cli/tests/cli.rs`),
//!   a Docker Engine on its own Linux host over its local socket (T-4.13's Linux leg, a shared FUSE mount), and
//!   rootless podman on its own Linux host (T-4.13's rootless leg, an unshared FUSE mount: A-520, from
//!   `a_rootless_container_reads_and_writes_the_volume_through_its_mount`, 2026-10-19). Every other profile is
//!   refused `ProfileUntested`, naming the engine and host.
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

/// Format: podman's CLI, whose own interface the handshake speaks too (A-520), as its executable's name (with the
/// Windows suffix).
const PODMAN: [&str; 2] = ["podman", "podman.exe"];

/// Format: what a refusal names as the endpoint of a podman whose own statement says its service is remote.
const REMOTE_PODMAN: &str = "podman's remote service";

/// Format: Docker's name for its Desktop engine in `docker info`'s `OperatingSystem`.
const DOCKER_DESKTOP: &str = "Docker Desktop";

/// Format: the security option `docker info` lists for a rootless engine (`name=rootless`).
const ROOTLESS: &str = "rootless";
/// Format: the security option `docker info` lists for user-namespace remapping (`name=userns`).
const USER_NAMESPACE: &str = "userns";

/// Format: the security option `docker info` lists for an engine enforcing SELinux labels (`name=selinux`).
const SELINUX: &str = "selinux";

/// Format: the test that ran a container workload through the tested profile.
const TESTED_BY: &str = "T-4.13";
/// Format: the test that ran container workloads through the Linux profile (the `linux-oci` CI lane).
const TESTED_ON_LINUX_BY: &str = "T-4.13 (Linux, a shared FUSE mount)";
/// Format: the test that ran a container workload through rootless podman on Linux
/// (`a_rootless_container_reads_and_writes_the_volume_through_its_mount`, 2026-10-19).
const TESTED_ROOTLESS_PODMAN_BY: &str = "T-4.13 (Linux, rootless podman, an unshared FUSE mount)";

/// A local endpoint the runtime's engine answers on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
  /// A Unix domain socket (`unix://PATH`).
  UnixSocket(String),
  /// A Windows named pipe (`npipe://NAME`).
  NamedPipe(String),
  /// No endpoint: the runtime runs the container on this host itself, with no engine between (podman whose
  /// service is not remote, A-520).
  Local,
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
  /// Podman, which runs a container itself (A-520): its own process tree on this host, no daemon between.
  Podman,
}

/// The engine interface a runtime's CLI speaks, as the handshake asks it ([`runtime_interface`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
  /// The Docker CLI: an engine reached at an endpoint (`docker context inspect`, `docker info`).
  Docker,
  /// Podman's CLI (A-520): the runtime itself, which states whether its service is remote (`podman info`).
  Podman,
}

/// What podman states about itself (`podman info`'s host operating system, version, rootless and SELinux flags,
/// and whether its service is remote; A-520).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodmanFacts {
  /// The host's operating system as podman names it (`linux`).
  pub operating_system: String,
  /// Podman's version.
  pub server_version: String,
  /// Whether podman runs rootless: its containers' root is the user who runs it.
  pub rootless: bool,
  /// Whether SELinux labels what podman binds.
  pub selinux_enabled: bool,
  /// Whether podman runs its containers through a remote service (a `CONTAINER_HOST`, a podman machine's VM).
  pub service_is_remote: bool,
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
  /// Whether the engine enforces SELinux labels on what it binds.
  pub selinux_labels: bool,
}

/// How a container's identity reaches the export through a tested profile (AUD-29-74): measured, never
/// assumed, since it decides who a container acts as on the volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityRule {
  /// Every container identity reaches the export as the host user who runs the engine's file sharing:
  /// container ids and supplementary groups are not forwarded, the container sees its own ids on what it
  /// makes, and permission bits are kept. The authority is the attachment's capability, which the host
  /// mount presented; a container's uid grants and withholds nothing. Measured through Docker Desktop
  /// 29.3.1 on macOS (2026-10-01): as 501:20, 0:0, 1000:1000 and 501:20 with group 12345, each container
  /// wrote and saw its own ids; the host saw every file as 501:20, a 0700 directory kept 0700.
  HostUserThroughShare,
  /// A container's ids reach the export as themselves — no remapping — and the kernel checks each id against
  /// the permission bits (`default_permissions` on the shared FUSE mount); the container's root bypasses them,
  /// as root does on the host. New objects are owned by the container's ids. The mount being shared
  /// (`allow_other`), every local id reaches it exactly as far as its bits allow. Measured through a Linux
  /// Docker Engine (rootful, no user namespace) over `slates mount --shared` (2026-10-01): as 0:0 the
  /// container wrote root-owned files and read another user's 0700 directory; as 2000:2000 it could neither
  /// create in the mounting user's 0755 root nor read the 0700 directory; as the mounting user it wrote its own.
  ContainerIdsAsHostIds,
  /// The container's root reaches the export as the user who mounted it, since a rootless engine maps it so, and
  /// writes, reads and is refused as that user (A-520). Measured through rootless podman on Linux over an unshared
  /// FUSE mount (`a_rootless_container_reads_and_writes_the_volume_through_its_mount`, 2026-10-19): the container's
  /// root read what a client wrote, its write through a read-only bind was refused `EROFS` by the kernel, and its
  /// write through a writable bind landed and was read on the host through the same mount. The container's other
  /// ids are subordinate ids of the host's (`/etc/subuid`), no user of the mount, which an unshared FUSE mount
  /// admits no process of (FUSE without `allow_other` admits the mounting user alone): so a host runs its
  /// container as the container's root. That refusal by use is the profile's live check, owed (filed 2026-11-19).
  ContainerRootAsMountingUser,
}

/// How hard links behave through a tested profile's file sharing — measured, since a workload that links a
/// file and removes its first name (git finalizes every object so) depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardLinkRule {
  /// Once a hard-linked file's first name is removed, its other names fail (`ENOENT`) through the share
  /// until the guest's entry cache revalidates them. Docker Desktop resolves a file on a host filesystem
  /// without lookup by id by the first path it saw — the macOS NFS client answers `fsgetpath` by id
  /// `ENOTSUP` — while slates and the host serve the second name at once. Measured 2026-10-01 through Docker
  /// Desktop 29.3.1: link then remove the first name, then open the second — 99 of 100 failed at once, 0 of 10
  /// after two seconds; the same in a Desktop-shared APFS directory 0 of 100; on the host's own mount 0 of 200.
  /// A harness avoids it by not removing a linked file's first name while it reads another (git:
  /// `core.createObject=rename`).
  OtherNamesStaleAfterTheFirstIsRemoved,
  /// Every name of a hard-linked file is served at once, the first removed or not: the bind is the host's own
  /// FUSE mount, with no file sharing between. Measured 2026-10-01 through a Linux Docker Engine over a shared
  /// mount: link, remove the first name, open the second — 0 of 100 failed. Rootless podman on Linux binds the
  /// same mount the same way, with no sharing between (A-520): its own run of the probe is the profile's live
  /// check, owed (filed 2026-11-19).
  EveryNameServedAtOnce,
}

/// The evidence a judged profile holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestedProfile {
  /// The test that ran a container workload through this profile.
  pub test: &'static str,
  /// The profile, in words.
  pub description: &'static str,
  /// How a container's identity reaches the export.
  pub identity: IdentityRule,
  /// How hard links behave through the profile's file sharing.
  pub hard_links: HardLinkRule,
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
  /// The engine enforces SELinux labels: a bind would need the source relabelled, which nothing authorizes
  /// (no recursive relabel, chown or host policy edit is implicit in an attachment).
  SelinuxLabelsUntested,
  /// No container workload has run through this engine on this host.
  ProfileUntested {
    /// The engine's operating system as it named it.
    engine: String,
    /// The host.
    host: &'static str,
  },
  /// The engine runs rootless, and a rootless engine can't mount a volume's NFS export itself (A-520's export route:
  /// the kernel mounts a file system for a process with `CAP_SYS_ADMIN` in its initial user namespace alone).
  RootlessCannotMount,
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
      Self::SelinuxLabelsUntested => f.write_str(
        "SelinuxLabelsUntested: the engine enforces SELinux labels; slates never relabels a source",
      ),
      Self::ProfileUntested { engine, host } => write!(
        f,
        "ProfileUntested: no container workload has run through {engine:?} on {host}"
      ),
      Self::RootlessCannotMount => f.write_str(
        "RootlessCannotMount: the engine runs rootless, and a rootless engine can't mount a volume's NFS export itself",
      ),
    }
  }
}

/// Admits the runtime by its executable's name, before it is run: the handshake speaks only the Docker CLI.
pub fn admit_runtime(runtime: &str) -> Result<(), ProfileRefusal> {
  match runtime_interface(runtime)? {
    Interface::Docker => Ok(()),
    Interface::Podman => Err(ProfileRefusal::RuntimeUnsupported {
      runtime: runtime.to_owned(),
    }),
  }
}

/// The engine interface `runtime` speaks, by its executable's name, before it is run (A-520): the Docker CLI, or
/// podman's own. Any other runtime is refused `RuntimeUnsupported`: its name certifies nothing. [`admit_runtime`] and
/// [`admit_endpoint`] admit the Docker CLI's alone, whose engine an endpoint names.
pub fn runtime_interface(runtime: &str) -> Result<Interface, ProfileRefusal> {
  let name = runtime.rsplit(['/', '\\']).next().unwrap_or("");
  if DOCKER.contains(&name) {
    Ok(Interface::Docker)
  } else if PODMAN.contains(&name) {
    Ok(Interface::Podman)
  } else {
    Err(ProfileRefusal::RuntimeUnsupported {
      runtime: runtime.to_owned(),
    })
  }
}

/// The profile podman's statement establishes (A-520): podman runs the container on this host itself
/// ([`Endpoint::Local`], [`Engine::Podman`]), rootless or not and labelling or not as it states. A podman whose
/// service is remote is refused `RemoteEngine`, as a Docker endpoint reached over a network is: a bind source would
/// resolve on the service's host.
pub fn podman_profile(facts: &PodmanFacts) -> Result<RuntimeProfile, ProfileRefusal> {
  if facts.service_is_remote {
    return Err(ProfileRefusal::RemoteEngine {
      endpoint: REMOTE_PODMAN.to_owned(),
    });
  }
  Ok(RuntimeProfile {
    endpoint: Endpoint::Local,
    engine: Engine::Podman,
    operating_system: facts.operating_system.clone(),
    server_version: facts.server_version.clone(),
    rootless: facts.rootless,
    user_namespace_remap: false,
    selinux_labels: facts.selinux_enabled,
  })
}

/// Whether `profile` on `host` is rootless podman on its own Linux host: the one rootless profile a container
/// workload has run through (A-520).
fn rootless_podman_on_linux(profile: &RuntimeProfile, host: Host) -> bool {
  profile.engine == Engine::Podman
    && profile.endpoint == Endpoint::Local
    && host == Host::Linux
    && profile.rootless
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
    selinux_labels: facts.security_options.iter().any(|o| names(o, SELINUX)),
  }
}

/// The evidence `profile` holds on `host`, or why it holds none.
pub fn judge(profile: &RuntimeProfile, host: Host) -> Result<TestedProfile, ProfileRefusal> {
  if (profile.rootless || profile.user_namespace_remap) && !rootless_podman_on_linux(profile, host)
  {
    return Err(ProfileRefusal::UserNamespaceUntested);
  }
  if profile.selinux_labels {
    return Err(ProfileRefusal::SelinuxLabelsUntested);
  }
  match (profile.engine, &profile.endpoint, host) {
    (Engine::DockerDesktop, Endpoint::UnixSocket(_), Host::MacOs) => Ok(TestedProfile {
      test: TESTED_BY,
      description: "Docker Desktop on macOS over its local socket, binding the host mount through Desktop's file sharing",
      identity: IdentityRule::HostUserThroughShare,
      hard_links: HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved,
    }),
    (Engine::DockerEngine, Endpoint::UnixSocket(_), Host::Linux) => Ok(TestedProfile {
      test: TESTED_ON_LINUX_BY,
      description: "a Docker Engine on its own Linux host over its local socket, binding the daemon's shared FUSE mount",
      identity: IdentityRule::ContainerIdsAsHostIds,
      hard_links: HardLinkRule::EveryNameServedAtOnce,
    }),
    (Engine::Podman, Endpoint::Local, Host::Linux) if profile.rootless => Ok(TestedProfile {
      test: TESTED_ROOTLESS_PODMAN_BY,
      description: "rootless podman on its own Linux host, binding the daemon's unshared FUSE mount as the container's root",
      identity: IdentityRule::ContainerRootAsMountingUser,
      hard_links: HardLinkRule::EveryNameServedAtOnce,
    }),
    _ => Err(ProfileRefusal::ProfileUntested {
      engine: profile.operating_system.clone(),
      host: host.name(),
    }),
  }
}

/// Format: what put the export route's profiles through a container workload: kiln's drives of an agent in a container
/// on its volume's export (`slates_e2e`'s `container::`, A-520's fifth piece), and its A/B of the export route against
/// the bind (`workspace::container_ab_tests`), each on the host it names.
const EXPORT_TESTED_BY: &str = "kiln's container drives and A/B (A-520's export route)";

/// Where an engine mounts a volume's NFS export from (A-520's export route): the engine makes an ordinary volume of its
/// own from the export (Docker's and podman's `local` driver, `type=nfs`) and mounts it where it runs its containers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountsFrom {
  /// This host's own kernel, reaching the daemon's NFS listener at its loopback: a rootful Docker Engine, or podman run
  /// as root, on its own Linux host.
  HostKernel,
  /// Docker Desktop's Linux VM, reaching this host's loopback as `host.docker.internal`: on macOS (the
  /// Virtualization framework's VM) and on Windows (WSL 2's), where Slates' NFS server runs too (A-184).
  DesktopVm,
}

/// The export route's profile: the evidence it holds and where the engine mounts the export from ([`judge_export`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportProfile {
  /// What put a container workload through this profile.
  pub test: &'static str,
  /// The profile, in words.
  pub description: &'static str,
  /// Where the engine mounts the export from.
  pub mounts_from: MountsFrom,
}

/// Whether `profile` on `host` can mount a volume's NFS export as an engine volume of its own (A-520's export route),
/// and from where; or why not, typed. No share of the host's folders plays a part, so none of the bind's rules does
/// ([`judge`]'s identity and hard-link rules): the container runs as the uid the export is bound to and every other id
/// is a stranger to Slates (A-115). Refused: a rootless engine (it can't mount a file system), one that remaps users
/// (the export's uid would not be the container's), one that labels for SELinux (an NFS mount is labelled whole, which
/// nothing authorizes), and every engine on a host it doesn't run on as named here.
pub fn judge_export(profile: &RuntimeProfile, host: Host) -> Result<ExportProfile, ProfileRefusal> {
  if profile.rootless {
    return Err(ProfileRefusal::RootlessCannotMount);
  }
  if profile.user_namespace_remap {
    return Err(ProfileRefusal::UserNamespaceUntested);
  }
  if profile.selinux_labels {
    return Err(ProfileRefusal::SelinuxLabelsUntested);
  }
  let profile_of = |description: &'static str, mounts_from| {
    Ok(ExportProfile {
      test: EXPORT_TESTED_BY,
      description,
      mounts_from,
    })
  };
  match (profile.engine, &profile.endpoint, host) {
    (Engine::DockerDesktop, Endpoint::UnixSocket(_), Host::MacOs) => profile_of(
      "Docker Desktop on macOS over its local socket, its VM mounting the export at host.docker.internal",
      MountsFrom::DesktopVm,
    ),
    (Engine::DockerDesktop, Endpoint::NamedPipe(_), Host::Windows) => profile_of(
      "Docker Desktop on Windows over its named pipe, its WSL 2 VM mounting the export at host.docker.internal",
      MountsFrom::DesktopVm,
    ),
    (Engine::DockerEngine, Endpoint::UnixSocket(_), Host::Linux) => profile_of(
      "a rootful Docker Engine on its own Linux host over its local socket, the host's kernel mounting the export at its loopback",
      MountsFrom::HostKernel,
    ),
    (Engine::Podman, Endpoint::Local, Host::Linux) => profile_of(
      "podman run as root on its own Linux host, the host's kernel mounting the export at its loopback",
      MountsFrom::HostKernel,
    ),
    _ => Err(ProfileRefusal::ProfileUntested {
      engine: profile.operating_system.clone(),
      host: host.name(),
    }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: rootless podman's statement on its own Linux host, as `podman info`'s fields give it.
  fn rootless_podman() -> PodmanFacts {
    PodmanFacts {
      operating_system: "linux".to_owned(),
      server_version: "5.2.2".to_owned(),
      rootless: true,
      selinux_enabled: false,
      service_is_remote: false,
    }
  }

  /// A-520. Do: name each runtime the handshake speaks, by name and by path, then runtimes it does not speak, then
  /// podman at a Docker endpoint's admission. Expect the Docker CLI's interface and podman's own, every other
  /// runtime refused `RuntimeUnsupported`, and podman refused at the Docker endpoint's admission, which admits the
  /// Docker CLI's alone: podman names no engine at an endpoint.
  #[test]
  fn podman_is_spoken_in_its_own_interface_and_never_at_a_docker_endpoint() {
    for (runtime, interface) in [
      ("docker", Interface::Docker),
      ("/usr/bin/docker", Interface::Docker),
      ("docker.exe", Interface::Docker),
      ("podman", Interface::Podman),
      ("/usr/bin/podman", Interface::Podman),
      (
        "C:\\Program Files\\RedHat\\Podman\\podman.exe",
        Interface::Podman,
      ),
    ] {
      assert_eq!(runtime_interface(runtime), Ok(interface), "{runtime}");
    }
    for runtime in ["nerdctl", "runc", "crun", ""] {
      assert_eq!(
        runtime_interface(runtime),
        Err(ProfileRefusal::RuntimeUnsupported {
          runtime: runtime.to_owned()
        })
      );
    }
    assert_eq!(
      admit_endpoint("podman", "unix:///run/user/1000/podman/podman.sock"),
      Err(ProfileRefusal::RuntimeUnsupported {
        runtime: "podman".to_owned()
      })
    );
  }

  /// A-520. Do: judge rootless podman on its own Linux host, as its statement establishes it. Expect the profile
  /// T-4.13's rootless leg ran through: podman running the container itself, the container's root reaching the
  /// export as the user who mounted it, and every name of a hard-linked file served at once.
  #[test]
  fn rootless_podman_on_its_own_linux_host_holds_its_tested_profile() {
    let profile = podman_profile(&rootless_podman()).unwrap();
    assert_eq!(
      (profile.engine, &profile.endpoint),
      (Engine::Podman, &Endpoint::Local)
    );
    let tested = judge(&profile, Host::Linux).unwrap();
    assert_eq!(tested.test, TESTED_ROOTLESS_PODMAN_BY);
    assert_eq!(
      (tested.identity, tested.hard_links),
      (
        IdentityRule::ContainerRootAsMountingUser,
        HardLinkRule::EveryNameServedAtOnce
      )
    );
  }

  /// A-520. Do: judge podman whose service is remote, podman run as root on Linux, rootless podman that labels for
  /// SELinux, rootless podman judged on macOS, Windows and another host, and a rootless Docker engine on Linux.
  /// Expect `RemoteEngine` before any profile, `ProfileUntested` for podman as root, `SelinuxLabelsUntested`,
  /// `UserNamespaceUntested` on every other host, and `UserNamespaceUntested` for rootless Docker: the verdict is
  /// rootless podman's on its own Linux host, and no other rootless profile's.
  #[test]
  fn every_other_podman_and_every_other_rootless_profile_is_refused_typed() {
    let remote = PodmanFacts {
      service_is_remote: true,
      ..rootless_podman()
    };
    assert_eq!(
      podman_profile(&remote),
      Err(ProfileRefusal::RemoteEngine {
        endpoint: REMOTE_PODMAN.to_owned()
      })
    );
    let as_root = podman_profile(&PodmanFacts {
      rootless: false,
      ..rootless_podman()
    })
    .unwrap();
    assert_eq!(
      judge(&as_root, Host::Linux),
      Err(ProfileRefusal::ProfileUntested {
        engine: "linux".to_owned(),
        host: "linux"
      })
    );
    let labelled = podman_profile(&PodmanFacts {
      selinux_enabled: true,
      ..rootless_podman()
    })
    .unwrap();
    assert_eq!(
      judge(&labelled, Host::Linux),
      Err(ProfileRefusal::SelinuxLabelsUntested)
    );
    let rootless = podman_profile(&rootless_podman()).unwrap();
    for host in [Host::MacOs, Host::Windows, Host::Other] {
      assert_eq!(
        judge(&rootless, host),
        Err(ProfileRefusal::UserNamespaceUntested),
        "{}",
        host.name()
      );
    }
    let rootless_docker = runtime_profile(
      admit_endpoint("docker", "unix:///run/user/1000/docker.sock").unwrap(),
      &EngineFacts {
        operating_system: "Ubuntu 24.04.3 LTS".to_owned(),
        server_version: "29.7.2".to_owned(),
        security_options: vec![
          "name=seccomp,profile=builtin".to_owned(),
          "name=rootless".to_owned(),
        ],
      },
    );
    assert_eq!(
      judge(&rootless_docker, Host::Linux),
      Err(ProfileRefusal::UserNamespaceUntested)
    );
  }
}
