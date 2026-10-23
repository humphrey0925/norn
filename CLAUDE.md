# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Norn is a code index for large C (and C++) trees without a build: a Rust core
(`crates/si-core`), the `norn` executable that serves it as a language server
and a command line (`crates/si-cli`), a VS Code extension (`vscode/`), and the
website (`site/`). Users: [README.md](README.md), [docs/cli.md](docs/cli.md),
[docs/lsp-api.md](docs/lsp-api.md).

## Commits are the owner's

Every commit in this repository, its author and its committer both, is
the owner's own GitHub account:

    humphrey0925 <humphrey0925@users.noreply.github.com>

Never `Claude <noreply@anthropic.com>`, nor any other account. The CLA
check (`.github/workflows/cla.yml`) looks at who wrote each commit of a
pull request, and an author that is no GitHub account can never sign:
the pull request stays failed.

- Before the first commit of a session, set the identity for this
  repository (it overrides whatever the environment set):

      git config user.name humphrey0925
      git config user.email humphrey0925@users.noreply.github.com

- Before pushing, and before opening a pull request, check every commit
  of the branch:

      git log --format='%h %an <%ae> | %cn <%ce>' origin/master..HEAD

- A commit already made as someone else, on a branch of your own, is
  rewritten as the owner's before it is pushed (or pushed again, with
  `--force-with-lease`). Never rewrite a branch someone else works on.
- A trailer in the message (`Co-Authored-By: ...`) does not change who
  the author is, and may stay.

## Commits 必須是擁有者本人的帳號

這個 repository 的每個 commit，作者（author）和提交者（committer）都必須是擁有者自己的
GitHub 帳號 `humphrey0925 <humphrey0925@users.noreply.github.com>`，不可以是
`Claude <noreply@anthropic.com>` 或其他帳號。CLA 檢查會看 pull request 每個 commit
的作者，不是 GitHub 帳號的作者永遠無法簽署，PR 會一直失敗。做法見上：每個 session 第一次
commit 前先設定 `git config user.name` 和 `user.email`，推送和開 PR 前用 `git log` 檢查。

## Commands

What CI runs (`.github/workflows/ci.yml`, Rust 1.99.1, Linux and Windows on
every pull request):

    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    cargo build --release --locked          # target/release/norn

- One test, by any part of its name (tests live in `mod tests` at the end of
  each module): `cargo test --release -p si-core a_relink`.
- rustfmt is not enforced and the tree is not formatted with it: do not run
  `cargo fmt` over files; keep the style of the code around the change.
- The extension: `cd vscode && npm ci && npm run compile`. Its tests run VS
  Code against `corpus/c-only` (a few binutils files, not in the repository):
  fetch it once with `release/fetch-test-corpus.sh`, then `npm test`
  (`xvfb-run -a npm test` on Linux).
- The website: `node site/build.mjs` builds `out/site` (CI checks this);
  `npx wrangler dev` serves it. `wrangler.jsonc` deploys it to Cloudflare
  (production from `master`; other branches as previews, which need its
  `previews` block).

Running the index by hand (`norn --help`, [docs/cli.md](docs/cli.md)):

- `norn index <ROOT> --out <DIR>` indexes a tree and prints one JSON report:
  time per stage (`ms`), memory (`mem`, with `facts_parts`), counts.
- `norn symbols <DIR>` and `norn calls <DIR>` list what an index holds, one
  line each: the way to see whether a change altered results.
- `norn live <ROOT>` keeps an index over the sources, answering JSON lines on
  stdin (`{"op":"callers","name":"f"}`, `{"op":"update","files":[...]}`).
  `norn lsp` is the same index as the language server the extension starts.
- `norn bench-query <DIR>` (query latency) and `norn bench-incr <ROOT>`
  (re-extract one file and relink, with the time of each link stage).
- `crates/si-core/examples/` are diagnostics, each described in its first
  line, e.g. `cargo run --release -p si-core --example facts -- <root> <file>`.

## Architecture

The index is built in stages (`lib.rs` strings them together; `si-cli`'s
`index` command and `Session::open` both follow them):

1. **Scan** (`lib.rs`, `scope.rs`): the C and C++ files under the root, within
   the project's scope. A tree's own settings live in `.norn/config.json`, its
   cache in `.norn/cache` (`scope::home`; `.si/` is the old name).
2. **Conditions** (`cond.rs`, `builds.rs`, `kbuild.rs`, `units.rs`,
   `value.rs`): which macros the build defines, from the build's own files
   (IDE projects, CMake, Makefiles, Kconfig/Kbuild, compilation databases)
   and from the sources themselves (`settle`, repeated until nothing
   changes); `units.rs` works out, compilation by compilation, how headers
   read more than one way are read (`Readings`).
