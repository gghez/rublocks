#!/usr/bin/env bash
# SessionStart hook: fetch origin and rebase the current branch onto origin/main.
#
# Emits a one-line systemMessage so the result is visible in the Claude session.
# Skips (does NOT fail) when preconditions aren't met (no repo, no origin,
# detached HEAD, dirty tree, mid-rebase). Aborts cleanly on rebase conflicts.

set -u

emit() {
  # $1: short status keyword (ok|skip|fail), $2: human message
  local status="$1" msg="$2"
  printf '%s\n' "$msg" >&2
  jq -n --arg m "rublocks[$status]: $msg" '{systemMessage: $m}'
  exit 0
}

cd "${CLAUDE_PROJECT_DIR:-$PWD}" || emit skip "could not cd to project dir"

if ! git rev-parse --git-dir >/dev/null 2>&1; then
  emit skip "not a git repo"
fi

if ! git remote get-url origin >/dev/null 2>&1; then
  emit skip "no 'origin' remote"
fi

if [ -d "$(git rev-parse --git-dir)/rebase-merge" ] \
  || [ -d "$(git rev-parse --git-dir)/rebase-apply" ]; then
  emit skip "rebase already in progress, leaving it alone"
fi

branch=$(git symbolic-ref --short HEAD 2>/dev/null || true)
if [ -z "$branch" ]; then
  emit skip "detached HEAD"
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  emit skip "working tree dirty on $branch (commit or stash first)"
fi

if ! fetch_err=$(git fetch origin 2>&1); then
  emit fail "git fetch origin failed: $fetch_err"
fi

before=$(git rev-parse HEAD)
if ! rebase_out=$(git rebase origin/main 2>&1); then
  git rebase --abort >/dev/null 2>&1 || true
  first_line=$(printf '%s\n' "$rebase_out" | head -n1)
  emit fail "rebase $branch onto origin/main failed (aborted): $first_line"
fi
after=$(git rev-parse HEAD)

if [ "$before" = "$after" ]; then
  emit ok "$branch already up to date with origin/main"
else
  emit ok "$branch rebased onto origin/main ($(git rev-parse --short "$before") -> $(git rev-parse --short "$after"))"
fi
