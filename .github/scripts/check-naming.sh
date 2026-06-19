#!/usr/bin/env bash
# Naming guard — our local process is the "daemon".
#
#   • "companion" is RETIRED (allowed only in immutable applied migrations).
#   • "agent" means ONLY the user's AI tool. These stay and are NOT flagged:
#       the AGENTS enum, the `agent` event/wire field, the 402 /v1/agent/credit
#       surface, the /device/:claim/agent AI-facing view, the User-Agent header,
#       the "agent tools" UI category, AGENTS.md, "agentic", agent-framework names.
#
# This fails CI if a RETIRED our-process name reappears. It is a DENYLIST of the
# exact renamed tokens — deliberately narrow so it never flags the legit
# AI-tool "agent". If you're adding a genuinely new AI-tool "agent" reference and
# it trips a token here, the token list (not your code) is what to revisit.
set -uo pipefail

DENY='[Cc]ompanion|/v1/agent/heartbeat|valid_agent_phase|\bagent_url\b|AGENT_API_URL|AGENT_VERSION|ai\.modelstat\.agent|\bapps/agent\b|agent-sdk|modelstat-agent|\bagent_(status|version|queue_size|progress_done|progress_total|stats|message|last_event_at|last_heartbeat_at)\b'

# Immutable applied migrations keep their historical names; skip generated/vendored
# files and this guard itself (it necessarily contains the forbidden tokens).
hits="$(git grep -nIE "$DENY" -- . \
  ':!*/migrations/*' ':!*routeTree.gen.ts' ':!*pnpm-lock.yaml' ':!*Cargo.lock' \
  ':!.github/scripts/check-naming.sh' 2>/dev/null || true)"

if [ -n "$hits" ]; then
  echo "::error::Naming guard failed — a retired name reappeared."
  echo "Our process is the 'daemon'. 'companion' is retired; 'agent' = the user's AI tool only"
  echo "(AGENTS enum, event/wire .agent, /v1/agent/credit, the /device/:claim/agent view,"
  echo "User-Agent, the 'agent tools' category, AGENTS.md). Offenders:"
  echo "$hits"
  exit 1
fi
echo "✓ naming guard: clean"
