//! The Linux conformance adapter's mount authority (§4.6, §4.13, AC-9.7): acquire the same
//! durable host-mount attachment as `slates mount`, present its capability to MOUNT, and end it
//! after unmount or a failed mount attempt. The socket regression drives the real daemon; no
//! privileged kernel mount is needed to catch an adapter that supplies an unauthorized path.

use slates_client::{Client, ClientError, Deadlines, Intent, Refusal, VolumeId};

use crate::Failure;

/// A host-mount attachment owned by the harness until the kernel mount is torn down.
pub(super) struct MountExport {
  client: Client,
  attachment: u64,
  capability: String,
  source: String,
}

impl MountExport {
  /// Obtains a write mount's capability through the ordinary, authorized client path.
  pub(super) fn attach(
    instance: &str,
    id: &str,
    name: &str,
    server: &str,
    deadlines: Deadlines,
  ) -> Result<Self, Failure> {
    let volume = VolumeId {
      bytes: u128::from_str_radix(id, 16)
        .map_err(|error| Failure(format!("invalid volume id: {error}")))?
        .to_be_bytes(),
    };
    let mut client = Client::connect(instance, deadlines)
      .map_err(|error| Failure(format!("mount client: {error}")))?;
    let attached = client
      .attach_mount(volume, Intent::Write)
      .map_err(|error| Failure(format!("attach host mount: {error}")))?;
    let mut export = Self {
      client,
      attachment: attached.attachment,
      capability: String::new(),
      source: String::new(),
    };
    let token = attached
      .token
      .ok_or_else(|| Failure("the daemon returned no host-mount capability".to_owned()))?;
    let token: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
    export.capability = format!("{:x}.{token}", export.attachment);
    export.source = format!("{server}:/{name}@{}", export.capability);
    Ok(export)
  }

  /// The export passed to the OS's NFS client. Never print this bearer capability.
  pub(super) fn source(&self) -> &str {
    &self.source
  }

  /// The mount helper may echo its source even on failure; remove its secret before logging.
  pub(super) fn redact(&self, output: &[u8]) -> String {
    String::from_utf8_lossy(output).replace(&self.capability, "<mount-capability>")
  }
}

impl Drop for MountExport {
  fn drop(&mut self) {
    match self.client.detach(self.attachment) {
      Ok(()) | Err(ClientError::Refused(Refusal::NotFound)) => {}
      Err(error) => eprintln!("conformance mount attachment cleanup: {error}"),
    }
  }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
  use std::io::{Read, Write};
  use std::net::TcpStream;
  use std::time::Duration;

  use slates_client::{CreateSpec, NamePolicy, SizeClass};
  use slates_machine::{MachineProfile, ProfileOptions};
  use slates_server::{Daemon, DaemonConfig, SegmentSource};

  use super::*;

  /// MOUNT MNT over ONC RPC/TCP (RFC 1813 Appendix I), returning the server's status.
  fn mount_status(port: u16, source: &str) -> u32 {
    mount_call(port, source, 1).unwrap()
  }

  /// A MOUNT call: MNT returns a status; UMNT returns no payload.
  fn mount_call(port: u16, source: &str, procedure: u32) -> Option<u32> {
    let (_, path) = source.split_once(':').unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
      .set_read_timeout(Some(super::super::slates::MOUNT_WAIT))
      .unwrap();
    stream
      .set_write_timeout(Some(super::super::slates::MOUNT_WAIT))
      .unwrap();
    let mut call = Vec::new();
    for word in [1_u32, 0, 2, 100_005, 3, procedure, 0, 0, 0, 0] {
      call.extend_from_slice(&word.to_be_bytes());
    }
    call.extend_from_slice(&u32::try_from(path.len()).unwrap().to_be_bytes());
    call.extend_from_slice(path.as_bytes());
    call.resize(call.len().next_multiple_of(size_of::<u32>()), 0);
    let marker = 0x8000_0000 | u32::try_from(call.len()).unwrap();
    stream.write_all(&marker.to_be_bytes()).unwrap();
    stream.write_all(&call).unwrap();
    // Read only the fixed RPC accepted header and MOUNT status, so even a corrupt length cannot
    // cause an unbounded allocation. AUTH_NONE gives an empty verifier in this protocol.
    let mut reply = [0; 7 * size_of::<u32>()];
    stream.read_exact(&mut reply).unwrap();
    let words: Vec<_> = reply
      .as_chunks::<4>()
      .0
      .iter()
      .map(|bytes| u32::from_be_bytes(*bytes))
      .collect();
    assert_eq!(&words[1..7], &[1, 1, 0, 0, 0, 0], "RPC accepted");
    if procedure == 1 {
      let mut status = [0; size_of::<u32>()];
      stream.read_exact(&mut status).unwrap();
      Some(u32::from_be_bytes(status))
    } else {
      None
    }
  }

  /// AC-9.7/T-9.1: the exact source the Linux adapter supplies mounts successfully; tearing down
  /// or abandoning the mount removes its attachment and rejects reuse of the old source.
  #[test]
  fn the_linux_adapter_mounts_with_authority_and_releases_it() {
    let profile = MachineProfile::measure(ProfileOptions {
      budget_per_probe: Duration::from_millis(5),
      codecs: false,
      core_matrix: false,
    });
    let instance = format!("conf-mount-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance).with_shards(2);
    let daemon = Daemon::start(
      &profile,
      config,
      SegmentSource::Create {
        name: instance.clone(),
      },
    )
    .unwrap();
    daemon.bootstrap(true).unwrap();
    let deadlines = super::super::slates::mount_deadlines();
    let mut client = Client::connect(&instance, deadlines).unwrap();
    let volume = client
      .create(&CreateSpec {
        name: "adapter".to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    let id = format!("{:032x}", u128::from_be_bytes(volume.bytes));
    let export = MountExport::attach(&instance, &id, "adapter", "127.0.0.1", deadlines).unwrap();
    let port = client.status(volume).unwrap().nfs_port.unwrap();
    assert_eq!(
      mount_status(port, "127.0.0.1:/adapter"),
      2,
      "bare name refused"
    );
    assert_eq!(mount_status(port, export.source()), 0, "authorized MNT");
    assert_eq!(client.status(volume).unwrap().attachments, 1);
    let echoed = format!("mount.nfs: {} failed", export.source());
    assert_eq!(
      export.redact(echoed.as_bytes()),
      "mount.nfs: 127.0.0.1:/adapter@<mount-capability> failed"
    );
    let expired = export.source().to_owned();
    drop(export);
    assert_eq!(client.status(volume).unwrap().attachments, 0);
    assert_eq!(
      mount_status(port, &expired),
      2,
      "detached capability refused"
    );
    let abandoned = MountExport::attach(&instance, &id, "adapter", "127.0.0.1", deadlines).unwrap();
    drop(abandoned);
    assert_eq!(
      client.status(volume).unwrap().attachments,
      0,
      "failed mount cleaned up"
    );
    let unmounted = MountExport::attach(&instance, &id, "adapter", "127.0.0.1", deadlines).unwrap();
    assert_eq!(mount_status(port, unmounted.source()), 0);
    assert_eq!(mount_call(port, unmounted.source(), 3), None);
    assert_eq!(
      client.status(volume).unwrap().attachments,
      0,
      "kernel UMNT detached"
    );
    drop(unmounted);
    assert_eq!(
      client.status(volume).unwrap().lease_epoch,
      None,
      "write lease released"
    );
    drop(client);
    daemon.stop();
  }
}
