//! `cargo xtask kind export` — AUD-29-75's kernel leg (§4.6 "Kubernetes publication without privilege"):
//! kubelet itself mounts a slates volume into a pod as an `nfs` PersistentVolume, over the daemon's RFC 9289
//! RPC-with-TLS export, with no privilege anywhere in slates. Ada authorized its CI tooling on 2026-10-01: the
//! node image with Debian's ktls-utils, the runner's `modprobe tls`, and this step.
//!
//! The run, each step a command on the record:
//!
//! 1. The kernel's TLS record layer is checked (`/sys/module/tls`, seen from a container on the Docker
//!    host's kernel, which kind's nodes share). Without it no RPC-with-TLS mount can succeed; the step skips
//!    loudly, or with `--require-kernel-tls` (CI) fails. Docker Desktop's kernel has `CONFIG_TLS` unset
//!    (measured 2026-10-01), so this leg runs on the GitHub Linux runner.
//! 2. An authority, pod 0's certificate (the fleet's name, its failure domain, and the export's cluster IP,
//!    which the node's client verifies), and the node's client certificate are minted into the scratch.
//! 3. The slates image and the node image (`deploy/kind/node-tls.Dockerfile`) are built, and a one-worker
//!    cluster is created that mounts the client material into its nodes for `tlshd`.
//! 4. The chart is installed with one replica, the authority and the export Service. The group is
//!    bootstrapped, a volume created, and `slates export` prints its path.
//! 5. A PersistentVolume names the export (`nfsvers=4.2,xprtsec=mtls,nosuid,nodev`), and a pod writes a file
//!    through kubelet's mount. A second pod, a fresh mount, must read exactly those bytes, and the kernel must
//!    list its mount `nosuid,nodev`.
//!
//! On any failure the pods' events and the node's `tlshd` journal are printed before the cluster is deleted.

use std::path::Path;
use std::time::{Duration, Instant};

use super::{
  CLUSTER_WAIT, FLEET_NAME, Failure, Lane, NAMESPACE, Options, POLL_CLUSTER, RELEASE, Scratch,
  base_port, base64, capture, create_scratch, down, image, pause, stream, write_scratch,
};

/// Shape: the export leg's own kind cluster; created here, deleted at the end, never anyone else's.
const EXPORT_CLUSTER: &str = "slates-export";
/// Shape: the node image the leg builds from `deploy/kind/node-tls.Dockerfile`.
const NODE_TLS_TAG: &str = "slates-node-tls:lane";
/// Format: the base image the node image is built from, which also answers the kernel check (it is pulled
/// for the build anyway).
const NODE_BASE: &str = "kindest/node:v1.36.4";
/// Format: the export Service's cluster IP, inside kind's default service subnet (10.96.0.0/16), fixed so pod
/// 0's certificate can name it (RFC 9289 §5.2.1: the client checks the server's iPAddress against the
/// address it mounted).
const EXPORT_CLUSTER_IP: &str = "10.96.200.20";
/// Format: where the cluster config mounts the client material in every node (`deploy/kind/tlshd.conf`).
const TLS_MOUNT: &str = "/etc/slates-tls";
/// Shape: the workload pods' image (a shell, `printf` and `cat`), pulled by kubelet itself: loading a
/// registry image with `kind load` imports every platform its index names, and fails on the ones Docker never
/// pulled (measured 2026-10-01: `ctr: content digest … not found`).
const WORKLOAD_IMAGE: &str = "busybox:1.37";
/// Shape: the bounded volume the leg publishes, in slates' spelling.
const VOLUME_BOUND: &str = "64MiB";
/// Format: the same size as a Kubernetes quantity (binary suffix `Mi`), for the PersistentVolume and its claim.
const VOLUME_QUANTITY: &str = "64Mi";
/// Shape: how long a workload pod gets to mount, run and finish — kubelet's mount (the TLS handshake through
/// `tlshd`) and a pod start.
const POD_WAIT: Duration = Duration::from_secs(180);
/// Shape: how long the group gets to form after the bootstrap before a create is accepted.
const GROUP_WAIT: Duration = Duration::from_secs(60);
/// Format: what the writer pod writes and the reader pod must read.
const PAYLOAD: &str = "written through the kubelet RPC-with-TLS mount of a slates volume";

