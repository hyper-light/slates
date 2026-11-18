//! `--fleet PATH --node NAME`: the operator's one shared fleet manifest, read from disk and turned into
//! this node's deployment plan (§2.6 boot step 6; `slates_server::deploy` derives the socket map and the
//! member ids — this module only reads files and checks the manifest's shape).
//!
//! The manifest is JSON, one file for the whole fleet, started on every node with its own `--node`:
//!
//! ```json
//! {
//!   "name": "slates-fleet",
//!   "f": 1,
//!   "nodes": [
//!     { "node": "a", "address": "10.0.0.1:7000", "certificate": "a.crt.der", "key": "a.key.der" },
//!     { "node": "b", "address": "10.0.0.2:7000", "certificate": "b.crt.der", "key": "b.key.der" },
//!     { "node": "c", "address": "10.0.0.3:7000", "certificate": "c.crt.der", "key": "c.key.der" }
//!   ]
//! }
//! ```
//!
//! `name` is the TLS server name every certificate carries (its subject alternative name) and every peer
//! verifies the session against; `f` is the fault tolerance (a write commits at `f + 1` acknowledgements);
//! each node's `address` is the IP **or DNS name** its peers dial and the base of the port block it serves
//! them from (two ports per node in the manifest, in manifest order — `slates_server::deploy`); an optional
//! `bind` (A-277), an IPv4 literal and a nonzero base port, is where the node's own two sockets bind when a port
//! mapping stands between it and its peers (a NAT's port forward, a container's published ports, the chaos
//! harness's relay), the peers still dialing `address`; `certificate`
//! and `key` are DER files, resolved against the manifest's own directory. Every certificate is read (they
//! are what the peers pin, and what the member ids derive from); only this node's key is read. A name is
//! resolved by the daemon at every dial (`slates_server::dns`), through the nameservers of this host's
//! `/etc/resolv.conf`, which is read here once at boot when any node is named (a manifest of literal
//! addresses reads nothing). The reads are ordinary `std::fs` reads — this crate reads host paths (R1: it
//! never writes one).

use core::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use slates_db::register::{Quorum, RegionId};
use slates_server::deploy::{
  CertificateDer, FleetManifest, FleetNodeEntry, FleetPlan, NodeAddress, PrivateKeyDer,
};
use slates_server::{DurabilityBound, Resolver};

use super::{Failure, FleetSelection};

/// Format: where the operating system's resolver configuration lives (`resolv.conf(5)`).
const RESOLV_CONF: &str = "/etc/resolv.conf";
/// Format: the DNS port a nameserver listens on (RFC 1035 §4.2.1).
const DNS_PORT: u16 = 53;
/// Format: the per-query timeout when `options timeout:` is absent — `RES_TIMEOUT`, 5 seconds
/// (`resolv.conf(5)`); the file's value is capped at 30 as the resolver caps it.
const RESOLVER_TIMEOUT_DEFAULT_S: u64 = 5;
/// Format: the resolver's cap on `options timeout:` (`RES_MAXRETRANS`, `resolv.conf(5)`).
const RESOLVER_TIMEOUT_MAX_S: u64 = 30;
/// Format: the attempts when `options attempts:` is absent — `RES_DFLRETRY`, 2 (`resolv.conf(5)`); the
/// file's value is capped at 5 as the resolver caps it.
const RESOLVER_ATTEMPTS_DEFAULT: u32 = 2;
/// Format: the resolver's cap on `options attempts:` (`RES_MAXRETRY`, `resolv.conf(5)`).
const RESOLVER_ATTEMPTS_MAX: u32 = 5;
/// Format: how many `nameserver` lines the resolver honours (`MAXNS`, `resolv.conf(5)`); later ones are
/// ignored as the resolver ignores them.
const RESOLVER_MAX_NAMESERVERS: usize = 3;
/// Format: nanoseconds per second, for the timeout.
const NS_PER_S: u64 = 1_000_000_000;

