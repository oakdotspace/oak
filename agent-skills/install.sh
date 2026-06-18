#!/usr/bin/env sh
# Install the Oak agent skill so coding agents know how to use Oak (not Git).
#
# Usage:
#   ./install.sh                 # install the Claude skill into ~/.claude/skills
#   CLAUDE_SKILLS_DIR=... ./install.sh   # install into a custom skills dir
#
# For Codex / Cursor / other AGENTS.md-based agents, you don't need this —
# `oak init` and `oak space new` already write an AGENTS.md into each repo.
# This script is only for installing the *global* Claude skill.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SRC="$SCRIPT_DIR/oak/skills/oak"
DEST_DIR="${CLAUDE_SKILLS_DIR:-$HOME/.claude/skills}"
DEST="$DEST_DIR/oak"

if [ ! -f "$SRC/SKILL.md" ]; then
  echo "error: can't find skill source at $SRC" >&2
  exit 1
fi

mkdir -p "$DEST_DIR"
rm -rf "$DEST"
cp -R "$SRC" "$DEST"

echo "Installed the 'oak' skill to $DEST"
echo "Start (or restart) Claude Code; it will load the skill automatically when"
echo "you work in an Oak repository."
