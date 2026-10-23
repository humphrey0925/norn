# Norn for AI agents

AI coding agents (Copilot's agent mode, Claude Code, Codex, OpenCode) find
their way around code with text search. In C that misses what matters most:
calls a macro writes, calls through function pointers kept in struct members,
and which `#if` branches the build compiles at all. Norn gives the agents its
index as tools, and a skill that tells them to set the build configuration
first and use the tools rather than grep.
[繁體中文版](agents.zh-TW.md)

- [Setting it up](#setting-it-up)
- [How an agent should use it](#how-an-agent-should-use-it)
- [The tools](#the-tools)
- [Build configurations](#build-configurations)
- [Reading the answers](#reading-the-answers)
- [How it works](#how-it-works)
- [When something is wrong](#when-something-is-wrong)

## Setting it up

| Agent | What to do |
|---|---|
| Copilot's agent mode, in VS Code | Nothing: the extension gives it the tools (VS Code 1.101 or later). For the skill too, run **Norn: Connect AI Agents** once |
| Claude Code (terminal, VS Code, desktop app) | **Norn: Connect AI Agents** in VS Code, or `norn setup` |
| Codex (command line, VS Code, the ChatGPT desktop app's Codex) | The same |
| OpenCode | The same |
| Any other client of the Model Context Protocol | Give it the command `norn mcp` (see [below](#other-clients)) |

**Norn: Connect AI Agents** (in VS Code, `Ctrl+Shift+P`) and `norn setup` (in
a terminal) do the same thing. Without VS Code, one line installs `norn` from
the latest release, checks it against the release's `SHA256SUMS`, and runs
`norn setup`:

```sh
# Linux, macOS
curl -fsSL https://norn-c.com/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://norn-c.com/install.ps1 | iex
```

(`NORN_VERSION=0.26.0` for another release; `NORN_NO_SETUP=1` to set no
agent up.) Or download the executable from the
[releases](https://github.com/humphrey0925/norn/releases) yourself (see
[the command line](cli.md#installing)) and run `norn setup`.

What it does, for each agent found on the machine:

1. Puts the server at `~/.norn/bin/norn`, a path that does not move. Each
   start of the VS Code extension keeps that copy at the extension's version.
2. Adds Norn to the agent's MCP servers, to start as `norn mcp`. Only Norn's
   own entry is written; the rest of each file, comments included, stays as
   it was:

   | Agent | Where |
   |---|---|
   | Claude Code | `claude mcp add --scope user`, Claude Code's own command |
   | Codex | `[mcp_servers.norn]` in `~/.codex/config.toml` |
   | OpenCode | `"mcp": { "norn": … }` in `~/.config/opencode/opencode.json` |

3. Puts the skill, `norn`, where the agents read skills:
   `~/.claude/skills/norn/` (Claude Code) and `~/.agents/skills/norn/`
   (Codex, OpenCode, Copilot).

An agent running already has the tools once it starts again. To check:

| Agent | Check |
|---|---|
| Claude Code | `claude mcp list` shows `norn … ✔ Connected`; `/skills` lists `norn` |
| Codex | `codex mcp list` shows `norn` enabled |
| OpenCode | `opencode mcp list` shows `norn ✓ connected` |
| Copilot | The tools button in the chat lists Norn's tools |

To take it all out again: `norn setup --remove`. To see what would be
written, without writing anything: `norn setup --print`.

### Other clients

Any client of the Model Context Protocol can start the server itself: the
command `norn`, with the argument `mcp`. Add the project's folder after it if
the client neither starts the server in that folder nor tells the server
which folder it has open (MCP `roots`): `norn mcp /path/to/project`. The skill
is the file [`skills/norn/SKILL.md`](../skills/norn/SKILL.md); give it to the
client as its instructions, if it has no skills.

## How an agent should use it

This is what the skill tells the agent, in short:

1. **Set the context first.** `list_configurations`: which build
   configuration is in force, and what else there is. If the task is about
   another board, target or build, `switch_configuration` to it, and tell the
   user. Answers under the wrong configuration look right and are wrong.
2. **Then find, don't grep.** `search_symbols` or `find_definition` to find a
   name; `read_symbol` for its code rather than the whole file;
   `find_callers`, `find_callees`, `call_tree` and `find_references` for how
   it is used.
3. **Read and search C/C++ through Norn too**: `read_file` rather than a
   plain file read, `search_code` rather than grep. Code in a switched-off
   `#if` branch looks like any other in a plain read; these say where it
   begins and ends, or leave it out.
4. **Before editing inside `#if`**, look at those marks, or ask
   `compiled_lines`: do not change a branch the configuration does not
   compile, unless the task is about that other configuration.
5. **Plain tools for the rest**: build files, other languages, documentation.

Prompts that work well:

- "Who calls `uart_send` on the STM32F4 board? Use Norn."
- "Before changing `flash_write`'s signature, list everything that calls it,
  through function pointers too."
- "Which function ends up handling a `CMD_RESET` message? Follow the calls
  from `dispatch`."
- "Is the code under `#ifdef USE_DMA` in `spi.c` compiled for the release
  build?"

## The tools

| Tool | Answers |
|---|---|
| `list_configurations` | The build configurations, which is in force, and what it defines |
| `switch_configuration` | Puts another configuration in force (the project's setting, shared with the editor) and re-indexes |
| `search_symbols` | Symbols matching part of a name: fragments in any order, initials, letters in order |
| `find_definition` | Where a name is defined and declared; given the file and line it is used on, the definition that place sees |
| `read_symbol` | The source of a definition with the comment above it, numbered and marked |
| `read_file` | A file, or some of its lines, numbered, with where the configuration stops and starts compiling it said on a line of its own; `hide_inactive` folds what is not compiled into that line |
| `search_code` | Text or a regular expression in the C/C++ files, as grep, but only in what the configuration compiles (`include_inactive` for the rest, marked); each match with the function it is in |
| `find_callers` | Every call of a function, including through macros and function pointers |
| `find_callees` | Every call a function makes, including the functions a member or variable it calls through can hold |
| `find_references` | Every use of a symbol |
| `call_tree` | What a function reaches through calls, or what reaches it, a few levels deep |
| `compiled_lines` | Which lines of a file the configuration compiles, or whether one line is |

Lists stop at 100 results unless the agent asks for more (`limit`), and say
how many there are. A name with several definitions answers for each; `file`
chooses one. Paths are relative to the project's folder.

## Build configurations

A build configuration is what the build defines: which board, chip or
target, Debug or Release. It decides which `#if` branches are compiled, and
so which definitions, calls and lines the tools see. Norn finds them in the
project's own build files (Keil, IAR, Eclipse/DS-5, Visual Studio, CMake,
Makefile, Kconfig, PlatformIO, `compile_commands.json`), and a project can
have its own in `.norn/config.json` (see the
[user guide](norn-guide.md#10-compile-conditions)).

`switch_configuration` changes the project's setting, as **Norn: Switch
Compile Configuration…** does in VS Code: the editor, and every agent of the
project, see the same configuration from then. The skill tells the agent to
say which one it chose and why.

With no configuration, a branch nothing settles is indexed both ways, and
calls in it are marked `[maybe not compiled: …]`. An example, with
`BOARD_A` defined by one configuration and not the other:

```c
int main(void)
{
#ifdef BOARD_A
    return uart_a();
#else
    return uart_b();
#endif
}
```

```
read_file a.c                (BoardA in force)
     5  #ifdef BOARD_A
     6      return uart_a();
     7  #else
        >>> not compiled under BoardA (line 8)
     8      return uart_b();
        <<< end of not compiled (line 8)
     9  #endif

search_code uart_b
a.c:2   int uart_b(void) { return 2; }  [in uart_b]
(1 more where BoardA does not compile it: include_inactive to see it)
```

```
find_callees main            (no configuration in force)
  a.c:6  uart_a (function, a.c:1) [maybe not compiled: under the #if at line 5]
  a.c:8  uart_b (function, a.c:2) [maybe not compiled: under the #if at line 7]

switch_configuration BoardA
find_callees main
  a.c:6  uart_a (function, a.c:1)
```

## Reading the answers

| Marker | Means |
|---|---|
| `— via macro X` | The call is written by macro `X` where it is used |
| `— through member send, which can hold it` | A call through a function pointer kept in a struct member; the function listed is one stored there somewhere |
| `— through variable handler, which can hold it` | The same, through a variable |
| `— through a pointer that can hold it` | Through a local pointer variable |
| `[uncertain]` | Several definitions could be meant |
| `[maybe not compiled: under the #if at line N]` | The configuration in force does not settle that `#if` |
| `[in a file the build does not compile]` | No compilation of the build reads the file |
| `>>> not compiled under X (lines a–b)` … `<<< end of not compiled` (`read_file`, `read_symbol`) | The code between is there but not compiled under the configuration in force |
| `>>> maybe not compiled: under the #if at line N` … `<<< end` | The code between is under an `#if` the configuration does not settle |
| `✗` / `?` before a match (`search_code`) | The same, for one line |
| `Listed N of M` | Ask for more with `limit`, or narrow with `file` |

## How it works

- **One project, one index.** The first Norn of a project (an agent's
  `norn mcp`, or VS Code's language server) makes the index; the others are
  answered from it rather than make their own. VS Code's language server
  takes the project over when it opens the folder, so agents are answered
  from the editor's index. If the one holding the index ends, another takes
  over from the index kept on disk (`.norn/cache`). Who holds which project
  is kept in `~/.norn/run`; the others reach it on this machine only, with a
  word written there.
- **Which project.** The folder given; else the folder the client says it
  has open; else the folder the server was started in, but never the home
  folder or the root of the file system, where a desktop app starts servers.
- **Up to date.** Files are watched as the editor watches them: an edited
  source file is read again before the next answer; a changed build
  configuration has the whole tree looked at again.
- **Private.** Everything runs on your machine. The server speaks only to the
  agent that started it and to other Norn processes on the same machine.

## When something is wrong

| Symptom | What to do |
|---|---|
| The agent does not list Norn's tools | Start the agent again after `norn setup`. Check with the agent's MCP list (see [above](#setting-it-up)) |
| "Norn does not know which project to index" | The agent started the server outside the project: give the folder in its MCP settings (`norn mcp /path/to/project`) |
| The first answer takes a while | The project is being indexed; the next answers are immediate, and a restart reads the index from `.norn/cache` |
| Answers look wrong for the board or build in question | Another configuration is in force: `list_configurations`, then `switch_configuration` |
| The agent still greps for callers | Check that it has the skill (`/skills` in Claude Code, `ls ~/.agents/skills ~/.claude/skills`), and ask it to use Norn |
| A second index of the same project | The agent's `norn` and the editor's are of different versions: open VS Code once (it updates `~/.norn/bin`), or run `norn setup` with the newer one |
| Codex in VS Code or the ChatGPT desktop app does not find the project | Add the project's folder to `args` in `[mcp_servers.norn]` of `~/.codex/config.toml` |

### What has been tried

- Tried: Claude Code 2.1.309, Codex CLI 0.164.0 and OpenCode 1.19.4 start
  `norn mcp` from what `norn setup` writes, call its tools and get the
  answers, and load the skill. A local stand-in played the model, so this
  shows the wiring, not whether a real model chooses the tools.
- Tried: VS Code (Linux, Windows): the extension's MCP server answers
  `initialize`, `tools/list` and `tools/call`.
- Tried: `install.sh` on Linux, against a release, and with a checksum that
  did not match (nothing installed).
- Not tried: `install.ps1` (no PowerShell where it was written).
- Not tried: whether Copilot loads `~/.agents/skills/norn/` (VS Code's
  documentation says it reads that folder).
- Not tried: Codex in VS Code and in the ChatGPT desktop app, and the folder
  they start the server in.
