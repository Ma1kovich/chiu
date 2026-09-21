#!/usr/bin/env bash
set -euo pipefail

VERSION=${1:?version is required}
EXPECTED_ENDPOINT='https://ma1kovich.github.io/chiu/latest.json'

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid release version: $VERSION" >&2
  exit 1
fi

endpoint=$(jq -er '.plugins.updater.endpoints | if length == 1 then .[0] else error("expected one updater endpoint") end' src-tauri/tauri.conf.json)
test "$endpoint" = "$EXPECTED_ENDPOINT"
if [[ $# -ge 2 ]]; then
  test "$2" = "$EXPECTED_ENDPOINT"
fi

release_version="${VERSION%%+*}"
if [[ "$release_version" == *-* ]]; then
  stable_release=$(gh release list --exclude-drafts --exclude-pre-releases --limit 1 --json tagName --jq '.[0].tagName // empty')
  if [[ -n "$stable_release" ]]; then
    echo "stable release $stable_release already exists; prereleases are closed" >&2
    exit 1
  fi
fi
