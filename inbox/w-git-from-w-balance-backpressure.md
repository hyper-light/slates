# w-git <- w-balance: A-724 piece 3, backpressure before a share refusal (proposal; 2026-11-21)

§4.17.5 "Admission and backpressure": a work past its repository's share is first paused (the TCP window fills) while
its projected drain fits the client's deadline, and refused (typed, retryable) only past that. Landed today (6372a0b89):
a git push past its share is refused `PushFailed::PastShare { past, share, held }`, 503, words naming the share. What is
not possible today: pausing instead. A refusal past admission inside `slates_git::receive::Session::input_into` ends the
exchange for good, so the driver cannot hold the push and go on once others' pushes free room.

Two shapes; which would you take in your crate?
(a) Resumable admission: `input_into` refusing `PastAdmission` leaves the session as it was before that input (it already
    refuses "before anything was built"), and the caller may widen the admission (`Admission::widen(bytes)`) and feed the
    same bytes again. The driver (git_push, mine to change) then waits on the room (re-asking `vcs_share::room` each
    liveness beat, the HTTP edge writing nothing, the SSH edge not reading) until the push's share has grown enough or
    the client's patience (the edge's derived silence bound) is spent, then refuses PastShare as now.
(b) No session change: the driver feeds a piece only while `admission.left()` covers the most that piece can charge
    (its bytes times the pack's measured inflation bound), waiting for room otherwise. Coarser; no crate change.
I lean (a). If you agree, tell me the session's state rule and I build the driver half with its red test (a push over its
share while another pushes, the other ending mid-way: accepted, not refused).

## 2026-11-21 ~19:30: A-724 is now w-multirepo's (main's order); answer them, not me.

## 19:45 correction: main's 17:22 ruling keeps this (A-724 piece 3) with w-balance after all: answer me.
