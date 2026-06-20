# Changelog

## 0.0.3

**Fix:** read the daemon's renamed config path. In the agent/companion → daemon
rename, the daemon's on-disk config moved from `modelstat-agent-dev-nodejs` →
`modelstat-daemon-nodejs`. mcp ≤ 0.0.2 still looked for the old path and couldn't
find the device token, so queries failed against a freshly-installed daemon.
Upgrade with `npx @modelstat/mcp@latest`.
