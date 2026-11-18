//! The contract's async form (§4.12 "Embedding": "an async form over a host's reactor (R6: a readiness wait and a
//! timer over the client's driver)"; A-299): an instance whose verbs a host awaits on its own event loop, never a
//! thread blocked on a round trip.
//!
//! **What the host lends.** One wait ([`Reactor::wait`]): until the client's completion signal is readable, or a
//! timeout passes, whichever comes first. Every event loop has that (a readiness wait raced against a timer: kiln's
//! runtime, slates' own `rt`, an SDK's loop). Nothing here spawns, parks a thread or runs a loop of its own, so the
//! form adds no runtime (D-9).
//!
//! **How a call is awaited.** Over the state machine both SDKs drive (`slates_client::driver`, AUD-29-19, AUD-29-20,
//! A-232). A call is submitted and gets a ticket at once. Its future then loops:
//! - arm the completion signal;
//! - take what has landed;
//! - wait for the signal, or for the driver's next deadline, through the host's reactor;
//! - pump on a readable signal, tick on a timeout.
//!
//! A tick asks the daemon's liveness: a live daemon's call goes on waiting, and a gone one is reconnected and its call
//! sent again under its own id; past the reconnect budget the call fails `Unavailable`. So every call ends: answered,
//! refused, or failed typed.
//!
//! **One call at a time.** The verbs take `&mut self`, so the events a pump hands back are this call's. A host that
//! wants several calls at once holds several instances, each its own client and shard seat, as the SDKs' clients are.
//! A future dropped mid-call (a host's cancellation) cancels its ticket in the driver: queued, the call is never sent;
//! in flight, its reply is dropped when it comes. A cancellation leaks nothing and leaves the instance usable (CLAUDE.md
//! §3 "Cancellation safety by construction").
//!
//! **What it serves today.** Provisioning from every source, a snapshot, a status and a destroy. The paged verbs (the
//! list, a read, a landing's entries), the landings and merges, and the serving verbs (descriptors passed beside the
//! reply) stay on [`Instance`]. [`AsyncInstance::into_instance`] gives the instance back for them, and
//! [`AsyncInstance::new`] takes it again.

use std::future::Future;
use std::pin::Pin;

use slates_client::driver::{Begin, Driver, Event, Ticket};
use slates_client::{Client, ClientError, CreateSpec};
use slates_ipc::protocol::{ReplyBody, RequestBody, SizeClass, SnapshotId, VolumeId};

use crate::error::{Error, HostFault, Missing};
use crate::instance::Instance;
use crate::outcome::Work;
use crate::volume::{Provisioned, Source, VolumeSpec, VolumeStatus, status_of};

/// The client's completion signal a host waits on: a descriptor on Unix.
#[cfg(unix)]
pub type Completion = std::os::fd::RawFd;
/// The client's completion signal a host waits on: a socket on Windows.
#[cfg(windows)]
pub type Completion = std::os::windows::io::RawSocket;

/// What ended a [`Reactor::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
  /// The completion signal is readable: a reply, or the driver's own signal, landed.
  Readable,
  /// The timeout passed first: the driver's deadline is due.
  TimedOut,
}

/// A wait the host's reactor resolves.
pub type Waiting<'a> = Pin<Box<dyn Future<Output = std::io::Result<Woke>> + 'a>>;

/// What a host lends the async form (see the module doc): one wait.
pub trait Reactor {
  /// Waits until `completion` is readable or `timeout_ns` nanoseconds pass, whichever comes first; with no timeout,
  /// until it is readable. An error ends the call that waited, typed `Host`.
  fn wait(&self, completion: Completion, timeout_ns: Option<u64>) -> Waiting<'_>;
}

/// An instance whose verbs a host awaits (see the module doc).
pub struct AsyncInstance {
  /// The instance whose client carries the calls.
  instance: Instance,
  /// The state machine the calls run through.
  driver: Driver,
  /// The client's completion signal.
  completion: Completion,
}

/// Why an awaited round trip ended without a reply.
enum CallFault {
  /// The client or the daemon refused, or the channel failed.
  Client(ClientError),
  /// The host's reactor failed its wait.
  Reactor(std::io::Error),
}