/// Parses `resolv.conf(5)` text: the first three `nameserver` lines that carry an IPv4 address (the fleet
/// dials IPv4; an IPv6 nameserver is skipped, and a line with a bad address is ignored as the resolver
/// ignores it), and `options timeout:N attempts:N` with the documented defaults and caps. `None` when no
/// IPv4 nameserver is listed — the caller refuses a named manifest then.
pub(crate) fn parse_resolv_conf(text: &str) -> Option<Resolver> {
  let mut nameservers = Vec::new();
  let mut timeout_s = RESOLVER_TIMEOUT_DEFAULT_S;
  let mut attempts = RESOLVER_ATTEMPTS_DEFAULT;
  for line in text.lines() {
    // A comment starts with `#` or `;`; the rest of the line is words.
    let line = line.split(['#', ';']).next().unwrap_or("").trim();
    let mut words = line.split_whitespace();
    match words.next() {
      Some("nameserver") => {
        if nameservers.len() < RESOLVER_MAX_NAMESERVERS
          && let Some(address) = words.next().and_then(|word| word.parse::<Ipv4Addr>().ok())
        {
          nameservers.push(SocketAddrV4::new(address, DNS_PORT));
        }
      }
      Some("options") => {
        for option in words {
          if let Some(value) = option.strip_prefix("timeout:")
            && let Ok(value) = value.parse::<u64>()
          {
            timeout_s = value.min(RESOLVER_TIMEOUT_MAX_S);
          }
          if let Some(value) = option.strip_prefix("attempts:")
            && let Ok(value) = value.parse::<u32>()
          {
            attempts = value.min(RESOLVER_ATTEMPTS_MAX);
          }
        }
      }
      _ => {}
    }
  }
  if nameservers.is_empty() {
    return None;
  }
  Some(Resolver {
    nameservers,
    timeout_ns: timeout_s.saturating_mul(NS_PER_S),
    attempts: attempts.max(1),
  })
}

/// The host's resolver configuration, read from [`RESOLV_CONF`]: `Read` when the file cannot be read,
/// `Resolver` when it lists no IPv4 nameserver.
fn read_resolver() -> Result<Resolver, ManifestError> {
  let path = PathBuf::from(RESOLV_CONF);
  let text = std::fs::read_to_string(&path).map_err(|e| ManifestError::Read {
    path: path.clone(),
    reason: e.to_string(),
  })?;
  parse_resolv_conf(&text).ok_or(ManifestError::Resolver { path })
}

/// Why a manifest did not load: which file, or which field, and what was expected of it.
#[derive(Debug)]
enum ManifestError {
  /// A file could not be read.
  Read {
    /// The path.
    path: PathBuf,
    /// The OS's reason.
    reason: String,
  },
  /// The manifest is not JSON.
  NotJson {
    /// The parser's reason.
    reason: String,
  },
  /// A field is absent or of the wrong shape.
  Field {
    /// The field, as a path into the document (`nodes[2].address`).
    field: String,
    /// What it must be.
    expected: &'static str,
  },
  /// The key bytes are not a DER private key.
  Key {
    /// The path.
    path: PathBuf,
    /// The decoder's reason.
    reason: String,
  },
  /// A node is addressed by a DNS name, but the host's resolver configuration lists no IPv4 nameserver.
  Resolver {
    /// The configuration read.
    path: PathBuf,
  },
  /// The manifest's values yield no plan for this node.
  Plan(slates_server::DeployError),
}

impl std::fmt::Display for ManifestError {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Read { path, reason } => write!(out, "reading {}: {reason}", path.display()),
      Self::NotJson { reason } => write!(out, "not JSON: {reason}"),
      Self::Field { field, expected } => write!(out, "`{field}` must be {expected}"),
      Self::Key { path, reason } => {
        write!(out, "{} is not a DER private key: {reason}", path.display())
      }
      Self::Resolver { path } => write!(
        out,
        "a node is addressed by a DNS name, but {} lists no IPv4 nameserver to resolve it",
        path.display()
      ),
      Self::Plan(e) => write!(out, "{e}"),
    }
  }
}

/// A node as the manifest states it, before any file but the manifest is read.
struct NodeText {
  node: String,
  address: NodeAddress,
  /// Where the node's own serve sockets bind when a port mapping stands between it and its peers (A-277): an
  /// IPv4 literal and a nonzero base port. Absent = it binds its advertised address, the default.
  bind: Option<SocketAddrV4>,
  certificate: String,
  key: String,
  /// The node's failure domain, if the operator declared one (`DomainId` is a `u64`). Absent = the node is
  /// its own domain (unique-per-host), the default.
  domain: Option<u64>,
  /// The node's region, if the operator declared one (`RegionId` is a `u64`). Absent = the sole region 0,
  /// the single-region default (the root group is then the degenerate self-leading group).
  region: Option<u64>,
}

