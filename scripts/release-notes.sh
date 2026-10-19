#!/usr/bin/env bash
# Prints the changelog for a tagged release.
#
# GitHub's generated release notes only list merged pull requests, so a direct
# push to main disappears from the release page even though it shipped. This
# script walks every non-merge commit between the previous tag and the release
# tag instead, which is the same history the release binary was built from.
#
#   ./scripts/release-notes.sh v0.14.0
#   ./scripts/release-notes.sh v0.14.0 v0.13.0
#
# The second argument is optional; without it the previous reachable tag is
# used. GITHUB_REPOSITORY (set by Actions) overrides the repository inferred
# from `origin`, and the output is Markdown on stdout.

set -euo pipefail

tag="${1:-}"
if [ -z "$tag" ]; then
    echo "usage: $0 <tag> [previous-tag]" >&2
    exit 1
fi

if ! git rev-parse --verify --quiet "${tag}^{commit}" >/dev/null; then
    echo "error: unknown tag '$tag'" >&2
    exit 1
fi

previous="${2:-}"
if [ -z "$previous" ]; then
    # `tag^` is the tagged commit's first parent, so the previous release is the
    # newest version tag reachable from it.
    previous="$(git describe --tags --abbrev=0 --match 'v[0-9]*' "${tag}^" 2>/dev/null || true)"
fi

range="$tag"
if [ -n "$previous" ]; then
    range="$previous..$tag"
fi

repo="${GITHUB_REPOSITORY:-}"
if [ -z "$repo" ]; then
    remote="$(git remote get-url origin 2>/dev/null || true)"
    case "$remote" in
        git@github.com:*) repo="${remote#git@github.com:}" ;;
        https://github.com/*) repo="${remote#https://github.com/}" ;;
    esac
    repo="${repo%.git}"
fi

url_for_commit() {
    if [ -n "$repo" ]; then
        printf 'https://github.com/%s/commit/%s' "$repo" "$1"
    else
        printf '%s' "$1"
    fi
}

url_for_pull() {
    printf 'https://github.com/%s/pull/%s' "$repo" "$1"
}

echo "## What's Changed"
echo

git log --no-merges --reverse --format='%H%x09%s' "$range" |
    while IFS=$'\t' read -r hash subject; do
        # A squash-merged pull request ends its subject with `(#123)`. Link the
        # pull request when there is one and the commit otherwise, and keep the
        # subject itself free of the duplicate reference.
        pull=""
        if [[ "$subject" =~ ^(.*)[[:space:]]+\(#([0-9]+)\)$ ]]; then
            subject="${BASH_REMATCH[1]}"
            pull="${BASH_REMATCH[2]}"
        fi

        if [ -n "$pull" ] && [ -n "$repo" ]; then
            printf '* %s ([#%s](%s))\n' "$subject" "$pull" "$(url_for_pull "$pull")"
        else
            printf '* %s ([%s](%s))\n' "$subject" "${hash:0:7}" "$(url_for_commit "$hash")"
        fi
    done

if [ -n "$previous" ] && [ -n "$repo" ]; then
    echo
    printf '**Full Changelog**: https://github.com/%s/compare/%s...%s\n' \
        "$repo" "$previous" "$tag"
fi
