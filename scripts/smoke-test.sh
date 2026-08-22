#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_dir="$(cd "$script_dir/.." && pwd)"
binary_path="$repository_dir/target/debug/metasys-dashboard"
fixture_path="$repository_dir/tests/fixtures/portal-bootstrap.json"
smoke_port="${METASYS_TEST_PORT:-38083}"
smoke_directory="$(mktemp -d "${TMPDIR:-/tmp}/metasys-smoke.XXXXXX")"
smoke_pid=""
base_url="http://127.0.0.1:$smoke_port"

cleanup() {
  if [ -n "$smoke_pid" ] && kill -0 "$smoke_pid" 2>/dev/null; then
    kill "$smoke_pid" 2>/dev/null || true
    wait "$smoke_pid" 2>/dev/null || true
  fi
  case "$smoke_directory" in
    */metasys-smoke.*) rm -rf "$smoke_directory" ;;
    *) printf 'Refusing to remove unexpected smoke directory: %s\n' "$smoke_directory" >&2 ;;
  esac
}
trap cleanup EXIT

expect_status() {
  local expected="$1"
  local output_file="$2"
  shift 2
  local actual
  actual="$(curl --silent --show-error --max-time 8 --output "$output_file" --write-out '%{http_code}' "$@")"
  if [ "$actual" != "$expected" ]; then
    printf 'Expected HTTP %s, received %s for curl arguments: %s\n' "$expected" "$actual" "$*" >&2
    sed -n '1,40p' "$output_file" >&2 || true
    exit 1
  fi
}

assert_contains() {
  local file="$1"
  local text="$2"
  if ! grep -Fq "$text" "$file"; then
    printf 'Expected %s to contain: %s\n' "$file" "$text" >&2
    exit 1
  fi
}

if [ ! -x "$binary_path" ]; then
  printf 'Smoke-test binary is missing: %s\n' "$binary_path" >&2
  printf 'Run cargo build --bin metasys-dashboard first.\n' >&2
  exit 1
fi

METASYS_CONFIG="$smoke_directory/missing-config.toml" \
METASYS_DATABASE_PATH="$smoke_directory/dashboard.sqlite3" \
METASYS_HISTORY_DATABASE_PATH="$smoke_directory/history.duckdb" \
METASYS_BIND_ADDRESS="127.0.0.1" \
METASYS_PORT="$smoke_port" \
METASYS_OPEN_BROWSER="false" \
RUST_LOG="metasys_dashboard=info" \
"$binary_path" --demo >"$smoke_directory/service.log" 2>&1 &
smoke_pid="$!"

service_ready="false"
for _ in {1..60}; do
  if curl --silent --fail --max-time 1 "$base_url/api/portal/status" >/dev/null 2>&1; then
    service_ready="true"
    break
  fi
  if ! kill -0 "$smoke_pid" 2>/dev/null; then
    printf 'Disposable smoke service exited during startup.\n' >&2
    sed -n '1,120p' "$smoke_directory/service.log" >&2
    exit 1
  fi
  sleep 0.1
done
if [ "$service_ready" != "true" ]; then
  printf 'Disposable smoke service did not become ready.\n' >&2
  sed -n '1,120p' "$smoke_directory/service.log" >&2
  exit 1
fi

expect_status 200 "$smoke_directory/status.json" "$base_url/api/portal/status"
assert_contains "$smoke_directory/status.json" '"initialized":false'
assert_contains "$smoke_directory/status.json" '"bootstrapAllowed":true'

expect_status 200 "$smoke_directory/root.html" --dump-header "$smoke_directory/root.headers" "$base_url/"
assert_contains "$smoke_directory/root.html" "Building Maintenance Portal"
assert_contains "$smoke_directory/root.headers" "content-security-policy:"
assert_contains "$smoke_directory/root.headers" "x-frame-options: DENY"

expect_status 200 "$smoke_directory/navigation.js" "$base_url/navigation.js"
expect_status 200 "$smoke_directory/navigation.css" "$base_url/navigation.css"
expect_status 200 "$smoke_directory/diagnostics.js" "$base_url/diagnostics.js"
expect_status 200 "$smoke_directory/diagnostics.css" "$base_url/diagnostics.css"
expect_status 401 "$smoke_directory/unauthorized.json" "$base_url/operations"

expect_status 200 "$smoke_directory/session.json" \
  --dump-header "$smoke_directory/session.headers" \
  --cookie-jar "$smoke_directory/cookies.txt" \
  --request POST \
  --header "Content-Type: application/json" \
  --header "Sec-Fetch-Site: same-origin" \
  --data-binary "@$fixture_path" \
  "$base_url/api/portal/bootstrap"
assert_contains "$smoke_directory/session.headers" "HttpOnly"
assert_contains "$smoke_directory/session.headers" "SameSite=Strict"
csrf_token="$(node -e 'const fs=require("fs"); const body=JSON.parse(fs.readFileSync(process.argv[1], "utf8")); if (!body.csrfToken) process.exit(1); process.stdout.write(body.csrfToken);' "$smoke_directory/session.json")"

expect_status 200 "$smoke_directory/operations.html" --cookie "$smoke_directory/cookies.txt" "$base_url/operations"
assert_contains "$smoke_directory/operations.html" "Operations Dashboard"
assert_contains "$smoke_directory/operations.html" "primary-sidebar"
assert_contains "$smoke_directory/operations.html" "SQL history mirror"
expect_status 200 "$smoke_directory/trends.html" --cookie "$smoke_directory/cookies.txt" "$base_url/trends"
assert_contains "$smoke_directory/trends.html" "Metasys Trend Analysis"
expect_status 200 "$smoke_directory/diagnostics.html" --cookie "$smoke_directory/cookies.txt" "$base_url/diagnostics"
assert_contains "$smoke_directory/diagnostics.html" "System Diagnostics"
expect_status 200 "$smoke_directory/dashboard.json" --cookie "$smoke_directory/cookies.txt" "$base_url/api/dashboard"
expect_status 200 "$smoke_directory/diagnostics.json" --cookie "$smoke_directory/cookies.txt" "$base_url/api/diagnostics"
assert_contains "$smoke_directory/diagnostics.json" '"summary"'
expect_status 200 "$smoke_directory/sql-mirror-settings.json" --cookie "$smoke_directory/cookies.txt" "$base_url/api/settings/sql-mirror"
assert_contains "$smoke_directory/sql-mirror-settings.json" '"intervalHours":1'
assert_contains "$smoke_directory/sql-mirror-settings.json" '"recentRuns":[]'
expect_status 403 "$smoke_directory/missing-csrf.json" --cookie "$smoke_directory/cookies.txt" --request POST --header "Sec-Fetch-Site: same-origin" "$base_url/api/refresh"
expect_status 202 "$smoke_directory/refresh.json" --cookie "$smoke_directory/cookies.txt" --request POST --header "Sec-Fetch-Site: same-origin" --header "X-CSRF-Token: $csrf_token" "$base_url/api/refresh"
expect_status 200 "$smoke_directory/logout.json" --cookie "$smoke_directory/cookies.txt" --request POST --header "Sec-Fetch-Site: same-origin" --header "X-CSRF-Token: $csrf_token" "$base_url/api/portal/logout"
expect_status 401 "$smoke_directory/logged-out.json" --cookie "$smoke_directory/cookies.txt" "$base_url/operations"

printf 'Disposable authenticated HTTP smoke test passed on loopback port %s.\n' "$smoke_port"
