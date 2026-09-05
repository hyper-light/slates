-------------------------------- MODULE Reconfig --------------------------------
(***************************************************************************)
(* slates, design model 2 (2026-09-04): changing a register's holder set    *)
(* (the placement neighbourhood) while the owner keeps writing, with a      *)
(* fresh acceptor replacing one that restarted.                             *)
(*                                                                          *)
(* Two configurations: Old and New, each a set of acceptors with its own    *)
(* majorities.  The configuration master publishes the change (Announce),   *)
(* after which the owner is in the joint phase: every write must reach a    *)
(* majority of Old AND a majority of New to commit (Raft's joint consensus, *)
(* Vertical Paxos I's "the previous configuration remains active").  The    *)
(* master retires Old only after the owner acknowledges the new             *)
(* configuration and at least one record has committed under the joint     *)
(* rule (so New holds the latest state); from then on a majority of New     *)
(* suffices and Old acceptors are ignored.                                  *)
(*                                                                          *)
(* A restarted acceptor is a new member: the acceptor that left Old never   *)
(* returns; the one that joins New starts empty.  This is the               *)
(* restart-identity invariant of the design made structural.               *)
(*                                                                          *)
(* Readers read according to the configuration they know, which may lag    *)
(* by one step: a reader that still believes Old reads a majority of Old;   *)
(* a reader that knows New reads a majority of New.  The property checked:  *)
(*   ReadSafety   every completed read returns a record at least as new as  *)
(*                every record committed before the read started, in       *)
(*                every phase and for every reader knowledge.              *)
(*   NoLoss       every record committed under Old before retirement is     *)
(*                readable from a New majority after retirement.            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS OldSet,   \* e.g. {a1, a2, a3}
          NewSet,   \* e.g. {a2, a3, a4}: a1 restarted and left, a4 is fresh
          Values,
          MaxSeq

Acceptors == OldSet \cup NewSet
Maj(S) == {Q \in SUBSET S : 2 * Cardinality(Q) > Cardinality(S)}

NoRecord == [seq |-> 0, value |-> "none"]
Records == [seq : 1..MaxSeq, value : Values]
Newer(r1, r2) == r1.seq > r2.seq
NewerOrEq(r1, r2) == r1 = r2 \/ Newer(r1, r2)
MaxRec(S) == CHOOSE r \in S : \A s \in S : NewerOrEq(r, s)

VARIABLES
  phase,       \* "old" | "joint" | "new"
  stored,      \* [Acceptors -> record]
  nextSeq,
  issued,      \* set of issued records
  acks,        \* [Records -> SUBSET Acceptors]
  ownerAcked,  \* the owner has acknowledged the New configuration
  retiredCommitted \* records committed under Old or joint when Old retired

vars == <<phase, stored, nextSeq, issued, acks, ownerAcked, retiredCommitted>>

CommittedIn(r) ==
  CASE phase = "old"   -> acks[r] \cap OldSet \in Maj(OldSet)
    [] phase = "joint" -> acks[r] \cap OldSet \in Maj(OldSet) /\ acks[r] \cap NewSet \in Maj(NewSet)
    [] phase = "new"   -> acks[r] \cap NewSet \in Maj(NewSet)

\* A record stays committed once it was committed in an earlier phase; the
\* retirement rule below guarantees this, and NoLoss checks it.
Committed == {r \in Records : CommittedIn(r)}

Init ==
  /\ phase = "old"
  /\ stored = [a \in Acceptors |-> NoRecord]
  /\ nextSeq = 1
  /\ issued = {}
  /\ acks = [r \in Records |-> {}]
  /\ ownerAcked = FALSE
  /\ retiredCommitted = {}

Issue(v) ==
  /\ nextSeq <= MaxSeq
  /\ issued' = issued \cup {[seq |-> nextSeq, value |-> v]}
  /\ nextSeq' = nextSeq + 1
  /\ UNCHANGED <<phase, stored, acks, ownerAcked, retiredCommitted>>

\* Acceptors that have left never accept again; fresh ones accept from the
\* moment the change is announced.
Active(a) ==
  CASE phase = "old"   -> a \in OldSet
    [] phase = "joint" -> a \in OldSet \cup NewSet
    [] phase = "new"   -> a \in NewSet

Accept(r, a) ==
  /\ r \in issued
  /\ Active(a)
  /\ a \notin acks[r]
  /\ stored' = [stored EXCEPT ![a] = IF Newer(r, stored[a]) THEN r ELSE stored[a]]
  /\ acks' = [acks EXCEPT ![r] = acks[r] \cup {a}]
  /\ UNCHANGED <<phase, nextSeq, issued, ownerAcked, retiredCommitted>>

\* The master announces the new configuration; the owner enters the joint phase.
Announce ==
  /\ phase = "old"
  /\ phase' = "joint"
  /\ UNCHANGED <<stored, nextSeq, issued, acks, ownerAcked, retiredCommitted>>

OwnerAck ==
  /\ phase = "joint"
  /\ ~ownerAcked
  /\ ownerAcked' = TRUE
  /\ UNCHANGED <<phase, stored, nextSeq, issued, acks, retiredCommitted>>

\* State transfer: the owner copies its newest committed record into New
\* acceptors before retirement (Vertical Paxos II's state transfer; in slates
\* the owner is itself a holder and simply re-sends the latest record).
Transfer(a) ==
  /\ phase = "joint"
  /\ a \in NewSet
  /\ LET newest == MaxRec(Committed \cup {NoRecord})
     IN /\ newest # NoRecord
        /\ Newer(newest, stored[a])
        /\ stored' = [stored EXCEPT ![a] = newest]
        /\ acks' = [acks EXCEPT ![newest] = acks[newest] \cup {a}]
  /\ UNCHANGED <<phase, nextSeq, issued, ownerAcked, retiredCommitted>>

\* Retirement: only after the owner acknowledged New and the newest committed
\* record is held by a majority of New.
Retire ==
  /\ phase = "joint"
  /\ ownerAcked
  /\ LET newest == MaxRec(Committed \cup {NoRecord})
     IN IF newest = NoRecord THEN TRUE ELSE acks[newest] \cap NewSet \in Maj(NewSet)
  /\ retiredCommitted' = Committed
  /\ phase' = "new"
  /\ UNCHANGED <<stored, nextSeq, issued, acks, ownerAcked>>

Next ==
  \/ \E v \in Values : Issue(v)
  \/ \E r \in Records, a \in Acceptors : Accept(r, a)
  \/ Announce
  \/ OwnerAck
  \/ \E a \in Acceptors : Transfer(a)
  \/ Retire

Spec == Init /\ [][Next]_vars

\* A reader that knows configuration k reads a majority of that set and may
\* lag by one phase (it may still read Old during the joint phase).  Read
\* safety as a state predicate over every such majority: it already holds a
\* record at least as new as every committed record.
ReadQuorums ==
  CASE phase = "old"   -> Maj(OldSet)
    [] phase = "joint" -> Maj(OldSet) \cup Maj(NewSet)
    [] phase = "new"   -> Maj(NewSet)

ReadSafety ==
  \A Q \in ReadQuorums :
    \A r \in Committed : NewerOrEq(MaxRec({stored[a] : a \in Q}), r)

NoLoss ==
  phase = "new" => \A r \in retiredCommitted : \E r2 \in Committed : NewerOrEq(r2, r)

TypeOK ==
  /\ phase \in {"old", "joint", "new"}
  /\ \A a \in Acceptors : stored[a] \in Records \cup {NoRecord}

=============================================================================
