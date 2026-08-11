#!/usr/bin/env bash
# Ground-truth gate for the local PR-outcome check.
#
# For a sample of PRs that the forge says are MERGED, ask the daemon's own
# local check what it thinks, and fail on any CONFIDENT WRONG answer.
#
# The distinction this gate exists to enforce:
#
#   Some(true)  on a merged PR  -> correct
#   None        on a merged PR  -> acceptable. The local clone genuinely cannot
#                                  see it (never fetched, merge outside the
#                                  scanned window). "I don't know" is honest.
#   Some(false) on a merged PR  -> FAILURE. The check looked, missed it, and
#                                  reported it as not merged. That is the bug:
#                                  absence of evidence recorded as evidence of
#                                  absence, and it renders in the product as a
#                                  confident "Open" chip on shipped work.
#
# Usage:  verify-pr-outcomes.sh [sample-per-repo]     (default 8)
# Exit 0 only when WRONG=0. Prints a one-line summary the loop can read.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2
SAMPLE="${1:-8}"

# Real checkouts on this machine, resolved to their forge slug. Only repos with
# a github remote can be ground-truthed.
#
# Default to a small fixed set: ~/Documents is iCloud-backed and a cold `git log`
# there has been measured at 150s, so scanning every checkout would make the gate
# too slow to iterate against. Override with REPOS="path1 path2 …".
DEFAULT_REPOS="$HOME/Documents/modelstat $HOME/Documents/vendora/teoripass $HOME/Documents/core"
read -r -a REPOS <<< "${REPOS:-$DEFAULT_REPOS}"

WRONG=0; RIGHT=0; UNKNOWN=0; SKIPPED=0
WRONG_LINES=""

for repo in "${REPOS[@]}"; do
  [ -d "$repo/.git" ] || { SKIPPED=$((SKIPPED+1)); continue; }
  slug=$(git -C "$repo" remote get-url origin 2>/dev/null |
    sed -E 's|.*github\.com[:/]||; s|\.git$||')
  case "$slug" in */*) ;; *) SKIPPED=$((SKIPPED+1)); continue ;; esac

  # Ask the forge for merged PRs — the ground truth.
  prs=$(gh api "repos/$slug/pulls?state=closed&per_page=$SAMPLE&sort=updated&direction=desc" \
        --jq '.[] | select(.merged_at != null) | .number' 2>/dev/null | head -"$SAMPLE")
  [ -z "$prs" ] && { SKIPPED=$((SKIPPED+1)); continue; }

  args=(); for n in $prs; do args+=("$repo" "$n"); done
  out=$(cargo run -q -p modelstat-parsers --example pr_outcome_probe -- "${args[@]}" 2>/dev/null) || continue

  while IFS= read -r line; do
    [ -z "$line" ] && continue
    pr=$(printf '%s' "$line" | sed -E 's/.*"pr":([0-9]+).*/\1/')
    verdict=$(printf '%s' "$line" | sed -E 's/.*"verdict":"([^"]*)".*/\1/')
    case "$verdict" in
      "Some(true)"|"true")   RIGHT=$((RIGHT+1)) ;;
      "None"|"unreadable")   UNKNOWN=$((UNKNOWN+1)) ;;
      *)                     WRONG=$((WRONG+1))
                             WRONG_LINES="$WRONG_LINES\n  $slug#$pr -> $verdict (forge says MERGED)" ;;
    esac
  done <<< "$out"
done

echo "SUMMARY right=$RIGHT unknown=$UNKNOWN wrong=$WRONG skipped_repos=$SKIPPED"
if [ "$WRONG" -gt 0 ]; then
  # shellcheck disable=SC2059
  printf "CONFIDENT WRONG ANSWERS on merged PRs:$WRONG_LINES\n"
  echo "FAIL: a merged PR must never read as not-merged. Unknown is allowed; wrong is not."
  exit 1
fi
if [ "$RIGHT" -eq 0 ]; then
  echo "FAIL: nothing was verified — the probe answered for no merged PR at all."
  exit 1
fi
echo "PASS: no merged PR is reported as not-merged."
