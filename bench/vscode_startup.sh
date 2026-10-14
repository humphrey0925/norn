#!/bin/sh
# Norn starting in a real VS Code: bench/vscode_startup.sh <name> <folder> <file> [runs]
# Each run opens <folder> (its .norn as it is), opens <file> once the index
# is there, and lingers; the server's trace (NORN_TRACE) of every run is
# kept in out/startup-trace/<name>-<n>.txt with, at its top, when VS Code
# was launched. NORN_STARTUP_CPP=1: with the C/C++ extension.
set -e
name=$1 folder=$(cd "$2" && pwd) file=$3 runs=${4:-3}
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$here/out/startup-trace"
cd "$here/vscode"
for n in $(seq 1 "$runs"); do
  trace="$here/out/startup-trace/$name-$n.txt"
  rm -f "$trace"
  launched=$(NORN_TRACE=$(cygpath -w "$trace" 2>/dev/null || echo "$trace") NORN_STARTUP_FILE=$file NORN_STARTUP_LINGER=${LINGER:-15000} \
    node out/test/runTest.js startup "$folder" 2>&1 | sed -n 's/^launched at \([0-9]*\) ms.*/\1/p')
  python - "$trace" "$launched" "$name-$n" <<'EOF'
import re, sys
trace, launched, label = sys.argv[1], int(sys.argv[2]), sys.argv[3]
lines = open(trace, encoding="utf-8").read().splitlines()
started = int(re.search(r"started at (\d+)", lines[0]).group(1))
at = lambda what: next((int(l.split()[0]) for l in lines if what in l), None)
ready = next(l for l in lines if "index ready" in l)
changes = [l.strip() for l in lines if re.search(r"si/sync|si/set|didSave|didChangeWatchedFiles|synced|updated", l)]
host = int(open(trace + ".host").read())
print(f"{label}: extension host process started {(host - launched) / 1000:.2f} s after launch;")
print(f"{label}: server started {(started - launched) / 1000:.2f} s after VS Code was launched; "
      f"answering from the saved graph at +{at('initialize')} ms, index ready at +{ready.split()[0]} ms "
      f"({(started - launched) / 1000 + int(ready.split()[0]) / 1000:.2f} s from launch) -- {ready.split(':', 1)[1].strip()}")
print("   changes of the index after start:", changes or "none")
EOF
done
