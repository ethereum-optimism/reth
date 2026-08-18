#!/usr/bin/env bash
set -euo pipefail

readonly config_file=".circleci/config.yml"
readonly workflow_file=".github/workflows/lint.yml"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

yq -r '
  .workflows.reth-act-pull-request.jobs[] |
  select(has("act")) | .act |
  select(.wf == "lint.yml") |
  .matrix.parameters.job[]
' "$config_file" > "$tmp_dir/configured-targets"

if [ "$(wc -l < "$tmp_dir/configured-targets")" -ne "$(sort -u "$tmp_dir/configured-targets" | wc -l)" ]; then
  echo "Duplicate CircleCI lint targets:" >&2
  sort "$tmp_dir/configured-targets" | uniq -d >&2
  exit 1
fi

# crate-checks is the only matrix split into separate CircleCI targets.
sed -E 's/^crate-checks-[0-9]+$/crate-checks/' "$tmp_dir/configured-targets" |
  sort -u > "$tmp_dir/configured-jobs"
yq -r '.jobs | keys | .[] | select(. != "lint-success")' "$workflow_file" |
  sort -u > "$tmp_dir/workflow-jobs"

if ! diff -u "$tmp_dir/workflow-jobs" "$tmp_dir/configured-jobs"; then
  echo "CircleCI lint targets do not match $workflow_file" >&2
  exit 1
fi

awk -F- '/^crate-checks-[0-9]+$/ { print $NF }' "$tmp_dir/configured-targets" |
  sort -n > "$tmp_dir/configured-partitions"
yq -r '.jobs.crate-checks.strategy.matrix.partition[]' "$workflow_file" |
  sort -n > "$tmp_dir/workflow-partitions"

if ! diff -u "$tmp_dir/workflow-partitions" "$tmp_dir/configured-partitions"; then
  echo "CircleCI crate-checks shards do not match $workflow_file" >&2
  exit 1
fi

echo "CircleCI lint shards match $workflow_file"
