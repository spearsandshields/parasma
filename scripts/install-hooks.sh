#!/usr/bin/env bash
# Install the repo's git hooks. Run once per clone.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
install -m 0755 "$ROOT/scripts/hooks/pre-push" "$ROOT/.git/hooks/pre-push"
echo "installed pre-push -> scripts/ci-local.sh"
