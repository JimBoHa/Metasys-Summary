use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};

use crate::{
    history::{HistoryPollRun, HistoryStore, LegacyHistoryImport},
    models::AlarmRecord,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryMigrationReport {
    pub source: PathBuf,
    pub target: PathBuf,
    pub source_fingerprint: String,
    pub alarm_rows: usize,
    pub poll_rows: usize,
    pub dry_run: bool,
    pub already_imported: bool,
}

struct LegacyHistorySnapshot {
    alarms: Vec<AlarmRecord>,
    poll_runs: Vec<HistoryPollRun>,
    fingerprint: String,
}

pub fn migrate_sqlite_history(
    source: &Path,
    target: &Path,
    dry_run: bool,
) -> Result<HistoryMigrationReport> {
    let source = source
        .canonicalize()
        .with_context(|| format!("resolve legacy SQLite database {}", source.display()))?;
    if !source.is_file() {
        bail!("legacy SQLite source must be a regular file");
    }
    if target.exists()
        && target
            .canonicalize()
            .with_context(|| format!("resolve DuckDB target {}", target.display()))?
            == source
    {
        bail!("legacy SQLite source and DuckDB target must be different files");
    }

    let snapshot = read_legacy_history(&source)?;
    let mut report = HistoryMigrationReport {
        source: source.clone(),
        target: target.to_owned(),
        source_fingerprint: snapshot.fingerprint.clone(),
        alarm_rows: snapshot.alarms.len(),
        poll_rows: snapshot.poll_runs.len(),
        dry_run,
        already_imported: false,
    };
    if dry_run {
        return Ok(report);
    }

    let source_path = source.to_string_lossy();
    let history = HistoryStore::open(target)?;
    let result = history.import_legacy_history(&LegacyHistoryImport {
        source_fingerprint: &snapshot.fingerprint,
        source_path: &source_path,
        alarms: &snapshot.alarms,
        poll_runs: &snapshot.poll_runs,
    })?;
    report.already_imported = result.already_imported;
    Ok(report)
}

fn read_legacy_history(source: &Path) -> Result<LegacyHistorySnapshot> {
    let connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open legacy SQLite database read-only {}", source.display()))?;
    connection
        .pragma_update(None, "query_only", true)
        .context("enforce read-only SQLite migration connection")?;
    let quick_check: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .context("validate legacy SQLite database")?;
    if quick_check != "ok" {
        bail!("legacy SQLite quick check failed: {quick_check}");
    }
    if !table_exists(&connection, "alarms")? {
        bail!("legacy SQLite database does not contain an alarms table");
    }

    let alarms = read_alarms(&connection)?;
    let poll_runs = if table_exists(&connection, "poll_log")? {
        read_poll_runs(&connection)?
    } else {
        Vec::new()
    };
    let fingerprint = history_fingerprint(&alarms, &poll_runs);
    Ok(LegacyHistorySnapshot {
        alarms,
        poll_runs,
        fingerprint,
    })
}

fn read_alarms(connection: &Connection) -> Result<Vec<AlarmRecord>> {
    let columns = table_columns(connection, "alarms")?;
    require_columns(
        "alarms",
        &columns,
        &[
            "id",
            "object_id",
            "equipment",
            "point",
            "message",
            "alarm_type",
            "category",
            "priority",
            "occurred_at",
            "active",
            "acknowledged",
            "source",
        ],
    )?;
    let equipment_origin = optional_column_expression(
        &columns,
        "equipment_origin",
        "'unknown'",
        "equipment_origin",
    );
    let occurrence_count =
        optional_column_expression(&columns, "occurrence_count", "1", "occurrence_count");
    let last_seen_at =
        optional_column_expression(&columns, "last_seen_at", "occurred_at", "last_seen_at");
    let query = format!(
        r#"
        SELECT id, object_id, equipment, {equipment_origin}, point, message,
               alarm_type, category, priority, occurred_at, active, acknowledged,
               {occurrence_count}, source, {last_seen_at}
        FROM alarms
        ORDER BY occurred_at, id
        "#
    );
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([])?;
    let mut alarms = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let occurred_at: String = row.get(9)?;
        let last_seen_at: Option<String> = row.get(14)?;
        alarms.push(AlarmRecord {
            id: id.clone(),
            object_id: row.get(1)?,
            equipment: row.get(2)?,
            equipment_origin: row.get(3)?,
            point: row.get(4)?,
            message: row.get(5)?,
            alarm_type: row.get(6)?,
            category: row.get(7)?,
            priority: row.get::<_, i64>(8)?.clamp(0, i64::from(u16::MAX)) as u16,
            occurred_at: parse_timestamp(&occurred_at, "alarm occurred_at", &id)?,
            active: row.get::<_, i64>(10)? != 0,
            acknowledged: row.get::<_, i64>(11)? != 0,
            occurrence_count: row.get::<_, i64>(12)?.max(1) as u64,
            source: row.get(13)?,
            last_seen_at: last_seen_at
                .as_deref()
                .map(|value| parse_timestamp(value, "alarm last_seen_at", &id))
                .transpose()?,
        });
    }
    Ok(alarms)
}

