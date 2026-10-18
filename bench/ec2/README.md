# The benchmark on EC2

The comparison in [docs/benchmarks](../../docs/benchmarks/README.md) made
again from nothing on a machine of its own: the five projects fetched at
the versions measured, built, their ground truth made with clang, every
tool run three times on each, and the charts drawn.

## On EC2

Needs an AWS account, credentials for it (`aws configure`), an S3 bucket in
the region, and Python with boto3 (`pip install boto3`).

```bash
python bench/ec2/launch.py --bucket <your bucket>
```

says what it would launch and what that costs; add `--yes` to launch it.
It runs the committed checkout (HEAD) on a Spot `c6id.4xlarge` (16 vCPUs,
local NVMe) in `us-east-1`, waits, and puts the results in
`out/ec2/<run>/results`: one JSON per project and tool, and the charts and
page in `benchmarks/`. The instance has no key pair, accepts no
connections and holds no credentials (it reaches S3 through signed URLs
that expire), and it terminates itself when done or after `--hours`
(default 5) whatever happens.

## On any Ubuntu 24.04

The same three steps the instance runs:

```bash
bench/ec2/setup.sh      # compilers, clang's Python bindings, the tools compared, Rust, Norn
bench/ec2/corpora.sh    # the projects, built, and their ground truth in out/gt
bench/ec2/run.sh        # the comparison; results in ~/bench/results
```

## The projects

| Project | Source | Built with |
|---|---|---|
| Redis | redis/redis `fcc9b53` | `make` |
| git | git/git `777489f` | `make NO_CURL=1 NO_OPENSSL=1 NO_GETTEXT=1 NO_TCLTK=1` |
| binutils | sourceware binutils-gdb `ced7bfc`, without gdb | `configure` as in `corpora.sh`, `make` |
| STM32F4 HAL | ST's stm32f4xx-hal-driver `1f6451c` and cmsis-device-f4 `9192c7b`, Arm's CMSIS_5 `55b1983`, laid out as STM32CubeMX does | clang for the Cortex-M4 (STM32F407) |
| nrfx | Nordic's nrfx `d1f2c35` for an nRF52840, Arm's CMSIS_5 | clang for the Cortex-M4 |

The two embedded projects are assembled from their vendors' repositories
with the files in [`glue/`](glue): a Makefile and an Eclipse CDT project
for the STM32 build (which Norn reads its configuration from), and an
application's `main.c` and `nrfx_config.h` for nrfx.

## More projects

Ground truth for six more corpora, made on WSL2 Ubuntu 24.04 with clang 18
and `bear` (not yet in `corpora.sh` or the EC2 run). Each was built from a
copy of the tree Norn indexes (`corpus/<name>`, as released), out of tree
where the build allows, and `bench/gt_extract.py <src> <build> <out>` run on
the build's `compile_commands.json`. Extra packages: `libpsl-dev
libnghttp2-dev libbrotli-dev tcl libgnutls28-dev uuid-dev swig
device-tree-compiler python3-pyelftools efitools`.

| Project | Source | Built with |
|---|---|---|
| curl | curl/curl `a4d6184` (8.23.0-DEV) | `CC=clang cmake <src> -G Ninja -DCMAKE_BUILD_TYPE=Debug -DCURL_USE_OPENSSL=ON -DUSE_NGHTTP2=ON -DCURL_USE_LIBPSL=ON -DCURL_BROTLI=ON -DCURL_ZSTD=ON -DUSE_LIBIDN2=OFF -DCURL_USE_LIBSSH2=OFF`, `bear -- ninja` |
| SQLite | sqlite/sqlite `39315a4` (3.55.0) | `CC=clang <src>/configure --disable-amalgamation --all --disable-shared`, `bear -- make USE_AMALGAMATION=0 lib sqlite3d` (the library from `src/*.c`, not `sqlite3.c`); the entry bear records for clang's internal `-cc1` run of `shell.c` dropped |
| Mbed TLS | Mbed-TLS/mbedtls `38c5d91` (4.2.0), with its submodules framework `7d726d1` and TF-PSA-Crypto `a22ae75` (its framework `122857f`) fetched for the build only | `CC=clang cmake <src> -G Ninja -DCMAKE_BUILD_TYPE=Debug -DENABLE_TESTING=ON -DENABLE_PROGRAMS=ON`, `bear -- ninja`; the truth is of the translation units in the corpus (`library/`, `programs/`, `tests/src/`, and the generated `library/` files), not of the submodules nor the generated test suites |
| FreeRTOS | FreeRTOS/FreeRTOS-Kernel `8be86d4` | the POSIX port (`FREERTOS_PORT=GCC_POSIX`, heap 4) with the tree's `examples/template_configuration` and `examples/cmake_example/main.c`, from a CMakeLists.txt outside the tree that does `add_subdirectory(<src>)`; `CC=clang cmake -G Ninja`, `bear -- ninja` |
| lwIP | lwip-tcpip/lwip `d08f477` (2.2.2) | the unix port's `example_app` (the top-level CMakeLists.txt on Linux), `lwipcfg.h` a copy of `contrib/examples/example_app/lwipcfg.h.ci` outside the tree: `CC=clang cmake <src> -G Ninja -DCMAKE_BUILD_TYPE=Debug -DCMAKE_C_FLAGS=-I<dir of lwipcfg.h>`, `bear -- ninja` |
| U-Boot | u-boot/u-boot `v2026.10` (`5508406`) | `make O=<build> CC=clang HOSTCC=clang sandbox_defconfig`, `bear --output <build>/compile_commands.json -- make O=<build> CC=clang HOSTCC=clang NO_SDL=1`; the truth is of the `.c` entries (not `.S`, nor `-cc1` duplicates). Its `<build>/.config` and `include/generated/autoconf.h` are the configuration Norn should infer |

The source files were checked out on Windows (CRLF); SQLite's build
scripts (every file not `.c` or `.h`) were converted to LF in the build's
copy so `configure` and its Tcl tools run. Line numbers are unchanged.
