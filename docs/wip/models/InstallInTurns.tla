--------------------------- MODULE InstallInTurns ---------------------------
(***************************************************************************)
(* slates, design model (2026-11-18, g230): a holder installs a committed   *)
(* configuration on its register copies over several turns.                 *)
(*                                                                          *)
(* One register (a volume's head, or its catalog) held by a set of          *)
(* acceptors.  The council commits versions 1..MaxVersion: version 1 names  *)
(* the creator, owner 1, and each later version v retires owner v-1 and     *)
(* names owner v its successor (a takeover).  An owner's epoch is its       *)
(* version.  Each holder's node installs versions in order; each owner's    *)
(* node knows some version too, and may lag (a resumed stale owner, or one  *)
(* that learned of its own retirement and writes on regardless).           *)
(*                                                                          *)
(* A record carries the owner that wrote it, its epoch, the generation (the *)
(* version) its owner wrote it under, and a sequence.  A holder checks it   *)
(* twice: its node refuses a generation other than the version it has      *)
(* installed (`fleet::check_held_record`: ConfigurationStale below,        *)
(* ForeignGeneration above), and its copy refuses a generation or an owner  *)
(* other than its authority's and an epoch below its fence                  *)
(* (`Acceptor::check`).  The takeover's phase one (`PromoteAck`) is answered *)
(* by a copy whose authority names the successor at its version: it raises  *)
(* the fence and reports the highest record.                               *)
(*                                                                          *)
(* Today (Mode "eager") the install moves every copy at once: its authority *)
(* to the version's owner and its fence to the version's epoch                *)
(* (`fleet::sync_config_from_council`, `reconcile_held_authority`), one     *)
(* turn over every held copy.  The design change (Mode "lazy") installs the *)
(* version in one step and reconciles each copy later: when it is next      *)
(* asked, or by a sweep in turns (`Reconcile`).  `Disabled` removes         *)
(* protections, one or several, to learn which the safety rests on:        *)
(* "copy_gen", a copy's own generation check (it answers with the          *)
(* authority it had); "fence", every fence raise but an accepted record's   *)
(* own (phase one's and the install's); "promote_auth", phase one's demand *)
(* that the copy's authority name the successor.                          *)
(*                                                                          *)
(* Properties (FencedRegister's): TotalOrder, Continuity, StaleNeverCommits *)
(* and ReadSafety, over every interleaving of installs, reconciles,        *)
(* promotions, writes and accepts.                                          *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Acceptors,   \* the record holders, e.g. {a1, a2, a3}
          Values,      \* payloads, e.g. {v1}
          MaxVersion,  \* versions (and owners, and epochs) 1..MaxVersion
          MaxSeq,      \* records per owner 1..MaxSeq
          Mode,        \* "eager" or "lazy": how a holder installs a version on its copies
          Disabled     \* the protections a mutation removes: a subset of
                       \* {"copy_gen", "fence", "promote_auth"}

ASSUME Mode \in {"eager", "lazy"}
ASSUME Disabled \subseteq {"copy_gen", "fence", "promote_auth"}

Majority == {Q \in SUBSET Acceptors : 2 * Cardinality(Q) > Cardinality(Acceptors)}
Versions == 1..MaxVersion
NoAuthority == [gen |-> 0, owner |-> 0]
NoRecord == [owner |-> 0, epoch |-> 0, gen |-> 0, seq |-> 0, value |-> "none"]
Records == [owner : Versions, epoch : Versions, gen : Versions, seq : 1..MaxSeq, value : Values]
AnyRecord == Records \cup {NoRecord}

\* The order a reader and phase one take: epoch, then sequence.
Newer(r1, r2) == r1.epoch > r2.epoch \/ (r1.epoch = r2.epoch /\ r1.seq > r2.seq)
NewerOrEq(r1, r2) == r1 = r2 \/ Newer(r1, r2)
MaxRec(S) == CHOOSE r \in S : \A s \in S : NewerOrEq(r, s)

VARIABLES
  committed,  \* the council's newest committed version
  known,      \* [Versions -> 0..MaxVersion]: the version each owner's node knows
  installed,  \* [Acceptors -> 0..MaxVersion]: the version each holder's node installed
  auth,       \* [Acceptors -> authority]: each copy's authority (generation, owner)
  fence,      \* [Acceptors -> 0..MaxVersion]: each copy's fence
  stored,     \* [Acceptors -> AnyRecord]: the highest record each copy holds
  promAcked,  \* [Versions -> SUBSET Acceptors]: copies that answered owner v's phase one
  promVal,    \* [Versions -> [Acceptors -> AnyRecord]]
  promoted,   \* [Versions -> BOOLEAN]: owner v adopted its base
  base,       \* [Versions -> AnyRecord]
  nextSeq,    \* [Versions -> Nat]
  issued,     \* SUBSET Records
  stale,      \* SUBSET Records: issued after a later owner's promotion majority
  writeAcks   \* [Records -> SUBSET Acceptors]

vars == <<committed, known, installed, auth, fence, stored, promAcked, promVal, promoted, base,
          nextSeq, issued, stale, writeAcks>>

Committed == {r \in Records : writeAcks[r] \in Majority}
PromotionMajority(v) == promAcked[v] \in Majority

\* The authority and fence version v sets on a copy: its owner at its generation, fenced at its epoch.
AuthorityOf(v) == [gen |-> v, owner |-> v]

Init ==
  /\ committed = 1
  /\ known = [v \in Versions |-> IF v = 1 THEN 1 ELSE 0]
  /\ installed = [a \in Acceptors |-> 1]
  /\ auth = [a \in Acceptors |-> AuthorityOf(1)]
  /\ fence = [a \in Acceptors |-> 1]
  /\ stored = [a \in Acceptors |-> NoRecord]
  /\ promAcked = [v \in Versions |-> IF v = 1 THEN Acceptors ELSE {}]
  /\ promVal = [v \in Versions |-> [a \in Acceptors |-> NoRecord]]
  /\ promoted = [v \in Versions |-> v = 1]
  /\ base = [v \in Versions |-> NoRecord]
  /\ nextSeq = [v \in Versions |-> 1]
  /\ issued = {}
  /\ stale = {}
  /\ writeAcks = [r \in Records |-> {}]

(***************************************************************************)
(* The council commits the next version: owner v-1 retired, owner v its    *)
(* successor.                                                              *)
(***************************************************************************)
Commit ==
  /\ committed < MaxVersion
  /\ committed' = committed + 1
  /\ known' = [known EXCEPT ![committed + 1] = committed + 1]
  /\ UNCHANGED <<installed, auth, fence, stored, promAcked, promVal, promoted, base, nextSeq,
                 issued, stale, writeAcks>>

(***************************************************************************)
(* An owner's node learns a later committed version (a stale owner may     *)
(* learn of its own retirement and still write).                           *)
(***************************************************************************)
Learn(o) ==
  /\ known[o] > 0
  /\ known[o] < committed
  /\ known' = [known EXCEPT ![o] = known[o] + 1]
  /\ UNCHANGED <<committed, installed, auth, fence, stored, promAcked, promVal, promoted, base,
                 nextSeq, issued, stale, writeAcks>>

(***************************************************************************)
(* A holder's node installs the next committed version.  Eager: every copy *)
(* moves with it, in the same step.  Lazy: the version alone; the copy      *)
(* moves at its reconcile.                                                 *)
(***************************************************************************)
Install(a) ==
  /\ installed[a] < committed
  /\ installed' = [installed EXCEPT ![a] = installed[a] + 1]
  /\ IF Mode = "eager"
       THEN /\ auth' = [auth EXCEPT ![a] = AuthorityOf(installed[a] + 1)]
            /\ fence' = IF "fence" \in Disabled THEN fence
                        ELSE [fence EXCEPT ![a] = IF fence[a] < installed[a] + 1 THEN installed[a] + 1
                                                                                 ELSE fence[a]]
       ELSE UNCHANGED <<auth, fence>>
  /\ UNCHANGED <<committed, known, stored, promAcked, promVal, promoted, base, nextSeq, issued,
                 stale, writeAcks>>

\* A copy reconciled to its node's installed version: by the sweep, or when it is asked.
Reconcile(a) ==
  /\ Mode = "lazy"
  /\ auth[a].gen < installed[a]
  /\ auth' = [auth EXCEPT ![a] = AuthorityOf(installed[a])]
  /\ fence' = IF "fence" \in Disabled THEN fence
              ELSE [fence EXCEPT ![a] = IF fence[a] < installed[a] THEN installed[a] ELSE fence[a]]
  /\ UNCHANGED <<committed, known, installed, stored, promAcked, promVal, promoted, base,
                 nextSeq, issued, stale, writeAcks>>

\* The copy's own checks of an authority claim (generation, owner), as `Acceptor::check` makes them.
CopyAdmits(a, gen, owner) ==
  /\ owner = auth[a].owner
  /\ ("copy_gen" \in Disabled \/ gen = auth[a].gen)

(***************************************************************************)
(* Phase one: owner v asks a copy, under the version it knows (v itself:   *)
(* it is named by version v).  The node and the copy check the claim; the  *)
(* copy raises its fence and reports its highest record.                   *)
(***************************************************************************)
PromoteAck(v, a) ==
  /\ v > 1
  /\ known[v] >= v
  /\ a \notin promAcked[v]
  /\ installed[a] = v
  /\ ("promote_auth" \in Disabled \/ CopyAdmits(a, v, v))
  /\ v >= fence[a]
  /\ fence' = IF "fence" \in Disabled THEN fence ELSE [fence EXCEPT ![a] = v]
  /\ promAcked' = [promAcked EXCEPT ![v] = promAcked[v] \cup {a}]
  /\ promVal' = [promVal EXCEPT ![v][a] = stored[a]]
  /\ UNCHANGED <<committed, known, installed, auth, stored, promoted, base, nextSeq, issued,
                 stale, writeAcks>>

Adopt(v) ==
  /\ ~promoted[v]
  /\ PromotionMajority(v)
  /\ base' = [base EXCEPT ![v] = MaxRec({promVal[v][a] : a \in promAcked[v]})]
  /\ promoted' = [promoted EXCEPT ![v] = TRUE]
  /\ UNCHANGED <<committed, known, installed, auth, fence, stored, promAcked, promVal, nextSeq,
                 issued, stale, writeAcks>>

(***************************************************************************)
(* An owner that was promoted writes its next record under the version its *)
(* node knows now.  Stale once a later owner has its promotion majority.   *)
(***************************************************************************)
Issue(o, val) ==
  /\ promoted[o]
  /\ nextSeq[o] <= MaxSeq
  /\ LET r == [owner |-> o, epoch |-> o, gen |-> known[o], seq |-> nextSeq[o], value |-> val]
     IN /\ issued' = issued \cup {r}
        /\ stale' = IF \E later \in Versions : later > o /\ PromotionMajority(later)
                      THEN stale \cup {r} ELSE stale
  /\ nextSeq' = [nextSeq EXCEPT ![o] = nextSeq[o] + 1]
  /\ UNCHANGED <<committed, known, installed, auth, fence, stored, promAcked, promVal, promoted,
                 base, writeAcks>>

Accept(r, a) ==
  /\ r \in issued
  /\ a \notin writeAcks[r]
  /\ r.gen = installed[a]
  /\ CopyAdmits(a, r.gen, r.owner)
  /\ r.epoch >= fence[a]
  /\ fence' = [fence EXCEPT ![a] = r.epoch]
  /\ stored' = [stored EXCEPT ![a] = IF Newer(r, stored[a]) THEN r ELSE stored[a]]
  /\ writeAcks' = [writeAcks EXCEPT ![r] = writeAcks[r] \cup {a}]
  /\ UNCHANGED <<committed, known, installed, auth, promAcked, promVal, promoted, base, nextSeq,
                 issued, stale>>

Next ==
  \/ Commit
  \/ \E o \in Versions : Learn(o)
  \/ \E a \in Acceptors : Install(a) \/ Reconcile(a)
  \/ \E v \in Versions, a \in Acceptors : PromoteAck(v, a)
  \/ \E v \in Versions : Adopt(v)
  \/ \E o \in Versions, val \in Values : Issue(o, val)
  \/ \E r \in Records, a \in Acceptors : Accept(r, a)

Spec == Init /\ [][Next]_vars

\* The holders are interchangeable: TLC checks one state of each class of their permutations.
Perms == Permutations(Acceptors)

TotalOrder ==
  \A r1, r2 \in Committed : (r1.epoch = r2.epoch /\ r1.seq = r2.seq) => r1 = r2

Continuity ==
  \A v \in Versions : (promoted[v] /\ v > 1) =>
    \A r \in Committed : r.epoch < v => NewerOrEq(base[v], r)

StaleNeverCommits == stale \cap Committed = {}

ReadSafety ==
  \A Q \in Majority : \A r \in Committed : NewerOrEq(MaxRec({stored[a] : a \in Q}), r)

TypeOK ==
  /\ committed \in Versions
  /\ \A a \in Acceptors : installed[a] \in Versions /\ fence[a] \in 0..MaxVersion
  /\ \A a \in Acceptors : stored[a] \in AnyRecord
  /\ issued \subseteq Records
  /\ stale \subseteq issued
=============================================================================
