# SQL historian mirror

`mirror-sql-history` copies the complete configured JCI historian database and every accessible, online user database on the same SQL Server instance into DuckDB. The SQL Server connection is read-only. All non-value tables are refreshed atomically on each run, and the large append-only value tables are streamed in bounded transactions.

The target layout preserves all 24 historian tables under `jci_historian`. Each cycle also mirrors `JCIReportingDB`, `JCIAuditTrails`, `JCIEvents`, `JCIItemAnnotation`, and `MetasysReporting` into named schemas. Every other online user database the SQL login can access is discovered from `master` and copied to its own `sql_server__<database>` schema; the four SQL Server system databases are excluded. The 725-million-row reporting data table has its own timestamp-plus-identity checkpoints; smaller tables are atomically refreshed so changes to existing rows are retained. Source database mappings plus table, column, and index metadata are stored under `metasys_migration`, along with run history and per-point checkpoints.

## External-volume guard

The command requires a marker file containing exactly:

```text
METASYS_SUMMARY_EXTERNAL_STORAGE_V1
```

The target must be a `.duckdb` file on the same mounted filesystem and beneath the marker's directory. The command fails before creating a target directory if the marker is absent, altered, symlinked, or on another filesystem. This prevents a missing external disk from silently redirecting a large import onto the system disk.

## Initial copy or hourly continuation

```sh
metasys-history-mirror mirror-sql-history \
  --target /Volumes/TestStorage/MetasysData/JCIHistorian.duckdb \
  --volume-marker /Volumes/TestStorage/.metasys-storage-volume \
  --batch-size 100000
```

The same command performs both the initial backfill and later incremental passes. Each committed batch updates its checkpoint in the same DuckDB transaction, so rerunning after a disconnect, process termination, restart, or power loss resumes at the first uncommitted source timestamp.

For a bounded deployment test, add `--max-event-rows N`. This independently limits the raw historian stream and the large reporting stream to `N` new rows while still exercising complete atomic refreshes of the smaller databases. It pauses without invalidating the target or its checkpoints.

Use the read-only status check at any time the writer is stopped:

```sh
metasys-history-mirror check-sql-mirror \
  --target /Volumes/TestStorage/MetasysData/JCIHistorian.duckdb
```

`eventRows`, `checkpointRows`, `reportingRows`, and their checkpoint totals must match. `operationalSnapshotCountsCoverSource` must be true and `operationalSnapshotMismatches` must be empty; this audits every companion table against the source count captured when its copy began. A full initial pass is recorded in `completedFullPasses`; after that point, an hourly invocation only appends large-stream rows newer than each point's checkpoint while atomically refreshing smaller tables.

## Scheduling

Install or update the per-user LaunchAgent with:

```sh
scripts/install-history-mirror-launch-agent.sh
```

The LaunchAgent wakes once per hour and runs `run-scheduled-sql-mirror`. The command reads the enabled state, target, marker, cadence, and batch size from the dashboard's local SQLite settings. A cadence from 1 to 168 hours can be selected at **Administration → History mirror** (`/operations#mirror`); failed cycles are retried when that cadence is next due. launchd does not create a second instance while a backfill is still running.

Each scheduled attempt is written to local SQLite before the external volume or SQL password is accessed. The graphical health view therefore retains missing-volume, Keychain, helper, integrity, and interrupted-process errors even when the DuckDB target is unavailable. It displays LaunchAgent state, external-volume state, last success, next due time, row counts, duration, integrity, the latest error report, and recent attempt history. **Verify target now** performs a read-only marker and DuckDB integrity audit.

The fixed hourly LaunchAgent uses `ProcessType=Standard` with low-priority I/O. This avoids the restrictive CPU policy applied to long-running `Background` jobs while keeping the multi-billion-row copy polite to interactive workloads.

The mirror binary retrieves the existing SQL password from macOS Keychain at runtime and never stores it in the plist, script, command line, DuckDB file, or logs.

On macOS versions that require interactive removable-volume consent for a new background executable, the wrapper can also keep one authorized interactive process alive across cycles:

```sh
run-sql-history-mirror --continuous mirror-sql-history \
  --target /Volumes/TestStorage/MetasysData/JCIHistorian.duckdb \
  --volume-marker /Volumes/TestStorage/.metasys-storage-volume \
  --batch-size 250000
```

The default interval is 3600 seconds. Set `METASYS_SQL_MIRROR_INTERVAL_SECONDS` to another value of at least 60 seconds when testing.

### Removable-volume permission on macOS

Recent macOS releases require a background executable to have explicit removable-volume consent even when BSD ownership and mode bits permit access. `build-history-mirror-app.sh` creates `dist/Metasys History Mirror.app` with a stable bundle identifier and `NSRemovableVolumesUsageDescription`; the installer copies it to the user's Applications directory. Launch that app interactively once and approve the removable-volume prompt. With no command-line arguments its wrapper runs the saved schedule once; launchd supplies the recurring hourly wake-up.