/// The manifest as text values: what the JSON says, checked for shape.
struct ManifestText {
  enrollment_roots: Vec<String>,
  name: String,
  quorum: Quorum,
  durability: Option<DurabilityBound>,
  region_mirrors: std::collections::BTreeMap<RegionId, RegionId>,
  nodes: Vec<NodeText>,
}

fn field<'a>(
  object: &'a serde_json::Value,
  path: &str,
  key: &str,
  expected: &'static str,
) -> Result<&'a serde_json::Value, ManifestError> {
  object.get(key).ok_or_else(|| ManifestError::Field {
    field: format!("{path}{key}"),
    expected,
  })
}

fn string_field(
  object: &serde_json::Value,
  path: &str,
  key: &str,
) -> Result<String, ManifestError> {
  field(object, path, key, "a string")?
    .as_str()
    .map(str::to_owned)
    .ok_or_else(|| ManifestError::Field {
      field: format!("{path}{key}"),
      expected: "a string",
    })
}

fn node_text(entry: &serde_json::Value, index: usize) -> Result<NodeText, ManifestError> {
  let path = format!("nodes[{index}].");
  let address_text = string_field(entry, &path, "address")?;
  let address = NodeAddress::parse(&address_text).map_err(|expected| ManifestError::Field {
    field: format!("{path}address"),
    expected,
  })?;
  // `bind` is optional (A-277): present only when a port mapping stands between the node and its peers, and then an
  // IPv4 literal with a nonzero base port. The node's own sockets bind there, so a name (whose address is not the
  // manifest's to know) or port 0 (the kernel's choice, which no mapping could name) is refused.
  let bind = match entry.get("bind") {
    None => None,
    Some(value) => Some(
      value
        .as_str()
        .and_then(|text| text.parse::<SocketAddrV4>().ok())
        .filter(|bind| bind.port() != 0)
        .ok_or_else(|| ManifestError::Field {
          field: format!("{path}bind"),
          expected: "an IPv4 address and a nonzero base port where this node's own two serve sockets bind, like `0.0.0.0:7000`",
        })?,
    ),
  };
  // `domain` is optional: present only for a real failure-domain topology, and then a non-negative integer.
  let domain = match entry.get("domain") {
    None => None,
    Some(value) => Some(value.as_u64().ok_or_else(|| ManifestError::Field {
      field: format!("{path}domain"),
      expected: "a non-negative integer failure-domain id",
    })?),
  };
  // `region` is optional: present only for a genuine multi-region deployment, and then a non-negative integer.
  let region = match entry.get("region") {
    None => None,
    Some(value) => Some(value.as_u64().ok_or_else(|| ManifestError::Field {
      field: format!("{path}region"),
      expected: "a non-negative integer region id",
    })?),
  };
  Ok(NodeText {
    node: string_field(entry, &path, "node")?,
    address,
    bind,
    certificate: string_field(entry, &path, "certificate")?,
    key: string_field(entry, &path, "key")?,
    domain,
    region,
  })
}

/// Checks the manifest's shape: the fleet name, `f`, and every node's four fields.
fn parse(text: &str) -> Result<ManifestText, ManifestError> {
  let document: serde_json::Value =
    serde_json::from_str(text).map_err(|e| ManifestError::NotJson {
      reason: e.to_string(),
    })?;
  let name = string_field(&document, "", "name")?;
  let f = field(&document, "", "f", "a non-negative integer")?
    .as_u64()
    .and_then(|f| u32::try_from(f).ok())
    .ok_or(ManifestError::Field {
      field: "f".to_owned(),
      expected: "a non-negative integer",
    })?;
  let nodes = field(&document, "", "nodes", "an array of nodes")?
    .as_array()
    .ok_or(ManifestError::Field {
      field: "nodes".to_owned(),
      expected: "an array of nodes",
    })?
    .iter()
    .enumerate()
    .map(|(index, entry)| node_text(entry, index))
    .collect::<Result<Vec<NodeText>, ManifestError>>()?;
  let enrollment_roots = match document.get("enrollment_roots") {
    None => Vec::new(),
    Some(value) => value
      .as_array()
      .ok_or_else(|| ManifestError::Field {
        field: "enrollment_roots".to_owned(),
        expected: "an array of DER certificate paths",
      })?
      .iter()
      .map(|value| {
        value
          .as_str()
          .map(str::to_owned)
          .ok_or_else(|| ManifestError::Field {
            field: "enrollment_roots".to_owned(),
            expected: "a DER certificate path",
          })
      })
      .collect::<Result<Vec<_>, _>>()?,
  };
  Ok(ManifestText {
    enrollment_roots,
    name,
    quorum: Quorum { f },
    durability: durability_text(&document)?,
    region_mirrors: mirrors_text(&document)?,
    nodes,
  })
}

