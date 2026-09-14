//! The Helm chart's gates (docs/deploy.md; `deploy/helm/slates`): `helm lint` must pass, and `helm
//! template` over the fixed identities of `ci/values-golden.yaml` must equal `ci/golden.yaml` byte for
//! byte — so a template edit is a reviewed diff of the rendered Kubernetes objects, never a surprise on a
//! cluster — and a fleet whose identities are incomplete must be refused by name at render time, never
//! installed. Env-gated on `helm` being on PATH: the tests skip loudly where it is absent and never fail
//! for the tool being missing. The golden is regenerated deliberately, never by a normal run:
//! `cargo test -p xtask --test helm_chart -- --ignored regenerate_the_golden_render`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The chart, relative to the workspace root.
const CHART: &str = "deploy/helm/slates";
/// The fixed identities the golden is rendered with.
const GOLDEN_VALUES: &str = "deploy/helm/slates/ci/values-golden.yaml";
/// The golden render.
const GOLDEN: &str = "deploy/helm/slates/ci/golden.yaml";
/// The release name and namespace of the golden render (the pod names and DNS addresses embed them).
const RELEASE: &str = "slates";
const NAMESPACE: &str = "slates";

fn root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .map(Path::to_path_buf)
    .unwrap_or_default()
}

/// Whether `helm` answers on this machine; otherwise the caller skips loudly.
fn helm_present() -> bool {
  match Command::new("helm").arg("version").output() {
    Ok(out) if out.status.success() => true,
    _ => {
      eprintln!("SKIP (loud): helm is not on PATH; the chart gates did not run");
      false
    }
  }
}

/// `helm template` over the golden values, with extra `--set` pairs: (exit code, stdout, stderr), or
/// why helm could not be run.
fn render(extra: &[&str]) -> Result<(i32, String, String), String> {
  let root = root();
  let mut command = Command::new("helm");
  command.current_dir(&root).args([
    "template",
    RELEASE,
    CHART,
    "-f",
    GOLDEN_VALUES,
    "--namespace",
    NAMESPACE,
  ]);
  for pair in extra {
    command.args(["--set", pair]);
  }
  let out = command
    .output()
    .map_err(|e| format!("running helm template: {e}"))?;
  Ok((
    out.status.code().unwrap_or(-1),
    String::from_utf8_lossy(&out.stdout).into_owned(),
    String::from_utf8_lossy(&out.stderr).into_owned(),
  ))
}

/// AC (docs/deploy.md): the chart lints clean under its golden values.
#[test]
fn the_chart_lints_clean() {
  if !helm_present() {
    return;
  }
  let out = Command::new("helm")
    .current_dir(root())
    .args(["lint", CHART, "-f", GOLDEN_VALUES])
    .output()
    .expect("helm runs");
  let text = format!(
    "{}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  assert!(out.status.success(), "helm lint failed:\n{text}");
  assert!(
    text.contains("0 chart(s) failed"),
    "helm lint reported a failure:\n{text}"
  );
}

/// AC (docs/deploy.md): the rendered objects equal the golden byte for byte — a StatefulSet with
/// `podManagementPolicy: Parallel` and no volume claims, the headless Service publishing not-ready
/// addresses, the manifest ConfigMap with `f = 1` for three replicas and one per-pod DNS address per node,
/// and one Secret per pod.
#[test]
fn the_render_equals_the_golden() {
  if !helm_present() {
    return;
  }
  let (code, rendered, stderr) = render(&[]).expect("helm runs");
  assert_eq!(code, 0, "helm template failed: {stderr}");
  let golden = std::fs::read_to_string(root().join(GOLDEN)).expect("the golden render is on file");
  assert!(
    rendered == golden,
    "the render differs from {GOLDEN}; review the diff and regenerate it deliberately with\n  cargo test -p xtask --test helm_chart -- --ignored regenerate_the_golden_render\n--- rendered ---\n{rendered}"
  );
  // The shape the design decided, read from the render rather than assumed.
  assert!(rendered.contains("podManagementPolicy: Parallel"));
  assert!(
    !rendered.contains("volumeClaimTemplates"),
    "no PersistentVolume, ever (R1)"
  );
  assert!(rendered.contains("clusterIP: None"));
  assert!(rendered.contains("publishNotReadyAddresses: true"));
  assert!(rendered.contains("\"f\": 1"), "three replicas derive f = 1");
  assert!(rendered.contains("slates-2.slates.slates.svc.cluster.local:7004"));
  assert!(rendered.contains("name: slates-2-identity"));
  assert!(rendered.contains("readOnlyRootFilesystem: true"));
  assert!(rendered.contains("runAsNonRoot: true"));
}

/// AC (docs/deploy.md): a replica without an identity is refused at render time, naming the value the
/// operator must supply — never a pod that boots without a key.
#[test]
fn a_missing_identity_is_refused_by_name() {
  if !helm_present() {
    return;
  }
  let (code, _, stderr) = render(&["replicas=4"]).expect("helm runs");
  assert_ne!(code, 0, "a fourth replica with no identity must not render");
  assert!(
    stderr.contains("certificates.slates-3 is missing"),
    "the refusal names the missing identity: {stderr}"
  );
}

/// The derivation of `f` by use: five replicas render `f = 2`, one renders `f = 0` (the laptop degenerate).
#[test]
fn the_fault_tolerance_follows_the_replica_count() {
  if !helm_present() {
    return;
  }
  let five = [
    "replicas=5",
    "certificates.slates-3.certificate=eA==",
    "certificates.slates-3.key=eA==",
    "certificates.slates-4.certificate=eA==",
    "certificates.slates-4.key=eA==",
  ];
  let (code, rendered, stderr) = render(&five).expect("helm runs");
  assert_eq!(code, 0, "{stderr}");
  assert!(rendered.contains("\"f\": 2"), "five replicas derive f = 2");
  assert!(rendered.contains("slates-4.slates.slates.svc.cluster.local:7008"));
  let (code, rendered, stderr) = render(&["replicas=1"]).expect("helm runs");
  assert_eq!(code, 0, "{stderr}");
  assert!(rendered.contains("\"f\": 0"), "one replica is f = 0");
}

/// The deliberate writer of the golden (the doc-truth pattern): never part of a normal run.
#[test]
#[ignore = "regenerates deploy/helm/slates/ci/golden.yaml; run deliberately after reviewing a template change"]
fn regenerate_the_golden_render() {
  assert!(helm_present(), "helm is needed to regenerate the golden");
  let (code, rendered, stderr) = render(&[]).expect("helm runs");
  assert_eq!(code, 0, "{stderr}");
  #[allow(clippy::disallowed_methods)] // the deliberate golden writer, run by hand
  std::fs::write(root().join(GOLDEN), rendered).expect("the golden is written");
}
