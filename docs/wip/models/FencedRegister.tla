---------------------------- MODULE FencedRegister ----------------------------
(***************************************************************************)
(* slates, design model 1 (2026-09-04): the fenced single-writer register   *)
(* under a configuration master.                                            *)
(*                                                                          *)
(* One register (a volume head, or one slot of a green chain).  A set of    *)
(* acceptors (the record holders of a placement neighbourhood).  Hosts      *)
(* become the owner of the register by being assigned an epoch by the       *)
(* configuration master, which stands in for the regional Raft group and is *)
(* modelled as a linearizable oracle that hands out strictly increasing     *)
(* epochs, one host per epoch.                                              *)
(*                                                                          *)
(* An owner first promotes (phase 1 of Vertical Paxos II): it asks every    *)
(* acceptor to raise its maxEpoch to the owner's epoch and to report the    *)
(* highest record it holds; once a majority has answered, the owner adopts  *)
(* the highest reported record as its starting point.  Then it writes       *)
(* records (phase 2): each record carries (epoch, seq, value) and is        *)
(* committed when a majority of acceptors have accepted it.  Acceptors      *)
(* refuse any message whose epoch is below their maxEpoch (the acceptor's   *)
(* maxBallot rule; Chubby's sequencer check; BookKeeper's fence).           *)
(*                                                                          *)
(* A paused-and-resumed owner is modelled by letting any host that once     *)
(* held an epoch keep issuing records with it forever.                      *)
(*                                                                          *)
(* Readers read a majority and take the record with the highest (epoch,     *)
(* seq).  The properties checked:                                           *)
(*   TotalOrder         no two different committed records share            *)
(*                      (epoch, seq);                                        *)
(*   Continuity         once an owner has adopted a base, that base is at   *)
(*                      least as new as every record that ever commits      *)
(*                      under a lower epoch (no lineage fork; this is the   *)
(*                      fencing guarantee Paxos actually gives: a record     *)
(*                      partially acknowledged before the promotion may     *)
(*                      still complete, but the new owner already holds it); *)
(*   StaleNeverCommits  a record issued by a stale owner after a higher     *)
(*                      epoch has its promotion majority never commits;     *)
(*   ReadSafety         a completed read returns a record at least as new   *)
(*                      as every record committed before the read started.  *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Acceptors,    \* the record holders, e.g. {a1, a2, a3}
          Hosts,        \* candidate owners, e.g. {h1, h2}
          Values,       \* payloads, e.g. {v1, v2}
          MaxEpoch,     \* epochs 1..MaxEpoch
          MaxSeq        \* records per epoch 1..MaxSeq

ASSUME MaxEpoch \in Nat /\ MaxEpoch >= 1
ASSUME MaxSeq \in Nat /\ MaxSeq >= 1

Majority == {Q \in SUBSET Acceptors : 2 * Cardinality(Q) > Cardinality(Acceptors)}

Epochs == 1..MaxEpoch
NoRecord == [epoch |-> 0, seq |-> 0, value |-> "none"]
Records == [epoch : Epochs, seq : 1..MaxSeq, value : Values]
AnyRecord == Records \cup {NoRecord}

\* Lexicographic order on (epoch, seq).
Newer(r1, r2) == r1.epoch > r2.epoch \/ (r1.epoch = r2.epoch /\ r1.seq > r2.seq)
NewerOrEq(r1, r2) == r1 = r2 \/ Newer(r1, r2)
MaxRec(S) == CHOOSE r \in S : \A s \in S : NewerOrEq(r, s)

VARIABLES
  epochOwner,   \* [Epochs -> Hosts \cup {"none"}]: the master's assignments
  nextEpoch,    \* the next epoch the master will hand out
  maxEpoch,     \* [Acceptors -> Nat]: the fence at each acceptor
  stored,       \* [Acceptors -> AnyRecord]: the highest record each acceptor holds
  promAcked,    \* [Epochs -> SUBSET Acceptors]: acceptors that answered phase 1
  promVal,      \* [Epochs -> [Acceptors -> AnyRecord]]: what each reported
  promoted,     \* [Epochs -> BOOLEAN]: the owner has adopted a start record
  base,         \* [Epochs -> AnyRecord]: the record the owner started from
  nextSeq,      \* [Epochs -> Nat]: next seq the owner of that epoch will use
  issued,       \* SUBSET Records: records an owner has issued
  stale,        \* SUBSET Records: records issued after a higher epoch's promotion majority
  writeAcks     \* [Records -> SUBSET Acceptors]: phase-2 acceptances

vars == <<epochOwner, nextEpoch, maxEpoch, stored, promAcked, promVal, promoted,
          base, nextSeq, issued, stale, writeAcks>>

Committed == {r \in Records : writeAcks[r] \in Majority}

PromotionMajority(e) == promAcked[e] \in Majority

Init ==
  /\ epochOwner = [e \in Epochs |-> "none"]
  /\ nextEpoch = 1
  /\ maxEpoch = [a \in Acceptors |-> 0]
  /\ stored = [a \in Acceptors |-> NoRecord]
  /\ promAcked = [e \in Epochs |-> {}]
  /\ promVal = [e \in Epochs |-> [a \in Acceptors |-> NoRecord]]
  /\ promoted = [e \in Epochs |-> FALSE]
  /\ base = [e \in Epochs |-> NoRecord]
  /\ nextSeq = [e \in Epochs |-> 1]
  /\ issued = {}
  /\ stale = {}
  /\ writeAcks = [r \in Records |-> {}]

(***************************************************************************)
(* The configuration master (the regional Raft group) assigns the next     *)
(* epoch to a host.  It never reuses an epoch and never assigns two hosts   *)
(* the same epoch: that is what linearizable consensus buys.               *)
(***************************************************************************)
MasterAssign(h) ==
  /\ nextEpoch <= MaxEpoch
  /\ epochOwner' = [epochOwner EXCEPT ![nextEpoch] = h]
  /\ nextEpoch' = nextEpoch + 1
  /\ UNCHANGED <<maxEpoch, stored, promAcked, promVal, promoted, base, nextSeq,
                 issued, stale, writeAcks>>

(***************************************************************************)
(* Phase 1: an acceptor answers a promotion for epoch e by raising its      *)
(* fence and reporting its highest record.  Refused if e is below its       *)
(* fence.  Modelled per acceptor so that every interleaving is explored.    *)
(***************************************************************************)
PromoteAck(e, a) ==
  /\ epochOwner[e] # "none"
  /\ a \notin promAcked[e]
  /\ e >= maxEpoch[a]
  /\ maxEpoch' = [maxEpoch EXCEPT ![a] = e]
  /\ promAcked' = [promAcked EXCEPT ![e] = promAcked[e] \cup {a}]
  /\ promVal' = [promVal EXCEPT ![e][a] = stored[a]]
  /\ UNCHANGED <<epochOwner, nextEpoch, stored, promoted, base, nextSeq,
                 issued, stale, writeAcks>>

(***************************************************************************)
(* The owner of epoch e adopts the highest record reported by a majority.   *)
(***************************************************************************)
Adopt(e) ==
  /\ epochOwner[e] # "none"
  /\ ~promoted[e]
  /\ PromotionMajority(e)
  /\ base' = [base EXCEPT ![e] = MaxRec({promVal[e][a] : a \in promAcked[e]})]
  /\ promoted' = [promoted EXCEPT ![e] = TRUE]
  /\ UNCHANGED <<epochOwner, nextEpoch, maxEpoch, stored, promAcked, promVal,
                 nextSeq, issued, stale, writeAcks>>

(***************************************************************************)
(* Phase 2: the owner of epoch e issues its next record with a fixed value. *)
(* A resumed stale owner is an owner of an old epoch still issuing; if a    *)
(* higher epoch already has its promotion majority, the record is stale.    *)
(***************************************************************************)
Issue(e, v) ==
  /\ promoted[e]
  /\ nextSeq[e] <= MaxSeq
  /\ LET r == [epoch |-> e, seq |-> nextSeq[e], value |-> v]
     IN /\ issued' = issued \cup {r}
        /\ stale' = IF \E e2 \in Epochs : e2 > e /\ PromotionMajority(e2)
                      THEN stale \cup {r} ELSE stale
  /\ nextSeq' = [nextSeq EXCEPT ![e] = nextSeq[e] + 1]
  /\ UNCHANGED <<epochOwner, nextEpoch, maxEpoch, stored, promAcked, promVal,
                 promoted, base, writeAcks>>

Accept(r, a) ==
  /\ r \in issued
  /\ a \notin writeAcks[r]
  /\ r.epoch >= maxEpoch[a]
  /\ maxEpoch' = [maxEpoch EXCEPT ![a] = r.epoch]
  /\ stored' = [stored EXCEPT ![a] = IF Newer(r, stored[a]) THEN r ELSE stored[a]]
  /\ writeAcks' = [writeAcks EXCEPT ![r] = writeAcks[r] \cup {a}]
  /\ UNCHANGED <<epochOwner, nextEpoch, promAcked, promVal, promoted, base,
                 nextSeq, issued, stale>>

Next ==
  \/ \E h \in Hosts : MasterAssign(h)
  \/ \E e \in Epochs, a \in Acceptors : PromoteAck(e, a)
  \/ \E e \in Epochs : Adopt(e)
  \/ \E e \in Epochs, v \in Values : Issue(e, v)
  \/ \E r \in Records, a \in Acceptors : Accept(r, a)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Properties.                                                              *)
(***************************************************************************)

TotalOrder ==
  \A r1, r2 \in Committed :
    (r1.epoch = r2.epoch /\ r1.seq = r2.seq) => r1 = r2

Continuity ==
  \A e \in Epochs :
    promoted[e] =>
      \A r \in Committed : r.epoch < e => NewerOrEq(base[e], r)

StaleNeverCommits ==
  stale \cap Committed = {}

(***************************************************************************)
(* A read takes the newest record among any majority.  Because a read is    *)
(* atomic here (the pessimistic case: a longer read can only see more), read *)
(* safety is a state predicate: every majority already holds a record at    *)
(* least as new as every committed record.                                   *)
(***************************************************************************)
ReadSafety ==
  \A Q \in Majority :
    \A r \in Committed : NewerOrEq(MaxRec({stored[a] : a \in Q}), r)

TypeOK ==
  /\ nextEpoch \in 1..(MaxEpoch + 1)
  /\ \A a \in Acceptors : maxEpoch[a] \in 0..MaxEpoch
  /\ \A a \in Acceptors : stored[a] \in AnyRecord
  /\ \A e \in Epochs : base[e] \in AnyRecord
  /\ issued \subseteq Records
  /\ stale \subseteq issued

=============================================================================
