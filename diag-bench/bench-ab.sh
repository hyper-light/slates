#!/usr/bin/env bash
# bench-ab.sh STOCK_REPO SOURCE WORK NAME=BINARY...: one anchor per binary, v2.0.0 pushed to each, each warmed by one
# clone and deepen (the first fetch encodes the version) and settled; then TRIALS rounds of `git fetch --deepen 100`
# after a fresh `clone --depth 1`, stock git daemon first, then each binary, interleaved.
set -u
STOCK=$1 SOURCE=$2 WORK=$3; shift 3
rm -rf "$WORK" && mkdir -p "$WORK/logs" || exit 1
cd "$WORK" || exit 1
export GIT_TERMINAL_PROMPT=0 GIT_CONFIG_NOSYSTEM=1 HOME="$WORK/home"
mkdir -p "$HOME"; git config --global user.name B; git config --global user.email b@example.com
PIDS=()
trap 'kill ${PIDS[*]} 2>/dev/null' EXIT
now() { perl -MTime::HiRes=time -e 'printf "%.3f", time'; }
cpu_cs() { ps -p "$1" -o time= | awk -F'[:.]' '{print ($1*60+$2)*100+$3}'; }
settle() { local idle=0 a b; while [ $idle -lt 5 ]; do a=$(cpu_cs "$1"); sleep 2; b=$(cpu_cs "$1"); if [ $((b-a)) -lt 10 ]; then idle=$((idle+1)); else idle=0; fi; done; }
for pair in "$@"; do
  name=${pair%%=*} bin=${pair#*=} inst="bench-ab-$name-$$"
  "$bin" --instance "$inst" anchor --quick --shards 2 --vcs-http 127.0.0.1:0 >/dev/null 2>"$WORK/$name-anchor.log" &
  PIDS+=($!)
  for _ in $(seq 600); do grep -q "instance $inst serving on" "$WORK/$name-anchor.log" && grep -q 'issuer surface: export' "$WORK/$name-anchor.log" && break; sleep 0.1; done
  http=$(sed -n 's/.*HTTP edge serves cleartext on 127.0.0.1:\([0-9]*\).*/\1/p' "$WORK/$name-anchor.log" | head -1)
  issuer=$(sed -n 's/.*issuer surface: export //p' "$WORK/$name-anchor.log" | head -1)
  for _ in $(seq 100); do "$bin" --instance "$inst" volume list >/dev/null 2>&1 && break; sleep 0.1; done
  "$bin" --instance "$inst" bootstrap root >/dev/null || { echo "$name bootstrap failed"; exit 1; }
  "$bin" --instance "$inst" repository create project --tool git --default-branch master >/dev/null || exit 1
  token=$(env $issuer "$bin" --instance "$inst" credential token | sed -n 's/^token: //p')
  url_tmp="http://git:$token@127.0.0.1:$http/project"; eval "URL_$name=\"\$url_tmp\""
  eval "DPID_$name=$(pgrep -f -- "--instance $inst daemon" | head -1)"
  s=$(now); git -C "$SOURCE" push -q "$(eval echo \"\$URL_$name\")" 'v2.0.0^{commit}:refs/heads/master' 2>"$WORK/logs/$name-push.log" || { echo "$name push failed"; exit 1; }
  echo "$name push $(perl -e "printf '%.1f', $(now)-$s") s"
done
git daemon --reuseaddr --base-path="$(dirname "$STOCK")" --export-all --listen=127.0.0.1 --port=${DPORT:-19419} "$STOCK" >"$WORK/daemon.log" 2>&1 &
PIDS+=($!); sleep 1
URL_stock="git://127.0.0.1:${DPORT:-19419}/$(basename "$STOCK")"
trial() { # trial LABEL NAME
  rm -rf "$WORK/t" && git clone -q --depth 1 "$(eval echo \"\$URL_$2\")" "$WORK/t" 2>>"$WORK/logs/$2.log" || { echo "$1 $2 clone failed"; return; }
  local s e code; s=$(now)
  git -C "$WORK/t" fetch -q --deepen 100 >>"$WORK/logs/$2.log" 2>&1; code=$?
  e=$(now)
  echo "$1 $2 $(perl -e "printf '%.2f', $e-$s") s exit=$code commits=$(git -C "$WORK/t" rev-list --count HEAD 2>/dev/null) load=$(sysctl -n vm.loadavg | awk '{print $2}')"
}
for pair in "$@"; do name=${pair%%=*}; trial warm "$name"; settle "$(eval echo \$DPID_$name)"; echo "$name settled"; done
for i in $(seq ${TRIALS:-5}); do
  trial "round$i" stock
  for pair in "$@"; do trial "round$i" "${pair%%=*}"; done
done
