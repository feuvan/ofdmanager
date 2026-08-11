#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
hook="$repo_root/.githooks/pre-commit"

if [[ ! -x "$hook" ]]; then
  printf '%s\n' "pre-commit hook is missing or not executable: $hook" >&2
  exit 1
fi

git -C "$repo_root" config --local core.hooksPath .githooks
printf '%s\n' "Git hooks enabled: .githooks"
