//! A git read that met a let-go window, its window brought back from the node's store (goal 93 item 2a; main 01a20182's
//! ruling of 2026-11-22 05:45: a home's RAM copy may go once its node's store holds the pack durably, and is brought
//! back by identity on a read).
//!
//! **Why.** A landed push's pack lies twice in RAM (its staged copy on the repository's shard and its windows homed in
//! the region; 61.3 and 53.9 MB for git/git v2.0.0) while the node's store holds it as the push's changeset `Payload`
//! (A-767 C5). Letting those windows go needs a read that meets one to get it back: a let-go window is not lent, so a
//! read of it was refused `Unread` and the object was missing.
//!
//! **How.** A read of a let-go window is refused `PackedRefused::Away` naming the pack, its length and the offset met,
//! and the `Repository` keeps the first such miss (`Repository::waiting`). The turn's driver, off the shard's step, awaits
//! [`bring_back`]: on the shard, the window holding the offset and the pack's identity (the plain BLAKE3 of its bytes,
//! the key the node's store keeps the `Payload` under, `ContentStore::object_identity`); off it, the window's bytes read
//! from the node's store a piece at a time (`StoreHandle::piece`); then on the shard again, the window put back
//! (`ContentStore::bring_back`, which checks its identity), within the room its step is lent. The driver then takes the
//! same unit again. A window already back is nothing to do.
//!
//! **Bounded.** Each read of the node's store waits at most [`PATIENCE_NS`], past which the bring-back is refused
//! [`ReadThroughRefused::Unanswered`] and counted [`UNANSWERED`], so a store or owner that never answers fails the read
//! that waited on it, which its caller refuses as it refuses any other, never a wait without end (banned items 8 and 9;
//! main 01a20182's ruling of 2026-11-22 17:00). The bound is not a fix for what made the store not answer. A read of a
//! landed pack answers within a disk read, far inside the bound, so a refusal here names a defect of the store's read
//! path (its owner's queue, a piece read that never completes), counted so it is seen, never retried away.

use slates_git::store::packed::Waited;

use crate::state::with_state;

/// Why a let-go window was not brought back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadThroughRefused {
  /// The shard keeps no node store (a daemon without `--vcs-store`), so nothing it holds was let go to one.
  NoDisk,
  /// The shard holds no repository content store.
  NoStore,
  /// The store's owner refused the read.
  Owner(String),
  /// The node's store holds no bytes at this offset of the pack.
  Missing {
    /// Where.
    at: u64,
  },
  /// The content store refused the bytes back (another identity, or no room in its step).
  Refused(slates_vcs::ContentRefused),
  /// The node's store did not answer the read of the pack at `at` within [`PATIENCE_NS`] (module docs, "Bounded").
  Unanswered {
    /// The offset in the pack the read asked from.
    at: u64,
  },
  /// A Mercurial green's named file was not brought back by the revision its changeset names (A-887).
  Mercurial(super::hg_named::HgNamedRefused),
}

/// How long a bring-back waits for each read of the node's store before it is refused `Unanswered` (module docs,
/// "Bounded"). Derived: `vcs_share::wait::PATIENCE_NS`, the daemon's observation budget
/// (`crate::daemon::OBSERVE_BUDGET_NS`, ten liveness budgets, `slates_machine::budgets::observe_budget_ns`): the longest
/// the daemon waits on any party before judging it, as a fetch held past its room waits. A piece is one disk read of at
/// most `slates_store::store::PIECE_BYTES`, so a store that answers at all answers far inside it.
pub(crate) const PATIENCE_NS: u64 = crate::vcs_share::wait::PATIENCE_NS;

/// Format: the status counter of bring-backs refused because the node's store did not answer a read within
/// [`PATIENCE_NS`] ([`ReadThroughRefused::Unanswered`]).
pub(crate) const UNANSWERED: &str = "vcs_store.read_through.unanswered";

