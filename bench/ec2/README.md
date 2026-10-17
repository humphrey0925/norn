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
| git | git/git `777489f` | `make NO_CURL=1 NO_OPENSSL=1 NO_GETTEXT=1` |
| binutils | sourceware binutils-gdb `ced7bfc`, without gdb | `configure` as in `corpora.sh`, `make` |
| STM32F4 HAL | ST's stm32f4xx-hal-driver `1f6451c` and cmsis-device-f4 `9192c7b`, Arm's CMSIS_5 `55b1983`, laid out as STM32CubeMX does | clang for the Cortex-M4 (STM32F407) |
| nrfx | Nordic's nrfx `d1f2c35` for an nRF52840, Arm's CMSIS_5 | clang for the Cortex-M4 |

The two embedded projects are assembled from their vendors' repositories
with the files in [`glue/`](glue): a Makefile and an Eclipse CDT project
for the STM32 build (which Norn reads its configuration from), and an
application's `main.c` and `nrfx_config.h` for nrfx.
