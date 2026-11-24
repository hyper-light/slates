//! git's upload-pack behind the SSH edge (A-651 part 2; §4.17.2 "A fetch" and "Transports" item 2): A-640 part 2's one
//! entry point, `slates_git::upload::serve`, driven over an SSH session's standard input and output, the repository's
//! store lent a step at a time by the HTTP edge's [`StoreLender`] (A-625 part 3), so both edges reach a store one way.
//!
//! **The session.** Over SSH, upload-pack speaks first: the advertisement (`Asked::Advertise`) is written as soon as the
//! command starts. Then each request is read whole as the client's bytes arrive: `v2::request::whole_len` measures the
//! first whole request the held input begins with (A-640 part 2: "or what `request::whole_len` measured of an SSH
//! session's input"), which is consumed, served (`Asked::Request`) and answered. A flush alone ends the session
//! (`Answer::ends_session`), and the command exits 0. A request upload-pack refuses is answered in git's words (`ERR`,
//! or band 3 under `sideband-all`) and then ends the session with git's `die` status, 128, as git's own server dies on
//! such a request (`serve.c` at `e1ec09b2`: "invalid command", "unknown capability") \[C\]; bytes that are no pkt-lines are
//! handed to upload-pack whole, which answers them so. The end of the client's input before a request's first packet is
//! whole ends the session, 0; its end inside a request is refused in git's words with `die`'s status, as git's reader is
//! gentle on the end of its input only until it has peeked a request's first packet (`serve.c` `process_request` clears
//! `PACKET_READ_GENTLE_ON_EOF` then, and `pkt-line.c` `get_packet_data` dies at a later short read) \[C\].
//!
//! **Protocols.** A session is served at protocol v2, which git's client speaks over SSH by default (slates-git's
//! `upload` module doc: since git 2.26), asking for it through `GIT_PROTOCOL`, the session channel's one `env`. A
//! session at v0 or v1 is refused at its start, before any advertisement: an `ERR` line naming v2, the way upload-pack
//! refuses a request, then `die`'s status. slates-git answers those protocols statelessly, a whole request at a time as
//! smart HTTP sends them (A-640 part 3), but a v0 client over SSH negotiates in rounds on one stream, sending its wants
//! once and its haves round by round, so a stateless answer to its second round would have lost its wants. That session
//! is owed (GAPS).
//!
//! **Bounds.** Each request's answer is admitted what the repository's shard could admit when it began (the lender's
//! `room`, A-713; A-855: never one region, since its walk's tables grow with the repository), as the HTTP edge admits
//! one, and answered in turns as it does (A-745:
//! a fetch's pack's objects chosen within the shard's step quantum as the answer is written). An answer is written
//! one whole pkt-line's room a step (`pktline::MAX_LEN`, the format's own limit), and the edge steps the command again only
//! once that step's bytes have gone to the channel, so the output held is a packet at most. The input held is at most the
//! channel's window, which the session refuses to exceed; what a request consumed is given back to it.
//!
//! **git's keepalive** (A-722's on the fetch, as the HTTP edge's `vcs_http::git::upload`). An answer's step may write
//! nothing: its pack's objects still being chosen after the `packfile` section's head is written, or its source framing
//! nothing new. Once the answer has been silent for the keepalive interval (git's own five seconds,
//! `crate::config::GIT_KEEPALIVE_NS`, unless the site is given another), the step writes git's keepalive where the answer
//! may carry one (`Answer::write_keepalive`: inside a banded pack's section, as git's `create_pack_file` writes `0005\1`
//! while `pack-objects` is silent). Between requests the session waits on its client, and writes nothing.
//!
//! **The store.** A repository whose store the lender does not hold is not served (the site's not-served words). A
//! store that goes away mid-session ends it in the same words. A-662 holds each repository's store on its owner shard,
//! and the daemon's lender lends it from that map (`crate::vcs_store::upload::ShardStores`, A-682): each request's step
//! with the refs the session's principal may read, each step after it with the objects alone.

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::Admission;
use slates_git::http::Version;
use slates_git::pktline::{self, MAX_LEN};
use slates_git::upload::{self, Answer, Asked, PackSource, Served, Written};
use slates_git::v2::request::whole_len;

use super::site::{ClientGone, Command, GIT_DIE_STATUS, Next, Refused, Stepped};
use crate::vcs_http::git::StoreLender;

/// Format: git's error packet's prefix (gitprotocol-pack: an `ERR` pkt-line, whose reason the client shows).
const ERR: &[u8] = b"ERR ";

