#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_dir="$(cd "$script_dir/.." && pwd)"
cd "$repository_dir"

scan_pattern() {
  local description="$1"
  local pattern="$2"
  local allowed_pattern="${3:-}"
  local findings
  local grep_status
  set +e
  findings="$(git grep --untracked --exclude-standard -I -n -E -e "$pattern" -- . ':(exclude)scripts/check-repository-hygiene.sh')"
  grep_status=$?
  set -e
  if [ "$grep_status" -gt 1 ]; then
    printf 'Repository hygiene failed while scanning for %s.\n' "$description" >&2
    exit "$grep_status"
  fi
  if [ -n "$allowed_pattern" ]; then
    findings="$(printf '%s\n' "$findings" | grep -v -E "$allowed_pattern" || true)"
  fi
  if [ -n "$findings" ]; then
    printf '%s\n' "$findings" >&2
    printf 'Repository hygiene failed: found %s.\n' "$description" >&2
    exit 1
  fi
}

scan_pattern "a likely GitHub access token" '(gh[pousr]|github_pat)_[A-Za-z0-9_]{20,}'
scan_pattern "a likely AWS access key" 'AKIA[0-9A-Z]{16}'
# This exact example.test value is a unit-test case proving config validation
# rejects embedded URL credentials. No other URL credential is allowed.
scan_pattern "an embedded URL credential" 'https?://[^[:space:]/:@]+:[^[:space:]@/]+@' 'server_url: "https://name:secret@metasys\.example\.test"\.to_owned\(\),$'
scan_pattern "private-key material" '-----BEGIN ([A-Z0-9]+[[:space:]]+)*PRIVATE KEY-----'
scan_pattern "a private-network IPv4 address" '(^|[^0-9])(10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3})([^0-9]|$)'

if git ls-files | grep -E '(^|/)(target|dist)/|(^|/)config\.toml$|\.sqlite3(-wal|-shm)?$|\.DS_Store$|\.(pem|p12|pfx)$'; then
  printf 'Repository hygiene failed: generated data, local configuration, or credential files are tracked.\n' >&2
  exit 1
fi

printf 'Repository hygiene checks passed.\n'
