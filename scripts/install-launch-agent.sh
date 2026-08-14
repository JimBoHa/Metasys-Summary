#!/bin/zsh
set -euo pipefail

script_dir="${0:A:h}"
project_dir="${script_dir:h}"
program_path="$project_dir/target/release/metasys-dashboard"
legacy_helper_build="$project_dir/legacy-sql-helper/target/release/metasys-sql-legacy-helper"
legacy_helper_path="$project_dir/target/release/metasys-sql-legacy-helper"
template_path="$project_dir/packaging/io.github.metasys-summary.dashboard.plist"
agent_path="$HOME/Library/LaunchAgents/io.github.metasys-summary.dashboard.plist"
log_dir="$HOME/Library/Logs/Metasys Dashboard"
generated_plist=$(mktemp /tmp/metasys-dashboard-launch-agent.XXXXXX)

cd "$project_dir"
cargo build --release
cargo build --release --manifest-path "$project_dir/legacy-sql-helper/Cargo.toml"
install -m 755 "$legacy_helper_build" "$legacy_helper_path"
mkdir -p "${agent_path:h}" "$log_dir"

sed \
  -e "s|__PROGRAM__|$program_path|g" \
  -e "s|__WORKING_DIRECTORY__|$project_dir|g" \
  -e "s|__LOG_DIRECTORY__|$log_dir|g" \
  "$template_path" > "$generated_plist"
plutil -lint "$generated_plist"
install -m 644 "$generated_plist" "$agent_path"

launchctl bootout "gui/$(id -u)" "$agent_path" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$agent_path"
launchctl enable "gui/$(id -u)/io.github.metasys-summary.dashboard"

echo "Installed LaunchAgent: $agent_path"
echo "Dashboard: http://127.0.0.1:3030"