/// Why a session at protocol v0 or v1 is refused (the module doc's protocols), as its client shows it.
const VERSION_REFUSED: &[u8] =
  b"upload-pack serves protocol v2 alone over SSH: ask for it with git -c protocol.version=2";

/// What the session does once the answer being written has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
  /// Read the next request.
  Next,
  /// End the session: the request was a flush alone.
  End,
  /// End it as git's server dies: the request was refused.
  Die,
}

/// The words a session whose store is not held here ends with.
fn unheld() -> Refused {
  Refused {
    stderr: b"fatal: git-upload-pack is not served yet\n".to_vec(),
    exit_status: GIT_DIE_STATUS,
  }
}

/// The words git's reader dies with when its input ends inside a request (`pkt-line.c` `get_packet_data`).
fn hung_up() -> Refused {
  Refused {
    stderr: b"fatal: the remote end hung up unexpectedly\n".to_vec(),
    exit_status: GIT_DIE_STATUS,
  }
}

/// One upload-pack session over SSH (the module doc).
pub(crate) struct UploadCommand<L: StoreLender> {
  /// Where the store is lent.
  lender: L,
  /// The repository.
  repository: RepositoryId,
  /// The session's principal: the refs each request's step is lent are the ones it may read.
  reader: Principal,
  /// The protocol version the client asked for (`GIT_PROTOCOL`).
  version: Version,
  /// The client's input held, not yet consumed.
  input: Vec<u8>,
  /// Whether the client's input has ended.
  input_ended: bool,
  /// What the command wrote that the channel has not taken.
  out: Vec<u8>,
  /// Whether the advertisement was made.
  advertised: bool,
  /// Whether the store's versions were readied for a fetch (A-718): once, before the session's first fetch.
  readied: bool,
  /// The answer being written.
  answer: Option<Answer<<L::Packs as PackSource>::Pack>>,
  /// The generation a request's answer began on, held until the answer is written (A-745 rule 4).
  generation: Option<L::Hold>,
  /// The admission of the request it answers, lent to each step of its writing (A-653 continued (4)).
  admission: Admission,
  /// What follows it.
  after: After,
  /// How long an answer may write nothing before git's keepalive is written (the module doc).
  keepalive_ns: u64,
  /// When the session last wrote.
  last_written_ns: u64,
  /// The windows its answer brought back and has not let go (A-864 piece 3).
  resident: crate::vcs_http::git::upload::Resident,
  /// The request its answer answers, kept to make it again (A-864's readers).
  request: Vec<u8>,
  /// The room its answer was made within.
  admitted: u64,
  /// The let-go window its answer was last held for, until it writes again (A-864's readers' progress).
  last_away: Option<slates_git::store::packed::Waited>,
  /// What its answers have learned of the link (A-859 part A): nothing until a write of an answer waits for the client.
  pacing: crate::vcs_store::upload::Pacing,
  /// When its answer began to wait for room (A-855), until the room grows or its patience runs out.
  held_since: Option<u64>,
}

impl<L: StoreLender> UploadCommand<L> {
  /// A session of `reader` over the store of `repository` that `lender` lends, at protocol `version`, each request's
  /// answer admitted what its stores' shard could admit when it began (A-855); git's keepalive written after
  /// `keepalive_ns` of a silent answer.
  pub(crate) fn new(
    lender: L,
    (repository, reader): (RepositoryId, Principal),
    version: Version,
    keepalive_ns: u64,
  ) -> Self {
    Self {
      lender,
      repository,
      reader,
      version,
      input: Vec::new(),
      input_ended: false,
      out: Vec::new(),
      advertised: false,
      readied: false,
      answer: None,
      generation: None,
      admission: Admission::new(0),
      after: After::Next,
      keepalive_ns,
      last_written_ns: slates_rt::futures::now_ns(),
      resident: Vec::new(),
      request: Vec::new(),
      admitted: 0,
      last_away: None,
      pacing: crate::vcs_store::upload::Pacing::default(),
      held_since: None,
    }
  }

  /// Answers `asked` over the lent store in turns (A-745: a fetch's pack's objects chosen as the answer is written),
  /// within a fresh admission of what its repository's shard could admit now (A-855), kept for the
  /// answer's writing; refused when the store is no longer held.
  /// Answers `asked` as [`UploadCommand::serve_once`] does, made again while a read of it met a let-go window, once the
  /// window is back (A-864's readers; main 01a20182's r52): nothing of it is written yet. The same window met twice with
  /// none brought back between ends the retries, the refusal answered.
  async fn serve(&mut self, asked: Option<&[u8]>) -> Result<(), Refused> {
    let mut last = None;
    loop {
      let away = match asked {
        None => self.serve_once(Asked::Advertise)?,
        Some(request) => self.serve_once(Asked::Request(request))?,
      };
      match away {
        Some(waited)
          if last != Some(waited)
            && self
              .lender
              .bring_back_tracked(waited, &mut self.resident)
              .await =>
        {
          last = Some(waited)
        }
        _ => return Ok(()),
      }
    }
  }

