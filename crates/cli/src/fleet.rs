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
//! each node's `address` is the IP its peers dial and the base of the port block it serves them from (two
//! ports per node in the manifest, in manifest order — `slates_server::deploy`); `certificate` and `key`
//! are DER files, resolved against the manifest's own directory. Every certificate is read (they are what
//! the peers pin, and what the member ids derive from); only this node's key is read. The reads are
//! ordinary `std::fs` reads — this crate reads host paths (R1: it never writes one).

use core::net::SocketAddrV4;
use std::path::{Path, PathBuf};

use slates_db::register::Quorum;
use slates_server::deploy::{
  CertificateDer, FleetManifest, FleetNodeEntry, FleetPlan, PrivateKeyDer,
};

use crate::Failure;
use crate::args::FleetSelection;

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
      Self::Plan(e) => write!(out, "{e}"),
    }
  }
}

/// A node as the manifest states it, before any file but the manifest is read.
struct NodeText {
  node: String,
  address: SocketAddrV4,
  certificate: String,
  key: String,
  /// The node's failure domain, if the operator declared one (`DomainId` is a `u64`). Absent = the node is
  /// its own domain (unique-per-host), the default.
  domain: Option<u64>,
}

/// The manifest as text values: what the JSON says, checked for shape.
struct ManifestText {
  name: String,
  quorum: Quorum,
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
  let address = address_text
    .parse::<SocketAddrV4>()
    .map_err(|_| ManifestError::Field {
      field: format!("{path}address"),
      expected: "an IPv4 address with the base port, like `10.0.0.1:7000`",
    })?;
  // `domain` is optional: present only for a real failure-domain topology, and then a non-negative integer.
  let domain = match entry.get("domain") {
    None => None,
    Some(value) => Some(value.as_u64().ok_or_else(|| ManifestError::Field {
      field: format!("{path}domain"),
      expected: "a non-negative integer failure-domain id",
    })?),
  };
  Ok(NodeText {
    node: string_field(entry, &path, "node")?,
    address,
    certificate: string_field(entry, &path, "certificate")?,
    key: string_field(entry, &path, "key")?,
    domain,
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
  Ok(ManifestText {
    name,
    quorum: Quorum { f },
    nodes,
  })
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
  let stated = parse(&text)?;
  let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
  let mut nodes = Vec::with_capacity(stated.nodes.len());
  let mut key = None;
  for node in &stated.nodes {
    let certificate = CertificateDer::from(read_relative(base, &node.certificate)?);
    if node.node == selection.node {
      let key_path = base.join(&node.key);
      let bytes = read_relative(base, &node.key)?;
      key = Some(
        PrivateKeyDer::try_from(bytes).map_err(|reason| ManifestError::Key {
          path: key_path,
          reason: reason.to_owned(),
        })?,
      );
    }
    nodes.push(FleetNodeEntry {
      node: node.node.clone(),
      address: node.address,
      certificate,
      domain: node.domain,
    });
  }
  let manifest = FleetManifest {
    name: stated.name,
    quorum: stated.quorum,
    nodes,
  };
  let Some(key) = key else {
    // Not in the manifest: the plan's own refusal names it, with the same words for every caller.
    return Err(ManifestError::Plan(
      slates_server::DeployError::UnknownNode {
        node: selection.node.clone(),
      },
    ));
  };
  slates_server::deploy::plan(&manifest, &selection.node, key).map_err(ManifestError::Plan)
}

/// This node's deployment plan from `--fleet PATH --node NAME`, or why the manifest yields none — the
/// command fails (exit 4) naming the file or field.
pub(crate) fn load(selection: &FleetSelection) -> Result<FleetPlan, Failure> {
  load_plan(selection)
    .map_err(|e| Failure::Failed(format!("fleet manifest {}: {e}", selection.manifest)))
}

#[cfg(test)]
mod tests {
  use super::*;

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
      "10.0.0.2:7000".parse::<SocketAddrV4>().expect("an address")
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
