----------------------------- MODULE FencedLedger -----------------------------
(***************************************************************************)
(* slates, design model 4 (2026-11-18, GAP-A9-7's record half): the fenced  *)
(* single-writer ledger (a green's merge-record log, §4.16), adopted by a   *)
(* new owner position by position (§4.8: "For each ledger position ...     *)
(* phase one consults an authorized quorum of distinct holders and adopts  *)
(* the highest accepted proposal consistent with the committed prefix").   *)
(*                                                                          *)
(* FencedRegister models one register: a holder keeps only its highest     *)
(* record, and a successor adopts the highest record a quorum reports.  A   *)
(* successor whose own records end two or more positions below the adopted *)
(* one then lacks the positions between (the record half of GAP-A9-7).      *)
(* Here each holder keeps an accepted (epoch, value) per position, and      *)
(* phase one reports every position, so the successor adopts the whole     *)
(* committed prefix.                                                        *)
(*                                                                          *)
(* The configuration master hands out strictly increasing epochs, one host *)
(* an epoch (the regional Raft group).  An owner of epoch e:                *)
(*   - promotes: each holder raises its promise (fence) to e and reports    *)
(*     its accepted entry at every position; with a majority's reports, the *)
(*     owner adopts at each position the value accepted under the highest   *)
(*     epoch (or none);                                                     *)
(*   - proposes in order: the adopted positions first, each again under its *)
(*     own epoch (phase-two acceptance at a new epoch records that epoch    *)
(*     even when the bytes are the old ones, BUG-12), then new values; a    *)
(*     position is proposed only once the one before it has committed under *)
(*     this epoch (the merge plane ships a green's next version only once    *)
(*     the last is placed, §4.16 "Commit").                                 *)
(* A holder accepts a proposal at or above its promise, raising the         *)
(* promise, and keeps it at its position.  A position commits a value once  *)
(* a majority accepted that value there under one epoch.  A paused-and-    *)
(* resumed owner keeps proposing under its old epoch forever.               *)
(*                                                                          *)
(* Properties:                                                              *)
(*   Agreement          no position commits two values (under any epochs);  *)
(*   Continuity         an owner's adopted log holds, at every position,    *)
(*                      every value committed there under a lower epoch,    *)
(*                      whenever it commits (a proposal partly accepted     *)
(*                      before the promotion may complete later);           *)
(*   AdoptedIsPrefix    an adopted log has no hole below a position it      *)
(*                      holds (the committed prefix is exact);              *)
(*   StaleNeverCommits  a proposal issued after a higher epoch has its      *)
(*                      promotion majority never commits;                   *)
(*   ReadSafety         a majority's highest-epoch entry at each position   *)
(*                      is every value committed there.                     *)
(* TLC configurations: FencedLedger.cfg (two epochs), FencedLedger_3.cfg   *)
(* (three epochs, symmetric holders), FencedLedger_register.cfg (the        *)
(* register's rule, which must fail Continuity).                            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Acceptors,    \* the record holders, e.g. {a1, a2, a3}
          Hosts,        \* candidate owners, e.g. {h1, h2}
          Values,       \* payloads, e.g. {v1, v2}
          MaxEpoch,     \* epochs 1..MaxEpoch
          MaxPos,       \* ledger positions 1..MaxPos
          PerPosition   \* TRUE: adopt every position (§4.8); FALSE: the
                        \* register's rule, the highest record alone (the
                        \* server's round before GAP-A9-7's record half),
                        \* which the checker must refute (non-vacuity)

ASSUME MaxEpoch \in Nat /\ MaxEpoch >= 1
ASSUME MaxPos \in Nat /\ MaxPos >= 1
ASSUME PerPosition \in BOOLEAN

Majority == {Q \in SUBSET Acceptors : 2 * Cardinality(Q) > Cardinality(Acceptors)}

Epochs == 1..MaxEpoch
Positions == 1..MaxPos
Nothing == "none"
NoEntry == [epoch |-> 0, value |-> Nothing]
AnyEntry == [epoch : 0..MaxEpoch, value : Values \cup {Nothing}]
Proposals == [epoch : Epochs, pos : Positions, value : Values]
EmptyLog == [p \in Positions |-> Nothing]

\* The entry under the highest epoch among `S` (entries of one position); NoEntry for none.
Highest(S) == IF S = {} THEN NoEntry
              ELSE CHOOSE x \in S : \A y \in S : x.epoch >= y.epoch

VARIABLES
  epochOwner,   \* [Epochs -> Hosts \cup {"none"}]: the master's assignments
  nextEpoch,    \* the next epoch the master hands out
  promise,      \* [Acceptors -> 0..MaxEpoch]: each holder's fence
  accepted,     \* [Acceptors -> [Positions -> AnyEntry]]: each holder's entry per position
  promAcked,    \* [Epochs -> SUBSET Acceptors]: holders that answered phase one
  promVal,      \* [Epochs -> [Acceptors -> [Positions -> AnyEntry]]]: what each reported
  promoted,     \* [Epochs -> BOOLEAN]: the owner has adopted
  adopted,      \* [Epochs -> [Positions -> Values \cup {"none"}]]: what phase one adopted
  log,          \* [Epochs -> [Positions -> Values \cup {"none"}]]: what the owner proposed
  nextPos,      \* [Epochs -> 1..MaxPos+1]: the owner's next position
  issued,       \* SUBSET Proposals: phase-two proposals made
  stale,        \* SUBSET Proposals: made after a higher epoch's promotion majority
  acks          \* [Proposals -> SUBSET Acceptors]: phase-two acceptances

vars == <<epochOwner, nextEpoch, promise, accepted, promAcked, promVal, promoted,
          adopted, log, nextPos, issued, stale, acks>>

CommittedProposals == {r \in Proposals : acks[r] \in Majority}

Committed(p, v) == \E e \in Epochs : acks[[epoch |-> e, pos |-> p, value |-> v]] \in Majority

PromotionMajority(e) == promAcked[e] \in Majority

Init ==
  /\ epochOwner = [e \in Epochs |-> "none"]
  /\ nextEpoch = 1
  /\ promise = [a \in Acceptors |-> 0]
  /\ accepted = [a \in Acceptors |-> [p \in Positions |-> NoEntry]]
  /\ promAcked = [e \in Epochs |-> {}]
  /\ promVal = [e \in Epochs |-> [a \in Acceptors |-> [p \in Positions |-> NoEntry]]]
  /\ promoted = [e \in Epochs |-> FALSE]
  /\ adopted = [e \in Epochs |-> EmptyLog]
  /\ log = [e \in Epochs |-> EmptyLog]
  /\ nextPos = [e \in Epochs |-> 1]
  /\ issued = {}
  /\ stale = {}
  /\ acks = [r \in Proposals |-> {}]

MasterAssign(h) ==
  /\ nextEpoch <= MaxEpoch
  /\ epochOwner' = [epochOwner EXCEPT ![nextEpoch] = h]
  /\ nextEpoch' = nextEpoch + 1
  /\ UNCHANGED <<promise, accepted, promAcked, promVal, promoted, adopted, log,
                 nextPos, issued, stale, acks>>

(***************************************************************************)
(* Phase one, one holder at a time: it raises its promise to e and reports  *)
(* its whole log, every position's accepted entry.                          *)
(***************************************************************************)
PromoteAck(e, a) ==
  /\ epochOwner[e] # "none"
  /\ a \notin promAcked[e]
  /\ e >= promise[a]
  /\ promise' = [promise EXCEPT ![a] = e]
  /\ promAcked' = [promAcked EXCEPT ![e] = promAcked[e] \cup {a}]
  /\ promVal' = [promVal EXCEPT ![e][a] = accepted[a]]
  /\ UNCHANGED <<epochOwner, nextEpoch, accepted, promoted, adopted, log, nextPos,
                 issued, stale, acks>>

(***************************************************************************)
(* The owner of epoch e adopts, at every position, the value accepted under *)
(* the highest epoch among a majority's reports.                            *)
(***************************************************************************)
\* At every position, the value accepted under the highest epoch among e's reports.
PerPositionLog(e) ==
  [p \in Positions |-> Highest({promVal[e][a][p] : a \in promAcked[e]} \ {NoEntry}).value]

\* The register's rule: the highest record reported, its position alone.
TopLog(e) ==
  LET full == PerPositionLog(e)
      held == {p \in Positions : full[p] # Nothing}
      top == IF held = {} THEN 0 ELSE CHOOSE p \in held : \A q \in held : p >= q
  IN [p \in Positions |-> IF p = top THEN full[p] ELSE Nothing]

Adopt(e) ==
  /\ epochOwner[e] # "none"
  /\ ~promoted[e]
  /\ PromotionMajority(e)
  /\ adopted' = [adopted EXCEPT ![e] = IF PerPosition THEN PerPositionLog(e) ELSE TopLog(e)]
  /\ promoted' = [promoted EXCEPT ![e] = TRUE]
  /\ UNCHANGED <<epochOwner, nextEpoch, promise, accepted, promAcked, promVal, log,
                 nextPos, issued, stale, acks>>

(***************************************************************************)
(* Phase two, in order: the owner of e proposes its next position, the      *)
(* adopted value there if it adopted one (under e, BUG-12), else v; only    *)
(* once the position before committed under e.  A resumed stale owner is   *)
(* any owner of an old epoch still proposing.                               *)
(***************************************************************************)
Propose(e, v) ==
  /\ promoted[e]
  /\ nextPos[e] <= MaxPos
  /\ LET p == nextPos[e]
         value == IF adopted[e][p] # Nothing THEN adopted[e][p] ELSE v
         r == [epoch |-> e, pos |-> p, value |-> value]
     IN /\ IF p = 1 THEN TRUE ELSE acks[[epoch |-> e, pos |-> p - 1, value |-> log[e][p - 1]]] \in Majority
        /\ (adopted[e][p] # Nothing => v = adopted[e][p])
        /\ issued' = issued \cup {r}
        /\ stale' = IF \E e2 \in Epochs : e2 > e /\ PromotionMajority(e2)
                      THEN stale \cup {r} ELSE stale
        /\ log' = [log EXCEPT ![e][p] = value]
        /\ nextPos' = [nextPos EXCEPT ![e] = p + 1]
  /\ UNCHANGED <<epochOwner, nextEpoch, promise, accepted, promAcked, promVal,
                 promoted, adopted, acks>>

Accept(r, a) ==
  /\ r \in issued
  /\ a \notin acks[r]
  /\ r.epoch >= promise[a]
  /\ promise' = [promise EXCEPT ![a] = r.epoch]
  /\ accepted' = [accepted EXCEPT ![a][r.pos] = [epoch |-> r.epoch, value |-> r.value]]
  /\ acks' = [acks EXCEPT ![r] = acks[r] \cup {a}]
  /\ UNCHANGED <<epochOwner, nextEpoch, promAcked, promVal, promoted, adopted, log,
                 nextPos, issued, stale>>

Next ==
  \/ \E h \in Hosts : MasterAssign(h)
  \/ \E e \in Epochs, a \in Acceptors : PromoteAck(e, a)
  \/ \E e \in Epochs : Adopt(e)
  \/ \E e \in Epochs, v \in Values : Propose(e, v)
  \/ \E r \in Proposals, a \in Acceptors : Accept(r, a)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Properties.                                                              *)
(***************************************************************************)

Agreement ==
  \A p \in Positions, v1, v2 \in Values :
    (Committed(p, v1) /\ Committed(p, v2)) => v1 = v2

Continuity ==
  \A e \in Epochs :
    promoted[e] =>
      \A r \in CommittedProposals : r.epoch < e => adopted[e][r.pos] = r.value

AdoptedIsPrefix ==
  \A e \in Epochs, p \in Positions :
    (promoted[e] /\ p > 1 /\ adopted[e][p] # Nothing) => adopted[e][p - 1] # Nothing

StaleNeverCommits ==
  stale \cap CommittedProposals = {}

ReadSafety ==
  \A Q \in Majority, r \in CommittedProposals :
    Highest({accepted[a][r.pos] : a \in Q} \ {NoEntry}).value = r.value

TypeOK ==
  /\ nextEpoch \in 1..(MaxEpoch + 1)
  /\ \A a \in Acceptors : promise[a] \in 0..MaxEpoch
  /\ \A a \in Acceptors, p \in Positions : accepted[a][p] \in AnyEntry
  /\ \A e \in Epochs : nextPos[e] \in 1..(MaxPos + 1)
  /\ issued \subseteq Proposals
  /\ stale \subseteq issued

\* Holders are interchangeable: TLC explores one ordering of them.
Symmetric == Permutations(Acceptors)

=============================================================================
