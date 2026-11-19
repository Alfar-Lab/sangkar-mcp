# Agent skill

`SKILL.md` is the sangkar graph skill in the [Agent Skills](https://agentskills.io/specification)
format: a directory holding one `SKILL.md` with `name` + `description` frontmatter, read by Codex,
Claude Code, OpenCode, Pi, and anything else that implements the standard. It teaches an agent the
best-practice order of calls (`summary` → `search`/`trace` → `impact`, `status` before any absence
claim), how to read the envelope (`total_relation`, `coverage.complete`, `truncated`/`next`), and
which calls not to make (a `trace` is not a file reader, a bigger `budget` is not a fix, `index` is
not a query).

The skill assumes the server is registered. `SKILL.md` also covers the CLI verbs for a harness that
speaks no MCP.

## Install

```sh
./agent/install.sh --dry-run      # show the paths it would write
./agent/install.sh                # user scope, symlinks, every harness found below
./agent/install.sh --scope project --project-dir .   # commit the skill with a repository
./agent/install.sh --mode copy    # copy instead of symlink (portable, no stale links)
./agent/install.sh --uninstall    # remove what this script wrote
```

By default it symlinks, so editing `agent/SKILL.md` updates every harness at once. It never
overwrites a file it did not write: a differing `SKILL.md` is reported and left alone until you add
`--force`.

## Where each harness reads it

| Harness | User scope | Project scope | Verified from |
|---|---|---|---|
| Agent Skills standard / Codex | `~/.agents/skills/` | `.agents/skills/`, plus every ancestor up to the repo root; `/etc/codex/skills` is the machine-wide one | Codex "Build skills" docs |
| Claude Code | `~/.claude/skills/` | `.claude/skills/` | Claude Code skills layout |
| Pi | `~/.pi/agent/skills/` | `.agents/skills/` | `pi` docs, `docs/skills.md` |
| OpenCode | `${XDG_CONFIG_HOME:-~/.config}/opencode/skills/` | `.opencode/skills/` | local `~/.config/opencode/skills/` |
| Anything else | point the tool at `agent/SKILL.md`, or paste the body into its rules file (`AGENTS.md`, `CLAUDE.md`) — the frontmatter is the only harness-specific part | | |

`--harness agents|claude|codex|opencode|pi|all` narrows the install; `codex` and `agents` resolve to
the same directory, so the script writes it once.

## Register the server

The skill teaches the tools; the client still has to be told they exist. One stdio server, bound to
the repository at startup:

```sh
sangkar mcp --repo-root /path/to/repo
```

`--data-dir /path/to/dir` is the optional second flag. When `sangkar` is not on `PATH`, run the
script through Bun instead: `~/.bun/bin/bun run /path/to/bin/sangkar.ts mcp`.

| Client | Registration |
|---|---|
| Claude Code | `claude mcp add --scope user sangkar -- sangkar mcp` (or a `.mcp.json` `mcpServers` entry) |
| Codex | `[mcp_servers.sangkar]` in `~/.codex/config.toml`, `command = "sangkar"`, `args = ["mcp"]` |
| OpenCode | `"mcp": {"sangkar": {"type": "local", "command": ["sangkar", "mcp"]}}` in `opencode.json` |
| Pi | `pi mcp add sangkar -- sangkar mcp`, or a `mcpServers` entry in `~/.pi/agent/mcp.json` |
| Any `mcpServers` client | `{"mcpServers": {"sangkar": {"command": "sangkar", "args": ["mcp"]}}}` |

The README's [MCP](../README.md#mcp) section has the JSON form with `--repo-root` and
`--data-dir`, including the two-entry case for a linked worktree. One repository is one server
entry — a tool call never takes a project argument.

## Keeping it true

`SKILL.md` restates what `src/tools/*/manifest.ts` declares: argument names, defaults, enum values,
and the budget ceilings. `docs/INSTRUCTIONS.md` is the in-protocol version of the same advice. When
a manifest changes, the skill changes in the same commit, or an agent is told about a tool that no
longer answers that way.
