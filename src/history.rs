use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use duckdb::{Connection, params};

use crate::models::AlarmRecord;

const HISTORY_SCHEMA_VERSION: i32 = 2;
const ALARM_UPSERT: &str = r#"
    INSERT INTO alarm_events (
        alarm_id, object_id, equipment, equipment_origin, point, message,
        alarm_type, category, priority, occurred_at, active, acknowledged,
        occurrence_count, source, first_recorded_at, last_recorded_at
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CAST(?10 AS TIMESTAMPTZ),
        ?11, ?12, ?13, ?14, CAST(?15 AS TIMESTAMPTZ), CAST(?15 AS TIMESTAMPTZ)
    )
    ON CONFLICT (alarm_id) DO UPDATE SET
        object_id = excluded.object_id,
        equipment = excluded.equipment,
        equipment_origin = excluded.equipment_origin,
        point = excluded.point,
        message = excluded.message,
        alarm_type = excluded.alarm_type,
        category = excluded.category,
        priority = excluded.priority,
        occurred_at = excluded.occurred_at,
        active = excluded.active,
        acknowledged = excluded.acknowledged,
        occurrence_count = GREATEST(alarm_events.occurrence_count, excluded.occurrence_count),
        source = excluded.source,
        last_recorded_at = excluded.last_recorded_at