fn read_poll_runs(connection: &Connection) -> Result<Vec<HistoryPollRun>> {
    let columns = table_columns(connection, "poll_log")?;
    require_columns(
        "poll_log",
        &columns,
        &[
            "attempted_at",
            "succeeded",
            "active_alarm_count",
            "override_count",
        ],
    )?;
    let duration_ms = optional_column_expression(&columns, "duration_ms", "0", "duration_ms");
    let error_message =
        optional_column_expression(&columns, "error_message", "NULL", "error_message");
    let query = format!(
        r#"
        SELECT attempted_at, succeeded, active_alarm_count, override_count,
               {duration_ms}, {error_message}
        FROM poll_log
        ORDER BY attempted_at
        "#
    );
    let mut statement = connection.prepare(&query)?;
    let mut rows = statement.query([])?;
    let mut polls = Vec::new();
    while let Some(row) = rows.next()? {
        let attempted_at: String = row.get(0)?;
        polls.push(HistoryPollRun {
            attempted_at: parse_timestamp(&attempted_at, "poll attempted_at", &attempted_at)?,
            succeeded: row.get::<_, i64>(1)? != 0,
            active_alarm_count: row.get::<_, i64>(2)?.max(0) as usize,
            override_count: row.get::<_, i64>(3)?.max(0) as usize,
            duration_ms: row.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
            error_message: row.get(5)?,
        });
    }
    Ok(polls)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn table_columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let query = match table {
        "alarms" => "PRAGMA table_info(alarms)",
        "poll_log" => "PRAGMA table_info(poll_log)",
        _ => bail!("unsupported legacy SQLite table"),
    };
    let mut statement = connection.prepare(query)?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(columns.collect::<rusqlite::Result<BTreeSet<_>>>()?)
}

