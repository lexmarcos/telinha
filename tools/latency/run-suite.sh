#!/usr/bin/env bash
# Runs measure.mjs several times, waiting for the machine to be free of other puppeteer
# Chrome instances first, and flags runs where another one showed up mid-run.
# usage: run-suite.sh <appdir> <outfile> <label> [direct-runs=3] [relay-runs=1] [seconds=20]
# extra measure.mjs flags via env, e.g. EXTRA="--warmup 15 --server dominio.do.servidor"
APP=$1; OUT=$2; LABEL=$3; N=${4:-3}; R=${5:-1}; SECS=${6:-20}
HERE=$(cd "$(dirname "$0")" && pwd)
dirs() { ps -eo args | grep -o -- '--user-data-dir=[^ ]*puppeteer[^ ]*' | sort -u | wc -l; }
waitquiet() { for i in $(seq 600); do [ "$(dirs)" -eq 0 ] && return; sleep 1; done; echo "still busy, running anyway" >&2; }
one() {
  waitquiet
  local flag=$(mktemp); ( while :; do [ "$(dirs)" -gt 1 ] && echo 1 > "$flag"; sleep 1; done ) & local mon=$!
  local load=$(cut -d' ' -f1 /proc/loadavg)
  local line; line=$(node "$HERE/measure.mjs" --app "$APP" --seconds "$SECS" --label "$1" $2 $EXTRA 2>>"$OUT.log")
  kill $mon 2>/dev/null; wait $mon 2>/dev/null
  local cont=false; [ -s "$flag" ] && cont=true; rm -f "$flag"
  [ -n "$line" ] && echo "$line" | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{const j=JSON.parse(s);j.extra.loadAvgAtStart=$load;j.extra.otherChromeDuringRun=$cont;console.log(JSON.stringify(j))})" | tee -a "$OUT"
}
for i in $(seq "$N"); do one "$LABEL-$i" ""; done
for i in $(seq "$R"); do one "$LABEL-relay-$i" "--relay"; done