/// `kind export`.
pub(super) fn run(root: &Path, options: &Options) -> Result<(), Failure> {
  if !kernel_has_tls()? {
    let reason = "this kernel has no TLS record layer (CONFIG_TLS; `modprobe tls`), which the kernel NFS \
                  client's RPC-with-TLS needs";
    if options.require_kernel_tls {
      return Err(Failure(format!("kind export: refused: {reason}")));
    }
    eprintln!("kind export: SKIPPED (loud): {reason}");
    return Ok(());
  }
  let scratch = Scratch::create(options.keep)?;
  let values = mint(&scratch.path)?;
  image(root, &options.tag)?;
  node_image(root)?;
  let config = cluster_config(&scratch.path)?;
  stream(
    "kind",
    &[
      "create",
      "cluster",
      "--name",
      EXPORT_CLUSTER,
      "--config",
      &config,
      "--image",
      NODE_TLS_TAG,
      "--wait",
      CLUSTER_WAIT,
    ],
  )?;
  let lane = Lane {
    root: root.to_path_buf(),
    cluster: EXPORT_CLUSTER.to_owned(),
    tag: options.tag.clone(),
    _scratch: scratch,
    keep: options.keep,
    identities: values,
    replicas: 1,
    netem: None,
    trials: 0,
  };
  let outcome = prove(&lane);
  if let Err(e) = &outcome {
    eprintln!("kind export: {e}");
    lane.export_diagnostics();
  }
  if options.keep {
    eprintln!("kind export: cluster {EXPORT_CLUSTER} kept");
  } else if let Err(e) = down(EXPORT_CLUSTER) {
    eprintln!("kind export: {e}");
  }
  outcome
}

/// Whether the Docker host's kernel has its TLS record layer loaded (or built in): `/sys/module/tls`, as a
/// container on that kernel sees it.
fn kernel_has_tls() -> Result<bool, Failure> {
  let outcome = capture(
    "docker",
    &[
      "run",
      "--rm",
      "--entrypoint",
      "sh",
      NODE_BASE,
      "-c",
      "test -d /sys/module/tls",
    ],
  )?;
  Ok(outcome.code == 0)
}

/// The node image, from `deploy/kind/node-tls.Dockerfile`.
fn node_image(root: &Path) -> Result<(), Failure> {
  let dockerfile = root.join("deploy/kind/node-tls.Dockerfile");
  let context = root.join("deploy/kind");
  stream(
    "docker",
    &[
      "build",
      "-t",
      NODE_TLS_TAG,
      "-f",
      &dockerfile.to_string_lossy(),
      &context.to_string_lossy(),
    ],
  )
}

