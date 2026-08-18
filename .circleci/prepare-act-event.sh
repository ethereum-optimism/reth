#!/usr/bin/env bash
set -euo pipefail

readonly OUTPUT_DIR="${ACT_EVENT_OUTPUT_DIR:-/tmp/act-event}"
readonly EVENT_PATH="${OUTPUT_DIR}/github-event.json"
readonly EVENT_NAME_PATH="${OUTPUT_DIR}/event-name"
readonly BASE_REF_PATH="${OUTPUT_DIR}/base-ref"
readonly SUPERSEDED_PATH="${OUTPUT_DIR}/superseded"

mkdir -p "$OUTPUT_DIR"
rm -f "$EVENT_PATH" "$EVENT_NAME_PATH" "$BASE_REF_PATH" "$SUPERSEDED_PATH"

: "${CIRCLE_SHA1:?missing CircleCI revision}"
: "${CIRCLE_PROJECT_USERNAME:?missing CircleCI project owner}"
: "${CIRCLE_PROJECT_REPONAME:?missing CircleCI project repository}"

repository="${CIRCLE_PROJECT_USERNAME}/${CIRCLE_PROJECT_REPONAME}"

if [[ "${CIRCLE_BRANCH:-}" == gh-readonly-queue/* ]]; then
    event_name="merge_group"
else
    event_name="pull_request"
fi

case "$event_name" in
    pull_request)
        : "${CIRCLE_PULL_REQUEST:?pull-request trigger must provide CIRCLE_PULL_REQUEST}"
        pr_number="${CIRCLE_PULL_REQUEST##*/}"
        if [[ ! "$pr_number" =~ ^[1-9][0-9]*$ ]]; then
            echo "invalid pull-request URL: ${CIRCLE_PULL_REQUEST}" >&2
            exit 1
        fi

        pull_request_path="${ACT_PR_PAYLOAD_FILE:-${OUTPUT_DIR}/pull-request.json}"
        if [[ -z "${ACT_PR_PAYLOAD_FILE:-}" ]]; then
            api_url="https://api.github.com/repos/${repository}/pulls/${pr_number}"
            curl --fail --silent --show-error --location \
                -H "Accept: application/vnd.github+json" \
                -H "Authorization: Bearer ${MISE_GITHUB_TOKEN:?missing restricted read-only GitHub token}" \
                -H "X-GitHub-Api-Version: 2022-11-28" \
                "$api_url" > "$pull_request_path"
        fi

        jq -e \
            --argjson number "$pr_number" \
            --arg repository "$repository" \
            '.number == $number and .base.repo.full_name == $repository and
             (.head.sha | type == "string") and (.head.ref | type == "string") and
             (.base.ref | type == "string")' \
            "$pull_request_path" > /dev/null

        event_head_sha="$(jq -r '.head.sha' "$pull_request_path")"
        if [[ "$event_head_sha" != "$CIRCLE_SHA1" ]]; then
            printf 'PR head moved from CircleCI revision %s to %s\n' \
                "$CIRCLE_SHA1" "$event_head_sha" > "$SUPERSEDED_PATH"
            echo "$(cat "$SUPERSEDED_PATH"); treating this pipeline as superseded"
            exit 0
        fi

        jq '{
            action: "synchronize",
            number: .number,
            pull_request: .,
            repository: .base.repo,
            sender: .user
        }' "$pull_request_path" > "$EVENT_PATH"
        base_ref="$(jq -er '.pull_request.base.ref | select(length > 0)' "$EVENT_PATH")"
        head_ref="$(jq -er '.pull_request.head.ref | select(length > 0)' "$EVENT_PATH")"
        echo "act event: pull_request #${pr_number}, ${head_ref} -> ${base_ref}"
        ;;
    merge_group)
        : "${CIRCLE_BRANCH:?merge-queue trigger must provide CIRCLE_BRANCH}"
        if [[ ! "$CIRCLE_BRANCH" =~ ^gh-readonly-queue/(.+)/pr-([1-9][0-9]*)-[^/]+$ ]]; then
            echo "merge_group requires a gh-readonly-queue/<base>/pr-<number>-<suffix> branch, got: ${CIRCLE_BRANCH}" >&2
            exit 1
        fi
        base_ref="${BASH_REMATCH[1]}"

        resolved_head_sha="$(git rev-parse "${CIRCLE_SHA1}^{commit}")"
        if [[ "$resolved_head_sha" != "$CIRCLE_SHA1" ]]; then
            echo "CircleCI revision did not resolve exactly: ${CIRCLE_SHA1} -> ${resolved_head_sha}" >&2
            exit 1
        fi
        if ! base_sha="$(git rev-parse "${CIRCLE_SHA1}^1" 2>/dev/null)"; then
            git fetch --no-tags --depth=2 origin "$CIRCLE_SHA1"
            base_sha="$(git rev-parse "${CIRCLE_SHA1}^1")"
        fi

        jq -n \
            --arg head_sha "$CIRCLE_SHA1" \
            --arg head_ref "refs/heads/${CIRCLE_BRANCH}" \
            --arg base_sha "$base_sha" \
            --arg base_ref "refs/heads/${base_ref}" \
            --arg repository "$repository" \
            --arg repository_name "$CIRCLE_PROJECT_REPONAME" \
            --arg repository_owner "$CIRCLE_PROJECT_USERNAME" \
            '{
                action: "checks_requested",
                merge_group: {
                    head_sha: $head_sha,
                    head_ref: $head_ref,
                    base_sha: $base_sha,
                    base_ref: $base_ref
                },
                repository: {
                    full_name: $repository,
                    name: $repository_name,
                    owner: {login: $repository_owner}
                }
            }' > "$EVENT_PATH"
        echo "act event: merge_group ${CIRCLE_BRANCH} -> ${base_ref} at ${base_sha}"
        ;;
esac

printf '%s' "$base_ref" > "$BASE_REF_PATH"
printf '%s' "$event_name" > "$EVENT_NAME_PATH"