"#;
const POLL_UPSERT: &str = r#"
    INSERT INTO poll_runs (
        attempted_at, succeeded, active_alarm_count, override_count,
        duration_ms, error_message
    ) VALUES (CAST(?1 AS TIMESTAMPTZ), ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT (attempted_at) DO UPDATE SET
        succeeded = excluded.succeeded,
        active_alarm_count = excluded.active_alarm_count,
        override_count = excluded.override_count,
        duration_ms = excluded.duration_ms,
        error_message = excluded.error_message
"#;

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryPointSample {
    pub point_slice_id: i32,
    pub point_name: String,
    pub equipment: String,
    pub timestamp: DateTime<Utc>,
    pub value: f64,
    pub unit: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPollRun {
    pub attempted_at: DateTime<Utc>,
    pub succeeded: bool,
    pub active_alarm_count: usize,
    pub override_count: usize,
    pub duration_ms: u64,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySummary {
    pub schema_version: i32,
    pub point_samples: u64,
    pub alarm_events: u64,
    pub poll_runs: u64,
    pub legacy_imports: u64,
}

pub struct LegacyHistoryImport<'a> {
    pub source_fingerprint: &'a str,
    pub source_path: &'a str,
    pub alarms: &'a [AlarmRecord],
    pub poll_runs: &'a [HistoryPollRun],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyHistoryImportResult {
    pub already_imported: bool,
    pub alarm_events: usize,
    pub poll_runs: usize,
}

pub struct HistoryStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl HistoryStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create DuckDB history directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open DuckDB history database {}", path.display()))?;
        initialize_schema(&connection)?;
        Ok(Self {
            path: path.to_owned(),
            connection: Mutex::new(connection),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_point_samples(&self, samples: &[HistoryPointSample]) -> Result<usize> {
        for sample in samples {
            validate_point_sample(sample)?;
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .context("begin DuckDB point-sample transaction")?;
        let mut inserted = 0;
        {
            let mut statement = transaction.prepare(
                r#"
                INSERT INTO point_samples (
                    historian_point_slice_id, point_name, equipment, sample_time,
                    sample_value, unit, source, recorded_at
                ) VALUES (?1, ?2, ?3, CAST(?4 AS TIMESTAMPTZ), ?5, ?6, ?7, CURRENT_TIMESTAMP)
                ON CONFLICT DO NOTHING
                "#,
            )?;
            for sample in samples {
                inserted += statement.execute(params![
                    sample.point_slice_id,
                    sample.point_name,
                    sample.equipment,
                    sample.timestamp.to_rfc3339(),
                    sample.value,
                    sample.unit,
                    sample.source,
                ])?;
            }
        }
        transaction
            .commit()
            .context("commit DuckDB point-sample transaction")?;
        Ok(inserted)
    }

    pub fn record_alarms(
        &self,
        alarms: &[AlarmRecord],
        observed_at: DateTime<Utc>,
    ) -> Result<usize> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .context("begin DuckDB alarm transaction")?;
        let recorded = upsert_alarms(&transaction, alarms, Some(observed_at))?;
        transaction
            .commit()
            .context("commit DuckDB alarm transaction")?;
        Ok(recorded)
    }

    pub fn record_poll(&self, poll: &HistoryPollRun) -> Result<usize> {
        let connection = self.lock()?;
        connection
            .execute(
                POLL_UPSERT,
                params![
                    poll.attempted_at.to_rfc3339(),
                    poll.succeeded,
                    poll.active_alarm_count.min(i64::MAX as usize) as i64,
                    poll.override_count.min(i64::MAX as usize) as i64,
                    poll.duration_ms.min(i64::MAX as u64) as i64,
                    poll.error_message,
                ],
            )
            .context("record DuckDB poll run")
    }

    pub fn import_legacy_history(
        &self,
        import: &LegacyHistoryImport<'_>,
    ) -> Result<LegacyHistoryImportResult> {
        validate_legacy_import(import)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .context("begin atomic legacy-history import")?;
        let already_imported: bool = transaction.query_row(
            "SELECT COUNT(*) > 0 FROM legacy_imports WHERE source_fingerprint = ?1",
            [import.source_fingerprint],
            |row| row.get(0),
        )?;
        if already_imported {
            transaction.rollback()?;
            return Ok(LegacyHistoryImportResult {
                already_imported: true,
                alarm_events: 0,
                poll_runs: 0,
            });
        }

        let alarm_events = upsert_alarms(&transaction, import.alarms, None)?;
        let mut poll_runs = 0;
        {
            let mut statement = transaction.prepare(POLL_UPSERT)?;
            for poll in import.poll_runs {
                poll_runs += statement.execute(params![
                    poll.attempted_at.to_rfc3339(),
                    poll.succeeded,
                    poll.active_alarm_count.min(i64::MAX as usize) as i64,
                    poll.override_count.min(i64::MAX as usize) as i64,
                    poll.duration_ms.min(i64::MAX as u64) as i64,
                    poll.error_message,
                ])?;
            }
        }
        transaction.execute(
            r#"
            INSERT INTO legacy_imports (
                source_fingerprint, source_path, imported_at, alarm_rows, poll_rows
            ) VALUES (?1, ?2, CURRENT_TIMESTAMP, ?3, ?4)
            "#,
            params![
                import.source_fingerprint,
                import.source_path,
                import.alarms.len().min(i64::MAX as usize) as i64,
                import.poll_runs.len().min(i64::MAX as usize) as i64,
            ],
        )?;
        transaction
            .commit()
            .context("commit atomic legacy-history import")?;
        Ok(LegacyHistoryImportResult {
            already_imported: false,
            alarm_events,
            poll_runs,
        })
    }

    pub fn summary(&self) -> Result<HistorySummary> {
        let connection = self.lock()?;
        Ok(HistorySummary {
            schema_version: connection.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM history_schema",
                [],
                |row| row.get(0),
            )?,
            point_samples: count_rows(&connection, "point_samples")?,
            alarm_events: count_rows(&connection, "alarm_events")?,
            poll_runs: count_rows(&connection, "poll_runs")?,
            legacy_imports: count_rows(&connection, "legacy_imports")?,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("DuckDB history lock is poisoned"))
    }
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            r#"
            SET TimeZone = 'UTC';

            CREATE TABLE IF NOT EXISTS history_schema (
                version INTEGER PRIMARY KEY,
                applied_at TIMESTAMPTZ NOT NULL
            );

            CREATE TABLE IF NOT EXISTS point_samples (
                historian_point_slice_id INTEGER NOT NULL,
                point_name VARCHAR NOT NULL,
                equipment VARCHAR NOT NULL,
                sample_time TIMESTAMPTZ NOT NULL,
                sample_value DOUBLE NOT NULL,
                unit VARCHAR,
                source VARCHAR NOT NULL,
                recorded_at TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (historian_point_slice_id, sample_time, source)
            );

            CREATE INDEX IF NOT EXISTS idx_point_samples_time
                ON point_samples(sample_time);
            CREATE INDEX IF NOT EXISTS idx_point_samples_equipment_time
                ON point_samples(equipment, sample_time);

            CREATE TABLE IF NOT EXISTS alarm_events (
                alarm_id VARCHAR PRIMARY KEY,
                object_id VARCHAR NOT NULL,
                equipment VARCHAR NOT NULL,
                equipment_origin VARCHAR NOT NULL,
                point VARCHAR NOT NULL,
                message VARCHAR NOT NULL,
                alarm_type VARCHAR NOT NULL,
                category VARCHAR NOT NULL,
                priority INTEGER NOT NULL,
                occurred_at TIMESTAMPTZ NOT NULL,
                active BOOLEAN NOT NULL,
                acknowledged BOOLEAN NOT NULL,
                occurrence_count BIGINT NOT NULL,
                source VARCHAR NOT NULL,
                first_recorded_at TIMESTAMPTZ NOT NULL,
                last_recorded_at TIMESTAMPTZ NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_alarm_events_occurred_at
                ON alarm_events(occurred_at);
            CREATE INDEX IF NOT EXISTS idx_alarm_events_equipment
                ON alarm_events(equipment);

            CREATE TABLE IF NOT EXISTS poll_runs (
                attempted_at TIMESTAMPTZ PRIMARY KEY,
                succeeded BOOLEAN NOT NULL,
                active_alarm_count BIGINT NOT NULL,
                override_count BIGINT NOT NULL,
                duration_ms BIGINT NOT NULL,
                error_message VARCHAR
            );

            INSERT INTO history_schema (version, applied_at)
            VALUES (1, CURRENT_TIMESTAMP)
            ON CONFLICT DO NOTHING;

            CREATE TABLE IF NOT EXISTS legacy_imports (
                source_fingerprint VARCHAR PRIMARY KEY,
                source_path VARCHAR NOT NULL,
                imported_at TIMESTAMPTZ NOT NULL,
                alarm_rows BIGINT NOT NULL,
                poll_rows BIGINT NOT NULL
            );

            INSERT INTO history_schema (version, applied_at)
            VALUES (2, CURRENT_TIMESTAMP)
            ON CONFLICT DO NOTHING;
            "#,
        )
        .context("initialize DuckDB history schema")?;
    let schema_version: i32 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM history_schema",
        [],
        |row| row.get(0),
    )?;
    if schema_version != HISTORY_SCHEMA_VERSION {
        bail!(
            "unsupported DuckDB history schema version {schema_version}; expected {HISTORY_SCHEMA_VERSION}"
        );
    }
    Ok(())
}

fn validate_point_sample(sample: &HistoryPointSample) -> Result<()> {
    if sample.point_slice_id <= 0 {
        bail!("historian point slice identifiers must be positive");
    }
    if !sample.value.is_finite() {
        bail!("historian point samples must contain finite numeric values");
    }
    for (label, value, maximum) in [
        ("point name", sample.point_name.as_str(), 700),
        ("equipment", sample.equipment.as_str(), 300),
        ("source", sample.source.as_str(), 300),
    ] {
        if value.chars().count() > maximum || value.chars().any(char::is_control) {
            bail!("history {label} is invalid");
        }
    }
    if sample
        .unit
        .as_ref()
        .is_some_and(|unit| unit.chars().count() > 80 || unit.chars().any(char::is_control))
    {
        bail!("history point unit is invalid");
    }
    Ok(())
}

fn validate_legacy_import(import: &LegacyHistoryImport<'_>) -> Result<()> {
    if import.source_fingerprint.len() != 64
        || !import
            .source_fingerprint
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        bail!("legacy history source fingerprint must be a SHA-256 hex digest");
    }
    if import.source_path.is_empty()
        || import.source_path.chars().count() > 4_096
        || import.source_path.chars().any(char::is_control)
    {
        bail!("legacy history source path is invalid");
    }
    Ok(())
}

fn upsert_alarms(
    transaction: &duckdb::Transaction<'_>,
    alarms: &[AlarmRecord],
    observed_at: Option<DateTime<Utc>>,
) -> Result<usize> {
    let mut statement = transaction.prepare(ALARM_UPSERT)?;
    let mut recorded = 0;
    for alarm in alarms {
        let recorded_at = observed_at
            .or(alarm.last_seen_at)
            .unwrap_or(alarm.occurred_at)
            .to_rfc3339();
        recorded += statement.execute(params![
            alarm.id,
            alarm.object_id,
            alarm.equipment,
            alarm.equipment_origin,
            alarm.point,
            alarm.message,
            alarm.alarm_type,
            alarm.category,
            i32::from(alarm.priority),
            alarm.occurred_at.to_rfc3339(),
            alarm.active,
            alarm.acknowledged,
            alarm.occurrence_count.min(i64::MAX as u64) as i64,
            alarm.source,
            recorded_at,
        ])?;
    }
    Ok(recorded)
}

fn count_rows(connection: &Connection, table: &str) -> Result<u64> {
    let query = match table {
        "point_samples" => "SELECT COUNT(*) FROM point_samples",
        "alarm_events" => "SELECT COUNT(*) FROM alarm_events",
        "poll_runs" => "SELECT COUNT(*) FROM poll_runs",
        "legacy_imports" => "SELECT COUNT(*) FROM legacy_imports",
        _ => bail!("unsupported history table"),
    };
    let count: i64 = connection.query_row(query, [], |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    use super::{HistoryPointSample, HistoryPollRun, HistoryStore};
    use crate::models::AlarmRecord;

    fn alarm(id: &str) -> AlarmRecord {
        AlarmRecord {
            id: id.to_owned(),
            object_id: format!("object-{id}"),
            equipment: "AHU-1".to_owned(),
            equipment_origin: "fixture".to_owned(),
            point: "SA-T".to_owned(),
            message: "Supply air temperature alarm".to_owned(),
            alarm_type: "HighWarning".to_owned(),
            category: "HVAC".to_owned(),
            priority: 60,
            occurred_at: Utc::now() - Duration::minutes(5),
            active: true,
            acknowledged: false,
            occurrence_count: 1,
            source: "Synthetic test".to_owned(),
            last_seen_at: None,
        }
    }

    #[test]
    fn creates_reopens_and_summarizes_history_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.duckdb");
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.path(), path);
        assert_eq!(store.summary().unwrap().schema_version, 2);
        drop(store);
        assert_eq!(
            HistoryStore::open(&path)
                .unwrap()
                .summary()
                .unwrap()
                .point_samples,
            0
        );
    }

    #[test]
    fn upgrades_version_one_database_for_legacy_imports() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.duckdb");
        let connection = duckdb::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE history_schema (
                    version INTEGER PRIMARY KEY,
                    applied_at TIMESTAMPTZ NOT NULL
                );
                INSERT INTO history_schema VALUES (1, CURRENT_TIMESTAMP);
                "#,
            )
            .unwrap();
        drop(connection);

        let summary = HistoryStore::open(&path).unwrap().summary().unwrap();
        assert_eq!(summary.schema_version, 2);
        assert_eq!(summary.legacy_imports, 0);
    }

    #[test]
    fn records_point_samples_idempotently_and_rejects_invalid_values() {
        let directory = tempdir().unwrap();
        let store = HistoryStore::open(&directory.path().join("history.duckdb")).unwrap();
        let sample = HistoryPointSample {
            point_slice_id: 42,
            point_name: "ZN-T".to_owned(),
            equipment: "TB2-201".to_owned(),
            timestamp: Utc::now(),
            value: 71.5,
            unit: Some("degF".to_owned()),
            source: "Synthetic historian".to_owned(),
        };
        assert_eq!(
            store
                .record_point_samples(std::slice::from_ref(&sample))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .record_point_samples(std::slice::from_ref(&sample))
                .unwrap(),
            0
        );
        assert_eq!(store.summary().unwrap().point_samples, 1);

        let mut invalid = sample;
        invalid.value = f64::NAN;
        assert!(store.record_point_samples(&[invalid]).is_err());
        assert_eq!(store.summary().unwrap().point_samples, 1);
    }

    #[test]
    fn upserts_alarm_events_and_poll_runs() {
        let directory = tempdir().unwrap();
        let store = HistoryStore::open(&directory.path().join("history.duckdb")).unwrap();
        let observed_at = Utc::now();
        let mut event = alarm("alarm-1");
        store
            .record_alarms(std::slice::from_ref(&event), observed_at)
            .unwrap();
        event.active = false;
        event.acknowledged = true;
        event.occurrence_count = 3;
        store
            .record_alarms(&[event], observed_at + Duration::minutes(1))
            .unwrap();

        let poll = HistoryPollRun {
            attempted_at: observed_at,
            succeeded: true,
            active_alarm_count: 1,
            override_count: 0,
            duration_ms: 75,
            error_message: None,
        };
        store.record_poll(&poll).unwrap();
        store.record_poll(&poll).unwrap();
        let summary = store.summary().unwrap();
        assert_eq!(summary.alarm_events, 1);
        assert_eq!(summary.poll_runs, 1);
    }
}