/// Parses the optional fleet-level `mirrors` object (§4.8 "region loss promotes the mirror through the root
/// group"): a map of region id to its mirror region id, e.g. `{ "0": 1, "1": 0 }`. Absent — the default —
/// leaves every region without a mirror; present, each key must be a region id and each value a region id,
/// named by its path when malformed.
fn mirrors_text(
  document: &serde_json::Value,
) -> Result<std::collections::BTreeMap<RegionId, RegionId>, ManifestError> {
  let Some(value) = document.get("mirrors") else {
    return Ok(std::collections::BTreeMap::new());
  };
  let object = value.as_object().ok_or(ManifestError::Field {
    field: "mirrors".to_owned(),
    expected: "an object mapping a region id to its mirror region id",
  })?;
  let mut mirrors = std::collections::BTreeMap::new();
  for (key, mirror) in object {
    let region = key.parse::<u64>().map_err(|_| ManifestError::Field {
      field: format!("mirrors.{key}"),
      expected: "a region id key (a non-negative integer)",
    })?;
    let mirror = mirror.as_u64().ok_or_else(|| ManifestError::Field {
      field: format!("mirrors.{key}"),
      expected: "a mirror region id (a non-negative integer)",
    })?;
    mirrors.insert(RegionId(region), RegionId(mirror));
  }
  Ok(mirrors)
}

/// Parses the optional fleet-level `durability` policy (§4.8, D-14 — "the copyset count check at every
/// configuration change"): an object with `accepted_loss` (a probability in `[0.0, 1.0]`) and
/// `coincident_failures` (a host count). Absent — the default — leaves the check disabled; present, both
/// fields are required and each is named by its path when malformed.
fn durability_text(document: &serde_json::Value) -> Result<Option<DurabilityBound>, ManifestError> {
  let Some(value) = document.get("durability") else {
    return Ok(None);
  };
  let accepted_loss = value
    .get("accepted_loss")
    .and_then(serde_json::Value::as_f64)
    .filter(|loss| (0.0..=1.0).contains(loss))
    .ok_or(ManifestError::Field {
      field: "durability.accepted_loss".to_owned(),
      expected: "a probability in [0.0, 1.0]",
    })?;
  let coincident_failures = value
    .get("coincident_failures")
    .and_then(serde_json::Value::as_u64)
    .ok_or(ManifestError::Field {
      field: "durability.coincident_failures".to_owned(),
      expected: "a non-negative integer host count",
    })?;
  Ok(Some(DurabilityBound {
    accepted_loss,
    coincident_failures,
  }))
}

/// Reads a file the manifest names, relative to the manifest's directory.
fn read_relative(base: &Path, name: &str) -> Result<Vec<u8>, ManifestError> {
  let path = base.join(name);
  std::fs::read(&path).map_err(|e| ManifestError::Read {
    path,
    reason: e.to_string(),
  })
}

/// Reads the manifest and the DER files it names, and plans this node's deployment.
fn load_plan(selection: &FleetSelection) -> Result<FleetPlan, ManifestError> {
  let manifest_path = Path::new(&selection.manifest);
  let text = std::fs::read_to_string(manifest_path).map_err(|e| ManifestError::Read {
    path: manifest_path.to_path_buf(),
    reason: e.to_string(),
  })?;
  let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
  plan_from(
    &text,
    &selection.node,
    base,
    &mut |name| read_relative(base, name),
    &mut read_resolver,
  )
}

