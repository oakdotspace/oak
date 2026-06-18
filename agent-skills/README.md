# Oak agent skill

Teaches AI coding agents to use **Oak** version control (the `oak` CLI)
instead of Git. There are two layers, and you want both for full coverage:

| Layer | Scope | Who reads it |
| --- | --- | --- |
| **This skill** | Global, install once | Claude Code / Claude.ai |
| **Per-repo `AGENTS.md`** | Per repository, auto-generated | Codex, Cursor, and any AGENTS.md-aware agent |

The per-repo file is written for you by `oak init` and `oak space new` — you
don't install anything for Codex/Cursor. This skill is the *global* piece that
gives Claude Code the Oak playbook everywhere, even in a bare repo.

## Install the skill (Claude)

**Option A — plugin marketplace (recommended, gives you updates):**

```
/plugin marketplace add oakvcs/agent-skills
/plugin install oak@oak
```

(Replace `oakvcs/agent-skills` with wherever this directory is published.)

**Option B — copy-in script:**

```bash
./install.sh
```

This drops the skill into `~/.claude/skills/oak`. Restart Claude Code; it
loads automatically when you're in an Oak repo or mention `oak` commands.

## Other agents (Codex, Cursor, …)

These read a repo's `AGENTS.md`, which Oak already generates. If you want the
Oak guidance available *globally* (not just in Oak repos), append the skill's
content to your agent's global instructions file — e.g. Codex's
`~/.codex/AGENTS.md`:

```bash
cat oak/skills/oak/SKILL.md oak/skills/oak/reference/*.md >> ~/.codex/AGENTS.md
```

## Layout

```
agent-skills/
├── .claude-plugin/marketplace.json   # `/plugin marketplace add` entry point
├── install.sh                        # copy-in installer (no plugin system)
└── oak/                              # the plugin
    ├── .claude-plugin/plugin.json
    └── skills/oak/
        ├── SKILL.md                  # entry point + everyday commands
        └── reference/
            ├── commands.md           # full command reference
            └── spaces.md             # mounts & Oak spaces workflow
```
