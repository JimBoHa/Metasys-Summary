#!/bin/zsh
set -euo pipefail

script_dir="${0:A:h}"
project_dir="${script_dir:h}"
built_app="$project_dir/dist/Metasys History Mirror.app"
application_dir="$HOME/Applications"
installed_app="$application_dir/Metasys History Mirror.app"
template_path="$project_dir/packaging/io.github.metasys-summary.sql-history-mirror.plist"
agent_path="$HOME/Library/LaunchAgents/io.github.metasys-summary.sql-history-mirror.plist"
log_dir="$HOME/Library/Logs/Metasys Dashboard"
generated_plist="$(mktemp /tmp/metasys-history-mirror-launch-agent.XXXXXX)"
trap '/bin/rm -f "$generated_plist"' EXIT

"$project_dir/scripts/build-history-mirror-app.sh"
mkdir -p "$application_dir" "${agent_path:h}" "$log_dir"
/usr/bin/ditto "$built_app" "$installed_app"

sed \
  -e "s|__PROGRAM__|$installed_app/Contents/MacOS/run-sql-history-mirror|g" \
  -e "s|__WORKING_DIRECTORY__|$installed_app/Contents/MacOS|g" \
  -e "s|__LOG_DIRECTORY__|$log_dir|g" \
  "$template_path" > "$generated_plist"
plutil -lint "$generated_plist"
install -m 644 "$generated_plist" "$agent_path"

launchctl bootout "gui/$(id -u)" "$agent_path" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$agent_path"
launchctl enable "gui/$(id -u)/io.github.metasys-summary.sql-history-mirror"

echo "Installed SQL history mirror LaunchAgent: $agent_path"
echo "Configure and inspect health at http://127.0.0.1:3030/operations#mirror"