3. **Macro sets** (`mask::MacroSets`): which macros are transparent (expand to
   nothing or an attribute) and which expand into code; extraction depends on
   them, so a change to them re-extracts the files that use the macro.
4. **Extract**, per file and in parallel (`extract.rs`): `mask.rs` first blanks
   what derails a C parser without preprocessing (`#define`s, transparent
   macros, directives inside lists), keeping every byte offset; then a
   tree-sitter parse (C or C++) and one preorder walk produce `FileFacts`
   (`model.rs`): definitions, references, macro bodies and use sites, binds of
   functions to members, member hints, locals. A file's facts depend only on
   its text and the macro sets (`lex_key`), which is what lets them be cached
   and taken from another root's index.
5. **Link** (`link.rs`): include edges and their closures; macro expansion at
   link time (`expand.rs`, cached per file in `expand::Cache`); the name table
   (`names.rs` interns every name once per process; a link numbers the names in
   use by their order); the symbol table; resolving each file's references in
   parallel (a name means a definition in the file, then in what it includes,
   then anything visible; with C++ classes, overloads and conversions); then
   the graph in CSR form (`graph.rs`: forward and reverse adjacency, O(degree)
   queries).
6. **Session** (`session.rs`): the live index the language server holds. An
   update re-extracts the files that changed (and, when `#define`s changed, the
   files the macro sets now read otherwise), settles conditions again if it
   must, and relinks. A relink resolves again only what the edit can change
   (`link::Last`: per-file digests of what other files read of each file;
   files and their users otherwise read from the previous graph); new types,
   records, macros, `#if` groups and C++ files link in full.
7. **Store** (`store.rs`): each file's facts on disk, one small file per source
   file, plus the expansions, the macro sets and the graph, so that a start
   parses only what changed.

`si-cli`: `main.rs` (the commands; every one but the servers prints one JSON
object), `lsp.rs` (the language server), `lsp/more.rs` (folding, semantic
colouring, rename, inlay hints, includes). The extension (`vscode/src/extension.ts`)
starts `norn lsp` and adds the Source Insight-style views.

## Things that hold, and how to check them

- **Results do not change unless that is the point.** For a change meant to be
  faster or smaller only, index the same trees with `master`'s binary and the
  new one and compare the graph files byte for byte (`norn index ... --out a`,
  `cmp a/graph.si b/graph.si`), with 1 and with several threads; the session
  tests compare a relinked graph with one made anew the same way.
- **`store::FORMAT` is bumped** whenever what extraction records, or how it is
  laid out, changes: saved facts of another version are then not read. Types
  kept differently in memory (`model::Refs`, `model::Shared`) serialize as the
  plain `Vec` they stand for, so they need no bump.
- **Memory**: `FileFacts::shrink` copies vectors into blocks of their size,
  because mimalloc (the allocator here) does not give back what
  `shrink_to_fit` frees. Measure memory with `norn index` (`mem`) or with
  `SI_MEM_PROFILE`, not by reasoning.
- **Incremental relinks** are checked with `SI_LINK_CHECK=1`, which also
  resolves every file and says which files differ.

Diagnostics, all off unless set: `SI_START_PROFILE`, `SI_UPDATE_PROFILE`,
`SI_LINK_PROFILE` (where a start, an update, a link spends its time, and why a
link ran in full), `SI_EXPAND_PROFILE`, `SI_COND_PROFILE`, `SI_SCOPE_PROFILE`,
`SI_MEM_PROFILE` (memory at each stage), `SI_LINK_CHECK`, `SI_SETTLE_CHECK`,
and `SI_EDGE_WHY` / `SI_HINT_WHY` / `SI_COND_WHY` / `SI_KBUILD_WHY` (why an edge,
a member hint, a condition, a Kbuild file came out as it did). `NORN_CHUNK`
is an experiment (parse a file in pieces), off by default.

## Measurements

`bench/` measures accuracy against clang's own view of real builds and speed
against other indexers; `docs/benchmarks/README.md` and its charts are made by
`bench/compare.py` and `bench/chart.py` (nothing typed in), on a 20-core,
64 GB Windows machine (`bench/ec2/` redoes it on Ubuntu). A figure measured
anywhere else says which machine, and is compared only with figures from the
same machine under the same conditions: run both versions alternately rather
than against the README's numbers.

## Writing here

- Comments and commit messages are plain prose that says what the code does
  and why, in the voice of the code around them; commit messages carry the
  measurements that justify a change. Feature branches are merged with a merge
  commit titled `Merge <branch>: <what it does>`.
- User-facing documents come in pairs, English and Traditional Chinese
  (`README.md` / `README.zh-TW.md`, `docs/*.md` / `docs/*.zh-TW.md`, the site's
  two pages): change both. `docs/releasing.md` (the release process) is in
  Chinese only. Each release has its section in `CHANGELOG.md`.
