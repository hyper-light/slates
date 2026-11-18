//! A merge verdict's conflict windows at the scale a real change reaches (§4.16, D-27; A-267): a submit's or a rebase's
//! reply carries the windows one reply holds and counts the rest, the work keeps the verdict, and `Conflicts` reads
//! every window a page at a time, so the client hands its caller every window. Before A-267 a verdict was one list in
//! one reply, and past what a reply chunk holds the daemon answered `BadRequest { reason: "reply too large for the bulk
//! area" }` in its place: a work in conflict could learn nothing of what to resolve. A work whose name is off its
//! green's shard is judged on the green's owner and keeps its verdict on its own shard (A-247), where its reads go.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

use std::collections::BTreeMap;

use slates_client::{Client, ClientError, Rebased, Refusal, Submitted, VolumeId};
use slates_ipc::protocol::{MergeWindow, ReplyBody, RequestBody, Verification};
use slates_server::verbs::{owner_of, owner_of_name};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::connect;

/// Shape: one shard, the owner of the green and its works.
const TEST_SHARDS: u16 = 1;
/// Shape: enough shards that a work's name can hash off its green's (A-247).
const MANY_SHARDS: u16 = 8;
/// Shape: the files each work rewrites, one conflict window each: several replies' worth at these names.
const MANY: usize = 400;
/// Format: the counter a work's shard moves for each verb it asked its green's owner to serve (A-247).
const ASKED_THE_OWNER: &str = "merge.work.asked_owner";

fn daemon(tag: &str) -> (Daemon, String) {
  daemon_of(tag, TEST_SHARDS)
}

fn daemon_of(tag: &str, shards: u16) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-verdict-pages-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(shards));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-verdict-pages-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The `n`th file: long enough that a few dozen windows fill a reply.
fn file(n: usize) -> String {
  format!("a-file-named-long-enough-to-fill-a-reply-sooner-{n:05}.txt")
}

/// A green whose seed merged [`MANY`] files, and two works over it that each rewrote the first four bytes of every
/// file, the first merged: the second conflicts in every file. The green and the second work.
fn conflicting(client: &mut Client) -> (VolumeId, VolumeId) {
  conflicting_named(
    client,
    |client| client.create_green("green", false).unwrap(),
    |_| "second".to_owned(),
  )
}

/// [`conflicting`], the second work named by `second` from the green.
fn conflicting_named(
  client: &mut Client,
  make_green: impl FnOnce(&mut Client) -> VolumeId,
  second: impl Fn(VolumeId) -> String,
) -> (VolumeId, VolumeId) {
  let green = make_green(client);
  let (seed, _) = client.create_work(green, "seed").unwrap();
  for n in 0..MANY {
    client
      .edit(seed, &file(n), 0, 0, b"aaaa, the seed's")
      .unwrap();
  }
  assert!(matches!(client.submit(seed), Ok(Submitted::Accepted(_))));
  let (first, _) = client.create_work(green, "first").unwrap();
  let (second, _) = client.create_work(green, &second(green)).unwrap();
  for n in 0..MANY {
    client.edit(first, &file(n), 0, 4, b"bbbb").unwrap();
    client.edit(second, &file(n), 0, 4, b"cccc").unwrap();
  }
  assert!(matches!(client.submit(first), Ok(Submitted::Accepted(_))));
  (green, second)
}

/// Every window of `windows` once a file, and every file once: each window names a file of the conflict.
fn assert_every_file_once(windows: &[MergeWindow]) {
  let mut by_file: BTreeMap<&str, usize> = BTreeMap::new();
  for window in windows {
    *by_file.entry(window.path.as_str()).or_default() += 1;
  }
  assert_eq!(by_file.len(), MANY, "every file conflicts");
  assert!(
    (0..MANY).all(|n| by_file.get(file(n).as_str()) == Some(&1)),
    "each file once"
  );
}

/// Reads a work's kept verdict page by page through the wire, from `from`: every window, and the pages taken.
fn walk(
  client: &mut Client,
  work: VolumeId,
  verdict: u64,
  from: usize,
) -> (Vec<MergeWindow>, usize) {
  let mut windows = Vec::new();
  let mut from = u32::try_from(from).unwrap();
  let mut pages = 0;
  loop {
    let reply = client.call(&RequestBody::Conflicts {
      work,
      verdict,
      from,
    });
    let Ok(ReplyBody::Conflicts {
      windows: page,
      next,
    }) = reply
    else {
      panic!("a page of the verdict: {reply:?}");
    };
    pages += 1;
    windows.extend(page);
    match next {
      None => return (windows, pages),
      Some(next) => from = next,
    }
  }
}