  /// A turn of an answer held for a let-go window its read met before its pack (A-864's readers): the window brought back
  /// and tracked in `self.resident` so it is released when the fetch ends; resumed in place when paused in `choosing`,
  /// otherwise rebuilt over the same generation passing over what it wrote (A-855's rule).
  async fn read_through_held(
    &mut self,
    waited: slates_git::store::packed::Waited,
  ) -> Result<(), Refused> {
    let Some(made) = self.answer.as_mut() else {
      return Ok(());
    };
    if self.last_away == Some(waited)
      || self.generation.is_none()
      || !self
        .lender
        .bring_back_tracked(waited, &mut self.resident)
        .await
    {
      made.give_up();
      return Ok(());
    }
    self.last_away = Some(waited);
    if made.resume_away() {
      return Ok(());
    }
    let skip = made.written();
    let mut again_room = Admission::new(self.admitted);
    let request = self.request.clone();
    let version = self.version;
    let Some(held) = self.generation.as_ref() else {
      return Ok(());
    };
    let again = self
      .lender
      .lend_held(held, |served, packs| {
        upload::serve_in_turns(
          served,
          version,
          Asked::Request(&request),
          packs,
          &mut again_room,
        )
        .holding()
        .skip_written(skip)
      })
      .ok_or_else(unheld)?;
    self.answer = Some(again);
    self.admission = again_room;
    Ok(())
  }

  /// A turn of an answer held past its room (A-855; the HTTP edge's `Upload::wait_for_room`): where its repository's shard
  /// now gives more room than it was admitted, the request answered again within it over the same generation, passing
  /// over what it wrote ([`Answer::skip_written`]), counted `vcs_http.git.answered_again`; else a wait of a keepalive's
  /// interval at most, until [`crate::vcs_share::wait::PATIENCE_NS`] has passed, when it gives up and its refusal is
  /// answered as it was before.
  async fn wait_for_room(&mut self) -> Result<(), Refused> {
    let Self {
      lender,
      repository,
      version,
      admission,
      request,
      answer,
      generation,
      keepalive_ns,
      admitted,
      held_since,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Ok(());
    };
    let now = slates_rt::futures::now_ns();
    let room = lender.room(*repository).unwrap_or(0);
    if room > *admitted
      && let Some(held) = generation.as_ref()
    {
      let skip = made.written();
      let mut wider = Admission::new(room);
      let again = lender
        .lend_held(held, |served, packs| {
          upload::serve_in_turns(
            served,
            *version,
            Asked::Request(request.as_slice()),
            packs,
            &mut wider,
          )
          .holding()
          .skip_written(skip)
        })
        .ok_or_else(unheld)?;
      *made = again;
      *admission = wider;
      *admitted = room;
      *held_since = None;
      let _ = crate::state::with_state_counted(|state| {
        state.count(crate::vcs_http::git::upload::ANSWERED_AGAIN, 1);
      });
      return Ok(());
    }
    let patience_ns = crate::vcs_share::wait::PATIENCE_NS;
    let since = *held_since.get_or_insert(now);
    let waited = now.saturating_sub(since);
    if waited >= patience_ns {
      made.give_up();
      return Ok(());
    }
    // A sleep the runtime refuses (no timer here) ends the wait early: the next turn asks again.
    let _ =
      slates_rt::futures::sleep((*keepalive_ns).min(patience_ns.saturating_sub(waited))).await;
    Ok(())
  }