fn require_columns(table: &str, columns: &BTreeSet<String>, required: &[&str]) -> Result<()> {
    let missing = required
        .iter()
        .filter(|column| !columns.contains(**column))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "legacy SQLite {table} table is missing required columns: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

fn optional_column_expression(
    columns: &BTreeSet<String>,
    column: &str,
    fallback: &str,
    alias: &str,
) -> String {
    if columns.contains(column) {
        format!("{column} AS {alias}")
    } else {
        format!("{fallback} AS {alias}")
    }
}

fn parse_timestamp(value: &str, field: &str, row_identifier: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .with_context(|| format!("parse {field} for legacy row {row_identifier}"))
}

fn history_fingerprint(alarms: &[AlarmRecord], polls: &[HistoryPollRun]) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, "metasys-summary-sqlite-history-v1");
    for alarm in alarms {
        for value in [
            alarm.id.as_str(),
            alarm.object_id.as_str(),
            alarm.equipment.as_str(),
            alarm.equipment_origin.as_str(),
            alarm.point.as_str(),
            alarm.message.as_str(),
            alarm.alarm_type.as_str(),
            alarm.category.as_str(),
            alarm.source.as_str(),
        ] {
            hash_field(&mut hasher, value);
        }
        hash_field(&mut hasher, &alarm.priority.to_string());
        hash_field(&mut hasher, &alarm.occurred_at.to_rfc3339());
        hash_field(&mut hasher, if alarm.active { "1" } else { "0" });
        hash_field(&mut hasher, if alarm.acknowledged { "1" } else { "0" });
        hash_field(&mut hasher, &alarm.occurrence_count.to_string());
        hash_field(
            &mut hasher,
            &alarm
                .last_seen_at
                .map(|timestamp| timestamp.to_rfc3339())
                .unwrap_or_default(),
        );
    }
    for poll in polls {
        hash_field(&mut hasher, &poll.attempted_at.to_rfc3339());
        hash_field(&mut hasher, if poll.succeeded { "1" } else { "0" });
        hash_field(&mut hasher, &poll.active_alarm_count.to_string());
        hash_field(&mut hasher, &poll.override_count.to_string());
        hash_field(&mut hasher, &poll.duration_ms.to_string());
        hash_field(
            &mut hasher,
            poll.error_message.as_deref().unwrap_or_default(),
        );
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{Duration, Utc};
    use rusqlite::params;
    use tempfile::tempdir;

    use super::migrate_sqlite_history;
    use crate::{history::HistoryStore, models::AlarmRecord, store::Store};

    fn alarm() -> AlarmRecord {
        AlarmRecord {
            id: "legacy-alarm-1".to_owned(),
            object_id: "object-1".to_owned(),
            equipment: "TB2-201".to_owned(),
            equipment_origin: "fixture".to_owned(),
            point: "ZN-T".to_owned(),
            message: "Zone temperature alarm".to_owned(),
            alarm_type: "HighWarning".to_owned(),
            category: "HVAC".to_owned(),
            priority: 60,
            occurred_at: Utc::now() - Duration::hours(1),
            active: false,
            acknowledged: true,
            occurrence_count: 2,
            source: "Synthetic legacy fixture".to_owned(),
            last_seen_at: Some(Utc::now()),
        }
    }

    #[test]
    fn migrates_current_sqlite_history_read_only_and_idempotently() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("dashboard.sqlite3");
        let target = directory.path().join("history.duckdb");
        {
            let store = Store::open(&source).unwrap();
            store.upsert_alarms(&[alarm()]).unwrap();
            store.record_poll(true, 0, 1, 75, None).unwrap();
        }
        let source_before = fs::read(&source).unwrap();

        let first = migrate_sqlite_history(&source, &target, false).unwrap();
        assert!(!first.already_imported);
        assert_eq!(first.alarm_rows, 1);
        assert_eq!(first.poll_rows, 1);
        assert_eq!(fs::read(&source).unwrap(), source_before);
        let summary = HistoryStore::open(&target).unwrap().summary().unwrap();
        assert_eq!(summary.alarm_events, 1);
        assert_eq!(summary.poll_runs, 1);
        assert_eq!(summary.legacy_imports, 1);

        let second = migrate_sqlite_history(&source, &target, false).unwrap();
        assert!(second.already_imported);
        let summary = HistoryStore::open(&target).unwrap().summary().unwrap();
        assert_eq!(summary.alarm_events, 1);
        assert_eq!(summary.poll_runs, 1);
        assert_eq!(summary.legacy_imports, 1);
    }

    #[test]
    fn migrates_older_optional_column_layout() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("old-dashboard.sqlite3");
        let target = directory.path().join("history.duckdb");
        let connection = rusqlite::Connection::open(&source).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE alarms (
                    id TEXT PRIMARY KEY, object_id TEXT NOT NULL, equipment TEXT NOT NULL,
                    point TEXT NOT NULL, message TEXT NOT NULL, alarm_type TEXT NOT NULL,
                    category TEXT NOT NULL, priority INTEGER NOT NULL, occurred_at TEXT NOT NULL,
                    active INTEGER NOT NULL, acknowledged INTEGER NOT NULL, source TEXT NOT NULL
                );
                CREATE TABLE poll_log (
                    attempted_at TEXT PRIMARY KEY, succeeded INTEGER NOT NULL,
                    active_alarm_count INTEGER NOT NULL, override_count INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        let timestamp = Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO alarms VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, 0, ?10)",
                params![
                    "old-alarm",
                    "old-object",
                    "AHU-1",
                    "SA-T",
                    "Old alarm",
                    "HighWarning",
                    "HVAC",
                    60,
                    timestamp,
                    "Synthetic fixture",
                ],
            )
            .unwrap();
        connection
            .execute("INSERT INTO poll_log VALUES (?1, 1, 1, 0)", [&timestamp])
            .unwrap();
        drop(connection);

        let report = migrate_sqlite_history(&source, &target, false).unwrap();
        assert_eq!(report.alarm_rows, 1);
        assert_eq!(report.poll_rows, 1);
        let summary = HistoryStore::open(&target).unwrap().summary().unwrap();
        assert_eq!(summary.alarm_events, 1);
        assert_eq!(summary.poll_runs, 1);
    }

    #[test]
    fn dry_run_validates_without_creating_duckdb() {
        let directory = tempdir().unwrap();
        let source = directory.path().join("dashboard.sqlite3");
        let target = directory.path().join("history.duckdb");
        drop(Store::open(&source).unwrap());

        let report = migrate_sqlite_history(&source, &target, true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.alarm_rows, 0);
        assert!(!target.exists());
    }
}
