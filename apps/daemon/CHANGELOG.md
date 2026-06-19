# Changelog

## 0.2.0 — daemon-renamed wire (breaking)

The daemon's server contract was renamed `agent`/`companion` → `daemon` to match
the server. Daemons ≤ 0.1.3 speak the pre-rename wire and are no longer
compatible with the current server — upgrade with `npx modelstat@latest`.

- heartbeat `POST /v1/agent/heartbeat` → `POST /v1/daemon/heartbeat`
- producer-version field `companion_version` → `daemon_version`
- env `AGENT_API_URL` → `DAEMON_API_URL`
- launchd label `ai.modelstat.agent` → `ai.modelstat.daemon`
- config dir `modelstat-agent-dev` → `modelstat-daemon`
