#!/usr/bin/env bash
set -euo pipefail

# CircleCI supplies runner protection, but act cannot mint GitHub OIDC tokens.
# Opt out of secure-runner using its documented input only in these local copies.
# Keep all other action code (including scanners and installers) byte-identical.
readonly OUTPUT_DIR="${ACT_EVENT_OUTPUT_DIR:-/tmp/act-event}/actions"
readonly ACTION_REPOSITORY="https://github.com/tempoxyz/gh-actions.git"
readonly ACTION_REFS=(
    ee2960a6cc25d99bda45614135bfe1197d879bac
    9714ff353c4199ff9f498205ef7b3319869dd51d
    d0bd3f715fb4efadb78897db98fe87e7c94a6d95
)

scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT

git init --quiet "$scratch"
for attempt in 1 2 3; do
    if git -C "$scratch" -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=30 \
        fetch --quiet --no-tags --depth=1 "$ACTION_REPOSITORY" "${ACTION_REFS[@]}"; then
        break
    fi
    if [ "$attempt" -eq 3 ]; then
        echo "Could not fetch pinned action commits" >&2
        exit 1
    fi
    sleep "$((attempt * 5))"
done
for ref in "${ACTION_REFS[@]}"; do
    test "$(git -C "$scratch" rev-parse "${ref}^{commit}")" = "$ref"
    target="${OUTPUT_DIR}/${ref}"
    mkdir -p "$target"
    git -C "$scratch" archive "$ref" | tar -x -C "$target"
    python3 - "$target/actions/secure-runner/action.yml" <<'PY'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
original = path.read_text()
pattern = r'(?m)(^  disable-enforcement:\n(?:    [^\n]*\n)*?    default: )"false"(?=\n)'
updated, count = re.subn(pattern, r'\1"true"', original)
if count != 1:
    raise SystemExit("Expected exactly one secure-runner disable-enforcement default")
path.write_text(updated)
PY
    echo "act-only secure-runner opt-out prepared at ${ref}; CircleCI runner protection applies"
done