impl AsyncInstance {
  /// The async form of `instance`: its client's completion signal turned on and a driver over it. Refused as the
  /// client refuses it.
  pub fn new(mut instance: Instance) -> Result<AsyncInstance, Error> {
    let completion = instance
      .client
      .enable_async_completion()
      .map_err(|e| instance.error(e))?;
    let driver = Driver::new(&instance.client);
    Ok(AsyncInstance {
      instance,
      driver,
      completion,
    })
  }

  /// The instance back, for the verbs that stay synchronous.
  pub fn into_instance(self) -> Instance {
    self.instance
  }

  /// The completion signal a host's reactor waits on.
  pub fn completion(&self) -> Completion {
    self.completion
  }

  /// Makes a volume from `spec`, as [`Instance::provision`] does.
  pub async fn provision(
    &mut self,
    reactor: &dyn Reactor,
    spec: &VolumeSpec,
  ) -> Result<Provisioned, Error> {
    let name = spec.name.clone();
    let (body, missing) = match &spec.source {
      Source::Overlay {
        folder,
        max_bytes,
        names,
      } => (
        create_body(&CreateSpec {
          name,
          size: SizeClass::Dynamic { max: *max_bytes },
          names: *names,
          require_locked: false,
          base: Some(crate::absolute_text(folder)?),
        }),
        None,
      ),
      Source::Empty { limit_bytes, names } => (
        create_body(&CreateSpec {
          name,
          size: SizeClass::Bounded {
            limit: *limit_bytes,
          },
          names: *names,
          require_locked: false,
          base: None,
        }),
        None,
      ),
      Source::Clone { volume, snapshot } => (
        RequestBody::Clone {
          volume: *volume,
          snapshot: *snapshot,
          name,
        },
        Some(Missing::Snapshot {
          volume: *volume,
          snapshot: *snapshot,
        }),
      ),
      Source::Green {
        base,
        require_evidence,
      } => (
        RequestBody::CreateGreen {
          name,
          require_evidence: *require_evidence,
          base: *base,
        },
        None,
      ),
      Source::Work { green } => (
        RequestBody::CreateWork {
          green: *green,
          name,
        },
        Some(Missing::Volume(*green)),
      ),
    };
    let reply = self.call(reactor, body, missing).await?;
    match (reply, &spec.source) {
      (
        ReplyBody::Created { id } | ReplyBody::Cloned { id } | ReplyBody::GreenCreated { id },
        _,
      ) => Ok(Provisioned::Volume(id)),
      (ReplyBody::WorkCreated { id, base }, Source::Work { green }) => Ok(Provisioned::Work {
        work: Work {
          volume: id,
          green: *green,
        },
        base,
      }),
      _ => Err(self.unexpected("provision")),
    }
  }

  /// Takes a snapshot of `volume`, as [`Instance::snapshot`] does.
  pub async fn snapshot(
    &mut self,
    reactor: &dyn Reactor,
    volume: VolumeId,
  ) -> Result<SnapshotId, Error> {
    match self
      .call(
        reactor,
        RequestBody::Snapshot { volume },
        Some(Missing::Volume(volume)),
      )
      .await?
    {
      ReplyBody::Snapshotted { id, .. } => Ok(id),
      _ => Err(self.unexpected("snapshot")),
    }
  }

  /// `volume`'s status, typed, as [`Instance::status`] answers it.
  pub async fn status(
    &mut self,
    reactor: &dyn Reactor,
    volume: VolumeId,
  ) -> Result<VolumeStatus, Error> {
    match self
      .call(
        reactor,
        RequestBody::Status { volume },
        Some(Missing::Volume(volume)),
      )
      .await?
    {
      ReplyBody::Status { report } => Ok(status_of(*report)),
      _ => Err(self.unexpected("status")),
    }
  }

  /// Destroys `volume`, as [`Instance::destroy`] does.
  pub async fn destroy(&mut self, reactor: &dyn Reactor, volume: VolumeId) -> Result<(), Error> {
    match self
      .call(
        reactor,
        RequestBody::Destroy { volume },
        Some(Missing::Volume(volume)),
      )
      .await?
    {
      ReplyBody::Destroyed => Ok(()),
      _ => Err(self.unexpected("destroy")),
    }
  }

