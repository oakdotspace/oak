#!/usr/bin/env bash
# SPIKE: hand a task to the agent runner (.oak/workflows/agent.yml). Starts a
# branch off main whose description is the prompt, publishes it, and triggers
# the agent workflow at that exact head.
#
# Usage: scripts/agent-dispatch.sh "<prompt>"
#
# The first line of the prompt names the branch. Run it from a clean checkout.
# To try a changed workflow before it reaches main, point AGENT_WORKFLOW_FILE
# at it (outside the checkout, like a copy of this script, since starting the
# task switches branches); workflows are read at the triggering commit.
#
# Follow it with:
#   oak ci runs --limit 5
#   oak ci logs RUN_ID
#   oak switch BRANCH         once the run is done, to see the result
set -euo pipefail

prompt="${1:?usage: agent-dispatch.sh \"<prompt>\"}"
workflow=".oak/workflows/agent.yml"

if [ -n "$(oak status --porcelain)" ]; then
  echo "checkout is dirty; oak start would carry those files onto the task" >&2
  exit 4
fi

oak start "${prompt%%$'\n'*}" --no-push --no-watch
branch="$(oak branch --show-current)"

# CI pins a commit, and a branch has no head of its own until it has one. The
# job removes this file before the agent starts.
echo "Removed by .oak/workflows/agent.yml when the job starts." > .oak-agent-task
if [ -n "${AGENT_WORKFLOW_FILE:-}" ]; then
  cp "$AGENT_WORKFLOW_FILE" "$workflow"
fi
oak commit

printf '%s\n' "$prompt" | oak desc --file -
oak push

head="$(oak hash)"
oak ci trigger --workflow agent --expected-commit "$head" \
  --idempotency-key "agent-$branch-$head" --json
