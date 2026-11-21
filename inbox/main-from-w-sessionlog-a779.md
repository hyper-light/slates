# w-sessionlog -> main: A-779 landed (2026-11-20 ~15:35 PST)

- LANDED bbc605ac9 (merge f20719312 on origin/main, merge-base confirmed). Base: origin/main 1b63ab896 merged.
- Your ruling, as built: any client's presentation of the same volume into the same target supersedes every earlier
  one awaiting a grant -> LandingState::Superseded + superseded_by (appended), Op::LandingSuperseded; grant refused
  Refusal::LandingSuperseded { newer } (api: Stale::Superseded). No clock. Red first: the new test's grant came back
  Refused(NotFound) with the supersede disabled; green with it. A granted landing's presentation kept for its resume
  (aborted run) is not superseded.
- Found by use while building it, fixed in the same piece: a presentation whose client went away (AUD-29-07
  abandon) leaves its record AwaitingGrant forever, so A-773's log listed ungrantable presentations. The log now reads
  the shard's held presentations (bounded by landings_awaiting_per_shard) = exactly the grantable ones.
  Not fixed: those abandoned records stay AwaitingGrant in the partition (only a later presentation of the same
  volume+target supersedes them). Say if you want an Abandoned state.
- Tests (macOS arm64, this Mac, local): server --test session_log 2, --test presentations 10 (its replaced-presentation
  assertion now expects LandingSuperseded); db model every_recovery 2, wire_order 2; ipc wire_order 2; api --lib observe 2.
- Next: item 2 (identical vs accepted in the submit's reply).
