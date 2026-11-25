#!/bin/sh
# Install agent/SKILL.md into the skill directories of the agent harnesses on this machine.
#
# One file, five locations: the Agent Skills standard (~/.agents/skills) is read by Codex and Pi;
# Claude Code, Pi and OpenCode each add their own path. Nothing here edits a harness config other
# than adding one directory, so --uninstall reverses exactly one run.
set -eu

HARNESSES="agents claude codex opencode pi"
NAME="sangkar-graph"

scope="user"
harness="all"
mode="link"
project_dir=""
force=0
dry_run=0
uninstall=0

usage() {
  cat <<'EOF'
usage: install.sh [options]

  --scope user|project   where to install (default: user)
  --harness NAME         agents | claude | codex | opencode | pi | all (default: all)
  --mode link|copy       symlink this checkout's SKILL.md, or copy it (default: link)
  --project-dir DIR      repository root for --scope project (default: git top level, else cwd)
  --force                overwrite a SKILL.md that differs from this one
  --uninstall            remove the installed SKILL.md
  --dry-run              print what would happen, change nothing
  -h, --help             this text

Locations, --scope user:
  agents, codex   $HOME/.agents/skills
  claude          $HOME/.claude/skills
  pi              $HOME/.pi/agent/skills
  opencode        ${XDG_CONFIG_HOME:-$HOME/.config}/opencode/skills

Locations, --scope project:
  agents, codex, pi   <root>/.agents/skills
  claude              <root>/.claude/skills
  opencode            <root>/.opencode/skills
EOF
}

die() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --scope) scope="${2:-}"; shift 2 ;;
    --harness) harness="${2:-}"; shift 2 ;;
    --mode) mode="${2:-}"; shift 2 ;;
    --project-dir) project_dir="${2:-}"; shift 2 ;;
    --force) force=1; shift ;;
    --uninstall) uninstall=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

case "$scope" in
  user | project) ;;
  *) die "--scope must be user or project, not '$scope'" ;;
esac
case "$mode" in
  link | copy) ;;
  *) die "--mode must be link or copy, not '$mode'" ;;
esac
case "$harness" in
  agents | claude | codex | opencode | pi | all) ;;
  *) die "--harness must be one of: $HARNESSES, all" ;;
esac

self_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
src="$self_dir/SKILL.md"
[ -f "$src" ] || die "no SKILL.md next to this script ($src)"

if [ "$scope" = user ]; then
  root="${HOME:-}"
  [ -n "$root" ] || die "HOME is not set"
else
  if [ -z "$project_dir" ]; then
    project_dir=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
  fi
  root=$(CDPATH= cd -- "$project_dir" && pwd) || die "no such directory: $project_dir"
fi

# One target per distinct directory: codex and agents share a path, and so do agents and pi in a
# project, so an "all" install writes each file once and says which harnesses read it.
targets=""
for h in $HARNESSES; do
  if [ "$harness" != all ] && [ "$harness" != "$h" ]; then
    continue
  fi
  case "$h:$scope" in
    agents:user | codex:user) dir="$root/.agents/skills" ;;
    claude:user) dir="$root/.claude/skills" ;;
    pi:user) dir="$root/.pi/agent/skills" ;;
    opencode:user) dir="${XDG_CONFIG_HOME:-$root/.config}/opencode/skills" ;;
    agents:project | codex:project | pi:project) dir="$root/.agents/skills" ;;
    claude:project) dir="$root/.claude/skills" ;;
    opencode:project) dir="$root/.opencode/skills" ;;
    *) die "no location for $h/$scope" ;;
  esac
  targets="$targets$dir|$h
"
done

# Collapse the (directory, harness) pairs into one line per directory, so an "all" install writes
# each file once and prints every harness that reads it.
printf '%s\n' "$targets" | awk -F'|' '
  NF == 2 && $1 != "" {
    if (!($1 in seen)) { seen[$1] = 1; order[++n] = $1 }
    label[$1] = (label[$1] == "" ? $2 : label[$1] ", " $2)
  }
  END { for (i = 1; i <= n; i++) print order[i] "|" label[order[i]] }
' | while IFS='|' read -r dir h; do
  [ -n "${dir:-}" ] || continue
  dest="$dir/$NAME/SKILL.md"

  if [ "$uninstall" = 1 ]; then
    if [ -e "$dest" ] || [ -L "$dest" ]; then
      printf '%s: remove %s\n' "$h" "$dest"
      [ "$dry_run" = 1 ] || rm -f "$dest"
      [ "$dry_run" = 1 ] || rmdir "$dir/$NAME" 2>/dev/null || true
    else
      printf '%s: nothing at %s\n' "$h" "$dest"
    fi
    continue
  fi

  if [ -e "$dest" ] || [ -L "$dest" ]; then
    # A symlink to this exact file, or a byte-identical copy, is already the desired state; a
    # differing file is the user's, and is replaced only on request.
    if [ -L "$dest" ] && [ "$(readlink "$dest")" = "$src" ]; then
      printf '%s: up to date %s\n' "$h" "$dest"
      continue
    fi
    if [ ! -L "$dest" ] && cmp -s "$src" "$dest"; then
      printf '%s: up to date %s\n' "$h" "$dest"
      continue
    fi
    if [ "$force" != 1 ]; then
      printf '%s: kept existing %s (differs; rerun with --force to overwrite)\n' "$h" "$dest" >&2
      continue
    fi
  fi

  printf '%s: install %s -> %s\n' "$h" "$dest" "$( [ "$mode" = link ] && printf '%s' "$src" || printf 'copy' )"
  if [ "$dry_run" = 1 ]; then
    continue
  fi
  mkdir -p "$dir/$NAME" || { printf '%s: cannot create %s\n' "$h" "$dir/$NAME" >&2; continue; }
  rm -f "$dest"
  if [ "$mode" = link ]; then
    ln -s "$src" "$dest"
  else
    cp "$src" "$dest"
  fi
done

if [ "$uninstall" = 1 ] || [ "$dry_run" = 1 ]; then
  exit 0
fi

cat <<EOF

Installed '$NAME'. Register the server once per client so the tools exist at all:

  claude mcp add --scope user sangkar -- sangkar mcp
  codex:  add [mcp_servers.sangkar] to ~/.codex/config.toml with command = "sangkar", args = ["mcp"]
  opencode: add {"mcp":{"sangkar":{"type":"local","command":["sangkar","mcp"]}}} to opencode.json
  pi:     pi mcp add sangkar -- sangkar mcp

When 'sangkar' is not on PATH, register the npm launcher instead; it resolves the platform package
and execs the native binary with the same arguments and exit code:
  npx sangkar-mcp mcp --repo-root /path/to/repo
That binary is Linux x64 in this release; the launcher is Node.js >= 18 and exits 70 with a one-line
refusal on any other platform, naming the version to pin for the TypeScript build.
The repository is bound at startup, so registrations for two worktrees are two server entries,
each with its own --repo-root and --data-dir.
EOF
