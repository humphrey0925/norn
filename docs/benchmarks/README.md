# Benchmarks

Norn against other ways of indexing C, on public projects. Every number here is produced by
`bench/compare.py` and turned into this page by `bench/chart.py`; nothing is typed in.

![Calls found](calls-found.svg)

![Time to index](index-time.svg)

## How it is measured

- **Ground truth**: clang's own view of each project, compiled with its real build
  (`bench/gt_extract.py`): every function definition, and every call with the function it
  reaches. A tool's *calls found* is the share of those calls it reports with the right target;
  *precision* is the share of the calls it reports as certain that are right.
- **Index**: a cold index of the whole project, wall-clock, with peak memory and the size on disk.
- **Update**: the time to bring the index up to date after one file is edited.
- **Queries**: median latency of finding a symbol by name and of listing a function's callers,
  each tool queried the way it is meant to be (a warm server where it has one).
- **clangd** is given a compilation database with no include paths and no `-D`: what a project
  without a working `compile_commands.json` gives it. With a real build's database it would be
  slower to index and more accurate.
- ctags records definitions only, and GNU Global references to a name rather than calls from a
  function, so neither has a figure for calls.
- Norn marks a call it cannot be sure of (under an `#if` the configuration does not settle) as
  uncertain. *Calls found* counts every call a tool reports; *of them certain* counts only the
  ones it reports as certain, and *precision* is measured on those. The other tools mark none.
- Norn, clangd and codebase-memory-mcp run on Windows (NTFS); ctags, cscope and GNU Global, which
  are Unix tools, run in WSL 2 on a copy of each project on its ext4 file system.
- The charts compare the tools measured on every project; codebase-memory-mcp was measured on
  Redis and git only, and is in their tables.

Machine: Intel64 Family 6 Model 183 Stepping 1, GenuineIntel, 20 cores, 64 GB RAM, Windows-10-10.0.26200-SP0. Measured 2026-10-17.

## Redis

801 files, 392,046 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.69 s | 156 MB | 7 MB | 0.02 s | 0.02 ms | 0.03 ms | 100.0% | 100.0% | 99.3% | 99.8% |
| clangd | 24 s | 2,596 MB | 14 MB | 0.66 s | 0.45 ms | 32 ms | 97.4% | 95.8% | 95.8% | 96.0% |
| codebase-memory-mcp | 12 s | 1,054 MB | 118 MB | 5.40 s | 28 ms | 33 ms | 100.0% | 96.7% | 96.7% | 96.9% |
| cscope | 0.31 s | 83 MB | 23 MB | 0.16 s | 0.83 ms | 0.83 ms | 99.9% | 96.9% | 91.2% | 98.0% |
| GNU Global | 0.21 s | 18 MB | 8 MB | 0.02 s | 0.81 ms | 0.96 ms | 99.8% | – | – | – |
| Universal Ctags | 0.30 s | 15 MB | 4 MB | – | 0.47 ms | – | 100.0% | – | – | – |

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## git

919 files, 412,932 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.25 s | 111 MB | 5 MB | 0.01 s | 0.03 ms | 0.03 ms | 100.0% | 99.9% | 99.4% | 99.6% |
| clangd | 34 s | 4,478 MB | 14 MB | 1.26 s | 0.38 ms | 33 ms | 99.1% | 64.2% | 64.2% | 80.6% |
| codebase-memory-mcp | 12 s | 993 MB | 102 MB | 5.14 s | 28 ms | 34 ms | 96.9% | 85.9% | 85.9% | 99.7% |
| cscope | 0.37 s | 82 MB | 23 MB | 0.15 s | 0.77 ms | 0.79 ms | 97.0% | 87.9% | 79.3% | 99.9% |
| GNU Global | 0.22 s | 18 MB | 9 MB | 0.03 s | 1.00 ms | 1.03 ms | 96.8% | – | – | – |
| Universal Ctags | 0.31 s | 15 MB | 3 MB | – | 0.43 ms | – | 96.9% | – | – | – |

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## binutils

1,622 files, 2,145,473 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 2.71 s | 693 MB | 44 MB | 0.09 s | 0.02 ms | 0.02 ms | 100.0% | 99.6% | 95.5% | 99.7% |
| clangd | 23 s | 2,200 MB | 25 MB | 1.36 s | 0.45 ms | 33 ms | 97.0% | 32.2% | 32.2% | 63.5% |
| cscope | 1.54 s | 371 MB | 118 MB | 0.84 s | 0.96 ms | 0.99 ms | 94.2% | 70.6% | 64.9% | 89.8% |
| GNU Global | 0.87 s | 33 MB | 38 MB | 0.05 s | 0.82 ms | 1.11 ms | 96.3% | – | – | – |
| Universal Ctags | 1.38 s | 74 MB | 21 MB | – | 0.45 ms | – | 96.5% | – | – | – |

clangd is given no include paths and no `-D`, as a project without a compilation database would: most of the code that needs the build's include paths or definitions does not parse for it. With a real build's database it does far better; that is not what is measured here.

Versions: clangd 21.1.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## STM32F4 HAL

253 files, 639,267 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.88 s | 437 MB | 16 MB | 0.16 s | 0.03 ms | 0.03 ms | 99.4% | 96.5% | 96.5% | 100.0% |
| clangd | 2.19 s | 152 MB | 0 MB | 0.06 s | 0.24 ms | 15 ms | 4.1% | 0.0% | 0.0% | 0.0% |
| cscope | 0.53 s | 115 MB | 40 MB | 0.22 s | 0.93 ms | 0.88 ms | 54.5% | 75.7% | 74.3% | 97.7% |
| GNU Global | 0.70 s | 59 MB | 61 MB | 0.06 s | 0.83 ms | 1.01 ms | 100.0% | – | – | – |
| Universal Ctags | 0.73 s | 86 MB | 40 MB | – | 0.51 ms | – | 100.0% | – | – | – |

clangd is given no include paths and no `-D`, as a project without a compilation database would: most of the code that needs the build's include paths or definitions does not parse for it. With a real build's database it does far better; that is not what is measured here.

Versions: clangd 21.1.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## nrfx

535 files, 532,196 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.79 s | 228 MB | 10 MB | 0.03 s | 0.02 ms | 0.03 ms | 99.2% | 98.0% | 96.4% | 99.8% |
| clangd | 1.66 s | 202 MB | 0 MB | 0.08 s | 0.24 ms | 24 ms | 100.0% | 14.1% | 14.1% | 100.0% |
| cscope | 0.31 s | 52 MB | 24 MB | 0.13 s | 0.82 ms | 0.85 ms | 35.8% | 12.0% | 10.9% | 79.5% |
| GNU Global | 0.38 s | 34 MB | 29 MB | 0.03 s | 0.87 ms | 0.90 ms | 100.0% | – | – | – |
| Universal Ctags | 0.35 s | 50 MB | 23 MB | – | 0.48 ms | – | 100.0% | – | – | – |

clangd is given no include paths and no `-D`, as a project without a compilation database would: most of the code that needs the build's include paths or definitions does not parse for it. With a real build's database it does far better; that is not what is measured here.

Versions: clangd 21.1.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.
