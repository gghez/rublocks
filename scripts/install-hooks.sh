#!/usr/bin/env bash
# Point git at the in-repo .githooks/ directory so pre-commit / pre-push
# hooks run for every contributor without copying files around.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

cd "$repo_root"
git config core.hooksPath .githooks
echo "Installed: core.hooksPath = .githooks"