  /// Answers `asked` once over the lent store (the module doc): the let-go window a read of its refused answer met, if
  /// one did.
  fn serve_once(
    &mut self,
    asked: Asked<'_>,
  ) -> Result<Option<slates_git::store::packed::Waited>, Refused> {
    let Self {
      lender,
      repository,
      reader,
      version,
      answer,
      generation,
      admission,
      after,
      admitted,
      request: kept,
      ..
    } = self;
    // A-855: an answer is admitted what its repository's shard admits it (A-724's share), never one region: its walk's
    // tables grow with the repository's objects and wants (A-745 rule 1), past a region's room at git/git's 1,019 tags.
    let shard = lender.room(*repository).ok_or_else(unheld)?;
    *admission = Admission::new(shard);
    *admitted = shard;
    let answered = |served: &Served<'_>, packs: &mut L::Packs, admission: &mut Admission| {
      let made = upload::serve_in_turns(served, *version, asked, packs, admission);
      // A-855: a fetch's negotiation run past its room is held, not answered, until the room grows (`wait_for_room`).
      let made = match asked {
        Asked::Request(_) => made.holding(),
        Asked::Advertise => made,
      };
      let away = made.refusal().and(served.repository.waiting());
      (made, away)
    };
    let (made, away) = match asked {
      Asked::Advertise => lender.lend((*repository, reader), |served, packs| {
        answered(served, packs, admission)
      }),
      Asked::Request(request) => {
        let held = lender.hold(*repository, reader).ok_or_else(unheld)?;
        let made = lender.lend_held(&held, |served, packs| answered(served, packs, admission));
        *generation = Some(held);
        *kept = request.to_vec();
        made
      }
    }
    .ok_or_else(unheld)?;
    *after = if made.ends_session() {
      After::End
    } else {
      After::Next
    };
    *answer = Some(made);
    Ok(away)
  }

  /// A turn of the answer being written ([`Command::step`]): the windows its next write reads brought back first, then
  /// one step of the answer ([`UploadCommand::write_answer`]), and, once it is written, every window it brought back
  /// let go.
  async fn answer_turn(&mut self) -> Result<Stepped, Refused> {
    // A-864 piece 2: the windows the next turn reads brought back first, off the turn.
    if let Some(pack) = self.answer.as_ref().and_then(|made| made.pack_source()) {
      let reads = L::pack_reads(pack);
      if reads.next.is_some() {
        self.lender.let_go_behind(&mut self.resident, reads.next);
      }
      self.lender.read_ahead(reads, &mut self.resident).await;
    }
    let next = self.write_answer()?;
    // A-864 piece 3: an answer written lets go every window it brought back.
    if self.answer.is_none() {
      self.lender.let_go_behind(&mut self.resident, None);
    }
    Ok(Stepped { consumed: 0, next })
  }

  /// Writes one step of the answer, a whole pkt-line's room at most; what follows once it has ended.
  fn write_answer(&mut self) -> Result<Next, Refused> {
    let Self {
      lender,
      repository,
      answer,
      generation,
      admission,
      out,
      after,
      keepalive_ns,
      last_written_ns,
      last_away,
      ..
    } = self;
    let Some(made) = answer.as_mut() else {
      return Ok(Next::More);
    };
    let before = out.len();
    let step = |served: &Served<'_>, packs: &mut L::Packs| {
      let mut spent = packs.turn();
      made.write_within(served, packs, admission, out, MAX_LEN, &mut spent)
    };
    let written = match generation.as_ref() {
      Some(held) => lender.lend_held(held, step),
      None => lender.lend_objects(*repository, step),
    }
    .ok_or_else(unheld)?;
    if let Some(held) = generation.as_ref() {
      lender.wrote(
        held,
        u64::try_from(out.len().saturating_sub(before)).unwrap_or(u64::MAX),
      );
    }
    let now = slates_rt::futures::now_ns();
    if out.len() > before {
      *last_away = None;
    }
    if out.len() > before
      || (now.saturating_sub(*last_written_ns) >= *keepalive_ns && made.write_keepalive(out))
    {
      *last_written_ns = now;
    }
    // A-864's readers (main 01a20182's r227: the re-lift's SSH fetch of git/git ended mid-answer): an answer held for a
    // let-go window its pack's choice met, its sections written, is kept, not dropped as written; the next step brings
    // the window back and makes it again past what it wrote (`read_through_held`), as the HTTP edge does.
    // A-855: an answer held for room, its keepalive written meanwhile, is kept for `wait_for_room`.
    if written == Written::More || made.held_away().is_some() || made.held().is_some() {
      return Ok(Next::More);
    }
    // A refusal is the answer's to say, as it is made or as it is written (a pack's choice refused midway): its session
    // dies as git's upload-pack dies.
    if made.refusal().is_some() && matches!(after, After::Next) {
      *after = After::Die;
    }
    *answer = None;
    // The answer is written: its generation is given back.
    *generation = None;
    Ok(match after {
      After::Next => Next::More,
      After::End => Next::Exited(0),
      After::Die => Next::Exited(GIT_DIE_STATUS),
    })
  }

  /// Refuses a session at protocol v0 or v1 at its start (the module doc's protocols): one `ERR` packet, written as
  /// upload-pack writes a refusal, then git's `die` status. A line too long for a packet, which these words never are,
  /// is refused in the same words on standard error.
  fn refuse_version(&mut self) -> Result<Stepped, Refused> {
    let line = [ERR, VERSION_REFUSED].concat();
    pktline::write(pktline::Packet::Data(&line), &mut self.out).map_err(|_too_long| Refused {
      stderr: [b"fatal: ".as_slice(), VERSION_REFUSED, b"\n"].concat(),
      exit_status: GIT_DIE_STATUS,
    })?;
    Ok(Stepped {
      consumed: 0,
      next: Next::Exited(GIT_DIE_STATUS),
    })
  }

  /// Serves the next whole request the input begins with: what it consumed, and what follows.
  async fn next_request(&mut self) -> Result<Stepped, Refused> {
    let length = match whole_len(&self.input) {
      Ok(Some(length)) => length.min(self.input.len()),
      Ok(None) if self.input_ended => return self.ended(),
      Ok(None) => {
        return Ok(Stepped {
          consumed: 0,
          next: Next::Input,
        });
      }
      // No pkt-lines: upload-pack answers them as it refuses them, and the session ends.
      Err(_) => self.input.len(),
    };
    let request: Vec<u8> = self.input.drain(..length).collect();
    // A-718: the store's versions derived and encoded once, before the session's first fetch is answered; a listing reads
    // neither (goal 93, r405: `git ls-remote` after a whole push waited 29 to 54 s for them).
    if !self.readied && crate::vcs_http::git::upload::fetches(self.version, &request) {
      self.readied = true;
      self.lender.encode(self.repository).await;
    }
    self.serve(Some(&request)).await?;
    Ok(Stepped {
      consumed: length,
      next: Next::More,
    })
  }

  /// The client's input ended before a whole request (the module doc): the session ended, 0, while the request's first
  /// packet is short; refused with git's `die` status once it is whole, in its reader's words. (git judges each line as
  /// it reads it, so where a line before the end is one it refuses, it dies in that refusal's words instead, as at a
  /// delimiter before any command: "no command requested"; the status is the same.) `whole_len` has read the first
  /// packet's length already, so it is whole or short here.
  fn ended(&self) -> Result<Stepped, Refused> {
    if matches!(pktline::read(&self.input, 0), Ok(Some(_))) {
      return Err(hung_up());
    }
    Ok(Stepped {
      consumed: 0,
      next: Next::Exited(0),
    })
  }
}

