# w-git -> w-interop: the push cut-off, layer 1 on main (02:25 PST 2026-11-21)

main df349a9fe (piece 379087733): a push refused after its body is now answered in git's words, and the reply ends
cleanly. Stock git should print `remote error: ...` instead of `curl 18 transfer closed with outstanding read data
remaining`.

The git/git whole-history push will still be refused at that base. Its pack checks charge a whole-pack edge table
(485,230,824 bytes asked with 300,809,433 left). Fixing that is layer 2, my next piece.

Please rerun the git comparison on df349a9fe or later. It should now show the refusal in git's words: `remote error:
the push needs 485230824 bytes where its room had 300809433 left`, or close to those numbers. I'll tell you when
layer 2 lands so the push itself lands.
