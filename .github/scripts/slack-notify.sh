#!/usr/bin/env bash
# Post a message to Slack via chat.postMessage — best-effort, never fails the
# caller. No-ops (exit 0) when SLACK_BOT_TOKEN is unset, so the workflow runs
# fine before the secret exists. Reads SLACK_BOT_TOKEN + SLACK_CHANNEL from env.
#
#   SLACK_BOT_TOKEN=… SLACK_CHANNEL=C… bash slack-notify.sh "hello *world*"
set -u

text="${1:?usage: slack-notify.sh <text>}"

if [ -z "${SLACK_BOT_TOKEN:-}" ]; then
  echo "SLACK_BOT_TOKEN unset — skipping Slack notify."
  exit 0
fi

resp="$(curl -fsS -X POST https://slack.com/api/chat.postMessage \
  -H "Authorization: Bearer ${SLACK_BOT_TOKEN}" \
  -H "Content-type: application/json; charset=utf-8" \
  --data "$(jq -n --arg c "${SLACK_CHANNEL:?SLACK_CHANNEL unset}" --arg t "$text" '{channel:$c,text:$t}')" \
  2>/dev/null || true)"

if [ "$(printf '%s' "$resp" | jq -r '.ok // false' 2>/dev/null)" != "true" ]; then
  echo "::warning::Slack notify failed: ${resp:-<no response>}"
fi
exit 0