impl<L: StoreLender> Drop for UploadCommand<L> {
  fn drop(&mut self) {
    if !self.resident.is_empty() {
      self.lender.let_go_behind(&mut self.resident, None);
    }
  }
}

impl<L: StoreLender> Command for UploadCommand<L> {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    self.input.extend_from_slice(run);
    Ok(())
  }

  fn input_end(&mut self) {
    self.input_ended = true;
  }

  async fn step(&mut self, _gone: ClientGone<'_>) -> Result<Stepped, Refused> {
    // A-864's readers: an answer held for a let-go window is made again once it is back.
    if let Some(waited) = self.answer.as_ref().and_then(|made| made.held_away()) {
      self.read_through_held(waited).await?;
      return Ok(Stepped {
        consumed: 0,
        next: Next::More,
      });
    }
    // A-855: an answer held for room waits for it, a keepalive's interval at a time, then writes (git's keepalive while
    // it waits; its answer once made again within the wider room, or its refusal once its patience runs out).
    if self
      .answer
      .as_ref()
      .is_some_and(|made| made.held().is_some())
    {
      self.wait_for_room().await?;
    }
    if self.answer.is_some() {
      return self.answer_turn().await;
    }
    if !self.advertised {
      self.advertised = true;
      if !matches!(self.version, Version::V2) {
        return self.refuse_version();
      }
      self.serve(None).await?;
      return Ok(Stepped {
        consumed: 0,
        next: Next::More,
      });
    }
    self.next_request().await
  }

  fn output(&self) -> (&[u8], &[u8]) {
    (&[], &self.out)
  }

  fn sent(&mut self, _stderr: usize, stdout: usize) {
    self.out.drain(..stdout.min(self.out.len()));
  }

  /// A write of the connection while an answer is written (A-859 part A on SSH, as the HTTP edge's `Upload::sent`):
  /// learned into the shard's link price once a write of an answer has waited for the client; the advertisement's and a
  /// negotiation's teach nothing, their bytes too few to fill a socket.
  fn paced(&mut self, sent: crate::vcs_store::upload::Sent) {
    if self.answer.is_none() {
      return;
    }
    if let Some(taught) = self.pacing.sent(sent) {
      self.lender.paced(taught);
    }
  }
}

#[cfg(test)]
#[path = "tests/git.rs"]
mod tests;
