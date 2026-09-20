# Collectors expired before reading replies delivered during their last sleep

Date: 2026-09-20. Design: §4.8, §4.10, AC-8.12.

## Symptom and evidence

The Linux io_uring fleet hedge history failed at 4.486584211 s in the workspace
suite. After correcting its independent candidate-selection defect, it failed in
the first isolated trial at 3.332623669 s against a 3 s hold. The hedge selected
the healthy peer, but repeatedly collected only the owner's acknowledgement.

Tagged diagnostics reproduced it at 3.195309834 s. Eight offer collections logged
`returned=0 discarded=1`: the healthy holder's missing-set reply was already in
the channel when the collector expired. The head committed promptly after content
finally succeeded. Log: `/private/tmp/slates-hedge-offer-expiry.log`.

Commands, on 2026-09-20:

- `cargo test --offline -p slates-cluster --test content`: deterministic simulated
  content history fails in 0.01 s, `not placed: 1 of the quorum acknowledged`.
  The valid missing-set reply was delivered at 100000 virtual ns; collection at
  20100000 ns discarded it.
- `cargo test --offline -p slates-cluster --test commit
  an_acknowledgement_delivered_during_the_final_poll_places_the_record`: fails in
  0.01 s, `uncertain: ... 1 acknowledgement(s)` despite the holder answering.
- `cargo test --offline -p slates-server --test fleet
  a_slow_first_round_candidate_is_hedged_after_the_measured_p95 -- --nocapture`
  in the approved Linux io_uring container reproduces the original timing failure.

## Root cause and impact

`DispatchWait::keep_waiting` slept, then judged expiration using the reply count
from before the sleep. At expiration its callers exited without consuming the
replies that arrived during the sleep. The content offer collector dropped those
replies and their sessions entirely. Record and promise collectors recovered the
sessions but ignored the acknowledgements/promises. Broadcast deferred replies
unnecessarily to a later coordinator period.

With a measured content p95 close to the collection poll interval, every healthy
offer could hit this boundary. Retrying closed and re-established its session,
turning a millisecond reply into seconds of non-placement.

## Fix

Judge the current, fully drained reply count before sleeping. If waiting is still
permitted, sleep once and always return to the caller's receive loop. The next
empty-channel check judges expiration using all replies delivered during the sleep.
This changes no budget or timeout and preserves the existing progress-extension law.
All five collectors use this shared rule: content offers, bound acknowledgements,
consensus broadcasts, register promises, and ledger promises.

The regression histories use real authenticated endpoints and holder logic on the
simulated fabric. One poll spans the collection budget, forcing the boundary without
wall-clock noise; content must be verified and its session returned for reuse.

## Sibling finding

An offer that is genuinely still in flight when collection expires also loses its
reply channel in `put_content`. A second simulated
regression confirmed this: recovery returned `[]` instead of `[HostId(2)]` in
0.01 s (`cargo test --offline -p slates-cluster --test content`, log
`/private/tmp/slates-late-offer-red.log`). Preserve both offer and put reply channels
in `Stragglers`, with a fixed two-channel bound matching the two exchanges. Late
missing-set replies return sessions but cannot count as content acknowledgements.

## Validation

- `cargo test --offline -p slates-cluster`: **190 passed, zero failed** on macOS,
  including content, record, both takeover protocols, consensus and WAN histories.
  This preceded the additional silent-holder test, covered by the workspace run.
  `/private/tmp/slates-cluster-green.log`.
- Ten serial original Linux io_uring hedge histories: **10/10 passed**; placement
  times in ms: 500.343, 499.584, 281.630, 506.180, 500.402, 496.281, 496.701,
  284.177, 497.945, 282.544. Same four-CPU / 4-GiB disposable container, no concurrent
  builds or tests. `/private/tmp/slates-hedge-green.log`.
- Temporary tagged diagnostics removed. Full Linux workspace run: **1,524 passed,
  zero failed, 14 ignored**, including all 49 fleet histories in 248.00 s. Strict
  workspace Clippy and `cargo xtask check` passed in that same serial container run.
  `/private/tmp/slates-special-workspace-collector-fixed.log`.
