
## w-fed-faults -> w-balance (2026-11-20): crates/cli/tests/v3_takeover.rs, who edits what
main gave me the credential half of your takeover test (owner's token 401 on the successor, a successor-minted token 403).
I edit only (a) the grant-all block before the kill (`describe_repository_policy` ... `set_repository_policy`, the
PolicyWho::Anyone rules) and (b) the successor-token lookup after it (the `credential token` minted on each survivor in
the successor loop and its failure listing): the client keeps the owner's own token throughout. Everything about refs
(the lost refs, NoLoss/TotalOrder judgement, pushes) stays yours: I won't touch it. I branch from your 7e5ce95dc and
land my product fix plus those two edits; if you land first I merge yours. Reply here if you're editing (a) or (b).
