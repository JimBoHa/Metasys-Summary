# SQLite history migration

The `migrate-history` command copies historical alarms and poll outcomes from an older Metasys Summary SQLite database into the DuckDB analytical store. It does not replace SQLite: portal accounts, settings, floor plans, service requests, and other transactional application state remain in SQLite.

Stop the dashboard service before migrating a production database so the source is a stable snapshot and the DuckDB target has a single writer. Always validate first:

```bash
metasys-dashboard migrate-history \
  --source "/path/to/dashboard.sqlite3" \
  --target "/path/to/history.duckdb" \
  --dry-run
```

The dry run opens SQLite with read-only and query-only enforcement, runs SQLite's quick integrity check, validates required columns and timestamps, and reports row counts. It does not create or change the DuckDB target.

Run the import after the dry run succeeds:

```bash
metasys-dashboard migrate-history \
  --source "/path/to/dashboard.sqlite3" \
  --target "/path/to/history.duckdb"
```

When paths are omitted, the command uses `database_path` and `history_database_path` from configuration. The import:

- reads the source SQLite file without schema upgrades or writes;
- supports older alarm schemas that predate `equipment_origin`, `occurrence_count`, or `last_seen_at` and poll schemas that predate duration/error fields;
- writes alarms, polls, and a content fingerprint in one DuckDB transaction;
- deduplicates by alarm ID and poll timestamp;
- records the logical-source fingerprint so rerunning the same import is a no-op;
- leaves point samples already recorded in DuckDB untouched.

Afterward, verify the target:

```bash
metasys-dashboard check-history
```

Keep the original SQLite database and a DuckDB backup until row counts and application behavior have been reviewed. Passwords are not stored in either database; they remain in macOS Keychain.