/// Mints the authority, pod 0's identity and the node's client identity: the client material as PEM in
/// `scratch/tls` (for `tlshd`), and the chart values (pod 0's identity, the authority, the export) as a values
/// file, whose path is returned.
fn mint(scratch: &Path) -> Result<std::path::PathBuf, Failure> {
  fn fail(what: &'static str) -> impl Fn(rcgen::Error) -> Failure {
    move |e| Failure(format!("kind export: {what}: {e}"))
  }
  let authority_key = rcgen::KeyPair::generate().map_err(fail("authority key"))?;
  let mut authority_params =
    rcgen::CertificateParams::new(Vec::<String>::new()).map_err(fail("authority params"))?;
  authority_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
  let authority = authority_params
    .self_signed(&authority_key)
    .map_err(fail("authority"))?;
  let issue = |names: Vec<String>| -> Result<(rcgen::Certificate, rcgen::KeyPair), Failure> {
    let key = rcgen::KeyPair::generate().map_err(fail("leaf key"))?;
    let cert = rcgen::CertificateParams::new(names)
      .map_err(fail("leaf params"))?
      .signed_by(&key, &authority, &authority_key)
      .map_err(fail("leaf"))?;
    Ok((cert, key))
  };
  // Pod 0: the fleet's name, its failure domain under the authority (`r0.d0`), and the export's address.
  let (server, server_key) = issue(vec![
    FLEET_NAME.to_owned(),
    format!("r0.d0.{FLEET_NAME}"),
    EXPORT_CLUSTER_IP.to_owned(),
  ])?;
  let (client, client_key) = issue(vec!["slates-export-node".to_owned()])?;
  let tls = scratch.join("tls");
  create_scratch(&tls)?;
  write_scratch(
    &tls.join("authority.pem"),
    pem("CERTIFICATE", authority.der()).as_bytes(),
  )?;
  write_scratch(
    &tls.join("client.pem"),
    pem("CERTIFICATE", client.der()).as_bytes(),
  )?;
  let key_path = tls.join("client.key");
  write_scratch(
    &key_path,
    pem("PRIVATE KEY", &client_key.serialize_der()).as_bytes(),
  )?;
  // tlshd refuses a private key other users can read (measured 2026-10-01: "File …/client.key: expected mode
  // 600", then no key loaded); the bind mount carries the host file's mode into the node.
  {
    use std::os::unix::fs::PermissionsExt;
    #[allow(clippy::disallowed_methods)] // the development tool's own scratch directory
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(PRIVATE_KEY_MODE))
      .map_err(|e| Failure(format!("kind export: {}: {e}", key_path.display())))?;
  }
  let values = format!(
    "certificates:\n  {RELEASE}-0:\n    certificate: {}\n    key: {}\nauthority:\n  certificate: {}\nexport:\n  enabled: true\n  clusterIP: {EXPORT_CLUSTER_IP}\n",
    base64(server.der()),
    base64(&server_key.serialize_der()),
    base64(authority.der()),
  );
  let path = scratch.join("export-values.yaml");
  write_scratch(&path, values.as_bytes())?;
  Ok(path)
}

/// Format: the private key's mode: owner read and write only, what tlshd requires of a key file.
const PRIVATE_KEY_MODE: u32 = 0o600;
/// Format: the characters per line of a PEM body (RFC 7468 §2: "exactly 64 characters" per full line).
const PEM_LINE: usize = 64;

/// `der` as PEM with `label` (RFC 7468): the textual form `tlshd`'s GnuTLS reads. The key is PKCS#8, so its
/// label is `PRIVATE KEY` (RFC 7468 §10).
fn pem(label: &str, der: &[u8]) -> String {
  let body = base64(der);
  let mut out = format!("-----BEGIN {label}-----\n");
  for line in body.as_bytes().chunks(PEM_LINE) {
    out.push_str(&String::from_utf8_lossy(line));
    out.push('\n');
  }
  out.push_str(&format!("-----END {label}-----\n"));
  out
}

/// The cluster config: one control plane and one worker, each mounting the client material for `tlshd`.
fn cluster_config(scratch: &Path) -> Result<String, Failure> {
  let tls = scratch.join("tls");
  let mount = format!(
    "    extraMounts:\n      - hostPath: {}\n        containerPath: {TLS_MOUNT}\n        readOnly: true\n",
    tls.display()
  );
  let config = format!(
    "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes:\n  - role: control-plane\n{mount}  - role: worker\n{mount}"
  );
  let path = scratch.join("cluster-export.yaml");
  write_scratch(&path, config.as_bytes())?;
  Ok(path.to_string_lossy().into_owned())
}

