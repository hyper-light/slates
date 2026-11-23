#!/bin/bash
# A-879 on Windows, by hand: a real anchor, `slates attach --write`, then `slates write` of a few bytes and of 64 MiB
# under it, and `slates attach --hold`, each one's exit code and stderr printed whole.
set -u
slates="$1"
inst="a879dbg-$$"
log="$RUNNER_TEMP/a879-anchor.log"
"$slates" --instance "$inst" anchor --quick --shards 1 > "$RUNNER_TEMP/a879-anchor.out" 2> "$log" &
anchor=$!
served=0
for _ in $(seq 1 1200); do if grep -q "serving on" "$log"; then served=1; break; fi; sleep 0.1; done
echo "served: $served"
"$slates" --instance "$inst" bootstrap root 2>&1 | tail -2
id=$("$slates" --instance "$inst" volume create v --bounded 256MiB | sed -n 's/^id: //p')
echo "volume: $id"
att=$("$slates" --instance "$inst" attach "$id" --write | sed -n 's/^attachment: //p')
echo "attachment: $att"
echo "--- a tiny write"
printf 'tiny' | "$slates" --instance "$inst" write "$id" "$att" tiny.txt > "$RUNNER_TEMP/w1.out" 2> "$RUNNER_TEMP/w1.err"
echo "exit $?"; cat "$RUNNER_TEMP/w1.out"; grep -v '^slates daemon: \|^slates anchor: ' "$RUNNER_TEMP/w1.err" | head -20
echo "--- a 64 MiB write"
head -c $((64 << 20)) /dev/zero | "$slates" --instance "$inst" write "$id" "$att" big.bin > "$RUNNER_TEMP/w2.out" 2> "$RUNNER_TEMP/w2.err"
echo "exit $?"; cat "$RUNNER_TEMP/w2.out"; grep -v '^slates daemon: \|^slates anchor: ' "$RUNNER_TEMP/w2.err" | head -20
echo "--- attach --hold for 5 s"
"$slates" --instance "$inst" attach "$id" --read --hold > "$RUNNER_TEMP/h.out" 2> "$RUNNER_TEMP/h.err" &
holder=$!
sleep 5
kill "$holder" 2> /dev/null; wait "$holder" 2> /dev/null; echo "holder exit $?"
cat "$RUNNER_TEMP/h.out"; grep -v '^slates daemon: \|^slates anchor: ' "$RUNNER_TEMP/h.err" | head -20
kill "$anchor" 2> /dev/null; wait "$anchor" 2> /dev/null
echo "--- the anchor's log, filtered"
grep -i 'renew\|hold\|refus\|claim\|error' "$log" | head -30
