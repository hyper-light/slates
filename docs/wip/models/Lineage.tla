------------------------------- MODULE Lineage -------------------------------
(***************************************************************************)
(* slates, design model 3 (A-309, 2026-11-18; A-272 (ii)): where a retired *)
(* owner's objects go, and when a holder may let its copy go.  Extended by *)
(* A-518 (2026-11-19) with A-341, A-484 and A-508.                          *)
(*                                                                          *)
(* Written as text.  Main runs TLC on it (MC_Lineage.tla with             *)
(* MC_Lineage.cfg, run-requests.md of 2026-11-19); a counterexample comes  *)
(* back as a new piece, and the result is recorded here and in GAPS'       *)
(* model table.  Its executable counterpart is                             *)
(* crates/db/tests/lineage_model.rs, which drives the real                 *)
(* RegionalConfiguration through generated histories (its module doc      *)
(* lists the defects it found).                                            *)
(*                                                                          *)
(* The region's council keeps, per retired host, a retirement: the         *)
(* neighbourhoods its records were placed on (settled, and current while a *)
(* change was in flight), its survivors (the members of those               *)
(* neighbourhoods when it retired, and of the takeovers it carries), the   *)
(* survivors that confirmed their share, and the earlier takeovers it      *)
(* carries (those it left owing a share of).  Each of its objects is       *)
(* ranked to one survivor: among the survivors in the object's cohorts,    *)
(* else among all of them.  A survivor's round asks the members of the     *)
(* recovery neighbourhoods, learns each copy whose line falls to it,       *)
(* adopts each learned object (a newer record placed under its own         *)
(* placement) and confirms its share only when every object it learned is *)
(* adopted (A-272).  A retirement completes, and becomes a completion      *)
(* record, when it had survivors and each confirmed or left and carried it *)
(* into a takeover that completed (A-309).  The record keeps the settled   *)
(* neighbourhoods the takeover recovered through and the survivors that    *)
(* were still members when it completed (A-484).                           *)
(*                                                                          *)
(* A holder resolves each copy it keeps from the configuration.  A line    *)
(* that names a successor moves the copy's routing to it.  It lets the     *)
(* copy go under four rules, each an action below, each protecting the     *)
(* properties named with it:                                                *)
(*   1. AskedSettled (A-309): the line ends at a completion record and the *)
(*      holder took part in that takeover.  ReclaimOnlyStale, NoLastCopyLost.*)
(*   2. AskedConfirmed (A-309): the copy's successor confirmed, and the    *)
(*      holder, asked in that round, is outside its placement.             *)
(*      ReclaimOnlyStale, NoLastCopyLost.                                  *)
(*   3. UnaskedCovered (A-341, A-484): the holder no round asked lets the  *)
(*      copy go where the first takeover since the copy was written, its   *)
(*      writer's, recovered the object within tolerance with its share     *)
(*      done, judged on that takeover's kept retirement or its completion  *)
(*      record.  Judged by the last takeover in the line instead, a copy   *)
(*      whose writer's takeover was past tolerance went at a later takeover *)
(*      within tolerance (the bug record of 2026-11-18).                    *)
(*      ReclaimOnlyStale, NoLastCopyLost.                                  *)
(*   4. SettledAway (A-508): a live owner's copy that the owner wrote, held *)
(*      outside all of the owner's neighbourhoods, goes where the owner's  *)
(*      settled cohort is whole and at least F + 1 wide.                   *)
(*      SettleKeepsFPlusOne, NoLastCopyLost.                               *)
(* Every other verdict keeps the copy.                                      *)
(*                                                                          *)
(* The properties:                                                          *)
(*   ReclaimOnlyStale     a copy let go on a takeover's line (rules 1-3)    *)
(*                        was older than an adoption of its object: a      *)
(*                        newer record exists (A-309's oracle);            *)
(*   SettleKeepsFPlusOne  a live owner's copy let go (rule 4), whose record *)
(*                        a commit reached, leaves a record at least as new *)
(*                        on F + 1 other members (A-508's guarantee);      *)
(*   NoLastCopyLost       no let-go removes the last member copy that the  *)
(*                        object's newest committed record names (one at   *)
(*                        least as new): whatever rule fired, a committed  *)
(*                        record keeps a copy;                             *)
(* and the invariants TypeOK, SettledOnRecord (a line reads settled only at*)
(* a completion record) and Bounded (the kept retirements and completion   *)
(* records stay within the bound, the records a kept retirement needs      *)
(* aside).  A retirement's own loss of copies is the failure being         *)
(* modelled, never a let-go: the properties judge only members' let-gos.  *)
(*                                                                          *)
(* Abstractions: a cohort is a whole neighbourhood (copysets narrow it in  *)
(* the code, which changes no rule here); rendezvous is a fixed weight per *)
(* object and host; the counting rule is takeover_model's subject, so the  *)
(* round hears every member at once; a record is its object's adoption     *)
(* count and a sequence, the newer by adoption, then by sequence; content  *)
(* and explicit moves are not modelled (rule 3 and rule 4 refuse moved     *)
(* objects in the code, and keep content a head names).                    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Hosts,      \* every member id there will ever be (a restart joins under a new one)
          Objects,    \* the objects
          Creator,    \* [Objects -> Hosts]: the host that created each object
          Weight,     \* [Objects -> [Hosts -> Nat]]: rendezvous weights, distinct per object
          F,          \* the faults a cohort tolerates
          Bound,      \* the most retirements and completion records kept (one per member in the code)
          MaxSeq,     \* the most records written per object (the state space's bound on writes)
          None        \* a model value: no host, no copy

ASSUME None \notin Hosts /\ None \notin Nat
ASSUME F \in Nat /\ Bound \in Nat /\ Bound >= 1 /\ MaxSeq \in Nat
ASSUME Creator \in [Objects -> Hosts]

VARIABLES
  members,     \* the region's members
  joined,      \* every host that ever was a member
  settled,     \* [Hosts -> SUBSET Hosts]: each member's settled neighbourhood
  current,     \* [Hosts -> SUBSET Hosts]: each member's current neighbourhood
  kept,        \* the hosts whose retirement is kept
  ret,         \* [Hosts -> retirement record]: read only on kept
  done,        \* the hosts whose takeover has a completion record
  doneBy,      \* [Hosts -> SUBSET Hosts]: each completion record's confirmed survivors
  doneAt,      \* [Hosts -> Nat]: the version each record was made at
  doneCohorts, \* [Hosts -> SUBSET SUBSET Hosts]: each record's recovery cohorts (A-484: the settled neighbourhoods)
  doneHeld,    \* [Hosts -> SUBSET Hosts]: each record's survivors still members at completion (A-484)
  version,     \* the configuration version
  owner,       \* [Objects -> Hosts]: the host that wrote each object's newest record
  adopted,     \* [Objects -> Nat]: how many times each object was adopted
  nextSeq,     \* [Objects -> Nat]: each object's next record sequence
  cmt,         \* [Objects -> [a, s]]: each object's newest committed record (ground truth, never read by a rule)
  copy,        \* [Hosts -> [Objects -> Nat \cup {None}]]: the adoption count a copy was written at
  seq,         \* [Hosts -> [Objects -> Nat]]: the sequence of a copy's record
  writer,      \* [Hosts -> [Objects -> Hosts \cup {None}]]: the owner that wrote a copy (A-484, Acceptor::written_by)
  route,       \* [Hosts -> [Objects -> Hosts]]: the owner a holder routes its copy to
  movedFrom,   \* [Hosts -> [Objects -> Hosts \cup {None}]]: the retired owner an install moved it from
  departed     \* [Hosts -> [Objects -> Hosts \cup {None}]]: the takeover a copy is recovered in

vars == <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld, version,
          owner, adopted, nextSeq, cmt, copy, seq, writer, route, movedFrom, departed>>

Max(a, b) == IF a >= b THEN a ELSE b
Min(a, b) == IF a <= b THEN a ELSE b

(* ---------------------------------------------------------------------- *)
(* The configuration's rules (crates/db/src/register.rs).                 *)
(* ---------------------------------------------------------------------- *)

\* A member still owes a confirmation of d's takeover.
Owes(d, h) == d \in kept /\ h \in members /\ h \in ret[d].survivors /\ h \notin ret[d].confirmed

\* d and every kept retirement it carries, transitively (recovery_owners).
RECURSIVE Closure(_, _)
Closure(frontier, acc) ==
  IF frontier = {} THEN acc
  ELSE LET x == CHOOSE y \in frontier : TRUE
       IN Closure((frontier \ {x}) \cup ((ret[x].carried \cap kept) \ (acc \cup {x})), acc \cup {x})
RecoveryOwners(d) == Closure({d} \cap kept, {})

\* The hosts that may hold d's records, and among whose members its round asks (recovery_hosts).
RecoveryHosts(d) == UNION {ret[x].settled \cup ret[x].current : x \in RecoveryOwners(d)}
\* The object's cohorts: one per recovery owner (recovery_cohorts), a whole settled neighbourhood here.
Cohorts(d) == {ret[x].settled : x \in RecoveryOwners(d)}

\* The successor ranked for o in d's takeover: among the survivors in its cohorts, else among all the
\* survivors (A-309), fixed at the retirement.
Ranked(d) == LET inCohorts == (UNION Cohorts(d)) \cap ret[d].survivors
             IN IF inCohorts # {} THEN inCohorts ELSE ret[d].survivors
First(S, o) == CHOOSE s \in S : \A t \in S : Weight[o][s] >= Weight[o][t]
Successor(d, o) == IF Ranked(d) = {} THEN None ELSE First(Ranked(d), o)

\* How a line ends at a host no kept retirement is of and no member (line_end).
LineEnd(h) == IF h \in done THEN [kind |-> "settled", takeover |-> h] ELSE [kind |-> "unresolved"]

\* Where o, last owned by h, belongs now (lineage).
RECURSIVE Walk(_, _, _)
Walk(h, o, steps) ==
  IF steps = 0 THEN [kind |-> "unresolved"]
  ELSE IF h \notin kept THEN LineEnd(h)
  ELSE LET s == Successor(h, o)
       IN IF s = None THEN [kind |-> "lost"]
          ELSE IF s \in kept THEN Walk(s, o, steps - 1)
          ELSE IF s \in members THEN [kind |-> "successor", departed |-> h, successor |-> s]
          ELSE LineEnd(s)
Lineage(h, o) == Walk(h, o, Cardinality(Hosts) + 1)

\* A holder's copy falls to s in d's round: its owner's line, or the line of the owner an install moved
\* its authority from, ends there with s the successor (copy_falls_to).
FallsTo(x, o, d, s) ==
  LET l == Lineage(x, o) IN l.kind = "successor" /\ l.departed = d /\ l.successor = s
Falls(h, o, d, s) ==
  /\ copy[h][o] # None
  /\ \/ FallsTo(route[h][o], o, d, s)
     \/ (movedFrom[h][o] # None /\ FallsTo(movedFrom[h][o], o, d, s))

\* The members s's round asks, itself among them, and the objects it learns from them.
Asked(d, s) == (RecoveryHosts(d) \cap members) \cup {s}
Learned(d, s) == {o \in Objects : \E h \in Asked(d, s) : Falls(h, o, d, s)}

\* The acknowledgements a cohort C needs for a commit (Quorum::committed_within): F + 1 of it, or every host
\* of a smaller one, and at least one; and the promises a round needs from it (Quorum::recovery).
CommitQuorum(C) == Max(Min(F + 1, Cardinality(C)), 1)
RecoveryQuorum(C) == Max(Cardinality(C) - F, 1)

\* Whether a write by x acknowledged by acks commits: each of x's cohorts (both while a change is in
\* flight, the joint write) holds its commit quorum (Placement::placed_with).
Placed(x, acks) == \A C \in {settled[x], current[x]} : Cardinality(acks \cap C) >= CommitQuorum(C)

\* d's takeover recovered its objects within tolerance (RegionalConfiguration::within_tolerance, A-341,
\* A-484): each of its cohorts keeps its recovery quorum of the hosts that stayed members through the
\* takeover: the survivors still members while the retirement is kept, the record's held hosts after.
WithinTolerance(d) ==
  IF d \in kept
  THEN \A C \in Cohorts(d) : Cardinality(C \cap members \cap ret[d].survivors) >= RecoveryQuorum(C)
  ELSE /\ d \in done
       /\ doneCohorts[d] # {}
       /\ \A C \in doneCohorts[d] : Cardinality(C \cap doneHeld[d]) >= RecoveryQuorum(C)

\* o's share of d's takeover is done (confirmed_for, A-484): the takeover completed, or while it is kept
\* o's successor there confirmed, or left owing it and its own takeover, which carried it, completed.
ConfirmedFor(d, o) ==
  IF d \in kept
  THEN LET s == Successor(d, o) IN s # None /\ (s \in ret[d].confirmed \/ s \in done)
  ELSE d \in done

\* A copy w wrote is stale wherever it is held (copy_stale, A-484): w's takeover, the first since the copy
\* was written, recovered the object within tolerance with its share done.
CopyStale(w, o) == w # None /\ ConfirmedFor(w, o) /\ WithinTolerance(w)

\* d's retirement completes (completes): it had survivors, nobody owes, nobody carries it, and each
\* survivor confirmed or left and carried it into a takeover that completed.
Carried(d) == \E x \in kept : d \in ret[x].carried
Completes(d) ==
  /\ d \in kept
  /\ ret[d].survivors # {}
  /\ ~\E h \in Hosts : Owes(d, h)
  /\ ~Carried(d)
  /\ \A s \in ret[d].survivors : s \in ret[d].confirmed \/ s \in done

\* The completion records a kept retirement still needs: a survivor's that left owing its share and carried it.
NeededRecords == {s \in done : \E x \in kept : s \in ret[x].survivors \ ret[x].confirmed}

\* Where s places o (both neighbourhoods while a change is in flight), among the members.
Placement(s) == (settled[s] \cup current[s]) \cap members

\* A live owner's copy held by h is settled away (settled_away, A-508): h is in none of r's
\* neighbourhoods, so no takeover of r would ask it, and r's settled cohort is whole and F + 1 wide.
SettledAway(r, h) ==
  /\ r \in members
  /\ h \notin settled[r] \cup current[r]
  /\ Cardinality(settled[r]) >= F + 1
  /\ settled[r] \subseteq members

\* Whether h's copy of o is at least as new as the record (a, s): a later adoption, or the same one and a
\* sequence no lower.
AtLeastAsNew(h, o, a, s) ==
  /\ copy[h][o] # None
  /\ \/ copy[h][o] > a
     \/ (copy[h][o] = a /\ seq[h][o] >= s)

\* Whether o's newest committed record is at least as new as the record (a, s): the record, or a newer one, was
\* acknowledged. A record no commit reaches was never acknowledged, and the design may lose it.
CommittedAtLeast(o, a, s) == cmt[o].a > a \/ (cmt[o].a = a /\ cmt[o].s >= s)

(* ---------------------------------------------------------------------- *)
(* Actions.                                                               *)
(* ---------------------------------------------------------------------- *)

Init ==
  /\ joined = {Creator[o] : o \in Objects}
  /\ members = joined
  /\ settled = [h \in Hosts |-> joined]
  /\ current = [h \in Hosts |-> joined]
  /\ kept = {}
  /\ ret = [h \in Hosts |-> [settled |-> {}, current |-> {}, survivors |-> {}, confirmed |-> {},
                             carried |-> {}, at |-> 0]]
  /\ done = {}
  /\ doneBy = [h \in Hosts |-> {}]
  /\ doneAt = [h \in Hosts |-> 0]
  /\ doneCohorts = [h \in Hosts |-> {}]
  /\ doneHeld = [h \in Hosts |-> {}]
  /\ version = 0
  /\ owner = Creator
  /\ adopted = [o \in Objects |-> 0]
  \* The formation's record of each object, sequence 0, placed on every first member.
  /\ nextSeq = [o \in Objects |-> 1]
  /\ cmt = [o \in Objects |-> [a |-> 0, s |-> 0]]
  /\ copy = [h \in Hosts |-> [o \in Objects |-> IF h \in joined THEN 0 ELSE None]]
  /\ seq = [h \in Hosts |-> [o \in Objects |-> 0]]
  /\ writer = [h \in Hosts |-> [o \in Objects |-> IF h \in joined THEN Creator[o] ELSE None]]
  /\ route = [h \in Hosts |-> [o \in Objects |-> Creator[o]]]
  /\ movedFrom = [h \in Hosts |-> [o \in Objects |-> None]]
  /\ departed = [h \in Hosts |-> [o \in Objects |-> None]]

\* The owner of o writes a newer record to a subset of its placement holding itself (the record plane): the
\* acknowledging hosts hold it, written by the owner, and it is the newest committed record once placed.
Write(o) ==
  /\ owner[o] \in members
  /\ nextSeq[o] <= MaxSeq
  /\ \E acks \in SUBSET Placement(owner[o]) :
       /\ owner[o] \in acks
       /\ copy' = [h \in Hosts |-> IF h \in acks THEN [copy[h] EXCEPT ![o] = adopted[o]] ELSE copy[h]]
       /\ seq' = [h \in Hosts |-> IF h \in acks THEN [seq[h] EXCEPT ![o] = nextSeq[o]] ELSE seq[h]]
       /\ writer' = [h \in Hosts |-> IF h \in acks THEN [writer[h] EXCEPT ![o] = owner[o]] ELSE writer[h]]
       /\ route' = [h \in Hosts |-> IF h \in acks THEN [route[h] EXCEPT ![o] = owner[o]] ELSE route[h]]
       /\ movedFrom' = [h \in Hosts |-> IF h \in acks THEN [movedFrom[h] EXCEPT ![o] = None] ELSE movedFrom[h]]
       /\ departed' = [h \in Hosts |-> IF h \in acks THEN [departed[h] EXCEPT ![o] = None] ELSE departed[h]]
       /\ cmt' = IF Placed(owner[o], acks)
                 THEN [cmt EXCEPT ![o] = [a |-> adopted[o], s |-> nextSeq[o]]]
                 ELSE cmt
  /\ nextSeq' = [nextSeq EXCEPT ![o] = @ + 1]
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld,
                 version, owner, adopted>>

\* A fresh host joins, holding nothing, settled on itself.
Join(h) ==
  /\ h \notin joined
  /\ joined' = joined \cup {h}
  /\ members' = members \cup {h}
  /\ settled' = [settled EXCEPT ![h] = {h}]
  /\ current' = [current EXCEPT ![h] = {h}]
  /\ version' = version + 1
  /\ UNCHANGED <<kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld, owner, adopted, nextSeq, cmt, copy,
                 seq, writer, route, movedFrom, departed>>

\* A member's neighbourhood moves (a change in flight until it settles).
Rebalance(h, N) ==
  /\ h \in members /\ h \in N /\ N \subseteq members
  /\ current' = [current EXCEPT ![h] = N]
  /\ version' = version + 1
  /\ UNCHANGED <<members, joined, settled, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld, owner,
                 adopted, nextSeq, cmt, copy, seq, writer, route, movedFrom, departed>>

\* A member settles its current neighbourhood once the newest record of every object it owns is on the
\* commit quorum of it (settle's report, placed_on_current).
Settle(h) ==
  /\ h \in members
  /\ \A o \in Objects : owner[o] = h =>
       Cardinality({x \in current[h] \cap members :
                      copy[x][o] = adopted[o] /\ seq[x][o] = nextSeq[o] - 1}) >= CommitQuorum(current[h])
  /\ settled' = [settled EXCEPT ![h] = current[h]]
  /\ version' = version + 1
  /\ UNCHANGED <<members, joined, current, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld, owner,
                 adopted, nextSeq, cmt, copy, seq, writer, route, movedFrom, departed>>

\* The council takes d over: its copies are gone, its retirement is kept (retire). The bound runs in the
\* same change in the code; here a retirement waits until the bound has been applied to the last one.
Retire(d) ==
  /\ d \in members
  /\ Cardinality(kept) <= Bound
  /\ LET carried == {x \in kept : Owes(x, d)}
         survivors == ((settled[d] \cup current[d] \cup UNION {RecoveryHosts(x) : x \in carried})
                       \cap members) \ {d}
     IN ret' = [ret EXCEPT ![d] = [settled |-> settled[d], current |-> current[d],
                                   survivors |-> survivors, confirmed |-> {}, carried |-> carried,
                                   at |-> version + 1]]
  /\ kept' = kept \cup {d}
  /\ members' = members \ {d}
  /\ copy' = [copy EXCEPT ![d] = [o \in Objects |-> None]]
  /\ version' = version + 1
  /\ UNCHANGED <<joined, settled, current, done, doneBy, doneAt, doneCohorts, doneHeld, owner, adopted,
                 nextSeq, cmt, seq, writer, route, movedFrom, departed>>

\* s adopts o in d's round: a newer record, written by s and placed under s's placement, which commits
\* before s confirms (a successor confirms only once what it learned is adopted and placed). Within
\* tolerance on its cohorts' quorums; past it once every surviving member of its cohorts answered, which
\* every member's visible state stands for here.
Adopt(d, s, o) ==
  /\ Owes(d, s)
  /\ o \in Learned(d, s)
  /\ owner[o] # s
  /\ nextSeq[o] <= MaxSeq
  /\ \E acks \in SUBSET Placement(s) :
       /\ s \in acks
       /\ Placed(s, acks)
       /\ copy' = [h \in Hosts |-> IF h \in acks THEN [copy[h] EXCEPT ![o] = adopted[o] + 1] ELSE copy[h]]
       /\ seq' = [h \in Hosts |-> IF h \in acks THEN [seq[h] EXCEPT ![o] = nextSeq[o]] ELSE seq[h]]
       /\ writer' = [h \in Hosts |-> IF h \in acks THEN [writer[h] EXCEPT ![o] = s] ELSE writer[h]]
       /\ route' = [h \in Hosts |-> IF h \in acks THEN [route[h] EXCEPT ![o] = s] ELSE route[h]]
       /\ movedFrom' = [h \in Hosts |-> IF h \in acks THEN [movedFrom[h] EXCEPT ![o] = None] ELSE movedFrom[h]]
       /\ departed' = [h \in Hosts |-> IF h \in acks THEN [departed[h] EXCEPT ![o] = None] ELSE departed[h]]
  /\ adopted' = [adopted EXCEPT ![o] = @ + 1]
  /\ owner' = [owner EXCEPT ![o] = s]
  /\ cmt' = [cmt EXCEPT ![o] = [a |-> adopted[o] + 1, s |-> nextSeq[o]]]
  /\ nextSeq' = [nextSeq EXCEPT ![o] = @ + 1]
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld,
                 version>>

\* s confirms its share of d's takeover once every object it learned is its own (A-272).
Confirm(d, s) ==
  /\ Owes(d, s)
  /\ \A o \in Learned(d, s) : owner[o] = s
  /\ ret' = [ret EXCEPT ![d].confirmed = @ \cup {s}]
  /\ version' = version + 1
  /\ UNCHANGED <<members, joined, settled, current, kept, done, doneBy, doneAt, doneCohorts, doneHeld, owner,
                 adopted, nextSeq, cmt, copy, seq, writer, route, movedFrom, departed>>

\* A retirement that completes becomes its completion record (complete_retirements), which keeps the
\* settled neighbourhoods it recovered through and the survivors still members (A-484). As for Retire,
\* the records' bound has been applied to the last one first.
Complete(d) ==
  /\ Completes(d)
  /\ Cardinality(done) <= Max(Bound, Cardinality(NeededRecords))
  /\ kept' = kept \ {d}
  /\ done' = done \cup {d}
  /\ doneBy' = [doneBy EXCEPT ![d] = ret[d].confirmed]
  /\ doneAt' = [doneAt EXCEPT ![d] = version]
  /\ doneCohorts' = [doneCohorts EXCEPT ![d] = Cohorts(d)]
  /\ doneHeld' = [doneHeld EXCEPT ![d] = ret[d].survivors \cap members]
  /\ UNCHANGED <<members, joined, settled, current, ret, version, owner, adopted, nextSeq, cmt, copy, seq,
                 writer, route, movedFrom, departed>>

\* Past the bound the council lets a retirement go, every kept retirement that carries it with it
\* (bound_retirements): one nobody owes and nobody carries first, then the oldest.
Pending(d) == (\E h \in Hosts : Owes(d, h)) \/ Carried(d)
DropRetirement(d) ==
  /\ Cardinality(kept) > Bound
  /\ d \in kept
  /\ \A x \in kept : \/ (Pending(d) = Pending(x) /\ ret[d].at <= ret[x].at)
                     \/ (~Pending(d) /\ Pending(x))
  /\ kept' = {x \in kept : d \notin RecoveryOwners(x)}
  /\ UNCHANGED <<members, joined, settled, current, ret, done, doneBy, doneAt, doneCohorts, doneHeld, version,
                 owner, adopted, nextSeq, cmt, copy, seq, writer, route, movedFrom, departed>>

\* Past the bound the council lets the oldest completion record go, never one a kept retirement needs
\* (bound_completed).
EvictRecord(d) ==
  /\ Cardinality(done) > Max(Bound, Cardinality(NeededRecords))
  /\ d \in done \ NeededRecords
  /\ \A x \in done \ NeededRecords : doneAt[d] <= doneAt[x]
  /\ done' = done \ {d}
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, doneBy, doneAt, doneCohorts, doneHeld, version,
                 owner, adopted, nextSeq, cmt, copy, seq, writer, route, movedFrom, departed>>

\* The judgment of a successor's confirmation (stale_after_confirmation): h's copy, routed to the live r on
\* d's takeover line, with r's share confirmed in the kept retirement or its record and h outside r's placement.
Confirmed(h, o) ==
  LET d == departed[h][o]
      r == route[h][o]
  IN /\ d # None
     /\ r # h
     /\ h \notin Placement(r)
     /\ \/ (d \in kept /\ r \in ret[d].confirmed)
        \/ (d \in done /\ r \in doneBy[d])
\* Whether h was asked in that round: one of the retirement's survivors, or a confirmed one on the record.
AskedIn(h, o) ==
  LET d == departed[h][o]
  IN \/ (d \in kept /\ h \in ret[d].survivors)
     \/ (d \in done /\ h \in doneBy[d])

\* A member lets a copy go.
Reclaim(h, o) ==
  /\ copy' = [copy EXCEPT ![h][o] = None]
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld,
                 version, owner, adopted, nextSeq, cmt, seq, writer, route, movedFrom, departed>>

\* Rule 1 (A-309): the line ends at a completion record of a takeover h took part in.
AskedSettled(h, o) ==
  LET l == Lineage(route[h][o], o)
  IN /\ route[h][o] \notin members \/ route[h][o] \in kept
     /\ l.kind = "settled"
     /\ h \in doneBy[l.takeover]
     /\ Reclaim(h, o)

\* Rule 2 (A-309): the successor confirmed, and h, asked in its round, is outside its placement.
AskedConfirmed(h, o) ==
  /\ route[h][o] \in members /\ route[h][o] \notin kept
  /\ Confirmed(h, o)
  /\ AskedIn(h, o)
  /\ Reclaim(h, o)

\* Rule 3 (A-341, A-484): a copy no round asked h for, judged by its writer's takeover, on a successor's
\* confirmation or on a line ending at a completion record.
UnaskedCovered(h, o) ==
  /\ CopyStale(writer[h][o], o)
  /\ \/ /\ route[h][o] \in members /\ route[h][o] \notin kept
        /\ Confirmed(h, o)
        /\ ~AskedIn(h, o)
     \/ LET l == Lineage(route[h][o], o)
        IN /\ route[h][o] \notin members \/ route[h][o] \in kept
           /\ l.kind = "settled"
           /\ h \notin doneBy[l.takeover]
  /\ Reclaim(h, o)

\* Rule 4 (A-508): a live owner's copy the owner wrote, held outside all of its neighbourhoods, where its
\* settled cohort is whole and F + 1 wide.
SettledAwayRule(h, o) ==
  /\ route[h][o] \in members /\ route[h][o] \notin kept
  /\ writer[h][o] = route[h][o]
  /\ SettledAway(route[h][o], h)
  /\ Reclaim(h, o)

\* A line that names a successor moves the copy's routing to it (resolve_held_objects, reconcile_held_authority).
Reroute(h, o) ==
  LET r == route[h][o]
      l == Lineage(r, o)
  IN /\ r \notin members \/ r \in kept
     /\ l.kind = "successor"
     /\ l.successor # r
     /\ route' = [route EXCEPT ![h][o] = l.successor]
     /\ movedFrom' = [movedFrom EXCEPT ![h][o] = IF r \in kept THEN r ELSE movedFrom[h][o]]
     /\ departed' = [departed EXCEPT ![h][o] = l.departed]
     /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, doneCohorts, doneHeld,
                    version, owner, adopted, nextSeq, cmt, copy, seq, writer>>

\* A member resolves one copy at an install.
Resolve(h, o) ==
  /\ h \in members
  /\ copy[h][o] # None
  /\ \/ Reroute(h, o)
     \/ AskedSettled(h, o)
     \/ AskedConfirmed(h, o)
     \/ UnaskedCovered(h, o)
     \/ SettledAwayRule(h, o)

Next ==
  \/ \E o \in Objects : Write(o)
  \/ \E h \in Hosts : Join(h) \/ Retire(h) \/ Settle(h) \/ Complete(h) \/ DropRetirement(h) \/ EvictRecord(h)
  \/ \E h \in Hosts, N \in SUBSET Hosts : Rebalance(h, N)
  \/ \E d, s \in Hosts, o \in Objects : Adopt(d, s, o)
  \/ \E d, s \in Hosts : Confirm(d, s)
  \/ \E h \in Hosts, o \in Objects : Resolve(h, o)

Spec == Init /\ [][Next]_vars

(* ---------------------------------------------------------------------- *)
(* Properties.                                                            *)
(* ---------------------------------------------------------------------- *)

\* h, a member before and after, let its copy of o go in this step (a retirement's loss is no let-go).
LetGo(h, o) == h \in members /\ h \in members' /\ copy[h][o] # None /\ copy'[h][o] = None

\* Whether the step let h's copy go under rule 4: a live owner's copy that owner wrote. Rules 1-3 judge only
\* copies on a takeover's line, which their writer no longer owns, so the rules never meet on one copy.
LiveOwnCopy(h, o) == route[h][o] \in members /\ route[h][o] \notin kept /\ writer[h][o] = route[h][o]

\* Rules 1-3: a copy let go on a takeover's line was older than an adoption of its object (A-309's oracle).
ReclaimOnlyStale ==
  [][\A h \in Hosts, o \in Objects :
       (LetGo(h, o) /\ ~LiveOwnCopy(h, o)) => adopted[o] > copy[h][o]]_vars

\* Rule 4: a live owner's copy let go, whose record a commit reached, leaves a record at least as new on F + 1
\* other members (A-508).
SettleKeepsFPlusOne ==
  [][\A h \in Hosts, o \in Objects :
       (LetGo(h, o) /\ LiveOwnCopy(h, o) /\ CommittedAtLeast(o, copy[h][o], seq[h][o]))
         => Cardinality({x \in members \ {h} : AtLeastAsNew(x, o, copy[h][o], seq[h][o])}) >= F + 1]_vars

\* Every rule: no let-go removes the last member copy that the object's newest committed record names.
NoLastCopyLost ==
  [][\A h \in Hosts, o \in Objects :
       (LetGo(h, o) /\ AtLeastAsNew(h, o, cmt[o].a, cmt[o].s))
         => \E x \in members \ {h} : AtLeastAsNew(x, o, cmt[o].a, cmt[o].s)]_vars

\* The variables keep their shapes.
TypeOK ==
  /\ members \subseteq Hosts /\ joined \subseteq Hosts /\ kept \subseteq Hosts /\ done \subseteq Hosts
  /\ \A o \in Objects :
       /\ owner[o] \in Hosts
       /\ adopted[o] \in Nat
       /\ nextSeq[o] \in Nat
       /\ cmt[o].a \in Nat
       /\ cmt[o].s \in Nat
  /\ \A h \in Hosts : doneHeld[h] \subseteq Hosts /\ \A C \in doneCohorts[h] : C \subseteq Hosts
  /\ \A h \in Hosts, o \in Objects :
       /\ copy[h][o] = None \/ copy[h][o] \in Nat
       /\ seq[h][o] \in Nat
       /\ writer[h][o] = None \/ writer[h][o] \in Hosts
       /\ route[h][o] \in Hosts

\* A line reads settled only at a completion record.
SettledOnRecord ==
  \A h \in Hosts, o \in Objects :
    LET l == Lineage(h, o) IN l.kind = "settled" => l.takeover \in done

\* The kept retirements and the completion records stay within the bound (one over it at most, until the
\* bound's own step), the records a kept retirement needs aside.
Bounded ==
  /\ Cardinality(kept) <= Bound + 1
  /\ Cardinality(done) <= Max(Bound, Cardinality(NeededRecords)) + 1

=============================================================================
