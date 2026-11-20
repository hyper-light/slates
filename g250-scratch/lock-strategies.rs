/// One of `choices`, each drawn in proportion to its weight: a plain range mapped through the cumulative weights, so
/// the strategy needs no `Arc`-backed union (R2).
fn weighted<T: Copy + Debug + 'static>(choices: &'static [(u32, T)]) -> impl Strategy<Value = T> {
  let total: u32 = choices.iter().map(|(weight, _)| weight).sum();
  (0..total).prop_map(move |draw| pick(choices, draw))
}

/// The choice a draw below the weights' total lands on; the last for any draw past it.
fn pick<T: Copy>(choices: &[(u32, T)], draw: u32) -> T {
  let mut left = draw;
  for (weight, choice) in choices {
    if left < *weight {
      return *choice;
    }
    left -= weight;
  }
  choices[choices.len() - 1].1
}

/// A step's principal: one of [`USERS`], now and then none.
fn user() -> impl Strategy<Value = Option<usize>> {
  (0..9u32, 0..USERS.len()).prop_map(|(draw, user)| (draw < 8).then_some(user))
}

/// A path's index: [`UNIVERSE`]'s, and now and then [`NOT_CANONICAL`]'s.
fn path() -> impl Strategy<Value = usize> {
  (0..31u32, 0..UNIVERSE.len()).prop_map(|(draw, path)| if draw < 30 { path } else { UNIVERSE.len() })
}

/// A lock request's step.
fn lock_step() -> impl Strategy<Value = Step> {
  let current = weighted(&[
    (6, Current::None),
    (2, Current::Youngest),
    (2, Current::Created),
    (1, Current::Older),
    (1, Current::Future),
  ]);
  let supply = weighted(&[
    (12, Supply::Drawn),
    (2, Supply::Valid),
    (1, Supply::BadScheme),
    (1, Supply::Control),
  ]);
  let comment = weighted(&[
    (6, Comment::None),
    (3, Comment::Plain),
    (1, Comment::Allowed),
    (1, Comment::Control),
    (1, Comment::Nul),
    (1, Comment::NotUtf8),
  ]);
  let expires = weighted(&[
    (6, Expiry::Never),
    (1, Expiry::Zero),
    (3, Expiry::Soon),
    (1, Expiry::Later),
    (2, Expiry::Past),
    (1, Expiry::Negative),
  ]);
  let targets = proptest::collection::vec((path(), current, supply), 0..4);
  (
    user(),
    targets,
    comment,
    expires,
    proptest::bool::weighted(0.25),
  )
    .prop_map(|(user, targets, comment, expires, steal)| Step::Lock {
      user,
      targets,
      comment,
      expires,
      steal,
    })
}

/// The token a step sends for a path.
fn present() -> impl Strategy<Value = Present> {
  weighted(&[
    (6, Present::Own),
    (2, Present::Another),
    (1, Present::Stale),
    (1, Present::Garbage),
    (2, Present::Absent),
  ])
}

/// An unlock request's step.
fn unlock_step() -> impl Strategy<Value = Step> {
  let targets = proptest::collection::vec((path(), present()), 0..3);
  (user(), targets, proptest::bool::weighted(0.25)).prop_map(|(user, targets, break_lock)| {
    Step::Unlock {
      user,
      targets,
      break_lock,
    }
  })
}

/// A commit's step.
fn commit_step() -> impl Strategy<Value = Step> {
  let action = weighted(&[
    (4, Action::Modify),
    (2, Action::Delete),
    (1, Action::Add),
    (1, Action::Replace),
  ]);
  let changes = proptest::collection::vec((path(), action), 1..4);
  let presented = proptest::collection::vec((path(), present()), 0..4);
  (user(), changes, presented, proptest::bool::weighted(0.3)).prop_map(
    |(user, changes, presented, keep)| Step::Commit {
      user,
      changes,
      presented,
      keep,
    },
  )
}

/// One step of a history: more locks than anything, the clock moving a little or past every expiry now and then. Each
/// kind of step is generated and a weighted draw keeps one, so no `Arc`-backed union is needed (R2).
fn step() -> impl Strategy<Value = Step> {
  let tick = (0..4u32, 0..4i64, 1_000..100_000i64).prop_map(|(draw, small, large)| Step::Tick {
    micros: if draw < 3 { small } else { large },
  });
  (0..12u32, lock_step(), unlock_step(), commit_step(), tick).prop_map(
    |(draw, lock, unlock, commit, tick)| match draw {
      0..=4 => lock,
      5..=6 => unlock,
      7..=9 => commit,
      _ => tick,
    },
  )
}

/// A history: its room, and its steps.
fn histories() -> impl Strategy<Value = (usize, Vec<Step>)> {
  let room = weighted(&[(1, 2usize), (1, 3), (1, 5), (1, ALL_ROOM)]);
  (room, proptest::collection::vec(step(), 1..48))
}

