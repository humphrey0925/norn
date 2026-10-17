#!/bin/bash
# The comparison on every project (bench/ec2/corpora.sh made them): each
# tool three times, clangd also with each build's database, then the
# charts and the page (bench/chart.py) in $OUT.
#   bench/ec2/run.sh
set -euo pipefail
REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
W=${W:-$HOME/bench}
OUT=${OUT:-$W/results}
mkdir -p "$OUT"
cd "$REPO"
lscpu > "$OUT/machine.txt"; free -g >> "$OUT/machine.txt"; df -h "$W" >> "$OUT/machine.txt"

files=()
for c in redis git-c c-only stm32f4 nrfx; do
  case $c in
    redis) build=$W/build/redis ;;
    git-c) build=$W/build/git ;;
    c-only) build=$W/build/binutils ;;
    *) build=$W/build/$c ;;
  esac
  # clangd with the build's database runs on the tree it was built in.
  case $c in
    c-only) tree=$W/build/binutils-src ;;
    stm32f4 | nrfx) tree=$W/corpus/$c ;;
    *) tree=$build ;;
  esac
  echo "== $c $(date +%T)"
  python3 bench/compare.py --corpus "$W/corpus/$c" --tools si,cbm,clangd,ctags,cscope,gtags --clangd /usr/bin/clangd-20 \
    --out "$OUT/$c.json" --work "$W/work/$c" --index-reps 3 --names 20 --reps 3 --timeout 3600 > "$OUT/$c.log" 2>&1 || true
  truth=$(python3 -c "import sys; sys.path.insert(0, 'bench'); import compare; print(compare.TRUTH['$c'])")
  python3 bench/compare.py --corpus "$tree" --truth "out/gt/$truth" --tools clangd-db --clangd /usr/bin/clangd-20 \
    --clangd-db "$build/compile_commands.json" --out "$OUT/$c-db.json" --work "$W/work/$c-db" \
    --index-reps 3 --names 20 --reps 3 --timeout 3600 > "$OUT/$c-db.log" 2>&1 || true
  # (Named as the comparison names it, whatever the folder.)
  python3 -c "import json,sys; p=sys.argv[1]; d=json.load(open(p)); d['corpus']['name']=sys.argv[2]; json.dump(d, open(p,'w'), indent=1)" "$OUT/$c-db.json" "$c"
  files+=("$OUT/$c.json" "$OUT/$c-db.json")
done
python3 bench/chart.py "${files[@]}" --out "$OUT/benchmarks"
echo "results in $OUT"
