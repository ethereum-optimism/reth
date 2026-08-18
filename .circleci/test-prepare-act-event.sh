#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly PREPARE_SCRIPT="${SCRIPT_DIR}/prepare-act-event.sh"
readonly FIXTURE_DIR="${SCRIPT_DIR}/fixtures"
TMP_DIR="$(mktemp -d)"
readonly TMP_DIR
trap 'rm -rf "$TMP_DIR"' EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_file_contains() {
    local path="$1"
    local expected="$2"
    grep -Fq "$expected" "$path" || fail "${path} does not contain: ${expected}"
}

run_pull_request_test() {
    local output_dir="${TMP_DIR}/pull-request"
    ACT_EVENT_OUTPUT_DIR="$output_dir" \
    ACT_PR_PAYLOAD_FILE="${FIXTURE_DIR}/pull-request-api.json" \
    CIRCLE_SHA1="1111111111111111111111111111111111111111" \
    CIRCLE_BRANCH="feature/event-snapshot" \
    CIRCLE_PULL_REQUEST="https://github.com/ethereum-optimism/reth/pull/42" \
    CIRCLE_PROJECT_USERNAME="ethereum-optimism" \
    CIRCLE_PROJECT_REPONAME="reth" \
        "$PREPARE_SCRIPT"

    [[ "$(<"${output_dir}/event-name")" == "pull_request" ]] || fail "wrong PR event name"
    [[ "$(<"${output_dir}/base-ref")" == "optimism" ]] || fail "wrong PR base ref"
    jq -e \
        --slurpfile pull_request "${FIXTURE_DIR}/pull-request-api.json" \
        '.action == "synchronize" and .number == 42 and
         .pull_request == $pull_request[0] and
         .repository == $pull_request[0].base.repo and
         .sender == $pull_request[0].user' \
        "${output_dir}/github-event.json" > /dev/null || fail "generated PR event does not match fixture"
}

run_superseded_pull_request_test() {
    local output_dir="${TMP_DIR}/superseded"
    ACT_EVENT_OUTPUT_DIR="$output_dir" \
    ACT_PR_PAYLOAD_FILE="${FIXTURE_DIR}/pull-request-api.json" \
    CIRCLE_SHA1="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" \
    CIRCLE_BRANCH="feature/event-snapshot" \
    CIRCLE_PULL_REQUEST="https://github.com/ethereum-optimism/reth/pull/42" \
    CIRCLE_PROJECT_USERNAME="ethereum-optimism" \
    CIRCLE_PROJECT_REPONAME="reth" \
        "$PREPARE_SCRIPT"

    [[ -f "${output_dir}/superseded" ]] || fail "moved PR was not marked superseded"
    [[ ! -e "${output_dir}/github-event.json" ]] || fail "superseded PR produced an event"
    assert_file_contains "${output_dir}/superseded" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    assert_file_contains "${output_dir}/superseded" "1111111111111111111111111111111111111111"
}

run_merge_group_test() {
    local output_dir="${TMP_DIR}/merge-group"
    local bin_dir="${TMP_DIR}/bin"
    mkdir -p "$bin_dir"
    cat > "${bin_dir}/git" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == "rev-parse" ]] || exit 2
case "$2" in
    '3333333333333333333333333333333333333333^{commit}')
        echo '3333333333333333333333333333333333333333'
        ;;
    '3333333333333333333333333333333333333333^1')
        echo '2222222222222222222222222222222222222222'
        ;;
    *)
        exit 2
        ;;
esac
EOF
    chmod +x "${bin_dir}/git"

    PATH="${bin_dir}:${PATH}" \
    ACT_EVENT_OUTPUT_DIR="$output_dir" \
    CIRCLE_SHA1="3333333333333333333333333333333333333333" \
    CIRCLE_BRANCH="gh-readonly-queue/release/v1/pr-42-deadbeef" \
    CIRCLE_PROJECT_USERNAME="ethereum-optimism" \
    CIRCLE_PROJECT_REPONAME="reth" \
        "$PREPARE_SCRIPT"

    [[ "$(<"${output_dir}/event-name")" == "merge_group" ]] || fail "wrong merge-group event name"
    [[ "$(<"${output_dir}/base-ref")" == "release/v1" ]] || fail "queue branch with slash parsed incorrectly"
    jq -S . "${output_dir}/github-event.json" > "${TMP_DIR}/actual-merge-group.json"
    jq -S . "${FIXTURE_DIR}/merge-group-event.json" > "${TMP_DIR}/expected-merge-group.json"
    diff -u "${TMP_DIR}/expected-merge-group.json" "${TMP_DIR}/actual-merge-group.json" || \
        fail "generated merge-group event does not match fixture"
}

run_invalid_merge_group_branch_test() {
    local output_dir="${TMP_DIR}/invalid-merge-group"
    if ACT_EVENT_OUTPUT_DIR="$output_dir" \
        CIRCLE_SHA1="3333333333333333333333333333333333333333" \
        CIRCLE_BRANCH="gh-readonly-queue/release/v1/not-a-pr" \
        CIRCLE_PROJECT_USERNAME="ethereum-optimism" \
        CIRCLE_PROJECT_REPONAME="reth" \
            "$PREPARE_SCRIPT" > "${TMP_DIR}/invalid.log" 2>&1; then
        fail "invalid merge-queue branch was accepted"
    fi
    assert_file_contains "${TMP_DIR}/invalid.log" "gh-readonly-queue/<base>/pr-<number>-<suffix>"
}

run_pull_request_test
run_superseded_pull_request_test
run_merge_group_test
run_invalid_merge_group_branch_test

echo "act event preparation tests passed"
