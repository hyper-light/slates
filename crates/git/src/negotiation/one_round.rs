//! One round on a version head (A-615 part 2; §4.17.2 "A fetch" item 2 and "What each version brought", item 3).
//!
//! A want answered here is a commit that is some green's head at version N; one of the client's common haves headed the
//! same green at a version M; and versions M+1 to N each fast-forwarded the one before. Then D(M+1) to D(N) is exactly
//! what the client lacks of the want's history: D(k) is what version k reaches and version k-1 does not, a fast-forward
//! loses nothing, so the union telescopes to what N reaches and M does not. Where some version between rewrote the
//! branch, the union would also hold what that rewrite dropped, so the want takes the walk. Only the latest such M
//! needs trying: a rewrite between it and N lies between every earlier M and N too. Among several greens the want
//! heads, the shortest run is taken.
//!
//! The path is open only when the request asks nothing a run cannot answer: no client shallow commits, no deepening,
//! and a filter, if any, that decides each object by its own row ([`super::Filter::is_per_object`]).

use super::{Graph, Request, Run, charge};
use crate::Admission;

/// The runs of versions found, and the wants they answer.
pub(super) struct Runs<G, N> {
  /// The runs, each once, in order.
  pub(super) runs: Vec<Run<G>>,
  /// The wants answered by a run, one that is already the client's included.
  pub(super) wants: Vec<N>,
  /// How many wants were answered so.
  pub(super) answered: u64,
}

/// The wants answered in one round, and their runs.
pub(super) fn runs<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  wants: &[S::Node],
  common: &[S::Node],
  client_shallow: &[S::Node],
  admission: &mut Admission,
) -> Result<Runs<S::Green, S::Node>, super::Refusal> {
  let mut found = Runs {
    runs: Vec::new(),
    wants: Vec::new(),
    answered: 0,
  };
  let open = client_shallow.is_empty()
    && request.deepen.is_none()
    && request.filter.is_none_or(super::Filter::is_per_object);
  if !open {
    return Ok(found);
  }
  for want in wants.iter().filter(|want| graph.commit(**want).is_some()) {
    let Some(run) = best_run(graph, *want, common) else {
      continue;
    };
    charge::<(S::Node, Run<S::Green>)>(admission)?;
    found.wants.push(*want);
    found.answered = found.answered.saturating_add(1);
    if run.after < run.through {
      found.runs.push(run);
    }
  }
  found.runs.sort();
  found.runs.dedup();
  Ok(found)
}

/// The shortest run that answers `want`: over each green it heads, the latest version a common have headed, when every
/// version after it fast-forwarded.
fn best_run<S: Graph>(graph: &S, want: S::Node, common: &[S::Node]) -> Option<Run<S::Green>> {
  graph
    .greens_at(want)
    .filter_map(|(green, through)| {
      let after = common
        .iter()
        .flat_map(|have| graph.versions_at(green, *have))
        .filter(|version| *version <= through)
        .max()?;
      let forward =
        (after.saturating_add(1)..=through).all(|version| graph.fast_forward(green, version));
      forward.then_some(Run {
        green,
        after,
        through,
      })
    })
    .min_by_key(|run| (run.through.saturating_sub(run.after), run.green))
}