/// Plans node `node`'s deployment from the manifest `text`: each DER file it names read by `read` (by its name as the
/// manifest states it, resolved against `base`), and the host's resolver configuration by `resolver`, asked only when
/// a root is trusted or a node is named. [`load`] reads them from the disk; a test hands its own.
fn plan_from(
  text: &str,
  node: &str,
  base: &Path,
  read: &mut dyn FnMut(&str) -> Result<Vec<u8>, ManifestError>,
  resolver: &mut dyn FnMut() -> Result<Resolver, ManifestError>,
) -> Result<FleetPlan, ManifestError> {
  let stated = parse(text)?;
  // The host's resolver, when any node is addressed by name; a manifest of literal addresses reads nothing.
  let resolver = if !stated.enrollment_roots.is_empty()
    || stated.nodes.iter().any(|node| node.address.is_named())
  {
    Some(resolver()?)
  } else {
    None
  };
  let mut nodes = Vec::with_capacity(stated.nodes.len());
  let mut key = None;
  for entry in &stated.nodes {
    let certificate = CertificateDer::from(read(&entry.certificate)?);
    if entry.node == node {
      let bytes = read(&entry.key)?;
      key = Some(
        PrivateKeyDer::try_from(bytes).map_err(|reason| ManifestError::Key {
          path: base.join(&entry.key),
          reason: reason.to_owned(),
        })?,
      );
    }
    nodes.push(FleetNodeEntry {
      node: entry.node.clone(),
      address: entry.address.clone(),
      bind: entry.bind,
      certificate,
      domain: entry.domain,
      region: entry.region.map(RegionId),
    });
  }
  let enrollment_roots = stated
    .enrollment_roots
    .iter()
    .map(|path| read(path).map(CertificateDer::from))
    .collect::<Result<Vec<_>, _>>()?;
  let manifest = FleetManifest {
    enrollment_roots,
    name: stated.name,
    quorum: stated.quorum,
    durability: stated.durability,
    region_mirrors: stated.region_mirrors,
    nodes,
  };
  let Some(key) = key else {
    // Not in the manifest: the plan's own refusal names it, with the same words for every caller.
    return Err(ManifestError::Plan(
      slates_server::DeployError::UnknownNode {
        node: node.to_owned(),
      },
    ));
  };
  slates_server::deploy::plan(&manifest, node, key, resolver).map_err(ManifestError::Plan)
}

/// This node's deployment plan from `--fleet PATH --node NAME`, or why the manifest yields none — the
/// command fails (exit 4) naming the file or field.
pub(crate) fn load(selection: &FleetSelection) -> Result<FleetPlan, Failure> {
  load_plan(selection)
    .map_err(|e| Failure::Failed(format!("fleet manifest {}: {e}", selection.manifest)))
}

/// What a fleet node's anchor and daemon read of their manifest at their start ([`check_fleet`]): the fleet's TLS name,
/// its fault tolerance, how many peers the node starts beside, and how many enrollment roots it trusts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetCheck {
  /// The TLS name every node's certificate carries.
  pub name: String,
  /// The fault tolerance: a write commits at `f + 1` acknowledgements.
  pub f: u32,
  /// The nodes the manifest names besides this one.
  pub peers: usize,
  /// The enrollment roots a node the manifest does not name may enroll under.
  pub enrollment_roots: usize,
}

impl FleetCheck {
  fn of(plan: &FleetPlan) -> FleetCheck {
    FleetCheck {
      name: plan.transport.name.clone(),
      f: plan.membership.quorum.f,
      peers: plan.membership.peers.len(),
      enrollment_roots: plan.transport.enrollment_roots.len(),
    }
  }
}