/// The leg on its cluster: images loaded, the chart installed, a volume published, and kubelet's mount
/// written through and read back by a second mount.
fn prove(lane: &Lane) -> Result<(), Failure> {
  stream(
    "kind",
    &["load", "docker-image", &lane.tag, "--name", EXPORT_CLUSTER],
  )?;
  let elapsed = lane.install(1, None)?;
  eprintln!(
    "kind export: one enrolled node with its export rolled out in {:.1} s",
    elapsed.as_secs_f64()
  );
  lane.bootstrap()?;
  let path = lane.publish()?;
  eprintln!("kind export: published {path}");
  lane.apply("persistent-volume", &persistent_volume(&path))?;
  let written = lane.workload(
    "slates-writer",
    &format!("printf '{PAYLOAD}' > /data/hello.txt && cat /data/hello.txt"),
  )?;
  let read = lane.workload(
    "slates-reader",
    "cat /data/hello.txt && echo && grep ' /data ' /proc/self/mountinfo",
  )?;
  let mut lines = read.lines();
  let read_bytes = lines.next().unwrap_or("");
  if written.trim() != PAYLOAD || read_bytes.trim() != PAYLOAD {
    return Err(Failure(format!(
      "kind export: the bytes did not round-trip through kubelet's mounts: wrote {written:?}, a fresh mount read {read:?}"
    )));
  }
  // Condition 4 (A-94): a setuid binary one pod plants on the shared volume must not run with privilege in another.
  // Kubelet's own mount carries the PersistentVolume's `nosuid,nodev`, but containerd's bind of it into a pod does
  // not (measured on this lane 2026-10-06: the node `rw,nosuid,nodev,relatime`, the pod `rw,relatime`), so the
  // pod's mount flags cannot be the guard. The guard is the pod's `allowPrivilegeEscalation: false` (Pod Security's
  // restricted profile; `no_new_privs`, under which exec ignores setuid bits), which `docs/deploy.md` requires.
  // Proven by behaviour: the escalation is shown possible without it, and absent with it.
  let mount_options = lines
    .find_map(|line| line.split(' ').nth(5))
    .unwrap_or("")
    .to_owned();
  eprintln!("kind export: the pod's mount is {mount_options} (kubelet's carries nosuid,nodev)");
  lane.kubelet_mount_diagnostics();
  let (unguarded, guarded) = setuid_probe(lane)?;
  if unguarded.trim() != "0" {
    return Err(Failure(format!(
      "kind export: the setuid probe proves nothing: without no_new_privs it ran as {unguarded:?}, not 0"
    )));
  }
  if guarded.trim() != NON_ROOT {
    return Err(Failure(format!(
      "kind export: a setuid binary planted on the volume ran as {guarded:?} in a pod with allowPrivilegeEscalation: false"
    )));
  }
  eprintln!(
    "kind export: a planted setuid-root binary ran as 0 without allowPrivilegeEscalation: false and as {NON_ROOT} with it"
  );
  eprintln!(
    "kind export: kubelet mounted the volume over RPC-with-TLS; a second mount read the bytes the first wrote"
  );
  Ok(())
}

/// Shape: an image with a real `id` (coreutils): busybox's `id` applet drops setuid privilege by itself, which
/// would make the probe pass whatever the pod's settings.
const SETUID_IMAGE: &str = "debian:bookworm-slim";
/// Format: the unprivileged uid the probe's pods run as (`nobody`), as text.
const NON_ROOT: &str = "65534";

/// A root pod plants a root-owned setuid `id` on the volume; a non-root pod runs it without `no_new_privs` and another
/// with it (`allowPrivilegeEscalation: false`). Returns the effective uid each printed.
fn setuid_probe(lane: &Lane) -> Result<(String, String), Failure> {
  let uid: u64 = NON_ROOT.parse().unwrap_or(u64::MAX);
  lane.workload_as(
    "slates-planter",
    SETUID_IMAGE,
    "cp /usr/bin/id /data/suid-id && chmod 4755 /data/suid-id && echo planted",
    &serde_json::json!({ "runAsUser": 0 }),
  )?;
  let unguarded = lane.workload_as(
    "slates-suid-unguarded",
    SETUID_IMAGE,
    "/data/suid-id -u",
    &serde_json::json!({ "runAsUser": uid, "runAsGroup": uid, "allowPrivilegeEscalation": true }),
  )?;
  let guarded = lane.workload_as(
    "slates-suid-guarded",
    SETUID_IMAGE,
    "/data/suid-id -u",
    &serde_json::json!({ "runAsUser": uid, "runAsGroup": uid, "allowPrivilegeEscalation": false }),
  )?;
  Ok((unguarded, guarded))
}

