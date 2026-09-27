//! The v4 front end over a synchronous [`NfsService`] (A-35): the backend the standalone server, the
//! examples and the tests drive, where every v3 call is answered in place. The daemon has its own
//! backend, which routes each call to the volume's owner shard.

use std::future::{Future, Ready, ready};
use std::task::{Context, Poll, Waker};

use super::compound::{self, Backend, Server};
use crate::multi::NfsService;
use crate::nfs::Nfsfh3;
use crate::xdr::XdrReader;

/// A backend whose v3 calls are served by `service` in place.
pub struct ServiceBackend<'a> {
  service: &'a mut dyn NfsService,
  server: &'a mut Server,
  root: Nfsfh3,
  principal: u32,
  now_ns: u64,
}

impl<'a> ServiceBackend<'a> {
  /// A backend over `service` and the v4 state `server`, for a connection running as `principal`, at
  /// `now_ns` on the caller's monotonic clock (one instant for the whole compound).
  pub fn new(
    service: &'a mut dyn NfsService,
    server: &'a mut Server,
    principal: u32,
    now_ns: u64,
  ) -> Self {
    let root = service.v4_root();
    ServiceBackend {
      service,
      server,
      root,
      principal,
      now_ns,
    }
  }
}

impl Backend for ServiceBackend<'_> {
  fn call_v3(&mut self, procedure: u32, args: Vec<u8>) -> impl Future<Output = Vec<u8>> {
    let mut reader = XdrReader::new(&args);
    let result: Ready<Vec<u8>> = ready(
      self
        .service
        .serve_procedure(procedure, &mut reader)
        .unwrap_or_default(),
    );
    result
  }

  fn root_handle(&self) -> Nfsfh3 {
    self.root.clone()
  }

  fn principal(&self) -> u32 {
    self.principal
  }

  fn now_ns(&self) -> u64 {
    self.now_ns
  }

  fn with_v4<R>(&mut self, f: impl FnOnce(&mut Server) -> R) -> R {
    f(self.server)
  }
}

/// Serves one v4 `COMPOUND` at `now_ns` (a monotonic clock that the leases are measured on) over a
/// synchronous service: every call the compound makes is answered in place, so the compound completes
/// in one poll. `None` if it did not (a backend that pends has no place in this synchronous driver).
pub fn serve_compound(
  service: &mut dyn NfsService,
  server: &mut Server,
  principal: u32,
  now_ns: u64,
  args: &[u8],
) -> Option<Vec<u8>> {
  let mut backend = ServiceBackend::new(service, server, principal, now_ns);
  let future = compound::serve(&mut backend, args);
  let mut future = std::pin::pin!(future);
  match future
    .as_mut()
    .poll(&mut Context::from_waker(Waker::noop()))
  {
    Poll::Ready(reply) => Some(reply),
    Poll::Pending => None,
  }
}

/// Nanoseconds on a monotonic clock since the first call in this process: the clock the standalone
/// server's leases are measured on.
pub fn monotonic_ns() -> u64 {
  static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
  let epoch = EPOCH.get_or_init(std::time::Instant::now);
  u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
