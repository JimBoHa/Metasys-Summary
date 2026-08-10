#!/bin/bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repository_dir="$(cd "$script_dir/.." && pwd)"
cd "$repository_dir"

run_step() {
  local name="$1"
  shift
  printf '[verify] %s\n' "$name"
  "$@"
}

run_step "repository hygiene" "$script_dir/check-repository-hygiene.sh"
run_step "browser static contracts" node "$script_dir/check-static-contracts.mjs"

for javascript_file in static/*.js scripts/*.mjs; do
  run_step "JavaScript syntax: $javascript_file" node --check "$javascript_file"
done

run_step "main rustfmt" cargo fmt --all -- --check
run_step "legacy helper rustfmt" cargo fmt --manifest-path legacy-sql-helper/Cargo.toml --all -- --check
run_step "main tests" cargo test --locked --all-targets --all-features
run_step "legacy helper tests" cargo test --manifest-path legacy-sql-helper/Cargo.toml --locked --all-targets --all-features
run_step "main Clippy" cargo clippy --locked --all-targets --all-features -- -D warnings
run_step "legacy helper Clippy" cargo clippy --manifest-path legacy-sql-helper/Cargo.toml --locked --all-targets --all-features -- -D warnings
run_step "debug service build" cargo build --locked --bin metasys-dashboard
run_step "disposable HTTP smoke test" "$script_dir/smoke-test.sh"

printf '[verify] all checks passed\n'