/// A-267 (§4.16, D-27). Do: conflict a work in 400 files, submit it through the wire, read the rest of its verdict a
/// page at a time, then submit it through the client. Expect: the reply carries some windows and counts the rest (the
/// count above zero), naming its verdict; the pages from where the reply stopped hold the rest, more than one page of
/// them; the two together name every file once; and the client's submit hands back the same windows. Before A-267 the
/// submit was refused "reply too large for the bulk area".
#[test]
fn a_submits_verdict_past_one_reply_is_read_whole() {
  let (daemon, instance) = daemon("submit");
  let mut client = connect(&instance);
  let (_, work) = conflicting(&mut client);
  let submitted = client.call(&RequestBody::Submit {
    work,
    evidence: Vec::new(),
  });
  let Ok(ReplyBody::Submitted {
    version: None,
    conflicts,
    conflicts_elided,
    verdict,
  }) = submitted
  else {
    panic!("the submit's conflict is answered: {submitted:?}");
  };
  assert!(conflicts_elided > 0, "the test outgrows one reply");
  assert_ne!(verdict, 0, "the reply names its verdict");
  let (rest, pages) = walk(&mut client, work, verdict, conflicts.len());
  assert!(pages > 1, "the rest takes more than one page");
  assert_eq!(
    rest.len(),
    conflicts_elided as usize,
    "the count the reply gave"
  );
  let every: Vec<MergeWindow> = conflicts.into_iter().chain(rest).collect();
  assert_every_file_once(&every);
  let Ok(Submitted::Conflict(through_the_client)) = client.submit(work) else {
    panic!("the client's submit conflicts");
  };
  assert_eq!(
    through_the_client, every,
    "the client hands back every window"
  );
  drop(client);
  daemon.stop();
}

/// A-267 (§4.16 "Rebase, the only corrective path"). Do: conflict a work in 400 files and rebase it through the client,
/// synchronously and through the asynchronous begin and poll. Expect: each answers every window, every file once, the
/// two the same. Before A-267 the rebase was refused "reply too large for the bulk area".
#[test]
fn a_rebases_verdict_past_one_reply_is_read_whole() {
  let (daemon, instance) = daemon("rebase");
  let mut client = connect(&instance);
  let (_, work) = conflicting(&mut client);
  let Ok(Rebased::Conflict(windows)) = client.rebase(work) else {
    panic!("the rebase conflicts");
  };
  assert_every_file_once(&windows);
  let id = client.rebase_begin(work).unwrap();
  let asynchronous = loop {
    if let Some(outcome) = client.rebase_poll(id.word()).unwrap() {
      break outcome;
    }
    std::hint::spin_loop();
  };
  assert_eq!(asynchronous, Rebased::Conflict(windows));
  drop(client);
  daemon.stop();
}

/// A-267. Do: conflict a work in 400 files, submit it through the wire, then submit it again, and read the first
/// verdict's windows. Expect: refused `VerdictReplaced` naming the work — the second submit replaced the verdict — and
/// the second verdict's read from its reply's end answers. A read never answers one verdict's windows for another's.
#[test]
fn a_replaced_verdict_is_refused_typed() {
  let (daemon, instance) = daemon("replaced");
  let mut client = connect(&instance);
  let (_, work) = conflicting(&mut client);
  let verdict_of = |client: &mut Client| {
    let reply = client.call(&RequestBody::Submit {
      work,
      evidence: Vec::new(),
    });
    let Ok(ReplyBody::Submitted {
      conflicts, verdict, ..
    }) = reply
    else {
      panic!("a verdict: {reply:?}");
    };
    (conflicts.len(), verdict)
  };
  let (_, first) = verdict_of(&mut client);
  let (carried, second) = verdict_of(&mut client);
  assert_ne!(first, second, "each verdict its own");
  let stale = client.call(&RequestBody::Conflicts {
    work,
    verdict: first,
    from: 0,
  });
  assert!(
    matches!(
      stale,
      Err(ClientError::Refused(Refusal::VerdictReplaced { work: named })) if named == work
    ),
    "{stale:?}"
  );
  let (rest, _) = walk(&mut client, work, second, carried);
  assert_eq!(rest.len() + carried, MANY, "the second verdict reads whole");
  drop(client);
  daemon.stop();
}

/// A green made by a client of another shard than `client`'s on an eight-shard daemon (A-400: a work lives on the shard
/// of the client that makes it, a green on its maker's), so `client`'s works are off its shard.
fn green_off(client: &Client, instance: &str) -> VolumeId {
  let here = client.client_id() % u32::from(MANY_SHARDS);
  loop {
    let mut maker = connect(instance);
    if maker.client_id() % u32::from(MANY_SHARDS) != here {
      return maker.create_green("green", false).unwrap();
    }
  }
}

