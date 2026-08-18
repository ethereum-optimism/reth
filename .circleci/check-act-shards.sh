#!/usr/bin/env bash
set -euo pipefail

readonly config_file=".circleci/config.yml"
readonly lint_workflow=".github/workflows/lint.yml"
readonly unit_workflow=".github/workflows/unit.yml"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

configured_targets() {
  local circle_workflow="$1" github_workflow="$2"
  yq -r "
    .workflows.${circle_workflow}.jobs[] |
    select(has(\"act\")) | .act |
    select(.wf == \"${github_workflow}\") |
    .matrix.parameters.job[]
  " "$config_file"
}

assert_unique() {
  local targets_file="$1" label="$2"
  if [ "$(wc -l < "$targets_file")" -ne "$(sort -u "$targets_file" | wc -l)" ]; then
    echo "Duplicate CircleCI ${label} targets:" >&2
    sort "$targets_file" | uniq -d >&2
    exit 1
  fi
}

configured_targets reth-act lint.yml > "$tmp_dir/lint-targets"
assert_unique "$tmp_dir/lint-targets" lint

# crate-checks is the only matrix split into separate CircleCI targets.
sed -E 's/^crate-checks-[0-9]+$/crate-checks/' "$tmp_dir/lint-targets" |
  sort -u > "$tmp_dir/lint-jobs"
yq -r '.jobs | keys | .[] | select(. != "lint-success")' "$lint_workflow" |
  sort -u > "$tmp_dir/lint-workflow-jobs"
if ! diff -u "$tmp_dir/lint-workflow-jobs" "$tmp_dir/lint-jobs"; then
  echo "CircleCI lint targets do not match $lint_workflow" >&2
  exit 1
fi

awk -F- '/^crate-checks-[0-9]+$/ { print $NF }' "$tmp_dir/lint-targets" |
  sort -n > "$tmp_dir/configured-partitions"
yq -r '.jobs.crate-checks.strategy.matrix.partition[]' "$lint_workflow" |
  sort -n > "$tmp_dir/workflow-partitions"
yq -r '.jobs.crate-checks.strategy.matrix.total_partitions[]' "$lint_workflow" \
  > "$tmp_dir/total-partitions"
if [ "$(wc -l < "$tmp_dir/total-partitions")" -ne 1 ] ||
  ! grep -Eq '^[1-9][0-9]*$' "$tmp_dir/total-partitions"; then
  echo "crate-checks must define exactly one positive total_partitions value" >&2
  exit 1
fi
seq 1 "$(cat "$tmp_dir/total-partitions")" > "$tmp_dir/expected-partitions"
if ! diff -u "$tmp_dir/expected-partitions" "$tmp_dir/workflow-partitions"; then
  echo "crate-checks partitions do not cover 1..total_partitions in $lint_workflow" >&2
  exit 1
fi
if ! diff -u "$tmp_dir/workflow-partitions" "$tmp_dir/configured-partitions"; then
  echo "CircleCI crate-checks shards do not match $lint_workflow" >&2
  exit 1
fi

configured_targets reth-act unit.yml > "$tmp_dir/unit-targets"
assert_unique "$tmp_dir/unit-targets" unit
yq -r '.jobs | keys | .[] | select(. != "unit-success")' "$unit_workflow" |
  sort -u > "$tmp_dir/unit-workflow-jobs"
sort -u "$tmp_dir/unit-targets" > "$tmp_dir/unit-jobs"
if ! diff -u "$tmp_dir/unit-workflow-jobs" "$tmp_dir/unit-jobs"; then
  echo "CircleCI unit targets do not match $unit_workflow" >&2
  exit 1
fi

echo "CircleCI shards match $lint_workflow and $unit_workflow"
