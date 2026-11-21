# main <- w-fed-move: the next largest owner ask is the rebase (19.8 a client at 10 hot files). It needs your call (origin/main 356b28d96)

## Measured
1,000-client stand-in, release, in-process, kernel/, 10 hot files, main 356b28d96. Asks per client: 31.9 in all.
- rebase 19.8 (one ask per rebase), verdict 5.5, lengths 4.7, admit 1.0, name 1.0.
- Rebases 19.8 a client, of which 15.3 (77%) conflicted again (harness rebase_conflicts 15,266).
- Submits 5.6 a client (4.6 of them conflicts); ReadHead 55.0 a client.

## Why
On a conflict, the harness's client:
1. reads the head's bytes for each window;
2. writes them into its work;
3. rebases.
With 1,000 clients on 40 hot slots, the head has changed those windows again before the rebase lands, so it conflicts
and the loop goes round. No ask is redundant: each rebase judges against a head the work's shard does not hold. Cutting
the rebase asks means fewer rebase rounds, which is either a protocol change or a different client resolution.

## Options (I'd take 1)
1. **Resolve through A-796's ResolveMerge.** The harness takes the head on a conflict's windows by retiring its own
   conflicting ops () and re-writing its slots () in one ask. That converges without the read and
   re-rebase race, though a stale stamp can still repeat it. The server is unchanged; the harness's client models the
   documented resolution path, and the oracle's "accepted / identical / conflict" rule is unchanged.
2. **A conflicting rebase carries the head's window bytes and the version it judged at.** The client writes them and
   rebases pinned there, saving the ReadHead round. It still races the head, so it cuts reads more than rebase asks.
3. **Cut the next kind instead:** lengths (4.7 a client, each work's first edit of a file). A shard-level cache of base
   lengths keyed by path and valid from the file's last change would serve every work on the shard. It is a server
   change with an A/B, but it is the third kind, not the largest.

Your goal says "the next largest kind". 1 cuts that kind by changing the client, 3 cuts a smaller kind in the server.
Tell me which.
