#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${BASE_REF:-}" ]]; then
    printf '%s\n' "$BASE_REF"
    exit 0
fi

case "${REF:?missing workflow ref}" in
    refs/heads/op-v*)
        printf '%s\n' "${REF#refs/heads/op-}"
        ;;
    refs/heads/op-*)
        echo "no compact base for ${REF}: only op-v<version> branches map to an upstream tag" >&2
        exit 1
        ;;
    *)
        if [[ "${REPOSITORY:?missing workflow repository}" == "ethereum-optimism/reth" ]]; then
            release_version="$(awk -F '"' '/^\[workspace.package\]/{package_section=1;next} package_section && /^\[/{exit} package_section && /^version *=/{print $2;exit}' Cargo.toml)"
            if [[ ! "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                echo "cannot select a compact release base for workspace version ${release_version}" >&2
                exit 1
            fi
            printf 'v%s\n' "$release_version"
        else
            printf 'main\n'
        fi
        ;;
esac
