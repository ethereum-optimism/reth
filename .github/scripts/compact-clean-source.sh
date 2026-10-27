#!/usr/bin/env bash
set -euo pipefail

tracked_paths_file="$(mktemp)"
trap 'rm -f -- "$tracked_paths_file"' EXIT
git ls-files -z > "$tracked_paths_file"

# act overlays checkouts, so remove tracked base files that the candidate may no longer contain.
while IFS= read -r -d '' tracked_path; do
    case "$tracked_path" in
        testdata/micro/compact/*) continue ;;
    esac
    if [[ -f "$tracked_path" || -L "$tracked_path" ]]; then
        rm -f -- "$tracked_path"
    fi
done < "$tracked_paths_file"
