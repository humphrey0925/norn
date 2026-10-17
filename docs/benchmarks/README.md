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
- **Index**: an index of the whole project from nothing (the files already in the OS cache),
  wall-clock, with peak memory and the size on disk.
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
- Each tool indexes each project three times; the median is reported.

Machine: Intel64 Family 6 Model 183 Stepping 1, GenuineIntel, 20 cores, 64 GB RAM, Windows-10-10.0.26200-SP0. Measured 2026-10-17.

## Redis

801 files, 392,046 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.32 s | 161 MB | 7 MB | 0.02 s | 0.02 ms | 0.03 ms | 100.0% | 100.0% | 99.3% | 99.8% |
| clangd | 11 s | 2,414 MB | 14 MB | 0.69 s | 0.46 ms | 32 ms | 97.4% | 95.8% | 95.8% | 96.0% |
| codebase-memory-mcp | 13 s | 1,030 MB | 136 MB | 6.20 s | 28 ms | 34 ms | 100.0% | 97.1% | 97.1% | 97.3% |
| cscope | 0.26 s | 83 MB | 23 MB | 0.14 s | 0.79 ms | 0.78 ms | 99.9% | 96.9% | 91.2% | 98.0% |
| GNU Global | 0.18 s | 18 MB | 8 MB | 0.03 s | 0.79 ms | 0.92 ms | 99.8% | – | – | – |
| Universal Ctags | 0.17 s | 15 MB | 4 MB | – | 0.42 ms | – | 100.0% | – | – | – |

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## git

919 files, 412,932 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.25 s | 110 MB | 5 MB | 0.01 s | 0.02 ms | 0.03 ms | 100.0% | 99.9% | 99.4% | 99.6% |
| clangd | 32 s | 4,411 MB | 14 MB | 1.23 s | 0.36 ms | 31 ms | 99.1% | 64.2% | 64.2% | 80.6% |
| codebase-memory-mcp | 12 s | 1,018 MB | 119 MB | 5.61 s | 28 ms | 34 ms | 96.9% | 85.8% | 85.8% | 99.7% |
| cscope | 0.27 s | 83 MB | 23 MB | 0.15 s | 0.76 ms | 0.79 ms | 97.0% | 87.9% | 79.3% | 99.9% |
| GNU Global | 0.18 s | 18 MB | 9 MB | 0.03 s | 0.79 ms | 0.85 ms | 96.8% | – | – | – |
| Universal Ctags | 0.17 s | 15 MB | 3 MB | – | 0.46 ms | – | 96.9% | – | – | – |

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## binutils

1,622 files, 2,145,473 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 2.72 s | 716 MB | 44 MB | 0.10 s | 0.02 ms | 0.02 ms | 100.0% | 99.6% | 95.5% | 99.7% |
| clangd | 16 s | 2,281 MB | 25 MB | 1.34 s | 0.45 ms | 31 ms | 97.0% | 32.2% | 32.2% | 63.5% |
| codebase-memory-mcp | 37 s | 2,090 MB | 399 MB | 6.97 s | 45 ms | 48 ms | 94.1% | 81.0% | 81.0% | 97.1% |
| cscope | 1.34 s | 374 MB | 118 MB | 0.00 s | 0.78 ms | 0.76 ms | 94.2% | 70.6% | 64.9% | 89.8% |
| GNU Global | 0.81 s | 30 MB | 38 MB | 0.07 s | 0.78 ms | 0.99 ms | 96.3% | – | – | – |
| Universal Ctags | 0.99 s | 73 MB | 21 MB | – | 0.44 ms | – | 96.5% | – | – | – |

clangd is given no include paths and no `-D`, as a project without a compilation database would: most of the code that needs the build's include paths or definitions does not parse for it. With a real build's database it does far better; that is not what is measured here.

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## STM32F4 HAL

253 files, 639,267 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.64 s | 422 MB | 16 MB | 0.14 s | 0.02 ms | 0.02 ms | 99.4% | 96.5% | 96.5% | 100.0% |
| clangd | 1.14 s | 166 MB | 0 MB | 0.05 s | 0.21 ms | 14 ms | 4.1% | 0.0% | 0.0% | 0.0% |
| codebase-memory-mcp | 9.62 s | 1,124 MB | 412 MB | 6.16 s | 62 ms | 60 ms | 99.7% | 85.8% | 85.8% | 98.1% |
| cscope | 0.44 s | 115 MB | 40 MB | 0.22 s | 0.77 ms | 0.76 ms | 54.5% | 75.7% | 74.3% | 97.7% |
| GNU Global | 0.68 s | 60 MB | 61 MB | 0.05 s | 0.79 ms | 0.89 ms | 100.0% | – | – | – |
| Universal Ctags | 0.60 s | 83 MB | 40 MB | – | 0.43 ms | – | 100.0% | – | – | – |

clangd is given no include paths and no `-D`, as a project without a compilation database would: most of the code that needs the build's include paths or definitions does not parse for it. With a real build's database it does far better; that is not what is measured here.

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.

## nrfx

535 files, 532,196 lines.

| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| **Norn** | 0.84 s | 244 MB | 10 MB | 0.02 s | 0.02 ms | 0.03 ms | 99.2% | 98.0% | 96.4% | 99.8% |
| clangd | 1.22 s | 210 MB | 0 MB | 0.13 s | 0.22 ms | 22 ms | 100.0% | 14.1% | 14.1% | 100.0% |
| codebase-memory-mcp | 8.29 s | 920 MB | 202 MB | 5.65 s | 47 ms | 45 ms | 96.2% | 94.2% | 94.2% | 97.2% |
| cscope | 0.28 s | 52 MB | 24 MB | 0.12 s | 0.81 ms | 0.80 ms | 35.8% | 12.0% | 10.9% | 79.5% |
| GNU Global | 0.36 s | 34 MB | 29 MB | 0.03 s | 0.79 ms | 0.83 ms | 100.0% | – | – | – |
| Universal Ctags | 0.32 s | 48 MB | 23 MB | – | 0.49 ms | – | 100.0% | – | – | – |

clangd is given no include paths and no `-D`, as a project without a compilation database would: most of the code that needs the build's include paths or definitions does not parse for it. With a real build's database it does far better; that is not what is measured here.

Versions: clangd 21.1.0; codebase-memory-mcp 0.11.0; cscope 15.9; GNU Global 6.6.11; Universal Ctags 5.9.0.