/// The PersistentVolume naming the export, and its claim (bound to it by name, no storage class).
fn persistent_volume(path: &str) -> String {
  let port = base_port(0);
  format!(
    "apiVersion: v1
kind: PersistentVolume
metadata:
  name: slates-export
spec:
  capacity:
    storage: {VOLUME_QUANTITY}
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: \"\"
  mountOptions: [nfsvers=4.2, xprtsec=mtls, port={port}, nosuid, nodev]
  nfs:
    server: {EXPORT_CLUSTER_IP}
    path: {path}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: slates-export
  namespace: {NAMESPACE}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: \"\"
  volumeName: slates-export
  resources:
    requests:
      storage: {VOLUME_QUANTITY}
"
  )
}

impl Lane {
  /// Creates a volume on pod 0 (once the group accepts it) and `slates export`s it: the path it prints.
  fn publish(&self) -> Result<String, Failure> {
    let pod = format!("{RELEASE}-0");
    let started = Instant::now();
    let id = loop {
      let created = self.verb(
        &pod,
        &["volume", "create", "published", "--bounded", VOLUME_BOUND],
      )?;
      if created.code == 0 {
        let reply: serde_json::Value = serde_json::from_str(&created.stdout)?;
        break reply
          .get("id")
          .and_then(serde_json::Value::as_str)
          .map(str::to_owned)
          .ok_or_else(|| {
            Failure(format!(
              "kind export: create printed no id: {}",
              created.stdout
            ))
          })?;
      }
      if started.elapsed() > GROUP_WAIT {
        return Err(Failure(format!(
          "kind export: the volume was not created within {GROUP_WAIT:?}: {}",
          created.stderr.trim()
        )));
      }
      pause(POLL_CLUSTER);
    };
    let exported = self.verb(&pod, &["export", &id])?;
    if exported.code != 0 {
      return Err(Failure(format!(
        "kind export: `slates export` refused: {}",
        exported.stderr.trim()
      )));
    }
    let reply: serde_json::Value = serde_json::from_str(&exported.stdout)?;
    reply
      .get("path")
      .and_then(serde_json::Value::as_str)
      .map(str::to_owned)
      .ok_or_else(|| {
        Failure(format!(
          "kind export: export printed no path: {}",
          exported.stdout
        ))
      })
  }

  /// `kubectl apply` of `manifest`, written to the scratch first as `<name>.yaml`.
  fn apply(&self, name: &str, manifest: &str) -> Result<(), Failure> {
    let path = self._scratch.path.join(format!("{name}.yaml"));
    write_scratch(&path, manifest.as_bytes())?;
    let outcome = self.kubectl(&["apply", "-f", &path.to_string_lossy()])?;
    if outcome.code != 0 {
      return Err(Failure(format!(
        "kind export: apply refused: {}",
        outcome.stderr.trim()
      )));
    }
    Ok(())
  }

  /// Runs `script` in a pod named `name` with the claim mounted at /data, waits (bounded) until it finishes,
  /// and returns its log: the pod's own view through kubelet's mount.
  fn workload(&self, name: &str, script: &str) -> Result<String, Failure> {
    self.workload_as(name, WORKLOAD_IMAGE, script, &serde_json::json!({}))
  }

  /// [`Self::workload`] in `image` with the container's `securityContext` set to `security`.
  fn workload_as(
    &self,
    name: &str,
    image: &str,
    script: &str,
    security: &serde_json::Value,
  ) -> Result<String, Failure> {
    let pod = serde_json::json!({
      "apiVersion": "v1",
      "kind": "Pod",
      "metadata": { "name": name, "namespace": NAMESPACE },
      "spec": {
        "restartPolicy": "Never",
        "containers": [{
          "name": "work",
          "image": image,
          "imagePullPolicy": "IfNotPresent",
          "command": ["sh", "-c", script],
          "volumeMounts": [{ "name": "data", "mountPath": "/data" }],
          "securityContext": security,
        }],
        "volumes": [{ "name": "data", "persistentVolumeClaim": { "claimName": "slates-export" } }],
      },
    });
    self.apply(name, &pod.to_string())?;
    let started = Instant::now();
    loop {
      let phase = self.kubectl(&["get", "pod", name, "-o", "jsonpath={.status.phase}"])?;
      match phase.stdout.trim() {
        "Succeeded" => break,
        "Failed" => {
          return Err(Failure(format!("kind export: pod {name} failed")));
        }
        _ if started.elapsed() > POD_WAIT => {
          return Err(Failure(format!(
            "kind export: pod {name} did not finish within {POD_WAIT:?} (phase {:?})",
            phase.stdout.trim()
          )));
        }
        _ => pause(POLL_CLUSTER),
      }
    }
    let logs = self.kubectl(&["logs", name])?;
    Ok(logs.stdout)
  }