// The tests' hooks, kept on the shard that brings back, so a test's daemon alone meets them.
#[cfg(test)]
thread_local! {
  /// A test's patience for each read of the node's store in place of [`PATIENCE_NS`], where more than none.
  pub(crate) static TEST_PATIENCE_NS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
  /// Whether a test's node store never answers a bring-back's read (a store or owner wedged).
  pub(crate) static TEST_UNANSWERED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// The patience for one read of the node's store: [`PATIENCE_NS`], or a test's on this shard's thread.
fn patience_ns() -> u64 {
  #[cfg(test)]
  {
    let test = TEST_PATIENCE_NS.with(core::cell::Cell::get);
    if test > 0 {
      return test;
    }
  }
  PATIENCE_NS
}

/// One read of the landed pack `identity` (`len` bytes) from `at` on the node's store `disk`, waited for at most
/// [`patience_ns`]: refused `Unanswered` past it, counted [`UNANSWERED`].
async fn piece_within(
  disk: &slates_store::owner::StoreHandle,
  (identity, len, at): (slates_vcs::ContentId, u64, u64),
) -> Result<Vec<u8>, ReadThroughRefused> {
  let read = async {
    #[cfg(test)]
    if TEST_UNANSWERED.with(core::cell::Cell::get) {
      std::future::pending::<()>().await;
    }
    disk.piece(identity.0, len, at).await
  };
  match slates_rt::futures::within(patience_ns(), read).await {
    Ok(Some(answered)) => answered
      .map_err(|refused| ReadThroughRefused::Owner(format!("{refused:?}")))?
      .filter(|piece| !piece.is_empty())
      .ok_or(ReadThroughRefused::Missing { at }),
    Ok(None) => {
      let _ = with_state(|state| state.count(UNANSWERED, 1));
      Err(ReadThroughRefused::Unanswered { at })
    }
    // No deadline can be kept (polled off a shard): refused as the owner's, never waited for without one.
    Err(refused) => Err(ReadThroughRefused::Owner(format!("{refused:?}"))),
  }
}

/// Format: the status counter of windows a git read brought back from the node's store ([`bring_back`]).
pub(crate) const BROUGHT_BACK: &str = "vcs_store.read_through.brought_back";
/// Format: the status counter of their bytes.
pub(crate) const BROUGHT_BACK_BYTES: &str = "vcs_store.read_through.brought_back_bytes";

/// Brings back the let-go window `waited` names (module docs): the bytes put back; 0 where it is back already.
pub(crate) async fn bring_back(waited: Waited) -> Result<u64, ReadThroughRefused> {
  let found = with_state(|state| {
    let disk = state.vcs_disk.clone().ok_or(ReadThroughRefused::NoDisk)?;
    let content = state
      .vcs_stores
      .content()
      .ok_or(ReadThroughRefused::NoStore)?;
    let away = content.away_window(&waited.pack, waited.len, waited.offset);
    Ok(away.map(|window| (disk, content.object_identity(&waited.pack), window)))
  })
  .ok_or(ReadThroughRefused::NoStore)??;
  let Some((disk, identity, (window, start, len))) = found else {
    return Ok(0);
  };
  let end = start.saturating_add(len);
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(usize::try_from(len).unwrap_or(usize::MAX))
    .map_err(|_| ReadThroughRefused::Refused(slates_vcs::ContentRefused::CountExhausted))?;
  let mut at = start;
  while at < end {
    let piece = piece_within(&disk, (identity, waited.len, at)).await?;
    let want = usize::try_from(end.saturating_sub(at)).unwrap_or(usize::MAX);
    let taken = piece.get(..want.min(piece.len())).unwrap_or_default();
    bytes.extend_from_slice(taken);
    at = at.saturating_add(u64::try_from(taken.len()).unwrap_or(u64::MAX));
  }
  with_state(|state| {
    let put = crate::vcs_store::within_budget(state, len, |stores| {
      let content = stores.content_mut().ok_or(ReadThroughRefused::NoStore)?;
      if !content.is_away(&window) {
        return Ok(0);
      }
      content
        .bring_back(&window, &bytes)
        .map(|()| len)
        .map_err(ReadThroughRefused::Refused)
    });
    if let Ok(brought) = put
      && brought > 0
    {
      state.count(BROUGHT_BACK, 1);
      state.count(BROUGHT_BACK_BYTES, brought);
    }
    put
  })
  .ok_or(ReadThroughRefused::NoStore)?
}

#[cfg(test)]
mod tests {
  use slates_git::store::packed::Waited;
  use slates_vcs::ContentId;

  /// A git changeset of repository number `repository` carrying `bytes` as its one `Payload` section, sequence 0, for a
  /// test to land on the node's store.
  fn payload_changeset(
    repository: u128,
    bytes: Vec<u8>,
  ) -> (
    slates_store::log::Header,
    Vec<(slates_store::log::SectionKind, Vec<u8>)>,
  ) {
    let header = slates_store::log::Header {
      tool: slates_store::log::Tool::Git,
      repository,
      sequence: 0,
      judged_over: 0,
      grant: 0,
      work: 0,
      time_ns: slates_rt::futures::now_ns(),
      sections: 0,
    };
    (
      header,
      vec![(slates_store::log::SectionKind::Payload, bytes)],
    )
  }

  /// Goal 93 item 2a (main 01a20182's ruling of 2026-11-22 05:45: a home's RAM copy may go once its node's store holds
  /// the pack, brought back by identity on a read). Do: on a store-backed daemon, put a pack of three windows and a part
  /// in the shard's content store; land its bytes on the node's store as a changeset's `Payload`; mark every window landed
  /// and let it go; then, for an offset in each window, await [`super::bring_back`] with what a read that met it waits
  /// for; then read the object whole. Expect: every window let go, each brought back from the node's store (one bring-back
  /// a window, counted, a second for the same window nothing), and the object read whole as it was put. Failing first:
  /// a read of a let-go window was refused `Unread`, and nothing brought it back.
  #[test]
  fn a_let_go_window_is_brought_back_from_the_nodes_store_and_read_whole() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    // A repository makes the shard's content store.
    crate::git_push::tests::git_repository_on(&daemon, "read-through");
    let (sent, got) = std::sync::mpsc::channel();
    let (disk, header, parts, (window, bytes, len, root)) =
      crate::daemon::observe_first(&daemon, |state| {
        let window = state
          .vcs_stores
          .content()
          .map_or(1 << 18, slates_vcs::ContentStore::window);
        let bytes: Vec<u8> = (0..3 * window + 100)
          .map(|at| u8::try_from(at % 251).unwrap())
          .collect();
        let len = u64::try_from(bytes.len()).unwrap();
        let root = crate::vcs_store::within_budget(state, len, |stores| {
          stores.content_mut().unwrap().put_object(&bytes).unwrap()
        });
        let (header, parts) = payload_changeset(1, bytes.clone());
        (
          state.vcs_disk.clone().unwrap(),
          header,
          parts,
          (window, bytes, len, root),
        )
      });
    // Landed from this thread, which waits, so the shard is never held through the store's commit.
    assert!(crate::test_store::land_blocking(disk, header, parts));
    crate::daemon::observe_first(&daemon, move |state| {
      let let_go = crate::vcs_store::within_budget(state, 0, |stores| {
        let content = stores.content_mut().unwrap();
        let ids: Vec<ContentId> = bytes.chunks(window).map(ContentId::of).collect();
        ids
          .iter()
          .filter(|id| content.mark_landed(id) && content.let_go(id).is_ok())
          .count()
      });
      let task = slates_rt::futures::spawn(async move {
        let mut brought = Vec::new();
        for index in 0..4u64 {
          let offset = index * u64::try_from(window).unwrap() + 7;
          let waited = Waited {
            pack: root,
            len,
            offset,
          };
          brought.push(super::bring_back(waited).await);
        }
        let again = super::bring_back(Waited {
          pack: root,
          len,
          offset: 7,
        })
        .await;
        let read = crate::state::with_state(|state| {
          crate::vcs_store::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
            stores
              .content()
              .and_then(|content| content.object_bytes(&root, len))
          })
        })
        .flatten();
        let counted = crate::state::with_state(|state| {
          state
            .refusals
            .get(super::BROUGHT_BACK)
            .copied()
            .unwrap_or(0)
        });
        let _ = sent.send((let_go, brought, again, read == Some(bytes), counted));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let (let_go, brought, again, whole, counted) = got.recv().expect("the bring-back answered");
    daemon.stop();
    assert_eq!(let_go, 4, "every window let go");
    assert!(
      brought
        .iter()
        .all(|one| matches!(one, Ok(bytes) if *bytes > 0)),
      "{brought:?}"
    );
    assert_eq!(again, Ok(0), "a window back already is nothing to do");
    assert!(whole, "the object read whole as it was put");
    assert_eq!(counted, Some(4));
  }

  /// Goal 93 item 2b(a) (main 01a20182's rulings of 2026-11-22 06:58, and 21:15: a window is pinned only where this
  /// shard holds it for the region). Do: on a store-backed daemon, put a pack of six windows and a part in the shard's
  /// content store, held twice more a window (a landed push's holders: its staged pack, its repository's), its bytes
  /// landed on the node's store; hold for the region here another object whose first window is the pack's first; let the
  /// pack go as a landed push's pack is let go; then bring each window let go back and read the object whole. Expect: the
  /// window the region object shares kept, refused `Pinned { by: "homes" }` on a let-go asked again (its pin read from the
  /// region's holds, which the image carries); every other window let go whatever its holders and wherever its identity
  /// ranks, counted with its bytes; the pinned one counted; the object read whole once brought back. Failing first: no
  /// landed let-go, every window held in RAM.
  #[test]
  fn a_landed_packs_windows_go_but_those_a_home_holds_here() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    crate::git_push::tests::git_repository_on(&daemon, "landed-pins");
    let (sent, got) = std::sync::mpsc::channel();
    let (disk, header, parts, (window, bytes, len, ids, root)) =
      crate::daemon::observe_first(&daemon, |state| {
        let window = state
          .vcs_stores
          .content()
          .map_or(1 << 18, slates_vcs::ContentStore::window);
        let bytes: Vec<u8> = (0..6 * window + 100)
          .map(|at| u8::try_from(at % 253).unwrap())
          .collect();
        let len = u64::try_from(bytes.len()).unwrap();
        let ids: Vec<ContentId> = bytes.chunks(window).map(ContentId::of).collect();
        let root = crate::vcs_store::within_budget(state, len, |stores| {
          let content = stores.content_mut().unwrap();
          let root = content.put_object(&bytes).unwrap();
          for id in &ids {
            content.hold(id).unwrap();
            content.hold(id).unwrap();
          }
          root
        });
        let (header, parts) = payload_changeset(2, bytes.clone());
        (
          state.vcs_disk.clone().unwrap(),
          header,
          parts,
          (window, bytes, len, ids, root),
        )
      });
    // Landed from this thread, which waits, so the shard is never held through the store's commit.
    assert!(crate::test_store::land_blocking(disk, header, parts));
    crate::daemon::observe_first(&daemon, move |state| {
      let here = state.shard;
      // An object the region holds here, sharing the pack's first window: that window is the one a home pins.
      let mut region_object = bytes.get(..window).unwrap().to_vec();
      region_object.extend_from_slice(b"held for the region");
      crate::vcs_store::homes::keep_for_region(
        state,
        &region_object,
        &[crate::vcs_store::homes::TEST_OWNER],
      )
      .unwrap();
      let pinned: Vec<ContentId> = ids.iter().copied().take(1).collect();
      crate::vcs_store::let_go_landed_pack_among(state, &root, &[here]);
      let (kept, refused, gone) = crate::vcs_store::within_budget(state, 0, |stores| {
        let content = stores.content_mut().unwrap();
        let kept = pinned.iter().all(|id| !content.is_away(id));
        let refused: Vec<_> = pinned
          .iter()
          .map(|id| content.let_go_landed(id, crate::vcs_store::landed_pin(id)))
          .collect();
        let gone = ids.iter().filter(|id| content.is_away(id)).count();
        (kept, refused, gone)
      });
      let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      let let_go_bytes = counted(crate::vcs_store::LANDED_LET_GO_BYTES);
      let pinned_count = counted(crate::vcs_store::LANDED_PINNED);
      let expected = ids.len() - pinned.len();
      let task = slates_rt::futures::spawn(async move {
        for index in 0..ids.len() {
          let offset = u64::try_from(index * window).unwrap();
          let _ = super::bring_back(Waited {
            pack: root,
            len,
            offset,
          })
          .await;
        }
        let read = crate::state::with_state(|state| {
          crate::vcs_store::lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
            stores
              .content()
              .and_then(|content| content.object_bytes(&root, len))
          })
        })
        .flatten();
        let _ = sent.send((
          kept,
          refused,
          (gone, expected),
          (let_go_bytes, pinned_count, pinned.len()),
          read == Some(bytes),
        ));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let (kept, refused, (gone, expected), (let_go_bytes, pinned_count, pinned), whole) =
      got.recv().expect("the let-go answered");
    daemon.stop();
    assert!(kept, "a window the region holds here stays");
    assert!(
      refused
        .iter()
        .all(|refused| *refused == Err(slates_vcs::LetGoRefused::Pinned { by: "homes" })),
      "{refused:?}"
    );
    assert_eq!(
      gone, expected,
      "every window the region holds none of here let go"
    );
    assert!(let_go_bytes > 0);
    assert_eq!(pinned_count, u64::try_from(pinned).unwrap());
    assert!(whole, "the object read whole once brought back");
  }

  /// w-git's fetch look-ahead (goal 93): a window a read brought back is let go again once passed
  /// ([`crate::vcs_store::let_go_read`]). Do: on a store-backed daemon, put a one-window object held twice, its bytes
  /// landed on the node's store, mark it landed, let it go landed, bring it back, then let it go again through
  /// `let_go_read`; bring it back, hold it for the region here, and let it go once more. Expect: the bring-back puts it
  /// back and it reads; `let_go_read` lets it go again (still landed, whatever its holders); held for the region here,
  /// refused `Pinned { by: "homes" }`.
  #[test]
  fn a_window_brought_back_is_let_go_again_once_read() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    crate::git_push::tests::git_repository_on(&daemon, "read-again");
    let answered = crate::daemon::observe_first(&daemon, |state| {
      let bytes = b"one window, read and let go again".to_vec();
      let id = ContentId::of(&bytes);
      crate::vcs_store::within_budget(state, 64, |stores| {
        let content = stores.content_mut().unwrap();
        content.put_object(&bytes).unwrap();
        content.hold(&id).unwrap();
        content.mark_landed(&id);
        content.let_go_landed(&id, None).unwrap();
        content.bring_back(&id, &bytes).unwrap();
      });
      let again = crate::vcs_store::let_go_read(state, &id);
      let away = state
        .vcs_stores
        .content()
        .is_some_and(|content| content.is_away(&id));
      crate::vcs_store::within_budget(state, 64, |stores| {
        stores
          .content_mut()
          .unwrap()
          .bring_back(&id, &bytes)
          .unwrap();
      });
      crate::vcs_store::homes::keep_for_region(
        state,
        &bytes,
        &[crate::vcs_store::homes::TEST_OWNER],
      )
      .unwrap();
      let pinned = crate::vcs_store::let_go_read(state, &id);
      (again, away, pinned)
    });
    daemon.stop();
    let (again, away, pinned) = answered;
    assert!(matches!(again, Ok(bytes) if bytes > 0), "{again:?}");
    assert!(away, "let go again");
    assert_eq!(
      pinned,
      Err(slates_vcs::LetGoRefused::Pinned { by: "homes" })
    );
  }

  /// Goal 93 item 2b(b). Read a landed object whose two logical windows share one identity.
  /// The first cold read and the following warm read must both return the entire object.
  #[test]
  fn a_homes_first_read_of_a_landed_object_with_a_repeated_window_is_whole() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    let (cold, warm, expected) = crate::test_store::run_on_first(
      &daemon,
      || async {
        let (window, object, expected, disk) = crate::state::with_state(|state| {
          let window = state.store.content.chunk_bytes();
          state.vcs_stores.content_made(window).unwrap();
          let bytes = vec![b'x'; window * 2];
          let (object, _) = crate::vcs_store::homes::keep_for_region(
            state,
            &bytes,
            &[crate::vcs_store::homes::TEST_OWNER],
          )
          .unwrap();
          (window, object, bytes, state.vcs_disk.clone().unwrap())
        })
        .unwrap();
        // Awaited in this task, so the shard serves on while the store commits.
        let (header, parts) = payload_changeset(3, expected.clone());
        assert!(disk.land(header, parts).await.is_ok());
        let (here, budget) = crate::state::with_state(|state| {
          let here = state.shard;
          crate::vcs_store::let_go_landed_pack_among(state, &object, &[here]);
          let content = state.vcs_stores.content().unwrap();
          assert!(!content.holds_whole(&object));
          assert_eq!(
            state
              .refusals
              .get(crate::vcs_store::homes::LANDED_HOME_LET_GO_BYTES)
              .copied(),
            Some(window as u64)
          );
          (here, crate::fleet::live_budget().max_deadline_ns())
        })
        .unwrap();
        let cold = crate::vcs_store::homes::read(here, &[here], object, budget).await;
        let warm = crate::vcs_store::homes::read(here, &[here], object, budget).await;
        (
          cold.map(|got| (got.len(), got == expected)),
          warm.map(|got| (got.len(), got == expected)),
          expected.len(),
        )
      },
      std::time::Duration::from_secs(60),
    );
    daemon.stop();
    assert_eq!(warm, Ok((expected, true)), "the retry read every byte");
    assert_eq!(
      cold,
      Ok((expected, true)),
      "the original cold read must also succeed"
    );
  }

  /// Goal 93 item 2b(b) (main 01a20182's ruling: homes keep window → (payload identity, offset) for the windows a landed
  /// pack carried, and a home's read of a window let go brings it back through that record). Do: on a store-backed
  /// daemon, hold a pack of several windows for the region on this shard (`homes::keep_for_region`), land its bytes as a
  /// changeset's `Payload`, then let the landed pack go among this shard alone, its home; read the pack through
  /// `homes::read`. Expect: each window recorded where it lies in the pack, by its offset; every window let go, its bytes
  /// counted, none pinned; the read answers the pack whole, each window brought back by its record. Failing first: the
  /// home's windows pinned, held in RAM for good, or let go with nothing to bring them back by.
  #[test]
  fn a_landed_pack_its_home_holds_is_let_go_and_read_back_through_its_record() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    let (sent, got) = std::sync::mpsc::channel();
    let (disk, header, parts, (window, bytes, len, pack)) =
      crate::daemon::observe_first(&daemon, |state| {
        let window = state.store.content.chunk_bytes();
        state.vcs_stores.content_made(window).unwrap();
        let window = state
          .vcs_stores
          .content()
          .map_or(1 << 18, slates_vcs::ContentStore::window);
        let bytes: Vec<u8> = (0..3 * window + 77)
          .map(|at| u8::try_from(at % 251).unwrap())
          .collect();
        let len = u64::try_from(bytes.len()).unwrap();
        let (pack, _) = crate::vcs_store::homes::keep_for_region(
          state,
          &bytes,
          &[crate::vcs_store::homes::TEST_OWNER],
        )
        .unwrap();
        let (header, parts) = payload_changeset(3, bytes.clone());
        (
          state.vcs_disk.clone().unwrap(),
          header,
          parts,
          (window, bytes, len, pack),
        )
      });
    // Landed from this thread, which waits, so the shard is never held through the store's commit.
    assert!(crate::test_store::land_blocking(disk, header, parts));
    crate::daemon::observe_first(&daemon, move |state| {
      let here = state.shard;
      crate::vcs_store::let_go_landed_pack_among(state, &pack, &[here]);
      let ids: Vec<ContentId> = bytes.chunks(window).map(ContentId::of).collect();
      let offsets: Vec<Option<u64>> = ids
        .iter()
        .map(|id| crate::vcs_store::homes::landed_at(id).map(|at| at.offset))
        .collect();
      let gone = state.vcs_stores.content().map_or(0, |content| {
        ids.iter().filter(|id| content.is_away(id)).count()
      });
      let counted = |state: &crate::state::ShardState, name: &str| {
        state.refusals.get(name).copied().unwrap_or(0)
      };
      let let_go_bytes = counted(state, crate::vcs_store::homes::LANDED_HOME_LET_GO_BYTES);
      let pinned = counted(state, crate::vcs_store::LANDED_PINNED);
      let windows = ids.len();
      let budget_ns = crate::fleet::live_budget().max_deadline_ns();
      let task = slates_rt::futures::spawn(async move {
        let read = crate::vcs_store::homes::read(here, &[here], pack, budget_ns).await;
        let brought = crate::state::with_state(|state| {
          counted(state, crate::vcs_store::homes::LANDED_BROUGHT_BACK)
        })
        .unwrap_or(0);
        let _ = sent.send((
          (offsets, window),
          (gone, windows),
          (let_go_bytes, len, pinned),
          (read == Ok(bytes), brought),
        ));
      })
      .unwrap();
      slates_rt::futures::detach(task).unwrap();
    });
    let ((offsets, window), (gone, windows), (let_go_bytes, len, pinned), (whole, brought)) =
      got.recv().expect("the read answered");
    daemon.stop();
    let expected: Vec<Option<u64>> = (0..windows)
      .map(|index| Some(u64::try_from(index * window).unwrap()))
      .collect();
    assert_eq!(
      offsets, expected,
      "each window recorded at its offset in the pack"
    );
    assert_eq!(gone, windows, "every window the home held let go");
    assert_eq!(let_go_bytes, len, "the pack's bytes counted let go");
    assert_eq!(pinned, 0, "no window pinned: the record brings it back");
    assert!(whole, "the home's read answers the pack whole");
    assert_eq!(
      brought,
      u64::try_from(windows).unwrap(),
      "each window brought back by its record"
    );
  }

  /// Goal 93 item 2b(a)'s window (main 01a20182, 2026-11-22: a fetch that starts while a push of the same repository is
  /// still deciding its refs reads every object). Do: on a store-backed daemon, put a landed pack of several windows in a
  /// git repository's content store and owe its let-go (the commit of a push's first entry deferred it), with a push of
  /// the repository under way (`vcs_share::Live::begin`, as `ReceivePack` holds one); let a fetch end, as its going idle
  /// runs (`git_encode::give_back_idle`); then end the push and go idle again. Expect: while the push is under way the
  /// pack held whole and still owed, nothing of it let go or pinned, so its later refs read every object; once the push
  /// has ended its let-go run (each window let go, or kept where a home holds it here: on this one-shard daemon every
  /// window's home is here) and owed no more. Failing first: the fetch's going idle ran the let-go under the push.
  #[test]
  fn a_landed_pack_owed_its_let_go_stays_whole_while_a_push_of_it_is_under_way() {
    let (_store, daemon) = crate::test_store::audit_daemon_stored();
    let id = crate::git_push::tests::git_repository_on(&daemon, "owed-under-push");
    let answered = crate::daemon::observe_first(&daemon, move |state| {
      let window = state
        .vcs_stores
        .content()
        .map_or(1 << 18, slates_vcs::ContentStore::window);
      let bytes: Vec<u8> = (0..3 * window + 11)
        .map(|at| u8::try_from(at % 247).unwrap())
        .collect();
      let len = u64::try_from(bytes.len()).unwrap();
      let pack = crate::vcs_store::within_budget(state, len, |stores| {
        stores.content_mut().unwrap().put_object(&bytes).unwrap()
      });
      state
        .vcs_stores
        .landed_owed
        .entry(id)
        .or_default()
        .push(pack);
      let whole = |state: &crate::state::ShardState| {
        state
          .vcs_stores
          .content()
          .is_some_and(|content| content.holds_whole(&pack))
      };
      let counted = |state: &crate::state::ShardState| {
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        count(crate::vcs_store::LANDED_LET_GO_BYTES) + count(crate::vcs_store::LANDED_PINNED)
      };
      let owed = |state: &crate::state::ShardState| {
        state
          .vcs_stores
          .landed_owed
          .get(&id)
          .is_some_and(|owed| owed.contains(&pack))
      };
      let pushing = crate::vcs_share::Live::begin(id);
      crate::vcs_store::git_encode::give_back_idle(state, id);
      let under_push = (whole(state), counted(state), owed(state));
      drop(pushing);
      crate::vcs_store::git_encode::give_back_idle(state, id);
      // The let-go is queued and turned a slice at a time by a task of the shard's (`vcs_store::let_go_turn`), which a
      // synchronous history turns itself.
      while crate::vcs_store::let_go_turn(state) {}
      (under_push, (counted(state), owed(state)))
    });
    daemon.stop();
    let ((whole_under_push, let_go_under_push, owed_under_push), (let_go_after, owed_after)) =
      answered;
    assert!(
      whole_under_push,
      "the pack held whole while a push of it is under way"
    );
    assert_eq!(
      let_go_under_push, 0,
      "nothing let go or pinned under the push"
    );
    assert!(owed_under_push, "its let-go still owed under the push");
    assert!(let_go_after > 0, "its let-go run once the push ended");
    assert!(!owed_after, "owed no more");
  }
}
