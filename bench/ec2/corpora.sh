#!/bin/bash
# The benchmark's five projects, at the versions it measures, each twice:
#   $W/corpus/<name>  as released, which the tools index
#   $W/build/<name>   built with `bear`, whose compile_commands.json gives
#                     clang's ground truth (bench/gt_extract.py) and
#                     clangd its build database
# and the ground truth in $REPO/out/gt. Ubuntu 24.04 (bench/ec2/setup.sh).
#   bench/ec2/corpora.sh [name...]     (default: all five)
set -euo pipefail
REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
W=${W:-$HOME/bench}
GT=$REPO/out/gt
GLUE=$REPO/bench/ec2/glue
J=$(nproc)
mkdir -p "$W/corpus" "$W/build" "$W/src" "$GT"

# A repository at one commit, fetched no further than needed.
fetch() {  # url commit dir
  local url=$1 commit=$2 dir=$3
  if [ ! -d "$dir/.git" ]; then
    git init -q "$dir"
    git -C "$dir" remote add origin "$url"
  fi
  git -C "$dir" fetch -q --depth 1 origin "$commit" 2>/dev/null \
    || git -C "$dir" fetch -q --filter=blob:none origin
  git -C "$dir" checkout -q --force "$commit"
}

# The tree as released: the commit's files and nothing a build made.
release() {  # repo dir out
  rm -rf "$2" && mkdir -p "$2"
  git -C "$1" archive HEAD | tar -x -C "$2"
}

truth() {  # src build out
  echo "  ground truth: $3"
  python3 "$REPO/bench/gt_extract.py" "$1" "$2" "$GT/$3" > "$W/build/$(basename "$3" .json).gt.log" 2>&1
}

redis() {
  fetch https://github.com/redis/redis fcc9b536dde7de4be01f6f4904eafa6c194c15ae "$W/src/redis"
  release "$W/src/redis" "$W/corpus/redis"
  rm -rf "$W/build/redis" && cp -a "$W/corpus/redis" "$W/build/redis"
  (cd "$W/build/redis" && bear -- make -j"$J" > ../redis.log 2>&1)
  # (The ground truth is of the server, src/, as the comparison's always was.)
  mkdir -p "$W/build/redis-src"
  python3 -c "import json,sys; d=json.load(open(sys.argv[1])); json.dump([e for e in d if '/src/' in e['file'] and '/deps/' not in e['file']], open(sys.argv[2], 'w'))" \
    "$W/build/redis/compile_commands.json" "$W/build/redis-src/compile_commands.json"
  truth "$W/build/redis" "$W/build/redis-src" truth-redis-src.json
}

git_() {
  fetch https://github.com/git/git 777489f9e09c8d0dd6b12f9d90de6376330577a2 "$W/src/git"
  release "$W/src/git" "$W/corpus/git-c"
  rm -rf "$W/build/git" && cp -a "$W/corpus/git-c" "$W/build/git"
  (cd "$W/build/git" && bear -- make -j"$J" NO_CURL=1 NO_OPENSSL=1 NO_GETTEXT=1 > ../git.log 2>&1)
  truth "$W/build/git" "$W/build/git" truth-git.json
}

binutils() {
  fetch https://sourceware.org/git/binutils-gdb.git ced7bfccaa63502f0765783dd513d174d415ef9e "$W/src/binutils-gdb"
  release "$W/src/binutils-gdb" "$W/build/binutils-src"
  # The comparison's binutils: the C programs and libraries, without gdb.
  rm -rf "$W/corpus/c-only" && mkdir -p "$W/corpus/c-only"
  for d in bfd binutils gas gprof include ld libctf libiberty libsframe opcodes; do
    cp -a "$W/build/binutils-src/$d" "$W/corpus/c-only/$d"
  done
  rm -rf "$W/build/binutils" && mkdir -p "$W/build/binutils"
  (cd "$W/build/binutils" && ../binutils-src/configure --enable-targets=all --enable-64-bit-bfd --disable-nls --disable-werror \
      --disable-gdb --disable-gdbserver --disable-sim --disable-gold --disable-gprofng --disable-readline \
      --without-debuginfod --without-zstd --with-system-zlib > ../binutils.log 2>&1 \
    && bear -- make -j"$J" >> ../binutils.log 2>&1)
  truth "$W/build/binutils-src" "$W/build/binutils" truth.json
}

# A compile_commands.json for sources clang compiles for an Arm chip: each
# .c file given, if it compiles with these flags.
cross_db() {  # root out flags... -- files...
  local root=$1 out=$2; shift 2
  local flags=()
  while [ "$1" != "--" ]; do flags+=("$1"); shift; done
  shift
  python3 - "$root" "$out" "$(clang-18 -print-resource-dir)" "${flags[@]}" -- "$@" <<'PY'
import json, subprocess, sys
root, out, res = sys.argv[1:4]
rest = sys.argv[4:]
cut = rest.index("--")
flags, files = rest[:cut], rest[cut + 1:]
db = []
for f in files:
    args = ["clang-18", "-resource-dir", res, *flags, "-c", f"{root}/{f}", "-o", "/dev/null"]
    if subprocess.run(["clang-18", *flags, "-fsyntax-only", f"{root}/{f}"], cwd=root, capture_output=True).returncode == 0:
        db.append({"directory": root, "file": f"{root}/{f}", "arguments": args})
json.dump(db, open(out, "w"), indent=0)
print(f"  {len(db)} of {len(files)} files compile")
PY
}

