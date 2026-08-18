#!/usr/bin/env bash
set -euo pipefail

readonly config_file=".circleci/config.yml"
readonly workflow_file=".github/workflows/lint.yml"
readonly lint_jobs_anchor="lint_jobs"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

# CircleCI matrices are expanded before checkout, so the shard list remains static. Compare
# it with act's view of the workflow to prevent new or renamed lint jobs from being skipped.
awk -v anchor="$lint_jobs_anchor" '
  $0 ~ "^[[:space:]]+job: &" anchor "[[:space:]]*$" { in_list = 1; next }
  in_list && $0 ~ /^[[:space:]]+-[[:space:]]+/ {
    sub(/^[[:space:]]+-[[:space:]]+/, "")
    print
    next
  }
  in_list { exit }
' "$config_file" > "$tmp_dir/configured-targets"

if [ ! -s "$tmp_dir/configured-targets" ]; then
  echo "Unable to read lint targets from &$lint_jobs_anchor in $config_file" >&2
  exit 1
fi

if [ "$(wc -l < "$tmp_dir/configured-targets")" -ne "$(sort -u "$tmp_dir/configured-targets" | wc -l)" ]; then
  echo "Duplicate lint targets found in &$lint_jobs_anchor:" >&2
  sort "$tmp_dir/configured-targets" | uniq -d >&2
  exit 1
fi

# The three crate-checks matrix entries are split into individual CircleCI targets. Collapse
# them back to their GitHub job ID before comparing the workflow's job list.
sed -E 's/^crate-checks-[0-9]+$/crate-checks/' "$tmp_dir/configured-targets" |
  sort -u > "$tmp_dir/configured-jobs"

if ! act pull_request -W "$workflow_file" --list \
  -P "ubuntu-latest=catthehacker/ubuntu@sha256:b839c14c4410998529ec18f951262bdf87a2b23bc1467304d07b491b9455e074" \
  > "$tmp_dir/act-list" 2> "$tmp_dir/act-list-errors"; then
  cat "$tmp_dir/act-list-errors" >&2
  exit 1
fi
awk '$1 ~ /^[0-9]+$/ && $2 != "lint-success" { print $2 }' "$tmp_dir/act-list" |
  sort -u > "$tmp_dir/workflow-jobs"

if [ ! -s "$tmp_dir/workflow-jobs" ]; then
  echo "act did not report any lint jobs:" >&2
  cat "$tmp_dir/act-list-errors" >&2
  exit 1
fi

if ! diff -u "$tmp_dir/workflow-jobs" "$tmp_dir/configured-jobs"; then
  echo "CircleCI lint targets do not match $workflow_file" >&2
  exit 1
fi

awk -F- '/^crate-checks-[0-9]+$/ { print $NF }' "$tmp_dir/configured-targets" |
  sort -n > "$tmp_dir/configured-partitions"
awk '
  /^  crate-checks:$/ { in_job = 1; next }
  in_job && /^  [A-Za-z0-9_-]+:$/ { exit }
  in_job && /^[[:space:]]+partition:[[:space:]]*\[/ {
    sub(/^[^[]*\[/, "")
    sub(/\].*$/, "")
    gsub(/,/, " ")
    count = split($0, values, /[[:space:]]+/)
    for (i = 1; i <= count; i++) {
      if (values[i] != "") print values[i]
    }
    exit
  }
' "$workflow_file" | sort -n > "$tmp_dir/workflow-partitions"

if [ ! -s "$tmp_dir/workflow-partitions" ]; then
  echo "Unable to read crate-checks partitions from $workflow_file" >&2
  exit 1
fi

if ! diff -u "$tmp_dir/workflow-partitions" "$tmp_dir/configured-partitions"; then
  echo "CircleCI crate-checks shards do not match $workflow_file" >&2
  exit 1
fi

echo "CircleCI lint shards match $workflow_file"
