------------------------------- MODULE Lineage -------------------------------
(***************************************************************************)
(* slates, design model 3 (2026-11-18, A-309; A-272 (ii)): where a retired *)
(* owner's objects go, and when a holder may let its copy go.              *)
(*                                                                          *)
(* READ AS TEXT ONLY.  CLAUDE.md §2 item 13 keeps TLA+ tooling off Ada's     *)
(* machines and out of CI, so no model checker has run on this file.  Its  *)
(* executable counterpart is crates/db/tests/lineage_model.rs: it drives    *)
(* the real RegionalConfiguration through generated histories against the  *)
(* property below, and it found four defects while A-309 was built (its    *)
(* module doc lists them).  Whoever next changes §4.8 and may run TLC is    *)
(* asked to write a model for it (Creator and Weight as definitions, a few *)
(* hosts and objects), run it, and record the result here.                 *)
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
(* adopts each learned object (a newer record under its own placement)     *)
(* and confirms its share only when every object it learned is adopted    *)
(* (A-272).  A retirement completes, and becomes a completion record, when *)
(* it had survivors and each confirmed or left and carried it into a       *)
(* takeover that completed (A-309).                                        *)
(*                                                                          *)
(* A holder resolves each copy it keeps from the configuration: a line     *)
(* that names a successor moves the copy's routing to it; a line ending at *)
(* a completion record lets the copy go when the holder took part in that  *)
(* takeover; a successor's confirmation lets it go when the holder, asked  *)
(* in that round, is outside the successor's placement.  Every other       *)
(* verdict keeps the copy.                                                 *)
(*                                                                          *)
(* A learned object is adoptable within tolerance (every cohort keeps its  *)
(* recovery quorum of members) and past it (A-309 piece 2: every surviving *)
(* member of its cohorts, settled and current, has answered; here every    *)
(* member's copies are visible, so the round has heard them all).          *)
(*                                                                          *)
(* Abstractions: a cohort is a whole neighbourhood (copysets narrow it in  *)
(* the code, which changes no rule here); rendezvous is a fixed weight per *)
(* object and host; the counting rule is takeover_model's subject, so the  *)
(* record adopted is not modelled, only that an adoption makes a newer one.*)
(*                                                                          *)
(* The property checked:                                                    *)
(*   ReclaimOnlyStale  a member lets a copy go only if its object was      *)
(*                     adopted after that copy was written (a newer record *)
(*                     exists, so the copy is stale);                       *)
(* and the invariants                                                       *)
(*   SettledOnRecord   a line reads settled only at a completion record;   *)
(*   Bounded           the kept retirements and the completion records    *)
(*                     stay within the bound, the records a kept            *)
(*                     retirement needs aside.                             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Hosts,      \* every member id there will ever be (a restart joins under a new one)
          Objects,    \* the objects
          Creator,    \* [Objects -> Hosts]: the host that created each object
          Weight,     \* [Objects -> [Hosts -> Nat]]: rendezvous weights, distinct per object
          F,          \* the faults a cohort tolerates
          Bound,      \* the most retirements and completion records kept (one per member in the code)
          None        \* a model value: no host, no copy

ASSUME None \notin Hosts /\ None \notin Nat
ASSUME F \in Nat /\ Bound \in Nat /\ Bound >= 1
ASSUME Creator \in [Objects -> Hosts]

VARIABLES
  members,    \* the region's members
  joined,     \* every host that ever was a member
  settled,    \* [Hosts -> SUBSET Hosts]: each member's settled neighbourhood
  current,    \* [Hosts -> SUBSET Hosts]: each member's current neighbourhood
  kept,       \* the hosts whose retirement is kept
  ret,        \* [Hosts -> retirement record]: read only on kept
  done,       \* the hosts whose takeover has a completion record
  doneBy,     \* [Hosts -> SUBSET Hosts]: each completion record's confirmed survivors
  doneAt,     \* [Hosts -> Nat]: the version each record was made at
  version,    \* the configuration version
  owner,      \* [Objects -> Hosts]: the host that wrote each object's newest record
  adopted,    \* [Objects -> Nat]: how many times each object was adopted
  copy,       \* [Hosts -> [Objects -> Nat \cup {None}]]: the adoption count a copy was written at
  route,      \* [Hosts -> [Objects -> Hosts]]: the owner a holder routes its copy to
  movedFrom,  \* [Hosts -> [Objects -> Hosts \cup {None}]]: the retired owner an install moved it from
  departed    \* [Hosts -> [Objects -> Hosts \cup {None}]]: the takeover a copy is recovered in

vars == <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, version,
          owner, adopted, copy, route, movedFrom, departed>>

Max(a, b) == IF a >= b THEN a ELSE b

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

\* Within tolerance every cohort keeps its recovery quorum of members (Quorum::recovery); past it the round waits
\* for every surviving member of the object's cohorts instead (readiness). Kept for a reader's reference: the
\* adoption below needs neither, since the model's round hears every member at once.
RecoveryQuorum(C) == Max(Cardinality(C) - F, 1)
WithinTolerance(d) == \A C \in Cohorts(d) : Cardinality(C \cap members) >= RecoveryQuorum(C)

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
  /\ version = 0
  /\ owner = Creator
  /\ adopted = [o \in Objects |-> 0]
  /\ copy = [h \in Hosts |-> [o \in Objects |-> IF h \in joined THEN 0 ELSE None]]
  /\ route = [h \in Hosts |-> [o \in Objects |-> Creator[o]]]
  /\ movedFrom = [h \in Hosts |-> [o \in Objects |-> None]]
  /\ departed = [h \in Hosts |-> [o \in Objects |-> None]]

\* The owner of o writes a newer record to a placing subset of its placement (the record plane).
Write(o) ==
  /\ owner[o] \in members
  /\ \E acks \in SUBSET Placement(owner[o]) :
       /\ owner[o] \in acks
       /\ copy' = [h \in Hosts |-> IF h \in acks THEN [copy[h] EXCEPT ![o] = adopted[o]] ELSE copy[h]]
       /\ route' = [h \in Hosts |-> IF h \in acks THEN [route[h] EXCEPT ![o] = owner[o]] ELSE route[h]]
       /\ movedFrom' = [h \in Hosts |-> IF h \in acks THEN [movedFrom[h] EXCEPT ![o] = None] ELSE movedFrom[h]]
       /\ departed' = [h \in Hosts |-> IF h \in acks THEN [departed[h] EXCEPT ![o] = None] ELSE departed[h]]
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, version, owner, adopted>>

\* A fresh host joins, holding nothing, settled on itself.
Join(h) ==
  /\ h \notin joined
  /\ joined' = joined \cup {h}
  /\ members' = members \cup {h}
  /\ settled' = [settled EXCEPT ![h] = {h}]
  /\ current' = [current EXCEPT ![h] = {h}]
  /\ version' = version + 1
  /\ UNCHANGED <<kept, ret, done, doneBy, doneAt, owner, adopted, copy, route, movedFrom, departed>>

\* A member's neighbourhood moves (a change in flight until it settles).
Rebalance(h, N) ==
  /\ h \in members /\ h \in N /\ N \subseteq members
  /\ current' = [current EXCEPT ![h] = N]
  /\ version' = version + 1
  /\ UNCHANGED <<members, joined, settled, kept, ret, done, doneBy, doneAt, owner, adopted, copy, route,
                 movedFrom, departed>>

\* A member settles its current neighbourhood once every object it owns is placed at F + 1 of it.
Settle(h) ==
  /\ h \in members
  /\ \A o \in Objects : owner[o] = h =>
       Cardinality({x \in current[h] \cap members : copy[x][o] = adopted[o]}) >= F + 1
  /\ settled' = [settled EXCEPT ![h] = current[h]]
  /\ version' = version + 1
  /\ UNCHANGED <<members, joined, current, kept, ret, done, doneBy, doneAt, owner, adopted, copy, route,
                 movedFrom, departed>>

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
  /\ UNCHANGED <<joined, settled, current, done, doneBy, doneAt, owner, adopted, route, movedFrom, departed>>

\* s adopts o in d's round: a newer record placed under s. Within tolerance on its cohorts' quorums; past it
\* once every surviving member of its cohorts answered, which every member's visible state stands for here.
Adopt(d, s, o) ==
  /\ Owes(d, s)
  /\ o \in Learned(d, s)
  /\ owner[o] # s
  /\ adopted' = [adopted EXCEPT ![o] = @ + 1]
  /\ owner' = [owner EXCEPT ![o] = s]
  /\ \E acks \in SUBSET Placement(s) :
       /\ s \in acks
       /\ copy' = [h \in Hosts |-> IF h \in acks THEN [copy[h] EXCEPT ![o] = adopted[o] + 1] ELSE copy[h]]
       /\ route' = [h \in Hosts |-> IF h \in acks THEN [route[h] EXCEPT ![o] = s] ELSE route[h]]
       /\ movedFrom' = [h \in Hosts |-> IF h \in acks THEN [movedFrom[h] EXCEPT ![o] = None] ELSE movedFrom[h]]
       /\ departed' = [h \in Hosts |-> IF h \in acks THEN [departed[h] EXCEPT ![o] = None] ELSE departed[h]]
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, version>>

\* s confirms its share of d's takeover once every object it learned is its own (A-272).
Confirm(d, s) ==
  /\ Owes(d, s)
  /\ \A o \in Learned(d, s) : owner[o] = s
  /\ ret' = [ret EXCEPT ![d].confirmed = @ \cup {s}]
  /\ version' = version + 1
  /\ UNCHANGED <<members, joined, settled, current, kept, done, doneBy, doneAt, owner, adopted, copy, route,
                 movedFrom, departed>>

\* A retirement that completes becomes its completion record (complete_retirements). As for Retire, the
\* records' bound has been applied to the last one first.
Complete(d) ==
  /\ Completes(d)
  /\ Cardinality(done) <= Max(Bound, Cardinality(NeededRecords))
  /\ kept' = kept \ {d}
  /\ done' = done \cup {d}
  /\ doneBy' = [doneBy EXCEPT ![d] = ret[d].confirmed]
  /\ doneAt' = [doneAt EXCEPT ![d] = version]
  /\ UNCHANGED <<members, joined, settled, current, ret, version, owner, adopted, copy, route, movedFrom,
                 departed>>

\* Past the bound the council lets a retirement go, every kept retirement that carries it with it
\* (bound_retirements): one nobody owes and nobody carries first, then the oldest.
Pending(d) == (\E h \in Hosts : Owes(d, h)) \/ Carried(d)
DropRetirement(d) ==
  /\ Cardinality(kept) > Bound
  /\ d \in kept
  /\ \A x \in kept : \/ (Pending(d) = Pending(x) /\ ret[d].at <= ret[x].at)
                     \/ (~Pending(d) /\ Pending(x))
  /\ kept' = {x \in kept : d \notin RecoveryOwners(x)}
  /\ UNCHANGED <<members, joined, settled, current, ret, done, doneBy, doneAt, version, owner, adopted, copy,
                 route, movedFrom, departed>>

\* Past the bound the council lets the oldest completion record go, never one a kept retirement needs
\* (bound_completed).
EvictRecord(d) ==
  /\ Cardinality(done) > Max(Bound, Cardinality(NeededRecords))
  /\ d \in done \ NeededRecords
  /\ \A x \in done \ NeededRecords : doneAt[d] <= doneAt[x]
  /\ done' = done \ {d}
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, doneBy, doneAt, version, owner, adopted, copy,
                 route, movedFrom, departed>>

\* Whether a successor's confirmation makes h's copy of o stale (stale_after_confirmation).
StaleAfterConfirmation(h, o) ==
  LET d == departed[h][o]
      r == route[h][o]
  IN /\ d # None
     /\ r # h
     /\ h \notin Placement(r)
     /\ \/ (d \in kept /\ r \in ret[d].confirmed /\ h \in ret[d].survivors)
        \/ (d \in done /\ r \in doneBy[d] /\ h \in doneBy[d])

\* A member lets a copy go.
Reclaim(h, o) ==
  /\ copy' = [copy EXCEPT ![h][o] = None]
  /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, version, owner, adopted,
                 route, movedFrom, departed>>

\* A member resolves one copy at an install (resolve_held_objects, reconcile_held_authority).
Resolve(h, o) ==
  /\ h \in members
  /\ copy[h][o] # None
  /\ LET r == route[h][o] IN
     IF r \in members /\ r \notin kept
     THEN /\ StaleAfterConfirmation(h, o)
          /\ Reclaim(h, o)
     ELSE LET l == Lineage(r, o) IN
          \/ /\ l.kind = "successor"
             /\ l.successor # r
             /\ route' = [route EXCEPT ![h][o] = l.successor]
             /\ movedFrom' = [movedFrom EXCEPT ![h][o] = IF r \in kept THEN r ELSE movedFrom[h][o]]
             /\ departed' = [departed EXCEPT ![h][o] = l.departed]
             /\ UNCHANGED <<members, joined, settled, current, kept, ret, done, doneBy, doneAt, version, owner,
                            adopted, copy>>
          \/ /\ l.kind = "settled"
             /\ h \in doneBy[l.takeover]
             /\ Reclaim(h, o)

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

\* A member lets a copy go only if an adoption came after that copy was written.
ReclaimOnlyStale ==
  [][\A h \in Hosts, o \in Objects :
       (h \in members' /\ copy[h][o] # None /\ copy'[h][o] = None) => adopted[o] > copy[h][o]]_vars

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