stm32f4() {
  fetch https://github.com/STMicroelectronics/stm32f4xx-hal-driver.git 1f6451c3e07728b4c830744de380e56bf5bc0026 "$W/src/stm32f4xx-hal-driver"
  fetch https://github.com/STMicroelectronics/cmsis-device-f4.git 9192c7b9df75a142f2027ab266601fe061fc00b3 "$W/src/cmsis-device-f4"
  fetch https://github.com/ARM-software/CMSIS_5.git 55b19837f5703e418ca37894d5745b1dc05e4c91 "$W/src/CMSIS_5"
  # Laid out as STM32CubeMX lays out a project of the STM32F407.
  local c=$W/corpus/stm32f4
  rm -rf "$c" && mkdir -p "$c/Core/Inc" "$c/Drivers/STM32F4xx_HAL_Driver" "$c/Drivers/CMSIS/Device/ST/STM32F4xx/Source"
  cp -a "$W/src/stm32f4xx-hal-driver/Inc" "$W/src/stm32f4xx-hal-driver/Src" "$c/Drivers/STM32F4xx_HAL_Driver/"
  rm -f "$c"/Drivers/STM32F4xx_HAL_Driver/Src/*_template.c
  cp "$W/src/stm32f4xx-hal-driver/Inc/stm32f4xx_hal_conf_template.h" "$c/Core/Inc/stm32f4xx_hal_conf.h"
  cp "$W/src/stm32f4xx-hal-driver/LICENSE.md" "$c/LICENSE-HAL.md"
  cp -a "$W/src/cmsis-device-f4/Include" "$c/Drivers/CMSIS/Device/ST/STM32F4xx/"
  cp "$W/src/cmsis-device-f4/Source/Templates/system_stm32f4xx.c" "$c/Drivers/CMSIS/Device/ST/STM32F4xx/Source/"
  cp "$W/src/cmsis-device-f4/LICENSE.md" "$c/LICENSE-CMSIS-DEVICE.md"
  cp -a "$W/src/CMSIS_5/CMSIS/Core/Include" "$c/Drivers/CMSIS/"
  cp "$W/src/CMSIS_5/LICENSE.txt" "$c/LICENSE-CMSIS.txt"
  cp "$GLUE/stm32f4/Makefile" "$GLUE/stm32f4/.cproject" "$c/"
  mkdir -p "$W/build/stm32f4"
  (cd "$c" && cross_db "$c" "$W/build/stm32f4/compile_commands.json" \
      --target=thumbv7em-none-eabihf -mcpu=cortex-m4 -mfpu=fpv4-sp-d16 -mfloat-abi=hard -ffreestanding -std=gnu11 \
      -DDEBUG -DSTM32F407xx -DUSE_HAL_DRIVER -DUSE_FULL_LL_DRIVER \
      -I"$c/Core/Inc" -I"$c/Drivers/STM32F4xx_HAL_Driver/Inc" -I"$c/Drivers/CMSIS/Device/ST/STM32F4xx/Include" -I"$c/Drivers/CMSIS/Include" \
      -- Drivers/STM32F4xx_HAL_Driver/Src/*.c Drivers/CMSIS/Device/ST/STM32F4xx/Source/system_stm32f4xx.c)
  truth "$c" "$W/build/stm32f4" truth-stm32f4.json
}

nrfx() {
  fetch https://github.com/NordicSemiconductor/nrfx.git d1f2c35a4820961f4f7b7b2ece007f8e037842db "$W/src/nrfx"
  fetch https://github.com/ARM-software/CMSIS_5.git 55b19837f5703e418ca37894d5745b1dc05e4c91 "$W/src/CMSIS_5"
  # An nRF52840 application: nrfx with the chips it is not about left out.
  local c=$W/corpus/nrfx
  rm -rf "$c" && mkdir -p "$c/CMSIS/Core" "$c/app"
  release "$W/src/nrfx" "$c/nrfx"
  rm -rf "$c"/nrfx/{doc,README.md,CHANGELOG.md,.gitignore} "$c"/nrfx/bsp/stable/mdk/{nrf54h,nrf54l,nrf71,nrf92}
  cp -a "$W/src/CMSIS_5/CMSIS/Core/Include" "$c/CMSIS/Core/"
  cp "$W/src/CMSIS_5/LICENSE.txt" "$c/CMSIS/"
  cp "$GLUE/nrfx/app/"* "$c/app/"
  local sys=$W/build/nrfx/sysroot
  mkdir -p "$sys" && cp -a "$GLUE/nrfx/sysroot/include" "$sys/"
  (cd "$c" && cross_db "$c" "$W/build/nrfx/compile_commands.json" \
      --target=thumbv7em-none-eabihf -mcpu=cortex-m4 -ffreestanding -std=gnu11 -DNRF52840_XXAA -isystem "$sys/include" \
      -I"$c/app" -I"$c/nrfx" -I"$c/nrfx/templates" -I"$c/nrfx/drivers/include" -I"$c/nrfx/bsp/stable" \
      -I"$c/nrfx/bsp/stable/templates" -I"$c/nrfx/bsp/stable/mdk" -I"$c/CMSIS/Core/Include" \
      -- app/main.c nrfx/drivers/src/*.c)
  truth "$c" "$W/build/nrfx" truth-nrfx.json
}

for name in "${@:-redis git binutils stm32f4 nrfx}"; do
  for n in $name; do
    echo "== $n $(date +%T)"
    case $n in
      redis) redis ;;
      git) git_ ;;
      binutils) binutils ;;
      stm32f4) stm32f4 ;;
      nrfx) nrfx ;;
      *) echo "unknown project $n" >&2; exit 2 ;;
    esac
  done
done
echo "corpora ready in $W/corpus, ground truth in $GT"
