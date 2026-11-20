------------------------------ MODULE MC_Lineage ------------------------------
(***************************************************************************)
(* TLC's models of Lineage (A-518, 2026-11-19).  Written as text; main      *)
(* runs them: MC_Lineage.cfg (one object, a region formed on its creator    *)
(* alone, the bootstrap shape seed 44 found) and MC_Lineage_two.cfg (two    *)
(* objects of two founding hosts, the weights ranking them apart).  Hosts   *)
(* and objects are TLC model values the configurations name.               *)
(***************************************************************************)
EXTENDS Lineage, TLC

CONSTANTS h1, h2, h3, h4, o1, o2

MCHosts == {h1, h2, h3, h4}

\* Shape: a host's rendezvous weight for an object, h1 first and h4 last; reversed for the second object, so the
\* two rank their successors apart.
Rank(h) == CASE h = h1 -> 4 [] h = h2 -> 3 [] h = h3 -> 2 [] OTHER -> 1

\* The bootstrap model: one object, created by h1, which forms the region alone.
MCObjects == {o1}
MCCreator == [o \in MCObjects |-> h1]
MCWeight == [o \in MCObjects |-> [h \in MCHosts |-> Rank(h)]]

\* The two-founder model: o1 created by h1 and o2 by h2.
MCObjectsTwo == {o1, o2}
MCCreatorTwo == [o \in MCObjectsTwo |-> IF o = o1 THEN h1 ELSE h2]
MCWeightTwo == [o \in MCObjectsTwo |-> [h \in MCHosts |-> IF o = o1 THEN Rank(h) ELSE 5 - Rank(h)]]

\* Shape: the configuration versions a history may reach; with MaxSeq's writes per object it bounds the state space
\* (joins, moves, settles, retirements and confirmations each take a version).
MaxVersion == 8
StateBound == version <= MaxVersion
==============================================================================
