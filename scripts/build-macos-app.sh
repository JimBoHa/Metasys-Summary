#!/bin/zsh
set -euo pipefail

script_dir="${0:A:h}"
project_dir="${script_dir:h}"
app_dir="$project_dir/dist/Metasys Dashboard.app"

cd "$project_dir"
cargo build --release
cargo build --release --manifest-path "$project_dir/legacy-sql-helper/Cargo.toml"

mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
install -m 755 "$project_dir/target/release/metasys-dashboard" "$app_dir/Contents/MacOS/metasys-dashboard"
install -m 755 "$project_dir/legacy-sql-helper/target/release/metasys-sql-legacy-helper" "$app_dir/Contents/MacOS/metasys-sql-legacy-helper"
install -m 644 "$project_dir/packaging/Info.plist" "$app_dir/Contents/Info.plist"

codesign --force --deep --sign - "$app_dir" >/dev/null
echo "Built: $app_dir"
echo "Open with: open '$app_dir'"