  /// One awaited round trip of `body`, its failure typed against `subject`.
  async fn call(
    &mut self,
    reactor: &dyn Reactor,
    body: RequestBody,
    subject: Option<Missing>,
  ) -> Result<ReplyBody, Error> {
    match self.round_trip(reactor, body).await {
      Ok(reply) => Ok(reply),
      Err(CallFault::Client(error)) => Err(self.instance.fail(error, subject)),
      Err(CallFault::Reactor(error)) => Err(Error::Host(HostFault::Os {
        call: "the host's reactor wait",
        errno: error.raw_os_error(),
      })),
    }
  }

  /// Submits `body` and awaits its reply, a refusal typed as the client's own round trip types it.
  async fn round_trip(
    &mut self,
    reactor: &dyn Reactor,
    body: RequestBody,
  ) -> Result<ReplyBody, CallFault> {
    let begin: Begin = Box::new(move |client: &mut Client| client.begin(&body));
    let ticket = self
      .driver
      .submit(&mut self.instance.client, begin)
      .map_err(CallFault::Client)?;
    let mut call = InFlight {
      this: self,
      ticket,
      done: false,
    };
    call.answered(reactor).await
  }

  /// A reply the verb does not expect.
  fn unexpected(&self, verb: &'static str) -> Error {
    self.instance.error(ClientError::UnexpectedReply { verb })
  }
}

/// The body of a create, as the client's own create sends it.
fn create_body(spec: &CreateSpec) -> RequestBody {
  RequestBody::Create {
    name: spec.name.clone(),
    size: spec.size,
    names: spec.names,
    require_locked: spec.require_locked,
    base: spec.base.clone(),
  }
}

/// A call in flight: its ticket is cancelled in the driver if its future is dropped before the call ended.
struct InFlight<'a> {
  /// The instance carrying it.
  this: &'a mut AsyncInstance,
  /// Its ticket.
  ticket: Ticket,
  /// Whether it ended (answered or failed), so nothing is left to cancel.
  done: bool,
}

impl InFlight<'_> {
  /// Loops until the call's reply or failure: arm the signal, take what landed, wait through the host's reactor for
  /// the signal or the driver's next deadline, then pump or tick.
  async fn answered(&mut self, reactor: &dyn Reactor) -> Result<ReplyBody, CallFault> {
    loop {
      self
        .this
        .instance
        .client
        .arm_async()
        .map_err(CallFault::Client)?;
      let landed = self.this.driver.pump(&mut self.this.instance.client);
      if let Some(end) = self.end_in(landed) {
        return end;
      }
      let timeout = self.this.driver.next_wake_ns(&self.this.instance.client);
      let woke = reactor
        .wait(self.this.completion, timeout)
        .await
        .map_err(CallFault::Reactor)?;
      self.this.instance.client.drain_completion();
      let events = match woke {
        Woke::Readable => self.this.driver.pump(&mut self.this.instance.client),
        Woke::TimedOut => self.this.driver.tick(&mut self.this.instance.client),
      };
      if let Some(end) = self.end_in(events) {
        return end;
      }
    }
  }

  /// This call's end among `events`, if they hold it: its reply taken and its ticket finished, or its failure.
  fn end_in(&mut self, events: Vec<Event>) -> Option<Result<ReplyBody, CallFault>> {
    for event in events {
      match event {
        Event::Ready { ticket, word } if ticket == self.ticket => {
          let reply = self.this.instance.client.poll_reply_word(word);
          self.this.driver.finish(ticket);
          self.done = true;
          return Some(match reply {
            Ok(Some(ReplyBody::Refused { refusal })) => {
              Err(CallFault::Client(ClientError::Refused(refusal)))
            }
            Ok(Some(body)) => Ok(body),
            Ok(None) => Err(CallFault::Client(ClientError::UnexpectedReply {
              verb: "a reply the driver reported ready",
            })),
            Err(error) => Err(CallFault::Client(error)),
          });
        }
        Event::Failed { ticket, error } if ticket == self.ticket => {
          self.done = true;
          return Some(Err(CallFault::Client(error)));
        }
        // One call at a time: no other ticket is this instance's.
        Event::Ready { .. } | Event::Failed { .. } => {}
      }
    }
    None
  }
}

impl Drop for InFlight<'_> {
  /// A call whose future was dropped before it ended is cancelled: queued, it is never sent; sent, its reply is
  /// dropped when it comes.
  fn drop(&mut self) {
    if !self.done {
      self
        .this
        .driver
        .cancel(&mut self.this.instance.client, self.ticket);
    }
  }
}