/// A name of `prefix` that hashes to a partition other than `green`'s: a work of that name made on another shard holds
/// it there by an alias (A-400).
fn off_owner_name(green: VolumeId, prefix: &str) -> String {
  (0u32..)
    .map(|suffix| format!("{prefix}-{suffix}"))
    .find(|name| owner_of_name(name, usize::from(MANY_SHARDS)) != owner_of(green))
    .unwrap()
}

fn asked_of_the_owner(daemon: &Daemon) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(ASKED_THE_OWNER)
    .copied()
    .unwrap_or(0)
}

/// A-267 under A-247 (§4.16: a green's works are served where they live; only the verdict is the green's). Do: on eight
/// shards, conflict a work off its green's shard (its client's, A-400) in 400 files, submit it through the wire and read the
/// rest of its verdict a page at a time, then rebase it through the client. Expect: the work's shard asked the green's
/// owner; the reply carries some windows and counts the rest under a verdict id; the pages, read from the work's shard,
/// hold the rest; the two name every file once; and the rebase answers every window. Merged as text, the branch kept a
/// verdict where it was judged, on the owner, which holds no work on another shard: the reply named verdict 0, and the
/// rest could not be read.
#[test]
fn a_verdict_judged_on_the_greens_shard_is_kept_with_its_work_on_another() {
  let (daemon, instance) = daemon_of("off-owner", MANY_SHARDS);
  let mut client = connect(&instance);
  let (_, work) = conflicting_named(
    &mut client,
    |client| green_off(client, &instance),
    |green| off_owner_name(green, "second"),
  );
  let asked_before = asked_of_the_owner(&daemon);
  let submitted = client.call(&RequestBody::Submit {
    work,
    evidence: Vec::new(),
  });
  assert!(
    asked_of_the_owner(&daemon) > asked_before,
    "the verdict was asked of the green's owner"
  );
  let Ok(ReplyBody::Submitted {
    version: None,
    conflicts,
    conflicts_elided,
    verdict,
  }) = submitted
  else {
    panic!("the submit's conflict is answered: {submitted:?}");
  };
  assert!(conflicts_elided > 0, "the test outgrows one reply");
  assert_ne!(verdict, 0, "the work's shard kept the verdict");
  let (rest, _) = walk(&mut client, work, verdict, conflicts.len());
  assert_eq!(rest.len(), conflicts_elided as usize);
  let every: Vec<MergeWindow> = conflicts.into_iter().chain(rest).collect();
  assert_every_file_once(&every);
  let Ok(Rebased::Conflict(windows)) = client.rebase(work) else {
    panic!("the rebase conflicts");
  };
  assert_every_file_once(&windows);
  drop(client);
  daemon.stop();
}

/// A-267 with A-321 (signed work). Do: conflict a work in 400 files and submit it signed, with a signature no key made,
/// through the wire, read the rest of its verdict, then submit it signed through the client. Expect: `SubmittedSigned`,
/// its signature not verified, carrying some windows and counting the rest under a verdict id; the pages hold the rest,
/// every file once; and the client's signed submit hands back every window and the same verification. Before, a signed
/// verdict carried every window in one reply, refused "reply too large for the bulk area" past what a chunk holds.
#[test]
fn a_signed_verdict_past_one_reply_is_read_whole() {
  let (daemon, instance) = daemon("signed");
  let mut client = connect(&instance);
  let (_, work) = conflicting(&mut client);
  let signature = b"a signature no key made".to_vec();
  let reply = client.call(&RequestBody::SubmitSigned {
    work,
    evidence: Vec::new(),
    signature: signature.clone(),
  });
  let Ok(ReplyBody::SubmittedSigned {
    version: None,
    conflicts,
    verification,
    conflicts_elided,
    verdict,
  }) = reply
  else {
    panic!("the signed submit's conflict is answered: {reply:?}");
  };
  assert!(
    !matches!(verification, Verification::Verified { .. }),
    "{verification:?}"
  );
  assert!(conflicts_elided > 0, "the test outgrows one reply");
  assert_ne!(verdict, 0, "the reply names its verdict");
  let (rest, _) = walk(&mut client, work, verdict, conflicts.len());
  assert_eq!(rest.len(), conflicts_elided as usize);
  let every: Vec<MergeWindow> = conflicts.into_iter().chain(rest).collect();
  assert_every_file_once(&every);
  let Ok((Submitted::Conflict(through_the_client), judged)) =
    client.submit_with_signature(work, &[], Some(&signature))
  else {
    panic!("the client's signed submit conflicts");
  };
  assert_eq!(through_the_client, every, "every window, signed");
  assert_eq!(judged, verification, "judged as the wire said");
  drop(client);
  daemon.stop();
}