/// `selection`'s manifest read and this node's deployment planned exactly as its anchor and daemon do at their start
/// (contract 39): for a starter that writes the manifest itself (kiln's runners, kiln goal 95101ef3, K3) to refuse a
/// manifest before it starts an instance on it, in the words the start would end with. Reads the manifest and the
/// files it names, and the host's resolver configuration when a root is trusted or a node is named; writes nothing.
pub fn check_fleet(selection: &FleetSelection) -> Result<FleetCheck, Failure> {
  load(selection).map(|plan| FleetCheck::of(&plan))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The fleet name the planning tests mint certificates for.
  const PLANNED: &str = "slates.planned";

  /// Three nodes' self-signed certificates and keys, as files a manifest names, and the manifest naming them.
  fn minted() -> (std::collections::BTreeMap<String, Vec<u8>>, String) {
    let mut files = std::collections::BTreeMap::new();
    let mut nodes = Vec::new();
    for (index, node) in ["a", "b", "c"].into_iter().enumerate() {
      let key = rcgen::KeyPair::generate().expect("a key pair");
      let certificate = rcgen::CertificateParams::new(vec![PLANNED.to_owned()])
        .expect("params")
        .self_signed(&key)
        .expect("a certificate");
      files.insert(format!("{node}.crt.der"), certificate.der().to_vec());
      files.insert(format!("{node}.key.der"), key.serialize_der());
      nodes.push(format!(
        r#"{{ "node": "{node}", "address": "10.0.0.{}:17300", "certificate": "{node}.crt.der", "key": "{node}.key.der" }}"#,
        index + 1
      ));
    }
    let text = format!(
      r#"{{ "name": "{PLANNED}", "f": 1, "nodes": [{}] }}"#,
      nodes.join(", ")
    );
    (files, text)
  }

  /// Contract 39 (kiln's runners write the manifest their instance starts with): do plan node `a` of a three-node
  /// manifest from files handed in, as the start does from the disk; expect the fleet's name, its `f` and the two
  /// peers. A node the manifest does not name, and a key that is no DER key, are refused by name; no resolver is asked
  /// of a manifest of literal addresses and no roots.
  #[test]
  fn a_manifest_is_planned_from_the_files_it_names_as_a_start_plans_it() {
    let (files, text) = minted();
    let mut read = |name: &str| {
      files.get(name).cloned().ok_or_else(|| ManifestError::Read {
        path: PathBuf::from(name),
        reason: "no such file".to_owned(),
      })
    };
    let mut no_resolver = || -> Result<Resolver, ManifestError> {
      Err(ManifestError::Read {
        path: PathBuf::from(RESOLV_CONF),
        reason: "asked of a manifest of literal addresses".to_owned(),
      })
    };
    let plan = plan_from(&text, "a", Path::new(""), &mut read, &mut no_resolver).expect("a's plan");
    assert_eq!(
      FleetCheck::of(&plan),
      FleetCheck {
        name: PLANNED.to_owned(),
        f: 1,
        peers: 2,
        enrollment_roots: 0
      }
    );
    let unknown = plan_from(&text, "d", Path::new(""), &mut read, &mut no_resolver).err();
    assert!(
      matches!(
        unknown,
        Some(ManifestError::Plan(slates_server::DeployError::UnknownNode { ref node })) if node == "d"
      ),
      "{unknown:?}"
    );
    let mut garbled = files.clone();
    garbled.insert("a.key.der".to_owned(), b"not a key".to_vec());
    let mut read_garbled = |name: &str| {
      garbled
        .get(name)
        .cloned()
        .ok_or_else(|| ManifestError::Read {
          path: PathBuf::from(name),
          reason: "no such file".to_owned(),
        })
    };
    assert!(matches!(
      plan_from(
        &text,
        "a",
        Path::new(""),
        &mut read_garbled,
        &mut no_resolver
      ),
      Err(ManifestError::Key { .. })
    ));
  }

  const MANIFEST: &str = r#"{
    "name": "slates-fleet",
    "f": 1,
    "nodes": [
      { "node": "a", "address": "10.0.0.1:7000", "certificate": "a.crt.der", "key": "a.key.der" },
      { "node": "b", "address": "10.0.0.2:7000", "certificate": "b.crt.der", "key": "b.key.der" }
    ]
  }"#;

  /// The documented shape parses to its values.
  #[test]
  fn the_manifest_shape_parses() {
    let stated = parse(MANIFEST).expect("parses");
    assert_eq!(stated.name, "slates-fleet");
    assert_eq!(stated.quorum, Quorum { f: 1 });
    assert_eq!(stated.nodes.len(), 2);
    assert_eq!(stated.nodes[1].node, "b");
    assert_eq!(
      stated.nodes[1].address,
      NodeAddress::Ip("10.0.0.2:7000".parse::<SocketAddrV4>().expect("an address"))
    );
    assert!(
      !stated.nodes[1].address.is_named(),
      "a literal address reads no resolver configuration"
    );
    assert_eq!(stated.nodes[1].certificate, "b.crt.der");
    assert_eq!(stated.nodes[1].key, "b.key.der");
    assert_eq!(
      stated.nodes[1].domain, None,
      "no failure domain declared: unique-per-host"
    );
  }

  /// A node may declare an optional failure domain; a non-integer one is named by its path.
  #[test]
  fn a_declared_domain_parses_and_a_bad_one_is_named() {
    let with_domain = MANIFEST.replace(
      r#""certificate": "a.crt.der", "key": "a.key.der""#,
      r#""certificate": "a.crt.der", "key": "a.key.der", "domain": 7"#,
    );
    let stated = parse(&with_domain).expect("parses");
    assert_eq!(
      stated.nodes[0].domain,
      Some(7),
      "the declared domain is parsed"
    );
    assert_eq!(
      stated.nodes[1].domain, None,
      "an undeclared node has no domain"
    );

    let bad = with_domain.replace(r#""domain": 7"#, r#""domain": "rack-1""#);
    match parse(&bad) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[0].domain"),
      other => panic!("expected a field error for a non-integer domain, got {other:?}"),
    }
  }

  /// A-277: a node may name where its own sockets bind, apart from where its peers dial it. Do: parse a manifest whose
  /// node a binds `0.0.0.0:17000`, then manifests whose bind is a name, has no port, has port 0, or is no string.
  /// Expect: the bind parsed for a and absent for b; each bad one refused naming `nodes[0].bind`.
  #[test]
  fn a_declared_bind_parses_and_a_bad_one_is_named() {
    let with_bind = MANIFEST.replace(
      r#""certificate": "a.crt.der", "key": "a.key.der""#,
      r#""certificate": "a.crt.der", "key": "a.key.der", "bind": "0.0.0.0:17000""#,
    );
    let stated = parse(&with_bind).expect("parses");
    assert_eq!(
      stated.nodes[0].bind,
      Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 17_000)),
      "the declared bind is parsed"
    );
    assert_eq!(
      stated.nodes[1].bind, None,
      "an undeclared node binds where it advertises"
    );
    for (bad, why) in [
      (r#""bind": "slates-0.svc:7000""#, "a name"),
      (r#""bind": "10.0.0.1""#, "no port"),
      (r#""bind": "10.0.0.1:0""#, "port 0"),
      (r#""bind": 17000"#, "not a string"),
    ] {
      let text = with_bind.replace(r#""bind": "0.0.0.0:17000""#, bad);
      match parse(&text) {
        Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[0].bind", "{why}"),
        other => panic!("expected a field error for a bind that is {why}, got {other:?}"),
      }
    }
  }

  /// A node may declare an optional region; a non-integer one is named by its path.
  #[test]
  fn a_declared_region_parses_and_a_bad_one_is_named() {
    let with_region = MANIFEST.replace(
      r#""certificate": "a.crt.der", "key": "a.key.der""#,
      r#""certificate": "a.crt.der", "key": "a.key.der", "region": 2"#,
    );
    let stated = parse(&with_region).expect("parses");
    assert_eq!(
      stated.nodes[0].region,
      Some(2),
      "the declared region is parsed"
    );
    assert_eq!(
      stated.nodes[1].region, None,
      "an undeclared node has no region"
    );

    let bad = with_region.replace(r#""region": 2"#, r#""region": "east""#);
    match parse(&bad) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[0].region"),
      other => panic!("expected a field error for a non-integer region, got {other:?}"),
    }
  }

  /// A fleet may declare an optional durability policy; it is absent by default, and a malformed field (an
  /// accepted loss outside `[0, 1]`) is named by its path.
  #[test]
  fn a_declared_durability_policy_parses_and_a_bad_one_is_named() {
    assert_eq!(
      parse(MANIFEST).expect("parses").durability,
      None,
      "no durability policy by default"
    );

    let with_durability = MANIFEST.replace(
      r#""f": 1,"#,
      r#""f": 1, "durability": { "accepted_loss": 0.01, "coincident_failures": 10 },"#,
    );
    assert_eq!(
      parse(&with_durability).expect("parses").durability,
      Some(DurabilityBound {
        accepted_loss: 0.01,
        coincident_failures: 10,
      }),
      "the declared durability policy is parsed"
    );

    let bad = with_durability.replace("0.01", "5.0"); // a probability above 1.0
    match parse(&bad) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "durability.accepted_loss"),
      other => panic!("expected a field error for an out-of-range accepted_loss, got {other:?}"),
    }
  }

  /// A fleet may declare region mirrors; the map is empty by default and a malformed mirror is named by its
  /// path.
  #[test]
  fn declared_region_mirrors_parse_and_a_bad_one_is_named() {
    assert!(
      parse(MANIFEST).expect("parses").region_mirrors.is_empty(),
      "no region mirrors by default"
    );

    let with_mirrors = MANIFEST.replace(r#""f": 1,"#, r#""f": 1, "mirrors": { "0": 1, "1": 0 },"#);
    let mirrors = parse(&with_mirrors).expect("parses").region_mirrors;
    assert_eq!(
      mirrors.get(&RegionId(0)),
      Some(&RegionId(1)),
      "region 0's mirror is region 1"
    );
    assert_eq!(
      mirrors.get(&RegionId(1)),
      Some(&RegionId(0)),
      "region 1's mirror is region 0"
    );

    let bad = with_mirrors.replace(r#""1": 0"#, r#""1": "west""#);
    match parse(&bad) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "mirrors.1"),
      other => panic!("expected a field error for a non-integer mirror, got {other:?}"),
    }
  }

  /// A node may be addressed by a DNS name with its base port (the Kubernetes deployment names each node
  /// by its per-pod DNS name); a name that is not a hostname is named by its path.
  #[test]
  fn a_named_address_parses_and_a_bad_one_is_named() {
    let named = MANIFEST.replace(
      "10.0.0.1:7000",
      "slates-0.slates.default.svc.cluster.local:7000",
    );
    let stated = parse(&named).expect("parses");
    assert_eq!(
      stated.nodes[0].address,
      NodeAddress::Name {
        host: "slates-0.slates.default.svc.cluster.local".to_owned(),
        port: 7000
      }
    );
    assert!(stated.nodes[0].address.is_named());

    let bad = MANIFEST.replace("10.0.0.1:7000", "slates_0.local:7000");
    match parse(&bad) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[0].address"),
      other => panic!("expected a field error for a bad hostname, got {other:?}"),
    }
    let no_port = MANIFEST.replace("10.0.0.1:7000", "slates-0.local");
    match parse(&no_port) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[0].address"),
      other => panic!("expected a field error for a missing port, got {other:?}"),
    }
  }

  /// `resolv.conf(5)` as a pod or a host writes it: the IPv4 nameservers (at most three, IPv6 skipped),
  /// the timeout and attempts options with their documented defaults and caps, comments ignored; a file
  /// with no IPv4 nameserver yields no resolver.
  #[test]
  fn resolv_conf_parses_to_the_resolver() {
    let pod = "search default.svc.cluster.local svc.cluster.local cluster.local\nnameserver 10.96.0.10\noptions ndots:5\n";
    assert_eq!(
      parse_resolv_conf(pod),
      Some(Resolver {
        nameservers: vec![SocketAddrV4::new(Ipv4Addr::new(10, 96, 0, 10), DNS_PORT)],
        timeout_ns: RESOLVER_TIMEOUT_DEFAULT_S * NS_PER_S,
        attempts: RESOLVER_ATTEMPTS_DEFAULT,
      })
    );
    let host = "# a comment\nnameserver fe80::1 ; ipv6 first\nnameserver 1.1.1.1\nnameserver 8.8.8.8\nnameserver 9.9.9.9\nnameserver 4.4.4.4\noptions timeout:2 attempts:7 rotate\n";
    let resolver = parse_resolv_conf(host).expect("a resolver");
    assert_eq!(
      resolver.nameservers,
      vec![
        SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), DNS_PORT),
        SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), DNS_PORT),
        SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), DNS_PORT),
      ],
      "the first three IPv4 nameservers, the IPv6 one skipped"
    );
    assert_eq!(resolver.timeout_ns, 2 * NS_PER_S);
    assert_eq!(
      resolver.attempts, RESOLVER_ATTEMPTS_MAX,
      "attempts is capped as the resolver caps it"
    );
    assert_eq!(
      parse_resolv_conf("options timeout:1\n"),
      None,
      "no nameserver: no resolver"
    );
    assert_eq!(
      parse_resolv_conf("nameserver fe80::1\n"),
      None,
      "an IPv6-only file: no resolver the fleet can dial"
    );
    assert_eq!(
      parse_resolv_conf("nameserver 10.0.0.1\noptions timeout:99\n")
        .expect("a resolver")
        .timeout_ns,
      RESOLVER_TIMEOUT_MAX_S * NS_PER_S,
      "the timeout is capped as the resolver caps it"
    );
  }

  /// A missing or malformed field is named by its path into the document.
  #[test]
  fn a_bad_field_is_named() {
    let no_key = MANIFEST.replace(r#""key": "b.key.der""#, r#""keys": "b.key.der""#);
    match parse(&no_key) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[1].key"),
      other => panic!("expected a field error, got {other:?}"),
    }
    let bad_address = MANIFEST.replace("10.0.0.1:7000", "10.0.0.1");
    match parse(&bad_address) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "nodes[0].address"),
      other => panic!("expected a field error, got {other:?}"),
    }
    let negative_f = MANIFEST.replace(r#""f": 1"#, r#""f": -1"#);
    match parse(&negative_f) {
      Err(ManifestError::Field { field, .. }) => assert_eq!(field, "f"),
      other => panic!("expected a field error, got {other:?}"),
    }
    assert!(matches!(parse("{"), Err(ManifestError::NotJson { .. })));
  }

  impl std::fmt::Debug for ManifestText {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      write!(
        out,
        "manifest {} f={} nodes={}",
        self.name,
        self.quorum.f,
        self.nodes.len()
      )
    }
  }
}
