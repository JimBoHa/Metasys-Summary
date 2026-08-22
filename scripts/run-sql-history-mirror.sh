#!/bin/zsh
set -euo pipefail

umask 077
mirror_binary="${0:A:h}/metasys-history-mirror"
if [[ ! -x "$mirror_binary" ]]; then
  print -u2 "SQL historian mirror binary is missing: $mirror_binary"
  exit 1
fi

if (( $# == 0 )); then
  set -- run-scheduled-sql-mirror
fi

continuous=false
if [[ "${1:-}" == "--continuous" ]]; then
  continuous=true
  shift
fi
interval_seconds="${METASYS_SQL_MIRROR_INTERVAL_SECONDS:-3600}"
if [[ ! "$interval_seconds" =~ '^[0-9]+$' ]] || (( interval_seconds < 60 )); then
  print -u2 "METASYS_SQL_MIRROR_INTERVAL_SECONDS must be at least 60"
  exit 1
fi

run_mirror_once() {
  "$mirror_binary" "$@"
}

if [[ "$continuous" == false ]]; then
  run_mirror_once "$@"
  exit $?
fi

while true; do
  if ! run_mirror_once "$@"; then
    print -u2 "SQL historian mirror cycle failed; retrying in $interval_seconds seconds"
  fi
  /bin/sleep "$interval_seconds"
done