  /// Where a mount flag the PersistentVolume asked for was lost: kubelet's own NFS mount on the worker, or the
  /// container runtime's bind of it into the pod (the pod sees only the bind). A holder pod keeps the claim mounted
  /// while the worker's mountinfo is read (kubelet unmounts a finished pod's volumes), then is deleted. Evidence only;
  /// errors here are printed, never raised, so the leg's own failure stays the one reported.
  fn kubelet_mount_diagnostics(&self) {
    let holder = serde_json::json!({
      "apiVersion": "v1",
      "kind": "Pod",
      "metadata": { "name": "slates-holder", "namespace": NAMESPACE },
      "spec": {
        "restartPolicy": "Never",
        "containers": [{
          "name": "hold",
          "image": WORKLOAD_IMAGE,
          "imagePullPolicy": "IfNotPresent",
          "command": ["sh", "-c", "grep ' /data ' /proc/self/mountinfo; sleep 300"],
          "volumeMounts": [{ "name": "data", "mountPath": "/data" }],
        }],
        "volumes": [{ "name": "data", "persistentVolumeClaim": { "claimName": "slates-export" } }],
      },
    });
    if let Err(e) = self.apply("slates-holder", &holder.to_string()) {
      eprintln!("kind export: the holder pod was refused: {}", e.0);
      return;
    }
    let started = Instant::now();
    while started.elapsed() < POD_WAIT {
      match self.kubectl(&[
        "get",
        "pod",
        "slates-holder",
        "-o",
        "jsonpath={.status.phase}",
      ]) {
        Ok(phase) if phase.stdout.trim() == "Running" => break,
        _ => pause(POLL_CLUSTER),
      }
    }
    let worker = format!("{EXPORT_CLUSTER}-worker");
    match capture(
      "docker",
      &[
        "exec",
        &worker,
        "grep",
        "-E",
        " - nfs4? ",
        "/proc/self/mountinfo",
      ],
    ) {
      Ok(node) => eprintln!(
        "kind export: {worker}'s NFS mounts (kubelet's own; field 6 is the per-mount flags):\n{}{}",
        node.stdout, node.stderr
      ),
      Err(e) => eprintln!("kind export: reading {worker}'s mountinfo failed: {}", e.0),
    }
    if let Ok(logs) = self.kubectl(&["logs", "slates-holder"]) {
      eprintln!("kind export: the holder pod's view: {}", logs.stdout);
    }
    let _ = self.kubectl(&["delete", "pod", "slates-holder", "--wait=false"]);
  }

  /// What a failed leg leaves to read: the workload pods' events (a refused mount names its error there) and
  /// the worker's `tlshd` journal (the handshake's side).
  fn export_diagnostics(&self) {
    for pod in ["slates-writer", "slates-reader", &format!("{RELEASE}-0")] {
      if let Ok(described) = self.kubectl(&["describe", "pod", pod]) {
        eprintln!("kind export: pod {pod}:\n{}", described.stdout);
      }
    }
    let worker = format!("{EXPORT_CLUSTER}-worker");
    if let Ok(status) = capture(
      "docker",
      &[
        "exec",
        &worker,
        "systemctl",
        "status",
        "tlshd",
        "--no-pager",
      ],
    ) {
      eprintln!(
        "kind export: {worker} tlshd status:\n{}{}",
        status.stdout, status.stderr
      );
    }
    if let Ok(journal) = capture(
      "docker",
      &[
        "exec",
        &worker,
        "journalctl",
        "-u",
        "tlshd",
        "--no-pager",
        "-n",
        "80",
      ],
    ) {
      eprintln!(
        "kind export: {worker} tlshd journal:\n{}{}",
        journal.stdout, journal.stderr
      );
    }
  }
}
